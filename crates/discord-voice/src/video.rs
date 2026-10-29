//! Small Annex-B H.264 packetizer for a single Go Live video stream.
use std::{collections::VecDeque, time::Duration};
use tokio::time::Instant;

const MTU: usize = 1200;
const RTP_HEADER: usize = 12;
// XChaCha20-Poly1305 tag plus the rtpsize nonce trailer.
const TRANSPORT_OVERHEAD: usize = 20;
// Reserve the original sequence number added by RFC 4588 retransmission.
const MAX_PAYLOAD: usize = MTU - RTP_HEADER - TRANSPORT_OVERHEAD - 2;
pub(crate) const MAX_FRAGMENTS: usize = 2048;
const MAX_DAVE_FRAME: usize = 2 * 1024 * 1024 + 64 * 1024;

pub(crate) struct Packet {
	pub header: [u8; RTP_HEADER],
	pub payload: Vec<u8>,
}

/// libwebrtc's default: the pacer drains at 2.5x the encoder target, so an average frame
/// leaves in well under its frame interval while keyframes still avoid a line-rate burst.
const PACING_FACTOR: f64 = 2.5;
/// Longest sleep the pacer accounts for. A coarse OS timer (Windows' 15.6 ms default tick)
/// or a delayed task sends proportionally more on its next wake instead of losing frames.
const MAX_CATCH_UP: Duration = Duration::from_millis(100);
/// Budget a fresh access unit may send before the first pacing sleep.
const INITIAL_BURST: f64 = (4 * MTU) as f64;

/// One bounded access unit, paced by elapsed time against the encoder target.
pub(crate) struct Pacer {
	packets: std::vec::IntoIter<Packet>,
	credit: f64,
	repair_credit: f64,
	updated: Instant,
	progress: Instant,
	pub deadline: Instant,
}
impl Pacer {
	pub fn new() -> Self {
		Self {
			packets: Vec::new().into_iter(),
			credit: INITIAL_BURST,
			repair_credit: 0.0,
			updated: Instant::now(),
			progress: Instant::now(),
			deadline: Instant::now(),
		}
	}
	pub fn is_empty(&self) -> bool {
		self.packets.len() == 0
	}
	pub fn clear(&mut self) {
		self.packets = Vec::new().into_iter();
	}
	pub fn stale(&self, now: Instant) -> bool {
		!self.is_empty() && now.duration_since(self.progress) >= Duration::from_millis(500)
	}
	pub fn queue(&mut self, packets: Vec<Packet>, now: Instant) {
		// Idle time is not banked into a burst; only the small initial allowance carries over.
		self.credit = self.credit.min(INITIAL_BURST);
		self.updated = now;
		self.packets = packets.into_iter();
		self.deadline = now;
		self.progress = now;
	}
	fn refill(&mut self, now: Instant, bitrate: u32) {
		let elapsed = now
			.saturating_duration_since(self.updated)
			.min(MAX_CATCH_UP)
			.as_secs_f64();
		self.updated = now;
		let target = f64::from(bitrate) / 8.0;
		let added = elapsed * target * PACING_FACTOR;
		// Keep a 5 ms bucket on precise timers; a longer wake may spend what it accrued.
		let limit = (target * PACING_FACTOR * 0.005)
			.max(INITIAL_BURST)
			.max(added);
		self.credit = (self.credit + added).min(limit);
		self.repair_credit = (self.repair_credit + elapsed * target * 0.20).min((2 * MTU) as f64);
	}
	pub fn allow_repair(&mut self, now: Instant, bitrate: u32) -> bool {
		self.refill(now, bitrate);
		if self.credit < MTU as f64 || self.repair_credit < MTU as f64 {
			return false;
		}
		self.credit -= MTU as f64;
		self.repair_credit -= MTU as f64;
		true
	}
	pub fn next_batch(&mut self, now: Instant, bitrate: u32) -> impl Iterator<Item = Packet> + '_ {
		self.refill(now, bitrate);
		let mut count = 0;
		let mut shortfall = 0.0;
		for packet in self.packets.as_slice() {
			let bytes = (RTP_HEADER + packet.payload.len() + TRANSPORT_OVERHEAD) as f64;
			if bytes > self.credit {
				shortfall = bytes - self.credit;
				break;
			}
			self.credit -= bytes;
			count += 1;
		}
		// Sleep until the next media packet, or else a pending repair, is affordable.
		let target = f64::from(bitrate.max(1)) / 8.0;
		let wait = if count < self.packets.len() {
			shortfall / (target * PACING_FACTOR)
		} else {
			((MTU as f64 - self.repair_credit) / (target * 0.20))
				.max((MTU as f64 - self.credit) / (target * PACING_FACTOR))
				.max(0.0)
		};
		self.deadline = now + Duration::from_secs_f64(wait.min(0.1)).max(Duration::from_millis(1));
		// A large IDR at a reduced bitrate needs time to drain. Only a lack of
		// progress expires it, otherwise every replacement IDR could be cut off too.
		if count > 0 {
			self.progress = now;
		}
		self.packets.by_ref().take(count)
	}
}

