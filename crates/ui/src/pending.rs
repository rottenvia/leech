//! Own messages waiting for Discord: dimmed like Discord's optimistic rows, with every
//! attachment shown and one upload strip for the whole batch.
use client_core::{Pending, State};
use egui::RichText;
use model::Delivery;

use crate::{attachments, design, icons};

/// One file of a pending batch; the texture is the composer thumbnail, never re-decoded here.
pub struct UploadFile {
	pub bytes: u64,
	pub preview: Option<egui::TextureHandle>,
}
pub struct Upload {
	pub nonce: String,
	pub files: Vec<UploadFile>,
	/// Bytes handed to HTTP over the whole batch; never a delivery receipt.
	pub progress: Option<(u64, u64)>,
}
impl Upload {
	pub fn bytes(&self) -> u64 {
		self.files.iter().map(|file| file.bytes).sum()
	}
}

/// Attachment tile width and overall card width follow the confirmed-message layout.
const MAX_WIDTH: f32 = 432.0;
/// Smallest the thumbnail area may get and still hold the bar and the cancel button.
const OVERLAY_MIN: egui::Vec2 = egui::vec2(64.0, 48.0);

/// Returns whether the row is still changing size, so the timeline can follow it smoothly
/// instead of re-measuring every frame.
pub fn show(
	ui: &mut egui::Ui,
	pending: &Pending,
	// Whether the row continues a group, and the gap above a new group.
	(compact, gap, irc): (bool, i8, bool),
	state: &State,
	media: (
		&mut crate::avatars::Avatars,
		&mut Option<String>,
		&mut crate::profiles::ProfileSession,
		&mut Option<model::Id>,
		&mut crate::markdown::FormatCache,
	),
	upload: Option<&Upload>,
	(restore, cancel): (&mut Option<String>, &mut bool),
) -> bool {
	let colors = design::palette(ui);
	let (avatars, opening, profile, channel, formats) = media;
	let mut growing = false;
	let compact = compact || irc;
	let upload = upload.filter(|upload| upload.nonce == pending.nonce);
	let sending = pending.delivery == Delivery::Sending;
	let artwork = pending.attachments.len() == 1
		&& attachments::artwork_edge(&pending.attachments[0]).is_some();
	let status = crate::i18n::translate(match pending.delivery {
		Delivery::Sending => "pending-status-sending",
		Delivery::Ambiguous => "pending-status-unknown",
		Delivery::Rejected => "pending-status-not-sent",
		Delivery::Confirmed => "pending-status-sent",
	});
	egui::Frame::NONE
		.inner_margin(egui::Margin {
			left: 16,
			right: 16,
			top: if irc {
				3
			} else if compact {
				1
			} else {
				gap
			},
			bottom: 1,
		})
		.show(ui, |ui| {
			ui.spacing_mut().item_spacing = egui::vec2(16.0, 4.0);
			ui.horizontal_top(|ui| {
				if irc {
					// Match a compact timeline row: a time-wide slot, the author, then the body.
					let body_spacing = ui.spacing().item_spacing.x;
					let line = ui.text_style_height(&egui::TextStyle::Body);
					ui.spacing_mut().item_spacing.x = 8.0;
					let time = ui.painter().layout_no_wrap(
						"00:00".into(),
						egui::FontId::proportional(12.0),
						colors.muted,
					);
					ui.allocate_exact_size(egui::vec2(time.size().x, line), egui::Sense::hover());
					let width = crate::timeline::compact_author_width(ui.available_width());
					crate::timeline::compact_header(
						ui,
						width,
						egui::FontId::new(15.5, design::medium_family(ui.ctx())),
						|ui| {
							ui.set_max_width(width);
							ui.add(
								egui::Label::new(
									design::medium(
										ui,
										state.user.as_ref().map_or_else(
											|| crate::i18n::translate("pending-show-you"),
											|u| u.name.clone(),
										),
										15.5,
									)
									.color(colors.muted),
								)
								.truncate()
								.selectable(false),
							);
						},
					);
					ui.spacing_mut().item_spacing.x = body_spacing;
				} else if compact {
					ui.allocate_exact_size(
						egui::vec2(40.0, crate::timeline::MESSAGE_LINE),
						egui::Sense::hover(),
					);
				} else {
					ui.scope(|ui| {
						ui.set_opacity(0.55);
						if let Some(user) = &state.user {
							avatars.show(ui, user, 40.0, state.demo);
						} else {
							ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
						}
					});
				}
				ui.vertical(|ui| {
					ui.set_width(ui.available_width());
					let mut text_line = egui::Rect::NOTHING;
					if !compact
						|| matches!(pending.delivery, Delivery::Rejected | Delivery::Ambiguous)
					{
						ui.horizontal_wrapped(|ui| {
							ui.spacing_mut().item_spacing.x = 8.0;
							if !compact {
								ui.label(
									design::medium(
										ui,
										state.user.as_ref().map_or_else(
											|| crate::i18n::translate("pending-show-you"),
											|u| u.name.clone(),
										),
										15.5,
									)
									.color(colors.muted),
								);
							}
							ui.label(RichText::new(&status).size(12.0).color(
								if pending.delivery == Delivery::Rejected {
									colors.danger
								} else {
									colors.muted
								},
							));
						});
					}
					if !pending.content.is_empty() {
						ui.scope(|ui| {
							ui.set_opacity(0.55);
							// Pending gray (or failure red) so confirmation visibly replaces the row.
							ui.visuals_mut().override_text_color =
								Some(if pending.delivery == Delivery::Rejected {
									colors.danger
								} else {
									colors.muted
								});
							// Source equality in FormatCache also handles a nonce hash collision.
							let formatted = formats.get(
								model::Id(egui::Id::unique(&pending.nonce).value()),
								&pending.content,
							);
							let id = ui.scope_id().with("spoilers");
							let mut revealed =
								ui.data_mut(|data| data.get_temp::<u32>(id).unwrap_or(0));
							let mut surface = crate::select::Surface::new(ui, "pending-body");
							let jumbo = formatted.jumbo();
							text_line = ui
								.scope(|ui| {
									if jumbo {
										crate::design::jumbo_emoji(ui);
									}
									let source = crate::mentions::MentionSource {
										state,
										channel: pending.channel,
									};
									formatted.show_references(
										ui,
										opening,
										&crate::mentions::known_users(state, pending.channel),
										Some(&source),
										profile,
										(
											&state.channels,
											channel,
											&state.guilds,
											crate::mentions::known_roles(state, pending.channel),
										),
										(avatars, state.demo, &mut revealed),
										&mut surface,
										crate::design::MessageCardSurface::Conversation,
									);
								})
								.response
								.rect;
							surface.finish(ui);
							if revealed != 0 {
								ui.data_mut(|data| data.insert_temp(id, revealed));
							}
						})
						.response
						.on_hover_text(status);
					}
					if let Some(sticker) = &pending.sticker {
						ui.scope(|ui| {
							ui.set_opacity(if sending { 0.55 } else { 1.0 });
							let edge = ui.available_width().min(160.0);
							avatars.sticker_image(ui, sticker, egui::Vec2::splat(edge), state.demo);
						});
					}
					// Pictures alone carry their progress on the image; any file card keeps the strip.
					let pictures_only = upload.is_some_and(|upload| {
						upload.files.len() == pending.attachments.len()
							&& upload.files.iter().all(|file| file.preview.is_some())
					});
					// They grow from small to full size as the upload goes.
					let progress = (sending && !artwork && pictures_only)
						.then(|| eased_progress(ui, &pending.nonce, upload));
					let mut gallery = None;
					if !pending.attachments.is_empty() {
						ui.scope(|ui| {
							ui.set_max_width(ui.available_width().min(MAX_WIDTH));
							// Discord fades the optimistic row until the server echoes it.
							ui.set_opacity(if sending { 0.6 } else { 1.0 });
							gallery = files(ui, pending, upload, progress.map(|(at, _)| at));
						});
					}
					if sending && !artwork {
						// The bar and the cancel button need room; a tiny picture keeps the strip.
						if let Some(gallery) = gallery.filter(|gallery| {
							pictures_only
								&& gallery.width() >= OVERLAY_MIN.x
								&& gallery.height() >= OVERLAY_MIN.y
						}) {
							let (at, moving) = progress.unwrap_or_default();
							if moving {
								growing = true;
								ui.ctx().request_repaint();
							}
							upload_overlay(ui, gallery, at, upload.is_some(), cancel);
						} else if !pending.attachments.is_empty() {
							ui.add_space(2.0);
							ui.scope(|ui| {
								ui.set_max_width(ui.available_width().min(MAX_WIDTH));
								upload_strip(ui, pending, upload, cancel);
							});
						}
					} else if !sending && pending.delivery != Delivery::Confirmed {
						if pending.delivery == Delivery::Ambiguous {
							ui.label(
								RichText::new(crate::i18n::translate(
									"pending-show-check-the-conversation-before-sending-again",
								))
								.small()
								.color(colors.muted),
							);
						}
						if ui
							.button(crate::i18n::translate_if_key(
								&(if pending.sticker.is_some() || pending.content.is_empty() {
									crate::i18n::translate("pending-show-dismiss")
								} else {
									crate::i18n::translate("pending-show-restore-to-composer")
								}),
							))
							.clicked()
						{
							*restore = Some(pending.nonce.clone());
						}
					}
					crate::timeline::fill_header_line(ui, compact, text_line);
				});
			});
		});
	growing
}

