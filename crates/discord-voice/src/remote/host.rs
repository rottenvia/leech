//! The controlled PC: captures its primary display, sends video and system audio, and injects
//! the viewer's input while the permission allows it.
use super::{End, Input, Link, MouseButton, Permission, Shared, stopped, wire};
use crate::screen::{AudioChunk, EncodedFrame, Settings, SourceId, Worker};
use opus2::{Application, Bitrate, Channels, Encoder};
use platform::remote_input::{Injector, ScreenRect, cursor_hidden};
use std::sync::{
	Arc,
	atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HostConfig {
	/// `None` turns remote control off, which ends the session.
	pub permission: Option<Permission>,
	pub paused: bool,
	pub audio: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostStatus {
	pub connected: bool,
	pub ended: Option<End>,
	pub rtt_ms: u32,
	pub kbps: u32,
}

const START_BITRATE: u32 = 8_000_000;
const MIN_BITRATE: u32 = 1_000_000;
const AUDIO_BITRATE: i32 = 96_000;
/// 20 ms of 48 kHz stereo.
const AUDIO_FRAME: usize = 960 * 2;
/// System audio waiting for a whole Opus frame; more than this is old and dropped.
const MAX_AUDIO_PENDING: usize = AUDIO_FRAME * 10;
/// Fragments sent back to back before pacing yields for a millisecond: a 12 Mbps 60 fps
/// picture goes in one burst, a keyframe spread over a few milliseconds.
const BURST: usize = 32;
/// Cursor-capture checks; state also goes out every fifth tick.
const TICK: Duration = Duration::from_millis(100);
const STATE_TICKS: u32 = 5;
/// Keyframe requests honoured at most this often (the viewer asks at most every 300 ms).
const KEYFRAME_SPACING: Duration = Duration::from_millis(200);
/// Queued injector commands; inputs leave room for releases.
const COMMANDS: usize = 256;
const RESERVED_COMMANDS: usize = 16;

enum Command {
	Apply(Vec<Input>, bool, bool),
	Sync(Vec<(u16, bool)>, Vec<MouseButton>, bool, bool),
	ReleaseAll,
	Screen(ScreenRect),
}

pub struct Host {
	link: Link,
	config: watch::Sender<HostConfig>,
	kbps: Arc<AtomicU32>,
}

impl Host {
	/// Starts capture and the session on `link`. `wake` runs when the session ends. Tasks run
	/// on the link's own media thread, so `runtime` is unused.
	pub fn start(
		mut link: Link,
		config: HostConfig,
		_runtime: &tokio::runtime::Handle,
		wake: impl Fn() + Send + Sync + 'static,
	) -> Result<Self, &'static str> {
		let shared = link.shared.clone();
		shared.set_wake(Arc::new(wake));
		if shared.ended().is_some() {
			return Err("The other PC already ended remote control");
		}
		if config.permission.is_none() {
			shared.finish(End::ByHost, Some(wire::BYE_ENDED));
			return Err("Remote control is turned off");
		}
		let kbps = Arc::new(AtomicU32::new(0));
		match launch(&mut link, config, kbps.clone()) {
			Ok(config) => Ok(Self { link, config, kbps }),
			Err(error) => {
				shared.finish(End::Failed(error), Some(wire::BYE_FAILED));
				Err(error)
			}
		}
	}

	pub fn set(&self, config: HostConfig) {
		if config.permission.is_none() {
			self.end();
		} else {
			self.config.send_replace(config);
		}
	}

	pub fn status(&self) -> HostStatus {
		let shared = &self.link.shared;
		HostStatus {
			connected: shared.connected(),
			ended: shared.ended(),
			rtt_ms: shared.rtt(),
			kbps: self.kbps.load(Ordering::Relaxed),
		}
	}

	pub fn end(&self) {
		self.link.shared.finish(End::ByHost, Some(wire::BYE_ENDED));
	}
}

fn launch(
	link: &mut Link,
	config: HostConfig,
	kbps: Arc<AtomicU32>,
) -> Result<watch::Sender<HostConfig>, &'static str> {
	let shared = link.shared.clone();
	let mut inbox = link
		.inbox
		.take()
		.ok_or("Remote control link already in use")?;
	// Anything the viewer sent during setup is stale.
	while inbox.try_recv().is_ok() {}
	let display = Display::primary()?;
	let screen = display
		.size()
		.ok_or("The primary display size is unavailable")?;
	let stream = if screen.0 >= 1920 || screen.1 >= 1080 {
		(1920, 1080)
	} else {
		(1280, 720)
	};
	let settings = Settings {
		source: display.source(),
		width: stream.0,
		height: stream.1,
		fps: 60,
		cursor: false,
		audio: true,
	};
	let (worker, mut video) = Worker::start(settings, || {})?;
	// No call encryption gates this capture; the link is already secure.
	video.ready.store(true, Ordering::Release);
	let max_bitrate = settings.bit_rate();
	video
		.bitrate
		.store(START_BITRATE.min(max_bitrate), Ordering::Release);

	let (inject, commands) = mpsc::channel(COMMANDS);
	let rect = rect(screen);
	std::thread::Builder::new()
		.name("remote-input".into())
		.spawn(move || inject_loop(rect, commands))
		.map_err(|_| "Remote control input could not start")?;

	let audio = Arc::new(AtomicBool::new(config.audio));
	let (config_send, configs) = watch::channel(config);
	let runtime = shared.runtime.clone();
	runtime.spawn(send_video(shared.clone(), video.frames));
	if let Some(chunks) = video.audio.take() {
		runtime.spawn(send_audio(shared.clone(), chunks, audio.clone()));
	}
	let control = Control {
		shared,
		inject,
		config,
		order: wire::Order::default(),
		keyframe: video.keyframe,
		bitrate: video.bitrate,
		max_bitrate,
		audio,
		display,
		screen,
		stream,
		relative_mouse: cursor_hidden(),
		keyframe_at: Instant::now(),
		kbps,
		measured: (Instant::now(), 0),
	};
	runtime.spawn(control.run(inbox, configs, worker));
	Ok(config_send)
}

