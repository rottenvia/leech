//! Native theme drafts. Package and image IO belongs to the desktop worker.
use crate::{ExtensionRequest, design, dialog, icons};
use extensions::{
	Background, BackgroundFit, BackgroundTarget, ExtensionKind, Manifest, Package, SectionOpacity,
	SurfaceFit, SurfaceImage, Theme, ThemeSurface,
};
use std::sync::{
	Arc,
	atomic::{AtomicU64, Ordering},
};

/// Where a shared theme goes for review: GitHub forks the catalog repository for anyone
/// without write access and opens a pull request with the uploaded package.
const MARKETPLACE_UPLOAD: &str = "https://github.com/rottenvia/leech/upload/main/extensions/themes";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EditorTab {
	#[default]
	Basics,
	Background,
	Bars,
	Colors,
	Advanced,
}
impl EditorTab {
	const ALL: [Self; 5] = [
		Self::Basics,
		Self::Background,
		Self::Bars,
		Self::Colors,
		Self::Advanced,
	];
	#[cfg(feature = "demo")]
	fn label(self) -> &'static str {
		match self {
			Self::Basics => "Basics",
			Self::Background => "Background",
			Self::Bars => "Bars",
			Self::Colors => "Colors",
			Self::Advanced => "Advanced",
		}
	}
	fn key(self) -> &'static str {
		match self {
			Self::Basics => "theme-editor-tab-basics",
			Self::Background => "theme-editor-tab-background",
			Self::Bars => "theme-editor-tab-bars",
			Self::Colors => "theme-editor-tab-colors",
			Self::Advanced => "theme-editor-tab-advanced",
		}
	}
}

/// Name, location hint and a typical size in points of each bar, for bars not on screen.
fn surface_info(surface: ThemeSurface) -> (&'static str, &'static str, egui::Vec2) {
	match surface {
		ThemeSurface::CallStage => (
			"theme-editor-bar-call-stage",
			"theme-editor-bar-call-stage-where",
			egui::vec2(1100.0, 300.0),
		),
		ThemeSurface::Composer => (
			"theme-editor-bar-composer",
			"theme-editor-bar-composer-where",
			egui::vec2(1000.0, 48.0),
		),
		ThemeSurface::ChannelList => (
			"theme-editor-bar-channel-list",
			"theme-editor-bar-channel-list-where",
			egui::vec2(240.0, 640.0),
		),
		ThemeSurface::VoicePanel => (
			"theme-editor-bar-voice-panel",
			"theme-editor-bar-voice-panel-where",
			egui::vec2(270.0, 110.0),
		),
		ThemeSurface::AccountPanel => (
			"theme-editor-bar-account-panel",
			"theme-editor-bar-account-panel-where",
			egui::vec2(270.0, 52.0),
		),
		ThemeSurface::ServerList => (
			"theme-editor-bar-server-list",
			"theme-editor-bar-server-list-where",
			egui::vec2(72.0, 800.0),
		),
		ThemeSurface::TitleBar => (
			"theme-editor-bar-title-bar",
			"theme-editor-bar-title-bar-where",
			egui::vec2(1400.0, 36.0),
		),
		ThemeSurface::MemberList => (
			"theme-editor-bar-member-list",
			"theme-editor-bar-member-list-where",
			egui::vec2(240.0, 640.0),
		),
		ThemeSurface::MessageList => (
			"theme-editor-bar-message-list",
			"theme-editor-bar-message-list-where",
			egui::vec2(1000.0, 640.0),
		),
	}
}

/// The translation key naming `surface`.
pub(crate) fn surface_name(surface: ThemeSurface) -> &'static str {
	surface_info(surface).0
}

/// The picture size to aim for on `surface`: twice its size on screen, so it stays sharp in a
/// larger or maximized window.
fn aim_size(ctx: &egui::Context, surface: ThemeSurface) -> egui::Vec2 {
	let shown = design::surface_size(surface).unwrap_or(surface_info(surface).2);
	(shown * ctx.pixels_per_point() * 2.0)
		.min(egui::vec2(2560.0, 1440.0))
		.round()
}

/// The part of `image` a bar of `aspect` (width / height) keeps in view by default: the
/// window of the bar's shape along the picture's free axis that holds the most edge detail,
/// mildly pulled toward the middle, where subjects usually are.
fn smart_area(image: &egui::ColorImage, aspect: f32) -> [u16; 4] {
	let small = thumbnail_image(image, 96);
	let [width, height] = small.size;
	if width < 3 || height < 3 {
		return SurfaceImage::FULL;
	}
	let luma: Vec<f32> = small
		.pixels
		.iter()
		.map(|p| 0.299 * f32::from(p.r()) + 0.587 * f32::from(p.g()) + 0.114 * f32::from(p.b()))
		.collect();
	let (mut columns, mut rows) = (vec![0.0; width], vec![0.0; height]);
	for y in 1..height - 1 {
		for x in 1..width - 1 {
			let at = |dx: usize, dy: usize| luma[(y + dy - 1) * width + x + dx - 1];
			let gx = at(2, 0) + 2.0 * at(2, 1) + at(2, 2) - at(0, 0) - 2.0 * at(0, 1) - at(0, 2);
			let gy = at(0, 2) + 2.0 * at(1, 2) + at(2, 2) - at(0, 0) - 2.0 * at(1, 0) - at(2, 0);
			let edge = gx.hypot(gy);
			columns[x] += edge;
			rows[y] += edge;
		}
	}
	// The bar's shape in picture units: above one, the bar is wider than the picture.
	let ratio = aspect / (image.size[0] as f32 / image.size[1].max(1) as f32);
	if ratio >= 1.0 {
		let (top, bottom) = best_window(&rows, 1.0 / ratio);
		[0, top, 10_000, bottom]
	} else {
		let (left, right) = best_window(&columns, ratio);
		[left, 0, right, 10_000]
	}
}

/// The run covering `share` of `energy` that holds the most of it, favouring the middle; its
/// start and end in ten-thousandths.
fn best_window(energy: &[f32], share: f32) -> (u16, u16) {
	let count = energy.len();
	let len = ((count as f32 * share).round() as usize).clamp(1, count);
	let mut sum: f32 = energy[..len].iter().sum();
	let mut best = (f32::MIN, 0);
	for start in 0..=count - len {
		if start > 0 {
			sum += energy[start + len - 1] - energy[start - 1];
		}
		let offset = ((start as f32 + len as f32 / 2.0) / count as f32 - 0.5).abs();
		// The constant keeps a flat picture centred instead of stuck at the first window.
		let score = (sum + 1.0) * (1.0 - 0.6 * offset);
		if score > best.0 {
			best = (score, start);
		}
	}
	let size = (share.min(1.0) * 10_000.0).round().max(200.0);
	let start = (best.1 as f32 / count as f32 * 10_000.0)
		.round()
		.min(10_000.0 - size);
	(start as u16, (start + size) as u16)
}

/// The crop box after dragging `grip` (a corner clockwise from the top left, or 4 for the whole
/// box) by `delta` ten-thousandths, kept inside the picture.
fn drag_area(start: [u16; 4], grip: usize, delta: egui::Vec2) -> [u16; 4] {
	const MIN: f32 = 200.0;
	let [mut left, mut top, mut right, mut bottom] = start.map(f32::from);
	if grip == 4 {
		let dx = delta.x.max(-left).min(10_000.0 - right);
		let dy = delta.y.max(-top).min(10_000.0 - bottom);
		(left, right, top, bottom) = (left + dx, right + dx, top + dy, bottom + dy);
	} else {
		if matches!(grip, 0 | 3) {
			left = (left + delta.x).min(right - MIN).max(0.0);
		} else {
			right = (right + delta.x).max(left + MIN).min(10_000.0);
		}
		if grip < 2 {
			top = (top + delta.y).min(bottom - MIN).max(0.0);
		} else {
			bottom = (bottom + delta.y).max(top + MIN).min(10_000.0);
		}
	}
	[left, top, right, bottom].map(|value| value.round() as u16)
}

/// The crop that shows `view` (texture units) panned by `pan` and scaled by `zoom` around
/// `anchor`, inside the picture and no smaller than `min` on either axis.
fn reframe(
	view: egui::Rect,
	pan: egui::Vec2,
	zoom: f32,
	anchor: egui::Pos2,
	min: egui::Vec2,
) -> [u16; 4] {
	let size = view.size();
	let zoom = zoom
		.max((min.x / size.x).max(min.y / size.y))
		.min(1.0 / size.x.max(size.y));
	let size = size * zoom;
	let start = (anchor - (anchor - view.min) * zoom + pan).clamp(
		egui::Pos2::ZERO,
		(egui::Vec2::splat(1.0) - size)
			.max(egui::Vec2::ZERO)
			.to_pos2(),
	);
	let rect = egui::Rect::from_min_size(start, size);
	[rect.left(), rect.top(), rect.right(), rect.bottom()]
		.map(|value| (value * 10_000.0).round().clamp(0.0, 10_000.0) as u16)
}

/// A bar picture on its way into the draft: first the crop dialog, then the overlay in the
/// app. The package only changes once the user accepts the framing.
pub(crate) struct Framing {
	pub surface: ThemeSurface,
	/// A newly picked picture; `None` reframes the one already in the draft.
	bytes: Option<Vec<u8>>,
	image: Arc<egui::ColorImage>,
	pub area: [u16; 4],
	pub smart: [u16; 4],
	/// The overlay in the app is showing instead of the crop dialog.
	pub in_app: bool,
	texture: Option<egui::TextureHandle>,
	/// The crop dialog's drag: a corner clockwise from the top left or 4 for the whole box,
	/// and the box when it began.
	grip: Option<(usize, [u16; 4])>,
}

impl Framing {
	fn new(
		surface: ThemeSurface,
		bytes: Option<Vec<u8>>,
		image: Arc<egui::ColorImage>,
		area: [u16; 4],
	) -> Self {
		let shown = design::surface_size(surface).unwrap_or(surface_info(surface).2);
		let smart = smart_area(&image, shown.x / shown.y.max(1.0));
		Self {
			surface,
			area: if bytes.is_some() { smart } else { area },
			bytes,
			image,
			smart,
			in_app: false,
			texture: None,
			grip: None,
		}
	}
	/// Dim the app around the bar being framed and make the bar a handle: dragging moves the
	/// picture, scrolling or pinching zooms around the pointer. `top` is where the app starts
	/// under the framing bar.
	pub(crate) fn overlay(
		&mut self,
		ctx: &egui::Context,
		rect: egui::Rect,
		top: f32,
		accent: egui::Color32,
	) {
		let screen = ctx.content_rect();
		let below = egui::Rect::from_min_max(egui::pos2(screen.left(), top), screen.max);
		let painter = ctx.layer_painter(egui::LayerId::new(
			egui::Order::Foreground,
			egui::Id::unique("bar-framing-shade"),
		));
		for part in around(below, rect) {
			painter.rect_filled(part, 0, egui::Color32::from_black_alpha(110));
		}
		painter.rect_stroke(
			rect,
			0,
			egui::Stroke::new(2.0, accent),
			egui::StrokeKind::Inside,
		);
		let image = egui::vec2(self.image.size[0] as f32, self.image.size[1] as f32);
		let view = design::surface_uv(
			image,
			rect.size(),
			SurfaceImage {
				opacity: 100,
				fit: SurfaceFit::Cover,
				area: self.area,
			},
		);
		let response = egui::Area::new(egui::Id::unique("bar-framing-handle"))
			.order(egui::Order::Foreground)
			.fixed_pos(rect.min)
			.show(ctx, |ui| {
				ui.allocate_exact_size(rect.size(), egui::Sense::drag()).1
			})
			.inner;
		let pan = -response.drag_delta() / rect.size() * view.size();
		let mut zoom = 1.0;
		if response.hovered() || response.dragged() {
			zoom = ctx.input_mut(|input| {
				let factor = (input.smooth_scroll_delta.y * 0.002).exp() * input.zoom_delta();
				input.smooth_scroll_delta = egui::Vec2::ZERO;
				1.0 / factor
			});
			ctx.set_cursor_icon(if response.dragged() {
				egui::CursorIcon::Grabbing
			} else {
				egui::CursorIcon::Grab
			});
		}
		if pan != egui::Vec2::ZERO || zoom != 1.0 {
			let anchor = response.hover_pos().map_or(view.center(), |pointer| {
				view.min + (pointer - rect.min) / rect.size() * view.size()
			});
			// No closer than twice the picture's own pixels, so zooming never turns it to mush.
			let min = rect.size() * ctx.pixels_per_point() / (image * 2.0);
			self.area = reframe(view, pan, zoom, anchor, min.max(egui::Vec2::splat(0.01)));
		}
	}
}

