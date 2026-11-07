//! Remote control: one Leech user controls another's PC while both are in the same call.
//!
//! **Access** asks to control someone's PC; **Control** lets someone control yours. The person
//! asked sees a notification of their own with a 15 second countdown, answers in Leech, and
//! the one giving control picks keyboard, mouse or both, whether their PC's sound goes along,
//! and one pause key. The controller's window opens fullscreen on the other PC's screen.
//!
//! The interface only draws; the desktop signals, connects, captures, injects and fills in
//! [`Phase`] as the session moves on.
use crate::{design, dialog, icons::Icon};
use model::Id;

/// How long the notification waits before it declines by itself.
pub const NOTICE_SECS: f64 = 15.0;
/// How long the one who asked waits for an answer, the setup included.
pub const ANSWER_SECS: f64 = 60.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Permission {
	Keyboard,
	Mouse,
	Full,
}

impl Permission {
	pub fn label(self) -> &'static str {
		match self {
			Self::Keyboard => "Keyboard only",
			Self::Mouse => "Mouse only",
			Self::Full => "Full control",
		}
	}
}

/// What the one giving control allows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grant {
	/// `None` disables control, which ends the session.
	pub permission: Option<Permission>,
	/// The PC's sound goes to the controller.
	pub audio: bool,
	pub paused: bool,
}

impl Default for Grant {
	fn default() -> Self {
		Self {
			permission: Some(Permission::Full),
			audio: true,
			paused: false,
		}
	}
}

/// One key as a low-level keyboard hook reports it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Key {
	pub scan: u16,
	pub extended: bool,
	/// Its name for the keycap ("F8", "Pause").
	pub name: String,
}

/// The keys this device remembers between sessions.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Keys {
	/// Pauses and resumes the controller while this PC is controlled.
	pub pause: Option<Key>,
	/// Hides and shows the label over the controlled screen.
	pub overlay: Option<Key>,
	/// The pause key warning was dismissed for good.
	pub skip_warning: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bind {
	Pause,
	Overlay,
}

pub enum Request {
	/// Ask to control this person's PC.
	Ask(Id),
	/// Let this person control this PC, as set up.
	Give(Id, Grant),
	/// Stop waiting for an answer.
	Cancel,
	/// Answer the request or offer that came in.
	Answer(bool),
	/// The setup after accepting a request is done.
	Start(Grant),
	/// The one in control changes what the controller may do.
	Change(Grant),
	/// End the session, from either side.
	End,
	/// Bring the main window forward (the notification was clicked).
	Focus,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
	Permission,
	Key,
	Warning,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum MouseButton {
	Left,
	Right,
	Middle,
	Back,
	Forward,
}

/// What the controller's pointer did, for the desktop to send.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MouseInput {
	/// Position as a fraction of the other screen, and the raw motion since the last one.
	Move {
		x: f32,
		y: f32,
		dx: i32,
		dy: i32,
	},
	Button {
		button: MouseButton,
		down: bool,
	},
	/// In wheel units (120 a notch).
	Wheel {
		dx: i32,
		dy: i32,
	},
}

/// The controller's side of a running session; the desktop fills the picture and state.
pub struct Viewing {
	pub peer: Id,
	pub name: String,
	/// Fullscreen and in control; otherwise a small window that only shows the screen.
	pub full: bool,
	pub texture: Option<egui::load::SizedTexture>,
	/// The other screen in pixels.
	pub screen: [u32; 2],
	pub connected: bool,
	/// The other side finished its setup.
	pub started: bool,
	pub permission: Option<Permission>,
	pub paused: bool,
	pub audio: bool,
	/// A game there captured the mouse: motion goes as raw deltas and the pointer is locked.
	pub relative: bool,
	pub rtt_ms: u32,
	/// The part of each picture that shows the other screen (x, y, width, height as
	/// fractions); the rest is the encoder's black bars.
	pub content: [f32; 4],
	/// Pointer input this pass, taken by the desktop.
	pub inputs: Vec<MouseInput>,
	/// Buttons held down in the window now.
	pub held: Vec<MouseButton>,
	/// The window is fullscreen, focused and allowed to steer: the keyboard goes there too.
	pub capturing: bool,
	shown_full: Option<bool>,
	opened_at: f64,
}

impl Viewing {
	pub fn new(peer: Id, name: String) -> Self {
		Self {
			peer,
			name,
			full: true,
			texture: None,
			screen: [0, 0],
			connected: false,
			started: false,
			permission: None,
			paused: false,
			audio: false,
			relative: false,
			rtt_ms: 0,
			content: [0.0, 0.0, 1.0, 1.0],
			inputs: Vec::new(),
			held: Vec::new(),
			capturing: false,
			shown_full: None,
			opened_at: f64::NAN,
		}
	}
}

#[derive(Default)]
pub enum Phase {
	#[default]
	Idle,
	/// Waiting for `peer` to answer: an Access request, or (`give`) a Control offer.
	Waiting {
		peer: Id,
		name: String,
		give: bool,
		since: f64,
		/// They said yes; the connection is being made.
		connecting: bool,
	},
	/// Someone asks for access, or (`give`) offers control of their PC.
	Incoming {
		peer: Id,
		name: String,
		give: bool,
		since: f64,
		/// The notification was clicked: the answer shows in Leech.
		opened: bool,
	},
	/// The one giving control chooses what to allow, before `Start` (they were asked) or
	/// `Give` (they offer).
	Setup {
		peer: Id,
		name: String,
		give: bool,
		step: Step,
		grant: Grant,
	},
	/// This PC is controlled.
	Hosting {
		peer: Id,
		name: String,
		grant: Grant,
		connected: bool,
	},
	Viewing(Box<Viewing>),
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Sheet {
	#[default]
	None,
	/// The current phase's own dialog (waiting, answer, setup).
	Phase,
	/// The one in control manages access.
	Access,
	/// The keys this device remembers.
	Keys,
}

#[derive(Default)]
pub struct Remote {
	pub phase: Phase,
	pub requests: Vec<Request>,
	pub keys: Keys,
	/// The keys changed and should be saved.
	pub keys_changed: bool,
	/// A key is being bound; the desktop's keyboard hook answers through [`Remote::bound`].
	pub binding: Option<Bind>,
	/// The label over the controlled screen is hidden.
	pub overlay_hidden: bool,
	/// The dialog shown in the main window.
	pub sheet: Sheet,
	/// The person the Access and Control menu acts on.
	chosen: Option<Id>,
	/// The controller's view never goes fullscreen (the offline preview's screenshots).
	pub windowed: bool,
}

impl Remote {
	pub fn active(&self) -> bool {
		!matches!(self.phase, Phase::Idle)
	}

	/// A window of its own is up: the notification or the controller's view.
	pub fn shows_windows(&self) -> bool {
		matches!(
			self.phase,
			Phase::Incoming { opened: false, .. } | Phase::Viewing(_)
		)
	}

	/// The hook caught a key for the binding in progress.
	pub fn bound(&mut self, key: Key) {
		match self.binding.take() {
			Some(Bind::Pause) => self.keys.pause = Some(key),
			Some(Bind::Overlay) => self.keys.overlay = Some(key),
			None => return,
		}
		self.keys_changed = true;
	}

	/// A new phase from the desktop opens its dialog, as clicks do.
	pub fn open_phase_sheet(&mut self) {
		self.sheet = Sheet::Phase;
	}

	pub fn viewing(&mut self) -> Option<&mut Viewing> {
		match &mut self.phase {
			Phase::Viewing(viewing) => Some(viewing),
			_ => None,
		}
	}

