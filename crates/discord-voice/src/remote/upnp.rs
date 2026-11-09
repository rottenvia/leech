//! Asks the home router to forward a UDP port to the link's socket (UPnP Internet Gateway
//! Device: SSDP search, the router's description, then SOAP calls). With that port the other PC
//! reaches this one directly whatever its own NAT does. Plain HTTP on the LAN, to the router
//! that answered the search only, bounded in size and time. The mapping is removed when the
//! session ends, and the router drops it after an hour if Leech cannot.
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SSDP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
const SEARCH_WAIT: Duration = Duration::from_millis(1200);
const SEARCH_RETRY: Duration = Duration::from_millis(300);
const HTTP_WAIT: Duration = Duration::from_millis(1500);
/// Descriptions and SOAP answers are a few kilobytes; anything longer is not a router's.
const MAX_HTTP: usize = 64 * 1024;
const LEASE_SECONDS: u32 = 3600;
const DESCRIPTION: &str = "Leech remote control";
/// UPnP error codes: the external port is taken by another mapping.
const CONFLICT: u32 = 718;

/// A port the router forwards to this PC until [`unmap`] or the lease ends.
#[derive(Debug)]
pub(super) struct Mapping {
	control: Control,
	/// The router's own address on its internet side, and the forwarded port.
	pub(super) external: SocketAddrV4,
}

/// Where the router takes SOAP calls for one WAN connection service.
#[derive(Clone, Debug, PartialEq)]
struct Control {
	router: SocketAddrV4,
	path: String,
	service: String,
}

/// Maps a router port to `port` on this PC; an error names what went wrong for the journal.
pub(super) async fn map(port: u16) -> Result<Mapping, &'static str> {
	let (router, path) = search().await?;
	let local = local_toward(*router.ip()).ok_or("no route to the router")?;
	let controls = controls(router, &path).await?;
	map_with(&controls, local, port).await
}

/// Removes the mapping; best effort.
pub(super) async fn unmap(mapping: &Mapping) {
	let arguments = format!(
		"<NewRemoteHost></NewRemoteHost><NewExternalPort>{}</NewExternalPort>\
		 <NewProtocol>UDP</NewProtocol>",
		mapping.external.port()
	);
	let _ = soap(&mapping.control, "DeletePortMapping", &arguments).await;
}

/// The first router that answers an SSDP search: its HTTP address and description path.
async fn search() -> Result<(SocketAddrV4, String), &'static str> {
	let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
		.await
		.map_err(|_| "no socket")?;
	let mut retry = tokio::time::interval(SEARCH_RETRY);
	let mut buffer = [0; 1500];
	let found = tokio::time::timeout(SEARCH_WAIT, async {
		loop {
			tokio::select! {
				_ = retry.tick() => {
					for version in [1, 2] {
						let request = format!(
							"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
							 MAN: \"ssdp:discover\"\r\nMX: 1\r\n\
							 ST: urn:schemas-upnp-org:device:InternetGatewayDevice:{version}\r\n\r\n"
						);
						let _ = socket.send_to(request.as_bytes(), SSDP).await;
					}
				}
				received = socket.recv_from(&mut buffer) => {
					let Ok((len, SocketAddr::V4(from))) = received else { continue };
					// Only the device that answered may be asked, so a description cannot send
					// Leech anywhere else on the network.
					if let Some(location) = header(&buffer[..len], "location")
						&& let Some((router, path)) = http_url(location)
						&& router.ip() == from.ip()
					{
						return (router, path);
					}
				}
			}
		}
	})
	.await;
	found.map_err(|_| "no router answered")
}

/// A header's value in an HTTP-like message, by case-insensitive name.
fn header<'a>(message: &'a [u8], name: &str) -> Option<&'a str> {
	std::str::from_utf8(message).ok()?.lines().find_map(|line| {
		let (key, value) = line.split_once(':')?;
		key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
	})
}

