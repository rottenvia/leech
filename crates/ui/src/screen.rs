//! Screen selection is a local intent; the desktop owns capture and stream credentials.
use client_core::{
	State,
	screen::{Settings, Source, SourceId},
};
use model::Id;

/// Seconds after going live before the LIVE badge appears: the button's ring has closed and
/// Discord's stream sound has sounded.
const LIVE_BADGE_DELAY: f32 = 0.35;

pub enum Request {
	Start(Settings),
	Stop,
}

/// The picker's two pages: Go Live to the call, or pictures only you see.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
	#[default]
	GoLive,
	OnlyMe,
}

pub struct ScreenUi {
	pub context: Option<(u64, Id, u64)>,
	pub open: bool,
	pub sources: Vec<Source>,
	pub selected: Option<SourceId>,
	pub refresh_requested: bool,
	pub request: Option<Request>,
	/// A share is starting or live. Stopping already reads as off.
	pub busy: bool,
	/// Discord carries the share: frames are flowing.
	pub live: bool,
	/// When the share last started and went live, on the interface clock, for the Go Live
	/// button's loading ring and the LIVE badge that follows it.
	started_at: Option<f64>,
	live_at: Option<f64>,
	pub status: &'static str,
	pub capture_status: Option<&'static str>,
	pub supported: bool,
	pub preview: Option<egui::TextureHandle>,
	height: u32,
	fps: u32,
	cursor: bool,
	audio: bool,
	pub tab: Tab,
	/// The screen or window chosen on the Stream to yourself page.
	local_selected: Option<SourceId>,
}
impl Default for ScreenUi {
	fn default() -> Self {
		Self {
			context: None,
			open: false,
			sources: Vec::new(),
			selected: None,
			refresh_requested: false,
			request: None,
			busy: false,
			live: false,
			started_at: None,
			live_at: None,
			status: "",
			capture_status: None,
			supported: false,
			preview: None,
			height: if cfg!(target_os = "linux") { 720 } else { 1080 },
			fps: 30,
			cursor: true,
			audio: cfg!(target_os = "macos"),
			tab: Tab::GoLive,
			local_selected: None,
		}
	}
}
impl ScreenUi {
	/// Opens the picker. While a share is live it opens on Stop streaming, with Stream to
	/// yourself one tab away.
	pub(crate) fn launch(&mut self, state: &State) {
		let Some(call) = &state.voice.active else {
			return;
		};
		self.context = Some((state.generation, call.channel, call.request));
		self.open = true;
		self.tab = Tab::GoLive;
		self.local_selected = None;
		if self.busy {
			if self.sources.is_empty() && !state.demo {
				self.refresh_requested = true;
			}
			return;
		}
		self.selected = None;
		self.sources.clear();
		if state.demo {
			self.sources = if cfg!(target_os = "macos") {
				vec![Source {
					id: SourceId::SystemPicker,
					name: "Choose with the macOS system picker".into(),
				}]
			} else {
				vec![
					Source {
						id: SourceId::Display(1),
						name: "Display 1 · Synthetic preview".into(),
					},
					Source {
						id: SourceId::Window(2),
						name: "Project notes · Synthetic window".into(),
					},
				]
			};
			self.selected = self.sources.first().map(|source| source.id);
			self.status = "Offline preview · no screen is captured";
		} else {
			self.refresh_requested = true;
			self.status = "Looking for screens and windows…";
		}
	}
	/// Stops the share. The button and LIVE clear on the click, before Discord confirms.
	pub(crate) fn stop(&mut self) {
		self.request = Some(Request::Stop);
		self.busy = false;
		self.live = false;
		self.started_at = None;
		self.live_at = None;
	}
	fn settings(&self) -> Option<Settings> {
		let source = self
			.selected
			.filter(|id| self.sources.iter().any(|s| s.id == *id))?;
		let settings = Settings {
			source,
			width: match self.height {
				480 => 854,
				1080 => 1920,
				_ => 1280,
			},
			height: self.height,
			fps: self.fps,
			cursor: self.cursor,
			audio: self.audio,
		};
		settings.valid().then_some(settings)
	}
	/// Seconds since the share started (while it is starting) and since it went live.
	pub(crate) fn since(&self, ctx: &egui::Context) -> (Option<f32>, Option<f32>) {
		let now = ctx.input(|input| input.time);
		// Recorded at the end of the frame, so a state seen first this frame has just begun.
		let since = |on: bool, at: Option<f64>| on.then(|| at.map_or(0.0, |at| (now - at) as f32));
		(
			since(self.busy && !self.live, self.started_at),
			since(self.live, self.live_at),
		)
	}
	/// Appearance (0 to 1) of this device's LIVE badge. It shows only once the share is live,
	/// after Discord's stream sound and the button's closing ring, and clears with the stop.
	pub(crate) fn live_reveal(&self, ctx: &egui::Context) -> f32 {
		let Some(live) = self.since(ctx).1 else {
			return 0.0;
		};
		let t = ((live - LIVE_BADGE_DELAY) / 0.2).clamp(0.0, 1.0);
		if t < 1.0 {
			ctx.request_repaint();
		}
		egui::emath::easing::cubic_out(t)
	}
	pub(super) fn show(
		&mut self,
		ctx: &egui::Context,
		state: &State,
		views: &mut crate::self_view::SelfViews,
	) {
		let now = ctx.input(|input| input.time);
		if !self.busy {
			self.started_at = None;
		} else if self.started_at.is_none() {
			self.started_at = Some(now);
		}
		if !self.live {
			self.live_at = None;
		} else if self.live_at.is_none() {
			self.live_at = Some(now);
		}
		let current = state
			.voice
			.active
			.as_ref()
			.map(|c| (state.generation, c.channel, c.request));
		if self.context.is_some() && current != self.context {
			self.preview = None;
			self.open = false;
			self.sources.clear();
			self.selected = None;
			self.request = None;
			self.refresh_requested = false;
		}
		if !self.open {
			return;
		}
		let mut cancel = false;
		let mut share = false;
		let mut stop = false;
		let local = self.tab == Tab::OnlyMe;
		let (title, subtitle) = if local {
			(
				"Stream to yourself".to_owned(),
				"Only you see these. They stay above your other windows, and nothing is sent to the call.".to_owned(),
			)
		} else {
			(
				crate::i18n::translate("screen-show-share-your-screen"),
				crate::i18n::translate("screen-show-choose-what-people-in-this-call-can-see"),
			)
		};
		let response = crate::dialog::Dialog::new("screen-share-settings", title)
			.subtitle(subtitle)
			.width(460.0)
			.show(ctx, |d| {
				d.scroll(260.0, |ui| {
					tabs(ui, &mut self.tab);
					ui.add_space(14.0);
					match self.tab {
						Tab::GoLive => self.body(ui, state),
						Tab::OnlyMe => self.local_body(ui, state, views),
					}
				});
				d.footer(|ui| match self.tab {
					Tab::OnlyMe => {
						let source = self
							.local_selected
							.and_then(|id| self.sources.iter().find(|source| source.id == id));
						ui.add_enabled_ui(source.is_some() && views.has_room(), |ui| {
							if crate::dialog::action(
								ui,
								"Show on top",
								crate::dialog::Action::Primary,
							)
							.clicked() && let Some(source) = source
							{
								views.requests.push(crate::self_view::Request::Capture(
									source.id,
									source.name.clone(),
								));
							}
						});
						cancel |= crate::dialog::action(ui, "Done", crate::dialog::Action::Neutral)
							.clicked();
					}
					Tab::GoLive if self.busy => {
						stop = crate::dialog::action(
							ui,
							"Stop streaming",
							crate::dialog::Action::Danger,
						)
						.clicked();
						cancel |=
							crate::dialog::action(ui, "Close", crate::dialog::Action::Neutral)
								.clicked();
					}
					Tab::GoLive => {
						let allowed = !state.offline_preview()
							&& self.supported && self.settings().is_some()
							&& state.call_connected()
							&& state
								.voice
								.active
								.as_ref()
								.is_some_and(|call| state.can_stream(call.channel));
						ui.add_enabled_ui(allowed, |ui| {
							share = crate::dialog::action(
								ui,
								"screen-show-share-screen",
								crate::dialog::Action::Primary,
							)
							.clicked();
						});
						cancel |= crate::dialog::action(
							ui,
							"screen-show-cancel",
							crate::dialog::Action::Neutral,
						)
						.clicked();
					}
				});
			});
		cancel |= response.close;
		if share && !cancel {
			self.request = self.settings().map(Request::Start);
		}
		if stop {
			self.stop();
		}
		if cancel || share || stop {
			self.open = false;
		}
	}

