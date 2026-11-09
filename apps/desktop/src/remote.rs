//! Remote control on the desktop: signals through hidden direct messages, the connection
//! and media through `discord_voice::remote`, and the keyboard through one low-level hook.
//! The interface in `ui::remote` draws each [`Phase`]; this module moves it along.
use crate::{egui, gpu_video::VideoTexture};
use client_core::{
	Command, State,
	remote::{Kind, Received, Signal},
};
use discord_voice::remote::{
	self as net, End, Endpoint, Host, HostConfig, HostStatus, Link, Offer, Viewer,
};
use model::Id;
use platform::keyboard_hook::{Hook, KeyEvent};
use platform::remote_input::helper::{self, Helper};
use std::{
	sync::{Arc, Mutex, mpsc},
	time::{Duration, Instant},
};
use ui::remote::{Admin, Grant, MouseButton, MouseInput, Permission, Phase, Request, Viewing};

/// How long hole punching may take.
const CONNECT: Duration = Duration::from_secs(10);
/// How often the controller tells the other PC what it holds.
const HELD_EVERY: Duration = Duration::from_millis(100);
/// The controller hears again that a click went nowhere after this long.
const NOTICE_EVERY: Duration = Duration::from_secs(30);
/// Escape held this long gives Leech back; a shorter press goes to the other PC.
const LEAVE_AFTER: Duration = Duration::from_secs(1);

const VK_ESCAPE: u16 = 0x1B;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;

type Gathered = mpsc::Receiver<Result<Endpoint, &'static str>>;
type Connected = mpsc::Receiver<Result<Link, &'static str>>;

/// What a session waits on.
enum Stage {
	/// The request or offer came in; nothing is gathered until it is accepted.
	Asked,
	Gathering(Gathered),
	/// This side's offer is out; the other side's is still to come.
	Ready(Endpoint),
	Connecting(Connected),
	/// Connected while the host's setup is still open.
	Linked(Link),
	Hosting(Host),
	/// Locked: the hook thread sends keys through it too.
	Viewing(Arc<Mutex<Viewer>>),
}

struct Session {
	id: u64,
	peer: Id,
	name: String,
	/// This PC is the one controlled.
	host: bool,
	stage: Stage,
	/// The signal this side sends once its offer is gathered.
	announce: Kind,
	theirs: Option<Offer>,
	/// The host's setup finished with this grant.
	grant: Option<Grant>,
	texture: VideoTexture,
	held_at: Instant,
	/// Refusal counts already shown (Leech itself, administrator apps) and when.
	refusals: [(u32, Option<Instant>); 2],
	/// This PC is controlled: the helper for programs running as administrator.
	elevation: Elevation,
}

/// The administrator helper on the controlled PC (`platform::remote_input::helper`): offered
/// once a session, when the controller first reaches a program running as administrator.
#[derive(Default)]
struct Elevation {
	/// Windows' prompt is up; the helper arrives here.
	asking: Option<mpsc::Receiver<Result<Helper, helper::Failure>>>,
	/// Refusals of administrator programs already seen.
	seen: u32,
	offered: bool,
	/// The helper ran at the last look.
	running: bool,
}

impl Elevation {
	/// Starts the helper through the UAC prompt, off the UI thread.
	fn start(&mut self, ctx: &egui::Context) {
		if self.asking.is_some() || self.running {
			return;
		}
		let (send, receive) = mpsc::sync_channel(1);
		let wake = ctx.clone();
		let started = std::thread::Builder::new()
			.name("admin-helper".into())
			.spawn(move || {
				let _ = send.send(Helper::start());
				wake.request_repaint();
			});
		if started.is_ok() {
			self.asking = Some(receive);
		}
	}

	/// Hands a started helper to the session and mirrors it in `admin`; returns a toast.
	fn update(
		&mut self,
		host: &Host,
		status: HostStatus,
		admin: &mut Admin,
		now: f64,
	) -> Option<(ui::design::Level, &'static str)> {
		let mut toast = None;
		if self.running && !status.admin {
			*admin = Admin::Blocked;
			toast = Some((
				ui::design::Level::Warning,
				"Admin apps are blocked again: the helper stopped",
			));
		}
		self.running = status.admin;
		let result = match self.asking.as_ref().map(mpsc::Receiver::try_recv) {
			Some(Ok(result)) => Some(result),
			Some(Err(mpsc::TryRecvError::Disconnected)) => Some(Err(helper::Failure::Failed)),
			Some(Err(mpsc::TryRecvError::Empty)) | None => None,
		};
		if let Some(result) = result {
			self.asking = None;
			let outcome = match result.map(|helper| host.use_helper(helper)) {
				Ok(true) => {
					// Watched from now on, even if it stops before the next look.
					self.running = true;
					*admin = Admin::Allowed;
					toast = Some((
						ui::design::Level::Success,
						"{name} can use apps that run as administrator until control ends",
					));
					"started"
				}
				Ok(false) => "session ended first",
				Err(helper::Failure::Declined) => "declined",
				Err(helper::Failure::Failed) => {
					toast = Some((
						ui::design::Level::Warning,
						"Admin apps could not be allowed: the helper did not start",
					));
					"failed"
				}
			};
			if *admin == Admin::Asking {
				*admin = Admin::Blocked;
			}
			client_core::journal::note("remote", format!("admin helper {outcome}"));
		}
		if status.refused_elevated != self.seen {
			self.seen = status.refused_elevated;
			if !self.offered && *admin == Admin::Blocked {
				self.offered = true;
				*admin = Admin::Offered(now);
			}
		}
		toast
	}
}

