use super::{
	MAX_FRAME_HEIGHT, MAX_FRAME_WIDTH, MAX_RAW_BYTES, MAX_SOURCE_HEIGHT, MAX_SOURCE_WIDTH,
	MAX_SOURCES, bounded_name,
};
use crate::screen::{AudioChunk, RawFrame, Settings as CaptureSettings, Source, SourceId, gpu};
use std::sync::{
	Arc, Mutex,
	atomic::{AtomicBool, AtomicU64, Ordering},
	mpsc::SyncSender,
};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows_capture::{
	capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
	frame::Frame,
	graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl},
	monitor::Monitor,
	settings::{
		ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
		MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
	},
	window::Window,
};

#[path = "audio_windows.rs"]
mod audio;

fn monitor_id(monitor: &Monitor) -> u64 {
	u64::try_from(monitor.as_raw_hmonitor() as usize).unwrap_or(0)
}

fn window_id(window: &Window) -> u64 {
	u64::try_from(window.as_raw_hwnd() as usize).unwrap_or(0)
}

pub(crate) fn sources() -> Result<Vec<Source>, &'static str> {
	let mut sources = Vec::with_capacity(MAX_SOURCES);
	for monitor in Monitor::enumerate().map_err(|_| "Displays could not be enumerated")? {
		let (Ok(width), Ok(height)) = (monitor.width(), monitor.height()) else {
			continue;
		};
		if width == 0 || height == 0 || width > MAX_SOURCE_WIDTH || height > MAX_SOURCE_HEIGHT {
			continue;
		}
		let name = monitor
			.name()
			.or_else(|_| monitor.device_string())
			.unwrap_or_else(|_| "Display".to_owned());
		sources.push(Source {
			id: SourceId::Display(monitor_id(&monitor)),
			name: bounded_name(name),
		});
		if sources.len() == MAX_SOURCES {
			return Ok(sources);
		}
	}

	for window in Window::enumerate().map_err(|_| "Windows could not be enumerated")? {
		let Ok(title) = window.title() else {
			continue;
		};
		let (Ok(width), Ok(height)) = (window.width(), window.height()) else {
			continue;
		};
		let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
			continue;
		};
		if title.trim().is_empty()
			|| width == 0
			|| height == 0
			|| width > MAX_SOURCE_WIDTH
			|| height > MAX_SOURCE_HEIGHT
		{
			continue;
		}
		sources.push(Source {
			id: SourceId::Window(window_id(&window)),
			name: bounded_name(title),
		});
		if sources.len() == MAX_SOURCES {
			break;
		}
	}
	Ok(sources)
}

#[derive(Clone)]
struct Flags {
	frames: SyncSender<RawFrame>,
	stop: Arc<AtomicBool>,
	pending: Arc<AtomicBool>,
	next_frame: Instant,
	interval: Duration,
	/// Shared with the encoder: send GPU pictures while set.
	gpu: Arc<AtomicBool>,
	/// GPU pictures have no pixels for the encoder thread to preview, so capture publishes it.
	preview: Arc<Mutex<Option<image::RgbaImage>>>,
	next_preview: Instant,
	/// Stream picture size.
	output: (u32, u32),
}

struct Handler(Flags, Option<gpu::Converter>);

impl GraphicsCaptureApiHandler for Handler {
	type Flags = Flags;
	type Error = &'static str;

	fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
		Ok(Self(context.flags, None))
	}

	fn on_frame_arrived(
		&mut self,
		frame: &mut Frame,
		control: InternalCaptureControl,
	) -> Result<(), Self::Error> {
		if self.0.stop.load(Ordering::Acquire) {
			control.stop();
			return Ok(());
		}
		crate::stream_log::add(crate::stream_log::Count::Captured, 1);
		let now = Instant::now();
		if now + Duration::from_millis(2) < self.0.next_frame {
			return Ok(());
		}
		if self.0.pending.swap(true, Ordering::AcqRel) {
			crate::stream_log::add(crate::stream_log::Count::CaptureBusy, 1);
			return Ok(());
		}
		self.0.next_frame = (self.0.next_frame + self.0.interval).max(now + self.0.interval / 2);
		let (width, height) = (frame.width(), frame.height());
		let Some(row_bytes) = (width as usize).checked_mul(4) else {
			self.0.stop.store(true, Ordering::Release);
			control.stop();
			return Err("Captured frame dimensions are invalid");
		};
		let Some(data_len) = row_bytes.checked_mul(height as usize) else {
			self.0.stop.store(true, Ordering::Release);
			control.stop();
			return Err("Captured frame dimensions are invalid");
		};
		if !crate::screen::fits_capture(width, height) || data_len > MAX_RAW_BYTES {
			self.0.stop.store(true, Ordering::Release);
			control.stop();
			return Err("Captured frame exceeds the 4K limit");
		}
		if self.0.gpu.load(Ordering::Acquire) {
			match self.send_surface(frame) {
				Ok(sent) => {
					if !sent {
						self.0.pending.store(false, Ordering::Release);
					}
					return Ok(());
				}
				// The memory path below carries the stream from here on.
				Err(error) => {
					crate::stream_log::event(format_args!(
						"capture: {error}; copying pictures through memory"
					));
					self.0.gpu.store(false, Ordering::Release);
				}
			}
		}
		self.1 = None;
		let mut buffer = frame.buffer().map_err(|_| {
			self.0.stop.store(true, Ordering::Release);
			"Captured frame could not be read"
		})?;
		let stride = buffer.row_pitch() as usize;
		let Some(source_len) = stride.checked_mul(height as usize) else {
			self.0.stop.store(true, Ordering::Release);
			return Err("Captured frame stride is invalid");
		};
		if stride < row_bytes || source_len > MAX_RAW_BYTES {
			self.0.stop.store(true, Ordering::Release);
			return Err("Captured frame stride exceeds the 4K limit");
		}
		let source = buffer.as_raw_buffer();
		if source.len() < source_len {
			self.0.stop.store(true, Ordering::Release);
			return Err("Captured frame buffer is incomplete");
		}
		let mut data = Vec::with_capacity(data_len);
		for row in source[..source_len].chunks_exact(stride) {
			data.extend_from_slice(&row[..row_bytes]);
		}
		if self
			.0
			.frames
			.try_send(RawFrame {
				width,
				height,
				stride: row_bytes,
				data,
				surface: None,
			})
			.is_err()
		{
			self.0.pending.store(false, Ordering::Release);
		}
		Ok(())
	}

	fn on_closed(&mut self) -> Result<(), Self::Error> {
		self.0.stop.store(true, Ordering::Release);
		Ok(())
	}
}

impl Handler {
	/// Send `frame` scaled to NV12 in video memory. `Ok(false)` drops it while the encoder
	/// still holds every pooled picture.
	#[allow(unsafe_code)] // Rebinding windows-capture COM pointers.
	fn send_surface(&mut self, frame: &Frame) -> Result<bool, &'static str> {
		// SAFETY: windows-capture's objects are these same COM interfaces in its windows-rs.
		let (device, context, texture) = unsafe {
			(
				gpu::rebind::<_, ID3D11Device>(frame.device()),
				gpu::rebind::<_, ID3D11DeviceContext>(frame.device_context()),
				gpu::rebind::<_, ID3D11Texture2D>(frame.as_raw_texture()),
			)
		};
		let (Some(device), Some(context), Some(texture)) = (device, context, texture) else {
			return Err("Captured frame could not be bound");
		};
		let input = (frame.width(), frame.height());
		if self
			.1
			.as_ref()
			.is_none_or(|converter| converter.input() != input)
		{
			self.1 = None;
			self.1 = Some(gpu::Converter::new(device, context, input, self.0.output)?);
		}
		let converter = self.1.as_mut().expect("GPU screen converter");
		let now = Instant::now();
		if now >= self.0.next_preview {
			self.0.next_preview = now + Duration::from_millis(100);
			if let Some(image) = converter.preview(&texture)
				&& let Ok(mut slot) = self.0.preview.try_lock()
			{
				*slot = Some(image);
			}
		}
		let Some(surface) = converter.convert(&texture)? else {
			crate::stream_log::add(crate::stream_log::Count::CapturePool, 1);
			return Ok(false);
		};
		Ok(self
			.0
			.frames
			.try_send(RawFrame {
				width: self.0.output.0,
				height: self.0.output.1,
				stride: 0,
				data: Vec::new(),
				surface: Some(surface),
			})
			.is_ok())
	}
}

pub(crate) struct Capture {
	control: Option<CaptureControl<Handler, &'static str>>,
	audio: Option<audio::Audio>,
	/// Why system audio is off although the share asked for it, until it is reported.
	audio_warning: Option<&'static str>,
}

