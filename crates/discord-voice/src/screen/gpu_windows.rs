//! GPU stream path for Windows capture.
//!
//! The D3D11 video processor scales each captured BGRA texture and converts it to NV12 at the
//! stream size, and the hardware encoder reads that texture in place. A 1440p desktop streamed
//! at 1080p then never crosses into system memory. The memory path (readback, `convert`,
//! upload) costs a 1440p frame about 15 ms of CPU, competing with the game being shown.
#![allow(unsafe_code)]

use crate::video_encode::hardware::Surface;
use std::{ffi::c_void, mem::ManuallyDrop, sync::Arc};
use windows::{
	Win32::{
		Foundation::RECT,
		Graphics::{
			Direct3D10::ID3D10Multithread,
			Direct3D11::*,
			Dxgi::{Common::*, IDXGIDevice},
		},
	},
	core::Interface,
};

const FAILED: &str = "GPU screen conversion is unavailable";

/// Pictures in flight: one being written, one queued, one kept for keyframe requests and a spare.
const POOL: usize = 4;

/// Preview images are at most this size, like the memory path's `preview_frame`.
const PREVIEW: (f64, f64) = (640.0, 360.0);

/// Rebind a windows-capture COM object (built on windows 0.62) as this crate's 0.61 binding.
///
/// # Safety
/// `From` must be a windows-rs interface for the same COM interface as `To`. Every
/// windows-rs version defines interfaces as `#[repr(transparent)]` non-null COM pointers.
pub(crate) unsafe fn rebind<From, To: Interface + Clone>(object: &From) -> Option<To> {
	if size_of::<From>() != size_of::<*mut c_void>() {
		return None;
	}
	// SAFETY: Per the contract above, `object` is exactly one COM pointer, borrowed and then
	// cloned (AddRef) into an owned binding.
	unsafe {
		let raw = std::mem::transmute_copy::<From, *mut c_void>(object);
		To::from_raw_borrowed(&raw).cloned()
	}
}

/// One video processor from an `input`-sized BGRA picture to `output` pixels of `format`.
struct Scaler {
	enumerator: ID3D11VideoProcessorEnumerator,
	processor: ID3D11VideoProcessor,
}