#[derive(Default)]
enum Mode {
	#[default]
	Pass,
	/// The next key binds.
	Bind,
	/// This PC is controlled: the pause key pauses, and holding the take-over key gives this
	/// PC's own mouse and keyboard back.
	Host {
		pause: Option<(u16, bool)>,
		take_over: Option<(u16, bool)>,
		hand: Option<net::Hand>,
	},
	/// This PC controls: keys go to the other one.
	View {
		viewer: Arc<Mutex<Viewer>>,
		overlay: Option<(u16, bool)>,
		keyboard: bool,
	},
}

enum Caught {
	Bound(u16, bool),
	CancelBind,
	Pause,
	Minimize,
	Stop,
	Overlay,
}

/// Shared between the hook thread and the interface.
#[derive(Default)]
struct Shared {
	mode: Mode,
	/// Keys swallowed on the way down, whose ups are swallowed too.
	swallowed: Vec<(u16, bool)>,
	/// Keys pressed on the other PC.
	forwarded: Vec<(u16, bool)>,
	caught: Vec<Caught>,
	/// The take-over key is down: its release hands control back.
	taking: Option<((u16, bool), net::Hand)>,
	/// When Escape went down on the other PC; held long enough, it leaves control.
	escape_at: Option<Instant>,
}

#[derive(Default)]
pub struct Remote {
	session: Option<Session>,
	hook: Option<Hook>,
	shared: Arc<Mutex<Shared>>,
	/// The hook could not start; it is not retried every frame.
	hook_failed: bool,
	#[cfg(feature = "demo")]
	demo: Option<egui::TextureHandle>,
}

/// What the desktop does for the remote control besides sending commands.
#[derive(Default)]
pub struct Outcome {
	pub commands: Vec<Command>,
	/// Bring the main window forward.
	pub focus: bool,
}

impl Remote {
	pub fn poll(
		&mut self,
		ui: &mut ui::MessagingUi,
		state: &mut State,
		ctx: &egui::Context,
		runtime: &tokio::runtime::Handle,
	) -> Outcome {
		let mut out = Outcome::default();
		let (received, commands) = state.take_remote();
		out.commands = commands;
		for received in received {
			self.received(ui, state, received, ctx, &mut out);
		}
		for request in std::mem::take(&mut ui.remote.requests) {
			self.request(ui, state, request, ctx, runtime, &mut out);
		}
		self.advance(ui, state, ctx, runtime, &mut out);
		// Someone leaving the call ends what they were part of.
		if let Some(session) = &self.session
			&& !state.remote_peers().contains(&session.peer)
		{
			let name = session.name.clone();
			self.finish(ui, state, &mut out, false);
			ui.toasts.push(
				ui::design::Level::Info,
				format!("Remote control ended: {name} is no longer in the call"),
			);
		}
		self.notify_refusals(ui);
		self.sync_hook(ui, ctx, state.demo);
		#[cfg(feature = "demo")]
		self.demo(ui, state, ctx);
		out
	}