	/// Stream to yourself: a screen or window, files or the clipboard, and what is open now.
	fn local_body(
		&mut self,
		ui: &mut egui::Ui,
		state: &State,
		views: &mut crate::self_view::SelfViews,
	) {
		let colors = crate::design::palette(ui);
		self.sources_header(ui, state);
		ui.add_space(6.0);
		egui::ScrollArea::vertical()
			.id_salt("local-sources")
			.max_height(148.0)
			.scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
			.show(ui, |ui| self.source_list(ui, true));
		ui.add_space(10.0);
		ui.label(crate::design::eyebrow(
			ui,
			"From a file or the clipboard",
			colors.muted,
		));
		ui.add_space(6.0);
		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = 8.0;
			ui.add_enabled_ui(!views.busy && views.has_room(), |ui| {
				if crate::design::button(
					ui,
					"Video, GIF or image…",
					crate::design::ButtonKind::Outline,
				)
				.on_hover_text("Choose one or more files; each opens in its own window")
				.clicked()
				{
					views.requests.push(crate::self_view::Request::Files);
				}
				if crate::design::button(ui, "Paste image", crate::design::ButtonKind::Outline)
					.on_hover_text("Show the image you copied")
					.clicked()
				{
					views.requests.push(crate::self_view::Request::Paste);
				}
			});
			if views.busy {
				ui.add(egui::Spinner::new().size(16.0).color(colors.muted));
			}
		});
		if !views.views.is_empty() {
			ui.add_space(14.0);
			let mut close_all = false;
			ui.horizontal(|ui| {
				ui.label(crate::design::eyebrow(
					ui,
					format!("On your screen · {}", views.views.len()),
					colors.muted,
				));
				ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
					close_all = crate::design::text_action(ui, "Close all").clicked();
				});
			});
			ui.add_space(6.0);
			let mut closing = None;
			for view in &views.views {
				if open_row(ui, view) {
					closing = Some(view.id);
				}
			}
			// The desktop drops a window's capture or decoder once its view is gone.
			if close_all {
				views.views.clear();
			} else if let Some(id) = closing {
				views.views.retain(|view| view.id != id);
			}
		}
		let status = if !views.has_room() {
			"Close a window to open another: at most 8 show at once."
		} else {
			views.status
		};
		if !status.is_empty() {
			ui.add_space(10.0);
			status_note(ui, status);
		}
	}

	/// "Screen or window" over the source list, with Refresh at its right.
	fn sources_header(&mut self, ui: &mut egui::Ui, state: &State) {
		let colors = crate::design::palette(ui);
		ui.horizontal(|ui| {
			ui.label(crate::design::eyebrow(
				ui,
				crate::i18n::translate("screen-body-screen-or-window"),
				colors.muted,
			));
			ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
				if ui
					.add_enabled_ui(!state.demo && !cfg!(target_os = "linux"), |ui| {
						crate::design::text_action(
							ui,
							&crate::i18n::translate("screen-body-refresh"),
						)
					})
					.inner
					.clicked()
				{
					if !self.busy {
						self.selected = None;
					}
					self.local_selected = None;
					self.sources.clear();
					self.refresh_requested = true;
					self.status = "Looking for screens and windows…";
				}
			});
		});
	}

	/// Source list, then the capture options Discord exposes without an entitlement. While
	/// live, the options describe the running share and stay as they are.
	fn body(&mut self, ui: &mut egui::Ui, state: &State) {
		let colors = crate::design::palette(ui);
		if self.busy {
			status_note(
				ui,
				if self.live {
					"You are live. Stop streaming below, or open Stream to yourself to show something only you see."
				} else {
					"Your stream is starting…"
				},
			);
			return;
		}
		self.sources_header(ui, state);
		ui.add_space(6.0);
		egui::ScrollArea::vertical()
			.id_salt("screen-sources")
			.max_height(196.0)
			.scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
			.show(ui, |ui| self.source_list(ui, false));
		ui.add_space(6.0);
		ui.label(crate::design::eyebrow(
			ui,
			crate::i18n::translate("screen-body-quality"),
			colors.muted,
		));
		ui.add_space(6.0);
		ui.horizontal_wrapped(|ui| {
			ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
			for height in [480, 720, 1080] {
				if segment(ui, &format!("{height}p"), self.height == height).clicked() {
					self.height = height;
				}
			}
		});
		ui.add_space(14.0);
		ui.label(crate::design::eyebrow(
			ui,
			crate::i18n::translate("screen-body-frame-rate"),
			colors.muted,
		));
		ui.add_space(6.0);
		ui.horizontal_wrapped(|ui| {
			ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
			for fps in [15, 30, 60] {
				if segment(ui, &format!("{fps} fps"), self.fps == fps).clicked() {
					self.fps = fps;
				}
			}
		});
		ui.add_space(4.0);
		ui.add(
			egui::Label::new(
				egui::RichText::new(crate::i18n::translate(
					"screen-body-quality-selection-does-not-require-nitro",
				))
				.size(12.0)
				.color(colors.muted),
			)
			.wrap(),
		);
		ui.add_space(10.0);
		crate::design::switch(
			ui,
			"screen-body-show-cursor",
			Some("screen-body-include-the-pointer-in-the-shared-video"),
			&mut self.cursor,
		);
		if self.supported {
			ui.add_space(6.0);
			crate::design::switch(
				ui,
				"screen-body-share-system-audio",
				Some(if cfg!(target_os = "macos") {
					"screen-body-send-what-your-mac-plays-along-with-the-screen-leech"
				} else {
					"screen-body-share-sound-from-other-apps-even-when-sharing-one-window"
				}),
				&mut self.audio,
			);
		}
		ui.add_space(4.0);
		ui.add(
			egui::Label::new(
				egui::RichText::new(crate::i18n::translate(
					"screen-body-your-call-microphone-keeps-its-current-settings",
				))
				.size(12.0)
				.color(colors.muted),
			)
			.wrap(),
		);
		if !self.status.is_empty() {
			ui.add_space(10.0);
			status_note(ui, self.status);
		}
	}

	/// Selectable source rows; the caller bounds the height so options stay visible. `local`
	/// picks for Stream to yourself, which keeps its own choice.
	fn source_list(&mut self, ui: &mut egui::Ui, local: bool) {
		let chosen = if local {
			&mut self.local_selected
		} else {
			&mut self.selected
		};
		let colors = crate::design::palette(ui);
		if self.sources.is_empty() {
			egui::Frame::new()
				.fill(colors.base)
				.corner_radius(8)
				.inner_margin(egui::Margin::symmetric(12, 14))
				.show(ui, |ui| {
					ui.set_width(ui.available_width());
					ui.add(
						egui::Label::new(
							egui::RichText::new(crate::i18n::translate(
								"screen-source-list-no-screens-or-windows-are-available-yet",
							))
							.size(13.0)
							.color(colors.muted),
						)
						.wrap(),
					);
				});
		}
		for source in &self.sources {
			let selected = *chosen == Some(source.id);
			let (rect, response) = ui
				.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::click());
			let fill = if selected {
				colors.accent.gamma_multiply(0.18)
			} else if response.hovered() || response.has_focus() {
				colors.hover
			} else {
				colors.base
			};
			ui.painter().rect_filled(rect, 8, fill);
			if selected {
				ui.painter().rect_stroke(
					rect,
					8,
					egui::Stroke::new(1.0, colors.accent),
					egui::StrokeKind::Inside,
				);
			}
			let display = matches!(source.id, SourceId::Display(_) | SourceId::X11Desktop);
			crate::icons::paint(
				ui.painter(),
				if display {
					crate::icons::Icon::Television
				} else {
					crate::icons::Icon::Window
				},
				egui::Rect::from_center_size(
					egui::pos2(rect.left() + 26.0, rect.center().y),
					egui::Vec2::splat(20.0),
				),
				if selected {
					colors.accent
				} else {
					colors.muted
				},
			);
			let text_left = rect.left() + 46.0;
			let text_width = (rect.right() - 36.0 - text_left).max(40.0);
			let source_name = if source.id == SourceId::SystemPicker {
				crate::i18n::translate("screen-macos-system-picker")
			} else {
				source.name.clone()
			};
			let name = ui.painter().layout(
				source_name.clone(),
				egui::FontId::new(14.0, crate::design::medium_family(ui.ctx())),
				colors.text_strong,
				text_width,
			);
			let kind = ui.painter().layout_no_wrap(
				match source.id {
					SourceId::Display(_) | SourceId::X11Desktop => "Screen".to_owned(),
					SourceId::Window(_) => "Window".to_owned(),
					SourceId::SystemPicker => {
						crate::i18n::translate("screen-macos-system-picker-kind")
					}
					#[allow(unreachable_patterns)] // Portal may be absent outside Linux.
					_ => "System permission dialog".to_owned(),
				},
				egui::FontId::proportional(11.0),
				colors.muted,
			);
			let total = name.size().y + kind.size().y;
			let mut y = rect.center().y - total * 0.5;
			ui.painter()
				.galley(egui::pos2(text_left, y), name, colors.text_strong);
			y += total - kind.size().y;
			ui.painter()
				.galley(egui::pos2(text_left, y), kind, colors.muted);
			if selected {
				crate::icons::paint(
					ui.painter(),
					crate::icons::Icon::Check,
					egui::Rect::from_center_size(
						egui::pos2(rect.right() - 20.0, rect.center().y),
						egui::Vec2::splat(16.0),
					),
					colors.accent,
				);
			}
			response.widget_info(|| {
				egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, &source_name)
			});
			if response.clicked() {
				*chosen = Some(source.id);
			}
			ui.add_space(4.0);
		}
	}
}