	fn end(&mut self, request: Request) {
		self.requests.push(request);
		self.phase = Phase::Idle;
		self.sheet = Sheet::None;
		self.binding = None;
	}
}

impl crate::MessagingUi {
	/// The call bar's remote control button, between screen share and More.
	pub(crate) fn remote_control(&mut self, ui: &mut egui::Ui, state: &client_core::State) {
		let peers = state.remote_peers();
		let colors = design::palette(ui);
		let now = ui.input(|input| input.time);
		let remote = &mut self.remote;
		let (label, hint, tint, lit) = match &remote.phase {
			Phase::Idle if peers.is_empty() => (
				"Remote control",
				"Remote control needs someone else in the call",
				None,
				0.0,
			),
			Phase::Idle => (
				"Remote control",
				"Access someone's PC or let them control yours · right-click for keys",
				None,
				0.0,
			),
			Phase::Waiting { .. } | Phase::Incoming { .. } | Phase::Setup { .. } => (
				"Remote control",
				"Show the remote control request",
				Some(colors.accent),
				0.5 + 0.5 * (now * std::f64::consts::TAU / 1.6).sin() as f32,
			),
			Phase::Hosting { grant, .. } => (
				"End remote control",
				"End control · right-click to manage access",
				Some(if grant.paused {
					colors.warning
				} else {
					colors.accent
				}),
				1.0,
			),
			Phase::Viewing(_) => (
				"Back to control",
				"Back to controlling · right-click for keys",
				Some(colors.positive),
				1.0,
			),
		};
		let enabled = !peers.is_empty() || remote.active();
		let (rect, response) = ui.allocate_exact_size(
			egui::vec2(48.0, crate::voice::CONTROL_HEIGHT),
			if enabled {
				egui::Sense::click()
			} else {
				egui::Sense::hover()
			},
		);
		let hover = if enabled {
			design::hover_motion(ui, &response)
		} else {
			0.0
		};
		let press = if enabled {
			design::press_motion(ui, &response)
		} else {
			0.0
		};
		let plate = design::pressed_rect(rect.shrink(4.0), press);
		if let Some(tint) = tint {
			ui.painter().rect_filled(
				plate,
				8,
				tint.gamma_multiply(0.14 + 0.16 * lit + 0.08 * hover),
			);
			if lit < 1.0 {
				ui.ctx().request_repaint();
			}
		} else if hover > 0.0 {
			ui.painter().rect_filled(
				plate,
				8,
				egui::Color32::from_white_alpha((28.0 * hover + 14.0 * press) as u8),
			);
		}
		let glyph = if !enabled {
			crate::voice::stage_tones().muted.gamma_multiply(0.45)
		} else if let Some(tint) = tint {
			crate::voice::stage_tones()
				.text
				.lerp_to_gamma(tint, 0.35 + 0.5 * lit)
		} else {
			crate::voice::stage_tones().text
		};
		paint_remote_glyph(
			ui.painter(),
			egui::Rect::from_center_size(
				rect.center(),
				egui::Vec2::splat(22.0 * (1.0 - 0.1 * press)),
			),
			glyph,
		);
		response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
		let response = response.on_hover_text(hint);
		if response.secondary_clicked() {
			remote.sheet = if matches!(remote.phase, Phase::Hosting { .. }) {
				Sheet::Access
			} else {
				Sheet::Keys
			};
		}
		if response.clicked() {
			match &mut remote.phase {
				Phase::Idle => {}
				Phase::Hosting { .. } => remote.end(Request::End),
				Phase::Viewing(viewing) => viewing.full = true,
				Phase::Incoming { opened, .. } => {
					*opened = true;
					remote.sheet = Sheet::Phase;
				}
				_ => remote.sheet = Sheet::Phase,
			}
		}
		if matches!(remote.phase, Phase::Idle) {
			egui::Popup::menu(&response)
				.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
				.show(|ui| self.remote_menu(ui, state, &peers));
		}
	}

	/// Access | Control, and who it is for when more than one other person is in the call.
	fn remote_menu(&mut self, ui: &mut egui::Ui, state: &client_core::State, peers: &[Id]) {
		let remote = &mut self.remote;
		let chosen = remote
			.chosen
			.filter(|chosen| peers.contains(chosen))
			.or(peers.first().copied());
		remote.chosen = chosen;
		let Some(chosen) = chosen else {
			return;
		};
		ui.set_width(320.0);
		let colors = design::palette(ui);
		ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
		let name = |user| peer_name(state, user);
		let mut picked = None;
		ui.horizontal(|ui| {
			for (give, title, detail) in [
				(false, "ACCESS", "Ask to control their PC"),
				(true, "CONTROL", "Let them control yours"),
			] {
				if choice_tile(ui, give, title, detail, &colors).clicked() {
					picked = Some(give);
				}
			}
		});
		if peers.len() > 1 {
			ui.add_space(2.0);
			ui.label(design::eyebrow(ui, "WHO", colors.muted));
			for &peer in peers {
				let selected = peer == chosen;
				if design::radio_row(ui, selected, &name(peer), None).clicked() {
					remote.chosen = Some(peer);
				}
			}
		} else {
			ui.label(
				egui::RichText::new(format!("With {}", name(chosen)))
					.size(12.0)
					.color(colors.muted),
			);
		}
		let Some(give) = picked else {
			return;
		};
		let name = name(chosen);
		let now = ui.input(|input| input.time);
		remote.phase = if give {
			Phase::Setup {
				peer: chosen,
				name,
				give: true,
				step: Step::Permission,
				grant: Grant::default(),
			}
		} else {
			remote.requests.push(Request::Ask(chosen));
			Phase::Waiting {
				peer: chosen,
				name,
				give: false,
				since: now,
				connecting: false,
			}
		};
		remote.sheet = Sheet::Phase;
		ui.close();
	}

	/// The dialogs in the main window, the notification and the controller's window.
	pub fn show_remote(&mut self, ctx: &egui::Context, state: &client_core::State) {
		let now = ctx.input(|input| input.time);
		if let Phase::Waiting {
			name,
			since,
			connecting: false,
			..
		} = &self.remote.phase
			&& now - since > ANSWER_SECS
		{
			let name = name.clone();
			self.remote.end(Request::Cancel);
			self.toasts.push(
				design::Level::Info,
				format!("{name} did not answer the remote control request"),
			);
		}
		if matches!(self.remote.phase, Phase::Waiting { .. }) {
			ctx.request_repaint_after(std::time::Duration::from_millis(500));
		}
		self.remote_notification(ctx, state);
		self.remote_viewer(ctx);
		if self.window_hidden {
			return;
		}
		match self.remote.sheet {
			Sheet::None => {}
			Sheet::Phase => self.remote_phase_sheet(ctx, state),
			Sheet::Access => self.remote_access_sheet(ctx),
			Sheet::Keys => self.remote_keys_sheet(ctx),
		}
	}

