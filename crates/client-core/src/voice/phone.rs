//! The call controller: every join, leave, mute, camera, screen-share and watch decision for
//! this device, as one deterministic state machine.
//!
//! The user's intent (`State::active` and the share and watch choices) is the only truth the
//! interface edits. Everything else follows from it in `pump`:
//!
//! - **Wire.** One Gateway voice-state operation is in flight at a time. The next one, carrying
//!   the newest intent, goes out when Discord echoes the previous one or after `ACK_TIMEOUT`.
//!   Joining, leaving and toggling in any order and at any speed therefore never queues stale
//!   work, and an event always belongs to the latest operation it can belong to.
//! - **Link.** The session and voice server Discord gave for the channel last asked for.
//!   Asking for another channel, or leaving, discards it.
//! - **Media.** A run exists exactly while the intent, the link and the wire agree. Its id
//!   changes whenever anything it depends on changes, so the desktop starts and stops workers
//!   by comparing ids, and late reports from an old run are ignored.
//! - **Streams.** The own screen share and a watched stream are Discord stream keys with the
//!   same one-operation pacing, created only while the call's media is connected.
//!
//! No action is refused because older work is still finishing: old work is superseded.
use super::{Command, Phase, Report, Secret, StreamEvent};
use crate::{State as ClientState, screen::Settings};
use model::Id;
use std::{
	collections::VecDeque,
	sync::atomic::{AtomicU64, Ordering},
	time::{Duration, Instant},
};

/// A voice-state operation Discord has not echoed by then no longer holds the next one back.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(3);
/// Stream setup and teardown get longer before they count as lost.
pub const STREAM_TIMEOUT: Duration = Duration::from_secs(10);
/// Media failures retried with backoff (1 s, 2 s, 4 s) before the call ends.
pub const MAX_RETRIES: u8 = 3;
/// Commands waiting for the desktop to send them; it drains them every frame.
pub const MAX_OUTBOX: usize = 64;

static IDS: AtomicU64 = AtomicU64::new(1);
/// Ids are unique for the process, so a run started before a sign-out never matches a new one.
pub(crate) fn next_id() -> u64 {
	IDS.fetch_add(1, Ordering::Relaxed)
}

/// One Gateway voice state (opcode 4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Target {
	pub guild: Option<Id>,
	pub channel: Option<Id>,
	pub mute: bool,
	pub deaf: bool,
	pub video: bool,
}
impl Target {
	fn same(&self, other: &Self) -> bool {
		match (self.channel, other.channel) {
			(None, None) => true,
			_ => self == other,
		}
	}
	fn leave(guild: Option<Id>) -> Self {
		Self {
			guild,
			..Self::default()
		}
	}
}

#[derive(Default)]
pub(crate) struct Wire {
	/// The latest voice state sent: what Discord holds once it catches up.
	pub told: Target,
	/// Deadline of the operation still waiting for Discord's echo.
	pub pending: Option<Instant>,
	/// The socket dropped with an operation unanswered; send the intent again after resuming.
	pub resend: bool,
	/// Leave and join again for fresh credentials after a media failure.
	pub rejoin: bool,
}

pub(crate) struct Server {
	pub token: Secret,
	pub endpoint: String,
}
fn server(token: Option<Secret>, endpoint: Option<String>) -> Option<Server> {
	match (token, endpoint) {
		(Some(token), Some(endpoint)) if !endpoint.is_empty() => Some(Server { token, endpoint }),
		// A null endpoint means Discord is moving the call; wait for the next update.
		_ => None,
	}
}

/// Discord's credentials for the channel in `Wire::told`.
#[derive(Default)]
pub(crate) struct Link {
	pub session: Option<Secret>,
	pub server: Option<Server>,
	/// Changes whenever the session or the server does; a media run is bound to one revision.
	pub revision: u64,
}
impl Link {
	fn reset(&mut self) {
		self.session = None;
		self.server = None;
		self.revision = next_id();
	}
}

pub(crate) struct Run {
	pub id: u64,
	pub channel: Id,
	pub revision: u64,
	pub phase: Phase,
	pub ready: bool,
	pub camera: bool,
}