/// The parts of `outer` around `hole`.
fn around(outer: egui::Rect, hole: egui::Rect) -> [egui::Rect; 4] {
	let hole = hole.intersect(outer);
	[
		egui::Rect::from_min_max(outer.min, egui::pos2(outer.max.x, hole.top())),
		egui::Rect::from_min_max(egui::pos2(outer.min.x, hole.bottom()), outer.max),
		egui::Rect::from_min_max(
			egui::pos2(outer.min.x, hole.top()),
			egui::pos2(hole.left(), hole.bottom()),
		),
		egui::Rect::from_min_max(
			egui::pos2(hole.right(), hole.top()),
			egui::pos2(outer.max.x, hole.bottom()),
		),
	]
}

/// A small copy of `image` for editor thumbnails, so a 4K bar picture costs no 4K texture.
fn thumbnail_image(image: &egui::ColorImage, max: usize) -> egui::ColorImage {
	let [width, height] = image.size;
	let step = width.max(height).div_ceil(max).max(1);
	let size = [width.div_ceil(step), height.div_ceil(step)];
	let pixels = (0..size[1])
		.flat_map(|y| (0..size[0]).map(move |x| (x, y)))
		.map(|(x, y)| image.pixels[(y * step).min(height - 1) * width + (x * step).min(width - 1)])
		.collect();
	egui::ColorImage::new(size, pixels)
}

const REGIONS: [ImageRegion; 6] = [
	ImageRegion::TopBars,
	ImageRegion::ServerList,
	ImageRegion::PeopleChannels,
	ImageRegion::MessageList,
	ImageRegion::MemberList,
	ImageRegion::InputArea,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ImageRegion {
	TopBars,
	ServerList,
	PeopleChannels,
	#[default]
	MessageList,
	MemberList,
	InputArea,
}
impl ImageRegion {
	fn label_key(self) -> &'static str {
		match self {
			Self::TopBars => "theme-editor-label-top-bars",
			Self::ServerList => "theme-editor-label-server-list",
			Self::PeopleChannels => "theme-editor-label-people-channels",
			Self::MessageList => "theme-editor-label-message-list",
			Self::MemberList => "theme-editor-label-member-list",
			Self::InputArea => "theme-editor-label-message-input-area",
		}
	}
	fn description_key(self) -> &'static str {
		match self {
			Self::TopBars => "theme-editor-description-window-title-and-conversation-header",
			Self::ServerList => "theme-editor-description-the-left-server-rail",
			Self::PeopleChannels => {
				"theme-editor-description-direct-messages-and-channel-navigation"
			}
			Self::MessageList => "theme-editor-description-the-conversation-timeline",
			Self::MemberList => "theme-editor-description-the-member-and-search-pane-on-the-right",
			Self::InputArea => "theme-editor-description-the-area-around-the-message-box",
		}
	}
	fn opacity(self, sections: &mut SectionOpacity) -> &mut u8 {
		match self {
			Self::TopBars => &mut sections.top_bar,
			Self::ServerList => &mut sections.server_list,
			Self::PeopleChannels => &mut sections.channel_list,
			Self::MessageList => &mut sections.message_list,
			Self::MemberList => &mut sections.member_list,
			Self::InputArea => &mut sections.composer,
		}
	}
}

pub(crate) struct ThemeEditor {
	pub package: Box<Package>,
	pub image: Option<Arc<egui::ColorImage>>,
	pub cover: Option<Arc<egui::ColorImage>>,
	pub dirty: bool,
	pub preview: bool,
	/// Which palette the editor changes; both are saved and applied per appearance.
	dark: bool,
	/// A newly opened editor starts on the palette the app is currently showing.
	dark_pending: bool,
	tab: EditorTab,
	region: ImageRegion,
	discard: bool,
	show_errors: bool,
	validation_error: Option<(EditorTab, &'static str)>,
	reveal_advanced_colors: bool,
	reveal_gradient: bool,
	/// Open state of the Advanced disclosures, so they survive a repaint.
	open_colors: bool,
	open_gradient: bool,
	open_effects: bool,
	open_metrics: bool,
	thumbnail: Option<egui::TextureHandle>,
	cover_thumbnail: Option<egui::TextureHandle>,
	/// Decoded bar images for the live preview and thumbnails; bytes live in the package.
	pub surfaces: design::SurfaceImages,
	surface_thumbnails: Vec<Option<egui::TextureHandle>>,
	pub framing: Option<Framing>,
}

fn identity() -> String {
	static NEXT: AtomicU64 = AtomicU64::new(0);
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_nanos();
	format!(
		"local-theme-{now:x}-{:x}",
		NEXT.fetch_add(1, Ordering::Relaxed)
	)
}

impl ThemeEditor {
	#[cfg(feature = "demo")]
	pub(crate) fn preview_tab(&mut self, label: &str) {
		if let Some(tab) = EditorTab::ALL
			.into_iter()
			.find(|tab| tab.label().eq_ignore_ascii_case(label))
		{
			self.tab = tab;
		}
	}
	pub fn new() -> Self {
		Self {
			package: Box::new(Package {
				manifest: Manifest {
					api_version: extensions::API_VERSION,
					id: identity(),
					name: "My theme".into(),
					version: "1.0.0".into(),
					author: String::new(),
					license: "CC0-1.0".into(),
					source: String::new(),
					kind: ExtensionKind::Theme,
					capabilities: vec![],
					actions: vec![],
				},
				theme: Some(Theme::default()),
				wasm: vec![],
				background_image: vec![],
				cover_image: vec![],
				surface_images: Default::default(),
			}),
			image: None,
			cover: None,
			dirty: false,
			preview: false,
			dark: true,
			dark_pending: true,
			tab: EditorTab::Basics,
			region: ImageRegion::MessageList,
			discard: false,
			show_errors: false,
			validation_error: None,
			reveal_advanced_colors: false,
			reveal_gradient: false,
			open_colors: false,
			open_gradient: false,
			open_effects: false,
			open_metrics: false,
			thumbnail: None,
			cover_thumbnail: None,
			surfaces: Default::default(),
			surface_thumbnails: vec![None; ThemeSurface::ALL.len()],
			framing: None,
		}
	}
	pub fn edit(
		package: Box<Package>,
		image: Option<Arc<egui::ColorImage>>,
		cover: Option<Arc<egui::ColorImage>>,
		surfaces: design::SurfaceImages,
	) -> Self {
		Self {
			package,
			image,
			cover,
			surfaces,
			..Self::new()
		}
	}
	pub fn duplicate(
		mut package: Box<Package>,
		image: Option<Arc<egui::ColorImage>>,
		cover: Option<Arc<egui::ColorImage>>,
		surfaces: design::SurfaceImages,
	) -> Self {
		package.manifest.id = identity();
		package.manifest.name = format!(
			"{} copy",
			package.manifest.name.chars().take(30).collect::<String>()
		);
		let mut editor = Self::edit(package, image, cover, surfaces);
		editor.dirty = true;
		editor
	}
	pub fn receive_cover(&mut self, bytes: Vec<u8>, image: Arc<egui::ColorImage>) {
		self.package.cover_image = bytes;
		self.cover = Some(image);
		self.cover_thumbnail = None;
		self.dirty = true;
	}
	pub fn receive_image(&mut self, bytes: Vec<u8>, image: Arc<egui::ColorImage>) {
		self.package.background_image = bytes;
		self.image = Some(image);
		self.thumbnail = None;
		self.dirty = true;
		if let Some(theme) = self.package.theme.as_mut() {
			for palette in [&mut theme.light, &mut theme.dark] {
				let background = palette.background.get_or_insert(Background::default());
				background.opacity = 100;
				background.target = BackgroundTarget::Window;
				background.sections.get_or_insert_default();
			}
		}
	}
	/// A picked bar picture opens the crop dialog on its smart crop; tiles and stretched
	/// pictures have nothing to frame and go straight into the draft.
	pub fn receive_surface(
		&mut self,
		surface: ThemeSurface,
		bytes: Vec<u8>,
		image: Arc<egui::ColorImage>,
	) {
		let cover = self
			.package
			.theme
			.as_ref()
			.and_then(|theme| theme.surfaces.get(&surface))
			.is_none_or(|settings| settings.fit == SurfaceFit::Cover);
		self.framing = Some(Framing::new(
			surface,
			Some(bytes),
			image,
			SurfaceImage::FULL,
		));
		if !cover {
			self.accept_framing();
		}
	}
	/// Put the framed picture and its crop into the draft.
	pub(crate) fn accept_framing(&mut self) {
		let Some(framing) = self.framing.take() else {
			return;
		};
		let surface = framing.surface;
		if let Some(bytes) = framing.bytes {
			self.package.surface_images.insert(surface, bytes);
			let mut surfaces = (*self.surfaces).clone();
			surfaces.insert(surface, framing.image);
			self.surfaces = Arc::new(surfaces);
			self.surface_thumbnails[surface as usize] = None;
		}
		if let Some(theme) = self.package.theme.as_mut() {
			theme.surfaces.entry(surface).or_default().area = framing.area;
		}
		self.dirty = true;
	}
	/// What the app paints on the bar being framed, ahead of the theme it will be saved to.
	pub(crate) fn framing_override(&self) -> Option<(ThemeSurface, SurfaceImage)> {
		let framing = self
			.framing
			.as_ref()
			.filter(|framing| framing.in_app && self.preview)?;
		let settings = self
			.package
			.theme
			.as_ref()
			.and_then(|theme| theme.surfaces.get(&framing.surface).copied())
			.unwrap_or_default();
		Some((
			framing.surface,
			SurfaceImage {
				fit: SurfaceFit::Cover,
				area: framing.area,
				..settings
			},
		))
	}
	pub fn preview_request(&self) -> ExtensionRequest {
		let mut theme = self.package.theme.clone();
		let mut surfaces = self.surfaces.clone();
		if let Some(framing) = &self.framing {
			if let Some(theme) = theme.as_mut() {
				theme.surfaces.entry(framing.surface).or_default().area = framing.area;
			}
			if framing.bytes.is_some() {
				let mut images = (*surfaces).clone();
				images.insert(framing.surface, framing.image.clone());
				surfaces = Arc::new(images);
			}
		}
		ExtensionRequest::PreviewTheme {
			theme: theme.map(Box::new),
			image: self.image.clone(),
			surfaces,
		}
	}
	/// Queue a save of the draft, or open the first thing that blocks it.
	pub(crate) fn request_save(&mut self, requests: &mut Vec<ExtensionRequest>) -> bool {
		self.show_errors = true;
		if self.ready_to_save() {
			self.validation_error = None;
			requests.push(ExtensionRequest::SaveTheme {
				package: self.package.clone(),
			});
			return true;
		}
		self.reveal_save_error();
		false
	}
	fn reveal_save_error(&mut self) {
		let (tab, message, dark, reveal_colors) = self.save_error();
		self.tab = tab;
		if let Some(dark) = dark {
			self.dark = dark;
		}
		self.reveal_advanced_colors = reveal_colors;
		self.reveal_gradient = self.invalid_gradient();
		self.validation_error = Some((tab, message));
	}
	pub fn tab_key(&self) -> u8 {
		self.tab as u8
	}
	fn follow_appearance(&mut self, ui: &egui::Ui) {
		if std::mem::take(&mut self.dark_pending) {
			self.dark = ui.visuals().dark_mode;
		}
	}
	fn ready_to_save(&self) -> bool {
		let manifest = &self.package.manifest;
		[
			&manifest.name,
			&manifest.author,
			&manifest.license,
			&manifest.version,
		]
		.into_iter()
		.all(|value| !value.trim().is_empty())
			&& self.package.validate().is_ok()
	}
	fn invalid_gradient(&self) -> bool {
		self.package.theme.as_ref().is_some_and(|theme| {
			[&theme.light, &theme.dark].into_iter().any(|palette| {
				palette.backdrop.as_ref().is_some_and(|stops| {
					stops
						.iter()
						.any(|stop| extensions::parse_color(stop).is_err())
				})
			})
		})
	}
	fn save_error(&self) -> (EditorTab, &'static str, Option<bool>, bool) {
		let manifest = &self.package.manifest;
		if manifest.name.trim().is_empty() || manifest.author.trim().is_empty() {
			return (
				EditorTab::Basics,
				"Add a theme name and creator name before saving.",
				None,
				false,
			);
		}
		if manifest.license.trim().is_empty() || manifest.version.trim().is_empty() {
			return (
				EditorTab::Advanced,
				"Add a license and version before saving.",
				None,
				false,
			);
		}
		if manifest.validate().is_err() {
			return (
				EditorTab::Advanced,
				"Check the license, version, and optional source URL.",
				None,
				false,
			);
		}
		if let Some(theme) = &self.package.theme {
			for (dark, palette) in [(false, &theme.light), (true, &theme.dark)] {
				if let Some((name, _)) = palette
					.colors
					.iter()
					.find(|(_, color)| extensions::parse_color(color).is_err())
				{
					let basic =
						["chat", "accent", "text", "muted", "sidebar"].contains(&name.as_str());
					return (
						if basic {
							EditorTab::Colors
						} else {
							EditorTab::Advanced
						},
						"Correct the highlighted color value.",
						Some(dark),
						!basic,
					);
				}
				if palette.background.is_some_and(|background| {
					background.opacity > 100
						|| background.sections.is_some_and(|sections| {
							[
								sections.top_bar,
								sections.server_list,
								sections.channel_list,
								sections.message_list,
								sections.member_list,
								sections.composer,
							]
							.into_iter()
							.any(|opacity| opacity > 100)
						})
				}) {
					return (
						EditorTab::Background,
						"Keep image and section opacity between 0% and 100%.",
						Some(dark),
						false,
					);
				}
				if palette.backdrop.as_ref().is_some_and(|stops| {
					stops
						.iter()
						.any(|stop| extensions::parse_color(stop).is_err())
				}) {
					return (
						EditorTab::Advanced,
						"Correct the highlighted gradient value.",
						Some(dark),
						false,
					);
				}
			}
		}
		(
			EditorTab::Advanced,
			"Check the remaining theme settings before saving.",
			None,
			false,
		)
	}
	/// The settings shell keeps these actions outside its scrolling content.
	pub fn toolbar(
		&mut self,
		ui: &mut egui::Ui,
		busy: bool,
		requests: &mut Vec<ExtensionRequest>,
	) -> bool {
		self.follow_appearance(ui);
		let mut close = false;
		ui.add_enabled_ui(!busy, |ui| {
			ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
			ui.horizontal_wrapped(|ui| {
				if dialog::action(ui, "theme-editor-toolbar-back", dialog::Action::Outline)
					.clicked()
				{
					if self.dirty {
						self.discard = true;
					} else {
						close = true;
					}
				}
				if self.dirty {
					ui.label(
						egui::RichText::new(crate::i18n::translate(
							"theme-editor-toolbar-unsaved-changes",
						))
						.size(12.0)
						.color(design::palette(ui).warning),
					);
				}
				if ui.max_rect().width() >= 600.0 {
					ui.add_space((ui.available_size_before_wrap().x - 278.0).max(0.0));
				}
				let valid = self
					.package
					.theme
					.as_ref()
					.is_some_and(|theme| theme.validate().is_ok());
				ui.add_enabled_ui(valid, |ui| {
					if dialog::action(
						ui,
						"theme-editor-toolbar-preview-in-app",
						dialog::Action::Outline,
					)
					.clicked()
					{
						self.preview = true;
						requests.push(self.preview_request());
					}
				});
				if dialog::action(
					ui,
					"theme-editor-toolbar-save-and-apply",
					dialog::Action::Primary,
				)
				.clicked()
				{
					self.request_save(requests);
				}
			});
		});
		if busy {
			ui.horizontal(|ui| {
				ui.spacing_mut().item_spacing.x = 8.0;
				ui.add(egui::Spinner::new().size(14.0));
				ui.label(
					egui::RichText::new(crate::i18n::translate("theme-editor-toolbar-working"))
						.size(12.0)
						.color(design::palette(ui).muted),
				);
			});
		}
		ui.add_space(8.0);
		ui.horizontal_wrapped(|ui| {
			ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
			let labels: Vec<&str> = EditorTab::ALL.iter().map(|tab| tab.key()).collect();
			let current = EditorTab::ALL
				.iter()
				.position(|tab| *tab == self.tab)
				.unwrap_or_default();
			if let Some(index) = design::segmented(ui, &labels, current) {
				self.tab = EditorTab::ALL[index];
			}
			// Bar images are shared by both appearances, so that tab has no switch either.
			if !matches!(self.tab, EditorTab::Basics | EditorTab::Bars) {
				if ui.available_size_before_wrap().x >= 230.0 {
					ui.add_space((ui.available_size_before_wrap().x - 220.0).max(0.0));
				}
				appearance_switch(ui, &mut self.dark);
			}
		});
		ui.add_space(4.0);
		design::card_divider(ui);
		close
	}

