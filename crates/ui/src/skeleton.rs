//! Loading placeholders. Each surface waits in its own shape and motion, so the wait already
//! reads as that surface arriving: a channel's messages rise from the composer under a
//! sweeping light, a direct conversation fills in turn by turn, an app's embeds unfold, forum
//! posts drop in as cards, search results slide in from the pane's edge and people fill the
//! member list.
//!
//! None shows for a wait shorter than [`GRACE`], and none is cut off when its content comes:
//! its last frame fades out under the content while the content fades in.
use crate::design;
use egui::{Color32, Painter, Rect, pos2, vec2};

/// Waits shorter than this show no placeholder: one that flashes up and is cut off at once
/// reads as unfinished.
pub const GRACE: f32 = 0.15;
/// How long a placeholder takes to hand over to its content.
const HANDOFF: f32 = 0.22;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
	/// A server channel: message groups with names, now and then an attachment.
	Channel,
	/// A direct or group conversation: short replies taking turns, some with reactions.
	Direct,
	/// A conversation with an app: replies carry embed cards and buttons.
	App,
	/// Forum posts: cards with a title, a snippet, tags and sometimes a thumbnail.
	Forum,
	/// Search results and pins: message cards under their channel.
	Results,
	/// The member list: a heading over rows of people.
	People,
}

#[derive(Clone, Copy)]
struct Record {
	kind: Kind,
	rect: Rect,
	start: f64,
	pass: u64,
	handoff: Option<f64>,
}

/// Takes the rest of the space for a loading `kind` placeholder, as [`show`] draws it.
pub fn fill(ui: &mut egui::Ui, id: egui::Id, kind: Kind, label: &str) {
	let (rect, response) = ui.allocate_exact_size(
		vec2(ui.available_width(), ui.available_height().max(0.0)),
		egui::Sense::hover(),
	);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, false, label));
	show(ui, id, kind, rect, true);
}

/// Draws the `kind` placeholder in `rect` while `loading`, and its hand-off once it is not.
/// `id` names the surface and what it loads (a channel, a forum), so going elsewhere starts
/// afresh. Returns the opacity for the content: 0 while loading, then rising from a quarter
/// to 1 as the placeholder fades, so the content is never drawn invisible.
pub fn show(ui: &egui::Ui, id: egui::Id, kind: Kind, rect: Rect, loading: bool) -> f32 {
	let now = ui.input(|input| input.time);
	let pass = ui.ctx().cumulative_pass_nr();
	let record = ui.data(|data| data.get_temp::<Record>(id));
	// A placeholder carries on only from the previous pass; an older one was left behind.
	let current = record.filter(|record| record.handoff.is_some() || record.pass + 1 >= pass);
	if loading {
		let start = current
			.filter(|record| record.handoff.is_none())
			.map_or(now, |record| record.start);
		ui.data_mut(|data| {
			data.insert_temp(
				id,
				Record {
					kind,
					rect,
					start,
					pass,
					handoff: None,
				},
			);
		});
		paint(ui, rect, kind, (now - start) as f32, 1.0);
		return 0.0;
	}
	let Some(mut record) = current else {
		if record.is_some() {
			ui.data_mut(|data| data.remove::<Record>(id));
		}
		return 1.0;
	};
	let handoff = *record.handoff.get_or_insert(now);
	let t = ((now - handoff) as f32 / HANDOFF).min(1.0);
	if t >= 1.0 {
		ui.data_mut(|data| data.remove::<Record>(id));
		return 1.0;
	}
	ui.data_mut(|data| data.insert_temp(id, record));
	ui.ctx().request_repaint();
	let eased = egui::emath::easing::cubic_out(t);
	// The placeholder stops where it was and fades; the light keeps moving through it.
	paint(
		ui,
		record.rect,
		record.kind,
		(handoff - record.start) as f32,
		1.0 - eased,
	);
	0.25 + 0.75 * eased
}