impl Scaler {
	unsafe fn new(
		video: &ID3D11VideoDevice,
		context: &ID3D11VideoContext,
		input: (u32, u32),
		output: (u32, u32),
		format: DXGI_FORMAT,
	) -> windows::core::Result<Self> {
		// SAFETY: Plain descriptor structs and owned COM objects on the capture thread.
		unsafe {
			let rate = DXGI_RATIONAL {
				Numerator: 60,
				Denominator: 1,
			};
			let enumerator =
				video.CreateVideoProcessorEnumerator(&D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
					InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
					InputFrameRate: rate,
					InputWidth: input.0,
					InputHeight: input.1,
					OutputFrameRate: rate,
					OutputWidth: output.0,
					OutputHeight: output.1,
					Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
				})?;
			let supports = |format, flag: D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT| {
				enumerator
					.CheckVideoProcessorFormat(format)
					.is_ok_and(|support| support & flag.0 as u32 != 0)
			};
			if !supports(
				DXGI_FORMAT_B8G8R8A8_UNORM,
				D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
			) || !supports(format, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT)
			{
				return Err(windows::core::Error::empty());
			}
			let processor = video.CreateVideoProcessor(&enumerator, 0)?;
			// BT.601 limited range, matching `convert` and what receivers assume without VUI.
			let yuv = format == DXGI_FORMAT_NV12;
			if let Ok(context) = context.cast::<ID3D11VideoContext1>() {
				context.VideoProcessorSetStreamColorSpace1(
					&processor,
					0,
					DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
				);
				context.VideoProcessorSetOutputColorSpace1(
					&processor,
					if yuv {
						DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601
					} else {
						DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
					},
				);
			} else {
				// Bit fields: Usage, RGB_Range (0 = full), YCbCr_Matrix (0 = BT.601),
				// YCbCr_xvYCC, then a two-bit Nominal_Range (1 = 16-235).
				context.VideoProcessorSetStreamColorSpace(
					&processor,
					0,
					&D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
				);
				context.VideoProcessorSetOutputColorSpace(
					&processor,
					&D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
						_bitfield: if yuv { 1 << 4 } else { 0 },
					},
				);
			}
			let (width, height) = super::convert::fit(
				(input.0 as usize, input.1 as usize),
				(output.0 as usize, output.1 as usize),
			);
			let (left, top) = (
				(output.0 as usize - width) / 2,
				(output.1 as usize - height) / 2,
			);
			context.VideoProcessorSetStreamFrameFormat(
				&processor,
				0,
				D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
			);
			context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
			context.VideoProcessorSetStreamSourceRect(
				&processor,
				0,
				true,
				Some(&rect((0, 0), input)),
			);
			context.VideoProcessorSetStreamDestRect(
				&processor,
				0,
				true,
				Some(&rect((left, top), (width as u32, height as u32))),
			);
			context.VideoProcessorSetOutputTargetRect(
				&processor,
				true,
				Some(&rect((0, 0), output)),
			);
			// Drivers fill an RGB background as full-range YUV (Y = 0), so give studio black
			// directly; the letterbox then matches `convert` and the encoder's black.
			context.VideoProcessorSetOutputBackgroundColor(
				&processor,
				yuv,
				&D3D11_VIDEO_COLOR {
					Anonymous: if yuv {
						D3D11_VIDEO_COLOR_0 {
							YCbCr: D3D11_VIDEO_COLOR_YCbCrA {
								Y: 16.0 / 255.0,
								Cb: 128.0 / 255.0,
								Cr: 128.0 / 255.0,
								A: 1.0,
							},
						}
					} else {
						D3D11_VIDEO_COLOR_0 {
							RGBA: D3D11_VIDEO_COLOR_RGBA {
								R: 0.0,
								G: 0.0,
								B: 0.0,
								A: 1.0,
							},
						}
					},
				},
			);
			Ok(Self {
				enumerator,
				processor,
			})
		}
	}

	unsafe fn target(
		&self,
		video: &ID3D11VideoDevice,
		texture: &ID3D11Texture2D,
	) -> windows::core::Result<ID3D11VideoProcessorOutputView> {
		let mut view = None;
		// SAFETY: The descriptor names mip 0 of a render-target texture of this device.
		unsafe {
			video.CreateVideoProcessorOutputView(
				texture,
				&self.enumerator,
				&D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
					ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
					Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
						Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
					},
				},
				Some(&mut view),
			)?;
		}
		view.ok_or_else(windows::core::Error::empty)
	}

	unsafe fn blt(
		&self,
		video: &ID3D11VideoDevice,
		context: &ID3D11VideoContext,
		source: &ID3D11Texture2D,
		target: &ID3D11VideoProcessorOutputView,
	) -> windows::core::Result<()> {
		// SAFETY: The input view lives until after the blit; the stream's ManuallyDrop field
		// is released exactly once below.
		unsafe {
			let mut view = None;
			video.CreateVideoProcessorInputView(
				source,
				&self.enumerator,
				&D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
					FourCC: 0,
					ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
					Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
						Texture2D: D3D11_TEX2D_VPIV {
							MipSlice: 0,
							ArraySlice: 0,
						},
					},
				},
				Some(&mut view),
			)?;
			let stream = D3D11_VIDEO_PROCESSOR_STREAM {
				Enable: true.into(),
				pInputSurface: ManuallyDrop::new(view),
				..Default::default()
			};
			let result = context.VideoProcessorBlt(
				&self.processor,
				target,
				0,
				std::slice::from_ref(&stream),
			);
			drop(ManuallyDrop::into_inner(stream.pInputSurface));
			result
		}
	}
}