	pub fn show(
		&mut self,
		ui: &mut egui::Ui,
		busy: bool,
		requests: &mut Vec<ExtensionRequest>,
	) -> bool {
		self.follow_appearance(ui);
		let mut changed = false;
		ui.add_enabled_ui(!busy, |ui| {
			ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
			if let Some((_, message)) = self.validation_error.filter(|(tab, _)| *tab == self.tab) {
				design::notice(ui, design::Level::Error, message);
				ui.add_space(12.0);
			}
			match self.tab {
				EditorTab::Basics => {
					design::section(
						ui,
						&crate::i18n::translate("theme-editor-show-theme-details"),
						Some(&crate::i18n::translate(
							"theme-editor-show-how-your-theme-appears-in-the-gallery",
						)),
					);
					let show_errors = self.show_errors;
					design::card(ui, |ui| {
						let manifest = &mut self.package.manifest;
						changed |= text_field(
							ui,
							"theme-editor-show-theme-name",
							&mut manifest.name,
							32,
							"My theme",
						);
						if show_errors && manifest.name.trim().is_empty() {
							design::notice(
								ui,
								design::Level::Error,
								&crate::i18n::translate("theme-editor-show-theme-name-is-required"),
							);
						}
						changed |= text_field(
							ui,
							"theme-editor-show-created-by",
							&mut manifest.author,
							32,
							"Your name",
						);
						if show_errors && manifest.author.trim().is_empty() {
							design::notice(
								ui,
								design::Level::Error,
								&crate::i18n::translate(
									"theme-editor-show-creator-name-is-required",
								),
							);
						}
					});
					ui.add_space(20.0);
					design::section(
						ui,
						&crate::i18n::translate("theme-editor-show-card-cover"),
						Some(&crate::i18n::translate(
							"theme-editor-show-choose-the-image-shown-on-your-theme-card-in-themes",
						)),
					);
					self.cover_card(ui, requests, &mut changed);
				}
				EditorTab::Background => {
					design::section(
						ui,
						&crate::i18n::translate("theme-editor-show-app-background"),
						Some(&crate::i18n::translate(
							"theme-editor-show-use-one-image-behind-your-conversations-and-sidebars",
						)),
					);
					self.image_card(ui, requests, &mut changed);
					if !self.package.background_image.is_empty() {
						let theme = self
							.package
							.theme
							.as_mut()
							.expect("theme editor always holds a theme");
						let palette = if self.dark {
							&mut theme.dark
						} else {
							&mut theme.light
						};
						let background = palette.background.get_or_insert(Background {
							opacity: 100,
							sections: Some(SectionOpacity::default()),
							..Default::default()
						});
						if background.sections.is_none() {
							design::hint(
								ui,
								&crate::i18n::translate(
									"theme-editor-show-this-older-theme-uses-its-original-image-placement",
								),
							);
							if dialog::action(
								ui,
								"theme-editor-show-use-image-across-the-app",
								dialog::Action::Outline,
							)
							.clicked()
							{
								background.target = BackgroundTarget::Window;
								background.opacity = 100;
								background.sections = Some(SectionOpacity::default());
								changed = true;
							} else {
								changed |= design::slider_row(
									ui,
									"theme-editor-show-image-opacity",
									None,
									&mut background.opacity,
									0..=100,
									"%",
								)
								.changed();
							}
						}
						changed |= row(ui, "theme-editor-show-image-fit", |ui| {
							let mut changed = false;
							crate::design::combo("image-fit")
								.selected_text(crate::i18n::translate_if_key(
									&(match background.fit {
										BackgroundFit::Cover => {
											crate::i18n::translate("theme-editor-show-fill-area")
										}
										BackgroundFit::Contain => crate::i18n::translate(
											"theme-editor-show-fit-entire-image",
										),
									}),
								))
								.show_ui(ui, |ui| {
									changed |= ui
										.selectable_value(
											&mut background.fit,
											BackgroundFit::Cover,
											crate::i18n::translate("theme-editor-show-fill-area"),
										)
										.changed();
									changed |= ui
										.selectable_value(
											&mut background.fit,
											BackgroundFit::Contain,
											crate::i18n::translate(
												"theme-editor-show-fit-entire-image",
											),
										)
										.changed();
								});
							changed
						});
						if let Some(sections) = &mut background.sections {
							ui.add_space(16.0);
							design::section(
								ui,
								&crate::i18n::translate("theme-editor-show-section-opacity"),
								Some(
									"theme-editor-show-select-an-area-then-choose-how-much-of-the-image",
								),
							);
							let base = design::builtin_colors(self.dark, design::variant());
							let map_colors = map_palette(base, &palette.colors);
							let fit = background.fit;
							changed |= section_map(
								ui,
								self.thumbnail.as_ref(),
								&mut self.region,
								map_colors,
								fit,
								sections,
							);
						}
					} else {
						design::hint(
							ui,
							&crate::i18n::translate(
								"theme-editor-show-choose-an-image-to-adjust-the-top-bar-lists-and",
							),
						);
					}
				}
				EditorTab::Bars => self.bars(ui, requests, &mut changed),
				EditorTab::Colors => {
					design::section(
						ui,
						&crate::i18n::translate("theme-editor-show-conversation-colors"),
						Some(&crate::i18n::translate(
							"theme-editor-show-click-a-swatch-to-choose-a-color-or-enter-its",
						)),
					);
					let theme = self
						.package
						.theme
						.as_mut()
						.expect("theme editor always holds a theme");
					let palette = if self.dark {
						&mut theme.dark
					} else {
						&mut theme.light
					};
					let base = design::builtin_colors(self.dark, design::variant());
					design::card(ui, |ui| {
						for (key, fallback) in [
							("accent", base.accent),
							("text", base.text),
							("muted", base.muted),
							("sidebar", base.sidebar),
							("chat", base.chat),
						] {
							changed |= color_override(ui, key, &mut palette.colors, fallback);
						}
					});
					if design::primary_color().is_some() {
						ui.add_space(12.0);
						design::hint(
							ui,
							&crate::i18n::translate(
								"theme-editor-show-your-primary-color-in-appearance-takes-precedence-over-this-accent",
							),
						);
					}
				}
				EditorTab::Advanced => {
					design::section(
						ui,
						&crate::i18n::translate("theme-editor-show-advanced"),
						Some(&crate::i18n::translate(
							"theme-editor-show-additional-colors-app-controls-and-sharing-details",
						)),
					);
					{
						let theme = self
							.package
							.theme
							.as_mut()
							.expect("theme editor always holds a theme");
						let palette = if self.dark {
							&mut theme.dark
						} else {
							&mut theme.light
						};
						let base = design::builtin_colors(self.dark, design::variant());
						self.open_colors |= std::mem::take(&mut self.reveal_advanced_colors);
						if design::disclosure(
							ui,
							&crate::i18n::translate("theme-editor-show-more-colors"),
							self.open_colors,
						)
						.clicked()
						{
							self.open_colors = !self.open_colors;
						}
						if self.open_colors {
							design::card(ui, |ui| {
								for (key, fallback) in colors(base) {
									if !["chat", "accent", "text", "muted", "sidebar"]
										.contains(&key)
									{
										changed |=
											color_override(ui, key, &mut palette.colors, fallback);
									}
								}
							});
						}
						ui.add_space(12.0);
						self.open_gradient |= std::mem::take(&mut self.reveal_gradient);
						if design::disclosure(
							ui,
							&crate::i18n::translate("theme-editor-show-window-gradient"),
							self.open_gradient,
						)
						.clicked()
						{
							self.open_gradient = !self.open_gradient;
						}
						if self.open_gradient {
							design::card(ui, |ui| {
								let mut enabled = palette.backdrop.is_some();
								if design::switch(
									ui,
									"theme-editor-show-use-a-gradient",
									Some(
										"theme-editor-show-blend-two-colors-behind-the-app-s-surfaces",
									),
									&mut enabled,
								)
								.changed()
								{
									palette.backdrop =
										enabled.then(|| [hex(base.base), hex(base.chat)]);
									changed = true;
								}
								if let Some(stops) = &mut palette.backdrop {
									for (index, stop) in stops.iter_mut().enumerate() {
										changed |= row(
											ui,
											if index == 0 {
												"Start color"
											} else {
												"End color"
											},
											|ui| color_input(ui, stop),
										);
										if extensions::parse_color(stop).is_err() {
											design::notice(
												ui,
												design::Level::Error,
												&crate::i18n::translate(
													"theme-editor-show-use-rrggbb-or-rrggbbaa",
												),
											);
										}
									}
								}
							});
						}
						ui.add_space(12.0);
						if design::disclosure(
							ui,
							&crate::i18n::translate("theme-editor-show-window-effects"),
							self.open_effects,
						)
						.clicked()
						{
							self.open_effects = !self.open_effects;
						}
						if self.open_effects {
							design::card(ui, |ui| {
								design::hint(
									ui,
									&crate::i18n::translate(
										"theme-editor-show-requires-transparency-blur-in-appearance-then-an-app-restart",
									),
								);
								let style = &mut theme.style;
								let defaults = design::default_window_effects();
								let mut transparency =
									style.transparency_blur.unwrap_or(defaults.0);
								if design::switch(
									ui,
									"theme-editor-show-transparency-blur",
									Some(
										"theme-editor-show-override-the-default-appearance-setting-for-this-theme",
									),
									&mut transparency,
								)
								.changed()
								{
									style.transparency_blur = Some(transparency);
									if transparency {
										style.transparency.get_or_insert(defaults.1);
										style.blur.get_or_insert(defaults.2);
									}
									changed = true;
								}
								if transparency {
									let mut amount = style.transparency.unwrap_or(defaults.1);
									let mut blur = style.blur.unwrap_or(defaults.2);
									let mut effects_changed = design::slider_row(
										ui,
										"theme-editor-show-transparency",
										None,
										&mut amount,
										0..=100,
										"%",
									)
									.changed();
									ui.add_space(8.0);
									effects_changed |= design::blur_control(
										ui,
										"theme-editor-show-blur",
										"theme-editor-show-the-system-applies-its-standard-blur-strength",
										&mut blur,
									)
									.changed();
									if effects_changed {
										style.transparency = Some(amount);
										style.blur = Some(blur);
										changed = true;
									}
								}
							});
						}
						ui.add_space(12.0);
						if design::disclosure(
							ui,
							&crate::i18n::translate("theme-editor-show-text-spacing-corners"),
							self.open_metrics,
						)
						.clicked()
						{
							self.open_metrics = !self.open_metrics;
						}
						if self.open_metrics {
							design::card(ui, |ui| {
								design::hint(
									ui,
									&crate::i18n::translate(
										"theme-editor-show-these-settings-apply-to-dark-and-light-appearances",
									),
								);
								let style = &mut theme.style;
								for (label, value, default, min, max) in [
									("Body text", &mut style.body_size, 15, 10, 28),
									("Headings", &mut style.heading_size, 20, 12, 40),
									("Buttons", &mut style.button_size, 14, 10, 28),
									("Small text", &mut style.small_size, 12, 10, 28),
									("Code", &mut style.monospace_size, 14, 10, 28),
									("Control height", &mut style.control_height, 32, 24, 56),
								] {
									changed |= metric(ui, label, value, default, min..=max);
								}
								changed |= pair_metric(
									ui,
									"Item spacing",
									&mut style.item_spacing,
									[8, 8],
								);
								changed |= pair_metric(
									ui,
									"Button padding",
									&mut style.button_padding,
									[12, 6],
								);
								for (label, value, default) in [
									("Control corners", &mut style.widget_radius, 10),
									("Window corners", &mut style.window_radius, 14),
									("Menu corners", &mut style.menu_radius, 12),
								] {
									changed |= metric(ui, label, value, default, 0..=24);
								}
							});
						}
					}
					ui.add_space(12.0);
					design::section(
						ui,
						&crate::i18n::translate("theme-editor-show-sharing-export"),
						Some(
							"theme-editor-show-the-license-and-version-are-required-a-source-url-is",
						),
					);
					design::card(ui, |ui| {
						let manifest = &mut self.package.manifest;
						changed |= text_field(
							ui,
							"theme-editor-show-license",
							&mut manifest.license,
							32,
							"CC0-1.0",
						);
						changed |= text_field(
							ui,
							"theme-editor-show-version",
							&mut manifest.version,
							32,
							"1.0.0",
						);
						changed |= text_field(
							ui,
							"theme-editor-show-source-url",
							&mut manifest.source,
							512,
							"Optional",
						);
						if self.show_errors
							&& !manifest.source.is_empty()
							&& [
								&manifest.name,
								&manifest.author,
								&manifest.license,
								&manifest.version,
							]
							.into_iter()
							.all(|value| !value.trim().is_empty())
							&& manifest.validate().is_err()
						{
							design::notice(
								ui,
								design::Level::Error,
								&crate::i18n::translate(
									"theme-editor-show-use-a-valid-https-source-url-or-leave-this-blank",
								),
							);
						}
						if self.show_errors
							&& (manifest.license.trim().is_empty()
								|| manifest.version.trim().is_empty())
						{
							design::notice(
								ui,
								design::Level::Error,
								&crate::i18n::translate(
									"theme-editor-show-license-and-version-are-required",
								),
							);
						}
						design::hint(
							ui,
							&crate::i18n::translate(
								"theme-editor-show-only-share-images-you-own-or-have-permission-to-use",
							),
						);
						ui.add_space(8.0);
						let has_source = !self.package.manifest.source.trim().is_empty();
						let (export, publish) = ui
							.horizontal_wrapped(|ui| {
								let export = dialog::action(
									ui,
									"theme-editor-show-export-theme",
									dialog::Action::Outline,
								)
								.clicked();
								let publish = ui
									.add_enabled_ui(has_source, |ui| {
										dialog::action(
											ui,
											"theme-editor-publish",
											dialog::Action::Outline,
										)
									})
									.inner
									.on_disabled_hover_text(crate::i18n::translate(
										"theme-editor-publish-needs-source",
									))
									.clicked();
								(export, publish)
							})
							.inner;
						design::hint(ui, &crate::i18n::translate("theme-editor-publish-hint"));
						if export || publish {
							self.show_errors = true;
							if self.ready_to_save() {
								requests.push(ExtensionRequest::ExportTheme {
									package: self.package.clone(),
								});
								if publish {
									ui.ctx()
										.open_url(egui::OpenUrl::new_tab(MARKETPLACE_UPLOAD));
								}
							} else {
								self.reveal_save_error();
							}
						}
					});
				}
			}
			self.dirty |= changed;
			if changed {
				if self.ready_to_save()
					|| self.validation_error.is_some_and(|issue| {
						let (tab, message, _, _) = self.save_error();
						issue != (tab, message)
					}) {
					self.validation_error = None;
				}
				if self.preview {
					requests.push(self.preview_request());
				}
			}
		});
		self.framing_dialog(ui.ctx(), requests);
		if self.discard {
			match dialog::Confirm::new(
				"discard-theme-draft",
				crate::i18n::translate("theme-editor-discard-title"),
				crate::i18n::translate("theme-editor-discard-detail"),
			)
			.confirm_label(crate::i18n::translate("theme-editor-discard-confirm"))
			.cancel_label(crate::i18n::translate("theme-editor-discard-keep"))
			.danger()
			.show(ui.ctx())
			{
				Some(dialog::Choice::Confirmed) => return true,
				Some(dialog::Choice::Cancelled) => self.discard = false,
				None => {}
			}
		}
		false
	}