/// First target before any feedback: sharp enough to read text, low enough that a weak
/// uplink does not begin with a second of queued video.
const START_BITRATE: u32 = 2_500_000;
const MIN_BITRATE: u32 = 250_000;
/// How often the target is reconsidered, and how long a cut gets to drain the queue
/// before the next one.
const DECIDE: Duration = Duration::from_millis(250);
const SETTLE: Duration = Duration::from_millis(500);
/// Round-trip time above the path's own minimum, spent in a queue (usually the sender's
/// uplink). Above `QUEUE_HOLD` the target stops growing; above `QUEUE_CUT` it is cut.
const QUEUE_HOLD: Duration = Duration::from_millis(30);
const QUEUE_CUT: Duration = Duration::from_millis(80);

/// Sender rate control. Loss that viewers report, imply through NACKs or that the local
/// socket refuses cuts the target within a quarter second, and a round trip growing
/// through the uplink queue cuts it before any loss shows, since a weak uplink buffers
/// seconds of video first. REMB caps it, and clean feedback grows it 8% a second, or 2%
/// near the rate of the last cut, where the uplink ran out before. A still picture that
/// leaves the target unused never grows it, so motion afterwards cannot burst past the
/// uplink.
pub(crate) struct Rate {
	pub target: u32,
	maximum: u32,
	knee: Option<u32>,
	at: Instant,
	grown: Instant,
	cut: Option<Instant>,
	heard: Option<Instant>,
	loss: Option<u8>,
	sent: u32,
	lost: u32,
	limit: Option<(u32, Instant)>,
	estimate: Option<u32>,
	trip: RoundTrip,
}
impl Rate {
	pub fn new(maximum: u32, now: Instant) -> Self {
		Self {
			target: START_BITRATE.min(maximum),
			maximum,
			knee: None,
			at: now,
			grown: now,
			cut: None,
			heard: None,
			loss: None,
			sent: 0,
			lost: 0,
			limit: None,
			estimate: None,
			trip: RoundTrip::default(),
		}
	}
	/// A receiver report's fraction lost (/256) and a REMB estimate.
	pub fn observe(&mut self, loss: Option<u8>, estimate: Option<u32>, now: Instant) {
		if let Some(loss) = loss {
			self.loss = Some(self.loss.map_or(loss, |old| old.max(loss)));
			self.heard = Some(now);
		}
		if let Some(estimate) = estimate {
			// REMB describes the wire rate; leave room for RTP/AEAD and bounded repairs.
			let estimate = (u64::from(estimate) * 4 / 5) as u32;
			self.estimate = Some(self.estimate.map_or(estimate, |old| old.min(estimate)));
			self.heard = Some(now);
		}
	}
	/// Media packets handed to the socket, and how many it refused.
	pub fn sent(&mut self, packets: u32, refused: u32) {
		self.sent = self.sent.saturating_add(packets);
		self.lost = self.lost.saturating_add(refused);
	}
	/// Packets a receiver asked for again for the first time.
	pub fn nacked(&mut self, packets: u32) {
		self.lost = self.lost.saturating_add(packets);
	}
	pub fn round_trip(&mut self, sample: Duration, now: Instant) {
		self.trip.add(sample, now);
		self.heard = Some(now);
	}
	/// The latest round trip and how much of it was queueing, for the stream log.
	pub fn delay(&self, now: Instant) -> Option<(Duration, Duration)> {
		Some((self.trip.latest(now)?, self.trip.queued(now)?))
	}
	pub fn tick(&mut self, now: Instant) -> Option<u32> {
		let window = now.duration_since(self.at);
		if window < DECIDE {
			return None;
		}
		self.at = now;
		// Packets leave nearly full; half the target is enough to learn from.
		let used = f64::from(self.sent) * (MTU * 8) as f64
			>= f64::from(self.target) * window.as_secs_f64() / 2.0;
		let mut loss = f64::from(self.loss.take().unwrap_or(0)) / 256.0;
		// A handful of packets says nothing about a loss rate.
		if self.sent >= 20 {
			loss = loss.max(f64::from(self.lost) / f64::from(self.sent));
		}
		(self.sent, self.lost) = (0, 0);
		if let Some(estimate) = self.estimate.take() {
			self.limit = Some((estimate, now));
		}
		let floor = MIN_BITRATE.min(self.maximum);
		let ceiling = self
			.limit
			.filter(|(_, at)| now.duration_since(*at) < Duration::from_secs(5))
			.map_or(self.maximum, |(limit, _)| limit.min(self.maximum))
			.max(floor);
		let queued = self.trip.queued(now).unwrap_or_default();
		let mut target = self.target.min(ceiling);
		let since_cut = self.cut.map_or(Duration::MAX, |at| now.duration_since(at));
		if (loss >= 0.10 || queued >= QUEUE_CUT) && since_cut >= SETTLE {
			// Heavy loss cuts in proportion; a growing queue cuts below the rate that
			// filled it, so it drains instead of holding its delay.
			let keep = (1.0 - loss / 2.0).clamp(0.5, 0.85);
			self.knee = Some(target);
			target = (f64::from(target) * keep) as u32;
			self.cut = Some(now);
			self.grown = now;
		} else if loss >= 0.02 || queued >= QUEUE_HOLD {
			self.grown = now;
		} else if used
			&& self
				.heard
				.is_some_and(|at| now.duration_since(at) < Duration::from_secs(2))
			&& now.duration_since(self.grown) >= Duration::from_secs(1)
			&& since_cut >= Duration::from_millis(1500)
		{
			let near = self.knee.is_some_and(|knee| {
				u64::from(target) * 10 >= u64::from(knee) * 9
					&& u64::from(target) * 10 <= u64::from(knee) * 11
			});
			target = target.saturating_add(if near {
				(target / 50).max(10_000)
			} else {
				(target / 12).max(50_000)
			});
			self.grown = now;
		}
		target = target.clamp(floor, ceiling);
		if target == self.target {
			return None;
		}
		self.target = target;
		Some(target)
	}
}