	fn received(
		&mut self,
		ui: &mut ui::MessagingUi,
		state: &mut State,
		Received { from, signal }: Received,
		ctx: &egui::Context,
		out: &mut Outcome,
	) {
		let now = ctx.input(|input| input.time);
		let name = peer_name(state, from);
		match signal.kind {
			Kind::Ask | Kind::Give => {
				let offer = Offer::from_token(&signal.token);
				if self.session.is_some() || ui.remote.active() || offer.is_none() {
					// Busy: one session at a time.
					out.commands.extend(state.send_remote_signal(
						from,
						Signal {
							kind: Kind::No,
							session: signal.session,
							token: String::new(),
						},
					));
					return;
				}
				let give = signal.kind == Kind::Give;
				self.session = Some(Session {
					id: signal.session,
					peer: from,
					name: name.clone(),
					host: !give,
					stage: Stage::Asked,
					announce: Kind::Yes,
					theirs: offer,
					grant: None,
					texture: VideoTexture::default(),
					held_at: Instant::now(),
					refusals: [(0, None); 2],
					elevation: Elevation::default(),
				});
				ui.remote.phase = Phase::Incoming {
					peer: from,
					name,
					give,
					since: now,
					opened: false,
				};
				ctx.request_repaint();
			}
			Kind::Yes => {
				let Some(session) = self
					.session
					.as_mut()
					.filter(|session| session.id == signal.session && session.peer == from)
				else {
					return;
				};
				if session.theirs.is_none() {
					session.theirs = Offer::from_token(&signal.token);
				}
				if let Phase::Waiting { connecting, .. } = &mut ui.remote.phase {
					*connecting = true;
				}
			}
			Kind::No => {
				if self
					.session
					.as_ref()
					.is_some_and(|session| session.id == signal.session && session.peer == from)
				{
					let waiting = matches!(ui.remote.phase, Phase::Waiting { .. });
					self.finish(ui, state, out, false);
					ui.toasts.push(
						ui::design::Level::Info,
						if waiting {
							format!("{name} declined remote control")
						} else {
							format!("{name} cancelled remote control")
						},
					);
				}
			}
		}
	}

	fn request(
		&mut self,
		ui: &mut ui::MessagingUi,
		state: &mut State,
		request: Request,
		ctx: &egui::Context,
		runtime: &tokio::runtime::Handle,
		out: &mut Outcome,
	) {
		match request {
			Request::Ask(peer) if self.session.is_none() => {
				let mut id = [0; 8];
				if getrandom::fill(&mut id).is_err() {
					return;
				}
				let id = u64::from_le_bytes(id);
				self.session = Some(Session {
					id,
					peer,
					name: peer_name(state, peer),
					host: false,
					stage: Stage::Gathering(gather(id, ctx, runtime)),
					announce: Kind::Ask,
					theirs: None,
					grant: None,
					texture: VideoTexture::default(),
					held_at: Instant::now(),
					refusals: [(0, None); 2],
					elevation: Elevation::default(),
				});
			}
			Request::Answer(true) => {
				if let Some(session) = &mut self.session
					&& matches!(session.stage, Stage::Asked)
				{
					session.stage = Stage::Gathering(gather(session.id, ctx, runtime));
				}
			}
			Request::Start(grant) => {
				if let Some(session) = &mut self.session {
					session.grant = Some(grant);
				}
			}
			Request::Change(grant) => {
				if let Some(Session {
					stage: Stage::Hosting(host),
					grant: current,
					..
				}) = &mut self.session
				{
					*current = Some(grant);
					host.set(config(grant));
				}
			}
			Request::Answer(false) | Request::Cancel | Request::End => {
				self.finish(ui, state, out, true);
			}
			Request::AllowAdmin => {
				if state.demo {
					// The offline preview never asks Windows for anything.
					if let Phase::Hosting { admin, .. } = &mut ui.remote.phase {
						*admin = Admin::Allowed;
					}
				} else if let Some(Session {
					stage: Stage::Hosting(_),
					elevation,
					..
				}) = &mut self.session
				{
					elevation.start(ctx);
				}
			}
			Request::Focus => out.focus = true,
			Request::Ask(_) => {}
		}
	}

