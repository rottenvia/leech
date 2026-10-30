//! The compact launch window. Leech opens as a 300 by 350 splash and grows to its saved
//! size once the account is ready or the sign-in screen is needed; an update found at launch
//! installs here instead, the worm drinking it in.
use eframe::egui::{self, Color32, Pos2, Vec2, pos2, vec2};
use std::f32::consts::TAU;
use std::time::{Duration, Instant};

pub const SIZE: [f32; 2] = [300.0, 350.0];
/// One crawl of the mark, so a quick restore reads as motion rather than a flash.
const SHORTEST: Duration = Duration::from_millis(900);
/// A restore still running by now moves to the full restore screen, which has a way out.
pub const LONGEST: Duration = Duration::from_secs(12);
/// The first update check may hold the splash this long, so a waiting update installs here
/// instead of popping up in the title bar a moment after the window grows.
pub const UPDATE_WAIT: Duration = Duration::from_secs(4);
/// Seconds the finished bar holds and the splash fades to its bare background.
const FADE: f32 = 0.22;

pub enum Look<'a> {
	/// Restoring the session: the current stage, and whether it has finished.
	Starting { stage: Option<&'a str>, done: bool },
	/// Installing an update found at launch; `progress` while it downloads.
	Updating {
		version: &'a str,
		progress: Option<f32>,
		installing: bool,
	},
}

pub struct Splash {
	started: Instant,
	/// Typical restore length, from the last few launches.
	expected: f32,
	bar: f32,
	fill: f32,
	/// UI time the fade began.
	leaving: Option<f64>,
}

impl Splash {
	pub fn new(restore_ms: u32) -> Self {
		Self {
			started: Instant::now(),
			expected: if restore_ms == 0 {
				2.4
			} else {
				restore_ms as f32 / 1000.0
			},
			bar: 0.0,
			fill: 0.0,
			leaving: None,
		}
	}

	pub fn elapsed(&self) -> Duration {
		self.started.elapsed()
	}

