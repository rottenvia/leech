//! Stream to yourself: the pictures behind `ui::self_view` windows. A screen or window is
//! captured in memory without an encoder, files and pasted images decode on worker threads,
//! and every picture reaches its window through a mipmapped GPU texture. Nothing is sent.
use crate::gpu_video::VideoTexture;
// A capture's picture and a decoded image share one shape: packed, opaque RGBA.
use discord_voice::screen::{LocalCapture, LocalFrame as Frame, SourceId};
use eframe::egui;
use platform::video::{Color, Layout, Picture, Sample};
use std::{
	io::Read,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex, PoisonError,
		atomic::{AtomicBool, Ordering},
		mpsc,
	},
	time::{Duration, Instant},
};
use ui::self_view::{Kind, MAX_VIEWS, Request};

/// Screens and windows refresh at most this often: a widget, not a stream.
const CAPTURE_FPS: u32 = 30;
/// Longest edge kept for a still image; larger ones are scaled down once when they load.
const MAX_EDGE: u32 = 3840;
/// Longest edge of animation frames, and the decoded bytes one animation may keep.
const ANIMATION_EDGE: u32 = 1280;
const MAX_ANIMATION_BYTES: usize = 192 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 1200;
/// Largest image or GIF file read into memory.
const MAX_IMAGE_FILE: u64 = 96 * 1024 * 1024;
const VIDEO_EXTENSIONS: [&str; 7] = ["mp4", "m4v", "mov", "wmv", "avi", "mkv", "webm"];

enum Source {
	Capture(LocalCapture),
	Video(VideoLoop),
	/// A still image is one frame; an animation loops through its frames and delays.
	Frames {
		frames: Vec<(Duration, Frame)>,
		index: usize,
		next: Option<Instant>,
	},
}

struct Local {
	id: u64,
	source: Source,
	texture: VideoTexture,
}

enum Loaded {
	Frames(Vec<(Duration, Frame)>),
	Video(VideoLoop),
}