fn rect((left, top): (usize, usize), (width, height): (u32, u32)) -> RECT {
	RECT {
		left: left as i32,
		top: top as i32,
		right: left as i32 + width as i32,
		bottom: top as i32 + height as i32,
	}
}

/// A small BGRA copy of the capture, read back for the local preview.
struct Preview {
	scaler: Scaler,
	size: (u32, u32),
	target: ID3D11VideoProcessorOutputView,
	texture: ID3D11Texture2D,
	staging: ID3D11Texture2D,
}

/// Converts captured textures of one size on windows-capture's device.
pub(crate) struct Converter {
	device: ID3D11Device,
	immediate: ID3D11DeviceContext,
	video: ID3D11VideoDevice,
	context: ID3D11VideoContext,
	input: (u32, u32),
	output: (u32, u32),
	stream: Scaler,
	/// Copy of a capture texture that cannot be viewed by the video processor directly.
	copy: Option<ID3D11Texture2D>,
	preview: Option<Preview>,
	pool: Vec<(Arc<Surface>, ID3D11VideoProcessorOutputView)>,
}

// SAFETY: windows-capture calls its handler, which owns the converter, under its own mutex,
// so the device context is never used from two threads at once.
unsafe impl Send for Converter {}

impl Converter {
	pub(crate) fn new(
		device: ID3D11Device,
		immediate: ID3D11DeviceContext,
		input: (u32, u32),
		output: (u32, u32),
	) -> Result<Self, &'static str> {
		// SAFETY: Owned COM objects of one device, used from the capture thread.
		unsafe {
			// The encoder worker reads the converted textures through this device; serialize
			// its use of the immediate context with the capture thread's.
			let _ = device
				.cast::<ID3D10Multithread>()
				.map_err(|_| FAILED)?
				.SetMultithreadProtected(true);
			// A game keeping the GPU busy must not delay the capture's small conversions: raise
			// this device's scheduling priority, as OBS does. Windows may refuse; that is harmless.
			if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
				let _ = dxgi.SetGPUThreadPriority(7);
			}
			let video: ID3D11VideoDevice = device.cast().map_err(|_| FAILED)?;
			let context: ID3D11VideoContext = immediate.cast().map_err(|_| FAILED)?;
			let stream = Scaler::new(&video, &context, input, output, DXGI_FORMAT_NV12)
				.map_err(|_| FAILED)?;
			let mut converter = Self {
				device,
				immediate,
				video,
				context,
				input,
				output,
				stream,
				copy: None,
				preview: None,
				pool: Vec::with_capacity(POOL),
			};
			converter.preview = converter.preview_target().ok();
			Ok(converter)
		}
	}

	pub(crate) fn input(&self) -> (u32, u32) {
		self.input
	}

	/// The capture as NV12 at the stream size, or `None` while every pooled picture is in use.
	pub(crate) fn convert(
		&mut self,
		source: &ID3D11Texture2D,
	) -> Result<Option<Arc<Surface>>, &'static str> {
		let index = match self
			.pool
			.iter()
			.position(|(surface, _)| Arc::strong_count(surface) == 1)
		{
			Some(index) => index,
			None if self.pool.len() < POOL => {
				let entry = self.surface().map_err(|_| FAILED)?;
				self.pool.push(entry);
				self.pool.len() - 1
			}
			None => return Ok(None),
		};
		let (surface, target) = self.pool[index].clone();
		// SAFETY: Both textures belong to this converter's device.
		if unsafe { self.blt(&self.stream, source, &target) }.is_err() {
			// A driver may refuse a view on the capture texture itself; the video processor
			// can always read a shader-resource copy of it.
			if self.copy.is_some() {
				return Err(FAILED);
			}
			self.copy = Some(
				self.texture(self.input, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_USAGE_DEFAULT)
					.map_err(|_| FAILED)?,
			);
			// SAFETY: As above, now reading this converter's own copy.
			unsafe { self.blt(&self.stream, source, &target) }.map_err(|_| FAILED)?;
		}
		Ok(Some(surface))
	}

	/// A preview image bounded to 640 by 360 pixels, read back from the GPU.
	pub(crate) fn preview(&mut self, source: &ID3D11Texture2D) -> Option<image::RgbaImage> {
		let preview = self.preview.as_ref()?;
		// SAFETY: The staging texture is mapped for reading and unmapped before returning;
		// rows are read within the mapped pitch.
		unsafe {
			self.blt(&preview.scaler, source, &preview.target).ok()?;
			self.immediate
				.CopyResource(&preview.staging, &preview.texture);
			let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
			self.immediate
				.Map(&preview.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
				.ok()?;
			let (width, height) = (preview.size.0 as usize, preview.size.1 as usize);
			let pitch = mapped.RowPitch as usize;
			let pixels = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * height);
			let mut image = Vec::with_capacity(width * height * 4);
			for row in pixels.chunks_exact(pitch) {
				for bgra in row[..width * 4].as_chunks::<4>().0 {
					image.extend_from_slice(&[bgra[2], bgra[1], bgra[0], 255]);
				}
			}
			self.immediate.Unmap(&preview.staging, 0);
			image::RgbaImage::from_raw(preview.size.0, preview.size.1, image)
		}
	}

	unsafe fn blt(
		&self,
		scaler: &Scaler,
		source: &ID3D11Texture2D,
		target: &ID3D11VideoProcessorOutputView,
	) -> windows::core::Result<()> {
		// SAFETY: All objects belong to this converter's device and capture thread.
		unsafe {
			if let Some(copy) = &self.copy {
				self.immediate.CopyResource(copy, source);
				return scaler.blt(&self.video, &self.context, copy, target);
			}
			scaler.blt(&self.video, &self.context, source, target)
		}
	}

	fn surface(&mut self) -> windows::core::Result<(Arc<Surface>, ID3D11VideoProcessorOutputView)> {
		let texture = self.texture(self.output, DXGI_FORMAT_NV12, D3D11_USAGE_DEFAULT)?;
		// SAFETY: A render-target texture of this device.
		let target = unsafe { self.stream.target(&self.video, &texture)? };
		Ok((
			Arc::new(Surface {
				texture,
				device: self.device.clone(),
			}),
			target,
		))
	}

	fn preview_target(&self) -> windows::core::Result<Preview> {
		let scale = (PREVIEW.0 / f64::from(self.input.0))
			.min(PREVIEW.1 / f64::from(self.input.1))
			.min(1.0);
		let size = (
			((f64::from(self.input.0) * scale).round() as u32).max(1),
			((f64::from(self.input.1) * scale).round() as u32).max(1),
		);
		// SAFETY: Owned COM objects of this converter's device.
		unsafe {
			let scaler = Scaler::new(
				&self.video,
				&self.context,
				self.input,
				size,
				DXGI_FORMAT_B8G8R8A8_UNORM,
			)?;
			let texture = self.texture(size, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_USAGE_DEFAULT)?;
			let target = scaler.target(&self.video, &texture)?;
			let staging = self.texture(size, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_USAGE_STAGING)?;
			Ok(Preview {
				scaler,
				size,
				target,
				texture,
				staging,
			})
		}
	}

	fn texture(
		&self,
		(width, height): (u32, u32),
		format: DXGI_FORMAT,
		usage: D3D11_USAGE,
	) -> windows::core::Result<ID3D11Texture2D> {
		let staging = usage == D3D11_USAGE_STAGING;
		let mut texture = None;
		// SAFETY: A fully initialized descriptor; the device returns an owned texture.
		unsafe {
			self.device.CreateTexture2D(
				&D3D11_TEXTURE2D_DESC {
					Width: width,
					Height: height,
					MipLevels: 1,
					ArraySize: 1,
					Format: format,
					SampleDesc: DXGI_SAMPLE_DESC {
						Count: 1,
						Quality: 0,
					},
					Usage: usage,
					BindFlags: if staging {
						0
					} else {
						(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32
					},
					CPUAccessFlags: if staging {
						D3D11_CPU_ACCESS_READ.0 as u32
					} else {
						0
					},
					MiscFlags: 0,
				},
				None,
				Some(&mut texture),
			)?;
		}
		texture.ok_or_else(windows::core::Error::empty)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::screen::{RawFrame, convert};
	use openh264::formats::YUVSource;

	fn psnr(actual: &[u8], expected: &[u8]) -> f64 {
		assert_eq!(actual.len(), expected.len());
		let error = actual
			.iter()
			.zip(expected)
			.map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
			.sum::<f64>()
			/ actual.len() as f64;
		10.0 * (255.0 * 255.0 / error.max(1e-9)).log10()
	}

	/// NV12 pixels of `texture`, rows packed.
	fn read(converter: &Converter, texture: &ID3D11Texture2D) -> Vec<u8> {
		let (width, height) = (converter.output.0 as usize, converter.output.1 as usize);
		let staging = converter
			.texture(converter.output, DXGI_FORMAT_NV12, D3D11_USAGE_STAGING)
			.unwrap();
		let mut nv12 = Vec::with_capacity(width * height * 3 / 2);
		// SAFETY: The staging copy is mapped for reading; both planes lie within it.
		unsafe {
			converter.immediate.CopyResource(&staging, texture);
			let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
			converter
				.immediate
				.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
				.unwrap();
			let pitch = mapped.RowPitch as usize;
			let bytes =
				std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * height * 3 / 2);
			for row in bytes.chunks_exact(pitch) {
				nv12.extend_from_slice(&row[..width]);
			}
			converter.immediate.Unmap(&staging, 0);
		}
		nv12
	}

	#[test]
	fn gpu_pictures_match_the_memory_path_and_encode_in_place() {
		// A 4:3 source streamed at 16:9 exercises scaling and the letterbox together.
		const INPUT: (u32, u32) = (960, 720);
		const OUTPUT: (u32, u32) = (640, 360);
		let (width, height) = (INPUT.0 as usize, INPUT.1 as usize);
		let mut bgra = vec![0; width * height * 4];
		for (index, pixel) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
			let (x, y) = (index % width, index / width);
			let block = if (x / 120 + y / 120) % 2 == 0 {
				220
			} else {
				40
			};
			*pixel = [
				(x * 255 / width) as u8,
				(y * 255 / height) as u8,
				block,
				255,
			];
		}
		// windows-capture's own device, rebound like a captured frame's.
		let Ok((device, immediate)) = windows_capture::d3d11::create_d3d_device() else {
			eprintln!("No Direct3D 11 device; GPU screen path not exercised");
			return;
		};
		let (device, immediate): (ID3D11Device, ID3D11DeviceContext) =
			unsafe { (rebind(&device).unwrap(), rebind(&immediate).unwrap()) };
		let mut texture = None;
		// SAFETY: The initial data covers every row of the described texture.
		unsafe {
			device
				.CreateTexture2D(
					&D3D11_TEXTURE2D_DESC {
						Width: INPUT.0,
						Height: INPUT.1,
						MipLevels: 1,
						ArraySize: 1,
						Format: DXGI_FORMAT_B8G8R8A8_UNORM,
						SampleDesc: DXGI_SAMPLE_DESC {
							Count: 1,
							Quality: 0,
						},
						Usage: D3D11_USAGE_DEFAULT,
						BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
						CPUAccessFlags: 0,
						MiscFlags: 0,
					},
					Some(&D3D11_SUBRESOURCE_DATA {
						pSysMem: bgra.as_ptr().cast(),
						SysMemPitch: INPUT.0 * 4,
						SysMemSlicePitch: 0,
					}),
					Some(&mut texture),
				)
				.unwrap();
		}
		let texture = texture.unwrap();
		let Ok(mut converter) = Converter::new(device.clone(), immediate, INPUT, OUTPUT) else {
			eprintln!("No D3D11 video processor; GPU screen path not exercised");
			return;
		};
		let surface = converter.convert(&texture).unwrap().unwrap();
		let frame = RawFrame {
			width: INPUT.0,
			height: INPUT.1,
			stride: width * 4,
			data: bgra,
			surface: None,
		};
		let (out_width, out_height) = (OUTPUT.0 as usize, OUTPUT.1 as usize);
		let mut expected = vec![0; out_width * out_height * 3 / 2];
		convert::bgra_to_yuv420(
			&frame,
			out_width,
			out_height,
			convert::Chroma::Interleaved,
			&mut expected,
		)
		.unwrap();
		let actual = read(&converter, &surface.texture);
		let quality = psnr(&actual, &expected);
		assert!(quality > 45.0, "GPU conversion PSNR {quality:.1} dB");
		// Black pillars in limited range: 480 picture columns centered in 640.
		assert_eq!(actual[180 * out_width + 40], 16);
		assert_eq!(
			&actual[out_width * out_height + 90 * out_width + 40..][..2],
			&[128, 128]
		);

		let preview = converter.preview(&texture).unwrap();
		assert_eq!(preview.dimensions(), (480, 360));
		let [r, g, b, a] = preview.get_pixel(470, 5).0;
		assert!(r.abs_diff(40) < 8 && g < 12 && b > 240 && a == 255);

		// The pool hands out each picture once; a full pool drops frames instead of overwriting.
		let held: Vec<_> = (1..POOL)
			.map(|_| converter.convert(&texture).unwrap().unwrap())
			.collect();
		assert!(converter.convert(&texture).unwrap().is_none());
		drop(held);
		assert!(converter.convert(&texture).unwrap().is_some());

		let config = crate::video_encode::Config {
			width: OUTPUT.0,
			height: OUTPUT.1,
			fps: 60,
			bit_rate: 4_000_000,
			max_bytes: crate::screen::MAX_ENCODED_BYTES,
			keyframe_interval: 3000,
			profile: crate::video_encode::Profile::Main,
		};
		let Ok(mut encoder) = crate::video_encode::hardware::Encoder::with_device(config, &device)
		else {
			eprintln!("No hardware H.264 encoder on this adapter; in-place encoding not exercised");
			return;
		};
		let mut planar = vec![0; out_width * out_height * 3 / 2];
		convert::bgra_to_yuv420(
			&frame,
			out_width,
			out_height,
			convert::Chroma::Planar,
			&mut planar,
		)
		.unwrap();
		let mut decoder = openh264::decoder::Decoder::new().unwrap();
		let mut decoded_any = false;
		for index in 0..4 {
			let (data, keyframe) = encoder.encode_surface(&surface, index == 0).unwrap();
			if data.is_empty() {
				continue;
			}
			assert!(keyframe || decoded_any);
			let Some(picture) = decoder.decode(&data).unwrap() else {
				continue;
			};
			assert_eq!(picture.dimensions(), (out_width, out_height));
			let (y_stride, u_stride, v_stride) = picture.strides();
			let mut decoded = Vec::with_capacity(planar.len());
			for row in picture.y().chunks(y_stride).take(out_height) {
				decoded.extend_from_slice(&row[..out_width]);
			}
			for (plane, stride) in [(picture.u(), u_stride), (picture.v(), v_stride)] {
				for row in plane.chunks(stride).take(out_height / 2) {
					decoded.extend_from_slice(&row[..out_width / 2]);
				}
			}
			let quality = psnr(&decoded, &planar);
			assert!(quality > 28.0, "Encoded GPU picture PSNR {quality:.1} dB");
			decoded_any = true;
		}
		assert!(decoded_any, "the hardware encoder produced no picture");
	}
}