	/// Moves the session to its next stage and mirrors it in the interface.
	fn advance(
		&mut self,
		ui: &mut ui::MessagingUi,
		state: &mut State,
		ctx: &egui::Context,
		runtime: &tokio::runtime::Handle,
		out: &mut Outcome,
	) {
		let Some(session) = &mut self.session else {
			return;
		};
		let mut failed = None;
		session.stage = match std::mem::replace(&mut session.stage, Stage::Asked) {
			Stage::Gathering(receive) => match receive.try_recv() {
				Ok(Ok(endpoint)) => {
					out.commands.extend(state.send_remote_signal(
						session.peer,
						Signal {
							kind: session.announce,
							session: session.id,
							token: endpoint.offer().to_token(),
						},
					));
					Stage::Ready(endpoint)
				}
				Ok(Err(error)) => {
					failed = Some(error);
					Stage::Asked
				}
				Err(mpsc::TryRecvError::Empty) => Stage::Gathering(receive),
				Err(mpsc::TryRecvError::Disconnected) => {
					failed = Some("Remote control could not start");
					Stage::Asked
				}
			},
			Stage::Ready(endpoint) => match session.theirs.clone() {
				Some(theirs) => {
					let (send, receive) = mpsc::sync_channel(1);
					let wake = ctx.clone();
					let host = session.host;
					runtime.spawn(async move {
						let _ = send.send(endpoint.connect(&theirs, host, CONNECT).await);
						wake.request_repaint();
					});
					Stage::Connecting(receive)
				}
				None => Stage::Ready(endpoint),
			},
			Stage::Connecting(receive) => match receive.try_recv() {
				Ok(Ok(link)) if session.host => Stage::Linked(link),
				Ok(Ok(link)) => {
					let wake = ctx.clone();
					match Viewer::start(link, runtime, move || wake.request_repaint()) {
						Ok(viewer) => {
							if !matches!(ui.remote.phase, Phase::Viewing(_)) {
								ui.remote.phase = Phase::Viewing(Box::new(Viewing::new(
									session.peer,
									session.name.clone(),
								)));
							}
							Stage::Viewing(Arc::new(Mutex::new(viewer)))
						}
						Err(error) => {
							failed = Some(error);
							Stage::Asked
						}
					}
				}
				Ok(Err(error)) => {
					failed = Some(error);
					Stage::Asked
				}
				Err(mpsc::TryRecvError::Empty) => Stage::Connecting(receive),
				Err(mpsc::TryRecvError::Disconnected) => {
					failed = Some("Remote control could not connect");
					Stage::Asked
				}
			},
			Stage::Linked(link) if link.ended().is_some() => {
				failed = link.ended().map(|end| end_text(end, true));
				Stage::Linked(link)
			}
			Stage::Linked(link) => match session.grant {
				Some(grant) => {
					let wake = ctx.clone();
					match Host::start(link, config(grant), runtime, move || wake.request_repaint())
					{
						Ok(host) => {
							ui.remote.phase = Phase::Hosting {
								peer: session.peer,
								name: session.name.clone(),
								grant,
								connected: true,
								admin: Admin::Blocked,
							};
							Stage::Hosting(host)
						}
						Err(error) => {
							failed = Some(error);
							Stage::Asked
						}
					}
				}
				None => Stage::Linked(link),
			},
			Stage::Hosting(host) => {
				let status = host.status();
				if let Some(end) = status.ended {
					failed = Some(end_text(end, true));
				} else if let Phase::Hosting {
					connected,
					admin,
					name,
					..
				} = &mut ui.remote.phase
				{
					*connected = status.connected;
					let now = ctx.input(|input| input.time);
					if let Some((level, text)) = session.elevation.update(&host, status, admin, now)
					{
						ui.toasts.push(level, text.replace("{name}", name));
					}
				}
				Stage::Hosting(host)
			}
			Stage::Viewing(viewer) => {
				// The hook thread takes the same lock for keys: hold it only for a moment, and
				// always after the shared state, as the hook does.
				let keys = (session.held_at.elapsed() >= HELD_EVERY)
					.then(|| lock(&self.shared).forwarded.clone());
				let (status, picture) = {
					let viewer = lock(&viewer);
					(viewer.status(), viewer.take_picture())
				};
				if let Some(end) = status.ended {
					failed = Some(end_text(end, false));
				} else if let Phase::Viewing(viewing) = &mut ui.remote.phase {
					// Control starts fullscreen over Leech, brought forward, as soon as the
					// other side finished its setup.
					if status.started && !viewing.started {
						viewing.full = true;
						ui.remote.dialog = false;
						ui.remote.menu = ui::remote::Menu::None;
						out.focus = true;
					}
					if let Some(picture) = picture {
						viewing.texture = session.texture.upload(
							ctx,
							"remote-control",
							picture.with_data(picture.data.as_slice()),
						);
					}
					viewing.connected = status.connected;
					viewing.started = status.started;
					viewing.permission = status.permission.map(|permission| match permission {
						net::Permission::Keyboard => Permission::Keyboard,
						net::Permission::Mouse => Permission::Mouse,
						net::Permission::Full => Permission::Full,
					});
					viewing.paused = status.paused;
					viewing.taken = status.taken;
					viewing.audio = status.audio;
					viewing.screen = [status.screen.0, status.screen.1];
					viewing.relative = status.relative_mouse;
					viewing.rtt_ms = status.rtt_ms;
					viewing.content = status.content;
					let inputs = std::mem::take(&mut viewing.inputs)
						.into_iter()
						.map(input)
						.collect::<Vec<_>>();
					if !inputs.is_empty() {
						lock(&viewer).send(&inputs);
					}
					if let Some(keys) = keys {
						session.held_at = Instant::now();
						let buttons = viewing.held.iter().copied().map(button).collect::<Vec<_>>();
						lock(&viewer).held(&keys, &buttons);
					}
					ctx.request_repaint_after(HELD_EVERY);
				}
				Stage::Viewing(viewer)
			}
			stage => stage,
		};
		if let Some(error) = failed {
			let name = session.name.clone();
			self.finish(ui, state, out, false);
			ui.toasts
				.push(ui::design::Level::Info, error.replace("{name}", &name));
		}
	}