	/// Paints one frame over the whole window. True once a finished splash has faded out
	/// and the window may grow.
	pub fn show(&mut self, ui: &mut egui::Ui, look: &Look<'_>) -> bool {
		let rect = ui.max_rect();
		let time = ui.input(|input| input.time);
		let dt = ui.input(|input| input.stable_dt).min(0.1);
		let ease = |value: &mut f32, target: f32, rate: f32| {
			*value += (target - *value) * (1.0 - (-dt * rate).exp());
		};
		let elapsed = self.elapsed().as_secs_f32();
		let done = matches!(look, Look::Starting { done: true, .. }) && self.elapsed() >= SHORTEST;
		let (bar_target, title, detail) = match *look {
			Look::Starting { stage, .. } => {
				// About 90% once the usual restore time has passed; full only when done.
				let estimate = (1.0 - (-2.3 * elapsed / self.expected).exp()).min(0.94);
				let left = (self.expected - elapsed).max(0.0);
				let eta = if left >= 1.0 {
					format!("about {} s left", left.ceil() as u32)
				} else {
					"almost there".to_owned()
				};
				let detail = match stage {
					Some(stage) if !done => format!("{stage}… · {eta}"),
					_ => ui::i18n::translate("main-splash-loading"),
				};
				(
					if done { 1.0 } else { estimate },
					ui::i18n::translate("main-splash-starting"),
					detail,
				)
			}
			Look::Updating {
				version,
				progress,
				installing,
			} => {
				let detail = match progress {
					Some(progress) => format!(
						"{} · {}%",
						ui::i18n::translate_args(
							"main-splash-downloading",
							&[("version", version)]
						),
						(progress * 100.0).floor() as u32
					),
					None if installing => {
						ui::i18n::translate_args("main-splash-installing", &[("version", version)])
					}
					None => ui::i18n::translate("main-splash-restarting"),
				};
				let target = if installing || progress.is_none() {
					1.0
				} else {
					progress.unwrap_or(0.0)
				};
				(target, ui::i18n::translate("main-splash-updating"), detail)
			}
		};
		let updating = matches!(look, Look::Updating { .. });
		ease(&mut self.bar, bar_target, 8.0);
		ease(&mut self.fill, if updating { bar_target } else { 0.0 }, 5.0);
		if done && self.bar > 0.985 && self.leaving.is_none() {
			self.leaving = Some(time);
		}
		let fade = self
			.leaving
			.map_or(0.0, |start| ((time - start) as f32 / FADE).clamp(0.0, 1.0));

		let palette = ui::design::palette(ui);
		let canvas = ui::design::window_palette(ui).canvas;
		ui::design::window_drag(ui, rect);
		let painter = ui.painter();
		painter.rect_filled(rect, 0, canvas);
		// Everything but the background fades, so the window grows from a plain colour.
		let alpha = 1.0 - smooth(fade);
		let t = time as f32;
		worm(
			painter,
			pos2(rect.center().x, rect.top()),
			t,
			updating.then_some(self.fill),
			palette.accent,
			canvas,
			ui.visuals().dark_mode,
			alpha,
		);
		let text = |y: f32, text: String, font: egui::FontId, color: Color32| {
			let color = color.gamma_multiply(alpha);
			let galley = painter.layout_no_wrap(text, font, color);
			let size = galley.size();
			painter.galley(
				pos2(rect.center().x - size.x * 0.5, y - size.y * 0.5),
				galley,
				color,
			);
		};
		text(
			rect.top() + 232.0,
			title.clone(),
			egui::FontId::new(16.0, ui::design::semibold_family(ui.ctx())),
			palette.text_strong,
		);
		// The bar, with a glow under its head and a light running along the fill.
		let track = egui::Rect::from_center_size(
			pos2(rect.center().x, rect.top() + 258.0),
			vec2(168.0, 4.0),
		);
		painter.rect_filled(track, 2, palette.raised.gamma_multiply(alpha));
		let fill = egui::Rect::from_min_max(
			track.min,
			pos2(track.left() + track.width() * self.bar, track.bottom()),
		);
		if fill.width() > 0.5 {
			radial(
				painter,
				pos2(fill.right(), fill.center().y),
				9.0,
				palette.accent.gamma_multiply(0.45 * alpha),
			);
			painter.rect_filled(fill, 2, palette.accent.gamma_multiply(alpha));
			let run = (t % 1.6) / 1.6;
			let x = fill.left() + (fill.width() + 60.0) * run - 30.0;
			let mut mesh = egui::Mesh::default();
			let lit = Color32::WHITE.gamma_multiply(0.35 * alpha);
			for (dx, color) in [
				(-30.0, Color32::TRANSPARENT),
				(0.0, lit),
				(30.0, Color32::TRANSPARENT),
			] {
				mesh.colored_vertex(pos2(x + dx, fill.top()), color);
				mesh.colored_vertex(pos2(x + dx, fill.bottom()), color);
			}
			for base in [0, 2] {
				mesh.add_triangle(base, base + 1, base + 2);
				mesh.add_triangle(base + 1, base + 2, base + 3);
			}
			painter.with_clip_rect(fill).add(egui::Shape::mesh(mesh));
		}
		text(
			rect.top() + 278.0,
			detail.clone(),
			egui::FontId::proportional(12.0),
			palette.muted,
		);
		text(
			rect.bottom() - 20.0,
			format!("v{}", env!("CARGO_PKG_VERSION")),
			egui::FontId::proportional(11.0),
			palette.muted.gamma_multiply(0.6),
		);
		let response = ui.interact(rect, egui::Id::unique("leech-splash"), egui::Sense::hover());
		response.widget_info(|| {
			egui::WidgetInfo::labeled(
				egui::Role::ProgressIndicator,
				true,
				format!("{title}, {detail}"),
			)
		});
		ui.ctx().request_repaint();
		fade >= 1.0
	}
}

fn smooth(t: f32) -> f32 {
	t * t * (3.0 - 2.0 * t)
}

/// A soft rose light behind the mark that breathes with it.
fn glow(painter: &egui::Painter, center: Pos2, accent: Color32, dark: bool, t: f32, alpha: f32) {
	let breath = 0.5 + 0.5 * (t * 2.0).sin();
	let strength = if dark { 0.20 } else { 0.11 } * (0.85 + 0.15 * breath) * alpha;
	radial(
		painter,
		center,
		128.0 + 6.0 * breath,
		accent.gamma_multiply(strength),
	);
}

