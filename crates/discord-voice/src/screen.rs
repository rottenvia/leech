//! Explicitly selected, memory-only screen capture and H.264 encoding.
pub use client_core::screen::{Settings, Source, SourceId};
#[cfg(target_os = "linux")]
#[path = "screen/audio_linux.rs"]
mod audio_linux;
#[cfg(all(test, not(target_os = "windows")))]
#[path = "screen/audio_windows.rs"]
mod audio_windows;
#[cfg(not(target_os = "linux"))]
#[path = "screen/capture.rs"]
mod capture;
#[cfg(any(test, not(target_os = "linux")))]
#[path = "screen/convert.rs"]
mod convert;
#[cfg(target_os = "windows")]
#[path = "screen/gpu_windows.rs"]
mod gpu;
#[cfg(target_os = "linux")]
#[path = "screen/gstreamer.rs"]
mod gstreamer;
#[cfg(target_os = "linux")]
#[path = "screen/linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
#[path = "screen/portal_linux.rs"]
mod portal_linux;

use openh264::{
	OpenH264API,
	encoder::{
		BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
		RateControlMode, UsageType,
	},
	formats::{BgraSliceU8, YUVBuffer, YUVSource},
};
use std::sync::{
	Arc, Mutex, OnceLock,
	atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
	mpsc,
};

#[cfg(not(target_os = "linux"))]
use std::time::{Duration, Instant};

pub const MAX_RAW_BYTES: usize = 3840 * 2160 * 4;

/// Whether a captured picture fits the raw-frame budget. The bound is the byte count of one
/// 3840x2160 frame rather than each axis, so ultrawide (5120x1440) and portrait (2160x3840)
/// monitors pass with the same memory ceiling.
pub(crate) fn fits_capture(width: u32, height: u32) -> bool {
	width != 0
		&& height != 0
		&& width <= 7680
		&& height <= 7680
		&& u64::from(width) * u64::from(height) * 4 <= MAX_RAW_BYTES as u64
}
pub const MAX_ENCODED_BYTES: usize = 2 * 1024 * 1024;

/// Frames between scheduled IDR pictures: libwebrtc's H.264 default. Viewers that lose a
/// picture ask for one with PLI or FIR, and every DAVE transition (a viewer joining) already
/// forces one, so frequent periodic IDRs only add a large burst and a blurry frame each time.
const KEYFRAME_INTERVAL: u32 = 3000;

pub struct RawFrame {
	pub width: u32,
	pub height: u32,
	pub stride: usize,
	pub data: Vec<u8>,
	/// Windows GPU path: the picture already scaled to the stream size as NV12 in video
	/// memory, for the hardware encoder to read in place. `data` is then empty.
	#[cfg(target_os = "windows")]
	pub(crate) surface: Option<Arc<crate::video_encode::hardware::Surface>>,
}

pub struct EncodedFrame {
	pub data: Vec<u8>,
	pub timestamp: u32,
	pub keyframe: bool,
}

/// Longest system-audio chunk accepted from the OS: 100 ms of 48 kHz stereo.
pub const MAX_AUDIO_SAMPLES: usize = 4800 * 2;

#[derive(Debug)]
pub struct AudioChunk {
	pub samples: Vec<f32>,
	/// Capture generation; old buffers must not cross an encryption transition.
	pub epoch: u64,
}

pub struct Video {
	pub settings: Settings,
	pub frames: tokio::sync::mpsc::Receiver<EncodedFrame>,
	pub ready: Arc<AtomicBool>,
	pub keyframe: Arc<AtomicBool>,
	/// Transport feedback target in bits per second, bounded by the selected quality.
	pub bitrate: Arc<AtomicU32>,
	/// Interleaved 48 kHz stereo system audio, present only when the share requested it.
	pub audio: Option<tokio::sync::mpsc::Receiver<AudioChunk>>,
	pub audio_epoch: Arc<AtomicU64>,
}

/// Whether this platform can capture system audio with the screen.
pub fn audio_supported() -> bool {
	supported()
}

pub fn supported() -> bool {
	cfg!(any(
		target_os = "macos",
		target_os = "windows",
		target_os = "linux"
	))
}

pub fn sources() -> Result<Vec<Source>, &'static str> {
	#[cfg(target_os = "linux")]
	{
		let mut sources = vec![Source {
			id: SourceId::Portal,
			name: "Choose in the system picker".into(),
		}];
		if linux::x11_session() {
			sources.push(Source {
				id: SourceId::X11Desktop,
				name: "Entire X11 desktop · all monitors · no portal".into(),
			});
		}
		Ok(sources)
	}
	#[cfg(not(target_os = "linux"))]
	capture::sources()
}

pub struct Worker {
	stop: Arc<AtomicBool>,
	ready: Arc<AtomicBool>,
	preview: Arc<Mutex<Option<image::RgbaImage>>>,
	done: Option<mpsc::Receiver<Result<(), &'static str>>>,
	audio_warning: Arc<OnceLock<&'static str>>,
	#[cfg(target_os = "linux")]
	status: Arc<Mutex<&'static str>>,
	#[cfg(target_os = "linux")]
	preview_visible: Arc<AtomicBool>,
}