type Load = (String, Kind, mpsc::Receiver<Result<Loaded, &'static str>>);

#[derive(Default)]
pub struct SelfViews {
	locals: Vec<Local>,
	next_id: u64,
	picking: Option<mpsc::Receiver<Option<Vec<PathBuf>>>>,
	pasting: Option<mpsc::Receiver<Result<Pasted, &'static str>>>,
	loading: Vec<Load>,
	#[cfg(feature = "demo")]
	previewed: bool,
}

enum Pasted {
	Files(Vec<PathBuf>),
	Image(Frame),
}

impl SelfViews {
	/// Takes the windows' requests, finishes loads, and hands each window its newest picture.
	pub fn poll(
		&mut self,
		views: &mut ui::self_view::SelfViews,
		ctx: &egui::Context,
		runtime: &tokio::runtime::Handle,
		window: &Arc<winit::window::Window>,
	) {
		// `--demo --demo-self-view`: a still zone plate and a two-frame animation, synthetic
		// pictures without any capture or file.
		#[cfg(feature = "demo")]
		if !std::mem::replace(&mut self.previewed, true)
			&& std::env::args().any(|arg| arg == "--demo")
			&& std::env::args().any(|arg| arg == "--demo-self-view")
		{
			let still = vec![(Duration::ZERO, zone_plate(1920, 1080, 0.0))];
			self.add(
				views,
				ctx,
				"Zone plate.png".into(),
				Kind::Image,
				frames(still),
			);
			let pulse = (0..2)
				.map(|phase| {
					(
						Duration::from_millis(600),
						zone_plate(640, 360, phase as f32),
					)
				})
				.collect();
			self.add(
				views,
				ctx,
				"Pulse.gif".into(),
				Kind::Animation,
				frames(pulse),
			);
		}
		for request in std::mem::take(&mut views.requests) {
			match request {
				Request::Capture(source, name) if views.has_room() => {
					let wake = ctx.clone();
					match LocalCapture::start(source, CAPTURE_FPS, move || wake.request_repaint()) {
						Ok(capture) => {
							let kind = if matches!(source, SourceId::Window(_)) {
								Kind::Window
							} else {
								Kind::Screen
							};
							self.add(views, ctx, name, kind, Source::Capture(capture));
						}
						Err(error) => views.status = error,
					}
				}
				Request::Files if self.picking.is_none() => {
					let (send, receive) = mpsc::sync_channel(1);
					let dialog = platform::save::media_sources(window.clone());
					let wake = ctx.clone();
					runtime.spawn(async move {
						let _ = send.send(dialog.await);
						wake.request_repaint();
					});
					self.picking = Some(receive);
				}
				Request::Paste if self.pasting.is_none() => {
					let (send, receive) = mpsc::sync_channel(1);
					let wake = ctx.clone();
					runtime.spawn_blocking(move || {
						let _ = send.send(paste());
						wake.request_repaint();
					});
					self.pasting = Some(receive);
				}
				_ => {}
			}
		}
		if let Some(picking) = &self.picking
			&& let Ok(paths) = picking.try_recv()
		{
			self.picking = None;
			self.load_files(views, ctx, runtime, paths.unwrap_or_default());
		}
		if let Some(pasting) = &self.pasting
			&& let Ok(pasted) = pasting.try_recv()
		{
			self.pasting = None;
			match pasted {
				Ok(Pasted::Files(paths)) => self.load_files(views, ctx, runtime, paths),
				Ok(Pasted::Image(frame)) => self.add(
					views,
					ctx,
					"Pasted image".into(),
					Kind::Image,
					frames(vec![(Duration::ZERO, frame)]),
				),
				Err(error) => views.status = error,
			}
		}
		let mut index = 0;
		while index < self.loading.len() {
			match self.loading[index].2.try_recv() {
				Err(mpsc::TryRecvError::Empty) => index += 1,
				done => {
					let (name, kind, _) = self.loading.swap_remove(index);
					match done {
						Ok(Ok(Loaded::Frames(frames))) => {
							let kind = if frames.len() > 1 { kind } else { Kind::Image };
							self.add(views, ctx, name, kind, self::frames(frames));
						}
						Ok(Ok(Loaded::Video(video))) => {
							self.add(views, ctx, name, Kind::Video, Source::Video(video))
						}
						Ok(Err(error)) => views.status = error,
						Err(_) => views.status = "That file could not be opened",
					}
				}
			}
		}
		views.busy = self.picking.is_some() || self.pasting.is_some() || !self.loading.is_empty();
		// A window closed in the interface takes its capture or decoder with it.
		self.locals
			.retain(|local| views.views.iter().any(|view| view.id == local.id));
		for local in &mut self.locals {
			let Some(view) = views.views.iter_mut().find(|view| view.id == local.id) else {
				continue;
			};
			match &mut local.source {
				Source::Capture(capture) => {
					if let Some(frame) = capture.take() {
						view.texture = upload(&mut local.texture, ctx, &frame);
					}
					if capture.ended() {
						view.status = "This screen or window is no longer available";
					}
				}
				Source::Video(video) => {
					if let Some(picture) = video.take() {
						view.texture = local.texture.upload(
							ctx,
							"self-view",
							picture.with_data(picture.data.as_slice()),
						);
					}
					if let Some(error) = video.failed() {
						view.status = error;
					}
				}
				Source::Frames {
					frames,
					index,
					next,
				} => {
					let now = Instant::now();
					let due = match next {
						None => true,
						Some(at) => frames.len() > 1 && now >= *at,
					};
					if due {
						if next.is_some() {
							*index = (*index + 1) % frames.len();
						}
						let delay = frames[*index].0;
						// Behind (a busy or hidden moment) restarts the frame's delay from now.
						*next = Some(match *next {
							Some(at) if at + delay > now => at + delay,
							_ => now + delay,
						});
						view.texture = upload(&mut local.texture, ctx, &frames[*index].1);
					}
					if frames.len() > 1
						&& let Some(at) = next
					{
						ctx.request_repaint_after(at.saturating_duration_since(now));
					}
				}
			}
		}
	}

	fn add(
		&mut self,
		views: &mut ui::self_view::SelfViews,
		ctx: &egui::Context,
		name: String,
		kind: Kind,
		source: Source,
	) {
		if !views.has_room() {
			return;
		}
		self.next_id += 1;
		let id = self.next_id;
		views.open(ctx, id, name, kind);
		self.locals.push(Local {
			id,
			source,
			texture: VideoTexture::default(),
		});
	}

	fn load_files(
		&mut self,
		views: &mut ui::self_view::SelfViews,
		ctx: &egui::Context,
		runtime: &tokio::runtime::Handle,
		paths: Vec<PathBuf>,
	) {
		let room = MAX_VIEWS.saturating_sub(views.views.len() + self.loading.len());
		for path in paths.into_iter().take(room) {
			let name = path.file_name().map_or_else(
				|| "File".to_owned(),
				|name| name.to_string_lossy().into_owned(),
			);
			let video = is_video(&path);
			let (send, receive) = mpsc::sync_channel(1);
			let wake = ctx.clone();
			runtime.spawn_blocking(move || {
				let repaint = wake.clone();
				let loaded = if video {
					VideoLoop::start(path, move || repaint.request_repaint()).map(Loaded::Video)
				} else {
					load_image(&path).map(Loaded::Frames)
				};
				let _ = send.send(loaded);
				wake.request_repaint();
			});
			let kind = if video { Kind::Video } else { Kind::Animation };
			self.loading.push((name, kind, receive));
		}
	}
}

fn frames(frames: Vec<(Duration, Frame)>) -> Source {
	Source::Frames {
		frames,
		index: 0,
		next: None,
	}
}

/// A grey zone plate whose rings reach one cycle per two pixels at the left and right edges,
/// for judging scaling and zoom sharpness.
#[cfg(feature = "demo")]
fn zone_plate(width: u32, height: u32, phase: f32) -> Frame {
	let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
	let k = std::f32::consts::PI / width as f32;
	for y in 0..height {
		for x in 0..width {
			let (dx, dy) = (
				x as f32 - width as f32 * 0.5,
				y as f32 - height as f32 * 0.5,
			);
			let wave = ((dx * dx + dy * dy) * k + phase * std::f32::consts::PI).cos();
			let value = ((0.5 + 0.5 * wave) * 255.0) as u8;
			rgba.extend_from_slice(&[value, value, value, 255]);
		}
	}
	Frame {
		width,
		height,
		rgba,
	}
}

fn upload(
	texture: &mut VideoTexture,
	ctx: &egui::Context,
	frame: &Frame,
) -> Option<egui::load::SizedTexture> {
	texture.upload(
		ctx,
		"self-view",
		Picture {
			width: frame.width,
			height: frame.height,
			layout: Layout::Rgba,
			color: Color::WEBRTC,
			rotation: 0,
			data: frame.rgba.as_slice(),
		},
	)
}

fn is_video(path: &Path) -> bool {
	path.extension()
		.and_then(|extension| extension.to_str())
		.is_some_and(|extension| {
			VIDEO_EXTENSIONS
				.iter()
				.any(|known| extension.eq_ignore_ascii_case(known))
		})
}

/// A still image, or every frame of an animated GIF, WebP or PNG with its delay.
fn load_image(path: &Path) -> Result<Vec<(Duration, Frame)>, &'static str> {
	let file = std::fs::File::open(path).map_err(|_| "That file could not be opened")?;
	let length = file.metadata().map_or(0, |metadata| metadata.len());
	if length > MAX_IMAGE_FILE {
		return Err("Images and GIFs up to 96 MB can be shown");
	}
	let mut bytes = Vec::with_capacity(length as usize);
	file.take(MAX_IMAGE_FILE)
		.read_to_end(&mut bytes)
		.map_err(|_| "That file could not be read")?;
	// Animated PNGs carry an acTL chunk before their pictures; other PNGs skip the frame pass.
	let animated = match image::guess_format(&bytes) {
		Ok(image::ImageFormat::Gif | image::ImageFormat::WebP) => true,
		Ok(image::ImageFormat::Png) => bytes[..bytes.len().min(1 << 16)]
			.windows(4)
			.any(|chunk| chunk == b"acTL"),
		_ => false,
	};
	if animated
		&& let Some(frames) = animation(&bytes)
		&& frames.len() > 1
	{
		return Ok(frames);
	}
	let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
		.with_guessed_format()
		.map_err(|_| "That file is not an image or video Leech can show")?;
	let mut limits = image::Limits::default();
	limits.max_image_width = Some(16_384);
	limits.max_image_height = Some(16_384);
	limits.max_alloc = Some(512 * 1024 * 1024);
	reader.limits(limits);
	let image = reader
		.decode()
		.map_err(|_| "That file is not an image or video Leech can show")?
		.into_rgba8();
	let image = crate::avatars::resize_to(image, MAX_EDGE);
	Ok(vec![(
		Duration::ZERO,
		Frame {
			width: image.width(),
			height: image.height(),
			rgba: image.into_raw(),
		},
	)])
}