fn paint(ui: &egui::Ui, rect: Rect, kind: Kind, elapsed: f32, opacity: f32) {
	ui.ctx().request_repaint();
	let elapsed = elapsed - GRACE;
	if elapsed <= 0.0 || opacity <= 0.0 || !ui.is_rect_visible(rect) {
		return;
	}
	let mut painter = ui.painter().with_clip_rect(ui.clip_rect().intersect(rect));
	painter.set_opacity(opacity);
	match kind {
		Kind::Channel => channel(ui, &painter, rect, elapsed),
		Kind::Direct => direct(ui, &painter, rect, elapsed),
		Kind::App => app(ui, &painter, rect, elapsed),
		Kind::Forum => forum(ui, &painter, rect, elapsed),
		Kind::Results => results(ui, &painter, rect, elapsed),
		Kind::People => people(ui, &painter, rect, elapsed),
	}
}

/// How far (0 to 1) part `index` has arrived, `elapsed` seconds in: parts follow one another
/// every `gap` seconds and each takes `length`.
fn arrive(elapsed: f32, index: usize, gap: f32, length: f32) -> f32 {
	egui::emath::easing::cubic_out(((elapsed - index as f32 * gap) / length).clamp(0.0, 1.0))
}

fn bar(painter: &Painter, min: egui::Pos2, size: egui::Vec2, fill: Color32) {
	painter.rect_filled(Rect::from_min_size(min, size), size.y * 0.5, fill);
}

/// Placeholder ink lit by `lit` (0 to 1), as [`design::shimmer`] colours it.
fn glow(ui: &egui::Ui, lit: f32, strength: f32) -> Color32 {
	let p = design::palette(ui);
	let light = design::mix(p.text_strong, p.accent, 0.25);
	design::mix(p.muted, light, 0.55 * lit).gamma_multiply(strength * (1.0 + 0.7 * lit))
}

/// Message rows put the avatar at 16 and the text at 72; lines stop where a reader's eye
/// would, as real paragraphs rarely span a very wide window.
fn text_column(rect: Rect, cap: f32) -> (f32, f32) {
	let left = rect.left() + 72.0;
	(left, (rect.right() - 16.0 - left).clamp(0.0, cap))
}

/// A server channel. The timeline is anchored to the composer, so groups rise in one after
/// another from there, where arriving messages will land, while a light sweeps up through
/// them.
fn channel(ui: &egui::Ui, painter: &Painter, rect: Rect, elapsed: f32) {
	// Name width, line widths as a share of the column, and an attachment: a fixed table, so
	// the placeholder reads like a real conversation and never changes between frames.
	const GROUPS: [(f32, &[f32], bool); 8] = [
		(112.0, &[0.62], false),
		(84.0, &[0.9, 0.48], false),
		(136.0, &[0.34], true),
		(96.0, &[0.76, 0.85, 0.3], false),
		(72.0, &[0.55], false),
		(124.0, &[0.82, 0.64], false),
		(104.0, &[0.4], false),
		(90.0, &[0.7, 0.92, 0.58], false),
	];
	let shimmer = design::shimmer(ui, rect, true);
	let (left, column) = text_column(rect, 760.0);
	let mut bottom = rect.bottom() - 12.0;
	for (index, (name, lines, attachment)) in GROUPS.iter().cycle().take(64).enumerate() {
		let block = 24.0 + lines.len() as f32 * 22.0 + if *attachment { 152.0 } else { 0.0 };
		if bottom < rect.top() {
			break;
		}
		let rise = arrive(elapsed, index, 0.045, 0.3);
		let top = bottom - block + 10.0 * (1.0 - rise);
		bottom -= block + 20.0;
		if rise <= 0.0 {
			continue;
		}
		let ink = |y: f32, strength: f32| shimmer(y, strength).gamma_multiply(rise);
		painter.circle_filled(
			pos2(rect.left() + 36.0, top + 20.0),
			20.0,
			ink(top + 20.0, 0.12),
		);
		bar(
			painter,
			pos2(left, top + 4.0),
			vec2(name.min(column), 12.0),
			ink(top, 0.32),
		);
		bar(
			painter,
			pos2(left + name + 10.0, top + 6.0),
			vec2(44.0, 8.0),
			ink(top, 0.12),
		);
		let mut y = top + 30.0;
		for share in *lines {
			let width = (column * share).max(24.0).min(column);
			bar(painter, pos2(left, y), vec2(width, 10.0), ink(y, 0.2));
			y += 22.0;
		}
		if *attachment {
			painter.rect_filled(
				Rect::from_min_size(pos2(left, y - 4.0), vec2(column.min(280.0), 140.0)),
				8.0,
				ink(y + 66.0, 0.12),
			);
		}
	}
}

