//! "Stream to yourself": pictures only this device shows (a screen or window, a video, a GIF
//! or an image), each in a borderless window that stays above other windows. On one monitor
//! that works as a widget or banner, without OBS. The desktop captures and decodes; nothing
//! is sent to the call, though a shared screen shows these windows like any other.
use crate::{
	design,
	voice::{
		PIP_INSET, PictureCrop, PictureZoom, chrome_plate, fit_rect, paint_crop_glyph,
		paint_cutout_glyph, paint_stretch_glyph, pip_resize, sized_to,
	},
};
use client_core::screen::SourceId;

/// Most windows open at once; each holds one picture texture and a capture or decoder.
pub const MAX_VIEWS: usize = 8;

pub enum Request {
	/// Show a screen or window from the picker, under its name.
	Capture(SourceId, String),
	/// Choose videos, GIFs or images in a file dialog.
	Files,
	/// Show the image or files on the clipboard.
	Paste,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
	Screen,
	Window,
	Video,
	Animation,
	Image,
}

impl Kind {
	/// Images and GIFs can lose their background; screens, windows and videos keep theirs.
	pub fn cuts_out(self) -> bool {
		matches!(self, Self::Image | Self::Animation)
	}

	pub(crate) fn icon(self) -> crate::icons::Icon {
		match self {
			Self::Screen => crate::icons::Icon::Television,
			Self::Window => crate::icons::Icon::Window,
			Self::Video => crate::icons::Icon::FileVideo,
			Self::Animation => crate::icons::Icon::Gif,
			Self::Image => crate::icons::Icon::Image,
		}
	}
}

pub struct View {
	pub id: u64,
	pub name: String,
	pub kind: Kind,
	/// The latest picture; `None` while the first one loads.
	pub texture: Option<egui::load::SizedTexture>,
	/// Over the picture when it stopped, such as a closed window; empty otherwise.
	pub status: &'static str,
	/// No backdrop behind an image or GIF: its transparent parts, or a plain background the
	/// desktop cuts out, show what is under the window.
	pub remove_background: bool,
	/// Letterboxed at the picture's own shape instead of stretched over the window.
	fit: bool,
	unpinned: bool,
	/// The first picture has resized the window to its shape.
	sized: bool,
	zoom: PictureZoom,
	crop: PictureCrop,
	/// The whole window's opacity in percent.
	opacity: u16,
	opened: egui::Rect,
}

#[derive(Default)]
pub struct SelfViews {
	pub views: Vec<View>,
	pub requests: Vec<Request>,
	/// Why the latest file, paste or capture did not open, for the Stream to yourself tab.
	pub status: &'static str,
	/// A file dialog, a paste or a decode is in progress.
	pub busy: bool,
}

impl SelfViews {
	/// A new window over the main one, cascaded so several never sit exactly on each other.
	/// `transparent` pictures (a GIF with see-through parts) open without a backdrop.
	pub fn open(
		&mut self,
		ctx: &egui::Context,
		id: u64,
		name: String,
		kind: Kind,
		transparent: bool,
	) {
		let parent =
			ctx.input(|input| input.viewport().inner_rect)
				.unwrap_or(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(1280.0, 800.0),
				));
		let step = 32.0 * (self.views.len() % 6) as f32;
		let opened = egui::Rect::from_min_size(
			parent.min + egui::vec2(56.0 + step, 72.0 + step),
			egui::vec2(480.0, 270.0),
		);
		self.status = "";
		self.views.push(View {
			id,
			name,
			kind,
			texture: None,
			status: "",
			remove_background: transparent && kind.cuts_out(),
			fit: false,
			unpinned: false,
			sized: false,
			zoom: PictureZoom::default(),
			crop: PictureCrop::default(),
			opacity: 100,
			opened,
		});
	}

	pub fn has_room(&self) -> bool {
		self.views.len() < MAX_VIEWS
	}

	/// Draws every window; a window asked to close leaves the list, and the desktop drops
	/// its picture source with it.
	pub fn show(&mut self, ctx: &egui::Context) {
		self.views.retain_mut(|view| {
			let builder = egui::ViewportBuilder::default()
				.with_title(format!("{} · only you", view.name))
				.with_position(view.opened.min)
				.with_inner_size(view.opened.size())
				.with_min_inner_size([160.0, 90.0])
				.with_transparent(true)
				.with_decorations(false)
				.with_taskbar(false)
				.with_active(false)
				.with_window_level(if view.unpinned {
					egui::WindowLevel::Normal
				} else {
					egui::WindowLevel::AlwaysOnTop
				});
			!ctx.show_viewport_immediate(
				egui::ViewportId::from_hash_of(("self-view", view.id)),
				builder,
				|ui, _| view.ui(ui),
			)
		});
	}
}

