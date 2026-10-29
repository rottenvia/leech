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
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Gdi::{
	GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::{
	GA_ROOT, GetAncestor, GetClassNameW, GetWindowRect, IsIconic, IsWindow, WindowFromPoint,
};
use windows_capture::{
	capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
	dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat, Error as DuplicationError},
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

/// Pacing, conversion and hand-off to the encoder, shared by window capture and display
/// duplication. Each backend owns one, with a converter on its own device.
struct Sink(Flags, Option<gpu::Converter>);

impl Sink {
	fn new(mut flags: Flags) -> Self {
		flags.next_frame = Instant::now();
		Self(flags, None)
	}

	/// Whether a picture arriving now is delivered; capture pacing and the encoder's single
	/// picture in flight drop the rest.
	fn admit(&mut self) -> bool {
		crate::stream_log::add(crate::stream_log::Count::Captured, 1);
		let now = Instant::now();
		if now + Duration::from_millis(2) < self.0.next_frame {
			return false;
		}
		if self.0.pending.swap(true, Ordering::AcqRel) {
			crate::stream_log::add(crate::stream_log::Count::CaptureBusy, 1);
			return false;
		}
		self.0.next_frame = (self.0.next_frame + self.0.interval).max(now + self.0.interval / 2);
		true
	}

	/// Stops the share on a picture outside the capture bounds.
	fn check(&self, width: u32, height: u32) -> Result<(), &'static str> {
		let fits = (width as usize)
			.checked_mul(4)
			.and_then(|row| row.checked_mul(height as usize))
			.is_some_and(|len| len <= MAX_RAW_BYTES);
		if fits && crate::screen::fits_capture(width, height) {
			return Ok(());
		}
		self.0.stop.store(true, Ordering::Release);
		Err("Captured frame exceeds the 4K limit")
	}

	/// Sends a GPU picture while the encoder reads them. `false` means this picture still needs
	/// the memory path: GPU pictures are off, or just failed and are off from now on.
	fn try_surface(
		&mut self,
		bound: Option<(ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D)>,
		input: (u32, u32),
	) -> bool {
		if !self.0.gpu.load(Ordering::Acquire) {
			return false;
		}
		let result = bound.ok_or("Captured frame could not be bound").and_then(
			|(device, context, texture)| self.send_surface(device, context, &texture, input),
		);
		match result {
			Ok(sent) => {
				if !sent {
					self.0.pending.store(false, Ordering::Release);
				}
				true
			}
			// The memory path carries the stream from here on.
			Err(error) => {
				crate::stream_log::event(format_args!(
					"capture: {error}; copying pictures through memory"
				));
				self.0.gpu.store(false, Ordering::Release);
				false
			}
		}
	}

	/// Send the picture scaled to NV12 in video memory. `Ok(false)` drops it while the encoder
	/// still holds every pooled picture.
	fn send_surface(
		&mut self,
		device: ID3D11Device,
		context: ID3D11DeviceContext,
		texture: &ID3D11Texture2D,
		input: (u32, u32),
	) -> Result<bool, &'static str> {
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
			if let Some(image) = converter.preview(texture)
				&& let Ok(mut slot) = self.0.preview.try_lock()
			{
				*slot = Some(image);
			}
		}
		let Some(surface) = converter.convert(texture)? else {
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

	/// Copies a mapped BGRA picture into memory for the encoder.
	fn send_pixels(
		&mut self,
		(width, height): (u32, u32),
		stride: usize,
		source: &[u8],
	) -> Result<(), &'static str> {
		self.1 = None;
		let row_bytes = width as usize * 4;
		let Some(source_len) = stride.checked_mul(height as usize) else {
			self.0.stop.store(true, Ordering::Release);
			return Err("Captured frame stride is invalid");
		};
		if stride < row_bytes || source_len > MAX_RAW_BYTES {
			self.0.stop.store(true, Ordering::Release);
			return Err("Captured frame stride exceeds the 4K limit");
		}
		if source.len() < source_len {
			self.0.stop.store(true, Ordering::Release);
			return Err("Captured frame buffer is incomplete");
		}
		let mut data = Vec::with_capacity(row_bytes * height as usize);
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
}

