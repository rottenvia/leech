//! Remote control: one Leech user controls another's PC while both are in the same call.
//!
//! The call bar button offers **Access**: ask someone in the call to control their PC. They
//! see a notification of their own with a 15 second countdown, answer in Leech, and choose
//! keyboard, mouse or both, whether their PC's sound goes along, and one pause key. The one
//! in control then sees the other screen fullscreen over Leech, like a watched screen share.
//! Escape gives them Leech back without pausing anyone; the screen stays in the call, and a
//! double-click on it takes control again. A left click on the button ends control on either
//! side; a right click holds the keys and, for the one controlled, what the other may do.
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
	/// Stop waiting for an answer.
	Cancel,
	/// Answer the request (or an older Leech's offer) that came in.
	Answer(bool),
	/// The setup after accepting a request is done.
	Start(Grant),
	/// The one controlled changes what the controller may do.
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
	/// Fullscreen over Leech and in control; otherwise the screen only shows in the call.
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
	/// Buttons held down on the screen now.
	pub held: Vec<MouseButton>,
	/// Fullscreen, focused and allowed to steer: the keyboard goes there too.
	pub capturing: bool,
	/// When the session began, for the waiting dialog's seconds.
	since: f64,
	/// When control last went fullscreen, for the hint that fades out.
	full_since: f64,
	/// The call stage enlarged this screen once.
	staged: bool,
}

impl Viewing {
	pub fn new(peer: Id, name: String) -> Self {
		Self {
			peer,
			name,
			full: false,
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
			since: f64::NAN,
			full_since: f64::NAN,
			staged: false,
		}
	}

	/// Not steering: nothing is captured and every button held there goes up.
	fn release(&mut self) {
		self.capturing = false;
		for button in std::mem::take(&mut self.held) {
			self.inputs.push(MouseInput::Button {
				button,
				down: false,
			});
		}
	}
}

#[derive(Default)]
pub enum Phase {
	#[default]
	Idle,
	/// Waiting for `peer` to answer an Access request.
	Waiting {
		peer: Id,
		name: String,
		since: f64,
		/// They said yes; the connection is being made.
		connecting: bool,
	},
	/// Someone asks for access, or (`give`, from an older Leech) offers control of their PC.
	Incoming {
		peer: Id,
		name: String,
		give: bool,
		since: f64,
		/// The notification was clicked: the answer shows in Leech.
		opened: bool,
	},
	/// The one asked chooses what to allow before control starts.
	Setup {
		peer: Id,
		name: String,
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

/// The call bar button's popover.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Menu {
	#[default]
	None,
	/// Left click: the Access choice.
	Access,
	/// After Access with more than one other person in the call: whose PC.
	Who,
	/// Right click: pause and access for the one controlled, and the keys.
	Manage,
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
	/// A key pressed in Leech while binding, as (scan code, extended), for the desktop to
	/// name and bind: it arrives here when the keyboard hook did not catch it.
	pub typed: Option<(u16, bool)>,
	/// The label over the controlled screen is hidden.
	pub overlay_hidden: bool,
	/// The current phase's dialog (waiting, answer, setup) shows in the main window.
	pub dialog: bool,
	pub menu: Menu,
	/// The controller's view never makes the window fullscreen (the offline preview).
	pub windowed: bool,
	/// The window's fullscreen state before the controlled screen filled it.
	fullscreen_from: Option<bool>,
	/// The controller's pointer is confined and hidden over the controlled screen.
	pointer_locked: bool,
}

impl Remote {
	pub fn active(&self) -> bool {
		!matches!(self.phase, Phase::Idle)
	}