impl View {
	/// One window's surface, like picture-in-picture: the picture (stretched unless fitted),
	/// moved from anywhere, zoomed by holding left click and scrolling, with its actions
	/// fading in under the pointer. Returns true when the window asks to close.
	fn ui(&mut self, ui: &mut egui::Ui) -> bool {
		let rect = ui.max_rect();
		let opacity = f32::from(self.opacity) / 100.0;
		let backdrop = !(self.remove_background && self.kind.cuts_out());
		if backdrop {
			ui.painter()
				.rect_filled(rect, 0, egui::Color32::BLACK.gamma_multiply(opacity));
		}
		let (maximized, mut close) = ui.input(|input| {
			(
				input.viewport().maximized == Some(true),
				input.viewport().close_requested(),
			)
		});
		// The first picture gives the window its shape at about the same area, so a stretched
		// picture starts undistorted.
		if !self.sized
			&& let Some(texture) = self.texture
		{
			self.sized = true;
			let size = sized_to(480.0 * 270.0, texture.size);
			if !maximized {
				ui.ctx()
					.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
			}
		}
		// Every window draws in one context: ids carry the window's own id.
		let id = egui::Id::unique(("self-view", self.id));
		if self.crop.editing()
			&& let Some(texture) = self.texture
		{
			if self.crop.edit(ui, id.with("crop"), rect, texture) == Some(true) {
				self.zoom.reset(ui);
				if !maximized {
					let size = sized_to(rect.area(), self.crop.size(texture.size));
					ui.ctx()
						.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
				}
			}
			if !maximized {
				pip_resize(ui, rect);
			}
			return close;
		}
		let surface = ui.interact(rect, id.with("surface"), egui::Sense::click_and_drag());
		let content = match self.texture {
			Some(texture) if self.fit => fit_rect(rect, self.crop.size(texture.size)),
			_ => rect,
		};
		let uv = self.crop.uv(self.zoom.update(ui, &surface, content));
		self.zoom.move_window(ui, &surface);
		if surface.double_clicked() {
			if self.zoom.zoomed() {
				self.zoom.reset(ui);
			} else if cfg!(target_os = "windows") {
				ui.ctx()
					.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
			}
		}
		let muted = design::palette(ui).muted;
		match self.texture {
			Some(texture) => {
				ui.painter().image(
					texture.id,
					content,
					uv,
					egui::Color32::WHITE.gamma_multiply(opacity),
				);
			}
			None if self.status.is_empty() => {
				ui.put(
					egui::Rect::from_center_size(
						rect.center() - egui::vec2(0.0, 14.0),
						egui::Vec2::splat(20.0),
					),
					egui::Spinner::new().color(muted),
				);
				ui.painter().text(
					rect.center() + egui::vec2(0.0, 16.0),
					egui::Align2::CENTER_CENTER,
					"Loading…",
					egui::FontId::proportional(12.0),
					muted,
				);
			}
			None => {}
		}
		if !self.status.is_empty() {
			// The last picture stays, dimmed, under why it stopped.
			ui.painter()
				.rect_filled(rect, 0, egui::Color32::from_black_alpha(150));
			let layout = |text: &str| {
				let mut job = egui::text::LayoutJob::simple(
					text.to_owned(),
					egui::FontId::proportional(13.0),
					egui::Color32::WHITE,
					(rect.width() - 32.0).max(80.0),
				);
				job.halign = egui::Align::Center;
				ui.painter().layout_job(job)
			};
			// A small window keeps the first paragraph, the reason, without the advice.
			let mut galley = layout(self.status);
			if galley.size().y > rect.height() - 24.0
				&& let Some((reason, _)) = self.status.split_once("\n\n")
			{
				galley = layout(reason);
			}
			// A panel keeps the note readable over a busy picture.
			ui.painter().rect_filled(
				egui::Rect::from_center_size(rect.center(), galley.size() + egui::vec2(28.0, 20.0)),
				8,
				egui::Color32::from_black_alpha(190),
			);
			// Centred rows sit around x = 0, so place the galley by its own rect.
			ui.painter().galley(
				rect.center() - galley.rect.center().to_vec2(),
				galley,
				egui::Color32::WHITE,
			);
		}
		let hovered = ui
			.input(|input| input.pointer.hover_pos())
			.is_some_and(|pointer| rect.contains(pointer));
		if self.texture.is_some() {
			self.zoom.badge(ui, id.with("zoom"), content, hovered, true);
		}
		let shown = ui
			.ctx()
			.animate_bool_with_time(id.with("chrome"), hovered, 0.15);
		if shown > 0.0 && !backdrop {
			// A window without a backdrop shows where it is while the pointer is over it.
			ui.painter().rect_filled(
				rect,
				0,
				egui::Color32::from_black_alpha(40).gamma_multiply(shown),
			);
			ui.painter().rect_stroke(
				rect.shrink(0.5),
				0,
				egui::Stroke::new(
					1.0,
					egui::Color32::from_white_alpha(70).gamma_multiply(shown),
				),
				egui::StrokeKind::Inside,
			);
		}
		if shown > 0.0 {
			close |= self.chrome(ui, id, rect, hovered, shown);
		}
		if let Some(menu) = egui::Popup::context_menu(&surface)
			.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
			.show(|ui| self.menu(ui))
		{
			close |= menu.inner;
		}
		if !maximized {
			pip_resize(ui, rect);
		}
		close
	}