/// Go Live | Stream to yourself, as one segmented control whose selection slides.
fn tabs(ui: &mut egui::Ui, tab: &mut Tab) {
	let colors = crate::design::palette(ui);
	let (rect, _) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::hover());
	ui.painter().rect_filled(rect, 10, colors.base);
	let half = egui::vec2(rect.width() * 0.5, rect.height());
	let at = ui.ctx().animate_value_with_time(
		ui.scope_id().with("screen-tab-slide"),
		if *tab == Tab::GoLive { 0.0 } else { 1.0 },
		0.18,
	);
	let slider =
		egui::Rect::from_min_size(rect.min + egui::vec2(half.x * at, 0.0), half).shrink(3.0);
	ui.painter()
		.rect_filled(slider, 8, colors.accent.gamma_multiply(0.18));
	ui.painter().rect_stroke(
		slider,
		8,
		egui::Stroke::new(1.0, colors.accent.gamma_multiply(0.6)),
		egui::StrokeKind::Inside,
	);
	for (index, (choice, label)) in [
		(Tab::GoLive, "Go Live"),
		(Tab::OnlyMe, "Stream to yourself"),
	]
	.into_iter()
	.enumerate()
	{
		let area =
			egui::Rect::from_min_size(rect.min + egui::vec2(half.x * index as f32, 0.0), half);
		let response = ui.interact(
			area,
			ui.scope_id().with(("screen-tab", index)),
			egui::Sense::click(),
		);
		let selected = *tab == choice;
		let hover = crate::design::hover_motion(ui, &response);
		ui.painter().text(
			area.center(),
			egui::Align2::CENTER_CENTER,
			label,
			egui::FontId::new(14.0, crate::design::medium_family(ui.ctx())),
			if selected {
				colors.text_strong
			} else {
				colors.muted.lerp_to_gamma(colors.text_strong, hover * 0.6)
			},
		);
		response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Tab, true, selected, label));
		if response.clicked() {
			*tab = choice;
		}
	}
}