	/// A window of its own is up: the notification.
	pub fn shows_windows(&self) -> bool {
		matches!(self.phase, Phase::Incoming { opened: false, .. })
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

	pub fn viewing(&mut self) -> Option<&mut Viewing> {
		match &mut self.phase {
			Phase::Viewing(viewing) => Some(viewing),
			_ => None,
		}
	}

	/// Whose screen this device shows in the call while it controls one.
	pub(crate) fn viewing_peer(&self) -> Option<Id> {
		match &self.phase {
			Phase::Viewing(viewing) => Some(viewing.peer),
			_ => None,
		}
	}

	/// The pause key on the controlled PC: pauses or resumes the controller, and says which.
	pub fn toggle_pause(&mut self) -> Option<String> {
		let Phase::Hosting { grant, name, .. } = &mut self.phase else {
			return None;
		};
		grant.paused = !grant.paused;
		let grant = *grant;
		let text = if grant.paused {
			format!("{name}'s control is paused")
		} else {
			format!("{name}'s control resumed")
		};
		self.requests.push(Request::Change(grant));
		Some(text)
	}

	fn ask(&mut self, peer: Id, name: String, now: f64) {
		self.requests.push(Request::Ask(peer));
		self.phase = Phase::Waiting {
			peer,
			name,
			since: now,
			connecting: false,
		};
		self.menu = Menu::None;
		self.dialog = true;
	}

	fn end(&mut self, request: Request) {
		self.requests.push(request);
		self.phase = Phase::Idle;
		self.dialog = false;
		self.menu = Menu::None;
		self.binding = None;
	}

	/// The pause key's name for advice, or how to get one.
	fn pause_name(&self) -> String {
		self.keys
			.pause
			.as_ref()
			.map_or_else(|| "not set".to_owned(), |key| key.name.clone())
	}
}

impl crate::MessagingUi {
	/// The call bar's remote control button, between screen share and More.
	pub(crate) fn remote_control(&mut self, ui: &mut egui::Ui, state: &client_core::State) {
		let peers = state.remote_peers();
		let colors = design::palette(ui);
		let now = ui.input(|input| input.time);
		let response = {
			let remote = &mut self.remote;
			let pending = match &remote.phase {
				Phase::Waiting { .. } | Phase::Incoming { .. } | Phase::Setup { .. } => true,
				Phase::Viewing(viewing) => !viewing.started,
				Phase::Idle | Phase::Hosting { .. } => false,
			};
			let (label, hint, tint, lit) = match &remote.phase {
				Phase::Idle if peers.is_empty() => (
					"Remote control",
					"Remote control needs someone else in the call",
					None,
					0.0,
				),
				Phase::Idle => (
					"Remote control",
					"Ask to control someone's PC · right-click for keys",
					None,
					0.0,
				),
				_ if pending => (
					"Remote control",
					"Show the remote control request",
					Some(colors.accent),
					0.5 + 0.5 * (now * std::f64::consts::TAU / 1.6).sin() as f32,
				),
				Phase::Hosting { grant, .. } => (
					"End remote control",
					"End control · right-click to pause or change access",
					Some(if grant.paused {
						colors.warning
					} else {
						colors.accent
					}),
					1.0,
				),
				_ => (
					"End remote control",
					"End control · right-click for keys · double-click their screen to take control",
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
			let open = remote.menu != Menu::None;
			if let Some(tint) = tint {
				ui.painter().rect_filled(
					plate,
					8,
					tint.gamma_multiply(0.14 + 0.16 * lit + 0.08 * hover),
				);
				if lit < 1.0 {
					ui.ctx().request_repaint();
				}
			} else if hover > 0.0 || open {
				ui.painter().rect_filled(
					plate,
					8,
					egui::Color32::from_white_alpha(
						(28.0 * hover.max(if open { 0.8 } else { 0.0 }) + 14.0 * press) as u8,
					),
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
			// No tooltip over an open menu: it would cover the menu's first row.
			let response = if open {
				response
			} else {
				response.on_hover_text(hint)
			};
			if response.secondary_clicked() && enabled {
				remote.menu = if remote.menu == Menu::Manage {
					Menu::None
				} else {
					Menu::Manage
				};
			}
			if response.clicked() {
				match &mut remote.phase {
					Phase::Idle => {
						remote.menu = if matches!(remote.menu, Menu::Access | Menu::Who) {
							Menu::None
						} else {
							Menu::Access
						};
					}
					Phase::Hosting { .. } => remote.end(Request::End),
					Phase::Viewing(viewing) if viewing.started => remote.end(Request::End),
					Phase::Incoming { opened, .. } => {
						*opened = true;
						remote.dialog = true;
						remote.menu = Menu::None;
					}
					_ => {
						remote.dialog = true;
						remote.menu = Menu::None;
					}
				}
			}
			// A request that came in, or anyone leaving, closes the Access choice.
			if matches!(remote.menu, Menu::Access | Menu::Who)
				&& (!matches!(remote.phase, Phase::Idle) || peers.is_empty())
			{
				remote.menu = Menu::None;
			}
			response
		};
		let menu = self.remote.menu;
		if menu != Menu::None {
			let mut open = true;
			egui::Popup::menu(&response)
				.id(response.id.with(("remote-menu", menu as u8)))
				.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
				.open_bool(&mut open)
				.show(|ui| match menu {
					Menu::Access => self.remote_access_menu(ui, state, &peers, now),
					Menu::Who => self.remote_who_menu(ui, state, &peers, now),
					Menu::Manage => self.remote_manage_menu(ui),
					Menu::None => {}
				});
			if !open && self.remote.menu == menu {
				self.remote.menu = Menu::None;
			}
		}
		// A key binding started in the menu stops with it; the setup binds in its own dialog.
		if self.remote.menu != Menu::Manage
			&& self.remote.binding.is_some()
			&& !matches!(
				self.remote.phase,
				Phase::Setup {
					step: Step::Key,
					..
				}
			) {
			self.remote.binding = None;
		}
	}

	/// Left click: only Access. With one other person it asks them at once; with more, whose
	/// PC comes next, in a menu of its own.
	fn remote_access_menu(
		&mut self,
		ui: &mut egui::Ui,
		state: &client_core::State,
		peers: &[Id],
		now: f64,
	) {
		let detail = match peers {
			[peer] => format!("Ask to control {}'s PC", peer_name(state, *peer)),
			_ => "Ask to control someone's PC".to_owned(),
		};
		// Wide enough for the name; a very long one still ends in an ellipsis.
		let text = ui
			.painter()
			.layout_no_wrap(
				detail.clone(),
				egui::FontId::proportional(12.5),
				egui::Color32::WHITE,
			)
			.size()
			.x;
		ui.set_width((text + 72.0).clamp(272.0, 380.0));
		if access_row(ui, &detail).clicked() {
			match peers {
				[peer] => self.remote.ask(*peer, peer_name(state, *peer), now),
				_ => self.remote.menu = Menu::Who,
			}
		}
	}

	/// Whose PC: everyone else in the call, by avatar and name.
	fn remote_who_menu(
		&mut self,
		ui: &mut egui::Ui,
		state: &client_core::State,
		peers: &[Id],
		now: f64,
	) {
		ui.set_width(248.0);
		let colors = design::palette(ui);
		menu_heading(ui, "Access · whose PC?", None, &colors);
		for &peer in peers {
			let name = peer_name(state, peer);
			let user = state_user(state, peer);
			if person_row(ui, &mut self.avatars, user, state.demo, &name).clicked() {
				self.remote.ask(peer, name, now);
			}
		}
	}

	/// Right click: for the one controlled, pause and what the other may do; for the
	/// controller, taking control again; for everyone, the keys.
	fn remote_manage_menu(&mut self, ui: &mut egui::Ui) {
		ui.set_width(292.0);
		let (hosting, viewing) = (
			matches!(self.remote.phase, Phase::Hosting { .. }),
			matches!(self.remote.phase, Phase::Viewing(_)),
		);
		if hosting {
			self.remote_host_menu(ui);
		} else if viewing {
			self.remote_viewer_menu(ui);
		}
		let colors = design::palette(ui);
		let remote = &mut self.remote;
		let hosting = matches!(remote.phase, Phase::Hosting { .. });
		menu_heading(ui, "Keys", None, &colors);
		for (bind, title, detail) in [
			(Bind::Pause, "Pause key", "While someone controls your PC"),
			(
				Bind::Overlay,
				"Hide the control label",
				"While you control someone's PC",
			),
		] {
			let key = match bind {
				Bind::Pause => &mut remote.keys.pause,
				Bind::Overlay => &mut remote.keys.overlay,
			};
			// The pause key stays while it is the only way to stop the controller.
			let clearable = !(hosting && bind == Bind::Pause);
			if key_menu_row(ui, title, detail, key, &mut remote.binding, bind, clearable) {
				remote.keys_changed = true;
			}
		}
		if !remote.active()
			&& design::menu_check(
				ui,
				"Skip the pause key warning",
				&mut remote.keys.skip_warning,
			)
			.changed()
		{
			remote.keys_changed = true;
		}
		if matches!(remote.phase, Phase::Hosting { .. } | Phase::Viewing(_)) {
			ui.add_space(4.0);
			ui.horizontal(|ui| {
				ui.add_space(10.0);
				ui.label(
					egui::RichText::new("Left click on the button ends control")
						.size(12.0)
						.color(colors.muted),
				);
			});
			ui.add_space(2.0);
		}
	}

	/// Pause first, then what the controller may do: access changes only while paused.
	fn remote_host_menu(&mut self, ui: &mut egui::Ui) {
		let colors = design::palette(ui);
		let pause_name = self.remote.pause_name();
		let pause_key = self.remote.keys.pause.as_ref().map(|key| key.name.clone());
		let remote = &mut self.remote;
		let Phase::Hosting { name, grant, .. } = &mut remote.phase else {
			return;
		};
		let before = *grant;
		let state = match (grant.paused, grant.permission) {
			(true, _) => "Paused · they can only watch".to_owned(),
			(false, Some(permission)) => format!(
				"{} · {}",
				permission.label(),
				if grant.audio {
					"hears your PC"
				} else {
					"no sound"
				}
			),
			(false, None) => "No control".to_owned(),
		};
		status_header(
			ui,
			&format!("{name} controls your PC"),
			&state,
			if grant.paused {
				colors.warning
			} else {
				colors.positive
			},
		);
		let label = if grant.paused {
			"Resume control"
		} else {
			"Pause control"
		};
		if key_hint_item(ui, label, pause_key.as_deref()).clicked() {
			grant.paused = !grant.paused;
		}
		menu_heading(
			ui,
			"Access",
			(!grant.paused).then_some("Pause to change"),
			&colors,
		);
		let mut locked = false;
		for (permission, label) in [
			(Some(Permission::Full), "Full control"),
			(Some(Permission::Keyboard), "Keyboard only"),
			(Some(Permission::Mouse), "Mouse only"),
			(None, "Disable · ends control"),
		] {
			let response = menu_radio(
				ui,
				label,
				grant.permission == permission,
				!grant.paused,
				permission.is_none(),
			);
			if response.clicked() {
				if grant.paused {
					grant.permission = permission;
				} else {
					locked = true;
				}
			}
		}
		design::menu_check(ui, "Share PC audio", &mut grant.audio);
		let name = name.clone();
		let grant = *grant;
		if locked {
			self.toasts.push(
				design::Level::Warning,
				format!(
					"Pause {name}'s control before changing access · your pause key is {pause_name}"
				),
			);
		}
		if grant.permission.is_none() {
			self.remote.end(Request::End);
		} else if grant != before {
			self.remote.requests.push(Request::Change(grant));
		}
	}

	fn remote_viewer_menu(&mut self, ui: &mut egui::Ui) {
		let colors = design::palette(ui);
		let Phase::Viewing(viewing) = &mut self.remote.phase else {
			return;
		};
		let state = if !viewing.started {
			"Setting up".to_owned()
		} else if viewing.paused {
			format!("Paused by {}", viewing.name)
		} else {
			viewing
				.permission
				.map_or("No control", Permission::label)
				.to_owned()
		};
		status_header(
			ui,
			&format!("You control {}'s PC", viewing.name),
			&state,
			if viewing.paused || !viewing.started {
				colors.warning
			} else {
				colors.positive
			},
		);
		if viewing.started
			&& !viewing.full
			&& design::menu_item(ui, Some(Icon::Fullscreen), "Take control", false).clicked()
		{
			viewing.full = true;
			self.remote.menu = Menu::None;
		}
	}

	/// The dialogs in the main window, the notification, and the controlled screen's
	/// fullscreen state.
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
		if let Phase::Viewing(viewing) = &mut self.remote.phase {
			if viewing.since.is_nan() {
				viewing.since = now;
			}
			if !viewing.staged {
				viewing.staged = true;
				self.voice_focus = Some(crate::voice::StageFocus::Remote(viewing.peer));
			}
			if !viewing.full {
				viewing.release();
				viewing.full_since = f64::NAN;
			}
		}
		self.sync_remote_fullscreen(ctx);
		self.remote_notification(ctx, state);
		if self.window_hidden || !self.remote.dialog {
			return;
		}
		self.remote_dialog(ctx, state);
	}

	/// Fills the window while the controlled screen is fullscreen, and gives the window, its
	/// pointer and the media fullscreen back afterwards. Runs every frame, signed in or not,
	/// so control that ends with the account or while minimized still gives them back.
	pub fn sync_remote_fullscreen(&mut self, ctx: &egui::Context) {
		let full = matches!(&self.remote.phase, Phase::Viewing(viewing) if viewing.full);
		match (full, self.remote.fullscreen_from) {
			(true, None) => {
				// A fullscreen screen share gives way; leaving control restores what was there.
				let was = self.voice_fullscreen.as_ref().map_or_else(
					|| ctx.input(|input| input.viewport().fullscreen.unwrap_or(false)),
					|(_, _, previous, _)| *previous,
				);
				self.exit_voice_fullscreen();
				self.remote.fullscreen_from = Some(was);
				if !self.remote.windowed {
					self.voice_fullscreen_request = Some(true);
				}
				ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
			}
			(false, Some(was)) => {
				self.remote.fullscreen_from = None;
				if !self.remote.windowed {
					self.voice_fullscreen_request = Some(was);
				}
			}
			_ => {}
		}
		if !full && self.remote.pointer_locked {
			self.remote.pointer_locked = false;
			lock_pointer(ctx, false);
		}
	}

	/// Keys pressed in Leech itself: the binding in progress, and the pause key while this
	/// PC is controlled. The keyboard hook takes them first whenever it runs; these only
	/// arrive when it does not, so nothing happens twice. Runs before anything else reads
	/// the keyboard, so a bound key never also types or triggers a shortcut.
	pub(crate) fn capture_remote_keys(&mut self, ctx: &egui::Context) {
		let remote = &mut self.remote;
		let pause = remote
			.keys
			.pause
			.as_ref()
			.map(|key| (key.scan, key.extended));
		let hosting = matches!(remote.phase, Phase::Hosting { .. });
		if remote.binding.is_none() && !(hosting && pause.is_some()) {
			return;
		}
		let binding = remote.binding.is_some();
		let pressed = ctx.input_mut(|input| {
			let mut pressed = None;
			input.events.retain(|event| match event {
				egui::Event::Key {
					key,
					physical_key,
					pressed: true,
					repeat: false,
					..
				} if pressed.is_none() => {
					let key = physical_key.unwrap_or(*key);
					let hit = binding || scan_code(key).is_some_and(|code| Some(code) == pause);
					if hit {
						pressed = Some(key);
					}
					!hit
				}
				// The bound key's text and release stay out of the composer.
				egui::Event::Text(_) | egui::Event::Key { .. } => !(binding && pressed.is_some()),
				_ => true,
			});
			pressed
		});
		let Some(key) = pressed else {
			return;
		};
		if binding {
			if key == egui::Key::Escape {
				remote.binding = None;
			} else if let Some(code) = scan_code(key) {
				remote.typed = Some(code);
			}
		} else if let Some(text) = remote.toggle_pause() {
			self.toasts.push(design::Level::Info, text);
		}
		ctx.request_repaint();
	}

	fn remote_dialog(&mut self, ctx: &egui::Context, state: &client_core::State) {
		let now = ctx.input(|input| input.time);
		let remote = &mut self.remote;
		let waiting = match &remote.phase {
			Phase::Waiting {
				name,
				since,
				connecting,
				..
			} => Some((
				name.clone(),
				now - since,
				if *connecting {
					Wait::Connecting
				} else {
					Wait::Asking
				},
			)),
			Phase::Viewing(viewing) if !viewing.started => Some((
				viewing.name.clone(),
				now - viewing.since,
				if viewing.connected {
					Wait::SettingUp
				} else {
					Wait::Connecting
				},
			)),
			_ => None,
		};
		if let Some((name, elapsed, wait)) = waiting {
			let mut cancel = false;
			let response = dialog::Dialog::new("remote-waiting", "Asking for access")
				.width(400.0)
				.show(ctx, |body| {
					body.content(|ui| waiting_scene(ui, &name, elapsed, wait));
					body.footer(|ui| {
						cancel = dialog::action(ui, "Cancel", dialog::Action::Neutral).clicked();
					});
				});
			// Closing only hides the window; the call bar button brings it back.
			if response.close {
				remote.dialog = false;
			}
			if cancel {
				let request = if matches!(remote.phase, Phase::Waiting { .. }) {
					Request::Cancel
				} else {
					Request::End
				};
				remote.end(request);
			}
			ctx.request_repaint();
			return;
		}
		match &mut remote.phase {
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
					}
					Some(true) => {
						remote.requests.push(Request::Answer(true));
						remote.phase = Phase::Setup {
							peer,
							name,
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
				step,
				grant,
			} => {
				let title = format!("Allow {name} to control your PC");
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
						// The accepted request is connecting: cancelling tells them.
						Step::Permission => remote.end(Request::End),
						Step::Key => *step = Step::Permission,
						Step::Warning => *step = Step::Key,
					}
				} else if next && ready {
					remote.binding = None;
					match step {
						Step::Permission => *step = Step::Key,
						Step::Key if !remote.keys.skip_warning => *step = Step::Warning,
						_ => {
							let (peer, name, grant) = (*peer, name.clone(), *grant);
							remote.requests.push(Request::Start(grant));
							remote.phase = Phase::Hosting {
								peer,
								name,
								grant,
								connected: false,
							};
							remote.dialog = false;
						}
					}
				} else if response.close {
					// Escape or a click outside hides the setup; the button brings it back.
					remote.binding = None;
					remote.dialog = false;
				}
			}
			_ => remote.dialog = false,
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
				self.remote.dialog = true;
				self.remote.requests.push(Request::Focus);
			}
			Some(false) => self.remote.end(Request::Answer(false)),
			None => {}
		}
	}

