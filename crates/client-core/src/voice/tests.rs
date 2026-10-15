use super::{sim::*, *};
use crate::screen::{Settings, SourceId};
use model::{Channel, User};

const OWNER: Id = Id(1);
const DM: Id = Id(2);
const PEER: Id = Id(3);
const GUILD: Id = Id(10);
const ROOM: Id = Id(20);
const OTHER_ROOM: Id = Id(21);

fn user(id: u64, name: &str) -> User {
	User {
		primary_guild: None,
		cosmetics: None,
		id: Id(id),
		name: name.into(),
		avatar: None,
		webhook: false,
		kind: Default::default(),
		discriminator: 0,
	}
}
fn channel(id: Id, guild: Option<Id>, kind: u8, recipients: Vec<User>) -> Channel {
	Channel {
		id,
		guild,
		kind,
		name: "Synthetic".into(),
		last_message: None,
		parent_id: None,
		position: 0,
		recipients,
		icon: None,
		member_list_id: None,
		tags: None,
		message_count: None,
	}
}
fn fixture() -> ClientState {
	let mut state = ClientState {
		auth: AuthState::Authenticated,
		gateway_connected: true,
		user: Some(user(OWNER.0, "Owner")),
		guilds: vec![model::Guild {
			default_message_notifications: None,
			id: GUILD,
			name: "Synthetic".into(),
			icon: None,
			emojis: None,
			stickers: None,
		}],
		channels: vec![
			channel(DM, None, 1, vec![user(PEER.0, "Peer")]),
			channel(ROOM, Some(GUILD), 2, vec![]),
			channel(OTHER_ROOM, Some(GUILD), 2, vec![]),
		],
		status: "",
		..ClientState::default()
	};
	crate::tests::grant_permissions(&mut state);
	state
}
fn settings() -> Settings {
	Settings {
		source: SourceId::Display(1),
		width: 1280,
		height: 720,
		fps: 30,
		cursor: true,
		audio: false,
	}
}

/// A client and a simulated Discord on one virtual clock.
struct World {
	state: ClientState,
	discord: Discord,
	now: Instant,
}
impl World {
	fn new(seed: u64, config: Config) -> Self {
		Self {
			state: fixture(),
			discord: Discord::new(seed, config),
			now: Instant::now(),
		}
	}
	fn quiet() -> Self {
		Self::new(1, Config::default())
	}
	fn run(&mut self, duration: Duration) {
		let end = self.now + duration;
		while self.now < end {
			self.now += Duration::from_millis(10);
			self.step();
		}
	}
	fn step(&mut self) {
		self.discord.step(&mut self.state, self.now);
	}
	fn sent(&self) -> &[Command] {
		&self.discord.log
	}
	fn settle(&mut self) {
		self.run(Duration::from_secs(20));
	}
	fn voice_states(&self) -> usize {
		self.sent()
			.iter()
			.filter(|c| matches!(c, Command::VoiceState { .. }))
			.count()
	}
	fn connected_to(&self, channel: Id) -> bool {
		self.state
			.voice
			.active
			.as_ref()
			.is_some_and(|call| call.channel == channel && call.phase.connected())
			&& self
				.state
				.voice
				.media_plan()
				.is_some_and(|plan| plan.channel == channel)
			&& self
				.discord
				.truth
				.voice
				.is_some_and(|voice| voice.holder == Holder::Ours && voice.channel == channel)
	}
	fn idle(&self) -> bool {
		self.state.voice.active.is_none()
			&& self.state.voice.media_plan().is_none()
			&& self
				.discord
				.truth
				.voice
				.is_none_or(|voice| voice.holder != Holder::Ours)
	}
	fn own_streams(&self) -> usize {
		self.discord
			.truth
			.streams
			.keys()
			.filter(|key| key.ends_with(&format!(":{OWNER}")))
			.count()
	}
}

#[test]
fn deafen_implies_mute_and_undeafen_restores_the_previous_microphone() {
	use super::toggle_self_voice as toggle;
	assert_eq!(toggle(false, false, true), (false, true));
	assert_eq!(toggle(false, true, true), (false, false));
	assert_eq!(toggle(true, false, true), (true, true));
	assert_eq!(toggle(true, true, true), (true, false));
	assert_eq!(toggle(true, true, false), (false, false));
	assert_eq!(toggle(false, true, false), (false, false));
}

