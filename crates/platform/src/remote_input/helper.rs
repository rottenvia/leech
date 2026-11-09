//! The administrator helper for remote control. Windows (UIPI) drops input a program sends to
//! one running with more rights, so a controller could not use an app this PC runs as
//! administrator. When the host allows it, Leech starts itself again through the UAC prompt as
//! `leech.exe --remote-input-helper <pipe>`. That copy replays the `INPUT` records this PC's
//! injector built, read from a named pipe, and nothing else; it exits when the pipe closes.
//!
//! Each side checks the other. Leech writes only to the process it started (the pipe server's
//! id). The helper takes one client, its parent, from this PC, and quits at the first record
//! that is not exactly what the injector builds.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// The switch that makes `leech.exe` the helper; the pipe's name follows it.
pub const ARG: &str = "--remote-input-helper";

/// Pipe names are this, then 32 random hex digits.
const PREFIX: &str = "leech-input-";
/// Bytes per record on the pipe: five little-endian 32-bit fields.
const RECORD: usize = 20;
/// Records per message; the injector's largest batch (a held-state sync) stays under 140.
const MAX_RECORDS: usize = 256;

// The `INPUT` constants the records use, as Windows defines them.
const MOUSE: u32 = 0;
const KEYBOARD: u32 = 1;
const MOVE: u32 = 0x0001;
const LEFT_DOWN: u32 = 0x0002;
const LEFT_UP: u32 = 0x0004;
const RIGHT_DOWN: u32 = 0x0008;
const RIGHT_UP: u32 = 0x0010;
const MIDDLE_DOWN: u32 = 0x0020;
const MIDDLE_UP: u32 = 0x0040;
const X_DOWN: u32 = 0x0080;
const X_UP: u32 = 0x0100;
const WHEEL: u32 = 0x0800;
const HWHEEL: u32 = 0x1000;
const VIRTUAL_DESK: u32 = 0x4000;
const ABSOLUTE: u32 = 0x8000;
const ABSOLUTE_MOVE: u32 = MOVE | ABSOLUTE | VIRTUAL_DESK;
const EXTENDED_KEY: u32 = 0x0001;
const KEY_UP: u32 = 0x0002;
const SCAN_CODE: u32 = 0x0008;

/// One `INPUT` on the pipe: its type, then the mouse member's `dx`, `dy`, `mouseData` and
/// `dwFlags`, or the keyboard member's `wVk`, `wScan`, nothing and `dwFlags`. The helper sets
/// the time and the extra info itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
	kind: u32,
	a: i32,
	b: i32,
	data: u32,
	flags: u32,
}

impl Record {
	/// Exactly the shapes [`super::windows_input::to_input`] builds; anything else is refused.
	fn valid(self) -> bool {
		let still = self.a == 0 && self.b == 0;
		match self.kind {
			MOUSE => match self.flags {
				// Normalized over the virtual desktop.
				ABSOLUTE_MOVE => {
					(0..=0xFFFF).contains(&self.a)
						&& (0..=0xFFFF).contains(&self.b)
						&& self.data == 0
				}
				MOVE => self.data == 0,
				// The signed delta travels in `mouseData`.
				WHEEL | HWHEEL => still,
				X_DOWN | X_UP => still && matches!(self.data, 1 | 2),
				LEFT_DOWN | LEFT_UP | RIGHT_DOWN | RIGHT_UP | MIDDLE_DOWN | MIDDLE_UP => {
					still && self.data == 0
				}
				_ => false,
			},
			KEYBOARD => {
				self.a == 0
					&& (1..=0xFFFF).contains(&self.b)
					&& self.data == 0
					&& self.flags & !(EXTENDED_KEY | KEY_UP) == SCAN_CODE
			}
			_ => false,
		}
	}
}

