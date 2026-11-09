//! Remote control between two Leech users of one call: the host's primary display goes to the
//! viewer as H.264 (plus optional system audio) and the viewer's mouse and keyboard come back,
//! peer to peer over one hole-punched, encrypted UDP path. Offers travel over the users' own
//! Discord DMs; nothing passes through a relay or server of ours. Besides STUN hole punching,
//! each side asks its router for a forwarded port (`upnp.rs`), offers its IPv6 address, and
//! tries the ports next to those a NAT that changes ports per destination showed STUN.
//!
//! Wire: datagrams of at most 1200 bytes with a plaintext header (magic, version, type,
//! session, counter) authenticated as AAD, ChaCha20-Poly1305 with one key per direction from
//! the X25519 secret, and a 64-packet replay window. Unverified against real NATs.
mod host;
mod playback;
mod upnp;
mod viewer;
mod wire;

pub use host::{Hand, Host, HostConfig, HostStatus};
pub use platform::remote_input::{Input, MouseButton};
pub use viewer::{Viewer, ViewerStatus};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{
	Arc, Mutex, PoisonError,
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

/// Three servers at different addresses, so a NAT that changes ports per destination shows.
const STUN_SERVERS: [(&str, u16); 3] = [
	("stun.l.google.com", 19302),
	("stun1.l.google.com", 19302),
	("stun.cloudflare.com", 3478),
];
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
/// The host moves to another path only after the one it uses has been quiet this long.
const PATH_STALE: Duration = Duration::from_secs(2);
/// Goodbyes go out three times, this far apart.
const BYE_SPACING: Duration = Duration::from_millis(20);
/// How long the router may take to forward a port (it is asked while STUN runs).
const MAP_WAIT: Duration = Duration::from_millis(2500);
/// Ports above the highest one the other PC's NAT showed STUN, tried when its ports change per
/// destination, and how often.
const SPRAY_PORTS: u16 = 32;
const SPRAY: Duration = Duration::from_millis(200);
/// Where the route lookup for this PC's IPv6 address points (Cloudflare's resolver).
const IPV6_PROBE: &str = "[2606:4700:4700::1111]:9";

/// A bound UDP socket with its public and local candidates, ready to connect.
pub struct Endpoint {
	socket: std::net::UdpSocket,
	session: u64,
	secret: StaticSecret,
	addrs: Vec<SocketAddr>,
	/// The router port forwarded to the socket; removed when the link ends. An endpoint dropped
	/// before connecting leaves it to the router's one-hour lease.
	mapping: Option<upnp::Mapping>,
}

impl Endpoint {
	/// Binds UDP (dual-stack when IPv6 is available), asks STUN (three servers, 1.5 s) for the
	/// public address and the router for a forwarded port (UPnP, 2.5 s) at once, adds this PC's
	/// IPv6 and local IPv4 addresses, makes an X25519 secret.
	pub async fn gather(session: u64) -> Result<Self, &'static str> {
		Self::open(session, IpAddr::V6(Ipv6Addr::UNSPECIFIED), true).await
	}

	/// Loopback only, without STUN, for tests.
	#[cfg(test)]
	async fn local(session: u64) -> Result<Self, &'static str> {
		Self::open(session, IpAddr::V4(Ipv4Addr::LOCALHOST), false).await
	}

	async fn open(session: u64, ip: IpAddr, public: bool) -> Result<Self, &'static str> {
		let failed = "Remote control could not open a network socket";
		// Registered with the caller's runtime for STUN, then handed to the link's own.
		let socket = tokio::net::UdpSocket::from_std(bind(ip)?).map_err(|_| failed)?;
		let local = socket.local_addr().map_err(|_| failed)?;
		let port = local.port();
		let (stun, mapping, interfaces) = if public {
			let (stun, mapping) = tokio::join!(
				discover(&socket),
				tokio::time::timeout(MAP_WAIT, upnp::map(port))
			);
			let mapping = mapping.unwrap_or(Err("router did not answer in time"));
			// IPv6 needs no NAT traversal: this PC's own address is the public one.
			let v6 = local_ipv6().filter(|_| local.is_ipv6()).map(IpAddr::V6);
			let interfaces: Vec<IpAddr> = v6
				.into_iter()
				.chain(local_ipv4().into_iter().map(IpAddr::V4))
				.collect();
			(stun, mapping, interfaces)
		} else {
			(Vec::new(), Err("not asked"), vec![ip])
		};
		let forwarded = mapping
			.as_ref()
			.ok()
			.map(|mapping| SocketAddr::V4(mapping.external));
		client_core::journal::note(
			"remote",
			format!(
				"gathered: STUN {}, router port {}, {}IPv6, {} local",
				nat(&stun),
				match &forwarded {
					Some(addr) if public_ip(addr.ip()) => "forwarded",
					Some(_) => "forwarded behind another NAT",
					None => mapping.as_ref().err().copied().unwrap_or("none"),
				},
				if interfaces.iter().any(IpAddr::is_ipv6) {
					""
				} else {
					"no "
				},
				interfaces.iter().filter(|ip| ip.is_ipv4()).count()
			),
		);
		// A forwarded port on a public address goes first: it works whatever the other side's
		// NAT does. One behind another NAT (the provider's) still helps a PC behind that NAT.
		let mut addrs = Vec::new();
		let candidates = forwarded
			.filter(|addr| public_ip(addr.ip()))
			.into_iter()
			.chain(stun)
			.chain(forwarded)
			.chain(interfaces.into_iter().map(|ip| SocketAddr::new(ip, port)));
		for addr in candidates {
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
			mapping: mapping.ok(),
		})
	}

	pub fn offer(&self) -> Offer {
		Offer {
			session: self.session,
			key: PublicKey::from(&self.secret).to_bytes(),
			addrs: self.addrs.clone(),
		}
	}

	/// Punches to every peer address (and the ports next to theirs when their NAT changes ports
	/// per destination) for up to `timeout`; keepalives start at once. The link then runs on
	/// its own media thread until it ends or is dropped.
	pub async fn connect(
		self,
		peer: &Offer,
		host: bool,
		timeout: Duration,
	) -> Result<Link, &'static str> {
		if peer.session != self.session {
			return Err("The remote control offer belongs to another session");
		}
		let dual = self.socket.local_addr().is_ok_and(|local| local.is_ipv6());
		let candidates: Vec<SocketAddr> = peer
			.addrs
			.iter()
			.copied()
			.filter(|addr| dual || addr.is_ipv4())
			.take(MAX_ADDRS)
			.collect();
		if candidates.is_empty() {
			return Err("The other PC sent no reachable address");
		}
		let nearby = spray(&candidates);
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
			dual,
			runtime: runtime.handle().clone(),
			cipher,
			host,
			counter: AtomicU64::new(1),
			peer: Mutex::new(None),
			peer_heard: AtomicU64::new(0),
			datagrams: AtomicU64::new(0),
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
		client_core::journal::note(
			"remote",
			format!(
				"connecting to {} addresses ({} IPv6){}",
				candidates.len(),
				candidates.iter().filter(|addr| addr.is_ipv6()).count(),
				if nearby.is_empty() {
					""
				} else {
					", their NAT changes ports: trying the next ones too"
				}
			),
		);
		let started = Instant::now();
		let (inbox_send, inbox) = mpsc::channel(INBOX);
		let (locked_send, locked) = oneshot::channel();
		let task = run(
			shared.clone(),
			[candidates.clone(), nearby.clone()],
			self.mapping,
			inbox_send,
			locked_send,
		);
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
		let reached = matches!(tokio::time::timeout(timeout, locked).await, Ok(Ok(())));
		// The connection journal says how (or why not), never where.
		let how = match link.shared.peer() {
			Some(addr) => match candidates.iter().position(|known| *known == addr) {
				Some(index) => format!(
					"connected to their {} address {} of {}",
					kind(addr.ip()),
					index + 1,
					candidates.len()
				),
				None if nearby.contains(&addr) => "connected to a port next to theirs".to_owned(),
				None => "connected to an address they did not list".to_owned(),
			},
			None => format!(
				"no answer: {} datagrams heard, none from them",
				link.shared.datagrams.load(Ordering::Relaxed)
			),
		};
		client_core::journal::note(
			"remote",
			format!("{how} after {} ms", started.elapsed().as_millis()),
		);
		if reached {
			Ok(link)
		} else {
			Err("Could not reach the other PC; one of the networks may block direct connections")
		}
	}
}