	fn cover_card(
		&mut self,
		ui: &mut egui::Ui,
		requests: &mut Vec<ExtensionRequest>,
		changed: &mut bool,
	) {
		if self.cover_thumbnail.is_none()
			&& let Some(image) = &self.cover
			&& image
				.size
				.iter()
				.all(|side| *side <= ui.ctx().input(|input| input.max_texture_side))
		{
			self.cover_thumbnail = Some(ui.ctx().load_texture(
				"theme-cover-thumbnail",
				image.clone(),
				egui::TextureOptions::LINEAR,
			));
		}
		design::card(ui, |ui| {
			ui.horizontal_wrapped(|ui| {
				let (rect, _) =
					ui.allocate_exact_size(egui::vec2(112.0, 63.0), egui::Sense::hover());
				let colors = design::palette(ui);
				ui.painter().rect_filled(rect, 6, colors.base);
				if let Some(texture) = &self.cover_thumbnail {
					design::paint_background_image(
						ui.painter(),
						rect,
						texture,
						Background {
							opacity: 100,
							..Default::default()
						},
					);
				} else {
					crate::icons::paint(
						ui.painter(),
						crate::icons::Icon::Image,
						rect.shrink(20.0),
						colors.muted,
					);
				}
				ui.add_space(8.0);
				ui.vertical(|ui| {
					ui.label(design::medium(
						ui,
						crate::i18n::translate_if_key(if self.cover.is_some() {
							"theme-editor-cover-card-custom-cover"
						} else {
							"theme-editor-cover-card-automatic-preview"
						}),
						14.0,
					));
					ui.horizontal_wrapped(|ui| {
						if dialog::action(
							ui,
							if self.cover.is_some() {
								"theme-editor-cover-card-replace-cover"
							} else {
								"theme-editor-cover-card-choose-cover"
							},
							dialog::Action::Outline,
						)
						.clicked()
						{
							requests.push(ExtensionRequest::PickThemeCover);
						}
						if self.cover.is_some()
							&& dialog::action(
								ui,
								"theme-editor-cover-card-remove",
								dialog::Action::Neutral,
							)
							.clicked()
						{
							self.package.cover_image.clear();
							self.cover = None;
							self.cover_thumbnail = None;
							*changed = true;
						}
					});
				});
			});
		});
		design::hint(
			ui,
			&crate::i18n::translate(
				"theme-editor-cover-card-png-or-jpeg-up-to-2-mib-this-image-does",
			),
		);
	}

