//! A simulated Discord for calls: Gateway voice signaling, voice and stream servers, and media
//! workers, with seeded latency, reordering-free delivery like a real socket, and optional
//! chaos (other devices, kicks, server moves, socket drops, media failures).
//!
//! It drives `State` exactly like the desktop does: it drains the commands the controller
//! asks for, answers them as Discord would, starts and stops pretend media runs by plan id,
//! and rejects media whose credentials are not Discord's current ones, as a voice server would.
//! Tests step it in virtual time; the offline demo steps it with the wall clock.
use super::{Command, Event, Phase, Report, Secret, StreamEvent, phone::stream_key};
use crate::State as ClientState;
use model::Id;
use std::{
	collections::BTreeMap,
	time::{Duration, Instant},
};

/// Small deterministic generator (xorshift64*); no dependency, same run for the same seed.
pub struct Rng(u64);
impl Rng {
	pub fn new(seed: u64) -> Self {
		Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
	}
	pub fn next_u64(&mut self) -> u64 {
		self.0 ^= self.0 >> 12;
		self.0 ^= self.0 << 25;
		self.0 ^= self.0 >> 27;
		self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
	}
	pub fn below(&mut self, n: u64) -> u64 {
		self.next_u64() % n.max(1)
	}
	pub fn chance(&mut self, percent: u64) -> bool {
		self.below(100) < percent
	}
	pub fn millis(&mut self, low: u64, high: u64) -> Duration {
		Duration::from_millis(low + self.below(high.saturating_sub(low) + 1))
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
	/// This Gateway session.
	Ours,
	/// Another device signed in to the same account.
	Elsewhere,
}

/// Discord's record of the account's voice state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Voice {
	pub holder: Holder,
	pub guild: Option<Id>,
	pub channel: Id,
	pub mute: bool,
	pub deaf: bool,
	pub video: bool,
	/// Current voice server token number; media must present the latest.
	pub token: u64,
}

pub struct Truth {
	pub voice: Option<Voice>,
	/// Live streams by key, with their token number.
	pub streams: BTreeMap<String, u64>,
	/// Other participants per channel: user and whether they stream.
	pub others: BTreeMap<Id, Vec<(Id, bool)>>,
}

enum Delivery {
	Event(Event),
	/// A media report; stale ids are delivered too, as real workers would.
	Media(u64, Report),
	/// A REST ring answer.
	Ring(Id, Option<&'static str>),
	/// A rung recipient picks up.
	Answer(Id, Id),
}

#[derive(Clone, Copy)]
pub struct Config {
	/// Gateway latency per event, in milliseconds.
	pub latency: (u64, u64),
	/// Percent of media runs that fail to connect.
	pub media_failures: u64,
	/// Percent of rings that fail.
	pub ring_failures: u64,
	/// Percent of "no change" voice states Discord leaves unanswered (they time out).
	pub silent: u64,
	/// Rung private-call recipients join after this long; `None` leaves them ringing.
	pub answer: Option<Duration>,
	/// Own voice states name the Gateway session. Discord does not document this, so the
	/// controller must also work when they never do.
	pub sessions_match: bool,
	/// Percent of voice states Discord loses entirely: no change and no answer.
	pub dropped: u64,
}
impl Default for Config {
	fn default() -> Self {
		Self {
			latency: (20, 180),
			media_failures: 0,
			ring_failures: 0,
			silent: 100,
			answer: None,
			sessions_match: true,
			dropped: 0,
		}
	}
}

struct Run {
	id: u64,
}

pub struct Discord {
	pub rng: Rng,
	pub config: Config,
	pub truth: Truth,
	queue: Vec<(Instant, u64, Delivery)>,
	order: u64,
	/// Last gateway delivery time; the socket keeps events in order.
	last: Option<Instant>,
	tokens: u64,
	media: Option<Run>,
	share: Option<Run>,
	watch: Option<Run>,
	/// Call and own-share runs a pretend server refused because their credentials were stale.
	pub rejected: u64,
	/// Watch runs refused: the streamer stopped before the client heard, a legitimate race.
	pub rejected_watch: u64,
	/// Voice states sent while an earlier one was unanswered and not yet timed out.
	pub overlapping: u64,
	/// When the last voice state went out, and whether Discord has answered it since.
	sent: Option<(Instant, bool)>,
	/// Gateway socket up; commands are lost while it is down.
	pub online: bool,
	/// Every command the client sent, in order.
	pub log: Vec<Command>,
	session: u64,
	base: std::sync::OnceLock<Instant>,
}

impl Discord {
	pub fn new(seed: u64, config: Config) -> Self {
		Self {
			rng: Rng::new(seed),
			config,
			truth: Truth {
				voice: None,
				streams: BTreeMap::new(),
				others: BTreeMap::new(),
			},
			queue: Vec::new(),
			order: 0,
			last: None,
			tokens: 0,
			media: None,
			share: None,
			watch: None,
			rejected: 0,
			rejected_watch: 0,
			overlapping: 0,
			sent: None,
			online: true,
			log: Vec::new(),
			session: 1,
			base: std::sync::OnceLock::new(),
		}
	}

