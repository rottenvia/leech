//! Stream to yourself: the pictures behind `ui::self_view` windows. A screen or window is
//! captured in memory without an encoder, files and pasted images decode on worker threads,
//! a pasted GIF link (Giphy, Tenor) is fetched as the real file, and every picture reaches its
//! window through a mipmapped GPU texture. Nothing is sent.
use crate::gpu_video::VideoTexture;
// A capture's picture and a decoded image share one shape: packed RGBA, premultiplied where
// an image has see-through parts.
use discord_voice::screen::{LocalCapture, LocalFrame as Frame, SourceId};
use eframe::egui;
use platform::video::{Color, Layout, Picture, ReadSeek, Sample};
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
/// A capture with no picture this long after it started is waiting on an app that draws
/// nothing while hidden.
const FIRST_PICTURE: Duration = Duration::from_secs(3);
const MINIMIZED: &str = "This window is minimized, and Leech leaves it alone because it fills the screen when restored, like a game, or draws its own see-through edges. Minimized, it draws nothing; it shows here again once restored.\n\n\
	A game in exclusive fullscreen minimizes when you click away: set it to borderless or fullscreen windowed to keep it showing.";
const NOT_DRAWING: &str = "Nothing to show yet: this app draws nothing while it is hidden. Some games and browsers stop drawing when another window covers them.\n\n\
	Bring it to the front once, or set a game to borderless or fullscreen windowed.";
const GONE: &str = "This screen or window is no longer available";
/// Longest edge kept for a still image; larger ones are scaled down once when they load.
const MAX_EDGE: u32 = 3840;
/// Longest edge of animation frames, and the decoded bytes one animation may keep.
const ANIMATION_EDGE: u32 = 1280;
const MAX_ANIMATION_BYTES: usize = 192 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 1200;
/// Largest image or GIF file read into memory.
const MAX_IMAGE_FILE: u64 = 96 * 1024 * 1024;
const VIDEO_EXTENSIONS: [&str; 7] = ["mp4", "m4v", "mov", "wmv", "avi", "mkv", "webm"];
/// Largest web page read to find a link's picture.
const MAX_PAGE: usize = 2 * 1024 * 1024;
const FETCH_FAILED: &str = "That link could not be downloaded";

enum Source {
	/// With when it started, for the first-picture wait.
	Capture(LocalCapture, Instant),
	Video(VideoLoop),
	/// A still image is one frame; an animation loops through its frames and delays.
	Frames {
		frames: Arc<Vec<(Duration, Frame)>>,
		index: usize,
		next: Option<Instant>,
		/// No frame has a see-through pixel, so removing the background needs a cut-out.
		opaque: bool,
		cut: Cut,
		/// The window shows the cut-out; a change re-uploads the current frame.
		cut_shown: bool,
		/// Reused for a cut-out frame's pixels.
		scratch: Vec<u8>,
	},
}

/// Alpha masks that cut a plain background out of opaque frames, made once on a worker the
/// first time a window asks to remove its background.
enum Cut {
	None,
	Making(mpsc::Receiver<Option<Vec<Vec<u8>>>>),
	Ready(Vec<Vec<u8>>),
	/// The picture has no plain background (a photo); it stays whole.
	Unavailable,
}