#[test]
fn leaving_and_joining_again_at_once_connects_without_an_error() {
	let mut world = World::quiet();
	assert!(world.state.start_call(DM, false));
	world.settle();
	assert!(world.connected_to(DM));
	world.state.leave_call();
	assert!(
		world.state.start_call(DM, false),
		"rejoin is accepted at once"
	);
	world.settle();
	assert!(world.connected_to(DM));
	assert_eq!(world.state.status, "");
	assert_eq!(world.discord.rejected, 0);
	assert_eq!(world.discord.overlapping, 0);
}

#[test]
fn spamming_join_and_leave_ends_in_the_last_choice_with_paced_operations() {
	for last_join in [false, true] {
		let mut world = World::quiet();
		for round in 0..40 {
			assert!(world.state.start_call(DM, false));
			world.run(Duration::from_millis(if round % 3 == 0 { 0 } else { 10 }));
			world.state.leave_call();
			world.run(Duration::from_millis(10));
		}
		if last_join {
			assert!(world.state.start_call(DM, false));
		}
		world.settle();
		assert_eq!(world.connected_to(DM), last_join);
		assert_eq!(world.idle(), !last_join);
		assert_eq!(
			world.discord.overlapping, 0,
			"one voice state in flight at a time"
		);
		assert!(
			world.voice_states() < 40,
			"{} voice states for 40 rounds",
			world.voice_states()
		);
		assert_eq!(world.discord.rejected, 0);
	}
}

#[test]
fn leaving_while_sharing_leaves_no_stream_and_sharing_again_works() {
	let mut world = World::quiet();
	world.state.start_call(DM, false);
	world.settle();
	assert!(world.state.share_screen(settings()));
	world.settle();
	assert_eq!(world.state.voice.share_state(), ShareState::Live);
	assert_eq!(world.own_streams(), 1);

	world.state.leave_call();
	assert_eq!(world.state.voice.share_state(), ShareState::Stopping);
	assert!(
		world.state.voice.share_plan().is_none(),
		"capture stops at once"
	);
	world.settle();
	assert!(world.idle());
	assert_eq!(world.own_streams(), 0);
	assert_eq!(world.state.voice.share_state(), ShareState::Off);

	world.state.start_call(DM, false);
	world.run(Duration::from_millis(400));
	world.settle();
	assert!(world.state.share_screen(settings()));
	world.settle();
	assert_eq!(world.state.voice.share_state(), ShareState::Live);
	assert_eq!(world.own_streams(), 1);
	assert_eq!(world.discord.rejected, 0);
}

#[test]
fn stopping_and_starting_a_share_quickly_keeps_one_stream() {
	let mut world = World::quiet();
	world.state.start_call(ROOM, false);
	world.settle();
	for _ in 0..10 {
		assert!(world.state.share_screen(settings()));
		world.run(Duration::from_millis(30));
		world.state.stop_sharing();
		world.run(Duration::from_millis(20));
	}
	assert!(world.state.share_screen(settings()));
	world.settle();
	assert_eq!(world.state.voice.share_state(), ShareState::Live);
	assert_eq!(world.own_streams(), 1);
	world.state.stop_sharing();
	world.settle();
	assert_eq!(world.state.voice.share_state(), ShareState::Off);
	assert_eq!(world.own_streams(), 0);
}

#[test]
fn switching_channels_while_sharing_ends_the_share_and_connects_the_new_call() {
	let mut world = World::quiet();
	world.state.start_call(ROOM, false);
	world.settle();
	world.state.share_screen(settings());
	world.settle();
	assert!(world.state.start_call(OTHER_ROOM, false));
	assert_eq!(world.state.voice.share_state(), ShareState::Stopping);
	world.settle();
	assert!(world.connected_to(OTHER_ROOM));
	assert_eq!(world.own_streams(), 0);
	assert_eq!(world.state.voice.share_state(), ShareState::Off);
}