	fn remote_phase_sheet(&mut self, ctx: &egui::Context, state: &client_core::State) {
		let now = ctx.input(|input| input.time);
		let remote = &mut self.remote;
		match &mut remote.phase {
			Phase::Waiting {
				name,
				give,
				since,
				connecting,
				..
			} => {
				let (name, give, since, connecting) = (name.clone(), *give, *since, *connecting);
				let title = if give {
					"Giving control"
				} else {
					"Asking for access"
				};
				let mut cancel = false;
				let response = dialog::Dialog::new("remote-waiting", title)
					.width(400.0)
					.show(ctx, |body| {
						body.content(|ui| {
							waiting_scene(ui, &name, now - since, connecting, give);
						});
						body.footer(|ui| {
							cancel =
								dialog::action(ui, "Cancel", dialog::Action::Neutral).clicked();
						});
					});
				// Closing only hides the window; the call bar button brings it back.
				if response.close {
					remote.sheet = Sheet::None;
				}
				if cancel {
					remote.end(Request::Cancel);
				}
				ctx.request_repaint();
			}
			Phase::Incoming {
				peer,
				name,
				give,
				since,
				opened,
			} => {
				if !*opened {
					return;
				}
				let (peer, name, give, since) = (*peer, name.clone(), *give, *since);
				let mut answer = None;
				let user = state_user(state, peer);
				let response = dialog::Dialog::new("remote-answer", "Remote control")
					.width(420.0)
					.show(ctx, |body| {
						body.content(|ui| {
							answer_card(
								ui,
								&mut self.avatars,
								user,
								state.demo,
								&name,
								give,
								(now - since) / ANSWER_SECS,
							);
						});
						body.footer(|ui| {
							if dialog::action(ui, "Accept", dialog::Action::Primary).clicked() {
								answer = Some(true);
							}
							if dialog::action(ui, "Decline", dialog::Action::Neutral).clicked() {
								answer = Some(false);
							}
						});
					});
				if response.close {
					answer = Some(false);
				}
				ctx.request_repaint();
				match answer {
					Some(true) if give => {
						remote.requests.push(Request::Answer(true));
						remote.phase = Phase::Viewing(Box::new(Viewing::new(peer, name)));
						remote.sheet = Sheet::None;
					}
					Some(true) => {
						remote.requests.push(Request::Answer(true));
						remote.phase = Phase::Setup {
							peer,
							name,
							give: false,
							step: Step::Permission,
							grant: Grant::default(),
						};
					}
					Some(false) => remote.end(Request::Answer(false)),
					None => {}
				}
			}
			Phase::Setup {
				peer,
				name,
				give,
				step,
				grant,
			} => {
				let title = if *give {
					format!("Let {name} control your PC")
				} else {
					format!("Allow {name} to control your PC")
				};
				let mut back = false;
				let mut next = false;
				let ready = match step {
					Step::Permission => grant.permission.is_some(),
					Step::Key => remote.keys.pause.is_some() && remote.binding.is_none(),
					Step::Warning => true,
				};
				let finishes =
					*step == Step::Warning || (*step == Step::Key && remote.keys.skip_warning);
				let response = dialog::Dialog::new("remote-setup", title)
					.width(460.0)
					.show(ctx, |body| {
						body.content(|ui| match step {
							Step::Permission => permission_step(ui, grant, name),
							Step::Key => {
								key_step(ui, remote.keys.pause.as_ref(), &mut remote.binding, name)
							}
							Step::Warning => warning_step(
								ui,
								remote.keys.pause.as_ref(),
								&mut remote.keys.skip_warning,
								&mut remote.keys_changed,
								name,
							),
						});
						body.footer(|ui| {
							let label = if finishes { "Finish" } else { "Next" };
							next = ui
								.add_enabled_ui(ready, |ui| {
									dialog::action(ui, label, dialog::Action::Primary)
								})
								.inner
								.clicked();
							let label = if *step == Step::Permission {
								"Cancel"
							} else {
								"Back"
							};
							back = dialog::action(ui, label, dialog::Action::Neutral).clicked();
						});
					});
				if *step == Step::Key && remote.binding.is_none() && remote.keys.pause.is_none() {
					remote.binding = Some(Bind::Pause);
				}
				if back {
					remote.binding = None;
					match step {
						Step::Permission => {
							// An offer sent nothing yet; an accepted request is connecting.
							if !*give {
								remote.requests.push(Request::End);
							}
							remote.phase = Phase::Idle;
							remote.sheet = Sheet::None;
						}
						Step::Key => *step = Step::Permission,
						Step::Warning => *step = Step::Key,
					}
				} else if next && ready {
					remote.binding = None;
					match step {
						Step::Permission => *step = Step::Key,
						Step::Key if !remote.keys.skip_warning => *step = Step::Warning,
						_ => {
							let (peer, name, grant, give) = (*peer, name.clone(), *grant, *give);
							if give {
								remote.requests.push(Request::Give(peer, grant));
								remote.phase = Phase::Waiting {
									peer,
									name,
									give: true,
									since: now,
									connecting: false,
								};
							} else {
								remote.requests.push(Request::Start(grant));
								remote.phase = Phase::Hosting {
									peer,
									name,
									grant,
									connected: false,
								};
								remote.sheet = Sheet::None;
							}
						}
					}
				} else if response.close {
					// Escape or a click outside steps back once instead of throwing the setup away.
					remote.sheet = Sheet::None;
				}
			}
			_ => remote.sheet = Sheet::None,
		}
	}

	/// The one in control: change what the controller may do, pause, or end.
	fn remote_access_sheet(&mut self, ctx: &egui::Context) {
		let remote = &mut self.remote;
		let Phase::Hosting { name, grant, .. } = &mut remote.phase else {
			remote.sheet = Sheet::None;
			return;
		};
		let before = *grant;
		let mut end = false;
		let pause = remote.keys.pause.as_ref().map(|key| key.name.clone());
		let response = dialog::Dialog::new("remote-access", "Access")
			.subtitle(format!("What {name} can do on your PC"))
			.width(420.0)
			.show(ctx, |body| {
				body.content(|ui| {
					for (permission, label, detail) in [
						(Some(Permission::Full), "Full control", "Keyboard and mouse"),
						(
							Some(Permission::Keyboard),
							"Keyboard only",
							"Typing and key presses",
						),
						(
							Some(Permission::Mouse),
							"Mouse only",
							"Pointer, clicks and scrolling",
						),
						(None, "Disable", "Ends control"),
					] {
						if design::radio_row(
							ui,
							grant.permission == permission,
							label,
							Some(detail),
						)
						.clicked()
						{
							grant.permission = permission;
						}
					}
					ui.add_space(4.0);
					design::switch(
						ui,
						"Share PC audio",
						Some("They hear what your PC plays"),
						&mut grant.audio,
					);
					ui.add_space(4.0);
					let label = if grant.paused {
						"Resume control"
					} else {
						"Pause control"
					};
					if dialog::action(ui, label, dialog::Action::Outline).clicked() {
						grant.paused = !grant.paused;
					}
					if let Some(pause) = &pause {
						ui.label(
							egui::RichText::new(format!(
								"{pause} pauses and resumes from anywhere"
							))
							.size(12.0)
							.color(design::palette(ui).muted),
						);
					}
				});
				body.footer(|ui| {
					end = dialog::action(ui, "End control", dialog::Action::Danger).clicked();
				});
			});
		if response.close {
			remote.sheet = Sheet::None;
		}
		if end || grant.permission.is_none() {
			remote.end(Request::End);
		} else if *grant != before {
			let grant = *grant;
			remote.requests.push(Request::Change(grant));
		}
	}

	/// The keys this device remembers: the pause key and the label key.
	fn remote_keys_sheet(&mut self, ctx: &egui::Context) {
		let remote = &mut self.remote;
		let response = dialog::Dialog::new("remote-keys", "Remote control keys")
			.width(420.0)
			.show(ctx, |body| {
				body.content(|ui| {
					for (bind, title, detail) in [
						(
							Bind::Overlay,
							"Hide the control label",
							"While you control someone's PC",
						),
						(Bind::Pause, "Pause key", "While someone controls yours"),
					] {
						let key = match bind {
							Bind::Pause => &mut remote.keys.pause,
							Bind::Overlay => &mut remote.keys.overlay,
						};
						if key_row(ui, title, detail, key, &mut remote.binding, bind) {
							remote.keys_changed = true;
						}
					}
					ui.add_space(2.0);
					if design::checkbox(
						ui,
						&mut remote.keys.skip_warning,
						"Skip the pause key warning",
					)
					.changed()
					{
						remote.keys_changed = true;
					}
				});
				body.footer(|ui| {
					if dialog::action(ui, "Done", dialog::Action::Primary).clicked() {
						remote.sheet = Sheet::None;
					}
				});
			});
		if response.close || remote.sheet == Sheet::None {
			remote.sheet = Sheet::None;
			remote.binding = None;
		}
	}