/// `color` at `center`, fading to nothing at `radius`.
fn radial(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
	let mut mesh = egui::Mesh::default();
	mesh.colored_vertex(center, color);
	const SEGMENTS: u32 = 48;
	for i in 0..=SEGMENTS {
		let angle = i as f32 / SEGMENTS as f32 * TAU;
		mesh.colored_vertex(
			center + vec2(angle.cos(), angle.sin()) * radius,
			Color32::TRANSPARENT,
		);
	}
	for i in 1..=SEGMENTS {
		mesh.add_triangle(0, i, i + 1);
	}
	painter.add(egui::Shape::mesh(mesh));
}

/// The worm, seen from above: it crawls up a wave like a snake, then slips back down. Sizes
/// are in points from the window's top edge and centre line.
const LENGTH: f32 = 92.0;
/// Half-widths along the body from head (0) to tail (1), in the app icon's proportions: a
/// round head that tapers through the body to the tail.
const WIDTHS: [(f32, f32); 4] = [(0.0, 19.7), (0.33, 15.2), (0.67, 10.9), (1.0, 7.3)];
/// The wave it crawls along: its wavelength and how far it sways to each side. Its tightest
/// bend stays wider than the body, so the inner side never creases.
const WAVE: f32 = 84.0;
const SWAY: f32 = 8.0;
/// How far up the head climbs: a whole wave, so slipping straight back down lands the body
/// on the same curve it starts the next climb from.
const CLIMB: f32 = WAVE;
/// The head's highest point, just under the top edge.
const TOP: f32 = 38.0;
/// Seconds of each part of one loop: climbing, resting at the top, slipping down, settling.
const CRAWL: f32 = 5.2;
const REST: f32 = 0.5;
const SLIP: f32 = 2.8;
const SETTLE: f32 = 0.4;
const LOOP: f32 = CRAWL + REST + SLIP + SETTLE;
/// The line of oil its head leaves as it slips: how much of the head's width touches the
/// floor, and how long the oil takes to soak away once it climbs again.
const FILM: f32 = 0.66;
const SOAK: f32 = 1.6;
const SAMPLES: usize = 72;

/// Where the wave is at height `y` below the window top.
fn sway(y: f32) -> f32 {
	SWAY * (y * TAU / WAVE).sin()
}

/// Arc length along the wave, tabulated by height, so the body keeps its length on the bends.
struct Path {
	heights: Vec<f32>,
	lengths: Vec<f32>,
}

impl Path {
	const STEP: f32 = 0.5;

	fn new() -> Self {
		let (from, to) = (TOP - 16.0, TOP + CLIMB + LENGTH + 16.0);
		let mut heights = vec![from];
		let mut lengths = vec![0.0];
		let mut y = from;
		while y < to {
			let next = y + Self::STEP;
			let length = vec2(sway(next) - sway(y), Self::STEP).length();
			lengths.push(lengths.last().copied().unwrap_or(0.0) + length);
			heights.push(next);
			y = next;
		}
		Self { heights, lengths }
	}

	fn length_at(&self, y: f32) -> f32 {
		Self::lerp(&self.heights, &self.lengths, y)
	}

	fn height_at(&self, length: f32) -> f32 {
		Self::lerp(&self.lengths, &self.heights, length)
	}

	/// `to` at `at` along the increasing `from`.
	fn lerp(from: &[f32], to: &[f32], at: f32) -> f32 {
		let index = from
			.partition_point(|value| *value < at)
			.clamp(1, from.len() - 1);
		let (a, b) = (from[index - 1], from[index]);
		let t = ((at - a) / (b - a)).clamp(0.0, 1.0);
		to[index - 1] + (to[index] - to[index - 1]) * t
	}
}

/// The worm at one moment.
struct Pose {
	/// Head first: each point's offset from the window's top centre, its half-width before
	/// any swell, and how far along the body it is.
	body: Vec<(Vec2, f32, f32)>,
	/// How far below the head's top pose the oil reaches, and how strongly it shows.
	oil: (f32, f32),
}

