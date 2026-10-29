//! A short, bounded log of screen-share health, written while sharing or watching so a tester
//! can send it after a laggy stream. It holds counts, timings and hardware names only: no
//! media, people, addresses or keys, and it never leaves this computer.
// Windows: local time, performance counters, DXGI adapter names and the build number.
#![allow(unsafe_code)]
use std::{
	fmt::Write as _,
	io::Write as _,
	path::{Path, PathBuf},
	sync::{
		Mutex, OnceLock,
		atomic::{AtomicU64, AtomicUsize, Ordering},
		mpsc,
	},
	time::Duration,
};

/// Each file is cut at this size and kept once as `stream.previous.log`.
const MAX_BYTES: u64 = 1024 * 1024;
const FILE: &str = "stream.log";
const PREVIOUS: &str = "stream.previous.log";

static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static SHARING: AtomicUsize = AtomicUsize::new(0);
static WATCHING: AtomicUsize = AtomicUsize::new(0);
static COUNTS: [AtomicU64; COUNTS_LEN] = [const { AtomicU64::new(0) }; COUNTS_LEN];
static WRITER: Mutex<Option<mpsc::SyncSender<String>>> = Mutex::new(None);

/// Per-second counters, reset each second: sums, or the largest value for the `Max` ones.
/// The `Now` ones are the latest value and outlive the second.
#[derive(Clone, Copy)]
pub(crate) enum Count {
	/// Pictures the OS delivered, and those dropped because the encoder still had the
	/// previous one or every GPU picture was in use.
	Captured,
	CaptureBusy,
	CapturePool,
	/// Pictures encoded, total and longest encode time in microseconds, and keyframes.
	Encoded,
	EncodeMicros,
	EncodeMaxMicros,
	Keyframes,
	/// Pictures skipped before encoding because the network queue was full.
	Behind,
	/// Frames and bytes sent, and the longest time one frame took to leave in microseconds.
	SentFrames,
	SentBytes,
	SendMaxMicros,
	/// Viewer feedback: packets asked for again, packets resent, keyframes asked for.
	NacksIn,
	Resent,
	KeyframeAsks,
	/// Sender rate control in kbit/s, the loss viewers report in 1/256, and the voice
	/// server's ping round trip with the part of it spent queued, in milliseconds.
	TargetNow,
	EstimateNow,
	LossNow,
	RoundTripNow,
	QueuedNow,
	/// Watching: video packets, packets found missing, recovered, given up and asked for.
	Packets,
	Lost,
	Repaired,
	Unrepaired,
	NacksOut,
	/// Watching: pictures shown, the longest gap between two in milliseconds, keyframes
	/// asked for, and frames dropped for arriving too late to decode.
	Pictures,
	GapMaxMillis,
	KeyframeAsksOut,
	Stale,
}
const COUNTS_LEN: usize = Count::Stale as usize + 1;

fn is_now(count: usize) -> bool {
	[
		Count::TargetNow,
		Count::EstimateNow,
		Count::LossNow,
		Count::RoundTripNow,
		Count::QueuedNow,
	]
	.iter()
	.any(|now| *now as usize == count)
}

pub(crate) fn add(count: Count, value: u64) {
	COUNTS[count as usize].fetch_add(value, Ordering::Relaxed);
}

pub(crate) fn max(count: Count, value: u64) {
	COUNTS[count as usize].fetch_max(value, Ordering::Relaxed);
}

pub(crate) fn set(count: Count, value: u64) {
	COUNTS[count as usize].store(value, Ordering::Relaxed);
}

/// Where the log goes; the desktop sets its data folder once at startup.
pub fn set_directory(directory: PathBuf) {
	let _ = DIRECTORY.set(directory);
}

/// The current log file, for "Show log file".
pub fn path() -> Option<PathBuf> {
	DIRECTORY.get().map(|directory| directory.join(FILE))
}

/// One line with the time, dropped when the writer is busy or no folder was set.
pub(crate) fn event(text: impl std::fmt::Display) {
	if let Ok(writer) = WRITER.lock()
		&& let Some(writer) = writer.as_ref()
	{
		let _ = writer.try_send(format!("{} {text}", clock()));
	}
}