	/// Our own notification, bottom right above the taskbar: who asks, a countdown, and a
	/// click that opens the answer in Leech. It declines when the countdown runs out.
	fn remote_notification(&mut self, ctx: &egui::Context, state: &client_core::State) {
		let Phase::Incoming {
			peer,
			name,
			give,
			since,
			opened: false,
		} = &self.remote.phase
		else {
			return;
		};
		let (peer, name, give, since) = (*peer, name.clone(), *give, *since);
		let now = ctx.input(|input| input.time);
		if now - since > NOTICE_SECS {
			self.remote.end(Request::Answer(false));
			self.toasts.push(
				design::Level::Info,
				format!("Remote control from {name} declined: no answer in 15 seconds"),
			);
			return;
		}
		let size = egui::vec2(372.0, 104.0);
		// ponytail: assumes the main window's monitor starts at the desktop origin with a
		// 48 point taskbar below; a work-area query would place it exactly on other monitors.
		let monitor = ctx
			.input(|input| input.viewport().monitor_size)
			.unwrap_or(egui::vec2(1920.0, 1080.0));
		let position = monitor - size - egui::vec2(16.0, 16.0 + 48.0);
		let builder = egui::ViewportBuilder::default()
			.with_title(format!("{name} · remote control"))
			.with_position(position.to_pos2())
			.with_inner_size(size)
			.with_resizable(false)
			.with_transparent(true)
			.with_decorations(false)
			.with_taskbar(false)
			.with_active(false)
			.with_window_level(egui::WindowLevel::AlwaysOnTop);
		let user = state_user(state, peer).cloned();
		let mut clicked = None;
		ctx.show_viewport_immediate(
			egui::ViewportId::from_hash_of("remote-notification"),
			builder,
			|ui, _| {
				clicked = notification_card(
					ui,
					&mut self.avatars,
					user.as_ref(),
					state.demo,
					&name,
					give,
					now - since,
				);
			},
		);
		ctx.request_repaint();
		match clicked {
			Some(true) => {
				if let Phase::Incoming { opened, .. } = &mut self.remote.phase {
					*opened = true;
				}
				self.remote.sheet = Sheet::Phase;
				self.remote.requests.push(Request::Focus);
			}
			Some(false) => self.remote.end(Request::Answer(false)),
			None => {}
		}
	}

	/// The controller's window: fullscreen on the other screen while in control, a small
	/// window that only shows it after Escape. Double-click returns to control.
	fn remote_viewer(&mut self, ctx: &egui::Context) {
		let Phase::Viewing(viewing) = &mut self.remote.phase else {
			return;
		};
		let overlay_hidden = self.remote.overlay_hidden;
		let windowed = self.remote.windowed;
		let parent =
			ctx.input(|input| input.viewport().outer_rect)
				.unwrap_or(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(1280.0, 800.0),
				));
		let now = ctx.input(|input| input.time);
		if viewing.opened_at.is_nan() {
			viewing.opened_at = now;
		}
		let builder = egui::ViewportBuilder::default()
			.with_title(format!("{}'s PC", viewing.name))
			.with_position(parent.min + egui::vec2(40.0, 40.0))
			.with_inner_size([960.0, 540.0])
			.with_min_inner_size([320.0, 180.0])
			.with_fullscreen(viewing.full && !self.remote.windowed)
			.with_taskbar(true);
		let mut close = false;
		ctx.show_viewport_immediate(
			egui::ViewportId::from_hash_of("remote-viewer"),
			builder,
			|ui, _| {
				close = viewer_ui(ui, viewing, overlay_hidden, windowed, now);
			},
		);
		if close {
			self.remote.end(Request::End);
		}
	}
}

fn state_user(state: &client_core::State, user: Id) -> Option<&model::User> {
	let channel = state.voice.active.as_ref()?.channel;
	crate::voice::participant_user(state, channel, user)
}

fn peer_name(state: &client_core::State, user: Id) -> String {
	state_user(state, user).map_or_else(|| "Someone".to_owned(), |user| user.name.clone())
}

/// One of the menu's two choices, as a tile with a glyph, a title and what it does.
fn choice_tile(
	ui: &mut egui::Ui,
	give: bool,
	title: &str,
	detail: &str,
	colors: &design::Palette,
) -> egui::Response {
	let width = (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
	let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 96.0), egui::Sense::click());
	let hover = design::hover_motion(ui, &response);
	let press = design::press_motion(ui, &response);
	let rect = design::pressed_rect(rect, press);
	ui.painter().rect(
		rect,
		10,
		colors.raised.lerp_to_gamma(colors.hover, hover),
		egui::Stroke::new(1.0, colors.border.lerp_to_gamma(colors.accent, hover * 0.8)),
		egui::StrokeKind::Inside,
	);
	let glyph = egui::Rect::from_center_size(
		rect.center_top() + egui::vec2(0.0, 26.0),
		egui::Vec2::splat(24.0),
	);
	if give {
		paint_give_glyph(ui.painter(), glyph, colors.accent);
	} else {
		paint_remote_glyph(ui.painter(), glyph, colors.accent);
	}
	ui.painter().text(
		rect.center_top() + egui::vec2(0.0, 54.0),
		egui::Align2::CENTER_CENTER,
		title,
		egui::FontId::new(13.0, design::semibold_family(ui.ctx())),
		colors.text_strong,
	);
	ui.painter().text(
		rect.center_top() + egui::vec2(0.0, 74.0),
		egui::Align2::CENTER_CENTER,
		detail,
		egui::FontId::proportional(11.5),
		colors.muted,
	);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, title));
	response
}

/// Words that cycle under the waiting animation, a few seconds each.
const WAITING_WORDS: [&str; 6] = [
	"Knocking",
	"Waiting for a yes",
	"Ringing their notification",
	"Holding the door",
	"Warming up the link",
	"Still waiting",
];

/// Two screens and a signal travelling between them while the other side answers.
fn waiting_scene(ui: &mut egui::Ui, name: &str, elapsed: f64, connecting: bool, give: bool) {
	let colors = design::palette(ui);
	let (rect, _) = ui.allocate_exact_size(
		egui::vec2(ui.available_width(), 132.0),
		egui::Sense::hover(),
	);
	let t = elapsed as f32;
	let center = rect.center() - egui::vec2(0.0, 6.0);
	let left = center - egui::vec2(92.0, 0.0);
	let right = center + egui::vec2(92.0, 0.0);
	// Rings breathe out from the screen being asked.
	let target = if give { left } else { right };
	for ring in 0..3 {
		let phase = ((t / 2.4) + ring as f32 / 3.0).fract();
		ui.painter().circle_stroke(
			target,
			26.0 + 34.0 * phase,
			egui::Stroke::new(1.5, colors.accent.gamma_multiply(0.45 * (1.0 - phase))),
		);
	}
	for (at, them) in [(left, !give), (right, give)] {
		let screen = egui::Rect::from_center_size(at, egui::vec2(58.0, 40.0));
		ui.painter().rect(
			screen,
			6,
			colors.raised,
			egui::Stroke::new(1.5, if them { colors.accent } else { colors.border }),
			egui::StrokeKind::Inside,
		);
		ui.painter().line_segment(
			[
				screen.center_bottom() + egui::vec2(-10.0, 9.0),
				screen.center_bottom() + egui::vec2(10.0, 9.0),
			],
			egui::Stroke::new(2.0, colors.border),
		);
		ui.painter().line_segment(
			[
				screen.center_bottom(),
				screen.center_bottom() + egui::vec2(0.0, 9.0),
			],
			egui::Stroke::new(2.0, colors.border),
		);
		ui.painter().text(
			screen.center(),
			egui::Align2::CENTER_CENTER,
			if them {
				initial(name)
			} else {
				"You".to_owned()
			},
			egui::FontId::new(
				if them { 15.0 } else { 11.0 },
				design::semibold_family(ui.ctx()),
			),
			if them { colors.accent } else { colors.muted },
		);
	}
	// Dots run along the line, faster once connecting.
	let speed = if connecting { 0.9 } else { 1.6 };
	let (from, to) = (left + egui::vec2(36.0, 0.0), right - egui::vec2(36.0, 0.0));
	for dot in 0..5 {
		let phase = ((t / speed) + dot as f32 / 5.0).fract();
		let eased = egui::emath::easing::cubic_in_out(phase);
		let at = from.lerp(to, eased);
		let fade = (phase * std::f32::consts::PI).sin();
		ui.painter()
			.circle_filled(at, 2.5 + 1.0 * fade, colors.accent.gamma_multiply(fade));
	}
	// The phrase with a light sweeping across it, and the seconds in the open.
	let word = if connecting {
		"Connecting"
	} else {
		WAITING_WORDS[((elapsed / 3.2) as usize).min(WAITING_WORDS.len() - 1)]
	};
	let text = format!("{word}…");
	let font = egui::FontId::new(14.0, design::medium_family(ui.ctx()));
	let galley = ui
		.painter()
		.layout_no_wrap(text.clone(), font.clone(), colors.muted);
	let origin = egui::pos2(
		rect.center().x - galley.size().x * 0.5,
		rect.bottom() - 22.0,
	);
	let width = galley.size().x;
	ui.painter().galley(origin, galley, colors.muted);
	let sweep = ((t / 1.8).fract() * 1.6 - 0.3) * width;
	let band = egui::Rect::from_min_size(
		egui::pos2(origin.x + sweep - 18.0, origin.y - 2.0),
		egui::vec2(36.0, 22.0),
	);
	let bright = ui.painter().layout_no_wrap(text, font, colors.text_strong);
	ui.painter()
		.with_clip_rect(band)
		.galley(origin, bright, colors.text_strong);
	let detail = if connecting {
		format!("{name} said yes")
	} else if give {
		format!("{name} decides · {}s", elapsed as u32)
	} else {
		format!("{name} has to accept · {}s", elapsed as u32)
	};
	ui.painter().text(
		egui::pos2(rect.center().x, rect.bottom() - 2.0),
		egui::Align2::CENTER_BOTTOM,
		detail,
		egui::FontId::proportional(11.5),
		colors.muted.gamma_multiply(0.8),
	);
}