#[derive(Default)]
pub(crate) struct Retry {
	pub count: u8,
	pub at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamOp {
	Create,
	Delete,
}
/// One Go Live stream key on Discord's side, with the same one-operation pacing as the wire.
/// The key stays after the stream ends, so a late answer for it still reaches it.
#[derive(Default)]
pub(crate) struct Stream {
	pub key: Option<String>,
	pub exists: bool,
	/// Operation in flight and its deadline.
	pub pending: Option<(StreamOp, Instant)>,
	pub rtc: Option<(Id, Id)>,
	pub server: Option<Server>,
}
impl Stream {
	fn ready(&self) -> bool {
		self.exists && self.pending.is_none() && self.rtc.is_some() && self.server.is_some()
	}
	/// The voice session that carried this stream is gone, and Discord removes the stream
	/// with it. Wait briefly for that before creating the key again.
	fn doomed(&mut self, now: Instant) {
		if self.exists || matches!(self.pending, Some((StreamOp::Create, _))) {
			self.pending = Some((StreamOp::Delete, now + ACK_TIMEOUT));
		}
	}
	fn deleted(&mut self) {
		self.exists = false;
		self.pending = None;
		self.rtc = None;
		self.server = None;
	}
}

/// This device's own screen share, as the interface shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareState {
	Off,
	Starting,
	Live,
	Stopping,
}

#[derive(Default)]
pub(crate) struct Share {
	pub want: Option<(u64, Settings)>,
	pub stream: Stream,
	pub live: bool,
	pub status: &'static str,
}
#[derive(Default)]
pub(crate) struct Watch {
	pub want: Option<(u64, Id)>,
	pub stream: Stream,
	pub live: bool,
	pub status: &'static str,
}

#[derive(Default)]
pub(crate) struct Phone {
	pub wire: Wire,
	pub link: Link,
	pub media: Option<Run>,
	pub retry: Retry,
	/// The attempt whose recipients are rung once its media first connects.
	pub ring: Option<u64>,
	pub share: Share,
	pub watch: Watch,
	pub outbox: VecDeque<Command>,
}
impl Phone {
	pub(crate) fn send(&mut self, command: Command) {
		if self.outbox.len() == MAX_OUTBOX {
			self.outbox.pop_front();
		}
		self.outbox.push_back(command);
	}
}

/// What one media worker needs to start; credentials are read once with `media_credentials`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaPlan {
	pub id: u64,
	pub channel: Id,
	pub guild: Option<Id>,
}
#[derive(Clone, Copy, Debug)]
pub struct SharePlan {
	pub id: u64,
	pub settings: Settings,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchPlan {
	pub id: u64,
	pub streamer: Id,
}

pub(crate) fn stream_key(guild: Option<Id>, channel: Id, user: Id) -> String {
	guild.map_or_else(
		|| format!("call:{channel}:{user}"),
		|guild| format!("guild:{guild}:{channel}:{user}"),
	)
}

impl super::State {
	pub fn media_plan(&self) -> Option<MediaPlan> {
		let run = self.phone.media.as_ref()?;
		Some(MediaPlan {
			id: run.id,
			channel: run.channel,
			guild: self.active.as_ref()?.guild,
		})
	}
	pub fn share_plan(&self) -> Option<SharePlan> {
		let (id, settings) = self.phone.share.want?;
		(self.phone.share.stream.ready() && self.media_ready())
			.then_some(SharePlan { id, settings })
	}
	pub fn watch_plan(&self) -> Option<WatchPlan> {
		let (id, streamer) = self.phone.watch.want?;
		(self.phone.watch.stream.ready() && self.media_ready())
			.then_some(WatchPlan { id, streamer })
	}
	/// The call's media is connected, alone or with others.
	pub fn media_ready(&self) -> bool {
		self.phone.media.as_ref().is_some_and(|run| run.ready)
	}
	pub fn camera_negotiated(&self) -> bool {
		self.phone.media.as_ref().is_some_and(|run| run.camera)
	}
	pub fn share_state(&self) -> ShareState {
		let share = &self.phone.share;
		match (share.want.is_some(), share.live) {
			(true, true) => ShareState::Live,
			(true, false) => ShareState::Starting,
			(false, _) if share.stream.exists || share.stream.pending.is_some() => {
				ShareState::Stopping
			}
			(false, _) => ShareState::Off,
		}
	}
	pub fn share_status(&self) -> &'static str {
		self.phone.share.status
	}
	pub fn watch_status(&self) -> &'static str {
		self.phone.watch.status
	}
	pub fn watch_live(&self) -> bool {
		self.phone.watch.want.is_some() && self.phone.watch.live
	}
	/// Commands for the desktop to send, oldest first.
	pub fn take_commands(&mut self) -> impl Iterator<Item = Command> + '_ {
		self.phone.outbox.drain(..)
	}
	/// When `voice_tick` next has work: a timeout or a retry.
	pub fn next_deadline(&self) -> Option<Instant> {
		let phone = &self.phone;
		[
			phone.wire.pending,
			phone.share.stream.pending.map(|(_, at)| at),
			phone.watch.stream.pending.map(|(_, at)| at),
			phone.retry.at,
		]
		.into_iter()
		.flatten()
		.min()
	}
}

