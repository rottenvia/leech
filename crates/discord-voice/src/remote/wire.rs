//! Datagram layout, encryption and message encodings of remote control. Every layout is fixed
//! binary and little-endian; parsers return `None` on anything malformed and never allocate more
//! than one datagram's worth of data.
use super::{Input, MouseButton, Permission};
use chacha20poly1305::{AeadInPlace, ChaCha20Poly1305, KeyInit, Nonce, Tag};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Largest datagram either side sends or accepts: below every common path MTU.
pub(super) const MAX_DATAGRAM: usize = 1200;
/// Plaintext header, authenticated as AAD: magic, version, type, session, counter.
const HEADER: usize = 20;
const TAG_LEN: usize = 16;
pub(super) const MAX_PAYLOAD: usize = MAX_DATAGRAM - HEADER - TAG_LEN;
const MAGIC: [u8; 2] = *b"LR";
const VERSION: u8 = 1;

// Packet types. Hello, acknowledgement, ping, pong and goodbye belong to the link; the rest
// to the session.
pub(super) const HELLO: u8 = 1;
pub(super) const HELLO_ACK: u8 = 2;
pub(super) const PING: u8 = 3;
pub(super) const PONG: u8 = 4;
pub(super) const BYE: u8 = 5;
/// Host to viewer: permission, pause, audio, screen geometry, mouse mode.
pub(super) const STATE: u8 = 6;
pub(super) const VIDEO: u8 = 7;
pub(super) const AUDIO: u8 = 8;
pub(super) const KEYFRAME: u8 = 9;
/// Viewer to host: datagrams lost of the last ones sent, in permille.
pub(super) const LOSS: u8 = 10;
pub(super) const INPUT: u8 = 11;
pub(super) const HELD: u8 = 12;

/// Goodbye reasons.
pub(super) const BYE_ENDED: u8 = 0;
pub(super) const BYE_DECLINED: u8 = 1;
pub(super) const BYE_FAILED: u8 = 2;

/// One key per direction, both derived from the X25519 secret, the session and both public
/// keys. Direction 0 is host to viewer, 1 viewer to host.
pub(super) struct Cipher {
	session: u64,
	direction: u8,
	seal: ChaCha20Poly1305,
	open: ChaCha20Poly1305,
}

impl Cipher {
	pub(super) fn new(
		secret: &[u8; 32],
		session: u64,
		host_key: &[u8; 32],
		viewer_key: &[u8; 32],
		host: bool,
	) -> Self {
		let key = |direction: u8| {
			let digest = Sha256::new()
				.chain_update(b"leech remote control v1")
				.chain_update([direction])
				.chain_update(session.to_le_bytes())
				.chain_update(host_key)
				.chain_update(viewer_key)
				.chain_update(secret)
				.finalize();
			ChaCha20Poly1305::new(&digest)
		};
		let direction = u8::from(!host);
		Self {
			session,
			direction,
			seal: key(direction),
			open: key(1 - direction),
		}
	}

	/// Writes one datagram into `out`; `counter` must never repeat for this cipher.
	pub(super) fn seal(
		&self,
		kind: u8,
		counter: u64,
		payload: &[u8],
		out: &mut [u8; MAX_DATAGRAM],
	) -> Option<usize> {
		if payload.len() > MAX_PAYLOAD {
			return None;
		}
		out[..2].copy_from_slice(&MAGIC);
		out[2] = VERSION;
		out[3] = kind;
		out[4..12].copy_from_slice(&self.session.to_le_bytes());
		out[12..HEADER].copy_from_slice(&counter.to_le_bytes());
		let end = HEADER + payload.len();
		out[HEADER..end].copy_from_slice(payload);
		let (header, body) = out.split_at_mut(HEADER);
		let tag = self
			.seal
			.encrypt_in_place_detached(
				&nonce(self.direction, counter),
				header,
				&mut body[..payload.len()],
			)
			.ok()?;
		out[end..end + TAG_LEN].copy_from_slice(&tag);
		Some(end + TAG_LEN)
	}

