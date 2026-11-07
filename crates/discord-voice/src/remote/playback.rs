//! The host's system audio on the viewer's default speaker: Opus decoded on its own thread
//! into a small lock-free ring that the device callback reads.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use opus2::{Channels, Decoder};
use std::sync::{
	Arc,
	atomic::{AtomicU32, Ordering},
	mpsc,
};

/// Stereo frames at 48 kHz: playback starts (and restarts after running dry) with 60 ms
/// buffered, and skips back to that once more than 200 ms piles up.
const PREFILL: usize = 48 * 60;
const MAX_BUFFERED: usize = 48 * 200;
const RING: usize = 48 * 250;
/// Opus packets waiting for the decoder thread.
const QUEUE: usize = 16;
/// The longest Opus frame, 120 ms of stereo.
const MAX_DECODED: usize = 5760 * 2;

pub(super) struct Player {
	packets: Option<mpsc::SyncSender<Vec<u8>>>,
}

impl Player {
	/// Opens the speaker on its own thread, which ends when the player is dropped. Without a
	/// usable speaker, packets are dropped.
	pub(super) fn start(volume: Arc<AtomicU32>) -> Self {
		let (send, packets) = mpsc::sync_channel(QUEUE);
		let started = std::thread::Builder::new()
			.name("remote-audio".into())
			.spawn(move || play(packets, volume))
			.is_ok();
		Self {
			packets: started.then_some(send),
		}
	}

	/// One audio datagram: sequence, then an Opus packet.
	pub(super) fn push(&self, packet: Vec<u8>) {
		if let Some(packets) = &self.packets {
			let _ = packets.try_send(packet);
		}
	}
}

fn play(packets: mpsc::Receiver<Vec<u8>>, volume: Arc<AtomicU32>) {
	let (mut ring, output) = rtrb::RingBuffer::new(RING);
	let Ok(_stream) = open(output, volume) else {
		return;
	};
	let Ok(mut decoder) = Decoder::new(48_000, Channels::Stereo) else {
		return;
	};
	let mut pcm = vec![0.0; MAX_DECODED];
	let mut last = 0u32;
	while let Ok(packet) = packets.recv() {
		let Some((sequence, opus)) = packet.split_first_chunk::<4>() else {
			continue;
		};
		let sequence = u32::from_le_bytes(*sequence);
		// Late packets would only replay old sound.
		if sequence <= last {
			continue;
		}
		last = sequence;
		let Ok(frames) = decoder.decode_float(opus, &mut pcm, false) else {
			continue;
		};
		if ring.slots() < frames {
			continue;
		}
		for frame in pcm[..frames * 2].as_chunks::<2>().0 {
			let _ = ring.push(*frame);
		}
	}
}

fn open(
	output: rtrb::Consumer<[f32; 2]>,
	volume: Arc<AtomicU32>,
) -> Result<cpal::Stream, &'static str> {
	let device = cpal::default_host()
		.default_output_device()
		.ok_or("No speaker is available")?;
	let supported = device
		.default_output_config()
		.map_err(|_| "The speaker format is unavailable")?;
	let config = supported.config();
	let channels = usize::from(config.channels);
	if channels == 0 || !(8_000..=192_000).contains(&config.sample_rate) {
		return Err("The speaker format is unsupported");
	}
	let render = Render::new(output, config.sample_rate, volume);
	let stream = match supported.sample_format() {
		cpal::SampleFormat::F32 => build::<f32>(&device, &config, render, channels),
		cpal::SampleFormat::I16 => build::<i16>(&device, &config, render, channels),
		cpal::SampleFormat::I32 => build::<i32>(&device, &config, render, channels),
		cpal::SampleFormat::U16 => build::<u16>(&device, &config, render, channels),
		_ => Err("The speaker format is unsupported"),
	}?;
	stream.play().map_err(|_| "The speaker could not start")?;
	Ok(stream)
}

