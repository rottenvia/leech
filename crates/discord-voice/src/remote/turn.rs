//! A small TURN client (RFC 8656) on the link's own UDP socket, for PCs that cannot reach each
//! other directly (a symmetric NAT or carrier-grade NAT on either side, or a firewall that
//! only lets replies in). The relay forwards datagrams that are already sealed, so it only
//! ever sees ciphertext. UDP only, Send and Data indications, no channels: the indication
//! header costs 36 bytes on a 1200-byte datagram.
use super::wire::Reader;
use md5::Md5;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Where a relay is and the long-term login for it.
#[derive(Clone, Debug)]
pub struct RelayLogin {
	pub server: SocketAddr,
	pub username: String,
	pub password: String,
}

const COOKIE: u32 = 0x2112_a442;
const ALLOCATE: u16 = 0x0003;
const REFRESH: u16 = 0x0004;
const CREATE_PERMISSION: u16 = 0x0008;
const SEND_INDICATION: u16 = 0x0016;
const DATA_INDICATION: u16 = 0x0017;
const SUCCESS: u16 = 0x0100;
const ERROR: u16 = 0x0110;

const USERNAME: u16 = 0x0006;
const MESSAGE_INTEGRITY: u16 = 0x0008;
const ERROR_CODE: u16 = 0x0009;
const LIFETIME: u16 = 0x000d;
const XOR_PEER_ADDRESS: u16 = 0x0012;
const DATA: u16 = 0x0013;
const REALM: u16 = 0x0014;
const NONCE: u16 = 0x0015;
const XOR_RELAYED_ADDRESS: u16 = 0x0016;
const REQUESTED_TRANSPORT: u16 = 0x0019;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

const ALLOCATE_WAIT: Duration = Duration::from_millis(2500);
const ALLOCATE_RETRY: Duration = Duration::from_millis(400);

/// One STUN message being written.
struct Writer(Vec<u8>);

impl Writer {
	fn new(kind: u16, id: &[u8; 12]) -> Self {
		let mut bytes = Vec::with_capacity(1300);
		bytes.extend_from_slice(&kind.to_be_bytes());
		bytes.extend_from_slice(&[0, 0]);
		bytes.extend_from_slice(&COOKIE.to_be_bytes());
		bytes.extend_from_slice(id);
		Self(bytes)
	}

	fn attribute(mut self, kind: u16, value: &[u8]) -> Self {
		self.0.extend_from_slice(&kind.to_be_bytes());
		self.0
			.extend_from_slice(&(value.len() as u16).to_be_bytes());
		self.0.extend_from_slice(value);
		self.0.resize(self.0.len().next_multiple_of(4), 0);
		self.set_length(self.0.len() - 20);
		self
	}

	fn address(self, kind: u16, address: SocketAddrV4) -> Self {
		let port = address.port() ^ (COOKIE >> 16) as u16;
		let ip = u32::from(*address.ip()) ^ COOKIE;
		let mut value = [0, 1, 0, 0, 0, 0, 0, 0];
		value[2..4].copy_from_slice(&port.to_be_bytes());
		value[4..].copy_from_slice(&ip.to_be_bytes());
		self.attribute(kind, &value)
	}

	fn set_length(&mut self, length: usize) {
		self.0[2..4].copy_from_slice(&(length as u16).to_be_bytes());
	}

	/// Username, realm and nonce, then MESSAGE-INTEGRITY over everything before it.
	fn sign(mut self, auth: &Auth) -> Vec<u8> {
		use hmac::{KeyInit, Mac};
		self = self
			.attribute(USERNAME, auth.username.as_bytes())
			.attribute(REALM, &auth.realm)
			.attribute(NONCE, &auth.nonce);
		// The length covers the integrity attribute while it is computed (RFC 8489 14.5).
		let length = self.0.len() - 20 + 24;
		self.set_length(length);
		let Ok(mut mac) = hmac::Hmac::<sha1::Sha1>::new_from_slice(&auth.key) else {
			return self.0;
		};
		mac.update(&self.0);
		let digest = mac.finalize().into_bytes();
		self.attribute(MESSAGE_INTEGRITY, &digest).0
	}
}