/// Round trips of the voice server's UDP pong, which crosses the same uplink queue as
/// the video.
#[derive(Default)]
struct RoundTrip {
	/// Minimum of the current and previous ten seconds, so a changed route is learned.
	floor: [Option<Duration>; 2],
	window: Option<Instant>,
	recent: [Option<(Duration, Instant)>; 2],
}
impl RoundTrip {
	fn add(&mut self, sample: Duration, now: Instant) {
		if self
			.window
			.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(10))
		{
			self.floor = [None, self.floor[0]];
			self.window = Some(now);
		}
		self.floor[0] = Some(self.floor[0].map_or(sample, |floor| floor.min(sample)));
		self.recent = [Some((sample, now)), self.recent[0]];
	}
	fn latest(&self, now: Instant) -> Option<Duration> {
		self.recent[0]
			.filter(|(_, at)| now.duration_since(*at) < Duration::from_secs(1))
			.map(|(sample, _)| sample)
	}
	/// Queueing in the last two samples; one late pong is jitter, not a queue.
	fn queued(&self, now: Instant) -> Option<Duration> {
		let mut recent = self.recent.iter().flatten();
		let fresh = |(sample, at): &(Duration, Instant)| {
			(now.duration_since(*at) < Duration::from_secs(1)).then_some(*sample)
		};
		let sample = fresh(recent.next()?)?.min(fresh(recent.next()?)?);
		let floor = self.floor.iter().flatten().min()?;
		Some(sample.saturating_sub(*floor))
	}
}

const HISTORY_PACKETS: usize = 2048;
const HISTORY_BYTES: usize = 2 * 1024 * 1024;
const HISTORY_AGE: Duration = Duration::from_secs(1);
struct Sent {
	packet: Packet,
	at: Instant,
	retries: u8,
	last_retry: Option<Instant>,
}
impl Sent {
	fn bytes(&self) -> usize {
		std::mem::size_of::<Self>() + self.packet.payload.capacity()
	}
}

