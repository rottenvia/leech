//! Remote-control input on the host: the viewer's mouse and keyboard are replayed with
//! SendInput. The injector tracks what it holds, so a pause, a permission change or a lost
//! packet never leaves a key or button down.
#![allow(unsafe_code)]

/// Tags every event this module injects, so this PC's own keyboard hook can tell the
/// controller's keys from other injected ones (remappers, on-screen keyboards).
pub const INJECTED_MARK: usize = 0x4C45_4543;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
	/// Absolute position as a fraction (0..=1) of the host's captured screen, plus the raw
	/// mouse delta in device counts since the last move (for games with a captured mouse).
	Move {
		x: f32,
		y: f32,
		dx: i32,
		dy: i32,
	},
	Button {
		button: MouseButton,
		down: bool,
	},
	/// Wheel in WHEEL_DELTA units (120 per notch); dx is horizontal. Signs follow Windows:
	/// positive dy scrolls up (wheel away from the user), positive dx scrolls right.
	Wheel {
		dx: i32,
		dy: i32,
	},
	/// Keyboard by scan code (set 1), as a WH_KEYBOARD_LL hook reports it.
	Key {
		scan: u16,
		extended: bool,
		down: bool,
	},
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
	Left,
	Right,
	Middle,
	Back,
	Forward,
}

/// The captured screen, in virtual-desktop pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
	pub x: i32,
	pub y: i32,
	pub width: i32,
	pub height: i32,
}

/// Bounds the held set against a misbehaving peer; no keyboard holds this many keys.
const MAX_HELD_KEYS: usize = 64;
const ALT: u16 = 0x38;
const TAB: u16 = 0x0F;
/// Left and right Shift, then Ctrl and Alt (either side).
const MODIFIERS: [u16; 4] = [0x2A, 0x36, 0x1D, ALT];

/// Where a press would land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
	Open,
	/// One of this program's own windows: Leech never takes the controller's presses, so the
	/// controller cannot leave the call, end control or change what it may do.
	Own,
	/// A program running with more rights than this one (as administrator): Windows drops
	/// the input. It is still sent, in case Windows lets it through.
	Elevated,
}

/// Presses that did not reach their window since the last [`Injector::take_refused`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refused {
	/// Clicks and keys meant for Leech itself, which ignores them.
	pub own: bool,
	/// Input for a program running as administrator, which Windows blocks.
	pub elevated: bool,
}

/// One injected event. A batch goes out as a single SendInput call.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
enum Out {
	/// Virtual-desktop pixel.
	Absolute {
		x: i32,
		y: i32,
	},
	Relative {
		dx: i32,
		dy: i32,
	},
	Button {
		button: MouseButton,
		down: bool,
	},
	Wheel {
		horizontal: bool,
		delta: i32,
	},
	Key {
		scan: u16,
		extended: bool,
		down: bool,
	},
}

/// Where events go; tests record them instead of injecting.
trait Output {
	fn send(&mut self, events: &[Out]);
	/// The host's cursor is hidden or captured, so moves go out relative.
	fn captured(&self) -> bool;
	/// Where a press goes: the window in front for keys (`None`), or the one under the
	/// pointer, at the pixel this injector last moved it to when it knows it.
	fn target(&mut self, pointer: Option<Option<(i32, i32)>>) -> Target;
}

struct System;
impl Output for System {
	fn send(&mut self, events: &[Out]) {
		// Tests never inject real input, even through Drop.
		if !cfg!(test) && !events.is_empty() {
			send(events);
		}
	}
	fn captured(&self) -> bool {
		cursor_hidden()
	}
	fn target(&mut self, pointer: Option<Option<(i32, i32)>>) -> Target {
		target(pointer)
	}
}

pub struct Injector {
	screen: ScreenRect,
	keys: Vec<(u16, bool)>,
	buttons: Vec<MouseButton>,
	refused: Refused,
	/// The pixel of the last absolute move; Windows may not have moved the cursor there yet
	/// when the press right after it is checked.
	at: Option<(i32, i32)>,
}

impl Injector {
	pub fn new(screen: ScreenRect) -> Self {
		Self {
			screen,
			keys: Vec::new(),
			buttons: Vec::new(),
			refused: Refused::default(),
			at: None,
		}
	}