/// `http://host[:port]/path` with an IPv4 host.
fn http_url(url: &str) -> Option<(SocketAddrV4, String)> {
	if !url.get(..7)?.eq_ignore_ascii_case("http://") {
		return None;
	}
	let rest = &url[7..];
	let (host, path) = match rest.split_once('/') {
		Some((host, path)) => (host, format!("/{path}")),
		None => (rest, "/".to_owned()),
	};
	let router = match host.parse::<SocketAddrV4>() {
		Ok(router) => router,
		Err(_) => SocketAddrV4::new(host.parse().ok()?, 80),
	};
	Some((router, path))
}

/// This PC's address on the router's network.
fn local_toward(router: Ipv4Addr) -> Option<Ipv4Addr> {
	let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
	probe.connect((router, 1900)).ok()?;
	match probe.local_addr().ok()? {
		SocketAddr::V4(local) => Some(*local.ip()),
		SocketAddr::V6(_) => None,
	}
}

/// The WAN connection services in the router's description, in its order.
async fn controls(router: SocketAddrV4, path: &str) -> Result<Vec<Control>, &'static str> {
	let request = format!("GET {path} HTTP/1.0\r\nHost: {router}\r\nConnection: close\r\n\r\n");
	let (200, description) = http(router, &request).await? else {
		return Err("router description unavailable");
	};
	let mut controls = Vec::new();
	for service in description.split("<service>").skip(1) {
		let (Some(kind), Some(url)) = (tag(service, "serviceType"), tag(service, "controlURL"))
		else {
			continue;
		};
		if !kind.contains(":WANIPConnection:") && !kind.contains(":WANPPPConnection:") {
			continue;
		}
		let (at, path) = if url
			.get(..7)
			.is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"))
		{
			match http_url(url) {
				Some(found) if found.0.ip() == router.ip() => found,
				_ => continue,
			}
		} else if url.starts_with('/') {
			(router, url.to_owned())
		} else {
			(router, format!("/{url}"))
		};
		controls.push(Control {
			router: at,
			path,
			service: kind.to_owned(),
		});
	}
	if controls.is_empty() {
		Err("router has no port mapping service")
	} else {
		Ok(controls)
	}
}

/// The trimmed text of the first `<name>…</name>`.
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
	let start = text.find(&format!("<{name}>"))? + name.len() + 2;
	let end = start + text[start..].find(&format!("</{name}>"))?;
	Some(text[start..end].trim())
}

async fn map_with(
	controls: &[Control],
	local: Ipv4Addr,
	port: u16,
) -> Result<Mapping, &'static str> {
	let mut why = "router refused the mapping";
	for control in controls {
		let Ok(answer) = soap(control, "GetExternalIPAddress", "").await else {
			continue;
		};
		let Some(ip) = tag(&answer, "NewExternalIPAddress")
			.and_then(|ip| ip.parse::<Ipv4Addr>().ok())
			.filter(|ip| !ip.is_unspecified())
		else {
			why = "router is not connected";
			continue;
		};
		// The same port outside as inside first, so the router's outgoing translation and the
		// mapping agree; a random one when another device holds it.
		for external in [port, random_port(), random_port()] {
			let arguments = format!(
				"<NewRemoteHost></NewRemoteHost><NewExternalPort>{external}</NewExternalPort>\
				 <NewProtocol>UDP</NewProtocol><NewInternalPort>{port}</NewInternalPort>\
				 <NewInternalClient>{local}</NewInternalClient><NewEnabled>1</NewEnabled>\
				 <NewPortMappingDescription>{DESCRIPTION}</NewPortMappingDescription>\
				 <NewLeaseDuration>{LEASE_SECONDS}</NewLeaseDuration>"
			);
			match soap(control, "AddPortMapping", &arguments).await {
				Ok(_) => {
					return Ok(Mapping {
						control: control.clone(),
						external: SocketAddrV4::new(ip, external),
					});
				}
				Err(CONFLICT) => {}
				// ponytail: routers that only take permanent leases (725) are skipped, so a
				// mapping can never outlive a crash; add a lease-0 retry if the journal shows
				// many of them.
				Err(_) => break,
			}
		}
	}
	Err(why)
}