fn pose(path: &Path, t: f32) -> Pose {
	let top = path.length_at(TOP);
	let start = path.length_at(TOP + CLIMB);
	let at = t.rem_euclid(LOOP);
	// Where the head is along the wave, how far the whole body has slipped down, and the oil.
	let (head, slip, oil) = if at < CRAWL {
		let p = at / CRAWL;
		// Mostly steady, easing in and out at the ends.
		let q = 0.5 * (p + smooth(p));
		let head = start - (start - top) * q;
		// The oil from the last slip soaks away early in the climb; the first climb has none.
		let oil = if t >= LOOP {
			(CLIMB, 1.0 - smooth((at / SOAK).min(1.0)))
		} else {
			(0.0, 0.0)
		};
		(head, 0.0, oil)
	} else if at < CRAWL + REST {
		(top, 0.0, (0.0, 0.0))
	} else if at < CRAWL + REST + SLIP {
		// Loses its grip slowly, slides, and slows to a stop where it started, its head
		// oiling the floor it slides over.
		let p = (at - CRAWL - REST) / SLIP;
		let slip = CLIMB * smooth(smooth(p));
		(top, slip, (slip, 1.0))
	} else {
		(top, CLIMB, (CLIMB, 1.0))
	};
	let body = (0..SAMPLES)
		.map(|i| {
			let s = i as f32 / (SAMPLES - 1) as f32;
			let y = path.height_at(head + LENGTH * s);
			(vec2(sway(y), y + slip), width(s), s)
		})
		.collect();
	Pose { body, oil }
}

fn width(s: f32) -> f32 {
	let index = WIDTHS
		.partition_point(|(at, _)| *at < s)
		.clamp(1, WIDTHS.len() - 1);
	let (a, b) = (WIDTHS[index - 1], WIDTHS[index]);
	a.1 + (b.1 - a.1) * smooth(((s - a.0) / (b.0 - a.0)).clamp(0.0, 1.0))
}

/// The line of oil the worm's head left as it slipped, from its top pose to `reach` below.
/// Its edges are soft and uneven like a spill, glints catch the light along it, the older
/// end is fainter, and it narrows as it soaks away.
fn oil(
	painter: &egui::Painter,
	origin: Pos2,
	reach: f32,
	strength: f32,
	accent: Color32,
	alpha: f32,
) {
	const STEP: f32 = 1.5;
	/// How much oil shows across the film, from its left edge (0) to its right (1).
	const ACROSS: [(f32, f32); 5] = [
		(0.0, 0.7),
		(0.07, 1.0),
		(0.5, 0.85),
		(0.93, 0.9),
		(1.0, 0.6),
	];
	let amount = |u: f32| {
		let index = ACROSS
			.partition_point(|(at, _)| *at < u)
			.clamp(1, ACROSS.len() - 1);
		let (a, b) = (ACROSS[index - 1], ACROSS[index]);
		a.1 + (b.1 - a.1) * ((u - a.0) / (b.0 - a.0)).clamp(0.0, 1.0)
	};
	let shown = strength * alpha;
	if reach < 0.5 || shown <= 0.0 {
		return;
	}
	// The slip is straight down, so the head sweeps a straight band.
	let (x, half) = (sway(TOP), width(0.0) * FILM * (0.6 + 0.4 * strength));
	let (from, to) = (TOP - half, TOP + reach + half);
	let base = accent.gamma_multiply(0.26 * shown);
	let gloss =
		ui::design::mix(accent.to_opaque(), Color32::WHITE, 0.4).gamma_multiply(0.6 * shown);
	// A feather, the lit rim, the film's stops around a glint streak of three, a feather.
	const COLUMNS: u32 = 10;
	let mut mesh = egui::Mesh::default();
	let mut rows = 0;
	let mut y = from;
	while y <= to + STEP {
		let row = y.min(to);
		y += STEP;
		// Where the head's circle, swept down by `reach`, crosses this row.
		let off = (TOP - row).max(row - TOP - reach).max(0.0);
		let across = (half * half - off * off).max(0.0).sqrt();
		let (mut left, mut right) = (x - across, x + across);
		// Uneven edges in gentle bulges, narrowing to nothing at the ends.
		let edge = ((right - left) / 6.0).min(1.0);
		left += edge * (1.5 * (row * 0.13 + 1.3).sin() + 0.5 * (row * 0.31).sin());
		right += edge * (1.5 * (row * 0.11 + 4.1).sin() + 0.5 * (row * 0.29 + 2.0).sin());
		let right = right.max(left);
		// The newest oil, next to the head, shows most.
		let fresh = 0.5 + 0.5 * smooth(((row - from) / (to - from)).clamp(0.0, 1.0));
		let streak = 0.33 + 0.06 * (row * 0.09).sin();
		let glint = (0.5 + 0.5 * (row * 0.11 + 0.7).sin()) * (0.5 + 0.5 * (row * 0.29 + 2.1).sin());
		let point = |u: f32| origin + vec2(left + (right - left) * u, row);
		let film = |u: f32| base.gamma_multiply(amount(u) * fresh);
		mesh.colored_vertex(point(0.0) - vec2(1.0, 0.0), Color32::TRANSPARENT);
		// The edge toward the light catches it, as does a broken streak along the film.
		for (u, lit) in [
			(0.0, 0.5),
			(0.07, 0.0),
			(streak - 0.06, 0.0),
			(streak, glint),
			(streak + 0.06, 0.0),
			(0.5, 0.0),
			(0.93, 0.0),
			(1.0, 0.0),
		] {
			// The light over the film, both premultiplied.
			let (lit, under) = (gloss.gamma_multiply(lit * fresh), film(u));
			let cover = 1.0 - f32::from(lit.a()) / 255.0;
			let over = |a: u8, b: u8| (f32::from(a) + f32::from(b) * cover).round() as u8;
			mesh.colored_vertex(
				point(u),
				Color32::from_rgba_premultiplied(
					over(lit.r(), under.r()),
					over(lit.g(), under.g()),
					over(lit.b(), under.b()),
					over(lit.a(), under.a()),
				),
			);
		}
		mesh.colored_vertex(point(1.0) + vec2(1.0, 0.0), Color32::TRANSPARENT);
		if rows > 0 {
			let (above, here) = ((rows - 1) * COLUMNS, rows * COLUMNS);
			for column in 0..COLUMNS - 1 {
				mesh.add_triangle(above + column, above + column + 1, here + column);
				mesh.add_triangle(above + column + 1, here + column + 1, here + column);
			}
		}
		rows += 1;
	}
	painter.add(egui::Shape::mesh(mesh));
}