/// A direct conversation: two people's short replies, each sliding in from the left in turn,
/// while a light passes from reply to reply like the conversation itself.
fn direct(ui: &egui::Ui, painter: &Painter, rect: Rect, elapsed: f32) {
	// Theirs or yours, line widths and whether reactions sit under it.
	const TURNS: [(bool, &[f32], bool); 8] = [
		(true, &[0.42, 0.26], false),
		(false, &[0.3], true),
		(true, &[0.55], false),
		(false, &[0.22, 0.48, 0.34], false),
		(true, &[0.36], true),
		(false, &[0.5, 0.28], false),
		(true, &[0.24, 0.4], false),
		(false, &[0.46], false),
	];
	let p = design::palette(ui);
	let time = ui.input(|input| input.time);
	// The light settles on one reply after another, upwards from the newest.
	let turn = ((time * 2.2) % 11.0) as f32 - 1.5;
	let (left, column) = text_column(rect, 560.0);
	let mut bottom = rect.bottom() - 12.0;
	for (index, (theirs, lines, reactions)) in TURNS.iter().cycle().take(64).enumerate() {
		let block = 24.0 + lines.len() as f32 * 20.0 + if *reactions { 28.0 } else { 0.0 };
		if bottom < rect.top() {
			break;
		}
		let rise = arrive(elapsed, index, 0.07, 0.34);
		let top = bottom - block;
		bottom -= block + 18.0;
		if rise <= 0.0 {
			continue;
		}
		let lit = (-(index as f32 - turn).powi(2) / 0.9).exp();
		let ink = |strength: f32| glow(ui, lit, strength).gamma_multiply(rise);
		let shift = vec2(-18.0 * (1.0 - rise), 0.0);
		// Two speakers: theirs carries a hint of the accent, so the turns read apart.
		let face = if *theirs {
			design::mix(ink(0.14), p.accent.gamma_multiply(0.22 * rise), 0.5)
		} else {
			ink(0.12)
		};
		painter.circle_filled(pos2(rect.left() + 36.0, top + 20.0) + shift, 20.0, face);
		let name = if *theirs { 78.0 } else { 58.0 };
		bar(
			painter,
			pos2(left, top + 4.0) + shift,
			vec2(name, 12.0),
			ink(0.3),
		);
		bar(
			painter,
			pos2(left + name + 10.0, top + 6.0) + shift,
			vec2(36.0, 8.0),
			ink(0.1),
		);
		let mut y = top + 30.0;
		for share in *lines {
			let width = (column * share).max(24.0).min(column);
			bar(painter, pos2(left, y) + shift, vec2(width, 10.0), ink(0.2));
			y += 20.0;
		}
		if *reactions {
			let mut x = left;
			for width in [44.0, 36.0] {
				let pill = Rect::from_min_size(pos2(x, y) + shift, vec2(width, 22.0));
				painter.rect_filled(pill, 8.0, ink(0.08));
				painter.circle_filled(pos2(pill.left() + 12.0, pill.center().y), 6.0, ink(0.22));
				x += width + 6.0;
			}
		}
	}
}