impl ClientState {
	pub(crate) fn owner(&self) -> Option<Id> {
		self.user.as_ref().map(|user| user.id)
	}

	/// Drives every timeout and retry. Cheap; the desktop calls it every frame.
	pub fn voice_tick(&mut self, now: Instant) {
		let clock = self.voice.clock.map_or(now, |clock| clock.max(now));
		self.voice.clock = Some(clock);
		self.pump(clock);
	}
	pub(crate) fn voice_now(&self) -> Instant {
		self.voice.clock.unwrap_or_else(Instant::now)
	}

	/// Ends the wanted call locally; the wire follows on the next pump.
	pub(crate) fn end_call(&mut self, message: Option<&'static str>) {
		if self.voice.active.take().is_some()
			&& let Some(message) = message
		{
			self.status = message;
		}
		self.voice.phone.ring = None;
		self.voice.phone.retry = Retry::default();
		self.voice.phone.wire.rejoin = false;
		self.end_share(None);
		self.end_watch(None);
	}

	pub(crate) fn end_share(&mut self, message: Option<&'static str>) {
		let share = &mut self.voice.phone.share;
		share.want = None;
		share.live = false;
		share.status = message.unwrap_or("");
	}
	pub(crate) fn end_watch(&mut self, message: Option<&'static str>) {
		let watch = &mut self.voice.phone.watch;
		watch.want = None;
		watch.live = false;
		watch.status = message.unwrap_or("");
		if let Some(call) = &mut self.voice.active {
			call.watching = None;
		}
	}

	/// Discord dropped this session's voice state (a fresh READY). A wanted call joins again.
	pub(crate) fn voice_session_reset(&mut self, now: Instant) {
		let phone = &mut self.voice.phone;
		phone.wire = Wire::default();
		phone.link.reset();
		phone.share.stream.deleted();
		phone.watch.stream.deleted();
		if phone.share.want.is_some() {
			self.end_share(Some("Screen share ended when the connection reset"));
		}
		self.end_watch(None);
		self.pump(now);
	}

	/// The Gateway socket dropped. Voice survives a RESUME, but an unanswered operation may
	/// be lost with the socket, so it goes out again once the socket is back.
	pub(crate) fn voice_disconnected(&mut self, now: Instant) {
		let phone = &mut self.voice.phone;
		if phone.wire.pending.take().is_some() {
			phone.wire.resend = true;
		}
		for stream in [&mut phone.share.stream, &mut phone.watch.stream] {
			if let Some((op, _)) = stream.pending {
				stream.pending = Some((op, now + STREAM_TIMEOUT));
			}
		}
	}