impl Worker {
	pub fn start(
		settings: Settings,
		wake: impl Fn() + Send + 'static,
	) -> Result<(Self, Video), &'static str> {
		if !settings.valid() || !supported() {
			return Err("Screen sharing is unavailable for these settings or this platform");
		}
		let stop = Arc::new(AtomicBool::new(false));
		let ready = Arc::new(AtomicBool::new(false));
		let audio_epoch = Arc::new(AtomicU64::new(0));
		let worker_audio_epoch = audio_epoch.clone();
		let keyframe = Arc::new(AtomicBool::new(true));
		let bitrate = Arc::new(AtomicU32::new(settings.bit_rate()));
		let worker_bitrate = bitrate.clone();
		let preview = Arc::new(Mutex::new(None));
		let worker_preview = preview.clone();
		let audio_warning = Arc::new(OnceLock::new());
		#[cfg(not(target_os = "linux"))]
		let worker_audio_warning = audio_warning.clone();
		#[cfg(target_os = "linux")]
		let status = Arc::new(Mutex::new("Choose a screen or window in the system picker"));
		#[cfg(target_os = "linux")]
		let worker_status = status.clone();
		#[cfg(target_os = "linux")]
		let preview_visible = Arc::new(AtomicBool::new(true));
		#[cfg(target_os = "linux")]
		let worker_preview_visible = preview_visible.clone();
		// A few frames of slack absorbs send jitter without forcing keyframes on every hiccup.
		// One encoded picture may wait while the pacer drains the last; more is only delay.
		let (send, frames) = tokio::sync::mpsc::channel(1);
		let (complete, done) = mpsc::sync_channel(1);
		let (audio_send, audio) = if settings.audio && audio_supported() {
			let (send, receive) = tokio::sync::mpsc::channel(4);
			(Some(send), Some(receive))
		} else {
			(None, None)
		};
		let (worker_stop, worker_ready, worker_keyframe) =
			(stop.clone(), ready.clone(), keyframe.clone());
		std::thread::Builder::new()
			.name("screen-encoder".into())
			.spawn(move || {
				// The MFT event poll and capture pacing sleep 1 ms at a time.
				let _resolution = crate::timer::Resolution::acquire();
				let finished_ready = worker_ready.clone();
				#[cfg(target_os = "linux")]
				let result = linux::run(
					settings,
					worker_stop,
					worker_ready,
					worker_keyframe,
					worker_bitrate,
					send,
					audio_send,
					worker_audio_epoch,
					worker_preview,
					worker_status,
					worker_preview_visible,
					&wake,
				);
				#[cfg(not(target_os = "linux"))]
				let result = encode_loop(
					settings,
					worker_stop,
					worker_ready,
					worker_keyframe,
					worker_bitrate,
					send,
					audio_send,
					worker_audio_epoch,
					worker_preview,
					worker_audio_warning,
					&wake,
				);
				finished_ready.store(false, Ordering::Release);
				// The desktop only shows the latest status, which a later stop overwrites.
				// Name the cause once so a share that ends by itself is never a mystery.
				if std::env::var_os("LEECH_VOICE_DIAGNOSTICS").is_some_and(|value| value == "1") {
					match &result {
						Ok(()) => eprintln!("[Leech voice Screen] capture_stopped=ok"),
						Err(reason) => eprintln!("[Leech voice Screen] capture_stopped={reason}"),
					}
				}
				let _ = complete.try_send(result);
				wake();
			})
			.map_err(|_| "Could not start screen capture worker")?;
		Ok((
			Self {
				stop,
				ready: ready.clone(),
				preview,
				done: Some(done),
				audio_warning,
				#[cfg(target_os = "linux")]
				status,
				#[cfg(target_os = "linux")]
				preview_visible,
			},
			Video {
				settings,
				frames,
				ready,
				keyframe,
				bitrate,
				audio,
				audio_epoch,
			},
		))
	}

	pub fn set_preview_visible(&self, _visible: bool) {
		#[cfg(target_os = "linux")]
		self.preview_visible.store(_visible, Ordering::Release);
	}

	pub fn capture_status(&self) -> Option<&'static str> {
		#[cfg(target_os = "linux")]
		return self.status.try_lock().ok().map(|status| *status);
		#[cfg(not(target_os = "linux"))]
		None
	}

	/// Why system audio is off although the share asked for it.
	pub fn audio_warning(&self) -> Option<&'static str> {
		self.audio_warning.get().copied()
	}

	pub fn result(&self) -> Option<Result<(), &'static str>> {
		self.done.as_ref()?.try_recv().ok()
	}

	/// One local RGBA image, bounded to 640 by 360 pixels and replaced at most ten times a second.
	pub fn take_preview(&self) -> Option<image::RgbaImage> {
		self.preview.try_lock().ok()?.take()
	}

	pub fn shutdown(mut self) -> mpsc::Receiver<Result<(), &'static str>> {
		self.done.take().expect("screen worker completion")
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		self.ready.store(false, Ordering::Release);
		self.stop.store(true, Ordering::Release);
	}
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::too_many_arguments)] // Media outputs of one explicitly started capture.
fn encode_loop(
	settings: Settings,
	stop: Arc<AtomicBool>,
	ready: Arc<AtomicBool>,
	keyframe: Arc<AtomicBool>,
	bitrate: Arc<AtomicU32>,
	send: tokio::sync::mpsc::Sender<EncodedFrame>,
	audio: Option<tokio::sync::mpsc::Sender<AudioChunk>>,
	audio_epoch: Arc<AtomicU64>,
	preview: Arc<Mutex<Option<image::RgbaImage>>>,
	#[cfg_attr(not(target_os = "windows"), allow(unused_variables))] audio_warning: Arc<
		OnceLock<&'static str>,
	>,
	wake: &impl Fn(),
) -> Result<(), &'static str> {
	if stop.load(Ordering::Acquire) || send.is_closed() {
		return Ok(());
	}
	let origin = Instant::now();
	let (raw_send, raw) = mpsc::sync_channel(1);
	let capture_stop = Arc::new(AtomicBool::new(false));
	#[cfg(target_os = "windows")]
	let raw_pending = Arc::new(AtomicBool::new(false));
	// Windows starts on the GPU path. The encoder clears this once it cannot read GPU
	// pictures, which moves capture back to frames in memory.
	#[cfg(target_os = "windows")]
	let gpu = Arc::new(AtomicBool::new(true));
	#[cfg(not(target_os = "windows"))]
	let gpu = ();
	#[cfg(target_os = "windows")]
	let mut _native = capture::Capture::start(
		settings,
		raw_send,
		audio,
		capture_stop.clone(),
		ready.clone(),
		audio_epoch,
		raw_pending.clone(),
		gpu.clone(),
		preview.clone(),
	)?;
	// The worker's stop also reaches a pending macOS picker, so call teardown closes it now.
	#[cfg(not(target_os = "windows"))]
	let _native = capture::Capture::start(
		settings,
		raw_send,
		audio,
		capture_stop.clone(),
		ready.clone(),
		audio_epoch,
		stop.clone(),
	)?;
	let mut encoding = None;
	let mut first_frame_deadline = Some(Instant::now() + Duration::from_secs(15));
	let mut next_frame = Instant::now();
	let mut next_preview = Instant::now();
	let mut latest_frame = None;

	while !stop.load(Ordering::Acquire) && !send.is_closed() {
		#[cfg(target_os = "windows")]
		if let Some(warning) = _native.audio_lost() {
			let _ = audio_warning.set(warning);
			wake();
		}
		if capture_stop.load(Ordering::Acquire) {
			return Err("The selected screen or window stopped sharing");
		}
		let frame = match raw.recv_timeout(Duration::from_millis(100)) {
			Ok(frame) => {
				#[cfg(target_os = "windows")]
				raw_pending.store(false, Ordering::Release);
				Some(frame)
			}
			Err(_) if stop.load(Ordering::Acquire) || send.is_closed() => break,
			Err(mpsc::RecvTimeoutError::Timeout)
				if first_frame_deadline.is_none_or(|deadline| Instant::now() < deadline) =>
			{
				None
			}
			Err(_) => {
				return Err(
					"No screen frames received; check screen recording permission and the selected source",
				);
			}
		};
		if stop.load(Ordering::Acquire) || send.is_closed() {
			break;
		}
		let now = Instant::now();
		if let Some(frame) = &frame {
			first_frame_deadline = None;
			if now >= next_preview {
				// A GPU picture has no pixels here; its capture thread publishes the preview.
				if !on_gpu(frame) {
					let image = preview_frame(frame)?;
					if let Ok(mut slot) = preview.try_lock() {
						*slot = Some(image);
					}
				}
				next_preview = now + Duration::from_millis(100);
				wake();
			}
		}
		let encode = retain_screen_frame(
			&mut latest_frame,
			frame,
			ready.load(Ordering::Acquire),
			keyframe.load(Ordering::Acquire),
		)?;
		// Local capture remains available while alone; only secure media is encoded or queued.
		if !ready.load(Ordering::Acquire) {
			encoding = None;
			keyframe.store(true, Ordering::Release);
			continue;
		}
		if !encode {
			continue;
		}
		// Pace on an accumulating schedule with tolerance: capture already paces, and its
		// delivery jitter must not drop frames here. A stalled encoder resumes from now
		// instead of bursting.
		let interval = Duration::from_secs_f64(1.0 / f64::from(settings.fps));
		if now + interval / 4 < next_frame {
			continue;
		}
		next_frame = (next_frame + interval).max(now + interval / 2);
		// Drop before encoding while the transport is behind, so the encoder never references
		// a picture the receiver did not get.
		if send.capacity() == 0 {
			crate::stream_log::add(crate::stream_log::Count::Behind, 1);
			continue;
		}
		let target = bitrate
			.load(Ordering::Acquire)
			.clamp(250_000, settings.bit_rate());
		if encoding.is_none() {
			encoding = Some(ScreenEncoder::new(settings, target, gpu.clone())?);
		}
		if encoding
			.as_mut()
			.expect("secure screen encoder")
			.set_bitrate(target)?
		{
			keyframe.store(true, Ordering::Release);
		}
		// Retain one current source snapshot, never encoded media, for a viewer's keyframe
		// request on an unchanged desktop. VideoToolbox takes packed BGRA at the stream size
		// (ScreenCaptureKit already scales); other encoders convert the source in one pass.
		#[cfg(target_os = "macos")]
		{
			let pixels = fit_frame(
				latest_frame.take().expect("latest screen frame"),
				settings.width,
				settings.height,
			)?;
			latest_frame = Some(RawFrame {
				width: settings.width,
				height: settings.height,
				stride: settings.width as usize * 4,
				data: pixels,
			});
		}
		let force_keyframe = keyframe.swap(false, Ordering::AcqRel);
		let started = Instant::now();
		let (data, is_keyframe) = encoding.as_mut().expect("secure screen encoder").encode(
			latest_frame.as_ref().expect("latest screen frame"),
			force_keyframe,
		)?;
		if !data.is_empty() {
			use crate::stream_log::{Count, add, max};
			let micros = started.elapsed().as_micros() as u64;
			add(Count::Encoded, 1);
			add(Count::EncodeMicros, micros);
			max(Count::EncodeMaxMicros, micros);
			add(Count::Keyframes, u64::from(is_keyframe));
		}
		if force_keyframe && (data.is_empty() || !is_keyframe) {
			keyframe.store(true, Ordering::Release);
		}
		if data.is_empty() {
			continue;
		}
		if !ready.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
			continue;
		}
		let frame = EncodedFrame {
			data,
			// When the picture arrived, not when encoding finished: encoder time must not
			// jitter the receiver's playout.
			timestamp: (now.duration_since(origin).as_micros() * 90 / 1000) as u32,
			keyframe: is_keyframe,
		};
		if send.try_send(frame).is_err() {
			keyframe.store(true, Ordering::Release);
		}
	}
	capture_stop.store(true, Ordering::Release);
	Ok(())
}

