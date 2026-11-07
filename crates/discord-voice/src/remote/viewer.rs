//! The controlling PC: reassembles and decodes the host's video, plays its audio and sends
//! mouse and keyboard input.
use super::{
	End, Input, Link, MouseButton, Packet, Permission, Shared, Wake, playback::Player, stopped,
	wire,
};
use crate::video_receive::{self, DecoderQueue, Encoded, VideoSink};
use platform::video::Picture;
use std::sync::{
	Arc, Mutex, PoisonError,
	atomic::{AtomicU32, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, Default)]
pub struct ViewerStatus {
	pub connected: bool,
	/// The host finished setup and sent its first state.
	pub started: bool,
	pub ended: Option<End>,
	pub permission: Option<Permission>,
	pub paused: bool,
	pub audio: bool,
	/// Host screen in pixels.
	pub screen: (u32, u32),
	pub relative_mouse: bool,
	pub rtt_ms: u32,
	/// The part of each picture that shows the host screen, as fractions of the picture
	/// (x, y, width, height); the rest is letterbox. Draw this part and map the pointer over
	/// it to [`Input::Move`] fractions. Valid once `started`.
	pub content: [f32; 4],
}

/// Frames waiting for missing fragments are given up after this.
const FRAME_WAIT: Duration = Duration::from_millis(150);
/// Frames reassembled at once; older ones are given up for newer ones.
const MAX_PARTIAL: usize = 4;
const KEYFRAME_SPACING: Duration = Duration::from_millis(300);
const TICK: Duration = Duration::from_millis(50);
/// Loss reports every tenth tick, once enough datagrams arrived to judge.
const LOSS_TICKS: u32 = 10;
const LOSS_SAMPLE: u64 = 20;
/// The decoder keys its state by user; remote control has one source.
const SOURCE: u64 = 1;

pub struct Viewer {
	link: Link,
	picture: Arc<Mutex<Option<Picture>>>,
	view: Arc<View>,
	input: Mutex<Inputs>,
}

#[derive(Default)]
struct Inputs {
	sequence: u32,
	/// The last batch, repeated in the next datagram.
	previous: Vec<u8>,
}

struct View {
	remote: Mutex<ViewerStatus>,
	volume: Arc<AtomicU32>,
}

