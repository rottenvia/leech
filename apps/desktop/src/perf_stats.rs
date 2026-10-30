//! Performance statistics, kept on this computer in `logs/performance.jsonl`: startup time,
//! frame times and memory use, with the version, system and graphics card they came from.
//! Stream and call quality stay in `stream.log`. Nothing names a person, server, channel or
//! message, and the file is bounded like the stream log.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Each file is cut at this size and kept once as `performance.previous.jsonl`.
const MAX_BYTES: u64 = 512 * 1024;
const FILE: &str = "performance.jsonl";
const PREVIOUS: &str = "performance.previous.jsonl";
/// One summary line per interval of use, and one when Leech closes.
const INTERVAL: Duration = Duration::from_secs(5 * 60);
const MEMORY_INTERVAL: Duration = Duration::from_secs(30);
/// Upper bounds of the frame-time buckets, in microseconds; the last bucket is open.
const LIMITS: [u64; 6] = [4_000, 8_000, 16_000, 33_000, 66_000, 132_000];

#[derive(Default)]
pub struct Stats {
	directory: Option<PathBuf>,
	/// Whether this session's header line has been written.
	introduced: bool,
	started: Option<Instant>,
	buckets: [u64; LIMITS.len() + 1],
	max_micros: u64,
	memory: u64,
	memory_peak: u64,
	next_memory: Option<Instant>,
	next_write: Option<Instant>,
	startup_ms: Option<u32>,
	session_frames: u64,
	session_slow: u64,
}

impl Stats {
	pub fn new(directory: Option<PathBuf>) -> Self {
		Self {
			directory,
			..Self::default()
		}
	}

	pub fn begin(&mut self) {
		self.started = Some(Instant::now());
	}

	/// Ends the frame begun last and, while statistics are on, counts it and writes a
	/// summary once an interval has passed. The session summary, when memory was sampled.
	pub fn finish(
		&mut self,
		enabled: bool,
		gpu: &str,
		refresh: Option<Duration>,
	) -> Option<String> {
		let started = self.started.take()?;
		if !enabled {
			*self = Self::new(self.directory.take());
			return None;
		}
		let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
		self.buckets[LIMITS.partition_point(|limit| *limit < micros)] += 1;
		self.max_micros = self.max_micros.max(micros);
		self.session_frames += 1;
		self.session_slow += u64::from(micros > 16_000);
		let now = Instant::now();
		let next = *self.next_write.get_or_insert(now + INTERVAL);
		if now >= next {
			self.next_write = Some(now + INTERVAL);
			self.write(gpu, refresh, false);
		}
		if self.next_memory.is_some_and(|next| now < next) {
			return None;
		}
		self.next_memory = Some(now + MEMORY_INTERVAL);
		if let Some(bytes) = platform::memory::private_working_set() {
			self.memory = bytes;
			self.memory_peak = self.memory_peak.max(bytes);
		}
		Some(self.summary())
	}

	/// The launch-to-ready time of this session.
	pub fn startup(&mut self, ms: u32) {
		self.startup_ms.get_or_insert(ms);
	}

	/// Writes what this interval holds; on close, `wait` for the file before exiting.
	pub fn write(&mut self, gpu: &str, refresh: Option<Duration>, wait: bool) {
		let frames: u64 = self.buckets.iter().sum();
		let Some(directory) = self.directory.clone().filter(|_| frames > 0) else {
			return;
		};
		let header = (!self.introduced).then(|| {
			serde_json::json!({
				"session": unix_seconds(),
				"version": env!("CARGO_PKG_VERSION"),
				"system": discord_voice::stream_log::system(),
				"arch": std::env::consts::ARCH,
				"gpu": gpu,
				"refresh_hz": refresh.map(|period| (1.0 / period.as_secs_f64()).round()),
			})
		});
		self.introduced = true;
		let line = serde_json::json!({
			"at": unix_seconds(),
			"startup_ms": self.startup_ms.take(),
			"frame_us_limits": LIMITS,
			"frame_us_buckets": self.buckets,
			"frame_us_max": self.max_micros,
			"memory_mb": self.memory >> 20,
			"memory_peak_mb": self.memory_peak >> 20,
		});
		self.buckets = Default::default();
		self.max_micros = 0;
		// Off the UI thread: the disk may be slow or asleep.
		let writer = std::thread::spawn(move || {
			let mut text = String::new();
			for record in header.iter().chain([&line]) {
				text.push_str(&record.to_string());
				text.push('\n');
			}
			append(&directory, &text);
		});
		if wait {
			let _ = writer.join();
		}
	}

	/// This session in one line, for the diagnostics copied into an issue.
	pub fn summary(&self) -> String {
		let mut text = format!(
			"frames {}, over 16 ms {}",
			self.session_frames, self.session_slow
		);
		if self.memory > 0 {
			text.push_str(&format!(
				", memory {} MB (peak {} MB)",
				self.memory >> 20,
				self.memory_peak >> 20
			));
		}
		text
	}
}

fn unix_seconds() -> u64 {
	SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.map_or(0, |since| since.as_secs())
}

fn append(directory: &Path, text: &str) {
	use std::io::Write as _;
	let path = directory.join(FILE);
	if std::fs::metadata(&path).is_ok_and(|meta| meta.len() + text.len() as u64 > MAX_BYTES) {
		let _ = std::fs::rename(&path, directory.join(PREVIOUS));
	}
	let _ = std::fs::create_dir_all(directory);
	if let Ok(mut file) = std::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(&path)
	{
		let _ = file.write_all(text.as_bytes());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn statistics_bucket_frames_and_stay_bounded() {
		let directory =
			std::env::temp_dir().join(format!("leech-performance-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&directory);
		let mut stats = Stats::new(Some(directory.clone()));
		stats.begin();
		let summary = stats.finish(
			true,
			"Test GPU (Dx12)",
			Some(Duration::from_secs_f64(1.0 / 144.0)),
		);
		assert!(summary.is_some_and(|summary| summary.starts_with("frames 1")));
		assert_eq!(stats.buckets.iter().sum::<u64>(), 1);
		stats.startup(1234);
		stats.write("Test GPU (Dx12)", None, true);
		assert_eq!(stats.buckets.iter().sum::<u64>(), 0);
		// A second write with no frames adds nothing.
		stats.write("Test GPU (Dx12)", None, true);
		let path = directory.join(FILE);
		let text = std::fs::read_to_string(&path).unwrap();
		let lines: Vec<serde_json::Value> = text
			.lines()
			.map(|line| serde_json::from_str(line).unwrap())
			.collect();
		assert_eq!(lines[0]["version"], env!("CARGO_PKG_VERSION"));
		assert_eq!(lines[0]["refresh_hz"], serde_json::Value::Null);
		assert_eq!(lines[1]["startup_ms"], 1234);
		assert!(stats.summary().starts_with("frames 1"));

		// Turning statistics off forgets the interval.
		stats.begin();
		stats.finish(false, "", None);
		assert_eq!(stats.session_frames, 0);

		// A full file rolls over once.
		std::fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();
		append(&directory, "{}\n");
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
		assert!(directory.join(PREVIOUS).exists());
		let _ = std::fs::remove_dir_all(&directory);
	}
}