	/// Close and Keep on top at the top-right, Stretch at the top-left: picture-in-picture's
	/// plates and places. Returns true when Close is clicked.
	fn chrome(
		&mut self,
		ui: &mut egui::Ui,
		id: egui::Id,
		rect: egui::Rect,
		hovered: bool,
		shown: f32,
	) -> bool {
		const SIZE: f32 = 32.0;
		let slot = |index: f32, right: bool| {
			let offset = PIP_INSET + index * (SIZE + 6.0);
			let x = if right {
				rect.right() - offset - SIZE
			} else {
				rect.left() + offset
			};
			egui::Rect::from_min_size(
				egui::pos2(x, rect.top() + PIP_INSET),
				egui::Vec2::splat(SIZE),
			)
		};
		let mut close = false;
		let mut buttons = vec![slot(0.0, true), slot(1.0, true), slot(0.0, false)];
		if self.texture.is_some() {
			buttons.push(slot(1.0, false));
			if self.kind.cuts_out() {
				buttons.push(slot(2.0, false));
			}
		}
		for (index, area) in buttons.into_iter().enumerate() {
			let response = ui.interact(
				area,
				id.with(("button", index)),
				if hovered {
					egui::Sense::click()
				} else {
					egui::Sense::hover()
				},
			);
			let on = match index {
				1 => !self.unpinned,
				2 => !self.fit,
				3 => self.crop.cropped(),
				4 => self.remove_background,
				_ => false,
			};
			chrome_plate(ui, &response, on, shown);
			let glyph = area.shrink(7.0);
			let color = egui::Color32::WHITE.gamma_multiply(shown);
			let (label, hint) = match index {
				0 => {
					crate::icons::paint(ui.painter(), crate::icons::Icon::Close, glyph, color);
					("Close", "Close this window")
				}
				1 => {
					crate::icons::paint(ui.painter(), crate::icons::Icon::Pin, glyph, color);
					if self.unpinned {
						("Keep on top", "Keep this window above other windows")
					} else {
						("Unpin", "Stop keeping this window above other windows")
					}
				}
				2 => {
					paint_stretch_glyph(ui.painter(), glyph, color);
					if self.fit {
						(
							"Stretch to fill",
							"Stretch the picture over the whole window",
						)
					} else {
						("Fit picture", "Show the whole picture at its own shape")
					}
				}
				3 => {
					paint_crop_glyph(ui.painter(), glyph, color);
					("Crop", "Keep only part of the picture")
				}
				_ => {
					paint_cutout_glyph(ui.painter(), glyph, color);
					if self.remove_background {
						("Keep background", "Show the picture on its own background")
					} else {
						(
							"Remove background",
							"Show only the picture, without its background",
						)
					}
				}
			};
			response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
			if response.on_hover_text(hint).clicked() {
				match index {
					0 => close = true,
					1 => self.unpinned = !self.unpinned,
					2 => self.fit = !self.fit,
					3 => self.crop.start(),
					_ => self.remove_background = !self.remove_background,
				}
			}
		}
		close
	}

	/// Right-click menu: opacity, how the picture fills the window, its part and background,
	/// Keep on top, and Close.
	fn menu(&mut self, ui: &mut egui::Ui) -> bool {
		ui.set_width(232.0);
		design::menu_slider(ui, "Opacity", &mut self.opacity, 20..=100, "%");
		ui.separator();
		let mut stretched = !self.fit;
		if design::menu_check(ui, "Stretch to fill", &mut stretched).changed() {
			self.fit = !stretched;
		}
		let mut on_top = !self.unpinned;
		if design::menu_check(ui, "Keep on top", &mut on_top).changed() {
			self.unpinned = !on_top;
		}
		if self.kind.cuts_out() {
			design::menu_check(ui, "Remove background", &mut self.remove_background);
		}
		if self.texture.is_some() && design::menu_item(ui, None, "Crop…", false).clicked() {
			self.crop.start();
			ui.close();
		}
		if (self.zoom.zoomed() || self.crop.cropped())
			&& design::menu_item(ui, None, "Show the whole picture", false).clicked()
		{
			self.zoom.reset(ui);
			self.crop.reset();
			ui.close();
		}
		ui.separator();
		let close = design::menu_item(ui, None, "Close", true).clicked();
		if close {
			ui.close();
		}
		close
	}
}