#[cfg(any(test, not(target_os = "linux")))]
fn retain_screen_frame(
	latest: &mut Option<RawFrame>,
	frame: Option<RawFrame>,
	ready: bool,
	keyframe: bool,
) -> Result<bool, &'static str> {
	let fresh = frame.is_some();
	if let Some(frame) = frame {
		if !on_gpu(&frame) {
			validate_frame(&frame)?;
		}
		*latest = Some(frame);
	}
	Ok(ready && latest.is_some() && (fresh || keyframe))
}

/// Whether `frame` is a Windows GPU picture rather than pixels in memory.
#[cfg(any(test, not(target_os = "linux")))]
fn on_gpu(frame: &RawFrame) -> bool {
	#[cfg(target_os = "windows")]
	return frame.surface.is_some();
	#[cfg(not(target_os = "windows"))]
	{
		let _ = frame;
		false
	}
}

/// Windows: cleared once the encoder cannot read GPU pictures, which moves capture back to
/// frames in memory. macOS has no GPU path.
#[cfg(target_os = "windows")]
type GpuPath = Arc<AtomicBool>;
#[cfg(target_os = "macos")]
type GpuPath = ();

/// Screen encoder preferring the platform hardware H.264 encoder (Media Foundation on
/// Windows, VideoToolbox on macOS) and falling back to openh264 when it is unavailable or
/// fails mid-stream. On Windows the hardware encoder reads GPU pictures in place while
/// capture sends them.
#[cfg(not(target_os = "linux"))]
struct ScreenEncoder {
	diagnostics: crate::diagnostics::EncoderRegistration,
	software: Option<Encoder>,
	/// Reused I420 picture for openh264.
	i420: Vec<u8>,
	hardware: Option<crate::video_encode::hardware::Encoder>,
	settings: Settings,
	bitrate: u32,
	#[cfg_attr(target_os = "macos", allow(dead_code))]
	gpu: GpuPath,
	/// The capture device whose surfaces the hardware encoder reads, when it reads them.
	#[cfg(target_os = "windows")]
	device: Option<windows::Win32::Graphics::Direct3D11::ID3D11Device>,
	/// Recent failures of the GPU encoder. A game can stall one picture; only repeated
	/// failures move the stream to pictures in memory, which cost the CPU far more.
	#[cfg(target_os = "windows")]
	gpu_failures: Vec<Instant>,
}

