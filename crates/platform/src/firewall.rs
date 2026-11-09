//! A Windows Firewall rule that lets UDP reach Leech from addresses it has not sent to first.
//! Remote control needs it when the other PC's NAT picks a new port per destination: that PC's
//! datagrams then come from a port Leech never contacted, and Windows drops them even after the
//! router forwards them. Windows never asks about UDP on its own and only an administrator may
//! add rules, so Leech asks Windows to add one (the UAC prompt) when none lets it in.

/// What became of the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inbound {
	/// A rule already lets UDP in on every network, or this system has no such firewall.
	Allowed,
	Added,
	/// The UAC prompt was refused.
	Declined,
	Failed,
}

/// Makes sure UDP may reach this executable, adding a rule through the UAC prompt when none
/// does. Blocks until the prompt is answered: call it off the UI thread.
pub fn allow_inbound_udp() -> Inbound {
	#[cfg(target_os = "windows")]
	return native::allow_inbound_udp();
	#[cfg(not(target_os = "windows"))]
	Inbound::Allowed
}

/// Whether a rule as Windows stores it in the registry (`v2.10|Action=Allow|Dir=In|…|`) lets
/// UDP in to `program` from anywhere, on every network profile.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn allows(rule: &str, program: &str) -> bool {
	let mut profiles = 0;
	let mut fields = 0;
	for part in rule.trim_end_matches('|').split('|').skip(1) {
		let Some((key, value)) = part.split_once('=') else {
			return false;
		};
		let wanted = match key {
			"Action" => value == "Allow",
			"Active" => value == "TRUE",
			"Dir" => value == "In",
			"Protocol" => value == "17",
			"App" => value.eq_ignore_ascii_case(program),
			"Profile" => {
				profiles += 1;
				continue;
			}
			// Names and display settings do not narrow what the rule lets in.
			"Name" | "Desc" | "EmbedCtxt" | "Edge" | "Defer" => continue,
			// Anything else (ports, addresses, interfaces, services) narrows it.
			_ => return false,
		};
		if !wanted {
			return false;
		}
		fields += 1;
	}
	// Action, Active, Dir and App are required; a missing protocol means every protocol.
	let every_profile = profiles == 0 || profiles == 3;
	let protocol = usize::from(rule.contains("|Protocol="));
	fields == 4 + protocol && every_profile && rule.starts_with('v')
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod native {
	use super::{Inbound, allows};
	use windows::{
		Win32::{
			Foundation::{CloseHandle, ERROR_CANCELLED, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS},
			System::{
				Registry::{
					HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, REG_SZ, REG_VALUE_TYPE, RegCloseKey,
					RegEnumValueW, RegOpenKeyExW,
				},
				Threading::{GetExitCodeProcess, WaitForSingleObject},
			},
			UI::{
				Shell::{
					SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
					ShellExecuteExW,
				},
				WindowsAndMessaging::{GetForegroundWindow, SW_HIDE},
			},
		},
		core::{PCWSTR, PWSTR, w},
	};

	const RULES: PCWSTR = w!(
		"SYSTEM\\CurrentControlSet\\Services\\SharedAccess\\Parameters\\FirewallPolicy\\FirewallRules"
	);
	const NAME: &str = "Leech remote control";
	/// A system has a few hundred rules; this bounds a damaged or hostile list.
	const MAX_RULES: u32 = 20_000;
	/// How long `netsh` may take once the prompt is accepted.
	const NETSH_WAIT_MS: u32 = 30_000;

	pub fn allow_inbound_udp() -> Inbound {
		let Ok(executable) = std::env::current_exe() else {
			return Inbound::Failed;
		};
		let Some(program) = executable
			.to_str()
			.filter(|path| !path.contains('"') && !path.chars().any(char::is_control))
		else {
			return Inbound::Failed;
		};
		if allowed(program) {
			return Inbound::Allowed;
		}
		add(program)
	}

	/// Reads the firewall's rule list (readable without elevation) for one that lets UDP in.
	fn allowed(program: &str) -> bool {
		let mut key = HKEY::default();
		// SAFETY: the subkey name is terminated and static; `key` receives the handle.
		if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, RULES, None, KEY_QUERY_VALUE, &mut key) }
			!= ERROR_SUCCESS
		{
			return false;
		}
		let mut name = vec![0_u16; 16_384];
		let mut data = vec![0_u16; 4_096];
		let mut found = false;
		for index in 0..MAX_RULES {
			let mut name_len = name.len() as u32;
			let mut bytes = std::mem::size_of_val(data.as_slice()) as u32;
			let mut kind = REG_VALUE_TYPE::default();
			// SAFETY: both buffers are writable for the lengths passed, which the call updates;
			// a value longer than the buffer fails with ERROR_MORE_DATA and writes nothing.
			let status = unsafe {
				RegEnumValueW(
					key,
					index,
					Some(PWSTR(name.as_mut_ptr())),
					&mut name_len,
					None,
					Some(&mut kind.0),
					Some(data.as_mut_ptr().cast()),
					Some(&mut bytes),
				)
			};
			if status == ERROR_NO_MORE_ITEMS {
				break;
			}
			if status != ERROR_SUCCESS || kind != REG_SZ {
				continue;
			}
			let units = (bytes as usize / 2).min(data.len());
			let rule = String::from_utf16_lossy(&data[..units]);
			if allows(rule.trim_end_matches('\0'), program) {
				found = true;
				break;
			}
		}
		// SAFETY: `key` was opened above and is closed once.
		unsafe {
			let _ = RegCloseKey(key);
		}
		found
	}

	/// Runs `netsh` elevated; Windows shows its UAC prompt over the window in front.
	fn add(program: &str) -> Inbound {
		let parameters = format!(
			"advfirewall firewall add rule name=\"{NAME}\" dir=in action=allow protocol=UDP \
			 profile=any enable=yes program=\"{program}\""
		);
		let parameters: Vec<u16> = parameters.encode_utf16().chain(Some(0)).collect();
		let mut info = SHELLEXECUTEINFOW {
			cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
			fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
			// SAFETY: reads which window is in front; no other effect.
			hwnd: unsafe { GetForegroundWindow() },
			lpVerb: w!("runas"),
			lpFile: w!("netsh.exe"),
			lpParameters: PCWSTR(parameters.as_ptr()),
			nShow: SW_HIDE.0,
			..Default::default()
		};
		// SAFETY: every string is terminated and outlives the call; `info` is sized for it.
		if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
			return if error.code() == ERROR_CANCELLED.to_hresult() {
				Inbound::Declined
			} else {
				Inbound::Failed
			};
		}
		if info.hProcess.is_invalid() {
			return Inbound::Failed;
		}
		let mut code = 1;
		// SAFETY: the process handle came from ShellExecuteExW and is closed once.
		unsafe {
			WaitForSingleObject(info.hProcess, NETSH_WAIT_MS);
			let _ = GetExitCodeProcess(info.hProcess, &mut code);
			let _ = CloseHandle(info.hProcess);
		}
		if code == 0 {
			Inbound::Added
		} else {
			Inbound::Failed
		}
	}

	#[cfg(test)]
	mod tests {
		/// The rule list reads without elevation and has no rule for a test binary.
		#[test]
		fn the_rule_list_reads_without_elevation() {
			let executable = std::env::current_exe().unwrap();
			assert!(!super::allowed(executable.to_str().unwrap()));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::allows;

	const APP: &str = "C:\\Users\\Me\\AppData\\Local\\Programs\\Leech\\leech.exe";

	#[test]
	fn only_an_open_inbound_udp_rule_for_this_program_counts() {
		let ours = format!(
			"v2.33|Action=Allow|Active=TRUE|Dir=In|Protocol=17|App={}|Name=Leech remote control|",
			APP.to_lowercase()
		);
		assert!(allows(&ours, APP));
		// What Windows' own prompt writes: per profile, so one profile alone is not enough.
		let public_only = ours.replace("|App=", "|Profile=Public|App=");
		assert!(!allows(&public_only, APP));
		let every_profile = ours.replace(
			"|App=",
			"|Profile=Domain|Profile=Private|Profile=Public|App=",
		);
		assert!(allows(&every_profile, APP));
		// Any protocol counts; TCP alone, blocking, outbound, disabled or narrowed rules do not.
		assert!(allows(&ours.replace("|Protocol=17", ""), APP));
		for narrower in [
			ours.replace("Protocol=17", "Protocol=6"),
			ours.replace("Action=Allow", "Action=Block"),
			ours.replace("Dir=In", "Dir=Out"),
			ours.replace("Active=TRUE", "Active=FALSE"),
			ours.replace("|Name=", "|LPort=5000|Name="),
			ours.replace("|Name=", "|RA4=192.168.0.0/16|Name="),
			ours.replace("leech.exe", "other.exe"),
			ours.replace("|App=", "|Nope|App="),
			String::new(),
		] {
			assert!(!allows(&narrower, APP), "{narrower}");
		}
	}
}
