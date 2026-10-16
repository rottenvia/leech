//! Desktop media for calls: audio, the voice transport, camera, screen share and watched
//! streams, each started and stopped to match the call controller's plans by id. Nothing here
//! decides whether a call exists; reports flow back to the controller.
use client_core::{
	State,
	voice::{self, Phase, Report},
};
use discord_voice::{
	Controls, Status,
	audio::{Audio, Devices},
};
use eframe::egui;
use model::{Id, notification_preferences::Sound};
use std::{
	sync::{Arc, OnceLock, mpsc},
	time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::watch, task::JoinHandle};

/// Audio pipelines allowed to finish closing in the background at once.
const MAX_RETIRING: usize = 4;

fn permission_mutes_microphone(
	state: &State,
	channel: Id,
	push_to_talk: bool,
	ptt_active: bool,
) -> bool {
	!state.can_speak(channel)
		|| (state.permission(channel, model::permissions::USE_VAD) != Some(true)
			&& !(push_to_talk && ptt_active))
}

const DEVICE_OPEN_TIMEOUT: Duration = Duration::from_secs(20);

fn device_wait(
	deadline: &mut Option<Instant>,
	pending: bool,
	now: Instant,
) -> Result<Option<Duration>, &'static str> {
	if !pending {
		*deadline = None;
		return Ok(None);
	}
	deadline
		.get_or_insert(now + DEVICE_OPEN_TIMEOUT)
		.checked_duration_since(now)
		.filter(|remaining| !remaining.is_zero())
		.map(Some)
		.ok_or(
			"Audio device opening timed out; check device selection and system microphone permission",
		)
}

enum Notice {
	Camera(bool),
	Connected {
		others: bool,
		privacy_code: Option<String>,
	},
	Progress(Phase),
	DeviceReady,
	RemoteAudio,
}

#[derive(Default)]
struct CallCues {
	request: Option<u64>,
	joined: bool,
	peers: Option<[u64; voice::MAX_PARTICIPANTS]>,
}
impl CallCues {
	fn poll(
		&mut self,
		ready: bool,
		gateway_connected: bool,
		owner: Id,
		participants: &[voice::Participant],
	) -> Option<Sound> {
		let joined = !self.joined && ready;
		self.joined |= ready;
		if !self.joined {
			return None;
		}
		let cue = joined.then_some(Sound::UserJoin);
		if !gateway_connected {
			self.peers = None;
			return cue;
		}
		let mut peers = [0; voice::MAX_PARTICIPANTS];
		for (slot, participant) in peers.iter_mut().zip(
			participants
				.iter()
				.filter(|participant| participant.user != owner),
		) {
			*slot = participant.user.0;
		}
		// ponytail: membership scans are capped at 64 IDs; no per-frame set allocation.
		let membership_cue = self.peers.replace(peers).and_then(|previous| {
			if previous
				.iter()
				.any(|user| *user != 0 && !peers.contains(user))
			{
				Some(Sound::UserLeave)
			} else {
				peers
					.iter()
					.any(|user| *user != 0 && !previous.contains(user))
					.then_some(Sound::UserJoin)
			}
		});
		cue.or(membership_cue)
	}
}
/// Remote cameras kept as textures at once; matches the transport's source limit.
const MAX_REMOTE_VIDEO: usize = 16;
/// Latest native picture per remote user plus a dirty flag; the UI uploads dirty ones.
type RemotePictures = Vec<(u64, platform::video::Picture, bool)>;
type CameraPicture = Option<(Arc<egui::ColorImage>, bool)>;

/// Replace one user's latest picture in place: a plain copy of the decoder's planes, reusing
/// the previous allocation. Colour conversion happens on the GPU at upload.
fn store_remote_frame(
	pictures: &mut RemotePictures,
	frame: discord_voice::RemoteFrame<'_>,
) -> bool {
	if !frame.picture.is_valid() {
		return false;
	}
	let entry = if let Some(index) = pictures.iter().position(|(user, _, _)| *user == frame.user) {
		&mut pictures[index]
	} else if pictures.len() < MAX_REMOTE_VIDEO {
		pictures.push((frame.user, frame.picture.with_data(Vec::new()), false));
		pictures.last_mut().expect("remote frame inserted")
	} else {
		return false;
	};
	let mut data = std::mem::take(&mut entry.1.data);
	data.clear();
	data.extend_from_slice(frame.picture.data);
	entry.1 = frame.picture.with_data(data);
	entry.2 = true;
	true
}

#[allow(clippy::chunks_exact_to_as_chunks)] // Matches egui's allocation fallback loop.
fn store_camera_frame(picture: &mut CameraPicture, rgb: &[u8]) -> bool {
	let size = [discord_voice::camera::WIDTH, discord_voice::camera::HEIGHT];
	if rgb.len() != size[0] * size[1] * 3 {
		return false;
	}
	let (image, dirty) =
		picture.get_or_insert_with(|| (Arc::new(egui::ColorImage::default()), false));
	if let Some(image) = Arc::get_mut(image) {
		image.size = size;
		image.source_size = egui::vec2(size[0] as f32, size[1] as f32);
		image.pixels.clear();
		image.pixels.extend(
			rgb.chunks_exact(3)
				.map(|pixel| egui::Color32::from_rgb(pixel[0], pixel[1], pixel[2])),
		);
	} else {
		*image = Arc::new(egui::ColorImage::from_rgb(size, rgb));
	}
	*dirty = true;
	true
}

struct MicPreview {
	audio: Audio,
	devices: Devices,
	failure: Arc<OnceLock<&'static str>>,
	started: Instant,
}

struct CameraTest {
	camera: discord_voice::camera::Camera,
	device: Option<String>,
	picture: Arc<std::sync::Mutex<CameraPicture>>,
}