/// Keeps the log running while a share or a watched stream lasts.
pub(crate) struct Session {
	watching: bool,
}

impl Session {
	pub(crate) fn sharing(settings: crate::screen::Settings) -> Self {
		let session = Self::start(false);
		event(format_args!(
			"=== sharing {}x{} at {} fps, up to {:.1} Mbps · Leech {} · {} ===",
			settings.width,
			settings.height,
			settings.fps,
			f64::from(settings.bit_rate()) / 1e6,
			env!("CARGO_PKG_VERSION"),
			system(),
		));
		session
	}

	pub(crate) fn watching() -> Self {
		let session = Self::start(true);
		event(format_args!(
			"=== watching a stream · Leech {} · {} ===",
			env!("CARGO_PKG_VERSION"),
			system(),
		));
		session
	}

	fn start(watching: bool) -> Self {
		if watching { &WATCHING } else { &SHARING }.fetch_add(1, Ordering::AcqRel);
		if let Ok(mut writer) = WRITER.lock()
			&& writer.is_none()
			&& let Some(directory) = DIRECTORY.get()
		{
			let (send, receive) = mpsc::sync_channel(64);
			let directory = directory.clone();
			if std::thread::Builder::new()
				.name("stream-log".into())
				.spawn(move || write_loop(&directory, &receive))
				.is_ok()
			{
				*writer = Some(send);
			}
		}
		Self { watching }
	}
}

impl Drop for Session {
	fn drop(&mut self) {
		event(if self.watching {
			"=== stopped watching ==="
		} else {
			"=== stopped sharing ==="
		});
		if self.watching { &WATCHING } else { &SHARING }.fetch_sub(1, Ordering::AcqRel);
	}
}

fn write_loop(directory: &Path, receive: &mpsc::Receiver<String>) {
	let mut load = load::Load::new();
	let mut file = None;
	let mut written = 0;
	let mut was_active = false;
	let mut next = std::time::Instant::now() + Duration::from_secs(1);
	loop {
		let line =
			match receive.recv_timeout(next.saturating_duration_since(std::time::Instant::now())) {
				Ok(line) => Some(line),
				Err(mpsc::RecvTimeoutError::Timeout) => None,
				Err(mpsc::RecvTimeoutError::Disconnected) => return,
			};
		let mut lines: Vec<String> = line.into_iter().collect();
		if std::time::Instant::now() >= next {
			// A late wake starts the next second now rather than writing empty catch-up lines.
			next = (next + Duration::from_secs(1)).max(std::time::Instant::now());
			let sharing = SHARING.load(Ordering::Acquire) > 0;
			let watching = WATCHING.load(Ordering::Acquire) > 0;
			let counts: [u64; COUNTS_LEN] = std::array::from_fn(|index| {
				if is_now(index) {
					COUNTS[index].load(Ordering::Relaxed)
				} else {
					COUNTS[index].swap(0, Ordering::Relaxed)
				}
			});
			// Usage is a rate since the previous sample, so the first sample of a stream (which
			// spans the idle time before it) only primes the counters; idle seconds take none.
			let active = sharing || watching;
			let system = if active {
				load.as_mut().map(load::Load::sample).filter(|_| was_active)
			} else {
				None
			};
			was_active = active;
			if active {
				let mut line = clock();
				if sharing {
					line.push_str(&share_line(&counts));
				}
				if watching {
					line.push_str(&watch_line(&counts));
				}
				if let Some(system) = system {
					line.push_str(&system);
				}
				lines.push(line);
			} else {
				for now in (0..COUNTS_LEN).filter(|count| is_now(*count)) {
					COUNTS[now].store(0, Ordering::Relaxed);
				}
			}
		}
		for line in lines {
			if file.is_none() || written >= MAX_BYTES {
				file = open(directory, &mut written);
			}
			if let Some(handle) = file.as_mut()
				&& handle.write_all(line.as_bytes()).is_ok()
				&& handle.write_all(b"\n").is_ok()
			{
				written += line.len() as u64 + 1;
			}
		}
	}
}