	/// Ends the session: `tell` says the other side has to hear it (it did not end it).
	fn finish(
		&mut self,
		ui: &mut ui::MessagingUi,
		state: &mut State,
		out: &mut Outcome,
		tell: bool,
	) {
		if let Some(session) = self.session.take() {
			match session.stage {
				Stage::Hosting(host) => host.end(),
				Stage::Viewing(viewer) => lock(&viewer).end(),
				Stage::Linked(link) => link.decline(),
				_ if tell => out.commands.extend(state.send_remote_signal(
					session.peer,
					Signal {
						kind: Kind::No,
						session: session.id,
						token: String::new(),
					},
				)),
				_ => {}
			}
		}
		ui.remote.phase = Phase::Idle;
		ui.remote.binding = None;
		ui.remote.dialog = false;
		ui.remote.menu = ui::remote::Menu::None;
		let mut shared = lock(&self.shared);
		shared.forwarded.clear();
		shared.taking = None;
		shared.mode = Mode::Pass;
	}

	/// Tells the controller, now and then, that a click or key did not get through.
	fn notify_refusals(&mut self, ui: &mut ui::MessagingUi) {
		let Some(Session {
			stage: Stage::Viewing(viewer),
			name,
			refusals,
			..
		}) = &mut self.session
		else {
			return;
		};
		let status = lock(viewer).status();
		for ((seen, shown), (count, text)) in refusals.iter_mut().zip([
			(
				status.refused_own,
				"Leech on {name}'s PC ignores your clicks and keys, so their call stays theirs. Click another window there to type",
			),
			(
				status.refused_elevated,
				"That app runs as administrator on {name}'s PC: Windows blocks your mouse and keyboard there until they allow admin apps",
			),
		]) {
			if count == *seen {
				continue;
			}
			*seen = count;
			if shown.is_none_or(|at| at.elapsed() >= NOTICE_EVERY) {
				*shown = Some(Instant::now());
				ui.toasts
					.push(ui::design::Level::Info, text.replace("{name}", name));
			}
		}
	}

	/// Sends the pointer input this frame collected at once, rather than with the next frame.
	pub fn flush_input(&mut self, ui: &mut ui::MessagingUi) {
		let (
			Some(Session {
				stage: Stage::Viewing(viewer),
				..
			}),
			Phase::Viewing(viewing),
		) = (&self.session, &mut ui.remote.phase)
		else {
			return;
		};
		if viewing.inputs.is_empty() {
			return;
		}
		let inputs = std::mem::take(&mut viewing.inputs)
			.into_iter()
			.map(input)
			.collect::<Vec<_>>();
		lock(viewer).send(&inputs);
	}

	/// Keeps one keyboard hook while anything needs it, in the mode the phase calls for, and
	/// takes what it caught.
	fn sync_hook(&mut self, ui: &mut ui::MessagingUi, ctx: &egui::Context, demo: bool) {
		let remote = &mut ui.remote;
		let key = |key: &Option<ui::remote::Key>| key.as_ref().map(|key| (key.scan, key.extended));
		let mode = match (&remote.phase, &self.session) {
			_ if remote.binding.is_some() => Mode::Bind,
			(Phase::Hosting { .. }, session) => Mode::Host {
				pause: key(&remote.keys.pause),
				take_over: key(&remote.keys.take_over),
				hand: match session {
					Some(Session {
						stage: Stage::Hosting(host),
						..
					}) => Some(host.hand()),
					_ => None,
				},
			},
			(
				Phase::Viewing(viewing),
				Some(Session {
					stage: Stage::Viewing(viewer),
					..
				}),
			) if viewing.capturing => Mode::View {
				viewer: viewer.clone(),
				overlay: key(&remote.keys.overlay),
				keyboard: matches!(
					viewing.permission,
					Some(Permission::Keyboard | Permission::Full)
				),
			},
			_ => Mode::Pass,
		};
		// The offline preview never hooks the keyboard: it runs beside other work.
		let needed = !demo
			&& (remote.binding.is_some()
				|| matches!(remote.phase, Phase::Hosting { .. } | Phase::Viewing(_)));
		let caught = {
			let mut shared = lock(&self.shared);
			// Leaving control lifts every key the other PC holds.
			let still_viewing = matches!(mode, Mode::View { keyboard: true, .. });
			if !still_viewing && !shared.forwarded.is_empty() {
				if let Mode::View { viewer, .. } = &shared.mode {
					let ups = shared
						.forwarded
						.iter()
						.map(|&(scan, extended)| net::Input::Key {
							scan,
							extended,
							down: false,
						})
						.collect::<Vec<_>>();
					lock(viewer).send(&ups);
				}
				shared.forwarded.clear();
			}
			// Binding switches itself off after the first key; do not switch it back on.
			if !(matches!(mode, Mode::Bind)
				&& shared
					.caught
					.iter()
					.any(|caught| matches!(caught, Caught::Bound(..) | Caught::CancelBind)))
			{
				shared.mode = mode;
			}
			std::mem::take(&mut shared.caught)
		};
		if needed && self.hook.is_none() && !self.hook_failed {
			let shared = self.shared.clone();
			let wake = ctx.clone();
			match Hook::start(move |event| filter(&shared, &wake, event)) {
				Ok(hook) => self.hook = Some(hook),
				Err(error) => {
					self.hook_failed = true;
					ui.toasts.push(
						ui::design::Level::Warning,
						format!("{error}: remote control keys only work while Leech has focus"),
					);
				}
			}
		} else if !needed {
			// Tried again the next time a key is needed.
			self.hook_failed = false;
			if self.hook.take().is_some() {
				lock(&self.shared).swallowed.clear();
			}
		}
		// A key pressed in Leech itself, which the hook did not catch.
		if let Some((scan, extended)) = remote.typed.take() {
			remote.bound(ui::remote::Key {
				scan,
				extended,
				name: platform::keyboard_hook::key_name(scan, extended),
			});
		}
		// The mode above came before these keys: one more frame sets the one they leave.
		if !caught.is_empty() {
			ctx.request_repaint();
		}
		for caught in caught {
			match caught {
				Caught::Bound(scan, extended) => remote.bound(ui::remote::Key {
					scan,
					extended,
					name: platform::keyboard_hook::key_name(scan, extended),
				}),
				Caught::CancelBind => remote.binding = None,
				Caught::Pause => {
					if let Some(text) = remote.toggle_pause() {
						ui.toasts.push(ui::design::Level::Info, text);
					}
				}
				// Leech comes back; the other side stays as it is.
				Caught::Minimize => {
					if let Some(viewing) = remote.viewing() {
						viewing.full = false;
					}
				}
				Caught::Stop => remote.requests.push(Request::End),
				Caught::Overlay => remote.overlay_hidden = !remote.overlay_hidden,
			}
		}
	}
}

