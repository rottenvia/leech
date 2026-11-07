//! Remote control between two Leech users of one call: the host's primary display goes to the
//! viewer as H.264 (plus optional system audio) and the viewer's mouse and keyboard come back,
//! peer to peer over one hole-punched, encrypted UDP path. Offers travel over the users' own
//! Discord DMs; nothing passes through a relay or server of ours.
//!
//! Wire: datagrams of at most 1200 bytes with a plaintext header (magic, version, type,
//! session, counter) authenticated as AAD, ChaCha20-Poly1305 with one key per direction from
//! the X25519 secret, and a 64-packet replay window. Unverified against real NATs.
mod host;
mod playback;
mod viewer;
mod wire;

pub use host::{Host, HostConfig, HostStatus};
pub use platform::remote_input::{Input, MouseButton};
pub use viewer::{Viewer, ViewerStatus};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{
	Arc, Mutex, OnceLock, PoisonError,
	atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use x25519_dalek::{PublicKey, StaticSecret};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
	Keyboard,
	Mouse,
	Full,
}

impl Permission {
	fn keyboard(self) -> bool {
		matches!(self, Self::Keyboard | Self::Full)
	}
	fn mouse(self) -> bool {
		matches!(self, Self::Mouse | Self::Full)
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
	ByHost,
	ByViewer,
	Declined,
	/// Nothing arrived for six seconds.
	Lost,
	Failed(&'static str),
}

/// What one side publishes to the other: the session both sides share, its X25519 public
/// key and the UDP addresses it can be reached at.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
	pub session: u64,
	pub key: [u8; 32],
	pub addrs: Vec<SocketAddr>,
}

/// Addresses an offer carries at most.
const MAX_ADDRS: usize = 8;
const TOKEN_VERSION: u8 = 1;
/// Longest accepted token: eight IPv6 addresses fit well below it.
const MAX_TOKEN: usize = 400;

impl Offer {
	/// URL-safe base64 of a fixed binary layout; under 400 characters.
	pub fn to_token(&self) -> String {
		let addrs = &self.addrs[..self.addrs.len().min(MAX_ADDRS)];
		let mut bytes = Vec::with_capacity(42 + addrs.len() * 19);
		bytes.push(TOKEN_VERSION);
		bytes.extend_from_slice(&self.session.to_le_bytes());
		bytes.extend_from_slice(&self.key);
		bytes.push(addrs.len() as u8);
		for addr in addrs {
			match addr.ip() {
				IpAddr::V4(ip) => {
					bytes.push(4);
					bytes.extend_from_slice(&ip.octets());
				}
				IpAddr::V6(ip) => {
					bytes.push(6);
					bytes.extend_from_slice(&ip.octets());
				}
			}
			bytes.extend_from_slice(&addr.port().to_le_bytes());
		}
		URL_SAFE_NO_PAD.encode(bytes)
	}