/// Every frame of an animation, scaled down until all of them fit the memory budget.
fn animation(bytes: &[u8]) -> Option<Vec<(Duration, Frame)>> {
	let mut fit = ANIMATION_EDGE;
	'decode: for _ in 0..5 {
		let mut frames = Vec::new();
		let mut total = 0;
		for frame in crate::avatars::animation_frames(bytes)? {
			if frames.len() == MAX_ANIMATION_FRAMES {
				break;
			}
			let frame = frame.ok()?;
			let (numerator, denominator) = frame.delay().numer_denom_ms();
			let delay = Duration::from_millis(
				(u64::from(numerator) / u64::from(denominator.max(1))).clamp(20, 10_000),
			);
			let image = crate::avatars::resize_to(frame.into_buffer(), fit);
			total += image.as_raw().len();
			if total > MAX_ANIMATION_BYTES {
				fit = (fit as f32 * 0.7) as u32;
				continue 'decode;
			}
			frames.push((
				delay,
				Frame {
					width: image.width(),
					height: image.height(),
					rgba: image.into_raw(),
				},
			));
		}
		return Some(frames);
	}
	None
}

/// The clipboard's copied files (as from Explorer) or its image.
fn paste() -> Result<Pasted, &'static str> {
	let mut clipboard = arboard::Clipboard::new().map_err(|_| "The clipboard is unavailable")?;
	if let Ok(paths) = clipboard.get().file_list()
		&& !paths.is_empty()
	{
		return Ok(Pasted::Files(paths));
	}
	let image = clipboard
		.get_image()
		.map_err(|_| "Copy an image or a file first, then paste it here")?;
	let (width, height) = (image.width as u32, image.height as u32);
	if width == 0 || height == 0 || image.bytes.len() != image.width * image.height * 4 {
		return Err("The copied image could not be read");
	}
	let image = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())
		.ok_or("The copied image could not be read")?;
	let image = crate::avatars::resize_to(image, MAX_EDGE);
	Ok(Pasted::Image(Frame {
		width: image.width(),
		height: image.height(),
		rgba: image.into_raw(),
	}))
}