/// DAVE ciphertext only, cleared across every encryption transition.
#[derive(Default)]
pub(crate) struct History {
	packets: VecDeque<Sent>,
	bytes: usize,
	pending: VecDeque<u16>,
}
impl History {
	pub fn clear(&mut self) {
		self.packets.clear();
		self.pending.clear();
		self.bytes = 0;
	}
	pub fn expire(&mut self, now: Instant) {
		while self
			.packets
			.front()
			.is_some_and(|sent| now.duration_since(sent.at) >= HISTORY_AGE)
		{
			self.bytes -= self.packets.pop_front().unwrap().bytes();
		}
		if self.packets.is_empty() {
			self.pending.clear();
		}
	}
	pub fn remember(&mut self, packet: Packet, now: Instant) {
		self.expire(now);
		let sent = Sent {
			packet,
			at: now,
			retries: 0,
			last_retry: None,
		};
		let bytes = sent.bytes();
		if bytes > HISTORY_BYTES {
			return;
		}
		while self.packets.len() >= HISTORY_PACKETS || self.bytes + bytes > HISTORY_BYTES {
			self.bytes -= self.packets.pop_front().unwrap().bytes();
		}
		self.bytes += bytes;
		self.packets.push_back(sent);
	}
	/// A cache miss needs an IDR instead of resending media outside the repair window.
	/// Also counts packets asked for the first time, which the rate control reads as loss.
	pub fn request(&mut self, sequences: &[u16], now: Instant) -> (bool, u32) {
		self.expire(now);
		let mut missing = false;
		let mut fresh = 0;
		for &seq in sequences.iter().take(128) {
			let Some(sent) = self
				.packets
				.iter()
				.find(|sent| sent.packet.header[2..4] == seq.to_be_bytes())
			else {
				missing = true;
				continue;
			};
			if sent.retries >= 2 {
				missing = true;
				continue;
			}
			if sent.retries == 0 && !self.pending.contains(&seq) {
				fresh += 1;
			}
			if self.pending.len() < 128
				&& !self.pending.contains(&seq)
				&& sent
					.last_retry
					.is_none_or(|at| now.duration_since(at) >= Duration::from_millis(50))
			{
				self.pending.push_back(seq);
			}
		}
		(missing, fresh)
	}
	pub fn has_pending(&self) -> bool {
		!self.pending.is_empty()
	}
	pub fn repair(&mut self, ssrc: u32, sequence: &mut u16, now: Instant) -> Option<Packet> {
		self.expire(now);
		if ssrc == 0 {
			return None;
		}
		// Drain stale requests in this bounded pass, rather than charging an MTU
		// of repair budget for each one and blocking newer useful retransmissions.
		while let Some(seq) = self.pending.pop_front() {
			let Some(sent) = self
				.packets
				.iter_mut()
				.find(|sent| sent.packet.header[2..4] == seq.to_be_bytes())
			else {
				continue;
			};
			if sent.retries >= 2 {
				continue;
			}
			sent.retries += 1;
			sent.last_retry = Some(now);
			let mut header = sent.packet.header;
			header[1] = (header[1] & 0x80) | 102;
			header[2..4].copy_from_slice(&sequence.to_be_bytes());
			header[8..12].copy_from_slice(&ssrc.to_be_bytes());
			*sequence = sequence.wrapping_add(1);
			let mut payload = Vec::with_capacity(sent.packet.payload.len() + 2);
			payload.extend_from_slice(&seq.to_be_bytes());
			payload.extend_from_slice(&sent.packet.payload);
			return Some(Packet { header, payload });
		}
		None
	}
}