	/// The controlled screen over all of Leech while in control, like a fullscreen screen
	/// share. Returns true while it owns the window.
	pub(crate) fn show_remote_fullscreen(&mut self, ctx: &egui::Context) -> bool {
		let overlay_hidden = self.remote.overlay_hidden;
		let Phase::Viewing(viewing) = &mut self.remote.phase else {
			return false;
		};
		if !viewing.full {
			return false;
		}
		let now = ctx.input(|input| input.time);
		if viewing.full_since.is_nan() {
			viewing.full_since = now;
		}
		let screen = ctx.content_rect();
		let id = egui::Id::unique("remote-fullscreen");
		let mut lock = false;
		let overlay = egui::Modal::new(id)
			.area(
				egui::Modal::default_area(id)
					.anchor(egui::Align2::LEFT_TOP, egui::Vec2::ZERO)
					.fade_in(false),
			)
			.backdrop_color(egui::Color32::BLACK)
			.frame(egui::Frame::NONE)
			.show(ctx, |ui| {
				ui.set_min_size(screen.size());
				let rect = egui::Rect::from_min_size(screen.min, screen.size());
				lock = controlled_screen(ui, viewing, rect, overlay_hidden, now);
			});
		// Escape reaches Leech only while nothing is captured (the hook takes it otherwise):
		// it gives Leech back and leaves the other side as it is.
		if overlay.should_close() {
			viewing.full = false;
			viewing.release();
		}
		let lock = lock && viewing.full;
		if lock != self.remote.pointer_locked {
			self.remote.pointer_locked = lock;
			lock_pointer(ctx, lock);
		}
		true
	}
}

/// Confines and hides the main window's pointer for a game that captured the other mouse.
fn lock_pointer(ctx: &egui::Context, lock: bool) {
	ctx.send_viewport_cmd_to(
		egui::ViewportId::ROOT,
		egui::ViewportCommand::CursorGrab(if lock {
			egui::CursorGrab::Confined
		} else {
			egui::CursorGrab::None
		}),
	);
	ctx.send_viewport_cmd_to(
		egui::ViewportId::ROOT,
		egui::ViewportCommand::CursorVisible(!lock),
	);
}

/// Where a key sits as a set-1 scan code and extended flag, the way the desktop's keyboard
/// hook reports it. Leech's own key events carry no scan codes; their physical keys follow
/// US positions, which is what scan codes name too, whatever the layout.
// ponytail: egui gives the numpad digits and Enter the main keys' names, so a numpad key
// bound here (only while the hook is off) is stored as the main key; the hook tells them apart.
pub(crate) fn scan_code(key: egui::Key) -> Option<(u16, bool)> {
	use egui::Key as K;
	let code = match key {
		K::Escape => 0x01,
		K::Num1 => 0x02,
		K::Num2 => 0x03,
		K::Num3 => 0x04,
		K::Num4 => 0x05,
		K::Num5 => 0x06,
		K::Num6 => 0x07,
		K::Num7 => 0x08,
		K::Num8 => 0x09,
		K::Num9 => 0x0A,
		K::Num0 => 0x0B,
		K::Minus => 0x0C,
		K::Equals => 0x0D,
		K::Backspace => 0x0E,
		K::Tab => 0x0F,
		K::Q => 0x10,
		K::W => 0x11,
		K::E => 0x12,
		K::R => 0x13,
		K::T => 0x14,
		K::Y => 0x15,
		K::U => 0x16,
		K::I => 0x17,
		K::O => 0x18,
		K::P => 0x19,
		K::OpenBracket => 0x1A,
		K::CloseBracket => 0x1B,
		K::Enter => 0x1C,
		K::ControlLeft => 0x1D,
		K::A => 0x1E,
		K::S => 0x1F,
		K::D => 0x20,
		K::F => 0x21,
		K::G => 0x22,
		K::H => 0x23,
		K::J => 0x24,
		K::K => 0x25,
		K::L => 0x26,
		K::Semicolon => 0x27,
		K::Quote => 0x28,
		K::Backtick => 0x29,
		K::ShiftLeft => 0x2A,
		K::Backslash => 0x2B,
		K::Z => 0x2C,
		K::X => 0x2D,
		K::C => 0x2E,
		K::V => 0x2F,
		K::B => 0x30,
		K::N => 0x31,
		K::M => 0x32,
		K::Comma => 0x33,
		K::Period => 0x34,
		K::Slash => 0x35,
		K::ShiftRight => 0x36,
		K::AltLeft => 0x38,
		K::Space => 0x39,
		K::CapsLock => 0x3A,
		K::F1 => 0x3B,
		K::F2 => 0x3C,
		K::F3 => 0x3D,
		K::F4 => 0x3E,
		K::F5 => 0x3F,
		K::F6 => 0x40,
		K::F7 => 0x41,
		K::F8 => 0x42,
		K::F9 => 0x43,
		K::F10 => 0x44,
		K::Pause => 0x45,
		K::ScrollLock => 0x46,
		K::IntlBackslash => 0x56,
		K::F11 => 0x57,
		K::F12 => 0x58,
		K::F13 => 0x64,
		K::F14 => 0x65,
		K::F15 => 0x66,
		K::F16 => 0x67,
		K::F17 => 0x68,
		K::F18 => 0x69,
		K::F19 => 0x6A,
		K::F20 => 0x6B,
		K::F21 => 0x6C,
		K::F22 => 0x6D,
		K::F23 => 0x6E,
		K::F24 => 0x76,
		_ => {
			let code = match key {
				K::ControlRight => 0x1D,
				K::PrintScreen => 0x37,
				K::AltRight => 0x38,
				K::NumLock => 0x45,
				K::Home => 0x47,
				K::ArrowUp => 0x48,
				K::PageUp => 0x49,
				K::ArrowLeft => 0x4B,
				K::ArrowRight => 0x4D,
				K::End => 0x4F,
				K::ArrowDown => 0x50,
				K::PageDown => 0x51,
				K::Insert => 0x52,
				K::Delete => 0x53,
				_ => return None,
			};
			return Some((code, true));
		}
	};
	Some((code, false))
}

fn state_user(state: &client_core::State, user: Id) -> Option<&model::User> {
	let channel = state.voice.active.as_ref()?.channel;
	crate::voice::participant_user(state, channel, user)
}

fn peer_name(state: &client_core::State, user: Id) -> String {
	state_user(state, user).map_or_else(|| "Someone".to_owned(), |user| user.name.clone())
}

/// Hover plate shared by the menu rows below, as [`design::menu_item`] draws it.
fn row_hover(ui: &egui::Ui, response: &egui::Response, rect: egui::Rect) -> f32 {
	let hover = design::hover_motion(ui, response);
	if hover > 0.0 {
		ui.painter().rect_filled(
			rect,
			6,
			design::palette(ui).text_strong.gamma_multiply(0.08 * hover),
		);
	}
	hover
}

/// The Access choice: the glyph on an accent plate, the title and what it does.
fn access_row(ui: &mut egui::Ui, detail: &str) -> egui::Response {
	let colors = design::palette(ui);
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 56.0), egui::Sense::click());
	let hover = row_hover(ui, &response, rect);
	let plate = egui::Rect::from_min_size(
		rect.left_center() + egui::vec2(8.0, -18.0),
		egui::Vec2::splat(36.0),
	);
	ui.painter()
		.rect_filled(plate, 9, colors.accent.gamma_multiply(0.16 + 0.08 * hover));
	paint_remote_glyph(ui.painter(), plate.shrink(8.0), colors.accent);
	let left = plate.right() + 12.0;
	ui.painter().text(
		egui::pos2(left, rect.center().y - 9.0),
		egui::Align2::LEFT_CENTER,
		"Access",
		egui::FontId::new(14.5, design::semibold_family(ui.ctx())),
		colors.text_strong,
	);
	let mut job = egui::text::LayoutJob::simple_singleline(
		detail.to_owned(),
		egui::FontId::proportional(12.5),
		colors.muted.lerp_to_gamma(colors.text, hover),
	);
	job.wrap.max_width = rect.right() - 8.0 - left;
	job.wrap.max_rows = 1;
	let galley = ui.painter().layout_job(job);
	ui.painter().galley(
		egui::pos2(left, rect.center().y + 10.0 - galley.size().y * 0.5),
		galley,
		colors.muted,
	);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Access"));
	response
}