fn build<T>(
	device: &cpal::Device,
	config: &cpal::StreamConfig,
	mut render: Render,
	channels: usize,
) -> Result<cpal::Stream, &'static str>
where
	T: cpal::SizedSample + cpal::FromSample<f32>,
{
	device
		.build_output_stream(
			*config,
			move |data: &mut [T], _| {
				crate::timer::raise_audio_thread();
				render.render(data, channels);
			},
			|_| {},
			None,
		)
		.map_err(|_| "The speaker could not be opened")
}

/// Device callback state: prefill, latency cap, linear resampling to the device rate.
struct Render {
	input: rtrb::Consumer<[f32; 2]>,
	playing: bool,
	previous: [f32; 2],
	next: [f32; 2],
	phase: f64,
	step: f64,
	volume: Arc<AtomicU32>,
}

impl Render {
	fn new(input: rtrb::Consumer<[f32; 2]>, rate: u32, volume: Arc<AtomicU32>) -> Self {
		Self {
			input,
			playing: false,
			previous: [0.0; 2],
			next: [0.0; 2],
			phase: 1.0,
			step: 48_000.0 / f64::from(rate.max(1)),
			volume,
		}
	}

	fn render<T: cpal::SizedSample + cpal::FromSample<f32>>(
		&mut self,
		data: &mut [T],
		channels: usize,
	) {
		let buffered = self.input.slots();
		if buffered > MAX_BUFFERED
			&& let Ok(skipped) = self.input.read_chunk(buffered - PREFILL)
		{
			skipped.commit_all();
		}
		if !self.playing && self.input.slots() >= PREFILL {
			self.playing = true;
		}
		let gain = f32::from_bits(self.volume.load(Ordering::Relaxed));
		let level = |sample: f32| {
			T::from_sample(if sample.is_finite() {
				(sample * gain).clamp(-1.0, 1.0)
			} else {
				0.0
			})
		};
		for frame in data.chunks_mut(channels.max(1)) {
			let [left, right] = if self.playing {
				self.sample()
			} else {
				[0.0; 2]
			};
			match frame {
				[mono] => *mono = level((left + right) / 2.0),
				[first, second, rest @ ..] => {
					*first = level(left);
					*second = level(right);
					rest.fill(T::from_sample(0.0));
				}
				[] => {}
			}
		}
	}

	fn sample(&mut self) -> [f32; 2] {
		while self.phase >= 1.0 {
			self.previous = self.next;
			self.next = self.input.pop().unwrap_or_else(|_| {
				// Ran dry: silence until the prefill is back.
				self.playing = false;
				[0.0; 2]
			});
			self.phase -= 1.0;
		}
		let along = self.phase as f32;
		self.phase += self.step;
		[0, 1].map(|channel| {
			self.previous[channel] + (self.next[channel] - self.previous[channel]) * along
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn playback_prefills_caps_latency_and_maps_channels() {
		let (mut ring, output) = rtrb::RingBuffer::new(RING);
		let mut render = Render::new(output, 48_000, Arc::new(AtomicU32::new(0.5f32.to_bits())));
		let mut out = vec![1.0f32; 480 * 2];
		for _ in 0..PREFILL - 1 {
			ring.push([0.8, -0.4]).unwrap();
		}
		render.render(&mut out, 2);
		assert!(
			out.iter().all(|sample| *sample == 0.0),
			"silent before the prefill"
		);
		ring.push([0.8, -0.4]).unwrap();
		render.render(&mut out, 2);
		assert_eq!(&out[out.len() - 2..], [0.4, -0.2]);
		// Over the cap, playback skips back to the prefill.
		while ring.push([0.8, -0.4]).is_ok() {}
		render.render(&mut out, 2);
		assert!(render.input.slots() <= PREFILL);
		// Mono speakers get the mix; extra channels stay silent.
		let mut mono = vec![1.0f32; 4];
		render.render(&mut mono, 1);
		assert!(mono.iter().all(|sample| (*sample - 0.1).abs() < 1e-6));
		let mut surround = vec![1.0f32; 12];
		render.render(&mut surround, 6);
		assert_eq!(&surround[6..], [0.4, -0.2, 0.0, 0.0, 0.0, 0.0]);
	}
}