fn initial(name: &str) -> String {
	name.chars()
		.find(|c| c.is_alphanumeric())
		.map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
}

fn avatar(
	ui: &mut egui::Ui,
	avatars: &mut crate::avatars::Avatars,
	user: Option<&model::User>,
	demo: bool,
	name: &str,
	size: f32,
) {
	match user {
		Some(user) => {
			avatars.show_quiet(ui, user, size, demo);
		}
		None => {
			let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
			design::paint_avatar(ui, name, size, rect);
		}
	}
}

/// The answer in Leech, in the notification's style: who, what they want, a draining bar.
fn answer_card(
	ui: &mut egui::Ui,
	avatars: &mut crate::avatars::Avatars,
	user: Option<&model::User>,
	demo: bool,
	name: &str,
	give: bool,
	spent: f64,
) {
	let colors = design::palette(ui);
	ui.horizontal(|ui| {
		avatar(ui, avatars, user, demo, name, 56.0);
		ui.add_space(6.0);
		ui.vertical(|ui| {
			ui.add_space(4.0);
			ui.label(design::semibold(
				ui,
				if give {
					format!("{name} offers you control of their PC")
				} else {
					format!("{name} wants to control your PC")
				},
				16.0,
			));
			ui.label(
				egui::RichText::new(if give {
					"You will see their screen fullscreen and steer it. Escape gives Leech back to you; the Windows key stops."
				} else {
					"Next you choose keyboard, mouse or both, and a key that pauses them at once."
				})
				.size(12.5)
				.color(colors.muted),
			);
		});
	});
	ui.add_space(10.0);
	drain_bar(ui, (1.0 - spent).clamp(0.0, 1.0) as f32, colors.accent);
}

fn drain_bar(ui: &mut egui::Ui, left: f32, color: egui::Color32) {
	let (rect, _) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
	ui.painter()
		.rect_filled(rect, 2, color.gamma_multiply(0.15));
	ui.painter().rect_filled(
		egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * left, rect.height())),
		2,
		color,
	);
}

/// The notification's surface. Returns `Some(true)` when clicked, `Some(false)` when
/// dismissed with its ×.
fn notification_card(
	ui: &mut egui::Ui,
	avatars: &mut crate::avatars::Avatars,
	user: Option<&model::User>,
	demo: bool,
	name: &str,
	give: bool,
	elapsed: f64,
) -> Option<bool> {
	let colors = design::palette(ui);
	let full = ui.max_rect();
	// Slides in from the right and fades, like the system's own.
	let enter = egui::emath::easing::cubic_out((elapsed / 0.32).clamp(0.0, 1.0) as f32);
	let rect = full
		.shrink(6.0)
		.translate(egui::vec2(40.0 * (1.0 - enter), 0.0));
	ui.set_opacity(enter);
	let surface = ui.interact(
		full,
		egui::Id::unique("remote-notification-card"),
		egui::Sense::click(),
	);
	let hover = design::hover_motion(ui, &surface);
	ui.painter().add(
		egui::epaint::RectShape::filled(rect, 14, colors.chat.to_opaque()).with_blur_width(0.0),
	);
	ui.painter().rect_stroke(
		rect,
		14,
		egui::Stroke::new(
			1.0,
			colors
				.border
				.lerp_to_gamma(colors.accent, 0.5 + 0.5 * hover),
		),
		egui::StrokeKind::Inside,
	);
	// The accent edge on the left names it as Leech's.
	ui.painter().rect_filled(
		egui::Rect::from_min_size(
			rect.min + egui::vec2(0.0, 18.0),
			egui::vec2(3.0, rect.height() - 36.0),
		),
		2,
		colors.accent,
	);
	let avatar_rect =
		egui::Rect::from_min_size(rect.min + egui::vec2(18.0, 22.0), egui::Vec2::splat(48.0));
	let mut child = ui.new_child(egui::UiBuilder::new().max_rect(avatar_rect));
	avatar(&mut child, avatars, user, demo, name, 48.0);
	let text_left = avatar_rect.right() + 14.0;
	let ring_center = egui::pos2(rect.right() - 40.0, rect.center().y + 4.0);
	let wrap = ring_center.x - 30.0 - text_left;
	let title = ui.painter().layout(
		if give {
			format!("{name} offers you control of their PC")
		} else {
			format!("{name} wants to control your PC")
		},
		egui::FontId::new(14.0, design::semibold_family(ui.ctx())),
		colors.text_strong,
		wrap,
	);
	let title_height = title.size().y;
	ui.painter().galley(
		egui::pos2(text_left, rect.top() + 22.0),
		title,
		colors.text_strong,
	);
	ui.painter().text(
		egui::pos2(text_left, rect.top() + 28.0 + title_height),
		egui::Align2::LEFT_TOP,
		"Click to answer in Leech",
		egui::FontId::proportional(12.0),
		colors.muted.lerp_to_gamma(colors.accent_text, hover),
	);
	// The countdown: a draining ring around the seconds left.
	let left = (NOTICE_SECS - elapsed).max(0.0);
	let fraction = (left / NOTICE_SECS) as f32;
	let radius = 16.0;
	ui.painter().circle_stroke(
		ring_center,
		radius,
		egui::Stroke::new(3.0, colors.accent.gamma_multiply(0.18)),
	);
	let steps = 48;
	let points = (0..=steps)
		.map(|i| {
			let angle = -std::f32::consts::FRAC_PI_2
				+ std::f32::consts::TAU * fraction * i as f32 / steps as f32;
			ring_center + radius * egui::vec2(angle.cos(), angle.sin())
		})
		.collect::<Vec<_>>();
	let urgent = if left < 5.0 {
		colors.warning
	} else {
		colors.accent
	};
	ui.painter()
		.add(egui::Shape::line(points, egui::Stroke::new(3.0, urgent)));
	ui.painter().text(
		ring_center,
		egui::Align2::CENTER_CENTER,
		format!("{}", left.ceil() as u32),
		egui::FontId::new(13.0, design::semibold_family(ui.ctx())),
		colors.text_strong,
	);
	// The × declines without opening Leech.
	let close_rect = egui::Rect::from_center_size(
		egui::pos2(rect.right() - 16.0, rect.top() + 16.0),
		egui::Vec2::splat(20.0),
	);
	let close = ui.interact(
		close_rect,
		egui::Id::unique("remote-notification-close"),
		egui::Sense::click(),
	);
	let close_hover = design::hover_motion(ui, &close);
	if close_hover > 0.0 {
		ui.painter().circle_filled(
			close_rect.center(),
			10.0,
			colors.hover.gamma_multiply(close_hover),
		);
	}
	crate::icons::paint(
		ui.painter(),
		Icon::Close,
		egui::Rect::from_center_size(close_rect.center(), egui::Vec2::splat(10.0)),
		colors.muted,
	);
	if close.clicked() {
		Some(false)
	} else if surface.clicked() {
		Some(true)
	} else {
		None
	}
}