/// One person to ask: avatar and name.
fn person_row(
	ui: &mut egui::Ui,
	avatars: &mut crate::avatars::Avatars,
	user: Option<&model::User>,
	demo: bool,
	name: &str,
) -> egui::Response {
	let colors = design::palette(ui);
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 44.0), egui::Sense::click());
	let hover = row_hover(ui, &response, rect);
	let face = egui::Rect::from_min_size(
		rect.left_center() + egui::vec2(8.0, -14.0),
		egui::Vec2::splat(28.0),
	);
	let mut child = ui.new_child(egui::UiBuilder::new().max_rect(face));
	avatar(&mut child, avatars, user, demo, name, 28.0);
	let mut job = egui::text::LayoutJob::simple_singleline(
		name.to_owned(),
		egui::FontId::new(14.0, design::medium_family(ui.ctx())),
		colors.text.lerp_to_gamma(colors.text_strong, hover),
	);
	job.wrap.max_width = rect.right() - 8.0 - (face.right() + 10.0);
	job.wrap.max_rows = 1;
	let galley = ui.painter().layout_job(job);
	ui.painter().galley(
		egui::pos2(face.right() + 10.0, rect.center().y - galley.size().y * 0.5),
		galley,
		colors.text,
	);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
	response
}

/// Who the session is with and how it stands, above the menu's actions.
fn status_header(ui: &mut egui::Ui, title: &str, detail: &str, dot: egui::Color32) {
	let colors = design::palette(ui);
	let (rect, _) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::hover());
	ui.painter()
		.circle_filled(egui::pos2(rect.left() + 14.0, rect.top() + 15.0), 4.0, dot);
	let mut job = egui::text::LayoutJob::simple_singleline(
		title.to_owned(),
		egui::FontId::new(14.0, design::semibold_family(ui.ctx())),
		colors.text_strong,
	);
	job.wrap.max_width = rect.width() - 34.0;
	job.wrap.max_rows = 1;
	let galley = ui.painter().layout_job(job);
	ui.painter().galley(
		egui::pos2(
			rect.left() + 26.0,
			rect.top() + 15.0 - galley.size().y * 0.5,
		),
		galley,
		colors.text_strong,
	);
	ui.painter().text(
		egui::pos2(rect.left() + 26.0, rect.top() + 34.0),
		egui::Align2::LEFT_CENTER,
		detail,
		egui::FontId::proportional(12.5),
		colors.muted,
	);
	ui.separator();
}