	/// Authenticates and decrypts one datagram in place: (type, counter, payload). Replays
	/// and anything older than the 64-packet window are rejected.
	pub(super) fn open<'a>(
		&self,
		datagram: &'a mut [u8],
		window: &mut Window,
	) -> Option<(u8, u64, &'a [u8])> {
		if !(HEADER + TAG_LEN..=MAX_DATAGRAM).contains(&datagram.len())
			|| datagram[..2] != MAGIC
			|| datagram[2] != VERSION
		{
			return None;
		}
		let kind = datagram[3];
		let mut header = Reader(&datagram[4..HEADER]);
		let (session, counter) = (header.u64()?, header.u64()?);
		if session != self.session || !window.fresh(counter) {
			return None;
		}
		let (header, rest) = datagram.split_at_mut(HEADER);
		let (body, tag) = rest.split_at_mut(rest.len() - TAG_LEN);
		self.open
			.decrypt_in_place_detached(
				&nonce(1 - self.direction, counter),
				header,
				body,
				Tag::from_slice(tag),
			)
			.ok()?;
		window.mark(counter);
		Some((kind, counter, body))
	}
}

fn nonce(direction: u8, counter: u64) -> Nonce {
	let mut nonce = [0; 12];
	nonce[0] = direction;
	nonce[4..].copy_from_slice(&counter.to_le_bytes());
	*Nonce::from_slice(&nonce)
}

/// Replay window over the 64 counters up to the newest accepted one. Counters start at 1.
#[derive(Default)]
pub(super) struct Window {
	top: u64,
	/// Bit `n` set: counter `top - n` was accepted.
	seen: u64,
}

impl Window {
	fn fresh(&self, counter: u64) -> bool {
		counter > self.top
			|| (counter > 0
				&& self.top - counter < 64
				&& self.seen & (1 << (self.top - counter)) == 0)
	}

	fn mark(&mut self, counter: u64) {
		if counter > self.top {
			let shift = counter - self.top;
			self.seen = if shift >= 64 { 0 } else { self.seen << shift };
			self.seen |= 1;
			self.top = counter;
		} else {
			self.seen |= 1 << (self.top - counter);
		}
	}
}

/// Bounds-checked little-endian reads.
pub(super) struct Reader<'a>(pub(super) &'a [u8]);

impl<'a> Reader<'a> {
	pub(super) fn take(&mut self, count: usize) -> Option<&'a [u8]> {
		let (taken, rest) = self.0.split_at_checked(count)?;
		self.0 = rest;
		Some(taken)
	}
	pub(super) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
		self.take(N)?.try_into().ok()
	}
	pub(super) fn u8(&mut self) -> Option<u8> {
		Some(self.array::<1>()?[0])
	}
	pub(super) fn u16(&mut self) -> Option<u16> {
		self.array().map(u16::from_le_bytes)
	}
	pub(super) fn u32(&mut self) -> Option<u32> {
		self.array().map(u32::from_le_bytes)
	}
	pub(super) fn u64(&mut self) -> Option<u64> {
		self.array().map(u64::from_le_bytes)
	}
	fn i32(&mut self) -> Option<i32> {
		self.array().map(i32::from_le_bytes)
	}
	fn f32(&mut self) -> Option<f32> {
		self.array().map(f32::from_le_bytes)
	}
}

/// Host state as the viewer needs it, sent on change and every 500 ms.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct State {
	pub(super) permission: Option<Permission>,
	pub(super) paused: bool,
	pub(super) audio: bool,
	pub(super) relative_mouse: bool,
	/// Host display in pixels.
	pub(super) screen: (u16, u16),
	/// Encoded picture size, and where the display sits inside it (x, y, width, height).
	pub(super) stream: (u16, u16),
	pub(super) content: [u16; 4],
}