impl Cut {
	fn start(frames: Arc<Vec<(Duration, Frame)>>, ctx: egui::Context) -> Self {
		let (send, receive) = mpsc::sync_channel(1);
		let spawned = std::thread::Builder::new()
			.name("self-view-cutout".into())
			.spawn(move || {
				let masks = frames
					.first()
					.and_then(|(_, first)| background_key(first))
					.map(|key| {
						frames
							.iter()
							.map(|(_, frame)| cut_out(frame, key))
							.collect()
					});
				let _ = send.send(masks);
				ctx.request_repaint();
			});
		if spawned.is_ok() {
			Self::Making(receive)
		} else {
			Self::Unavailable
		}
	}
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
/// A link being downloaded: its window's name, the copied bitmap shown if it fails, and the
/// download.
type Fetch = (
	String,
	Option<Frame>,
	mpsc::Receiver<Result<Vec<u8>, &'static str>>,
);

#[derive(Default)]
pub struct SelfViews {
	locals: Vec<Local>,
	next_id: u64,
	picking: Option<mpsc::Receiver<Option<Vec<PathBuf>>>>,
	pasting: Option<mpsc::Receiver<Result<Pasted, &'static str>>>,
	/// A pasted link being downloaded, under the name its window gets.
	fetching: Option<Fetch>,
	loading: Vec<Load>,
	#[cfg(feature = "demo")]
	previewed: bool,
}

enum Pasted {
	Files(Vec<PathBuf>),
	Image(Frame),
	/// A GIF, picture or video address: copied from a page (with the copied bitmap, a GIF's
	/// first frame, for when the address cannot be downloaded), or a copied Giphy or Tenor link.
	Link(url::Url, Option<Frame>),
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
			let sticker = vec![(Duration::ZERO, sticker(360))];
			self.add(
				views,
				ctx,
				"Sticker.gif".into(),
				Kind::Image,
				frames(sticker),
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
							self.add(
								views,
								ctx,
								name,
								kind,
								Source::Capture(capture, Instant::now()),
							);
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
				Ok(Pasted::Link(url, fallback)) if self.fetching.is_none() => {
					let (send, receive) = mpsc::sync_channel(1);
					let wake = ctx.clone();
					let name = link_name(&url);
					runtime.spawn(async move {
						let _ = send.send(fetch_media(url).await);
						wake.request_repaint();
					});
					self.fetching = Some((name, fallback, receive));
				}
				Ok(Pasted::Link(..)) => {}
				Err(error) => views.status = error,
			}
		}
		if let Some((_, _, fetching)) = &self.fetching
			&& let Ok(fetched) = fetching.try_recv()
		{
			let (name, fallback, _) = self.fetching.take().expect("checked fetch");
			match (fetched, fallback) {
				(Ok(bytes), _) => self.load_bytes(views, ctx, runtime, name, bytes),
				(Err(_), Some(frame)) => self.add(
					views,
					ctx,
					name,
					Kind::Image,
					frames(vec![(Duration::ZERO, frame)]),
				),
				(Err(error), None) => views.status = error,
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
		views.busy = self.picking.is_some()
			|| self.pasting.is_some()
			|| self.fetching.is_some()
			|| !self.loading.is_empty();
		// A window closed in the interface takes its capture or decoder with it.
		self.locals
			.retain(|local| views.views.iter().any(|view| view.id == local.id));
		for local in &mut self.locals {
			let Some(view) = views.views.iter_mut().find(|view| view.id == local.id) else {
				continue;
			};
			match &mut local.source {
				Source::Capture(capture, started) => {
					if let Some(frame) = capture.take() {
						view.texture = upload(&mut local.texture, ctx, &frame);
					}
					let minimized = capture.minimized();
					let waiting = view.texture.is_none() && started.elapsed() >= FIRST_PICTURE;
					// A minimized or silent source says why instead of loading forever; the
					// last picture stays under it, and it clears once pictures come again.
					view.status = if capture.ended() {
						GONE
					} else if minimized {
						MINIMIZED
					} else if waiting {
						NOT_DRAWING
					} else {
						""
					};
					if !capture.ended() && (minimized || view.texture.is_none()) {
						// Restoring a window wakes nothing here, so look again.
						ctx.request_repaint_after(Duration::from_millis(500));
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
					opaque,
					cut,
					cut_shown,
					scratch,
				} => {
					let want_cut = view.remove_background && *opaque;
					if want_cut && matches!(cut, Cut::None) {
						*cut = Cut::start(frames.clone(), ctx.clone());
					}
					if let Cut::Making(making) = cut
						&& let Ok(masks) = making.try_recv()
					{
						*cut = masks.map_or(Cut::Unavailable, Cut::Ready);
					}
					let masks = match cut {
						Cut::Ready(masks) if want_cut => Some(masks),
						_ => None,
					};
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
					}
					if due || masks.is_some() != *cut_shown {
						*cut_shown = masks.is_some();
						let frame = &frames[*index].1;
						view.texture = match masks {
							Some(masks) => {
								apply_mask(&frame.rgba, &masks[*index], scratch);
								upload_rgba(
									&mut local.texture,
									ctx,
									frame.width,
									frame.height,
									scratch,
								)
							}
							None => upload(&mut local.texture, ctx, frame),
						};
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
		let transparent = matches!(&source, Source::Frames { opaque: false, .. });
		views.open(ctx, id, name, kind, transparent);
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

/// Frames to show; ones with see-through pixels are premultiplied once, as the GPU blends.
impl SelfViews {
	/// A fetched file: an image, GIF or video, decoded from memory.
	fn load_bytes(
		&mut self,
		views: &mut ui::self_view::SelfViews,
		ctx: &egui::Context,
		runtime: &tokio::runtime::Handle,
		name: String,
		bytes: Vec<u8>,
	) {
		if views.views.len() + self.loading.len() >= MAX_VIEWS {
			return;
		}
		let video = is_video_bytes(&bytes);
		let (send, receive) = mpsc::sync_channel(1);
		let wake = ctx.clone();
		runtime.spawn_blocking(move || {
			let loaded = if video {
				let repaint = wake.clone();
				VideoLoop::start_bytes(bytes, move || repaint.request_repaint()).map(Loaded::Video)
			} else {
				decode_image(&bytes).map(Loaded::Frames)
			};
			let _ = send.send(loaded);
			wake.request_repaint();
		});
		let kind = if video { Kind::Video } else { Kind::Animation };
		self.loading.push((name, kind, receive));
	}
}

fn is_video_bytes(bytes: &[u8]) -> bool {
	bytes.get(4..8) == Some(b"ftyp") || bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
}

/// The window name for a fetched link: its file name, or the site's.
fn link_name(url: &url::Url) -> String {
	url.path_segments()
		.and_then(|mut segments| segments.next_back())
		.filter(|segment| segment.contains('.'))
		.map(str::to_owned)
		.or_else(|| url.host_str().map(str::to_owned))
		.unwrap_or_else(|| "Link".to_owned())
}

/// A copied Giphy page, sticker or embed link as its GIF (Giphy pages name no picture of
/// their own); other links stay as they are.
fn giphy_media(url: &url::Url) -> Option<url::Url> {
	let host = url.host_str()?;
	if host != "giphy.com" && host != "www.giphy.com" {
		return None;
	}
	let mut segments = url.path_segments()?;
	if !matches!(segments.next()?, "gifs" | "stickers" | "embed") {
		return None;
	}
	let id = segments.next()?.rsplit('-').next()?;
	if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
		return None;
	}
	url::Url::parse(&format!("https://i.giphy.com/media/{id}/giphy.gif")).ok()
}

/// The `src` of the first `<img>` in copied HTML (a browser's Copy image), when it is a web
/// address.
fn image_source(html: &str) -> Option<url::Url> {
	let tag = &html[html.find("<img")?..];
	let tag = &tag[..tag.find('>')?];
	attribute(tag, "src")
}

/// A page's own picture or clip (`og:image`, `og:video`), preferring a GIF.
fn page_media(html: &str) -> Option<url::Url> {
	let mut found: Vec<url::Url> = Vec::new();
	let mut rest = html;
	while let Some(start) = rest.find("<meta") {
		let tag = &rest[start..];
		let end = tag.find('>').unwrap_or(tag.len());
		let (meta, after) = tag.split_at(end);
		rest = after;
		let property = attribute_text(meta, "property").or_else(|| attribute_text(meta, "name"));
		if matches!(
			property.as_deref(),
			Some("og:image" | "og:video" | "og:video:url" | "twitter:image")
		) && let Some(url) = attribute(meta, "content")
		{
			found.push(url);
		}
	}
	let gif = found
		.iter()
		.position(|url| url.path().to_ascii_lowercase().ends_with(".gif"));
	gif.map(|index| found.swap_remove(index))
		.or_else(|| found.into_iter().next())
}

fn attribute(tag: &str, name: &str) -> Option<url::Url> {
	let url = url::Url::parse(&attribute_text(tag, name)?).ok()?;
	matches!(url.scheme(), "https" | "http").then_some(url)
}

fn attribute_text(tag: &str, name: &str) -> Option<String> {
	let mut rest = tag;
	loop {
		let at = rest.find(name)?;
		let after = rest[at + name.len()..].trim_start();
		let before_ok = at == 0 || rest.as_bytes()[at - 1].is_ascii_whitespace();
		rest = &rest[at + name.len()..];
		let Some(after) = after.strip_prefix('=') else {
			continue;
		};
		if !before_ok {
			continue;
		}
		let after = after.trim_start();
		let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
		let value = &after[1..];
		let value = &value[..value.find(quote)?];
		return Some(value.replace("&amp;", "&"));
	}
}

/// Downloads a pasted link's picture or video: the file itself, or the picture a page (Tenor,
/// a Giphy page) names.
async fn fetch_media(url: url::Url) -> Result<Vec<u8>, &'static str> {
	let client = reqwest::Client::builder()
		.redirect(reqwest::redirect::Policy::limited(5))
		.timeout(Duration::from_secs(30))
		.build()
		.map_err(|_| FETCH_FAILED)?;
	let url = giphy_media(&url).unwrap_or(url);
	let (bytes, page) = fetch(&client, &url).await?;
	if !page {
		return Ok(bytes);
	}
	let media = page_media(&String::from_utf8_lossy(&bytes))
		.ok_or("That link has no picture or video Leech can show")?;
	match fetch(&client, &media).await? {
		(bytes, false) => Ok(bytes),
		(_, true) => Err("That link has no picture or video Leech can show"),
	}
}

/// One bounded download; true when it is a web page rather than a file.
async fn fetch(client: &reqwest::Client, url: &url::Url) -> Result<(Vec<u8>, bool), &'static str> {
	if !matches!(url.scheme(), "https" | "http") {
		return Err(FETCH_FAILED);
	}
	let mut response = client
		.get(url.clone())
		.send()
		.await
		.map_err(|_| FETCH_FAILED)?;
	if !response.status().is_success() {
		return Err(FETCH_FAILED);
	}
	let page = response
		.headers()
		.get(reqwest::header::CONTENT_TYPE)
		.and_then(|value| value.to_str().ok())
		.is_some_and(|value| value.starts_with("text/html"));
	let limit = if page {
		MAX_PAGE
	} else {
		MAX_IMAGE_FILE as usize
	};
	if response
		.content_length()
		.is_some_and(|length| length > limit as u64)
	{
		return Err("Images, GIFs and videos up to 96 MB can be shown");
	}
	let mut bytes = Vec::new();
	while let Some(chunk) = response.chunk().await.map_err(|_| FETCH_FAILED)? {
		if bytes.len() + chunk.len() > limit {
			return Err("Images, GIFs and videos up to 96 MB can be shown");
		}
		bytes.extend_from_slice(&chunk);
	}
	Ok((bytes, page))
}

fn frames(mut frames: Vec<(Duration, Frame)>) -> Source {
	let mut opaque = true;
	for (_, frame) in &mut frames {
		if frame
			.rgba
			.as_chunks::<4>()
			.0
			.iter()
			.any(|pixel| pixel[3] < 255)
		{
			opaque = false;
			premultiply(&mut frame.rgba);
		}
	}
	Source::Frames {
		frames: Arc::new(frames),
		index: 0,
		next: None,
		opaque,
		cut: Cut::None,
		cut_shown: false,
		scratch: Vec::new(),
	}
}

fn premultiply(rgba: &mut [u8]) {
	for pixel in rgba.as_chunks_mut::<4>().0 {
		let alpha = u16::from(pixel[3]);
		for channel in &mut pixel[..3] {
			*channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
		}
	}
}

/// `rgba` (opaque) with `mask` as its alpha, premultiplied, into `out`.
fn apply_mask(rgba: &[u8], mask: &[u8], out: &mut Vec<u8>) {
	out.clear();
	out.reserve(rgba.len());
	for (pixel, &alpha) in rgba.as_chunks::<4>().0.iter().zip(mask) {
		let scale = u16::from(alpha);
		for &channel in &pixel[..3] {
			out.push(((u16::from(channel) * scale + 127) / 255) as u8);
		}
		out.push(alpha);
	}
}

/// How far a pixel may differ from the background colour and still be background, as a sum
/// of channel differences; twice this is the soft edge.
const KEY_TOLERANCE: u32 = 48;

/// The plain colour most of a picture's border shares, or `None` when the border has no
/// single colour (a photo), so nothing is cut.
fn background_key(frame: &Frame) -> Option<[u8; 3]> {
	let (width, height) = (frame.width as usize, frame.height as usize);
	if width < 3 || height < 3 {
		return None;
	}
	let pixel = |x: usize, y: usize| {
		let at = (y * width + x) * 4;
		[frame.rgba[at], frame.rgba[at + 1], frame.rgba[at + 2]]
	};
	let border: Vec<[u8; 3]> = (0..width)
		.flat_map(|x| [pixel(x, 0), pixel(x, height - 1)])
		.chain((1..height - 1).flat_map(|y| [pixel(0, y), pixel(width - 1, y)]))
		.collect();
	// The most common colour, in coarse buckets, then the average of its members.
	let mut counts = std::collections::HashMap::<[u8; 3], u32>::new();
	for color in &border {
		*counts.entry(color.map(|channel| channel >> 4)).or_default() += 1;
	}
	let (bucket, _) = counts.into_iter().max_by_key(|(_, count)| *count)?;
	let members: Vec<&[u8; 3]> = border
		.iter()
		.filter(|color| color.map(|channel| channel >> 4) == bucket)
		.collect();
	let mean = |channel: usize| {
		(members
			.iter()
			.map(|color| u32::from(color[channel]))
			.sum::<u32>()
			/ members.len() as u32) as u8
	};
	let key = [mean(0), mean(1), mean(2)];
	let close = border
		.iter()
		.filter(|color| distance(**color, key) <= KEY_TOLERANCE)
		.count();
	(close * 10 >= border.len() * 6).then_some(key)
}

fn distance(a: [u8; 3], b: [u8; 3]) -> u32 {
	a.iter().zip(b).map(|(a, b)| u32::from(a.abs_diff(b))).sum()
}

/// An alpha mask without the background: pixels of the key colour reached from the border
/// (so the same colour inside the subject stays), with a soft edge where the subject meets it.
fn cut_out(frame: &Frame, key: [u8; 3]) -> Vec<u8> {
	let (width, height) = (frame.width as usize, frame.height as usize);
	let color = |at: usize| {
		[
			frame.rgba[at * 4],
			frame.rgba[at * 4 + 1],
			frame.rgba[at * 4 + 2],
		]
	};
	let mut mask = vec![255_u8; width * height];
	let mut queue = std::collections::VecDeque::new();
	let visit = |at: usize, mask: &mut Vec<u8>, queue: &mut std::collections::VecDeque<usize>| {
		if mask[at] == 255 && distance(color(at), key) <= KEY_TOLERANCE {
			mask[at] = 0;
			queue.push_back(at);
		}
	};
	for x in 0..width {
		visit(x, &mut mask, &mut queue);
		visit((height - 1) * width + x, &mut mask, &mut queue);
	}
	for y in 0..height {
		visit(y * width, &mut mask, &mut queue);
		visit(y * width + width - 1, &mut mask, &mut queue);
	}
	while let Some(at) = queue.pop_front() {
		let (x, y) = (at % width, at / width);
		if x > 0 {
			visit(at - 1, &mut mask, &mut queue);
		}
		if x + 1 < width {
			visit(at + 1, &mut mask, &mut queue);
		}
		if y > 0 {
			visit(at - width, &mut mask, &mut queue);
		}
		if y + 1 < height {
			visit(at + width, &mut mask, &mut queue);
		}
	}
	// Subject pixels next to the cut fade by how close they are to the background colour,
	// so anti-aliased outlines keep no hard halo.
	let edge: Vec<usize> = (0..width * height)
		.filter(|&at| {
			let (x, y) = (at % width, at / width);
			mask[at] == 255
				&& ((x > 0 && mask[at - 1] == 0)
					|| (x + 1 < width && mask[at + 1] == 0)
					|| (y > 0 && mask[at - width] == 0)
					|| (y + 1 < height && mask[at + width] == 0))
		})
		.collect();
	for at in edge {
		let off = distance(color(at), key).saturating_sub(KEY_TOLERANCE);
		mask[at] = (off * 255 / KEY_TOLERANCE).min(255) as u8;
	}
	mask
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
	upload_rgba(texture, ctx, frame.width, frame.height, &frame.rgba)
}

fn upload_rgba(
	texture: &mut VideoTexture,
	ctx: &egui::Context,
	width: u32,
	height: u32,
	rgba: &[u8],
) -> Option<egui::load::SizedTexture> {
	texture.upload(
		ctx,
		"self-view",
		Picture {
			width,
			height,
			layout: Layout::Rgba,
			color: Color::WEBRTC,
			rotation: 0,
			data: rgba,
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
	decode_image(&bytes)
}

/// Like [`load_image`], from bytes in memory.
fn decode_image(bytes: &[u8]) -> Result<Vec<(Duration, Frame)>, &'static str> {
	// Animated PNGs carry an acTL chunk before their pictures; other PNGs skip the frame pass.
	let animated = match image::guess_format(bytes) {
		Ok(image::ImageFormat::Gif | image::ImageFormat::WebP) => true,
		Ok(image::ImageFormat::Png) => bytes[..bytes.len().min(1 << 16)]
			.windows(4)
			.any(|chunk| chunk == b"acTL"),
		_ => false,
	};
	if animated
		&& let Some(frames) = animation(bytes)
		&& frames.len() > 1
	{
		return Ok(frames);
	}
	let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
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

/// The clipboard's copied files (as from Explorer), a GIF or picture address (a browser's
/// Copy image carries the page's HTML; a copied Giphy or Tenor link is text), or its image.
fn paste() -> Result<Pasted, &'static str> {
	let mut clipboard = arboard::Clipboard::new().map_err(|_| "The clipboard is unavailable")?;
	if let Ok(paths) = clipboard.get().file_list()
		&& !paths.is_empty()
	{
		return Ok(Pasted::Files(paths));
	}
	// The bitmap a browser copies is a GIF's first frame; its address gives the whole GIF.
	if let Some(url) = clipboard
		.get()
		.html()
		.ok()
		.and_then(|html| image_source(&html))
	{
		return Ok(Pasted::Link(url, clipboard_image(&mut clipboard).ok()));
	}
	if let Some(url) = clipboard.get_text().ok().and_then(|text| {
		let text = text.trim();
		(!text.contains(char::is_whitespace))
			.then(|| url::Url::parse(text).ok())
			.flatten()
			.filter(|url| matches!(url.scheme(), "https" | "http"))
	}) {
		return Ok(Pasted::Link(url, None));
	}
	clipboard_image(&mut clipboard).map(Pasted::Image)
}

fn clipboard_image(clipboard: &mut arboard::Clipboard) -> Result<Frame, &'static str> {
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
	Ok(Frame {
		width: image.width(),
		height: image.height(),
		rgba: image.into_raw(),
	})
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
		Self::start_with(
			move || {
				std::fs::File::open(&path)
					.map(|file| Box::new(std::io::BufReader::new(file)) as Box<dyn ReadSeek>)
					.map_err(|_| "That video could not be opened")
			},
			wake,
		)
	}

	/// Like [`Self::start`], from a video in memory (a downloaded link).
	fn start_bytes(bytes: Vec<u8>, wake: impl Fn() + Send + 'static) -> Result<Self, &'static str> {
		Self::start_with(
			move || Ok(Box::new(std::io::Cursor::new(bytes)) as Box<dyn ReadSeek>),
			wake,
		)
	}

	fn start_with(
		open: impl FnOnce() -> Result<Box<dyn ReadSeek>, &'static str> + Send + 'static,
		wake: impl Fn() + Send + 'static,
	) -> Result<Self, &'static str> {
		let stop = Arc::new(AtomicBool::new(false));
		let latest = Arc::new(Mutex::new(None));
		let error = Arc::new(Mutex::new(None));
		let (opened, open_result) = mpsc::sync_channel(1);
		let (thread_stop, thread_latest, thread_error) =
			(stop.clone(), latest.clone(), error.clone());
		std::thread::Builder::new()
			.name("self-view-video".into())
			.spawn(move || {
				let source = match open() {
					Ok(source) => source,
					Err(error) => {
						let _ = opened.send(Err(error));
						return;
					}
				};
				let mut decoder = match platform::video::Decoder::open(source) {
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

/// A synthetic sticker: a soft-edged ring and dot on a see-through background.
#[cfg(feature = "demo")]
fn sticker(size: u32) -> Frame {
	let mut rgba = Vec::with_capacity((size * size * 4) as usize);
	let middle = size as f32 / 2.0;
	for y in 0..size {
		for x in 0..size {
			let distance = ((x as f32 - middle).powi(2) + (y as f32 - middle).powi(2)).sqrt();
			let ring = (1.0 - (distance - middle * 0.72).abs() / (middle * 0.12)).clamp(0.0, 1.0);
			let dot = (middle * 0.3 - distance).clamp(0.0, 1.0);
			let alpha = (ring.max(dot) * 255.0) as u8;
			let pink = [255, 120 + (x * 100 / size) as u8, 190, alpha];
			rgba.extend_from_slice(&pink);
		}
	}
	Frame {
		width: size,
		height: size,
		rgba,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn picture(width: u32, height: u32, paint: impl Fn(u32, u32) -> [u8; 4]) -> Frame {
		let mut rgba = Vec::new();
		for y in 0..height {
			for x in 0..width {
				rgba.extend_from_slice(&paint(x, y));
			}
		}
		Frame {
			width,
			height,
			rgba,
		}
	}

	#[test]
	fn the_plain_background_goes_and_the_same_colour_inside_the_subject_stays() {
		// White around a black ring, white again inside the ring.
		let frame = picture(40, 40, |x, y| {
			let (dx, dy) = (x as i32 - 20, y as i32 - 20);
			let ring = (36..=100).contains(&(dx * dx + dy * dy));
			if ring {
				[0, 0, 0, 255]
			} else {
				[250, 250, 250, 255]
			}
		});
		let key = background_key(&frame).expect("a plain border");
		let mask = cut_out(&frame, key);
		assert_eq!(mask[0], 0, "the corner is background");
		assert_eq!(mask[20 * 40 + 20], 255, "the white inside the ring stays");
		assert_eq!(mask[20 * 40 + 13], 255, "the ring stays");
		// A photo-like border has no key.
		let noise = picture(40, 40, |x, y| {
			[
				(x * 37 % 255) as u8,
				(y * 91 % 255) as u8,
				((x + y) * 53 % 255) as u8,
				255,
			]
		});
		assert!(background_key(&noise).is_none());
	}

	#[test]
	fn links_resolve_to_the_gif_itself() {
		let page =
			url::Url::parse("https://giphy.com/gifs/hello-kitty-wave-3o7TKSjRrfIPjeiVyM").unwrap();
		assert_eq!(
			giphy_media(&page).unwrap().as_str(),
			"https://i.giphy.com/media/3o7TKSjRrfIPjeiVyM/giphy.gif"
		);
		let sticker =
			url::Url::parse("https://giphy.com/stickers/kitty-l0MYt5jPR6QX5pnqM").unwrap();
		assert!(
			giphy_media(&sticker)
				.unwrap()
				.as_str()
				.contains("/l0MYt5jPR6QX5pnqM/")
		);
		let other = url::Url::parse("https://giphy.com/explore/cats").unwrap();
		assert!(giphy_media(&other).is_none());
		let html = r#"<html><body><img alt="x" src="https://media2.giphy.com/media/abc/giphy.gif?cid=1&amp;rid=giphy.gif"></body>"#;
		assert_eq!(
			image_source(html).unwrap().as_str(),
			"https://media2.giphy.com/media/abc/giphy.gif?cid=1&rid=giphy.gif"
		);
		let tenor = r#"<meta property="og:image" content="https://media.tenor.com/a/b.png"><meta property="og:image" content="https://media.tenor.com/a/b.gif">"#;
		assert_eq!(page_media(tenor).unwrap().path(), "/a/b.gif");
		assert!(image_source(r#"<img src="data:image/png;base64,AAAA">"#).is_none());
	}

	#[test]
	fn a_transparent_gif_keeps_its_see_through_pixels() {
		// Two frames of a pink dot on a transparent background, like a sticker GIF.
		let dot = |radius: u32| {
			image::RgbaImage::from_fn(32, 32, |x, y| {
				let (dx, dy) = (x as i32 - 16, y as i32 - 16);
				if (dx * dx + dy * dy) as u32 <= radius * radius {
					image::Rgba([255, 120, 190, 255])
				} else {
					image::Rgba([0, 0, 0, 0])
				}
			})
		};
		let mut gif = Vec::new();
		{
			let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
			encoder
				.set_repeat(image::codecs::gif::Repeat::Infinite)
				.unwrap();
			for radius in [8, 12] {
				let frame = image::Frame::from_parts(
					dot(radius),
					0,
					0,
					image::Delay::from_numer_denom_ms(100, 1),
				);
				encoder.encode_frame(frame).unwrap();
			}
		}
		let decoded = decode_image(&gif).expect("decodes");
		assert_eq!(decoded.len(), 2);
		assert_eq!(decoded[0].1.rgba[3], 0, "the corner stays see-through");
		let Source::Frames { opaque, .. } = frames(decoded) else {
			unreachable!()
		};
		assert!(!opaque, "opens without a backdrop");
	}

	#[test]
	fn see_through_frames_are_premultiplied_and_marked() {
		let half = picture(2, 1, |x, _| {
			if x == 0 {
				[200, 100, 50, 128]
			} else {
				[10, 10, 10, 255]
			}
		});
		let Source::Frames { frames, opaque, .. } = frames(vec![(Duration::ZERO, half)]) else {
			unreachable!()
		};
		assert!(!opaque);
		assert_eq!(&frames[0].1.rgba[..4], &[100, 50, 25, 128]);
	}
}