	/// Own voice state for this account. `ours` is false when another device holds it.
	pub(crate) fn own_voice_state(
		&mut self,
		ours: bool,
		session: Option<Secret>,
		guild: Option<Id>,
		channel: Option<Id>,
		now: Instant,
	) {
		let phone = &mut self.voice.phone;
		let told = phone.wire.told;
		if !ours {
			// Another device of this account joined a call, so Discord moved the voice session
			// there. An answer to our own operation still in flight decides first.
			if channel.is_some() && phone.wire.pending.is_none() && told.channel.is_some() {
				phone.wire.told = Target::default();
				phone.link.reset();
				phone.share.stream.doomed(now);
				phone.watch.stream.doomed(now);
				self.end_call(Some("Call continued on another device"));
				self.pump(now);
			}
			return;
		}
		if channel == told.channel {
			// Answered, possibly by a replay after the socket resumed.
			phone.wire.pending = None;
			phone.wire.resend = false;
		}
		if channel == told.channel {
			if channel.is_some()
				&& let Some(session) = session
				&& phone
					.link
					.session
					.as_ref()
					.is_none_or(|old| old.expose() != session.expose())
			{
				phone.link.session = Some(session);
				phone.link.revision = next_id();
			}
		} else if phone.wire.pending.is_none() {
			// Discord changed the state without an operation of ours: a disconnect or a move.
			match channel {
				None => {
					phone.wire.told = Target::default();
					phone.link.reset();
					phone.share.stream.doomed(now);
					phone.watch.stream.doomed(now);
					self.end_call(Some("You were disconnected from the call"));
				}
				Some(moved) => {
					let followed = self.voice.active.as_mut().filter(|call| {
						call.guild.is_some()
							&& call.guild == guild
							&& told.channel == Some(call.channel)
					});
					phone.link.session = session;
					phone.link.revision = next_id();
					phone.share.stream.doomed(now);
					phone.watch.stream.doomed(now);
					if let Some(call) = followed {
						// A moderator moved this account; follow like Discord does.
						call.channel = moved;
						call.participants.clear();
						phone.wire.told.channel = Some(moved);
						self.end_share(None);
						self.end_watch(None);
						self.refresh_voice_participants();
					} else {
						// Discord holds a voice state nobody here wants now, such as a join
						// answered after a disconnect; record it so the next pump corrects it.
						phone.link.server = None;
						phone.wire.told = Target {
							guild,
							channel: Some(moved),
							..Target::default()
						};
					}
				}
			}
		}
		self.pump(now);
	}

	pub(crate) fn voice_server(
		&mut self,
		guild: Option<Id>,
		channel: Option<Id>,
		token: Option<Secret>,
		endpoint: Option<String>,
		now: Instant,
	) {
		let phone = &mut self.voice.phone;
		let Some(told) = phone.wire.told.channel else {
			return;
		};
		let matches = match phone.wire.told.guild {
			Some(own) => guild == Some(own),
			None => guild.is_none() && channel == Some(told),
		};
		if !matches {
			return;
		}
		phone.link.server = server(token, endpoint);
		phone.link.revision = next_id();
		self.pump(now);
	}

	pub(crate) fn stream_event(&mut self, key: &str, event: StreamEvent, now: Instant) {
		let phone = &mut self.voice.phone;
		let own = phone.share.stream.key.as_deref() == Some(key);
		if !own && phone.watch.stream.key.as_deref() != Some(key) {
			return;
		}
		let stream = if own {
			&mut phone.share.stream
		} else {
			&mut phone.watch.stream
		};
		match event {
			StreamEvent::Created {
				rtc_server,
				rtc_channel,
			} => {
				stream.exists = true;
				if stream
					.rtc
					.is_some_and(|rtc| rtc != (rtc_server, rtc_channel))
				{
					// Another stream under the same key; the old server data is stale.
					stream.server = None;
				}
				stream.rtc = Some((rtc_server, rtc_channel));
				if matches!(stream.pending, Some((StreamOp::Create, _))) {
					stream.pending = None;
				}
			}
			StreamEvent::Server { token, endpoint } => {
				if stream.exists || matches!(stream.pending, Some((StreamOp::Create, _))) {
					stream.server = server(token, endpoint);
				}
			}
			StreamEvent::Deleted { reason } => {
				let requested = matches!(stream.pending, Some((StreamOp::Delete, _)));
				stream.deleted();
				if !requested {
					if own {
						if phone.share.want.is_some() {
							self.end_share(Some(
								reason.unwrap_or("Discord ended the screen share"),
							));
						}
					} else if phone.watch.want.is_some() {
						self.end_watch(reason);
					}
				}
			}
		}
		self.pump(now);
	}