	/// What was refused since the last call.
	pub fn take_refused(&mut self) -> Refused {
		std::mem::take(&mut self.refused)
	}

	/// Whether a new press may go to its window; notes the ones that do not get there.
	fn admits(&mut self, out: &mut impl Output, pointer: bool) -> bool {
		match out.target(pointer.then_some(self.at)) {
			Target::Open => true,
			Target::Own => {
				self.refused.own = true;
				false
			}
			Target::Elevated => {
				self.refused.elevated = true;
				true
			}
		}
	}

	/// Whether a new key press may go to the window in front. Shift, Ctrl and Alt do nothing
	/// alone and Alt+Tab goes to Windows, so they pass even into Leech: the controller can
	/// still switch away from it.
	fn admits_key(&mut self, out: &mut impl Output, scan: u16) -> bool {
		MODIFIERS.contains(&scan)
			|| (scan == TAB && self.keys.iter().any(|&(held, _)| held == ALT))
			|| self.admits(out, false)
	}

	/// Applies one input when allowed; tracks what is held.
	pub fn apply(&mut self, input: Input, keyboard: bool, mouse: bool) {
		self.apply_to(&mut System, input, keyboard, mouse);
	}

	/// Releases every key and button this injector holds (pause, permission change, end).
	pub fn release_all(&mut self) {
		self.release_all_to(&mut System);
	}

	/// Brings held state in line with the viewer's snapshot (lost packets never leave a key down).
	pub fn sync_held(
		&mut self,
		keys: &[(u16, bool)],
		buttons: &[MouseButton],
		keyboard: bool,
		mouse: bool,
	) {
		self.sync_held_to(&mut System, keys, buttons, keyboard, mouse);
	}

	fn apply_to(&mut self, out: &mut impl Output, input: Input, keyboard: bool, mouse: bool) {
		match input {
			Input::Move { x, y, dx, dy } if mouse => {
				let event = if out.captured() {
					if dx == 0 && dy == 0 {
						return;
					}
					self.at = None;
					Out::Relative { dx, dy }
				} else {
					let at = (
						pixel(x, self.screen.x, self.screen.width),
						pixel(y, self.screen.y, self.screen.height),
					);
					self.at = Some(at);
					Out::Absolute { x: at.0, y: at.1 }
				};
				out.send(&[event]);
			}
			Input::Wheel { dx, dy } if mouse => {
				if !self.admits(out, true) {
					return;
				}
				let mut events = Vec::with_capacity(2);
				if dy != 0 {
					events.push(Out::Wheel {
						horizontal: false,
						delta: dy,
					});
				}
				if dx != 0 {
					events.push(Out::Wheel {
						horizontal: true,
						delta: dx,
					});
				}
				out.send(&events);
			}
			// Releasing what is held is always allowed; pressing needs the permission.
			Input::Button { button, down } => {
				let held = self.buttons.contains(&button);
				if down {
					if !mouse || held || !self.admits(out, true) {
						return;
					}
					self.buttons.push(button);
				} else if held {
					self.buttons.retain(|held| *held != button);
				} else {
					return;
				}
				out.send(&[Out::Button { button, down }]);
			}
			Input::Key {
				scan,
				extended,
				down,
			} => {
				let key = (scan, extended);
				let held = self.keys.contains(&key);
				if down {
					// A down for a held key is auto-repeat and passes through.
					if !keyboard
						|| scan == 0 || (!held
						&& (self.keys.len() >= MAX_HELD_KEYS || !self.admits_key(out, scan)))
					{
						return;
					}
					if !held {
						self.keys.push(key);
					}
				} else if held {
					self.keys.retain(|held| *held != key);
				} else {
					return;
				}
				out.send(&[Out::Key {
					scan,
					extended,
					down,
				}]);
			}
			Input::Move { .. } | Input::Wheel { .. } => {}
		}
	}

	fn release_all_to(&mut self, out: &mut impl Output) {
		let keys = self.keys.drain(..).map(|(scan, extended)| Out::Key {
			scan,
			extended,
			down: false,
		});
		let buttons = self.buttons.drain(..).map(|button| Out::Button {
			button,
			down: false,
		});
		out.send(&keys.chain(buttons).collect::<Vec<_>>());
	}