/// Binds `ip`; the unspecified IPv6 address means a dual-stack socket that reaches IPv4 too
/// (through IPv4-mapped addresses), or IPv4 alone where IPv6 is turned off.
fn bind(ip: IpAddr) -> Result<std::net::UdpSocket, &'static str> {
	use socket2::{Domain, Protocol, Socket, Type};
	let failed = "Remote control could not open a network socket";
	let dual = || {
		let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).ok()?;
		socket.set_only_v6(false).ok()?;
		socket.bind(&SocketAddr::from((ip, 0)).into()).ok()?;
		Some(socket)
	};
	let socket = match ip {
		IpAddr::V6(_) => dual(),
		IpAddr::V4(_) => None,
	};
	let socket = match socket {
		Some(socket) => socket,
		None => {
			let ip = match ip {
				IpAddr::V4(ip) => ip,
				IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
			};
			let socket =
				Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|_| failed)?;
			socket
				.bind(&SocketAddr::from((ip, 0)).into())
				.map_err(|_| failed)?;
			socket
		}
	};
	// Larger buffers only absorb bursts; the defaults still work.
	let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
	let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
	socket.set_nonblocking(true).map_err(|_| failed)?;
	Ok(socket.into())
}

/// `addr` as a socket of this kind sends to it: IPv4 becomes IPv4-mapped IPv6 on a dual-stack
/// socket.
fn outward(dual: bool, addr: SocketAddr) -> SocketAddr {
	match addr {
		SocketAddr::V4(v4) if dual => SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port()),
		_ => addr,
	}
}