/// Packetize an already DAVE-encrypted Annex-B H.264 frame as RFC 6184 NAL/FU-A RTP.
pub(crate) fn packetize(
	frame: &[u8],
	sequence: &mut u16,
	timestamp: u32,
	ssrc: u32,
) -> Result<Vec<Packet>, &'static str> {
	if frame.len() > MAX_DAVE_FRAME {
		return Err("DAVE H264 frame exceeds the sharing limit");
	}
	let nalus = nalus(frame)?;
	let mut packets = Vec::new();
	for (index, nalu) in nalus.iter().enumerate() {
		if nalu.is_empty() {
			continue;
		}
		let last_nalu = index + 1 == nalus.len();
		if nalu.len() <= MAX_PAYLOAD {
			push(
				&mut packets,
				sequence,
				timestamp,
				ssrc,
				nalu.to_vec(),
				last_nalu,
			)?;
			continue;
		}
		if nalu.len() < 2 {
			return Err("Invalid H264 NAL unit");
		}
		let indicator = (nalu[0] & 0xe0) | 28;
		let kind = nalu[0] & 0x1f;
		let mut offset = 1;
		while offset < nalu.len() {
			let take = (nalu.len() - offset).min(MAX_PAYLOAD - 2);
			let mut payload = Vec::with_capacity(take + 2);
			payload.push(indicator);
			payload.push(
				kind | if offset == 1 { 0x80 } else { 0 }
					| if offset + take == nalu.len() { 0x40 } else { 0 },
			);
			payload.extend_from_slice(&nalu[offset..offset + take]);
			let last = last_nalu && offset + take == nalu.len();
			push(&mut packets, sequence, timestamp, ssrc, payload, last)?;
			offset += take;
		}
	}
	if packets.is_empty() {
		return Err("H264 frame has no NAL units");
	}
	Ok(packets)
}

pub(crate) fn validate_source(frame: &[u8]) -> Result<(), &'static str> {
	if frame.len() > 2 * 1024 * 1024 {
		return Err("H264 frame exceeds the sharing limit");
	}
	nalus(frame).map(|_| ())
}

fn push(
	packets: &mut Vec<Packet>,
	sequence: &mut u16,
	timestamp: u32,
	ssrc: u32,
	payload: Vec<u8>,
	marker: bool,
) -> Result<(), &'static str> {
	if packets.len() == MAX_FRAGMENTS {
		return Err("H264 frame exceeds fragment limit");
	}
	let mut header = [0; RTP_HEADER];
	header[0] = 0x80;
	header[1] = 101 | u8::from(marker) << 7;
	header[2..4].copy_from_slice(&sequence.to_be_bytes());
	header[4..8].copy_from_slice(&timestamp.to_be_bytes());
	header[8..12].copy_from_slice(&ssrc.to_be_bytes());
	*sequence = sequence.wrapping_add(1);
	packets.push(Packet { header, payload });
	Ok(())
}