/// A conversation with an app: each reply's embed unfolds from its accent stripe, then its
/// buttons pop in under it.
fn app(ui: &egui::Ui, painter: &Painter, rect: Rect, elapsed: f32) {
	// Line widths, rows of fields and buttons per reply.
	const REPLIES: [(&[f32], usize, usize); 4] = [
		(&[0.7, 0.45], 2, 2),
		(&[0.55], 0, 3),
		(&[0.8, 0.62, 0.3], 1, 0),
		(&[0.5], 0, 2),
	];
	const BUTTONS: [f32; 3] = [96.0, 116.0, 84.0];
	let p = design::palette(ui);
	let time = ui.input(|input| input.time);
	let shimmer = design::shimmer(ui, rect, true);
	let (left, column) = text_column(rect, 432.0);
	let mut bottom = rect.bottom() - 12.0;
	for (index, (lines, fields, buttons)) in REPLIES.iter().cycle().take(32).enumerate() {
		let card = 40.0 + lines.len() as f32 * 20.0 + *fields as f32 * 40.0;
		let row = if *buttons > 0 { 38.0 } else { 0.0 };
		let block = 28.0 + card + row;
		if bottom < rect.top() {
			break;
		}
		let start = index as f32 * 0.16;
		let rise = arrive(elapsed - start, 0, 0.0, 0.25);
		let top = bottom - block;
		bottom -= block + 22.0;
		if rise <= 0.0 {
			continue;
		}
		let ink = |y: f32, strength: f32| shimmer(y, strength).gamma_multiply(rise);
		painter.circle_filled(
			pos2(rect.left() + 36.0, top + 20.0),
			20.0,
			ink(top + 20.0, 0.12),
		);
		bar(
			painter,
			pos2(left, top + 4.0),
			vec2(92.0, 12.0),
			ink(top, 0.32),
		);
		// The app tag beside the name, in the accent.
		painter.rect_filled(
			Rect::from_min_size(pos2(left + 100.0, top + 3.0), vec2(30.0, 14.0)),
			4.0,
			p.accent.gamma_multiply(0.35 * rise),
		);
		bar(
			painter,
			pos2(left + 138.0, top + 6.0),
			vec2(40.0, 8.0),
			ink(top, 0.12),
		);
		let unfold = arrive(elapsed - start - 0.1, 0, 0.0, 0.36);
		if unfold <= 0.0 {
			continue;
		}
		let frame = Rect::from_min_size(pos2(left, top + 28.0), vec2(column, card * unfold));
		painter.rect_filled(frame, 6.0, ink(frame.center().y, 0.07));
		let pulse = 0.55 + 0.25 * ((time * 2.6) as f32 - index as f32).sin();
		painter.rect_filled(
			Rect::from_min_size(frame.min, vec2(4.0, frame.height())),
			egui::CornerRadius {
				nw: 6,
				sw: 6,
				..Default::default()
			},
			p.accent.gamma_multiply(pulse * rise),
		);
		let inner = painter.with_clip_rect(painter.clip_rect().intersect(frame));
		let content = unfold * unfold;
		let inner_ink = |y: f32, strength: f32| ink(y, strength).gamma_multiply(content);
		let x = frame.left() + 16.0;
		let width = (column - 32.0).max(0.0);
		bar(
			&inner,
			pos2(x, frame.top() + 12.0),
			vec2(width * 0.5, 12.0),
			inner_ink(top, 0.3),
		);
		let mut y = frame.top() + 36.0;
		for share in *lines {
			bar(
				&inner,
				pos2(x, y),
				vec2(width * share, 9.0),
				inner_ink(y, 0.18),
			);
			y += 20.0;
		}
		for _ in 0..*fields {
			for side in 0..2 {
				let field = x + side as f32 * width * 0.5;
				bar(
					&inner,
					pos2(field, y + 4.0),
					vec2(56.0, 8.0),
					inner_ink(y, 0.26),
				);
				bar(
					&inner,
					pos2(field, y + 20.0),
					vec2(width * 0.3, 8.0),
					inner_ink(y, 0.16),
				);
			}
			y += 40.0;
		}
		let mut x = left;
		for (button, width) in BUTTONS.iter().take(*buttons).enumerate() {
			let pop = arrive(elapsed - start - 0.38, button, 0.06, 0.22);
			if pop > 0.0 {
				let slot =
					Rect::from_min_size(pos2(x, top + 28.0 + card + 8.0), vec2(*width, 30.0));
				let size = slot.size() * (0.86 + 0.14 * pop);
				painter.rect_filled(
					Rect::from_center_size(slot.center(), size),
					6.0,
					ink(slot.center().y, 0.14).gamma_multiply(pop),
				);
			}
			x += width + 8.0;
		}
	}
}