/// How far an upload looks along, and whether that is still moving: a critically damped
/// spring toward the bytes sent, so progress arriving in chunks never jumps the bar or the
/// growing pictures, and finishing settles exactly on full.
fn eased_progress(ui: &egui::Ui, nonce: &str, upload: Option<&Upload>) -> (f32, bool) {
	/// Settles a step in about half a second.
	const RATE: f32 = 11.0;
	let target = match upload.and_then(|upload| upload.progress) {
		Some((sent, total)) if total > 0 => (sent as f32 / total as f32).clamp(0.0, 1.0),
		_ => 0.0,
	};
	let id = egui::Id::unique(("upload-progress", nonce));
	let now = ui.input(|input| input.time);
	let (mut at, mut speed, then) = ui
		.data(|data| data.get_temp::<(f32, f32, f64)>(id))
		.unwrap_or((0.0, 0.0, now));
	// By the clock, so a discarded pass that runs the frame again does not step it twice.
	let dt = ((now - then) as f32).clamp(0.0, 0.05);
	speed += (RATE * RATE * (target - at) - 2.0 * RATE * speed) * dt;
	at = (at + speed * dt).clamp(0.0, 1.0);
	let moving = (target - at).abs() > 0.001 || speed.abs() > 0.01;
	if !moving {
		(at, speed) = (target, 0.0);
	}
	ui.data_mut(|data| data.insert_temp(id, (at, speed, now)));
	(at, moving)
}