#[test]
fn another_device_taking_the_call_ends_it_here_without_a_hangup() {
	let mut world = World::quiet();
	world.state.start_call(DM, false);
	world.settle();
	let before = world.voice_states();
	world
		.discord
		.other_device_joins(&world.state, world.now, DM);
	world.settle();
	assert!(world.state.voice.active.is_none());
	assert_eq!(world.state.status, "Call continued on another device");
	assert_eq!(
		world.voice_states(),
		before,
		"the other device keeps the call"
	);
	assert_eq!(world.discord.truth.voice.unwrap().holder, Holder::Elsewhere);
	assert!(
		world.state.start_call(DM, false),
		"taking it back is a normal join"
	);
	world.settle();
	assert!(world.connected_to(DM));
}

#[test]
fn a_kick_ends_the_call_and_a_server_move_keeps_it() {
	let mut world = World::quiet();
	world.state.start_call(ROOM, false);
	world.settle();
	let first = world.state.voice.media_plan().unwrap().id;
	world.discord.migrate(world.now);
	world.settle();
	assert!(world.connected_to(ROOM));
	assert_ne!(
		world.state.voice.media_plan().unwrap().id,
		first,
		"media reconnected"
	);
	world.discord.kick(&world.state, world.now);
	world.settle();
	assert!(world.idle());
	assert_eq!(world.state.status, "You were disconnected from the call");
}

#[test]
fn a_fresh_session_rejoins_the_wanted_call_and_a_resume_keeps_it() {
	let mut world = World::quiet();
	world.state.start_call(DM, false);
	world.settle();
	world.discord.disconnect(&mut world.state, world.now);
	world.run(Duration::from_secs(2));
	world.discord.resume(&mut world.state, world.now);
	world.settle();
	assert!(world.connected_to(DM));
	world.discord.new_session(&mut world.state, world.now);
	world.settle();
	assert!(
		world.connected_to(DM),
		"the call comes back after a fresh READY"
	);
	assert_eq!(world.discord.overlapping, 0);
}

#[test]
fn media_failures_retry_with_fresh_credentials_then_end_the_call() {
	let mut world = World::quiet();
	world.state.start_call(DM, false);
	world.settle();
	world.discord.media_drop(world.now);
	world.settle();
	assert!(world.connected_to(DM), "one drop reconnects");
	world.discord.config.media_failures = 100;
	world.discord.media_drop(world.now);
	world.run(Duration::from_secs(30));
	assert!(world.idle(), "repeated failures end the call");
	assert_eq!(world.state.status, "Voice connection failed");
}

#[test]
fn stale_media_reports_never_touch_the_current_run() {
	let mut world = World::quiet();
	world.state.start_call(DM, false);
	world.settle();
	let current = world.state.voice.media_plan().unwrap().id;
	world.state.apply_voice_at(
		Event::Media {
			id: current - 1,
			report: Report::Failed("old run"),
		},
		world.now,
	);
	assert_eq!(world.state.voice.media_plan().unwrap().id, current);
	assert!(world.state.voice.active.as_ref().unwrap().phase.connected());
}

#[test]
fn new_private_calls_ring_once_and_hanging_up_stops_the_ringing() {
	let mut world = World::quiet();
	world.state.start_call(DM, true);
	world.settle();
	let rings = |world: &World, stop: bool| {
		world
			.sent()
			.iter()
			.filter(|c| matches!(c, Command::Ring { stop: s, .. } if *s == stop))
			.count()
	};
	assert_eq!(rings(&world, false), 1);
	world.state.apply_voice_at(
		Event::Call {
			channel: DM,
			ringing: Some(vec![PEER]),
			participants: None,
			unavailable: false,
		},
		world.now,
	);
	assert_eq!(world.state.outgoing_ring(), Some(DM));
	world.state.leave_call();
	world.step();
	assert_eq!(rings(&world, true), 1);

	// Joining a call someone is already in rings nobody.
	let mut world = World::quiet();
	world
		.discord
		.other_state(&world.state, world.now, PEER, Some(DM), false);
	world.state.apply_voice_at(
		Event::Call {
			channel: DM,
			ringing: Some(vec![]),
			participants: Some(vec![]),
			unavailable: false,
		},
		world.now,
	);
	world.settle();
	world.state.start_call(DM, true);
	world.settle();
	assert_eq!(rings(&world, false), 0);
}