	/// Releases held entries missing from the snapshot (and all of a kind that is not allowed),
	/// then presses snapshot entries not held yet.
	fn sync_held_to(
		&mut self,
		out: &mut impl Output,
		keys: &[(u16, bool)],
		buttons: &[MouseButton],
		keyboard: bool,
		mouse: bool,
	) {
		let mut events = Vec::new();
		self.keys.retain(|&(scan, extended)| {
			let keep = keyboard && keys.contains(&(scan, extended));
			if !keep {
				events.push(Out::Key {
					scan,
					extended,
					down: false,
				});
			}
			keep
		});
		self.buttons.retain(|&button| {
			let keep = mouse && buttons.contains(&button);
			if !keep {
				events.push(Out::Button {
					button,
					down: false,
				});
			}
			keep
		});
		if keyboard {
			for &(scan, extended) in keys {
				if scan != 0
					&& self.keys.len() < MAX_HELD_KEYS
					&& !self.keys.contains(&(scan, extended))
					&& self.admits_key(out, scan)
				{
					self.keys.push((scan, extended));
					events.push(Out::Key {
						scan,
						extended,
						down: true,
					});
				}
			}
		}
		if mouse {
			for &button in buttons {
				if !self.buttons.contains(&button) && self.admits(out, true) {
					self.buttons.push(button);
					events.push(Out::Button { button, down: true });
				}
			}
		}
		out.send(&events);
	}
}

impl Drop for Injector {
	fn drop(&mut self) {
		self.release_all();
	}
}

/// The pixel at `fraction` (0..=1) along a span of the screen; out-of-range and NaN clamp.
fn pixel(fraction: f32, start: i32, size: i32) -> i32 {
	let size = size.max(1);
	// `as` saturates and maps NaN to 0.
	let offset = (f64::from(fraction) * f64::from(size)).floor() as i32;
	start.saturating_add(offset.clamp(0, size - 1))
}

/// SendInput maps a normalized coordinate n to pixel origin + floor(n * size / 65536) (as
/// observed, not documented), so the smallest n that lands on `pixel` is
/// ceil((pixel - origin) * 65536 / size). That stays under half a pixel past it for any size
/// below 32768, so a rounding mapping lands there too.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn normalize(pixel: i32, origin: i32, size: i32) -> i32 {
	let size = i64::from(size.max(1));
	let offset = (i64::from(pixel) - i64::from(origin)).clamp(0, size - 1);
	((offset * 65536 + size - 1) / size).min(65535) as i32
}

#[cfg(target_os = "windows")]
pub use windows_input::cursor_hidden;
#[cfg(target_os = "windows")]
use windows_input::{send, target};

#[cfg(target_os = "windows")]
mod windows_input {
	use super::{MouseButton, Out, Target, normalize};
	use windows::Win32::Foundation::{CloseHandle, HANDLE, POINT, RECT};
	use windows::Win32::Security::{
		GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
		TOKEN_QUERY, TokenIntegrityLevel,
	};
	use windows::Win32::System::Threading::{
		GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
		PROCESS_QUERY_LIMITED_INFORMATION,
	};
	use windows::Win32::UI::Input::KeyboardAndMouse::{
		INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
		KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
		MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
		MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
		MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
		SendInput, VIRTUAL_KEY,
	};
	use windows::Win32::UI::WindowsAndMessaging::{
		CURSOR_SHOWING, CURSOR_SUPPRESSED, CURSORINFO, GetClipCursor, GetCursorInfo, GetCursorPos,
		GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SM_CXVIRTUALSCREEN,
		SM_CYVIRTUALSCREEN, SM_SWAPBUTTON, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WindowFromPoint,
		XBUTTON1, XBUTTON2,
	};

	/// A cursor clipped into a square this small (px) is captured by a game.
	const CAPTURE_CLIP: i32 = 32;

	/// The virtual desktop at send time, and whether the user swapped the mouse buttons.
	#[derive(Clone, Copy)]
	pub(super) struct Desk {
		pub x: i32,
		pub y: i32,
		pub width: i32,
		pub height: i32,
		pub swapped: bool,
	}