	pub fn from_token(token: &str) -> Option<Self> {
		let token = token.trim();
		if token.len() > MAX_TOKEN {
			return None;
		}
		let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
		let mut reader = wire::Reader(&bytes);
		if reader.u8()? != TOKEN_VERSION {
			return None;
		}
		let (session, key) = (reader.u64()?, reader.array::<32>()?);
		let count = usize::from(reader.u8()?);
		if count > MAX_ADDRS {
			return None;
		}
		let mut addrs = Vec::with_capacity(count);
		for _ in 0..count {
			let ip = match reader.u8()? {
				4 => IpAddr::from(reader.array::<4>()?),
				6 => IpAddr::from(reader.array::<16>()?),
				_ => return None,
			};
			addrs.push(SocketAddr::new(ip, reader.u16()?));
		}
		reader.0.is_empty().then_some(Self {
			session,
			key,
			addrs,
		})
	}
}

const STUN_SERVERS: [(&str, u16); 2] =
	[("stun.l.google.com", 19302), ("stun.cloudflare.com", 3478)];
const STUN_WAIT: Duration = Duration::from_millis(1500);
const STUN_RETRY: Duration = Duration::from_millis(400);
/// Socket buffers: a 1080p keyframe arrives as a few hundred datagrams.
const SOCKET_BUFFER: usize = 4 * 1024 * 1024;
const PUNCH: Duration = Duration::from_millis(50);
const KEEPALIVE: Duration = Duration::from_secs(1);
const LOST_AFTER: Duration = Duration::from_secs(6);
/// The status shows the peer as connected while something arrived this recently.
const CONNECTED_WITHIN: Duration = Duration::from_secs(3);
/// Session datagrams waiting for the host or viewer task; a keyframe burst fits.
const INBOX: usize = 1024;
/// Goodbyes go out three times, this far apart.
const BYE_SPACING: Duration = Duration::from_millis(20);

/// A bound UDP socket with its public and local candidates, ready to connect.
pub struct Endpoint {
	socket: std::net::UdpSocket,
	session: u64,
	secret: StaticSecret,
	addrs: Vec<SocketAddr>,
}

impl Endpoint {
	/// Binds UDP, asks STUN (stun.l.google.com:19302, stun.cloudflare.com:3478; 1.5 s) for the
	/// public address, adds local IPv4 interface addresses, makes an X25519 secret.
	pub async fn gather(session: u64) -> Result<Self, &'static str> {
		Self::open(session, Ipv4Addr::UNSPECIFIED, true).await
	}

	/// Loopback only, without STUN, for tests.
	#[cfg(test)]
	async fn local(session: u64) -> Result<Self, &'static str> {
		Self::open(session, Ipv4Addr::LOCALHOST, false).await
	}

	async fn open(session: u64, ip: Ipv4Addr, public: bool) -> Result<Self, &'static str> {
		let failed = "Remote control could not open a network socket";
		// Registered with the caller's runtime for STUN, then handed to the link's own.
		let socket = tokio::net::UdpSocket::from_std(bind(ip)?).map_err(|_| failed)?;
		let port = socket.local_addr().map_err(|_| failed)?.port();
		let (mut addrs, interfaces) = if public {
			(discover(&socket).await, local_ipv4())
		} else {
			(Vec::new(), vec![ip])
		};
		for ip in interfaces {
			let addr = SocketAddr::from((ip, port));
			if !addrs.contains(&addr) {
				addrs.push(addr);
			}
		}
		addrs.truncate(MAX_ADDRS);
		if addrs.is_empty() {
			return Err("No network connection is available for remote control");
		}
		Ok(Self {
			socket: socket.into_std().map_err(|_| failed)?,
			session,
			secret: secret()?,
			addrs,
		})
	}

	pub fn offer(&self) -> Offer {
		Offer {
			session: self.session,
			key: PublicKey::from(&self.secret).to_bytes(),
			addrs: self.addrs.clone(),
		}
	}

	/// Punches to every peer address for up to `timeout`; keepalives start at once. The link
	/// then runs on its own media thread until it ends or is dropped.
	pub async fn connect(
		self,
		peer: &Offer,
		host: bool,
		timeout: Duration,
	) -> Result<Link, &'static str> {
		if peer.session != self.session {
			return Err("The remote control offer belongs to another session");
		}
		let candidates: Vec<SocketAddr> = peer
			.addrs
			.iter()
			.copied()
			.filter(SocketAddr::is_ipv4)
			.take(MAX_ADDRS)
			.collect();
		if candidates.is_empty() {
			return Err("The other PC sent no reachable address");
		}
		let public = PublicKey::from(&self.secret).to_bytes();
		let secret = self.secret.diffie_hellman(&PublicKey::from(peer.key));
		if !secret.was_contributory() {
			return Err("The other PC sent an invalid key");
		}
		let (host_key, viewer_key) = if host {
			(&public, &peer.key)
		} else {
			(&peer.key, &public)
		};
		let cipher = wire::Cipher::new(secret.as_bytes(), self.session, host_key, viewer_key, host);
		// Like calls and streams, the link keeps its own thread and runtime so app tasks
		// cannot delay input or paced video, and joins MMCSS under a game's load.
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.map_err(|_| "Remote control could not start")?;
		let socket = {
			let _context = runtime.enter();
			tokio::net::UdpSocket::from_std(self.socket)
		}
		.map_err(|_| "Remote control could not open a network socket")?;
		let shared = Arc::new(Shared {
			socket,
			runtime: runtime.handle().clone(),
			cipher,
			host,
			counter: AtomicU64::new(1),
			peer: OnceLock::new(),
			epoch: Instant::now(),
			heard: AtomicU64::new(0),
			received: AtomicU64::new(0),
			highest: AtomicU64::new(0),
			sent: AtomicU64::new(0),
			rtt: AtomicU32::new(0),
			ended: Mutex::new(None),
			bye: AtomicU8::new(0),
			stop: watch::channel(false).0,
			wake: Mutex::new(None),
		});
		let (inbox_send, inbox) = mpsc::channel(INBOX);
		let (locked_send, locked) = oneshot::channel();
		let task = run(shared.clone(), candidates, inbox_send, locked_send);
		std::thread::Builder::new()
			.name("remote-link".into())
			.spawn(move || {
				let _resolution = crate::timer::Resolution::acquire();
				runtime.block_on(task);
				// Dropping the runtime here ends the host or viewer tasks with the link.
			})
			.map_err(|_| "Remote control could not start")?;
		// Dropping the link on timeout (or when this future is dropped) stops its thread.
		let link = Link {
			shared,
			inbox: Some(inbox),
		};
		match tokio::time::timeout(timeout, locked).await {
			Ok(Ok(())) => Ok(link),
			_ => Err(
				"Could not reach the other PC; one of the networks may block direct connections",
			),
		}
	}
}