/// A sender as the candidates list it: IPv4-mapped IPv6 back to IPv4.
fn inward(addr: SocketAddr) -> SocketAddr {
	SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// Not private, loopback, link-local, shared (carrier-grade NAT) or otherwise special.
fn public_ip(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(ip) => {
			let [a, b, ..] = ip.octets();
			!(ip.is_private()
				|| ip.is_loopback()
				|| ip.is_link_local()
				|| ip.is_unspecified()
				|| ip.is_broadcast()
				|| ip.is_documentation()
				|| ip.is_multicast()
				|| (a == 100 && (64..128).contains(&b)))
		}
		// Global unicast (2000::/3), without Teredo (2001::/32).
		IpAddr::V6(ip) => {
			let [first, second, ..] = ip.segments();
			first & 0xe000 == 0x2000 && !(first == 0x2001 && second == 0)
		}
	}
}

/// What sort of address the journal calls it.
fn kind(ip: IpAddr) -> &'static str {
	if ip.is_ipv6() {
		"IPv6"
	} else if public_ip(ip) {
		"public"
	} else {
		"local"
	}
}

/// What the STUN servers saw of this socket, for the journal.
fn nat(seen: &[SocketAddr]) -> &'static str {
	match seen {
		[] => "no answer",
		[_] => "one answer",
		[first, rest @ ..] if rest.iter().all(|addr| addr == first) => "same port for every server",
		[first, rest @ ..] if rest.iter().all(|addr| addr.ip() == first.ip()) => {
			"port changes per server"
		}
		_ => "several public addresses",
	}
}

/// The ports just above the highest one the other PC's NAT showed STUN, for each public address
/// it listed with more than one port. A NAT that picks a new port per destination often picks
/// the next free ones for this PC, and sending there opens this side's NAT to them.
// ponytail: only sequential port choice is covered; a NAT that picks ports at random needs
// many sockets on its side (the birthday approach) or a relay.
fn spray(candidates: &[SocketAddr]) -> Vec<SocketAddr> {
	let mut nearby = Vec::new();
	for (index, addr) in candidates.iter().enumerate() {
		let same: Vec<u16> = candidates
			.iter()
			.filter(|other| other.ip() == addr.ip())
			.map(SocketAddr::port)
			.collect();
		if !public_ip(addr.ip())
			|| same.len() < 2
			|| candidates[..index]
				.iter()
				.any(|other| other.ip() == addr.ip())
		{
			continue;
		}
		let top = same.iter().copied().max().unwrap_or(0);
		for port in (1..=SPRAY_PORTS).filter_map(|step| top.checked_add(step)) {
			let next = SocketAddr::new(addr.ip(), port);
			if !candidates.contains(&next) {
				nearby.push(next);
			}
		}
	}
	nearby
}

fn secret() -> Result<StaticSecret, &'static str> {
	let mut bytes = [0; 32];
	getrandom::fill(&mut bytes).map_err(|_| "Remote control random initialization failed")?;
	let secret = StaticSecret::from(bytes);
	zeroize::Zeroize::zeroize(&mut bytes);
	Ok(secret)
}

