//! A low-level keyboard hook for the remote-control viewer, so system keys (Alt+Tab, the
//! Windows key, ...) reach the controlled PC instead of this one.
#![allow(unsafe_code)]

#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
	pub scan: u16,
	pub extended: bool,
	pub vk: u16,
	pub down: bool,
	pub injected: bool,
	/// The injector's tag; [`crate::remote_input::INJECTED_MARK`] marks Leech's own input.
	pub extra: usize,
}

#[cfg(target_os = "windows")]
pub use windows_hook::{Hook, key_name};

#[cfg(target_os = "windows")]
mod windows_hook {
	use super::{KeyEvent, fallback_name};
	use std::cell::RefCell;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::mpsc;
	use std::thread::JoinHandle;
	use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
	use windows::Win32::System::LibraryLoader::GetModuleHandleW;
	use windows::Win32::System::Threading::{
		GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
	};
	use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyNameTextW;
	use windows::Win32::UI::WindowsAndMessaging::{
		CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
		LLKHF_UP, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
		UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_QUIT,
	};
	use windows::core::PCWSTR;

	type Filter = Box<dyn Fn(KeyEvent) -> bool + Send + Sync>;

	/// Only one hook runs at a time.
	static RUNNING: AtomicBool = AtomicBool::new(false);
	thread_local! {
		/// The running hook's filter, on its own thread: Windows calls the hook procedure only
		/// there, while it waits in GetMessageW.
		static FILTER: RefCell<Option<Filter>> = const { RefCell::new(None) };
	}

	pub struct Hook {
		thread_id: u32,
		thread: Option<JoinHandle<()>>,
	}

	impl Hook {
		/// Low-level keyboard hook on its own message-loop thread. `filter` returns true to
		/// swallow the key (it never reaches other apps). Must return fast; never blocks.
		/// Swallow a key-up only when its down was swallowed too, or the key stays down here.
		pub fn start(
			filter: impl Fn(KeyEvent) -> bool + Send + Sync + 'static,
		) -> Result<Self, &'static str> {
			if RUNNING.swap(true, Ordering::AcqRel) {
				return Err("A keyboard hook is already running");
			}
			let (ready, started) = mpsc::sync_channel(1);
			let filter: Filter = Box::new(filter);
			let thread = std::thread::Builder::new()
				.name("keyboard-hook".to_owned())
				.spawn(move || run(filter, ready));
			let Ok(thread) = thread else {
				RUNNING.store(false, Ordering::Release);
				return Err("Could not start the keyboard hook thread");
			};
			match started
				.recv()
				.unwrap_or(Err("The keyboard hook thread stopped"))
			{
				Ok(thread_id) => Ok(Self {
					thread_id,
					thread: Some(thread),
				}),
				Err(error) => {
					let _ = thread.join();
					RUNNING.store(false, Ordering::Release);
					Err(error)
				}
			}
		}
	}

	impl Drop for Hook {
		fn drop(&mut self) {
			// SAFETY: posts to the thread this Hook owns. Its queue exists before `start`
			// returns; if the thread already left its loop, the post fails harmlessly.
			let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
			// Joined, the thread has unhooked: the filter is never called after this.
			if let Some(thread) = self.thread.take() {
				let _ = thread.join();
			}
			RUNNING.store(false, Ordering::Release);
		}
	}

	fn run(filter: Filter, ready: mpsc::SyncSender<Result<u32, &'static str>>) {
		FILTER.with_borrow_mut(|slot| *slot = Some(filter));
		// SAFETY: thread-local calls on this thread, a stack MSG, and a hook procedure that
		// lives for the whole program.
		let hook = unsafe {
			// Every keystroke on the system waits for this thread; keep it ahead of busy work.
			let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
			// Creates this thread's message queue, so Drop's WM_QUIT can always be posted.
			let mut message = MSG::default();
			let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
			let module = GetModuleHandleW(PCWSTR::null()).ok().map(Into::into);
			SetWindowsHookExW(WH_KEYBOARD_LL, Some(procedure), module, 0)
		};
		let Ok(hook) = hook else {
			FILTER.with_borrow_mut(|slot| *slot = None);
			let _ = ready.send(Err("Could not install the keyboard hook"));
			return;
		};
		// SAFETY: plain query of the calling thread.
		let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
		let mut message = MSG::default();
		// SAFETY: this thread's own queue; 0 (WM_QUIT) or -1 (error) ends the loop. Hook calls
		// arrive inside GetMessageW, so none comes once it stops being called.
		while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {}
		// SAFETY: the hook this thread installed above.
		let _ = unsafe { UnhookWindowsHookEx(hook) };
		FILTER.with_borrow_mut(|slot| *slot = None);
	}

	unsafe extern "system" fn procedure(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
		if code == HC_ACTION as i32 {
			// SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT valid for this call.
			let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
			let event = KeyEvent {
				scan: info.scanCode as u16,
				extended: info.flags.0 & LLKHF_EXTENDED.0 != 0,
				vk: info.vkCode as u16,
				down: info.flags.0 & LLKHF_UP.0 == 0,
				injected: info.flags.0 & LLKHF_INJECTED.0 != 0,
				extra: info.dwExtraInfo,
			};
			// A panic must not unwind into Windows; the key then passes through.
			let swallow = FILTER.with_borrow(|filter| {
				filter.as_ref().is_some_and(|filter| {
					std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filter(event)))
						.unwrap_or(false)
				})
			});
			if swallow {
				return LRESULT(1);
			}
		}
		// SAFETY: forwards this call's own arguments down the hook chain.
		unsafe { CallNextHookEx(None, code, wparam, lparam) }
	}

	/// Human name of a key for the UI ("F8", "Pause", "Right Ctrl"), from the keyboard layout.
	pub fn key_name(scan: u16, extended: bool) -> String {
		if scan == 0 || scan > 0xFF {
			return fallback_name(scan, extended);
		}
		// Bits 16-23 scan code, bit 24 extended; bit 25 stays clear to keep left/right apart.
		let lparam = (i32::from(scan) << 16) | (i32::from(extended) << 24);
		let mut name = [0_u16; 64];
		// SAFETY: a stack buffer; GetKeyNameTextW writes at most its length.
		let length = unsafe { GetKeyNameTextW(lparam, &mut name) };
		match usize::try_from(length) {
			Ok(length) if length > 0 => String::from_utf16_lossy(&name[..length.min(name.len())]),
			_ => fallback_name(scan, extended),
		}
	}
}