impl Capture {
	#[allow(clippy::too_many_arguments)] // Media outputs of one explicitly started capture.
	pub(crate) fn start(
		settings: CaptureSettings,
		frames: SyncSender<RawFrame>,
		audio: Option<tokio::sync::mpsc::Sender<AudioChunk>>,
		stop: Arc<AtomicBool>,
		ready: Arc<AtomicBool>,
		audio_epoch: Arc<AtomicU64>,
		pending: Arc<AtomicBool>,
		gpu: Arc<AtomicBool>,
		preview: Arc<Mutex<Option<image::RgbaImage>>>,
	) -> Result<Self, &'static str> {
		if settings.width == 0
			|| settings.height == 0
			|| settings.width > MAX_FRAME_WIDTH
			|| settings.height > MAX_FRAME_HEIGHT
			|| settings.fps == 0
		{
			return Err("Invalid screen capture settings");
		}
		let mut capture = match settings.source {
			SourceId::Display(id) => {
				let monitor = Monitor::enumerate()
					.map_err(|_| "Displays could not be enumerated")?
					.into_iter()
					.find(|monitor| monitor_id(monitor) == id)
					.ok_or("Selected display is no longer available")?;
				start_item(
					settings,
					monitor,
					frames,
					stop.clone(),
					pending,
					gpu,
					preview,
				)
			}
			SourceId::Window(id) => {
				let window = Window::enumerate()
					.map_err(|_| "Windows could not be enumerated")?
					.into_iter()
					.find(|window| window_id(window) == id)
					.ok_or("Selected window is no longer available")?;
				start_item(
					settings,
					window,
					frames,
					stop.clone(),
					pending,
					gpu,
					preview,
				)
			}
			#[allow(unreachable_patterns)] // Portal may be absent from platform-scoped models.
			_ => return Err("The desktop screen picker is available only on Linux"),
		}?;
		if let Some(send) = audio {
			// The video goes on without system audio rather than failing the whole share.
			match audio::Audio::start(send, ready, audio_epoch) {
				Ok(audio) => capture.audio = Some(audio),
				// ponytail: leaks one short string per share whose audio failed to start.
				Err(error) => {
					capture.audio_warning = Some(Box::leak(
						format!("Sharing without system audio: it could not start ({error})")
							.into_boxed_str(),
					))
				}
			}
		}
		Ok(capture)
	}

	/// Why system audio went off, once: it failed to start or its worker stopped.
	pub(crate) fn audio_lost(&mut self) -> Option<&'static str> {
		if self.audio.as_ref().is_some_and(audio::Audio::failed) {
			self.audio = None;
			return Some("System audio stopped; sharing continues without it");
		}
		self.audio_warning.take()
	}
}

trait NativeSource:
	TryInto<windows_capture::settings::GraphicsCaptureItemType> + Send + 'static
{
	fn dimensions(&self) -> Option<(u32, u32)>;
}

impl NativeSource for Monitor {
	fn dimensions(&self) -> Option<(u32, u32)> {
		self.width().ok().zip(self.height().ok())
	}
}

impl NativeSource for Window {
	fn dimensions(&self) -> Option<(u32, u32)> {
		self.width()
			.ok()
			.and_then(|width| u32::try_from(width).ok())
			.zip(
				self.height()
					.ok()
					.and_then(|height| u32::try_from(height).ok()),
			)
	}
}

fn start_item<T>(
	settings: CaptureSettings,
	item: T,
	frames: SyncSender<RawFrame>,
	stop: Arc<AtomicBool>,
	pending: Arc<AtomicBool>,
	gpu: Arc<AtomicBool>,
	preview: Arc<Mutex<Option<image::RgbaImage>>>,
) -> Result<Capture, &'static str>
where
	T: NativeSource,
{
	let Some((width, height)) = item.dimensions() else {
		return Err("Selected source dimensions are unavailable");
	};
	if !crate::screen::fits_capture(width, height) {
		return Err("Selected source has more pixels than the 4K capture limit");
	}
	let flags = Flags {
		frames,
		stop,
		pending,
		next_frame: Instant::now(),
		interval: Duration::from_secs_f64(1.0 / f64::from(settings.fps)),
		gpu,
		preview,
		next_preview: Instant::now(),
		output: (settings.width, settings.height),
	};
	let native = Settings::new(
		item,
		if settings.cursor {
			CursorCaptureSettings::Default
		} else {
			CursorCaptureSettings::WithoutCursor
		},
		// Hide the border where supported; older Windows must keep the system default.
		if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
			DrawBorderSettings::WithoutBorder
		} else {
			DrawBorderSettings::Default
		},
		SecondaryWindowSettings::Default,
		MinimumUpdateIntervalSettings::Default,
		DirtyRegionSettings::Default,
		ColorFormat::Bgra8,
		flags,
	);
	let control =
		Handler::start_free_threaded(native).map_err(|_| "Screen capture could not be started")?;
	Ok(Capture {
		control: Some(control),
		audio: None,
		audio_warning: None,
	})
}

impl Drop for Capture {
	fn drop(&mut self) {
		self.audio.take();
		if let Some(control) = self.control.take() {
			let _ = control.stop();
		}
	}
}