	pub(crate) fn media_report(&mut self, id: u64, report: Report, now: Instant) {
		let phone = &mut self.voice.phone;
		if let Some(run) = phone.media.as_mut().filter(|run| run.id == id) {
			match report {
				Report::Progress(phase) => {
					run.phase = phase;
					run.ready = false;
				}
				Report::Waiting | Report::Ready => {
					run.phase = if matches!(report, Report::Waiting) {
						Phase::Waiting
					} else {
						Phase::Connected
					};
					run.ready = true;
					phone.retry = Retry::default();
				}
				Report::CameraAvailable(available) => run.camera = available,
				Report::Failed(message) => {
					phone.media = None;
					phone.retry.count += 1;
					if phone.retry.count > MAX_RETRIES {
						self.end_call(Some(message));
					} else {
						// Fresh credentials too: the old ones may be what failed.
						phone.retry.at =
							Some(now + Duration::from_secs(1 << (phone.retry.count - 1)));
						phone.wire.rejoin = true;
						self.status = message;
					}
				}
				Report::Status(_) | Report::Live => {}
			}
		} else if phone.share.want.is_some_and(|(want, _)| want == id) {
			match report {
				Report::Status(status) => phone.share.status = status,
				Report::Live => {
					phone.share.live = true;
					phone.share.status = "Sharing your screen";
				}
				Report::Failed(message) => self.end_share(Some(message)),
				_ => {}
			}
		} else if phone.watch.want.is_some_and(|(want, _)| want == id) {
			match report {
				Report::Status(status) => phone.watch.status = status,
				Report::Live => {
					phone.watch.live = true;
					phone.watch.status = "";
				}
				Report::Failed(message) => self.end_watch(Some(message)),
				_ => {}
			}
		}
		self.pump(now);
	}

	/// Brings the wire, the link, the media runs and the streams in line with the intent.
	/// Idempotent, and safe after any input.
	pub(crate) fn pump(&mut self, now: Instant) {
		// The plain offline preview only renders fixture calls; nothing drives them.
		if self.offline_preview() {
			return;
		}
		// Access can disappear with any update; the call ends rather than failing in place.
		if let Some(channel) = self.voice.active.as_ref().map(|call| call.channel)
			&& self.voice_revoked(channel)
		{
			self.end_call(Some("The call is no longer available"));
		}
		self.expire(now);
		self.pump_wire(now);
		self.pump_media(now);
		self.pump_stream(now, true);
		self.pump_stream(now, false);
	}

	fn expire(&mut self, now: Instant) {
		let phone = &mut self.voice.phone;
		if phone.wire.pending.is_some_and(|at| now >= at) {
			phone.wire.pending = None;
		}
		if phone.retry.at.is_some_and(|at| now >= at) {
			phone.retry.at = None;
		}
		let mut lost = [false; 2];
		for (stream, lost) in [&mut phone.share.stream, &mut phone.watch.stream]
			.into_iter()
			.zip(&mut lost)
		{
			match stream.pending {
				Some((StreamOp::Delete, at)) if now >= at => stream.deleted(),
				// A late answer to a lost create still finds the key, and is deleted then.
				Some((StreamOp::Create, at)) if now >= at => {
					stream.pending = None;
					*lost = true;
				}
				_ => {}
			}
		}
		if lost[0] && phone.share.want.is_some() {
			self.end_share(Some("Discord did not start the screen share; try again"));
		}
		if lost[1] && self.voice.phone.watch.want.is_some() {
			self.end_watch(Some("The stream did not start; try watching again"));
		}
	}

	fn desired(&self) -> Target {
		let told = self.voice.phone.wire.told;
		match &self.voice.active {
			Some(_) if self.voice.phone.wire.rejoin && told.channel.is_some() => {
				Target::leave(told.guild)
			}
			Some(call) => Target {
				guild: call.guild,
				channel: Some(call.channel),
				mute: call.self_muted(),
				deaf: call.deafened,
				video: call.camera,
			},
			None => Target::leave(told.guild),
		}
	}