/// Forum posts: cards drop in one after another down the list under a falling light. `rect`
/// is the list's own column.
fn forum(ui: &egui::Ui, painter: &Painter, rect: Rect, elapsed: f32) {
	// Title width, a thumbnail and the number of tags per card.
	const CARDS: [(f32, bool, usize); 6] = [
		(0.46, true, 3),
		(0.62, false, 2),
		(0.38, true, 1),
		(0.54, false, 3),
		(0.42, false, 2),
		(0.58, true, 2),
	];
	const TAGS: [f32; 3] = [58.0, 74.0, 48.0];
	let shimmer = design::shimmer(ui, rect, false);
	let mut top = rect.top();
	for (index, (title, thumbnail, tags)) in CARDS.iter().cycle().take(32).enumerate() {
		if top > rect.bottom() {
			break;
		}
		let rise = arrive(elapsed, index, 0.06, 0.32);
		let card = Rect::from_min_size(
			pos2(rect.left(), top - 10.0 * (1.0 - rise)),
			vec2(rect.width(), 116.0),
		);
		top += 116.0 + 12.0;
		if rise <= 0.0 {
			continue;
		}
		let ink = |y: f32, strength: f32| shimmer(y, strength).gamma_multiply(rise);
		painter.rect_filled(card, 10.0, ink(card.center().y, 0.06));
		let thumb = if *thumbnail { 96.0 } else { 0.0 };
		let width = (card.width() - 32.0 - thumb).max(0.0);
		let x = card.left() + 16.0;
		let mut tag_x = x;
		for tag in TAGS.iter().take(*tags) {
			bar(
				painter,
				pos2(tag_x, card.top() + 14.0),
				vec2(*tag, 18.0),
				ink(card.top(), 0.14),
			);
			tag_x += tag + 6.0;
		}
		bar(
			painter,
			pos2(x, card.top() + 42.0),
			vec2(width * title, 14.0),
			ink(card.top(), 0.34),
		);
		painter.circle_filled(pos2(x + 8.0, card.top() + 74.0), 8.0, ink(card.top(), 0.16));
		bar(
			painter,
			pos2(x + 22.0, card.top() + 70.0),
			vec2(width * 0.62, 9.0),
			ink(card.top(), 0.18),
		);
		bar(
			painter,
			pos2(x, card.top() + 94.0),
			vec2(36.0, 9.0),
			ink(card.bottom(), 0.14),
		);
		bar(
			painter,
			pos2(x + 48.0, card.top() + 94.0),
			vec2(60.0, 9.0),
			ink(card.bottom(), 0.1),
		);
		if *thumbnail {
			painter.rect_filled(
				Rect::from_min_size(
					pos2(card.right() - 16.0 - 80.0, card.top() + 18.0),
					vec2(80.0, 80.0),
				),
				8.0,
				ink(card.center().y, 0.12),
			);
		}
	}
}

/// Search results and pins: message cards under their channel, sliding in from the pane's
/// outer edge one after another.
fn results(ui: &egui::Ui, painter: &Painter, rect: Rect, elapsed: f32) {
	const CARDS: [(f32, &[f32]); 5] = [
		(0.42, &[0.86, 0.5]),
		(0.3, &[0.64]),
		(0.5, &[0.92, 0.78, 0.4]),
		(0.36, &[0.58]),
		(0.44, &[0.8, 0.46]),
	];
	let shimmer = design::shimmer(ui, rect, false);
	let mut top = rect.top() + 8.0;
	for (index, (name, lines)) in CARDS.iter().cycle().take(32).enumerate() {
		let height = 44.0 + lines.len() as f32 * 18.0;
		if top > rect.bottom() {
			break;
		}
		let rise = arrive(elapsed, index, 0.05, 0.3);
		let shift = vec2(14.0 * (1.0 - rise), 0.0);
		let ink = |y: f32, strength: f32| shimmer(y, strength).gamma_multiply(rise);
		let width = (rect.width() - 16.0).max(0.0);
		bar(
			painter,
			pos2(rect.left() + 8.0, top) + shift,
			vec2(84.0, 9.0),
			ink(top, 0.2),
		);
		let card = Rect::from_min_size(
			pos2(rect.left() + 8.0, top + 18.0) + shift,
			vec2(width, height),
		);
		top += 18.0 + height + 14.0;
		if rise <= 0.0 {
			continue;
		}
		painter.rect_filled(card, 8.0, ink(card.center().y, 0.06));
		painter.circle_filled(
			card.left_top() + vec2(26.0, 26.0),
			16.0,
			ink(card.top(), 0.12),
		);
		let x = card.left() + 52.0;
		let column = (card.width() - 64.0).max(0.0);
		bar(
			painter,
			pos2(x, card.top() + 14.0),
			vec2(column * name, 11.0),
			ink(card.top(), 0.3),
		);
		let mut y = card.top() + 36.0;
		for share in *lines {
			bar(painter, pos2(x, y), vec2(column * share, 9.0), ink(y, 0.18));
			y += 18.0;
		}
	}
}