	fn gateway(&mut self, now: Instant, event: Event) {
		let (low, high) = self.config.latency;
		let at = (now + self.rng.millis(low, high)).max(self.last.unwrap_or(now));
		self.last = Some(at);
		self.push(at, Delivery::Event(event));
	}
	fn push(&mut self, at: Instant, delivery: Delivery) {
		self.order += 1;
		self.queue.push((at, self.order, delivery));
	}
	fn token(&mut self) -> u64 {
		self.tokens += 1;
		self.tokens
	}
	fn secret(label: &str, number: u64) -> Secret {
		Secret::new(format!("{label}-{number}")).expect("synthetic secret")
	}

	/// Another participant's voice state, delivered to the client.
	pub fn other_state(
		&mut self,
		state: &ClientState,
		now: Instant,
		user: Id,
		channel: Option<Id>,
		streaming: bool,
	) {
		let guild_of = |channel: Id| state.channel(channel).and_then(|c| c.guild);
		let previous = self
			.truth
			.others
			.iter()
			.find(|(_, members)| members.iter().any(|(id, _)| *id == user))
			.map(|(channel, _)| *channel);
		for members in self.truth.others.values_mut() {
			members.retain(|(id, _)| *id != user);
		}
		if let Some(channel) = channel {
			self.truth
				.others
				.entry(channel)
				.or_default()
				.push((user, streaming));
		}
		let guild = channel.and_then(guild_of);
		let key = channel.map(|channel| stream_key(guild, channel, user));
		// A stream ends when its streamer stops, leaves or moves to another channel.
		let stopped: Vec<_> = self
			.truth
			.streams
			.keys()
			.filter(|k| {
				k.ends_with(&format!(":{user}")) && (!streaming || key.as_ref() != Some(*k))
			})
			.cloned()
			.collect();
		for key in stopped {
			self.truth.streams.remove(&key);
			self.gateway(
				now,
				Event::Stream {
					key,
					event: StreamEvent::Deleted { reason: None },
				},
			);
		}
		let state_event = |guild: Option<Id>, channel: Option<Id>, streaming: bool| Event::State {
			guild,
			member: None,
			channel,
			user,
			ours: false,
			session: None,
			muted: false,
			deafened: false,
			server_muted: false,
			server_deafened: false,
			video: false,
			streaming,
		};
		// Leaving a guild channel for a private call (or back) also leaves the old context.
		if let Some(previous) = previous
			&& guild_of(previous) != guild
		{
			self.gateway(now, state_event(guild_of(previous), None, false));
		}
		self.gateway(now, state_event(guild, channel, streaming));
		if streaming
			&& let Some(key) = key
			&& !self.truth.streams.contains_key(&key)
		{
			let token = self.token();
			self.truth.streams.insert(key, token);
		}
	}