#[cfg(not(target_os = "linux"))]
impl ScreenEncoder {
	fn new(settings: Settings, bitrate: u32, gpu: GpuPath) -> Result<Self, &'static str> {
		let mut encoder = Self {
			diagnostics: crate::diagnostics::EncoderRegistration::new(true, false),
			software: None,
			i420: Vec::new(),
			hardware: None,
			settings,
			bitrate,
			gpu,
			#[cfg(target_os = "windows")]
			device: None,
			#[cfg(target_os = "windows")]
			gpu_failures: Vec::new(),
		};
		// GPU pictures open their encoder on the first surface, for that surface's device.
		#[cfg(target_os = "windows")]
		if encoder.gpu.load(Ordering::Acquire) {
			encoder.diagnostics.set(None);
			return Ok(encoder);
		}
		encoder.open()?;
		Ok(encoder)
	}

	fn config(&self) -> crate::video_encode::Config {
		crate::video_encode::Config {
			width: self.settings.width,
			height: self.settings.height,
			fps: self.settings.fps,
			bit_rate: self.bitrate,
			max_bytes: MAX_ENCODED_BYTES,
			keyframe_interval: KEYFRAME_INTERVAL,
			profile: crate::video_encode::Profile::Main,
		}
	}

	/// Open an encoder for pictures in memory: hardware when available, else openh264.
	fn open(&mut self) -> Result<(), &'static str> {
		// Release a scarce hardware session before requesting its replacement.
		self.hardware = None;
		#[cfg(target_os = "windows")]
		{
			self.device = None;
		}
		#[cfg(target_os = "macos")]
		let hardware = crate::video_encode::hardware::Encoder::new(
			self.config(),
			crate::video_encode::SourceFormat::Bgra,
		)
		.ok();
		#[cfg(target_os = "windows")]
		let hardware = crate::video_encode::hardware::Encoder::new(self.config()).ok();
		self.software = match hardware {
			Some(_) => None,
			None => Some(encoder(self.settings, self.bitrate)?),
		};
		crate::stream_log::event(format_args!(
			"encoder: {} for pictures in memory",
			hardware
				.as_ref()
				.map_or("OpenH264 (software)", |hardware| hardware.name()),
		));
		self.diagnostics.set(Some(hardware.is_some()));
		self.hardware = hardware;
		Ok(())
	}

	fn set_bitrate(&mut self, bitrate: u32) -> Result<bool, &'static str> {
		if self.bitrate == bitrate {
			return Ok(false);
		}
		if self
			.hardware
			.as_mut()
			.is_some_and(|encoder| encoder.set_bitrate(bitrate).is_ok())
		{
			self.bitrate = bitrate;
			return Ok(false);
		}
		if self.software.is_some() {
			// OpenH264 exposes no safe runtime bitrate setter, and a restart costs an IDR:
			// follow only large moves so gradual recovery cannot cause a keyframe per second.
			if !software_rate_change(self.bitrate, bitrate) {
				return Ok(false);
			}
			self.software = Some(encoder(self.settings, bitrate)?);
			self.bitrate = bitrate;
			return Ok(true);
		}
		self.bitrate = bitrate;
		// A GPU encoder (re)opens at the new rate on its next surface.
		#[cfg(target_os = "windows")]
		if self.hardware.is_none() || self.device.is_some() {
			self.device = None;
			self.diagnostics.set(None);
			return Ok(self.hardware.take().is_some());
		}
		// Reopen native encoders that refuse a live rate change at the target rate.
		self.open()?;
		Ok(true)
	}

	fn encode(
		&mut self,
		frame: &RawFrame,
		force_keyframe: bool,
	) -> Result<(Vec<u8>, bool), &'static str> {
		#[allow(unused_mut)] // Only Windows switches encoders between GPU and memory pictures.
		let mut force_keyframe = force_keyframe;
		#[cfg(target_os = "windows")]
		{
			if let Some(surface) = &frame.surface {
				return Ok(self.encode_surface(surface, force_keyframe));
			}
			// Capture left the GPU path, or never took it: encode from memory instead.
			if self.device.is_some() || (self.hardware.is_none() && self.software.is_none()) {
				self.open()?;
				force_keyframe = true;
			}
		}
		let (width, height) = (self.settings.width as usize, self.settings.height as usize);
		let mut software_force = force_keyframe;
		if let Some(hardware) = self.hardware.as_mut() {
			#[cfg(target_os = "macos")]
			let encoded = hardware.encode(&frame.data, (width, height), force_keyframe);
			#[cfg(target_os = "windows")]
			let encoded = hardware.encode_with(width * height * 3 / 2, force_keyframe, |picture| {
				convert::bgra_to_yuv420(frame, width, height, convert::Chroma::Interleaved, picture)
			});
			if let Ok(encoded) = encoded {
				return Ok(encoded);
			}
			// The viewer must restart from a keyframe once the software encoder takes over.
			crate::stream_log::event(
				"encoder: the hardware encoder failed; OpenH264 (software) from here",
			);
			self.hardware = None;
			self.diagnostics.set(None);
			self.software = Some(encoder(self.settings, self.bitrate)?);
			self.diagnostics.set(Some(false));
			software_force = true;
		}
		self.i420.resize(width * height * 3 / 2, 0);
		convert::bgra_to_yuv420(
			frame,
			width,
			height,
			convert::Chroma::Planar,
			&mut self.i420,
		)?;
		let (y, chroma) = self.i420.split_at(width * height);
		let (u, v) = chroma.split_at(width * height / 4);
		encode_yuv(
			self.software.as_mut().expect("software screen encoder"),
			&openh264::formats::YUVSlices::new(
				(y, u, v),
				(width, height),
				(width, width / 2, width / 2),
			),
			software_force,
		)
	}

	/// Encode a GPU picture in place. When the GPU path cannot encode, this clears it and
	/// returns nothing, leaving the keyframe request for capture's first frame in memory.
	#[cfg(target_os = "windows")]
	fn encode_surface(
		&mut self,
		surface: &crate::video_encode::hardware::Surface,
		force_keyframe: bool,
	) -> (Vec<u8>, bool) {
		if !self.gpu.load(Ordering::Acquire) {
			return (Vec::new(), false);
		}
		let mut force_keyframe = force_keyframe;
		if self.device.as_ref() != Some(&surface.device) {
			self.hardware = None;
			self.software = None;
			match crate::video_encode::hardware::Encoder::with_device(
				self.config(),
				&surface.device,
			) {
				Ok(encoder) => {
					crate::stream_log::event(format_args!(
						"encoder: {} reading GPU pictures",
						encoder.name()
					));
					self.hardware = Some(encoder);
					self.device = Some(surface.device.clone());
					self.diagnostics.set(Some(true));
					force_keyframe = true;
				}
				Err(_) => {
					crate::stream_log::event(
						"encoder: no hardware encoder reads this GPU's pictures; copying them through memory",
					);
					self.gpu.store(false, Ordering::Release);
					return (Vec::new(), false);
				}
			}
		}
		match self
			.hardware
			.as_mut()
			.map(|encoder| encoder.encode_surface(surface, force_keyframe))
		{
			Some(Ok(encoded)) => encoded,
			_ => {
				// Reopen on the next picture, with a keyframe. Only a third failure within a
				// minute gives up the GPU path.
				self.hardware = None;
				self.device = None;
				self.diagnostics.set(None);
				let now = Instant::now();
				self.gpu_failures
					.retain(|at| now.duration_since(*at) < Duration::from_secs(60));
				self.gpu_failures.push(now);
				if self.gpu_failures.len() >= 3 {
					crate::stream_log::event(
						"encoder: GPU encoding failed 3 times in a minute; copying pictures through memory",
					);
					self.gpu.store(false, Ordering::Release);
				} else {
					crate::stream_log::event("encoder: GPU encoding failed; reopening it");
				}
				(Vec::new(), false)
			}
		}
	}
}