	/// The crop dialog after a pick: the whole picture with a box over the part that must stay
	/// in view, what the bar shows in this window dashed, and what makes a good picture.
	fn framing_dialog(&mut self, ctx: &egui::Context, requests: &mut Vec<ExtensionRequest>) {
		let Some(framing) = self.framing.as_mut().filter(|framing| !framing.in_app) else {
			return;
		};
		let (name, _, typical) = surface_info(framing.surface);
		let shown = design::surface_size(framing.surface).unwrap_or(typical);
		let aim = aim_size(ctx, framing.surface);
		let image = framing.image.clone();
		let texture = framing
			.texture
			.get_or_insert_with(|| {
				ctx.load_texture(
					"bar-framing",
					thumbnail_image(&image, 1024),
					egui::TextureOptions::LINEAR,
				)
			})
			.clone();
		let picture = egui::vec2(image.size[0] as f32, image.size[1] as f32);
		let max_height = (ctx.content_rect().height() - 360.0).clamp(160.0, 420.0);
		let (mut proceed, mut cancel) = (false, false);
		let response = dialog::Dialog::new(
			"bar-framing",
			crate::i18n::translate("theme-editor-framing-title"),
		)
		.subtitle(format!(
			"{} · {}",
			crate::i18n::translate(name),
			crate::i18n::translate("theme-editor-framing-subtitle")
		))
		.icon(icons::Icon::Image)
		.width(680.0)
		.show(ctx, |body| {
			body.content(|ui| {
				let colors = design::palette(ui);
				// A margin keeps the corner handles whole at the picture's edges.
				let scale = ((ui.available_width() - 16.0) / picture.x).min(max_height / picture.y);
				let (slot, _) = ui.allocate_exact_size(
					egui::vec2(ui.available_width(), picture.y * scale + 16.0),
					egui::Sense::hover(),
				);
				let canvas = egui::Rect::from_center_size(slot.center(), picture * scale);
				let response = ui.interact(
					canvas.expand(8.0),
					ui.scope_id().with("framing-canvas"),
					egui::Sense::drag(),
				);
				let to_screen = |area: [u16; 4]| {
					let [left, top, right, bottom] = area.map(|value| f32::from(value) / 10_000.0);
					egui::Rect::from_min_max(
						canvas.lerp_inside(egui::vec2(left, top)),
						canvas.lerp_inside(egui::vec2(right, bottom)),
					)
				};
				let corners = |rect: egui::Rect| {
					[
						rect.left_top(),
						rect.right_top(),
						rect.right_bottom(),
						rect.left_bottom(),
					]
				};
				let corner = response
					.hover_pos()
					.or(response.interact_pointer_pos())
					.and_then(|pointer| {
						corners(to_screen(framing.area))
							.iter()
							.position(|corner| corner.distance(pointer) <= 14.0)
					});
				if response.drag_started() {
					framing.grip = Some((corner.unwrap_or(4), framing.area));
				}
				if let Some((grip, start)) = framing.grip
					&& let (Some(origin), Some(pointer)) = (
						ui.input(|input| input.pointer.press_origin()),
						response.interact_pointer_pos(),
					) {
					framing.area =
						drag_area(start, grip, (pointer - origin) / canvas.size() * 10_000.0);
				}
				if response.drag_stopped() {
					framing.grip = None;
				}
				if response.hovered() || framing.grip.is_some() {
					ui.ctx()
						.set_cursor_icon(match framing.grip.map(|(grip, _)| grip).or(corner) {
							Some(0 | 2) => egui::CursorIcon::ResizeNwSe,
							Some(1 | 3) => egui::CursorIcon::ResizeNeSw,
							_ if framing.grip.is_some() => egui::CursorIcon::Grabbing,
							_ => egui::CursorIcon::Grab,
						});
				}
				let boxed = to_screen(framing.area);
				let painter = ui.painter_at(slot);
				painter.image(
					texture.id(),
					canvas,
					egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
					egui::Color32::WHITE,
				);
				for part in around(canvas, boxed) {
					painter.rect_filled(part, 0, egui::Color32::from_black_alpha(150));
				}
				let view = design::surface_uv(
					picture,
					shown,
					SurfaceImage {
						opacity: 100,
						fit: SurfaceFit::Cover,
						area: framing.area,
					},
				);
				let view = egui::Rect::from_min_max(
					canvas.lerp_inside(view.min.to_vec2()),
					canvas.lerp_inside(view.max.to_vec2()),
				);
				let mut outline = corners(view).to_vec();
				outline.push(view.left_top());
				painter.extend(egui::Shape::dashed_line(
					&outline,
					egui::Stroke::new(1.5, egui::Color32::WHITE),
					6.0,
					4.0,
				));
				painter.rect_stroke(
					boxed,
					0,
					egui::Stroke::new(2.0, colors.accent),
					egui::StrokeKind::Middle,
				);
				for corner in corners(boxed) {
					painter.circle(
						corner,
						5.0,
						egui::Color32::WHITE,
						egui::Stroke::new(2.0, colors.accent),
					);
				}
				ui.add_space(8.0);
				ui.horizontal_wrapped(|ui| {
					if dialog::action(ui, "theme-editor-framing-smart", dialog::Action::Outline)
						.on_hover_text(crate::i18n::translate("theme-editor-framing-smart-detail"))
						.clicked()
					{
						framing.area = framing.smart;
					}
					if dialog::action(ui, "theme-editor-framing-whole", dialog::Action::Outline)
						.clicked()
					{
						framing.area = SurfaceImage::FULL;
					}
				});
				design::hint(ui, "theme-editor-framing-legend");
				design::hint(
					ui,
					&format!(
						"{} {} × {} px. {}",
						crate::i18n::translate("theme-editor-bars-aim-for"),
						aim.x,
						aim.y,
						crate::i18n::translate("theme-editor-framing-tips")
					),
				);
			});
			body.footer(|ui| {
				proceed =
					dialog::action(ui, "theme-editor-framing-continue", dialog::Action::Primary)
						.clicked();
				cancel = dialog::action(ui, "theme-editor-framing-cancel", dialog::Action::Neutral)
					.clicked();
			});
		});
		if cancel || response.close {
			self.framing = None;
		} else if proceed {
			framing.in_app = true;
			framing.grip = None;
			self.preview = true;
			requests.push(self.preview_request());
		}
	}

	/// Bar images: one row per bar with its thumbnail in the bar's own shape, the size to
	/// aim for, and fit, strength and framing once it has a picture.
	fn bars(
		&mut self,
		ui: &mut egui::Ui,
		requests: &mut Vec<ExtensionRequest>,
		changed: &mut bool,
	) {
		design::section(
			ui,
			&crate::i18n::translate("theme-editor-bars-title"),
			Some(&crate::i18n::translate("theme-editor-bars-description")),
		);
		design::card(ui, |ui| {
			for (index, surface) in ThemeSurface::ALL.into_iter().enumerate() {
				if index > 0 {
					design::card_divider(ui);
				}
				self.bar_row(ui, surface, requests, changed);
			}
		});
		design::hint(ui, &crate::i18n::translate("theme-editor-bars-formats"));
	}