/// Rebinds windows-capture's COM objects (its own windows-rs version) to ours.
#[allow(unsafe_code)]
fn bind<D, C, T>(
	device: &D,
	context: &C,
	texture: &T,
) -> Option<(ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D)> {
	// SAFETY: windows-capture's objects are these same COM interfaces in its windows-rs.
	unsafe {
		Some((
			gpu::rebind(device)?,
			gpu::rebind(context)?,
			gpu::rebind(texture)?,
		))
	}
}

struct Handler(Sink);

impl GraphicsCaptureApiHandler for Handler {
	type Flags = Flags;
	type Error = &'static str;

	fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
		Ok(Self(Sink::new(context.flags)))
	}

	fn on_frame_arrived(
		&mut self,
		frame: &mut Frame,
		control: InternalCaptureControl,
	) -> Result<(), Self::Error> {
		let sink = &mut self.0;
		if sink.0.stop.load(Ordering::Acquire) {
			control.stop();
			return Ok(());
		}
		if !sink.admit() {
			return Ok(());
		}
		let size = (frame.width(), frame.height());
		if let Err(error) = sink.check(size.0, size.1) {
			control.stop();
			return Err(error);
		}
		let bound = bind(
			frame.device(),
			frame.device_context(),
			frame.as_raw_texture(),
		);
		if sink.try_surface(bound, size) {
			return Ok(());
		}
		let mut buffer = frame.buffer().map_err(|_| {
			sink.0.stop.store(true, Ordering::Release);
			"Captured frame could not be read"
		})?;
		let stride = buffer.row_pitch() as usize;
		sink.send_pixels(size, stride, buffer.as_raw_buffer())
	}

	fn on_closed(&mut self) -> Result<(), Self::Error> {
		self.0.0.stop.store(true, Ordering::Release);
		Ok(())
	}
}

/// Desktop duplication of the display a fullscreen game fills. A game in fullscreen skips the
/// desktop compositor, so window and display capture get almost no pictures of it, while
/// duplication reads what the display shows. It carries no cursor.
struct Duplication {
	monitor: isize,
	api: DxgiDuplicationApi,
	sink: Sink,
}

impl Duplication {
	fn open(monitor: isize, flags: Flags) -> Result<Self, DuplicationError> {
		let api = DxgiDuplicationApi::new_options(
			Monitor::from_raw_hmonitor(monitor as *mut _),
			&[DxgiDuplicationFormat::Bgra8],
		)?;
		Ok(Self {
			monitor,
			api,
			sink: Sink::new(flags),
		})
	}

	/// Waits up to `timeout` for a new picture of the display and delivers it.
	fn pump(&mut self, timeout: Duration) -> Result<(), DuplicationError> {
		let Self { api, sink, .. } = self;
		let mut frame = match api.acquire_next_frame(timeout.as_millis().min(1000) as u32) {
			Ok(frame) => frame,
			Err(DuplicationError::Timeout) => return Ok(()),
			Err(error) => return Err(error),
		};
		// A cursor move alone repeats the last picture.
		if frame.frame_info().LastPresentTime == 0 || !sink.admit() {
			return Ok(());
		}
		let size = (frame.width(), frame.height());
		if sink.check(size.0, size.1).is_err() {
			return Ok(());
		}
		let bound = bind(frame.device(), frame.device_context(), frame.texture());
		if sink.try_surface(bound, size) {
			return Ok(());
		}
		let mut buffer = frame.buffer()?;
		let stride = buffer.row_pitch() as usize;
		if let Err(error) = sink.send_pixels(size, stride, buffer.as_raw_buffer()) {
			crate::stream_log::event(format_args!("capture: {error}"));
		}
		Ok(())
	}
}

#[allow(unsafe_code)] // Win32 window and display queries.
fn monitor_rect(monitor: HMONITOR) -> Option<RECT> {
	let mut info = MONITORINFO {
		cbSize: size_of::<MONITORINFO>() as u32,
		..Default::default()
	};
	// SAFETY: A plain query with a correctly sized out structure.
	unsafe { GetMonitorInfoW(monitor, &mut info) }
		.as_bool()
		.then_some(info.rcMonitor)
}

