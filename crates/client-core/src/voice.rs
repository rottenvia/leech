//! Calls: one wanted DM or guild voice call, its screen share and watched stream, and the
//! bounded rosters of calls this account can see. The lifecycle lives in `phone`.
use crate::{
	State as ClientState,
	auth::{AuthState, Failure},
	screen::Settings,
};
use model::{Id, Member};
use std::{
	fmt,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

mod phone;
#[cfg(any(test, feature = "sim"))]
pub mod sim;

pub use phone::{
	ACK_TIMEOUT, MAX_RETRIES, MediaPlan, STREAM_TIMEOUT, SharePlan, ShareState, WatchPlan,
};

pub const MAX_PARTICIPANTS: usize = 64;
pub const MAX_DM_CALLS: usize = 64;
pub const MAX_DM_CALL_BYTES: usize = MAX_DM_CALLS * size_of::<Id>();
pub const MAX_ROSTER: usize = 4096;
pub const MAX_ROSTER_BYTES: usize = 1024 * 1024;

pub struct Secret(Zeroizing<String>);
impl Secret {
	pub fn new(value: String) -> Result<Self, Failure> {
		let value = Zeroizing::new(value);
		if value.is_empty() || value.len() > 2048 || !value.bytes().all(|b| b.is_ascii_graphic()) {
			return Err(Failure::Protocol);
		}
		Ok(Self(value))
	}
	pub fn expose(&self) -> &str {
		&self.0
	}
	pub(crate) fn bytes(&self) -> usize {
		self.0.capacity()
	}
	pub(crate) fn duplicate(&self) -> Self {
		Self(Zeroizing::new(self.0.as_str().to_owned()))
	}
}
impl fmt::Debug for Secret {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("VoiceSecret([REDACTED])")
	}
}
/// Everything one media worker needs to reach a voice server.
pub struct VoiceConnection {
	pub channel: Id,
	pub guild: Option<Id>,
	pub user: Id,
	/// Expected peer for a one-to-one DM; groups use the authenticated voice roster.
	pub peer: Option<Id>,
	pub session: Secret,
	pub token: Secret,
	pub endpoint: String,
	/// The media run this connection belongs to.
	pub request: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
	/// Waiting for Discord's session and voice server.
	Connecting,
	ConnectingTransport,
	Discovering,
	OpeningAudio,
	Securing,
	Connected,
	/// Connected, and nobody else is in the call yet.
	Waiting,
}
impl Phase {
	pub fn label(self) -> &'static str {
		match self {
			Self::Connecting => "Connecting call…",
			Self::ConnectingTransport => "Connecting to voice server…",
			Self::Discovering => "Checking voice network…",
			Self::OpeningAudio => "Opening audio devices…",
			Self::Securing => "Securing audio…",
			Self::Waiting => "Connected · waiting for others",
			Self::Connected => "Voice connected",
		}
	}
	pub fn connected(self) -> bool {
		matches!(self, Self::Connected | Self::Waiting)
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Participant {
	pub user: Id,
	pub muted: bool,
	pub deafened: bool,
	pub server_muted: bool,
	pub server_deafened: bool,
	/// Camera on (`self_video`); frames arrive on the call transport.
	pub video: bool,
	/// Go Live screen share announced (`self_stream`); viewing needs an explicit watch.
	pub streaming: bool,
}
#[derive(Clone)]
pub struct RosterEntry {
	pub guild: Id,
	pub channel: Id,
	pub participant: Participant,
	pub member: Option<Member>,
}
impl RosterEntry {
	pub fn bytes(&self) -> usize {
		size_of::<Self>() + self.member.as_ref().map_or(0, Member::bytes)
	}
}

pub struct StreamPreview {
	pub guild: Id,
	pub channel: Id,
	pub user: Id,
	pub request: u64,
	pub loading: bool,
	pub url: Option<String>,
	pub error: Option<&'static str>,
}

/// The call this device wants to be in, as the interface shows it.
pub struct Call {
	pub channel: Id,
	pub guild: Option<Id>,
	pub connected_at: Option<Instant>,
	pub channel_started_at: Option<Instant>,
	pub server_muted: bool,
	pub server_deafened: bool,
	/// This attempt; every join starts a new one.
	pub request: u64,
	pub phase: Phase,
	pub muted: bool,
	pub deafened: bool,
	pub participants: Vec<Participant>,
	pub camera: bool,
	/// Participant whose screen share this device explicitly chose to watch.
	pub watching: Option<Id>,
}
impl Call {
	/// Discord's microphone indicator: deafening always reads (and is sent) as muted.
	pub fn self_muted(&self) -> bool {
		self.muted || self.deafened
	}
}
/// Applies one local mute (`deafen == false`) or deafen toggle to the latest `(muted, deafened)`.
///
/// `muted` is the microphone choice kept underneath a deafen, so undeafening restores it.
/// Unmuting while deafened clears both, as in Discord. Callers must pass the newest state,
/// never a snapshot taken before an earlier toggle in the same burst.
pub fn toggle_self_voice(muted: bool, deafened: bool, deafen: bool) -> (bool, bool) {
	match (deafen, deafened) {
		(true, _) => (muted, !deafened),
		(false, true) => (false, false),
		(false, false) => (!muted, false),
	}
}

/// Gateway and REST work the call controller asks the desktop to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
	/// Unofficial opcode 13: refresh a private call's state; never joins or rings.
	Sync { channel: Id },
	/// Opcode 4: this account's whole voice state. `channel: None` leaves.
	VoiceState {
		guild: Option<Id>,
		channel: Option<Id>,
		mute: bool,
		deaf: bool,
		video: bool,
	},
	/// Opcode 43: ask for a guild voice channel's start time.
	ChannelInfo { guild: Id },
	/// Unofficial opcode 18: start this account's stream in the call.
	StreamCreate { guild: Option<Id>, channel: Id },
	/// Unofficial opcode 19: stop a stream, own or watched.
	StreamDelete { key: String },
	/// Unofficial opcode 20: watch another participant's stream.
	StreamWatch { key: String },
	/// REST ring or stop-ringing; `None` addresses every recipient.
	Ring {
		channel: Id,
		recipient: Option<Id>,
		stop: bool,
	},
}