/// The Leech worm, seen from above. It crawls up a wave toward the top edge like a snake,
/// rests a moment, then slowly slips back down the same wave, as if on a wet floor, to where
/// it started, its head leaving a line of oil that soaks away as it climbs again. The eye
/// blinks now and then. While an update installs (`fill` is `Some`), drops fall onto its head
/// as it goes and each gulp carries the colour further down its body, which fills from head
/// to tail with the progress.
#[allow(clippy::too_many_arguments)]
fn worm(
	painter: &egui::Painter,
	origin: Pos2,
	t: f32,
	fill: Option<f32>,
	accent: Color32,
	canvas: Color32,
	dark: bool,
	alpha: f32,
) {
	const DRIP: f32 = 0.95;
	let path = Path::new();
	let pose = pose(&path, t);
	// Only a gulp swells the body.
	let swell = |s: f32| -> f32 {
		match fill {
			None => 0.0,
			Some(_) => (0..2)
				.map(|back| {
					// Each drop lands at 0.55 of its cycle and its gulp travels for a cycle.
					let phase = (t / DRIP).fract() + back as f32 - 0.55;
					if !(0.0..1.0).contains(&phase) {
						return 0.0;
					}
					0.18 * (1.0 - phase) * (-((s - phase * 1.1) / 0.08).powi(2)).exp()
				})
				.sum(),
		}
	};
	// A gulp dips the head.
	let dip = match fill {
		None => 0.0,
		Some(_) => {
			let phase = (t / DRIP).fract();
			let land = ((phase - 0.5) / 0.08).clamp(0.0, 1.0);
			let rise = ((phase - 0.58) / 0.3).clamp(0.0, 1.0);
			2.0 * smooth(land) * (1.0 - smooth(rise))
		}
	};
	let breath = 1.0 + 0.02 * (t * 2.0).sin();
	let points: Vec<(Pos2, f32, f32)> = pose
		.body
		.iter()
		.map(|&(at, half, s)| {
			let at = at + vec2(0.0, dip * (1.0 - s).powi(3));
			(origin + at, half * breath * (1.0 + swell(s)), s)
		})
		.collect();

	let shade = |color: Color32| color.gamma_multiply(alpha);
	glow(painter, points[SAMPLES / 2].0, accent, dark, t, alpha);
	oil(painter, origin, pose.oil.0, pose.oil.1, accent, alpha);
	let body_color = match fill {
		None => accent,
		Some(_) => ui::design::mix(canvas.to_opaque(), accent.to_opaque(), 0.26),
	};
	// Tail first, so the head end stays on top.
	for &(point, radius, _) in points.iter().rev() {
		painter.circle_filled(point, radius, shade(body_color));
	}
	if let Some(fill) = fill {
		// The colour's front soaks in over a short stretch instead of ending in a hard cap.
		const FRONT: f32 = 0.08;
		// Past the tail at full progress, so the tip fills too.
		let reached = fill.clamp(0.0, 1.0) * (1.0 + FRONT);
		for &(point, radius, s) in points.iter().rev() {
			if s <= reached {
				let depth = smooth(((reached - s) / FRONT).min(1.0));
				painter.circle_filled(
					point,
					radius,
					shade(ui::design::mix(body_color, accent, depth)),
				);
			}
		}
		// The drop: falls onto the head, then sinks in and lights it from inside.
		let phase = (t / DRIP).fract();
		let (head, radius, _) = points[0];
		let top = head - vec2(0.0, radius);
		if phase < 0.55 {
			let fall = phase / 0.55;
			let start = top - vec2(0.0, 44.0);
			let at = start + (top - start) * (fall * fall);
			let appear = (fall / 0.2).min(1.0);
			let stretch = 1.0 + 0.5 * fall * fall;
			painter.add(egui::Shape::ellipse_filled(
				at,
				vec2(4.0 / stretch.sqrt(), 4.0 * stretch),
				shade(accent.gamma_multiply(appear)),
			));
		} else if phase < 0.85 {
			let sink = (phase - 0.55) / 0.3;
			let lit = ui::design::mix(accent.to_opaque(), Color32::WHITE, 0.45);
			radial(
				painter,
				head,
				6.0 + 11.0 * sink,
				shade(lit.gamma_multiply(0.7 * (1.0 - sink))),
			);
		}
	}
	// The eye sits toward the front of the head and blinks every few seconds.
	let (head, radius, _) = points[0];
	let ahead = (head - points[3].0).normalized();
	let blink = {
		let phase = (t % 3.7) / 0.16;
		if phase < 1.0 {
			(phase * TAU / 2.0).sin()
		} else {
			0.0
		}
	};
	let eye = radius * 0.42;
	painter.add(egui::Shape::ellipse_filled(
		head + ahead * radius * 0.18,
		vec2(eye, (eye * (1.0 - 0.9 * blink)).max(0.6)),
		shade(canvas.to_opaque()),
	));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_worm_loops_inside_its_band_and_finished_splashes_fade_out() {
		let path = Path::new();
		// The slip lands the body exactly where the next climb starts.
		let start = pose(&path, 0.0).body;
		let settled = pose(&path, LOOP - 0.01).body;
		for (a, b) in start.iter().zip(&settled) {
			assert!((a.0 - b.0).length() < 0.6, "{:?} {:?}", a.0, b.0);
		}
		// It stays between the top edge and the title, and keeps its length on the bends.
		for frame in 0..400 {
			let pose = pose(&path, frame as f32 * LOOP / 400.0);
			for &(at, half, _) in &pose.body {
				assert!(at.y - half > 16.0 && at.y + half < 222.0 && at.x.abs() < 40.0);
			}
			let length: f32 = pose
				.body
				.windows(2)
				.map(|pair| (pair[1].0 - pair[0].0).length())
				.sum();
			assert!((length - LENGTH).abs() < 1.0, "{length}");
		}
		// The oil is laid by the slip and soaks away early in the next climb, never the first.
		let oil = |t: f32| pose(&path, t).oil;
		assert_eq!(oil(1.0).1, 0.0);
		assert!((oil(LOOP - 0.01).0 - CLIMB).abs() < 0.01 && oil(LOOP - 0.01).1 == 1.0);
		assert!(oil(LOOP + 0.01).1 > 0.99 && oil(LOOP + SOAK).1 == 0.0);
		assert!(oil(CRAWL + REST + SLIP * 0.5).0 < CLIMB);

		let ctx = egui::Context::default();
		let mut splash = Splash::new(0);
		splash.started -= SHORTEST;
		let mut finished = false;
		for frame in 0..240 {
			let input = egui::RawInput {
				time: Some(frame as f64 / 60.0),
				predicted_dt: 1.0 / 60.0,
				screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, SIZE.into())),
				..Default::default()
			};
			let mut output = ctx.run_ui(input, |ui| {
				finished |= splash.show(
					ui,
					&Look::Starting {
						stage: None,
						done: true,
					},
				);
			});
			output.textures_delta.clear();
		}
		assert!(finished, "a finished splash fades out within four seconds");
	}
}