	fn own_state(&mut self, state: &ClientState, now: Instant, voice: Option<Voice>, ours: bool) {
		let Some(owner) = state.owner() else {
			return;
		};
		let session = Some(if ours {
			Self::secret("session", self.session)
		} else {
			Self::secret("phone", self.session)
		});
		let ours = ours && self.config.sessions_match;
		self.gateway(
			now,
			Event::State {
				guild: voice.and_then(|voice| voice.guild),
				member: None,
				channel: voice.map(|voice| voice.channel),
				user: owner,
				ours,
				session,
				muted: voice.is_some_and(|voice| voice.mute || voice.deaf),
				deafened: voice.is_some_and(|voice| voice.deaf),
				server_muted: false,
				server_deafened: false,
				video: voice.is_some_and(|voice| voice.video),
				streaming: false,
			},
		);
	}

	fn server_update(&mut self, now: Instant, voice: Voice) {
		self.gateway(
			now,
			Event::Server {
				guild: voice.guild,
				channel: voice.guild.is_none().then_some(voice.channel),
				token: Some(Self::secret("voice", voice.token)),
				endpoint: Some(format!("sim-{}.discord.media", voice.token)),
			},
		);
	}

	fn drop_own_streams(&mut self, state: &ClientState, now: Instant, tell: bool) {
		let Some(owner) = state.owner() else {
			return;
		};
		let own: Vec<_> = self
			.truth
			.streams
			.keys()
			.filter(|key| key.ends_with(&format!(":{owner}")))
			.cloned()
			.collect();
		for key in own {
			self.truth.streams.remove(&key);
			if tell {
				self.gateway(
					now,
					Event::Stream {
						key,
						event: StreamEvent::Deleted { reason: None },
					},
				);
			}
		}
	}