impl State {
	pub(super) fn encode(&self) -> [u8; 18] {
		let mut out = [0; 18];
		out[0] = match self.permission {
			None => 0,
			Some(Permission::Keyboard) => 1,
			Some(Permission::Mouse) => 2,
			Some(Permission::Full) => 3,
		};
		out[1] =
			u8::from(self.paused) | u8::from(self.audio) << 1 | u8::from(self.relative_mouse) << 2;
		let values = [
			self.screen.0,
			self.screen.1,
			self.stream.0,
			self.stream.1,
			self.content[0],
			self.content[1],
			self.content[2],
			self.content[3],
		];
		for (slot, value) in out[2..].as_chunks_mut::<2>().0.iter_mut().zip(values) {
			*slot = value.to_le_bytes();
		}
		out
	}

	pub(super) fn read(payload: &[u8]) -> Option<Self> {
		let mut reader = Reader(payload);
		let permission = match reader.u8()? {
			0 => None,
			1 => Some(Permission::Keyboard),
			2 => Some(Permission::Mouse),
			3 => Some(Permission::Full),
			_ => return None,
		};
		let flags = reader.u8()?;
		let mut value = || reader.u16();
		let state = Self {
			permission,
			paused: flags & 1 != 0,
			audio: flags & 2 != 0,
			relative_mouse: flags & 4 != 0,
			screen: (value()?, value()?),
			stream: (value()?, value()?),
			content: [value()?, value()?, value()?, value()?],
		};
		let fits = u32::from(state.content[0]) + u32::from(state.content[2])
			<= u32::from(state.stream.0)
			&& u32::from(state.content[1]) + u32::from(state.content[3])
				<= u32::from(state.stream.1);
		(reader.0.is_empty() && fits).then_some(state)
	}
}

/// Video fragment header: frame id, index, count, flags, 90 kHz timestamp.
const FRAGMENT_HEADER: usize = 13;
/// Every fragment but a frame's last carries exactly this many bytes.
pub(super) const MAX_FRAGMENT: usize = MAX_PAYLOAD - FRAGMENT_HEADER;
/// Fragments of the largest access unit the encoder emits.
pub(super) const MAX_FRAGMENTS: usize = crate::screen::MAX_ENCODED_BYTES.div_ceil(MAX_FRAGMENT);

pub(super) struct Fragment<'a> {
	pub(super) frame: u32,
	pub(super) index: u16,
	pub(super) count: u16,
	pub(super) keyframe: bool,
	pub(super) timestamp: u32,
	pub(super) data: &'a [u8],
}

impl<'a> Fragment<'a> {
	/// Writes this fragment as a payload into `out`, returning its length.
	pub(super) fn write(&self, out: &mut [u8; MAX_PAYLOAD]) -> usize {
		out[..4].copy_from_slice(&self.frame.to_le_bytes());
		out[4..6].copy_from_slice(&self.index.to_le_bytes());
		out[6..8].copy_from_slice(&self.count.to_le_bytes());
		out[8] = u8::from(self.keyframe);
		out[9..FRAGMENT_HEADER].copy_from_slice(&self.timestamp.to_le_bytes());
		let end = FRAGMENT_HEADER + self.data.len();
		out[FRAGMENT_HEADER..end].copy_from_slice(self.data);
		end
	}

	pub(super) fn read(payload: &'a [u8]) -> Option<Self> {
		let mut reader = Reader(payload);
		let fragment = Self {
			frame: reader.u32()?,
			index: reader.u16()?,
			count: reader.u16()?,
			keyframe: reader.u8()? & 1 != 0,
			timestamp: reader.u32()?,
			data: reader.0,
		};
		let last = u32::from(fragment.index) + 1 == u32::from(fragment.count);
		let sized = if last {
			!fragment.data.is_empty()
		} else {
			fragment.data.len() == MAX_FRAGMENT
		};
		(fragment.index < fragment.count && usize::from(fragment.count) <= MAX_FRAGMENTS && sized)
			.then_some(fragment)
	}
}

/// Inputs per batch: two batches of the largest inputs fit one datagram.
pub(super) const MAX_BATCH: usize = 32;
/// Encoded size of the largest input (a move).
pub(super) const MAX_INPUT: usize = 17;
const _: () = assert!(4 + 2 * (1 + MAX_BATCH * MAX_INPUT) <= MAX_PAYLOAD);