#[test]
fn watching_follows_the_streamer_and_ends_when_they_stop() {
	let mut world = World::quiet();
	world.state.start_call(ROOM, false);
	world.settle();
	world
		.discord
		.other_state(&world.state, world.now, PEER, Some(ROOM), true);
	world.settle();
	assert!(world.state.watch_stream(PEER));
	world.settle();
	assert!(world.state.voice.watch_live());
	world
		.discord
		.other_state(&world.state, world.now, PEER, Some(ROOM), false);
	world.settle();
	assert!(!world.state.voice.watch_live());
	assert!(
		world
			.state
			.voice
			.active
			.as_ref()
			.unwrap()
			.watching
			.is_none()
	);
	assert!(world.state.voice.watch_plan().is_none());
}

#[test]
fn camera_and_mute_reach_discord_after_any_burst() {
	let mut world = World::quiet();
	world.state.start_call(ROOM, false);
	world.settle();
	for index in 0..21 {
		world.state.set_call_mute(index % 2 == 0, false);
		world.state.set_call_camera(index % 3 == 0);
		world.run(Duration::from_millis(5));
	}
	world.settle();
	let call = world.state.voice.active.as_ref().unwrap();
	let voice = world.discord.truth.voice.unwrap();
	assert_eq!((voice.mute, voice.video), (call.self_muted(), call.camera));
	assert_eq!(world.discord.overlapping, 0);
}

/// One random session of user actions and Discord behaviour; panics on a broken invariant.
fn chaos_run(seed: u64, chaos: bool) {
	run_session(seed, chaos, (5, 250));
}
fn run_session(seed: u64, chaos: bool, latency: (u64, u64)) {
	let slow = latency.1 > 1000;
	let config = Config {
		// Every other session never sees its own Gateway session in voice states. That
		// fallback is best effort: with multi-second latency, another device's late states
		// can still be taken for ours, so the slow run keeps the matching world.
		// ponytail: order-based ownership; live tests show whether Discord ever needs it.
		sessions_match: slow || seed.is_multiple_of(2),
		latency,
		media_failures: if chaos { 5 } else { 0 },
		ring_failures: 10,
		silent: 50,
		answer: seed.is_multiple_of(3).then_some(Duration::from_millis(700)),
	};
	let mut world = World::new(seed, config);
	let mut rng = Rng::new(seed ^ 0xA5A5);
	let channels = [DM, ROOM, OTHER_ROOM];
	let mut offline_until: Option<Instant> = None;
	for _ in 0..250 {
		world.run(if rng.chance(30) {
			Duration::ZERO
		} else {
			rng.millis(0, 400)
		});
		if offline_until.is_some_and(|at| world.now >= at) {
			offline_until = None;
			world.discord.resume(&mut world.state, world.now);
		}
		let channel = channels[rng.below(3) as usize];
		let roll = rng.below(100);
		world.discord.trace(
			world.now,
			&format_args!(
				"ACTION {roll} {channel:?} active={:?}",
				world
					.state
					.voice
					.active
					.as_ref()
					.map(|c| (c.channel, c.phase))
			),
		);
		let state = &mut world.state;
		match roll {
			0..=24 => {
				let expected = state.can_call(channel);
				assert_eq!(
					state.start_call(channel, rng.chance(50)),
					expected,
					"join refused"
				);
			}
			25..=39 => {
				state.leave_call();
			}
			40..=49 => {
				if let Some(call) = &state.voice.active {
					let (muted, deafened) =
						toggle_self_voice(call.muted, call.deafened, rng.chance(30));
					state.set_call_mute(muted, deafened);
				}
			}
			50..=56 => {
				let on = rng.chance(60);
				state.set_call_camera(on);
			}
			57..=66 => {
				let expected = state
					.voice
					.active
					.as_ref()
					.is_some_and(|call| state.can_stream(call.channel))
					&& state.voice.media_ready();
				assert_eq!(state.share_screen(settings()), expected, "share refused");
			}
			67..=72 => state.stop_sharing(),
			73..=78 => {
				let room = channels[1 + rng.below(2) as usize];
				let target = rng
					.chance(70)
					.then_some(if rng.chance(50) { room } else { DM });
				let streaming = rng.chance(50);
				world
					.discord
					.other_state(&world.state, world.now, PEER, target, streaming);
			}
			79..=83 => {
				if rng.chance(70) {
					state.watch_stream(PEER);
				} else {
					state.stop_watching();
				}
			}
			84..=85 if chaos => {
				world
					.discord
					.other_device_joins(&world.state, world.now, channel);
			}
			86..=87 if chaos => world.discord.kick(&world.state, world.now),
			88..=89 if chaos => world.discord.migrate(world.now),
			90..=91 if chaos => world.discord.media_drop(world.now),
			92..=93 if chaos && offline_until.is_none() => {
				world.discord.disconnect(&mut world.state, world.now);
				offline_until = Some(world.now + rng.millis(100, 3000));
			}
			94 if chaos && offline_until.is_none() => {
				world.discord.new_session(&mut world.state, world.now);
			}
			_ => {}
		}
		world.step();
		check(&world, chaos || slow, seed);
	}
	if offline_until.is_some() {
		world.discord.resume(&mut world.state, world.now);
	}
	world.discord.config.media_failures = 0;
	world.run(Duration::from_secs(if slow { 90 } else { 40 }));
	converged(&world, seed);
}