/// One window open on Stream to yourself: its kind, its name and Close. True on Close.
fn open_row(ui: &mut egui::Ui, view: &crate::self_view::View) -> bool {
	let colors = crate::design::palette(ui);
	let (rect, _) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());
	ui.painter().rect_filled(rect, 8, colors.base);
	crate::icons::paint(
		ui.painter(),
		view.kind.icon(),
		egui::Rect::from_center_size(
			egui::pos2(rect.left() + 22.0, rect.center().y),
			egui::Vec2::splat(18.0),
		),
		colors.muted,
	);
	let action = egui::Rect::from_min_max(
		egui::pos2(rect.right() - 72.0, rect.top()),
		egui::pos2(rect.right() - 12.0, rect.bottom()),
	);
	let name = egui::WidgetText::from(
		egui::RichText::new(&view.name)
			.size(14.0)
			.color(colors.text_strong),
	)
	.into_galley(
		ui,
		Some(egui::TextWrapMode::Truncate),
		(action.left() - 8.0 - (rect.left() + 42.0)).max(40.0),
		egui::TextStyle::Body,
	);
	ui.painter().galley(
		egui::pos2(rect.left() + 42.0, rect.center().y - name.size().y * 0.5),
		name,
		colors.text_strong,
	);
	ui.scope_builder(
		egui::UiBuilder::new()
			.max_rect(action)
			.id_salt(("self-view-row", view.id))
			.layout(egui::Layout::right_to_left(egui::Align::Center)),
		|ui| crate::design::text_action(ui, "Close").clicked(),
	)
	.inner
}

