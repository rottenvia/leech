//! The client identity Leech presents to Discord. A webview sign-in hands over the sign-in
//! page's own `X-Super-Properties` with the token; Leech then presents exactly that client
//! (browser, version, build) instead of a fixed one. Discord ended sessions within seconds when
//! the token came back under a client years older than the one that created it. The value is
//! kept beside the saved logins, so a restart presents the same client. It names no account.
use std::path::Path;

const FILE: &str = "identity";

/// Presents `value` (base64 `X-Super-Properties`) from the next connection on, if it is the
/// JSON object Discord sends, with a user agent and a build number.
pub fn adopt(value: &str) -> bool {
	let Some(properties) = client_core::fingerprint::decode_base64(value.trim()) else {
		return false;
	};
	let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(&properties) else {
		return false;
	};
	let user_agent = fields
		.get("browser_user_agent")
		.and_then(serde_json::Value::as_str)
		.filter(|agent| {
			agent.starts_with("Mozilla/5.0 (")
				&& agent.len() <= 512
				&& agent.bytes().all(|byte| (b' '..=b'~').contains(&byte))
		});
	let build = fields
		.get("client_build_number")
		.and_then(serde_json::Value::as_u64);
	let (Some(user_agent), Some(build)) = (user_agent, build) else {
		return false;
	};
	let text = |key| {
		fields
			.get(key)
			.and_then(serde_json::Value::as_str)
			.unwrap_or("?")
			.chars()
			.take(32)
			.collect::<String>()
	};
	client_core::journal::note(
		"session",
		format!(
			"presenting {} {} on {}, build {build}",
			text("browser"),
			text("browser_version"),
			text("os")
		),
	);
	client_core::fingerprint::adopt(value.trim().to_owned(), properties, user_agent.to_owned());
	true
}

/// The identity kept from the last webview sign-in, if any.
pub fn load(directory: &Path) {
	if let Ok(value) = std::fs::read_to_string(directory.join(FILE)) {
		adopt(&value);
	}
}

/// Adopts and keeps a sign-in page's identity; a malformed one changes nothing.
pub fn save(directory: &Path, value: &str) {
	if adopt(value) {
		let _ = std::fs::create_dir_all(directory);
		let _ = std::fs::write(directory.join(FILE), value.trim());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_sign_in_identity_is_adopted_kept_and_validated() {
		let base64 = |text: &str| {
			// Standard base64 for the fixture, the same encoding the page sends.
			const TABLE: &[u8; 64] =
				b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
			let mut out = String::new();
			for chunk in text.as_bytes().chunks(3) {
				let word = u32::from_be_bytes([
					0,
					chunk[0],
					*chunk.get(1).unwrap_or(&0),
					*chunk.get(2).unwrap_or(&0),
				]);
				for i in 0..4 {
					out.push(if i <= chunk.len() {
						TABLE[((word >> (18 - 6 * i)) & 63) as usize] as char
					} else {
						'='
					});
				}
			}
			out
		};
		let agent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36 Edg/156.0.0.0";
		let page = format!(
			r#"{{"os":"Windows","browser":"Edge","browser_user_agent":"{agent}","browser_version":"156.0.0.0","client_build_number":663810,"client_launch_id":"synthetic"}}"#
		);
		for bad in [
			"not base64!".to_owned(),
			base64("[1,2]"),
			base64(r#"{"browser_user_agent":"curl/8","client_build_number":1}"#),
			base64(&page.replace("663810", "\"663810\"")),
		] {
			assert!(!adopt(&bad), "{bad}");
		}
		let directory = std::env::temp_dir().join(format!("leech-identity-{}", std::process::id()));
		save(&directory, &base64(&page));
		assert_eq!(client_core::fingerprint::user_agent(), agent);
		assert_eq!(client_core::fingerprint::properties(), page);
		assert_eq!(client_core::fingerprint::super_properties(), base64(&page));
		// A restart presents the same client.
		assert_eq!(
			std::fs::read_to_string(directory.join(FILE)).unwrap(),
			base64(&page)
		);
		let _ = std::fs::remove_dir_all(&directory);
	}
}
