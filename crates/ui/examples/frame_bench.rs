//! Frame-time workload: CPU time of one UI pass plus tessellation on synthetic screens.
//! Run optimized: `cargo run --release -p ui --example frame_bench`. Numbers are per pass, so
//! they compare builds on the same machine; they are not frames per second on any display.
use std::time::{Duration, Instant};

use client_core::State;
use model::{Id, Reaction, ReactionEmoji};

const SIZE: egui::Vec2 = egui::vec2(1280.0, 800.0);
const WARMUP: usize = 60;
const SAMPLES: usize = 400;

struct Bench {
	ctx: egui::Context,
	view: ui::MessagingUi,
	time: f64,
}

impl Bench {
	fn new(show_members: bool) -> Self {
		let ctx = egui::Context::default();
		ui::fonts::install(&ctx);
		let _ = ui::emoji::install(&ctx);
		ui::icons::install(&ctx);
		ui::design::apply(&ctx);
		let mut view = ui::MessagingUi::default();
		view.reading_preferences.show_members = show_members;
		Self {
			ctx,
			view,
			time: 0.0,
		}
	}

	/// CPU time of the pass, and how soon it asked for the next one.
	fn pass(&mut self, state: &mut State, events: Vec<egui::Event>) -> (Duration, Duration) {
		self.time += 1.0 / 144.0;
		let start = Instant::now();
		let mut output = self.ctx.run_ui(
			egui::RawInput {
				screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SIZE)),
				time: Some(self.time),
				predicted_dt: 1.0 / 144.0,
				focused: true,
				events,
				..Default::default()
			},
			|ui| {
				self.view.show(ui, state);
			},
		);
		// The first pass is warmup, so listing its text for a content check costs no sample.
		if std::env::var_os("FRAME_BENCH_DUMP").is_some() && self.time < 0.01 {
			let mut texts = Vec::new();
			for shape in &output.shapes {
				collect(&shape.shape, &mut texts);
			}
			println!("{} text shapes: {texts:?}", texts.len());
		}
		let primitives = self
			.ctx
			.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
		let elapsed = start.elapsed();
		std::hint::black_box(primitives.len());
		let repaint = output
			.viewport_output
			.get(&egui::ViewportId::ROOT)
			.map_or(Duration::MAX, |viewport| viewport.repaint_delay);
		output.drop_without_applying_deltas();
		(elapsed, repaint)
	}
}

fn collect(shape: &egui::Shape, out: &mut Vec<String>) {
	match shape {
		egui::Shape::Text(text) => out.push(text.galley.job.text.chars().take(24).collect()),
		egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| collect(shape, out)),
		_ => {}
	}
}

fn report(name: &str, samples: Vec<(Duration, Duration)>) {
	// Passes that asked for another frame right away: an idle screen would keep rendering.
	let eager = samples
		.iter()
		.filter(|(_, repaint)| *repaint < Duration::from_millis(20))
		.count();
	let mut samples: Vec<_> = samples.into_iter().map(|(time, _)| time).collect();
	samples.sort();
	let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize].as_secs_f64() * 1e3;
	let mean = samples.iter().sum::<Duration>().as_secs_f64() * 1e3 / samples.len() as f64;
	println!(
		"{name:<18} median {:>7.3} ms  p95 {:>7.3} ms  mean {mean:>7.3} ms  eager {:>3.0}%",
		at(0.5),
		at(0.95),
		100.0 * eager as f64 / SAMPLES as f64
	);
}

fn message(id: u64, channel: Id) -> model::Message {
	let mut message = test_support::message(id, channel);
	// Runs of two or three rows per author, as in a busy channel.
	message.author.id = Id(100 + (id / 3) % 7);
	message.author.name = format!("Member {}", (id / 3) % 7);
	message.attachments.clear();
	if id.is_multiple_of(5) {
		message.reactions = Some(vec![Reaction {
			emoji: ReactionEmoji {
				id: None,
				name: Some("👍".into()),
			},
			count: (id % 9 + 1) as u32,
			me: id.is_multiple_of(2),
			me_burst: false,
		}]);
	}
	if id.is_multiple_of(7) {
		message.embeds = vec![model::Embed {
			kind: "rich".into(),
			title: Some(format!("Build {id}")),
			description: Some("The job finished with **3** warnings.".into()),
			url: Some(format!("https://example.com/build/{id}")),
			color: Some(0x5865F2),
			..Default::default()
		}];
	}
	message
}

fn channel_state(channel: Id, count: u64) -> State {
	let mut state = test_support::demo_state();
	state.select(channel);
	state.timeline.clear();
	for id in 1..=count {
		state
			.timeline
			.insert(message(10_000 + id, channel), false, false)
			.unwrap();
	}
	state.history_pending = false;
	state.older_exhausted = true;
	state.revision += 1;
	state
}

fn run(
	name: &str,
	bench: &mut Bench,
	state: &mut State,
	mut step: impl FnMut(usize, &mut State) -> Vec<egui::Event>,
) {
	for frame in 0..WARMUP {
		let events = step(frame, state);
		bench.pass(state, events);
	}
	let samples = (0..SAMPLES)
		.map(|frame| {
			let events = step(WARMUP + frame, state);
			bench.pass(state, events)
		})
		.collect();
	report(name, samples);
}

fn main() {
	let channel = Id(21);
	let pointer = egui::pos2(SIZE.x * 0.55, SIZE.y * 0.5);

	let mut bench = Bench::new(true);
	let mut state = channel_state(channel, 300);
	run("channel idle", &mut bench, &mut state, |_, _| Vec::new());

	run("channel scroll", &mut bench, &mut state, |frame, _| {
		// Back and forth so the view never parks at either end.
		let direction = if (frame / 90).is_multiple_of(2) {
			1.0
		} else {
			-1.0
		};
		vec![
			egui::Event::PointerMoved(pointer),
			egui::Event::MouseWheel {
				unit: egui::MouseWheelUnit::Point,
				delta: egui::vec2(0.0, 36.0 * direction),
				modifiers: egui::Modifiers::NONE,
				phase: egui::TouchPhase::Move,
				source: egui::MouseWheelSource::Wheel,
			},
		]
	});

	let mut bench = Bench::new(true);
	let mut state = channel_state(channel, 300);
	let mut next = 20_000;
	run("gateway churn", &mut bench, &mut state, |frame, state| {
		// A state change every pass (presence, typing, read state), a new message every eighth.
		if frame.is_multiple_of(8) {
			next += 1;
			state
				.timeline
				.insert(message(next, channel), false, false)
				.unwrap();
		}
		state.revision += 1;
		Vec::new()
	});

	let mut bench = Bench::new(true);
	let mut state = test_support::voice_demo_state();
	run("voice channel", &mut bench, &mut state, |_, _| Vec::new());

	let mut bench = Bench::new(false);
	let mut state = test_support::call_demo_state();
	run("dm call", &mut bench, &mut state, |_, _| Vec::new());
}
