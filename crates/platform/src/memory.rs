//! glibc allocator settings for a long-running client that decodes media on many threads,
//! and this process's memory use. Other platforms and C libraries keep their defaults.

/// Applied once at startup, before worker threads exist. Each GStreamer streaming thread
/// otherwise gets its own arena, and glibc's dynamic mmap threshold moves multi-megabyte
/// video frames into those arenas once one is freed; every arena then keeps tens of MB of
/// freed frames resident. A fixed 1 MiB threshold maps such buffers separately, so freeing
/// them returns the memory, and four arenas bound what the remaining heaps can keep.
#[allow(unsafe_code)]
pub fn tune_allocator() {
	#[cfg(all(target_os = "linux", target_env = "gnu"))]
	{
		const M_MMAP_THRESHOLD: i32 = -3;
		const M_ARENA_MAX: i32 = -8;
		unsafe extern "C" {
			fn mallopt(param: i32, value: i32) -> i32;
		}
		// SAFETY: mallopt only changes allocator tuning and is called before other threads.
		unsafe {
			mallopt(M_MMAP_THRESHOLD, 1024 * 1024);
			mallopt(M_ARENA_MAX, 4);
		}
	}
}

/// This process's private working set in bytes: the "Memory" column of Task Manager.
/// `None` where it is not measured.
#[allow(unsafe_code)]
pub fn private_working_set() -> Option<u64> {
	#[cfg(windows)]
	{
		use windows::Win32::System::ProcessStatus::{
			GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
		};
		use windows::Win32::System::Threading::GetCurrentProcess;
		let mut counters = PROCESS_MEMORY_COUNTERS_EX2 {
			cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
			..Default::default()
		};
		// SAFETY: The pseudo handle needs no closing, and `cb` gives the struct's real size.
		unsafe {
			GetProcessMemoryInfo(
				GetCurrentProcess(),
				(&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
				counters.cb,
			)
		}
		.ok()
		.map(|()| counters.PrivateWorkingSetSize as u64)
	}
	#[cfg(not(windows))]
	None
}

/// Returns freed decoder memory to the system once playback or a poster batch ends.
/// Elsewhere this does nothing.
#[allow(unsafe_code)]
pub fn release_freed_memory() {
	#[cfg(all(target_os = "linux", target_env = "gnu"))]
	{
		unsafe extern "C" {
			fn malloc_trim(pad: usize) -> i32;
		}
		// SAFETY: glibc's malloc_trim is thread-safe and only releases free pages.
		unsafe {
			malloc_trim(0);
		}
	}
}

/// Maps a file read-only. Its pages stay file-backed, so the system can drop and reread them,
/// and they never count as this process's private memory the way a heap copy does.
#[allow(unsafe_code)]
pub fn map_file(
	path: &std::path::Path,
) -> std::io::Result<std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>> {
	let file = std::fs::File::open(path)?;
	// SAFETY: callers map only files the app wrote whole and renamed into place. Windows refuses
	// to truncate or rewrite a file while a mapping of it exists.
	let map = unsafe { memmap2::Mmap::map(&file)? };
	Ok(std::sync::Arc::new(map))
}

#[cfg(test)]
mod tests {
	#[test]
	fn a_mapped_file_reads_back_its_bytes() {
		let path = std::env::temp_dir().join(format!("leech-map-{}", std::process::id()));
		std::fs::write(&path, b"mapped bytes").unwrap();
		let map = super::map_file(&path).unwrap();
		assert_eq!((*map).as_ref(), b"mapped bytes");
		drop(map);
		std::fs::remove_file(&path).unwrap();
		assert!(super::map_file(&path).is_err());
	}

	#[test]
	#[cfg(windows)]
	fn the_private_working_set_is_measured() {
		let bytes = super::private_working_set().unwrap();
		assert!(
			bytes > 1024 * 1024 && bytes < 64 * 1024 * 1024 * 1024,
			"{bytes}"
		);
	}
}