impl Viewer {
	/// Starts decoding and waits for the host's first state. `wake` runs per picture and on
	/// every state change. Tasks run on the link's own media thread, so `runtime` is unused.
	pub fn start(
		mut link: Link,
		_runtime: &tokio::runtime::Handle,
		wake: impl Fn() + Send + Sync + 'static,
	) -> Result<Self, &'static str> {
		let shared = link.shared.clone();
		let wake: Wake = Arc::new(wake);
		shared.set_wake(wake.clone());
		let Some(inbox) = link.inbox.take() else {
			return Err("Remote control link already in use");
		};
		let picture = Arc::new(Mutex::new(None));
		let slot = picture.clone();
		let picture_wake = wake.clone();
		let sink: VideoSink = Arc::new(move |frame: video_receive::RemoteFrame| {
			if frame.picture.is_valid() {
				store(&slot, frame.picture);
				picture_wake();
			}
		});
		let (decoder, lost) = match video_receive::spawn_decoder(sink) {
			Ok(decoder) => decoder,
			Err(error) => {
				shared.finish(End::Failed(error), Some(wire::BYE_FAILED));
				return Err(error);
			}
		};
		let view = Arc::new(View {
			remote: Mutex::new(ViewerStatus {
				content: [0.0, 0.0, 1.0, 1.0],
				..ViewerStatus::default()
			}),
			volume: Arc::new(AtomicU32::new(1f32.to_bits())),
		});
		shared.runtime.spawn(run(
			shared.clone(),
			inbox,
			decoder,
			lost,
			view.clone(),
			wake,
		));
		Ok(Self {
			link,
			picture,
			view,
			input: Mutex::new(Inputs::default()),
		})
	}

	/// Newest decoded picture since the last call.
	pub fn take_picture(&self) -> Option<Picture> {
		self.picture
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.take()
	}

	/// Sends inputs at once. Each datagram goes out twice and repeats the previous batch, so
	/// neither a lost datagram nor a short burst of loss drops a key press.
	pub fn send(&self, inputs: &[Input]) {
		if inputs.is_empty() || !self.view.started() {
			return;
		}
		let mut state = self.input.lock().unwrap_or_else(PoisonError::into_inner);
		for batch in inputs.chunks(wire::MAX_BATCH) {
			let mut encoded = Vec::with_capacity(1 + batch.len() * wire::MAX_INPUT);
			wire::put_batch(&mut encoded, batch);
			state.sequence = state.sequence.wrapping_add(1);
			let first = if state.previous.is_empty() {
				state.sequence
			} else {
				state.sequence.wrapping_sub(1)
			};
			let mut payload = Vec::with_capacity(4 + state.previous.len() + encoded.len());
			payload.extend_from_slice(&first.to_le_bytes());
			payload.extend_from_slice(&state.previous);
			payload.extend_from_slice(&encoded);
			self.link.shared.try_send(wire::INPUT, &payload);
			self.link.shared.try_send(wire::INPUT, &payload);
			state.previous = encoded;
		}
	}

	/// Every 100 ms while controlling: what the viewer holds, so the host can repair losses.
	pub fn held(&self, keys: &[(u16, bool)], buttons: &[MouseButton]) {
		if !self.view.started() {
			return;
		}
		let sequence = self
			.input
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.sequence;
		self.link
			.shared
			.try_send(wire::HELD, &wire::Held::encode(sequence, keys, buttons));
	}

	/// Host audio playback gain, 0 to 2.
	pub fn set_volume(&self, volume: f32) {
		let volume = if volume.is_finite() {
			volume.clamp(0.0, 2.0)
		} else {
			0.0
		};
		self.view.volume.store(volume.to_bits(), Ordering::Relaxed);
	}

	pub fn status(&self) -> ViewerStatus {
		let shared = &self.link.shared;
		ViewerStatus {
			connected: shared.connected(),
			ended: shared.ended(),
			rtt_ms: shared.rtt(),
			..*self
				.view
				.remote
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
		}
	}

	pub fn end(&self) {
		self.link
			.shared
			.finish(End::ByViewer, Some(wire::BYE_ENDED));
	}
}

impl View {
	fn started(&self) -> bool {
		self.remote
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.started
	}

	/// Applies a host state; true when anything visible changed.
	fn update(&self, state: wire::State) -> bool {
		let (width, height) = (f32::from(state.stream.0), f32::from(state.stream.1));
		let content = if width > 0.0 && height > 0.0 && state.content[2] > 0 && state.content[3] > 0
		{
			[
				f32::from(state.content[0]) / width,
				f32::from(state.content[1]) / height,
				f32::from(state.content[2]) / width,
				f32::from(state.content[3]) / height,
			]
		} else {
			[0.0, 0.0, 1.0, 1.0]
		};
		let mut remote = self.remote.lock().unwrap_or_else(PoisonError::into_inner);
		let next = ViewerStatus {
			started: true,
			permission: state.permission,
			paused: state.paused,
			audio: state.audio,
			screen: (u32::from(state.screen.0), u32::from(state.screen.1)),
			relative_mouse: state.relative_mouse,
			content,
			..*remote
		};
		let visible = |status: &ViewerStatus| {
			(
				status.started,
				status.permission,
				status.paused,
				status.audio,
				status.screen,
				status.relative_mouse,
				status.content,
			)
		};
		let changed = visible(&remote) != visible(&next);
		*remote = next;
		changed
	}
}

/// Keeps only the newest picture, reusing its buffer while the window has not taken it.
fn store(slot: &Mutex<Option<Picture>>, picture: Picture<&[u8]>) {
	let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
	let mut data = slot.take().map(|old| old.data).unwrap_or_default();
	data.clear();
	data.extend_from_slice(picture.data);
	*slot = Some(picture.with_data(data));
}