#[allow(unsafe_code)] // Win32 window and display queries.
fn window_rect(window: HWND) -> Option<RECT> {
	let mut rect = RECT::default();
	// SAFETY: A plain query of a window handle that may already be gone.
	unsafe { GetWindowRect(window, &mut rect) }
		.ok()
		.map(|()| rect)
}

fn covers(outer: RECT, inner: RECT) -> bool {
	outer.left <= inner.left
		&& outer.top <= inner.top
		&& outer.right >= inner.right
		&& outer.bottom >= inner.bottom
		&& inner.right > inner.left
		&& inner.bottom > inner.top
}

/// The top-level window drawn at five points spread over `area`, when it is one window.
#[allow(unsafe_code)] // Win32 window and display queries.
fn window_on_top(area: RECT) -> Option<HWND> {
	let (width, height) = (area.right - area.left, area.bottom - area.top);
	let inset = (width.min(height) / 8).max(1);
	let points = [
		(area.left + width / 2, area.top + height / 2),
		(area.left + inset, area.top + inset),
		(area.right - inset, area.top + inset),
		(area.left + inset, area.bottom - inset),
		(area.right - inset, area.bottom - inset),
	];
	let mut found = None;
	for (x, y) in points {
		// SAFETY: Plain hit tests on screen coordinates.
		let root = unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) };
		if root.is_invalid() || found.is_some_and(|window| window != root) {
			return None;
		}
		found = Some(root);
	}
	found
}

/// The desktop's own wallpaper and icon windows also cover a whole display.
#[allow(unsafe_code)] // Win32 window and display queries.
fn is_desktop(window: HWND) -> bool {
	let mut class = [0_u16; 16];
	// SAFETY: Reads at most the buffer length.
	let len = unsafe { GetClassNameW(window, &mut class) }.max(0) as usize;
	matches!(
		String::from_utf16_lossy(&class[..len]).as_str(),
		"WorkerW" | "Progman"
	)
}

/// The display `window` fills completely: a fullscreen game or video.
#[allow(unsafe_code)] // Win32 window and display queries.
fn fills_display(window: HWND) -> Option<HMONITOR> {
	// SAFETY: Plain queries of a window handle that may already be gone.
	if unsafe { IsIconic(window) }.as_bool() {
		return None;
	}
	// SAFETY: As above.
	let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONULL) };
	if monitor.is_invalid() {
		return None;
	}
	covers(window_rect(window)?, monitor_rect(monitor)?).then_some(monitor)
}

/// The display to duplicate instead of capturing: the one a fullscreen window fills while it
/// is the window on top there. A shared window that is minimized or covered never qualifies,
/// so duplication never shows anything the user did not choose to share.
fn fullscreen_display(source: SourceId) -> Option<isize> {
	match source {
		SourceId::Window(id) => {
			let window = HWND(id as usize as *mut _);
			let monitor = fills_display(window)?;
			(window_on_top(monitor_rect(monitor)?)? == window).then_some(monitor.0 as isize)
		}
		SourceId::Display(id) => {
			let monitor = HMONITOR(id as usize as *mut _);
			let window = window_on_top(monitor_rect(monitor)?)?;
			(!is_desktop(window) && fills_display(window) == Some(monitor))
				.then_some(monitor.0 as isize)
		}
		#[allow(unreachable_patterns)] // Portal may be absent from platform-scoped models.
		_ => None,
	}
}

/// How often the watcher looks for a fullscreen window.
const CHECK: Duration = Duration::from_millis(250);
/// After duplication fails, capture carries the share this long before a retry.
const RETRY: Duration = Duration::from_secs(10);