fn rect(screen: (u32, u32)) -> ScreenRect {
	// The primary display's top-left corner is the virtual desktop's origin.
	ScreenRect {
		x: 0,
		y: 0,
		width: screen.0 as i32,
		height: screen.1 as i32,
	}
}

/// Owns the injector on its own MMCSS thread, so a game's load cannot delay input; dropping
/// it at the end releases everything still held.
fn inject_loop(rect: ScreenRect, mut commands: mpsc::Receiver<Command>) {
	let _resolution = crate::timer::Resolution::acquire();
	let mut injector = Injector::new(rect);
	while let Some(command) = commands.blocking_recv() {
		match command {
			Command::Apply(inputs, keyboard, mouse) => {
				for input in inputs {
					injector.apply(input, keyboard, mouse);
				}
			}
			Command::Sync(keys, buttons, keyboard, mouse) => {
				injector.sync_held(&keys, &buttons, keyboard, mouse);
			}
			Command::ReleaseAll => injector.release_all(),
			Command::Screen(rect) => {
				injector.release_all();
				injector = Injector::new(rect);
			}
		}
	}
}

struct Control {
	shared: Arc<Shared>,
	inject: mpsc::Sender<Command>,
	config: HostConfig,
	order: wire::Order,
	keyframe: Arc<AtomicBool>,
	bitrate: Arc<AtomicU32>,
	max_bitrate: u32,
	audio: Arc<AtomicBool>,
	display: Display,
	screen: (u32, u32),
	stream: (u32, u32),
	relative_mouse: bool,
	keyframe_at: Instant,
	kbps: Arc<AtomicU32>,
	measured: (Instant, u64),
}

impl Control {
	async fn run(
		mut self,
		mut inbox: mpsc::Receiver<super::Packet>,
		mut configs: watch::Receiver<HostConfig>,
		worker: Worker,
	) {
		let mut stop = self.shared.stop.subscribe();
		let mut tick = tokio::time::interval(TICK);
		tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
		let mut ticks = 0u32;
		self.send_state().await;
		loop {
			tokio::select! {
				() = stopped(&mut stop) => break,
				packet = inbox.recv() => match packet {
					Some((kind, payload)) => self.handle(kind, &payload),
					None => break,
				},
				changed = configs.changed() => {
					if changed.is_err() {
						break;
					}
					let config = *configs.borrow_and_update();
					self.configure(config);
					self.send_state().await;
				}
				_ = tick.tick() => {
					ticks = ticks.wrapping_add(1);
					let relative_mouse = cursor_hidden();
					let mut changed = relative_mouse != self.relative_mouse;
					self.relative_mouse = relative_mouse;
					if ticks.is_multiple_of(STATE_TICKS) {
						if let Some(result) = worker.result() {
							let reason = result.err().unwrap_or("Screen capture stopped");
							self.shared.finish(End::Failed(reason), Some(wire::BYE_FAILED));
							break;
						}
						self.refresh_display();
						self.measure();
						changed = true;
					}
					if changed {
						self.send_state().await;
					}
				}
			}
		}
		// Dropping the worker stops capture; dropping `inject` releases every held input.
	}

	fn allowed(&self) -> (bool, bool) {
		match (self.config.paused, self.config.permission) {
			(false, Some(permission)) => (permission.keyboard(), permission.mouse()),
			_ => (false, false),
		}
	}