async fn run(
	shared: Arc<Shared>,
	mut inbox: mpsc::Receiver<Packet>,
	decoder: DecoderQueue,
	lost: video_receive::Lost,
	view: Arc<View>,
	wake: Wake,
) {
	let mut stop = shared.stop.subscribe();
	let mut frames = Reassembly::default();
	let mut requested = Instant::now();
	let mut player: Option<Player> = None;
	let mut tick = tokio::time::interval(TICK);
	tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	let mut ticks = 0u32;
	let mut sampled = (0, 0);
	loop {
		tokio::select! {
			() = stopped(&mut stop) => break,
			packet = inbox.recv() => {
				let Some((kind, payload)) = packet else { break };
				match kind {
					wire::STATE => {
						if let Some(state) = wire::State::read(&payload)
							&& view.update(state)
						{
							wake();
						}
					}
					wire::VIDEO => {
						if let Some(fragment) = wire::Fragment::read(&payload)
							&& let Some((data, keyframe)) = frames.push(&fragment, Instant::now())
						{
							let frame = Encoded { user: SOURCE, data, keyframe };
							if !matches!(video_receive::offer(&decoder, frame), Ok(true)) {
								frames.lose();
							}
						}
					}
					wire::AUDIO => player
						.get_or_insert_with(|| Player::start(view.volume.clone()))
						.push(payload),
					_ => {}
				}
			}
			_ = tick.tick() => {
				frames.expire(Instant::now());
				// The decoder failed a picture: it waits for a keyframe too.
				if !std::mem::take(&mut *lost.lock().unwrap_or_else(PoisonError::into_inner)).is_empty() {
					frames.lose();
				}
				ticks = ticks.wrapping_add(1);
				if ticks.is_multiple_of(LOSS_TICKS) && view.started() {
					let now = (shared.highest.load(Ordering::Relaxed), shared.received.load(Ordering::Relaxed));
					let expected = now.0.saturating_sub(sampled.0);
					if expected >= LOSS_SAMPLE {
						let missing = expected.saturating_sub(now.1.saturating_sub(sampled.1));
						let permille = (missing * 1000 / expected).min(1000) as u16;
						shared.send(wire::LOSS, &permille.to_le_bytes()).await;
						sampled = now;
					}
				}
			}
		}
		if frames.need_keyframe && view.started() && requested.elapsed() >= KEYFRAME_SPACING {
			requested = Instant::now();
			shared.send(wire::KEYFRAME, &[]).await;
		}
	}
}

/// Rebuilds access units from fragments. A frame missing fragments is given up when a newer
/// one completes or after [`FRAME_WAIT`]; predicted frames are then skipped until a keyframe.
struct Reassembly {
	partial: Vec<Partial>,
	/// Newest frame completed or given up; older fragments are late.
	last: u32,
	need_keyframe: bool,
}

struct Partial {
	id: u32,
	count: u16,
	got: u16,
	keyframe: bool,
	received: Vec<bool>,
	data: Vec<u8>,
	len: usize,
	started: Instant,
}

impl Default for Reassembly {
	fn default() -> Self {
		Self {
			partial: Vec::with_capacity(MAX_PARTIAL),
			last: 0,
			// The decoder starts from a keyframe; the host's first picture is one.
			need_keyframe: true,
		}
	}
}

impl Reassembly {
	fn lose(&mut self) {
		self.need_keyframe = true;
	}