	/// Discord's answer to one command from the client.
	pub(crate) fn trace(&self, now: Instant, what: &dyn std::fmt::Debug) {
		if std::env::var_os("SIM_TRACE").is_some() {
			let base = *self.base.get_or_init(|| now);
			eprintln!("{:>7} {what:?}", now.duration_since(base).as_millis());
		}
	}
	fn command(&mut self, state: &ClientState, now: Instant, command: Command) {
		self.trace(
			now,
			&format_args!("SEND {command:?} truth={:?}", self.truth.voice),
		);
		self.log.push(command.clone());
		if !self.online {
			return;
		}
		match command {
			Command::VoiceState {
				guild,
				channel,
				mute,
				deaf,
				video,
			} => {
				if let Some((at, answered)) = self.sent
					&& !answered && now < at + super::ACK_TIMEOUT
				{
					self.overlapping += 1;
				}
				self.sent = Some((now, false));
				if self.rng.chance(self.config.dropped) {
					self.trace(now, &"DROPPED");
					return;
				}
				let current = self.truth.voice;
				match channel {
					None => match current {
						Some(voice) if voice.holder == Holder::Ours => {
							self.truth.voice = None;
							self.drop_own_streams(state, now, true);
							self.own_state(state, now, None, true);
							if voice.guild.is_none() {
								self.call_update(state, now, voice.channel, Some(vec![]));
							}
						}
						// Nothing of ours to leave; Discord stays silent.
						_ => {}
					},
					Some(channel) => match current {
						Some(voice)
							if voice.holder == Holder::Ours
								&& voice.channel == channel
								&& voice.guild == guild =>
						{
							let next = Voice {
								mute,
								deaf,
								video,
								..voice
							};
							self.truth.voice = Some(next);
							if next != voice || !self.rng.chance(self.config.silent) {
								self.own_state(state, now, Some(next), true);
							}
						}
						_ => {
							if current.is_some_and(|voice| voice.holder == Holder::Ours) {
								self.drop_own_streams(state, now, true);
							}
							let voice = Voice {
								holder: Holder::Ours,
								guild,
								channel,
								mute,
								deaf,
								video,
								token: self.token(),
							};
							self.truth.voice = Some(voice);
							if guild.is_none() {
								self.call_update(state, now, channel, Some(vec![]));
							}
							if self.rng.chance(30) {
								// The server update sometimes comes first, or after a null endpoint.
								if self.rng.chance(50) {
									self.gateway(
										now,
										Event::Server {
											guild: voice.guild,
											channel: voice.guild.is_none().then_some(channel),
											token: None,
											endpoint: None,
										},
									);
								}
								self.server_update(now, voice);
								self.own_state(state, now, Some(voice), true);
							} else {
								self.own_state(state, now, Some(voice), true);
								self.server_update(now, voice);
							}
						}
					},
				}
			}
			Command::StreamCreate { guild, channel } => {
				let Some(owner) = state.owner() else {
					return;
				};
				let ours_here = self.truth.voice.is_some_and(|voice| {
					voice.holder == Holder::Ours && voice.channel == channel && voice.guild == guild
				});
				if !ours_here {
					return;
				}
				let key = stream_key(guild, channel, owner);
				let token = self.token();
				self.truth.streams.insert(key.clone(), token);
				self.stream_answer(now, key, token);
			}
			Command::StreamWatch { key } => {
				let ours_in_call = self
					.truth
					.voice
					.is_some_and(|voice| voice.holder == Holder::Ours);
				if let Some(token) = self.truth.streams.get(&key).copied()
					&& ours_in_call
				{
					self.stream_answer(now, key, token);
				}
			}
			Command::StreamDelete { key } => {
				let owner = state.owner().map(|owner| format!(":{owner}"));
				let own = owner.is_some_and(|owner| key.ends_with(&owner));
				if own && self.truth.streams.remove(&key).is_none() {
					return;
				}
				if !own && !self.truth.streams.contains_key(&key) {
					return;
				}
				self.gateway(
					now,
					Event::Stream {
						key,
						event: StreamEvent::Deleted { reason: None },
					},
				);
			}
			Command::Ring {
				channel,
				recipient,
				stop,
			} => {
				let failure = self
					.rng
					.chance(self.config.ring_failures)
					.then_some("Ringing failed");
				let at = now + self.rng.millis(30, 300);
				self.push(at, Delivery::Ring(channel, failure));
				if failure.is_some() {
					return;
				}
				let owner = state.owner();
				let present: Vec<Id> = self
					.truth
					.others
					.get(&channel)
					.into_iter()
					.flatten()
					.map(|(user, _)| *user)
					.collect();
				let recipients: Vec<Id> = state
					.channel(channel)
					.into_iter()
					.flat_map(|c| c.recipients.iter().map(|u| u.id))
					.filter(|id| Some(*id) != owner && !present.contains(id))
					.filter(|id| recipient.is_none_or(|one| one == *id))
					.collect();
				let ringing = if stop { vec![] } else { recipients.clone() };
				self.call_update(state, now, channel, Some(ringing));
				if !stop && let Some(delay) = self.config.answer {
					for user in recipients {
						self.push(now + delay, Delivery::Answer(channel, user));
					}
				}
			}
			Command::Sync { .. } | Command::ChannelInfo { .. } => {}
		}
	}

	/// CALL_CREATE/UPDATE for a private call, with everyone Discord has in it.
	fn call_update(
		&mut self,
		state: &ClientState,
		now: Instant,
		channel: Id,
		ringing: Option<Vec<Id>>,
	) {
		let owner = state.owner();
		let mut participants: Vec<_> = self
			.truth
			.others
			.get(&channel)
			.into_iter()
			.flatten()
			.map(|(user, streaming)| super::Participant {
				user: *user,
				muted: false,
				deafened: false,
				server_muted: false,
				server_deafened: false,
				video: false,
				streaming: *streaming,
			})
			.collect();
		if let (Some(owner), Some(voice)) = (owner, self.truth.voice)
			&& voice.channel == channel
		{
			participants.push(super::Participant {
				user: owner,
				muted: voice.mute || voice.deaf,
				deafened: voice.deaf,
				server_muted: false,
				server_deafened: false,
				video: voice.video,
				streaming: false,
			});
		}
		if participants.is_empty() && ringing.as_ref().is_none_or(Vec::is_empty) {
			self.gateway(now, Event::Deleted { channel });
			return;
		}
		self.gateway(
			now,
			Event::Call {
				channel,
				ringing,
				participants: Some(participants),
				unavailable: false,
			},
		);
	}