/// Whether a software encoder should restart for a new transport target.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn software_rate_change(current: u32, target: u32) -> bool {
	// Always honor congestion cuts of at least 15%; grow in 25% steps.
	u64::from(target) * 100 <= u64::from(current) * 85
		|| u64::from(target) * 100 >= u64::from(current) * 125
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn i420_to_nv12(
	y: &[u8],
	u: &[u8],
	v: &[u8],
	output: &mut [u8],
) -> Result<(), &'static str> {
	if u.len() != v.len() || y.len() != u.len() * 4 || output.len() != y.len() + u.len() + v.len() {
		return Err("Invalid screen encoder color planes");
	}
	output[..y.len()].copy_from_slice(y);
	for (pair, (&u, &v)) in output[y.len()..]
		.as_chunks_mut::<2>()
		.0
		.iter_mut()
		.zip(u.iter().zip(v))
	{
		pair.copy_from_slice(&[u, v]);
	}
	Ok(())
}

pub(super) fn encoder(settings: Settings, bitrate: u32) -> Result<Encoder, &'static str> {
	let config = EncoderConfig::new()
		.bitrate(BitRate::from_bps(
			bitrate.clamp(250_000, settings.bit_rate()),
		))
		.max_frame_rate(FrameRate::from_hz(settings.fps as f32))
		.usage_type(UsageType::ScreenContentRealTime)
		.rate_control_mode(RateControlMode::Bitrate)
		.complexity(if cfg!(target_os = "windows") {
			Complexity::Low
		} else {
			Complexity::Medium
		})
		.num_threads(encoder_threads())
		.intra_frame_period(IntraFramePeriod::from_num_frames(KEYFRAME_INTERVAL));
	Encoder::with_api_config(OpenH264API::from_source(), config)
		.map_err(|_| "Screen video encoder is unavailable")
}