	/// Adds one fragment; returns a complete frame to decode.
	fn push(&mut self, fragment: &wire::Fragment, now: Instant) -> Option<(Vec<u8>, bool)> {
		if fragment.frame <= self.last {
			return None;
		}
		let at = match self
			.partial
			.iter()
			.position(|partial| partial.id == fragment.frame)
		{
			Some(at) => at,
			None => {
				if self.partial.len() == MAX_PARTIAL {
					let oldest = (0..self.partial.len()).min_by_key(|&at| self.partial[at].id)?;
					self.give_up(oldest);
					if fragment.frame <= self.last {
						return None;
					}
				}
				let count = usize::from(fragment.count);
				self.partial.push(Partial {
					id: fragment.frame,
					count: fragment.count,
					got: 0,
					keyframe: false,
					received: vec![false; count],
					data: vec![0; count * wire::MAX_FRAGMENT],
					len: 0,
					started: now,
				});
				self.partial.len() - 1
			}
		};
		let partial = &mut self.partial[at];
		let index = usize::from(fragment.index);
		if partial.count != fragment.count || partial.received[index] {
			return None;
		}
		let start = index * wire::MAX_FRAGMENT;
		partial.data[start..start + fragment.data.len()].copy_from_slice(fragment.data);
		partial.received[index] = true;
		partial.got += 1;
		partial.keyframe |= fragment.keyframe;
		if index + 1 == usize::from(partial.count) {
			partial.len = start + fragment.data.len();
		}
		if partial.got < partial.count {
			return None;
		}
		let Partial {
			id,
			keyframe,
			mut data,
			len,
			..
		} = self.partial.swap_remove(at);
		// Older frames still waiting are lost now, as are frames never seen at all.
		let before = self.partial.len();
		self.partial.retain(|partial| partial.id > id);
		if self.partial.len() != before || id != self.last.wrapping_add(1) {
			self.need_keyframe = true;
		}
		self.last = id;
		if self.need_keyframe && !keyframe {
			return None;
		}
		self.need_keyframe = false;
		data.truncate(len);
		Some((data, keyframe))
	}

	/// Gives up frames that waited too long for their fragments.
	fn expire(&mut self, now: Instant) {
		while let Some(at) = self
			.partial
			.iter()
			.position(|partial| now.duration_since(partial.started) >= FRAME_WAIT)
		{
			self.give_up(at);
		}
	}