fn button_code(button: MouseButton) -> u8 {
	match button {
		MouseButton::Left => 0,
		MouseButton::Right => 1,
		MouseButton::Middle => 2,
		MouseButton::Back => 3,
		MouseButton::Forward => 4,
	}
}

fn button(code: u8) -> Option<MouseButton> {
	Some(match code {
		0 => MouseButton::Left,
		1 => MouseButton::Right,
		2 => MouseButton::Middle,
		3 => MouseButton::Back,
		4 => MouseButton::Forward,
		_ => return None,
	})
}

/// Appends one batch: its input count, then each input.
pub(super) fn put_batch(out: &mut Vec<u8>, inputs: &[Input]) {
	out.push(inputs.len().min(MAX_BATCH) as u8);
	for input in inputs.iter().take(MAX_BATCH) {
		match *input {
			Input::Move { x, y, dx, dy } => {
				out.push(1);
				for value in [
					x.to_le_bytes(),
					y.to_le_bytes(),
					dx.to_le_bytes(),
					dy.to_le_bytes(),
				] {
					out.extend_from_slice(&value);
				}
			}
			Input::Button { button, down } => {
				out.extend_from_slice(&[2, button_code(button), u8::from(down)])
			}
			Input::Wheel { dx, dy } => {
				out.push(3);
				out.extend_from_slice(&dx.to_le_bytes());
				out.extend_from_slice(&dy.to_le_bytes());
			}
			Input::Key {
				scan,
				extended,
				down,
			} => {
				out.push(4);
				out.extend_from_slice(&scan.to_le_bytes());
				out.push(u8::from(extended) | u8::from(down) << 1);
			}
		}
	}
}

fn read_input(reader: &mut Reader) -> Option<Input> {
	Some(match reader.u8()? {
		1 => {
			let (x, y) = (reader.f32()?, reader.f32()?);
			if !x.is_finite() || !y.is_finite() {
				return None;
			}
			Input::Move {
				x: x.clamp(0.0, 1.0),
				y: y.clamp(0.0, 1.0),
				dx: reader.i32()?,
				dy: reader.i32()?,
			}
		}
		2 => Input::Button {
			button: button(reader.u8()?)?,
			down: reader.u8()? != 0,
		},
		3 => Input::Wheel {
			dx: reader.i32()?,
			dy: reader.i32()?,
		},
		4 => {
			let scan = reader.u16()?;
			let flags = reader.u8()?;
			Input::Key {
				scan,
				extended: flags & 1 != 0,
				down: flags & 2 != 0,
			}
		}
		_ => return None,
	})
}

/// An input datagram: the sequence of its first batch, then one or two consecutive batches
/// (the previous one repeated, then the new one).
pub(super) fn read_batches(payload: &[u8]) -> Option<(u32, Vec<Vec<Input>>)> {
	let mut reader = Reader(payload);
	let first = reader.u32()?;
	let mut batches = Vec::with_capacity(2);
	while !reader.0.is_empty() {
		let count = usize::from(reader.u8()?);
		if batches.len() == 2 || count == 0 || count > MAX_BATCH {
			return None;
		}
		let mut inputs = Vec::with_capacity(count);
		for _ in 0..count {
			inputs.push(read_input(&mut reader)?);
		}
		batches.push(inputs);
	}
	Some((first, batches))
}

/// Host-side input order: each batch applies once, in sequence; batches that arrive after a
/// newer one are dropped (the viewer's held-state snapshots repair what they carried).
#[derive(Default)]
pub(super) struct Order {
	next: u32,
}

impl Order {
	/// Whether batch `sequence` is new; it then applies.
	pub(super) fn take(&mut self, sequence: u32) -> bool {
		let fresh = sequence >= self.next;
		if fresh {
			self.next = sequence.saturating_add(1);
		}
		fresh
	}

