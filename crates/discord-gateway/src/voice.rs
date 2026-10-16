//! Voice signaling on the main Gateway: framing the call controller's commands and parsing
//! voice, call and stream dispatches into its events. No call state lives here; the
//! controller in `client_core::voice` decides everything. Never joins on its own.
use client_core::{
	Event,
	auth::Failure,
	voice::{self, Command, Participant, RosterEntry, Secret, StreamEvent},
};
use discord_protocol::{
	CallDto, ChannelInfoDto, GuildDto, UserDto, VoiceChannelStartTimeDto, VoiceMemberDto,
	VoiceServerDto, VoiceStateDto, decode, stream,
};
use model::{Id, Member, User};
use serde_json::json;
use std::collections::BTreeMap;
use tokio_tungstenite::tungstenite::Message as Frame;
use zeroize::Zeroizing;

const MAX_STREAM_KEY: usize = 128;

#[derive(Default)]
pub(super) struct Calls {
	/// Channels that can hold a call: private calls (`None`) and guild voice channels.
	pub(super) allowed: BTreeMap<Id, Option<Id>>,
	/// The guild voice channel last asked for, so its start-time updates are kept.
	joined: Option<(Id, Id)>,
	// Optional metadata, retained until READY_SUPPLEMENTAL or reset within the roster budget.
	// Overflow leaves participants using the existing user-ID fallback.
	pub(super) users: BTreeMap<Id, User>,
}
impl Calls {
	/// The socket dropped. A RESUME keeps Discord's voice state.
	pub(super) fn disconnected(&mut self) {
		self.users.clear();
	}
	/// A fresh READY: Discord dropped every voice state bound to the previous session.
	pub(super) fn session_reset(&mut self) {
		self.joined = None;
		self.users.clear();
	}
	pub(super) fn invalidate(&mut self, channel: Id) {
		self.allowed.remove(&channel);
		if self.joined.is_some_and(|(_, joined)| joined == channel) {
			self.joined = None;
		}
	}
	pub(super) fn remember_users(&mut self, users: Vec<UserDto>) {
		self.users.clear();
		let mut bytes = 0;
		for user in users {
			if self.users.len() == voice::MAX_ROSTER {
				break;
			}
			if user.id.0 == 0 || self.users.contains_key(&user.id) {
				continue;
			}
			let user = user.into_model();
			let added = size_of::<User>() + user.heap_bytes();
			if bytes + added <= voice::MAX_ROSTER_BYTES {
				bytes += added;
				self.users.insert(user.id, user);
			}
		}
	}
	fn members(&self, members: Vec<VoiceMemberDto>) -> BTreeMap<Id, Member> {
		let mut retained = BTreeMap::new();
		let mut bytes = 0;
		for member in members {
			if retained.len() == voice::MAX_ROSTER {
				break;
			}
			let Some(member) = self.member(member) else {
				continue;
			};
			if member.user.id.0 == 0 || retained.contains_key(&member.user.id) {
				continue;
			}
			let added = member.bytes();
			if bytes + added <= voice::MAX_ROSTER_BYTES {
				bytes += added;
				retained.insert(member.user.id, member);
			}
		}
		retained
	}
	fn member(&self, member: VoiceMemberDto) -> Option<Member> {
		Some(Member {
			roles: vec![],
			user: member
				.user
				.map(UserDto::into_model)
				.or_else(|| member.user_id.and_then(|id| self.users.get(&id).cloned()))?,
			nick: member.nick.map(|n| n.chars().take(128).collect()),
			status: None,
			custom_status: None,
			activities: vec![],
			clients: model::ClientPlatforms::default(),
		})
	}
	pub(super) fn snapshot(&self, guild: &mut GuildDto, partial: bool) -> Result<Event, Failure> {
		let members = self.members(std::mem::take(&mut guild.members));
		let mut participants = Vec::new();
		let mut bytes = 0;
		for mut state in std::mem::take(&mut guild.voice_states) {
			let _secret = state.session_id.take().map(Zeroizing::new);
			let Some(channel) = state.channel_id else {
				continue;
			};
			if self.allowed.get(&channel) != Some(&Some(guild.id)) {
				continue;
			}
			if participants.len() == voice::MAX_ROSTER {
				return Err(Failure::CapacityAt(
					"Voice roster participant limit exceeded",
				));
			}
			let participant = participant(&state);
			let member = state
				.member
				.and_then(|m| self.member(m))
				.or_else(|| members.get(&state.user_id).cloned());
			let entry = RosterEntry {
				guild: guild.id,
				channel,
				participant,
				member,
			};
			bytes += entry.bytes();
			if bytes > voice::MAX_ROSTER_BYTES {
				return Err(Failure::CapacityAt("Voice roster byte limit exceeded"));
			}
			participants.push(entry);
		}
		Ok(Event::Voice(voice::Event::Snapshot {
			partial,
			guild: Some(guild.id),
			participants,
		}))
	}
	pub(super) fn passive(
		&mut self,
		mut update: discord_protocol::PassiveVoiceUpdate,
		owner: Option<Id>,
		session: Option<&str>,
		emit: &impl Fn(Event) -> Result<(), Failure>,
	) -> Result<(), Failure> {
		if update.updated_voice_states.len() + update.removed_voice_states.len() > voice::MAX_ROSTER
		{
			return Err(Failure::Capacity);
		}
		let Some(guild) = update.guild_id else {
			return if update.updated_voice_states.is_empty()
				&& update.removed_voice_states.is_empty()
			{
				Ok(())
			} else {
				Err(Failure::Protocol)
			};
		};
		let members = self.members(update.updated_members);
		for mut state in update.updated_voice_states.drain(..) {
			state.guild_id = Some(guild);
			let member = members.get(&state.user_id).cloned();
			self.state(state, member, owner, session, false, emit)?;
		}
		for user in update.removed_voice_states {
			self.state(
				VoiceStateDto {
					guild_id: Some(guild),
					channel_id: None,
					user_id: user,
					member: None,
					session_id: None,
					self_mute: false,
					self_deaf: false,
					mute: false,
					deaf: false,
					suppress: false,
					self_video: false,
					self_stream: false,
				},
				None,
				owner,
				session,
				false,
				emit,
			)?;
		}
		Ok(())
	}
	/// Frames one controller command for the Gateway; REST work (ringing) yields nothing.
	pub(super) fn packet(&mut self, command: &Command) -> Result<Option<Frame>, Failure> {
		let value = match command {
			Command::Sync { channel } => {
				if self.allowed.get(channel) != Some(&None) {
					return Ok(None);
				}
				// Unofficial opcode 13 requests existing call state; it never joins or rings.
				json!({"op":13,"d":{"channel_id":channel}})
			}
			Command::VoiceState {
				guild,
				channel,
				mute,
				deaf,
				video,
			} => {
				if let Some(channel) = channel {
					if self.allowed.get(channel) != Some(guild) {
						return Err(Failure::Forbidden);
					}
					self.joined = guild.map(|guild| (guild, *channel));
				} else {
					self.joined = None;
				}
				json!({"op":4,"d":{"guild_id":guild,"channel_id":channel,"self_mute":mute,"self_deaf":deaf,"self_video":video}})
			}
			Command::ChannelInfo { guild } => {
				json!({"op":43,"d":{"guild_id":guild,"fields":["voice_start_time"]}})
			}
			Command::StreamCreate { guild, channel } => {
				if self.allowed.get(channel) != Some(guild) {
					return Err(Failure::Forbidden);
				}
				let kind = if guild.is_some() { "guild" } else { "call" };
				json!({"op":18,"d":{"type":kind,"guild_id":guild,"channel_id":channel,"preferred_region":null}})
			}
			Command::StreamDelete { key } | Command::StreamWatch { key }
				if key.len() > MAX_STREAM_KEY =>
			{
				return Err(Failure::Protocol);
			}
			Command::StreamDelete { key } => json!({"op":19,"d":{"stream_key":key}}),
			Command::StreamWatch { key } => json!({"op":20,"d":{"stream_key":key}}),
			Command::Ring { .. } | Command::Regions | Command::SetRegion { .. } => return Ok(None),
		};
		Ok(Some(Frame::Text(value.to_string().into())))
	}
	fn channel_started(
		&self,
		guild: Id,
		channel: Id,
		unix_seconds: Option<u64>,
		emit: &impl Fn(Event) -> Result<(), Failure>,
	) -> Result<(), Failure> {
		if self.joined == Some((guild, channel)) && self.allowed.get(&channel) == Some(&Some(guild))
		{
			emit(Event::Voice(voice::Event::ChannelStarted {
				guild,
				channel,
				unix_seconds,
			}))?;
		}
		Ok(())
	}
	fn state(
		&mut self,
		mut state: VoiceStateDto,
		member: Option<Member>,
		owner: Option<Id>,
		session: Option<&str>,
		direct: bool,
		emit: &impl Fn(Event) -> Result<(), Failure>,
	) -> Result<(), Failure> {
		let voice_session = state.session_id.take().map(Zeroizing::new);
		let own = owner == Some(state.user_id);
		let allowed = state
			.channel_id
			.is_none_or(|channel| self.allowed.get(&channel) == Some(&state.guild_id));
		if !allowed && !own {
			return Ok(());
		}
		// Discord names the Gateway session holding a voice state: ours, or another device's.
		// A direct departure may omit it; it is then ours to acknowledge. Passive guild updates
		// never carry one, so they never speak for this device.
		let ours = own
			&& match voice_session.as_deref() {
				Some(voice) => session == Some(voice.as_str()),
				None => direct,
			};
		// The controller decides whether an own state is this device's; it needs the session.
		let secret = match &voice_session {
			Some(value) if own => Some(Secret::new(value.to_string())?),
			_ => None,
		};
		let participant = participant(&state);
		emit(Event::Voice(voice::Event::State {
			guild: state.guild_id,
			channel: state.channel_id.filter(|_| allowed),
			user: state.user_id,
			ours,
			session: secret,
			member: state
				.member
				.and_then(|m| self.member(m))
				.or(member)
				.map(Box::new),
			muted: participant.muted,
			deafened: participant.deafened,
			server_muted: participant.server_muted,
			server_deafened: participant.server_deafened,
			video: participant.video,
			streaming: participant.streaming,
		}))
	}
	fn stream(
		&self,
		kind: &str,
		data: &[u8],
		emit: &impl Fn(Event) -> Result<(), Failure>,
	) -> Result<(), Failure> {
		let Ok(key) = decode::<stream::Key>(data) else {
			return Ok(());
		};
		if key.stream_key.len() > MAX_STREAM_KEY {
			return Ok(());
		}
		let event = match kind {
			"STREAM_CREATE" => {
				let Ok(created) = decode::<stream::Created>(data) else {
					return Ok(());
				};
				StreamEvent::Created {
					rtc_server: created.rtc_server_id,
					// Viewer payloads have been seen without it; the RTC server then stands in.
					rtc_channel: created.rtc_channel_id.unwrap_or(created.rtc_server_id),
				}
			}
			"STREAM_SERVER_UPDATE" => {
				let Ok(mut server) = decode::<stream::ServerUpdate>(data) else {
					return Ok(());
				};
				if server.token.len() > 2048
					|| server
						.endpoint
						.as_ref()
						.is_some_and(|endpoint| endpoint.len() > 512)
				{
					return Err(Failure::Capacity);
				}
				let token = Zeroizing::new(std::mem::take(&mut server.token));
				StreamEvent::Server {
					token: Secret::new(token.to_string()).ok(),
					endpoint: server.endpoint,
				}
			}
			_ => StreamEvent::Deleted {
				reason: decode::<stream::Deleted>(data)
					.ok()
					.and_then(|deleted| deletion_reason(deleted.reason.as_deref())),
			},
		};
		emit(Event::Voice(voice::Event::Stream {
			key: key.stream_key,
			event,
		}))
	}
	pub(super) fn dispatch(
		&mut self,
		kind: &str,
		data: &[u8],
		owner: Option<Id>,
		session: Option<&str>,
		emit: &impl Fn(Event) -> Result<(), Failure>,
	) -> Result<(), Failure> {
		match kind {
			"STREAM_CREATE" | "STREAM_SERVER_UPDATE" | "STREAM_DELETE" => {
				self.stream(kind, data, emit)?;
			}
			"CALL_CREATE" | "CALL_UPDATE" | "CALL_DELETE" => {
				let call: CallDto = decode(data).map_err(|_| Failure::Protocol)?;
				if self.allowed.get(&call.channel_id) != Some(&None) {
					return Ok(());
				}
				if kind == "CALL_DELETE" {
					emit(Event::Voice(voice::Event::Deleted {
						channel: call.channel_id,
					}))?;
				} else {
					if call
						.ringing
						.as_ref()
						.is_some_and(|v| v.len() > voice::MAX_PARTICIPANTS)
						|| call
							.voice_states
							.as_ref()
							.is_some_and(|v| v.len() > voice::MAX_PARTICIPANTS)
					{
						return Err(Failure::Capacity);
					}
					let participants = call.voice_states.map(|states| {
						states
							.into_iter()
							.map(|mut state| {
								let _secret = state.session_id.take().map(Zeroizing::new);
								participant(&state)
							})
							.collect()
					});
					emit(Event::Voice(voice::Event::Call {
						channel: call.channel_id,
						ringing: call.ringing,
						participants,
						unavailable: call.unavailable,
					}))?;
				}
			}
			"VOICE_STATE_UPDATE" => self.state(
				decode(data).map_err(|_| Failure::Protocol)?,
				None,
				owner,
				session,
				true,
				emit,
			)?,
			"CHANNEL_INFO" => {
				let info: ChannelInfoDto = decode(data).map_err(|_| Failure::Protocol)?;
				if info.channels.len() > client_core::MAX_NAV {
					return Err(Failure::Capacity);
				}
				if let Some((channel, unix_seconds)) = info
					.channels
					.into_iter()
					.find(|channel| self.joined.is_some_and(|(_, joined)| joined == channel.id))
					.map(|channel| (channel.id, channel.voice_start_time))
				{
					self.channel_started(info.guild_id, channel, unix_seconds, emit)?;
				}
			}
			"VOICE_CHANNEL_START_TIME_UPDATE" => {
				let update: VoiceChannelStartTimeDto =
					decode(data).map_err(|_| Failure::Protocol)?;
				self.channel_started(update.guild_id, update.id, update.voice_start_time, emit)?;
			}
			"VOICE_SERVER_UPDATE" => {
				let mut server: VoiceServerDto = decode(data).map_err(|_| Failure::Protocol)?;
				let token = Zeroizing::new(std::mem::take(&mut server.token));
				if server.endpoint.as_ref().is_some_and(|s| s.len() > 512) {
					return Err(Failure::Capacity);
				}
				emit(Event::Voice(voice::Event::Server {
					guild: server.guild_id,
					channel: server.channel_id,
					token: Secret::new(token.to_string()).ok(),
					endpoint: server.endpoint,
				}))?;
			}
			_ => {}
		}
		Ok(())
	}
}