/// Step one: keyboard, mouse or both, and the sound.
fn permission_step(ui: &mut egui::Ui, grant: &mut Grant, name: &str) {
	let colors = design::palette(ui);
	ui.label(
		egui::RichText::new(format!("Choose what {name} can do. You can change it any time by right-clicking the remote control button."))
			.size(12.5)
			.color(colors.muted),
	);
	ui.add_space(6.0);
	ui.horizontal(|ui| {
		ui.spacing_mut().item_spacing.x = 8.0;
		let width = (ui.available_width() - 16.0) / 3.0;
		for permission in [Permission::Keyboard, Permission::Mouse, Permission::Full] {
			let selected = grant.permission == Some(permission);
			let (rect, response) =
				ui.allocate_exact_size(egui::vec2(width, 92.0), egui::Sense::click());
			let hover = design::hover_motion(ui, &response);
			let shown = ui.ctx().animate_bool_with_time(
				egui::Id::unique(("remote-permission", permission as u8)),
				selected,
				0.14,
			);
			ui.painter().rect(
				rect,
				10,
				colors
					.raised
					.lerp_to_gamma(colors.accent.gamma_multiply(0.16), shown)
					.lerp_to_gamma(colors.hover, hover * (1.0 - shown)),
				egui::Stroke::new(
					1.0 + shown * 0.5,
					colors.border.lerp_to_gamma(colors.accent, shown),
				),
				egui::StrokeKind::Inside,
			);
			let glyph = egui::Rect::from_center_size(
				rect.center_top() + egui::vec2(0.0, 32.0),
				egui::vec2(40.0, 26.0),
			);
			let tint = colors.muted.lerp_to_gamma(colors.accent, shown);
			match permission {
				Permission::Keyboard => paint_keyboard(ui.painter(), glyph, tint),
				Permission::Mouse => paint_mouse(ui.painter(), glyph, tint),
				Permission::Full => {
					paint_keyboard(
						ui.painter(),
						glyph
							.translate(egui::vec2(-7.0, 0.0))
							.shrink2(egui::vec2(4.0, 2.0)),
						tint,
					);
					paint_mouse(
						ui.painter(),
						glyph.translate(egui::vec2(15.0, 3.0)).shrink(4.0),
						tint,
					);
				}
			}
			ui.painter().text(
				rect.center_bottom() - egui::vec2(0.0, 20.0),
				egui::Align2::CENTER_CENTER,
				permission.label(),
				egui::FontId::new(12.5, design::medium_family(ui.ctx())),
				colors.text_strong,
			);
			if response.clicked() {
				grant.permission = Some(permission);
			}
			response.widget_info(|| {
				egui::WidgetInfo::selected(
					egui::Role::RadioButton,
					true,
					selected,
					permission.label(),
				)
			});
		}
	});
	ui.add_space(8.0);
	design::switch(
		ui,
		"Share PC audio",
		Some("They hear what your PC plays"),
		&mut grant.audio,
	);
}

/// Step two: one key that pauses the controller from anywhere.
fn key_step(ui: &mut egui::Ui, key: Option<&Key>, binding: &mut Option<Bind>, name: &str) {
	let colors = design::palette(ui);
	ui.label(
		egui::RichText::new(format!("Bind one key. Pressing it freezes {name}'s control at once, even while a game has focus; press it again to resume."))
			.size(12.5)
			.color(colors.muted),
	);
	ui.add_space(10.0);
	let listening = *binding == Some(Bind::Pause);
	let response = ui
		.vertical_centered(|ui| big_keycap(ui, key.map(|key| key.name.as_str()), listening))
		.inner;
	if response.clicked() {
		*binding = Some(Bind::Pause);
	}
	ui.add_space(6.0);
	ui.vertical_centered(|ui| {
		ui.label(
			egui::RichText::new(if listening {
				"Press a key"
			} else {
				"Click the key to change it"
			})
			.size(12.0)
			.color(colors.muted),
		);
	});
}

/// Step three: remember the key.
fn warning_step(
	ui: &mut egui::Ui,
	key: Option<&Key>,
	skip: &mut bool,
	changed: &mut bool,
	name: &str,
) {
	let colors = design::palette(ui);
	let key = key.map_or("your pause key", |key| key.name.as_str());
	design::notice(
		ui,
		design::Level::Warning,
		&format!(
			"Remember {key}. It is the fastest way to stop {name} if something goes wrong: it works before anything else, wherever you are."
		),
	);
	ui.add_space(10.0);
	ui.vertical_centered(|ui| big_keycap(ui, Some(key), false));
	ui.add_space(10.0);
	if design::checkbox(ui, skip, "I will remember it · don't warn me again").changed() {
		*changed = true;
	}
	let _ = colors;
}

/// A large key, pulsing while it listens.
fn big_keycap(ui: &mut egui::Ui, name: Option<&str>, listening: bool) -> egui::Response {
	let colors = design::palette(ui);
	let font = egui::FontId::new(20.0, design::semibold_family(ui.ctx()));
	let text = if listening {
		"…"
	} else {
		name.unwrap_or("Set")
	};
	let galley = ui
		.painter()
		.layout_no_wrap(text.to_owned(), font, colors.text_strong);
	let size = egui::vec2((galley.size().x + 44.0).max(72.0), 64.0);
	let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
	let pulse = if listening {
		ui.ctx().request_repaint();
		0.5 + 0.5 * (ui.input(|input| input.time) * 5.0).sin() as f32
	} else {
		0.0
	};
	let hover = design::hover_motion(ui, &response);
	let press = design::press_motion(ui, &response);
	let face = rect.translate(egui::vec2(0.0, -3.0 + 2.0 * press));
	ui.painter()
		.rect_filled(rect.translate(egui::vec2(0.0, 2.0)), 10, colors.border);
	ui.painter().rect(
		face,
		10,
		colors.raised.lerp_to_gamma(colors.hover, hover),
		egui::Stroke::new(
			1.0 + pulse,
			colors.border.lerp_to_gamma(
				colors.accent,
				if listening { 0.5 + 0.5 * pulse } else { 0.0 },
			),
		),
		egui::StrokeKind::Inside,
	);
	ui.painter().galley(
		face.center() - galley.size() * 0.5,
		galley,
		colors.text_strong,
	);
	response
}

/// One remembered key: what it does, its keycap (click to bind) and a clear action.
fn key_row(
	ui: &mut egui::Ui,
	title: &str,
	detail: &str,
	key: &mut Option<Key>,
	binding: &mut Option<Bind>,
	bind: Bind,
) -> bool {
	let colors = design::palette(ui);
	let mut changed = false;
	ui.horizontal(|ui| {
		ui.vertical(|ui| {
			ui.label(design::medium(ui, title, 14.0));
			ui.label(egui::RichText::new(detail).size(12.0).color(colors.muted));
		});
		ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
			if key.is_some() && *binding != Some(bind) && design::text_action(ui, "Clear").clicked()
			{
				*key = None;
				changed = true;
			}
			let listening = *binding == Some(bind);
			let text = if listening {
				"Press a key…".to_owned()
			} else {
				key.as_ref()
					.map_or_else(|| "Not set".to_owned(), |key| key.name.clone())
			};
			let (rect, response) =
				ui.allocate_exact_size(egui::vec2(120.0, 32.0), egui::Sense::click());
			let hover = design::hover_motion(ui, &response);
			ui.painter().rect(
				rect,
				6,
				if listening {
					colors.accent.gamma_multiply(0.18)
				} else {
					colors.raised.lerp_to_gamma(colors.hover, hover)
				},
				egui::Stroke::new(
					if listening { 1.5 } else { 1.0 },
					if listening {
						colors.accent
					} else {
						colors.border
					},
				),
				egui::StrokeKind::Inside,
			);
			ui.painter().text(
				rect.center(),
				egui::Align2::CENTER_CENTER,
				text,
				egui::FontId::new(12.5, design::medium_family(ui.ctx())),
				if key.is_some() || listening {
					colors.text_strong
				} else {
					colors.muted
				},
			);
			if response.clicked() {
				*binding = if listening { None } else { Some(bind) };
			}
		});
	});
	ui.add_space(6.0);
	changed
}