/// One Go Live stream update from the Gateway, by stream key.
#[derive(Debug)]
pub enum StreamEvent {
	Created {
		rtc_server: Id,
		rtc_channel: Id,
	},
	Server {
		token: Option<Secret>,
		endpoint: Option<String>,
	},
	/// `reason` names Discord's cause when it sent one we recognise.
	Deleted {
		reason: Option<&'static str>,
	},
}

/// What a media worker reports about its run, by run id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Report {
	Progress(Phase),
	/// Connected and alone.
	Waiting,
	/// Connected with others.
	Ready,
	CameraAvailable(bool),
	Failed(&'static str),
	/// Share or watch progress text.
	Status(&'static str),
	/// Share or watch frames are flowing.
	Live,
}

pub enum Event {
	Snapshot {
		partial: bool,
		guild: Option<Id>,
		participants: Vec<RosterEntry>,
	},
	Call {
		channel: Id,
		ringing: Option<Vec<Id>>,
		participants: Option<Vec<Participant>>,
		unavailable: bool,
	},
	Deleted {
		channel: Id,
	},
	ChannelStarted {
		guild: Id,
		channel: Id,
		unix_seconds: Option<u64>,
	},
	/// One voice state. For this account, `ours` is true when this Gateway session holds it,
	/// and only then does `session` carry its voice session.
	State {
		guild: Option<Id>,
		member: Option<Box<Member>>,
		channel: Option<Id>,
		user: Id,
		ours: bool,
		session: Option<Secret>,
		muted: bool,
		deafened: bool,
		server_muted: bool,
		server_deafened: bool,
		video: bool,
		streaming: bool,
	},
	Server {
		guild: Option<Id>,
		channel: Option<Id>,
		token: Option<Secret>,
		endpoint: Option<String>,
	},
	Stream {
		key: String,
		event: StreamEvent,
	},
	Media {
		id: u64,
		report: Report,
	},
	/// A ring or stop-ringing request failed; the call and its media continue.
	RingFailed {
		channel: Id,
		message: &'static str,
	},
	/// A command could not be sent; the controller sends it again when it can.
	Unsent(Command),
}
impl Event {
	pub fn bytes(&self) -> usize {
		match self {
			Self::Snapshot { participants, .. } => {
				participants.capacity() * size_of::<RosterEntry>()
					+ participants.iter().map(RosterEntry::bytes).sum::<usize>()
			}
			Self::Call {
				ringing,
				participants,
				..
			} => {
				ringing
					.as_ref()
					.map_or(0, |v| v.capacity() * size_of::<Id>())
					+ participants
						.as_ref()
						.map_or(0, |v| v.capacity() * size_of::<Participant>())
			}
			Self::State {
				session, member, ..
			} => {
				session.as_ref().map_or(0, Secret::bytes)
					+ member.as_deref().map_or(0, Member::bytes)
			}
			Self::Server {
				token, endpoint, ..
			} => {
				token.as_ref().map_or(0, Secret::bytes)
					+ endpoint.as_ref().map_or(0, String::capacity)
			}
			Self::Stream { key, event } => {
				key.capacity()
					+ match event {
						StreamEvent::Server { token, endpoint } => {
							token.as_ref().map_or(0, Secret::bytes)
								+ endpoint.as_ref().map_or(0, String::capacity)
						}
						_ => 0,
					}
			}
			_ => 0,
		}
	}
}

#[derive(Default)]
pub struct State {
	pub active: Option<Call>,
	pub incoming: Option<Id>,
	/// Known service calls, independent of ringing and this device's media session.
	pub(crate) dm_calls: Vec<Id>,
	/// Last reported members of each known DM call, so answering or joining shows them at once.
	pub(crate) dm_participants: Vec<(Id, Vec<Participant>)>,
	/// Service-confirmed ringing, at most 64 calls with 64 recipient IDs each.
	pub(crate) dm_ringing: Vec<(Id, Vec<Id>)>,
	pub ring_error: Option<(Id, &'static str)>,
	pub roster: Vec<RosterEntry>,
	pub preview: Option<StreamPreview>,
	/// The offline demo drives calls against the simulated Discord instead of refusing them.
	pub simulated: bool,
	/// Why the call, share or watch last ended or reconnected, for the interface to show once.
	pub notice: Option<&'static str>,
	pub(crate) phone: phone::Phone,
	/// The controller's clock: the latest time it was given. Actions run on it, so deadlines
	/// never mix it with another clock.
	clock: Option<Instant>,
	sequence: u64,
}
impl State {
	/// Bounded, retained private-call membership across conversations.
	pub fn dm_call_participants(&self) -> impl Iterator<Item = (Id, &[Participant])> {
		self.dm_participants
			.iter()
			.map(|(channel, participants)| (*channel, participants.as_slice()))
	}
	/// Known private-call membership, including calls this device has not joined.
	pub fn dm_participants(&self, channel: Id) -> &[Participant] {
		self.dm_participants
			.iter()
			.find(|(id, _)| *id == channel)
			.map_or_else(
				|| {
					self.active
						.as_ref()
						.filter(|call| call.channel == channel && call.guild.is_none())
						.map_or(&[][..], |call| call.participants.as_slice())
				},
				|(_, participants)| participants.as_slice(),
			)
	}
	pub fn has_dm_call(&self, channel: Id) -> bool {
		self.dm_calls.contains(&channel)
	}
	pub fn ringing(&self, channel: Id) -> &[Id] {
		self.dm_ringing
			.iter()
			.find(|(id, _)| *id == channel)
			.map_or(&[], |(_, recipients)| recipients)
	}
	pub fn take_notice(&mut self) -> Option<&'static str> {
		self.notice.take()
	}
	fn next_request(&mut self) -> u64 {
		self.sequence = self.sequence.wrapping_add(1);
		self.sequence
	}
}

impl ClientState {
	pub fn request_stream_preview(
		&mut self,
		guild: Id,
		channel: Id,
		user: Id,
	) -> Option<crate::Command> {
		let request = self.voice.next_request();
		let allowed = !self.demo
			&& self.auth == AuthState::Authenticated
			&& self.gateway_connected
			&& self.has_voice_access(channel)
			&& self.voice.roster.iter().any(|entry| {
				entry.guild == guild
					&& entry.channel == channel
					&& entry.participant.user == user
					&& entry.participant.streaming
			});
		self.voice.preview = Some(StreamPreview {
			guild,
			channel,
			user,
			request,
			loading: allowed,
			url: None,
			error: (!allowed).then_some("Stream preview unavailable while disconnected"),
		});
		allowed.then_some(crate::Command::StreamPreview {
			guild,
			channel,
			user,
			request,
		})
	}

	pub(crate) fn apply_stream_preview(
		&mut self,
		guild: Id,
		channel: Id,
		user: Id,
		request: u64,
		result: Result<String, Failure>,
	) {
		if let Err(failure) = &result
			&& failure.ends_session()
			&& *failure != Failure::Capacity
		{
			self.fail(*failure);
			return;
		}
		if !self.can_call(channel) {
			if self
				.voice
				.preview
				.as_ref()
				.is_some_and(|preview| preview.channel == channel)
			{
				self.voice.preview = None;
			}
			return;
		}
		let Some(preview) = self.voice.preview.as_mut().filter(|preview| {
			preview.guild == guild
				&& preview.channel == channel
				&& preview.user == user
				&& preview.request == request
		}) else {
			return;
		};
		preview.loading = false;
		match result {
			Ok(url) if url.len() <= 2048 => preview.url = Some(url),
			Ok(_) => preview.error = Some("Stream preview response was too large"),
			Err(_) => preview.error = Some("Stream preview hidden or unavailable"),
		}
	}

	/// The outgoing call is still ringing its recipients.
	pub fn outgoing_ring(&self) -> Option<Id> {
		let call = self.voice.active.as_ref()?;
		let owner = self.owner()?;
		(call.guild.is_none()
			&& !call.participants.iter().any(|p| p.user != owner)
			&& self
				.voice
				.ringing(call.channel)
				.iter()
				.any(|id| *id != owner))
		.then_some(call.channel)
	}
	/// A call in this channel may run: signed in, with access. The socket may be resuming.
	pub(crate) fn voice_allowed(&self, channel: Id) -> bool {
		(!self.demo || self.voice.simulated)
			&& self.auth == AuthState::Authenticated
			&& self.has_voice_access(channel)
	}
	/// A wanted call must end: signed out, the channel is gone, or Discord denies access.
	/// Permissions still loading after a fresh READY keep it.
	pub(crate) fn voice_revoked(&self, channel: Id) -> bool {
		self.auth != AuthState::Authenticated
			|| self.channel(channel).is_none()
			|| self.permission(
				channel,
				model::permissions::VIEW_CHANNEL | model::permissions::CONNECT,
			) == Some(false)
	}
	/// The desktop could not queue a call command; the controller sends it again.
	pub(crate) fn voice_command_rejected(&mut self, command: Command) {
		match command {
			Command::VoiceState { .. } => {
				self.voice.phone.wire.pending = None;
				self.voice.phone.wire.resend = true;
			}
			Command::Ring { channel, .. } => self.apply_voice(Event::RingFailed {
				channel,
				message: "Ringing was not sent; the work queue is full",
			}),
			Command::Sync { .. } | Command::ChannelInfo { .. } => {}
			// A lost stream operation times out and is retried by the controller.
			Command::StreamCreate { .. }
			| Command::StreamDelete { .. }
			| Command::StreamWatch { .. } => {
				self.status = "Screen-share control was not sent; retrying";
			}
		}
	}
	/// A call in this channel may start now.
	pub fn can_call(&self, channel: Id) -> bool {
		self.voice_allowed(channel) && self.gateway_connected
	}
	pub(crate) fn has_voice_access(&self, channel: Id) -> bool {
		self.permission(
			channel,
			model::permissions::VIEW_CHANNEL | model::permissions::CONNECT,
		) == Some(true)
			&& self.channels.iter().any(|c| {
				c.id == channel
					&& ((c.guild.is_some() && c.kind == 2)
						|| (c.guild.is_none()
							&& ((c.kind == 1 && c.recipients.len() == 1)
								|| (c.kind == 3 && c.recipients.len() < MAX_PARTICIPANTS))))
			})
	}
	fn dm_call_participant(&self, channel: Id, user: Id) -> bool {
		self.channel(channel).is_some_and(|c| {
			c.guild.is_none()
				&& matches!(c.kind, 1 | 3)
				&& (self.owner() == Some(user) || c.recipients.iter().any(|r| r.id == user))
		})
	}
	pub fn can_camera(&self, channel: Id) -> bool {
		self.can_call(channel) && self.permission(channel, model::permissions::STREAM) == Some(true)
	}

	pub fn start_call(&mut self, channel: Id, ring: bool) -> bool {
		self.start_call_with_mute(channel, ring, false, false)
	}
	/// Wants this call. Any current call is left first; nothing older blocks the join.
	pub fn start_call_with_mute(
		&mut self,
		channel: Id,
		ring: bool,
		muted: bool,
		deafened: bool,
	) -> bool {
		if !self.can_call(channel) {
			return false;
		}
		let Some(guild) = self.channel(channel).map(|c| c.guild) else {
			return false;
		};
		if self
			.voice
			.active
			.as_ref()
			.is_some_and(|call| call.channel == channel)
		{
			return true;
		}
		let participants: Vec<_> = if guild.is_some() {
			self.voice
				.roster
				.iter()
				.filter(|r| r.channel == channel)
				.map(|r| r.participant)
				.collect()
		} else {
			self.voice.dm_participants(channel).to_vec()
		};
		if participants.len() >= MAX_PARTICIPANTS {
			self.status = "Voice channel exceeds the 64 participant limit";
			return false;
		}
		let now = self.voice_now();
		if self.voice.active.is_some() {
			self.leave_call();
		}
		let own = participants.iter().find(|p| Some(p.user) == self.owner());
		let (server_muted, server_deafened) =
			own.map_or((false, false), |p| (p.server_muted, p.server_deafened));
		let muted = muted || !self.can_speak(channel);
		let request = self.voice.next_request();
		// Ring only a new private call; joining an existing one is silent.
		let ring = ring
			&& guild.is_none()
			&& !self.voice.has_dm_call(channel)
			&& !participants.iter().any(|p| Some(p.user) != self.owner());
		self.voice.phone.ring = ring.then_some(request);
		self.voice.ring_error = None;
		self.voice.active = Some(Call {
			channel,
			guild,
			connected_at: None,
			channel_started_at: None,
			server_muted,
			server_deafened,
			request,
			phase: Phase::Connecting,
			muted,
			deafened,
			camera: false,
			watching: None,
			participants,
		});
		if self.voice.incoming == Some(channel) {
			self.voice.incoming = None;
		}
		self.pump(now);
		true
	}
	pub fn leave_call(&mut self) -> bool {
		let Some(call) = self.voice.active.as_ref() else {
			return false;
		};
		let (channel, guild) = (call.channel, call.guild);
		let owner = self.owner();
		if guild.is_none()
			&& self
				.voice
				.ringing(channel)
				.iter()
				.any(|id| Some(*id) != owner)
		{
			// Hanging up while still ringing stops the ringing, as in Discord.
			self.voice.phone.send(Command::Ring {
				channel,
				recipient: None,
				stop: true,
			});
		}
		self.voice.ring_error = None;
		self.end_call(None);
		self.pump(self.voice_now());
		true
	}
	/// Demo without the simulated Discord: calls only render.
	pub fn offline_preview(&self) -> bool {
		self.demo && !self.voice.simulated
	}
	/// The wanted call's media is connected; a rendered offline fixture counts by its phase.
	pub fn call_connected(&self) -> bool {
		if self.offline_preview() {
			self.voice
				.active
				.as_ref()
				.is_some_and(|call| call.phase.connected())
		} else {
			self.voice.media_ready()
		}
	}
	/// One recipient of the connected private call can be rung (or stop ringing) now.
	pub fn can_ring_recipient(&self, recipient: Id, stop: bool) -> bool {
		let Some(call) = self.voice.active.as_ref() else {
			return false;
		};
		let channel = call.channel;
		call.guild.is_none()
			&& call.phase.connected()
			&& self.can_call(channel)
			&& self.owner().is_some_and(|own| own != recipient)
			&& self.dm_call_participant(channel, recipient)
			&& (stop || !call.participants.iter().any(|p| p.user == recipient))
			&& self.voice.ringing(channel).contains(&recipient) == stop
	}
	pub fn ring_recipient(&mut self, recipient: Id, stop: bool) -> bool {
		if !self.can_ring_recipient(recipient, stop) {
			return false;
		}
		let channel = self.voice.active.as_ref().expect("active call").channel;
		self.voice.phone.send(Command::Ring {
			channel,
			recipient: Some(recipient),
			stop,
		});
		true
	}
	pub fn set_call_mute(&mut self, muted: bool, deafened: bool) -> bool {
		let Some(channel) = self.voice.active.as_ref().map(|call| call.channel) else {
			return false;
		};
		let muted = muted || (!self.demo && !self.can_speak(channel));
		let call = self.voice.active.as_mut().expect("active call");
		call.muted = muted;
		call.deafened = deafened;
		self.pump(self.voice_now());
		true
	}
	pub fn decline_call(&mut self) -> bool {
		let (Some(channel), Some(owner)) = (self.voice.incoming.take(), self.owner()) else {
			return false;
		};
		self.voice.phone.send(Command::Ring {
			channel,
			recipient: Some(owner),
			stop: true,
		});
		true
	}
	pub fn set_call_camera(&mut self, enabled: bool) -> bool {
		let Some(call) = self.voice.active.as_ref() else {
			return false;
		};
		if call.camera == enabled {
			return true;
		}
		if enabled && (!self.call_connected() || !self.can_camera(call.channel)) {
			return false;
		}
		self.voice.active.as_mut().expect("active call").camera = enabled;
		self.pump(self.voice_now());
		true
	}
	/// Start sharing the screen in the connected call. Restarting replaces the previous share.
	pub fn share_screen(&mut self, settings: Settings) -> bool {
		let Some(channel) = self.voice.active.as_ref().map(|call| call.channel) else {
			return false;
		};
		if !settings.valid() || !self.can_stream(channel) || !self.call_connected() {
			return false;
		}
		let share = &mut self.voice.phone.share;
		share.want = Some((phone::next_id(), settings));
		share.live = false;
		share.status = "Starting screen share…";
		self.pump(self.voice_now());
		true
	}
	pub fn stop_sharing(&mut self) {
		self.end_share(None);
		self.pump(self.voice_now());
	}
	/// Watch one streaming participant of the connected call.
	pub fn watch_stream(&mut self, streamer: Id) -> bool {
		let owner = self.owner();
		let connected = self.call_connected();
		let Some(call) = self.voice.active.as_mut() else {
			return false;
		};
		if !connected
			|| owner == Some(streamer)
			|| !call
				.participants
				.iter()
				.any(|p| p.user == streamer && p.streaming)
		{
			return false;
		}
		if call.watching == Some(streamer) {
			return true;
		}
		call.watching = Some(streamer);
		let watch = &mut self.voice.phone.watch;
		watch.want = Some((phone::next_id(), streamer));
		watch.live = false;
		watch.status = "Connecting to the stream…";
		self.pump(self.voice_now());
		true
	}
	pub fn stop_watching(&mut self) {
		self.end_watch(None);
		self.pump(self.voice_now());
	}
	/// Credentials for the current media run, or `None` if `id` is no longer current.
	pub fn media_credentials(&self, id: u64) -> Option<VoiceConnection> {
		let phone = &self.voice.phone;
		let run = phone.media.as_ref().filter(|run| run.id == id)?;
		let call = self.voice.active.as_ref()?;
		let server = phone.link.server.as_ref()?;
		Some(VoiceConnection {
			channel: run.channel,
			guild: call.guild,
			user: self.owner()?,
			peer: self.dm_peer(run.channel),
			session: phone.link.session.as_ref()?.duplicate(),
			token: server.token.duplicate(),
			endpoint: server.endpoint.clone(),
			request: id,
		})
	}
	/// Credentials for the current share or watch run, on the stream's own RTC server.
	pub fn stream_credentials(&self, id: u64) -> Option<VoiceConnection> {
		let phone = &self.voice.phone;
		let stream = if phone.share.want.is_some_and(|(want, _)| want == id) {
			&phone.share.stream
		} else if phone.watch.want.is_some_and(|(want, _)| want == id) {
			&phone.watch.stream
		} else {
			return None;
		};
		let call = self.voice.active.as_ref()?;
		let (rtc_server, rtc_channel) = stream.rtc?;
		let server = stream.server.as_ref()?;
		Some(VoiceConnection {
			channel: rtc_channel,
			guild: Some(rtc_server),
			user: self.owner()?,
			peer: self.dm_peer(call.channel),
			session: phone.link.session.as_ref()?.duplicate(),
			token: server.token.duplicate(),
			endpoint: server.endpoint.clone(),
			request: id,
		})
	}
	/// Only one-to-one DMs pin a peer; group calls use the authenticated voice roster.
	fn dm_peer(&self, channel: Id) -> Option<Id> {
		self.channel(channel)
			.filter(|c| c.guild.is_none() && c.kind == 1)
			.and_then(|c| c.recipients.first().map(|u| u.id))
	}

	pub fn apply_voice(&mut self, event: Event) {
		self.apply_voice_at(event, Instant::now());
	}
	pub(crate) fn apply_voice_at(&mut self, event: Event, now: Instant) {
		self.voice.clock = Some(self.voice.clock.map_or(now, |clock| clock.max(now)));
		match event {
			Event::RingFailed { channel, message } => {
				if self
					.voice
					.active
					.as_ref()
					.is_some_and(|call| call.channel == channel)
				{
					self.status = message;
					self.voice.notice = Some(message);
					self.voice.ring_error = Some((channel, message));
				}
			}
			Event::Media { id, report } => self.media_report(id, report, now),
			Event::Unsent(command) => {
				self.voice_command_rejected(command);
				self.pump(now);
			}
			Event::Stream { key, event } => self.stream_event(&key, event, now),
			Event::Server {
				guild,
				channel,
				token,
				endpoint,
			} => self.voice_server(guild, channel, token, endpoint, now),
			Event::Snapshot {
				partial,
				guild,
				participants,
			} => {
				if !partial {
					self.voice
						.roster
						.retain(|r| Some(r.guild) != guild && guild.is_some());
				}
				for entry in participants {
					if guild.is_some_and(|guild| entry.guild != guild) {
						continue;
					}
					if !self.update_roster(entry) {
						break;
					}
				}
				self.drop_stale_preview();
				self.refresh_voice_participants();
				self.pump(now);
			}
			Event::Call {
				channel,
				ringing,
				participants,
				unavailable,
			} => self.apply_call(channel, ringing, participants, unavailable, now),
			Event::Deleted { channel } => {
				self.end_voice_channel(channel);
				self.pump(now);
			}
			Event::ChannelStarted {
				guild,
				channel,
				unix_seconds,
			} => {
				if let Some(call) = &mut self.voice.active
					&& call.guild == Some(guild)
					&& call.channel == channel
				{
					call.channel_started_at = unix_seconds.and_then(|timestamp| {
						let age = SystemTime::now()
							.duration_since(UNIX_EPOCH)
							.ok()?
							.as_secs()
							.checked_sub(timestamp)?;
						Instant::now().checked_sub(Duration::from_secs(age))
					});
				}
			}
			Event::State {
				guild,
				member,
				channel,
				user,
				ours,
				session,
				muted,
				deafened,
				server_muted,
				server_deafened,
				video,
				streaming,
			} => {
				let participant = Participant {
					user,
					muted,
					deafened,
					server_muted,
					server_deafened,
					video,
					streaming,
				};
				self.apply_participant(guild, member, channel, participant);
				if self.owner() == Some(user) {
					if let Some(call) = &mut self.voice.active
						&& channel == Some(call.channel)
						&& guild == call.guild
					{
						call.server_muted = server_muted;
						call.server_deafened = server_deafened;
					}
					self.own_voice_state(ours, session, guild, channel, now);
				} else {
					self.pump(now);
				}
			}
		}
	}

	fn drop_stale_preview(&mut self) {
		if self.voice.preview.as_ref().is_some_and(|preview| {
			!self.voice.roster.iter().any(|entry| {
				entry.guild == preview.guild
					&& entry.channel == preview.channel
					&& entry.participant.user == preview.user
					&& entry.participant.streaming
			})
		}) {
			self.voice.preview = None;
		}
	}

	/// Roster and private-call membership for one voice state, the account's own included.
	fn apply_participant(
		&mut self,
		guild: Option<Id>,
		member: Option<Box<Member>>,
		channel: Option<Id>,
		participant: Participant,
	) {
		let user = participant.user;
		if let Some(guild) = guild {
			let previous = self
				.voice
				.roster
				.iter()
				.find(|r| r.guild == guild && r.participant.user == user)
				.and_then(|r| r.member.clone());
			self.voice
				.roster
				.retain(|r| r.guild != guild || r.participant.user != user);
			if let Some(channel) = channel {
				self.update_roster(RosterEntry {
					guild,
					channel,
					participant,
					member: member.map(|member| *member).or(previous),
				});
			}
		} else {
			if channel.is_some_and(|channel| !self.dm_call_participant(channel, user)) {
				return;
			}
			// DM voice states arrive whether or not this device has joined; keep the known
			// call membership current so a later join shows everyone.
			for (id, participants) in &mut self.voice.dm_participants {
				participants.retain(|p| p.user != user);
				if channel == Some(*id) && participants.len() < MAX_PARTICIPANTS {
					participants.push(participant);
				}
			}
			if let Some(channel) = channel
				&& self.voice.has_dm_call(channel)
				&& !self
					.voice
					.dm_participants
					.iter()
					.any(|(id, _)| *id == channel)
			{
				self.remember_dm_participants(channel, vec![participant]);
			}
		}
		if self.voice.preview.as_ref().is_some_and(|preview| {
			preview.user == user
				&& (guild != Some(preview.guild)
					|| channel != Some(preview.channel)
					|| !participant.streaming)
		}) {
			self.voice.preview = None;
		}
		let Some(call) = &mut self.voice.active else {
			return;
		};
		if guild != call.guild {
			return;
		}
		call.participants.retain(|p| p.user != user);
		if channel == Some(call.channel) {
			if call.participants.len() >= MAX_PARTICIPANTS {
				self.end_call(Some("Voice channel exceeds the 64 participant limit"));
				return;
			}
			call.participants.push(participant);
		}
		if call.watching == Some(user) && (channel != Some(call.channel) || !participant.streaming)
		{
			self.end_watch(None);
		}
	}

	fn apply_call(
		&mut self,
		channel: Id,
		ringing: Option<Vec<Id>>,
		participants: Option<Vec<Participant>>,
		unavailable: bool,
		now: Instant,
	) {
		if self.auth != AuthState::Authenticated
			|| !self.has_voice_access(channel)
			|| self.channel(channel).is_none_or(|c| c.guild.is_some())
		{
			return;
		}
		if ringing.as_ref().is_some_and(|r| {
			r.len() > MAX_PARTICIPANTS
				|| r.iter().enumerate().any(|(index, user)| {
					!self.dm_call_participant(channel, *user) || r[..index].contains(user)
				})
		}) || participants.as_ref().is_some_and(|p| {
			p.len() > MAX_PARTICIPANTS
				|| p.iter().enumerate().any(|(index, participant)| {
					!self.dm_call_participant(channel, participant.user)
						|| p[..index]
							.iter()
							.any(|other| other.user == participant.user)
				})
		}) {
			return;
		}
		if unavailable {
			self.end_voice_channel(channel);
			self.pump(now);
			return;
		}
		if !self.voice.has_dm_call(channel) {
			if self.voice.dm_calls.len() >= MAX_DM_CALLS
				|| (self.voice.dm_calls.len() + 1) * size_of::<Id>() > MAX_DM_CALL_BYTES
			{
				// ponytail: oldest call metadata is evicted; viewing that DM queries it again.
				let evicted = self.voice.dm_calls.remove(0);
				self.voice.dm_participants.retain(|(id, _)| *id != evicted);
				self.voice.dm_ringing.retain(|(id, _)| *id != evicted);
			}
			self.voice.dm_calls.push(channel);
		}
		if let Some(ringing) = ringing {
			let owner = self.owner();
			if owner.is_some_and(|own| ringing.contains(&own))
				&& self
					.voice
					.active
					.as_ref()
					.is_none_or(|c| c.channel != channel)
			{
				if self.voice.incoming.is_none() {
					self.voice.incoming = Some(channel);
				}
			} else if self.voice.incoming == Some(channel) {
				self.voice.incoming = None;
			}
			self.voice.dm_ringing.retain(|(id, _)| *id != channel);
			let mut retained = Vec::with_capacity(MAX_PARTICIPANTS);
			retained.extend(ringing);
			self.voice.dm_ringing.push((channel, retained));
		}
		if let Some(mut participants) = participants {
			participants.shrink_to_fit();
			if let Some(call) = &mut self.voice.active
				&& call.channel == channel
			{
				call.participants = participants.clone();
				if call
					.watching
					.is_some_and(|user| !participants.iter().any(|p| p.user == user && p.streaming))
				{
					self.end_watch(None);
				}
			}
			self.remember_dm_participants(channel, participants);
		}
		self.pump(now);
	}

	fn update_roster(&mut self, entry: RosterEntry) -> bool {
		if !self.can_view(entry.channel)
			|| !self
				.channels
				.iter()
				.any(|c| c.id == entry.channel && c.guild == Some(entry.guild) && c.kind == 2)
		{
			return true;
		}
		self.voice
			.roster
			.retain(|r| r.guild != entry.guild || r.participant.user != entry.participant.user);
		if self.voice.roster.len() >= MAX_ROSTER
			|| self
				.voice
				.roster
				.iter()
				.map(RosterEntry::bytes)
				.sum::<usize>()
				+ entry.bytes()
				> MAX_ROSTER_BYTES
		{
			self.disconnect_voice("Voice roster exceeds safe capacity; reconnect to refresh");
			self.status = "Voice roster exceeds safe capacity; reconnect to refresh";
			return false;
		}
		self.voice.roster.push(entry);
		true
	}
	pub(crate) fn refresh_voice_participants(&mut self) {
		let owner = self.owner();
		let Some(call) = self
			.voice
			.active
			.as_mut()
			.filter(|call| call.guild.is_some())
		else {
			return;
		};
		call.participants = self
			.voice
			.roster
			.iter()
			.filter(|r| r.channel == call.channel)
			.map(|r| r.participant)
			.take(MAX_PARTICIPANTS)
			.collect();
		if let Some(own) = call.participants.iter().find(|p| Some(p.user) == owner) {
			call.server_muted = own.server_muted;
			call.server_deafened = own.server_deafened;
		}
		if call
			.watching
			.is_some_and(|w| !call.participants.iter().any(|p| p.user == w && p.streaming))
		{
			self.end_watch(None);
		}
	}
	fn remember_dm_participants(&mut self, channel: Id, participants: Vec<Participant>) {
		if participants.len() > MAX_PARTICIPANTS || !self.voice.has_dm_call(channel) {
			return;
		}
		self.voice.dm_participants.retain(|(id, _)| *id != channel);
		while self.voice.dm_participants.len() >= MAX_DM_CALLS {
			self.voice.dm_participants.remove(0);
		}
		// Fixed capacity bounds every retained private-call roster by both items and bytes.
		let mut retained = Vec::with_capacity(MAX_PARTICIPANTS);
		retained.extend(participants);
		self.voice.dm_participants.push((channel, retained));
	}
	/// The channel is gone or no longer a call: forget its records and end a call there.
	pub(crate) fn end_voice_channel(&mut self, channel: Id) {
		self.voice.dm_calls.retain(|id| *id != channel);
		if self.voice.ring_error.is_some_and(|(id, _)| id == channel) {
			self.voice.ring_error = None;
		}
		self.voice.dm_participants.retain(|(id, _)| *id != channel);
		self.voice.dm_ringing.retain(|(id, _)| *id != channel);
		self.voice.roster.retain(|r| r.channel != channel);
		if self
			.voice
			.preview
			.as_ref()
			.is_some_and(|preview| preview.channel == channel)
		{
			self.voice.preview = None;
		}
		if self.voice.incoming == Some(channel) {
			self.voice.incoming = None;
		}
		if self
			.voice
			.active
			.as_ref()
			.is_some_and(|c| c.channel == channel && c.guild.is_some())
		{
			// A guild channel deletion ends the call; a private call record ending does not,
			// since our own voice state still says whether we are in it.
			self.end_call(Some("The call ended"));
		}
	}
	/// Forget every call record; the wanted call ends with `reason`.
	pub fn disconnect_voice(&mut self, reason: &'static str) {
		self.voice.dm_calls.clear();
		self.voice.dm_participants.clear();
		self.voice.dm_ringing.clear();
		self.voice.ring_error = None;
		self.voice.roster.clear();
		self.voice.preview = None;
		self.voice.incoming = None;
		self.end_call(Some(reason));
		self.pump(self.voice_now());
	}
}

#[cfg(test)]
mod tests;