/// A small section title over menu rows, with an optional note at its right.
fn menu_heading(ui: &mut egui::Ui, title: &str, note: Option<&str>, colors: &design::Palette) {
	ui.add_space(4.0);
	let (rect, _) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
	ui.painter().text(
		egui::pos2(rect.left() + 10.0, rect.center().y),
		egui::Align2::LEFT_CENTER,
		crate::i18n::translate_if_key(title).to_uppercase(),
		egui::FontId::new(12.0, design::semibold_family(ui.ctx())),
		colors.muted,
	);
	if let Some(note) = note {
		ui.painter().text(
			rect.right_center() - egui::vec2(10.0, 0.0),
			egui::Align2::RIGHT_CENTER,
			note,
			egui::FontId::proportional(12.0),
			colors.warning,
		);
	}
}

/// A menu row with a keycap at its right: the key that does the same.
fn key_hint_item(ui: &mut egui::Ui, label: &str, key: Option<&str>) -> egui::Response {
	let colors = design::palette(ui);
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
	let hover = row_hover(ui, &response, rect);
	ui.painter().text(
		egui::pos2(rect.left() + 10.0, rect.center().y),
		egui::Align2::LEFT_CENTER,
		label,
		egui::FontId::new(14.0, design::medium_family(ui.ctx())),
		colors.text.lerp_to_gamma(colors.text_strong, hover),
	);
	if let Some(key) = key {
		keycap(
			ui,
			rect.right_center() - egui::vec2(8.0, 0.0),
			key,
			false,
			0.0,
		);
	}
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
	response
}