fn encode(records: impl Iterator<Item = Record>) -> Vec<u8> {
	let mut message = Vec::with_capacity(RECORD * 8);
	for record in records {
		for field in [
			record.kind,
			record.a as u32,
			record.b as u32,
			record.data,
			record.flags,
		] {
			message.extend_from_slice(&field.to_le_bytes());
		}
	}
	message
}

/// A message's records, or `None` when it is empty, too long, cut short, or holds anything the
/// injector does not build.
fn decode(message: &[u8]) -> Option<Vec<Record>> {
	let (records, rest) = message.as_chunks::<RECORD>();
	if records.is_empty() || records.len() > MAX_RECORDS || !rest.is_empty() {
		return None;
	}
	records
		.iter()
		.map(|bytes| {
			let field = |index: usize| u32::from_le_bytes(bytes.as_chunks::<4>().0[index]);
			let record = Record {
				kind: field(0),
				a: field(1) as i32,
				b: field(2) as i32,
				data: field(3),
				flags: field(4),
			};
			record.valid().then_some(record)
		})
		.collect()
}

/// The pipe's full path for a name from the command line, which must look like one Leech made.
fn pipe_path(name: &str) -> Option<String> {
	let digits = name.strip_prefix(PREFIX)?;
	(digits.len() == 32
		&& digits
			.bytes()
			.all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
	.then(|| format!(r"\\.\pipe\{name}"))
}

/// Why the helper is not running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
	/// The UAC prompt was refused.
	Declined,
	Failed,
}

/// The connection to a running helper. Dropping it closes the pipe, which ends the helper.
pub struct Helper {
	#[cfg(target_os = "windows")]
	pipe: native::Pipe,
	#[cfg(not(target_os = "windows"))]
	never: std::convert::Infallible,
}

impl Helper {
	/// Starts the helper through the UAC prompt and connects to it. Blocks until the prompt is
	/// answered and the helper is up: call it off the UI thread.
	pub fn start() -> Result<Self, Failure> {
		#[cfg(target_os = "windows")]
		return native::start().map(|pipe| Self { pipe });
		#[cfg(not(target_os = "windows"))]
		Err(Failure::Failed)
	}

	/// Hands one batch to the helper; false once it is gone.
	pub(super) fn send(&mut self, events: &[super::Out]) -> bool {
		#[cfg(target_os = "windows")]
		return self.pipe.send(events);
		#[cfg(not(target_os = "windows"))]
		match self.never {}
	}
}