fn bind(ip: Ipv4Addr) -> Result<std::net::UdpSocket, &'static str> {
	use socket2::{Domain, Protocol, Socket, Type};
	let failed = "Remote control could not open a network socket";
	let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|_| failed)?;
	// Larger buffers only absorb bursts; the defaults still work.
	let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
	let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
	socket
		.bind(&SocketAddr::from((ip, 0)).into())
		.map_err(|_| failed)?;
	socket.set_nonblocking(true).map_err(|_| failed)?;
	Ok(socket.into())
}

fn secret() -> Result<StaticSecret, &'static str> {
	let mut bytes = [0; 32];
	getrandom::fill(&mut bytes).map_err(|_| "Remote control random initialization failed")?;
	let secret = StaticSecret::from(bytes);
	zeroize::Zeroize::zeroize(&mut bytes);
	Ok(secret)
}

/// Public addresses STUN reports for this socket, from whichever servers answer in time.
async fn discover(socket: &tokio::net::UdpSocket) -> Vec<SocketAddr> {
	let mut found = Vec::new();
	let _ = tokio::time::timeout(STUN_WAIT, async {
		let lookup = |(name, port): (&'static str, u16)| async move {
			tokio::net::lookup_host((name, port))
				.await
				.ok()?
				.find(SocketAddr::is_ipv4)
		};
		let (first, second) = tokio::join!(lookup(STUN_SERVERS[0]), lookup(STUN_SERVERS[1]));
		let mut pending = Vec::new();
		for server in [first, second].into_iter().flatten() {
			let mut id = [0; 12];
			if getrandom::fill(&mut id).is_ok() {
				pending.push((server, id));
			}
		}
		let mut retry = tokio::time::interval(STUN_RETRY);
		let mut buffer = [0; 576];
		while !pending.is_empty() {
			tokio::select! {
				_ = retry.tick() => {
					for (server, id) in &pending {
						let _ = socket.send_to(&wire::stun_request(id), *server).await;
					}
				}
				received = socket.recv_from(&mut buffer) => {
					let Ok((len, from)) = received else { continue };
					if let Some((id, mapped)) = wire::stun_response(&buffer[..len])
						&& let Some(at) = pending.iter().position(|entry| *entry == (from, id))
					{
						pending.swap_remove(at);
						if !found.contains(&mapped) {
							found.push(mapped);
						}
					}
				}
			}
		}
	})
	.await;
	found
}