/// Appends to the log, first moving a full one aside so at most two files exist.
fn open(directory: &Path, written: &mut u64) -> Option<std::fs::File> {
	std::fs::create_dir_all(directory).ok()?;
	let path = directory.join(FILE);
	let size = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
	if size >= MAX_BYTES {
		let _ = std::fs::rename(&path, directory.join(PREVIOUS));
	}
	let file = std::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(&path)
		.ok()?;
	*written = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
	Some(file)
}

fn share_line(counts: &[u64; COUNTS_LEN]) -> String {
	let get = |count: Count| counts[count as usize];
	let encoded = get(Count::Encoded);
	let mut line = format!(
		" share capture={} busy={} pool={} encoded={} enc_ms={:.1}/{:.1} kf={} behind={} sent={} mbps={:.1} send_ms_max={:.1}",
		get(Count::Captured),
		get(Count::CaptureBusy),
		get(Count::CapturePool),
		encoded,
		get(Count::EncodeMicros) as f64 / encoded.max(1) as f64 / 1e3,
		get(Count::EncodeMaxMicros) as f64 / 1e3,
		get(Count::Keyframes),
		get(Count::Behind),
		get(Count::SentFrames),
		get(Count::SentBytes) as f64 * 8.0 / 1e6,
		get(Count::SendMaxMicros) as f64 / 1e3,
	);
	let _ = write!(
		line,
		" target_mbps={:.1} estimate_mbps={} loss={:.1}% rtt_ms={} queue_ms={} nack_in={} resent={} kf_asks={}",
		get(Count::TargetNow) as f64 / 1e3,
		match get(Count::EstimateNow) {
			0 => "-".into(),
			kbps => format!("{:.1}", kbps as f64 / 1e3),
		},
		get(Count::LossNow) as f64 * 100.0 / 256.0,
		match get(Count::RoundTripNow) {
			0 => "-".into(),
			millis => millis.to_string(),
		},
		get(Count::QueuedNow),
		get(Count::NacksIn),
		get(Count::Resent),
		get(Count::KeyframeAsks),
	);
	line
}

fn watch_line(counts: &[u64; COUNTS_LEN]) -> String {
	let get = |count: Count| counts[count as usize];
	format!(
		" watch packets={} lost={} repaired={} gave_up={} nack_out={} pictures={} gap_ms_max={} kf_asks_out={} late={}",
		get(Count::Packets),
		get(Count::Lost),
		get(Count::Repaired),
		get(Count::Unrepaired),
		get(Count::NacksOut),
		get(Count::Pictures),
		get(Count::GapMaxMillis),
		get(Count::KeyframeAsksOut),
		get(Count::Stale),
	)
}

/// Local wall-clock time of day, `HH:MM:SS`.
fn clock() -> String {
	#[cfg(target_os = "windows")]
	{
		// SAFETY: GetLocalTime only fills the returned struct.
		let time = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
		format!("{:02}:{:02}:{:02}", time.wHour, time.wMinute, time.wSecond)
	}
	#[cfg(not(target_os = "windows"))]
	{
		let seconds = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map_or(0, |elapsed| elapsed.as_secs());
		format!(
			"{:02}:{:02}:{:02} UTC",
			seconds / 3600 % 24,
			seconds / 60 % 60,
			seconds % 60
		)
	}
}

/// The OS and graphics adapters, for the session header.
fn system() -> String {
	#[cfg(target_os = "windows")]
	{
		use windows::Win32::Graphics::Dxgi::{
			CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
		};
		let mut text = format!("{} {}", std::env::consts::OS, os_build());
		// SAFETY: Enumeration of owned DXGI objects; descriptions are bounded UTF-16 arrays.
		unsafe {
			if let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() {
				let mut index = 0;
				while let Ok(adapter) = factory.EnumAdapters1(index) {
					index += 1;
					let Ok(desc) = adapter.GetDesc1() else {
						continue;
					};
					if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
						continue;
					}
					let end = desc
						.Description
						.iter()
						.position(|unit| *unit == 0)
						.unwrap_or(128);
					let name = String::from_utf16_lossy(&desc.Description[..end]);
					let _ = write!(
						text,
						" · GPU: {name} ({} MB)",
						desc.DedicatedVideoMemory >> 20
					);
				}
			}
		}
		text
	}
	#[cfg(not(target_os = "windows"))]
	std::env::consts::OS.to_owned()
}