	fn bar_row(
		&mut self,
		ui: &mut egui::Ui,
		surface: ThemeSurface,
		requests: &mut Vec<ExtensionRequest>,
		changed: &mut bool,
	) {
		let index = surface as usize;
		let image = self.surfaces.get(&surface).cloned();
		match &image {
			Some(image) if self.surface_thumbnails[index].is_none() => {
				self.surface_thumbnails[index] = Some(ui.ctx().load_texture(
					format!("theme-bar-thumbnail-{index}"),
					thumbnail_image(image, 256),
					egui::TextureOptions::LINEAR_REPEAT,
				));
			}
			None => self.surface_thumbnails[index] = None,
			_ => {}
		}
		let (name, place, typical) = surface_info(surface);
		let shown = design::surface_size(surface).unwrap_or(typical);
		let pixels_per_point = ui.ctx().pixels_per_point();
		let aim = aim_size(ui.ctx(), surface);
		let colors = design::palette(ui);
		let theme = self
			.package
			.theme
			.as_mut()
			.expect("theme editor always holds a theme");
		ui.push_id(index, |ui| {
			ui.horizontal(|ui| {
				ui.spacing_mut().item_spacing.x = 12.0;
				// The thumbnail takes the bar's own proportions, so fit and framing read true.
				let (slot, _) =
					ui.allocate_exact_size(egui::vec2(96.0, 56.0), egui::Sense::hover());
				let aspect = (shown.x / shown.y.max(1.0)).clamp(0.1, 10.0);
				let size = if aspect >= slot.width() / slot.height() {
					egui::vec2(slot.width(), slot.width() / aspect)
				} else {
					egui::vec2(slot.height() * aspect, slot.height())
				}
				.max(egui::vec2(8.0, 8.0));
				let frame = egui::Rect::from_center_size(slot.center(), size);
				ui.painter().rect_filled(frame, 4, colors.base);
				match (
					&self.surface_thumbnails[index],
					&image,
					theme.surfaces.get(&surface),
				) {
					(Some(texture), Some(image), Some(settings)) => {
						// Tiles keep their size relative to the real bar.
						let picture = egui::vec2(image.size[0] as f32, image.size[1] as f32)
							/ pixels_per_point * (frame.width() / shown.x.max(1.0));
						ui.painter().add(
							egui::epaint::RectShape::filled(frame, 4, egui::Color32::WHITE)
								.with_texture(
									texture.id(),
									design::surface_uv(picture, frame.size(), *settings),
								),
						);
					}
					_ => crate::icons::paint(
						ui.painter(),
						crate::icons::Icon::Image,
						egui::Rect::from_center_size(frame.center(), egui::Vec2::splat(18.0)),
						colors.muted,
					),
				}
				ui.vertical(|ui| {
					ui.spacing_mut().item_spacing.y = 2.0;
					ui.label(
						design::medium(ui, crate::i18n::translate(name), 15.0)
							.color(colors.text_strong),
					);
					design::hint(ui, &crate::i18n::translate(place));
					let aim_for = crate::i18n::translate("theme-editor-bars-aim-for");
					let size_hint = match &image {
						Some(image) => format!(
							"{} × {} px · {aim_for} {} × {} px",
							image.size[0], image.size[1], aim.x, aim.y
						),
						None => format!("{aim_for} {} × {} px", aim.x, aim.y),
					};
					design::hint(ui, &size_hint);
					ui.horizontal_wrapped(|ui| {
						if dialog::action(
							ui,
							if image.is_some() {
								"theme-editor-image-card-replace-image"
							} else {
								"theme-editor-image-card-choose-image"
							},
							dialog::Action::Outline,
						)
						.clicked()
						{
							requests.push(ExtensionRequest::PickThemeSurface(
								surface,
								[aim.x as u32, aim.y as u32],
							));
						}
						if let Some(image) = &image
							&& let Some(settings) = theme
								.surfaces
								.get(&surface)
								.filter(|settings| settings.fit == SurfaceFit::Cover)
							&& dialog::action(
								ui,
								"theme-editor-bars-frame",
								dialog::Action::Outline,
							)
							.on_hover_text(crate::i18n::translate("theme-editor-bars-frame-detail"))
							.clicked()
						{
							self.framing =
								Some(Framing::new(surface, None, image.clone(), settings.area));
						}
						if image.is_some()
							&& dialog::action(
								ui,
								"theme-editor-image-card-remove",
								dialog::Action::Neutral,
							)
							.clicked()
						{
							self.package.surface_images.remove(&surface);
							let mut surfaces = (*self.surfaces).clone();
							surfaces.remove(&surface);
							self.surfaces = Arc::new(surfaces);
							theme.surfaces.remove(&surface);
							*changed = true;
						}
						if let Some(settings) =
							theme.surfaces.get_mut(&surface).filter(|_| image.is_some())
						{
							let label = |fit| match fit {
								SurfaceFit::Cover => "theme-editor-bars-fit-cover",
								SurfaceFit::Stretch => "theme-editor-bars-fit-stretch",
								SurfaceFit::Tile => "theme-editor-bars-fit-tile",
							};
							design::combo("bar-fit")
								.selected_text(crate::i18n::translate(label(settings.fit)))
								.show_ui(ui, |ui| {
									for fit in
										[SurfaceFit::Cover, SurfaceFit::Stretch, SurfaceFit::Tile]
									{
										*changed |= ui
											.selectable_value(
												&mut settings.fit,
												fit,
												crate::i18n::translate(label(fit)),
											)
											.changed();
									}
								})
								.response
								.on_hover_text(crate::i18n::translate("theme-editor-bars-fit"));
						}
					});
				});
			});
			let Some(settings) = theme.surfaces.get_mut(&surface).filter(|_| image.is_some())
			else {
				return;
			};
			*changed |= design::slider_row(
				ui,
				"theme-editor-bars-strength",
				Some("theme-editor-bars-strength-detail"),
				&mut settings.opacity,
				0..=100,
				"%",
			)
			.changed();
		});
	}

	fn image_card(
		&mut self,
		ui: &mut egui::Ui,
		requests: &mut Vec<ExtensionRequest>,
		changed: &mut bool,
	) {
		if self.thumbnail.is_none()
			&& let Some(image) = &self.image
			&& image
				.size
				.iter()
				.all(|side| *side <= ui.ctx().input(|input| input.max_texture_side))
		{
			self.thumbnail = Some(ui.ctx().load_texture(
				"theme-image-thumbnail",
				image.clone(),
				egui::TextureOptions::LINEAR,
			));
		}
		design::card(ui, |ui| {
			ui.horizontal_wrapped(|ui| {
				let (rect, _) =
					ui.allocate_exact_size(egui::vec2(96.0, 68.0), egui::Sense::hover());
				let colors = design::palette(ui);
				ui.painter().rect_filled(rect, 6, colors.base);
				if let Some(texture) = &self.thumbnail {
					design::paint_background_image(
						ui.painter(),
						rect,
						texture,
						Background {
							opacity: 100,
							..Default::default()
						},
					);
				} else {
					crate::icons::paint(
						ui.painter(),
						crate::icons::Icon::Image,
						rect.shrink(20.0),
						colors.muted,
					);
				}
				ui.add_space(8.0);
				ui.vertical(|ui| {
					let selected = self.image.is_some();
					ui.label(design::medium(
						ui,
						crate::i18n::translate_if_key(if selected {
							"theme-editor-image-card-background-image"
						} else {
							"theme-editor-image-card-no-image-selected"
						}),
						14.0,
					));
					if let Some(image) = &self.image {
						design::hint(
							ui,
							&format!(
								"{} × {} {}",
								image.size[0],
								image.size[1],
								crate::i18n::translate("theme-editor-image-card-pixels")
							),
						);
					}
					ui.horizontal_wrapped(|ui| {
						if dialog::action(
							ui,
							if selected {
								"theme-editor-image-card-replace-image"
							} else {
								"theme-editor-image-card-choose-image"
							},
							dialog::Action::Outline,
						)
						.clicked()
						{
							requests.push(ExtensionRequest::PickThemeImage);
						}
						if selected
							&& dialog::action(
								ui,
								"theme-editor-image-card-remove",
								dialog::Action::Neutral,
							)
							.clicked()
						{
							self.package.background_image.clear();
							self.image = None;
							self.thumbnail = None;
							if let Some(theme) = self.package.theme.as_mut() {
								theme.light.background = None;
								theme.dark.background = None;
							}
							*changed = true;
						}
					});
				});
			});
		});
		design::hint(
			ui,
			&crate::i18n::translate("theme-editor-image-card-png-or-jpeg-up-to-2-mib"),
		);
	}
}

fn appearance_switch(ui: &mut egui::Ui, dark: &mut bool) {
	ui.horizontal(|ui| {
		ui.spacing_mut().item_spacing.x = 8.0;
		ui.label(
			egui::RichText::new(crate::i18n::translate(
				"theme-editor-appearance-switch-editing",
			))
			.size(12.0)
			.color(design::palette(ui).muted),
		)
		.on_hover_text(crate::i18n::translate(
			"theme-editor-appearance-switch-colors-and-opacity-are-saved-separately-for-dark-and-light",
		));
		if let Some(index) = design::segmented(
			ui,
			&[
				"theme-editor-appearance-switch-dark",
				"theme-editor-appearance-switch-light",
			],
			usize::from(!*dark),
		) {
			*dark = index == 0;
		}
	});
}

fn map_palette(
	mut base: design::Palette,
	overrides: &std::collections::BTreeMap<String, String>,
) -> design::Palette {
	let color = |key: &str, fallback| {
		overrides
			.get(key)
			.and_then(|value| extensions::parse_color(value).ok())
			.map(rgba)
			.unwrap_or(fallback)
	};
	base.base = color("base", base.base);
	base.sidebar = color("sidebar", base.sidebar);
	base.chat = color("chat", base.chat);
	base.raised = color("raised", base.raised);
	base.accent = color("accent", base.accent);
	base.text_strong = color("text_strong", base.text_strong);
	base
}

fn section_map(
	ui: &mut egui::Ui,
	texture: Option<&egui::TextureHandle>,
	selected: &mut ImageRegion,
	colors: design::Palette,
	fit: BackgroundFit,
	sections: &mut SectionOpacity,
) -> bool {
	let mut changed = false;
	if ui.available_width() >= 620.0 {
		ui.horizontal_top(|ui| {
			let width = ui.available_width() * 0.55;
			ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
				section_diagram(ui, texture, selected, colors, fit, sections);
			});
			ui.add_space(12.0);
			ui.vertical(|ui| {
				changed = section_controls(ui, selected, sections);
			});
		});
	} else {
		section_diagram(ui, texture, selected, colors, fit, sections);
		ui.add_space(12.0);
		changed = section_controls(ui, selected, sections);
	}
	changed
}

fn section_controls(
	ui: &mut egui::Ui,
	selected: &mut ImageRegion,
	sections: &mut SectionOpacity,
) -> bool {
	let mut changed = false;
	design::card(ui, |ui| {
		let palette = design::palette(ui);
		ui.visuals_mut().widgets.inactive.bg_fill = palette.base;
		ui.visuals_mut().widgets.inactive.weak_bg_fill = palette.base;
		ui.visuals_mut().selection.bg_fill = palette.accent;
		ui.label(design::medium(
			ui,
			crate::i18n::translate("theme-editor-section-controls-selected-section"),
			13.0,
		));
		crate::design::combo("background-section")
			.width(ui.available_width())
			.selected_text(crate::i18n::translate_if_key(selected.label_key()))
			.show_ui(ui, |ui| {
				for region in REGIONS {
					ui.selectable_value(
						selected,
						region,
						crate::i18n::translate_if_key(region.label_key()),
					);
				}
			});
		design::hint(ui, selected.description_key());
		ui.add_space(12.0);
		changed = design::slider_row(
			ui,
			"theme-editor-section-controls-surface-opacity",
			None,
			selected.opacity(sections),
			0..=100,
			"%",
		)
		.changed();
		design::hint(
			ui,
			&crate::i18n::translate(
				"theme-editor-section-controls-0-shows-the-image-100-is-a-solid-section-color",
			),
		);
		ui.add_space(8.0);
		// The picture always spans the window; these set every other section in one go.
		ui.horizontal_wrapped(|ui| {
			let only_here = dialog::action(
				ui,
				"theme-editor-section-only-here",
				dialog::Action::Outline,
			)
			.on_hover_text(crate::i18n::translate(
				"theme-editor-section-only-here-detail",
			))
			.clicked();
			let everywhere = dialog::action(
				ui,
				"theme-editor-section-everywhere",
				dialog::Action::Neutral,
			)
			.on_hover_text(crate::i18n::translate(
				"theme-editor-section-everywhere-detail",
			))
			.clicked();
			if only_here || everywhere {
				let shown = if only_here {
					0
				} else {
					*selected.opacity(sections)
				};
				for region in REGIONS {
					*region.opacity(sections) = if region == *selected || everywhere {
						shown
					} else {
						100
					};
				}
				changed = true;
			}
		});
	});
	changed
}