	/// Whether a held-state snapshot taken after batch `sequence` is current: it supersedes
	/// every batch up to `sequence`, including ones still in flight.
	pub(super) fn held(&mut self, sequence: u32) -> bool {
		let current = sequence.saturating_add(1) >= self.next;
		if current {
			self.next = sequence.saturating_add(1);
		}
		current
	}
}

pub(super) const MAX_HELD_KEYS: usize = 64;

/// What the viewer holds after batch `sequence`.
pub(super) struct Held {
	pub(super) sequence: u32,
	pub(super) keys: Vec<(u16, bool)>,
	pub(super) buttons: Vec<MouseButton>,
}

impl Held {
	pub(super) fn encode(sequence: u32, keys: &[(u16, bool)], buttons: &[MouseButton]) -> Vec<u8> {
		let keys = &keys[..keys.len().min(MAX_HELD_KEYS)];
		let mut out = Vec::with_capacity(6 + keys.len() * 3);
		out.extend_from_slice(&sequence.to_le_bytes());
		out.push(
			buttons
				.iter()
				.fold(0, |mask, button| mask | 1 << button_code(*button)),
		);
		out.push(keys.len() as u8);
		for &(scan, extended) in keys {
			out.extend_from_slice(&scan.to_le_bytes());
			out.push(u8::from(extended));
		}
		out
	}

	pub(super) fn read(payload: &[u8]) -> Option<Self> {
		let mut reader = Reader(payload);
		let sequence = reader.u32()?;
		let mask = reader.u8()?;
		let count = usize::from(reader.u8()?);
		if count > MAX_HELD_KEYS {
			return None;
		}
		let mut keys = Vec::with_capacity(count);
		for _ in 0..count {
			keys.push((reader.u16()?, reader.u8()? != 0));
		}
		let buttons = (0..5)
			.filter(|bit| mask & 1 << bit != 0)
			.filter_map(button)
			.collect();
		reader.0.is_empty().then_some(Self {
			sequence,
			keys,
			buttons,
		})
	}
}

const STUN_COOKIE: u32 = 0x2112_a442;

/// RFC 5389 Binding request.
pub(super) fn stun_request(id: &[u8; 12]) -> [u8; 20] {
	let mut request = [0; 20];
	request[..2].copy_from_slice(&1u16.to_be_bytes());
	request[4..8].copy_from_slice(&STUN_COOKIE.to_be_bytes());
	request[8..].copy_from_slice(id);
	request
}