	/// Others already in a fixture's call, so a demo call starts populated.
	pub fn adopt(&mut self, state: &ClientState) {
		let Some(call) = &state.voice.active else {
			return;
		};
		let owner = state.owner();
		let members = call
			.participants
			.iter()
			.filter(|p| Some(p.user) != owner)
			.map(|p| (p.user, p.streaming))
			.collect();
		self.truth.others.insert(call.channel, members);
	}

	fn stream_answer(&mut self, now: Instant, key: String, token: u64) {
		self.gateway(
			now,
			Event::Stream {
				key: key.clone(),
				event: StreamEvent::Created {
					rtc_server: Id(900 + token),
					rtc_channel: Id(901 + token),
				},
			},
		);
		self.gateway(
			now,
			Event::Stream {
				key,
				event: StreamEvent::Server {
					token: Some(Self::secret("stream", token)),
					endpoint: Some(format!("sim-stream-{token}.discord.media")),
				},
			},
		);
	}

	/// Starts and stops pretend media by plan id, like the desktop's media runner.
	fn reconcile(&mut self, state: &ClientState, now: Instant) {
		let plan = state.voice.media_plan().map(|plan| plan.id);
		if self.media.as_ref().map(|run| run.id) != plan {
			self.media = None;
			if let Some(id) = plan {
				let credentials = state
					.media_credentials(id)
					.expect("current plan credentials");
				let valid = self.truth.voice.is_some_and(|voice| {
					voice.holder == Holder::Ours
						&& voice.channel == credentials.channel
						&& credentials.token.expose() == format!("voice-{}", voice.token)
				});
				let fail = self.rng.chance(self.config.media_failures);
				let mut at = now + self.rng.millis(10, 60);
				self.push(
					at,
					Delivery::Media(id, Report::Progress(Phase::ConnectingTransport)),
				);
				at += self.rng.millis(20, 120);
				if !valid {
					self.trace(
						now,
						&format_args!(
							"REJECT media {id} token={} truth={:?}",
							credentials.token.expose(),
							self.truth.voice
						),
					);
					self.rejected += 1;
					self.push(
						at,
						Delivery::Media(id, Report::Failed("Voice server refused the session")),
					);
				} else if fail {
					self.push(
						at,
						Delivery::Media(id, Report::Failed("Voice connection failed")),
					);
				} else {
					self.push(at, Delivery::Media(id, Report::Progress(Phase::Securing)));
					at += self.rng.millis(20, 120);
					self.push(at, Delivery::Media(id, Report::CameraAvailable(true)));
					let alone = self
						.truth
						.others
						.get(&credentials.channel)
						.is_none_or(Vec::is_empty);
					self.push(
						at,
						Delivery::Media(
							id,
							if alone {
								Report::Waiting
							} else {
								Report::Ready
							},
						),
					);
				}
				self.media = Some(Run { id });
			}
		}
		let plans = [
			(state.voice.share_plan().map(|plan| plan.id), true),
			(state.voice.watch_plan().map(|plan| plan.id), false),
		];
		for (plan, own) in plans {
			let slot = if own { &self.share } else { &self.watch };
			if slot.as_ref().map(|run| run.id) == plan {
				continue;
			}
			let mut run = None;
			if let Some(id) = plan {
				let credentials = state
					.stream_credentials(id)
					.expect("current stream credentials");
				let live = self
					.truth
					.streams
					.values()
					.any(|token| credentials.token.expose() == format!("stream-{token}"));
				let at = now + self.rng.millis(20, 150);
				if live {
					self.push(at, Delivery::Media(id, Report::Live));
				} else {
					self.trace(
						now,
						&format_args!(
							"REJECT stream {id} token={} truth={:?}",
							credentials.token.expose(),
							self.truth.streams
						),
					);
					if own {
						self.rejected += 1;
					} else {
						self.rejected_watch += 1;
					}
					self.push(
						at,
						Delivery::Media(id, Report::Failed("Stream server refused the session")),
					);
				}
				run = Some(Run { id });
			}
			if own {
				self.share = run;
			} else {
				self.watch = run;
			}
		}
	}