fn encoder_threads() -> u16 {
	std::thread::available_parallelism().map_or(2, |count| count.get().clamp(2, 8) as u16)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn encode_pixels(
	encoder: &mut Encoder,
	yuv: &mut YUVBuffer,
	pixels: &[u8],
	dimensions: (usize, usize),
	force_keyframe: bool,
) -> Result<(Vec<u8>, bool), &'static str> {
	yuv.read_bgra8(BgraSliceU8::new(pixels, dimensions));
	encode_yuv(encoder, yuv, force_keyframe)
}

fn encode_yuv(
	encoder: &mut Encoder,
	yuv: &impl YUVSource,
	force_keyframe: bool,
) -> Result<(Vec<u8>, bool), &'static str> {
	if force_keyframe {
		encoder.force_intra_frame();
	}
	let encoded = encoder
		.encode(yuv)
		.map_err(|_| "Screen video encoding failed")?;
	let is_keyframe = matches!(encoded.frame_type(), FrameType::IDR);
	let mut encoded_len = 0usize;
	for layer_index in 0..encoded.num_layers() {
		let layer = encoded
			.layer(layer_index)
			.ok_or("Screen video encoder returned an invalid layer")?;
		for nal_index in 0..layer.nal_count() {
			encoded_len = encoded_len
				.checked_add(
					layer
						.nal_unit(nal_index)
						.ok_or("Screen video encoder returned an invalid NAL")?
						.len(),
				)
				.filter(|length| *length <= MAX_ENCODED_BYTES)
				.ok_or("Encoded screen frame exceeds the sharing limit; choose a lower quality")?;
		}
	}
	let mut data = Vec::with_capacity(encoded_len);
	encoded.write_vec(&mut data);
	Ok((data, is_keyframe))
}