fn participant(state: &VoiceStateDto) -> Participant {
	Participant {
		user: state.user_id,
		muted: state.self_mute || state.mute || state.suppress || state.self_deaf || state.deaf,
		deafened: state.self_deaf || state.deaf,
		server_muted: state.mute || state.suppress,
		server_deafened: state.deaf,
		video: state.self_video,
		streaming: state.self_stream,
	}
}

/// Maps Discord's STREAM_DELETE `reason` to a user-facing message. Unknown values still
/// tell the user Discord ended it, so a silent stop is never mistaken for a local one. With
/// `LEECH_VOICE_DIAGNOSTICS=1` the bounded raw value also reaches stderr for reports.
fn deletion_reason(raw: Option<&str>) -> Option<&'static str> {
	let raw = raw?;
	if raw.len() > 64 {
		return Some("Discord ended the stream");
	}
	if std::env::var_os("LEECH_VOICE_DIAGNOSTICS").is_some_and(|v| v == "1") {
		eprintln!("[Leech voice Stream] discord_delete_reason={raw}");
	}
	Some(match raw {
		"user_requested" => return None,
		"stream_ended" => "Discord reported the stream as ended",
		"stream_full" => "Discord reported the stream as full",
		"unauthorized" => "Discord refused the stream: not authorized",
		"safety_guidelines_violated" => "Discord ended the stream for a safety guideline",
		"session_terminated" => "Discord terminated the stream session",
		_ => "Discord ended the stream",
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn frame(calls: &mut Calls, command: Command) -> Option<serde_json::Value> {
		calls
			.packet(&command)
			.unwrap()
			.map(|frame| serde_json::from_str(frame.to_text().unwrap()).unwrap())
	}
	fn collect(
		calls: &mut Calls,
		kind: &str,
		data: serde_json::Value,
		session: Option<&str>,
	) -> Vec<voice::Event> {
		let events = std::sync::Mutex::new(Vec::new());
		calls
			.dispatch(
				kind,
				data.to_string().as_bytes(),
				Some(Id(1)),
				session,
				&|event| {
					if let Event::Voice(event) = event {
						events.lock().unwrap().push(event);
					}
					Ok(())
				},
			)
			.unwrap();
		events.into_inner().unwrap()
	}

	#[test]
	fn deletion_reasons_map_to_messages_and_user_requests_stay_silent() {
		assert_eq!(deletion_reason(None), None);
		assert_eq!(deletion_reason(Some("user_requested")), None);
		assert_eq!(
			deletion_reason(Some("stream_ended")),
			Some("Discord reported the stream as ended")
		);
		assert_eq!(
			deletion_reason(Some(&"x".repeat(65))),
			Some("Discord ended the stream")
		);
	}

	#[test]
	fn commands_frame_only_admitted_channels() {
		let mut calls = Calls::default();
		calls.allowed.insert(Id(2), None);
		calls.allowed.insert(Id(20), Some(Id(10)));
		assert_eq!(frame(&mut calls, Command::Sync { channel: Id(20) }), None);
		assert_eq!(
			frame(&mut calls, Command::Sync { channel: Id(2) }).unwrap(),
			json!({"op":13,"d":{"channel_id":"2"}})
		);
		let join = Command::VoiceState {
			guild: Some(Id(10)),
			channel: Some(Id(20)),
			mute: true,
			deaf: false,
			video: false,
		};
		assert_eq!(
			frame(&mut calls, join).unwrap(),
			json!({"op":4,"d":{"guild_id":"10","channel_id":"20","self_mute":true,"self_deaf":false,"self_video":false}})
		);
		assert!(
			calls
				.packet(&Command::VoiceState {
					guild: None,
					channel: Some(Id(20)),
					mute: false,
					deaf: false,
					video: false,
				})
				.is_err(),
			"a guild channel never joins as a private call"
		);
		assert!(
			calls
				.packet(&Command::StreamCreate {
					guild: None,
					channel: Id(9)
				})
				.is_err()
		);
		assert_eq!(
			frame(
				&mut calls,
				Command::StreamCreate {
					guild: None,
					channel: Id(2)
				}
			)
			.unwrap(),
			json!({"op":18,"d":{"type":"call","guild_id":null,"channel_id":"2","preferred_region":null}})
		);
		assert_eq!(
			frame(
				&mut calls,
				Command::StreamWatch {
					key: "call:2:3".into()
				}
			)
			.unwrap(),
			json!({"op":20,"d":{"stream_key":"call:2:3"}})
		);
		assert!(
			calls
				.packet(&Command::StreamDelete {
					key: "x".repeat(129)
				})
				.is_err()
		);
		assert_eq!(
			frame(
				&mut calls,
				Command::Ring {
					channel: Id(2),
					recipient: None,
					stop: false
				}
			),
			None,
			"ringing is REST"
		);
	}

	#[test]
	fn own_voice_states_name_their_device_and_carry_only_our_session() {
		let mut calls = Calls::default();
		calls.allowed.insert(Id(2), None);
		let state = |session: &str, channel: Option<&str>| json!({"user_id":"1","channel_id":channel,"session_id":session,"self_mute":true});
		let events = collect(
			&mut calls,
			"VOICE_STATE_UPDATE",
			state("gateway", Some("2")),
			Some("gateway"),
		);
		let voice::Event::State {
			ours: true,
			session: Some(secret),
			muted: true,
			..
		} = &events[0]
		else {
			panic!("our own voice state")
		};
		assert_eq!(secret.expose(), "gateway");
		let events = collect(
			&mut calls,
			"VOICE_STATE_UPDATE",
			state("phone", Some("2")),
			Some("gateway"),
		);
		let voice::Event::State {
			ours: false,
			session: Some(secret),
			..
		} = &events[0]
		else {
			panic!("another device's voice state")
		};
		assert_eq!(secret.expose(), "phone");
		let events = collect(
			&mut calls,
			"VOICE_STATE_UPDATE",
			state("gateway", None),
			Some("gateway"),
		);
		assert!(matches!(
			&events[0],
			voice::Event::State {
				ours: true,
				session: Some(_),
				channel: None,
				..
			}
		));
		// Other users never carry a session, and calls outside navigation are dropped.
		let events = collect(
			&mut calls,
			"VOICE_STATE_UPDATE",
			json!({"user_id":"3","channel_id":"9","session_id":"x"}),
			Some("gateway"),
		);
		assert!(events.is_empty());
	}

	#[test]
	fn servers_and_streams_pass_through_bounded() {
		let mut calls = Calls::default();
		let events = collect(
			&mut calls,
			"VOICE_SERVER_UPDATE",
			json!({"guild_id":null,"channel_id":"2","token":"t","endpoint":"e.discord.media"}),
			None,
		);
		assert!(matches!(
			&events[0],
			voice::Event::Server {
				channel: Some(Id(2)),
				token: Some(_),
				endpoint: Some(_),
				..
			}
		));
		let events = collect(
			&mut calls,
			"STREAM_CREATE",
			json!({"stream_key":"call:2:1","rtc_server_id":"30"}),
			None,
		);
		assert!(matches!(
			&events[0],
			voice::Event::Stream {
				key,
				event: StreamEvent::Created {
					rtc_server: Id(30),
					rtc_channel: Id(30)
				}
			} if key == "call:2:1"
		));
		let events = collect(
			&mut calls,
			"STREAM_DELETE",
			json!({"stream_key":"call:2:1","reason":"stream_full"}),
			None,
		);
		assert!(matches!(
			&events[0],
			voice::Event::Stream {
				event: StreamEvent::Deleted { reason: Some(_) },
				..
			}
		));
		assert!(
			collect(
				&mut calls,
				"STREAM_DELETE",
				json!({"stream_key":"x".repeat(129)}),
				None
			)
			.is_empty()
		);
	}

	#[test]
	fn optional_login_users_are_bounded_without_rejecting_the_session() {
		{
			let user = |id, name: &str| UserDto {
				premium_type: model::Patch::Absent,
				primary_guild: None,
				avatar_decoration_data: None,
				collectibles: None,
				clan: None,
				id: Id(id),
				username: name.into(),
				global_name: None,
				bot: false,
				public_flags: 0,
				flags: 0,
				avatar: None,
				discriminator: String::new(),
			};
			let mut calls = Calls::default();
			calls.remember_users((1..=5000).map(|id| user(id, "Short")).collect());
			assert_eq!(calls.users.len(), voice::MAX_ROSTER);
			let long = "\u{754c}".repeat(128);
			calls.remember_users((1..=3000).map(|id| user(id, &long)).collect());
			assert!(!calls.users.is_empty());
			assert!(calls.users.len() < 3000);
			assert!(
				calls
					.users
					.values()
					.map(|u| size_of::<User>() + u.heap_bytes())
					.sum::<usize>() <= voice::MAX_ROSTER_BYTES
			);
			let mut users: Vec<_> = (0..5000).map(|_| user(1, &long)).collect();
			users.push(user(0, "Invalid"));
			users.push(user(2, "Other"));
			calls.remember_users(users);
			assert_eq!(calls.users.len(), 2);
			assert_eq!(calls.users[&Id(2)].name, "Other");
			calls.disconnected();
			assert!(calls.users.is_empty());
			calls.remember_users(vec![user(1, "Again")]);
			calls.session_reset();
			assert!(calls.users.is_empty());
		}
		{
			let mut calls = Calls::default();
			calls.allowed.insert(Id(20), Some(Id(10)));
			let member = |id: u64, name: &str| {
				decode::<VoiceMemberDto>(
					json!({"user":{"id":id.to_string(),"username":name}})
						.to_string()
						.as_bytes(),
				)
				.unwrap()
			};
			let mut guild: GuildDto = decode(br#"{"id":"10"}"#).unwrap();
			guild.members = (1..=5000u64).map(|id| member(id, "Short")).collect();
			guild.voice_states = (0..5000)
				.map(|_| decode(br#"{"channel_id":null,"user_id":"1"}"#).unwrap())
				.collect();
			guild
				.voice_states
				.push(decode(br#"{"channel_id":"999","user_id":"2"}"#).unwrap());
			guild
				.voice_states
				.push(decode(br#"{"channel_id":"20","user_id":"5000"}"#).unwrap());
			let Event::Voice(voice::Event::Snapshot { participants, .. }) =
				calls.snapshot(&mut guild, true).unwrap()
			else {
				panic!()
			};
			assert_eq!(participants.len(), 1);
			assert!(participants[0].member.is_none());
			let long = "\u{754c}".repeat(128);
			let retained = calls.members((1..=3000u64).map(|id| member(id, &long)).collect());
			assert!(!retained.is_empty());
			assert!(retained.len() < 3000);
			assert!(retained.values().map(Member::bytes).sum::<usize>() <= voice::MAX_ROSTER_BYTES);
			let retained = calls.members(
				(0..5000)
					.map(|_| member(1u64, &long))
					.chain([member(2u64, "Other")])
					.collect(),
			);
			assert_eq!(retained.len(), 2);
		}
	}
}
