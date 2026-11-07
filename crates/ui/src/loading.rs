//! Colours of every loading surface: the launch splash and upload progress. A theme with a
//! background picture lends them its two main colours; Custom uses the user's two; anything
//! else, or two colours too alike to read, keeps the theme's palette.
use egui::Color32;
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Weak};

use crate::design::{contrast, luminance, mix};

/// What a loading surface paints with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
	/// The opaque background: the splash, an upload bar's track.
	pub surface: Color32,
	/// The progress bar, the worm and the splash's accents.
	pub bar: Color32,
	/// A quiet track for a bar drawn on `surface`.
	pub track: Color32,
	/// Readable on `surface`: titles, and smaller muted details.
	pub text: Color32,
	pub muted: Color32,
	/// Whether `surface` is dark, so light text sits on it.
	pub dark: bool,
}

/// A picture, held weakly so its pixels can go, and its two main colours if it has them.
type PictureColors = (Weak<egui::ColorImage>, Option<[Color32; 2]>);

thread_local! {
	static CUSTOM: Cell<Option<[[u8; 3]; 2]>> = const { Cell::new(None) };
	/// The last background picture's main colours, so each picture is read once.
	static IMAGE: RefCell<Option<PictureColors>> = const { RefCell::new(None) };
}

/// The Custom choice from Settings, background then bar; `None` follows the theme.
pub fn set_custom(colors: Option<[[u8; 3]; 2]>) {
	CUSTOM.set(colors);
}

/// The loading colours, or `None` to keep the palette: no picture and no Custom choice, or
/// two colours that would not read against each other.
pub fn colors() -> Option<Colors> {
	let [surface, bar] = pair()?;
	readable(surface, bar)
}

/// The two colours behind [`colors`] before the readability check: Custom ones as chosen, a
/// picture's darker first.
pub fn pair() -> Option<[Color32; 2]> {
	if let Some([surface, bar]) = CUSTOM.get() {
		return Some([surface, bar].map(|[r, g, b]| Color32::from_rgb(r, g, b)));
	}
	let image = crate::design::background_image()?;
	IMAGE.with(|cache| {
		let mut cache = cache.borrow_mut();
		if let Some((seen, colors)) = &*cache
			&& seen.as_ptr() == Arc::as_ptr(&image)
		{
			return *colors;
		}
		let colors = main_colors(&image);
		*cache = Some((Arc::downgrade(&image), colors));
		colors
	})
}

/// Text and track colours for a surface and bar, or `None` when the bar would not stand out.
pub fn readable(surface: Color32, bar: Color32) -> Option<Colors> {
	// The WCAG minimum for a control against what it sits on.
	if contrast(surface, bar) < 3.0 {
		return None;
	}
	let surface = surface.to_opaque();
	let (light, ink) = (Color32::WHITE, Color32::from_gray(17));
	let dark = contrast(surface, light) >= contrast(surface, ink);
	let neutral = if dark { light } else { ink };
	// A touch of the bar in the text ties it to the theme while it stays readable.
	let tinted = mix(neutral, bar.to_opaque(), 0.25);
	let text = if contrast(surface, tinted) >= 4.5 {
		tinted
	} else {
		neutral
	};
	let muted = (0..=8)
		.map(|step| mix(surface, text, 0.6 + 0.05 * step as f32))
		.find(|muted| contrast(surface, *muted) >= 4.5)
		.unwrap_or(text);
	Some(Colors {
		surface,
		bar: bar.to_opaque(),
		track: mix(surface, bar.to_opaque(), 0.18),
		text,
		muted,
		dark,
	})
}