	/// One step: send what the client asked for, deliver everything due, keep media in line.
	pub fn step(&mut self, state: &mut ClientState, now: Instant) {
		state.voice_tick(now);
		loop {
			let commands: Vec<_> = state.voice.take_commands().collect();
			for command in commands {
				self.command(state, now, command);
			}
			self.reconcile(state, now);
			let Some(index) = self
				.queue
				.iter()
				.enumerate()
				.filter(|(_, (at, _, delivery))| {
					*at <= now && (self.online || !matches!(delivery, Delivery::Event(_)))
				})
				.min_by_key(|(_, (at, order, _))| (*at, *order))
				.map(|(index, _)| index)
			else {
				break;
			};
			let (_, _, delivery) = self.queue.remove(index);
			match &delivery {
				Delivery::Event(event) => {
					self.trace(now, &format_args!("  GOT {}", describe(event)))
				}
				Delivery::Media(id, report) => {
					self.trace(now, &format_args!("  MEDIA {id} {report:?}"))
				}
				Delivery::Ring(..) => {}
				Delivery::Answer(channel, user) => {
					self.trace(now, &format_args!("  ANSWER {channel:?} {user:?}"))
				}
			}
			match delivery {
				Delivery::Event(event) => {
					if let Event::State { user, .. } = &event
						&& state.owner() == Some(*user)
						&& let Some((at, _)) = self.sent
					{
						self.sent = Some((at, true));
					}
					state.apply_voice_at(event, now);
				}
				Delivery::Media(id, report) => {
					state.apply_voice_at(Event::Media { id, report }, now);
				}
				Delivery::Ring(channel, failure) => {
					if let Some(message) = failure {
						state.apply_voice_at(Event::RingFailed { channel, message }, now);
					}
				}
				Delivery::Answer(channel, user) => {
					// Only while this account is still in the call it rang from.
					if self.truth.voice.is_some_and(|voice| {
						voice.holder == Holder::Ours && voice.channel == channel
					}) {
						self.other_state(state, now, user, Some(channel), false);
						self.call_update(state, now, channel, Some(vec![]));
					}
				}
			}
		}
	}

	/// Steps virtual time from `now` until Discord has nothing left to say (at most 30 s);
	/// offline fixtures start already connected. Returns the virtual time reached.
	pub fn settle(&mut self, state: &mut ClientState, now: Instant) -> Instant {
		let mut at = now;
		for _ in 0..3000 {
			at += Duration::from_millis(10);
			self.step(state, at);
			if self.idle()
				&& state
					.voice
					.next_deadline()
					.is_none_or(|due| due > at + Duration::from_secs(1))
			{
				break;
			}
		}
		at
	}
	/// The next time anything is due, for pacing a wall-clock driver.
	pub fn next_due(&self) -> Option<Instant> {
		self.queue.iter().map(|(at, _, _)| *at).min()
	}
	pub fn idle(&self) -> bool {
		self.queue.is_empty()
	}

	// ---- chaos: Discord-side changes nobody on this device asked for ----