fn fallback_name(scan: u16, extended: bool) -> String {
	let code = if extended {
		0xE000 | u32::from(scan)
	} else {
		u32::from(scan)
	};
	format!("Key 0x{code:02X}")
}

#[cfg(not(target_os = "windows"))]
pub struct Hook {
	_private: (),
}

#[cfg(not(target_os = "windows"))]
impl Hook {
	/// Low-level keyboard hook on its own message-loop thread. `filter` returns true to swallow
	/// the key (it never reaches other apps). Must return fast; never blocks.
	pub fn start(
		_filter: impl Fn(KeyEvent) -> bool + Send + Sync + 'static,
	) -> Result<Self, &'static str> {
		Err("Keyboard hooks are available on Windows")
	}
}

/// Human name of a key for the UI ("F8", "Pause", "Right Ctrl").
#[cfg(not(target_os = "windows"))]
pub fn key_name(scan: u16, extended: bool) -> String {
	fallback_name(scan, extended)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unknown_keys_fall_back_to_their_code() {
		assert_eq!(key_name(0x1FF, false), "Key 0x1FF");
		assert_eq!(key_name(0, true), "Key 0xE000");
	}

	#[cfg(target_os = "windows")]
	#[test]
	fn function_keys_are_named_by_the_layout() {
		assert_eq!(key_name(0x42, false), "F8");
		assert_ne!(key_name(0x1D, true), key_name(0x1D, false));
	}

	#[cfg(target_os = "windows")]
	#[test]
	#[ignore = "installs a system-wide keyboard hook (passes every key through)"]
	fn only_one_hook_runs_at_a_time() {
		let hook = Hook::start(|_| false).expect("first hook");
		assert!(Hook::start(|_| false).is_err());
		drop(hook);
		drop(Hook::start(|_| false).expect("hook after drop"));
	}
}