/// A small keycap whose right edge sits at `right`; returns its rect.
fn keycap(ui: &egui::Ui, right: egui::Pos2, text: &str, listening: bool, hover: f32) -> egui::Rect {
	let colors = design::palette(ui);
	let galley = ui.painter().layout_no_wrap(
		text.to_owned(),
		egui::FontId::new(12.5, design::medium_family(ui.ctx())),
		colors.text_strong,
	);
	let cap = egui::Rect::from_min_max(
		egui::pos2(right.x - (galley.size().x + 18.0).max(30.0), right.y - 12.0),
		egui::pos2(right.x, right.y + 12.0),
	);
	ui.painter()
		.rect_filled(cap.translate(egui::vec2(0.0, 1.5)), 6, colors.border);
	ui.painter().rect(
		cap,
		6,
		if listening {
			colors.accent.gamma_multiply(0.2)
		} else {
			colors.raised.lerp_to_gamma(colors.hover, hover)
		},
		egui::Stroke::new(
			1.0,
			if listening {
				colors.accent
			} else {
				colors.border
			},
		),
		egui::StrokeKind::Inside,
	);
	ui.painter().galley(
		cap.center() - galley.size() * 0.5,
		galley,
		colors.text_strong,
	);
	cap
}

/// One choice of what the controller may do, with a radio at its right. `locked` rows look
/// dimmed but still answer a click, so the menu can say why nothing changed.
fn menu_radio(
	ui: &mut egui::Ui,
	label: &str,
	selected: bool,
	locked: bool,
	danger: bool,
) -> egui::Response {
	let colors = design::palette(ui);
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
	response
		.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, label));
	let hover = row_hover(ui, &response, rect);
	let dim = if locked { 0.5 } else { 1.0 };
	let ink = if danger {
		colors.danger
	} else {
		colors.text.lerp_to_gamma(colors.text_strong, hover)
	};
	ui.painter().text(
		egui::pos2(rect.left() + 10.0, rect.center().y),
		egui::Align2::LEFT_CENTER,
		label,
		egui::FontId::new(14.0, design::medium_family(ui.ctx())),
		ink.gamma_multiply(dim),
	);
	let center = egui::pos2(rect.right() - 19.0, rect.center().y);
	if selected {
		ui.painter()
			.circle_filled(center, 9.0, colors.accent.gamma_multiply(dim));
		ui.painter()
			.circle_filled(center, 3.5, colors.accent_text.gamma_multiply(dim));
	} else {
		ui.painter().circle_stroke(
			center,
			8.25,
			egui::Stroke::new(
				1.5,
				colors
					.text_strong
					.gamma_multiply((0.32 + 0.23 * hover) * dim),
			),
		);
	}
	response
}