	/// Another device of the account joins `channel`.
	pub fn other_device_joins(&mut self, state: &ClientState, now: Instant, channel: Id) {
		let guild = state.channel(channel).and_then(|c| c.guild);
		if self
			.truth
			.voice
			.is_some_and(|voice| voice.holder == Holder::Ours)
		{
			self.drop_own_streams(state, now, true);
		}
		let voice = Voice {
			holder: Holder::Elsewhere,
			guild,
			channel,
			mute: false,
			deaf: false,
			video: false,
			token: self.token(),
		};
		self.truth.voice = Some(voice);
		self.own_state(state, now, Some(voice), false);
	}
	/// A moderator or Discord disconnects the account's voice.
	pub fn kick(&mut self, state: &ClientState, now: Instant) {
		if self
			.truth
			.voice
			.is_some_and(|voice| voice.holder == Holder::Ours)
		{
			self.truth.voice = None;
			self.drop_own_streams(state, now, true);
			self.own_state(state, now, None, true);
		}
	}
	/// Discord moves the call to another voice server.
	pub fn migrate(&mut self, now: Instant) {
		if let Some(mut voice) = self
			.truth
			.voice
			.filter(|voice| voice.holder == Holder::Ours)
		{
			voice.token = self.token();
			self.truth.voice = Some(voice);
			self.gateway(
				now,
				Event::Server {
					guild: voice.guild,
					channel: voice.guild.is_none().then_some(voice.channel),
					token: None,
					endpoint: None,
				},
			);
			self.server_update(now, voice);
		}
	}
	/// A running media run drops, as a lost voice socket would.
	pub fn media_drop(&mut self, now: Instant) {
		if let Some(run) = &self.media {
			let id = run.id;
			self.push(
				now + Duration::from_millis(5),
				Delivery::Media(id, Report::Failed("Voice connection lost")),
			);
		}
	}
	/// The Gateway socket drops; commands are lost until `resume`.
	pub fn disconnect(&mut self, state: &mut ClientState, now: Instant) {
		self.online = false;
		// An operation sent before the drop may be lost; sending it again is expected.
		self.sent = None;
		state.gateway_connected = false;
		state.voice_disconnected(now);
	}
	/// The socket resumes: events Discord produced meanwhile are replayed in order, then
	/// RESUMED marks the Gateway connected again.
	pub fn resume(&mut self, state: &mut ClientState, now: Instant) {
		self.online = true;
		self.step(state, now);
		state.gateway_connected = true;
		self.step(state, now);
	}
	/// A fresh session replaces the old one: Discord drops the old session's voice state and
	/// streams without telling the new session, and events still queued for the old socket
	/// are lost.
	pub fn new_session(&mut self, state: &mut ClientState, now: Instant) {
		self.queue.retain(|(_, _, delivery)| {
			matches!(delivery, Delivery::Media(..) | Delivery::Ring(..))
		});
		self.last = None;
		self.sent = None;
		if self
			.truth
			.voice
			.is_some_and(|voice| voice.holder == Holder::Ours)
		{
			self.truth.voice = None;
		}
		self.drop_own_streams(state, now, false);
		self.session += 1;
		self.online = true;
		state.gateway_connected = true;
		state.voice_session_reset(now);
	}
}

fn describe(event: &Event) -> String {
	match event {
		Event::State {
			user,
			channel,
			ours,
			session,
			muted,
			video,
			streaming,
			..
		} => format!(
			"VSU user={user:?} channel={channel:?} ours={ours} session={} mute={muted} video={video} stream={streaming}",
			session.as_ref().map_or("-", |s| s.expose())
		),
		Event::Server {
			guild,
			channel,
			token,
			endpoint,
		} => format!(
			"SERVER guild={guild:?} channel={channel:?} token={} endpoint={endpoint:?}",
			token.as_ref().map_or("-", |s| s.expose())
		),
		Event::Stream { key, event } => format!("STREAM {key} {event:?}"),
		_ => "other".into(),
	}
}