fn random_port() -> u16 {
	let mut bytes = [0; 2];
	let _ = getrandom::fill(&mut bytes);
	49_152 + u16::from_le_bytes(bytes) % 16_384
}

/// One SOAP call: the answer's body, or the UPnP error code (0 when the router did not say).
async fn soap(control: &Control, action: &str, arguments: &str) -> Result<String, u32> {
	let service = &control.service;
	let body = format!(
		"<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
		 s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
		 <u:{action} xmlns:u=\"{service}\">{arguments}</u:{action}></s:Body></s:Envelope>"
	);
	let request = format!(
		"POST {path} HTTP/1.0\r\nHost: {router}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\n\
		 SOAPAction: \"{service}#{action}\"\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n\
		 {body}",
		path = control.path,
		router = control.router,
		length = body.len(),
	);
	match http(control.router, &request).await {
		Ok((200, answer)) => Ok(answer),
		Ok((_, answer)) => Err(tag(&answer, "errorCode")
			.and_then(|code| code.parse().ok())
			.unwrap_or(0)),
		Err(_) => Err(0),
	}
}

/// One HTTP/1.0 exchange: status and body, de-chunked.
async fn http(router: SocketAddrV4, request: &str) -> Result<(u16, String), &'static str> {
	let exchange = async {
		let mut stream = tokio::net::TcpStream::connect(router).await.ok()?;
		stream.write_all(request.as_bytes()).await.ok()?;
		let mut response = Vec::new();
		(&mut stream)
			.take(MAX_HTTP as u64 + 1)
			.read_to_end(&mut response)
			.await
			.ok()?;
		if response.len() > MAX_HTTP {
			return None;
		}
		let text = String::from_utf8_lossy(&response);
		let (head, body) = text.split_once("\r\n\r\n")?;
		let status = head.split_whitespace().nth(1)?.parse().ok()?;
		let chunked = header(head.as_bytes(), "transfer-encoding")
			.is_some_and(|coding| coding.eq_ignore_ascii_case("chunked"));
		let body = if chunked {
			dechunk(body)?
		} else {
			body.to_owned()
		};
		Some((status, body))
	};
	tokio::time::timeout(HTTP_WAIT, exchange)
		.await
		.ok()
		.flatten()
		.ok_or("router did not answer")
}