/// A received STUN message: its type, transaction id and attributes.
struct Message<'a> {
	kind: u16,
	id: [u8; 12],
	attributes: Vec<(u16, &'a [u8])>,
	/// The whole datagram, to find DATA's place in it.
	data: &'a [u8],
}

impl<'a> Message<'a> {
	fn parse(data: &'a [u8]) -> Option<Self> {
		let mut reader = Reader(data);
		let kind = u16::from_be_bytes(reader.array()?);
		let length = usize::from(u16::from_be_bytes(reader.array()?));
		if kind & 0xc000 != 0 || u32::from_be_bytes(reader.array()?) != COOKIE {
			return None;
		}
		let id = reader.array()?;
		let mut rest = Reader(reader.take(length)?);
		let mut attributes = Vec::new();
		while rest.0.len() >= 4 {
			let kind = u16::from_be_bytes(rest.array()?);
			let size = usize::from(u16::from_be_bytes(rest.array()?));
			attributes.push((kind, rest.take(size)?));
			rest.take(((4 - size % 4) % 4).min(rest.0.len()))?;
		}
		Some(Self {
			kind,
			id,
			attributes,
			data,
		})
	}

	fn get(&self, kind: u16) -> Option<&'a [u8]> {
		self.attributes
			.iter()
			.find(|(found, _)| *found == kind)
			.map(|(_, value)| *value)
	}

	fn address(&self, kind: u16) -> Option<SocketAddr> {
		let value = self.get(kind)?;
		if value.len() != 8 || value[1] != 1 {
			return None;
		}
		let port = u16::from_be_bytes([value[2], value[3]]) ^ (COOKIE >> 16) as u16;
		let ip = u32::from_be_bytes(value[4..8].try_into().ok()?) ^ COOKIE;
		Some(SocketAddr::from((Ipv4Addr::from(ip), port)))
	}

	fn error(&self) -> Option<u16> {
		let value = self.get(ERROR_CODE)?;
		(value.len() >= 4).then(|| u16::from(value[2] & 7) * 100 + u16::from(value[3]))
	}

	/// Where DATA's value sits in the received buffer.
	fn data_range(&self) -> Option<std::ops::Range<usize>> {
		let value = self.get(DATA)?;
		let start = value.as_ptr() as usize - self.data.as_ptr() as usize;
		Some(start..start + value.len())
	}
}

/// The long-term credential (RFC 8489 9.2): the key is MD5 of "username:realm:password".
#[derive(Clone)]
struct Auth {
	username: String,
	password: String,
	realm: Vec<u8>,
	nonce: Vec<u8>,
	key: [u8; 16],
}

impl Auth {
	fn new(login: &RelayLogin, realm: &[u8], nonce: &[u8]) -> Self {
		use md5::Digest;
		let mut hash = Md5::new();
		hash.update(login.username.as_bytes());
		hash.update(b":");
		hash.update(realm);
		hash.update(b":");
		hash.update(login.password.as_bytes());
		Self {
			username: login.username.clone(),
			password: login.password.clone(),
			realm: realm.to_vec(),
			nonce: nonce.to_vec(),
			key: hash.finalize().into(),
		}
	}
}

/// What the relay sent the link.
pub(super) enum Incoming {
	/// A datagram from `peer`, at this range of the buffer.
	Data(SocketAddr, std::ops::Range<usize>),
	/// The relay wants the requests again with a new nonce.
	Retry,
}

/// An allocation on a relay: the address others reach this PC at through it.
pub(super) struct Relay {
	pub(super) server: SocketAddr,
	pub(super) relayed: SocketAddr,
	/// This PC's public address as the relay sees it.
	pub(super) mapped: Option<SocketAddr>,
	auth: Mutex<Auth>,
}