	fn command(&self, command: Command, control: bool) {
		// Inputs never fill the queue: a release must always fit.
		if control || self.inject.capacity() > RESERVED_COMMANDS {
			let _ = self.inject.try_send(command);
		}
	}

	fn handle(&mut self, kind: u8, payload: &[u8]) {
		let (keyboard, mouse) = self.allowed();
		match kind {
			wire::INPUT => {
				let Some((first, batches)) = wire::read_batches(payload) else {
					return;
				};
				for (offset, inputs) in batches.into_iter().enumerate() {
					if self.order.take(first.saturating_add(offset as u32)) && (keyboard || mouse) {
						self.command(Command::Apply(inputs, keyboard, mouse), false);
					}
				}
			}
			wire::HELD => {
				if let Some(held) = wire::Held::read(payload)
					&& self.order.held(held.sequence)
					&& (keyboard || mouse)
				{
					self.command(
						Command::Sync(held.keys, held.buttons, keyboard, mouse),
						false,
					);
				}
			}
			wire::KEYFRAME => {
				if self.keyframe_at.elapsed() >= KEYFRAME_SPACING {
					self.keyframe_at = Instant::now();
					self.keyframe.store(true, Ordering::Release);
				}
			}
			wire::LOSS => {
				if let Some(permille) = wire::Reader(payload).u16() {
					let rate = self.bitrate.load(Ordering::Acquire);
					let rate = match permille {
						31.. => rate / 4 * 3,
						0..10 => rate / 100 * 108,
						_ => rate,
					};
					self.bitrate
						.store(rate.clamp(MIN_BITRATE, self.max_bitrate), Ordering::Release);
				}
			}
			_ => {}
		}
	}

	fn configure(&mut self, config: HostConfig) {
		if config.permission != self.config.permission || config.paused != self.config.paused {
			self.command(Command::ReleaseAll, true);
		}
		self.audio.store(config.audio, Ordering::Release);
		self.config = config;
	}

	fn refresh_display(&mut self) {
		if let Some(screen) = self.display.size()
			&& screen != self.screen
		{
			self.screen = screen;
			self.command(Command::Screen(rect(screen)), true);
		}
	}

	fn measure(&mut self) {
		let (at, bytes) = self.measured;
		let sent = self.shared.sent.load(Ordering::Relaxed);
		let millis = at.elapsed().as_millis().max(1) as u64;
		self.kbps.store(
			(sent.saturating_sub(bytes) * 8 / millis).min(u64::from(u32::MAX)) as u32,
			Ordering::Relaxed,
		);
		self.measured = (Instant::now(), sent);
	}

	async fn send_state(&self) {
		let narrow = |value: u32| value.min(u32::from(u16::MAX)) as u16;
		let state = wire::State {
			permission: self.config.permission,
			paused: self.config.paused,
			audio: self.config.audio,
			relative_mouse: self.relative_mouse,
			screen: (narrow(self.screen.0), narrow(self.screen.1)),
			stream: (narrow(self.stream.0), narrow(self.stream.1)),
			content: content(self.screen, self.stream).map(narrow),
		};
		self.shared.send(wire::STATE, &state.encode()).await;
	}
}

/// Where the encoder places the display inside the stream (x, y, width, height): scaled to
/// fit with its aspect ratio and centred, the rest letterboxed. Matches `screen::convert::fit`,
/// which both the CPU converter and the GPU video processor use.
fn content(screen: (u32, u32), stream: (u32, u32)) -> [u32; 4] {
	if screen.0 == 0 || screen.1 == 0 {
		return [0, 0, stream.0, stream.1];
	}
	let scale =
		(f64::from(stream.0) / f64::from(screen.0)).min(f64::from(stream.1) / f64::from(screen.1));
	let width = ((f64::from(screen.0) * scale).round() as u32).clamp(1, stream.0);
	let height = ((f64::from(screen.1) * scale).round() as u32).clamp(1, stream.1);
	[
		(stream.0 - width) / 2,
		(stream.1 - height) / 2,
		width,
		height,
	]
}

/// Fragments each access unit; big ones are paced so a keyframe never leaves as one burst.
async fn send_video(shared: Arc<Shared>, mut frames: mpsc::Receiver<EncodedFrame>) {
	let mut stop = shared.stop.subscribe();
	let mut payload = [0; wire::MAX_PAYLOAD];
	let mut id = 0u32;
	loop {
		let frame = tokio::select! {
			() = stopped(&mut stop) => return,
			frame = frames.recv() => frame,
		};
		let Some(frame) = frame else { return };
		let count = frame.data.len().div_ceil(wire::MAX_FRAGMENT);
		if count == 0 || count > wire::MAX_FRAGMENTS {
			continue;
		}
		id = id.wrapping_add(1);
		for (index, data) in frame.data.chunks(wire::MAX_FRAGMENT).enumerate() {
			if index > 0 && index.is_multiple_of(BURST) {
				tokio::time::sleep(Duration::from_millis(1)).await;
				if *stop.borrow() {
					return;
				}
			}
			let len = wire::Fragment {
				frame: id,
				index: index as u16,
				count: count as u16,
				keyframe: frame.keyframe,
				timestamp: frame.timestamp,
				data,
			}
			.write(&mut payload);
			shared.send(wire::VIDEO, &payload[..len]).await;
		}
	}
}