/// Transaction id and mapped IPv4 address of a Binding success response.
pub(super) fn stun_response(data: &[u8]) -> Option<([u8; 12], SocketAddr)> {
	let mut reader = Reader(data);
	let kind = u16::from_be_bytes(reader.array()?);
	let length = usize::from(u16::from_be_bytes(reader.array()?));
	if kind != 0x0101 || u32::from_be_bytes(reader.array()?) != STUN_COOKIE {
		return None;
	}
	let id: [u8; 12] = reader.array()?;
	let mut attributes = Reader(reader.take(length)?);
	let mut mapped = None;
	while attributes.0.len() >= 4 {
		let kind = u16::from_be_bytes(attributes.array()?);
		let size = usize::from(u16::from_be_bytes(attributes.array()?));
		let mut value = Reader(attributes.take(size)?);
		let padding = ((4 - size % 4) % 4).min(attributes.0.len());
		attributes.take(padding)?;
		let xor = match kind {
			0x0020 => true,
			0x0001 => false,
			_ => continue,
		};
		let _reserved = value.u8()?;
		if value.u8()? != 1 {
			continue;
		}
		let mut port = u16::from_be_bytes(value.array()?);
		let mut ip = u32::from_be_bytes(value.array()?);
		if xor {
			port ^= (STUN_COOKIE >> 16) as u16;
			ip ^= STUN_COOKIE;
		}
		let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port);
		// XOR-MAPPED-ADDRESS wins over the legacy attribute.
		if xor || mapped.is_none() {
			mapped = Some(address);
		}
	}
	Some((id, mapped?))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pair() -> (Cipher, Cipher) {
		let (secret, host_key, viewer_key) = ([7; 32], [1; 32], [2; 32]);
		(
			Cipher::new(&secret, 9, &host_key, &viewer_key, true),
			Cipher::new(&secret, 9, &host_key, &viewer_key, false),
		)
	}

	#[test]
	fn datagrams_reject_tampering_replay_and_the_wrong_direction() {
		let (host, viewer) = pair();
		let mut window = Window::default();
		let mut out = [0; MAX_DATAGRAM];
		let len = host.seal(STATE, 1, b"state", &mut out).unwrap();
		let sealed = out;

		let mut copy = sealed;
		assert_eq!(
			viewer.open(&mut copy[..len], &mut window),
			Some((STATE, 1, &b"state"[..]))
		);
		// The same counter again is a replay.
		let mut copy = sealed;
		assert!(viewer.open(&mut copy[..len], &mut window).is_none());
		// Any flipped bit, in the header (AAD), the body or the tag, fails authentication.
		for at in [3, 12, 22, len - 1] {
			let mut copy = sealed;
			copy[at] ^= 1;
			assert!(
				viewer
					.open(&mut copy[..len], &mut Window::default())
					.is_none(),
				"byte {at}"
			);
		}
		// A host cannot read its own direction back.
		let mut copy = sealed;
		assert!(
			host.open(&mut copy[..len], &mut Window::default())
				.is_none()
		);
		// Oversized payloads are never sealed.
		assert!(
			host.seal(VIDEO, 2, &[0; MAX_PAYLOAD + 1], &mut out)
				.is_none()
		);
	}

	#[test]
	fn replay_window_accepts_reordering_once_and_drops_the_too_old() {
		let (host, viewer) = pair();
		let mut window = Window::default();
		let mut open = |counter: u64| {
			let mut out = [0; MAX_DATAGRAM];
			let len = host.seal(PING, counter, &[], &mut out).unwrap();
			viewer.open(&mut out[..len], &mut window).is_some()
		};
		assert!(open(5));
		assert!(open(3), "reordered within the window");
		assert!(!open(3), "replayed");
		assert!(open(100));
		assert!(open(37), "the window's oldest slot");
		assert!(!open(36), "older than the window");
		assert!(!open(0), "counters start at one");
	}

	#[test]
	fn input_batches_round_trip_and_reject_garbage() {
		let inputs = [
			Input::Move {
				x: 0.25,
				y: 1.0,
				dx: -3,
				dy: 7,
			},
			Input::Button {
				button: MouseButton::Forward,
				down: true,
			},
			Input::Wheel { dx: 0, dy: -120 },
			Input::Key {
				scan: 0x1d,
				extended: true,
				down: false,
			},
		];
		let mut payload = 41u32.to_le_bytes().to_vec();
		put_batch(&mut payload, &inputs[..2]);
		put_batch(&mut payload, &inputs[2..]);
		assert_eq!(
			read_batches(&payload),
			Some((41, vec![inputs[..2].to_vec(), inputs[2..].to_vec()]))
		);
		// A move cannot place the pointer outside the screen or at NaN.
		let mut wild = 1u32.to_le_bytes().to_vec();
		put_batch(
			&mut wild,
			&[Input::Move {
				x: 7.0,
				y: -1.0,
				dx: 0,
				dy: 0,
			}],
		);
		let (_, batches) = read_batches(&wild).unwrap();
		assert_eq!(
			batches[0][0],
			Input::Move {
				x: 1.0,
				y: 0.0,
				dx: 0,
				dy: 0
			}
		);
		let mut nan = 1u32.to_le_bytes().to_vec();
		put_batch(
			&mut nan,
			&[Input::Move {
				x: f32::NAN,
				y: 0.0,
				dx: 0,
				dy: 0,
			}],
		);
		assert!(read_batches(&nan).is_none());
		// Truncation, unknown tags and a third batch are refused; no panic on any prefix.
		for end in 0..payload.len() {
			let _ = read_batches(&payload[..end]);
		}
		assert!(read_batches(&payload[..payload.len() - 1]).is_none());
		assert!(read_batches(&[1, 0, 0, 0, 1, 9]).is_none());
		let mut three = payload.clone();
		put_batch(&mut three, &inputs[..1]);
		assert!(read_batches(&three).is_none());
	}

	#[test]
	fn input_order_applies_each_batch_once_in_sequence() {
		let mut order = Order::default();
		let mut applied = Vec::new();
		// Every datagram is sent twice and repeats the previous batch.
		let datagrams: [&[u32]; 9] = [
			&[1],
			&[1],
			&[1, 2],
			&[1, 2],
			&[3, 4],
			&[2, 3],
			&[6, 7],
			&[5, 6],
			&[7, 8],
		];
		for batches in datagrams {
			for &sequence in batches {
				if order.take(sequence) {
					applied.push(sequence);
				}
			}
		}
		// 5 was lost with its repeat and arrived only after 6.
		assert_eq!(applied, [1, 2, 3, 4, 6, 7, 8]);
		// A snapshot taken after batch 8 is current; one from before it is stale.
		assert!(order.held(8));
		assert!(!order.held(7));
		// A snapshot newer than anything applied supersedes the batches still in flight.
		assert!(order.held(10));
		assert!(!order.take(9) && !order.take(10) && order.take(11));
	}

	#[test]
	fn state_and_held_round_trip() {
		let state = State {
			permission: Some(Permission::Mouse),
			paused: true,
			audio: false,
			relative_mouse: true,
			screen: (2560, 1080),
			stream: (1920, 1080),
			content: [0, 135, 1920, 810],
		};
		assert_eq!(State::read(&state.encode()), Some(state));
		let mut outside = state;
		outside.content[3] = 1000;
		assert!(State::read(&outside.encode()).is_none());

		let held = Held::encode(
			12,
			&[(0x2a, false), (0x38, true)],
			&[MouseButton::Left, MouseButton::Back],
		);
		let read = Held::read(&held).unwrap();
		assert_eq!(read.sequence, 12);
		assert_eq!(read.keys, [(0x2a, false), (0x38, true)]);
		assert_eq!(read.buttons, [MouseButton::Left, MouseButton::Back]);
		assert!(Held::read(&held[..held.len() - 1]).is_none());
	}

	#[test]
	fn stun_parses_the_rfc_5769_ipv4_response() {
		// RFC 5769 section 2.2: XOR-MAPPED-ADDRESS 192.0.2.1:32853, with SOFTWARE,
		// MESSAGE-INTEGRITY and FINGERPRINT around it.
		let response: [u8; 80] = [
			0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34,
			0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74,
			0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x08, 0x00, 0x01,
			0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43, 0x00, 0x08, 0x00, 0x14, 0x2b, 0x91, 0xf5, 0x99,
			0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b,
			0xe7, 0xd7, 0x80, 0x28, 0x00, 0x04, 0xc0, 0x7d, 0x4c, 0x96,
		];
		let id = [
			0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
		];
		assert_eq!(
			stun_response(&response),
			Some((id, "192.0.2.1:32853".parse().unwrap()))
		);
		for end in 0..response.len() {
			assert!(stun_response(&response[..end]).is_none());
		}
		assert_eq!(&stun_request(&id)[8..], &id);
	}

	#[test]
	fn fragments_keep_the_fixed_size_contract() {
		let mut out = [0; MAX_PAYLOAD];
		let data = [5; MAX_FRAGMENT];
		let len = Fragment {
			frame: 3,
			index: 0,
			count: 2,
			keyframe: true,
			timestamp: 90,
			data: &data,
		}
		.write(&mut out);
		let read = Fragment::read(&out[..len]).unwrap();
		assert!(
			(
				read.frame,
				read.index,
				read.count,
				read.keyframe,
				read.timestamp
			) == (3, 0, 2, true, 90)
		);
		// A short fragment that is not the last, or an index past the count, is refused.
		assert!(Fragment::read(&out[..len - 1]).is_none());
		out[4] = 2;
		assert!(Fragment::read(&out[..len]).is_none());
	}
}
