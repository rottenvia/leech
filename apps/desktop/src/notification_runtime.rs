//! Applies notification device choices to filtered messages and explicit call ringing state.
use client_core::{State, auth::AuthState};
use model::{
	Id, PresenceStatus,
	notification_preferences::{Device, Sound},
};
use std::time::{Duration, Instant};

pub enum Alert {
	Message {
		channel: Id,
		title: String,
		body: String,
		avatar_key: String,
		image_path: Option<String>,
	},
}
#[derive(Default)]
pub struct Runtime {
	sounds: crate::notification_sounds::Sounds,
	options: Device,
	was_audible: bool,
	/// What rings, when its cycle began and when the next cue is due.
	ring: Option<((Id, Sound), Instant, Instant)>,
	badge: Option<u32>,
	badge_check: Option<Instant>,
	badge_status: &'static str,
}
impl Runtime {
	fn ring_cue(&mut self, ringing: Option<(Id, Sound)>, now: Instant) -> Option<Sound> {
		if self.ring.map(|(key, ..)| key) != ringing {
			if self.ring.is_some() {
				self.sounds.stop();
			}
			// Dialing waits for the join chime to ring out; an incoming ring starts at once.
			let start = match ringing {
				Some((_, Sound::OutgoingRing)) => {
					now + crate::notification_sounds::OUTGOING_RING_DELAY
				}
				_ => now,
			};
			self.ring = ringing.map(|key| (key, start, start));
		}
		let ((_, cue), _, next) = self.ring.as_mut()?;
		if now < *next {
			return None;
		}
		let interval = if *cue == Sound::OutgoingRing {
			crate::notification_sounds::OUTGOING_RING_INTERVAL
		} else {
			crate::notification_sounds::RING_INTERVAL
		};
		// A fixed cadence from the first cue keeps the call stage's pulse on the beat.
		while *next <= now {
			*next += interval;
		}
		Some(*cue)
	}
	pub fn clear(&mut self, window: &winit::window::Window) {
		self.sounds.stop();
		self.ring = None;
		if self.badge.is_some_and(|count| count > 0) {
			let _ = platform::badge::set(window, 0);
		}
		self.badge = None;
	}
	/// Returns a coalesced desktop alert request. Sound is independent of desktop alerts.
	pub fn poll(
		&mut self,
		state: &mut State,
		ui: &mut ui::MessagingUi,
		window: &winit::window::Window,
		ctx: &eframe::egui::Context,
		fixture: bool,
	) -> Option<Alert> {
		let live = !fixture && !state.demo && state.auth == AuthState::Authenticated;
		let audible = live && ui.own_presence.status != PresenceStatus::DoNotDisturb;
		let options = ui.notification_options;
		let badges = platform::badge::supported() && options.unread_badge;
		if options != self.options || (self.was_audible && !audible) {
			self.sounds.stop();
			self.options = options;
		}
		self.was_audible = audible;
		let focused = ctx.input(|i| {
			i.focused
				&& i.viewport().visible() != Some(false)
				&& i.viewport().minimized != Some(true)
		});
		let mut alert = None;
		let mut sound = None;
		while let Some(notification) = state.take_notification() {
			let current = focused && ui.viewing_latest(notification.channel);
			let cue = if current {
				Sound::CurrentChannel
			} else {
				Sound::Message
			};
			if audible {
				if ui.notifications_enabled && !current {
					let image_path = state.user.as_ref().and_then(|user| {
						crate::avatars::notification_image_path(user.id, &notification.avatar_key)
					});
					alert = Some(Alert::Message {
						channel: notification.channel,
						title: notification.sender,
						body: notification.preview,
						avatar_key: notification.avatar_key,
						image_path,
					});
				}
				if options.allows(cue) {
					sound = Some(cue);
				}
			}
		}
		let incoming = state.voice.incoming.filter(|id| {
			audible && state.notification_allowed(*id) && options.allows(Sound::IncomingRing)
		});
		// Dialing starts once the call connects. The cycle runs even when its sound is off or
		// silent in the demo, so the ringing seat still pulses on the stage.
		let outgoing = state.outgoing_ring().filter(|_| state.call_connected());
		let ringing = incoming
			.map(|id| (id, Sound::IncomingRing))
			.or_else(|| outgoing.map(|id| (id, Sound::OutgoingRing)));
		let now = Instant::now();
		if let Some(cue) = self.ring_cue(ringing, now)
			// Dialing is explicit local feedback, like mute/camera cues; DND only silences alerts.
			&& (cue != Sound::OutgoingRing || (live && options.allows(Sound::OutgoingRing)))
		{
			sound = Some(cue);
		}
		ui.voice_ring_epoch = None;
		if let Some(((_, cue), start, next)) = self.ring {
			ctx.request_repaint_after(next.saturating_duration_since(now));
			if cue == Sound::OutgoingRing {
				let time = ctx.input(|input| input.time);
				ui.voice_ring_epoch = Some(if start > now {
					time + (start - now).as_secs_f64()
				} else {
					time - (now - start).as_secs_f64()
				});
			}
		}
		// Explicit previews are allowed in the offline demo and intentionally ignore automatic mute choices.
		if let Some(preview) = ui.notification_preview.take() {
			sound = Some(preview);
		}
		if let Some(sound) = sound {
			self.sounds.play(sound, options.volume, ctx);
		}
		// Counts only change while frames run, so an idle window needs no badge timer: a
		// throttled frame schedules one follow-up recount, and then the window can sleep.
		let since = self.badge_check.map(|time| time.elapsed());
		if since.is_none_or(|since| since >= Duration::from_secs(1)) || !badges || !live {
			self.badge_check = Some(Instant::now());
			let pings = if live && badges {
				state
					.channels
					.iter()
					.try_fold(0u32, |total, channel| {
						if total >= 100 {
							return None;
						}
						Some(
							total
								.saturating_add(state.mention_count(channel.id))
								.min(100),
						)
					})
					.unwrap_or(100)
			} else {
				0
			};
			if platform::badge::supported() && self.badge != Some(pings) {
				self.badge_status = platform::badge::set(window, pings).err().unwrap_or("");
				self.badge = Some(pings);
			}
		} else if let Some(since) = since {
			ctx.request_repaint_after(Duration::from_secs(1).saturating_sub(since));
		}
		ui.notification_sound_status = if self.sounds.status().is_empty() {
			self.badge_status
		} else {
			self.sounds.status()
		};
		alert
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ringtone_timer_repeats_each_cue_and_stops_on_clear() {
		let mut runtime = Runtime::default();
		let now = Instant::now();
		let outgoing = Some((Id(2), Sound::OutgoingRing));
		let first = now + crate::notification_sounds::OUTGOING_RING_DELAY;
		// Dialing waits for the join chime, then keeps a fixed cadence from its first cue.
		assert_eq!(runtime.ring_cue(outgoing, now), None);
		assert_eq!(runtime.ring_cue(outgoing, first), Some(Sound::OutgoingRing));
		assert_eq!(
			runtime.ring_cue(outgoing, first + Duration::from_secs(2)),
			None
		);
		assert_eq!(
			runtime.ring_cue(outgoing, first + Duration::from_millis(3200)),
			Some(Sound::OutgoingRing)
		);
		assert_eq!(
			runtime.ring_cue(outgoing, first + Duration::from_millis(5900)),
			None
		);
		assert_eq!(
			runtime.ring_cue(outgoing, first + Duration::from_secs(6)),
			Some(Sound::OutgoingRing)
		);
		let incoming = Some((Id(2), Sound::IncomingRing));
		assert_eq!(
			runtime.ring_cue(incoming, now + Duration::from_secs(3)),
			Some(Sound::IncomingRing)
		);
		assert_eq!(
			runtime.ring_cue(incoming, now + Duration::from_secs(6)),
			None
		);
		assert_eq!(
			runtime.ring_cue(incoming, now + Duration::from_secs(9)),
			Some(Sound::IncomingRing)
		);
		assert_eq!(runtime.ring_cue(None, now + Duration::from_secs(9)), None);
		assert!(runtime.ring.is_none());
		assert_eq!(runtime.ring_cue(None, now + Duration::from_secs(15)), None);
	}
}