impl Relay {
	/// Asks `login.server` for an allocation: the first request learns the realm and nonce,
	/// the second is signed with them. `None` when the relay does not answer in 2.5 s or
	/// refuses the login.
	pub(super) async fn allocate(
		socket: &tokio::net::UdpSocket,
		login: &RelayLogin,
	) -> Result<Self, &'static str> {
		let mut id = [0; 12];
		getrandom::fill(&mut id).map_err(|_| "no random id")?;
		let transport = [17, 0, 0, 0];
		let mut auth: Option<Auth> = None;
		let mut buffer = [0; 1500];
		let mut retry = tokio::time::interval(ALLOCATE_RETRY);
		let deadline = tokio::time::sleep(ALLOCATE_WAIT);
		tokio::pin!(deadline);
		loop {
			tokio::select! {
				() = &mut deadline => return Err("no answer"),
				_ = retry.tick() => {
					let request = Writer::new(ALLOCATE, &id)
						.attribute(REQUESTED_TRANSPORT, &transport);
					let request = match &auth {
						Some(auth) => request.sign(auth),
						None => request.0,
					};
					let _ = socket.send_to(&request, login.server).await;
				}
				received = socket.recv_from(&mut buffer) => {
					let Ok((len, from)) = received else { continue };
					let Some(message) = Message::parse(&buffer[..len]) else { continue };
					if from != login.server || message.id != id {
						continue;
					}
					match message.kind {
						kind if kind == ALLOCATE | SUCCESS => {
							let (Some(relayed), Some(auth)) =
								(message.address(XOR_RELAYED_ADDRESS), auth)
							else {
								return Err("unexpected answer");
							};
							return Ok(Self {
								server: login.server,
								relayed,
								mapped: message.address(XOR_MAPPED_ADDRESS),
								auth: Mutex::new(auth),
							});
						}
						kind if kind == ALLOCATE | ERROR => {
							let (Some(realm), Some(nonce)) = (message.get(REALM), message.get(NONCE))
							else {
								return Err("refused");
							};
							// 401 asks for the login, 438 for a fresh nonce; the same nonce
							// twice means the login itself was refused.
							if auth.as_ref().is_some_and(|auth| auth.nonce == nonce)
								|| !matches!(message.error(), Some(401 | 438))
							{
								return Err("refused");
							}
							auth = Some(Auth::new(login, realm, nonce));
							getrandom::fill(&mut id).map_err(|_| "no random id")?;
							retry.reset_immediately();
						}
						_ => {}
					}
				}
			}
		}
	}

	fn signed(&self, kind: u16, build: impl FnOnce(Writer) -> Writer) -> Vec<u8> {
		let mut id = [0; 12];
		let _ = getrandom::fill(&mut id);
		let auth = self
			.auth
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone();
		build(Writer::new(kind, &id)).sign(&auth)
	}

	/// Keeps the allocation for `seconds` more; zero gives it back.
	pub(super) fn refresh(&self, seconds: u32) -> Vec<u8> {
		self.signed(REFRESH, |request| {
			request.attribute(LIFETIME, &seconds.to_be_bytes())
		})
	}

	/// Lets datagrams from (and to) `peer`'s address through the relay for five minutes.
	pub(super) fn permission(&self, peer: Ipv4Addr) -> Vec<u8> {
		self.signed(CREATE_PERMISSION, |request| {
			request.address(XOR_PEER_ADDRESS, SocketAddrV4::new(peer, 0))
		})
	}

	/// `data` for `peer`, sent from the relay's address. Indications are not signed.
	pub(super) fn send(&self, peer: SocketAddr, data: &[u8]) -> Option<Vec<u8>> {
		let SocketAddr::V4(peer) = peer else {
			return None;
		};
		// Indications need no unique id; the relay never answers them.
		Some(
			Writer::new(SEND_INDICATION, &[0; 12])
				.address(XOR_PEER_ADDRESS, peer)
				.attribute(DATA, data)
				.0,
		)
	}

	/// A datagram from the relay: data from a peer, a refused request to repeat with a new
	/// nonce, or nothing to act on.
	pub(super) fn receive(&self, datagram: &[u8]) -> Option<Incoming> {
		let message = Message::parse(datagram)?;
		match message.kind {
			DATA_INDICATION => Some(Incoming::Data(
				message.address(XOR_PEER_ADDRESS)?,
				message.data_range()?,
			)),
			kind if kind & ERROR == ERROR && matches!(message.error(), Some(401 | 438)) => {
				let nonce = message.get(NONCE)?;
				let mut auth = self.auth.lock().unwrap_or_else(PoisonError::into_inner);
				// The same nonce again: the login is no longer accepted; do not loop.
				if auth.nonce == nonce {
					return None;
				}
				let login = RelayLogin {
					server: self.server,
					username: auth.username.clone(),
					password: auth.password.clone(),
				};
				*auth = Auth::new(&login, message.get(REALM).unwrap_or(&auth.realm), nonce);
				Some(Incoming::Retry)
			}
			_ => None,
		}
	}
}