/// IPv4 addresses of the interfaces that route to the internet and to common gaming VPNs.
fn local_ipv4() -> Vec<Ipv4Addr> {
	// ponytail: route lookups (a connected UDP socket sends nothing) toward the internet and
	// the ranges of Radmin VPN (26/8), Hamachi (25/8) and Tailscale (100.64/10) instead of
	// enumerating adapters; use GetAdaptersAddresses if other interfaces must be offered.
	let mut found = Vec::new();
	for target in ["1.1.1.1:9", "26.0.0.1:9", "25.0.0.1:9", "100.100.100.100:9"] {
		let Ok(probe) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
			continue;
		};
		if probe.connect(target).is_ok()
			&& let Ok(SocketAddr::V4(local)) = probe.local_addr()
			&& !local.ip().is_unspecified()
			&& !local.ip().is_loopback()
			&& !found.contains(local.ip())
		{
			found.push(*local.ip());
		}
	}
	found
}

type Packet = (u8, Vec<u8>);
type Wake = Arc<dyn Fn() + Send + Sync>;

/// A connected, authenticated path to the other PC. It answers pings and keeps the path open
/// until a [`Host`] or [`Viewer`] takes it over; dropping it ends the session.
pub struct Link {
	shared: Arc<Shared>,
	inbox: Option<mpsc::Receiver<Packet>>,
}

impl Link {
	/// The host refuses the session; the viewer sees [`End::Declined`].
	pub fn decline(self) {
		self.shared.finish(End::Declined, Some(wire::BYE_DECLINED));
	}

	/// Why the session ended, if it did, while the host is still deciding.
	pub fn ended(&self) -> Option<End> {
		self.shared.ended()
	}
}

impl Drop for Link {
	fn drop(&mut self) {
		let end = if self.shared.host {
			End::ByHost
		} else {
			End::ByViewer
		};
		self.shared.finish(end, Some(wire::BYE_ENDED));
	}
}

/// State of one link, shared by its thread and the host or viewer that owns it.
struct Shared {
	socket: tokio::net::UdpSocket,
	/// The link thread's runtime, where host and viewer tasks run.
	runtime: tokio::runtime::Handle,
	cipher: wire::Cipher,
	host: bool,
	/// Next datagram counter; never repeats under this link's keys.
	counter: AtomicU64,
	/// The address of the first authenticated datagram; the only one used from then on.
	peer: OnceLock<SocketAddr>,
	epoch: Instant,
	/// Microseconds since `epoch` of the last authenticated datagram.
	heard: AtomicU64,
	/// Authenticated datagrams and the highest counter among them, for loss reports.
	received: AtomicU64,
	highest: AtomicU64,
	sent: AtomicU64,
	rtt: AtomicU32,
	ended: Mutex<Option<End>>,
	/// Goodbye reason plus one, to repeat as the link stops; zero for none.
	bye: AtomicU8,
	stop: watch::Sender<bool>,
	wake: Mutex<Option<Wake>>,
}

impl Shared {
	fn micros(&self) -> u64 {
		self.epoch.elapsed().as_micros() as u64
	}

	fn seal(&self, kind: u8, payload: &[u8]) -> Option<([u8; wire::MAX_DATAGRAM], usize)> {
		let counter = self.counter.fetch_add(1, Ordering::Relaxed);
		let mut datagram = [0; wire::MAX_DATAGRAM];
		let len = self.cipher.seal(kind, counter, payload, &mut datagram)?;
		Some((datagram, len))
	}