/// The controller's window. Returns true when it asks to close, which ends control.
fn viewer_ui(
	ui: &mut egui::Ui,
	viewing: &mut Viewing,
	overlay_hidden: bool,
	windowed: bool,
	now: f64,
) -> bool {
	let rect = ui.max_rect();
	ui.painter().rect_filled(rect, 0, egui::Color32::BLACK);
	let (close, focused, fullscreen) = ui.input(|input| {
		(
			input.viewport().close_requested(),
			input.viewport().focused == Some(true),
			input.viewport().fullscreen == Some(true),
		)
	});
	if viewing.shown_full != Some(viewing.full) {
		if viewing.shown_full.is_some() && !windowed {
			ui.ctx()
				.send_viewport_cmd(egui::ViewportCommand::Fullscreen(viewing.full));
			if viewing.full {
				ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
			}
		}
		viewing.shown_full = Some(viewing.full);
	}
	// Only the other screen, at its own shape: never the encoder's bars, never stretched.
	let [x, y, width, height] = viewing.content;
	let uv = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, height));
	let screen = egui::vec2(viewing.screen[0] as f32, viewing.screen[1] as f32);
	let content = match viewing.texture {
		Some(texture) if screen.x <= 0.0 || screen.y <= 0.0 => {
			crate::voice::fit_rect(rect, texture.size * uv.size())
		}
		_ => crate::voice::fit_rect(rect, screen),
	};
	if let Some(texture) = viewing.texture {
		ui.painter()
			.image(texture.id, content, uv, egui::Color32::WHITE);
	}
	let steering = viewing.full
		&& fullscreen
		&& focused
		&& viewing.started
		&& !viewing.paused
		&& viewing.permission.is_some();
	viewing.capturing = steering;
	let mouse = steering
		&& matches!(
			viewing.permission,
			Some(Permission::Mouse | Permission::Full)
		);
	// A pointer hidden for a moment (Windows hides it while typing) does not lock this one.
	let since_id = egui::Id::unique("remote-relative-since");
	let since = if viewing.relative {
		ui.data_mut(|data| *data.get_temp_mut_or_insert_with(since_id, || now))
	} else {
		ui.data_mut(|data| data.remove::<f64>(since_id));
		now
	};
	if viewing.relative && now - since < 0.25 {
		ui.ctx()
			.request_repaint_after(std::time::Duration::from_millis(260));
	}
	let lock = mouse && viewing.relative && now - since >= 0.25;
	let locked_id = egui::Id::unique("remote-pointer-locked");
	let was_locked = ui
		.data(|data| data.get_temp::<bool>(locked_id))
		.unwrap_or(false);
	if lock != was_locked {
		ui.data_mut(|data| data.insert_temp(locked_id, lock));
		ui.ctx()
			.send_viewport_cmd(egui::ViewportCommand::CursorGrab(if lock {
				egui::CursorGrab::Confined
			} else {
				egui::CursorGrab::None
			}));
		ui.ctx()
			.send_viewport_cmd(egui::ViewportCommand::CursorVisible(!lock));
	}
	if mouse {
		collect_pointer(ui, viewing, content);
	} else if !viewing.held.is_empty() {
		for button in std::mem::take(&mut viewing.held) {
			viewing.inputs.push(MouseInput::Button {
				button,
				down: false,
			});
		}
	}
	let colors = design::palette(ui);
	if !viewing.full {
		// The small window only shows; a double-click goes back to control.
		let surface = ui.interact(
			rect,
			egui::Id::unique("remote-viewer-surface"),
			egui::Sense::click(),
		);
		if surface.double_clicked() {
			viewing.full = true;
		}
		let hint = egui::Rect::from_center_size(
			egui::pos2(rect.center().x, rect.bottom() - 24.0),
			egui::vec2(236.0, 28.0),
		);
		ui.painter()
			.rect_filled(hint, 14, egui::Color32::from_black_alpha(170));
		ui.painter().text(
			hint.center(),
			egui::Align2::CENTER_CENTER,
			"Double-click to take control",
			egui::FontId::proportional(12.5),
			egui::Color32::WHITE,
		);
	}
	let center_card = |ui: &egui::Ui, title: &str, detail: &str, spin: bool| {
		ui.painter()
			.rect_filled(rect, 0, egui::Color32::from_black_alpha(150));
		let card = egui::Rect::from_center_size(
			rect.center(),
			egui::vec2(360.0, if spin { 120.0 } else { 92.0 }),
		);
		ui.painter().rect_filled(
			card,
			14,
			egui::Color32::from_rgba_unmultiplied(20, 20, 24, 235),
		);
		let mut y = card.top() + 24.0;
		if spin {
			let t = now as f32;
			for dot in 0..3 {
				let phase = (t * 1.4 - dot as f32 * 0.18).rem_euclid(1.0);
				let lift = (phase * std::f32::consts::TAU).sin().max(0.0) * 5.0;
				ui.painter().circle_filled(
					egui::pos2(card.center().x - 14.0 + 14.0 * dot as f32, y + 6.0 - lift),
					3.5,
					colors.accent,
				);
			}
			y += 30.0;
			ui.ctx().request_repaint();
		}
		ui.painter().text(
			egui::pos2(card.center().x, y + 6.0),
			egui::Align2::CENTER_CENTER,
			title,
			egui::FontId::new(16.0, design::semibold_family(ui.ctx())),
			egui::Color32::WHITE,
		);
		ui.painter().text(
			egui::pos2(card.center().x, y + 32.0),
			egui::Align2::CENTER_CENTER,
			detail,
			egui::FontId::proportional(12.5),
			egui::Color32::from_gray(190),
		);
	};
	if !viewing.connected && viewing.texture.is_none() {
		center_card(
			ui,
			"Connecting…",
			&format!("Reaching {}'s PC", viewing.name),
			true,
		);
	} else if !viewing.started {
		center_card(
			ui,
			&format!("{} is setting up", viewing.name),
			"Control starts when they finish",
			true,
		);
	} else if viewing.paused {
		center_card(
			ui,
			&format!("{} paused your control", viewing.name),
			"You can watch; control comes back when they resume",
			false,
		);
	}
	if viewing.full && !overlay_hidden {
		// What this control allows, small at the top right.
		let text = format!(
			"{} · {}'s PC",
			viewing.permission.map_or("No control", Permission::label),
			viewing.name
		);
		let galley = ui.painter().layout_no_wrap(
			text,
			egui::FontId::new(12.0, design::medium_family(ui.ctx())),
			egui::Color32::WHITE,
		);
		let pill = egui::Rect::from_min_size(
			egui::pos2(
				rect.right() - galley.size().x - 28.0 - 12.0,
				rect.top() + 12.0,
			),
			galley.size() + egui::vec2(28.0, 12.0),
		);
		ui.painter()
			.rect_filled(pill, 12, egui::Color32::from_black_alpha(150));
		ui.painter().circle_filled(
			pill.left_center() + egui::vec2(10.0, 0.0),
			3.0,
			if viewing.paused {
				colors.warning
			} else {
				colors.positive
			},
		);
		ui.painter().galley(
			pill.min + egui::vec2(18.0, 6.0),
			galley,
			egui::Color32::WHITE,
		);
	}
	// How to get out, for the first seconds of control.
	let since = (now - viewing.opened_at) as f32;
	if viewing.full && since < 6.0 {
		let fade = (1.0 - (since - 4.5).max(0.0) / 1.5).clamp(0.0, 1.0);
		let text = "Esc shows Leech · Windows key stops · Shift+Esc sends Esc";
		let galley = ui.painter().layout_no_wrap(
			text.to_owned(),
			egui::FontId::proportional(13.0),
			egui::Color32::WHITE.gamma_multiply(fade),
		);
		let pill = egui::Rect::from_center_size(
			egui::pos2(rect.center().x, rect.top() + 34.0),
			galley.size() + egui::vec2(32.0, 16.0),
		);
		ui.painter().rect_filled(
			pill,
			16,
			egui::Color32::from_black_alpha((170.0 * fade) as u8),
		);
		ui.painter().galley(
			pill.center() - galley.size() * 0.5,
			galley,
			egui::Color32::WHITE,
		);
		ui.ctx().request_repaint();
	}
	close
}

