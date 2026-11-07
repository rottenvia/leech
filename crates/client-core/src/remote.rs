//! Remote control signalling. Two Leech users in one call trade connection offers through
//! their direct messages: silent messages with a marker that Leech never shows, deleted by
//! their sender as soon as Discord confirms them. Other clients may glimpse one for a moment.
use crate::{Command, Event, State};
use model::Id;

/// Invisible separator and tag in front of the signal itself.
const MARK: &str = "\u{2063}lr1 ";
/// What other clients show for the moment the message exists.
const NOTE: &str = "-# Leech remote control · open Leech to answer ";
/// Bounds a signal's body; offers stay under 400 characters.
const MAX_TOKEN: usize = 600;
/// Signals and deletions waiting for the desktop; anything past this is dropped.
const MAX_QUEUE: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	/// The sender asks to control the recipient's PC.
	Ask,
	/// The sender offers control of its own PC, already set up.
	Give,
	/// Accepts an `Ask` or a `Give`, with the responder's offer.
	Yes,
	/// Declines, cancels or ends before a connection.
	No,
}

impl Kind {
	fn word(self) -> &'static str {
		match self {
			Self::Ask => "ask",
			Self::Give => "give",
			Self::Yes => "yes",
			Self::No => "no",
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signal {
	pub kind: Kind,
	pub session: u64,
	/// The sender's connection offer; empty for `No`.
	pub token: String,
}

impl Signal {
	pub fn encode(&self) -> String {
		format!(
			"@silent {NOTE}{MARK}{} {:x} {}",
			self.kind.word(),
			self.session,
			self.token
		)
	}

	pub fn decode(content: &str) -> Option<Self> {
		let (_, body) = content.split_once(MARK)?;
		let mut words = body.split(' ');
		let kind = match words.next()? {
			"ask" => Kind::Ask,
			"give" => Kind::Give,
			"yes" => Kind::Yes,
			"no" => Kind::No,
			_ => return None,
		};
		let session = u64::from_str_radix(words.next()?, 16).ok()?;
		let token = words.next().unwrap_or_default();
		(words.next().is_none()
			&& token.len() <= MAX_TOKEN
			&& token
				.bytes()
				.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
		.then(|| Self {
			kind,
			session,
			token: token.to_owned(),
		})
	}
}

/// Whether a message is a signal, which no timeline, notification or unread count shows.
pub fn is_signal(content: &str) -> bool {
	content.contains(MARK)
}

/// A signal from someone in this device's call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
	pub from: Id,
	pub signal: Signal,
}

#[derive(Default)]
pub struct Remote {
	received: Vec<Received>,
	/// Signals for users whose direct message is still being opened, and whether the open
	/// was asked for.
	outbox: Vec<(Id, Signal, bool)>,
	/// Nonces of signals this device sent and has not seen confirmed.
	sent: Vec<String>,
	/// This device's own confirmed signals, to delete.
	deletes: Vec<(Id, Id)>,
}

impl State {
	/// Users in this device's connected call, other than this account: the only people remote
	/// control is offered to and accepted from.
	pub fn remote_peers(&self) -> Vec<Id> {
		let own = self.user.as_ref().map(|user| user.id);
		self.voice
			.active
			.as_ref()
			.filter(|call| call.phase.connected())
			.map(|call| {
				call.participants
					.iter()
					.map(|participant| participant.user)
					.filter(|user| Some(*user) != own)
					.collect()
			})
			.unwrap_or_default()
	}

	/// Sends a signal to someone in the call through their direct message, opening it first
	/// (without navigating) when there is none yet.
	pub fn send_remote_signal(&mut self, user: Id, signal: Signal) -> Option<Command> {
		if !self.remote_peers().contains(&user) {
			return None;
		}
		match self.remote_dm(user) {
			Some(channel) => Some(self.remote_send(channel, &signal)),
			None => {
				if self.remote.outbox.len() < MAX_QUEUE {
					self.remote.outbox.push((user, signal, false));
				}
				None
			}
		}
	}

	/// What the desktop sends now: deletions of confirmed signals, and signals whose direct
	/// message has opened. Also hands over the signals received since the last call.
	pub fn take_remote(&mut self) -> (Vec<Received>, Vec<Command>) {
		let mut commands = self
			.remote
			.deletes
			.drain(..)
			.map(|(channel, message)| Command::Delete { channel, message })
			.collect::<Vec<_>>();
		let waiting = std::mem::take(&mut self.remote.outbox);
		for (user, signal, requested) in waiting {
			if let Some(channel) = self.remote_dm(user) {
				commands.push(self.remote_send(channel, &signal));
			} else if !self.remote_peers().contains(&user) {
				// They left the call.
			} else if self.user_action_pending() {
				self.remote.outbox.push((user, signal, requested));
			} else if !requested {
				// Another signal for the same person may have asked already.
				let asked = self
					.remote
					.outbox
					.iter()
					.any(|(to, _, asked)| *to == user && *asked);
				let command = (!asked).then(|| self.request_remote_dm(user)).flatten();
				if asked || command.is_some() {
					commands.extend(command);
					self.remote.outbox.push((user, signal, true));
				}
			}
			// Asked, finished, and still no direct message: the open failed.
		}
		(std::mem::take(&mut self.remote.received), commands)
	}

	fn remote_dm(&self, user: Id) -> Option<Id> {
		self.channels
			.iter()
			.find(|channel| {
				channel.guild.is_none()
					&& channel.kind == 1
					&& channel.recipients.len() == 1
					&& channel.recipients[0].id == user
					&& !channel.recipients[0].webhook
			})
			.map(|channel| channel.id)
	}

	fn remote_send(&mut self, channel: Id, signal: &Signal) -> Command {
		self.send_sequence += 1;
		let epoch = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis();
		let nonce = crate::fingerprint::nonce(epoch, self.send_sequence);
		if self.remote.sent.len() >= MAX_QUEUE {
			self.remote.sent.remove(0);
		}
		self.remote.sent.push(nonce.clone());
		Command::Send {
			sticker: None,
			channel,
			content: signal.encode(),
			nonce,
			reply: None,
		}
	}

	/// Swallows everything about signals before the timeline, notifications or unread counts
	/// see it: received signals queue for the desktop, this device's own are deleted.
	pub(crate) fn intercept_remote(&mut self, event: &mut Event) -> bool {
		match event {
			Event::Message(message) if is_signal(&message.content) => {
				let own = self
					.user
					.as_ref()
					.is_some_and(|user| user.id == message.author.id);
				if !own
					&& self.remote_dm(message.author.id) == Some(message.channel)
					&& self.remote_peers().contains(&message.author.id)
					&& self.remote.received.len() < MAX_QUEUE
					&& let Some(signal) = Signal::decode(&message.content)
				{
					self.remote.received.push(Received {
						from: message.author.id,
						signal,
					});
				}
				true
			}
			Event::SendResult { nonce, result } => {
				let Some(index) = self.remote.sent.iter().position(|sent| sent == nonce) else {
					return false;
				};
				self.remote.sent.remove(index);
				if let Ok(message) = result
					&& self.remote.deletes.len() < MAX_QUEUE
				{
					self.remote.deletes.push((message.channel, message.id));
				}
				true
			}
			Event::History { messages, .. } => {
				messages.retain(|message| !is_signal(&message.content));
				false
			}
			Event::Patch(patch) => {
				matches!(&patch.content, model::Patch::Value(content) if is_signal(content))
			}
			_ => false,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn signals_round_trip_and_reject_noise() {
		let signal = Signal {
			kind: Kind::Ask,
			session: 0xdead_beef_0042,
			token: "AbC-_09".into(),
		};
		let text = signal.encode();
		assert!(text.starts_with("@silent "));
		assert!(is_signal(&text));
		assert_eq!(Signal::decode(&text), Some(signal));
		let no = Signal {
			kind: Kind::No,
			session: 7,
			token: String::new(),
		};
		assert_eq!(Signal::decode(&no.encode()), Some(no));
		for bad in [
			"hello",
			"\u{2063}lr1 take 1 x",
			"\u{2063}lr1 ask zz x",
			"\u{2063}lr1 ask 1 x y",
			"\u{2063}lr1 ask 1 <script>",
		] {
			assert_eq!(Signal::decode(bad), None, "{bad}");
		}
		let long = format!("\u{2063}lr1 ask 1 {}", "a".repeat(MAX_TOKEN + 1));
		assert_eq!(Signal::decode(&long), None);
	}
}