/// Invariants that hold after every step.
fn check(world: &World, relaxed: bool, seed: u64) {
	let voice = &world.state.voice;
	assert_eq!(
		world.discord.overlapping, 0,
		"seed {seed}: voice states overlapped"
	);
	if !relaxed {
		assert_eq!(
			world.discord.rejected, 0,
			"seed {seed}: media started with stale credentials"
		);
	}
	if let Some(plan) = voice.media_plan() {
		assert_eq!(
			voice.active.as_ref().map(|call| call.channel),
			Some(plan.channel),
			"media for a call nobody wants"
		);
	}
	if voice.share_plan().is_some() || voice.watch_plan().is_some() {
		assert!(
			voice.media_ready(),
			"stream media without connected call media"
		);
	}
	assert!(voice.phone.outbox.is_empty(), "commands left unsent");
}

/// Once nobody acts, everything agrees with the last choice.
fn converged(world: &World, seed: u64) {
	let voice = &world.state.voice;
	let truth = &world.discord.truth;
	match &voice.active {
		Some(call) => {
			assert!(
				world.connected_to(call.channel),
				"seed {seed}: not connected to the wanted call"
			);
			let own = truth.voice.unwrap();
			assert_eq!(
				(own.mute, own.deaf, own.video),
				(call.self_muted(), call.deafened, call.camera),
				"seed {seed}: voice flags differ"
			);
			let shares = voice.phone.share.want.is_some();
			assert_eq!(
				world.own_streams(),
				usize::from(shares),
				"seed {seed}: own streams"
			);
			assert_eq!(
				voice.share_state(),
				if shares {
					ShareState::Live
				} else {
					ShareState::Off
				},
				"seed {seed}: share state"
			);
			if let Some(streamer) = call.watching {
				assert!(
					voice.watch_live(),
					"seed {seed}: watch of {streamer:?} not live"
				);
			}
		}
		None => {
			assert!(world.idle(), "seed {seed}: still in a call on Discord");
			assert_eq!(world.own_streams(), 0, "seed {seed}: stream left behind");
			assert_eq!(
				voice.share_state(),
				ShareState::Off,
				"seed {seed}: share state"
			);
		}
	}
	assert!(
		voice.phone.wire.pending.is_none(),
		"seed {seed}: operation pending"
	);
}

/// `SIM_SEED=n` replays one session; `SIM_RUNS=n` runs more than the default.
fn seeds() -> std::ops::Range<u64> {
	let var = |name| {
		std::env::var(name)
			.ok()
			.and_then(|value| value.parse().ok())
	};
	match (var("SIM_SEED"), var("SIM_RUNS")) {
		(Some(seed), _) => seed..seed + 1,
		(None, runs) => 0..runs.unwrap_or(1500),
	}
}

#[test]
fn simulated_discord_survives_random_spam() {
	for seed in seeds() {
		chaos_run(seed, false);
	}
}

#[test]
fn simulated_discord_survives_random_spam_with_chaos() {
	for seed in seeds() {
		chaos_run(seed, true);
	}
}

#[test]
fn simulated_discord_survives_a_slow_gateway() {
	for seed in seeds().take(400) {
		run_session(seed, true, (100, 4500));
	}
}
