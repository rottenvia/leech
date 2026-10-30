//! The compact launch window. Leech opens as a 300 by 350 splash and grows to its saved
//! size once the account is ready or the sign-in screen is needed; an update found at launch
//! installs here instead, the mark drinking it in.
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

/// The mark's centre line, a cubic through these points, and its radius along that line, at
/// a third and two thirds of its length. Fitted to the 256 px app icon (98% overlap).
const SPINE: [Vec2; 4] = [
	vec2(93.0, 83.1),
	vec2(81.2, 159.9),
	vec2(111.5, 210.3),
	vec2(185.8, 171.5),
];
const RADII: [f32; 4] = [37.8, 29.3, 21.0, 14.1];
const EYE: Vec2 = vec2(93.1, 82.7);
const EYE_RADIUS: f32 = 15.7;
/// The icon's mark spans x 56 to 200 and y 45 to 205.
const MARK_CENTER: Vec2 = vec2(128.0, 125.0);
const SAMPLES: usize = 84;

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
		let center = pos2(rect.center().x, rect.top() + 138.0);
		let t = time as f32;
		glow(
			painter,
			center,
			palette.accent,
			ui.visuals().dark_mode,
			t,
			alpha,
		);
		mark(
			painter,
			center,
			0.66,
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

fn cubic(spine: &[Vec2; 4], t: f32) -> Vec2 {
	let u = 1.0 - t;
	spine[0] * (u * u * u)
		+ spine[1] * (3.0 * u * u * t)
		+ spine[2] * (3.0 * u * t * t)
		+ spine[3] * (t * t * t)
}

/// Ease out with a small overshoot, so a reach stretches a little past rest and settles.
fn overshoot(t: f32) -> f32 {
	const BACK: f32 = 0.9;
	let t = t - 1.0;
	1.0 + (BACK + 1.0) * t * t * t + BACK * t * t
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

/// The Leech mark, alive. While Leech starts it crawls on the spot: the tail lifts and pulls
/// in from right to left, bunching the body, which thickens and lifts the head while a swell
/// rides up to it; then the tail reaches back out along the ground, a little past rest, and
/// settles. The eye blinks now and then. While an update installs (`fill` is `Some`), drops
/// fall into its head and each gulp carries the colour further down its body, which fills
/// from head to tail with the progress.
#[allow(clippy::too_many_arguments)]
fn mark(
	painter: &egui::Painter,
	center: Pos2,
	scale: f32,
	t: f32,
	fill: Option<f32>,
	accent: Color32,
	canvas: Color32,
	dark: bool,
	alpha: f32,
) {
	let place = |point: Vec2| center + (point - MARK_CENTER) * scale;
	// One crawl: the tail pulls in, holds, then reaches back out. Gulps follow the drops.
	const CRAWL: f32 = 1.6;
	const PULL: f32 = 0.42;
	const HOLD: f32 = 0.5;
	const DRIP: f32 = 0.95;
	let phase = (t / CRAWL).fract();
	// How far the tail has pulled in (1) and how high it is lifted while moving.
	let (bunch, lift) = match fill {
		Some(_) => (0.0, 0.0),
		None if phase < PULL => (smooth(phase / PULL), (phase / PULL * TAU / 2.0).sin()),
		None if phase < HOLD => (1.0, 0.0),
		None => (1.0 - overshoot((phase - HOLD) / (1.0 - HOLD)), 0.0),
	};
	let swell = |s: f32| -> f32 {
		match fill {
			// The pull's swell rides from the tail up to the head.
			None => {
				if phase > PULL + 0.12 {
					return 0.0;
				}
				let at = 1.0 - phase / (PULL + 0.12);
				0.09 * (-((s - at) / 0.1).powi(2)).exp()
			}
			Some(_) => (0..2)
				.map(|back| {
					// Each drop lands at 0.55 of its cycle and its gulp travels for a cycle.
					let phase = (t / DRIP).fract() + back as f32 - 0.55;
					if !(0.0..1.0).contains(&phase) {
						return 0.0;
					}
					0.13 * (1.0 - phase) * (-((s - phase * 1.1) / 0.08).powi(2)).exp()
				})
				.sum(),
		}
	};
	// The bunched body lifts the head; a gulp dips it.
	let reach = match fill {
		None => bunch,
		Some(_) => {
			let phase = (t / DRIP).fract();
			let land = ((phase - 0.5) / 0.08).clamp(0.0, 1.0);
			let rise = ((phase - 0.58) / 0.3).clamp(0.0, 1.0);
			-0.6 * smooth(land) * (1.0 - smooth(rise))
		}
	};
	let head_shift = vec2(1.0, -3.5) * reach;
	let breath = 1.0 + 0.018 * (t * 2.0).sin();

	// The tail slides in along its own curve, so the bend keeps its round inner corner.
	let end = 1.0 - 0.24 * bunch;
	let mut spine = [(Vec2::ZERO, 0.0f32); SAMPLES];
	let mut length = [0.0f32; SAMPLES];
	for (i, sample) in spine.iter_mut().enumerate() {
		let u = i as f32 / (SAMPLES - 1) as f32;
		sample.0 = cubic(&SPINE, u * end);
		if i > 0 {
			length[i] = length[i - 1]
				+ (sample.0 - cubic(&SPINE, (i - 1) as f32 / (SAMPLES - 1) as f32 * end)).length();
		}
	}
	let total = length[SAMPLES - 1];
	let points: Vec<(Pos2, f32, f32)> = spine
		.iter()
		.zip(length)
		.map(|(&(point, _), length)| {
			let s = length / total;
			// A bunched body is shorter, so its lower half thickens.
			let thicken = 1.0 + 0.06 * bunch * ((s - 0.3) / 0.4).clamp(0.0, 1.0);
			let radius = interpolate(s) * breath * thicken * (1.0 + swell(s));
			// The head end follows the reach; the tail lifts while it moves.
			let pull = (1.0 - s).powi(2);
			let raise = vec2(0.0, -6.0 * lift * s.powi(3));
			(place(point + head_shift * pull + raise), radius * scale, s)
		})
		.collect();

	let shade = |color: Color32| color.gamma_multiply(alpha);
	// A soft shadow ties the mark to the ground.
	let ground = place(vec2(128.0 - 14.0 * bunch, 212.0));
	painter.add(egui::Shape::ellipse_filled(
		ground,
		vec2(46.0 * scale * (1.0 - 0.2 * bunch), 5.0 * scale),
		Color32::BLACK.gamma_multiply(if dark { 0.16 } else { 0.08 } * alpha),
	));
	let body = match fill {
		None => accent,
		Some(_) => ui::design::mix(canvas.to_opaque(), accent.to_opaque(), 0.26),
	};
	for &(point, radius, _) in &points {
		painter.circle_filled(point, radius, shade(body));
	}
	if let Some(fill) = fill {
		// The colour's front soaks in over a short stretch instead of ending in a hard cap.
		// Painted tail first, so the fuller circles nearer the head stay on top.
		const FRONT: f32 = 0.08;
		// Past the tail at full progress, so the tip fills too.
		let reached = fill.clamp(0.0, 1.0) * (1.0 + FRONT);
		for &(point, radius, s) in points.iter().rev() {
			if s <= reached {
				let depth = smooth(((reached - s) / FRONT).min(1.0));
				painter.circle_filled(point, radius, shade(ui::design::mix(body, accent, depth)));
			}
		}
		// The drop: falls onto the head, then sinks in with a ripple.
		let phase = (t / DRIP).fract();
		let head = points[0];
		let top = head.0 - vec2(0.0, head.1);
		if phase < 0.55 {
			let fall = phase / 0.55;
			let start = top - vec2(0.0, 62.0);
			let at = start + (top - start) * (fall * fall);
			let appear = (fall / 0.2).min(1.0);
			let stretch = 1.0 + 0.5 * fall * fall;
			painter.add(egui::Shape::ellipse_filled(
				at,
				vec2(4.4 / stretch.sqrt(), 4.4 * stretch),
				shade(accent.gamma_multiply(appear)),
			));
		} else if phase < 0.85 {
			// Sinking in, the drop lights the head from inside.
			let sink = (phase - 0.55) / 0.3;
			let lit = ui::design::mix(accent.to_opaque(), Color32::WHITE, 0.45);
			radial(
				painter,
				top + vec2(0.0, 6.0 + 10.0 * sink),
				(7.0 + 12.0 * sink) * scale / 0.66,
				shade(lit.gamma_multiply(0.7 * (1.0 - sink))),
			);
		}
	}
	// The eye moves with the head and blinks every few seconds.
	let eye = place(EYE + head_shift);
	let blink = {
		let phase = (t % 3.7) / 0.16;
		if phase < 1.0 {
			(phase * TAU / 2.0).sin()
		} else {
			0.0
		}
	};
	painter.add(egui::Shape::ellipse_filled(
		eye,
		vec2(EYE_RADIUS, (EYE_RADIUS * (1.0 - 0.9 * blink)).max(1.2)) * scale,
		shade(canvas.to_opaque()),
	));
}

fn interpolate(s: f32) -> f32 {
	let at = (s * 3.0).clamp(0.0, 3.0);
	let index = (at as usize).min(2);
	RADII[index] + (RADII[index + 1] - RADII[index]) * (at - index as f32)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_mark_spans_the_icon_and_finished_splashes_fade_out() {
		// Head and tail caps reach the icon's mark bounds within a few icon pixels.
		let head = cubic(&SPINE, 0.0);
		let tail = cubic(&SPINE, 1.0);
		assert!((head.y - RADII[0] - 45.0).abs() < 3.0);
		assert!((tail.x + RADII[3] - 200.0).abs() < 3.0);
		assert_eq!(interpolate(0.0), RADII[0]);
		assert_eq!(interpolate(1.0), RADII[3]);

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