/// The helper process: replays what its parent sends until the pipe closes. Returns the exit
/// code (0 when the parent closed the pipe).
pub fn serve(name: &str) -> i32 {
	let Some(path) = pipe_path(name) else {
		return 2;
	};
	#[cfg(target_os = "windows")]
	return native::serve(&path);
	#[cfg(not(target_os = "windows"))]
	{
		let _ = path;
		1
	}
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod native {
	use super::{
		super::{INJECTED_MARK, Out, windows_input},
		ARG, Failure, KEYBOARD, MAX_RECORDS, MOUSE, PREFIX, RECORD, Record, decode, encode,
	};
	use std::sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	};
	use std::time::{Duration, Instant};
	use windows::{
		Win32::{
			Foundation::{
				CloseHandle, ERROR_BROKEN_PIPE, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND,
				ERROR_PIPE_CONNECTED, GENERIC_WRITE, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
			},
			Security::{
				Authorization::{
					ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
					SDDL_REVISION_1,
				},
				GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
				TOKEN_USER, TokenUser,
			},
			Storage::FileSystem::{
				CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES,
				FILE_READ_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
				ReadFile, WriteFile,
			},
			System::{
				Diagnostics::ToolHelp::{
					CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
					TH32CS_SNAPPROCESS,
				},
				Pipes::{
					ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
					GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
					PIPE_TYPE_MESSAGE, PIPE_WAIT,
				},
				Threading::{
					GetCurrentProcessId, GetProcessId, OpenProcess, OpenProcessToken,
					PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
				},
			},
			UI::{
				Input::KeyboardAndMouse::{
					INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
					MOUSE_EVENT_FLAGS, MOUSEINPUT, SendInput, VIRTUAL_KEY,
				},
				Shell::{
					SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
					ShellExecuteExW,
				},
				WindowsAndMessaging::{GetForegroundWindow, SW_HIDE},
			},
		},
		core::{HSTRING, PCWSTR, PWSTR, w},
	};

	/// How long the helper's pipe may take to appear once the prompt is accepted, and how long
	/// the helper waits for Leech to connect.
	const CONNECT: Duration = Duration::from_secs(10);
	/// The pipe's inbound buffer: many batches.
	const BUFFER: u32 = 64 * 1024;

	/// An owned handle, closed on drop.
	struct Owned(HANDLE);

	// SAFETY: a kernel handle may be used and closed from any thread.
	unsafe impl Send for Owned {}

	impl Drop for Owned {
		fn drop(&mut self) {
			// SAFETY: the handle is owned here and closed once.
			unsafe {
				let _ = CloseHandle(self.0);
			}
		}
	}

	/// Leech's end of the pipe.
	pub(super) struct Pipe(Owned);

	impl Pipe {
		pub(super) fn send(&mut self, events: &[Out]) -> bool {
			let desk = windows_input::desk();
			let inputs: Vec<INPUT> = events
				.iter()
				.map(|event| windows_input::to_input(*event, desk))
				.collect();
			inputs.chunks(MAX_RECORDS).all(|chunk| {
				let message = encode(chunk.iter().map(record));
				let mut written = 0;
				// SAFETY: a synchronous write of a buffer that outlives the call.
				let sent = unsafe { WriteFile(self.0.0, Some(&message), Some(&mut written), None) };
				sent.is_ok() && written as usize == message.len()
			})
		}
	}

	fn record(input: &INPUT) -> Record {
		// SAFETY: each branch reads the union member its type says is filled.
		unsafe {
			if input.r#type == INPUT_KEYBOARD {
				let key = input.Anonymous.ki;
				Record {
					kind: KEYBOARD,
					a: i32::from(key.wVk.0),
					b: i32::from(key.wScan),
					data: 0,
					flags: key.dwFlags.0,
				}
			} else {
				let mouse = input.Anonymous.mi;
				Record {
					kind: MOUSE,
					a: mouse.dx,
					b: mouse.dy,
					data: mouse.mouseData,
					flags: mouse.dwFlags.0,
				}
			}
		}
	}

	/// The `INPUT` a checked record stands for, marked as remote control's own.
	fn input(record: Record) -> INPUT {
		if record.kind == KEYBOARD {
			INPUT {
				r#type: INPUT_KEYBOARD,
				Anonymous: INPUT_0 {
					ki: KEYBDINPUT {
						wVk: VIRTUAL_KEY(record.a as u16),
						wScan: record.b as u16,
						dwFlags: KEYBD_EVENT_FLAGS(record.flags),
						time: 0,
						dwExtraInfo: INJECTED_MARK,
					},
				},
			}
		} else {
			INPUT {
				r#type: INPUT_MOUSE,
				Anonymous: INPUT_0 {
					mi: MOUSEINPUT {
						dx: record.a,
						dy: record.b,
						mouseData: record.data,
						dwFlags: MOUSE_EVENT_FLAGS(record.flags),
						time: 0,
						dwExtraInfo: INJECTED_MARK,
					},
				},
			}
		}
	}

	pub(super) fn start() -> Result<Pipe, Failure> {
		let mut random = [0; 16];
		getrandom::fill(&mut random).map_err(|_| Failure::Failed)?;
		let name = format!("{PREFIX}{:032x}", u128::from_le_bytes(random));
		let executable = std::env::current_exe().map_err(|_| Failure::Failed)?;
		let file = HSTRING::from(executable.as_os_str());
		let parameters = HSTRING::from(format!("{ARG} {name}"));
		let mut info = SHELLEXECUTEINFOW {
			cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
			fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
			// SAFETY: reads which window is in front; Windows shows its prompt over it.
			hwnd: unsafe { GetForegroundWindow() },
			lpVerb: w!("runas"),
			lpFile: PCWSTR(file.as_ptr()),
			lpParameters: PCWSTR(parameters.as_ptr()),
			nShow: SW_HIDE.0,
			..Default::default()
		};
		// SAFETY: every string is terminated and outlives the call; `info` is sized for it.
		if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
			return Err(if error.code() == ERROR_CANCELLED.to_hresult() {
				Failure::Declined
			} else {
				Failure::Failed
			});
		}
		if info.hProcess.is_invalid() {
			return Err(Failure::Failed);
		}
		let process = Owned(info.hProcess);
		// SAFETY: a process handle ShellExecuteExW returned.
		let id = unsafe { GetProcessId(process.0) };
		connect(&format!(r"\\.\pipe\{name}"), id, || {
			// SAFETY: polls the helper's handle without waiting.
			unsafe { WaitForSingleObject(process.0, 0) != WAIT_OBJECT_0 }
		})
	}

	/// Opens the pipe once it is up, and only when process `server` serves it.
	fn connect(path: &str, server: u32, alive: impl Fn() -> bool) -> Result<Pipe, Failure> {
		let path = HSTRING::from(path);
		let deadline = Instant::now() + CONNECT;
		let pipe = loop {
			// SAFETY: the path is terminated; the handle is owned once it opens.
			match unsafe {
				CreateFileW(
					&path,
					GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0,
					FILE_SHARE_NONE,
					None,
					OPEN_EXISTING,
					FILE_FLAGS_AND_ATTRIBUTES(0),
					None,
				)
			} {
				Ok(handle) => break Owned(handle),
				Err(error)
					if error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
						&& alive() && Instant::now() < deadline =>
				{
					std::thread::sleep(Duration::from_millis(20));
				}
				// Busy means another client got there first; the helper drops it and quits.
				Err(_) => return Err(Failure::Failed),
			}
		};
		let mut id = 0;
		// SAFETY: `id` receives the server's process id.
		let checked = unsafe { GetNamedPipeServerProcessId(pipe.0, &mut id) };
		if checked.is_err() || id != server {
			return Err(Failure::Failed);
		}
		Ok(Pipe(pipe))
	}

	pub(super) fn serve(path: &str) -> i32 {
		let Some(parent) = parent_id() else {
			return 3;
		};
		// Holding the parent open keeps its id from going to another process meanwhile.
		// SAFETY: the handle is owned once opened.
		let Ok(owner) = (unsafe {
			OpenProcess(
				PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
				false,
				parent,
			)
		})
		.map(Owned) else {
			return 3;
		};
		let Some(pipe) = listen(path, owner.0) else {
			return 4;
		};
		// Leech connects within moments; a helper nobody connects to, or whose parent is gone
		// before it does, does not linger.
		let connected = Arc::new(AtomicBool::new(false));
		let watch = connected.clone();
		let handle = owner.0.0 as usize;
		let _ = std::thread::Builder::new()
			.name("helper-watch".into())
			.spawn(move || {
				// SAFETY: `owner` outlives this process's useful life: it is only closed when
				// `serve` returns, after which the process exits.
				unsafe {
					WaitForSingleObject(HANDLE(handle as *mut _), CONNECT.as_millis() as u32);
				}
				if !watch.load(Ordering::Acquire) {
					std::process::exit(5);
				}
			});
		replay(&pipe, parent, &connected, |inputs| {
			// SAFETY: initialized INPUTs whose union member matches their type, with the
			// documented element size.
			unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
		})
	}

	/// The process that started this one.
	pub(super) fn parent_id() -> Option<u32> {
		// SAFETY: the snapshot handle is owned; the entry is sized as the API requires.
		unsafe {
			let snapshot = Owned(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?);
			let me = GetCurrentProcessId();
			let mut entry = PROCESSENTRY32W {
				dwSize: size_of::<PROCESSENTRY32W>() as u32,
				..Default::default()
			};
			let mut more = Process32FirstW(snapshot.0, &mut entry).is_ok();
			while more {
				if entry.th32ProcessID == me {
					return Some(entry.th32ParentProcessID).filter(|&id| id != 0);
				}
				more = Process32NextW(snapshot.0, &mut entry).is_ok();
			}
			None
		}
	}

	/// The account a process runs as, as a SID string.
	fn user_of(process: HANDLE) -> Option<String> {
		// SAFETY: the token is owned once opened; the user is read from a buffer aligned for
		// it that GetTokenInformation filled, its SID pointing inside it; the string Windows
		// allocates is freed once.
		unsafe {
			let mut token = HANDLE::default();
			OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
			let token = Owned(token);
			let mut buffer = [0_u64; 32];
			let mut length = 0;
			GetTokenInformation(
				token.0,
				TokenUser,
				Some(buffer.as_mut_ptr().cast()),
				size_of_val(&buffer) as u32,
				&mut length,
			)
			.ok()?;
			let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
			let mut text = PWSTR::null();
			ConvertSidToStringSidW(user.User.Sid, &mut text).ok()?;
			let sid = text.to_string().ok();
			LocalFree(Some(HLOCAL(text.0.cast())));
			sid
		}
	}

	/// Creates the only instance of the pipe, writable only by the account `owner` runs as and
	/// only from this PC. Its integrity label is Medium, so Leech, which is not elevated, may
	/// write to it.
	fn listen(path: &str, owner: HANDLE) -> Option<Owned> {
		let sid = user_of(owner)?;
		let sddl = HSTRING::from(format!("D:P(A;;GRGW;;;{sid})S:(ML;;NW;;;ME)"));
		let mut descriptor = PSECURITY_DESCRIPTOR::default();
		// SAFETY: the SDDL is terminated; the descriptor Windows allocates is freed below.
		unsafe {
			ConvertStringSecurityDescriptorToSecurityDescriptorW(
				&sddl,
				SDDL_REVISION_1,
				&mut descriptor,
				None,
			)
			.ok()?;
		}
		let attributes = SECURITY_ATTRIBUTES {
			nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
			lpSecurityDescriptor: descriptor.0,
			bInheritHandle: false.into(),
		};
		// SAFETY: the path is terminated and the attributes outlive the call; the descriptor
		// is freed once, after it.
		let pipe = unsafe {
			let pipe = CreateNamedPipeW(
				&HSTRING::from(path),
				PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
				PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
				1,
				0,
				BUFFER,
				0,
				Some(&attributes),
			);
			LocalFree(Some(HLOCAL(descriptor.0)));
			pipe
		};
		(!pipe.is_invalid()).then_some(Owned(pipe))
	}

	/// Waits for the client, which must be process `client`, then hands each checked batch to
	/// `sink` until the pipe closes (0) or anything is wrong (not 0).
	fn replay(
		pipe: &Owned,
		client: u32,
		connected: &AtomicBool,
		mut sink: impl FnMut(&[INPUT]),
	) -> i32 {
		// SAFETY: a synchronous wait on a pipe this process created.
		if let Err(error) = unsafe { ConnectNamedPipe(pipe.0, None) }
			&& error.code() != ERROR_PIPE_CONNECTED.to_hresult()
		{
			return 6;
		}
		let mut id = 0;
		// SAFETY: `id` receives the client's process id.
		let checked = unsafe { GetNamedPipeClientProcessId(pipe.0, &mut id) };
		if checked.is_err() || id != client {
			return 7;
		}
		connected.store(true, Ordering::Release);
		let mut buffer = vec![0_u8; RECORD * MAX_RECORDS];
		loop {
			let mut read = 0;
			// SAFETY: a synchronous read into a buffer that outlives the call. A message
			// longer than the buffer fails with ERROR_MORE_DATA.
			if let Err(error) =
				unsafe { ReadFile(pipe.0, Some(&mut buffer), Some(&mut read), None) }
			{
				return if error.code() == ERROR_BROKEN_PIPE.to_hresult() {
					0
				} else {
					8
				};
			}
			let Some(records) = decode(&buffer[..read as usize]) else {
				return 9;
			};
			sink(&records.into_iter().map(input).collect::<Vec<_>>());
		}
	}

	#[cfg(test)]
	mod tests {
		use super::super::super::{Injector, MouseButton, ScreenRect};
		use super::*;
		use windows::Win32::System::Threading::GetCurrentProcess;

		fn pipe_name() -> String {
			let mut random = [0; 16];
			getrandom::fill(&mut random).unwrap();
			format!(
				r"\\.\pipe\{PREFIX}test-{:032x}",
				u128::from_le_bytes(random)
			)
		}

		/// The helper's exit code and the batches it would have replayed.
		type Replayed = std::thread::JoinHandle<(i32, Vec<Vec<Record>>)>;

		/// A helper in this process: the server's end on a thread, recording what it would
		/// replay. Returns the pipe's path and the recording.
		fn helper(client: u32) -> (String, Replayed) {
			let path = pipe_name();
			// SAFETY: the pseudo handle of this process.
			let pipe = listen(&path, unsafe { GetCurrentProcess() }).expect("pipe");
			let thread = std::thread::spawn(move || {
				let mut seen = Vec::new();
				let code = replay(&pipe, client, &AtomicBool::new(false), |inputs| {
					seen.push(inputs.iter().map(record).collect());
				});
				(code, seen)
			});
			(path, thread)
		}

		fn me() -> u32 {
			// SAFETY: reads this process's id.
			unsafe { GetCurrentProcessId() }
		}

		#[test]
		fn the_injector_goes_through_the_helper_until_it_drops() {
			let (path, thread) = helper(me());
			let pipe = connect(&path, me(), || true).expect("connect");
			let screen = ScreenRect {
				x: 0,
				y: 0,
				width: 1920,
				height: 1080,
			};
			let mut injector = Injector::new(screen);
			injector.use_helper(super::super::Helper { pipe });
			assert!(injector.helper_running());
			injector.apply(
				crate::remote_input::Input::Key {
					scan: 0x1E,
					extended: false,
					down: true,
				},
				true,
				true,
			);
			injector.apply(
				crate::remote_input::Input::Button {
					button: MouseButton::Back,
					down: true,
				},
				true,
				true,
			);
			// Dropping lets go of what is held, through the helper, then closes the pipe.
			drop(injector);
			let (code, seen) = thread.join().unwrap();
			assert_eq!(code, 0);
			let desk = windows_input::desk();
			let expected: Vec<Vec<Record>> = [
				vec![Out::Key {
					scan: 0x1E,
					extended: false,
					down: true,
				}],
				vec![Out::Button {
					button: MouseButton::Back,
					down: true,
				}],
				vec![
					Out::Key {
						scan: 0x1E,
						extended: false,
						down: false,
					},
					Out::Button {
						button: MouseButton::Back,
						down: false,
					},
				],
			]
			.iter()
			.map(|batch| {
				batch
					.iter()
					.map(|event| record(&windows_input::to_input(*event, desk)))
					.collect()
			})
			.collect();
			assert_eq!(seen, expected);
		}

		#[test]
		fn the_helper_takes_only_its_parent() {
			// Process 4 is the System process, never this one.
			let (path, thread) = helper(4);
			let mut pipe = connect(&path, me(), || true).expect("connect");
			let (code, seen) = thread.join().unwrap();
			assert_eq!(code, 7);
			assert!(seen.is_empty());
			// Its pipe is closed: the injector's next send fails and it falls back.
			assert!(!pipe.send(&[Out::Relative { dx: 1, dy: 0 }]));
		}

		#[test]
		fn leech_talks_only_to_the_helper_it_started() {
			let (path, thread) = helper(me());
			assert!(matches!(connect(&path, 4, || true), Err(Failure::Failed)));
			// The refused connection closed its end, so the helper ends too, having replayed
			// nothing.
			assert!(thread.join().unwrap().1.is_empty());
			// A pipe that never appears is given up once the helper is gone.
			assert!(matches!(
				connect(&pipe_name(), me(), || false),
				Err(Failure::Failed)
			));
		}

		#[test]
		fn one_instance_and_garbage_ends_the_helper() {
			let (path, thread) = helper(me());
			// SAFETY: the pseudo handle of this process.
			assert!(listen(&path, unsafe { GetCurrentProcess() }).is_none());
			let pipe = connect(&path, me(), || true).expect("connect");
			let mut written = 0;
			// A keyboard record with a virtual key: not what the injector builds.
			let bad = encode(std::iter::once(Record {
				kind: KEYBOARD,
				a: 0x41,
				b: 0x1E,
				data: 0,
				flags: 8,
			}));
			// SAFETY: a synchronous write of a live buffer.
			unsafe { WriteFile(pipe.0.0, Some(&bad), Some(&mut written), None).unwrap() };
			let (code, seen) = thread.join().unwrap();
			assert_eq!(code, 9);
			assert!(seen.is_empty());
		}

		#[test]
		fn the_parent_is_known() {
			let parent = parent_id().expect("parent");
			assert_ne!(parent, me());
		}

		#[test]
		fn every_record_the_injector_builds_passes() {
			let desk = windows_input::Desk {
				x: -1920,
				y: 0,
				width: 3840,
				height: 1080,
				swapped: false,
			};
			let mut events = vec![
				Out::Absolute { x: -1920, y: 0 },
				Out::Absolute { x: 1919, y: 1079 },
				Out::Absolute { x: 5000, y: -50 },
				Out::Relative { dx: -40_000, dy: 7 },
				Out::Wheel {
					horizontal: false,
					delta: -360,
				},
				Out::Wheel {
					horizontal: true,
					delta: 120,
				},
			];
			for button in [
				MouseButton::Left,
				MouseButton::Right,
				MouseButton::Middle,
				MouseButton::Back,
				MouseButton::Forward,
			] {
				for down in [true, false] {
					events.push(Out::Button { button, down });
				}
			}
			for (scan, extended, down) in
				[(0x1E, false, true), (0x1D, true, false), (0x5B, true, true)]
			{
				events.push(Out::Key {
					scan,
					extended,
					down,
				});
			}
			let records: Vec<Record> = events
				.iter()
				.map(|event| record(&windows_input::to_input(*event, desk)))
				.collect();
			let message = encode(records.iter().copied());
			assert_eq!(decode(&message), Some(records.clone()));
			// What the helper replays is what was built, marked as remote control's own.
			for (event, record) in events.iter().zip(&records) {
				let built = windows_input::to_input(*event, desk);
				let replayed = input(*record);
				assert_eq!(replayed.r#type, built.r#type);
				// SAFETY: both carry the member their type names.
				unsafe {
					if built.r#type == INPUT_KEYBOARD {
						assert_eq!(replayed.Anonymous.ki, built.Anonymous.ki);
					} else {
						assert_eq!(replayed.Anonymous.mi, built.Anonymous.mi);
					}
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn key(scan: i32, flags: u32) -> Record {
		Record {
			kind: KEYBOARD,
			a: 0,
			b: scan,
			data: 0,
			flags,
		}
	}

	fn mouse(a: i32, b: i32, data: u32, flags: u32) -> Record {
		Record {
			kind: MOUSE,
			a,
			b,
			data,
			flags,
		}
	}

	#[test]
	fn only_the_injectors_shapes_pass() {
		for good in [
			mouse(0, 65535, 0, MOVE | ABSOLUTE | VIRTUAL_DESK),
			mouse(-3, 9000, 0, MOVE),
			mouse(0, 0, (-240_i32) as u32, WHEEL),
			mouse(0, 0, 120, HWHEEL),
			mouse(0, 0, 2, X_UP),
			mouse(0, 0, 0, LEFT_DOWN),
			mouse(0, 0, 0, MIDDLE_UP),
			key(0x1E, SCAN_CODE),
			key(0x1D, SCAN_CODE | EXTENDED_KEY | KEY_UP),
		] {
			assert!(good.valid(), "{good:?}");
		}
		for bad in [
			// Hardware input, unknown types.
			Record {
				kind: 2,
				..key(0x1E, SCAN_CODE)
			},
			Record {
				kind: 7,
				..mouse(0, 0, 0, MOVE)
			},
			// Absolute without the virtual desktop, outside it, or with data.
			mouse(10, 10, 0, MOVE | ABSOLUTE),
			mouse(65536, 0, 0, MOVE | ABSOLUTE | VIRTUAL_DESK),
			mouse(-1, 0, 0, MOVE | ABSOLUTE | VIRTUAL_DESK),
			mouse(1, 1, 5, MOVE),
			// Moves joined to presses, two presses at once, uncoalesced moves.
			mouse(1, 1, 0, MOVE | LEFT_DOWN),
			mouse(0, 0, 0, LEFT_DOWN | RIGHT_DOWN),
			mouse(1, 0, 0, MOVE | 0x2000),
			// A wheel or button that moves, a third X button, data on a button.
			mouse(5, 0, 120, WHEEL),
			mouse(0, 0, 3, X_DOWN),
			mouse(0, 0, 1, LEFT_UP),
			mouse(0, 0, 0, 0),
			// Virtual keys, Unicode text, no scan code, scan code 0, extra data.
			Record {
				a: 0x41,
				..key(0x1E, SCAN_CODE)
			},
			key(0x1E, SCAN_CODE | 0x4),
			key(0x1E, 0),
			key(0x1E, KEY_UP),
			key(0, SCAN_CODE),
			key(0x10000, SCAN_CODE),
			Record {
				data: 1,
				..key(0x1E, SCAN_CODE)
			},
		] {
			assert!(!bad.valid(), "{bad:?}");
		}
	}

	#[test]
	fn messages_are_whole_bounded_records() {
		let record = key(0x1E, SCAN_CODE);
		let one = encode(std::iter::once(record));
		assert_eq!(one.len(), RECORD);
		assert_eq!(decode(&one), Some(vec![record]));
		let full = encode(std::iter::repeat_n(record, MAX_RECORDS));
		assert_eq!(
			decode(&full).map(|records| records.len()),
			Some(MAX_RECORDS)
		);
		let over = encode(std::iter::repeat_n(record, MAX_RECORDS + 1));
		assert_eq!(decode(&over), None);
		assert_eq!(decode(&[]), None);
		assert_eq!(decode(&one[..RECORD - 1]), None);
		let mut longer = one.clone();
		longer.push(0);
		assert_eq!(decode(&longer), None);
		// One bad record spoils the batch.
		let mixed = encode([record, key(0x1E, SCAN_CODE | 0x4)].into_iter());
		assert_eq!(decode(&mixed), None);
	}

	#[test]
	fn only_names_leech_makes_are_served() {
		let name = format!("{PREFIX}{}", "0123456789abcdef".repeat(2));
		assert_eq!(
			pipe_path(&name).as_deref(),
			Some(format!(r"\\.\pipe\{name}").as_str())
		);
		for bad in [
			String::new(),
			PREFIX.to_owned(),
			format!("{PREFIX}{}", "0".repeat(31)),
			format!("{PREFIX}{}", "0".repeat(33)),
			format!("{PREFIX}{}", "A".repeat(32)),
			format!("{PREFIX}{}\\..\\x", "0".repeat(28)),
			format!("other-{}", "0".repeat(32)),
		] {
			assert_eq!(pipe_path(&bad), None, "{bad}");
		}
	}
}