/// A picture's two main colours, darker first: the most common one, and the most common one
/// that stands out clearly from it. `None` for a flat or low-contrast picture.
fn main_colors(image: &egui::ColorImage) -> Option<[Color32; 2]> {
	const GRID: usize = 64;
	const SEEDS: usize = 6;
	let [width, height] = image.size;
	if width == 0 || height == 0 || image.pixels.len() != width * height {
		return None;
	}
	// At most 64 by 64 samples, each a 3 by 3 patch, which calms film grain without blending
	// neighbouring areas.
	let (columns, rows) = (width.min(GRID), height.min(GRID));
	let mut samples = Vec::with_capacity(columns * rows);
	for row in 0..rows {
		let y = (2 * row + 1) * height / (2 * rows);
		for column in 0..columns {
			let x = (2 * column + 1) * width / (2 * columns);
			let (mut sum, mut count) = ([0.0f32; 3], 0.0f32);
			for py in y.saturating_sub(1)..(y + 2).min(height) {
				for px in x.saturating_sub(1)..(x + 2).min(width) {
					let [r, g, b, a] = image.pixels[py * width + px].to_srgba_unmultiplied();
					// Mostly see-through pixels show the window, not the picture.
					if a >= 128 {
						for (total, value) in sum.iter_mut().zip([r, g, b]) {
							*total += f32::from(value);
						}
						count += 1.0;
					}
				}
			}
			if count > 0.0 {
				samples.push(sum.map(|total| total / count));
			}
		}
	}
	if samples.len() < 16 {
		return None;
	}
	// Seeds are the busiest cells of a coarse 4 by 4 by 4 histogram; a few k-means rounds
	// then settle each on the middle of its colour.
	let mut bins = [([0.0f32; 3], 0usize); 64];
	for sample in &samples {
		let [r, g, b] = sample.map(|value| (value as usize / 64).min(3));
		let bin = &mut bins[r * 16 + g * 4 + b];
		for (total, value) in bin.0.iter_mut().zip(sample) {
			*total += value;
		}
		bin.1 += 1;
	}
	let mut seeds: Vec<_> = bins.into_iter().filter(|(_, count)| *count > 0).collect();
	seeds.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
	let mut centres: Vec<[f32; 3]> = seeds
		.iter()
		.take(SEEDS)
		.map(|(sum, count)| sum.map(|total| total / *count as f32))
		.collect();
	let distance = |a: &[f32; 3], b: &[f32; 3]| -> f32 {
		a.iter().zip(b).map(|(a, b)| (a - b) * (a - b)).sum()
	};
	let nearest = |centres: &[[f32; 3]], sample: &[f32; 3]| -> usize {
		(0..centres.len())
			.min_by(|a, b| {
				distance(&centres[*a], sample).total_cmp(&distance(&centres[*b], sample))
			})
			.unwrap_or(0)
	};
	let mut counts = vec![0usize; centres.len()];
	for _ in 0..8 {
		let mut sums = vec![[0.0f32; 3]; centres.len()];
		counts.fill(0);
		for sample in &samples {
			let index = nearest(&centres, sample);
			for (total, value) in sums[index].iter_mut().zip(sample) {
				*total += value;
			}
			counts[index] += 1;
		}
		for ((centre, sum), count) in centres.iter_mut().zip(&sums).zip(&counts) {
			if *count > 0 {
				*centre = sum.map(|total| total / *count as f32);
			}
		}
	}
	// Close clusters are one colour split in two; merging them keeps "most common" honest.
	let mut clusters: Vec<([f32; 3], usize)> = Vec::new();
	let mut order: Vec<_> = centres.into_iter().zip(counts).collect();
	order.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
	for (centre, count) in order.into_iter().filter(|(_, count)| *count > 0) {
		match clusters
			.iter_mut()
			.find(|(kept, _)| distance(kept, &centre) < 40.0 * 40.0)
		{
			Some((kept, kept_count)) => {
				let total = (*kept_count + count) as f32;
				for (kept, value) in kept.iter_mut().zip(centre) {
					*kept = (*kept * *kept_count as f32 + value * count as f32) / total;
				}
				*kept_count += count;
			}
			None => clusters.push((centre, count)),
		}
	}
	clusters.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
	let color =
		|[r, g, b]: [f32; 3]| Color32::from_rgb(r.round() as u8, g.round() as u8, b.round() as u8);
	let main = color(clusters.first()?.0);
	// A speck of colour is noise, not one of the picture's colours.
	let other = clusters[1..]
		.iter()
		.filter(|(_, count)| count * 100 >= samples.len() * 3)
		.map(|(centre, _)| color(*centre))
		.find(|other| contrast(main, *other) >= 3.0)?;
	Some(if luminance(main) <= luminance(other) {
		[main, other]
	} else {
		[other, main]
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A grainy black poster with white lettering, like a black-and-white print.
	fn poster(width: usize, height: usize) -> egui::ColorImage {
		let mut seed = 0x2545_f491_u32;
		let pixels = (0..width * height)
			.map(|index| {
				seed ^= seed << 13;
				seed ^= seed >> 17;
				seed ^= seed << 5;
				let grain = (seed % 41) as i32 - 20;
				let (x, y) = (index % width, index / width);
				// Lettering in a band across the middle third, and a pale frame.
				let letter =
					(height / 3..height / 2).contains(&y) && (x / (width / 24)).is_multiple_of(2);
				let frame = x < width / 40 || y < height / 40;
				let base = if letter || frame { 232 } else { 18 };
				Color32::from_gray((base + grain).clamp(0, 255) as u8)
			})
			.collect();
		egui::ColorImage {
			size: [width, height],
			source_size: egui::vec2(width as f32, height as f32),
			pixels,
		}
	}

	#[test]
	fn a_black_and_white_poster_gives_near_black_and_near_white() {
		let image = poster(1200, 1800);
		let started = std::time::Instant::now();
		let [dark, light] = main_colors(&image).expect("two main colours");
		// The cost follows the 64 by 64 samples, not the picture's size: about 14 ms in an
		// unoptimized test build, once per picture.
		assert!(
			started.elapsed().as_millis() < 200,
			"{:?}",
			started.elapsed()
		);
		assert!(
			dark.r() < 40 && dark.r() == dark.g() && dark.g() == dark.b(),
			"{dark:?}"
		);
		assert!(light.r() > 200, "{light:?}");
		let colors = readable(dark, light).unwrap();
		assert_eq!((colors.surface, colors.bar), (dark, light));
		assert!(colors.dark);
		assert!(contrast(colors.surface, colors.text) >= 4.5);
		assert!(contrast(colors.surface, colors.muted) >= 4.5);
		assert!(colors.surface.a() == 255 && colors.track != colors.surface);
	}

	#[test]
	fn flat_pictures_and_close_colours_keep_the_palette() {
		let flat = egui::ColorImage::filled([300, 200], Color32::from_rgb(88, 101, 242));
		assert_eq!(main_colors(&flat), None);
		// Two blues that barely differ, half and half.
		let mut close = flat.clone();
		for pixel in &mut close.pixels[..30_000] {
			*pixel = Color32::from_rgb(70, 84, 220);
		}
		assert_eq!(main_colors(&close), None);
		assert_eq!(
			readable(Color32::from_gray(90), Color32::from_gray(120)),
			None
		);
		// Light Custom colours read too: dark text on the light surface.
		let light = readable(
			Color32::from_rgb(245, 240, 230),
			Color32::from_rgb(40, 90, 200),
		)
		.unwrap();
		assert!(!light.dark && contrast(light.surface, light.text) >= 4.5);
		assert!(contrast(light.surface, light.muted) >= 4.5);
	}

	#[test]
	fn custom_colours_win_and_a_picture_is_read_once() {
		let ctx = egui::Context::default();
		assert_eq!(colors(), None);
		let image = Arc::new(poster(256, 256));
		crate::design::set_background_image(&ctx, Some(image.clone()));
		let themed = colors().expect("the poster's colours");
		assert!(themed.dark && themed.bar.r() > 200);
		let cached = |image: &Arc<egui::ColorImage>| {
			IMAGE.with(|cache| {
				cache
					.borrow()
					.as_ref()
					.is_some_and(|(seen, _)| seen.as_ptr() == Arc::as_ptr(image))
			})
		};
		assert!(cached(&image));
		set_custom(Some([[250, 250, 250], [200, 30, 30]]));
		let custom = colors().unwrap();
		assert_eq!(custom.surface, Color32::from_rgb(250, 250, 250));
		assert_eq!(custom.bar, Color32::from_rgb(200, 30, 30));
		set_custom(None);
		assert_eq!(colors(), Some(themed));
		crate::design::set_background_image(&ctx, None);
		assert_eq!(colors(), None);
	}
}