fn section_diagram(
	ui: &mut egui::Ui,
	texture: Option<&egui::TextureHandle>,
	selected: &mut ImageRegion,
	colors: design::Palette,
	fit: BackgroundFit,
	sections: &mut SectionOpacity,
) {
	let width = ui.available_width().clamp(1.0, 520.0);
	let (rect, _) =
		ui.allocate_exact_size(egui::vec2(width, width * 9.0 / 16.0), egui::Sense::hover());
	let painter = ui.painter().with_clip_rect(rect);
	painter.rect_filled(rect, 8, colors.base.to_opaque());
	if let Some(texture) = texture {
		design::paint_background_image(
			&painter,
			rect,
			texture,
			Background {
				opacity: 100,
				fit,
				..Default::default()
			},
		);
	}
	let area = |x: f32, y: f32, w: f32, h: f32| {
		egui::Rect::from_min_max(
			rect.min + egui::vec2(rect.width() * x, rect.height() * y),
			rect.min + egui::vec2(rect.width() * (x + w), rect.height() * (y + h)),
		)
	};
	let regions = [
		(ImageRegion::TopBars, area(0.0, 0.0, 1.0, 0.11), "Top", 0_u8),
		(ImageRegion::ServerList, area(0.0, 0.11, 0.09, 0.89), "S", 0),
		(
			ImageRegion::PeopleChannels,
			area(0.09, 0.11, 0.23, 0.89),
			"People",
			0,
		),
		(
			ImageRegion::TopBars,
			area(0.32, 0.11, 0.46, 0.11),
			"Header",
			1,
		),
		(
			ImageRegion::MessageList,
			area(0.32, 0.22, 0.46, 0.63),
			"Messages",
			0,
		),
		(
			ImageRegion::MemberList,
			area(0.78, 0.11, 0.22, 0.89),
			"Members",
			0,
		),
		(
			ImageRegion::InputArea,
			area(0.32, 0.85, 0.46, 0.15),
			"Input",
			0,
		),
	];
	for (region, region_rect, short_label, part) in regions {
		let surface = match region {
			ImageRegion::TopBars | ImageRegion::ServerList => colors.base,
			ImageRegion::PeopleChannels | ImageRegion::MemberList => colors.sidebar,
			ImageRegion::MessageList | ImageRegion::InputArea => colors.chat,
		};
		let [r, g, b, _] = surface.to_srgba_unmultiplied();
		let opacity = *region.opacity(sections);
		painter.rect_filled(
			region_rect.shrink(1.0),
			2,
			egui::Color32::from_rgba_unmultiplied(r, g, b, (u16::from(opacity) * 255 / 100) as u8),
		);
		let response = ui
			.interact(
				region_rect,
				ui.scope_id().with((region as u8, part)),
				egui::Sense::click(),
			)
			.on_hover_text(crate::i18n::translate_if_key(region.label_key()));
		response.widget_info(|| {
			egui::WidgetInfo::labeled(
				egui::Role::Button,
				true,
				crate::i18n::translate_if_key(region.label_key()),
			)
		});
		if response.clicked()
			|| response.has_focus()
				&& ui.input(|input| {
					input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Space)
				}) {
			*selected = region;
		}
		if response.hovered() {
			ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
		}
		let highlighted = *selected == region || response.has_focus();
		let outline = if highlighted {
			colors.accent
		} else {
			colors.border
		};
		painter.rect_stroke(
			region_rect.shrink(0.5),
			2,
			egui::Stroke::new(if highlighted { 2.0 } else { 1.0 }, outline),
			egui::StrokeKind::Inside,
		);
		if region_rect.width() >= 16.0 {
			let font = egui::FontId::proportional((rect.width() / 38.0).clamp(10.0, 13.0));
			let label = if region_rect.width() < 60.0 {
				match region {
					ImageRegion::TopBars => "T",
					ImageRegion::ServerList => "S",
					ImageRegion::PeopleChannels => "P",
					ImageRegion::MessageList => "M",
					ImageRegion::MemberList => "M",
					ImageRegion::InputArea => "I",
				}
			} else {
				short_label
			};
			let galley = painter.layout_no_wrap(label.into(), font, colors.text_strong);
			let label_rect = egui::Rect::from_center_size(
				region_rect.center(),
				galley.size() + egui::vec2(6.0, 4.0),
			);
			painter.rect_filled(label_rect, 3, colors.raised.to_opaque());
			painter.galley(
				label_rect.center() - galley.size() / 2.0,
				galley,
				colors.text_strong,
			);
		}
	}
}

fn rgba([r, g, b, a]: [u8; 4]) -> egui::Color32 {
	egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}
fn hex(color: egui::Color32) -> String {
	let [r, g, b, a] = color.to_srgba_unmultiplied();
	format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
}
fn colors(p: design::Palette) -> [(&'static str, egui::Color32); 18] {
	[
		("base", p.base),
		("sidebar", p.sidebar),
		("chat", p.chat),
		("raised", p.raised),
		("hover", p.hover),
		("selected", p.selected),
		("border", p.border),
		("text_strong", p.text_strong),
		("text", p.text),
		("muted", p.muted),
		("link", p.link),
		("accent", p.accent),
		("accent_text", p.accent_text),
		("positive", p.positive),
		("warning", p.warning),
		("danger", p.danger),
		("mention_bg", p.mention_bg),
		("mention_text", p.mention_text),
	]
}
fn color_input(ui: &mut egui::Ui, value: &mut String) -> bool {
	ui.allocate_ui_with_layout(
		egui::vec2(180.0, 42.0),
		egui::Layout::left_to_right(egui::Align::Center),
		|ui| {
			let mut color = extensions::parse_color(value)
				.map(rgba)
				.unwrap_or(egui::Color32::TRANSPARENT);
			let mut changed = ui.color_edit_button_srgba(&mut color).changed();
			if changed {
				*value = hex(color);
			}
			changed |= ui
				.allocate_ui_with_layout(
					egui::vec2(132.0, 42.0),
					egui::Layout::left_to_right(egui::Align::Center),
					|ui| {
						design::input(
							ui,
							egui::TextEdit::singleline(value)
								.align(egui::Align2::LEFT_CENTER)
								.char_limit(9)
								.font(egui::FontId::proportional(14.0)),
						)
						.changed()
					},
				)
				.inner;
			if extensions::parse_color(value).is_err() {
				let (rect, response) =
					ui.allocate_exact_size(egui::Vec2::splat(16.0), egui::Sense::hover());
				icons::paint(
					ui.painter(),
					icons::Icon::ShieldWarning,
					rect,
					design::palette(ui).danger,
				);
				response.on_hover_text(crate::i18n::translate(
					"theme-editor-color-input-use-rrggbb-or-rrggbbaa",
				));
			}
			changed
		},
	)
	.inner
}
fn color_label(key: &str) -> &str {
	match key {
		"base" => "Window background",
		"sidebar" => "Sidebar",
		"chat" => "Message area",
		"raised" => "Cards & message input",
		"hover" => "Hover",
		"selected" => "Selection",
		"border" => "Borders",
		"text_strong" => "Headings",
		"text" => "Body text",
		"muted" => "Secondary text",
		"link" => "Links",
		"accent" => "Accent",
		"accent_text" => "Text on accent",
		"positive" => "Success",
		"warning" => "Warning",
		"danger" => "Error & danger",
		"mention_bg" => "Mention background",
		"mention_text" => "Mention text",
		_ => key,
	}
}
/// Settings rows align values at the right; narrow pages stack instead of clipping controls.
fn row(ui: &mut egui::Ui, label: &str, controls: impl FnOnce(&mut egui::Ui) -> bool) -> bool {
	settings_row(ui, label, None, controls)
}
fn settings_row(
	ui: &mut egui::Ui,
	label: &str,
	description: Option<&str>,
	controls: impl FnOnce(&mut egui::Ui) -> bool,
) -> bool {
	ui.push_id(label, |ui| {
		let label = crate::i18n::translate_if_key(label);
		let description = description.map(crate::i18n::translate_if_key);
		ui.add_space(4.0);
		let height = if description.is_some() { 54.0 } else { 42.0 };
		let heading = |ui: &mut egui::Ui| {
			ui.label(design::medium(ui, &label, 14.0).color(design::palette(ui).text_strong));
			if let Some(description) = description {
				ui.add(
					egui::Label::new(
						egui::RichText::new(description)
							.size(12.0)
							.color(design::palette(ui).muted),
					)
					.wrap(),
				);
			}
		};
		if ui.available_width() < 500.0 {
			heading(ui);
			ui.allocate_ui_with_layout(
				egui::vec2(ui.available_width(), 42.0),
				egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
				controls,
			)
			.inner
		} else {
			ui.allocate_ui_with_layout(
				egui::vec2(ui.available_width(), height),
				egui::Layout::left_to_right(egui::Align::Center),
				|ui| {
					let width = 250.0;
					let label_width =
						(ui.available_width() - width - ui.spacing().item_spacing.x).max(1.0);
					ui.allocate_ui_with_layout(
						egui::vec2(label_width, height),
						egui::Layout::top_down(egui::Align::Min)
							.with_main_align(egui::Align::Center),
						|ui| {
							ui.set_min_width(label_width);
							ui.set_min_height(height);
							heading(ui);
						},
					);
					ui.allocate_ui_with_layout(
						egui::vec2(width, 42.0),
						egui::Layout::left_to_right(egui::Align::Center),
						controls,
					)
					.inner
				},
			)
			.inner
		}
	})
	.inner
}
fn text_field(
	ui: &mut egui::Ui,
	label: &str,
	value: &mut String,
	limit: usize,
	hint: &str,
) -> bool {
	ui.push_id(label, |ui| {
		let label = crate::i18n::translate_if_key(label);
		let hint = crate::i18n::translate_if_key(hint);
		let colors = design::palette(ui);
		let label = ui.label(design::medium(ui, &label, 13.0).color(colors.text_strong));
		let changed = design::input(
			ui,
			egui::TextEdit::singleline(value)
				.align(egui::Align2::LEFT_CENTER)
				.char_limit(limit)
				.font(egui::FontId::proportional(15.0))
				.hint_text(hint),
		)
		.labelled_by(label.id)
		.changed();
		ui.add_space(8.0);
		changed
	})
	.inner
}
fn color_override(
	ui: &mut egui::Ui,
	key: &str,
	map: &mut std::collections::BTreeMap<String, String>,
	fallback: egui::Color32,
) -> bool {
	let description = match key {
		"accent" => Some("Buttons, selection and highlights"),
		"text" => Some("Messages and regular labels"),
		"muted" => Some("Timestamps and supporting text"),
		"sidebar" => Some("Channel, conversation and member lists"),
		"chat" => Some("Background behind your messages"),
		_ => None,
	};
	let changed = settings_row(ui, color_label(key), description, |ui| {
		let mut value = map.get(key).cloned().unwrap_or_else(|| hex(fallback));
		let mut changed = color_input(ui, &mut value);
		if changed {
			map.insert(key.into(), value);
		}
		if map.contains_key(key)
			&& design::text_action(
				ui,
				&crate::i18n::translate("theme-editor-color-override-reset"),
			)
			.on_hover_text(crate::i18n::translate(
				"theme-editor-color-override-use-the-default-color-for-this-appearance",
			))
			.clicked()
		{
			map.remove(key);
			changed = true;
		}
		changed
	});
	if map
		.get(key)
		.is_some_and(|value| extensions::parse_color(value).is_err())
	{
		design::notice(
			ui,
			design::Level::Error,
			&crate::i18n::translate("theme-editor-color-override-use-rrggbb-or-rrggbbaa"),
		);
	}
	changed
}
fn metric(
	ui: &mut egui::Ui,
	label: &str,
	value: &mut Option<u8>,
	default: u8,
	range: std::ops::RangeInclusive<u8>,
) -> bool {
	let mut changed = false;
	ui.push_id(label, |ui| {
		let mut n = value.unwrap_or(default);
		if metric_label(ui, label, value.is_some()) {
			*value = None;
			n = default;
			changed = true;
		}
		if design::slider(ui, &mut n, range, " px").changed() {
			*value = Some(n);
			changed = true;
		}
		ui.add_space(8.0);
	});
	changed
}

/// Metric title with a quiet Reset on the right; returns whether Reset was pressed.
fn metric_label(ui: &mut egui::Ui, label: &str, overridden: bool) -> bool {
	let label = crate::i18n::translate_if_key(label);
	let mut reset = false;
	ui.horizontal(|ui| {
		ui.label(design::medium(ui, &label, 14.0).color(design::palette(ui).text_strong));
		if overridden {
			ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
				reset = design::text_action(
					ui,
					&crate::i18n::translate("theme-editor-metric-label-reset"),
				)
				.on_hover_text(crate::i18n::translate(
					"theme-editor-metric-label-use-the-built-in-value",
				))
				.clicked();
			});
		}
	});
	reset
}