/// One remembered key in the menu: what it does, its keycap (click to bind, Escape to stop)
/// and a × that forgets it. Returns true when the key was cleared.
fn key_menu_row(
	ui: &mut egui::Ui,
	title: &str,
	detail: &str,
	key: &mut Option<Key>,
	binding: &mut Option<Bind>,
	bind: Bind,
	clearable: bool,
) -> bool {
	let colors = design::palette(ui);
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 44.0), egui::Sense::click());
	let listening = *binding == Some(bind);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, title));
	let hover = row_hover(ui, &response, rect);
	ui.painter().text(
		egui::pos2(rect.left() + 10.0, rect.center().y - 8.0),
		egui::Align2::LEFT_CENTER,
		title,
		egui::FontId::new(14.0, design::medium_family(ui.ctx())),
		colors.text.lerp_to_gamma(colors.text_strong, hover),
	);
	ui.painter().text(
		egui::pos2(rect.left() + 10.0, rect.center().y + 10.0),
		egui::Align2::LEFT_CENTER,
		detail,
		egui::FontId::proportional(12.0),
		colors.muted,
	);
	let mut right = rect.right_center() - egui::vec2(8.0, 0.0);
	let mut cleared = false;
	if key.is_some() && clearable && !listening {
		let clear =
			egui::Rect::from_center_size(right - egui::vec2(10.0, 0.0), egui::Vec2::splat(20.0));
		let clear_response = ui.interact(clear, response.id.with("clear"), egui::Sense::click());
		let over = design::hover_motion(ui, &clear_response);
		if over > 0.0 {
			ui.painter()
				.circle_filled(clear.center(), 10.0, colors.hover.gamma_multiply(over));
		}
		crate::icons::paint(
			ui.painter(),
			Icon::Close,
			egui::Rect::from_center_size(clear.center(), egui::Vec2::splat(9.0)),
			colors.muted.lerp_to_gamma(colors.text_strong, over),
		);
		clear_response.widget_info(|| {
			egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Clear {title}"))
		});
		if clear_response.clicked() {
			*key = None;
			cleared = true;
		}
		right.x = clear.left() - 4.0;
	}
	let text = if listening {
		"Press a key…".to_owned()
	} else {
		key.as_ref()
			.map_or_else(|| "Set".to_owned(), |key| key.name.clone())
	};
	keycap(ui, right, &text, listening, hover);
	if listening {
		ui.ctx().request_repaint();
	}
	if response.clicked() && !cleared {
		*binding = if listening { None } else { Some(bind) };
	}
	cleared
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

/// How far an Access request got.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wait {
	/// The notification is out.
	Asking,
	/// They said yes; the two PCs are finding each other.
	Connecting,
	/// Connected; they choose what you can do and their pause key.
	SettingUp,
}