	fn give_up(&mut self, at: usize) {
		let partial = self.partial.swap_remove(at);
		self.last = self.last.max(partial.id);
		self.need_keyframe = true;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Fragments of frame `id` with `size` bytes, each byte its frame id.
	fn fragments(id: u32, size: usize, keyframe: bool) -> (Vec<u8>, Vec<Vec<u8>>) {
		let data = vec![id as u8; size];
		let count = size.div_ceil(wire::MAX_FRAGMENT);
		let payloads = data
			.chunks(wire::MAX_FRAGMENT)
			.enumerate()
			.map(|(index, chunk)| {
				let mut out = [0; wire::MAX_PAYLOAD];
				let len = wire::Fragment {
					frame: id,
					index: index as u16,
					count: count as u16,
					keyframe,
					timestamp: id * 1500,
					data: chunk,
				}
				.write(&mut out);
				out[..len].to_vec()
			})
			.collect();
		(data, payloads)
	}

	fn feed(frames: &mut Reassembly, payloads: &[Vec<u8>], now: Instant) -> Vec<(Vec<u8>, bool)> {
		payloads
			.iter()
			.filter_map(|payload| frames.push(&wire::Fragment::read(payload).unwrap(), now))
			.collect()
	}

	#[test]
	fn reassembly_skips_to_a_keyframe_after_loss() {
		let now = Instant::now();
		let mut frames = Reassembly::default();
		let (key, mut payloads) = fragments(1, 3 * wire::MAX_FRAGMENT + 10, true);
		// Out of order and duplicated fragments still make one frame.
		payloads.swap(0, 3);
		payloads.push(payloads[1].clone());
		assert_eq!(feed(&mut frames, &payloads, now), [(key, true)]);
		assert!(!frames.need_keyframe);

		// Frame 2 loses a fragment; frame 3 completes first, so 2 is given up and 3 (a
		// predicted frame) skipped, and a keyframe is needed.
		let (_, mut second) = fragments(2, 2 * wire::MAX_FRAGMENT, false);
		second.remove(1);
		assert!(feed(&mut frames, &second, now).is_empty());
		assert!(feed(&mut frames, &fragments(3, 500, false).1, now).is_empty());
		assert!(frames.need_keyframe);
		// Late fragments of the given-up frame are ignored.
		assert!(
			feed(
				&mut frames,
				&fragments(2, 2 * wire::MAX_FRAGMENT, false).1,
				now
			)
			.is_empty()
		);
		assert!(feed(&mut frames, &fragments(4, 500, false).1, now).is_empty());
		let (key, payloads) = fragments(5, 900, true);
		assert_eq!(feed(&mut frames, &payloads, now), [(key, true)]);
		assert!(!frames.need_keyframe);
		let (delta, payloads) = fragments(6, 100, false);
		assert_eq!(feed(&mut frames, &payloads, now), [(delta, false)]);

		// A frame that never completes is given up after the wait; one never seen at all
		// (11 vanished) is noticed when 12 completes.
		let (_, payloads) = fragments(7, 2 * wire::MAX_FRAGMENT, false);
		feed(&mut frames, &payloads[..1], now);
		frames.expire(now + FRAME_WAIT / 2);
		assert!(!frames.need_keyframe);
		frames.expire(now + FRAME_WAIT);
		assert!(frames.need_keyframe && frames.partial.is_empty());
		let (key, payloads) = fragments(10, 50, true);
		assert_eq!(feed(&mut frames, &payloads, now), [(key, true)]);
		assert!(feed(&mut frames, &fragments(12, 50, false).1, now).is_empty());
		assert!(frames.need_keyframe);
	}

	#[test]
	fn reassembly_bounds_frames_in_flight() {
		let now = Instant::now();
		let mut frames = Reassembly::default();
		for id in 1..=10 {
			let (_, payloads) = fragments(id, 2 * wire::MAX_FRAGMENT, true);
			feed(&mut frames, &payloads[..1], now);
			assert!(frames.partial.len() <= MAX_PARTIAL);
		}
		// A count disagreeing with the frame's first fragment is refused.
		let (_, payloads) = fragments(10, 3 * wire::MAX_FRAGMENT, true);
		assert!(feed(&mut frames, &payloads, now).is_empty());
	}

	#[test]
	fn viewer_waits_for_the_host_then_follows_its_state_and_sends_input() {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(2)
			.enable_all()
			.build()
			.unwrap();
		runtime.block_on(async {
			let (mut host, viewer) = super::super::tests::pair(90).await;
			let viewer = Viewer::start(viewer, &tokio::runtime::Handle::current(), || {}).unwrap();
			// Before the host's first state: not started, and input goes nowhere.
			viewer.send(&[Input::Wheel { dx: 0, dy: 120 }]);
			assert!(!viewer.status().started);
			let state = wire::State {
				permission: Some(Permission::Full),
				paused: false,
				audio: true,
				relative_mouse: false,
				screen: (2560, 1080),
				stream: (1920, 1080),
				content: [0, 135, 1920, 810],
			};
			host.shared.send(wire::STATE, &state.encode()).await;
			let deadline = Instant::now() + Duration::from_secs(5);
			while !viewer.status().started {
				assert!(Instant::now() < deadline);
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
			let status = viewer.status();
			assert_eq!(status.permission, Some(Permission::Full));
			assert_eq!(status.screen, (2560, 1080));
			assert_eq!(status.content, [0.0, 0.125, 1.0, 0.75]);

			let key = Input::Key {
				scan: 0x1e,
				extended: false,
				down: true,
			};
			viewer.send(&[key]);
			viewer.held(&[(0x1e, false)], &[]);
			let inbox = host.inbox.as_mut().unwrap();
			let mut order = wire::Order::default();
			let mut applied = Vec::new();
			let mut held = None;
			while held.is_none() {
				let (kind, payload) = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
					.await
					.unwrap()
					.unwrap();
				match kind {
					wire::INPUT => {
						let (first, batches) = wire::read_batches(&payload).unwrap();
						for (offset, inputs) in batches.into_iter().enumerate() {
							if order.take(first + offset as u32) {
								applied.extend(inputs);
							}
						}
					}
					wire::HELD => held = wire::Held::read(&payload),
					_ => {}
				}
			}
			// The pre-start wheel never left; the key arrived once despite its duplicate.
			assert_eq!(applied, [key]);
			assert_eq!(held.unwrap().keys, [(0x1e, false)]);
			viewer.end();
			assert_eq!(viewer.status().ended, Some(End::ByViewer));
			let deadline = Instant::now() + Duration::from_secs(5);
			while host.ended() != Some(End::ByViewer) {
				assert!(Instant::now() < deadline);
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		});
	}
}