fn nalus(frame: &[u8]) -> Result<Vec<&[u8]>, &'static str> {
	let mut starts = Vec::new();
	let mut at = 0;
	while at + 3 <= frame.len() {
		let size = if frame[at..].starts_with(&[0, 0, 0, 1]) {
			4
		} else if frame[at..].starts_with(&[0, 0, 1]) {
			3
		} else {
			at += 1;
			continue;
		};
		if starts.len() == MAX_FRAGMENTS {
			return Err("H264 frame exceeds fragment limit");
		}
		starts.push((at, size));
		at += size;
	}
	if starts.is_empty() || starts[0].0 != 0 {
		return Err("H264 frame is not Annex-B");
	}
	let mut out = Vec::with_capacity(starts.len());
	for (index, (start, size)) in starts.iter().enumerate() {
		let end = starts.get(index + 1).map_or(frame.len(), |next| next.0);
		if start + size >= end {
			return Err("H264 frame contains an empty NAL unit");
		}
		out.push(&frame[start + size..end]);
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{crypto::Dave, test_mls::Delivery};

	fn depacketize(packets: &[Packet]) -> Vec<u8> {
		let mut frame = Vec::new();
		let mut fragmented = false;
		for packet in packets {
			if packet.payload[0] & 0x1f == 28 {
				if packet.payload[1] & 0x80 != 0 {
					assert!(!fragmented);
					frame.extend([0, 0, 0, 1]);
					frame.push((packet.payload[0] & 0xe0) | (packet.payload[1] & 0x1f));
					fragmented = true;
				} else {
					assert!(fragmented);
				}
				frame.extend_from_slice(&packet.payload[2..]);
				if packet.payload[1] & 0x40 != 0 {
					fragmented = false;
				}
			} else {
				assert!(!fragmented);
				frame.extend([0, 0, 0, 1]);
				frame.extend_from_slice(&packet.payload);
			}
		}
		assert!(!fragmented);
		frame
	}
	#[test]
	fn fragments_annex_b_h264_with_a_final_marker() {
		assert!(validate_source(&[0, 0, 0, 1, 0x65, 1, 0, 0, 1]).is_err());
		let mut frame = vec![0, 0, 0, 1, 0x65];
		frame.resize(frame.len() + MAX_PAYLOAD * 2, 7);
		frame.extend([0, 0, 1, 0x41, 9]);
		let mut sequence = 7;
		let packets = packetize(&frame, &mut sequence, 90_000, 11).unwrap();
		assert!(packets.len() > 2 && packets.len() <= MAX_FRAGMENTS);
		assert!(
			packets[..packets.len() - 1]
				.iter()
				.all(|packet| packet.header[1] & 0x80 == 0)
		);
		assert_eq!(packets.last().unwrap().header[1], 101 | 0x80);
		assert!(
			packets
				.iter()
				.all(
					|packet| packet.header.len() + packet.payload.len() + TRANSPORT_OVERHEAD <= MTU
				)
		);
		assert_eq!(packets[0].payload[0] & 0x1f, 28);
		assert_ne!(packets[0].payload[1] & 0x80, 0);
		let mut rebuilt = Vec::new();
		for packet in packets
			.iter()
			.take_while(|packet| packet.payload[0] & 0x1f == 28)
		{
			if packet.payload[1] & 0x80 != 0 {
				rebuilt.push((packet.payload[0] & 0xe0) | (packet.payload[1] & 0x1f));
			}
			rebuilt.extend_from_slice(&packet.payload[2..]);
		}
		assert_eq!(rebuilt, frame[4..5 + MAX_PAYLOAD * 2]);
	}
	#[test]
	fn dave_h264_ciphertext_packetizes_without_extra_start_codes() {
		let server = Delivery::new();
		let mut alice = Dave::new(1, Some(2), 3).unwrap();
		let mut bob = Dave::new(2, Some(1), 3).unwrap();
		alice.session.set_external_sender(&server.external).unwrap();
		bob.session.set_external_sender(&server.external).unwrap();
		let (commit, welcome) = server.add(&mut alice, &bob.key_package().unwrap());
		let mut committed = vec![0, 5];
		committed.extend(commit);
		let mut welcomed = vec![0, 5];
		welcomed.extend(welcome);
		assert_eq!(alice.group_changed(29, &committed).unwrap(), 5);
		assert_eq!(bob.group_changed(30, &welcomed).unwrap(), 5);
		alice.execute(5).unwrap();
		bob.execute(5).unwrap();

		let mut original = vec![0, 0, 0, 1, 0x67, 0x64, 0, 0x1f, 0xac, 0xd9, 0x40];
		original.extend([0, 0, 0, 1, 0x65, 0x88]);
		original.resize(original.len() + MAX_PAYLOAD * 2, 7);
		let encrypted = alice
			.session
			.encrypt(davey::MediaType::VIDEO, davey::Codec::H264, &original)
			.unwrap()
			.into_owned();
		assert_eq!(nalus(&encrypted).unwrap().len(), 2);
		let mut sequence = 0;
		let packets = packetize(&encrypted, &mut sequence, 90_000, 11).unwrap();
		let restored = depacketize(&packets);
		assert_eq!(restored, encrypted);
		assert_eq!(
			bob.session
				.decrypt(1, davey::MediaType::VIDEO, &restored)
				.unwrap(),
			original
		);
	}
	/// A sender behind an uplink of `capacity` bit/s with a drop-tail buffer of `buffer`,
	/// a 40 ms path and Discord's fixed 4.2 Mbps REMB. `pongs` answers the UDP pings.
	/// Returns the queue delay (ms) and target after each of `seconds` seconds.
	fn uplink(capacity: f64, buffer: Duration, pongs: bool, seconds: u32) -> Vec<(f64, u32)> {
		let start = Instant::now();
		let mut rate = Rate::new(12_000_000, start);
		let step = Duration::from_millis(10);
		let (mut queue, mut sent, mut lost) = (0.0_f64, 0.0, 0.0);
		let mut trace = Vec::new();
		for index in 1..=seconds * 100 {
			let now = start + step * index;
			queue += f64::from(rate.target) * step.as_secs_f64();
			queue = (queue - capacity * step.as_secs_f64()).max(0.0);
			let packets = f64::from(rate.target) * step.as_secs_f64() / 9600.0;
			let overflow = (queue - capacity * buffer.as_secs_f64()).max(0.0);
			queue -= overflow;
			rate.sent(packets.round() as u32, 0);
			rate.nacked((overflow / 9600.0).round() as u32);
			sent += packets;
			lost += overflow / 9600.0;
			if pongs && index % 25 == 0 {
				rate.round_trip(Duration::from_secs_f64(0.04 + queue / capacity), now);
			}
			if index % 100 == 0 {
				let fraction = (lost / sent.max(1.0) * 256.0).min(255.0) as u8;
				rate.observe(Some(fraction), Some(4_200_000), now);
				(sent, lost) = (0.0, 0.0);
				trace.push((queue / capacity * 1000.0, rate.target));
			}
			rate.tick(now);
		}
		trace
	}

	#[test]
	fn a_weak_uplink_settles_below_its_capacity_without_a_queue() {
		let trace = uplink(2_000_000.0, Duration::from_millis(500), true, 60);
		let settled = &trace[5..];
		let worst = settled.iter().map(|(queue, _)| *queue).fold(0.0, f64::max);
		let mean = settled
			.iter()
			.map(|(_, target)| f64::from(*target))
			.sum::<f64>()
			/ settled.len() as f64;
		let average = settled.iter().map(|(queue, _)| *queue).sum::<f64>() / settled.len() as f64;
		eprintln!(
			"2 Mbps uplink: queue {average:.0} ms average, {worst:.0} ms worst, target {mean:.0}"
		);
		assert!(worst < 120.0, "queue reached {worst:.0} ms: {trace:?}");
		assert!(
			(1_200_000.0..2_000_000.0).contains(&mean),
			"mean target {mean:.0}: {trace:?}"
		);
	}

	#[test]
	fn loss_alone_still_finds_a_weak_uplink() {
		// Without pongs only overflow loss signals, so the queue fills before each cut,
		// but the target still settles under the capacity instead of holding the REMB.
		let trace = uplink(2_000_000.0, Duration::from_millis(100), false, 60);
		let settled = &trace[10..];
		let mean = settled
			.iter()
			.map(|(_, target)| f64::from(*target))
			.sum::<f64>()
			/ settled.len() as f64;
		assert!(mean < 2_200_000.0, "mean target {mean:.0}: {trace:?}");
	}

	#[test]
	fn clean_feedback_grows_to_the_remb_ceiling_and_stops() {
		let trace = uplink(50_000_000.0, Duration::from_millis(500), true, 20);
		assert!(trace.iter().all(|(_, target)| *target <= 3_360_000));
		assert_eq!(trace.last().unwrap().1, 3_360_000, "{trace:?}");
	}

	#[test]
	fn nacks_cut_the_target_within_a_quarter_second_and_silence_never_grows_it() {
		let start = Instant::now();
		let mut rate = Rate::new(12_000_000, start);
		assert_eq!(rate.target, START_BITRATE);
		for second in 1..10 {
			assert_eq!(rate.tick(start + Duration::from_secs(second)), None);
		}
		let now = start + Duration::from_secs(10);
		rate.sent(100, 0);
		rate.nacked(20);
		assert_eq!(rate.tick(now), Some(2_125_000));
		// A further cut waits for the first to drain the queue.
		rate.sent(100, 0);
		rate.nacked(20);
		assert_eq!(rate.tick(now + DECIDE), None);
		rate.sent(100, 0);
		rate.nacked(20);
		assert!(
			rate.tick(now + SETTLE)
				.is_some_and(|target| target < 2_125_000)
		);
	}

	#[test]
	fn a_still_picture_never_grows_the_target() {
		let start = Instant::now();
		let mut rate = Rate::new(12_000_000, start);
		let mut run = |from: u32, packets: u32| {
			for index in from..from + 40 {
				let now = start + DECIDE * index;
				rate.round_trip(Duration::from_millis(40), now);
				rate.sent(packets, 0);
				rate.tick(now);
			}
			rate.target
		};
		assert_eq!(run(1, 5), START_BITRATE);
		assert!(run(41, 100) > START_BITRATE);
	}

	#[test]
	fn one_late_pong_is_jitter_but_two_are_a_queue() {
		let start = Instant::now();
		let mut trip = RoundTrip::default();
		for index in 0..4 {
			trip.add(Duration::from_millis(40), start + DECIDE * index);
		}
		let now = start + DECIDE * 4;
		trip.add(Duration::from_millis(200), now);
		assert_eq!(trip.queued(now), Some(Duration::ZERO));
		trip.add(Duration::from_millis(150), now + DECIDE);
		assert_eq!(trip.queued(now + DECIDE), Some(Duration::from_millis(110)));
		assert_eq!(trip.queued(now + Duration::from_secs(2)), None);
	}
}