	/// Sends to the peer, waiting for socket buffer space.
	async fn send(&self, kind: u8, payload: &[u8]) {
		if let Some(&peer) = self.peer.get()
			&& let Some((datagram, len)) = self.seal(kind, payload)
			&& self.socket.send_to(&datagram[..len], peer).await.is_ok()
		{
			self.sent.fetch_add(len as u64, Ordering::Relaxed);
		}
	}

	/// Sends to the peer now, from any thread; drops the datagram if the socket is full.
	fn try_send(&self, kind: u8, payload: &[u8]) {
		if let Some(&peer) = self.peer.get() {
			self.try_send_to(kind, payload, peer);
		}
	}

	fn try_send_to(&self, kind: u8, payload: &[u8], to: SocketAddr) {
		if let Some((datagram, len)) = self.seal(kind, payload)
			&& self.socket.try_send_to(&datagram[..len], to).is_ok()
		{
			self.sent.fetch_add(len as u64, Ordering::Relaxed);
		}
	}

	fn ended(&self) -> Option<End> {
		*self.ended.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn connected(&self) -> bool {
		self.peer.get().is_some()
			&& self.ended().is_none()
			&& self
				.micros()
				.saturating_sub(self.heard.load(Ordering::Relaxed))
				< CONNECTED_WITHIN.as_micros() as u64
	}

	fn rtt(&self) -> u32 {
		self.rtt.load(Ordering::Relaxed)
	}

	fn set_wake(&self, wake: Wake) {
		*self.wake.lock().unwrap_or_else(PoisonError::into_inner) = Some(wake);
	}

	fn wake(&self) {
		let wake = self
			.wake
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone();
		if let Some(wake) = wake {
			wake();
		}
	}

	/// Ends the session once: records why, says goodbye (now, then twice more from the link
	/// thread) and stops every task of the link.
	fn finish(&self, end: End, bye: Option<u8>) {
		{
			let mut ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
			if ended.is_some() {
				return;
			}
			*ended = Some(end);
		}
		if let Some(reason) = bye {
			self.bye.store(reason + 1, Ordering::Release);
			self.try_send(wire::BYE, &[reason]);
		}
		self.stop.send_replace(true);
		self.wake();
	}

	/// Authenticates one datagram and handles link traffic; session traffic goes to `inbox`.
	fn receive(
		&self,
		datagram: &mut [u8],
		from: SocketAddr,
		window: &mut wire::Window,
		inbox: &mpsc::Sender<Packet>,
		locked: &mut Option<oneshot::Sender<()>>,
	) {
		if self.peer.get().is_some_and(|peer| *peer != from) {
			return;
		}
		let Some((kind, counter, payload)) = self.cipher.open(datagram, window) else {
			return;
		};
		if self.peer.set(from).is_ok()
			&& let Some(locked) = locked.take()
		{
			let _ = locked.send(());
		}
		self.heard.store(self.micros(), Ordering::Relaxed);
		self.received.fetch_add(1, Ordering::Relaxed);
		self.highest.fetch_max(counter, Ordering::Relaxed);
		match kind {
			wire::HELLO => self.try_send(wire::HELLO_ACK, &[]),
			wire::HELLO_ACK => {}
			wire::PING => self.try_send(wire::PONG, payload),
			wire::PONG => {
				if let Ok(sent) = payload.try_into().map(u64::from_le_bytes)
					&& let Some(rtt) = self.micros().checked_sub(sent)
				{
					self.rtt
						.store((rtt / 1000).min(60_000) as u32, Ordering::Relaxed);
				}
			}
			wire::BYE => {
				let end = match payload.first() {
					Some(&wire::BYE_DECLINED) => End::Declined,
					Some(&wire::BYE_FAILED) => {
						End::Failed("The other PC could not continue remote control")
					}
					_ if self.host => End::ByViewer,
					_ => End::ByHost,
				};
				self.finish(end, None);
			}
			_ => {
				let _ = inbox.try_send((kind, payload.to_vec()));
			}
		}
	}
}

/// The link thread: punching, keepalives, loss detection and receiving.
async fn run(
	shared: Arc<Shared>,
	candidates: Vec<SocketAddr>,
	inbox: mpsc::Sender<Packet>,
	locked: oneshot::Sender<()>,
) {
	let mut locked = Some(locked);
	let mut stop = shared.stop.subscribe();
	let mut window = wire::Window::default();
	let mut buffer = [0; 2048];
	let mut punch = tokio::time::interval(PUNCH);
	let mut keepalive = tokio::time::interval(KEEPALIVE);
	keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	loop {
		tokio::select! {
			() = stopped(&mut stop) => break,
			received = shared.socket.recv_from(&mut buffer) => match received {
				Ok((len, from)) if len <= wire::MAX_DATAGRAM => {
					shared.receive(&mut buffer[..len], from, &mut window, &inbox, &mut locked);
				}
				Ok(_) => {}
				// Windows reports an ICMP unreachable from an unused candidate as a receive
				// error; anything persistent must not spin.
				Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
			},
			_ = punch.tick(), if shared.peer.get().is_none() => {
				for &candidate in &candidates {
					shared.try_send_to(wire::HELLO, &[], candidate);
				}
			}
			_ = keepalive.tick() => {
				if shared.peer.get().is_some() {
					let silence = shared.micros().saturating_sub(shared.heard.load(Ordering::Relaxed));
					if silence > LOST_AFTER.as_micros() as u64 {
						shared.finish(End::Lost, None);
					} else {
						shared.try_send(wire::PING, &shared.micros().to_le_bytes());
					}
				}
			}
		}
	}
	let reason = shared.bye.load(Ordering::Acquire);
	if let Some(reason) = reason.checked_sub(1) {
		for _ in 0..2 {
			tokio::time::sleep(BYE_SPACING).await;
			shared.try_send(wire::BYE, &[reason]);
		}
	}
}

/// Stops when the link ends.
async fn stopped(stop: &mut watch::Receiver<bool>) {
	let _ = stop.wait_for(|stop| *stop).await;
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn offer_tokens_round_trip_within_their_bound() {
		let offer = Offer {
			session: 0x0123_4567_89ab_cdef,
			key: [9; 32],
			addrs: (0..MAX_ADDRS as u16)
				.map(|index| {
					if index % 2 == 0 {
						SocketAddr::from(([203, 0, 113, index as u8], 40_000 + index))
					} else {
						SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, index], 50_000))
					}
				})
				.collect(),
		};
		let token = offer.to_token();
		assert!(token.len() < 400, "{} characters", token.len());
		assert!(
			token
				.bytes()
				.all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
		);
		assert_eq!(Offer::from_token(&token), Some(offer.clone()));
		assert_eq!(Offer::from_token(&format!(" {token}\n")), Some(offer));
		assert_eq!(Offer::from_token(&token[..token.len() - 2]), None);
		assert_eq!(Offer::from_token("not a token!"), None);
		assert_eq!(Offer::from_token(&"A".repeat(MAX_TOKEN + 1)), None);
	}

	fn runtime() -> tokio::runtime::Runtime {
		tokio::runtime::Builder::new_multi_thread()
			.worker_threads(2)
			.enable_all()
			.build()
			.unwrap()
	}

	/// Two links over loopback: (host, viewer).
	pub(super) async fn pair(session: u64) -> (Link, Link) {
		let (host, viewer) = (
			Endpoint::local(session).await.unwrap(),
			Endpoint::local(session).await.unwrap(),
		);
		let (host_offer, viewer_offer) = (host.offer(), viewer.offer());
		let timeout = Duration::from_secs(5);
		let (host, viewer) = tokio::join!(
			host.connect(&viewer_offer, true, timeout),
			viewer.connect(&host_offer, false, timeout)
		);
		(host.unwrap(), viewer.unwrap())
	}

	async fn until(mut done: impl FnMut() -> bool) {
		let deadline = Instant::now() + Duration::from_secs(5);
		while !done() {
			assert!(Instant::now() < deadline, "timed out");
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
	}

	#[test]
	fn loopback_links_punch_ping_and_decline() {
		runtime().block_on(async {
			let (host, viewer) = pair(77).await;
			assert!(host.shared.connected() && viewer.shared.connected());
			// A ping comes back as a pong that measures the round trip (here: sent 5 ms ago).
			tokio::time::sleep(Duration::from_millis(10)).await;
			let sent = viewer.shared.micros() - 5_000;
			viewer.shared.try_send(wire::PING, &sent.to_le_bytes());
			until(|| viewer.shared.rtt() >= 5).await;
			// Session traffic reaches the inbox; link traffic does not.
			viewer.shared.try_send(wire::KEYFRAME, &[]);
			let mut host = host;
			let inbox = host.inbox.as_mut().unwrap();
			let packet = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
				.await
				.unwrap();
			assert_eq!(packet, Some((wire::KEYFRAME, Vec::new())));
			host.decline();
			until(|| viewer.ended() == Some(End::Declined)).await;
			assert!(!viewer.shared.connected());
		});
	}

	#[test]
	fn dropping_a_link_ends_the_session_on_both_sides() {
		runtime().block_on(async {
			let (host, viewer) = pair(78).await;
			drop(viewer);
			until(|| host.ended() == Some(End::ByViewer)).await;
		});
	}

	#[test]
	fn mismatched_sessions_and_unreachable_peers_fail() {
		runtime().block_on(async {
			let (one, other) = (
				Endpoint::local(1).await.unwrap(),
				Endpoint::local(2).await.unwrap(),
			);
			assert!(
				one.connect(&other.offer(), true, Duration::from_millis(200))
					.await
					.is_err()
			);
			// Both sides claiming to host never authenticate each other.
			let (one, other) = (
				Endpoint::local(3).await.unwrap(),
				Endpoint::local(3).await.unwrap(),
			);
			let (one_offer, other_offer) = (one.offer(), other.offer());
			let timeout = Duration::from_millis(300);
			let (one, other) = tokio::join!(
				one.connect(&other_offer, true, timeout),
				other.connect(&one_offer, true, timeout)
			);
			assert!(one.is_err() && other.is_err());
		});
	}

	/// The whole path on this PC: the primary display captured and encoded, sent over
	/// loopback, decoded. Control stays paused, so nothing is injected. Needs a desktop and a
	/// GPU, so it runs only on request: `cargo test -p discord-voice remote_end_to_end -- --ignored`.
	#[test]
	#[ignore]
	fn remote_end_to_end_on_this_pc() {
		let runtime = runtime();
		runtime.block_on(async {
			let (host_link, viewer_link) = pair(91).await;
			let handle = tokio::runtime::Handle::current();
			let viewer = Viewer::start(viewer_link, &handle, || {}).unwrap();
			let host = Host::start(
				host_link,
				HostConfig {
					permission: Some(Permission::Keyboard),
					paused: true,
					audio: false,
				},
				&handle,
				|| {},
			)
			.unwrap();
			let started = Instant::now();
			let mut pictures = 0u32;
			let mut first = None;
			let mut size = (0, 0);
			while started.elapsed() < Duration::from_secs(6) {
				if let Some(picture) = viewer.take_picture() {
					pictures += 1;
					first.get_or_insert(started.elapsed());
					size = (picture.width, picture.height);
				}
				tokio::time::sleep(Duration::from_millis(2)).await;
			}
			let status = viewer.status();
			eprintln!(
				"pictures {pictures}, first after {first:?}, {size:?}, screen {:?}, content {:?}, started {}, paused {}, rtt {} ms, host {:?}",
				status.screen,
				status.content,
				status.started,
				status.paused,
				status.rtt_ms,
				host.status()
			);
			assert!(status.started && status.paused && status.ended.is_none());
			assert!(pictures >= 5, "only {pictures} pictures in 6 s");
			host.end();
		});
	}
}