/// Two screens and a signal travelling between them while the other side answers.
fn waiting_scene(ui: &mut egui::Ui, name: &str, elapsed: f64, wait: Wait) {
	let colors = design::palette(ui);
	let (rect, _) = ui.allocate_exact_size(
		egui::vec2(ui.available_width(), 158.0),
		egui::Sense::hover(),
	);
	let t = elapsed as f32;
	// Screens on top, then the phrase, then the detail: rings never reach the words.
	let center = egui::pos2(rect.center().x, rect.top() + 54.0);
	let left = center - egui::vec2(92.0, 0.0);
	let right = center + egui::vec2(92.0, 0.0);
	// You on the left, them on the right: the ask travels across and rings at their screen.
	for ring in 0..3 {
		let phase = ((t / 2.4) + ring as f32 / 3.0).fract();
		ui.painter().circle_stroke(
			right,
			26.0 + 24.0 * phase,
			egui::Stroke::new(1.5, colors.accent.gamma_multiply(0.45 * (1.0 - phase))),
		);
	}
	for (at, them) in [(left, false), (right, true)] {
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
	// Dots run along the line, faster once they said yes.
	let speed = if wait == Wait::Asking { 1.6 } else { 0.9 };
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
	let word = match wait {
		Wait::Asking => WAITING_WORDS[((elapsed / 3.2) as usize).min(WAITING_WORDS.len() - 1)],
		Wait::Connecting => "Connecting",
		Wait::SettingUp => "Almost there",
	};
	let text = format!("{word}…");
	let font = egui::FontId::new(14.0, design::medium_family(ui.ctx()));
	let galley = ui
		.painter()
		.layout_no_wrap(text.clone(), font.clone(), colors.muted);
	let origin = egui::pos2(rect.center().x - galley.size().x * 0.5, rect.top() + 114.0);
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
	let detail = match wait {
		Wait::Asking => format!("{name} has to accept · {}s", elapsed as u32),
		Wait::Connecting => format!("{name} said yes"),
		Wait::SettingUp => format!("{name} is choosing what you can do"),
	};
	ui.painter().text(
		egui::pos2(rect.center().x, rect.top() + 138.0),
		egui::Align2::CENTER_TOP,
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
		egui::RichText::new(format!("Choose what {name} can do. To change it later, pause their control, then right-click the remote control button."))
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

/// Where the other screen sits in `rect`: at its own shape, never the encoder's bars, never
/// stretched. Draws it there, rounded by `corner`.
pub(crate) fn paint_screen(
	ui: &egui::Ui,
	viewing: &Viewing,
	rect: egui::Rect,
	corner: u8,
) -> egui::Rect {
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
		ui.painter().add(
			egui::epaint::RectShape::filled(content, corner, egui::Color32::WHITE)
				.with_texture(texture.id, uv),
		);
	}
	content
}

/// Why the other screen cannot be steered yet, as a title and a detail, with a spinner while
/// something is still on its way.
pub(crate) fn screen_status(viewing: &Viewing) -> Option<(String, String, bool)> {
	if !viewing.connected && viewing.texture.is_none() {
		Some((
			"Connecting…".to_owned(),
			format!("Reaching {}'s PC", viewing.name),
			true,
		))
	} else if !viewing.started {
		Some((
			format!("{} is setting up", viewing.name),
			"Control starts when they finish".to_owned(),
			true,
		))
	} else if viewing.paused {
		Some((
			format!("{} paused your control", viewing.name),
			"You can watch; control comes back when they resume".to_owned(),
			false,
		))
	} else {
		None
	}
}

/// A dark card in the middle of the screen with a status; small screens get a smaller one.
pub(crate) fn paint_status_card(
	ui: &egui::Ui,
	rect: egui::Rect,
	(title, detail, spin): &(String, String, bool),
	now: f64,
) {
	let colors = design::palette(ui);
	let compact = rect.width() < 420.0 || rect.height() < 200.0;
	let width = (rect.width() - 24.0).min(if compact { 260.0 } else { 360.0 });
	let height = if compact { 64.0 } else { 92.0 } + if *spin { 26.0 } else { 0.0 };
	if width < 80.0 || rect.height() < height + 8.0 {
		return;
	}
	ui.painter()
		.rect_filled(rect, 0, egui::Color32::from_black_alpha(120));
	let card = egui::Rect::from_center_size(rect.center(), egui::vec2(width, height));
	ui.painter().rect_filled(
		card,
		14,
		egui::Color32::from_rgba_unmultiplied(20, 20, 24, 235),
	);
	let mut y = card.top() + if compact { 14.0 } else { 22.0 };
	if *spin {
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
		y += 26.0;
		ui.ctx().request_repaint();
	}
	let fit = |text: &str, font: egui::FontId, color: egui::Color32| {
		let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
		job.wrap.max_width = width - 24.0;
		job.wrap.max_rows = 1;
		ui.painter().layout_job(job)
	};
	let title = fit(
		title,
		egui::FontId::new(
			if compact { 14.0 } else { 16.0 },
			design::semibold_family(ui.ctx()),
		),
		egui::Color32::WHITE,
	);
	let title_height = title.size().y;
	ui.painter().galley(
		egui::pos2(card.center().x - title.size().x * 0.5, y),
		title,
		egui::Color32::WHITE,
	);
	let detail = fit(
		detail,
		egui::FontId::proportional(if compact { 11.5 } else { 12.5 }),
		egui::Color32::from_gray(190),
	);
	ui.painter().galley(
		egui::pos2(
			card.center().x - detail.size().x * 0.5,
			y + title_height + if compact { 4.0 } else { 8.0 },
		),
		detail,
		egui::Color32::from_gray(190),
	);
}

/// The controlled screen filling the window. Returns true when the pointer should be locked
/// (a game there captured the mouse).
fn controlled_screen(
	ui: &mut egui::Ui,
	viewing: &mut Viewing,
	rect: egui::Rect,
	overlay_hidden: bool,
	now: f64,
) -> bool {
	ui.painter().rect_filled(rect, 0, egui::Color32::BLACK);
	let (focused, fullscreen) = ui.input(|input| {
		(
			input.viewport().focused == Some(true),
			input.viewport().fullscreen == Some(true),
		)
	});
	let content = paint_screen(ui, viewing, rect, 0);
	let steering =
		fullscreen && focused && viewing.started && !viewing.paused && viewing.permission.is_some();
	if steering {
		viewing.capturing = true;
	} else {
		viewing.release();
	}
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
	if mouse {
		collect_pointer(ui, viewing, content);
	}
	let colors = design::palette(ui);
	if let Some(status) = screen_status(viewing) {
		paint_status_card(ui, rect, &status, now);
	}
	if !overlay_hidden {
		// What this control allows, small at the top right.
		let what = if !viewing.started {
			"Starting"
		} else if viewing.paused {
			"Paused"
		} else {
			viewing.permission.map_or("No control", Permission::label)
		};
		let text = format!("{what} · {}'s PC", viewing.name);
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
			if viewing.paused || !viewing.started {
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
	let since = (now - viewing.full_since) as f32;
	if since < 6.0 {
		let fade = (1.0 - (since - 4.5).max(0.0) / 1.5).clamp(0.0, 1.0);
		let text = "Esc returns to Leech · Windows key ends control · Shift+Esc sends Esc";
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
	lock
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

	#[test]
	fn keys_map_to_what_the_hook_sees() {
		assert_eq!(scan_code(egui::Key::F8), Some((0x42, false)));
		assert_eq!(scan_code(egui::Key::A), Some((0x1E, false)));
		assert_eq!(scan_code(egui::Key::ArrowUp), Some((0x48, true)));
		assert_eq!(scan_code(egui::Key::Delete), Some((0x53, true)));
		assert_eq!(scan_code(egui::Key::F24), Some((0x76, false)));
	}

	#[test]
	fn pause_flips_and_tells_the_other_side() {
		let mut remote = Remote::default();
		assert!(remote.toggle_pause().is_none() && remote.requests.is_empty());
		remote.phase = Phase::Hosting {
			peer: Id(7),
			name: "mafaka".into(),
			grant: Grant::default(),
			connected: true,
		};
		assert_eq!(
			remote.toggle_pause().as_deref(),
			Some("mafaka's control is paused")
		);
		assert_eq!(
			remote.toggle_pause().as_deref(),
			Some("mafaka's control resumed")
		);
		let paused: Vec<bool> = remote
			.requests
			.iter()
			.filter_map(|request| match request {
				Request::Change(grant) => Some(grant.paused),
				_ => None,
			})
			.collect();
		assert_eq!(paused, [true, false]);
	}
}
