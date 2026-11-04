//! Connection journal: what the connection did, kept to trace sign-outs and dropped calls.
//! Every REST request (method, route with IDs removed, status, time, Discord's error code),
//! gateway and voice socket opens, closes and close codes, and why a session ended.
//!
//! Entries are fixed labels, numbers and Discord's own close reasons: never a token, message,
//! name or ID. Each is at most `DETAIL` bytes and at most `PENDING` wait to be written, so a
//! storm of failures cannot grow memory. `apps/desktop/src/perf_stats.rs` writes and sends them
//! with the performance statistics, and only while those are on.
use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix milliseconds, kind (`rest`, `gateway`, `voice`, `session`) and detail.
pub type Entry = (u64, &'static str, String);

const PENDING: usize = 4000;
/// Kept apart from the pending entries, so a copied failure report still has them after a write.
const RECENT: usize = 60;
const DETAIL: usize = 300;

struct Journal {
	pending: VecDeque<Entry>,
	recent: VecDeque<Entry>,
	dropped: u64,
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
	pending: VecDeque::new(),
	recent: VecDeque::new(),
	dropped: 0,
});

pub fn note(kind: &'static str, detail: impl Into<String>) {
	let mut detail = detail.into();
	if detail.len() > DETAIL {
		let mut end = DETAIL;
		while !detail.is_char_boundary(end) {
			end -= 1;
		}
		detail.truncate(end);
	}
	let at = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |since| since.as_millis() as u64);
	let mut journal = JOURNAL.lock().unwrap_or_else(PoisonError::into_inner);
	if journal.recent.len() == RECENT {
		journal.recent.pop_front();
	}
	journal.recent.push_back((at, kind, detail.clone()));
	if journal.pending.len() == PENDING {
		journal.pending.pop_front();
		journal.dropped += 1;
	}
	journal.pending.push_back((at, kind, detail));
}

/// Everything not yet written, oldest first; says how many were dropped for room.
pub fn take() -> Vec<Entry> {
	let mut journal = JOURNAL.lock().unwrap_or_else(PoisonError::into_inner);
	let dropped = std::mem::take(&mut journal.dropped);
	let mut entries: Vec<Entry> = journal.pending.drain(..).collect();
	if dropped > 0 {
		let at = entries.first().map_or(0, |entry| entry.0);
		entries.insert(
			0,
			(
				at,
				"journal",
				format!("{dropped} older entries dropped for room"),
			),
		);
	}
	entries
}

/// The latest entries, written or not, for a copied failure report.
pub fn recent() -> Vec<Entry> {
	let journal = JOURNAL.lock().unwrap_or_else(PoisonError::into_inner);
	journal.recent.iter().cloned().collect()
}

/// A REST route with every ID, key, code and query removed: `/channels/:x/messages`.
pub fn route(path: &str) -> String {
	let path = path.split('?').next().unwrap_or_default();
	let mut route = String::with_capacity(path.len());
	for segment in path.split('/').skip(1) {
		route.push('/');
		let word = segment.len() <= 24
			&& segment
				.bytes()
				.all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'-' | b'_' | b'@'));
		route.push_str(if word { segment } else { ":x" });
	}
	route
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn entries_are_bounded_and_routes_carry_no_ids() {
		assert_eq!(
			route("/channels/123456789012345678/messages?limit=50&before=1"),
			"/channels/:x/messages"
		);
		assert_eq!(route("/streams/guild:1:2:3/preview"), "/streams/:x/preview");
		assert_eq!(
			route("/users/@me/connections/spotify/AbC123/access-token"),
			"/users/@me/connections/spotify/:x/access-token"
		);
		assert_eq!(route("/invites/SomeCode"), "/invites/:x");

		for index in 0..PENDING + 5 {
			note("test", format!("{index}"));
		}
		// Cut on a character boundary: byte 300 falls inside an "é".
		note("test", format!("a{}", "é".repeat(DETAIL)));
		let recent = recent();
		assert_eq!(recent.len(), RECENT);
		assert_eq!(recent[RECENT - 2].2, format!("{}", PENDING + 4));
		assert_eq!(recent[RECENT - 1].2.len(), DETAIL - 1);
		let entries = take();
		// Other tests may note concurrently; the bound and the drop notice must hold anyway.
		assert!(entries.len() <= PENDING + 1);
		assert!(
			entries[0].2.ends_with("older entries dropped for room"),
			"{:?}",
			entries[0]
		);
		assert!(entries.iter().all(|entry| entry.2.len() <= DETAIL));
		assert!(take().len() < 10);
		assert_eq!(
			super::recent().len(),
			RECENT,
			"a write keeps the recent entries"
		);
	}
}