/// Addresses a relay can send to: not private, loopback, link-local or shared (CGNAT).
pub(super) fn routable(ip: IpAddr) -> Option<Ipv4Addr> {
	let IpAddr::V4(ip) = ip else {
		return None;
	};
	let [a, b, ..] = ip.octets();
	let shared = a == 100 && (64..128).contains(&b);
	(!ip.is_private()
		&& !ip.is_loopback()
		&& !ip.is_link_local()
		&& !ip.is_unspecified()
		&& !ip.is_broadcast()
		&& !ip.is_multicast()
		&& !ip.is_documentation()
		&& !shared)
		.then_some(ip)
}

/// A relay on loopback for tests: asks for the login once, grants every request and forwards
/// indications both ways. It checks no permissions or signatures (RFC 5769 covers signing).
#[cfg(test)]
pub(super) async fn test_relay() -> RelayLogin {
	let control = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
	let relayed = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
	let server = control.local_addr().unwrap();
	let SocketAddr::V4(relay_addr) = relayed.local_addr().unwrap() else {
		unreachable!()
	};
	tokio::spawn(async move {
		let mut client = None;
		let (mut from_client, mut from_peer) = ([0; 1500], [0; 1500]);
		loop {
			tokio::select! {
				received = control.recv_from(&mut from_client) => {
					let Ok((len, from)) = received else { continue };
					let Some(message) = Message::parse(&from_client[..len]) else { continue };
					let SocketAddr::V4(from_v4) = from else { continue };
					let reply = match message.kind {
						ALLOCATE if message.get(MESSAGE_INTEGRITY).is_none() => Writer::new(ALLOCATE | ERROR, &message.id)
							.attribute(ERROR_CODE, &[0, 0, 4, 1])
							.attribute(REALM, b"leech")
							.attribute(NONCE, b"first")
							.0,
						ALLOCATE => {
							client = Some(from);
							Writer::new(ALLOCATE | SUCCESS, &message.id)
								.address(XOR_RELAYED_ADDRESS, relay_addr)
								.address(XOR_MAPPED_ADDRESS, from_v4)
								.attribute(LIFETIME, &600u32.to_be_bytes())
								.0
						}
						REFRESH | CREATE_PERMISSION => Writer::new(message.kind | SUCCESS, &message.id).0,
						SEND_INDICATION => {
							if let (Some(peer), Some(data)) = (message.address(XOR_PEER_ADDRESS), message.get(DATA)) {
								let _ = relayed.send_to(data, peer).await;
							}
							continue;
						}
						_ => continue,
					};
					let _ = control.send_to(&reply, from).await;
				}
				received = relayed.recv_from(&mut from_peer) => {
					let (Ok((len, SocketAddr::V4(peer))), Some(client)) = (received, client) else { continue };
					let data = Writer::new(DATA_INDICATION, &[0; 12])
						.address(XOR_PEER_ADDRESS, peer)
						.attribute(DATA, &from_peer[..len])
						.0;
					let _ = control.send_to(&data, client).await;
				}
			}
		}
	});
	RelayLogin {
		server,
		username: "leech".into(),
		password: "test".into(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// RFC 5769 section 2.4: a request signed with a long-term credential.
	#[test]
	fn signing_matches_rfc_5769() {
		let login = RelayLogin {
			server: "192.0.2.1:3478".parse().unwrap(),
			username: "\u{30de}\u{30c8}\u{30ea}\u{30c3}\u{30af}\u{30b9}".into(),
			// The password after SASLprep.
			password: "TheMatrIX".into(),
		};
		let auth = Auth::new(&login, b"example.org", b"f//499k954d6OL34oL9FSTvy64sA");
		let id = [
			0x78, 0xad, 0x34, 0x33, 0xc6, 0xad, 0x72, 0xc0, 0x29, 0xda, 0x41, 0x2e,
		];
		// The RFC orders USERNAME, NONCE, REALM; sign() writes REALM before NONCE, so build
		// this one by hand with the same integrity step.
		let mut request = Writer::new(0x0001, &id)
			.attribute(USERNAME, auth.username.as_bytes())
			.attribute(NONCE, &auth.nonce)
			.attribute(REALM, &auth.realm);
		let length = request.0.len() - 20 + 24;
		request.set_length(length);
		use hmac::{KeyInit, Mac};
		let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(&auth.key).unwrap();
		mac.update(&request.0);
		let request = request
			.attribute(MESSAGE_INTEGRITY, &mac.finalize().into_bytes())
			.0;
		assert_eq!(&request[..4], &[0x00, 0x01, 0x00, 0x60]);
		assert_eq!(
			&request[request.len() - 20..],
			&[
				0xf6, 0x70, 0x24, 0x65, 0x6d, 0xd6, 0x4a, 0x3e, 0x02, 0xb8, 0xe0, 0x71, 0x2e, 0x85,
				0xc9, 0xa2, 0x8c, 0xa8, 0x96, 0x66
			]
		);
	}

	#[test]
	fn indications_round_trip_their_peer_and_data() {
		let relay = Relay {
			server: "192.0.2.1:3478".parse().unwrap(),
			relayed: "192.0.2.1:50000".parse().unwrap(),
			mapped: None,
			auth: Mutex::new(Auth::new(
				&RelayLogin {
					server: "192.0.2.1:3478".parse().unwrap(),
					username: "u".into(),
					password: "p".into(),
				},
				b"r",
				b"n",
			)),
		};
		let peer: SocketAddr = "203.0.113.7:4242".parse().unwrap();
		let mut sent = relay.send(peer, b"sealed bytes").unwrap();
		// A Data indication is a Send indication with another type.
		sent[..2].copy_from_slice(&DATA_INDICATION.to_be_bytes());
		let Some(Incoming::Data(from, range)) = relay.receive(&sent) else {
			panic!("no data");
		};
		assert_eq!((from, &sent[range]), (peer, &b"sealed bytes"[..]));
		for end in 0..sent.len() {
			assert!(relay.receive(&sent[..end]).is_none());
		}
	}

	#[test]
	fn only_public_ipv4_gets_permissions() {
		for private in [
			"10.1.2.3",
			"192.168.1.5",
			"172.20.0.1",
			"127.0.0.1",
			"100.70.1.1",
		] {
			assert_eq!(routable(private.parse().unwrap()), None, "{private}");
		}
		assert!(routable("26.10.0.3".parse().unwrap()).is_some());
		assert!(routable("::1".parse().unwrap()).is_none());
	}
}