/// The build Windows itself shows in winver, such as `26200.6899`.
#[cfg(target_os = "windows")]
fn os_build() -> String {
	use windows::{
		Win32::System::Registry::{
			HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
		},
		core::w,
	};
	let key = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
	let (mut build, mut build_bytes) = ([0u16; 16], 32u32);
	let (mut update, mut update_bytes) = (0u32, 4u32);
	// SAFETY: Each buffer and its byte size are passed together; values are bounded.
	unsafe {
		let _ = RegGetValueW(
			HKEY_LOCAL_MACHINE,
			key,
			w!("CurrentBuild"),
			RRF_RT_REG_SZ,
			None,
			Some(build.as_mut_ptr().cast()),
			Some(&mut build_bytes),
		);
		let _ = RegGetValueW(
			HKEY_LOCAL_MACHINE,
			key,
			w!("UBR"),
			RRF_RT_REG_DWORD,
			None,
			Some((&raw mut update).cast()),
			Some(&mut update_bytes),
		);
	}
	let end = build
		.iter()
		.position(|unit| *unit == 0)
		.unwrap_or(build.len());
	format!("{}.{update}", String::from_utf16_lossy(&build[..end]))
}

#[cfg(target_os = "windows")]
mod load {
	//! Whole-system GPU and CPU use from Windows performance counters, so a log shows when a
	//! game takes the GPU the stream needs.
	use std::collections::HashMap;
	use windows::{
		Win32::System::Performance::{
			PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
			PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
			PdhGetFormattedCounterArrayW, PdhOpenQueryW,
		},
		core::w,
	};

	pub(super) struct Load {
		query: PDH_HQUERY,
		gpu: PDH_HCOUNTER,
		cpu: PDH_HCOUNTER,
	}

	impl Load {
		pub(super) fn new() -> Option<Self> {
			let mut load = Self {
				query: PDH_HQUERY::default(),
				gpu: PDH_HCOUNTER::default(),
				cpu: PDH_HCOUNTER::default(),
			};
			// SAFETY: Out-parameters receive PDH handles; Drop closes the query and counters.
			unsafe {
				if PdhOpenQueryW(None, 0, &mut load.query) != 0 {
					return None;
				}
				if PdhAddEnglishCounterW(
					load.query,
					w!("\\GPU Engine(*)\\Utilization Percentage"),
					0,
					&mut load.gpu,
				) != 0 || PdhAddEnglishCounterW(
					load.query,
					w!("\\Processor(_Total)\\% Processor Time"),
					0,
					&mut load.cpu,
				) != 0
				{
					return None;
				}
				let _ = PdhCollectQueryData(load.query);
			}
			Some(load)
		}

		/// The busiest adapter's use per engine type and the CPU's, as ` gpu3d=… cpu=…`.
		pub(super) fn sample(&mut self) -> String {
			// SAFETY: The query and counters stay open until Drop.
			unsafe {
				if PdhCollectQueryData(self.query) != 0 {
					return String::new();
				}
				// Engine type per adapter: the sum over processes, as Task Manager shows it.
				let mut engines: HashMap<(String, String), f64> = HashMap::new();
				for (name, value) in values(self.gpu) {
					let (Some(adapter), Some(kind)) = (
						name.split("_phys_")
							.next()
							.and_then(|head| head.split("luid_").nth(1)),
						name.split("engtype_").nth(1),
					) else {
						continue;
					};
					*engines
						.entry((adapter.to_owned(), kind.to_owned()))
						.or_default() += value;
				}
				let mut busiest: HashMap<&str, f64> = HashMap::new();
				for ((_, kind), value) in &engines {
					let slot = busiest.entry(kind).or_default();
					*slot = slot.max(*value);
				}
				let mut text = String::new();
				for (label, kind) in [
					("gpu3d", "3D"),
					("gpu_encode", "VideoEncode"),
					("gpu_copy", "Copy"),
				] {
					if let Some(value) = busiest.get(kind) {
						text.push_str(&format!(" {label}={:.0}%", value.min(100.0)));
					}
				}
				if let Some((_, value)) = values(self.cpu).into_iter().next() {
					text.push_str(&format!(" cpu={value:.0}%"));
				}
				text
			}
		}
	}