/// What each STUN server that answers in time saw of this socket, in the order they answered.
async fn discover(socket: &tokio::net::UdpSocket) -> Vec<SocketAddr> {
	let dual = socket.local_addr().is_ok_and(|local| local.is_ipv6());
	let mut found = Vec::new();
	let _ = tokio::time::timeout(STUN_WAIT, async {
		let lookup = |(name, port): (&'static str, u16)| async move {
			tokio::net::lookup_host((name, port))
				.await
				.ok()?
				.find(SocketAddr::is_ipv4)
		};
		let servers = futures_util::future::join_all(STUN_SERVERS.map(lookup)).await;
		let mut pending = Vec::new();
		for server in servers.into_iter().flatten() {
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
						let _ = socket.send_to(&wire::stun_request(id), outward(dual, *server)).await;
					}
				}
				received = socket.recv_from(&mut buffer) => {
					let Ok((len, from)) = received else { continue };
					if let Some((id, mapped)) = wire::stun_response(&buffer[..len])
						&& let Some(at) = pending.iter().position(|entry| *entry == (inward(from), id))
					{
						pending.swap_remove(at);
						found.push(inward(mapped));
					}
				}
			}
		}
	})
	.await;
	found
}

/// This PC's global IPv6 address, when it has a route to the IPv6 internet.
fn local_ipv6() -> Option<Ipv6Addr> {
	let probe = std::net::UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).ok()?;
	probe.connect(IPV6_PROBE).ok()?;
	match probe.local_addr().ok()? {
		SocketAddr::V6(local) if public_ip(IpAddr::V6(*local.ip())) => Some(*local.ip()),
		_ => None,
	}
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
	/// A dual-stack socket, which takes IPv4 addresses in their IPv4-mapped form.
	dual: bool,
	/// The link thread's runtime, where host and viewer tasks run.
	runtime: tokio::runtime::Handle,
	cipher: wire::Cipher,
	host: bool,
	/// Next datagram counter; never repeats under this link's keys.
	counter: AtomicU64,
	/// Where datagrams go. When two paths work (IPv4 and IPv6, or the LAN and the internet)
	/// each side may first lock onto a different one. The viewer always answers where the
	/// host's newest authenticated datagram came from, so it settles on the host's path; the
	/// host keeps its path until that has been quiet for `PATH_STALE` (a NAT that moved the
	/// viewer), so the two never chase each other.
	peer: Mutex<Option<SocketAddr>>,
	/// Microseconds since `epoch` of the last authenticated datagram from `peer`.
	peer_heard: AtomicU64,
	/// Every datagram that arrived, authenticated or not, for the journal.
	datagrams: AtomicU64,
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
	fn peer(&self) -> Option<SocketAddr> {
		*self.peer.lock().unwrap_or_else(PoisonError::into_inner)
	}

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
		if let Some(peer) = self.peer()
			&& let Some((datagram, len)) = self.seal(kind, payload)
			&& self
				.socket
				.send_to(&datagram[..len], outward(self.dual, peer))
				.await
				.is_ok()
		{
			self.sent.fetch_add(len as u64, Ordering::Relaxed);
		}
	}

	/// Sends to the peer now, from any thread; drops the datagram if the socket is full.
	fn try_send(&self, kind: u8, payload: &[u8]) {
		if let Some(peer) = self.peer() {
			self.try_send_to(kind, payload, peer);
		}
	}

	fn try_send_to(&self, kind: u8, payload: &[u8], to: SocketAddr) {
		if let Some((datagram, len)) = self.seal(kind, payload)
			&& self
				.socket
				.try_send_to(&datagram[..len], outward(self.dual, to))
				.is_ok()
		{
			self.sent.fetch_add(len as u64, Ordering::Relaxed);
		}
	}

	fn ended(&self) -> Option<End> {
		*self.ended.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn connected(&self) -> bool {
		self.peer().is_some()
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
		let Some((kind, counter, payload)) = self.cipher.open(datagram, window) else {
			return;
		};
		// Only a datagram that authenticated and is newer than every other moves the path, so
		// a late or replayed one cannot.
		let newest = self.highest.fetch_max(counter, Ordering::Relaxed) < counter;
		let now = self.micros();
		let first = {
			let mut peer = self.peer.lock().unwrap_or_else(PoisonError::into_inner);
			let first = peer.is_none();
			let quiet = now.saturating_sub(self.peer_heard.load(Ordering::Relaxed))
				> PATH_STALE.as_micros() as u64;
			if first || (*peer != Some(from) && newest && (!self.host || quiet)) {
				*peer = Some(from);
			}
			if *peer == Some(from) {
				self.peer_heard.store(now, Ordering::Relaxed);
			}
			first
		};
		if first && let Some(locked) = locked.take() {
			let _ = locked.send(());
		}
		self.heard.store(now, Ordering::Relaxed);
		self.received.fetch_add(1, Ordering::Relaxed);
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

/// The link thread: punching (to the candidates, and to the nearby ports less often),
/// keepalives, loss detection and receiving; it removes the router's port at the end.
async fn run(
	shared: Arc<Shared>,
	[candidates, nearby]: [Vec<SocketAddr>; 2],
	mapping: Option<upnp::Mapping>,
	inbox: mpsc::Sender<Packet>,
	locked: oneshot::Sender<()>,
) {
	let mut locked = Some(locked);
	let mut stop = shared.stop.subscribe();
	let mut window = wire::Window::default();
	let mut buffer = [0; 2048];
	let mut punch = tokio::time::interval(PUNCH);
	let mut next_ports = tokio::time::interval(SPRAY);
	let mut keepalive = tokio::time::interval(KEEPALIVE);
	keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	loop {
		tokio::select! {
			() = stopped(&mut stop) => break,
			received = shared.socket.recv_from(&mut buffer) => match received {
				Ok((len, from)) if len <= wire::MAX_DATAGRAM => {
					shared.datagrams.fetch_add(1, Ordering::Relaxed);
					shared.receive(&mut buffer[..len], inward(from), &mut window, &inbox, &mut locked);
				}
				Ok(_) => {}
				// Windows reports an ICMP unreachable from an unused candidate as a receive
				// error; anything persistent must not spin.
				Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
			},
			_ = punch.tick(), if shared.peer().is_none() => {
				for &candidate in &candidates {
					shared.try_send_to(wire::HELLO, &[], candidate);
				}
			}
			_ = next_ports.tick(), if shared.peer().is_none() && !nearby.is_empty() => {
				for &port in &nearby {
					shared.try_send_to(wire::HELLO, &[], port);
				}
			}
			_ = keepalive.tick() => {
				if shared.peer().is_some() {
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
	if let Some(mapping) = mapping {
		let _ = tokio::time::timeout(Duration::from_secs(1), upnp::unmap(&mapping)).await;
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

	/// Dual-stack sockets: one side offers only IPv6, the other only IPv4 (sent as IPv4-mapped
	/// IPv6), and they still meet and talk.
	#[test]
	fn dual_stack_links_meet_over_ipv4_and_ipv6() {
		runtime().block_on(async {
			let open = || Endpoint::open(78, IpAddr::V6(Ipv6Addr::UNSPECIFIED), false);
			let (mut host, mut viewer) = (open().await.unwrap(), open().await.unwrap());
			let (Ok(SocketAddr::V6(host_at)), Ok(SocketAddr::V6(viewer_at))) =
				(host.socket.local_addr(), viewer.socket.local_addr())
			else {
				return; // IPv6 is turned off here; the IPv4 tests cover the rest.
			};
			host.addrs = vec![SocketAddr::from((Ipv6Addr::LOCALHOST, host_at.port()))];
			viewer.addrs = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, viewer_at.port()))];
			let (host_offer, viewer_offer) = (host.offer(), viewer.offer());
			let timeout = Duration::from_secs(5);
			let (host, viewer) = tokio::join!(
				host.connect(&viewer_offer, true, timeout),
				viewer.connect(&host_offer, false, timeout)
			);
			let (host, viewer) = (host.unwrap(), viewer.unwrap());
			assert!(host.shared.connected() && viewer.shared.connected());
			// Each side may first lock onto another path (IPv6 one way, IPv4 the other); the
			// viewer moves to the host's, and every datagram still arrives meanwhile.
			for _ in 0..5 {
				let (to_host, to_viewer) = (
					host.shared.received.load(Ordering::Relaxed),
					viewer.shared.received.load(Ordering::Relaxed),
				);
				viewer.shared.send(wire::PING, &1u64.to_le_bytes()).await;
				host.shared.send(wire::PING, &1u64.to_le_bytes()).await;
				until(|| {
					host.shared.received.load(Ordering::Relaxed) > to_host
						&& viewer.shared.received.load(Ordering::Relaxed) > to_viewer
				})
				.await;
			}
			let (to_viewer, to_host) = (host.shared.peer().unwrap(), viewer.shared.peer().unwrap());
			assert_eq!(
				to_viewer.is_ipv6(),
				to_host.is_ipv6(),
				"{to_viewer} / {to_host}"
			);
			for addr in [to_viewer, to_host] {
				assert_eq!(addr, inward(addr), "mapped form leaked");
			}
		});
	}

	/// A newer authenticated datagram from another address moves the viewer at once; the host
	/// stays on a path it still hears from.
	#[test]
	fn the_viewer_follows_the_host_and_the_host_keeps_a_live_path() {
		runtime().block_on(async {
			let (host, viewer) = pair(79).await;
			let moved: SocketAddr = "127.0.0.1:9".parse().unwrap();
			let (inbox, _inbox) = mpsc::channel(4);
			let deliver = |from: &Link, to: &Link| {
				let (mut datagram, len) = from.shared.seal(wire::HELLO_ACK, &[]).unwrap();
				let mut window = wire::Window::default();
				to.shared
					.receive(&mut datagram[..len], moved, &mut window, &inbox, &mut None);
			};
			let kept = host.shared.peer();
			deliver(&viewer, &host);
			assert_eq!(host.shared.peer(), kept);
			deliver(&host, &viewer);
			assert_eq!(viewer.shared.peer(), Some(moved));
		});
	}

	#[test]
	fn a_nat_that_changes_ports_gets_the_next_ones_tried() {
		let them: [SocketAddr; 4] = [
			"81.2.69.160:40001".parse().unwrap(),
			"81.2.69.160:40003".parse().unwrap(),
			"192.168.1.2:5000".parse().unwrap(),
			"81.2.69.161:6000".parse().unwrap(),
		];
		let nearby = spray(&them);
		assert_eq!(nearby.len(), usize::from(SPRAY_PORTS));
		assert_eq!(nearby[0], "81.2.69.160:40004".parse().unwrap());
		assert!(
			nearby
				.iter()
				.all(|addr| addr.ip() == them[0].ip() && addr.port() > 40_003)
		);
		// One port per address, or a private one, gives nothing to try.
		assert!(spray(&them[1..]).is_empty());
		let private = ["10.0.0.2:1".parse().unwrap(), "10.0.0.2:2".parse().unwrap()];
		assert!(spray(&private).is_empty());
		assert_eq!(nat(&them[..2]), "port changes per server");
		assert_eq!(
			nat(&[them[0], them[0], them[0]]),
			"same port for every server"
		);
		assert_eq!(nat(&them[2..]), "several public addresses");
	}

	#[test]
	fn only_internet_addresses_count_as_public() {
		for ip in ["81.2.69.160", "2a02:6b8::1"] {
			assert!(public_ip(ip.parse().unwrap()), "{ip}");
		}
		for ip in [
			"10.1.2.3",
			"172.16.0.1",
			"192.168.0.1",
			"100.64.0.1",
			"127.0.0.1",
			"169.254.1.1",
			"0.0.0.0",
			"fe80::1",
			"fd00::1",
			"::1",
			"2001:0:4136:e378::1",
		] {
			assert!(!public_ip(ip.parse().unwrap()), "{ip}");
		}
		assert_eq!(
			inward("[::ffff:81.2.69.160]:9".parse().unwrap()),
			"81.2.69.160:9".parse().unwrap()
		);
		assert_eq!(
			outward(true, "81.2.69.160:9".parse().unwrap()),
			"[::ffff:81.2.69.160]:9".parse().unwrap()
		);
	}

	/// What this PC offers on its real network, as kinds and ports (no addresses):
	/// `cargo test -p discord-voice gather_on_this_pc -- --ignored --nocapture`.
	#[test]
	#[ignore]
	fn gather_on_this_pc() {
		runtime().block_on(async {
			let started = Instant::now();
			let endpoint = Endpoint::gather(5).await.unwrap();
			let local = endpoint.socket.local_addr().unwrap();
			println!(
				"{} ms, socket {} port {}, router port {:?}",
				started.elapsed().as_millis(),
				if local.is_ipv6() {
					"dual-stack"
				} else {
					"IPv4"
				},
				local.port(),
				endpoint.mapping.as_ref().map(|mapping| (
					kind(IpAddr::V4(*mapping.external.ip())),
					mapping.external.port()
				))
			);
			for addr in &endpoint.addrs {
				println!("  {} port {}", kind(addr.ip()), addr.port());
			}
			if let Some(mapping) = &endpoint.mapping {
				upnp::unmap(mapping).await;
			}
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