/// The call's running media: one audio pipeline and one transport for one plan id.
struct CallMedia {
	id: u64,
	identity: Arc<discord_voice::Identity>,
	audio: Audio,
	controls: watch::Sender<Controls>,
	events: mpsc::Receiver<Notice>,
	// Terminal errors must survive a full progress queue; retain the first safe reason.
	failure: Arc<OnceLock<&'static str>>,
	speakers: watch::Receiver<[u64; 64]>,
	task: JoinHandle<()>,
	devices: Devices,
	device_deadline: Option<Instant>,
	camera_frames: mpsc::SyncSender<discord_voice::camera_video::Frame>,
	camera_clock: Instant,
	/// Latest decoded camera picture per remote user, replaced (never queued) by the decoder.
	remote_video: Arc<std::sync::Mutex<RemotePictures>>,
	/// Decoded audio of a watched stream, mixed into this call's playback.
	stream_audio: mpsc::SyncSender<discord_voice::Frame>,
	/// The transport is connected: `Some(true)` with others, `Some(false)` alone.
	connected: Option<bool>,
	privacy_code: Option<String>,
	/// Last report given to the controller, so each change is reported once.
	reported: Option<Report>,
}

#[derive(Default)]
pub struct Voice {
	camera_test: Option<CameraTest>,
	mic_preview: Option<MicPreview>,
	screen: crate::screen::Screen,
	watch: crate::watch::Watch,
	call: Option<CallMedia>,
	/// Audio pipelines still closing. New calls never wait for them.
	retiring: Vec<mpsc::Receiver<()>>,
	camera: Option<discord_voice::camera::Camera>,
	camera_device: Option<String>,
	camera_generation: u64,
	camera_preview: Option<std::sync::Arc<std::sync::Mutex<CameraPicture>>>,
	cues: CallCues,
	/// Whether Discord's voice states name this Gateway session, reported once per run.
	sessions_reported: bool,
	device_scan: Option<mpsc::Receiver<Result<discord_voice::audio::DeviceList, &'static str>>>,
	camera_scan: Option<mpsc::Receiver<Result<discord_voice::camera::DeviceList, &'static str>>>,
	/// Remote camera textures behind `ui.voice_remote_video`, by user.
	remote_textures: Vec<(Id, crate::gpu_video::VideoTexture)>,
}
impl Voice {
	/// Sign-out or a new login: stop every worker now.
	pub fn stop(&mut self) {
		self.camera_test = None;
		self.mic_preview = None;
		// Logout stops polling; the next poll (or `MessagingUi::clear`) drops the borrowed
		// ids before a frame paints, and disposal waits for the current frame.
		self.remote_textures.clear();
		self.screen.stop();
		self.watch.stop();
		self.stop_camera();
		self.stop_call();
		self.cues = CallCues::default();
	}
	fn stop_call(&mut self) {
		if let Some(call) = self.call.take() {
			call.audio.set_ready(false);
			call.task.abort();
			if self.retiring.len() < MAX_RETIRING {
				self.retiring.push(call.audio.shutdown());
			}
		}
	}
	pub fn stop_camera(&mut self) {
		if let Some(camera) = &self.camera {
			camera.stop();
		}
		self.camera_preview = None;
		if let Some(call) = &self.call {
			call.controls.send_if_modified(|controls| {
				let changed = controls.camera != 0;
				controls.camera = 0;
				changed
			});
		}
	}
	fn reap(&mut self) {
		if self.camera_preview.is_none()
			&& self.camera.as_ref().is_some_and(|camera| camera.stopped())
		{
			self.camera = None;
		}
		self.retiring
			.retain(|done| matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
	}

	/// Keeps every worker in line with the call controller's plans; reports go back to it.
	pub fn poll(
		&mut self,
		runtime: &Runtime,
		state: &mut State,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
	) {
		self.reap();
		self.poll_mic_preview(state, ui, ctx);
		self.poll_camera_test(state, ui, ctx);
		ui.voice_speaking.clear();
		ui.voice_microphone_unavailable = false;
		self.poll_camera_devices(state.demo, ui, ctx);
		self.poll_audio_devices(state.demo, ui, ctx);
		self.poll_cues(state, ui, ctx);
		if !self.sessions_reported
			&& state.voice.media_plan().is_some()
			&& std::env::var_os("LEECH_VOICE_DIAGNOSTICS").is_some_and(|value| value == "1")
		{
			self.sessions_reported = true;
			eprintln!(
				"[Leech voice Call] voice_session_is_gateway_session={}",
				state.voice.sessions_verified()
			);
		}
		if state.voice.simulated {
			// The simulated Discord runs pretend media itself; nothing real opens.
			ui.voice_camera_available = state.voice.camera_negotiated();
			self.screen.poll_simulated(state, ui);
			self.watch.poll_simulated(state, ui);
			return;
		}
		let plan = state.voice.media_plan();
		if self.call.as_ref().map(|call| call.id) != plan.map(|plan| plan.id) {
			self.stop_camera();
			ui.voice_camera_preview = None;
			ui.voice_remote_video.clear();
			self.remote_textures.clear();
			ui.voice_privacy_code = None;
			self.stop_call();
			if let Some(plan) = plan
				&& let Err(error) = self.start_media(runtime, state, plan.id, ui, ctx)
			{
				state.apply_voice(voice::Event::Media {
					id: plan.id,
					report: Report::Failed(error),
				});
			}
		}
		if state.voice.active.is_none() {
			ui.voice_camera_available = false;
			ui.voice_camera_preview = None;
			ui.voice_camera_status = "";
		}
		if self.call.is_some() {
			self.poll_call(state, ui, ctx);
			self.poll_camera(state, ui, ctx);
		}
		let identity = self.call.as_ref().map(|call| call.identity.clone());
		let stream_audio = self.call.as_ref().map(|call| call.stream_audio.clone());
		self.screen.poll(runtime, state, ui, ctx, identity.clone());
		self.watch
			.poll(runtime, state, ui, ctx, identity, stream_audio);
	}

	fn poll_audio_devices(&mut self, demo: bool, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		if ui.voice_refresh_devices {
			ui.voice_refresh_devices = false;
			if !demo && self.device_scan.is_none() {
				let (send, receive) = mpsc::sync_channel(1);
				let wake = ctx.clone();
				match std::thread::Builder::new()
					.name("audio-devices".into())
					.spawn(move || {
						let _ = send.send(discord_voice::audio::devices());
						wake.request_repaint();
					}) {
					Ok(_) => {
						self.device_scan = Some(receive);
						ui.voice_device_status = "Looking for audio devices…";
					}
					Err(_) => ui.voice_device_status = "Could not start audio device discovery",
				}
			}
		}
		if let Some(scan) = &self.device_scan {
			match scan.try_recv() {
				Ok(Ok(devices)) => {
					ui.voice_inputs = devices.inputs;
					ui.voice_outputs = devices.outputs;
					ui.voice_device_status =
						"Audio devices loaded · headphones avoid microphone echo";
					self.device_scan = None;
				}
				Ok(Err(error)) => {
					ui.voice_device_status = error;
					self.device_scan = None;
				}
				Err(mpsc::TryRecvError::Disconnected) => {
					ui.voice_device_status = "Audio device discovery stopped";
					self.device_scan = None;
				}
				Err(mpsc::TryRecvError::Empty) => {}
			}
		}
	}

	/// Join and leave sounds for the wanted call, whichever media runs it. This device
	/// leaving is silent by design: a leave cue of our own only doubled the sound the owner
	/// already hears when they hang up.
	fn poll_cues(&mut self, state: &State, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		let Some(call) = state.voice.active.as_ref() else {
			self.cues = CallCues::default();
			return;
		};
		if self.cues.request != Some(call.request) {
			self.cues = CallCues {
				request: Some(call.request),
				..CallCues::default()
			};
			// A soft tick acknowledges the click; the join sound follows once connected.
			if !call.phase.connected() && ui.notification_options.allows(Sound::Joining) {
				ui.notification_preview = Some(Sound::Joining);
				ctx.request_repaint();
			}
		}
		let Some(owner) = state.user.as_ref().map(|user| user.id) else {
			return;
		};
		if let Some(cue) = self.cues.poll(
			call.phase.connected(),
			state.gateway_connected,
			owner,
			&call.participants,
		) && ui.notification_options.allows(cue)
		{
			ui.notification_preview = Some(cue);
			ctx.request_repaint();
		}
	}

	/// Controls in, transport and device progress out.
	fn poll_call(&mut self, state: &mut State, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		let Some(media) = self.call.as_mut() else {
			return;
		};
		let Some(call) = state.voice.active.as_ref() else {
			return;
		};
		let deafened = call.deafened || call.server_deafened;
		let muted = call.muted
			|| permission_mutes_microphone(
				state,
				call.channel,
				ui.voice_push_to_talk,
				ui.voice_ptt_active,
			) || call.server_muted
			|| deafened
			|| (ui.voice_push_to_talk && !ui.voice_ptt_active)
			|| ui.voice_ptm_active;
		media.audio.set_controls(muted, deafened);
		media.audio.set_processing(ui.voice_processing.effective());
		media.audio.set_input_enabled(state.can_speak(call.channel));
		media
			.audio
			.set_gain(ui.voice_gain.input_percent, ui.voice_gain.output_percent);
		let user_volumes = ui.voice_user_volumes();
		let stream_volume = ui.voice_stream_volume();
		let activity_threshold_db = ui
			.voice_processing
			.effective()
			.sensitivity_db
			.unwrap_or(-70);
		media.controls.send_if_modified(|control| {
			let next = (
				muted,
				deafened,
				user_volumes,
				stream_volume,
				activity_threshold_db,
			);
			let current = (
				control.muted,
				control.deafened,
				control.user_volumes,
				control.stream_volume,
				control.activity_threshold_db,
			);
			if next == current {
				return false;
			}
			control.muted = muted;
			control.deafened = deafened;
			control.user_volumes = user_volumes;
			control.stream_volume = stream_volume;
			control.activity_threshold_db = activity_threshold_db;
			true
		});
		if ui.voice_input != media.devices.input || ui.voice_output != media.devices.output {
			let devices = Devices {
				input: ui.voice_input.clone(),
				output: ui.voice_output.clone(),
			};
			media.audio.set_devices(devices.clone());
			media.devices = devices;
			media.device_deadline = None;
		}
		let mut progress = None;
		let mut camera = None;
		for _ in 0..16 {
			let Ok(notice) = media.events.try_recv() else {
				break;
			};
			match notice {
				Notice::Camera(available) => camera = Some(available),
				Notice::Connected {
					others,
					privacy_code,
				} => {
					media.connected = Some(others);
					if privacy_code.is_some() {
						media.privacy_code = privacy_code;
					}
					media.audio.set_ready(true);
				}
				Notice::Progress(phase) => {
					media.connected = None;
					media.privacy_code = None;
					media.audio.set_ready(false);
					media.device_deadline = None;
					progress = Some(phase);
				}
				// Notices wake the UI; only the current device configuration can be ready.
				Notice::DeviceReady | Notice::RemoteAudio => {}
			}
		}
		ui.voice_privacy_code = media.privacy_code.clone();
		ui.voice_microphone_unavailable = media.audio.microphone_unavailable();
		if ui.voice_settings_open() {
			ui.voice_preview_level = Some(media.audio.preview_level_db());
			ctx.request_repaint_after(Duration::from_millis(50));
		}
		let devices_ready = media.audio.is_ready();
		let mut failure = media.failure.get().copied();
		let report = match media.connected {
			Some(_) if !devices_ready => {
				match device_wait(&mut media.device_deadline, true, Instant::now()) {
					Ok(remaining) => {
						if let Some(remaining) = remaining {
							ctx.request_repaint_after(remaining);
						}
						Report::Progress(Phase::OpeningAudio)
					}
					Err(error) => {
						failure = failure.or(Some(error));
						Report::Progress(Phase::OpeningAudio)
					}
				}
			}
			Some(others) => {
				media.device_deadline = None;
				if others {
					Report::Ready
				} else {
					Report::Waiting
				}
			}
			None => Report::Progress(progress.unwrap_or(match media.reported {
				Some(Report::Progress(phase)) => phase,
				_ => Phase::ConnectingTransport,
			})),
		};
		if media.audio.is_stopped() && failure.is_none() {
			failure =
				Some("Audio devices stopped; check microphone permission and device selection");
		}
		if media.task.is_finished() && failure.is_none() {
			// A worker can finish between draining notices and checking its lifecycle.
			failure = media
				.failure
				.get()
				.copied()
				.or(Some("Voice connection ended"));
		}
		let id = media.id;
		let mut reports = Vec::new();
		if let Some(available) = camera {
			reports.push(Report::CameraAvailable(available));
		}
		let report = failure.map_or(report, Report::Failed);
		if media.reported != Some(report) {
			media.reported = Some(report);
			reports.push(report);
		}
		if !matches!(report, Report::Failed(_))
			&& let Some(call) = &state.voice.active
			&& call.phase.connected()
			&& !call.deafened
			&& !call.server_deafened
		{
			let controls = *media.controls.borrow();
			let own = state.user.as_ref().map(|user| user.id.0);
			ui.voice_speaking.extend(
				media
					.speakers
					.borrow()
					.iter()
					.copied()
					.filter(|user| *user != 0 && !(controls.muted && own == Some(*user)))
					.map(Id),
			);
		}
		self.poll_remote_video(state, ui, ctx);
		for report in reports {
			state.apply_voice(voice::Event::Media { id, report });
		}
	}

	fn poll_remote_video(&mut self, state: &State, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		let Some(media) = &self.call else {
			return;
		};
		// Uploading is a plane copy into GPU staging, so it runs under the slot lock
		// instead of cloning each picture out first.
		if let Ok(mut slot) = media.remote_video.try_lock() {
			for (user, picture, dirty) in slot.iter_mut() {
				if !std::mem::take(dirty) {
					continue;
				}
				let user = Id(*user);
				let index = match self.remote_textures.iter().position(|(id, _)| *id == user) {
					Some(index) => index,
					None if self.remote_textures.len() < MAX_REMOTE_VIDEO => {
						self.remote_textures.push((user, Default::default()));
						self.remote_textures.len() - 1
					}
					None => continue,
				};
				let name = format!("remote-camera-{}", user.0);
				let Some(texture) =
					self.remote_textures[index]
						.1
						.upload(ctx, &name, picture.borrow())
				else {
					continue;
				};
				match ui.voice_remote_video.iter_mut().find(|(id, _)| *id == user) {
					Some((_, shown)) => *shown = texture,
					None => ui.voice_remote_video.push((user, texture)),
				}
			}
		}
		// Cameras announced off, or participants who left, release their textures.
		let visible: Vec<Id> = state
			.voice
			.active
			.as_ref()
			.map(|call| {
				call.participants
					.iter()
					.filter(|participant| participant.video)
					.map(|participant| participant.user)
					.collect()
			})
			.unwrap_or_default();
		ui.voice_remote_video.retain(|(id, _)| visible.contains(id));
		self.remote_textures.retain(|(id, _)| visible.contains(id));
		if let Ok(mut pictures) = media.remote_video.try_lock() {
			pictures.retain(|(id, _, _)| visible.contains(&Id(*id)));
		}
	}
	fn poll_mic_preview(&mut self, state: &State, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		if state.demo
			|| !ui.voice_available
			|| !ui.voice_settings_open()
			|| state.voice.active.is_some()
			|| self.call.is_some()
		{
			ui.voice_preview_requested = false;
			ui.voice_preview_status = "";
		}
		if !ui.voice_preview_requested {
			if self.mic_preview.is_some() {
				ui.voice_preview_status = "";
			}
			self.mic_preview = None;
			ui.voice_preview_level = None;
			return;
		}
		if self.mic_preview.is_none() {
			let failure = Arc::new(OnceLock::new());
			let worker_failure = failure.clone();
			let wake = ctx.clone();
			match Audio::preview(
				Devices {
					input: ui.voice_input.clone(),
					output: ui.voice_output.clone(),
				},
				move |result| {
					if let Err(error) = result {
						let _ = worker_failure.set(error);
					}
					wake.request_repaint();
				},
			) {
				Ok(audio) => {
					self.mic_preview = Some(MicPreview {
						audio,
						devices: Devices {
							input: ui.voice_input.clone(),
							output: ui.voice_output.clone(),
						},
						failure,
						started: Instant::now(),
					})
				}
				Err(error) => {
					ui.voice_preview_status = error;
					ui.voice_preview_requested = false;
					return;
				}
			}
		}
		let preview = self.mic_preview.as_mut().expect("preview started");
		let devices = Devices {
			input: ui.voice_input.clone(),
			output: ui.voice_output.clone(),
		};
		if devices != preview.devices {
			preview.audio.set_devices(devices.clone());
			preview.devices = devices;
			preview.started = Instant::now();
		}
		preview
			.audio
			.set_gain(ui.voice_gain.input_percent, ui.voice_gain.output_percent);
		preview
			.audio
			.set_processing(ui.voice_processing.effective());
		preview.audio.set_ready(true);
		let error = preview.failure.get().copied().or_else(|| {
			if preview.audio.is_stopped() {
				Some("Microphone test stopped; try again.")
			} else if !preview.audio.is_ready() && preview.started.elapsed() >= DEVICE_OPEN_TIMEOUT
			{
				Some(
					"Audio devices did not open; check device selection and microphone permission.",
				)
			} else {
				None
			}
		});
		if let Some(error) = error {
			ui.voice_preview_requested = false;
			ui.voice_preview_level = None;
			ui.voice_preview_status = error;
			self.mic_preview = None;
			return;
		}
		ui.voice_preview_level = Some(preview.audio.preview_level_db());
		ui.voice_preview_status = if preview.audio.microphone_unavailable() {
			"Microphone unavailable; check permission or choose another input. Retrying…"
		} else if preview.audio.is_ready() {
			"Playing your microphone through the selected speakers."
		} else {
			"Opening microphone and speakers…"
		};
		ctx.request_repaint_after(Duration::from_millis(50));
	}

	fn poll_camera_test(&mut self, state: &State, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		ui.camera_test_available = !state.demo
			&& ui.voice_available
			&& discord_voice::camera::SUPPORTED
			&& state.voice.active.is_none()
			&& self.call.is_none();
		if !ui.camera_test_available || !ui.voice_settings_open() {
			ui.camera_test_requested = false;
			ui.camera_test_status = "";
		}
		if let Some(preview) = &self.camera_test {
			if preview.device != ui.voice_camera_device {
				ui.camera_test_requested = false;
				ui.camera_test_status = "Camera changed. Click Preview camera to use it.";
			} else if let Some(error) = preview.camera.error() {
				ui.camera_test_requested = false;
				ui.camera_test_status = error;
			}
		}
		if !ui.camera_test_requested {
			self.camera_test = None;
			ui.camera_test_texture = None;
			return;
		}
		if self.camera_test.is_none() {
			let picture = Arc::new(std::sync::Mutex::new(None));
			let frames = picture.clone();
			let wake = ctx.clone();
			// No transport sender is captured: these frames can only reach the settings texture.
			let on_frame = Arc::new(move |frame: discord_voice::camera::Frame| {
				if let Ok(mut slot) = frames.try_lock()
					&& store_camera_frame(&mut slot, &frame.rgb)
				{
					wake.request_repaint();
				}
			});
			let wake = ctx.clone();
			match discord_voice::camera::Camera::start(
				ui.voice_camera_device.clone(),
				on_frame,
				Arc::new(move || wake.request_repaint()),
			) {
				Ok(camera) => {
					self.camera_test = Some(CameraTest {
						camera,
						device: ui.voice_camera_device.clone(),
						picture,
					});
					ui.camera_test_status = "Opening camera…";
				}
				Err(error) => {
					ui.camera_test_requested = false;
					ui.camera_test_status = error;
					return;
				}
			}
		}
		let image = self.camera_test.as_ref().and_then(|preview| {
			let mut slot = preview.picture.try_lock().ok()?;
			let (image, dirty) = slot.as_mut()?;
			std::mem::take(dirty).then(|| image.clone())
		});
		if let Some(image) = image {
			if let Some(texture) = &mut ui.camera_test_texture {
				texture.set(image, egui::TextureOptions::LINEAR);
			} else {
				ui.camera_test_texture =
					Some(ctx.load_texture("settings-camera", image, egui::TextureOptions::LINEAR));
			}
			ui.camera_test_status = "Local camera preview · not shared";
		}
	}

	fn poll_camera_devices(&mut self, demo: bool, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		if demo {
			ui.voice_refresh_cameras = false;
			ui.voice_camera_devices_loading = false;
			self.camera_scan = None;
			return;
		}
		if std::mem::take(&mut ui.voice_refresh_cameras) && self.camera_scan.is_none() {
			let (send, receive) = mpsc::sync_channel(1);
			let wake = ctx.clone();
			match std::thread::Builder::new()
				.name("camera-devices".into())
				.spawn(move || {
					let result = std::panic::catch_unwind(discord_voice::camera::devices)
						.unwrap_or(Err("Camera device discovery failed"));
					let _ = send.send(result);
					wake.request_repaint();
				}) {
				Ok(_) => {
					self.camera_scan = Some(receive);
					ui.voice_camera_devices_loading = true;
					ui.voice_camera_device_status = "Looking for cameras…";
				}
				Err(_) => ui.voice_camera_device_status = "Could not start camera device discovery",
			}
		}
		if let Some(scan) = &self.camera_scan {
			match scan.try_recv() {
				Ok(result) => {
					ui.voice_camera_device_status = match result {
						Ok(devices) => {
							ui.voice_cameras = devices;
							if ui.voice_cameras.is_empty() {
								"No cameras found. Check the camera connection or virtual camera installation, then refresh."
							} else {
								"Cameras loaded"
							}
						}
						Err(error) => error,
					};
					self.camera_scan = None;
					ui.voice_camera_devices_loading = false;
				}
				Err(mpsc::TryRecvError::Disconnected) => {
					ui.voice_camera_device_status = "Camera device discovery stopped";
					ui.voice_camera_devices_loading = false;
					self.camera_scan = None;
				}
				Err(mpsc::TryRecvError::Empty) => {}
			}
		}
	}
	/// The local camera follows the call's camera choice while its media is connected.
	fn poll_camera(&mut self, state: &mut State, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		let supported = discord_voice::camera::SUPPORTED;
		ui.voice_camera_available = supported && state.voice.camera_negotiated();
		let requested = state.voice.active.as_ref().is_some_and(|call| call.camera);
		if requested && self.camera.is_some() && self.camera_device != ui.voice_camera_device {
			self.stop_camera();
			ui.voice_camera_preview = None;
			ui.voice_camera_status =
				"Camera device changed. Turn on the camera to use the selected device.";
			state.set_call_camera(false);
			return;
		}
		let error = self.camera.as_ref().and_then(|camera| camera.error());
		if !requested
			|| !camera_preview_allowed(state)
			|| !ui.voice_camera_available
			|| error.is_some()
		{
			self.stop_camera();
			ui.voice_camera_preview = None;
			if requested && let Some(error) = error {
				ui.voice_camera_status = error;
				state.set_call_camera(false);
			}
			return;
		}
		if self.camera_preview.is_none() {
			if self.camera.is_some() {
				// The previous camera is still closing; it opens again on a later frame.
				ui.voice_camera_status = "Opening camera…";
				ctx.request_repaint_after(Duration::from_millis(50));
				return;
			}
			let Some(media) = self.call.as_ref() else {
				return;
			};
			self.camera_generation = self.camera_generation.checked_add(1).unwrap_or(1);
			let generation = self.camera_generation;
			let send = media.camera_frames.clone();
			let receive = std::sync::Arc::new(std::sync::Mutex::new(None));
			let preview = receive.clone();
			let start = media.camera_clock;
			let wake = ctx.clone();
			let on_frame = std::sync::Arc::new(move |frame: discord_voice::camera::Frame| {
				let data = frame.h264;
				if data.len() > discord_voice::camera_video::MAX_FRAME_BYTES {
					return;
				}
				let _ = send.try_send(discord_voice::camera_video::Frame {
					generation,
					timestamp: (start.elapsed().as_micros() * 90 / 1000) as u32,
					data,
				});
				if let Ok(mut slot) = preview.try_lock()
					&& store_camera_frame(&mut slot, &frame.rgb)
				{
					wake.request_repaint();
				}
			});
			let wake = ctx.clone();
			match discord_voice::camera::Camera::start(
				ui.voice_camera_device.clone(),
				on_frame,
				std::sync::Arc::new(move || wake.request_repaint()),
			) {
				Ok(camera) => {
					self.camera_device = ui.voice_camera_device.clone();
					self.camera = Some(camera);
					self.camera_preview = Some(receive);
					ui.voice_camera_status = "Opening camera…";
					media
						.controls
						.send_modify(|controls| controls.camera = generation);
				}
				Err(error) => {
					ui.voice_camera_status = error;
					state.set_call_camera(false);
					return;
				}
			}
		}
		let image = self.camera_preview.as_ref().and_then(|preview| {
			let mut preview = preview.try_lock().ok()?;
			let (image, dirty) = preview.as_mut()?;
			std::mem::take(dirty).then(|| image.clone())
		});
		if let Some(image) = image {
			if let Some(texture) = &mut ui.voice_camera_preview {
				texture.set(image, egui::TextureOptions::LINEAR);
			} else {
				ui.voice_camera_preview =
					Some(ctx.load_texture("local-camera", image, egui::TextureOptions::LINEAR));
				let cue = model::notification_preferences::Sound::CameraOn;
				if ui.notification_options.allows(cue) {
					ui.notification_preview = Some(cue);
					ctx.request_repaint();
				}
			}
			ui.voice_camera_status = "Camera on · local preview";
		}
	}

	/// Opens audio and the voice transport for media run `id`.
	fn start_media(
		&mut self,
		runtime: &Runtime,
		state: &State,
		id: u64,
		ui: &ui::MessagingUi,
		ctx: &egui::Context,
	) -> Result<(), &'static str> {
		let credentials = state
			.media_credentials(id)
			.ok_or("Voice connection details expired")?;
		let channel = credentials.channel;
		let listen_only =
			permission_mutes_microphone(state, channel, ui.voice_push_to_talk, ui.voice_ptt_active)
				|| ui.voice_ptm_active;
		let (capture_send, capture) = mpsc::sync_channel(8);
		let (camera_frames, camera_receive) = mpsc::sync_channel(1);
		let (stream_audio, stream_audio_receive) = mpsc::sync_channel(8);
		let (playback, playback_receive) = mpsc::sync_channel(8);
		let (send, events) = mpsc::sync_channel(16);
		let failure = Arc::new(OnceLock::new());
		let audio_failure = failure.clone();
		let (speaking, speakers) = watch::channel([0; 64]);
		let audio_send = send.clone();
		let wake = ctx.clone();
		let devices = Devices {
			input: ui.voice_input.clone(),
			output: ui.voice_output.clone(),
		};
		let audio = Audio::start(
			devices.clone(),
			capture_send,
			playback_receive,
			move |result| {
				match result {
					Ok(()) => {
						let _ = audio_send.try_send(Notice::DeviceReady);
					}
					Err(error) => {
						let _ = audio_failure.set(error);
					}
				}
				wake.request_repaint();
			},
		)?;
		let (controls, control_receive) = watch::channel(Controls {
			activity_threshold_db: ui
				.voice_processing
				.effective()
				.sensitivity_db
				.unwrap_or(-70),
			muted: listen_only || ui.voice_push_to_talk,
			camera: 0,
			deafened: false,
			user_volumes: ui.voice_user_volumes(),
			stream_volume: ui.voice_stream_volume(),
		});
		let remote_video: Arc<std::sync::Mutex<RemotePictures>> =
			Arc::new(std::sync::Mutex::new(Vec::new()));
		let pictures = remote_video.clone();
		let picture_wake = ctx.clone();
		let sink: discord_voice::VideoSink = Arc::new(move |frame: discord_voice::RemoteFrame| {
			if pictures
				.lock()
				.is_ok_and(|mut pictures| store_remote_frame(&mut pictures, frame))
			{
				picture_wake.request_repaint();
			}
		});
		audio.set_controls(listen_only || ui.voice_push_to_talk, false);
		audio.set_input_enabled(state.can_speak(channel));
		audio.set_processing(ui.voice_processing.effective());
		audio.set_gain(ui.voice_gain.input_percent, ui.voice_gain.output_percent);
		let identity = discord_voice::Identity::generate();
		let media_identity = identity.clone();
		let transport_failure = failure.clone();
		let wake = ctx.clone();
		let task = runtime.spawn(async move {
			let status = send.clone();
			let status_wake = wake.clone();
			let result = discord_voice::run_with_identity(
				credentials,
				capture,
				playback,
				control_receive,
				if discord_voice::camera::SUPPORTED {
					Some(camera_receive)
				} else {
					None
				},
				Some(sink),
				Some(stream_audio_receive),
				move |event| {
					let notice = match event {
						Status::TransportReady => return Ok(()),
						Status::CameraAvailable(available) => Notice::Camera(available),
						Status::Connecting => Notice::Progress(Phase::ConnectingTransport),
						Status::Discovering => Notice::Progress(Phase::Discovering),
						Status::Securing => Notice::Progress(Phase::Securing),
						Status::WaitingForPeer => Notice::Connected {
							others: false,
							privacy_code: None,
						},
						Status::Ready { privacy_code } => {
							if privacy_code.len() > 256 {
								return Err(());
							}
							Notice::Connected {
								others: true,
								privacy_code: Some(privacy_code),
							}
						}
						Status::RemoteAudio => Notice::RemoteAudio,
						Status::Speaking(users) => {
							speaking.send_replace(*users);
							status_wake.request_repaint();
							return Ok(());
						}
					};
					status.try_send(notice).map_err(|_| ())?;
					status_wake.request_repaint();
					Ok(())
				},
				media_identity,
			)
			.await;
			if let Err(error) = result {
				let _ = transport_failure.set(error);
			}
			wake.request_repaint();
		});
		self.call = Some(CallMedia {
			id,
			identity,
			audio,
			controls,
			events,
			failure,
			speakers,
			task,
			devices,
			device_deadline: None,
			camera_frames,
			camera_clock: Instant::now(),
			remote_video,
			stream_audio,
			connected: None,
			privacy_code: None,
			reported: None,
		});
		Ok(())
	}
}
impl Drop for Voice {
	fn drop(&mut self) {
		self.stop();
	}
}