/// Image thumbnails in the confirmed-message grid, then one card per other file. Returns the
/// area the thumbnails cover. While `progress` is given the thumbnails grow with it, from
/// small to their full size.
fn files(
	ui: &mut egui::Ui,
	pending: &Pending,
	upload: Option<&Upload>,
	progress: Option<f32>,
) -> Option<egui::Rect> {
	let colors = design::palette(ui);
	let file = |index: usize| upload.and_then(|upload| upload.files.get(index));
	let images: Vec<(&str, &egui::TextureHandle)> = pending
		.attachments
		.iter()
		.enumerate()
		.filter_map(|(index, filename)| {
			file(index)
				.and_then(|file| file.preview.as_ref())
				.map(|preview| (filename.as_str(), preview))
		})
		.collect();
	let mut gallery: Option<egui::Rect> = None;
	if !images.is_empty() {
		const GAP: f32 = 6.0;
		let (columns, size) = attachments::image_layout(images.len(), ui.available_width());
		// Each tile at full size, with the scale that fits its picture into a grid cell.
		let tiles: Vec<(egui::Vec2, f32)> = images
			.iter()
			.map(|(filename, texture)| {
				let source = texture.size_vec2();
				let fit = (size.x / source.x).min(size.y / source.y);
				let tile = if let Some(edge) = attachments::artwork_edge(filename) {
					egui::Vec2::splat(edge.min(size.x).min(size.y))
				} else if images.len() > 1 {
					// Grid tiles share one height so rows stay aligned, like Discord.
					size
				} else {
					source * fit.clamp(f32::EPSILON, 1.0)
				};
				(tile, fit)
			})
			.collect();
		let full = tiles.chunks(columns).fold(egui::Vec2::ZERO, |area, row| {
			let width =
				row.iter().map(|(tile, _)| tile.x).sum::<f32>() + GAP * (row.len() - 1) as f32;
			let height = row.iter().map(|(tile, _)| tile.y).fold(0.0, f32::max);
			let gap = if area.y > 0.0 { GAP } else { 0.0 };
			egui::vec2(area.x.max(width), area.y + gap + height)
		});
		// Growing starts at a third of the size, never too small for the bar and the button.
		let grow = match progress {
			Some(at) if full.x >= OVERLAY_MIN.x && full.y >= OVERLAY_MIN.y => {
				let least = (1.0f32 / 3.0)
					.max((OVERLAY_MIN.x + 0.5) / full.x)
					.max((OVERLAY_MIN.y + 0.5) / full.y)
					.min(1.0);
				least + (1.0 - least) * at
			}
			_ => 1.0,
		};
		ui.spacing_mut().item_spacing = egui::Vec2::splat(GAP * grow);
		for (row, tiles) in images.chunks(columns).zip(tiles.chunks(columns)) {
			ui.horizontal_top(|ui| {
				for ((filename, texture), (tile, fit)) in row.iter().zip(tiles) {
					let (rect, _) = ui.allocate_exact_size(*tile * grow, egui::Sense::hover());
					if attachments::artwork_edge(filename).is_none() {
						ui.painter().rect_filled(rect, 8, colors.raised);
					}
					let shown = if images.len() > 1 {
						egui::Rect::from_center_size(
							rect.center(),
							texture.size_vec2() * *fit * grow,
						)
						.intersect(rect)
					} else {
						rect
					};
					ui.put(
						shown,
						egui::Image::from_texture(*texture)
							.fit_to_exact_size(shown.size())
							.corner_radius(8),
					);
					gallery = Some(gallery.map_or(rect, |area| area.union(rect)));
				}
			});
		}
	}
	for (index, filename) in pending.attachments.iter().enumerate() {
		if file(index).is_some_and(|file| file.preview.is_some()) {
			continue;
		}
		let kind = attachments::file_kind(filename, None);
		egui::Frame::new()
			.fill(colors.raised)
			.stroke(egui::Stroke::new(1.0, colors.border))
			.corner_radius(8)
			.inner_margin(egui::Margin::symmetric(12, 10))
			.show(ui, |ui| {
				ui.set_width(ui.available_width());
				ui.horizontal(|ui| {
					ui.spacing_mut().item_spacing.x = 10.0;
					icons::inline(ui, kind.icon(), 32.0, kind.tint(&colors));
					ui.vertical(|ui| {
						ui.spacing_mut().item_spacing.y = 0.0;
						ui.add(
							egui::Label::new(
								design::medium(ui, filename, 14.0).color(colors.text_strong),
							)
							.truncate()
							.selectable(false),
						)
						.on_hover_text(filename);
						if let Some(file) = file(index) {
							ui.add(
								egui::Label::new(
									RichText::new(attachments::format_size(file.bytes))
										.size(12.0)
										.color(colors.muted),
								)
								.selectable(false),
							);
						}
					});
				});
			});
	}
	gallery
}