/// Owns the capture backends of one share and switches between them: window or display
/// capture normally, display duplication while a fullscreen game fills the shared display.
#[allow(unsafe_code)] // Win32 window and display queries.
fn watch(
	source: SourceId,
	flags: Flags,
	cursor: bool,
	mut capture: Option<CaptureControl<Handler, &'static str>>,
	halt: Arc<AtomicBool>,
) {
	let mut duplication: Option<Duplication> = None;
	let mut retry = Instant::now();
	let mut next_check = Instant::now();
	let window = match source {
		SourceId::Window(id) => Some(HWND(id as usize as *mut _)),
		_ => None,
	};
	while !halt.load(Ordering::Acquire) && !flags.stop.load(Ordering::Acquire) {
		let now = Instant::now();
		if now >= next_check {
			next_check = now + CHECK;
			// SAFETY: A plain validity query.
			if window.is_some_and(|window| !unsafe { IsWindow(Some(window)) }.as_bool()) {
				flags.stop.store(true, Ordering::Release);
				break;
			}
			let target = fullscreen_display(source);
			if let Some(monitor) = target
				&& duplication.as_ref().is_none_or(|d| d.monitor != monitor)
				&& now >= retry
			{
				match Duplication::open(monitor, flags.clone()) {
					Ok(opened) => {
						if let Some(control) = capture.take() {
							let _ = control.stop();
						}
						crate::stream_log::event(format_args!(
							"capture: a fullscreen window fills the display; duplicating it"
						));
						duplication = Some(opened);
					}
					Err(error) => {
						crate::stream_log::event(format_args!(
							"capture: display duplication unavailable ({error}); capture continues"
						));
						retry = now + RETRY;
					}
				}
			} else if target.is_none() && duplication.take().is_some() {
				crate::stream_log::event(format_args!("capture: fullscreen ended"));
			}
			if duplication.is_none() && capture.is_none() {
				match start_capture(source, flags.clone(), cursor) {
					Ok(control) => capture = Some(control),
					Err(error) => {
						crate::stream_log::event(format_args!("capture: {error}"));
						flags.stop.store(true, Ordering::Release);
						break;
					}
				}
			}
		}
		let wait = next_check.saturating_duration_since(Instant::now());
		let Some(active) = duplication.as_mut() else {
			std::thread::sleep(wait.min(Duration::from_millis(50)));
			continue;
		};
		match active.pump(wait.min(Duration::from_millis(100))) {
			Ok(()) => {}
			// A display mode change, such as a game entering exclusive fullscreen, ends the
			// session; reopen it at once.
			Err(DuplicationError::AccessLost) => {
				let monitor = active.monitor;
				duplication = Duplication::open(monitor, flags.clone()).ok();
				if duplication.is_none() {
					retry = Instant::now() + Duration::from_secs(1);
					next_check = Instant::now();
				}
			}
			Err(error) => {
				crate::stream_log::event(format_args!(
					"capture: display duplication stopped ({error})"
				));
				duplication = None;
				retry = Instant::now() + RETRY;
				next_check = Instant::now();
			}
		}
	}
	drop(duplication);
	if let Some(control) = capture {
		let _ = control.stop();
	}
}