/// System audio as 20 ms stereo Opus frames, while the host allows it.
async fn send_audio(
	shared: Arc<Shared>,
	mut chunks: mpsc::Receiver<AudioChunk>,
	enabled: Arc<AtomicBool>,
) {
	let Ok(mut encoder) = Encoder::new(48_000, Channels::Stereo, Application::LowDelay) else {
		return;
	};
	if encoder.set_bitrate(Bitrate::Bits(AUDIO_BITRATE)).is_err() {
		return;
	}
	let mut stop = shared.stop.subscribe();
	let mut pending: Vec<f32> =
		Vec::with_capacity(MAX_AUDIO_PENDING + crate::screen::MAX_AUDIO_SAMPLES);
	// Opus never exceeds the buffer it is given, which keeps every packet within a datagram.
	let mut packet = [0; 4 + 1000];
	let mut sequence = 0u32;
	loop {
		let chunk = tokio::select! {
			() = stopped(&mut stop) => return,
			chunk = chunks.recv() => chunk,
		};
		let Some(chunk) = chunk else { return };
		if !enabled.load(Ordering::Acquire) {
			pending.clear();
			continue;
		}
		let samples =
			&chunk.samples[..chunk.samples.len().min(crate::screen::MAX_AUDIO_SAMPLES) & !1];
		pending.extend_from_slice(samples);
		if pending.len() > MAX_AUDIO_PENDING {
			pending.drain(..pending.len() - AUDIO_FRAME);
		}
		while pending.len() >= AUDIO_FRAME {
			sequence = sequence.wrapping_add(1);
			packet[..4].copy_from_slice(&sequence.to_le_bytes());
			if let Ok(len) = encoder.encode_float(&pending[..AUDIO_FRAME], &mut packet[4..]) {
				shared.send(wire::AUDIO, &packet[..4 + len]).await;
			}
			pending.drain(..AUDIO_FRAME);
		}
	}
}

/// The captured display: the primary one, whose top-left is the virtual desktop's origin.
#[cfg(target_os = "windows")]
struct Display(usize);

#[cfg(target_os = "windows")]
impl Display {
	fn primary() -> Result<Self, &'static str> {
		windows_capture::monitor::Monitor::primary()
			.map(|monitor| Self(monitor.as_raw_hmonitor() as usize))
			.map_err(|_| "The primary display is unavailable")
	}

	/// The same id screen capture gives this display.
	fn source(&self) -> SourceId {
		SourceId::Display(self.0 as u64)
	}

	/// Current mode in physical pixels.
	fn size(&self) -> Option<(u32, u32)> {
		let monitor = windows_capture::monitor::Monitor::from_raw_hmonitor(self.0 as *mut _);
		Some((monitor.width().ok()?, monitor.height().ok()?))
			.filter(|(width, height)| *width > 0 && *height > 0)
	}
}

#[cfg(not(target_os = "windows"))]
struct Display;

#[cfg(not(target_os = "windows"))]
impl Display {
	fn primary() -> Result<Self, &'static str> {
		Err("Being controlled is available on Windows")
	}
	fn source(&self) -> SourceId {
		SourceId::SystemPicker
	}
	fn size(&self) -> Option<(u32, u32)> {
		None
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn content_rect_matches_the_encoder_letterbox() {
		assert_eq!(content((1920, 1080), (1920, 1080)), [0, 0, 1920, 1080]);
		assert_eq!(content((3840, 2160), (1920, 1080)), [0, 0, 1920, 1080]);
		// Ultrawide: bars above and below; 4:3 and portrait: pillars.
		assert_eq!(content((2560, 1080), (1920, 1080)), [0, 135, 1920, 810]);
		assert_eq!(content((1600, 1200), (1920, 1080)), [240, 0, 1440, 1080]);
		assert_eq!(content((1080, 1920), (1920, 1080)), [656, 0, 608, 1080]);
		assert_eq!(content((1366, 768), (1280, 720)), [0, 0, 1280, 720]);
		assert_eq!(content((1280, 1024), (1280, 720)), [190, 0, 900, 720]);
	}
}