/// A video file playing silently on a loop, decoded on its own thread at its own pace; the
/// newest picture waits for the window.
struct VideoLoop {
	stop: Arc<AtomicBool>,
	latest: Arc<Mutex<Option<Picture>>>,
	error: Arc<Mutex<Option<&'static str>>>,
}

impl VideoLoop {
	/// Opens `path` on the decoding thread (decoders stay on the thread that made them) and
	/// returns once the first picture is decoding or the file failed to open.
	fn start(path: PathBuf, wake: impl Fn() + Send + 'static) -> Result<Self, &'static str> {
		let stop = Arc::new(AtomicBool::new(false));
		let latest = Arc::new(Mutex::new(None));
		let error = Arc::new(Mutex::new(None));
		let (opened, open_result) = mpsc::sync_channel(1);
		let (thread_stop, thread_latest, thread_error) =
			(stop.clone(), latest.clone(), error.clone());
		std::thread::Builder::new()
			.name("self-view-video".into())
			.spawn(move || {
				let file = match std::fs::File::open(&path) {
					Ok(file) => file,
					Err(_) => {
						let _ = opened.send(Err("That video could not be opened"));
						return;
					}
				};
				let mut decoder =
					match platform::video::Decoder::open(Box::new(std::io::BufReader::new(file))) {
						Ok(decoder) => decoder,
						Err(error) => {
							let _ = opened.send(Err(error));
							return;
						}
					};
				let _ = opened.send(Ok(()));
				let failure = play(&mut decoder, &thread_stop, &thread_latest, &wake);
				if let Err(failure) = failure {
					*thread_error.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);
					wake();
				}
			})
			.map_err(|_| "That video could not be opened")?;
		open_result
			.recv()
			.map_err(|_| "That video could not be opened")??;
		Ok(Self {
			stop,
			latest,
			error,
		})
	}

	fn take(&self) -> Option<Picture> {
		self.latest
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.take()
	}

	fn failed(&self) -> Option<&'static str> {
		*self.error.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

impl Drop for VideoLoop {
	fn drop(&mut self) {
		// The thread notices within one picture's wait and drops its decoder there.
		self.stop.store(true, Ordering::Release);
	}
}

/// Plays `decoder` until stopped, from the start again at its end. Audio is decoded only to
/// keep the reader from queueing it, and dropped: these windows are silent.
fn play(
	decoder: &mut platform::video::Decoder,
	stop: &AtomicBool,
	latest: &Mutex<Option<Picture>>,
	wake: &impl Fn(),
) -> Result<(), &'static str> {
	use std::task::Poll;
	let audio = decoder.info().sample_rate > 0;
	let mut clock: Option<(Instant, f64)> = None;
	let mut shown = 0_u32;
	while !stop.load(Ordering::Acquire) {
		let Poll::Ready(sample) = decoder.poll_video()? else {
			std::thread::sleep(Duration::from_millis(2));
			continue;
		};
		match sample {
			Some(Sample::Video { pts, picture }) => {
				// Audio up to this picture is decoded and dropped, so the reader never queues it.
				while audio
					&& let Ok(Poll::Ready(Some(Sample::Audio { pts: at, .. }))) =
						decoder.poll_audio()
					&& at < pts
				{}
				let (origin, start) = *clock.get_or_insert((Instant::now(), pts));
				let due = origin + Duration::from_secs_f64((pts - start).max(0.0));
				while !stop.load(Ordering::Acquire) {
					let left = due.saturating_duration_since(Instant::now());
					if left.is_zero() {
						break;
					}
					std::thread::sleep(left.min(Duration::from_millis(50)));
				}
				*latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(picture);
				shown += 1;
				wake();
			}
			Some(Sample::Audio { .. }) => {}
			None if shown == 0 => return Err("That video has no pictures Leech can show"),
			None => {
				decoder.seek(0.0)?;
				clock = None;
				shown = 0;
			}
		}
	}
	Ok(())
}
