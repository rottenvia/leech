//! Offline group-call check against the simulated Discord:
//! cargo run --locked -p leech --features demo --example group_call
use client_core::{
	Command, Envelope, Event, State,
	voice::{
		self,
		sim::{Config, Discord},
	},
};
use eframe::egui;
use model::Id;
use std::time::Instant;

fn participant(user: Id) -> voice::Participant {
	voice::Participant {
		user,
		muted: false,
		deafened: false,
		server_muted: false,
		server_deafened: false,
		video: false,
		streaming: false,
	}
}

fn render(state: &mut State, width: f32, required: &[&str]) {
	let ctx = egui::Context::default();
	ctx.enable_accesskit();
	ui::design::apply(&ctx);
	let mut view = ui::MessagingUi::default();
	view.voice_available = true;
	for _ in 0..3 {
		let output = ctx.run_ui(
			egui::RawInput {
				screen_rect: Some(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(width, 760.0),
				)),
				..Default::default()
			},
			|ui| {
				// Calls go through the controller; the view never queues voice commands itself.
				assert!(
					!view
						.show(ui, state)
						.iter()
						.any(|c| matches!(c, Command::Voice(_)))
				);
			},
		);
		let nodes = &output
			.platform_output
			.accesskit_update
			.as_ref()
			.unwrap()
			.nodes;
		for label in required {
			assert!(
				nodes.iter().any(|(_, node)| node.label() == Some(label)),
				"Missing {label} at width {width}"
			);
		}
		output.drop_without_applying_deltas();
	}
}

fn rang(discord: &Discord, recipient: Option<Id>, stop: bool) -> bool {
	discord.log.iter().any(|command| {
		*command
			== voice::Command::Ring {
				channel: Id(29),
				recipient,
				stop,
			}
	})
}

fn main() {
	let mut state = test_support::demo_state();
	let channel = Id(29);
	let own = state.user.as_ref().unwrap().id;
	state.select(channel);
	assert!(
		!state.can_call(channel),
		"the plain offline preview never calls"
	);
	state.voice.simulated = true;
	assert!(state.can_call(channel) && state.can_camera(channel) && state.can_stream(channel));
	let mut discord = Discord::new(29, Config::default());
	let mut now = Instant::now();
	for width in [1400.0, 800.0] {
		render(&mut state, width, &["Start voice call"]);
	}
	assert!(state.start_call(channel, true));
	now = discord.settle(&mut state, now);
	assert!(state.voice.media_ready() && rang(&discord, None, false));
	assert!(state.leave_call());
	now = discord.settle(&mut state, now);

	let mut peers: Vec<_> = state
		.channel(channel)
		.unwrap()
		.recipients
		.iter()
		.map(|u| participant(u.id))
		.collect();
	peers.push(participant(own));
	let call = |participants| voice::Event::Call {
		channel,
		ringing: Some(vec![own]),
		participants: Some(participants),
		unavailable: false,
	};
	state.apply_voice(call(peers.clone()));
	assert!(state.voice.has_dm_call(channel) && state.voice.active.is_none());
	assert_eq!(state.voice.incoming, Some(channel));
	render(&mut state, 800.0, &["Answer", "Decline"]);
	assert!(state.decline_call());
	now = discord.settle(&mut state, now);
	assert!(
		rang(&discord, Some(own), true),
		"declining stops ringing this account"
	);
	assert!(state.start_call(channel, true));
	assert_eq!(state.voice.active.as_ref().unwrap().participants, peers);
	now = discord.settle(&mut state, now);
	assert!(state.voice.media_ready());
	assert!(state.set_call_mute(true, true));
	assert!(state.set_call_camera(true));
	assert!(state.set_call_camera(false));
	assert!(state.set_call_mute(false, false));
	now = discord.settle(&mut state, now);

	let mut newcomer = state.channel(channel).unwrap().recipients[0].clone();
	newcomer.id = Id(900);
	newcomer.name = "New group participant (synthetic)".into();
	state.apply(Envelope {
		generation: state.generation,
		event: Event::RecipientAdded {
			channel,
			user: newcomer,
		},
	});
	discord.other_state(&state, now, Id(900), Some(channel), true);
	now = discord.settle(&mut state, now);
	assert!(state.watch_stream(Id(900)));
	now = discord.settle(&mut state, now);
	assert!(state.voice.watch_live());
	let mut invalid = state.voice.active.as_ref().unwrap().participants.clone();
	invalid.push(participant(Id(9999)));
	let before = state.voice.active.as_ref().unwrap().participants.clone();
	state.apply_voice(call(invalid));
	assert_eq!(state.voice.active.as_ref().unwrap().participants, before);
	state.apply(Envelope {
		generation: state.generation,
		event: Event::RecipientRemoved {
			channel,
			user: Id(900),
		},
	});
	assert!(state.voice.active.as_ref().unwrap().watching.is_none());
	now = discord.settle(&mut state, now);
	assert!(!state.voice.watch_live());

	// A ten-person group uses the shared bounded grid and all ordinary call controls.
	let group = state.channels.iter_mut().find(|c| c.id == channel).unwrap();
	let template = group.recipients[0].clone();
	for id in 100..107 {
		let mut user = template.clone();
		user.id = Id(id);
		user.name = format!("Synthetic member {id}");
		group.recipients.push(user);
	}
	peers = group.recipients.iter().map(|u| participant(u.id)).collect();
	peers.push(participant(own));
	assert_eq!(peers.len(), 10);
	state.apply_voice(call(peers));
	for width in [1400.0, 800.0] {
		render(
			&mut state,
			width,
			&[
				"Mute",
				"Deafen",
				"Turn on camera",
				"Share your screen",
				"Disconnect",
			],
		);
	}
	assert!(state.leave_call());
	now = discord.settle(&mut state, now);
	assert!(state.voice.has_dm_call(channel));
	render(&mut state, 800.0, &["Join call"]);
	assert!(state.start_call(channel, false));
	// The other nine show at once; this account appears once Discord confirms the join.
	assert_eq!(state.voice.active.as_ref().unwrap().participants.len(), 9);
	now = discord.settle(&mut state, now);
	assert_eq!(state.voice.active.as_ref().unwrap().participants.len(), 10);
	state.apply(Envelope {
		generation: state.generation,
		event: Event::RecipientRemoved { channel, user: own },
	});
	discord.settle(&mut state, now);
	assert!(
		!state.can_call(channel)
			&& state.voice.active.is_none()
			&& !state.voice.has_dm_call(channel)
	);
	println!(
		"PASS: simulated group start/ring, answer/decline/join, membership, media controls, watching, removal and wide/narrow UI. No Discord or devices accessed."
	);
}