fn validate_frame(frame: &RawFrame) -> Result<(usize, usize), &'static str> {
	let row_bytes = (frame.width as usize)
		.checked_mul(4)
		.ok_or("Screen capture returned an unsupported frame size")?;
	let required = frame
		.stride
		.checked_mul(frame.height as usize)
		.ok_or("Screen capture returned an unsupported frame size")?;
	if !fits_capture(frame.width, frame.height)
		|| frame.data.len() > MAX_RAW_BYTES
		|| frame.stride < row_bytes
		|| required > frame.data.len()
	{
		return Err("Screen capture returned an unsupported frame size");
	}

	Ok((row_bytes, required))
}

pub(super) fn preview_frame(frame: &RawFrame) -> Result<image::RgbaImage, &'static str> {
	validate_frame(frame)?;
	let scale = (640.0 / f64::from(frame.width))
		.min(360.0 / f64::from(frame.height))
		.min(1.0);
	let width = (f64::from(frame.width) * scale).round().max(1.0) as u32;
	let height = (f64::from(frame.height) * scale).round().max(1.0) as u32;
	// ponytail: nearest sampling keeps the ten-fps preview cheap; use filtered scaling if needed.
	Ok(image::RgbaImage::from_fn(width, height, |x, y| {
		let source_x = (x * frame.width / width) as usize;
		let source_y = (y * frame.height / height) as usize;
		let offset = source_y * frame.stride + source_x * 4;
		image::Rgba([
			frame.data[offset + 2],
			frame.data[offset + 1],
			frame.data[offset],
			255,
		])
	}))
}