pub(crate) struct Capture {
	halt: Arc<AtomicBool>,
	watcher: Option<std::thread::JoinHandle<()>>,
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
		// Started here so a source that cannot be captured fails the share at once.
		let first = start_capture(settings.source, flags.clone(), settings.cursor)?;
		let halt = Arc::new(AtomicBool::new(false));
		let watcher_halt = halt.clone();
		let (source, cursor) = (settings.source, settings.cursor);
		let watcher = std::thread::Builder::new()
			.name("screen-capture".into())
			.spawn(move || watch(source, flags, cursor, Some(first), watcher_halt))
			.map_err(|_| "Screen capture could not be started")?;
		let mut capture = Self {
			halt,
			watcher: Some(watcher),
			audio: None,
			audio_warning: None,
		};
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

/// Window or display capture of `source`.
fn start_capture(
	source: SourceId,
	flags: Flags,
	cursor: bool,
) -> Result<CaptureControl<Handler, &'static str>, &'static str> {
	match source {
		SourceId::Display(id) => {
			let monitor = Monitor::enumerate()
				.map_err(|_| "Displays could not be enumerated")?
				.into_iter()
				.find(|monitor| monitor_id(monitor) == id)
				.ok_or("Selected display is no longer available")?;
			start_item(monitor, flags, cursor)
		}
		SourceId::Window(id) => {
			let window = Window::enumerate()
				.map_err(|_| "Windows could not be enumerated")?
				.into_iter()
				.find(|window| window_id(window) == id)
				.ok_or("Selected window is no longer available")?;
			// A game that fills the display is captured without the cursor even while covered:
			// capturing it can leave a fullscreen game without its own cursor.
			let cursor = cursor && fills_display(HWND(window.as_raw_hwnd())).is_none();
			start_item(window, flags, cursor)
		}
		#[allow(unreachable_patterns)] // Portal may be absent from platform-scoped models.
		_ => Err("The desktop screen picker is available only on Linux"),
	}
}

fn start_item<T>(
	item: T,
	flags: Flags,
	cursor: bool,
) -> Result<CaptureControl<Handler, &'static str>, &'static str>
where
	T: NativeSource,
{
	let Some((width, height)) = item.dimensions() else {
		return Err("Selected source dimensions are unavailable");
	};
	if !crate::screen::fits_capture(width, height) {
		return Err("Selected source has more pixels than the 4K capture limit");
	}
	let native = Settings::new(
		item,
		if cursor {
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
	Handler::start_free_threaded(native).map_err(|_| "Screen capture could not be started")
}

impl Drop for Capture {
	fn drop(&mut self) {
		self.audio.take();
		self.halt.store(true, Ordering::Release);
		if let Some(watcher) = self.watcher.take() {
			let _ = watcher.join();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
		RECT {
			left,
			top,
			right,
			bottom,
		}
	}

	#[test]
	fn only_a_window_covering_the_whole_display_is_fullscreen() {
		let display = rect(0, 0, 2560, 1440);
		assert!(covers(rect(0, 0, 2560, 1440), display));
		// Games often overshoot the display by their invisible frame.
		assert!(covers(rect(-8, -8, 2568, 1448), display));
		// A maximized window stops at the taskbar.
		assert!(!covers(rect(0, 0, 2560, 1392), display));
		assert!(!covers(rect(2560, 0, 5120, 1440), display));
		assert!(!covers(display, rect(0, 0, 0, 0)));
	}

	/// Live check on this machine's primary display: duplication delivers GPU pictures through
	/// the same converter as window capture. Reads the screen into memory only.
	#[test]
	#[ignore = "needs a desktop session and a GPU"]
	fn duplication_delivers_converted_pictures() {
		let (frames, received) = std::sync::mpsc::sync_channel(1);
		let pending = Arc::new(AtomicBool::new(false));
		let flags = Flags {
			frames,
			stop: Arc::new(AtomicBool::new(false)),
			pending: pending.clone(),
			next_frame: Instant::now(),
			interval: Duration::from_millis(16),
			gpu: Arc::new(AtomicBool::new(true)),
			preview: Arc::new(Mutex::new(None)),
			next_preview: Instant::now(),
			output: (1280, 720),
		};
		let monitor = Monitor::primary()
			.expect("primary display")
			.as_raw_hmonitor() as isize;
		let mut duplication = Duplication::open(monitor, flags).expect("duplication");
		let deadline = Instant::now() + Duration::from_secs(3);
		let (mut pictures, mut encoder, mut bytes) = (0, None, 0);
		while Instant::now() < deadline && pictures < 3 {
			duplication
				.pump(Duration::from_millis(100))
				.expect("duplicated picture");
			if let Ok(frame) = received.try_recv() {
				let surface = frame.surface.expect("GPU picture");
				assert_eq!((frame.width, frame.height), (1280, 720));
				// The hardware encoder reads pictures of the duplication's own device.
				let encoder = encoder.get_or_insert_with(|| {
					crate::video_encode::hardware::Encoder::with_device(
						crate::video_encode::Config {
							width: 1280,
							height: 720,
							fps: 60,
							bit_rate: 4_000_000,
							max_bytes: 4 << 20,
							keyframe_interval: 3000,
							profile: crate::video_encode::Profile::Main,
						},
						&surface.device,
					)
					.expect("hardware encoder on the duplication device")
				});
				bytes += encoder
					.encode_surface(&surface, pictures == 0)
					.expect("encode")
					.0
					.len();
				pending.store(false, Ordering::Release);
				pictures += 1;
			}
		}
		assert!(pictures > 0, "no duplicated pictures in 3 s");
		assert!(bytes > 0, "the encoder produced nothing");
	}
}