#[cfg(feature = "demo")]
impl Remote {
	/// `--demo --demo-call --demo-remote=<phase>`: one phase with the call's peer, for
	/// screenshots, without any signal, connection or hook. Phases: menu, who, keys, waiting,
	/// setting-up, incoming, answer, setup, key, warning, hosting, access, access-paused,
	/// admin (the offer to allow admin apps), admin-allowed, viewing, paused, taken, stage,
	/// viewer-menu.
	fn demo(&mut self, ui: &mut ui::MessagingUi, state: &State, ctx: &egui::Context) {
		use ui::remote::{Menu, Step};
		if !state.demo || self.demo.is_some() {
			return;
		}
		let Some(which) =
			std::env::args().find_map(|arg| arg.strip_prefix("--demo-remote=").map(str::to_owned))
		else {
			return;
		};
		// A synthetic desktop: a gradient with a taskbar and a window.
		let (width, height) = (1920usize, 1080usize);
		let mut pixels = vec![egui::Color32::BLACK; width * height];
		for (i, pixel) in pixels.iter_mut().enumerate() {
			let (x, y) = (i % width, i / width);
			let window = (400..1500).contains(&x) && (180..820).contains(&y);
			*pixel = if y >= height - 48 {
				egui::Color32::from_rgb(24, 26, 32)
			} else if window && y < 220 {
				egui::Color32::from_rgb(44, 48, 58)
			} else if window {
				egui::Color32::from_rgb(232, 234, 240)
			} else {
				egui::Color32::from_rgb(
					(30 + x * 60 / width) as u8,
					(60 + y * 50 / height) as u8,
					(120 + x * 80 / width) as u8,
				)
			};
		}
		let texture = ctx.load_texture(
			"remote-demo",
			egui::ColorImage::new([width, height], pixels),
			egui::TextureOptions::LINEAR,
		);
		let peer = Id(2);
		let name = peer_name(state, peer);
		let now = ctx.input(|input| input.time);
		let grant = Grant::default();
		let setup = |step| Phase::Setup {
			peer,
			name: name.clone(),
			step,
			grant,
		};
		let viewing = |paused: bool, connected: bool, started: bool, full: bool| {
			let mut viewing = Viewing::new(peer, name.clone());
			viewing.texture = connected.then(|| egui::load::SizedTexture::from_handle(&texture));
			viewing.screen = [1920, 1080];
			viewing.connected = connected;
			viewing.started = started;
			viewing.paused = paused;
			viewing.full = full;
			viewing.permission = Some(Permission::Full);
			Phase::Viewing(Box::new(viewing))
		};
		let hosting = |paused: bool, admin: Admin| Phase::Hosting {
			peer,
			name: name.clone(),
			grant: Grant { paused, ..grant },
			connected: true,
			admin,
		};
		let (phase, dialog, menu) = match which.as_str() {
			"menu" => (Phase::Idle, false, Menu::Access),
			"who" => (Phase::Idle, false, Menu::Who),
			"keys" => (Phase::Idle, false, Menu::Manage),
			"waiting" => (
				Phase::Waiting {
					peer,
					name: name.clone(),
					since: now,
					connecting: false,
				},
				true,
				Menu::None,
			),
			"setting-up" => (viewing(false, true, false, false), true, Menu::None),
			"incoming" | "answer" => (
				Phase::Incoming {
					peer,
					name: name.clone(),
					give: false,
					since: now,
					opened: which == "answer",
				},
				true,
				Menu::None,
			),
			"setup" => (setup(Step::Permission), true, Menu::None),
			"key" => (setup(Step::Key), true, Menu::None),
			"warning" => (setup(Step::Warning), true, Menu::None),
			"hosting" => (hosting(false, Admin::Blocked), false, Menu::None),
			"access" => (hosting(false, Admin::Blocked), false, Menu::Manage),
			"access-paused" => (hosting(true, Admin::Blocked), false, Menu::Manage),
			"admin" => (hosting(false, Admin::Offered(now)), false, Menu::None),
			"admin-allowed" => (hosting(false, Admin::Allowed), false, Menu::Manage),
			"viewing" => (viewing(false, true, true, true), false, Menu::None),
			"paused" => (viewing(true, true, true, true), false, Menu::None),
			"taken" => {
				let mut phase = viewing(true, true, true, true);
				if let Phase::Viewing(viewing) = &mut phase {
					viewing.taken = true;
				}
				(phase, false, Menu::None)
			}
			"stage" => (viewing(false, true, true, false), false, Menu::None),
			"viewer-menu" => (viewing(false, true, true, false), false, Menu::Manage),
			_ => return,
		};
		if matches!(
			which.as_str(),
			"key" | "warning" | "access" | "access-paused" | "admin-allowed" | "keys"
		) {
			ui.remote.keys.pause = Some(ui::remote::Key {
				scan: 0x42,
				extended: false,
				name: "F8".into(),
			});
		}
		if matches!(
			which.as_str(),
			"access" | "access-paused" | "admin-allowed" | "keys"
		) {
			ui.remote.keys.take_over = Some(ui::remote::Key {
				scan: 0x43,
				extended: false,
				name: "F9".into(),
			});
		}
		ui.remote.phase = phase;
		ui.remote.dialog = dialog;
		ui.remote.menu = menu;
		ui.remote.windowed = true;
		self.demo = Some(texture);
	}
}