	pub(super) fn send(events: &[Out]) {
		// SAFETY: GetSystemMetrics only reads system values.
		let desk = unsafe {
			Desk {
				x: GetSystemMetrics(SM_XVIRTUALSCREEN),
				y: GetSystemMetrics(SM_YVIRTUALSCREEN),
				width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
				height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
				swapped: GetSystemMetrics(SM_SWAPBUTTON) != 0,
			}
		};
		let inputs: Vec<INPUT> = events.iter().map(|event| to_input(*event, desk)).collect();
		// SAFETY: initialized INPUTs whose union member matches their type, with the
		// documented element size. UIPI or the secure desktop may still drop them silently.
		unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
	}

	pub(super) fn to_input(event: Out, desk: Desk) -> INPUT {
		let mouse = |dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS| INPUT {
			r#type: INPUT_MOUSE,
			Anonymous: INPUT_0 {
				mi: MOUSEINPUT {
					dx,
					dy,
					// Signed wheel deltas travel in this DWORD as two's complement.
					mouseData: data as u32,
					dwFlags: flags,
					time: 0,
					dwExtraInfo: super::INJECTED_MARK,
				},
			},
		};
		match event {
			Out::Absolute { x, y } => mouse(
				normalize(x, desk.x, desk.width),
				normalize(y, desk.y, desk.height),
				0,
				MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
			),
			Out::Relative { dx, dy } => mouse(dx, dy, 0, MOUSEEVENTF_MOVE),
			Out::Wheel { horizontal, delta } => mouse(
				0,
				0,
				delta,
				if horizontal {
					MOUSEEVENTF_HWHEEL
				} else {
					MOUSEEVENTF_WHEEL
				},
			),
			Out::Button { button, down } => {
				// SendInput presses physical buttons; the viewer means primary/secondary.
				let button = match (button, desk.swapped) {
					(MouseButton::Left, true) => MouseButton::Right,
					(MouseButton::Right, true) => MouseButton::Left,
					(button, _) => button,
				};
				let (flags, data) = match (button, down) {
					(MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
					(MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
					(MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
					(MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
					(MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
					(MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
					(MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
					(MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
					(MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
					(MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
				};
				mouse(0, 0, i32::from(data), flags)
			}
			Out::Key {
				scan,
				extended,
				down,
			} => {
				// By scan code, so games reading raw input or DirectInput see the key.
				let mut flags = KEYEVENTF_SCANCODE;
				if extended {
					flags |= KEYEVENTF_EXTENDEDKEY;
				}
				if !down {
					flags |= KEYEVENTF_KEYUP;
				}
				INPUT {
					r#type: INPUT_KEYBOARD,
					Anonymous: INPUT_0 {
						ki: KEYBDINPUT {
							wVk: VIRTUAL_KEY(0),
							wScan: scan,
							dwFlags: flags,
							time: 0,
							dwExtraInfo: super::INJECTED_MARK,
						},
					},
				}
			}
		}
	}

	/// The window a press reaches: the one in front for keys, or the one under the pointer.
	pub(super) fn target(pointer: Option<Option<(i32, i32)>>) -> Target {
		// SAFETY: plain window queries; a window that is gone gives a null handle or pid 0.
		let window = unsafe {
			match pointer {
				None => GetForegroundWindow(),
				Some(Some((x, y))) => WindowFromPoint(POINT { x, y }),
				Some(None) => {
					let mut at = POINT::default();
					if GetCursorPos(&mut at).is_err() {
						return Target::Open;
					}
					WindowFromPoint(at)
				}
			}
		};
		if window.is_invalid() {
			return Target::Open;
		}
		let mut process = 0;
		// SAFETY: as above.
		unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
		if process == 0 {
			return Target::Open;
		}
		// SAFETY: reads this process's id.
		if process == unsafe { GetCurrentProcessId() } {
			return Target::Own;
		}
		thread_local! {
			/// The last process asked about and whether it outranks this one.
			static LAST: std::cell::Cell<(u32, bool)> = const { std::cell::Cell::new((0, false)) };
		}
		let (known, elevated) = LAST.get();
		let elevated = if known == process {
			elevated
		} else {
			// A process whose rights cannot even be read runs above this one.
			let elevated = integrity(Some(process))
				.is_none_or(|theirs| integrity(None).is_some_and(|ours| theirs > ours));
			LAST.set((process, elevated));
			elevated
		};
		if elevated {
			Target::Elevated
		} else {
			Target::Open
		}
	}

	#[cfg(test)]
	pub(super) fn integrity_of_self() -> Option<u32> {
		integrity(None)
	}

	/// The mandatory integrity level (UIPI's rank) of a process, or of this one.
	fn integrity(process: Option<u32>) -> Option<u32> {
		// SAFETY: every handle opened here is closed here; the label is read from a buffer
		// aligned for it that GetTokenInformation filled, and its SID points inside it.
		unsafe {
			let handle = match process {
				Some(id) => OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, id).ok()?,
				None => GetCurrentProcess(),
			};
			let mut token = HANDLE::default();
			let opened = OpenProcessToken(handle, TOKEN_QUERY, &mut token);
			if process.is_some() {
				let _ = CloseHandle(handle);
			}
			opened.ok()?;
			let mut buffer = [0_u64; 16];
			let mut length = 0;
			let read = GetTokenInformation(
				token,
				TokenIntegrityLevel,
				Some(buffer.as_mut_ptr().cast()),
				size_of_val(&buffer) as u32,
				&mut length,
			);
			let _ = CloseHandle(token);
			read.ok()?;
			let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
			let count = *GetSidSubAuthorityCount(label.Label.Sid);
			Some(*GetSidSubAuthority(
				label.Label.Sid,
				u32::from(count).checked_sub(1)?,
			))
		}
	}

	/// True while the host's cursor is hidden (a game captured the mouse): the viewer then
	/// sends relative motion and locks its own pointer.
	pub fn cursor_hidden() -> bool {
		let mut info = CURSORINFO {
			cbSize: size_of::<CURSORINFO>() as u32,
			..Default::default()
		};
		let mut clip = RECT::default();
		// SAFETY: stack structs; cbSize is set as GetCursorInfo requires. Both fail on the
		// secure desktop, which then counts as not hidden.
		let (cursor, clipped) = unsafe { (GetCursorInfo(&mut info), GetClipCursor(&mut clip)) };
		if cursor.is_err() {
			return false;
		}
		// Games hide the cursor with ShowCursor (flags) or SetCursor(NULL) (no handle).
		info.flags.0 & CURSOR_SHOWING.0 == 0
			|| info.flags.0 & CURSOR_SUPPRESSED.0 != 0
			|| info.hCursor.is_invalid()
			|| (clipped.is_ok()
				&& clip.right - clip.left <= CAPTURE_CLIP
				&& clip.bottom - clip.top <= CAPTURE_CLIP)
	}
}

#[cfg(not(target_os = "windows"))]
fn send(_events: &[Out]) {}

#[cfg(not(target_os = "windows"))]
fn target(_pointer: Option<Option<(i32, i32)>>) -> Target {
	Target::Open
}

/// True while the host's cursor is hidden (a game captured the mouse): the viewer then sends
/// relative motion and locks its own pointer.
#[cfg(not(target_os = "windows"))]
pub fn cursor_hidden() -> bool {
	false
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Recorder {
		sent: Vec<Out>,
		captured: bool,
		target: Target,
	}
	impl Default for Recorder {
		fn default() -> Self {
			Self {
				sent: Vec::new(),
				captured: false,
				target: Target::Open,
			}
		}
	}
	impl Output for Recorder {
		fn send(&mut self, events: &[Out]) {
			self.sent.extend_from_slice(events);
		}
		fn captured(&self) -> bool {
			self.captured
		}
		fn target(&mut self, _pointer: Option<Option<(i32, i32)>>) -> Target {
			self.target
		}
	}
	impl Recorder {
		fn take(&mut self) -> Vec<Out> {
			std::mem::take(&mut self.sent)
		}
	}

	const SCREEN: ScreenRect = ScreenRect {
		x: -1920,
		y: 0,
		width: 1920,
		height: 1080,
	};
	fn key(scan: u16, down: bool) -> Input {
		Input::Key {
			scan,
			extended: false,
			down,
		}
	}
	fn out_key(scan: u16, down: bool) -> Out {
		Out::Key {
			scan,
			extended: false,
			down,
		}
	}

	#[test]
	fn normalized_coordinates_land_on_every_pixel() {
		for (origin, size) in [
			(0, 1),
			(0, 2),
			(0, 1080),
			(-1920, 1366),
			(-1920, 1920),
			(0, 2560),
			(-3840, 7680),
			(0, 32767),
		] {
			for offset in 0..size {
				let n = i64::from(normalize(origin + offset, origin, size));
				assert!((0..=65535).contains(&n));
				// The truncating mapping Windows uses and a rounding one both land on it.
				assert_eq!(
					n * i64::from(size) / 65536,
					i64::from(offset),
					"{size} {offset}"
				);
				let rounded = ((n * i64::from(size) + 32768) / 65536).min(i64::from(size) - 1);
				assert_eq!(rounded, i64::from(offset), "{size} {offset}");
			}
			assert_eq!(normalize(origin - 5, origin, size), 0);
			assert_eq!(
				normalize(origin + size + 5, origin, size),
				normalize(origin + size - 1, origin, size)
			);
		}
	}

	#[test]
	fn fractions_map_inside_the_screen() {
		assert_eq!(pixel(0.0, -1920, 1920), -1920);
		assert_eq!(pixel(0.5, -1920, 1920), -960);
		assert_eq!(pixel(1.0, -1920, 1920), -1);
		assert_eq!(pixel(1.7, 0, 1080), 1079);
		assert_eq!(pixel(-0.2, 0, 1080), 0);
		assert_eq!(pixel(f32::NAN, 10, 1080), 10);
		assert_eq!(pixel(f32::INFINITY, 0, 1080), 1079);
		assert_eq!(pixel(0.5, 0, 0), 0);
	}

	#[test]
	fn moves_are_absolute_unless_the_cursor_is_captured() {
		let mut out = Recorder::default();
		let mut injector = Injector::new(SCREEN);
		let moved = Input::Move {
			x: 1.0,
			y: 0.0,
			dx: 3,
			dy: -2,
		};
		injector.apply_to(&mut out, moved, true, false);
		assert!(out.take().is_empty());
		injector.apply_to(&mut out, moved, false, true);
		assert_eq!(out.take(), [Out::Absolute { x: -1, y: 0 }]);
		out.captured = true;
		injector.apply_to(&mut out, moved, false, true);
		assert_eq!(out.take(), [Out::Relative { dx: 3, dy: -2 }]);
		let still = Input::Move {
			x: 0.5,
			y: 0.5,
			dx: 0,
			dy: 0,
		};
		injector.apply_to(&mut out, still, false, true);
		assert!(out.take().is_empty());
		injector.apply_to(&mut out, Input::Wheel { dx: -120, dy: 240 }, false, true);
		assert_eq!(
			out.take(),
			[
				Out::Wheel {
					horizontal: false,
					delta: 240
				},
				Out::Wheel {
					horizontal: true,
					delta: -120
				}
			]
		);
	}

	#[test]
	fn keys_track_held_state_and_pass_auto_repeat() {
		let mut out = Recorder::default();
		let mut injector = Injector::new(SCREEN);
		// No permission, unknown key-up and scan 0 inject nothing.
		injector.apply_to(&mut out, key(0x1E, true), false, true);
		injector.apply_to(&mut out, key(0x1E, false), true, true);
		injector.apply_to(&mut out, key(0, true), true, true);
		assert!(out.take().is_empty());
		injector.apply_to(&mut out, key(0x1E, true), true, false);
		injector.apply_to(&mut out, key(0x1E, true), true, false);
		assert_eq!(out.take(), [out_key(0x1E, true), out_key(0x1E, true)]);
		assert_eq!(injector.keys, [(0x1E, false)]);
		// Releasing a held key works even after the permission is gone.
		injector.apply_to(&mut out, key(0x1E, false), false, false);
		injector.apply_to(&mut out, key(0x1E, false), true, false);
		assert_eq!(out.take(), [out_key(0x1E, false)]);
		assert!(injector.keys.is_empty());
		// Left and right Ctrl differ only by the extended bit.
		let right_ctrl = Input::Key {
			scan: 0x1D,
			extended: true,
			down: true,
		};
		injector.apply_to(&mut out, right_ctrl, true, false);
		injector.apply_to(&mut out, key(0x1D, false), true, false);
		assert_eq!(
			out.take(),
			[Out::Key {
				scan: 0x1D,
				extended: true,
				down: true
			}]
		);
		assert_eq!(injector.keys, [(0x1D, true)]);
		injector.keys.clear();
		for scan in 1..=200 {
			injector.apply_to(&mut out, key(scan, true), true, false);
		}
		assert_eq!(injector.keys.len(), MAX_HELD_KEYS);
		injector.keys.clear();
	}

	#[test]
	fn buttons_never_repeat_and_release_all_lets_go() {
		let mut out = Recorder::default();
		let mut injector = Injector::new(SCREEN);
		let press = |button, down| Input::Button { button, down };
		injector.apply_to(&mut out, press(MouseButton::Back, true), true, true);
		injector.apply_to(&mut out, press(MouseButton::Back, true), true, true);
		injector.apply_to(&mut out, press(MouseButton::Left, false), true, true);
		injector.apply_to(&mut out, press(MouseButton::Right, true), true, false);
		injector.apply_to(&mut out, key(0x2A, true), true, true);
		assert_eq!(
			out.take(),
			[
				Out::Button {
					button: MouseButton::Back,
					down: true
				},
				out_key(0x2A, true)
			]
		);
		injector.release_all_to(&mut out);
		assert_eq!(
			out.take(),
			[
				out_key(0x2A, false),
				Out::Button {
					button: MouseButton::Back,
					down: false
				}
			]
		);
		injector.release_all_to(&mut out);
		assert!(out.take().is_empty());
	}

	#[test]
	fn sync_repairs_lost_presses_and_releases() {
		let mut out = Recorder::default();
		let mut injector = Injector::new(SCREEN);
		injector.apply_to(&mut out, key(0x11, true), true, true);
		injector.apply_to(
			&mut out,
			Input::Button {
				button: MouseButton::Left,
				down: true,
			},
			true,
			true,
		);
		out.take();
		// W's up and A's down were lost; the right button's down too.
		injector.sync_held_to(
			&mut out,
			&[(0x1E, false), (0x1E, false)],
			&[MouseButton::Left, MouseButton::Right],
			true,
			true,
		);
		assert_eq!(
			out.take(),
			[
				out_key(0x11, false),
				out_key(0x1E, true),
				Out::Button {
					button: MouseButton::Right,
					down: true
				}
			]
		);
		// In line already: nothing to do.
		injector.sync_held_to(
			&mut out,
			&[(0x1E, false)],
			&[MouseButton::Right, MouseButton::Left],
			true,
			true,
		);
		assert!(out.take().is_empty());
		// A kind that is not allowed is released and never pressed.
		injector.sync_held_to(
			&mut out,
			&[(0x1E, false), (0x20, false)],
			&[MouseButton::Left, MouseButton::Right],
			false,
			true,
		);
		assert_eq!(out.take(), [out_key(0x1E, false)]);
		injector.release_all_to(&mut out);
		assert_eq!(out.take().len(), 2);
	}

	#[test]
	fn leech_itself_takes_no_presses_and_admin_apps_are_noted() {
		let mut out = Recorder {
			target: Target::Own,
			..Recorder::default()
		};
		let mut injector = Injector::new(SCREEN);
		let left = |down| Input::Button {
			button: MouseButton::Left,
			down,
		};
		injector.apply_to(&mut out, left(true), true, true);
		injector.apply_to(&mut out, key(0x1E, true), true, true);
		injector.apply_to(&mut out, Input::Wheel { dx: 0, dy: 120 }, true, true);
		injector.sync_held_to(&mut out, &[(0x1E, false)], &[MouseButton::Left], true, true);
		// Their ups have nothing to lift; moving over Leech is fine.
		injector.apply_to(&mut out, left(false), true, true);
		injector.apply_to(&mut out, key(0x1E, false), true, true);
		let moved = Input::Move {
			x: 0.0,
			y: 0.0,
			dx: 0,
			dy: 0,
		};
		injector.apply_to(&mut out, moved, true, true);
		assert_eq!(out.take(), [Out::Absolute { x: -1920, y: 0 }]);
		assert_eq!(
			injector.take_refused(),
			Refused {
				own: true,
				elevated: false
			}
		);
		assert_eq!(injector.take_refused(), Refused::default());
		// A key held before Leech came to the front still repeats and lifts.
		out.target = Target::Open;
		injector.apply_to(&mut out, key(0x1E, true), true, true);
		out.target = Target::Own;
		injector.apply_to(&mut out, key(0x1E, true), true, true);
		injector.apply_to(&mut out, key(0x1E, false), true, true);
		assert_eq!(
			out.take(),
			[
				out_key(0x1E, true),
				out_key(0x1E, true),
				out_key(0x1E, false)
			]
		);
		// Alt+Tab still switches away from Leech; Alt+F4 does not close it.
		for (scan, down) in [(0x38, true), (0x0F, true), (0x0F, false), (0x3E, true)] {
			injector.apply_to(&mut out, key(scan, down), true, true);
		}
		assert_eq!(
			out.take(),
			[
				out_key(0x38, true),
				out_key(0x0F, true),
				out_key(0x0F, false)
			]
		);
		assert!(injector.take_refused().own);
		injector.apply_to(&mut out, key(0x38, false), true, true);
		assert_eq!(out.take(), [out_key(0x38, false)]);
		// Admin apps get the input anyway, and it is noted.
		out.target = Target::Elevated;
		injector.apply_to(&mut out, left(true), true, true);
		assert_eq!(out.take().len(), 1);
		assert!(injector.take_refused().elevated);
		injector.release_all_to(&mut out);
	}

	/// This test binary runs at the same level as itself.
	#[cfg(target_os = "windows")]
	#[test]
	fn this_process_reads_its_own_integrity() {
		assert!(windows_input::integrity_of_self().is_some());
	}

	#[cfg(target_os = "windows")]
	#[test]
	fn windows_events_carry_the_right_flags() {
		use windows::Win32::UI::Input::KeyboardAndMouse::{
			INPUT_KEYBOARD, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
			MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_VIRTUALDESK,
			MOUSEEVENTF_WHEEL, MOUSEEVENTF_XUP,
		};
		let desk = windows_input::Desk {
			x: -1920,
			y: 0,
			width: 3840,
			height: 1080,
			swapped: true,
		};
		let mouse = |event| {
			// SAFETY: to_input fills the mouse member for mouse events.
			unsafe { windows_input::to_input(event, desk).Anonymous.mi }
		};
		let moved = mouse(Out::Absolute { x: 1919, y: 1079 });
		assert_eq!(
			moved.dwFlags,
			MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
		);
		assert_eq!(((moved.dx * 3840) >> 16) - 1920, 1919);
		assert_eq!((moved.dy * 1080) >> 16, 1079);
		let left = mouse(Out::Button {
			button: MouseButton::Left,
			down: true,
		});
		assert_eq!(left.dwFlags, MOUSEEVENTF_RIGHTDOWN);
		let back = mouse(Out::Button {
			button: MouseButton::Back,
			down: false,
		});
		assert_eq!((back.dwFlags, back.mouseData), (MOUSEEVENTF_XUP, 1));
		let wheel = mouse(Out::Wheel {
			horizontal: false,
			delta: -120,
		});
		assert_eq!(
			(wheel.dwFlags, wheel.mouseData as i32),
			(MOUSEEVENTF_WHEEL, -120)
		);
		let input = windows_input::to_input(
			Out::Key {
				scan: 0x1D,
				extended: true,
				down: false,
			},
			desk,
		);
		assert_eq!(input.r#type, INPUT_KEYBOARD);
		// SAFETY: to_input fills the keyboard member for key events.
		let key = unsafe { input.Anonymous.ki };
		assert_eq!(key.wScan, 0x1D);
		assert_eq!(
			key.dwFlags,
			KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
		);
	}
}