/// A quiet note in a soft box under the options.
fn status_note(ui: &mut egui::Ui, text: &str) {
	let colors = crate::design::palette(ui);
	egui::Frame::new()
		.fill(colors.base)
		.corner_radius(8)
		.inner_margin(egui::Margin::symmetric(12, 10))
		.show(ui, |ui| {
			ui.set_width(ui.available_width());
			ui.add(
				egui::Label::new(egui::RichText::new(text).size(12.0).color(colors.muted)).wrap(),
			);
		});
}

/// Compact segmented choice used by the quality and frame-rate rows.
fn segment(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
	let label = crate::i18n::translate_if_key(label);
	let colors = crate::design::palette(ui);
	let galley = ui.painter().layout_no_wrap(
		label.to_owned(),
		egui::FontId::new(13.0, crate::design::medium_family(ui.ctx())),
		colors.text_strong,
	);
	let (rect, response) = ui.allocate_exact_size(
		egui::vec2(galley.size().x + 24.0, 32.0),
		egui::Sense::click(),
	);
	let fill = if selected {
		colors.accent
	} else if response.hovered() || response.has_focus() {
		colors.hover
	} else {
		colors.base
	};
	ui.painter().rect_filled(rect, 8, fill);
	let color = if selected {
		colors.accent_text
	} else {
		colors.text
	};
	ui.painter().galley(
		egui::pos2(
			rect.center().x - galley.size().x * 0.5,
			rect.center().y - galley.size().y * 0.5,
		),
		galley,
		color,
	);
	response.widget_info(|| {
		egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, &label)
	});
	response
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn settings_require_a_current_source_and_offer_high_quality_without_entitlements() {
		let mut picker = ScreenUi {
			selected: Some(SourceId::Window(7)),
			..Default::default()
		};
		assert!(picker.settings().is_none());
		picker.sources.push(Source {
			id: SourceId::Window(7),
			name: "Notes".into(),
		});
		picker.height = 1080;
		picker.fps = 60;
		let settings = picker.settings().unwrap();
		assert_eq!(
			(settings.width, settings.height, settings.fps),
			(1920, 1080, 60)
		);
		assert_eq!(settings.bit_rate(), 12_000_000);
		picker.audio = true;
		assert!(picker.settings().unwrap().audio);
		picker.audio = false;
		assert!(!picker.settings().unwrap().audio);
		picker.sources.clear();
		assert!(picker.settings().is_none());
	}
}