/// The veil under the overlay's bar and button, the bar's fill and the button's glyph: the
/// loading colours when the theme has them, else a dark veil with the accent.
fn overlay_colors(ui: &egui::Ui) -> (egui::Color32, egui::Color32, egui::Color32) {
	match crate::loading::colors() {
		Some(loading) => (
			loading.surface.gamma_multiply(0.72),
			loading.bar,
			loading.text,
		),
		None => (
			egui::Color32::from_black_alpha(140),
			design::palette(ui).accent,
			egui::Color32::WHITE,
		),
	}
}

/// Progress for an upload of pictures only: a thin bar along the bottom of the thumbnails and a
/// cancel button in their corner, instead of a separate strip.
fn upload_overlay(
	ui: &mut egui::Ui,
	gallery: egui::Rect,
	fraction: f32,
	cancellable: bool,
	cancel: &mut bool,
) {
	let (shade, bar, glyph) = overlay_colors(ui);
	let track = egui::Rect::from_min_max(
		egui::pos2(gallery.left() + 8.0, gallery.bottom() - 12.0),
		egui::pos2(gallery.right() - 8.0, gallery.bottom() - 8.0),
	);
	let painter = ui.painter();
	painter.rect_filled(track, 2, shade);
	if fraction > 0.0 {
		let fill = egui::Rect::from_min_size(
			track.min,
			egui::vec2((track.width() * fraction).max(4.0), track.height()),
		);
		painter.rect_filled(fill, 2, bar);
	}
	if cancellable {
		let button = egui::Rect::from_min_size(
			egui::pos2(gallery.right() - 32.0, gallery.top() + 8.0),
			egui::Vec2::splat(24.0),
		);
		let label = crate::i18n::translate("pending-upload-strip-cancel-upload");
		let response = ui.interact(
			button,
			ui.scope_id().with("cancel-upload"),
			egui::Sense::click(),
		);
		response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
		// Painted here rather than as an icon button, whose glyph follows the light theme's
		// dark text and would vanish on the dark veil.
		let hover = design::hover_motion(ui, &response);
		let press = design::press_motion(ui, &response);
		let painter = ui.painter();
		painter.circle_filled(
			button.center(),
			12.0,
			design::mix(shade, glyph.gamma_multiply(0.9), 0.14 * hover),
		);
		icons::paint(
			painter,
			icons::Icon::Close,
			design::pressed_rect(button.shrink(4.8), press),
			glyph.gamma_multiply(0.8 + 0.2 * hover),
		);
		if response
			.on_hover_text(crate::i18n::translate(
				"pending-upload-strip-the-message-may-already-have-reached-discord-check-the-conversation",
			))
			.clicked()
		{
			*cancel = true;
		}
	}
}