#[cfg(any(test, target_os = "macos"))]
fn fit_frame(frame: RawFrame, width: u32, height: u32) -> Result<Vec<u8>, &'static str> {
	let (row_bytes, required) = validate_frame(&frame)?;

	let mut packed = if frame.stride == row_bytes {
		frame.data
	} else {
		let mut packed = Vec::with_capacity(row_bytes * frame.height as usize);
		for row in frame.data[..required].chunks_exact(frame.stride) {
			packed.extend_from_slice(&row[..row_bytes]);
		}
		packed
	};
	packed.truncate(row_bytes * frame.height as usize);
	if frame.width == width && frame.height == height {
		return Ok(packed);
	}
	let image = image::RgbaImage::from_raw(frame.width, frame.height, packed)
		.ok_or("Invalid screen frame")?;
	let scale = (f64::from(width) / f64::from(frame.width))
		.min(f64::from(height) / f64::from(frame.height));
	let (scaled_width, scaled_height) = (
		(f64::from(frame.width) * scale).round().max(1.0) as u32,
		(f64::from(frame.height) * scale).round().max(1.0) as u32,
	);
	let scaled = image::imageops::resize(
		&image,
		scaled_width,
		scaled_height,
		image::imageops::FilterType::Triangle,
	);
	let mut output = image::RgbaImage::new(width, height);
	image::imageops::replace(
		&mut output,
		&scaled,
		i64::from((width - scaled_width) / 2),
		i64::from((height - scaled_height) / 2),
	);
	Ok(output.into_raw())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn capture_limit_is_the_4k_pixel_budget_not_each_axis() {
		assert!(fits_capture(3840, 2160));
		assert!(fits_capture(5120, 1440));
		assert!(fits_capture(2160, 3840));
		assert!(!fits_capture(3840, 2400));
		assert!(!fits_capture(5120, 2880));
		assert!(!fits_capture(0, 1080));
		assert!(!fits_capture(7681, 1));
	}

	#[test]
	fn idle_screen_keyframe_uses_latest_snapshot_only_when_ready() {
		let frame = |value| RawFrame {
			width: 2,
			height: 2,
			stride: 8,
			data: vec![value; 16],
			#[cfg(target_os = "windows")]
			surface: None,
		};
		let mut latest = None;
		assert!(!retain_screen_frame(&mut latest, None, true, true).unwrap());
		assert!(retain_screen_frame(&mut latest, Some(frame(1)), true, false).unwrap());
		assert!(!retain_screen_frame(&mut latest, None, true, false).unwrap());
		assert!(retain_screen_frame(&mut latest, None, true, true).unwrap());
		// A security pause keeps tracking the source without encoding. Its newest snapshot
		// can be freshly encoded after readiness, even if no further capture event arrives.
		assert!(!retain_screen_frame(&mut latest, Some(frame(2)), false, true).unwrap());
		assert!(!retain_screen_frame(&mut latest, None, false, true).unwrap());
		assert!(retain_screen_frame(&mut latest, None, true, true).unwrap());
		assert_eq!(latest.as_ref().unwrap().data, vec![2; 16]);
		let mut oversized = frame(3);
		oversized.width = 3841;
		assert!(retain_screen_frame(&mut latest, Some(oversized), true, true).is_err());
		assert_eq!(latest.unwrap().data, vec![2; 16]);
	}

	#[test]
	fn local_preview_is_bounded_and_converts_padded_bgra_without_media_readiness() {
		let preview = preview_frame(&RawFrame {
			width: 1,
			height: 2,
			stride: 8,
			data: vec![1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0, 9, 9, 9, 9],
			#[cfg(target_os = "windows")]
			surface: None,
		})
		.unwrap();
		assert_eq!(preview.as_raw(), &[3, 2, 1, 255, 6, 5, 4, 255]);
		let worker = Worker {
			stop: Arc::new(AtomicBool::new(false)),
			ready: Arc::new(AtomicBool::new(false)),
			preview: Arc::new(Mutex::new(Some(preview))),
			done: None,
			audio_warning: Arc::default(),
			#[cfg(target_os = "linux")]
			status: Arc::new(Mutex::new("")),
			#[cfg(target_os = "linux")]
			preview_visible: Arc::new(AtomicBool::new(true)),
		};
		assert!(worker.take_preview().is_some());
		assert!(worker.take_preview().is_none());
		assert!(!worker.ready.load(Ordering::Acquire));
		for (width, height) in [(3840, 2160), (2, 2160), (3840, 2)] {
			let frame = RawFrame {
				width,
				height,
				stride: width as usize * 4,
				data: vec![0; width as usize * height as usize * 4],
				#[cfg(target_os = "windows")]
				surface: None,
			};
			let preview = preview_frame(&frame).unwrap();
			assert!(preview.width() > 0 && preview.width() <= 640);
			assert!(preview.height() > 0 && preview.height() <= 360);
			assert!(preview.as_raw().len() <= 640 * 360 * 4);
		}
		assert!(
			preview_frame(&RawFrame {
				width: 2,
				height: 2,
				stride: 4,
				data: vec![0; 8],
				#[cfg(target_os = "windows")]
				surface: None,
			})
			.is_err()
		);
	}

	#[test]
	fn synthetic_frame_is_bounded_and_encodes_a_keyframe() {
		let frame = RawFrame {
			width: 2,
			height: 2,
			stride: 12,
			data: vec![255; 24],
			#[cfg(target_os = "windows")]
			surface: None,
		};
		let pixels = fit_frame(frame, 1280, 720).unwrap();
		assert_eq!(pixels.len(), 1280 * 720 * 4);

		let settings = Settings {
			source: SourceId::Display(1),
			width: 1280,
			height: 720,
			fps: 30,
			cursor: true,
			audio: false,
		};
		let mut encoder = encoder(settings, settings.bit_rate()).unwrap();
		let mut yuv = YUVBuffer::new(1280, 720);
		let (encoded, keyframe) =
			encode_pixels(&mut encoder, &mut yuv, &pixels, (1280, 720), true).unwrap();
		assert!(keyframe);
		assert!(!encoded.is_empty() && encoded.len() <= MAX_ENCODED_BYTES);
		assert!(encoded.windows(5).any(|nal| nal == [0, 0, 0, 1, 0x65]));

		assert!(
			fit_frame(
				RawFrame {
					width: 2,
					height: 2,
					stride: 4,
					data: vec![0; 8],
					#[cfg(target_os = "windows")]
					surface: None,
				},
				1280,
				720,
			)
			.is_err()
		);
	}

	/// Shares the first display for six seconds with no network, consuming encoded frames as
	/// the transport would, and prints the stream log it wrote. Pictures stay in memory.
	/// `cargo test -p discord-voice compare_screen_share_log -- --ignored --nocapture`
	#[cfg(target_os = "windows")]
	#[test]
	#[ignore]
	fn compare_screen_share_log() {
		let directory =
			std::env::temp_dir().join(format!("leech-share-log-{}", std::process::id()));
		crate::stream_log::set_directory(directory.clone());
		let source = sources()
			.unwrap()
			.into_iter()
			.find(|source| matches!(source.id, SourceId::Display(_)))
			.unwrap();
		let settings = Settings {
			source: source.id,
			width: 1920,
			height: 1080,
			fps: 60,
			cursor: true,
			audio: false,
		};
		let session = crate::stream_log::Session::sharing(settings);
		let (worker, mut video) = Worker::start(settings, || {}).unwrap();
		video.ready.store(true, Ordering::Release);
		let end = Instant::now() + Duration::from_secs(6);
		while Instant::now() < end {
			match video.frames.try_recv() {
				Ok(frame) => {
					crate::stream_log::add(crate::stream_log::Count::SentFrames, 1);
					crate::stream_log::add(
						crate::stream_log::Count::SentBytes,
						frame.data.len() as u64,
					);
				}
				Err(_) => std::thread::sleep(Duration::from_millis(1)),
			}
		}
		drop(worker);
		drop(session);
		std::thread::sleep(Duration::from_millis(1500));
		let log = std::fs::read_to_string(directory.join("stream.log")).unwrap();
		eprintln!("{log}");
		let _ = std::fs::remove_dir_all(&directory);
		assert!(log.contains("=== sharing 1920x1080 at 60 fps"));
		assert!(log.contains("encoder: "));
		assert!(
			log.lines()
				.filter(|line| line.contains(" share capture="))
				.count() >= 4
		);
	}

	#[test]
	fn interleaves_i420_chroma_for_windows_nv12() {
		let mut nv12 = [0; 12];
		i420_to_nv12(&[1, 2, 3, 4, 5, 6, 7, 8], &[9, 10], &[11, 12], &mut nv12).unwrap();
		assert_eq!(nv12, [1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 10, 12]);
		assert!(i420_to_nv12(&[0; 4], &[0; 2], &[0; 2], &mut [0; 8]).is_err());
	}
}