fn pair_metric(
	ui: &mut egui::Ui,
	label: &str,
	value: &mut Option<[u8; 2]>,
	default: [u8; 2],
) -> bool {
	let mut changed = false;
	ui.push_id(label, |ui| {
		let mut pair = value.unwrap_or(default);
		if metric_label(ui, label, value.is_some()) {
			*value = None;
			pair = default;
			changed = true;
		}
		let mut edited = false;
		for (index, n) in pair.iter_mut().enumerate() {
			let axis = if index == 0 { "Horizontal" } else { "Vertical" };
			ui.label(
				egui::RichText::new(axis)
					.size(12.0)
					.color(design::palette(ui).muted),
			);
			edited |= design::slider(ui, n, 0..=24, " px").changed();
		}
		if edited {
			*value = Some(pair);
			changed = true;
		}
		ui.add_space(8.0);
	});
	changed
}

#[cfg(test)]
mod tests {
	use super::*;
	fn collect(shape: &egui::Shape, labels: &mut Vec<(String, egui::Rect)>) {
		match shape {
			egui::Shape::Text(text) => {
				labels.push((text.galley.job.text.clone(), text.visual_bounding_rect()))
			}
			egui::Shape::Vec(shapes) => {
				for shape in shapes {
					collect(shape, labels);
				}
			}
			_ => {}
		}
	}

	fn map_frame(
		ctx: &egui::Context,
		region: &mut ImageRegion,
		sections: &mut SectionOpacity,
		events: Vec<egui::Event>,
	) -> Vec<(String, egui::Rect)> {
		let output = ctx.run_ui(
			egui::RawInput {
				screen_rect: Some(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(500.0, 600.0),
				)),
				events,
				focused: true,
				..Default::default()
			},
			|ui| {
				section_map(
					ui,
					None,
					region,
					design::builtin_colors(true, design::Variant::Standard),
					BackgroundFit::Cover,
					sections,
				);
			},
		);
		let mut labels = Vec::new();
		for shape in &output.shapes {
			collect(&shape.shape, &mut labels);
		}
		output.drop_without_applying_deltas();
		labels
	}
	fn toolbar_frame(
		ctx: &egui::Context,
		editor: &mut ThemeEditor,
		events: Vec<egui::Event>,
	) -> (Vec<(String, egui::Rect)>, Vec<ExtensionRequest>) {
		let mut requests = Vec::new();
		let output = ctx.run_ui(
			egui::RawInput {
				screen_rect: Some(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(520.0, 600.0),
				)),
				events,
				focused: true,
				..Default::default()
			},
			|ui| {
				editor.toolbar(ui, false, &mut requests);
			},
		);
		let mut labels = Vec::new();
		for shape in &output.shapes {
			collect(&shape.shape, &mut labels);
		}
		output.drop_without_applying_deltas();
		(labels, requests)
	}
	fn toolbar_click(
		ctx: &egui::Context,
		editor: &mut ThemeEditor,
		label: &str,
	) -> Vec<ExtensionRequest> {
		let (labels, _) = toolbar_frame(ctx, editor, vec![]);
		let position = labels
			.iter()
			.find(|(text, _)| text == label)
			.unwrap_or_else(|| panic!("Missing {label}"))
			.1
			.center();
		let mut requests = Vec::new();
		for pressed in [true, false] {
			requests = toolbar_frame(
				ctx,
				editor,
				vec![
					egui::Event::PointerMoved(position),
					egui::Event::PointerButton {
						pos: position,
						button: egui::PointerButton::Primary,
						pressed,
						modifiers: egui::Modifiers::NONE,
					},
				],
			)
			.1;
		}
		requests
	}

	#[test]
	fn map_selects_the_member_panel_and_save_points_to_required_basics() {
		let ctx = egui::Context::default();
		ctx.set_theme(egui::ThemePreference::Dark);
		let mut editor = ThemeEditor::new();
		assert_eq!(editor.save_error().0, EditorTab::Basics);
		let mut sections = SectionOpacity::default();
		let labels = map_frame(&ctx, &mut editor.region, &mut sections, vec![]);
		let position = labels
			.iter()
			.find(|(label, _)| label == "Members")
			.unwrap()
			.1
			.center();
		for pressed in [true, false] {
			map_frame(
				&ctx,
				&mut editor.region,
				&mut sections,
				vec![
					egui::Event::PointerMoved(position),
					egui::Event::PointerButton {
						pos: position,
						button: egui::PointerButton::Primary,
						pressed,
						modifiers: egui::Modifiers::NONE,
					},
				],
			);
		}
		assert_eq!(editor.region, ImageRegion::MemberList);
		assert_eq!(sections, SectionOpacity::default());
		// The text selector provides the same navigation as the map.
		for label in ["Member list", "Top bars"] {
			let labels = map_frame(&ctx, &mut editor.region, &mut sections, vec![]);
			let position = labels
				.iter()
				.find(|(text, _)| text == label)
				.unwrap()
				.1
				.center();
			for pressed in [true, false] {
				map_frame(
					&ctx,
					&mut editor.region,
					&mut sections,
					vec![
						egui::Event::PointerMoved(position),
						egui::Event::PointerButton {
							pos: position,
							button: egui::PointerButton::Primary,
							pressed,
							modifiers: egui::Modifiers::NONE,
						},
					],
				);
			}
		}
		assert_eq!(editor.region, ImageRegion::TopBars);
		assert_eq!(sections, SectionOpacity::default());
		editor.package.manifest.author = "Creator".into();
		assert!(editor.package.validate().is_ok());
	}

	#[test]
	fn tabs_and_save_keep_required_fields_easy_to_find() {
		let ctx = egui::Context::default();
		ctx.set_theme(egui::ThemePreference::Dark);
		let mut editor = ThemeEditor::new();
		assert!(toolbar_click(&ctx, &mut editor, "Background").is_empty());
		assert_eq!(editor.tab, EditorTab::Background);
		assert!(toolbar_click(&ctx, &mut editor, "Save and apply").is_empty());
		assert_eq!(editor.tab, EditorTab::Basics);
		assert!(editor.validation_error.is_some());
		editor.package.manifest.author = "  ".into();
		assert!(toolbar_click(&ctx, &mut editor, "Save and apply").is_empty());
		editor.package.manifest.author = "Creator".into();
		assert!(matches!(
			toolbar_click(&ctx, &mut editor, "Save and apply").as_slice(),
			[ExtensionRequest::SaveTheme { .. }]
		));
	}

	#[test]
	fn editor_opens_on_the_active_appearance_and_keeps_the_chosen_palette() {
		let ctx = egui::Context::default();
		ctx.set_theme(egui::ThemePreference::Light);
		let mut editor = ThemeEditor::new();
		assert!(toolbar_click(&ctx, &mut editor, "Colors").is_empty());
		assert!(!editor.dark);
		assert!(toolbar_click(&ctx, &mut editor, "Dark").is_empty());
		toolbar_frame(&ctx, &mut editor, vec![]);
		assert!(editor.dark);
	}

	#[test]
	fn dragging_the_bar_in_the_app_moves_the_picture_with_the_pointer() {
		let ctx = egui::Context::default();
		let image = Arc::new(egui::ColorImage::new(
			[200, 100],
			vec![egui::Color32::BLACK; 200 * 100],
		));
		let mut framing = Framing::new(ThemeSurface::Composer, None, image, SurfaceImage::FULL);
		// Twice as wide as the picture: the full width and the middle half of its height.
		let bar = egui::Rect::from_min_size(egui::pos2(100.0, 300.0), egui::vec2(400.0, 100.0));
		let mut frame = |events: Vec<egui::Event>| {
			let output = ctx.run_ui(
				egui::RawInput {
					screen_rect: Some(egui::Rect::from_min_size(
						egui::Pos2::ZERO,
						egui::vec2(800.0, 600.0),
					)),
					events,
					focused: true,
					..Default::default()
				},
				|ui| framing.overlay(ui.ctx(), bar, 0.0, egui::Color32::WHITE),
			);
			output.drop_without_applying_deltas();
		};
		let button = |pressed| egui::Event::PointerButton {
			pos: egui::pos2(300.0, 350.0),
			button: egui::PointerButton::Primary,
			pressed,
			modifiers: egui::Modifiers::NONE,
		};
		// A new area sizes itself in its first pass before it takes input.
		for _ in 0..3 {
			frame(vec![egui::Event::PointerMoved(egui::pos2(300.0, 350.0))]);
		}
		frame(vec![button(true)]);
		// Pulling the picture up by a quarter of the bar shows a lower part of it.
		for y in [340.0, 330.0, 325.0] {
			frame(vec![egui::Event::PointerMoved(egui::pos2(300.0, y))]);
		}
		frame(vec![button(false)]);
		let [left, top, right, bottom] = framing.area;
		assert_eq!((left, right, bottom - top), (0, 10_000, 5_000));
		assert!((3_500..=3_750).contains(&top), "{top}");
	}

	#[test]
	fn framing_finds_the_detail_and_keeps_every_crop_inside_the_picture() {
		// A dark 200 x 100 picture with a checkered band across its bottom quarter.
		let picture = |band: bool| {
			let pixels = (0..100)
				.flat_map(|y| (0..200).map(move |x| (x, y)))
				.map(|(x, y)| {
					if band && y >= 75 && (x / 2 + y / 2) % 2 == 0 {
						egui::Color32::WHITE
					} else {
						egui::Color32::BLACK
					}
				})
				.collect();
			egui::ColorImage::new([200, 100], pixels)
		};
		// A bar eight times wider than tall shows a quarter of the height: the band.
		let [left, top, right, bottom] = smart_area(&picture(true), 8.0);
		assert_eq!((left, right, bottom - top), (0, 10_000, 2_500));
		assert!(top >= 7_000, "{top}");
		// A flat picture stays centred, and a tall bar picks columns instead.
		let [_, top, _, _] = smart_area(&picture(false), 8.0);
		assert!((3_400..=4_100).contains(&top), "{top}");
		let [left, top, right, bottom] = smart_area(&picture(false), 1.0);
		assert_eq!((top, bottom, right - left), (0, 10_000, 5_000));
		// Dragging the box or a corner never leaves the picture or collapses the box.
		let boxed = [1_000, 1_000, 3_000, 3_000];
		assert_eq!(
			drag_area(boxed, 4, egui::vec2(-5_000.0, 0.0)),
			[0, 1_000, 2_000, 3_000]
		);
		assert_eq!(
			drag_area(boxed, 2, egui::vec2(-9_000.0, -9_000.0)),
			[1_000, 1_000, 1_200, 1_200]
		);
		assert_eq!(
			drag_area(boxed, 0, egui::vec2(-9_000.0, 500.0)),
			[0, 1_500, 3_000, 3_000]
		);
		// Zooming in the app scales around the pointer; zooming out stops at the picture.
		let view = egui::Rect::from_min_max(egui::pos2(0.0, 0.25), egui::pos2(1.0, 0.75));
		let center = egui::pos2(0.5, 0.5);
		let min = egui::Vec2::splat(0.01);
		assert_eq!(
			reframe(view, egui::Vec2::ZERO, 0.5, center, min),
			[2_500, 3_750, 7_500, 6_250]
		);
		assert_eq!(
			reframe(view, egui::vec2(-1.0, 0.0), 4.0, center, min),
			[0, 2_500, 10_000, 7_500]
		);
		assert_eq!(
			reframe(view, egui::vec2(0.0, 1.0), 1.0, center, min),
			[0, 5_000, 10_000, 10_000]
		);
	}
}