/// Turns the window's pointer events into input for the other screen.
fn collect_pointer(ui: &egui::Ui, viewing: &mut Viewing, content: egui::Rect) {
	let to_screen = |pos: egui::Pos2| {
		let at = (pos - content.min) / content.size();
		(at.x.clamp(0.0, 1.0), at.y.clamp(0.0, 1.0))
	};
	let inside = |pos: egui::Pos2| content.contains(pos);
	let mut last = ui.input(|input| input.pointer.latest_pos());
	ui.input(|input| {
		for event in &input.events {
			match event {
				egui::Event::PointerMoved(pos) => {
					last = Some(*pos);
					let (x, y) = to_screen(*pos);
					viewing.inputs.push(MouseInput::Move { x, y, dx: 0, dy: 0 });
				}
				// Raw motion always goes along: the other PC uses it whenever its pointer is
				// hidden, even before this window hears about it.
				egui::Event::MouseMoved(delta) => {
					let (x, y) = last.map_or((0.5, 0.5), to_screen);
					viewing.inputs.push(MouseInput::Move {
						x,
						y,
						dx: delta.x.round() as i32,
						dy: delta.y.round() as i32,
					});
				}
				egui::Event::PointerButton {
					pos,
					button,
					pressed,
					..
				} => {
					let button = match button {
						egui::PointerButton::Primary => MouseButton::Left,
						egui::PointerButton::Secondary => MouseButton::Right,
						egui::PointerButton::Middle => MouseButton::Middle,
						egui::PointerButton::Extra1 => MouseButton::Back,
						egui::PointerButton::Extra2 => MouseButton::Forward,
					};
					let held = viewing.held.contains(&button);
					if *pressed && (inside(*pos) || viewing.relative) && !held {
						let (x, y) = to_screen(*pos);
						viewing.inputs.push(MouseInput::Move { x, y, dx: 0, dy: 0 });
						viewing
							.inputs
							.push(MouseInput::Button { button, down: true });
						viewing.held.push(button);
					} else if !*pressed && held {
						viewing.inputs.push(MouseInput::Button {
							button,
							down: false,
						});
						viewing.held.retain(|held| *held != button);
					}
				}
				egui::Event::MouseWheel { unit, delta, .. } => {
					let scale = match unit {
						egui::MouseWheelUnit::Line => 120.0,
						egui::MouseWheelUnit::Point => 120.0 / 50.0,
						egui::MouseWheelUnit::Page => 360.0,
					};
					let (dx, dy) = (
						(delta.x * scale).round() as i32,
						(delta.y * scale).round() as i32,
					);
					if dx != 0 || dy != 0 {
						viewing.inputs.push(MouseInput::Wheel { dx, dy });
					}
				}
				_ => {}
			}
		}
	});
}

/// Remote control: a screen with a pointer reaching into it, on Phosphor's grid.
pub(crate) fn paint_remote_glyph(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
	let unit = rect.width() / 256.0;
	let at = |x: f32, y: f32| rect.min + egui::vec2(x, y) * unit;
	let stroke = egui::Stroke::new(20.0 * unit, color);
	// The screen, open at the bottom right where the pointer sits.
	painter.add(egui::Shape::line(
		vec![
			at(150.0, 180.0),
			at(40.0, 180.0),
			at(28.0, 168.0),
			at(28.0, 52.0),
			at(40.0, 40.0),
			at(216.0, 40.0),
			at(228.0, 52.0),
			at(228.0, 110.0),
		],
		stroke,
	));
	painter.line_segment([at(88.0, 220.0), at(150.0, 220.0)], stroke);
	painter.line_segment([at(112.0, 180.0), at(112.0, 220.0)], stroke);
	// The pointer: an arrowhead notched at the back, as two convex halves.
	for half in [
		[at(160.0, 120.0), at(244.0, 156.0), at(204.0, 168.0)],
		[at(160.0, 120.0), at(204.0, 168.0), at(192.0, 210.0)],
	] {
		painter.add(egui::Shape::convex_polygon(
			half.to_vec(),
			color,
			egui::Stroke::NONE,
		));
	}
}

/// Giving control: a screen with an arrow leaving it.
fn paint_give_glyph(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
	let unit = rect.width() / 256.0;
	let at = |x: f32, y: f32| rect.min + egui::vec2(x, y) * unit;
	let stroke = egui::Stroke::new(20.0 * unit, color);
	painter.add(egui::Shape::line(
		vec![
			at(228.0, 120.0),
			at(228.0, 168.0),
			at(216.0, 180.0),
			at(40.0, 180.0),
			at(28.0, 168.0),
			at(28.0, 52.0),
			at(40.0, 40.0),
			at(120.0, 40.0),
		],
		stroke,
	));
	painter.line_segment([at(88.0, 220.0), at(168.0, 220.0)], stroke);
	painter.line_segment([at(128.0, 180.0), at(128.0, 220.0)], stroke);
	painter.line_segment([at(120.0, 116.0), at(224.0, 28.0)], stroke);
	painter.add(egui::Shape::line(
		vec![at(164.0, 28.0), at(224.0, 28.0), at(224.0, 84.0)],
		stroke,
	));
}

fn paint_keyboard(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
	painter.rect_stroke(
		rect,
		4,
		egui::Stroke::new(1.8, color),
		egui::StrokeKind::Inside,
	);
	let inner = rect.shrink2(egui::vec2(6.0, 6.0));
	for row in 0..2 {
		for column in 0..5 {
			let at = egui::pos2(
				inner.left() + inner.width() * column as f32 / 4.0,
				inner.top() + inner.height() * 0.3 * row as f32,
			);
			painter.circle_filled(at, 1.4, color);
		}
	}
	painter.line_segment(
		[
			egui::pos2(inner.left() + inner.width() * 0.25, inner.bottom()),
			egui::pos2(inner.right() - inner.width() * 0.25, inner.bottom()),
		],
		egui::Stroke::new(1.8, color),
	);
}

fn paint_mouse(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
	let body = egui::Rect::from_center_size(
		rect.center(),
		egui::vec2(rect.height() * 0.68, rect.height()),
	);
	let radius = (body.width() * 0.5) as u8;
	painter.rect_stroke(
		body,
		radius,
		egui::Stroke::new(1.8, color),
		egui::StrokeKind::Inside,
	);
	painter.line_segment(
		[
			body.center_top() + egui::vec2(0.0, 3.0),
			body.center_top() + egui::vec2(0.0, body.height() * 0.32),
		],
		egui::Stroke::new(1.8, color),
	);
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn binding_fills_the_key_once() {
		let mut remote = Remote::default();
		remote.bound(Key {
			scan: 0x42,
			extended: false,
			name: "F8".into(),
		});
		assert!(remote.keys.pause.is_none() && !remote.keys_changed);
		remote.binding = Some(Bind::Pause);
		remote.bound(Key {
			scan: 0x42,
			extended: false,
			name: "F8".into(),
		});
		assert_eq!(remote.keys.pause.as_ref().map(|key| key.scan), Some(0x42));
		assert!(remote.keys_changed && remote.binding.is_none());
	}
}