/// One compact progress card for the whole batch, with Discord's dismiss cross.
fn upload_strip(ui: &mut egui::Ui, pending: &Pending, upload: Option<&Upload>, cancel: &mut bool) {
	let colors = design::palette(ui);
	let count = pending.attachments.len();
	let title = if count == 1 {
		format!("Uploading {}", pending.attachments[0])
	} else {
		format!("Uploading {count} files")
	};
	let (fraction, detail) = match upload.and_then(|u| u.progress) {
		Some((sent, total)) if total > 0 && sent < total => (
			(sent as f32 / total as f32).clamp(0.0, 1.0),
			format!(
				"{} of {} · {}%",
				attachments::format_size(sent),
				attachments::format_size(total),
				sent.saturating_mul(100) / total
			),
		),
		Some((_, total)) if total > 0 => (1.0, "Sending message…".into()),
		_ => (
			0.0,
			match upload {
				Some(upload) => format!(
					"Preparing upload · {}",
					attachments::format_size(upload.bytes())
				),
				None => "Preparing upload…".into(),
			},
		),
	};
	egui::Frame::new()
		.fill(colors.raised)
		.stroke(egui::Stroke::new(1.0, colors.border))
		.corner_radius(8)
		.inner_margin(egui::Margin::symmetric(12, 10))
		.show(ui, |ui| {
			ui.set_width(ui.available_width());
			ui.spacing_mut().item_spacing.y = 4.0;
			ui.horizontal(|ui| {
				ui.spacing_mut().item_spacing.x = 8.0;
				ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
					if upload.is_some()
						&& icons::button(
							ui,
							icons::Icon::Close,
							24.0,
							&crate::i18n::translate("pending-upload-strip-cancel-upload"),
						)
						.on_hover_text(crate::i18n::translate(
							"pending-upload-strip-the-message-may-already-have-reached-discord-check-the-conversation",
						))
						.clicked()
					{
						*cancel = true;
					}
					ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
						ui.vertical(|ui| {
							ui.spacing_mut().item_spacing.y = 0.0;
							ui.add(
								egui::Label::new(
									design::medium(ui, title, 14.0).color(colors.text_strong),
								)
								.truncate()
								.selectable(false),
							);
							ui.add(
								egui::Label::new(
									RichText::new(detail).size(12.0).color(colors.muted),
								)
								.truncate()
								.selectable(false),
							);
						});
					});
				});
			});
			let (bar, _) =
				ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
			// The loading colours carry their own contrast, so the bar reads on any card.
			let (track, fill_color) = crate::loading::colors()
				.map_or((colors.hover, colors.accent), |loading| {
					(loading.surface, loading.bar)
				});
			ui.painter().rect_filled(bar, 3, track);
			if fraction > 0.0 {
				let fill = egui::Rect::from_min_size(
					bar.min,
					egui::vec2((bar.width() * fraction).max(6.0), bar.height()),
				);
				ui.painter().rect_filled(fill, 3, fill_color);
			}
		});
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn pending_content_progress_and_failure_remain_visible() {
		fn text(shape: &egui::Shape, result: &mut String) {
			match shape {
				egui::Shape::Text(t) => result.push_str(&t.galley.job.text),
				egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| text(s, result)),
				_ => {}
			}
		}
		let ctx = egui::Context::default();
		let state = State {
			demo: true,
			..Default::default()
		};
		let mut pending = Pending {
			sticker: None,
			channel: model::Id(1),
			content: format!("{} FULL END", "Full message text ".repeat(10)),
			attachments: vec!["notes.txt".into(), "photo.png".into()],
			nonce: "local".into(),
			delivery: Delivery::Sending,
			confirmed: None,
		};
		let mut upload = Upload {
			nonce: pending.nonce.clone(),
			files: vec![
				UploadFile {
					bytes: 40,
					preview: None,
				},
				UploadFile {
					bytes: 60,
					preview: Some(ctx.load_texture(
						"synthetic-photo",
						egui::ColorImage::filled([320, 180], egui::Color32::WHITE),
						egui::TextureOptions::LINEAR,
					)),
				},
			],
			progress: Some((25, 100)),
		};
		assert_eq!(upload.bytes(), 100);
		for (delivery, progress, expected) in [
			(Delivery::Sending, Some((25, 100)), "Uploading 2 files"),
			(
				Delivery::Sending,
				Some((25, 100)),
				"25 bytes of 100 bytes · 25%",
			),
			(Delivery::Sending, Some((100, 100)), "Sending message…"),
			(Delivery::Sending, None, "Preparing upload · 100 bytes"),
			(
				Delivery::Sending,
				Some((0, 0)),
				"Preparing upload · 100 bytes",
			),
			(Delivery::Rejected, None, "Not sent"),
			(Delivery::Ambiguous, None, "Delivery unknown"),
		] {
			pending.delivery = delivery;
			upload.progress = progress;
			let mut painted = String::new();
			let mut profile = crate::profiles::ProfileSession::default();
			for _ in 0..2 {
				let output = ctx.run_ui(
					egui::RawInput {
						screen_rect: Some(egui::Rect::from_min_size(
							egui::Pos2::ZERO,
							egui::vec2(260.0, 900.0),
						)),
						..Default::default()
					},
					|ui| {
						show(
							ui,
							&pending,
							(true, 10, false),
							&state,
							(
								&mut crate::avatars::Avatars::default(),
								&mut None,
								&mut profile,
								&mut None,
								&mut crate::markdown::FormatCache::default(),
							),
							Some(&upload),
							(&mut None, &mut false),
						);
					},
				);
				painted.clear();
				for shape in &output.shapes {
					text(&shape.shape, &mut painted);
				}
				output.drop_without_applying_deltas();
			}
			assert!(painted.contains("FULL END"), "{painted}");
			assert!(painted.contains(expected), "{painted}");
			// Every file stays visible: the text file as a card, the image as a thumbnail only.
			assert!(painted.contains("notes.txt"), "{painted}");
			assert!(!painted.contains("photo.png"), "{painted}");
			assert_eq!(
				painted.contains("Restore to composer"),
				delivery != Delivery::Sending
			);
		}
		// A screenshot alone carries its progress on the picture, with no strip beside it.
		fn flat(shape: &egui::Shape, result: &mut Vec<egui::Shape>) {
			match shape {
				egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| flat(s, result)),
				shape => result.push(shape.clone()),
			}
		}
		pending.attachments = vec!["photo.png".into()];
		upload.files.remove(0);
		pending.delivery = Delivery::Sending;
		upload.progress = None;
		let shade = egui::Color32::from_black_alpha(140);
		let mut profile = crate::profiles::ProfileSession::default();
		let mut cancel = false;
		// One 60 Hz frame: the bar's track, its fill, the cancel button, and whether the row
		// is still growing.
		let mut frame = |upload: &Upload, events: Vec<egui::Event>, cancel: &mut bool| {
			let mut growing = false;
			let output = ctx.run_ui(
				egui::RawInput {
					screen_rect: Some(egui::Rect::from_min_size(
						egui::Pos2::ZERO,
						egui::vec2(260.0, 900.0),
					)),
					events,
					predicted_dt: 1.0 / 60.0,
					..Default::default()
				},
				|ui| {
					growing = show(
						ui,
						&pending,
						(true, 10, false),
						&state,
						(
							&mut crate::avatars::Avatars::default(),
							&mut None,
							&mut profile,
							&mut None,
							&mut crate::markdown::FormatCache::default(),
						),
						Some(upload),
						(&mut None, cancel),
					);
				},
			);
			let mut painted = String::new();
			let mut shapes = Vec::new();
			for shape in &output.shapes {
				text(&shape.shape, &mut painted);
				flat(&shape.shape, &mut shapes);
			}
			output.drop_without_applying_deltas();
			assert!(!painted.contains("Uploading"), "{painted}");
			assert!(!painted.contains('%'), "{painted}");
			let track = shapes
				.iter()
				.find_map(|shape| match shape {
					egui::Shape::Rect(rect) if rect.fill == shade && rect.rect.height() == 4.0 => {
						Some(rect.rect)
					}
					_ => None,
				})
				.expect("progress track");
			let fill = shapes.iter().find_map(|shape| match shape {
				egui::Shape::Rect(rect) if rect.fill != shade && rect.rect.min == track.min => {
					Some(rect.rect)
				}
				_ => None,
			});
			// The button sits in the picture's top-right corner, above the bar.
			let button = shapes
				.iter()
				.find_map(|shape| match shape {
					egui::Shape::Circle(circle)
						if circle.radius == 12.0
							&& circle.center.x > track.right() - 24.0
							&& circle.center.y < track.top() =>
					{
						Some(circle.center)
					}
					_ => None,
				})
				.expect("cancel button");
			(track, fill, button, growing)
		};
		// Before any bytes go, the picture shows at its smallest, standing still.
		let (start, fill, _, growing) = frame(&upload, vec![], &mut cancel);
		assert!(fill.is_none() && !growing);
		// A chunk of progress eases the bar and the picture along, never jumping.
		upload.progress = Some((25, 100));
		let mut previous = start;
		let mut settled = None;
		for index in 0..90 {
			let (track, fill, button, growing) = frame(&upload, vec![], &mut cancel);
			assert!(track.width() >= previous.width() - 0.01, "frame {index}");
			assert!(
				track.width() - previous.width() < 4.0,
				"frame {index} jumped"
			);
			assert!(growing || index > 0, "a new chunk starts it moving");
			previous = track;
			if growing {
				assert!(settled.is_none(), "once settled it stays still");
			} else {
				let fill = fill.expect("progress fill");
				assert!((fill.width() - track.width() * 0.25).abs() < 0.5);
				settled.get_or_insert(button);
			}
		}
		let center = settled.expect("the spring settles within 1.5 s");
		for events in [
			vec![egui::Event::PointerMoved(center)],
			[true, false]
				.map(|pressed| egui::Event::PointerButton {
					pos: center,
					button: egui::PointerButton::Primary,
					pressed,
					modifiers: egui::Modifiers::NONE,
				})
				.into(),
			vec![],
		] {
			frame(&upload, events, &mut cancel);
		}
		assert!(cancel, "the corner button cancels the upload");
		// Finishing reaches the picture's full size, from about half of it here.
		upload.progress = Some((100, 100));
		let mut last = (previous, true);
		for _ in 0..90 {
			let (track, _, _, growing) = frame(&upload, vec![], &mut cancel);
			last = (track, growing);
		}
		let (full, growing) = last;
		assert!(!growing);
		assert!(
			start.width() + 16.0 < (full.width() + 16.0) * 0.55,
			"{start:?} {full:?}"
		);
		assert!(previous.width() > start.width() && previous.width() < full.width());
		// A picture too small for the overlay keeps the strip.
		upload.files[0].preview = Some(ctx.load_texture(
			"tiny-photo",
			egui::ColorImage::filled([4, 3], egui::Color32::WHITE),
			egui::TextureOptions::LINEAR,
		));
		let mut painted = String::new();
		for _ in 0..2 {
			let output = ctx.run_ui(
				egui::RawInput {
					screen_rect: Some(egui::Rect::from_min_size(
						egui::Pos2::ZERO,
						egui::vec2(260.0, 900.0),
					)),
					..Default::default()
				},
				|ui| {
					show(
						ui,
						&pending,
						(true, 10, false),
						&state,
						(
							&mut crate::avatars::Avatars::default(),
							&mut None,
							&mut profile,
							&mut None,
							&mut crate::markdown::FormatCache::default(),
						),
						Some(&upload),
						(&mut None, &mut false),
					);
				},
			);
			painted.clear();
			for shape in &output.shapes {
				text(&shape.shape, &mut painted);
			}
			output.drop_without_applying_deltas();
		}
		assert!(painted.contains("Uploading photo.png"), "{painted}");
	}
}