	fn pump_wire(&mut self, now: Instant) {
		if !self.gateway_connected || self.voice.phone.wire.pending.is_some() {
			return;
		}
		let desired = self.desired();
		let phone = &mut self.voice.phone;
		let told = phone.wire.told;
		if told.channel.is_none() {
			phone.wire.rejoin = false;
		}
		if desired.same(&told) && !std::mem::take(&mut phone.wire.resend) {
			return;
		}
		// Switching channels is a full leave and a fresh join, so every event after the leave
		// belongs to the new connection.
		let target = if told.channel.is_some()
			&& desired.channel.is_some()
			&& told.channel != desired.channel
		{
			Target::leave(told.guild)
		} else {
			desired
		};
		if target.channel != told.channel {
			phone.link.reset();
			if told.channel.is_some() {
				phone.share.stream.doomed(now);
				phone.watch.stream.doomed(now);
			}
			if let (None, Some(channel)) = (target.guild, target.channel) {
				// Unofficial opcode 13 prepares a private call before the voice state.
				phone.send(Command::Sync { channel });
			}
		}
		phone.send(Command::VoiceState {
			guild: target.guild,
			channel: target.channel,
			mute: target.mute,
			deaf: target.deaf,
			video: target.video,
		});
		if target.channel != told.channel
			&& let (Some(guild), Some(_)) = (target.guild, target.channel)
		{
			phone.send(Command::ChannelInfo { guild });
		}
		phone.wire.told = target;
		phone.wire.pending = Some(now + ACK_TIMEOUT);
	}

	fn pump_media(&mut self, now: Instant) {
		let phone = &mut self.voice.phone;
		let wanted = self.voice.active.as_ref().filter(|call| {
			phone.wire.told.channel == Some(call.channel)
				&& phone.link.session.is_some()
				&& phone.link.server.is_some()
				&& phone.retry.at.is_none()
		});
		let current = phone.media.as_ref().is_some_and(|run| {
			wanted.is_some_and(|call| run.channel == call.channel)
				&& run.revision == phone.link.revision
		});
		if !current {
			phone.media = wanted.map(|call| Run {
				id: next_id(),
				channel: call.channel,
				revision: phone.link.revision,
				phase: Phase::ConnectingTransport,
				ready: false,
				camera: false,
			});
		}
		let Some(call) = &mut self.voice.active else {
			return;
		};
		let Some(run) = &phone.media else {
			call.phase = Phase::Connecting;
			return;
		};
		call.phase = run.phase;
		if run.ready {
			call.connected_at.get_or_insert(now);
			if phone.ring == Some(call.request) {
				phone.ring = None;
				let channel = call.channel;
				phone.send(Command::Ring {
					channel,
					recipient: None,
					stop: false,
				});
			}
		}
	}

	/// Own share (`own`) or watched stream: create or delete the key on Discord to match.
	fn pump_stream(&mut self, now: Instant, own: bool) {
		let ready = self.voice.media_ready();
		let call = self
			.voice
			.active
			.as_ref()
			.map(|call| (call.guild, call.channel, &call.participants));
		let key = match (call, self.owner()) {
			(Some((guild, channel, _)), Some(owner)) if own => self
				.voice
				.phone
				.share
				.want
				.map(|_| stream_key(guild, channel, owner)),
			(Some((guild, channel, participants)), Some(_)) => self
				.voice
				.phone
				.watch
				.want
				.filter(|(_, streamer)| {
					participants
						.iter()
						.any(|p| p.user == *streamer && p.streaming)
				})
				.map(|(_, streamer)| stream_key(guild, channel, streamer)),
			_ => None,
		};
		let call = call.map(|(guild, channel, _)| (guild, channel));
		if !self.gateway_connected {
			return;
		}
		let phone = &mut self.voice.phone;
		let stream = if own {
			&mut phone.share.stream
		} else {
			&mut phone.watch.stream
		};
		if stream.pending.is_some() {
			return;
		}
		if stream.exists {
			// Exists but unwanted, or wanted under another key: delete it first.
			if key.is_none() || key != stream.key {
				let key = stream.key.clone().unwrap_or_default();
				stream.pending = Some((StreamOp::Delete, now + STREAM_TIMEOUT));
				phone.send(Command::StreamDelete { key });
			}
		} else if let Some(key) = key.filter(|_| ready) {
			stream.key = Some(key.clone());
			stream.rtc = None;
			stream.server = None;
			stream.pending = Some((StreamOp::Create, now + STREAM_TIMEOUT));
			let command = match call {
				Some((guild, channel)) if own => Command::StreamCreate { guild, channel },
				_ => Command::StreamWatch { key },
			};
			phone.send(command);
		}
	}
}