/// Runs on the hook thread for every key: decides fast whether the key is this feature's.
fn filter(shared: &Mutex<Shared>, wake: &egui::Context, event: KeyEvent) -> bool {
	let mut shared = lock(shared);
	let key = (event.scan, event.extended);
	if !event.down {
		if shared.taking.as_ref().is_some_and(|(held, _)| *held == key)
			&& let Some((_, hand)) = shared.taking.take()
		{
			hand.hold(false);
		}
		if event.vk == VK_ESCAPE {
			shared.escape_at = None;
		}
		let Some(index) = shared.swallowed.iter().position(|held| *held == key) else {
			return false;
		};
		shared.swallowed.swap_remove(index);
		if let Some(index) = shared.forwarded.iter().position(|held| *held == key) {
			shared.forwarded.swap_remove(index);
			if let Mode::View { viewer, .. } = &shared.mode {
				lock(viewer).send(&[net::Input::Key {
					scan: key.0,
					extended: key.1,
					down: false,
				}]);
			}
		}
		return true;
	}
	// The controller's keys replayed on this PC never count as this PC's own; other
	// injected keys (remappers, on-screen keyboards) do.
	if event.injected && event.extra == platform::remote_input::INJECTED_MARK {
		return false;
	}
	let repeat = shared.swallowed.contains(&key);
	let caught = match &shared.mode {
		Mode::Pass => return false,
		Mode::Bind => {
			shared.mode = Mode::Pass;
			Some(if event.vk == VK_ESCAPE {
				Caught::CancelBind
			} else {
				Caught::Bound(key.0, key.1)
			})
		}
		Mode::Host {
			pause,
			take_over,
			hand,
		} => {
			if *take_over == Some(key) {
				// Straight from this thread, so the controller stops at once.
				if let Some(hand) = hand
					&& shared.taking.is_none()
				{
					hand.hold(true);
					shared.taking = Some((key, hand.clone()));
				}
				None
			} else if *pause == Some(key) {
				(!repeat).then_some(Caught::Pause)
			} else {
				return false;
			}
		}
		Mode::View {
			viewer,
			overlay,
			keyboard,
		} => {
			if event.vk == VK_LWIN || event.vk == VK_RWIN {
				(!repeat).then_some(Caught::Stop)
			} else if event.vk == VK_ESCAPE
				&& (!*keyboard
					|| shared
						.escape_at
						.is_some_and(|at| at.elapsed() >= LEAVE_AFTER))
			{
				// With the keyboard there a tap goes there and holding leaves; without it a
				// tap leaves. Repeats after leaving ask again, which changes nothing.
				(!repeat || *keyboard).then_some(Caught::Minimize)
			} else if *overlay == Some(key) {
				(!repeat).then_some(Caught::Overlay)
			} else if *keyboard {
				lock(viewer).send(&[net::Input::Key {
					scan: key.0,
					extended: key.1,
					down: true,
				}]);
				if !shared.forwarded.contains(&key) {
					shared.forwarded.push(key);
				}
				if event.vk == VK_ESCAPE && !repeat {
					shared.escape_at = Some(Instant::now());
				}
				None
			} else {
				return false;
			}
		}
	};
	if !repeat && shared.swallowed.len() < 64 {
		shared.swallowed.push(key);
	}
	if let Some(caught) = caught {
		shared.caught.push(caught);
		wake.request_repaint();
	}
	true
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	mutex
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn gather(id: u64, ctx: &egui::Context, runtime: &tokio::runtime::Handle) -> Gathered {
	allow_through_firewall();
	let (send, receive) = mpsc::sync_channel(1);
	let wake = ctx.clone();
	runtime.spawn(async move {
		let _ = send.send(Endpoint::gather(id).await);
		wake.request_repaint();
	});
	receive
}

/// Once per run, at the first remote control: makes sure Windows Firewall lets the other PC's
/// datagrams in even from a port this PC never sent to (its NAT may pick a new one per
/// destination). When no rule does, Windows asks for administrator approval once; remote
/// control goes on either way. Demo builds never ask.
fn allow_through_firewall() {
	#[cfg(not(feature = "demo"))]
	{
		static ONCE: std::sync::Once = std::sync::Once::new();
		ONCE.call_once(|| {
			let _ = std::thread::Builder::new()
				.name("firewall".into())
				.spawn(|| {
					use platform::firewall::Inbound;
					let outcome = match platform::firewall::allow_inbound_udp() {
						Inbound::Allowed => "already allows remote control",
						Inbound::Added => "rule added",
						Inbound::Declined => "administrator approval declined",
						Inbound::Failed => "rule could not be added",
					};
					client_core::journal::note("remote", format!("firewall {outcome}"));
				});
		});
	}
}

fn config(grant: Grant) -> HostConfig {
	HostConfig {
		permission: grant.permission.map(|permission| match permission {
			Permission::Keyboard => net::Permission::Keyboard,
			Permission::Mouse => net::Permission::Mouse,
			Permission::Full => net::Permission::Full,
		}),
		paused: grant.paused,
		audio: grant.audio,
	}
}

fn button(button: MouseButton) -> net::MouseButton {
	match button {
		MouseButton::Left => net::MouseButton::Left,
		MouseButton::Right => net::MouseButton::Right,
		MouseButton::Middle => net::MouseButton::Middle,
		MouseButton::Back => net::MouseButton::Back,
		MouseButton::Forward => net::MouseButton::Forward,
	}
}

fn input(input: MouseInput) -> net::Input {
	match input {
		MouseInput::Move { x, y, dx, dy } => net::Input::Move { x, y, dx, dy },
		MouseInput::Button {
			button: pressed,
			down,
		} => net::Input::Button {
			button: button(pressed),
			down,
		},
		// winit inverts Windows' sideways wheel (left is positive there); upward agrees.
		MouseInput::Wheel { dx, dy } => net::Input::Wheel { dx: -dx, dy },
	}
}

/// Why a running session ended, for a toast; `{name}` is the other person.
fn end_text(end: End, host: bool) -> &'static str {
	match (end, host) {
		(End::ByHost, true) | (End::ByViewer, false) => "Remote control ended",
		(End::ByHost, false) => "{name} ended remote control",
		(End::ByViewer, true) => "{name} stopped controlling your PC",
		(End::Declined, _) => "{name} declined remote control",
		(End::Lost, _) => "Remote control lost the connection to {name}",
		(End::Failed(error), _) => error,
	}
}

fn peer_name(state: &State, user: Id) -> String {
	let Some(call) = state.voice.active.as_ref() else {
		return "Someone".to_owned();
	};
	state
		.channels
		.iter()
		.find(|channel| channel.id == call.channel)
		.and_then(|channel| {
			channel
				.recipients
				.iter()
				.find(|recipient| recipient.id == user)
		})
		.or_else(|| {
			state
				.voice
				.roster
				.iter()
				.find(|entry| entry.channel == call.channel && entry.participant.user == user)
				.and_then(|entry| entry.member.as_ref())
				.map(|member| &member.user)
		})
		.map_or_else(|| "Someone".to_owned(), |user| user.name.clone())
}