/// The member list: a heading, then people sliding in from the window's edge, each one's
/// presence dot popping in after the row, under a falling light.
fn people(ui: &egui::Ui, painter: &Painter, rect: Rect, elapsed: f32) {
	// Name width and, for some, the width of an activity line under it.
	const ROWS: [(f32, f32); 12] = [
		(0.62, 0.0),
		(0.44, 0.5),
		(0.7, 0.0),
		(0.52, 0.0),
		(0.38, 0.62),
		(0.66, 0.0),
		(0.48, 0.0),
		(0.58, 0.4),
		(0.4, 0.0),
		(0.68, 0.0),
		(0.5, 0.56),
		(0.36, 0.0),
	];
	let p = design::palette(ui);
	let shimmer = design::shimmer(ui, rect, false);
	let width = (rect.width() - 56.0).max(0.0);
	let heading = arrive(elapsed, 0, 0.0, 0.3);
	bar(
		painter,
		rect.min + vec2(8.0, 8.0),
		vec2(88.0, 8.0),
		shimmer(rect.top() + 12.0, 0.24).gamma_multiply(heading),
	);
	for (index, (name, activity)) in ROWS.iter().enumerate() {
		let rise = arrive(elapsed, index + 1, 0.045, 0.3);
		let top = rect.top() + 26.0 + index as f32 * 42.0;
		if top > rect.bottom() {
			break;
		}
		if rise <= 0.0 {
			continue;
		}
		let shift = vec2(12.0 * (1.0 - rise), 0.0);
		let ink = |strength: f32| shimmer(top + 21.0, strength).gamma_multiply(rise);
		let face = pos2(rect.left() + 24.0, top + 21.0) + shift;
		painter.circle_filled(face, 16.0, ink(0.12));
		let dot = arrive(elapsed, index + 4, 0.045, 0.24);
		if dot > 0.0 {
			let at = face + vec2(11.0, 11.0);
			painter.circle_filled(at, 6.0 * dot, p.sidebar);
			painter.circle_filled(at, 4.0 * dot, ink(0.3));
		}
		let (y, second) = if *activity > 0.0 {
			(top + 11.0, Some(top + 27.0))
		} else {
			(top + 16.0, None)
		};
		bar(
			painter,
			pos2(rect.left() + 48.0, y) + shift,
			vec2(width * name, 10.0),
			ink(0.2),
		);
		if let Some(y) = second {
			bar(
				painter,
				pos2(rect.left() + 48.0, y) + shift,
				vec2(width * activity, 7.0),
				ink(0.12),
			);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pass(ctx: &egui::Context, time: f64, loading: bool) -> f32 {
		let mut shown = 0.0;
		let input = egui::RawInput {
			time: Some(time),
			screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
			..Default::default()
		};
		ctx.run_ui(input, |ui| {
			let rect = ui.max_rect();
			shown = show(ui, egui::Id::unique("test"), Kind::Channel, rect, loading);
		})
		.drop_without_applying_deltas();
		shown
	}

	#[test]
	fn a_placeholder_hands_over_to_its_content_instead_of_vanishing() {
		let ctx = egui::Context::default();
		assert_eq!(pass(&ctx, 0.0, true), 0.0);
		assert_eq!(pass(&ctx, 0.5, true), 0.0);
		let first = pass(&ctx, 0.52, false);
		assert!(first <= 0.25, "the content starts faint: {first}");
		let middle = pass(&ctx, 0.52 + f64::from(HANDOFF) * 0.5, false);
		assert!(middle > first && middle < 1.0, "it fades in: {middle}");
		assert_eq!(pass(&ctx, 0.52 + f64::from(HANDOFF), false), 1.0);
		assert_eq!(pass(&ctx, 1.0, false), 1.0, "and is done");
	}

	#[test]
	fn content_without_a_placeholder_shows_at_once() {
		let ctx = egui::Context::default();
		assert_eq!(pass(&ctx, 0.0, false), 1.0);
	}
}