	/// Every instance of one counter as (name, value).
	unsafe fn values(counter: PDH_HCOUNTER) -> Vec<(String, f64)> {
		// SAFETY: The caller passes a live counter; the buffer outlives the item pointers.
		unsafe {
			let (mut size, mut count) = (0u32, 0u32);
			let status =
				PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
			if status != PDH_MORE_DATA || size == 0 {
				return Vec::new();
			}
			let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
			let items = buffer.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
			if PdhGetFormattedCounterArrayW(
				counter,
				PDH_FMT_DOUBLE,
				&mut size,
				&mut count,
				Some(items),
			) != 0
			{
				return Vec::new();
			}
			std::slice::from_raw_parts(items, count as usize)
				.iter()
				.filter(|item| item.FmtValue.CStatus == 0)
				.map(|item| {
					(
						item.szName.to_string().unwrap_or_default(),
						item.FmtValue.Anonymous.doubleValue,
					)
				})
				.collect()
		}
	}

	impl Drop for Load {
		fn drop(&mut self) {
			// SAFETY: Closes the query opened in `new`, with its counters.
			unsafe {
				let _ = PdhCloseQuery(self.query);
			}
		}
	}
}

#[cfg(not(target_os = "windows"))]
mod load {
	pub(super) struct Load;
	impl Load {
		pub(super) fn new() -> Option<Self> {
			None
		}
		pub(super) fn sample(&mut self) -> String {
			String::new()
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lines_report_per_second_rates_and_keep_current_values() {
		let mut counts = [0u64; COUNTS_LEN];
		counts[Count::Captured as usize] = 60;
		counts[Count::Encoded as usize] = 60;
		counts[Count::EncodeMicros as usize] = 360_000;
		counts[Count::EncodeMaxMicros as usize] = 9_800;
		counts[Count::SentBytes as usize] = 1_500_000;
		counts[Count::TargetNow as usize] = 12_000;
		counts[Count::LossNow as usize] = 13;
		let line = share_line(&counts);
		assert!(line.contains("capture=60 "), "{line}");
		assert!(line.contains("enc_ms=6.0/9.8 "), "{line}");
		assert!(line.contains("mbps=12.0 "), "{line}");
		assert!(
			line.contains("target_mbps=12.0 estimate_mbps=- loss=5.1%"),
			"{line}"
		);
		counts[Count::Repaired as usize] = 3;
		assert!(watch_line(&counts).contains("repaired=3 "));
		assert!(is_now(Count::TargetNow as usize) && !is_now(Count::SentBytes as usize));
	}

	#[test]
	fn a_full_log_is_moved_aside_once() {
		let directory =
			std::env::temp_dir().join(format!("leech-stream-log-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&directory);
		std::fs::create_dir_all(&directory).unwrap();
		std::fs::write(directory.join(FILE), vec![b'x'; MAX_BYTES as usize]).unwrap();
		let mut written = 0;
		let mut file = open(&directory, &mut written).unwrap();
		assert_eq!(written, 0);
		file.write_all(b"fresh\n").unwrap();
		assert_eq!(
			std::fs::metadata(directory.join(PREVIOUS)).unwrap().len(),
			MAX_BYTES
		);
		assert_eq!(std::fs::read(directory.join(FILE)).unwrap(), b"fresh\n");
		drop(file);
		std::fs::remove_dir_all(&directory).unwrap();
	}
}