fn dechunk(mut body: &str) -> Option<String> {
	let mut whole = String::new();
	loop {
		let (size, rest) = body.split_once("\r\n")?;
		let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
		if size == 0 {
			return Some(whole);
		}
		whole.push_str(rest.get(..size)?);
		body = rest.get(size..)?.strip_prefix("\r\n")?;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::{Arc, Mutex};

	/// A router on loopback that answers each request with `answer(request)` as a full HTTP
	/// response, and keeps every request it got.
	async fn router(
		answer: impl Fn(&str) -> String + Send + Sync + 'static,
	) -> (SocketAddrV4, Arc<Mutex<Vec<String>>>) {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let SocketAddr::V4(at) = listener.local_addr().unwrap() else {
			unreachable!()
		};
		let seen = Arc::new(Mutex::new(Vec::new()));
		let log = seen.clone();
		tokio::spawn(async move {
			loop {
				let Ok((mut stream, _)) = listener.accept().await else {
					return;
				};
				let mut request = Vec::new();
				let mut buffer = [0; 4096];
				// Read the head, then as much body as it announces.
				while let Ok(read) = stream.read(&mut buffer).await {
					if read == 0 {
						break;
					}
					request.extend_from_slice(&buffer[..read]);
					let text = String::from_utf8_lossy(&request);
					if let Some((head, body)) = text.split_once("\r\n\r\n") {
						let length = header(head.as_bytes(), "content-length")
							.and_then(|length| length.parse().ok())
							.unwrap_or(0);
						if body.len() >= length {
							break;
						}
					}
				}
				let request = String::from_utf8_lossy(&request).into_owned();
				let response = answer(&request);
				log.lock().unwrap().push(request);
				let _ = stream.write_all(response.as_bytes()).await;
			}
		});
		(at, seen)
	}

	fn ok(body: &str) -> String {
		format!(
			"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
			body.len()
		)
	}

	#[test]
	fn a_router_maps_a_free_port_and_removes_it() {
		tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap()
			.block_on(async {
				const IP: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
				let (at, seen) = router(|request| {
					if request.starts_with("GET /rootDesc.xml") {
						// Chunked, with the control URL split across two chunks.
						let first = format!(
							"<root><device><serviceList><service><serviceType>\
							 urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>\
							 <controlURL>/ctl/L3F</controlURL></service><service>\
							 <serviceType>{IP}</serviceType><controlURL>/ctl/"
						);
						let second = "IPConn</controlURL></service></serviceList></device></root>";
						return format!(
							"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
							 {:x}\r\n{first}\r\n{:x}\r\n{second}\r\n0\r\n\r\n",
							first.len(),
							second.len()
						);
					}
					if request.contains("#GetExternalIPAddress") {
						return ok("<NewExternalIPAddress>203.0.113.9</NewExternalIPAddress>");
					}
					// The same port outside is taken; any other is free.
					if request.contains("#AddPortMapping")
						&& request.contains("<NewExternalPort>40000<")
					{
						let body = "<s:Fault><detail><UPnPError><errorCode>718</errorCode>\
						            </UPnPError></detail></s:Fault>";
						return format!(
							"HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{body}",
							body.len()
						);
					}
					ok("")
				})
				.await;
				let controls = controls(at, "/rootDesc.xml").await.unwrap();
				assert_eq!(
					controls,
					[Control {
						router: at,
						path: "/ctl/IPConn".into(),
						service: IP.into(),
					}]
				);
				let mapping = map_with(&controls, Ipv4Addr::new(192, 168, 1, 20), 40_000)
					.await
					.unwrap();
				assert_eq!(*mapping.external.ip(), Ipv4Addr::new(203, 0, 113, 9));
				assert!(mapping.external.port() >= 49_152);
				unmap(&mapping).await;
				let seen = seen.lock().unwrap();
				let added = seen
					.iter()
					.rfind(|request| request.contains("#AddPortMapping"))
					.unwrap();
				assert!(added.starts_with("POST /ctl/IPConn HTTP/1.0"));
				assert!(added.contains(&format!("SOAPAction: \"{IP}#AddPortMapping\"")));
				assert!(added.contains("<NewInternalPort>40000<"));
				assert!(added.contains("<NewInternalClient>192.168.1.20<"));
				assert!(added.contains("<NewProtocol>UDP<"));
				let removed = seen.last().unwrap();
				assert!(removed.contains("#DeletePortMapping"));
				assert!(
					removed.contains(&format!("<NewExternalPort>{}<", mapping.external.port()))
				);
			});
	}

	#[test]
	fn search_answers_and_urls_parse_strictly() {
		let answer = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\n\
		              Location: http://192.168.1.1:5000/rootDesc.xml\r\nST: x\r\n\r\n";
		let location = header(answer, "LOCATION").unwrap();
		assert_eq!(
			http_url(location),
			Some(("192.168.1.1:5000".parse().unwrap(), "/rootDesc.xml".into()))
		);
		assert_eq!(
			http_url("HTTP://10.0.0.1"),
			Some(("10.0.0.1:80".parse().unwrap(), "/".into()))
		);
		assert_eq!(http_url("https://192.168.1.1/x"), None);
		assert_eq!(http_url("http://router.local/x"), None);
		assert_eq!(dechunk("3\r\nabc\r\n0\r\n\r\n").as_deref(), Some("abc"));
		assert_eq!(dechunk("9\r\nabc\r\n0\r\n\r\n"), None);
		assert_eq!(tag("<a><b> x </b></a>", "b"), Some("x"));
		assert_eq!(tag("<b>open", "b"), None);
	}
}