fn camera_preview_allowed(state: &State) -> bool {
	state.voice.media_ready()
		&& state
			.voice
			.active
			.as_ref()
			.is_some_and(|call| state.can_camera(call.channel))
}

#[cfg(all(debug_assertions, feature = "demo"))]
pub fn debug_mic_preview_check() {
	discord_voice::audio::debug_processing_check();
	let ctx = egui::Context::default();
	ui::design::apply(&ctx);
	let mut state = test_support::demo_state();
	let mut view = ui::MessagingUi::default();
	view.voice_available = true;
	view.preview_settings("voice");
	assert!(view.voice_settings_open());
	for (width, profile) in [
		(480.0, model::voice_settings::InputProfile::VoiceIsolation),
		(1120.0, model::voice_settings::InputProfile::Studio),
		(1120.0, model::voice_settings::InputProfile::Custom),
	] {
		view.voice_processing.profile = profile;
		for _ in 0..3 {
			ctx.run_ui(
				egui::RawInput {
					screen_rect: Some(egui::Rect::from_min_size(
						egui::Pos2::ZERO,
						egui::vec2(width, 760.0),
					)),
					..Default::default()
				},
				|ui| {
					let _ = view.show(ui, &mut state);
				},
			)
			.drop_without_applying_deltas();
		}
	}
	assert!(!view.voice_preview_requested);
	assert!(!view.camera_test_requested);
	let mut camera_host = Voice::default();
	view.camera_test_requested = true;
	camera_host.poll_camera_test(&state, &mut view, &ctx);
	assert!(!view.camera_test_requested && camera_host.camera_test.is_none());
	state.demo = false;
	view.preview_settings("appearance");
	view.camera_test_requested = true;
	camera_host.poll_camera_test(&state, &mut view, &ctx);
	assert!(!view.camera_test_requested && camera_host.camera_test.is_none());
	state.demo = true;
	view.preview_settings("voice");
	let legacy: local_store::AppPreferences =
		serde_json::from_str(r#"{"voice_noise_suppression":true}"#).unwrap();
	let mut preferences = crate::app_settings::Settings {
		current: legacy,
		..Default::default()
	};
	preferences.apply(&mut view);
	assert_eq!(
		view.voice_processing.effective().suppression,
		model::voice_settings::NoiseSuppression::RnNoise
	);
	view.voice_processing.custom.sensitivity_db = Some(-63);
	preferences.observe(&view);
	let saved = serde_json::to_string(&preferences.current).unwrap();
	let restored: local_store::AppPreferences = serde_json::from_str(&saved).unwrap();
	assert_eq!(restored.voice_processing, Some(view.voice_processing));
	assert!(restored.is_valid());
	let mut voice = Voice::default();
	view.voice_preview_requested = true;
	voice.poll_mic_preview(&state, &mut view, &ctx);
	assert!(!view.voice_preview_requested && voice.mic_preview.is_none());
	state.demo = false;
	view.preview_settings("appearance");
	view.voice_preview_requested = true;
	voice.poll_mic_preview(&state, &mut view, &ctx);
	assert!(!view.voice_preview_requested && voice.mic_preview.is_none());
	println!(
		"Mic/camera preview debug check passed: settings render, opening settings never starts capture, demo and closed-page guards stop requests. No audio devices opened."
	);
}

/// Offline cue selection check; no Discord connection or audio devices are opened.
#[cfg(any(test, all(debug_assertions, feature = "demo")))]
pub fn debug_call_cues_check() {
	let participant = |id| voice::Participant {
		user: Id(id),
		muted: false,
		deafened: false,
		server_muted: false,
		server_deafened: false,
		video: false,
		streaming: false,
	};
	let owner = participant(1);
	let peer = participant(2);
	let mut cues = CallCues::default();
	assert_eq!(cues.poll(false, true, owner.user, &[owner, peer]), None);
	assert_eq!(
		cues.poll(true, true, owner.user, &[owner, peer]),
		Some(Sound::UserJoin)
	);
	let mut muted_peer = peer;
	muted_peer.muted = true;
	assert_eq!(
		cues.poll(true, true, owner.user, &[muted_peer, owner]),
		None
	);
	// Device reopening and rekeying do not announce this same call again.
	assert_eq!(cues.poll(false, true, owner.user, &[owner, peer]), None);
	assert_eq!(cues.poll(true, true, owner.user, &[owner, peer]), None);
	// Compare identities rather than counts; departures can themselves trigger rekeying.
	let replacement = participant(3);
	assert_eq!(
		cues.poll(false, true, owner.user, &[owner, replacement]),
		Some(Sound::UserLeave)
	);
	assert_eq!(
		cues.poll(true, true, owner.user, &[owner, replacement]),
		None
	);
	assert_eq!(cues.poll(false, false, owner.user, &[]), None);
	assert_eq!(cues.poll(true, true, owner.user, &[owner]), None);
	// A remote join is audible even while it rekeys media, and only once.
	assert_eq!(
		cues.poll(false, true, owner.user, &[owner, peer]),
		Some(Sound::UserJoin)
	);
	assert_eq!(cues.poll(true, true, owner.user, &[peer, owner]), None);
	assert_eq!(
		cues.poll(true, true, owner.user, &[owner, muted_peer]),
		None
	);
	assert_eq!(
		cues.poll(true, true, owner.user, &[owner]),
		Some(Sound::UserLeave)
	);
	assert_eq!(cues.poll(true, true, owner.user, &[owner]), None);
	assert_eq!(
		CallCues::default().poll(true, true, owner.user, &[owner]),
		Some(Sound::UserJoin),
		"a new explicitly started call has its own join cue"
	);
	println!(
		"Call cue debug check passed: local/remote joins, departures, rekeying and reconnect suppression. No audio devices opened."
	);
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn call_cues_track_joins_and_departures_without_reconnect_noise() {
		debug_call_cues_check();
	}

	#[test]
	fn optimization_remote_video_reuses_the_latest_frame_buffer() {
		let mut pictures = Vec::new();
		let frame = |data: &'static [u8], width| discord_voice::RemoteFrame {
			user: 7,
			picture: platform::video::Picture {
				width,
				height: 2,
				layout: platform::video::Layout::I420,
				color: platform::video::Color::WEBRTC,
				rotation: 0,
				data,
			},
		};
		const TWO: [u8; 6] = [16, 16, 16, 16, 128, 128];
		assert!(store_remote_frame(&mut pictures, frame(&TWO, 2)));
		let planes = pictures[0].1.data.as_ptr();
		pictures[0].2 = false;
		assert!(store_remote_frame(&mut pictures, frame(&TWO, 2)));
		assert_eq!(pictures[0].1.data.as_ptr(), planes);
		assert!(pictures[0].2);
		// A smaller picture keeps the allocation; invalid planes are rejected.
		assert!(store_remote_frame(
			&mut pictures,
			frame(&[16, 16, 128, 128], 1)
		));
		assert_eq!(pictures[0].1.data.as_ptr(), planes);
		assert_eq!((pictures[0].1.width, pictures[0].1.data.len()), (1, 4));
		assert!(!store_remote_frame(&mut pictures, frame(&[16], 1)));
	}

	#[test]
	fn optimization_local_camera_reuses_the_latest_frame_buffer() {
		let mut picture = None;
		let rgb = vec![127; discord_voice::camera::WIDTH * discord_voice::camera::HEIGHT * 3];
		assert!(store_camera_frame(&mut picture, &rgb));
		let pixels = picture.as_ref().unwrap().0.pixels.as_ptr();
		picture.as_mut().unwrap().1 = false;
		assert!(store_camera_frame(&mut picture, &rgb));
		assert_eq!(picture.as_ref().unwrap().0.pixels.as_ptr(), pixels);
		assert!(picture.unwrap().1);
	}

	#[test]
	#[allow(clippy::field_reassign_with_default)] // MessagingUi has private fields in another crate.
	fn camera_discovery_updates_selection_without_starting_capture_and_demo_does_not_scan() {
		let mut voice = Voice::default();
		let mut ui = ui::MessagingUi::default();
		ui.voice_refresh_cameras = true;
		let ctx = egui::Context::default();
		voice.poll_camera_devices(true, &mut ui, &ctx);
		assert!(voice.camera_scan.is_none() && !ui.voice_refresh_cameras);
		let (send, receive) = mpsc::sync_channel(1);
		voice.camera_scan = Some(receive);
		ui.voice_camera_devices_loading = true;
		ui.voice_camera_device = Some("dshow:second".into());
		send.send(Ok(vec![
			("dshow:first".into(), "First camera".into()),
			("dshow:second".into(), "Second camera".into()),
		]))
		.unwrap();
		voice.poll_camera_devices(false, &mut ui, &ctx);
		assert_eq!(ui.voice_cameras.len(), 2);
		assert_eq!(ui.voice_camera_device.as_deref(), Some("dshow:second"));
		assert!(!ui.voice_camera_devices_loading);
		assert!(voice.camera.is_none() && voice.camera_scan.is_none());
		let (send, receive) = mpsc::sync_channel(1);
		voice.camera_scan = Some(receive);
		send.send(Ok(vec![])).unwrap();
		voice.poll_camera_devices(true, &mut ui, &ctx);
		assert_eq!(
			ui.voice_cameras.len(),
			2,
			"Demo must not consume a real device scan"
		);
		assert!(voice.camera_scan.is_none());
	}
}
