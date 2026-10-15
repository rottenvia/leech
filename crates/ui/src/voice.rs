use crate::channel_marks::{self, Emphasis};
use crate::design::LazyHover;
use crate::{MessagingUi, design};
use client_core::{
	Command, State,
	auth::AuthState,
	voice::{Participant, Phase, RosterEntry},
};
use egui::RichText;
use model::{
	Id,
	server_admin::VoiceChange,
	voice_settings::{InputProfile, NoiseSuppression},
};

/// Local mutes share the 64 per-user volume slots sent to the mixer.
const MAX_USER_MUTES: usize = 64;

pub(super) struct VoiceDrag(pub Id, pub Id, pub Id);

pub(super) struct CallSwitch {
	from: (Id, u64),
	channel: Id,
	ring: bool,
	generation: u64,
	audio: Option<(bool, bool)>,
}

impl MessagingUi {
	pub(super) fn exit_voice_fullscreen(&mut self) {
		if let Some((_, ctx, previous, focus)) = self.voice_fullscreen.take() {
			self.voice_fullscreen_request = Some(previous);
			ctx.memory_mut(|memory| memory.request_focus(focus));
			ctx.request_repaint();
		}
	}

	pub(super) fn is_voice_fullscreen(&self) -> bool {
		self.voice_fullscreen.is_some()
	}

	/// The fullscreen share's call channel, while it is still the tile the stage would show.
	fn voice_fullscreen_channel(&self, state: &State) -> Option<Id> {
		let (focus, _, _, _) = self.voice_fullscreen.as_ref()?;
		state
			.voice
			.active
			.as_ref()
			.filter(|call| match focus {
				StageFocus::LocalScreen => {
					self.screen.context == Some((state.generation, call.channel, call.request))
						&& self.screen.busy
						&& self.screen.preview.is_some()
				}
				StageFocus::Stream(user) => call.watching == Some(*user),
				StageFocus::Participant(_) => false,
			})
			.map(|call| call.channel)
	}

	pub(super) fn show_fullscreen_voice(&mut self, ctx: &egui::Context, state: &mut State) -> bool {
		let Some((focus, _, _, _)) = self.voice_fullscreen else {
			return false;
		};
		let Some(channel) = self.voice_fullscreen_channel(state) else {
			self.exit_voice_fullscreen();
			return false;
		};
		let screen = ctx.content_rect();
		let id = egui::Id::unique("voice-fullscreen");
		let mut exit =
			ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
		let overlay = egui::Modal::new(id)
			.area(
				egui::Modal::default_area(id)
					.anchor(egui::Align2::LEFT_TOP, egui::Vec2::ZERO)
					.fade_in(false),
			)
			.backdrop_color(egui::Color32::BLACK)
			.frame(egui::Frame::NONE)
			.show(ctx, |ui| {
				ui.set_min_size(screen.size());
				let rect = egui::Rect::from_min_size(screen.min, screen.size());
				ui.painter().rect_filled(rect, 0, STAGE_FILL);
				match focus {
					StageFocus::LocalScreen => self.screen_tile(ui, rect, false),
					StageFocus::Stream(user) => {
						self.stream_tile(ui, state, rect, channel, user, true)
					}
					StageFocus::Participant(_) => unreachable!("validated screen share"),
				}
				let button = ui.put(
					egui::Rect::from_min_size(
						rect.right_top() + egui::vec2(-52.0, 12.0),
						egui::Vec2::splat(40.0),
					),
					egui::Button::new("")
						.fill(egui::Color32::from_black_alpha(180))
						.corner_radius(8),
				);
				crate::icons::paint(
					ui.painter(),
					crate::icons::Icon::Close,
					button.rect.shrink(9.0),
					egui::Color32::WHITE,
				);
				button.widget_info(|| {
					egui::WidgetInfo::labeled(egui::Role::Button, true, "Exit fullscreen")
				});
				exit |= button.on_hover_text("Exit fullscreen (Esc)").clicked();
			});
		// "Stop watching" inside the fullscreen tile applies now, not after leaving fullscreen.
		self.apply_watch_request(state);
		exit |= overlay.should_close() || self.voice_fullscreen_channel(state).is_none();
		if exit {
			self.exit_voice_fullscreen();
		}
		true
	}

	/// Effective screen-share audio level, independent of participant voice levels.
	pub fn voice_stream_volume(&self) -> u16 {
		if self.voice_stream_muted {
			0
		} else {
			self.voice_stream_volume.unwrap_or(100).min(200)
		}
	}

	/// Fixed session overrides; zero IDs are unused slots. A locally muted speaker is mixed at
	/// zero gain, so unmuting restores the volume chosen for them.
	pub fn voice_user_volumes(&self) -> [(u64, u16); 64] {
		let mut values = self
			.voice_user_volumes
			.as_deref()
			.copied()
			.unwrap_or([(0, 100); 64]);
		for user in self.voice_user_muted.iter().copied() {
			if let Some(slot) = values.iter_mut().find(|(id, _)| *id == user) {
				slot.1 = 0;
			} else if let Some(index) = values.iter().position(|(id, _)| *id == 0).or_else(|| {
				values
					.iter()
					.rposition(|(id, _)| !self.voice_user_muted.contains(id))
			}) {
				values[index] = (user, 0);
			}
		}
		values
	}

	/// Locally muted speakers, for persistence to device settings.
	pub fn voice_user_mutes(&self) -> &[u64] {
		&self.voice_user_muted
	}

	/// Restore persisted local mutes, e.g. at startup.
	pub fn set_voice_user_mutes(&mut self, values: &[u64]) {
		self.voice_user_muted = values
			.iter()
			.copied()
			.filter(|user| *user != 0)
			.take(MAX_USER_MUTES)
			.collect();
	}

	pub(super) fn voice_user_locally_muted(&self, user: Id) -> bool {
		self.voice_user_muted.contains(&user.0)
	}

	pub(super) fn set_voice_user_locally_muted(&mut self, user: Id, muted: bool) {
		self.voice_user_muted.retain(|id| *id != user.0);
		if muted && self.voice_user_muted.len() < MAX_USER_MUTES {
			self.voice_user_muted.push(user.0);
		}
	}

	/// Non-default volume overrides, for persistence to device settings.
	pub fn voice_user_volume_overrides(&self) -> Vec<(u64, u16)> {
		self.voice_user_volumes
			.as_deref()
			.into_iter()
			.flatten()
			.filter(|(id, volume)| *id != 0 && *volume != 100)
			.copied()
			.collect()
	}

	/// Restore persisted per-user volume overrides, e.g. at startup.
	pub fn set_voice_user_volume_overrides(&mut self, values: &[(u64, u16)]) {
		if values.is_empty() {
			self.voice_user_volumes = None;
			return;
		}
		let mut array = [(0u64, 100u16); 64];
		for (slot, value) in array.iter_mut().zip(values.iter().take(64)) {
			*slot = *value;
		}
		self.voice_user_volumes = Some(Box::new(array));
	}

	pub(super) fn set_voice_user_volume(&mut self, user: Id, volume: u16) {
		if volume == 100 {
			if let Some(slot) = self
				.voice_user_volumes
				.as_deref_mut()
				.and_then(|values| values.iter_mut().find(|(id, _)| *id == user.0))
			{
				*slot = (0, 100);
			}
			return;
		}
		let values = self
			.voice_user_volumes
			.get_or_insert_with(|| Box::new([(0, 100); 64]));
		let index = values
			.iter()
			.position(|(id, _)| *id == user.0)
			.or_else(|| values.iter().position(|(id, _)| *id == 0))
			.unwrap_or_else(|| {
				values.rotate_left(1);
				63
			});
		values[index] = (user.0, volume);
	}

	fn voice_participant_menu(
		&mut self,
		response: &egui::Response,
		state: &State,
		entry: &RosterEntry,
	) {
		crate::user_menu::popup(response, egui::Popup::default_response_id(response))
			.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
			.show(|ui| {
				ui.set_width(220.0);
				let id = entry.participant.user.0;
				if let Some(call) = state
					.voice
					.active
					.as_ref()
					.filter(|call| call.channel == entry.channel && call.guild.is_none())
					&& state.user.as_ref().is_some_and(|own| own.id.0 != id)
					&& !call.participants.iter().any(|p| p.user.0 == id)
				{
					let stop = state
						.voice
						.ringing(call.channel)
						.contains(&entry.participant.user);
					let enabled = state.can_ring_recipient(entry.participant.user, stop);
					if ui
						.add_enabled(
							enabled,
							egui::Button::new(crate::i18n::translate(if stop {
								"voice-recipient-stop-ringing"
							} else {
								"voice-recipient-ring"
							})),
						)
						.clicked()
					{
						self.ring_request =
							Some((call.channel, call.request, entry.participant.user, stop));
						ui.close();
					}
					ui.separator();
				}
				if state.user.as_ref().is_some_and(|own| own.id.0 != id)
					&& !inactive_recipient(state, entry.channel, entry.participant.user)
				{
					let muted = self.voice_user_locally_muted(entry.participant.user);
					if ui
						.button(crate::i18n::translate_if_key(
							&(if muted {
								crate::i18n::translate("voice-voice-participant-menu-unmute")
							} else {
								crate::i18n::translate("voice-voice-participant-menu-mute")
							}),
						))
						.on_hover_text(crate::i18n::translate(
							"voice-voice-participant-menu-silence-this-person-on-this-device-only-nobody-else-is",
						))
						.clicked()
					{
						self.set_voice_user_locally_muted(entry.participant.user, !muted);
					}
					let mut volume = self
						.voice_user_volumes
						.as_deref()
						.and_then(|values| values.iter().find(|(user, _)| *user == id))
						.map_or(100, |(_, volume)| *volume);
					let changed =
						gain_slider(ui, &mut volume, "voice-voice-participant-menu-user-volume")
							.changed();
					let reset = ui
						.add_enabled(
							volume != 100,
							egui::Button::new(crate::i18n::translate(
								"voice-voice-participant-menu-reset-volume",
							)),
						)
						.clicked();
					if changed || reset {
						self.set_voice_user_volume(
							entry.participant.user,
							if reset { 100 } else { volume },
						);
					}
					ui.separator();
				}
				let (guild, channel, user) = (entry.guild, entry.channel, entry.participant.user);
				let can_move =
					state.can_moderate_voice(guild, channel, user, VoiceChange::Move(None));
				let muted = entry.participant.server_muted;
				let can_mute =
					state.can_moderate_voice(guild, channel, user, VoiceChange::Mute(!muted));
				if can_move || can_mute {
					let enabled = !state.server_admin.pending
						&& (state.demo
							|| (state.gateway_connected && state.auth == AuthState::Authenticated));
					ui.add_enabled_ui(enabled, |ui| {
						if can_mute
							&& ui
								.button(crate::i18n::translate(if muted {
									"voice-server-unmute"
								} else {
									"voice-server-mute"
								}))
								.clicked()
						{
							self.voice_moderation =
								Some((guild, channel, user, VoiceChange::Mute(!muted)));
							ui.close();
						}
						if can_move {
							ui.menu_button(crate::i18n::translate("voice-move-to"), |ui| {
								egui::ScrollArea::vertical()
									.max_height(240.0)
									.show(ui, |ui| {
										for target in state.channels.iter().filter(|target| {
											target.guild == Some(guild)
												&& target.kind == 2 && target.id != channel
										}) {
											let change = VoiceChange::Move(Some(target.id));
											if state.can_view(target.id)
												&& state.permission(
													target.id,
													model::permissions::CONNECT,
												) == Some(true) && ui.button(&target.name).clicked()
											{
												self.voice_moderation =
													Some((guild, channel, user, change));
												ui.close();
											}
										}
									});
							});
							if ui
								.button(crate::i18n::translate("voice-disconnect-member"))
								.clicked()
							{
								self.voice_moderation =
									Some((guild, channel, user, VoiceChange::Move(None)));
								ui.close();
							}
						}
					});
					ui.separator();
				}
				if let Some(user) = resolve_member(state, entry).0 {
					crate::user_menu::contents(
						ui,
						state,
						user,
						&mut self.profile,
						&mut self.user_action,
						None,
					);
				}
			});
	}

	fn is_speaking(&self, state: &State, channel: Id, participant: &Participant) -> bool {
		!inactive_recipient(state, channel, participant.user)
			&& !self.voice_user_locally_muted(participant.user)
			&& !participant.muted
			&& !participant.deafened
			&& !participant.server_muted
			&& !participant.server_deafened
			&& state.voice.active.as_ref().is_some_and(|call| {
				call.channel == channel
					&& matches!(call.phase, Phase::Connected | Phase::Waiting)
					&& !call.deafened
					&& !call.server_deafened
			}) && self.voice_speaking.contains(&participant.user)
	}

	pub(super) fn voice_channel_button(
		&mut self,
		ui: &mut egui::Ui,
		state: &State,
		channel: &model::Channel,
		selected: bool,
		draggable: bool,
	) -> egui::Response {
		let colors = design::palette(ui);
		let call = state
			.voice
			.active
			.as_ref()
			.filter(|c| c.channel == channel.id);
		let connected = call.is_some_and(|c| matches!(c.phase, Phase::Waiting | Phase::Connected));
		let elapsed = call.and_then(elapsed_label);
		let access = state.channel_access(channel.id);
		let viewable = !access.hidden();
		let (rect, response) = ui
			.push_id(channel.id, |ui| {
				ui.allocate_exact_size(
					egui::vec2(ui.available_width(), 34.0),
					if viewable {
						if draggable {
							egui::Sense::click_and_drag()
						} else {
							egui::Sense::click()
						}
					} else {
						egui::Sense::hover()
					},
				)
			})
			.inner;
		let row = rect.shrink2(egui::vec2(0.0, 1.0));
		let hovered = viewable && (response.hovered() || response.has_focus());
		let hover = if viewable {
			design::hover_motion(ui, &response)
		} else {
			0.0
		};
		if selected {
			ui.painter().rect_filled(row, 8, colors.selected);
		} else if hover > 0.0 {
			ui.painter().rect_filled(
				row,
				8,
				crate::design::row_highlight(ui, colors.hover, hover),
			);
		}
		let text_color = channel_marks::tint(
			&colors,
			access,
			if !viewable {
				Emphasis::Unavailable
			} else if connected {
				Emphasis::Connected
			} else if selected || hovered {
				Emphasis::Focused
			} else {
				Emphasis::Idle
			},
		);
		let glyph = egui::Rect::from_center_size(
			row.left_center() + egui::vec2(18.0, 0.0),
			egui::Vec2::splat(20.0),
		);
		channel_marks::paint_glyph(
			ui.painter(),
			access,
			crate::icons::Icon::Speaker,
			glyph,
			text_color,
		);
		channel_marks::paint(ui.painter(), access, row, glyph, text_color);
		let marks = channel_marks::trailing(access);
		let elapsed_width = if elapsed.is_some() { 64.0 } else { 0.0 };
		let name = ui.painter().layout(
			channel.name.clone(),
			egui::FontId::new(15.0, design::medium_family(ui.ctx())),
			text_color,
			(row.width() - 40.0 - elapsed_width - marks).max(10.0),
		);
		let name_rect = egui::Rect::from_min_size(
			egui::pos2(row.left() + 34.0, row.center().y - name.size().y * 0.5),
			egui::vec2(row.width() - 40.0 - elapsed_width - marks, name.size().y),
		);
		ui.painter()
			.with_clip_rect(name_rect)
			.galley(name_rect.min, name, text_color);
		if let Some(elapsed) = &elapsed {
			ui.painter().text(
				row.right_center() - egui::vec2(8.0 + marks, 0.0),
				egui::Align2::RIGHT_CENTER,
				elapsed,
				egui::FontId::monospace(11.0),
				text_color,
			);
		}
		if elapsed.is_some() && ui.is_rect_visible(response.rect) {
			ui.ctx()
				.request_repaint_after(std::time::Duration::from_secs(1));
		}
		response.widget_info(|| {
			egui::WidgetInfo::selected(
				egui::Role::Button,
				viewable,
				selected,
				format!(
					"{} {}{}{}",
					channel.name,
					crate::i18n::translate("voice-voice-channel-button-voice-channel"),
					channel_marks::label(access),
					if connected {
						crate::i18n::translate("voice-voice-channel-button-connected")
					} else {
						String::new()
					}
				),
			)
		});
		response.on_hover_text_with(|| {
			format!(
				"{} · View voice channel{}{}",
				channel.name,
				channel_marks::label(access),
				if connected { " · Connected" } else { "" }
			)
		})
	}

	pub(super) fn voice_participant(
		&mut self,
		ui: &mut egui::Ui,
		state: &State,
		entry: &RosterEntry,
	) {
		if !state.can_view(entry.channel) {
			return;
		}
		let colors = design::palette(ui);
		let (user, name) = resolve_member(state, entry);
		ui.push_id(
			("voice-participant", entry.channel, entry.participant.user),
			|ui| {
				let draggable = !state.server_admin.pending
					&& (state.demo
						|| (state.gateway_connected && state.auth == AuthState::Authenticated))
					&& state.can_moderate_voice(
						entry.guild,
						entry.channel,
						entry.participant.user,
						VoiceChange::Move(None),
					);
				let (rect, row) = ui.allocate_exact_size(
					egui::vec2(ui.available_width(), 34.0),
					if draggable {
						egui::Sense::click_and_drag()
					} else {
						egui::Sense::click()
					},
				);
				if draggable && row.drag_started_by(egui::PointerButton::Primary) {
					row.dnd_set_drag_payload(VoiceDrag(
						entry.guild,
						entry.channel,
						entry.participant.user,
					));
				}
				row.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
				let hovered = row.contains_pointer() || row.has_focus();
				let hover = design::hover_motion(ui, &row);
				if hover > 0.0 {
					ui.painter().rect_filled(
						rect.shrink2(egui::vec2(0.0, 1.0)),
						8,
						crate::design::row_highlight(ui, colors.hover, hover),
					);
				}
				let name_color = if hovered {
					colors.text_strong
				} else {
					colors.muted
				};
				let mut inner = ui.new_child(
					egui::UiBuilder::new()
						.max_rect(rect)
						.layout(egui::Layout::left_to_right(egui::Align::Center)),
				);
				inner.spacing_mut().item_spacing.x = 8.0;
				inner.add_space(5.0);
				// The name label carries the row's only tooltip.
				let avatar = match user {
					Some(user) => self.avatars.show_quiet(&mut inner, user, 24.0, state.demo),
					None => {
						let (r, response) = inner
							.allocate_exact_size(egui::Vec2::splat(24.0), egui::Sense::hover());
						design::paint_avatar(&inner, name, 24.0, r);
						response
					}
				};
				if self.is_speaking(state, entry.channel, &entry.participant) {
					speaking_avatar(&inner, &avatar, name);
				}
				let locally_muted = self.voice_user_locally_muted(entry.participant.user);
				let shown = shown_participant(state, entry);
				inner.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
					// Deafening implies muted; Discord shows only the headphones then.
					if shown.deafened || shown.server_deafened {
						status_icon(
							ui,
							crate::icons::Icon::HeadphonesSlash,
							colors.muted,
							if shown.server_deafened {
								"voice-voice-participant-deafened-by-server"
							} else {
								"voice-voice-participant-deafened"
							},
						);
					} else if shown.muted || shown.server_muted {
						status_icon(
							ui,
							crate::icons::Icon::MicrophoneSlash,
							colors.muted,
							if shown.server_muted {
								"voice-voice-participant-muted-by-server"
							} else {
								"voice-voice-participant-microphone-muted"
							},
						);
					}
					if locally_muted {
						status_icon(
							ui,
							crate::icons::Icon::Speaker,
							colors.danger,
							"voice-voice-participant-muted-for-you-on-this-device",
						);
					}
					if self.live_shown(ui.ctx(), state, entry) > 0.0 {
						live_badge(ui);
					}
					ui.allocate_ui_with_layout(
						egui::vec2(ui.available_width(), 28.0),
						egui::Layout::left_to_right(egui::Align::Center),
						|ui| {
							ui.add(
								egui::Label::new(RichText::new(name).color(name_color))
									.truncate()
									.show_tooltip_when_elided(false)
									.selectable(false),
							)
							.on_hover_text(name);
						},
					);
				});
				self.voice_participant_menu(&row, state, entry);
				if entry.participant.streaming {
					self.stream_preview_popup(ui, &row, state, entry);
				}
				if let Some(user) = user {
					self.profile.person_click(ui, &row, None, user);
				}
			},
		);
	}

	fn stream_preview_popup(
		&mut self,
		ui: &mut egui::Ui,
		row: &egui::Response,
		state: &State,
		entry: &RosterEntry,
	) {
		let target = (entry.guild, entry.channel, entry.participant.user);
		// The gap clears the sidebar's own edge, so the card floats beside the column
		// instead of sitting on its border.
		let mut tooltip = egui::Tooltip::for_enabled(row)
			.gap(16.0)
			.width(324.0)
			.accessible_name("Stream preview");
		tooltip.popup = tooltip.popup.align(egui::RectAlign::RIGHT_START).frame(
			egui::Frame::popup(&row.ctx.style_of(row.ctx.theme()))
				.fill(design::palette(ui).raised)
				.stroke(design::hairline(ui))
				.inner_margin(12)
				.corner_radius(12),
		);
		let open = tooltip.popup.is_open();
		if open {
			let frame = ui.ctx().cumulative_frame_nr();
			if !self
				.stream_preview_open
				.is_some_and(|(previous, last_frame)| {
					previous == target && frame <= last_frame.saturating_add(1)
				}) {
				self.stream_preview_request = Some(target);
			}
			self.stream_preview_open = Some((target, frame));
		}
		let colors = design::palette(ui);
		tooltip.show(|ui| {
			ui.set_width(300.0);
			ui.spacing_mut().item_spacing.y = 10.0;
			ui.horizontal(|ui| {
				ui.label(design::semibold(ui, "Streaming Now", 15.0).color(colors.text_strong));
				live_badge(ui);
			});
			let url = state
				.voice
				.preview
				.as_ref()
				.filter(|preview| (preview.guild, preview.channel, preview.user) == target)
				.and_then(|preview| preview.url.clone())
				.or_else(|| {
					state.demo.then(|| {
						format!(
							"https://cdn.discordapp.com/streams/guild:{}:{}:{}/0123456789abcdef.png",
							entry.guild, entry.channel, entry.participant.user
						)
					})
				});
			if let Some(url) = url {
				self.avatars.show_media(
					ui,
					&model::EmbedMedia {
						url: Some(url),
						width: 512,
						height: 288,
						..Default::default()
					},
					egui::vec2(300.0, 169.0),
					state.demo,
					crate::avatars::Surface::Banner,
				);
			} else {
				let (rect, _) =
					ui.allocate_exact_size(egui::vec2(300.0, 169.0), egui::Sense::hover());
				ui.painter().rect_filled(rect, 8, colors.canvas);
				let preview = state
					.voice
					.preview
					.as_ref()
					.filter(|preview| (preview.guild, preview.channel, preview.user) == target);
				if preview.is_some_and(|preview| preview.loading) {
					ui.put(
						egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(24.0)),
						egui::Spinner::new().color(colors.muted),
					);
				} else {
					ui.painter().text(
						rect.center(),
						egui::Align2::CENTER_CENTER,
						preview
							.and_then(|preview| preview.error)
							.unwrap_or("Preview hidden or unavailable"),
						egui::FontId::proportional(13.0),
						colors.muted,
					);
				}
			}
			let watching = state.voice.active.as_ref().is_some_and(|call| {
				call.channel == entry.channel && call.watching == Some(entry.participant.user)
			});
			let connected = state.voice.active.as_ref().is_some_and(|call| {
				call.channel == entry.channel
					&& matches!(call.phase, Phase::Connected | Phase::Waiting)
			});
			let can_watch = !watching
				&& (connected || (state.voice.active.is_none() && state.can_call(entry.channel)))
				&& state
					.user
					.as_ref()
					.is_none_or(|user| user.id != entry.participant.user);
			let button = ui
				.add_enabled_ui(can_watch, |ui| {
					design::positive_icon_button(
						ui,
						crate::icons::Icon::ScreenShare,
						if watching { "Watching" } else { "Watch Stream" },
					)
				})
				.inner
				.on_disabled_hover_text(if watching {
					"Already watching this stream"
				} else {
					"Join this voice channel before watching"
				});
			if button.clicked() {
				self.stream_preview_watch = Some((entry.channel, entry.participant.user));
				ui.close();
			}
		});
		if !open
			&& self
				.stream_preview_open
				.is_some_and(|(previous, _)| previous == target)
		{
			self.stream_preview_open = None;
		}
	}

	pub(super) fn apply_stream_preview_watch(
		&mut self,
		state: &mut State,
		commands: &mut Vec<Command>,
	) {
		let Some((channel, user)) = self.stream_preview_watch else {
			return;
		};
		// DM calls have no guild roster entry; their live shares come from the call itself.
		let live = state.voice.roster.iter().any(|entry| {
			entry.channel == channel
				&& entry.participant.user == user
				&& entry.participant.streaming
		}) || state.voice.active.as_ref().is_some_and(|call| {
			call.channel == channel
				&& call.guild.is_none()
				&& matches!(call.phase, Phase::Connected | Phase::Waiting)
				&& call
					.participants
					.iter()
					.any(|participant| participant.user == user && participant.streaming)
		});
		if !live {
			self.stream_preview_watch = None;
			state.status = "This stream is no longer live";
			return;
		}
		// Watching a stream elsewhere joins its channel first and watches once connected. An
		// active call is never left silently: the switch is offered once, and watching after
		// it is a new click.
		match state.voice.active.as_ref().map(|call| call.channel) {
			Some(active) if active == channel => {
				if state.call_connected() {
					self.stream_preview_watch = None;
					if state.watch_stream(user) {
						self.voice_focus = Some(StageFocus::Stream(user));
					} else {
						state.status =
							"This stream is not available in the connected voice channel";
					}
				}
			}
			active => {
				if self
					.request_call_audio(state, channel, false, None, commands)
					.is_err()
				{
					self.stream_preview_watch = None;
					state.status = "Could not join this stream's voice channel";
				} else if active.is_some() {
					self.stream_preview_watch = None;
				}
			}
		}
	}

	/// Guild voice channel: Discord-style black stage with participant tiles and, when
	/// connected, the call control bar; otherwise a Join Voice button.
	pub(super) fn voice_channel(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		channel: Id,
		commands: &mut Vec<Command>,
	) {
		let connected = state
			.voice
			.active
			.as_ref()
			.is_some_and(|call| call.channel == channel);
		let stage = ui.available_rect_before_wrap();
		ui.painter().rect_filled(stage, 0, STAGE_FILL);
		let (rect, _) = ui.allocate_exact_size(stage.size(), egui::Sense::hover());
		let notices = self.stage_notices(state, channel, connected);
		let bottom = CONTROL_HEIGHT + 2.0 * STAGE_MARGIN;
		// In the call, the bar hides only over an enlarged video, which fills the stage; there
		// it fades in on hover.
		let reveal = if connected {
			chrome_reveal(ui, rect, "voice-stage-chrome")
		} else {
			1.0
		};
		let full = connected && self.voice_focus.is_some() && !self.voice_focus_participants;
		// An enlarged video keeps a slim margin: a rounded tile pressed against the sidebar
		// and the member list reads as cut off, not as full screen.
		let body = if full {
			rect.shrink(TILE_GAP)
		} else {
			egui::Rect::from_min_max(
				rect.left_top() + egui::vec2(STAGE_MARGIN, STAGE_MARGIN),
				egui::pos2(rect.right() - STAGE_MARGIN, rect.bottom() - bottom),
			)
		};
		let mut body_ui = ui.new_child(
			egui::UiBuilder::new()
				.max_rect(body)
				.layout(egui::Layout::top_down(egui::Align::Min)),
		);
		// A short stage clips notices and tiles instead of drawing them over the call bar.
		body_ui.set_clip_rect(body.intersect(ui.clip_rect()));
		stage_notices(&mut body_ui, &notices);
		if !state.can_view(channel) {
			body_ui.label(
				RichText::new(crate::i18n::translate(
					"voice-voice-channel-participant-list-unavailable-with-the-current-access",
				))
				.color(STAGE_MUTED),
			);
		} else {
			let entries = stage_participants(state, channel);
			if entries.is_empty() {
				body_ui.add_space((body_ui.available_height() * 0.4).max(0.0));
				body_ui.vertical_centered(|ui| {
					ui.label(
						design::semibold(
							ui,
							crate::i18n::translate_if_key(
								if !state.offline_preview() && !state.gateway_connected {
									"voice-voice-channel-participant-list-unavailable-while-disconnected"
								} else {
									"voice-voice-channel-no-one-s-here-yet"
								},
							),
							18.0,
						)
						.color(STAGE_TEXT),
					);
				});
			} else {
				if !state.offline_preview() && !state.gateway_connected {
					body_ui.label(
						RichText::new(crate::i18n::translate(
							"voice-voice-channel-last-known-participants-a-reconnect-to-refresh",
						))
						.small()
						.color(STAGE_MUTED),
					);
				}
				self.voice_chrome_hidden = 1.0 - reveal;
				self.participant_tiles(&mut body_ui, state, channel, &entries, false);
				self.voice_chrome_hidden = 0.0;
			}
		}
		self.apply_watch_request(state);
		let bar = egui::Rect::from_min_max(
			egui::pos2(rect.left(), rect.bottom() - bottom),
			rect.right_bottom(),
		);
		if full {
			chrome_shade(ui, rect, 96.0, false, reveal);
		}
		let mut bar_ui = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(
			egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
		));
		if connected {
			// The bar has its own strip under the tiles, so it only hides over an enlarged
			// video; an empty strip that fills on hover reads as a missing control.
			bar_ui.set_opacity(if full { reveal } else { 1.0 });
			self.call_controls(&mut bar_ui, state, channel);
		} else {
			bar_ui.horizontal_centered(|ui| {
				ui.add_space((ui.available_width() - 160.0).max(0.0) * 0.5);
				self.call_button(ui, state, channel, commands);
			});
		}
	}

	/// Which stage tiles a channel would show, in stage order.
	fn stage_tiles<'a>(
		&self,
		state: &State,
		channel: Id,
		entries: &'a [RosterEntry],
	) -> Vec<Tile<'a>> {
		let call = state
			.voice
			.active
			.as_ref()
			.filter(|call| call.channel == channel);
		let mut tiles = Vec::with_capacity(entries.len() + 2);
		if call.is_some_and(|call| {
			self.screen.context == Some((state.generation, channel, call.request))
				&& self.screen.busy
				&& self.screen.preview.is_some()
		}) {
			tiles.push(Tile::LocalScreen);
		}
		// Like Discord, every remote share gets its own box beside its streamer's tile; the
		// watched one leads, the rest stay previews until someone clicks to watch.
		let watching = call.and_then(|call| call.watching);
		if let Some(streamer) = watching {
			tiles.push(Tile::Stream(streamer));
		}
		let own = state.user.as_ref().map(|user| user.id);
		tiles.extend(
			entries
				.iter()
				.map(|entry| entry.participant.user)
				.filter(|user| Some(*user) != watching && Some(*user) != own)
				.filter(|user| {
					entries
						.iter()
						.any(|entry| entry.participant.user == *user && entry.participant.streaming)
				})
				.map(Tile::Stream),
		);
		tiles.extend(entries.iter().map(Tile::Participant));
		// Show Non-Video Participants off: while anyone shows video or a share, people who
		// show neither leave the stage.
		let shows = |tile: &Tile<'_>| {
			!matches!(tile, Tile::Participant(_)) || self.tile_has_video(state, channel, tile)
		};
		if self.call_hide_non_video && tiles.iter().any(shows) {
			tiles.retain(shows);
		}
		tiles
	}

	/// True once any stage tile carries video or a live share, so the direct-message stage
	/// can grow.
	pub(super) fn stage_shows_video(&self, state: &State, channel: Id) -> bool {
		let entries = stage_participants(state, channel);
		self.stage_tiles(state, channel, &entries)
			.iter()
			.any(|tile| {
				matches!(tile, Tile::Stream(_)) || self.tile_has_video(state, channel, tile)
			})
	}

	/// Stage tiles: a best-fit grid, or one enlarged video with the rest in a strip below.
	///
	/// `dm` drops the tile plates while no one shares video, matching Discord's
	/// direct-message calls where idle participants are avatars on the stage.
	fn participant_tiles(
		&mut self,
		ui: &mut egui::Ui,
		state: &State,
		channel: Id,
		entries: &[RosterEntry],
		dm: bool,
	) {
		let tiles = self.stage_tiles(state, channel, entries);
		let changed = self.seat_roster(ui.ctx(), channel, entries);
		self.departed_seats(ui, state, channel);
		if tiles.is_empty() {
			return;
		}
		// Focus survives only while that tile still shows video; Escape restores the grid.
		let focus = self.voice_focus.filter(|focus| {
			tiles
				.iter()
				.any(|tile| tile.focus() == *focus && self.tile_has_video(state, channel, tile))
		});
		// Grid View keeps every tile the same size, even while watching a share.
		let focus = focus
			.filter(|_| !self.call_grid && !ui.input(|input| input.key_pressed(egui::Key::Escape)));
		self.voice_focus = focus;
		// A live share turns the avatar-only DM stage into boxed tiles, as in Discord.
		let video = tiles.iter().any(|tile| {
			matches!(tile, Tile::Stream(_) | Tile::LocalScreen)
				|| self.tile_has_video(state, channel, tile)
		});
		let frameless = dm && !video && !state.is_group_dm(channel);
		let area = ui.available_rect_before_wrap();
		if area.width() < 40.0 || area.height() < 40.0 {
			return;
		}
		let mut toggle = None;
		if let Some(focus) = focus {
			let index = tiles
				.iter()
				.position(|tile| tile.focus() == focus)
				.expect("validated focus");
			// The enlarged tile keeps the stage; everyone else becomes a small strip below it,
			// only when the pill-bar toggle asks for them.
			// A short stage drops the strip instead of squeezing the enlarged tile.
			let strip = if tiles.len() > 1
				&& self.voice_focus_participants
				&& area.height() >= MIN_STRIP + TILE_GAP + MIN_FOCUS
			{
				(area.height() * 0.18).clamp(MIN_STRIP, 124.0)
			} else {
				0.0
			};
			let main = egui::Rect::from_min_size(
				area.min,
				egui::vec2(
					area.width(),
					area.height() - strip - if strip > 0.0 { TILE_GAP } else { 0.0 },
				),
			);
			toggle = self.tile(ui, state, channel, &tiles[index], main, false, true);
			if strip > 0.0 {
				// Narrow stages show as many square-or-wider tiles as fit, never clipped ones.
				let fits = ((area.width() + TILE_GAP) / (strip + TILE_GAP)).floor() as usize;
				let shown = (tiles.len() - 1).min(fits.max(1));
				let width = ((area.width() - TILE_GAP * (shown as f32 - 1.0)) / shown as f32)
					.min(strip * 16.0 / 9.0);
				let size = egui::vec2(width, strip);
				let row = size.x * shown as f32 + TILE_GAP * (shown as f32 - 1.0);
				let mut x = area.center().x - row * 0.5;
				let top = main.bottom() + TILE_GAP;
				for (_, tile) in tiles
					.iter()
					.enumerate()
					.filter(|(position, _)| *position != index)
					.take(shown)
				{
					let rect = egui::Rect::from_min_size(egui::pos2(x, top), size);
					x += size.x + TILE_GAP;
					if let Some(focus) = self.tile(ui, state, channel, tile, rect, false, false) {
						toggle = Some(focus);
					}
				}
			}
		} else {
			// Pick the column count that makes the tiles largest inside the stage, so two
			// participants fill the width instead of sitting in a corner. Frameless DM avatars
			// instead keep Discord's tight spacing and shrink to fit a short panel.
			let (columns, size) = if frameless {
				frameless_fit(tiles.len(), area.size())
			} else {
				// The cap only stops one or two tiles from swallowing a very large window; a
				// lower one left wide bands of empty stage on big monitors.
				best_fit(tiles.len(), area.size(), 960.0)
			};
			if size.x < 24.0 || size.y < 24.0 {
				return;
			}
			let rows = tiles.len().div_ceil(columns);
			let content = size.y * rows as f32 + TILE_GAP * (rows as f32 - 1.0);
			let top = area.top() + ((area.height() - content) * 0.5).max(0.0);
			for row in 0..rows {
				let first = row * columns;
				let in_row = (tiles.len() - first).min(columns);
				let width = size.x * in_row as f32 + TILE_GAP * (in_row as f32 - 1.0);
				let mut x = area.center().x - width * 0.5;
				let y = top + (size.y + TILE_GAP) * row as f32;
				for tile in &tiles[first..first + in_row] {
					let rect = egui::Rect::from_min_size(egui::pos2(x, y), size);
					x += size.x + TILE_GAP;
					let rect = self.seat_glide(ui.ctx(), tile, channel, rect, changed);
					if let Some(focus) = self.tile(ui, state, channel, tile, rect, frameless, false)
					{
						toggle = Some(focus);
					}
				}
			}
		}
		if let Some(focus) = toggle {
			// Enlarging a tile leaves Grid View, as in Discord.
			self.call_grid = false;
			self.voice_focus = (self.voice_focus != Some(focus)).then_some(focus);
		}
	}

	fn watching(&self, state: &State, channel: Id, streamer: Id) -> bool {
		state
			.voice
			.active
			.as_ref()
			.is_some_and(|call| call.channel == channel && call.watching == Some(streamer))
	}

	fn remote_texture(&self, user: Id) -> Option<&egui::load::SizedTexture> {
		self.voice_remote_video
			.iter()
			.find(|(id, _)| *id == user)
			.map(|(_, texture)| texture)
	}

	fn tile_has_video(&self, state: &State, channel: Id, tile: &Tile<'_>) -> bool {
		match tile {
			Tile::LocalScreen => self.screen.preview.is_some(),
			Tile::Stream(streamer) => self.watching(state, channel, *streamer),
			Tile::Participant(entry) => {
				let own = state
					.user
					.as_ref()
					.is_some_and(|user| user.id == entry.participant.user);
				if own {
					!self.call_hide_own_camera
						&& self.voice_camera_preview.is_some()
						&& state
							.voice
							.active
							.as_ref()
							.is_some_and(|call| call.channel == channel && call.camera)
				} else {
					entry.participant.video && self.remote_texture(entry.participant.user).is_some()
				}
			}
		}
	}

	/// One stage tile. Returns its focus key when a video tile is clicked to enlarge or restore.
	#[allow(clippy::too_many_arguments)] // Placement and framing flags of one tile.
	fn tile(
		&mut self,
		ui: &mut egui::Ui,
		state: &State,
		channel: Id,
		tile: &Tile<'_>,
		rect: egui::Rect,
		frameless: bool,
		focused: bool,
	) -> Option<StageFocus> {
		let has_video = self.tile_has_video(state, channel, tile);
		let response = ui.interact(
			rect,
			ui.scope_id().with(("voice-tile", tile.key())),
			if has_video || matches!(tile, Tile::Participant(_) | Tile::Stream(_)) {
				egui::Sense::click()
			} else {
				egui::Sense::hover()
			},
		);
		if !ui.is_rect_visible(rect) {
			return None;
		}
		let compact = rect.height() < 132.0;
		let hint = match tile {
			Tile::LocalScreen => {
				self.screen_tile(ui, rect, compact);
				self.screen
					.capture_status
					.unwrap_or("Your screen · local preview")
			}
			Tile::Stream(streamer) if has_video => {
				self.stream_tile(ui, state, rect, channel, *streamer, false);
				egui::Popup::context_menu(&response)
					.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
					.show(|ui| {
						self.stream_audio_controls(ui);
						ui.separator();
						if ui.button("Stop watching").clicked() {
							self.watch_request = Some(None);
							ui.close();
						}
					});
				"Screen share you are watching"
			}
			Tile::Stream(streamer) => {
				let watch = self.stream_invite_tile(ui, state, rect, channel, *streamer, compact);
				let can_watch = self.can_watch(state, channel, *streamer);
				response.widget_info(|| {
					egui::WidgetInfo::labeled(egui::Role::Button, can_watch, "Watch Stream")
				});
				if can_watch && (watch || response.clicked()) {
					// In the call, watch directly (DM calls have no guild roster entry);
					// otherwise join the channel first, like the hover preview's button.
					if state
						.voice
						.active
						.as_ref()
						.is_some_and(|call| call.channel == channel)
					{
						self.watch_request = Some(Some(*streamer));
					} else {
						self.stream_preview_watch = Some((channel, *streamer));
					}
				}
				if can_watch {
					response.on_hover_cursor(egui::CursorIcon::PointingHand);
					return None;
				}
				"Join this voice channel before watching"
			}
			Tile::Participant(entry) => {
				self.participant_tile(ui, state, entry, rect, frameless, compact);
				self.voice_participant_menu(&response, state, entry);
				""
			}
		};
		if has_video {
			let label = if focused {
				"Click or press Escape to return to the grid"
			} else {
				"Click to enlarge"
			};
			response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
			let hover = if hint.is_empty() {
				label.to_owned()
			} else {
				format!("{hint} · {label}")
			};
			if response.clicked() {
				return Some(tile.focus());
			}
			response.on_hover_text(hover);
		} else if !hint.is_empty() {
			response.on_hover_text(hint);
		}
		None
	}

	fn screen_tile(&self, ui: &mut egui::Ui, rect: egui::Rect, compact: bool) {
		let content = self
			.screen
			.preview
			.as_ref()
			.map(|texture| {
				let content = fit_rect(rect, texture.size_vec2());
				ui.put(
					content,
					egui::Image::from_texture((texture.id(), content.size())).corner_radius(10),
				);
				content
			})
			.unwrap_or(rect);
		if !compact {
			name_badge(ui, content, "Your screen", None, self.badge_opacity());
		}
	}

	/// The screen share this device chose to watch: the latest decoded picture or a status.
	fn stream_tile(
		&mut self,
		ui: &mut egui::Ui,
		state: &State,
		rect: egui::Rect,
		channel: Id,
		streamer: Id,
		overlay: bool,
	) {
		let name = participant_user(state, channel, streamer)
			.map_or_else(|| "Participant".to_owned(), |user| user.name.clone());
		let content = match &self.voice_stream_view {
			Some(texture) => {
				let content = fit_rect(rect, texture.size);
				ui.put(
					content,
					egui::Image::from_texture((texture.id, content.size())).corner_radius(10),
				);
				content
			}
			None => {
				ui.painter()
					.rect(rect, 10, TILE_FILL, STAGE_EDGE, egui::StrokeKind::Inside);
				if rect.height() >= 132.0 {
					let status = if self.voice_stream_status.is_empty() {
						"Connecting to the stream…"
					} else {
						self.voice_stream_status
					};
					let spinner = egui::Rect::from_center_size(
						rect.center() - egui::vec2(0.0, 18.0),
						egui::Vec2::splat(24.0),
					);
					ui.put(spinner, egui::Spinner::new().color(STAGE_MUTED));
					ui.painter().text(
						rect.center() + egui::vec2(0.0, 18.0),
						egui::Align2::CENTER_CENTER,
						status,
						egui::FontId::proportional(13.0),
						STAGE_MUTED,
					);
				}
				rect
			}
		};
		// Even a small strip-sized share names whose screen it is.
		if content.height() >= 72.0 {
			name_badge(
				ui,
				content,
				&format!("{name}'s screen"),
				None,
				self.badge_opacity(),
			);
		}
		// Fullscreen has no call bar, so it keeps its own small audio and stop actions.
		if overlay {
			self.stream_overlay_controls(ui, content);
		}
	}

	/// Bottom-right actions over a fullscreen share: stream audio and Stop watching.
	fn stream_overlay_controls(&mut self, ui: &mut egui::Ui, content: egui::Rect) {
		let height = 40.0;
		let stop_label = "Stop watching";
		let font = egui::FontId::new(13.0, design::medium_family(ui.ctx()));
		let galley = ui
			.painter()
			.layout_no_wrap(stop_label.to_owned(), font, egui::Color32::WHITE);
		let stop = egui::Rect::from_min_size(
			content.right_bottom() - egui::vec2(16.0 + galley.size().x + 24.0, 16.0 + height),
			egui::vec2(galley.size().x + 24.0, height),
		);
		let audio = egui::Rect::from_min_size(
			stop.left_top() - egui::vec2(8.0 + height, 0.0),
			egui::Vec2::splat(height),
		);
		let danger = design::palette(ui).danger;
		let response = ui.allocate_rect(audio, egui::Sense::click());
		ui.painter().rect_filled(
			audio,
			8,
			egui::Color32::from_black_alpha(if response.hovered() { 220 } else { 170 }),
		);
		crate::icons::paint(
			ui.painter(),
			crate::icons::Icon::Speaker,
			audio.shrink(10.0),
			if self.voice_stream_volume() == 0 {
				danger
			} else {
				egui::Color32::WHITE
			},
		);
		response
			.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Stream audio"));
		egui::Popup::menu(&response)
			.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
			.show(|ui| self.stream_audio_controls(ui));
		let response = ui.allocate_rect(stop, egui::Sense::click());
		ui.painter().rect_filled(
			stop,
			8,
			if response.hovered() {
				danger
			} else {
				egui::Color32::from_black_alpha(170)
			},
		);
		ui.painter().galley(
			stop.center() - galley.size() * 0.5,
			galley,
			egui::Color32::WHITE,
		);
		response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, stop_label));
		if response
			.on_hover_text("Stop receiving this screen share")
			.clicked()
		{
			self.watch_request = Some(None);
		}
	}

	/// Whether a click on a live share's box may start watching it (joining if needed).
	fn can_watch(&self, state: &State, channel: Id, streamer: Id) -> bool {
		let connected = state.voice.active.as_ref().is_some_and(|call| {
			call.channel == channel && matches!(call.phase, Phase::Connected | Phase::Waiting)
		});
		(connected || (state.voice.active.is_none() && state.can_call(channel)))
			&& state.user.as_ref().is_none_or(|user| user.id != streamer)
	}

	/// A live share nobody here watches yet: a dark box with a LIVE pill,
	/// the streamer's name and a Watch Stream button. Returns true when the box is clicked.
	fn stream_invite_tile(
		&mut self,
		ui: &mut egui::Ui,
		state: &State,
		rect: egui::Rect,
		channel: Id,
		streamer: Id,
		compact: bool,
	) -> bool {
		let colors = design::palette(ui);
		ui.painter()
			.rect(rect, 10, PILL_FILL, STAGE_EDGE, egui::StrokeKind::Inside);
		let name = participant_user(state, channel, streamer)
			.map_or("Participant", |user| user.name.as_str());
		let (pill, font, margin) = if compact {
			(egui::vec2(30.0, 15.0), 9.0, 5.0)
		} else {
			(egui::vec2(40.0, 20.0), 11.0, 8.0)
		};
		let live = egui::Rect::from_min_size(rect.left_top() + egui::Vec2::splat(margin), pill);
		ui.painter().rect_filled(live, 4, colors.danger);
		ui.painter().text(
			live.center(),
			egui::Align2::CENTER_CENTER,
			"LIVE",
			egui::FontId::new(font, design::medium_family(ui.ctx())),
			egui::Color32::WHITE,
		);
		let enabled = self.can_watch(state, channel, streamer);
		if !compact {
			self.watch_stream_button(ui, rect, name, enabled);
		}
		// One click target over the whole box, button included.
		let response = ui.interact(
			rect,
			ui.scope_id().with(("voice-watch-stream", streamer)),
			if enabled {
				egui::Sense::click()
			} else {
				egui::Sense::hover()
			},
		);
		enabled && response.clicked()
	}

	fn watch_stream_button(&self, ui: &mut egui::Ui, rect: egui::Rect, name: &str, enabled: bool) {
		let opacity = self.badge_opacity();
		name_badge(ui, rect, &format!("{name}'s screen"), None, opacity);
		let font = egui::FontId::new(14.0, design::medium_family(ui.ctx()));
		let label = "Watch Stream";
		let galley = ui
			.painter()
			.layout_no_wrap(label.to_owned(), font, egui::Color32::WHITE);
		let icon = 18.0;
		let size = egui::vec2(galley.size().x + icon + 8.0 + 32.0, 36.0);
		let button = egui::Rect::from_center_size(rect.center(), size);
		if !rect.contains_rect(button) {
			return;
		}
		let fill = if !enabled {
			egui::Color32::from_white_alpha(24)
		} else if ui.rect_contains_pointer(button) {
			egui::Color32::from_white_alpha(56)
		} else {
			egui::Color32::from_white_alpha(36)
		};
		ui.painter().rect_filled(button, 8, fill);
		let text = if enabled {
			egui::Color32::WHITE
		} else {
			STAGE_MUTED
		};
		let left = button.center().x - (galley.size().x + icon + 8.0) * 0.5;
		crate::icons::paint(
			ui.painter(),
			crate::icons::Icon::ScreenShare,
			egui::Rect::from_min_size(
				egui::pos2(left, button.center().y - icon * 0.5),
				egui::Vec2::splat(icon),
			),
			text,
		);
		ui.painter().galley(
			egui::pos2(left + icon + 8.0, button.center().y - galley.size().y * 0.5),
			galley,
			text,
		);
	}

	fn stream_audio_controls(&mut self, ui: &mut egui::Ui) {
		ui.set_width(220.0);
		design::checkbox(
			ui,
			&mut self.voice_stream_muted,
			crate::i18n::translate("voice-stream-audio-controls-mute-stream-audio"),
		);
		gain_slider(
			ui,
			self.voice_stream_volume.get_or_insert(100),
			"voice-stream-audio-controls-stream-volume",
		);
	}

	/// Apply a tile's watch click once the stage has mutable state again.
	pub(super) fn apply_watch_request(&mut self, state: &mut State) {
		match self.watch_request.take() {
			// Like Discord, a share opens enlarged as soon as you choose to watch it.
			Some(Some(user)) => {
				if state.watch_stream(user) {
					self.voice_focus = Some(StageFocus::Stream(user));
				}
			}
			Some(None) => state.stop_watching(),
			None => {}
		}
	}
	pub(super) fn apply_ring_request(&mut self, state: &mut State) {
		if let Some((channel, request, recipient, stop)) = self.ring_request.take()
			&& state
				.voice
				.active
				.as_ref()
				.is_some_and(|call| (call.channel, call.request) == (channel, request))
			&& state.ring_recipient(recipient, stop)
		{
			state.voice.ring_error = None;
		}
	}

	fn participant_tile(
		&mut self,
		ui: &mut egui::Ui,
		state: &State,
		entry: &RosterEntry,
		rect: egui::Rect,
		frameless: bool,
		compact: bool,
	) {
		let size = rect.size();
		let (user, name) = resolve_member(state, entry);
		let colors = design::palette(ui);
		let own = state
			.user
			.as_ref()
			.is_some_and(|user| user.id == entry.participant.user);
		let call = state
			.voice
			.active
			.as_ref()
			.filter(|call| call.channel == entry.channel);
		let inactive = inactive_recipient(state, entry.channel, entry.participant.user);
		// The local preview is mirrored like a webcam; remote cameras fill the tile edge to edge.
		let video = if own && call.is_some_and(|call| call.camera) {
			// Hidden from this device only when its options say so; others still see it.
			self.voice_camera_preview
				.as_ref()
				.filter(|_| !self.call_hide_own_camera)
				.as_ref()
				.map(|texture| (texture.id(), texture.size_vec2(), true))
		} else if entry.participant.video {
			self.remote_texture(entry.participant.user)
				.map(|texture| (texture.id, texture.size, false))
		} else {
			None
		};
		if !frameless {
			ui.painter()
				.rect(rect, 10, TILE_FILL, STAGE_EDGE, egui::StrokeKind::Inside);
		}
		if let Some((id, image, mirror)) = video {
			cover_image(ui, rect, id, image, mirror);
		}
		let speaking = self.is_speaking(state, entry.channel, &entry.participant);
		let avatar_size = tile_avatar(size, frameless);
		let ringing = inactive
			&& state
				.voice
				.ringing(entry.channel)
				.contains(&entry.participant.user);
		let offset = if compact || frameless { 0.0 } else { 10.0 };
		// A seat waits, smaller and faded, while its own call first connects (a comet circles
		// inside the avatar edge) or while it is only being rung (it beats with each ring). It
		// arrives in time with the join chime: the own comet closes on the first note, the
		// avatar springs to full size on the second and a soft halo spreads around it. Once
		// connected the own seat stays put: re-securing media when someone joins, or a
		// reconnect, never circles again.
		let joining = own
			&& video.is_none()
			&& call.is_some_and(|call| !call.phase.connected() && call.connected_at.is_none());
		let waiting = joining || inactive;
		let avatar_rect = egui::Rect::from_center_size(
			rect.center() - egui::vec2(0.0, offset),
			egui::Vec2::splat(avatar_size),
		);
		let motion = self.seat_motion(ui.ctx(), entry, waiting, avatar_rect);
		let glow = self.call_glow(ui);
		let rest = if own { 0.55 } else { 0.45 };
		let ripples = self
			.voice_ring_epoch
			.filter(|_| ringing && video.is_none())
			.map(|epoch| ring_ripples(ui.ctx(), ui.input(|input| input.time) - epoch))
			.unwrap_or_default();
		let (scale, opacity) = if let Some(elapsed) = motion.arrived {
			let lit = ((elapsed - CUE_LATENCY) / 0.18).clamp(0.0, 1.0);
			(
				arrival_scale(elapsed),
				rest + (1.0 - rest) * egui::emath::easing::cubic_out(lit),
			)
		} else if waiting {
			let beat: f32 = ripples
				.iter()
				.flatten()
				.map(|ripple| 0.02 * (-9.0 * ripple).exp())
				.sum();
			(WAITING_SCALE + beat, rest)
		} else {
			(1.0, 1.0)
		};
		let mut avatar_ui = ui.new_child(egui::UiBuilder::new().max_rect(
			egui::Rect::from_center_size(avatar_rect.center(), avatar_rect.size() * scale),
		));
		avatar_ui.set_opacity(if video.is_some() {
			0.0
		} else {
			opacity * motion.appear
		});
		let halo = self.halo_painter(ui);
		for ripple in ripples.into_iter().flatten() {
			self::halo(&halo, avatar_rect, glow, ripple, 0.5);
		}
		let avatar_size = avatar_size * scale;
		let avatar = if let Some(user) = user {
			self.avatars
				.show(&mut avatar_ui, user, avatar_size, state.demo)
		} else {
			design::avatar(&mut avatar_ui, name, avatar_size)
		};
		// Mute state reads as Discord's red badge with the matching slashed glyph.
		let shown = shown_participant(state, entry);
		let silenced = if inactive {
			None
		} else if shown.deafened || shown.server_deafened {
			Some(crate::icons::Icon::HeadphonesSlash)
		} else if shown.muted || shown.server_muted {
			Some(crate::icons::Icon::MicrophoneSlash)
		} else if self.voice_user_locally_muted(entry.participant.user) {
			// Silenced on this device only; the speaker glyph separates it from a microphone mute.
			Some(crate::icons::Icon::Speaker)
		} else {
			None
		};
		if let Some(icon) = silenced
			&& video.is_none()
		{
			// The badge scales down with small avatars so it never hides the face.
			let radius = (avatar_size * 0.2).clamp(7.0, 12.0);
			let badge = avatar.rect.right_bottom() - egui::Vec2::splat(avatar_size * 0.14);
			ui.painter().circle_filled(badge, radius, STAGE_FILL);
			ui.painter()
				.circle_filled(badge, radius - 2.0, colors.danger);
			crate::icons::paint(
				ui.painter(),
				icon,
				egui::Rect::from_center_size(badge, egui::Vec2::splat(radius)),
				egui::Color32::WHITE,
			);
		}
		if video.is_none() {
			if joining {
				connecting_comet(ui, avatar.rect, glow, 0.0, JOIN_RING);
			} else if let Some(elapsed) = motion.arrived {
				arrival_rings(ui, &halo, avatar.rect, glow, elapsed, own);
			}
		}
		if silenced.is_none() && speaking {
			if video.is_none() && frameless {
				speaking_avatar(ui, &avatar, name);
			}
			if !frameless {
				ui.painter().rect_stroke(
					rect.shrink(1.0),
					8,
					egui::Stroke::new(2.0, colors.positive),
					egui::StrokeKind::Inside,
				);
			}
		}
		self.voice_participant_menu(&avatar, state, entry);
		if inactive {
			let label = crate::i18n::translate(if ringing {
				"voice-recipient-ringing"
			} else {
				"voice-recipient-not-in-call"
			});
			let status = format!("{name} · {label}");
			avatar.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, &status));
			avatar.clone().on_hover_text(status);
			// Plates keep a small corner status; the frameless DM stage relies on the ring.
			if !frameless && !compact {
				ui.put(
					egui::Rect::from_min_size(
						rect.left_top() + egui::vec2(8.0, 6.0),
						egui::vec2((size.x - 16.0).max(16.0), 18.0),
					),
					egui::Label::new(RichText::new(label).size(11.0).color(STAGE_MUTED)).truncate(),
				);
			}
		}
		if let Some(user) = user {
			self.profile.person_click(ui, &avatar, None, user);
		}
		// Discord's LIVE pill marks a streamer on every tile size; strip tiles get a small one
		// so it never covers the avatar, and a bare DM avatar wears it on its lower edge, cut
		// out of the stage like the mute badge, so it never covers the face. It pops in as it
		// appears.
		let reveal = self.live_shown(ui.ctx(), state, entry);
		if reveal > 0.0 {
			let (pill, font) = if compact {
				(egui::vec2(30.0, 15.0), 9.0)
			} else if frameless {
				(egui::vec2(34.0, 16.0), 10.0)
			} else {
				(egui::vec2(40.0, 20.0), 11.0)
			};
			let margin = if compact { 5.0 } else { 8.0 };
			let center = if frameless && video.is_none() {
				egui::pos2(avatar.rect.center().x, avatar.rect.bottom() - 2.0)
			} else {
				rect.left_top() + egui::Vec2::splat(margin) + pill * 0.5
			};
			let live = egui::Rect::from_center_size(center, pill * (0.8 + 0.2 * reveal));
			if frameless && video.is_none() {
				ui.painter()
					.rect_filled(live.expand(2.0), 6, STAGE_FILL.gamma_multiply(reveal));
			}
			ui.painter()
				.rect_filled(live, 4, colors.danger.gamma_multiply(reveal));
			ui.painter().text(
				live.center(),
				egui::Align2::CENTER_CENTER,
				"LIVE",
				egui::FontId::new(font * (0.8 + 0.2 * reveal), design::medium_family(ui.ctx())),
				egui::Color32::WHITE.gamma_multiply(reveal),
			);
		}
		if compact {
			return;
		}
		// Plates carry a bottom-left name badge; frameless DM avatars name the person on hover,
		// as in Discord, so nothing sits between the avatars and the bar.
		if frameless {
			if !inactive {
				avatar.on_hover_text(name);
			}
		} else {
			name_badge(
				ui,
				rect,
				name,
				silenced.filter(|_| video.is_some()),
				self.badge_opacity(),
			);
		}
	}

	fn stage_notices(&self, state: &State, channel: Id, connected: bool) -> Vec<(String, bool)> {
		let mut notices = Vec::new();
		if let Some(call) = state.voice.active.as_ref().filter(|c| c.channel == channel) {
			if let Some((_, error)) = state.voice.ring_error.filter(|(id, _)| *id == channel) {
				notices.push((error.into(), true));
			}
			// Screen-share status stays in the share dialog and the button's tooltip: as a stage
			// notice it pushed the DM avatars into a smaller band (Discord shows none).
			if !self.voice_camera_status.is_empty() {
				notices.push((self.voice_camera_status.into(), false));
			}
			if call.watching.is_none() && !self.voice_stream_status.is_empty() {
				notices.push((self.voice_stream_status.into(), false));
			}
			if call.server_deafened {
				notices.push(("Deafened by the server".into(), false));
			} else if call.server_muted {
				notices.push(("Muted by the server".into(), false));
			}
			if !state.offline_preview() && !state.can_speak(channel) {
				notices.push((
					"Speaking is unavailable in this channel. You can still listen.".into(),
					false,
				));
			} else if !state.offline_preview()
				&& state.permission(channel, model::permissions::USE_VAD) != Some(true)
			{
				notices.push((
					"Push-to-talk is required to speak here. Enable it in Voice settings.".into(),
					false,
				));
			}
		} else if !connected {
			if state.offline_preview() {
				notices.push((
					"Synthetic participants · microphone and speakers are off.".into(),
					false,
				));
			} else if let Some(reason) = self.call_unavailable(state, channel) {
				notices.push((reason.to_owned(), false));
			}
		}
		notices
	}

	pub(crate) fn call_unavailable(&self, state: &State, channel: Id) -> Option<&'static str> {
		if state.offline_preview() {
			Some("Calls are unavailable in the offline preview. No microphone is accessed.")
		} else if !self.voice_available {
			Some("Voice is unavailable in this session.")
		} else if state.auth != AuthState::Authenticated || !state.gateway_connected {
			Some("Reconnect to Discord before calling.")
		} else if state
			.voice
			.active
			.as_ref()
			.is_some_and(|call| call.channel == channel)
		{
			Some("You are already in this call.")
		} else if !state.can_call(channel) {
			Some("Joining this channel is unavailable with current permission information.")
		} else {
			None
		}
	}

	/// Device-free membership check with a synthetic connected DM call.
	#[cfg(all(debug_assertions, feature = "demo"))]
	pub fn debug_call_membership_check(mut state: State) {
		state.demo = false;
		let call = state.voice.active.as_ref().unwrap();
		let channel = call.channel;
		let own = state.user.as_ref().unwrap().id;
		let peer = call
			.participants
			.iter()
			.find(|p| p.user != own)
			.unwrap()
			.user;
		assert!(
			stage_participants(&state, channel)
				.iter()
				.any(|p| p.participant.user == peer)
		);
		state.apply_voice(client_core::voice::Event::State {
			guild: None,
			member: None,
			server_muted: false,
			server_deafened: false,
			channel: None,
			user: peer,
			ours: false,
			session: None,
			muted: false,
			deafened: false,
			video: false,
			streaming: false,
		});
		assert!(
			!stage_participants(&state, channel)
				.iter()
				.any(|p| p.participant.user == peer)
		);
		for ringing in [vec![peer], vec![]] {
			let expected = !ringing.is_empty();
			state.apply_voice(client_core::voice::Event::Call {
				channel,
				ringing: Some(ringing),
				participants: None,
				unavailable: false,
			});
			assert_eq!(
				stage_participants(&state, channel)
					.iter()
					.any(|p| p.participant.user == peer),
				expected
			);
		}
	}

	pub(crate) fn request_call(
		&mut self,
		state: &mut State,
		channel: Id,
		ring: bool,
		commands: &mut Vec<Command>,
	) {
		let _ = self.request_call_audio(state, channel, ring, None, commands);
	}

	pub(crate) fn request_call_with_audio(
		&mut self,
		state: &mut State,
		channel: Id,
		ring: bool,
		muted: bool,
		deafened: bool,
		commands: &mut Vec<Command>,
	) -> Result<(), String> {
		self.request_call_audio(state, channel, ring, Some((muted, deafened)), commands)
	}

	fn request_call_audio(
		&mut self,
		state: &mut State,
		channel: Id,
		ring: bool,
		audio: Option<(bool, bool)>,
		commands: &mut Vec<Command>,
	) -> Result<(), String> {
		if let Some(reason) = self.call_unavailable(state, channel) {
			return Err(reason.into());
		}
		let _ = commands;
		if let Some(call) = &state.voice.active {
			self.voice_switch = Some(CallSwitch {
				from: (call.channel, call.request),
				channel,
				ring,
				generation: state.generation,
				audio,
			});
			return Ok(());
		}
		self.join_call(state, channel, ring, audio)
	}

	/// Joins at once; the call controller leaves a current call first.
	fn join_call(
		&mut self,
		state: &mut State,
		channel: Id,
		ring: bool,
		audio: Option<(bool, bool)>,
	) -> Result<(), String> {
		let (muted, deafened) = audio.unwrap_or((self.voice_muted, self.voice_deafened));
		if !state.start_call_with_mute(channel, ring, muted, deafened) {
			return Err("Joining this call is no longer available".into());
		}
		if audio.is_some() {
			self.voice_muted = muted;
			self.voice_deafened = deafened;
		}
		Ok(())
	}

	/// Leaves the current call and joins the chosen one; the controller does both in order.
	fn confirm_call_switch(&mut self, state: &mut State) {
		let Some(switch) = self.voice_switch.take() else {
			return;
		};
		if self
			.join_call(state, switch.channel, switch.ring, switch.audio)
			.is_err()
		{
			state.status = "Joining this call is no longer available";
		}
	}

	pub(super) fn show_call_switch(&mut self, ctx: &egui::Context, state: &mut State) {
		let Some(switch) = &self.voice_switch else {
			return;
		};
		if switch.generation != state.generation
			|| !self.voice_available
			|| !state.can_call(switch.channel)
		{
			self.voice_switch = None;
			return;
		}
		if state
			.voice
			.active
			.as_ref()
			.map(|call| (call.channel, call.request))
			!= Some(switch.from)
		{
			self.voice_switch = None;
			return;
		}
		let name = state
			.channel(switch.channel)
			.map_or("the selected channel", |channel| channel.name.as_str());
		match crate::dialog::Confirm::new(
			"switch-call",
			"Switch calls?",
			format!("You are already in another call. Leave it and join {name}?"),
		)
		.confirm_label("Switch call")
		.cancel_label("Stay in call")
		.show(ctx)
		{
			Some(crate::dialog::Choice::Confirmed) => self.confirm_call_switch(state),
			Some(crate::dialog::Choice::Cancelled) => self.voice_switch = None,
			None => {}
		}
	}

	pub(super) fn call_button(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		channel: Id,
		commands: &mut Vec<Command>,
	) -> egui::Response {
		let unavailable = self.call_unavailable(state, channel);
		let incoming = state.voice.incoming == Some(channel);
		let guild = state
			.channels
			.iter()
			.any(|c| c.id == channel && c.kind == 2);
		let label = if guild {
			"Join Voice"
		} else if incoming {
			"Answer call"
		} else if state.voice.has_dm_call(channel) {
			"Join call"
		} else {
			"Start voice call"
		};
		let hint = unavailable.unwrap_or(if state.can_speak(channel) {
			"Join audio. Your microphone starts after the call is secured."
		} else {
			"Join to listen. Speaking is unavailable in this channel."
		});
		let label = crate::i18n::translate_if_key(label);
		let hint = crate::i18n::translate_if_key(hint);
		// Guild channels keep Discord's green Join Voice button; DM headers use an icon.
		let response = if guild {
			let colors = design::palette(ui);
			ui.add_enabled(
				unavailable.is_none(),
				egui::Button::new(design::medium(ui, &label, 15.0).color(egui::Color32::WHITE))
					.fill(colors.positive)
					.stroke(egui::Stroke::NONE)
					.corner_radius(8)
					.min_size(egui::vec2(160.0, 40.0)),
			)
		} else {
			ui.add_enabled_ui(unavailable.is_none(), |ui| {
				crate::icons::button(ui, crate::icons::Icon::Phone, 32.0, &label)
			})
			.inner
		}
		.on_hover_text(&hint)
		.on_disabled_hover_text(hint);
		if response.clicked() {
			self.request_call(state, channel, !guild && !incoming, commands);
		}
		response
	}

	/// Discord's video call button beside the voice call in a DM header; the camera starts
	/// once the call connects.
	pub(super) fn video_call_button(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		channel: Id,
		commands: &mut Vec<Command>,
	) {
		let unavailable = self
			.call_unavailable(state, channel)
			.or_else(|| (!self.voice_camera_available).then_some("No camera is available."));
		let existing = state.voice.incoming == Some(channel) || state.voice.has_dm_call(channel);
		let response = ui
			.add_enabled_ui(unavailable.is_none(), |ui| {
				crate::icons::button(
					ui,
					crate::icons::Icon::Video,
					32.0,
					if existing {
						"Join with video"
					} else {
						"Start video call"
					},
				)
			})
			.inner
			.on_disabled_hover_text(unavailable.unwrap_or(""));
		if response.clicked() {
			self.voice_camera_on_join = Some(channel);
			self.request_call(state, channel, !existing, commands);
		}
	}

	/// Input or output half of Discord's voice popout, matching the chevron that opened it.
	fn voice_settings_popup(
		&mut self,
		trigger: &egui::Response,
		demo: bool,
		active: bool,
		input: bool,
	) {
		let id = trigger.id.with("voice-settings-open");
		let mut open = trigger
			.ctx
			.data_mut(|data| *data.get_temp_mut_or_default::<bool>(id));

		if trigger.clicked() {
			open = !open;
		}
		// Device dropdowns use egui's popup memory; keep the parent independently open.
		let close_behavior = if egui::Popup::is_any_open(&trigger.ctx) {
			egui::PopupCloseBehavior::IgnoreClicks
		} else {
			egui::PopupCloseBehavior::CloseOnClickOutside
		};
		egui::Popup::menu(trigger)
			.open_bool(&mut open)
			.style(|_: &mut egui::Style| {})
			.width(340.0)
			.close_behavior(close_behavior)
			.frame(
				egui::Frame::popup(&trigger.ctx.style_of(trigger.ctx.theme()))
					.inner_margin(16)
					.corner_radius(12),
			)
			.show(|ui| {
				ui.set_width(308.0);
				ui.spacing_mut().item_spacing.y = 10.0;
				ui.label(design::semibold(
					ui,
					crate::i18n::translate_if_key(if input {
						"voice-voice-settings-popup-input"
					} else {
						"voice-voice-settings-popup-output"
					}),
					18.0,
				));
				egui::ScrollArea::vertical()
					.max_height((ui.ctx().content_rect().height() - 180.0).clamp(180.0, 460.0))
					.show(ui, |ui| self.voice_popup_content(ui, demo, active, input));
				ui.separator();
				if ui
					.add_sized(
						[ui.available_width(), 32.0],
						egui::Button::new(crate::i18n::translate(
							"voice-voice-settings-popup-all-voice-settings",
						)),
					)
					.clicked()
				{
					self.open_voice_settings();
					ui.close();
				}
			});
		trigger.ctx.data_mut(|data| data.insert_temp(id, open));
	}

	/// One half of the voice popout: the device, its level and the toggles that belong to it.
	fn voice_popup_content(&mut self, ui: &mut egui::Ui, demo: bool, active: bool, input: bool) {
		let colors = design::palette(ui);
		ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
		if !demo && !self.voice_available {
			design::notice(
				ui,
				design::Level::Info,
				&crate::i18n::translate(
					"voice-voice-popup-content-install-a-voice-enabled-build-to-use-these-controls",
				),
			);
		}
		ui.add_enabled_ui(!demo && self.voice_available, |ui| {
			// Both settings surfaces share this path. Queue discovery once, without opening streams.
			if ui.is_enabled() && self.voice_device_status.is_empty() {
				self.voice_device_status = "Looking for audio devices...";
				self.voice_refresh_devices = true;
				ui.ctx().request_repaint();
			}
			let label = ui
				.horizontal(|ui| {
					crate::icons::inline(
						ui,
						if input {
							crate::icons::Icon::Microphone
						} else {
							crate::icons::Icon::Headphones
						},
						18.0,
						colors.muted,
					);
					ui.label(design::medium(
						ui,
						crate::i18n::translate_if_key(if input {
							"voice-voice-popup-content-microphone"
						} else {
							"voice-voice-popup-content-speakers"
						}),
						15.0,
					))
				})
				.inner;
			if input {
				device_combo(ui, "voice-input", &self.voice_inputs, &mut self.voice_input)
					.labelled_by(label.id);
				gain_slider(
					ui,
					&mut self.voice_gain.input_percent,
					"voice-voice-popup-content-microphone-gain",
				);
			} else {
				device_combo(
					ui,
					"voice-output",
					&self.voice_outputs,
					&mut self.voice_output,
				)
				.labelled_by(label.id);
				gain_slider(
					ui,
					&mut self.voice_gain.output_percent,
					"voice-voice-popup-content-speaker-volume",
				);
			}
			ui.horizontal_wrapped(|ui| {
				if ui
					.small_button(crate::i18n::translate(
						"voice-voice-popup-content-refresh-devices",
					))
					.clicked()
				{
					self.voice_refresh_devices = true;
				}
				if ui
					.small_button(crate::i18n::translate(
						"voice-voice-popup-content-reset-levels",
					))
					.clicked()
				{
					self.voice_gain = crate::VoiceGain::default();
				}
			});
			ui.separator();
			if input {
				let mut suppression =
					self.voice_processing.effective().suppression != NoiseSuppression::Off;
				if design::switch(
					ui,
					"voice-voice-popup-content-noise-suppression",
					Some("voice-voice-popup-content-choose-an-algorithm-in-all-voice-settings"),
					&mut suppression,
				)
				.changed()
				{
					self.voice_processing.edit().suppression = if suppression {
						NoiseSuppression::default()
					} else {
						NoiseSuppression::Off
					};
				}
				design::switch(
					ui,
					"voice-voice-popup-content-push-to-talk",
					Some(
						"voice-voice-popup-content-hold-your-configured-shortcut-when-you-want-to-speak",
					),
					&mut self.voice_push_to_talk,
				);
			} else {
				ui.label(
					RichText::new(crate::i18n::translate(
						"voice-voice-popup-content-deafen-turns-off-incoming-audio-and-mutes-your-microphone-with",
					))
					.size(12.0)
					.color(colors.muted),
				);
			}
		});
		if self.voice_microphone_unavailable {
			ui.label(
				RichText::new(crate::i18n::translate(
					"voice-voice-popup-content-microphone-unavailable-a-choose-another-input-you-are-still-connected",
				))
				.size(12.0)
				.color(colors.warning),
			);
		}
		if !self.voice_device_status.is_empty() {
			ui.label(
				RichText::new(crate::i18n::translate_if_key(self.voice_device_status))
					.size(12.0)
					.color(colors.muted),
			);
		}
		if active
			&& !input && let Some(code) = &self.voice_privacy_code
		{
			egui::CollapsingHeader::new(crate::i18n::translate(
				"voice-voice-popup-content-voice-privacy-code",
			))
			.show(ui, |ui| {
				ui.add(
					egui::Label::new(RichText::new(code).monospace())
						.selectable(true)
						.wrap(),
				);
			});
		}
	}

	fn microphone_preview_controls(&mut self, ui: &mut egui::Ui, active: bool) {
		let colors = design::palette(ui);
		ui.label(design::medium(
			ui,
			crate::i18n::translate("voice-microphone-preview-controls-microphone-test"),
			15.0,
		));
		ui.label(
			RichText::new(crate::i18n::translate_if_key(if active {
				"voice-microphone-preview-controls-leave-the-call-to-test-your-microphone-locally"
			} else {
				"voice-microphone-preview-controls-hear-yourself-through-your-selected-speakers-use-headphones-to-avoid"
			}))
			.size(13.0)
			.color(colors.muted),
		);
		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = 12.0;
			ui.add_enabled_ui(!active, |ui| {
				if design::button(
					ui,
					&crate::i18n::translate_if_key(if self.voice_preview_requested {
						"voice-microphone-preview-controls-stop-testing"
					} else {
						"voice-microphone-preview-controls-start-testing"
					}),
					if self.voice_preview_requested {
						design::ButtonKind::Outline
					} else {
						design::ButtonKind::Primary
					},
				)
				.clicked()
				{
					self.voice_preview_requested = !self.voice_preview_requested;
					self.voice_preview_status = "";
					self.voice_preview_level = None;
					ui.ctx().request_repaint();
				}
			});
			if self.voice_preview_requested || active {
				let db = self.voice_preview_level.unwrap_or(-100.0);
				ui.label(
					RichText::new(format!(
						"{} {db:.0} dBFS",
						crate::i18n::translate("voice-microphone-preview-controls-input-level")
					))
					.size(13.0)
					.color(colors.muted),
				);
			}
		});
		level_meter(ui, self.voice_preview_level, 18.0);
		if !self.voice_preview_status.is_empty() {
			ui.label(
				RichText::new(crate::i18n::translate_if_key(self.voice_preview_status))
					.size(13.0)
					.color(colors.muted),
			);
		}
	}

	/// The voice card's noise suppression popover: one switch, the filter and its strength,
	/// and the live microphone level, with a way into the full voice settings.
	fn noise_popup(&mut self, trigger: &egui::Response) {
		egui::Popup::menu(trigger)
			.width(300.0)
			.gap(8.0)
			.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
			.frame(
				egui::Frame::popup(&trigger.ctx.style_of(trigger.ctx.theme()))
					.inner_margin(16)
					.corner_radius(12),
			)
			.show(|ui| {
				let colors = design::palette(ui);
				ui.set_width(268.0);
				ui.spacing_mut().item_spacing.y = 8.0;
				let current = self.voice_processing.effective();
				let mut on = current.suppression != NoiseSuppression::Off;
				if design::switch(
					ui,
					"Noise suppression",
					Some(
						"Filters keyboard, fan and room noise out of your microphone, so people hear your voice.",
					),
					&mut on,
				)
				.changed()
				{
					self.voice_processing.edit().suppression = if on {
						NoiseSuppression::default()
					} else {
						NoiseSuppression::Off
					};
				}
				let current = self.voice_processing.effective();
				if current.suppression != NoiseSuppression::Off {
					design::card_divider(ui);
					for (value, label, detail) in [
						(
							NoiseSuppression::RnNoise,
							"RNNoise",
							"Neural filter that keeps speech clear. Best for most rooms.",
						),
						(
							NoiseSuppression::WebRtc,
							"WebRTC",
							"Lighter filter with a strength you choose.",
						),
					] {
						if design::radio_row(ui, current.suppression == value, label, Some(detail))
							.clicked()
						{
							self.voice_processing.edit().suppression = value;
						}
					}
					if current.suppression == NoiseSuppression::WebRtc
						&& let Some(level) = design::segmented(
							ui,
							&["Low", "Moderate", "High", "Very high"],
							usize::from(current.suppression_level.min(3)),
						) {
						self.voice_processing.edit().suppression_level = level as u8;
					}
				}
				design::card_divider(ui);
				ui.label(design::medium(ui, "Microphone level", 13.0).color(colors.text_strong));
				level_meter(ui, self.voice_preview_level, 10.0);
				design::card_divider(ui);
				if design::text_action(ui, "voice-voice-settings-popup-all-voice-settings")
					.clicked()
				{
					self.open_voice_settings();
					ui.close();
				}
			});
	}

	pub(super) fn voice_settings_content(
		&mut self,
		ui: &mut egui::Ui,
		demo: bool,
		active: bool,
		compact: bool,
	) {
		let colors = design::palette(ui);
		ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
		ui.spacing_mut().item_spacing.y = if compact { 6.0 } else { 12.0 };
		if !demo && !self.voice_available {
			design::notice(
				ui,
				design::Level::Info,
				&crate::i18n::translate(
					"voice-voice-settings-content-install-a-voice-enabled-build-to-use-these-controls",
				),
			);
		}
		ui.add_enabled_ui(!demo && self.voice_available, |ui| {
			if compact {
				self.voice_audio_controls(ui);
				egui::CollapsingHeader::new(crate::i18n::translate(
					"voice-voice-settings-content-voice-processing-input-mode",
				))
				.show(ui, |ui| self.voice_processing_controls(ui));
			} else {
				design::group(
					ui,
					&crate::i18n::translate("voice-voice-settings-content-devices-levels"),
					|ui| {
						self.voice_audio_controls(ui);
						design::card_divider(ui);
						self.microphone_preview_controls(ui, active);
					},
				);
				design::group(
					ui,
					&crate::i18n::translate("voice-voice-settings-content-voice-processing"),
					|ui| self.voice_processing_controls(ui),
				);
			}
		});
		if !compact {
			crate::keybinds::show_voice(
				ui,
				&mut self.keybinds,
				&mut self.keybind_capture,
				self.global_keybind_status,
			);
		}
		design::group(
			ui,
			&crate::i18n::translate("voice-voice-settings-content-camera"),
			|ui| self.camera_settings_content(ui, demo),
		);
		if active && let Some(code) = &self.voice_privacy_code {
			egui::CollapsingHeader::new(crate::i18n::translate(
				"voice-voice-settings-content-voice-privacy-code",
			))
			.show(ui, |ui| {
				ui.add(
					egui::Label::new(RichText::new(code).monospace())
						.selectable(true)
						.wrap(),
				);
				ui.label(
					RichText::new(crate::i18n::translate(
						"voice-voice-settings-content-compare-with-the-other-participants-this-code-changes-with-the",
					))
					.size(12.0)
					.color(colors.muted),
				);
			});
		}
	}

	fn camera_settings_content(&mut self, ui: &mut egui::Ui, demo: bool) {
		let colors = design::palette(ui);
		ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
		if !cfg!(any(
			target_os = "windows",
			target_os = "macos",
			target_os = "linux"
		)) {
			ui.label(crate::i18n::translate(
				"voice-camera-settings-content-camera-capture-is-unavailable-on-this-platform",
			));
			return;
		}
		if demo && self.voice_cameras.is_empty() {
			self.voice_cameras = vec![
				(
					"synthetic-integrated".into(),
					"Integrated Camera (preview)".into(),
				),
				("synthetic-usb".into(), "USB Camera (preview)".into()),
			];
		}
		if ui.is_enabled()
			&& !demo && self.voice_camera_device_status.is_empty()
			&& !self.voice_camera_devices_loading
		{
			self.voice_camera_device_status = "Looking for cameras...";
			self.voice_refresh_cameras = true;
			ui.ctx().request_repaint();
		}
		let label = ui.label(design::medium(
			ui,
			crate::i18n::translate("voice-camera-settings-content-camera-device"),
			15.0,
		));
		device_combo(
			ui,
			"voice-camera",
			&self.voice_cameras,
			&mut self.voice_camera_device,
		)
		.labelled_by(label.id);
		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = 4.0;
			ui.add_enabled_ui(!demo && !self.voice_camera_devices_loading, |ui| {
				if design::text_action(
					ui,
					&crate::i18n::translate("voice-camera-settings-content-refresh-cameras"),
				)
				.clicked()
				{
					self.voice_refresh_cameras = true;
				}
			});
			if !self.voice_camera_device_status.is_empty() && !demo {
				ui.label(
					RichText::new(crate::i18n::translate_if_key(
						self.voice_camera_device_status,
					))
					.size(12.0)
					.color(colors.muted),
				);
			}
		});
		if !demo {
			design::hint(
				ui,
				&crate::i18n::translate(
					"voice-camera-settings-content-changing-devices-stops-your-camera-and-takes-effect-the-next",
				),
			);
		}
		if self.voice_settings_open() {
			design::card_divider(ui);
			let width = ui.available_width();
			let (rect, _) = ui.allocate_exact_size(
				egui::vec2(width, (width * 9.0 / 16.0).clamp(160.0, 300.0)),
				egui::Sense::hover(),
			);
			ui.painter().rect_filled(rect, 12, colors.base);
			let texture = self
				.camera_test_texture
				.as_ref()
				.or(self.voice_camera_preview.as_ref());
			let has_picture = texture.is_some();
			if let Some(texture) = texture {
				let size = texture.size_vec2();
				let scale = (rect.width() / size.x).min(rect.height() / size.y);
				egui::Image::new(texture)
					.uv(egui::Rect::from_min_max(
						egui::pos2(1.0, 0.0),
						egui::pos2(0.0, 1.0),
					))
					.corner_radius(12)
					.paint_at(
						ui,
						egui::Rect::from_center_size(rect.center(), size * scale),
					);
			}
			if !has_picture || self.camera_test_requested {
				let center = if has_picture {
					egui::pos2(rect.center().x, rect.bottom() - 34.0)
				} else {
					rect.center()
				};
				let button_rect =
					egui::Rect::from_center_size(center, egui::vec2(184.0_f32.min(width), 44.0));
				ui.scope_builder(egui::UiBuilder::new().max_rect(button_rect), |ui| {
					ui.add_enabled_ui(!demo && self.camera_test_available, |ui| {
						let (icon, label) = if self.camera_test_requested {
							(crate::icons::Icon::VideoSlash, "Stop preview")
						} else {
							(crate::icons::Icon::Video, "Preview camera")
						};
						if design::primary_icon_button(ui, icon, label).clicked() {
							self.camera_test_requested = !self.camera_test_requested;
							self.camera_test_status = "";
						}
					});
				});
			}
			if !self.camera_test_status.is_empty() {
				design::hint(ui, self.camera_test_status);
			}
		}
	}

	fn camera_settings_popup(&mut self, trigger: &egui::Response, demo: bool) {
		let id = trigger.id.with("camera-settings-open");
		let mut open = trigger
			.ctx
			.data_mut(|data| *data.get_temp_mut_or_default::<bool>(id));
		if trigger.clicked() {
			open = !open;
		}
		let close_behavior = if egui::Popup::is_any_open(&trigger.ctx) {
			egui::PopupCloseBehavior::IgnoreClicks
		} else {
			egui::PopupCloseBehavior::CloseOnClickOutside
		};
		egui::Popup::menu(trigger)
			.open_bool(&mut open)
			.style(|_: &mut egui::Style| {})
			.width(300.0)
			.close_behavior(close_behavior)
			.show(|ui| self.camera_settings_content(ui, demo));
		trigger.ctx.data_mut(|data| data.insert_temp(id, open));
	}

	fn voice_audio_controls(&mut self, ui: &mut egui::Ui) {
		// Both settings surfaces use this path. Queue discovery once, without opening streams.
		if ui.is_enabled() && self.voice_device_status.is_empty() {
			self.voice_device_status = "Looking for audio devices...";
			self.voice_refresh_devices = true;
			ui.ctx().request_repaint();
		}
		let colors = design::palette(ui);
		let mut device = |ui: &mut egui::Ui, input: bool| {
			let label = ui
				.horizontal(|ui| {
					crate::icons::inline(
						ui,
						if input {
							crate::icons::Icon::Microphone
						} else {
							crate::icons::Icon::Headphones
						},
						18.0,
						colors.muted,
					);
					ui.label(design::medium(
						ui,
						crate::i18n::translate_if_key(if input {
							"voice-voice-audio-controls-microphone"
						} else {
							"voice-voice-audio-controls-speakers"
						}),
						15.0,
					))
				})
				.inner;
			if input {
				device_combo(ui, "voice-input", &self.voice_inputs, &mut self.voice_input)
					.labelled_by(label.id);
			} else {
				device_combo(
					ui,
					"voice-output",
					&self.voice_outputs,
					&mut self.voice_output,
				)
				.labelled_by(label.id);
			}
		};
		if ui.available_width() >= 480.0 {
			ui.columns(2, |columns| {
				device(&mut columns[0], true);
				device(&mut columns[1], false);
			});
		} else {
			device(ui, true);
			device(ui, false);
		}
		gain_controls(ui, &mut self.voice_gain);
		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = 4.0;
			if design::text_action(
				ui,
				&crate::i18n::translate("voice-voice-audio-controls-refresh-devices"),
			)
			.clicked()
			{
				self.voice_refresh_devices = true;
			}
			if self.voice_gain != crate::VoiceGain::default()
				&& design::text_action(
					ui,
					&crate::i18n::translate("voice-voice-audio-controls-reset-levels"),
				)
				.clicked()
			{
				self.voice_gain = crate::VoiceGain::default();
			}
			if !self.voice_device_status.is_empty() {
				ui.label(
					RichText::new(crate::i18n::translate_if_key(self.voice_device_status))
						.size(12.0)
						.color(colors.muted),
				);
			}
		});
		if self.voice_microphone_unavailable {
			design::notice(
				ui,
				design::Level::Warning,
				&crate::i18n::translate(
					"voice-voice-audio-controls-microphone-unavailable-a-choose-another-input-you-are-still-connected",
				),
			);
		}
	}

	fn voice_processing_controls(&mut self, ui: &mut egui::Ui) {
		let colors = design::palette(ui);
		design::section(
			ui,
			&crate::i18n::translate("voice-voice-processing-controls-input-profile"),
			Some(&crate::i18n::translate(
				"voice-voice-processing-controls-applies-to-calls-and-your-local-microphone-test",
			)),
		);
		for (profile, label, detail) in [
			(
				InputProfile::VoiceIsolation,
				"Voice Isolation",
				"RNNoise suppression, echo cancellation and automatic gain for speech.",
			),
			(
				InputProfile::Studio,
				"Studio",
				"Open microphone without suppression, echo cancellation or automatic gain.",
			),
			(
				InputProfile::Custom,
				"Custom",
				"Choose your noise suppression, sensitivity and processing.",
			),
		] {
			if design::radio_row(
				ui,
				self.voice_processing.profile == profile,
				label,
				Some(detail),
			)
			.clicked()
			{
				self.voice_processing.profile = profile;
			}
		}
		if self.voice_processing.profile == InputProfile::Custom {
			design::card_divider(ui);
			let processing = &mut self.voice_processing.custom;
			let mut sensitivity = processing.sensitivity_db.is_some();
			if design::switch(
				ui,
				"voice-voice-processing-controls-input-threshold",
				Some(if sensitivity {
					"voice-voice-processing-controls-only-transmit-sound-above-this-level-lower-values-pick-up"
				} else {
					"voice-voice-processing-controls-open-microphone-mute-and-push-to-talk-still-apply"
				}),
				&mut sensitivity,
			)
			.changed()
			{
				processing.sensitivity_db = sensitivity.then_some(-55);
			}
			if let Some(db) = &mut processing.sensitivity_db {
				ui.add_space(4.0);
				design::slider(ui, db, -80..=0, " dBFS");
			}
			if let Some(level) = self.voice_preview_level {
				ui.add(
					egui::ProgressBar::new(((level + 80.0) / 80.0).clamp(0.0, 1.0))
						.text(format!("Input level: {level:.0} dBFS"))
						.fill(
							if processing
								.sensitivity_db
								.is_none_or(|threshold| level >= f32::from(threshold))
							{
								colors.positive
							} else {
								colors.warning
							},
						),
				);
			} else {
				design::hint(
					ui,
					&crate::i18n::translate(
						"voice-voice-processing-controls-start-the-microphone-test-or-join-a-call-to-see",
					),
				);
			}
			design::card_divider(ui);
			let choices = [
				(NoiseSuppression::Off, "Off"),
				(NoiseSuppression::RnNoise, "RNNoise"),
				(NoiseSuppression::WebRtc, "WebRTC"),
			];
			design::row(
				ui,
				"voice-voice-processing-controls-noise-suppression",
				Some(
					"voice-voice-processing-controls-removes-keyboard-fan-and-room-noise-from-your-microphone",
				),
				|ui| {
					crate::design::combo("voice-noise-suppression")
						.selected_text(crate::i18n::translate_if_key(
							choices
								.iter()
								.find(|(value, _)| *value == processing.suppression)
								.map_or("voice-voice-processing-controls-off", |(_, label)| *label),
						))
						.width(ui.available_width().min(160.0))
						.show_ui(ui, |ui| {
							for (value, label) in choices {
								ui.selectable_value(
									&mut processing.suppression,
									value,
									crate::i18n::translate_if_key(label),
								);
							}
						});
				},
			);
			if processing.suppression == NoiseSuppression::WebRtc {
				ui.add_space(6.0);
				let strength = ["Low", "Moderate", "High", "Very high"];
				design::row(
					ui,
					"voice-voice-processing-controls-suppression-strength",
					None,
					|ui| {
						crate::design::combo("voice-suppression-strength")
							.selected_text(crate::i18n::translate_if_key(
								strength[usize::from(processing.suppression_level.min(3))],
							))
							.width(ui.available_width().min(160.0))
							.show_ui(ui, |ui| {
								for (index, label) in strength.iter().enumerate() {
									ui.selectable_value(
										&mut processing.suppression_level,
										index as u8,
										crate::i18n::translate_if_key(label),
									);
								}
							});
					},
				);
			}
			design::card_divider(ui);
			design::switch(
				ui,
				"voice-voice-processing-controls-echo-cancellation",
				Some(
					"voice-voice-processing-controls-reduce-speaker-audio-picked-up-by-your-microphone",
				),
				&mut processing.echo_cancellation,
			);
			design::card_divider(ui);
			design::switch(
				ui,
				"voice-voice-processing-controls-automatic-gain-control",
				Some("voice-voice-processing-controls-adjust-microphone-loudness-automatically"),
				&mut processing.automatic_gain,
			);
		}
		design::card_divider(ui);
		design::switch(
			ui,
			"voice-voice-processing-controls-push-to-talk",
			Some(
				"voice-voice-processing-controls-hold-your-configured-shortcut-when-you-want-to-speak",
			),
			&mut self.voice_push_to_talk,
		)
		.on_hover_text(crate::i18n::translate(
			"voice-voice-processing-controls-mute-and-deafen-always-take-priority",
		));
	}

	/// Whether the local mute/deafen controls may emit commands for the active call.
	fn controls_enabled(&self, state: &State) -> bool {
		self.voice_available && !state.offline_preview() && state.voice.active.as_ref().is_some()
	}

	fn queue_voice_toggle_cue(&mut self, deafen: bool, active: bool) {
		let cue = match (deafen, active) {
			(true, true) => model::notification_preferences::Sound::Deafen,
			(true, false) => model::notification_preferences::Sound::Undeafen,
			(false, true) => model::notification_preferences::Sound::Mute,
			(false, false) => model::notification_preferences::Sound::Unmute,
		};
		if self.notification_options.allows(cue) {
			self.notification_preview = Some(cue);
		}
	}

	/// Applies one mute (`deafen == false`) or deafen toggle to the newest local choice and
	/// queues its cue. Deafening implies muted; undeafening restores the earlier microphone.
	pub fn toggle_voice(&mut self, deafen: bool) -> (bool, bool) {
		let before = (self.voice_muted, self.voice_deafened);
		let after = client_core::voice::toggle_self_voice(before.0, before.1, deafen);
		(self.voice_muted, self.voice_deafened) = after;
		if after.1 != before.1 {
			self.queue_voice_toggle_cue(true, after.1);
		} else {
			self.queue_voice_toggle_cue(false, after.0);
		}
		after
	}

	/// Which glyph the mute/deafen toggle paints. `ptm_held` is a held push-to-mute
	/// on an otherwise live microphone: still slashed, but the caller paints it in
	/// the neutral tone rather than the red latched-mute tone. A toggled mute keeps
	/// red even while the key is held, and deafen never shows a microphone.
	fn mute_toggle_icon(deafen: bool, muted: bool, ptm_held: bool) -> crate::icons::Icon {
		match (deafen, muted) {
			(true, true) => crate::icons::Icon::HeadphonesSlash,
			(true, false) => crate::icons::Icon::Headphones,
			(false, _) if muted || ptm_held => crate::icons::Icon::MicrophoneSlash,
			(false, _) => crate::icons::Icon::Microphone,
		}
	}

	/// Mute or deafen toggle: red slashed glyph while active, like Discord's user area.
	/// A held push-to-mute on an otherwise live microphone shows the slashed
	/// microphone in the neutral tone instead, so a momentary hold reads
	/// differently from a latched mute.
	pub(super) fn mute_toggle(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		deafen: bool,
		size: f32,
	) -> egui::Response {
		let colors = design::palette(ui);
		let Some(call) = state.voice.active.as_ref() else {
			let active = if deafen {
				self.voice_deafened
			} else {
				self.voice_muted || self.voice_deafened
			};
			let label = crate::i18n::translate_if_key(match (deafen, active) {
				(true, true) => "voice-mute-toggle-undeafen",
				(true, false) => "voice-mute-toggle-deafen",
				(false, true) => "voice-mute-toggle-unmute",
				(false, false) => "voice-mute-toggle-mute",
			});
			let (rect, response) =
				ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::click());
			ui.painter().rect_filled(
				rect,
				crate::icons::PLATE_RADIUS,
				crate::icons::plate(ui, &response, false),
			);
			crate::icons::paint(
				ui.painter(),
				match (deafen, active) {
					(true, true) => crate::icons::Icon::HeadphonesSlash,
					(true, false) => crate::icons::Icon::Headphones,
					(false, true) => crate::icons::Icon::MicrophoneSlash,
					(false, false) => crate::icons::Icon::Microphone,
				},
				rect.shrink(size * 0.2),
				if active { colors.danger } else { colors.muted },
			);
			response.widget_info(|| {
				egui::WidgetInfo::selected(egui::Role::Button, true, active, &label)
			});
			if response.clicked() {
				self.toggle_voice(deafen);
			}
			return response.on_hover_text(format!(
				"{label}; {}",
				crate::i18n::translate("voice-mute-toggle-applies-to-your-next-call")
			));
		};
		let channel = call.channel;
		let can_speak = state.can_speak(channel);
		let active = if deafen {
			self.voice_deafened
		} else {
			self.voice_muted || self.voice_deafened || !can_speak
		};
		// A momentary push-to-mute stays neutral unless mute/deafen is latched.
		let ptm = !deafen && !active && self.voice_ptm_active;
		let enabled = (self.controls_enabled(state) || state.offline_preview())
			&& (deafen || can_speak || state.offline_preview());
		let label = crate::i18n::translate_if_key(match (deafen, active) {
			(true, true) => "voice-mute-toggle-undeafen",
			(true, false) => "voice-mute-toggle-deafen",
			(false, true) => "voice-mute-toggle-unmute",
			(false, false) => "voice-mute-toggle-mute",
		});
		let response = ui
			.add_enabled_ui(enabled, |ui| {
				let (rect, response) =
					ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::click());
				ui.painter().rect_filled(
					rect,
					crate::icons::PLATE_RADIUS,
					crate::icons::plate(ui, &response, false),
				);
				let icon = Self::mute_toggle_icon(deafen, active, ptm);
				let color = if !enabled {
					colors.muted.gamma_multiply(0.5)
				} else if active {
					colors.danger
				} else if ptm {
					colors.muted
				} else {
					colors
						.muted
						.lerp_to_gamma(colors.text_strong, design::hover_motion(ui, &response))
				};
				let press = design::press_motion(ui, &response);
				crate::icons::paint(
					ui.painter(),
					icon,
					design::pressed_rect(rect.shrink(size * 0.2), press),
					color,
				);
				response.widget_info(|| {
					egui::WidgetInfo::selected(egui::Role::Button, enabled, active, &label)
				});
				response
			})
			.inner
			.on_hover_text(crate::i18n::translate_if_key(
				&(if enabled {
					label.clone()
				} else if !can_speak && !deafen {
					crate::i18n::translate(
						"voice-mute-toggle-speaking-is-unavailable-in-this-channel",
					)
				} else {
					crate::i18n::translate(
						"voice-mute-toggle-controls-are-unavailable-in-this-build-or-preview",
					)
				}),
			));
		if response.clicked() {
			let (muted, deafened) = self.toggle_voice(deafen);
			state.set_call_mute(muted, deafened);
		}
		response
	}

	/// Discord's call control bar: one media pill and the red hang-up button.
	fn call_controls(&mut self, ui: &mut egui::Ui, state: &mut State, channel: Id) {
		let colors = design::palette(ui);
		let Some(call) = state.voice.active.as_ref() else {
			return;
		};
		let phase = call.phase;
		let camera = call.camera;
		let can_camera = self.voice_camera_available
			&& state.can_camera(channel)
			&& matches!(phase, Phase::Connected | Phase::Waiting);
		let can_speak = state.can_speak(channel);
		let deafened = self.voice_deafened;
		let muted = self.voice_muted || deafened || !can_speak;
		let controls = self.controls_enabled(state);
		let voice_toggles = controls || state.offline_preview();
		let focused = self.voice_focus.is_some();
		let screen_focused = matches!(
			self.voice_focus,
			Some(StageFocus::LocalScreen | StageFocus::Stream(_))
		);
		// Discord's call bar: microphone and camera with their device menus, then share and
		// more, then the red action; view actions sit at the right edge.
		let watched = match self.voice_focus {
			Some(StageFocus::Stream(user)) if call.watching == Some(user) => Some(user),
			_ => None,
		};
		let bar = ui.max_rect();
		// A narrow stage sheds secondary controls (view toggles, device chevrons, then
		// share/more) before anything overlaps; mute, camera and hang-up always stay.
		let actions = 2.0 * 48.0;
		let essential = 2.0 * 48.0 + BAR_GAP + HANG_UP;
		let side = [watched.is_some(), focused, screen_focused]
			.iter()
			.filter(|shown| **shown)
			.count() as f32
			* 44.0;
		let side_shown = bar.width() - 2.0 * STAGE_MARGIN >= essential + BAR_GAP + side;
		let side = if side_shown { side } else { 0.0 };
		let room = bar.width() - 2.0 * STAGE_MARGIN - if side > 0.0 { side + BAR_GAP } else { 0.0 };
		let chevrons = room >= 2.0 * 28.0 + actions + BAR_GAP + essential;
		let media = if chevrons {
			2.0 * (48.0 + 28.0)
		} else {
			2.0 * 48.0
		};
		let actions_shown = room >= media + BAR_GAP + actions + BAR_GAP + HANG_UP;
		let width = media
			+ BAR_GAP + if actions_shown {
			actions + BAR_GAP
		} else {
			0.0
		} + HANG_UP;
		let left = (bar.center().x - width * 0.5)
			.min(bar.right() - STAGE_MARGIN - side - BAR_GAP - width)
			.max(bar.left() + STAGE_MARGIN);
		let private = call.guild.is_none();
		let mut camera_clicked = false;
		let mut mute_clicked = false;
		let mut leave = false;
		let mut stop_watching = false;
		let mut row = ui.new_child(
			egui::UiBuilder::new()
				.max_rect(egui::Rect::from_min_size(
					egui::pos2(left, bar.center().y - CONTROL_HEIGHT * 0.5),
					egui::vec2(width, CONTROL_HEIGHT),
				))
				.layout(egui::Layout::left_to_right(egui::Align::Center)),
		);
		row.spacing_mut().item_spacing.x = BAR_GAP;
		pill(&mut row, media, |ui| {
			let latch = ui.painter().add(egui::Shape::Noop);
			let mic = control(
				ui,
				if muted {
					crate::icons::Icon::MicrophoneSlash
				} else {
					crate::icons::Icon::Microphone
				},
				48.0,
				voice_toggles && (can_speak || state.offline_preview()),
				if muted { colors.danger } else { STAGE_TEXT },
				if muted { "Unmute" } else { "Mute" },
				if !can_speak {
					"Speaking is unavailable in this channel."
				} else if muted {
					"Turn on microphone"
				} else {
					"Turn off microphone"
				},
			);
			mute_clicked = mic.clicked();
			let mut latched = mic.rect;
			if chevrons {
				let settings = control(
					ui,
					crate::icons::Icon::ChevronDown,
					28.0,
					true,
					STAGE_TEXT,
					"Voice settings",
					"Microphone and speaker settings",
				);
				latched = latched.union(settings.rect);
				self.voice_settings_popup(&settings, state.demo, true, true);
			}
			if muted {
				// Discord tints the whole microphone half red while it is muted.
				ui.painter().set(
					latch,
					egui::Shape::rect_filled(
						latched.shrink(4.0),
						crate::icons::PLATE_RADIUS,
						colors.danger.gamma_multiply(0.24),
					),
				);
			}
			camera_clicked = control(
				ui,
				if camera {
					crate::icons::Icon::Video
				} else {
					crate::icons::Icon::VideoSlash
				},
				48.0,
				controls && (camera || can_camera),
				if camera { colors.positive } else { STAGE_TEXT },
				if camera {
					"Turn off camera"
				} else {
					"Turn on camera"
				},
				if camera {
					"Stop sharing your camera"
				} else if state.offline_preview() {
					"Camera is off in the offline preview"
				} else if !cfg!(any(
					target_os = "macos",
					target_os = "windows",
					target_os = "linux"
				)) {
					"Camera capture is unavailable on this platform"
				} else if !self.voice_camera_available {
					"Camera requires H264 support from the voice server"
				} else if !state.can_camera(channel) {
					"Camera is unavailable with current channel permissions"
				} else {
					"Share your selected camera with this call"
				},
			)
			.clicked();
			if chevrons {
				let camera_settings = control(
					ui,
					crate::icons::Icon::ChevronDown,
					28.0,
					true,
					STAGE_TEXT,
					"Camera settings",
					"Choose a camera",
				);
				self.camera_settings_popup(&camera_settings, state.demo);
			}
		});
		if actions_shown {
			pill(&mut row, actions, |ui| {
				self.screen_share_control(ui, state);
				let more = control(
					ui,
					crate::icons::Icon::More,
					48.0,
					true,
					STAGE_TEXT,
					"More options",
					"More call options",
				);
				if more.clicked() && private {
					state.request_voice_regions();
				}
				egui::Popup::menu(&more)
					.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
					.show(|ui| self.call_options(ui, state, private));
			});
		}
		// While a watched share is enlarged, the red action stops watching, as in Discord.
		let hang_up = {
			let (rect, response) =
				row.allocate_exact_size(egui::vec2(HANG_UP, CONTROL_HEIGHT), egui::Sense::click());
			let enabled = watched.is_some() || !state.offline_preview();
			let press = if enabled {
				design::press_motion(&row, &response)
			} else {
				0.0
			};
			let fill = if enabled {
				colors.danger.gamma_multiply(
					1.0 - 0.15 * design::hover_motion(&row, &response) - 0.1 * press,
				)
			} else {
				colors.danger.gamma_multiply(0.45)
			};
			let rect = design::pressed_rect(rect, press);
			row.painter().rect_filled(rect, 12, fill);
			crate::icons::paint(
				row.painter(),
				if watched.is_some() {
					crate::icons::Icon::Close
				} else {
					crate::icons::Icon::HangUp
				},
				egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(22.0)),
				egui::Color32::WHITE,
			);
			let label = if watched.is_some() {
				"Stop watching".to_owned()
			} else {
				crate::i18n::translate_if_key("voice-call-controls-disconnect")
			};
			response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, &label));
			response.on_hover_text(if enabled {
				label.clone()
			} else {
				crate::i18n::translate(
					"voice-call-controls-leaving-is-unavailable-in-the-offline-preview",
				)
			})
		};
		if hang_up.clicked() {
			if watched.is_some() {
				stop_watching = true;
			} else {
				leave = !state.offline_preview();
			}
		}
		let mut view = ui.new_child(
			egui::UiBuilder::new()
				.max_rect(egui::Rect::from_min_max(
					egui::pos2(bar.right() - STAGE_MARGIN - side, bar.top()),
					egui::pos2(bar.right() - STAGE_MARGIN, bar.bottom()),
				))
				.layout(egui::Layout::right_to_left(egui::Align::Center)),
		);
		view.spacing_mut().item_spacing.x = 4.0;
		if screen_focused && side_shown {
			let fullscreen = fullscreen_control(&mut view, STAGE_TEXT);
			if fullscreen.clicked() {
				let previous = view.input(|input| input.viewport().fullscreen.unwrap_or(false));
				self.voice_fullscreen = self
					.voice_focus
					.map(|focus| (focus, view.ctx().clone(), previous, fullscreen.id));
				self.voice_fullscreen_request = Some(true);
			}
		}
		if focused && side_shown {
			let shown = self.voice_focus_participants;
			if control(
				&mut view,
				crate::icons::Icon::People,
				40.0,
				true,
				if shown { colors.accent } else { STAGE_TEXT },
				if shown {
					"Hide participants"
				} else {
					"Show participants"
				},
				if shown {
					"Hide the participant strip under the enlarged video"
				} else {
					"Show the other participants under the enlarged video"
				},
			)
			.clicked()
			{
				self.voice_focus_participants = !shown;
			}
		}
		if watched.is_some() && side_shown {
			let volume = control(
				&mut view,
				crate::icons::Icon::Speaker,
				40.0,
				true,
				if self.voice_stream_volume() == 0 {
					colors.danger
				} else {
					STAGE_TEXT
				},
				"Stream volume",
				"Stream audio",
			);
			egui::Popup::menu(&volume)
				.close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
				.show(|ui| self.stream_audio_controls(ui));
		}
		if stop_watching {
			self.watch_request = Some(None);
			self.voice_focus = None;
		}
		if camera_clicked && state.set_call_camera(!camera) {
			self.voice_camera_status = "";
		}
		if mute_clicked {
			let (muted, deafened) = self.toggle_voice(false);
			state.set_call_mute(muted, deafened);
		}
		if leave {
			state.leave_call();
		}
	}

	/// Discord's call options: view choices, voice and video settings, and the region a
	/// private call runs in.
	fn call_options(&mut self, ui: &mut egui::Ui, state: &mut State, private: bool) {
		// Discord's width; rows fill it, so the menu never grows to the free space.
		ui.set_width(250.0);
		design::menu_check(ui, "Grid View", &mut self.call_grid);
		let mut own_camera = !self.call_hide_own_camera;
		if design::menu_check(ui, "Show My Own Camera", &mut own_camera).changed() {
			self.call_hide_own_camera = !own_camera;
		}
		let mut non_video = !self.call_hide_non_video;
		if design::menu_check(ui, "Show Non-Video Participants", &mut non_video).changed() {
			self.call_hide_non_video = !non_video;
		}
		ui.separator();
		if design::menu_item(
			ui,
			Some(crate::icons::Icon::Gear),
			"Voice & Video Settings",
			false,
		)
		.clicked()
		{
			self.open_voice_settings();
			ui.close();
		}
		let Some(channel) = state
			.voice
			.active
			.as_ref()
			.filter(|_| private)
			.map(|call| call.channel)
		else {
			return;
		};
		let current = state.call_region(channel).map(str::to_owned);
		let automatic = crate::i18n::translate_if_key("Automatic");
		let name = current.as_deref().map_or_else(
			|| automatic.clone(),
			|id| {
				state
					.voice
					.regions
					.iter()
					.flatten()
					.find(|region| region.id == id)
					.map_or_else(|| id.to_owned(), |region| region.name.clone())
			},
		);
		let colors = design::palette(ui);
		let title = crate::i18n::translate_if_key("Region");
		let medium = design::medium_family(ui.ctx());
		// Discord's two-line row: the option, and under it where the call runs.
		let label = format!("{title}, {name}");
		let row = egui::Atom::paint(egui::vec2(194.0, 36.0), move |ui, args| {
			let painter = ui.painter();
			painter.text(
				egui::pos2(args.rect.left() + 8.0, args.rect.center().y - 8.0),
				egui::Align2::LEFT_CENTER,
				&title,
				egui::FontId::new(14.0, medium.clone()),
				colors.text,
			);
			painter.text(
				egui::pos2(args.rect.left() + 8.0, args.rect.center().y + 9.0),
				egui::Align2::LEFT_CENTER,
				&name,
				egui::FontId::proportional(12.0),
				colors.muted,
			);
		});
		ui.add_enabled_ui(state.call_connected(), |ui| {
			let menu = ui.menu_button(row, |ui| {
				ui.set_min_width(180.0);
				let mut pick = None;
				if ui.selectable_label(current.is_none(), automatic).clicked() {
					pick = Some(None);
				}
				match &state.voice.regions {
					None => {
						ui.label(
							RichText::new(crate::i18n::translate_if_key("Loading regions…"))
								.size(12.0)
								.color(colors.muted),
						);
					}
					Some(regions) => {
						for region in regions {
							let label = if region.optimal {
								format!(
									"{} · {}",
									region.name,
									crate::i18n::translate_if_key("Best")
								)
							} else {
								region.name.clone()
							};
							if ui
								.selectable_label(current.as_deref() == Some(&region.id), label)
								.clicked()
							{
								pick = Some(Some(region.id.clone()));
							}
						}
					}
				}
				if let Some(region) = pick {
					if region != current {
						state.set_call_region(region);
					}
					ui.close();
				}
			});
			let enabled = ui.is_enabled();
			menu.response
				.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, &label));
		});
	}

	/// Go Live, following the call's own motion: the plate waits small and faint while a
	/// loading line runs round its edge, then, once the share is up, the line closes round the
	/// plate, the plate fills and springs to full size, and LIVE appears after it.
	fn screen_share_control(&mut self, ui: &mut egui::Ui, state: &State) {
		let enabled = self.screen.busy
			|| state.offline_preview()
			|| (self.screen.supported
				&& state.voice.active.as_ref().is_some_and(|call| {
					matches!(call.phase, Phase::Connected | Phase::Waiting)
						&& state.can_stream(call.channel)
				}));
		let label = if self.screen.busy {
			"Stop sharing"
		} else {
			"Share your screen"
		};
		let colors = design::palette(ui);
		let (starting, live) = self.screen.since(ui.ctx());
		let close = CUE_LATENCY + CHIME_NOTE;
		let fill = live.map_or(0.0, |live| {
			egui::emath::easing::cubic_out(((live - close * 0.5) / 0.18).clamp(0.0, 1.0))
		});
		let plate = ui.painter().add(egui::Shape::Noop);
		let response = control(
			ui,
			crate::icons::Icon::ScreenShare,
			48.0,
			enabled,
			STAGE_TEXT.lerp_to_gamma(colors.accent_text, fill),
			label,
			if enabled {
				self.screen
					.capture_status
					.unwrap_or(if self.screen.status.is_empty() {
						label
					} else {
						self.screen.status
					})
			} else {
				"Screen sharing requires a connected call and video permission on a supported desktop."
			},
		);
		if starting.is_some() || live.is_some() {
			let rect = response.rect.shrink(4.0);
			let hover = design::hover_motion(ui, &response);
			let faint = colors.accent.gamma_multiply(0.16 + 0.06 * hover);
			let solid = colors
				.accent
				.lerp_to_gamma(colors.accent.gamma_multiply(1.15), hover);
			let scale = live.map_or(WAITING_SCALE, arrival_scale);
			let shape = egui::Rect::from_center_size(rect.center(), rect.size() * scale);
			ui.painter().set(
				plate,
				egui::Shape::rect_filled(shape, 8, faint.lerp_to_gamma(solid, fill)),
			);
			// The line runs on the plate's own rounded edge.
			let edge = shape.shrink(SHARE_RING * 0.5);
			let corner = 8.0 - SHARE_RING * 0.5;
			let outline = |lap| rounded_outline(edge, corner, lap);
			let glow = self.call_glow(ui);
			match live {
				None => comet(ui, outline, glow, 0.0, SHARE_RING),
				Some(live) if live < close => comet(
					ui,
					outline,
					glow,
					egui::emath::easing::cubic_in_out(live / close),
					SHARE_RING,
				),
				Some(live) => {
					let fade = 1.0 - ((live - close) / 0.3).min(1.0);
					if fade > 0.0 {
						ui.painter().rect_stroke(
							edge,
							corner,
							egui::Stroke::new(
								SHARE_RING,
								design::mix(glow, egui::Color32::WHITE, 0.6 * fade)
									.gamma_multiply(fade * fade),
							),
							egui::StrokeKind::Middle,
						);
					}
					if live < ARRIVE_SECS {
						ui.ctx().request_repaint();
					}
				}
			}
		}
		if response.clicked() {
			self.screen.launch(state);
		}
	}

	/// How much of the LIVE badge shows for `entry` (0 to 1). This device's own share shows it
	/// only once Discord carries the share and clears it on the stop click; everyone else's
	/// follows their voice state.
	fn live_shown(&self, ctx: &egui::Context, state: &State, entry: &RosterEntry) -> f32 {
		let own_call = state
			.user
			.as_ref()
			.is_some_and(|user| user.id == entry.participant.user)
			&& state
				.voice
				.active
				.as_ref()
				.is_some_and(|call| call.channel == entry.channel);
		if own_call {
			self.screen.live_reveal(ctx)
		} else if entry.participant.streaming {
			1.0
		} else {
			0.0
		}
	}

	fn badge_opacity(&self) -> f32 {
		1.0 - self.voice_chrome_hidden
	}

	/// The DM call this conversation shows as a stage, if any: the joined call, or one that
	/// rings or runs without this device.
	pub(super) fn dm_call_stage(&self, state: &State) -> Option<Id> {
		state
			.voice
			.active
			.as_ref()
			.filter(|call| call.guild.is_none() && Some(call.channel) == state.selected)
			.map(|call| call.channel)
			.or_else(|| join_stage(state))
	}

	/// The direct-message stage is folding away in the open conversation. It keeps the
	/// conversation header until it has.
	pub(super) fn stage_closing(&self, ui: &egui::Ui, selected: Option<Id>) -> bool {
		let motion = &self.voice_stage;
		let now = ui.input(|input| input.time);
		motion.channel == selected
			&& !motion.shot.shapes.is_empty()
			&& (motion.shown || motion.closed.is_some_and(|at| now - at < STAGE_SWEEP_SECS))
	}

	/// Opening progress (0 to 1) of the direct-message stage. The stage unfolds from the
	/// header when a call appears in the open conversation and folds back when it ends;
	/// switching conversations does neither.
	fn stage_sweep(&mut self, ui: &egui::Ui, selected: Option<Id>, showing: bool) -> f32 {
		let now = ui.input(|input| input.time);
		let motion = &mut self.voice_stage;
		if motion.channel != selected {
			*motion = StageMotion {
				channel: selected,
				shown: showing,
				..StageMotion::default()
			};
		}
		if showing && !motion.shown {
			motion.opened = Some(now);
		}
		if showing {
			motion.closed = None;
		} else if motion.shown && !motion.shot.shapes.is_empty() {
			motion.closed = Some(now);
		}
		motion.shown = showing;
		let open = motion
			.opened
			.map_or(1.0, |at| ((now - at) / STAGE_SWEEP_SECS) as f32);
		if open >= 1.0 {
			motion.opened = None;
			return 1.0;
		}
		ui.ctx().request_repaint();
		open
	}

	/// The stage's last frame folding back into the header once its call ended: the seats and
	/// bar fade as the stage shrinks.
	fn closing_stage(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		header: Option<(bool, bool, bool)>,
		commands: &mut Vec<Command>,
	) {
		let Some(at) = self.voice_stage.closed else {
			return;
		};
		let t = ((ui.input(|input| input.time) - at) / STAGE_SWEEP_SECS) as f32;
		if t >= 1.0 {
			self.voice_stage.closed = None;
			self.voice_stage.shot.shapes.clear();
			return;
		}
		ui.ctx().request_repaint();
		let shot = std::mem::take(&mut self.voice_stage.shot);
		let fold = egui::emath::easing::cubic_in_out(t);
		// The stage colour gives way to the conversation over the last part of the fold.
		let fill = 1.0 - ((t - 0.6) / 0.4).clamp(0.0, 1.0);
		egui::Panel::top("dm-call-closing")
			.exact_size(shot.header + (shot.height - shot.header) * (1.0 - fold))
			.show_separator_line(false)
			.frame(egui::Frame::new().fill(STAGE_FILL.gamma_multiply(fill)))
			.show(ui, |ui| {
				let rect = ui.max_rect();
				ui.expand_to_include_rect(rect);
				let mut painter = ui.painter().clone();
				painter.set_opacity(1.0 - egui::emath::easing::cubic_out((t / 0.55).min(1.0)));
				for shape in &shot.shapes {
					painter
						.with_clip_rect(shape.clip_rect)
						.add(shape.shape.clone());
				}
				self.stage_header(ui, state, rect, header, 1.0, commands);
			});
		self.voice_stage.shot = shot;
	}

	/// Rings and halos on call stages: the owner's call colour, else the theme accent lifted
	/// until it stands out on the dark stage.
	fn call_glow(&self, ui: &egui::Ui) -> egui::Color32 {
		self.call_color.map_or_else(
			|| stage_glow(design::palette(ui).accent),
			|[r, g, b]| egui::Color32::from_rgb(r, g, b),
		)
	}

	/// Painter for glows around avatars, clipped to the whole stage while one draws its seats,
	/// so a halo is never cut at a tile's edge.
	fn halo_painter(&self, ui: &egui::Ui) -> egui::Painter {
		let mut painter = ui
			.ctx()
			.layer_painter(ui.layer_id())
			.with_clip_rect(self.voice_halo_clip.unwrap_or_else(|| ui.clip_rect()));
		painter.set_opacity(ui.opacity());
		painter
	}

	/// Who sits on this stage now: forget stale seats, mark those gone since the last pass
	/// so they fade out, and pick how the others glide to their new places: quickly to make
	/// room for someone new, gently while someone who left is still fading.
	fn seat_roster(
		&mut self,
		ctx: &egui::Context,
		channel: Id,
		entries: &[RosterEntry],
	) -> Option<fn(f32) -> f32> {
		let now = ctx.input(|input| input.time);
		let pass = ctx.cumulative_pass_nr();
		self.voice_seats.retain(|seat| {
			seat.left
				.map_or(seat.pass + 1 >= pass, |at| now - at < f64::from(LEAVE_SECS))
		});
		let mut left = false;
		for seat in &mut self.voice_seats {
			if seat.channel == channel
				&& seat.left.is_none()
				&& seat.pass < pass
				&& !entries
					.iter()
					.any(|entry| entry.participant.user == seat.entry.participant.user)
			{
				seat.left = Some(now);
				left = true;
			}
		}
		let arrived = entries.iter().any(|entry| {
			!self.voice_seats.iter().any(|seat| {
				seat.channel == channel
					&& seat.left.is_none()
					&& seat.entry.participant.user == entry.participant.user
			})
		});
		if left {
			Some(egui::emath::easing::cubic_in_out)
		} else if arrived {
			Some(egui::emath::easing::cubic_out)
		} else {
			None
		}
	}

	/// Arrival motion of one seat drawn this pass. Someone new on a stage that was already
	/// showing fades in; the first seats of a stage just appear.
	fn seat_motion(
		&mut self,
		ctx: &egui::Context,
		entry: &RosterEntry,
		waiting: bool,
		avatar: egui::Rect,
	) -> SeatMotion {
		let now = ctx.input(|input| input.time);
		let (channel, user) = (entry.channel, entry.participant.user);
		let showing = self.voice_seats.iter().any(|seat| {
			seat.channel == channel && seat.left.is_none() && seat.entry.participant.user != user
		});
		let index = self.voice_seats.iter().position(|seat| {
			seat.channel == channel && seat.left.is_none() && seat.entry.participant.user == user
		});
		let seat = if let Some(index) = index {
			let seat = &mut self.voice_seats[index];
			if seat.waiting && !waiting {
				seat.arrived = Some(now);
			}
			seat.entry.participant = entry.participant;
			seat
		} else {
			let x = avatar.center().x;
			self.voice_seats.push(Seat {
				channel,
				entry: entry.clone(),
				avatar,
				pass: 0,
				waiting,
				arrived: (showing && !waiting).then_some(now),
				shown: showing.then_some(now),
				left: None,
				x,
				from: x,
				glide: now - f64::from(GLIDE_SECS),
				ease: egui::emath::easing::cubic_out,
			});
			self.voice_seats.last_mut().expect("seat added")
		};
		seat.avatar = avatar;
		seat.pass = ctx.cumulative_pass_nr();
		seat.waiting = waiting;
		let arrived = seat
			.arrived
			.map(|at| (now - at) as f32)
			.filter(|elapsed| *elapsed < ARRIVE_SECS);
		// A new seat fades in once the others have begun to make room for it.
		let appear = seat.shown.map_or(1.0, |at| {
			egui::emath::easing::cubic_out(((now - at) as f32 - 0.08).max(0.0) / APPEAR_SECS)
				.min(1.0)
		});
		if arrived.is_some() || appear < 1.0 {
			ctx.request_repaint();
		}
		SeatMotion { arrived, appear }
	}

	/// Seats slide to their new places when someone joins or leaves instead of jumping; a
	/// resized window moves them at once.
	fn seat_glide(
		&mut self,
		ctx: &egui::Context,
		tile: &Tile<'_>,
		channel: Id,
		rect: egui::Rect,
		changed: Option<fn(f32) -> f32>,
	) -> egui::Rect {
		let Tile::Participant(entry) = tile else {
			return rect;
		};
		let Some(seat) = self.voice_seats.iter_mut().find(|seat| {
			seat.channel == channel
				&& seat.left.is_none()
				&& seat.entry.participant.user == entry.participant.user
		}) else {
			return rect;
		};
		let now = ctx.input(|input| input.time);
		let place = |seat: &Seat| {
			let t = ((now - seat.glide) as f32 / GLIDE_SECS).clamp(0.0, 1.0);
			(seat.from + (seat.x - seat.from) * (seat.ease)(t), t)
		};
		let target = rect.center().x;
		if (target - seat.x).abs() > 0.5 {
			seat.from = if changed.is_some() {
				place(seat).0
			} else {
				target
			};
			seat.ease = changed.unwrap_or(seat.ease);
			seat.x = target;
			seat.glide = now;
		}
		let (x, t) = place(seat);
		if t < 1.0 && seat.from != seat.x {
			ctx.request_repaint();
		}
		rect.translate(egui::vec2(x - target, 0.0))
	}

	/// Seats that left fade and shrink where they sat.
	fn departed_seats(&mut self, ui: &mut egui::Ui, state: &State, channel: Id) {
		let now = ui.input(|input| input.time);
		let gone: Vec<_> = self
			.voice_seats
			.iter()
			.filter(|seat| seat.channel == channel)
			.filter_map(|seat| {
				let t = ((now - seat.left?) as f32 / LEAVE_SECS).min(1.0);
				let size = seat.avatar.size() * (1.0 - 0.12 * egui::emath::easing::cubic_in(t));
				let rest = if seat.waiting { 0.5 } else { 1.0 };
				Some((
					seat.entry.clone(),
					egui::Rect::from_center_size(seat.avatar.center(), size),
					rest * (1.0 - egui::emath::easing::cubic_out(t)),
				))
			})
			.collect();
		if gone.is_empty() {
			return;
		}
		ui.ctx().request_repaint();
		for (entry, rect, opacity) in gone {
			let (user, name) = resolve_member(state, &entry);
			let mut seat_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
			seat_ui.set_opacity(opacity);
			if let Some(user) = user {
				self.avatars
					.show(&mut seat_ui, user, rect.width(), state.demo);
			} else {
				design::avatar(&mut seat_ui, name, rect.width());
			}
		}
	}

	/// The conversation header drawn on the stage, which stays black in every theme.
	fn stage_header(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		rect: egui::Rect,
		header: Option<(bool, bool, bool)>,
		opacity: f32,
		commands: &mut Vec<Command>,
	) {
		let Some((selected_voice, show_members, wide_members)) = header else {
			return;
		};
		let mut header_ui = ui.new_child(
			egui::UiBuilder::new()
				.max_rect(egui::Rect::from_min_max(
					egui::pos2(rect.left() + 16.0, rect.top()),
					egui::pos2(rect.right() - 16.0, rect.top() + 48.0),
				))
				.layout(egui::Layout::left_to_right(egui::Align::Center)),
		);
		header_ui.visuals_mut().dark_mode = true;
		header_ui.set_opacity(opacity);
		self.channel_header_row(
			&mut header_ui,
			state,
			selected_voice,
			show_members,
			wide_members,
			commands,
		);
	}

	/// Discord's join screen: the black stage with whoever is in the call and green buttons
	/// to join with video or voice, plus decline while the call rings here.
	fn call_join_stage(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		channel: Id,
		header: Option<(bool, bool, bool)>,
		open: f32,
		commands: &mut Vec<Command>,
	) {
		let colors = design::palette(ui);
		let incoming = state.voice.incoming == Some(channel);
		let unavailable = self.call_unavailable(state, channel);
		let entries = join_participants(state, channel);
		let header_height = if header.is_some() { 48.0 } else { 0.0 };
		let full = header_height
			+ STAGE_MARGIN
			+ FRAMELESS_HEIGHT
			+ TILE_GAP
			+ CONTROL_HEIGHT
			+ STAGE_MARGIN;
		egui::Panel::top("dm-call-join")
			.exact_size(
				header_height + (full - header_height) * egui::emath::easing::cubic_out(open),
			)
			.show_separator_line(false)
			.frame(egui::Frame::new().fill(STAGE_FILL))
			.show(ui, |ui| {
				let visible = ui.max_rect();
				ui.expand_to_include_rect(visible);
				ui.set_clip_rect(visible.intersect(ui.clip_rect()));
				let rect =
					egui::Rect::from_min_size(visible.min, egui::vec2(visible.width(), full));
				let content = egui::emath::easing::cubic_out((open / 0.7).min(1.0));
				self.stage_header(ui, state, rect, header, 1.0, commands);
				let first = ui.painter().add(egui::Shape::Noop);
				let bar_top = rect.bottom() - STAGE_MARGIN - CONTROL_HEIGHT;
				let body = egui::Rect::from_min_max(
					egui::pos2(
						rect.left() + STAGE_MARGIN,
						rect.top() + header_height + STAGE_MARGIN,
					),
					egui::pos2(rect.right() - STAGE_MARGIN, bar_top - TILE_GAP),
				);
				let mut body_ui = ui.new_child(
					egui::UiBuilder::new()
						.max_rect(body)
						.layout(egui::Layout::top_down(egui::Align::Min)),
				);
				body_ui.set_clip_rect(body.intersect(ui.clip_rect()));
				body_ui.set_opacity(content);
				self.voice_halo_clip = Some(visible);
				self.participant_tiles(&mut body_ui, state, channel, &entries, true);
				self.voice_halo_clip = None;
				let count = if incoming { 3.0 } else { 2.0 };
				let width = JOIN_WIDTH * count + BAR_GAP * (count - 1.0);
				let mut bar_ui = ui.new_child(
					egui::UiBuilder::new()
						.max_rect(egui::Rect::from_min_size(
							egui::pos2(rect.center().x - width * 0.5, bar_top),
							egui::vec2(width, CONTROL_HEIGHT),
						))
						.layout(egui::Layout::left_to_right(egui::Align::Center)),
				);
				bar_ui.spacing_mut().item_spacing.x = BAR_GAP;
				bar_ui.set_opacity(content);
				let video = call_action(
					&mut bar_ui,
					crate::icons::Icon::Video,
					colors.positive,
					unavailable.is_none() && self.voice_camera_available,
					"Join with video",
				)
				.on_disabled_hover_text(unavailable.unwrap_or("No camera is available."));
				let voice = call_action(
					&mut bar_ui,
					crate::icons::Icon::Phone,
					colors.positive,
					unavailable.is_none(),
					if incoming {
						"Answer"
					} else {
						"voice-call-bar-join-call"
					},
				)
				.on_disabled_hover_text(unavailable.unwrap_or(""));
				if incoming
					&& call_action(
						&mut bar_ui,
						crate::icons::Icon::HangUp,
						colors.danger,
						!state.offline_preview(),
						"Decline",
					)
					.clicked()
				{
					state.decline_call();
				}
				if video.clicked() {
					self.voice_camera_on_join = Some(channel);
				}
				if video.clicked() || voice.clicked() {
					self.request_call(state, channel, false, commands);
				}
				self.voice_stage
					.capture(ui, first, visible.height(), header_height);
			});
	}

	/// "Join with video" turns the camera on once that call connects, and forgets the wish
	/// when the call fails or another one takes its place.
	fn apply_camera_on_join(&mut self, state: &mut State) {
		let Some(target) = self.voice_camera_on_join else {
			return;
		};
		let switching = self
			.voice_switch
			.as_ref()
			.is_some_and(|switch| switch.channel == target);
		match state
			.voice
			.active
			.as_ref()
			.filter(|call| call.channel == target)
		{
			Some(_) if state.call_connected() => {
				self.voice_camera_on_join = None;
				state.set_call_camera(true);
			}
			Some(_) => {}
			None if switching => {}
			_ => self.voice_camera_on_join = None,
		}
	}

	/// DM call stage above the conversation, plus the incoming-call banner. `header` carries
	/// the conversation header's flags when the stage draws that header itself.
	pub(super) fn call_bar(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		commands: &mut Vec<Command>,
		header: Option<(bool, bool, bool)>,
	) {
		let colors = design::palette(ui);
		let selected = state.selected;
		self.apply_camera_on_join(state);
		let active = state
			.voice
			.active
			.as_ref()
			.filter(|call| call.guild.is_none() && Some(call.channel) == selected)
			.map(|call| call.channel);
		let join = join_stage(state);
		let open = self.stage_sweep(ui, selected, active.or(join).is_some());
		let header_height = if header.is_some() { 48.0 } else { 0.0 };
		if let Some(channel) = active {
			let stage = self.stage_shows_video(state, channel) || state.is_group_dm(channel);
			// The stage also carries the conversation header when it draws one.
			let height = if stage {
				(ui.available_height() * 0.74).clamp(320.0, 900.0)
			} else {
				// Like Discord's voice call, only as tall as the avatars and the bar: no band of
				// empty stage between them. Dragging the edge still makes it taller.
				STAGE_MARGIN
					+ FRAMELESS_HEIGHT
					+ TILE_GAP + CONTROL_HEIGHT
					+ STAGE_MARGIN + if header.is_some() { 48.0 } else { 0.0 }
			};
			// Dragging the bottom edge resizes the call; video and voice-only keep separate sizes.
			// The conversation and composer below always keep at least 160 points.
			let maximum = (ui.available_height() - 160.0).max(180.0);
			let id = if stage {
				"dm-call-video"
			} else {
				"dm-call-voice"
			};
			// The voice-only strip is never shorter than its avatars and bar. egui remembers a
			// panel's size, so a frame with little room (opening the call while the join stage
			// still took space) used to leave the avatars squeezed for the rest of the session.
			let minimum = if stage { 180.0 } else { height.min(maximum) };
			// A new call unfolds the stage from the header. The sweep draws a fixed-size panel
			// under its own id, so the real one keeps the size the owner dragged it to.
			let full = egui::containers::panel::PanelState::load(
				ui.ctx(),
				ui.scope_id().with_salt(egui::IdSalt::new(id)),
			)
			.map_or(height, |panel| panel.size().y)
			.clamp(minimum, maximum);
			let panel = if open < 1.0 {
				egui::Panel::top((id, "opening")).exact_size(
					header_height + (full - header_height) * egui::emath::easing::cubic_out(open),
				)
			} else {
				egui::Panel::top(id)
					.resizable(true)
					.default_size(height)
					.size_range(minimum..=maximum)
			};
			panel
				.show_separator_line(false)
				.frame(egui::Frame::new().fill(STAGE_FILL))
				.show(ui, |ui| {
					let visible = ui.max_rect();
					// The children below do not grow this Ui; claim the whole panel so its
					// background follows a resize and egui keeps the dragged size.
					ui.expand_to_include_rect(visible);
					ui.set_clip_rect(visible.intersect(ui.clip_rect()));
					// While it opens, the stage is laid out at full size and revealed from the top.
					let rect = egui::Rect::from_min_size(
						visible.min,
						egui::vec2(
							visible.width(),
							if open < 1.0 { full } else { visible.height() },
						),
					);
					let content = egui::emath::easing::cubic_out((open / 0.7).min(1.0));
					let first = ui.painter().add(egui::Shape::Noop);
					let reveal = chrome_reveal(ui, rect, "dm-call-chrome");
					// The header and the bar keep their own strips beside the avatars, so they
					// hide only over an enlarged video, as in Discord.
					let chrome = if self.voice_focus.is_some() {
						reveal
					} else {
						1.0
					};
					let top = rect.top() + if header.is_some() { 48.0 } else { 0.0 };
					let notices = self.stage_notices(state, channel, true);
					let bar_top = rect.bottom() - CONTROL_HEIGHT - STAGE_MARGIN - TILE_GAP;
					let notice_rect = egui::Rect::from_min_max(
						egui::pos2(rect.left() + STAGE_MARGIN, top + STAGE_MARGIN),
						egui::pos2(rect.right() - STAGE_MARGIN, bar_top),
					);
					let mut notice_ui = ui.new_child(
						egui::UiBuilder::new()
							.max_rect(notice_rect)
							.layout(egui::Layout::top_down(egui::Align::Min)),
					);
					notice_ui.set_clip_rect(notice_rect.intersect(ui.clip_rect()));
					notice_ui.set_opacity(content);
					stage_notices(&mut notice_ui, &notices);
					// Without notices the avatars may use the whole height of a shrunken panel.
					let body_top = if notices.is_empty() {
						top + TILE_GAP
					} else {
						notice_ui.cursor().top() + TILE_GAP
					};
					// An enlarged video takes the whole stage under the hover chrome, inside a
					// slim margin; the grid keeps clear of the header and the call bar.
					let body = if self.voice_focus.is_some() {
						egui::Rect::from_min_max(
							rect.min + egui::Vec2::splat(TILE_GAP),
							egui::pos2(
								rect.right() - TILE_GAP,
								if self.voice_focus_participants {
									bar_top
								} else {
									rect.bottom() - TILE_GAP
								},
							),
						)
					} else {
						egui::Rect::from_min_max(
							egui::pos2(rect.left() + STAGE_MARGIN, body_top),
							egui::pos2(rect.right() - STAGE_MARGIN, bar_top.max(body_top)),
						)
					};
					let mut body_ui = ui.new_child(
						egui::UiBuilder::new()
							.max_rect(body)
							.layout(egui::Layout::top_down(egui::Align::Min)),
					);
					body_ui.set_clip_rect(body.intersect(ui.clip_rect()));
					body_ui.set_opacity(content);
					self.voice_chrome_hidden = 1.0 - reveal;
					self.voice_halo_clip = Some(visible);
					self.participant_tiles(
						&mut body_ui,
						state,
						channel,
						&stage_participants(state, channel),
						true,
					);
					self.voice_halo_clip = None;
					self.voice_chrome_hidden = 0.0;
					if self.voice_focus.is_some() {
						chrome_shade(ui, rect, 72.0, true, reveal);
						chrome_shade(ui, rect, 96.0, false, reveal);
					}
					let bar = egui::Rect::from_min_max(
						egui::pos2(rect.left(), rect.bottom() - CONTROL_HEIGHT - STAGE_MARGIN),
						egui::pos2(rect.right(), rect.bottom() - STAGE_MARGIN),
					);
					let mut bar_ui = ui.new_child(
						egui::UiBuilder::new()
							.max_rect(bar)
							.layout(egui::Layout::left_to_right(egui::Align::Center)),
					);
					bar_ui.set_opacity(chrome * content);
					self.call_controls(&mut bar_ui, state, channel);
					self.voice_stage
						.capture(ui, first, visible.height(), header_height);
					self.stage_header(ui, state, rect, header, chrome, commands);
				});
			self.apply_watch_request(state);
		}
		if let Some(channel) = join {
			self.call_join_stage(ui, state, channel, header, open, commands);
		} else if active.is_none() {
			self.closing_stage(ui, state, header, commands);
		}
		// A call ringing in another conversation keeps the slim banner.
		if let Some(channel) = state
			.voice
			.incoming
			.filter(|channel| join != Some(*channel))
		{
			let conversation = state.channel(channel).cloned();
			let name = state
				.channels
				.iter()
				.find(|c| c.id == channel)
				.map_or("Direct message", |c| c.name.as_str())
				.to_owned();
			let unavailable = self.call_unavailable(state, channel);
			egui::Panel::top("dm-incoming")
				.show_separator_line(false)
				.frame(
					egui::Frame::new()
						.fill(colors.raised)
						.inner_margin(egui::Margin::symmetric(16, 10)),
				)
				.show(ui, |ui| {
					ui.horizontal(|ui| {
						ui.spacing_mut().item_spacing.x = 12.0;
						match conversation.as_ref() {
							Some(group) if group.kind == 3 => {
								self.avatars.show_group(ui, group, 40.0, state.demo);
							}
							Some(dm) if dm.kind == 1 && !dm.recipients.is_empty() => {
								let user = &dm.recipients[0];
								self.avatars.show(ui, user, 40.0, state.demo);
							}
							_ => {
								design::avatar(ui, &name, 40.0);
							}
						}
						ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
							ui.spacing_mut().item_spacing.x = 8.0;
							let decline = call_action(
								ui,
								crate::icons::Icon::HangUp,
								colors.danger,
								!state.offline_preview(),
								"Decline",
							);
							if decline.clicked() {
								state.decline_call();
							}
							let answer = call_action(
								ui,
								crate::icons::Icon::Phone,
								colors.positive,
								unavailable.is_none(),
								"Answer",
							)
							.on_disabled_hover_text(unavailable.unwrap_or(""));
							if answer.clicked() {
								self.request_call(state, channel, false, commands);
							}
							ui.with_layout(
								egui::Layout::left_to_right(egui::Align::Center),
								|ui| {
									ui.vertical(|ui| {
										ui.spacing_mut().item_spacing.y = 2.0;
										ui.add(
											egui::Label::new(
												design::semibold(ui, &name, 15.0)
													.color(colors.text_strong),
											)
											.truncate(),
										);
										ui.label(
											RichText::new(crate::i18n::translate_if_key(
												unavailable
													.unwrap_or("voice-call-bar-incoming-call"),
											))
											.size(13.0)
											.color(colors.muted),
										);
									});
								},
							);
						});
					});
				});
		}
	}

	/// Call details drawn inside the account card while connected: Discord's "Voice
	/// Connected" header, then a row of quick actions above the identity row.
	pub(super) fn voice_card_section(&mut self, ui: &mut egui::Ui, state: &mut State) {
		let Some(call) = state.voice.active.as_ref() else {
			return;
		};
		let colors = design::palette(ui);
		let phase = call.phase;
		let channel_id = call.channel;
		let camera = call.camera;
		let connected = matches!(phase, Phase::Connected | Phase::Waiting);
		let channel = state
			.channel(call.channel)
			.map_or("Direct message", |c| c.name.as_str())
			.to_owned();
		let guild = call
			.guild
			.and_then(|id| state.guild(id))
			.map(|g| g.name.clone());
		let detail = match guild {
			Some(guild) => format!("{channel} / {guild}"),
			None => channel,
		};
		let title = if state.offline_preview() {
			"Voice preview"
		} else if connected {
			"Voice Connected"
		} else {
			"Connecting…"
		};
		// The connection colour eases from amber to green as the call connects.
		let settled = ui.ctx().animate_bool_with_time(
			egui::Id::unique(("voice-card-connected", channel_id)),
			connected || state.offline_preview(),
			0.3,
		);
		let color = colors.warning.lerp_to_gamma(colors.positive, settled);
		egui::Frame::new()
			.inner_margin(egui::Margin {
				left: 8,
				right: 8,
				top: 8,
				bottom: 8,
			})
			.show(ui, |ui| {
				ui.set_width(ui.available_width());
				ui.spacing_mut().item_spacing.y = 8.0;
				if self.voice_microphone_unavailable {
					ui.label(
						RichText::new(crate::i18n::translate(
							"voice-voice-card-section-microphone-unavailable-a-still-connected-choose-another-input-in-audio",
						))
						.size(12.0)
						.color(colors.warning),
					);
				}
				let processing = state.demo || self.voice_available;
				ui.horizontal(|ui| {
					ui.spacing_mut().item_spacing.x = 10.0;
					// Square status tile like Discord's, tinted with the connection colour.
					let (tile, _) =
						ui.allocate_exact_size(egui::Vec2::splat(40.0), egui::Sense::hover());
					// While connecting the tile breathes, so the wait reads as progress.
					let breath = if settled < 1.0 {
						ui.ctx().request_repaint();
						let time = ui.input(|input| input.time);
						(1.0 - settled) * (0.5 + 0.5 * (time * 4.0).sin() as f32)
					} else {
						0.0
					};
					ui.painter()
						.rect_filled(tile, 10, color.gamma_multiply(0.16 + 0.14 * breath));
					crate::icons::paint(
						ui.painter(),
						crate::icons::Icon::Signal,
						tile.shrink(10.0),
						color,
					);
					ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
						ui.spacing_mut().item_spacing.x = 6.0;
						let leave = ui
							.add_enabled_ui(!state.offline_preview(), |ui| {
								let label =
									crate::i18n::translate_if_key("voice-voice-card-section-disconnect");
								let (rect, response) = ui
									.allocate_exact_size(egui::Vec2::splat(32.0), egui::Sense::click());
								// Quiet like the rest of Discord's panel header; it turns red only
								// under the pointer, where a click would end the call.
								let (hover, press) = if ui.is_enabled() {
									(
										design::hover_motion(ui, &response),
										design::press_motion(ui, &response),
									)
								} else {
									(0.0, 0.0)
								};
								if hover > 0.0 {
									ui.painter().rect_filled(
										rect,
										crate::icons::PLATE_RADIUS,
										colors.danger.gamma_multiply(0.18 * hover + 0.12 * press),
									);
								}
								crate::icons::paint(
									ui.painter(),
									crate::icons::Icon::HangUp,
									design::pressed_rect(rect.shrink(6.0), press),
									if ui.is_enabled() {
										colors.muted.lerp_to_gamma(colors.danger, hover)
									} else {
										colors.muted.gamma_multiply(0.5)
									},
								);
								response.widget_info(|| {
									egui::WidgetInfo::labeled(
										egui::Role::Button,
										ui.is_enabled(),
										&label,
									)
								});
								response.on_hover_text(label)
							})
							.inner;
						if leave.clicked() {
							state.leave_call();
						}
						// Noise suppression sits beside disconnect, as in Discord's panel. It opens
						// a popover with the filter choice; the glyph shows whether it is on.
						let suppressing =
							self.voice_processing.effective().suppression != NoiseSuppression::Off;
						let suppression = ui
							.add_enabled_ui(processing, |ui| {
								let label = crate::i18n::translate_if_key("Noise suppression");
								let (rect, response) = ui
									.allocate_exact_size(egui::Vec2::splat(32.0), egui::Sense::click());
								let open = egui::Popup::is_id_open(
									ui.ctx(),
									egui::Popup::default_response_id(&response),
								);
								ui.painter().rect_filled(
									rect,
									crate::icons::PLATE_RADIUS,
									crate::icons::plate(ui, &response, open),
								);
								let hot = response.hovered() || response.has_focus() || open;
								// Discord's waveform stays grey; the slash marks it off.
								let icon = if suppressing {
									crate::icons::Icon::Noise
								} else {
									crate::icons::Icon::NoiseOff
								};
								let ink = if hot { colors.text_strong } else { colors.muted };
								crate::icons::paint(ui.painter(), icon, rect.shrink(6.0), ink);
								response.widget_info(|| {
									egui::WidgetInfo::selected(
										egui::Role::Button,
										ui.is_enabled(),
										suppressing,
										&label,
									)
								});
								if open {
									response
								} else {
									response.on_hover_text(label)
								}
							})
							.inner
							.on_disabled_hover_text(crate::i18n::translate_if_key(
								"Noise suppression is unavailable in this build or preview.",
							));
						self.noise_popup(&suppression);
						ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
							ui.vertical(|ui| {
								ui.spacing_mut().item_spacing.y = 0.0;
								ui.add(
									egui::Label::new(
										design::semibold(ui, title, 15.0).color(color),
									)
									.truncate()
									.selectable(false),
								);
								ui.add(
									egui::Label::new(
										RichText::new(detail).size(12.0).color(colors.muted),
									)
									.truncate()
									.selectable(false),
								);
							});
						});
					});
				});
				let controls = self.controls_enabled(state);
				let can_camera = self.voice_camera_available
					&& state.can_camera(channel_id)
					&& matches!(phase, Phase::Connected | Phase::Waiting);
				let can_share = self.screen.busy
					|| state.offline_preview() || (self.screen.supported
					&& matches!(phase, Phase::Connected | Phase::Waiting)
					&& state.can_stream(channel_id));
				let mut camera_clicked = false;
				let mut share_clicked = false;
				ui.horizontal(|ui| {
					ui.spacing_mut().item_spacing.x = 8.0;
					// Two equal halves. Camera and its settings chevron form one split cell
					// with a hairline seam; share fills the other half.
					const CHEVRON: f32 = 28.0;
					const SEAM: f32 = 1.0;
					let width = ((ui.available_width() - 8.0) / 2.0).max(CHEVRON + SEAM + 24.0);
					let whole = egui::CornerRadius::same(crate::icons::PLATE_RADIUS);
					camera_clicked = card_action(
						ui,
						width - SEAM - CHEVRON,
						egui::CornerRadius {
							ne: 0,
							se: 0,
							..whole
						},
						if camera {
							crate::icons::Icon::Video
						} else {
							crate::icons::Icon::VideoSlash
						},
						controls && (camera || can_camera),
						camera,
						if camera {
							"Turn off camera"
						} else {
							"Turn on camera"
						},
						if camera {
							"Stop sharing your camera"
						} else if state.offline_preview() {
							"Camera is off in the offline preview"
						} else if !self.voice_camera_available {
							"Camera requires H264 support from the voice server"
						} else if !state.can_camera(channel_id) {
							"Camera is unavailable with current channel permissions"
						} else {
							"Share your selected camera with this call"
						},
					)
					.clicked();
					ui.add_space(SEAM - 8.0);
					let rest = design::palette(ui).text_strong.gamma_multiply(0.07);
					let camera_settings = split_chevron(
						ui,
						egui::vec2(CHEVRON, CARD_ACTION_HEIGHT),
						rest,
						"voice-voice-card-section-camera-settings",
					);
					self.camera_settings_popup(&camera_settings, state.demo);
					share_clicked = card_action(
						ui,
						width,
						whole,
						crate::icons::Icon::ScreenShare,
						can_share,
						self.screen.busy,
						if self.screen.busy {
							"Stop sharing"
						} else {
							"Share your screen"
						},
						if can_share {
							if let Some(status) = self.screen.capture_status {
								status
							} else if self.screen.busy {
								"Stop sharing your screen"
							} else {
								"Share a screen or window"
							}
						} else {
							"Screen sharing requires a connected call and video permission on a supported desktop."
						},
					)
					.clicked();
				});
				if camera_clicked && state.set_call_camera(!camera) {
					self.voice_camera_status = "";
				}
				if share_clicked {
					self.screen.launch(state);
				}
			});
		if connected && ui.is_rect_visible(ui.max_rect()) {
			ui.ctx()
				.request_repaint_after(std::time::Duration::from_secs(1));
		}
	}

	/// Mic or headphones toggle joined to Discord's small chevron opening voice settings.
	pub(super) fn mute_toggle_with_settings(
		&mut self,
		ui: &mut egui::Ui,
		state: &mut State,
		deafen: bool,
	) {
		let colors = design::palette(ui);
		// One plate behind both halves, so the pair reads as a single control; red while latched.
		let plate = ui.painter().add(egui::Shape::Noop);
		let spacing = std::mem::replace(&mut ui.spacing_mut().item_spacing.x, 0.0);
		let chevron = split_chevron(
			ui,
			egui::vec2(18.0, 32.0),
			egui::Color32::TRANSPARENT,
			if deafen {
				"voice-mute-toggle-with-settings-output-settings"
			} else {
				"voice-mute-toggle-with-settings-input-settings"
			},
		);
		self.voice_settings_popup(&chevron, state.demo, state.voice.active.is_some(), !deafen);
		let toggle = self.mute_toggle(ui, state, deafen, 32.0);
		ui.spacing_mut().item_spacing.x = spacing;
		let latched = self.voice_deafened || (!deafen && self.voice_muted);
		ui.painter().set(
			plate,
			egui::Shape::rect_filled(
				chevron.rect.union(toggle.rect),
				crate::icons::PLATE_RADIUS,
				if latched {
					colors.danger.gamma_multiply(0.18)
				} else {
					colors.text_strong.gamma_multiply(0.06)
				},
			),
		);
	}
}

/// Height of the quick-action cells in the in-call account card.
const CARD_ACTION_HEIGHT: f32 = 36.0;

/// Resting, hovered and pressed fill of a quick-action cell: a veil of the text colour, so the
/// cell reads on the `raised` card, or of the accent while the feature is on.
fn card_action_fill(
	ui: &egui::Ui,
	response: &egui::Response,
	enabled: bool,
	active: bool,
) -> egui::Color32 {
	let colors = design::palette(ui);
	let (tone, rest) = if active && enabled {
		(colors.accent, 0.24)
	} else {
		(colors.text_strong, 0.07)
	};
	tone.gamma_multiply(if enabled {
		rest + 0.07 * design::hover_motion(ui, response) + 0.05 * design::press_motion(ui, response)
	} else {
		0.04
	})
}

/// Quick action in the in-call account card: filled cell, accent plate while the feature is on.
/// `radius` squares the edge that joins a [`split_chevron`].
#[allow(clippy::too_many_arguments)]
fn card_action(
	ui: &mut egui::Ui,
	width: f32,
	radius: egui::CornerRadius,
	icon: crate::icons::Icon,
	enabled: bool,
	active: bool,
	label: &str,
	hint: &str,
) -> egui::Response {
	let label = crate::i18n::translate_if_key(label);
	let hint = crate::i18n::translate_if_key(hint);
	let colors = design::palette(ui);
	let (rect, response) = ui.allocate_exact_size(
		egui::vec2(width, CARD_ACTION_HEIGHT),
		if enabled {
			egui::Sense::click()
		} else {
			egui::Sense::hover()
		},
	);
	ui.painter().rect_filled(
		rect,
		radius,
		card_action_fill(ui, &response, enabled, active),
	);
	let color = if !enabled {
		colors.muted.gamma_multiply(0.5)
	} else {
		colors.text_strong
	};
	crate::icons::paint(
		ui.painter(),
		icon,
		egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(20.0)),
		color,
	);
	response
		.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, enabled, active, &label));
	response.on_hover_text(hint)
}

/// Settings chevron forming the right half of a split control. `rest` is its resting fill;
/// pass transparent when the caller paints one plate behind both halves.
fn split_chevron(
	ui: &mut egui::Ui,
	size: egui::Vec2,
	rest: egui::Color32,
	label: &str,
) -> egui::Response {
	let label = crate::i18n::translate_if_key(label);
	let colors = design::palette(ui);
	let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
	let hot = response.hovered() || response.has_focus();
	let corners = egui::CornerRadius {
		nw: 0,
		sw: 0,
		ne: crate::icons::PLATE_RADIUS,
		se: crate::icons::PLATE_RADIUS,
	};
	// The hover veil fades in over the resting fill rather than replacing it.
	ui.painter().rect_filled(rect, corners, rest);
	ui.painter()
		.rect_filled(rect, corners, crate::icons::plate(ui, &response, false));
	crate::icons::paint(
		ui.painter(),
		crate::icons::Icon::ChevronDown,
		egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(12.0)),
		if hot {
			colors.text_strong
		} else {
			colors.muted
		},
	);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
	response.on_hover_text(label)
}

/// The call stage is dark in every appearance, like Discord's, but in the house plum cast
/// instead of black and neutral grey; pills and tiles sit on it in fixed tones.
const STAGE_FILL: egui::Color32 = egui::Color32::from_rgb(0x0a, 0x06, 0x08);
const TILE_FILL: egui::Color32 = egui::Color32::from_rgb(0x24, 0x18, 0x1e);
const PILL_FILL: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x13, 0x18);
const STAGE_TEXT: egui::Color32 = egui::Color32::from_rgb(0xec, 0xe2, 0xe6);
const STAGE_MUTED: egui::Color32 = egui::Color32::from_rgb(0xa3, 0x93, 0x9b);
/// Hairline around stage tiles and pills, the stage's share of the house edge.
const STAGE_EDGE: egui::Stroke = egui::Stroke {
	width: 1.0,
	color: egui::Color32::from_rgba_premultiplied(18, 18, 18, 18),
};
const STAGE_MARGIN: f32 = 16.0;
const TILE_GAP: f32 = 8.0;
/// Frameless DM tile height (an 80-point avatar) and the extra gap between avatars.
const FRAMELESS_HEIGHT: f32 = 88.0;
const FRAMELESS_MARGIN: f32 = 8.0;
/// Width of the green join and red decline buttons on the join stage.
const JOIN_WIDTH: f32 = 64.0;
/// Below these, the enlarged view hides its participant strip instead of overlapping it.
const MIN_STRIP: f32 = 64.0;
const MIN_FOCUS: f32 = 80.0;
/// Height shared by every control, pill and the hang-up button in the call bar.
const CONTROL_HEIGHT: f32 = 48.0;
const HANG_UP: f32 = 72.0;
const BAR_GAP: f32 = 12.0;

/// Green ring like Discord's speaking indicator; the accessible label still names the state.
/// Which stage tile is enlarged; cleared automatically when it stops showing video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageFocus {
	LocalScreen,
	Stream(Id),
	Participant(Id),
}

enum Tile<'a> {
	LocalScreen,
	Stream(Id),
	Participant(&'a RosterEntry),
}
impl Tile<'_> {
	fn focus(&self) -> StageFocus {
		match self {
			Self::LocalScreen => StageFocus::LocalScreen,
			Self::Stream(streamer) => StageFocus::Stream(*streamer),
			Self::Participant(entry) => StageFocus::Participant(entry.participant.user),
		}
	}
	/// Stable interaction identity, so a tile keeps its hover while the layout changes.
	fn key(&self) -> (u8, u64) {
		match self.focus() {
			StageFocus::LocalScreen => (0, 0),
			StageFocus::Stream(user) => (1, user.0),
			StageFocus::Participant(user) => (2, user.0),
		}
	}
}

/// Avatar diameter for a stage tile, never larger than the tile itself.
fn tile_avatar(size: egui::Vec2, frameless: bool) -> f32 {
	// Frameless DM avatars fill their tile like Discord's; no name sits under them.
	let avatar = if frameless {
		(size.y - 8.0).clamp(24.0, 96.0)
	} else if size.y < 132.0 {
		(size.y * 0.5).clamp(28.0, 48.0)
	} else {
		(size.y * 0.42).clamp(48.0, 128.0)
	};
	avatar.min(size.x - 8.0).min(size.y - 8.0).max(12.0)
}

/// Frameless DM avatars: Discord-tight columns (avatar plus a small name margin), wrapping
/// and shrinking so every row stays inside `area`.
fn frameless_fit(count: usize, area: egui::Vec2) -> (usize, egui::Vec2) {
	let count = count.max(1);
	let mut height = area.y.min(FRAMELESS_HEIGHT);
	loop {
		let width = tile_avatar(egui::vec2(f32::INFINITY, height), true) + FRAMELESS_MARGIN;
		let columns = (((area.x + TILE_GAP) / (width + TILE_GAP)).floor() as usize).clamp(1, count);
		let rows = count.div_ceil(columns);
		let needed = height * rows as f32 + TILE_GAP * (rows as f32 - 1.0);
		if needed <= area.y || height <= 24.0 {
			let fitted = ((area.y - TILE_GAP * (rows as f32 - 1.0)) / rows as f32).min(height);
			return (columns, egui::vec2(width.min(area.x), fitted));
		}
		height = (height - 8.0).max(24.0);
	}
}

/// Column count and tile size that make the tiles largest inside `area`, at 16:9.
fn best_fit(count: usize, area: egui::Vec2, cap: f32) -> (usize, egui::Vec2) {
	let mut best = (1, egui::Vec2::ZERO);
	for columns in 1..=count.max(1) {
		let rows = count.div_ceil(columns);
		let width = (area.x - TILE_GAP * (columns - 1) as f32) / columns as f32;
		let height = (area.y - TILE_GAP * (rows - 1) as f32) / rows as f32;
		if width <= 0.0 || height <= 0.0 {
			continue;
		}
		let width = width.min(height * 16.0 / 9.0).min(cap);
		// Prefer side-by-side tiles when the size cap makes multiple layouts tie.
		if width >= best.1.x {
			best = (columns, egui::vec2(width, width * 9.0 / 16.0));
		}
	}
	if best.1.x <= 0.0 {
		best = (count.max(1), egui::Vec2::ZERO);
	}
	best
}

/// The largest centred rectangle of the image's aspect ratio inside `rect`.
fn fit_rect(rect: egui::Rect, image: egui::Vec2) -> egui::Rect {
	if image.x <= 0.0 || image.y <= 0.0 {
		return rect;
	}
	let size = image * (rect.width() / image.x).min(rect.height() / image.y);
	egui::Rect::from_center_size(rect.center(), size)
}

/// Fill the tile edge to edge, cropping the overflow (Discord's camera framing).
fn cover_image(
	ui: &mut egui::Ui,
	rect: egui::Rect,
	id: egui::TextureId,
	image: egui::Vec2,
	mirror: bool,
) {
	if image.x <= 0.0 || image.y <= 0.0 {
		return;
	}
	let scale = (rect.width() / image.x).max(rect.height() / image.y);
	let shown = egui::vec2(
		(rect.width() / (image.x * scale)).min(1.0),
		(rect.height() / (image.y * scale)).min(1.0),
	);
	let mut uv = egui::Rect::from_center_size(egui::pos2(0.5, 0.5), shown);
	if mirror {
		std::mem::swap(&mut uv.min.x, &mut uv.max.x);
	}
	ui.put(
		rect,
		egui::Image::from_texture((id, rect.size()))
			.uv(uv)
			.corner_radius(10),
	);
}

/// Bottom-left translucent name plate, optionally with the mute glyph.
fn name_badge(
	ui: &mut egui::Ui,
	rect: egui::Rect,
	name: &str,
	icon: Option<crate::icons::Icon>,
	opacity: f32,
) {
	if opacity <= 0.0 {
		return;
	}
	let mut painter = ui.painter().clone();
	painter.multiply_opacity(opacity);
	let font = egui::FontId::new(13.0, design::medium_family(ui.ctx()));
	let icon_width = if icon.is_some() { 20.0 } else { 0.0 };
	// One line, truncated with an ellipsis; the tile hover text carries the full name.
	let mut job = egui::text::LayoutJob::simple_singleline(name.to_owned(), font, STAGE_TEXT);
	job.wrap.max_width = (rect.width() * 0.6 - icon_width).max(20.0);
	job.wrap.max_rows = 1;
	job.wrap.break_anywhere = true;
	let galley = ui.painter().layout_job(job);
	let badge = egui::Rect::from_min_size(
		rect.left_bottom() + egui::vec2(8.0, -8.0 - 24.0),
		egui::vec2(galley.size().x + 16.0 + icon_width, 24.0),
	);
	painter.rect_filled(badge, 6, egui::Color32::from_black_alpha(160));
	painter.galley(
		egui::pos2(badge.left() + 8.0, badge.center().y - galley.size().y * 0.5),
		galley,
		STAGE_TEXT,
	);
	if let Some(icon) = icon {
		crate::icons::paint(
			&painter,
			icon,
			egui::Rect::from_center_size(
				egui::pos2(badge.right() - 14.0, badge.center().y),
				egui::Vec2::splat(14.0),
			),
			design::palette(ui).danger,
		);
	}
}

/// Call chrome fades in while the pointer is over the stage or one of its menus is open.
fn chrome_reveal(ui: &egui::Ui, stage: egui::Rect, id: &str) -> f32 {
	let shown = ui.rect_contains_pointer(stage) || egui::Popup::is_any_open(ui.ctx());
	ui.ctx()
		.animate_bool_with_time(egui::Id::unique(id), shown, 0.15)
}

/// A dark gradient behind the header (`top`) or the call bar over an enlarged video.
fn chrome_shade(ui: &egui::Ui, stage: egui::Rect, height: f32, top: bool, reveal: f32) {
	if reveal <= 0.0 {
		return;
	}
	let dark = egui::Color32::from_black_alpha((170.0 * reveal) as u8);
	let rect = if top {
		egui::Rect::from_min_size(stage.min, egui::vec2(stage.width(), height))
	} else {
		egui::Rect::from_min_max(egui::pos2(stage.left(), stage.bottom() - height), stage.max)
	};
	let (start, end) = if top {
		(dark, egui::Color32::TRANSPARENT)
	} else {
		(egui::Color32::TRANSPARENT, dark)
	};
	let mut mesh = egui::Mesh::default();
	mesh.colored_vertex(rect.left_top(), start);
	mesh.colored_vertex(rect.right_top(), start);
	mesh.colored_vertex(rect.left_bottom(), end);
	mesh.colored_vertex(rect.right_bottom(), end);
	mesh.add_triangle(0, 1, 2);
	mesh.add_triangle(1, 2, 3);
	ui.painter().add(mesh);
}

/// Width of the join rings, the same as the speaking ring's.
const JOIN_RING: f32 = 3.0;
/// Width of the Go Live button's loading ring.
const SHARE_RING: f32 = 2.0;
/// Seconds from asking for a cue until it is heard (a fresh output stream plus the device
/// buffer). Visual beats aim at the audible notes.
const CUE_LATENCY: f32 = 0.03;
/// Discord's join chime has two notes 85 ms apart: the ring closes on the first and the
/// avatar springs on the second.
const CHIME_NOTE: f32 = 0.085;
/// Length of a seat's arrival: ring, spring and halo.
const ARRIVE_SECS: f32 = 0.8;
/// Fade-in of a waiting seat that appears on a stage already showing.
const APPEAR_SECS: f32 = 0.25;
/// Fade-out of a seat whose person left.
const LEAVE_SECS: f32 = 0.28;
/// Slide of the remaining seats after someone joins or leaves.
const GLIDE_SECS: f32 = 0.32;
/// Size of a seat that is connecting or only being rung, against its full size.
const WAITING_SCALE: f32 = 0.92;
/// Opening and closing of the direct-message stage.
const STAGE_SWEEP_SECS: f64 = 0.32;
/// Discord's outgoing ring cycle: one burst of notes at each offset; the desktop's ring
/// timer uses the same three seconds.
const OUTGOING_RING_CYCLE: f64 = 3.0;
const RING_BURSTS: [f64; 2] = [0.24, 0.93];
/// How long one ring pulse takes to spread and fade.
const RIPPLE_SECS: f64 = 1.1;

/// One seat on a call stage, remembered across frames so arrivals and departures animate.
pub(crate) struct Seat {
	channel: Id,
	entry: RosterEntry,
	/// Where its avatar was last drawn, at full size.
	avatar: egui::Rect,
	/// Pass in which it was last drawn.
	pass: u64,
	/// Connecting (the own seat) or only being rung.
	waiting: bool,
	/// When it arrived on a stage that was already showing.
	arrived: Option<f64>,
	/// When it appeared on a stage that was already showing.
	shown: Option<f64>,
	/// When its person left; the seat then fades out where it sat.
	left: Option<f64>,
	/// Horizontal glide from `from` to the layout's `x`, started at `glide`.
	x: f32,
	from: f32,
	glide: f64,
	ease: fn(f32) -> f32,
}

struct SeatMotion {
	/// Seconds since the seat arrived, while its arrival plays.
	arrived: Option<f32>,
	/// Fade-in of a seat new to the stage.
	appear: f32,
}

/// Opening and closing of the direct-message stage in the open conversation.
#[derive(Default)]
pub(crate) struct StageMotion {
	channel: Option<Id>,
	/// The stage was drawn in the previous pass.
	shown: bool,
	opened: Option<f64>,
	closed: Option<f64>,
	/// The last drawn stage, replayed while it closes.
	shot: Shot,
}

#[derive(Default)]
struct Shot {
	height: f32,
	header: f32,
	/// Shapes of the seats and the bar; the header stays live.
	shapes: Vec<egui::epaint::ClippedShape>,
}

impl StageMotion {
	/// Keep the seats and bar painted after `first` this frame for the closing fold.
	fn capture(&mut self, ui: &egui::Ui, first: egui::layers::ShapeIdx, height: f32, header: f32) {
		let shot = &mut self.shot;
		shot.height = height;
		shot.header = header;
		shot.shapes.clear();
		ui.ctx().graphics(|graphics| {
			if let Some(list) = graphics.get(ui.layer_id()) {
				// ponytail: capped copy of one frame's stage shapes, ample for 64 seats.
				shot.shapes
					.extend(list.all_entries().skip(first.0 + 1).take(4096).cloned());
			}
		});
	}
}

/// The theme accent lifted toward white until it stands out on the dark stage.
pub(crate) fn stage_glow(accent: egui::Color32) -> egui::Color32 {
	let mut glow = accent.to_opaque();
	for _ in 0..8 {
		if design::contrast(glow, STAGE_FILL) >= 6.0 {
			break;
		}
		glow = glow.lerp_to_gamma(egui::Color32::WHITE, 0.12);
	}
	glow
}

/// Seat scale `elapsed` seconds into its arrival: an underdamped spring from the waiting size
/// that overshoots by about two percent at the chime's second note and settles in under half
/// a second.
fn arrival_scale(elapsed: f32) -> f32 {
	const OMEGA: f32 = 24.0;
	const ZETA: f32 = 0.42;
	let t = (elapsed - CUE_LATENCY).max(0.0);
	let damped = OMEGA * (1.0 - ZETA * ZETA).sqrt();
	let envelope = (-ZETA * OMEGA * t).exp();
	1.0 - (1.0 - WAITING_SCALE)
		* envelope
		* ((damped * t).cos() + ZETA * OMEGA / damped * (damped * t).sin())
}

/// Progress (0 to 1) of the ring pulses spreading `since` seconds into the outgoing ring
/// cycle, one per burst of the ring sound. Repaints only while one spreads.
fn ring_ripples(ctx: &egui::Context, since: f64) -> [Option<f32>; 2] {
	let ripples = RING_BURSTS.map(|burst| {
		let after = since - burst - f64::from(CUE_LATENCY);
		(after >= 0.0)
			.then(|| after % OUTGOING_RING_CYCLE)
			.filter(|after| *after < RIPPLE_SECS)
			.map(|after| (after / RIPPLE_SECS) as f32)
	});
	if ripples.iter().any(Option::is_some) {
		ctx.request_repaint();
	} else {
		let next = RING_BURSTS
			.iter()
			.map(|burst| (burst + f64::from(CUE_LATENCY) - since).rem_euclid(OUTGOING_RING_CYCLE))
			.fold(OUTGOING_RING_CYCLE, f64::min);
		ctx.request_repaint_after(std::time::Duration::from_secs_f64(next));
	}
	ripples
}

/// A soft glow hugging the avatar: a band fading from `color` at the avatar edge to nothing
/// further out, spreading with `progress` while it fades. It never spreads past the painter's
/// clip, so it is never cut off.
fn halo(
	painter: &egui::Painter,
	avatar: egui::Rect,
	color: egui::Color32,
	progress: f32,
	strength: f32,
) {
	const SEGMENTS: u32 = 72;
	let clip = painter.clip_rect();
	let room = (avatar.top() - clip.top())
		.min(clip.bottom() - avatar.bottom())
		.min(avatar.left() - clip.left())
		.min(clip.right() - avatar.right())
		.clamp(0.0, 14.0);
	let alpha = strength * (1.0 - progress).powi(2);
	if room < 2.0 || alpha < 0.01 {
		return;
	}
	let center = avatar.center();
	let inner = avatar.width() * 0.5;
	let outer = inner + 2.0 + (room - 2.0) * egui::emath::easing::cubic_out(progress);
	let solid = color.gamma_multiply(alpha);
	let mut mesh = egui::Mesh::default();
	for step in 0..=SEGMENTS {
		let angle = std::f32::consts::TAU * step as f32 / SEGMENTS as f32;
		let direction = egui::vec2(angle.cos(), angle.sin());
		mesh.colored_vertex(center + direction * inner, solid);
		mesh.colored_vertex(center + direction * outer, egui::Color32::TRANSPARENT);
		if step > 0 {
			let base = step * 2;
			mesh.add_triangle(base - 2, base - 1, base);
			mesh.add_triangle(base - 1, base, base + 1);
		}
	}
	painter.add(mesh);
}

/// The own avatar while its call connects: a comet with a fading tail circling just inside
/// `avatar`'s edge over a faint track, `ring` points wide. `close` (0 to 1) grows the tail
/// into an even, full ring.
fn connecting_comet(
	ui: &egui::Ui,
	avatar: egui::Rect,
	color: egui::Color32,
	close: f32,
	ring: f32,
) {
	let center = avatar.center();
	let radius = avatar.width() * 0.5 - ring * 0.5;
	comet(
		ui,
		|lap| center + radius * egui::Vec2::angled(lap * std::f32::consts::TAU),
		color,
		close,
		ring,
	);
}

/// A comet running clockwise round a closed outline over a faint track, `width` points wide.
/// `outline(lap)` is the point a fraction `lap` of the way round, for any real `lap`.
/// `close` (0 to 1) grows the tail into the whole, even outline.
fn comet(
	ui: &egui::Ui,
	outline: impl Fn(f32) -> egui::Pos2,
	color: egui::Color32,
	close: f32,
	width: f32,
) {
	const SEGMENTS: usize = 64;
	// Laps per second, and the tail's share of a lap.
	const SPEED: f64 = 0.67;
	const TAIL: f32 = 0.73;
	let time = ui.input(|input| input.time);
	let head = (time * SPEED).fract() as f32;
	let painter = ui.painter();
	painter.add(egui::Shape::closed_line(
		(0..SEGMENTS)
			.map(|step| outline(step as f32 / SEGMENTS as f32))
			.collect(),
		egui::Stroke::new(width, color.gamma_multiply(0.14 * (1.0 - close))),
	));
	// One gradient stroke: the tail warms toward a lighter tip and fades out behind it.
	let tip = design::mix(color, egui::Color32::WHITE, 0.45);
	let length = TAIL + (0.998 - TAIL) * close;
	let points: Vec<egui::Pos2> = (0..=SEGMENTS)
		.map(|step| outline(head - length + length * step as f32 / SEGMENTS as f32))
		.collect();
	let samples = points.clone();
	painter.add(egui::Shape::line(
		points,
		egui::epaint::PathStroke::new_uv(width, move |_, pos| {
			// The stroke's vertices sit on the samples, so the nearest one says how far along.
			let nearest = (0..=SEGMENTS)
				.min_by(|&a, &b| {
					samples[a]
						.distance_sq(pos)
						.total_cmp(&samples[b].distance_sq(pos))
				})
				.unwrap_or(0);
			let along = nearest as f32 / SEGMENTS as f32;
			color
				.lerp_to_gamma(tip, along * along * (1.0 - close))
				.gamma_multiply(along.powf(1.6) * (1.0 - close) + close)
		}),
	));
	// A soft glow and a bright core at the head.
	let head = outline(head);
	let lit = 1.0 - close;
	painter.circle_filled(head, width * 2.0, color.gamma_multiply(0.18 * lit));
	painter.circle_filled(head, width * 1.3, color.gamma_multiply(0.45 * lit));
	painter.circle_filled(head, width * 0.8, tip.gamma_multiply(lit));
	// Every frame, like egui's own animations: a timer would round to every other vsync.
	ui.ctx().request_repaint();
}

/// The point a fraction `lap` of the way clockwise round `rect`'s outline with rounded
/// corners, starting at the left end of the top edge.
fn rounded_outline(rect: egui::Rect, corner: f32, lap: f32) -> egui::Pos2 {
	use std::f32::consts::FRAC_PI_2;
	let c = corner;
	let (wide, tall) = (rect.width() - 2.0 * c, rect.height() - 2.0 * c);
	let arc = FRAC_PI_2 * c;
	let mut along = lap.rem_euclid(1.0) * 2.0 * (wide + tall + 2.0 * arc);
	// Each side's start, direction and length, then the centre of the corner after it.
	let sides = [
		(
			rect.left_top() + egui::vec2(c, 0.0),
			egui::Vec2::X,
			wide,
			rect.right_top() + egui::vec2(-c, c),
		),
		(
			rect.right_top() + egui::vec2(0.0, c),
			egui::Vec2::Y,
			tall,
			rect.right_bottom() - egui::vec2(c, c),
		),
		(
			rect.right_bottom() - egui::vec2(c, 0.0),
			-egui::Vec2::X,
			wide,
			rect.left_bottom() + egui::vec2(c, -c),
		),
		(
			rect.left_bottom() - egui::vec2(0.0, c),
			-egui::Vec2::Y,
			tall,
			rect.left_top() + egui::vec2(c, c),
		),
	];
	for (side, &(start, direction, length, center)) in sides.iter().enumerate() {
		if along < length {
			return start + direction * along;
		}
		along -= length;
		if along < arc {
			return center + c * egui::Vec2::angled((side as f32 - 1.0) * FRAC_PI_2 + along / c);
		}
		along -= arc;
	}
	sides[0].0
}

/// A seat arriving (`elapsed` seconds ago). The own seat's comet closes into a ring on the
/// chime's first note, then the ring flashes and fades while a halo spreads around the
/// avatar. Someone picking up gets no ring, only the halo on the second note: nothing circles
/// once the call is up.
fn arrival_rings(
	ui: &egui::Ui,
	halo: &egui::Painter,
	avatar: egui::Rect,
	color: egui::Color32,
	elapsed: f32,
	comet: bool,
) {
	let closing = CUE_LATENCY + CHIME_NOTE;
	if elapsed < closing {
		if comet {
			let close = egui::emath::easing::cubic_in_out(elapsed / closing);
			connecting_comet(ui, avatar, color, close, JOIN_RING);
		}
		return;
	}
	let since = elapsed - closing;
	if comet {
		let fade = 1.0 - (since / 0.5).min(1.0);
		ui.painter().circle_stroke(
			avatar.center(),
			avatar.width() * 0.5 - JOIN_RING * 0.5,
			egui::Stroke::new(
				JOIN_RING,
				design::mix(color, egui::Color32::WHITE, 0.6 * fade).gamma_multiply(fade * fade),
			),
		);
	}
	let spread = (since / 0.65).min(1.0);
	if comet {
		self::halo(halo, avatar, color, spread, 0.55);
	} else {
		// Already soft at its first frame: a narrow band at the edge would read as a ring.
		self::halo(halo, avatar, color, 0.35 + 0.65 * spread, 0.7);
	}
}

fn speaking_avatar(ui: &egui::Ui, avatar: &egui::Response, name: &str) {
	let colors = design::palette(ui);
	// Inset the centered stroke so the ring stays inside the avatar.
	ui.painter().circle_stroke(
		avatar.rect.center(),
		avatar.rect.width() * 0.5 - 1.5,
		egui::Stroke::new(3.0, colors.positive),
	);
	let label = format!("{name} · Speaking");
	// Callers own the avatar's tooltip; a second one here would stack under it.
	avatar.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, &label));
}

fn stage_notices(ui: &mut egui::Ui, notices: &[(String, bool)]) {
	ui.spacing_mut().item_spacing.y = 2.0;
	for (text, strong) in notices {
		ui.add(
			egui::Label::new(
				RichText::new(text)
					.size(if *strong { 13.0 } else { 12.0 })
					.color(if *strong { STAGE_TEXT } else { STAGE_MUTED }),
			)
			.truncate()
			.selectable(false),
		);
	}
	if !notices.is_empty() {
		ui.add_space(6.0);
	}
	ui.spacing_mut().item_spacing.y = 8.0;
}

/// Rounded dark group holding several call controls. Every pill is exactly `CONTROL_HEIGHT`
/// tall and `width` wide so the groups and the hang-up button share one baseline.
fn pill<R>(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
	let (rect, _) = ui.allocate_exact_size(egui::vec2(width, CONTROL_HEIGHT), egui::Sense::hover());
	ui.painter()
		.rect(rect, 12, PILL_FILL, STAGE_EDGE, egui::StrokeKind::Inside);
	let mut inner = ui.new_child(
		egui::UiBuilder::new()
			.max_rect(rect)
			.layout(egui::Layout::left_to_right(egui::Align::Center)),
	);
	inner.spacing_mut().item_spacing.x = 0.0;
	add(&mut inner)
}

/// One control inside a pill; disabled controls stay visible but inert, like Discord's.
fn control(
	ui: &mut egui::Ui,
	icon: crate::icons::Icon,
	width: f32,
	enabled: bool,
	color: egui::Color32,
	label: &str,
	hint: &str,
) -> egui::Response {
	let label = crate::i18n::translate_if_key(label);
	let hint = crate::i18n::translate_if_key(hint);
	let (rect, response) = ui.allocate_exact_size(
		egui::vec2(width, CONTROL_HEIGHT),
		if enabled {
			egui::Sense::click()
		} else {
			egui::Sense::hover()
		},
	);
	let (hover, press) = if enabled {
		(
			design::hover_motion(ui, &response),
			design::press_motion(ui, &response),
		)
	} else {
		(0.0, 0.0)
	};
	if hover > 0.0 {
		ui.painter().rect_filled(
			design::pressed_rect(rect.shrink(4.0), press),
			8,
			egui::Color32::from_white_alpha((28.0 * hover + 14.0 * press) as u8),
		);
	}
	let size = if width < 40.0 { 14.0 } else { 22.0 };
	crate::icons::paint(
		ui.painter(),
		icon,
		egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(size * (1.0 - 0.1 * press))),
		if enabled {
			color
		} else {
			STAGE_MUTED.gamma_multiply(0.45)
		},
	);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, &label));
	response.on_hover_text(hint)
}

fn fullscreen_control(ui: &mut egui::Ui, color: egui::Color32) -> egui::Response {
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(48.0, CONTROL_HEIGHT), egui::Sense::click());
	let hover = design::hover_motion(ui, &response);
	if hover > 0.0 {
		ui.painter().rect_filled(
			rect.shrink(4.0),
			8,
			egui::Color32::from_white_alpha((28.0 * hover) as u8),
		);
	}
	crate::icons::paint(
		ui.painter(),
		crate::icons::Icon::Fullscreen,
		egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(22.0)),
		color,
	);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Fullscreen"));
	response.on_hover_text("View this screen share in fullscreen")
}

/// Filled call action (join, answer, decline): Discord's rounded green and red buttons.
fn call_action(
	ui: &mut egui::Ui,
	icon: crate::icons::Icon,
	fill: egui::Color32,
	enabled: bool,
	label: &str,
) -> egui::Response {
	let label = crate::i18n::translate_if_key(label);
	let (rect, response) = ui.allocate_exact_size(
		egui::vec2(JOIN_WIDTH, CONTROL_HEIGHT),
		if enabled {
			egui::Sense::click()
		} else {
			egui::Sense::hover()
		},
	);
	let press = if enabled {
		design::press_motion(ui, &response)
	} else {
		0.0
	};
	let fill = if enabled {
		fill.gamma_multiply(1.0 - 0.15 * design::hover_motion(ui, &response) - 0.1 * press)
	} else {
		fill.gamma_multiply(0.45)
	};
	let rect = design::pressed_rect(rect, press);
	ui.painter().rect_filled(rect, 12, fill);
	crate::icons::paint(
		ui.painter(),
		icon,
		egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(22.0)),
		egui::Color32::WHITE,
	);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, &label));
	response.on_hover_text(label)
}

/// The selected conversation's call this device can answer or join, shown as a join stage.
/// A call known to have nobody else in it is not one: after hanging up on an unanswered call,
/// Discord's record outlives the call by a moment and used to show the person rung as if
/// they had joined. A call whose members are not known yet still offers to join.
fn join_stage(state: &State) -> Option<Id> {
	let own = state.user.as_ref().map(|user| user.id);
	state.selected.filter(|channel| {
		state.can_view(*channel)
			&& (state.voice.incoming == Some(*channel)
				|| (state.voice.has_dm_call(*channel)
					&& state
						.voice
						.dm_call_participants()
						.find(|(id, _)| id == channel)
						.is_none_or(|(_, participants)| {
							participants
								.iter()
								.any(|participant| Some(participant.user) != own)
						}))) && state
			.voice
			.active
			.as_ref()
			.is_none_or(|call| call.channel != *channel)
	})
}

/// Who a join stage shows: the call's known members, else the people it would ring. This
/// device is not in that call, so its own seat is only Discord's echo of a leave still on the
/// way; it fades out instead of lingering.
fn join_participants(state: &State, channel: Id) -> Vec<RosterEntry> {
	let own = state.user.as_ref().map(|user| user.id);
	let known: Vec<Participant> = state
		.voice
		.dm_participants(channel)
		.iter()
		.filter(|participant| Some(participant.user) != own)
		.copied()
		.collect();
	let participants: Vec<Participant> = if known.is_empty() {
		state
			.channel(channel)
			.map_or_else(Vec::new, |conversation| {
				conversation
					.recipients
					.iter()
					.take(client_core::voice::MAX_PARTICIPANTS)
					.map(|user| Participant {
						user: user.id,
						muted: false,
						deafened: false,
						server_muted: false,
						server_deafened: false,
						video: false,
						streaming: false,
					})
					.collect()
			})
	} else {
		known
	};
	participants
		.into_iter()
		.map(|participant| RosterEntry {
			guild: Id(0),
			channel,
			participant,
			member: None,
		})
		.collect()
}

/// Resolve the display name and user for a roster entry from the entry, member list or self.
/// Include the local call participant before its gateway roster update arrives.
fn stage_participants(state: &State, channel: Id) -> Vec<RosterEntry> {
	let call = state
		.voice
		.active
		.as_ref()
		.filter(|call| call.channel == channel);
	let mut entries: Vec<_> = if let Some(call) = call.filter(|call| call.guild.is_none()) {
		call.participants
			.iter()
			.map(|participant| RosterEntry {
				guild: Id(0),
				channel,
				participant: *participant,
				member: None,
			})
			.collect()
	} else {
		state
			.voice
			.roster
			.iter()
			.filter(|entry| entry.channel == channel)
			.cloned()
			.collect()
	};
	if call.is_some_and(|call| call.guild.is_none())
		&& let Some(conversation) = state.channel(channel)
	{
		for user in conversation
			.recipients
			.iter()
			.take(client_core::voice::MAX_PARTICIPANTS - 1)
		{
			if state.voice.ringing(channel).contains(&user.id)
				&& entries.len() < client_core::voice::MAX_PARTICIPANTS
				&& !entries
					.iter()
					.any(|entry| entry.participant.user == user.id)
			{
				entries.push(RosterEntry {
					guild: Id(0),
					channel,
					member: None,
					participant: Participant {
						user: user.id,
						muted: false,
						deafened: false,
						server_muted: false,
						server_deafened: false,
						video: false,
						streaming: false,
					},
				});
			}
		}
	}
	if let Some(call) = call
		&& let Some(user) = &state.user
	{
		// Discord seats everyone else first and the local participant last.
		if let Some(index) = entries
			.iter()
			.position(|entry| entry.participant.user == user.id)
		{
			let own = entries.remove(index);
			entries.push(own);
		} else {
			entries.push(RosterEntry {
				guild: call.guild.unwrap_or(Id(0)),
				channel,
				participant: Participant {
					user: user.id,
					muted: call.muted,
					deafened: call.deafened,
					server_muted: call.server_muted,
					server_deafened: call.server_deafened,
					video: call.camera,
					streaming: false,
				},
				member: None,
			});
		}
	}
	entries
}

/// The local participant follows this device's toggles at once instead of waiting for the
/// gateway echo, so a deafen shows the deafened glyph immediately (and in the offline preview).
fn shown_participant(state: &State, entry: &RosterEntry) -> Participant {
	let mut participant = entry.participant;
	if let Some(call) = state
		.voice
		.active
		.as_ref()
		.filter(|call| call.channel == entry.channel)
		&& state
			.user
			.as_ref()
			.is_some_and(|own| own.id == participant.user)
	{
		participant.deafened = call.deafened;
		participant.muted = call.self_muted();
		participant.server_muted |= call.server_muted;
		participant.server_deafened |= call.server_deafened;
	}
	participant
}

fn inactive_recipient(state: &State, channel: Id, user: Id) -> bool {
	state.voice.active.as_ref().is_some_and(|call| {
		call.channel == channel
			&& call.guild.is_none()
			&& state.user.as_ref().is_some_and(|own| own.id != user)
			&& !call
				.participants
				.iter()
				.any(|participant| participant.user == user)
	})
}

fn resolve_member<'a>(
	state: &'a State,
	entry: &'a RosterEntry,
) -> (Option<&'a model::User>, &'a str) {
	let member = entry.member.as_ref().or_else(|| {
		state
			.members
			.as_ref()
			.filter(|list| list.guild == Some(entry.guild))
			.and_then(|list| {
				list.slots
					.iter()
					.flatten()
					.filter_map(|slot| match slot {
						model::MemberSlot::Person(m) => Some(m),
						_ => None,
					})
					.find(|m| m.user.id == entry.participant.user)
			})
	});
	let user = member
		.map(|m| &m.user)
		.or_else(|| participant_user(state, entry.channel, entry.participant.user));
	let name = member
		.and_then(|m| m.nick.as_deref())
		.or_else(|| user.map(|u| u.name.as_str()))
		.unwrap_or("Participant");
	(user, name)
}

/// Find a call participant's user from self, DM recipients or the roster.
pub(super) fn participant_user(state: &State, channel: Id, user: Id) -> Option<&model::User> {
	state
		.user
		.as_ref()
		.filter(|u| u.id == user)
		.or_else(|| {
			state
				.channels
				.iter()
				.find(|c| c.id == channel)
				.and_then(|c| c.recipients.iter().find(|u| u.id == user))
		})
		.or_else(|| {
			state
				.voice
				.roster
				.iter()
				.find(|e| e.channel == channel && e.participant.user == user)
				.and_then(|e| e.member.as_ref())
				.map(|m| &m.user)
		})
}

/// Labelled percentage slider shared by the voice popout and the settings page.
fn gain_slider(ui: &mut egui::Ui, value: &mut u16, title: &str) -> egui::Response {
	let title = crate::i18n::translate_if_key(title);
	ui.scope(|ui| {
		let colors = design::palette(ui);
		ui.spacing_mut().item_spacing.y = 4.0;
		let label = ui.label(RichText::new(title).size(13.0).color(colors.muted));
		design::slider(ui, value, 0..=200, "%").labelled_by(label.id)
	})
	.inner
}

fn gain_controls(ui: &mut egui::Ui, gain: &mut crate::VoiceGain) -> [egui::Response; 2] {
	let slider = gain_slider;
	let responses = if ui.available_width() >= 480.0 {
		ui.columns(2, |columns| {
			[
				slider(&mut columns[0], &mut gain.input_percent, "Microphone gain"),
				slider(&mut columns[1], &mut gain.output_percent, "Speaker volume"),
			]
		})
	} else {
		[
			slider(ui, &mut gain.input_percent, "Microphone gain"),
			slider(ui, &mut gain.output_percent, "Speaker volume"),
		]
	};
	design::hint(
		ui,
		&crate::i18n::translate(
			"voice-gain-controls-100-is-the-original-level-higher-levels-may-distort",
		),
	);
	responses
}

fn elapsed_label(call: &client_core::voice::Call) -> Option<String> {
	if !matches!(call.phase, Phase::Waiting | Phase::Connected) {
		return None;
	}
	let seconds = call
		.channel_started_at
		.or(call.connected_at)?
		.elapsed()
		.as_secs();
	Some(format!(
		"{:02}:{:02}:{:02}",
		seconds / 3600,
		seconds / 60 % 60,
		seconds % 60
	))
}

/// Segmented input meter: green while speaking, amber when loud, red when clipping. `level`
/// is in dBFS; None draws the empty track.
fn level_meter(ui: &mut egui::Ui, level: Option<f32>, height: f32) {
	let colors = design::palette(ui);
	let (rect, _) = ui.allocate_exact_size(
		egui::vec2(ui.available_width(), height),
		egui::Sense::hover(),
	);
	let level = ((level.unwrap_or(-100.0) + 80.0) / 80.0).clamp(0.0, 1.0);
	let bars = (rect.width() / 9.0).floor().max(1.0) as usize;
	for index in 0..bars {
		let fraction = index as f32 / bars as f32;
		let color = if fraction < level {
			if fraction > 0.9 {
				colors.danger
			} else if fraction > 0.7 {
				colors.warning
			} else {
				colors.positive
			}
		} else {
			colors.border
		};
		let left = rect.left() + index as f32 * rect.width() / bars as f32;
		ui.painter().rect_filled(
			egui::Rect::from_min_size(
				egui::pos2(left, rect.top()),
				egui::vec2((rect.width() / bars as f32 - 3.0).max(1.0), rect.height()),
			),
			2.0,
			color,
		);
	}
}

fn status_icon(ui: &mut egui::Ui, icon: crate::icons::Icon, color: egui::Color32, label: &str) {
	let label = crate::i18n::translate_if_key(label);
	let (rect, response) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
	crate::icons::paint(ui.painter(), icon, rect.shrink(1.0), color);
	response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &label));
	response.on_hover_text(label);
}

/// Discord's LIVE pill, sized for the channel list row rather than a stage tile.
fn live_badge(ui: &mut egui::Ui) {
	let (rect, response) = ui.allocate_exact_size(egui::vec2(32.0, 16.0), egui::Sense::hover());
	let colors = design::palette(ui);
	ui.painter().rect_filled(rect, 4, colors.danger);
	ui.painter().text(
		rect.center(),
		egui::Align2::CENTER_CENTER,
		crate::i18n::translate("voice-live-badge-live"),
		egui::FontId::new(9.0, design::medium_family(ui.ctx())),
		egui::Color32::WHITE,
	);
	response.widget_info(|| {
		egui::WidgetInfo::labeled(
			egui::Role::Label,
			true,
			crate::i18n::translate("voice-live-badge-live-2"),
		)
	});
	response.on_hover_text(crate::i18n::translate("voice-live-badge-streaming"));
}

fn device_combo(
	ui: &mut egui::Ui,
	id: &str,
	devices: &[(String, String)],
	selected: &mut Option<String>,
) -> egui::Response {
	let label = match selected.as_ref() {
		None => crate::i18n::translate("voice-device-default"),
		Some(id) => devices.iter().find(|(key, _)| key == id).map_or_else(
			|| crate::i18n::translate("voice-device-combo-device-unavailable"),
			|(_, label)| label.clone(),
		),
	};
	crate::design::combo(id)
		.selected_text(&label)
		.width(ui.available_width())
		.truncate()
		.height(220.0)
		.show_ui(ui, |ui| {
			ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
			ui.selectable_value(
				selected,
				None,
				crate::i18n::translate("voice-device-default"),
			);
			for (id, label) in devices.iter().take(32) {
				ui.selectable_value(selected, Some(id.clone()), label)
					.on_hover_text(label);
			}
		})
		.response
		.on_hover_text(&label)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn held_push_to_mute_shows_a_slashed_microphone_without_toggling() {
		use crate::icons::Icon;
		assert_eq!(
			MessagingUi::mute_toggle_icon(false, false, false),
			Icon::Microphone
		);
		assert_eq!(
			MessagingUi::mute_toggle_icon(false, false, true),
			Icon::MicrophoneSlash
		);
		assert_eq!(
			MessagingUi::mute_toggle_icon(false, true, false),
			Icon::MicrophoneSlash
		);
		assert_eq!(
			MessagingUi::mute_toggle_icon(false, true, true),
			Icon::MicrophoneSlash
		);
		assert_eq!(
			MessagingUi::mute_toggle_icon(true, false, true),
			Icon::Headphones
		);
		assert_eq!(
			MessagingUi::mute_toggle_icon(true, true, false),
			Icon::HeadphonesSlash
		);
	}

	#[test]
	fn explicit_join_audio_waits_for_the_switch_confirmation() {
		let mut state = test_support::call_demo_state();
		state.demo = false;
		state.gateway_connected = true;
		let mut view = MessagingUi {
			voice_available: true,
			..Default::default()
		};
		let before = (view.voice_muted, view.voice_deafened);
		let mut commands = Vec::new();
		view.request_call_with_audio(&mut state, Id(25), false, true, true, &mut commands)
			.unwrap();
		assert_eq!((view.voice_muted, view.voice_deafened), before);
		assert!(commands.is_empty());
		assert_eq!(state.voice.active.as_ref().unwrap().channel, Id(22));
		view.confirm_call_switch(&mut state);
		let call = state.voice.active.as_ref().unwrap();
		assert_eq!(
			(call.channel, call.muted, call.deafened),
			(Id(25), true, true)
		);
		assert_eq!((view.voice_muted, view.voice_deafened), (true, true));
		assert!(
			commands.is_empty(),
			"the controller sends call commands itself"
		);
		assert!(
			view.request_call_with_audio(
				&mut state,
				Id(999999),
				false,
				false,
				false,
				&mut commands
			)
			.is_err()
		);
		assert_eq!((view.voice_muted, view.voice_deafened), (true, true));
		assert!(commands.is_empty());
	}

	#[test]
	fn frameless_avatars_stay_close_and_inside_short_or_narrow_stages() {
		// Two DM avatars: Discord-tight spacing instead of wide plates.
		let (columns, size) = frameless_fit(2, egui::vec2(800.0, 300.0));
		assert_eq!(columns, 2);
		assert!(size.x - tile_avatar(size, true) <= FRAMELESS_MARGIN + 0.5);
		for (count, area) in [
			(2, egui::vec2(800.0, 60.0)),
			(6, egui::vec2(220.0, 200.0)),
			(9, egui::vec2(150.0, 120.0)),
		] {
			let (columns, size) = frameless_fit(count, area);
			let rows = count.div_ceil(columns) as f32;
			assert!(
				size.y * rows + TILE_GAP * (rows - 1.0) <= area.y + 0.5,
				"{count} {area:?}"
			);
			assert!(size.x * columns as f32 <= area.x + 0.5);
			let avatar = tile_avatar(size, true);
			assert!(avatar <= size.x && avatar <= size.y.max(12.0));
		}
	}

	#[test]
	fn deafening_mutes_and_undeafening_restores_the_microphone() {
		let mut view = MessagingUi::default();
		assert_eq!(view.toggle_voice(true), (false, true));
		assert_eq!(
			view.notification_preview,
			Some(model::notification_preferences::Sound::Deafen)
		);
		assert_eq!(view.toggle_voice(true), (false, false));
		view.toggle_voice(false);
		assert_eq!(view.toggle_voice(true), (true, true));
		assert_eq!(view.toggle_voice(true), (true, false));
		view.toggle_voice(true);
		// The microphone button while deafened clears both, with Discord's undeafen cue.
		assert_eq!(view.toggle_voice(false), (false, false));
		assert_eq!(
			view.notification_preview,
			Some(model::notification_preferences::Sound::Undeafen)
		);
	}

	#[test]
	fn local_mutes_zero_one_speaker_and_keep_their_stored_volume() {
		let mut view = MessagingUi::default();
		view.set_voice_user_volume_overrides(&[(7, 150)]);
		assert!(!view.voice_user_locally_muted(Id(7)));
		view.set_voice_user_locally_muted(Id(7), true);
		assert_eq!(view.voice_user_mutes(), [7]);
		assert!(view.voice_user_volumes().contains(&(7, 0)));
		// Muting is device-local and never rewrites the volume chosen for that speaker.
		assert_eq!(view.voice_user_volume_overrides(), vec![(7, 150)]);
		view.set_voice_user_locally_muted(Id(9), true);
		assert!(view.voice_user_volumes().contains(&(9, 0)));
		view.set_voice_user_locally_muted(Id(7), false);
		assert!(view.voice_user_volumes().contains(&(7, 150)));
		assert_eq!(view.voice_user_mutes(), [9]);
		view.set_voice_user_mutes(&(0..200).collect::<Vec<u64>>());
		assert_eq!(view.voice_user_mutes().len(), MAX_USER_MUTES);
		assert!(!view.voice_user_mutes().contains(&0));
	}

	#[test]
	fn voice_moderation_checks_permissions_source_and_reconciles_demo_actions() {
		use model::{
			permissions as p,
			server_admin::{Action, Result as Outcome},
		};
		let mut state = test_support::voice_demo_state();
		let guild = Id(10);
		let metadata = state.permissions.guilds.get_mut(&guild).unwrap();
		metadata.owner = Some(Id(99));
		metadata
			.roles
			.as_mut()
			.unwrap()
			.iter_mut()
			.find(|role| role.id == guild)
			.unwrap()
			.bits = p::VIEW_CHANNEL | p::CONNECT | p::MUTE_MEMBERS | p::MOVE_MEMBERS;
		for id in [Id(25), Id(26)] {
			state.permissions.channels.insert(
				id,
				p::Channel {
					id,
					guild,
					overwrites: Some(vec![]),
				},
			);
		}
		state.permissions.clear_cache();
		assert!(state.can_moderate_voice(guild, Id(25), Id(2), VoiceChange::Move(Some(Id(26)))));
		assert!(!state.can_moderate_voice(guild, Id(25), Id(1), VoiceChange::Mute(true)));
		assert!(!state.can_moderate_voice(guild, Id(25), Id(2), VoiceChange::Move(Some(Id(25)))));
		assert!(!state.can_moderate_voice(guild, Id(25), Id(2), VoiceChange::Move(Some(Id(20)))));
		state
			.permissions
			.channels
			.get_mut(&Id(26))
			.unwrap()
			.overwrites = Some(vec![p::Overwrite {
			id: guild,
			kind: 0,
			allow: 0,
			deny: p::CONNECT,
		}]);
		state.permissions.clear_cache();
		assert!(!state.can_moderate_voice(guild, Id(25), Id(2), VoiceChange::Move(Some(Id(26)))));
		state
			.permissions
			.channels
			.get_mut(&Id(26))
			.unwrap()
			.overwrites = Some(vec![]);
		state.permissions.clear_cache();
		for change in [
			VoiceChange::Mute(true),
			VoiceChange::Mute(false),
			VoiceChange::Move(Some(Id(26))),
			VoiceChange::Move(None),
		] {
			let channel = state
				.voice
				.roster
				.iter()
				.find(|entry| entry.participant.user == Id(2))
				.unwrap()
				.channel;
			let action = Action::Voice {
				user: Id(2),
				channel,
				change,
			};
			let Command::ServerAdmin { request, .. } =
				state.request_server_admin(guild, action.clone()).unwrap()
			else {
				panic!()
			};
			assert!(
				state.request_server_admin(guild, action.clone()).is_none(),
				"one outstanding mutation"
			);
			assert!(state.server_admin_command_allowed(guild, request, &action));
			state.apply(client_core::Envelope {
				generation: state.generation,
				event: client_core::Event::ServerAdmin(client_core::server_admin::Event {
					guild,
					request,
					result: Ok(Outcome::VoiceUpdated(Id(2))),
				}),
			});
			assert!(!state.server_admin.pending);
			match change {
				VoiceChange::Mute(muted) => assert_eq!(
					state
						.voice
						.roster
						.iter()
						.find(|entry| entry.participant.user == Id(2))
						.unwrap()
						.participant
						.server_muted,
					muted
				),
				VoiceChange::Move(to) => assert_eq!(
					state
						.voice
						.roster
						.iter()
						.find(|entry| entry.participant.user == Id(2))
						.map(|entry| entry.channel),
					to
				),
			}
		}
		assert!(
			!state.can_moderate_voice(guild, Id(26), Id(2), VoiceChange::Mute(true)),
			"departed source cannot be moderated"
		);
	}

	#[test]
	fn voice_moderation_menu_click_records_server_action() {
		for (label, change) in [
			("Server mute", VoiceChange::Mute(true)),
			("Disconnect", VoiceChange::Move(None)),
		] {
			let mut state = test_support::voice_demo_state();
			state.permissions.guilds.get_mut(&Id(10)).unwrap().owner =
				Some(state.user.as_ref().unwrap().id);
			state.permissions.clear_cache();
			let entry = state
				.voice
				.roster
				.iter()
				.find(|entry| entry.participant.user == Id(2))
				.unwrap()
				.clone();
			let ctx = egui::Context::default();
			crate::design::apply(&ctx);
			let mut view = MessagingUi::default();
			let frame = |view: &mut MessagingUi, events| {
				let output = ctx.run_ui(
					egui::RawInput {
						screen_rect: Some(egui::Rect::from_min_size(
							egui::Pos2::ZERO,
							egui::vec2(600.0, 700.0),
						)),
						events,
						..Default::default()
					},
					|ui| view.voice_participant(ui, &state, &entry),
				);
				let button = output.shapes.iter().find_map(|shape| match &shape.shape {
					egui::Shape::Text(text) if text.galley.job.text == label => {
						Some(text.galley.rect.translate(text.pos.to_vec2()).center())
					}
					_ => None,
				});
				output.drop_without_applying_deltas();
				button
			};
			frame(&mut view, vec![]);
			let point = egui::pos2(100.0, 17.0);
			let click = |point, button, pressed| {
				vec![
					egui::Event::PointerMoved(point),
					egui::Event::PointerButton {
						pos: point,
						button,
						pressed,
						modifiers: egui::Modifiers::NONE,
					},
				]
			};
			for pressed in [true, false] {
				frame(
					&mut view,
					click(point, egui::PointerButton::Secondary, pressed),
				);
			}
			let point = frame(&mut view, vec![]).expect("moderation menu item");
			for pressed in [true, false] {
				frame(
					&mut view,
					click(point, egui::PointerButton::Primary, pressed),
				);
			}
			assert_eq!(view.voice_moderation, Some((Id(10), Id(25), Id(2), change)));
			assert!(
				view.voice_user_mutes().is_empty(),
				"server mute is distinct from local mute"
			);
		}
	}

	#[test]
	fn voice_move_controls_dispatch_and_reject_denied_destinations() {
		use model::{permissions as p, server_admin::Action};
		for dragging in [false, true] {
			for denied in [false, true] {
				let mut state = test_support::voice_demo_state();
				let guild = Id(10);
				let metadata = state.permissions.guilds.get_mut(&guild).unwrap();
				metadata.owner = Some(Id(99));
				metadata
					.roles
					.as_mut()
					.unwrap()
					.iter_mut()
					.find(|role| role.id == guild)
					.unwrap()
					.bits = p::VIEW_CHANNEL | p::CONNECT | p::MOVE_MEMBERS;
				for id in [Id(25), Id(26)] {
					state.permissions.channels.insert(
						id,
						p::Channel {
							id,
							guild,
							overwrites: Some(vec![]),
						},
					);
				}
				if denied {
					state
						.permissions
						.channels
						.get_mut(&Id(26))
						.unwrap()
						.overwrites = Some(vec![p::Overwrite {
						id: guild,
						kind: 0,
						allow: 0,
						deny: p::CONNECT,
					}]);
				}
				state.permissions.clear_cache();
				let entry = state
					.voice
					.roster
					.iter()
					.find(|e| e.participant.user == Id(2))
					.unwrap()
					.clone();
				let ctx = egui::Context::default();
				crate::design::apply(&ctx);
				let mut view = MessagingUi {
					guild: Some(guild),
					..Default::default()
				};
				let frame = |view: &mut MessagingUi, state: &mut State, events| {
					let output = ctx.run_ui(
						egui::RawInput {
							screen_rect: Some(egui::Rect::from_min_size(
								egui::Pos2::ZERO,
								egui::vec2(600.0, 900.0),
							)),
							events,
							..Default::default()
						},
						|ui| {
							if dragging {
								view.channel_list(ui, state);
							} else {
								view.voice_participant(ui, state, &entry);
							}
						},
					);
					let text: Vec<_> = output
						.shapes
						.iter()
						.filter_map(|shape| match &shape.shape {
							egui::Shape::Text(t) => Some((
								t.galley.job.text.clone(),
								t.galley.rect.translate(t.pos.to_vec2()).center(),
							)),
							_ => None,
						})
						.collect();
					output.drop_without_applying_deltas();
					text
				};
				let click = |pos, button, pressed| {
					vec![
						egui::Event::PointerMoved(pos),
						egui::Event::PointerButton {
							pos,
							button,
							pressed,
							modifiers: egui::Modifiers::NONE,
						},
					]
				};
				let text = frame(&mut view, &mut state, vec![]);
				if dragging {
					let source = text
						.iter()
						.find(|(text, _)| text.starts_with("Robin"))
						.expect("participant row")
						.1;
					let target = text
						.iter()
						.find(|(text, _)| text == "Quiet room")
						.expect("destination channel")
						.1;
					frame(
						&mut view,
						&mut state,
						click(source, egui::PointerButton::Primary, true),
					);
					frame(
						&mut view,
						&mut state,
						vec![egui::Event::PointerMoved(source + egui::vec2(16.0, 0.0))],
					);
					assert!(
						egui::DragAndDrop::has_payload_of_type::<super::VoiceDrag>(&ctx),
						"participant begins voice drag"
					);
					frame(
						&mut view,
						&mut state,
						vec![egui::Event::PointerMoved(target)],
					);
					frame(
						&mut view,
						&mut state,
						click(target, egui::PointerButton::Primary, false),
					);
				} else {
					for pressed in [true, false] {
						frame(
							&mut view,
							&mut state,
							click(
								egui::pos2(100.0, 17.0),
								egui::PointerButton::Secondary,
								pressed,
							),
						);
					}
					let text = frame(&mut view, &mut state, vec![]);
					let submenu = text
						.iter()
						.find(|(text, _)| text == "Move to")
						.expect("move submenu")
						.1;
					for pressed in [true, false] {
						frame(
							&mut view,
							&mut state,
							click(submenu, egui::PointerButton::Primary, pressed),
						);
					}
					let text = frame(&mut view, &mut state, vec![]);
					let destination = text.iter().find(|(text, _)| text == "Quiet room");
					if denied {
						assert!(destination.is_none(), "denied destination is excluded");
					} else {
						for pressed in [true, false] {
							frame(
								&mut view,
								&mut state,
								click(
									destination.unwrap().1,
									egui::PointerButton::Primary,
									pressed,
								),
							);
						}
					}
				}
				let expected =
					(!denied).then_some((guild, Id(25), Id(2), VoiceChange::Move(Some(Id(26)))));
				assert_eq!(
					view.voice_moderation, expected,
					"dragging={dragging}, denied={denied}"
				);
				let mut commands = Vec::new();
				ctx.run_ui(
					egui::RawInput {
						screen_rect: Some(egui::Rect::from_min_size(
							egui::Pos2::ZERO,
							egui::vec2(1000.0, 900.0),
						)),
						..Default::default()
					},
					|ui| commands = view.show(ui, &mut state),
				)
				.drop_without_applying_deltas();
				let actions: Vec<_> = commands
					.into_iter()
					.filter_map(|command| match command {
						client_core::Command::ServerAdmin { action, .. } => match *action {
							Action::Voice {
								user,
								channel,
								change,
							} => Some((user, channel, change)),
							_ => None,
						},
						_ => None,
					})
					.collect();
				assert_eq!(
					actions,
					if denied {
						vec![]
					} else {
						vec![(Id(2), Id(25), VoiceChange::Move(Some(Id(26))))]
					}
				);
			}
		}
	}

	#[test]
	fn voice_toggle_cues_follow_the_resulting_state_and_preferences() {
		use model::notification_preferences::Sound;

		let mut view = MessagingUi::default();
		for (deafen, active, expected) in [
			(false, true, Sound::Mute),
			(false, false, Sound::Unmute),
			(true, true, Sound::Deafen),
			(true, false, Sound::Undeafen),
		] {
			view.notification_preview = None;
			view.queue_voice_toggle_cue(deafen, active);
			assert_eq!(view.notification_preview, Some(expected));
		}
		view.notification_options.mute = false;
		view.notification_preview = None;
		view.queue_voice_toggle_cue(false, true);
		assert_eq!(view.notification_preview, None);
	}

	#[test]
	fn camera_settings_bound_layout_and_only_request_discovery_once() {
		let mut disabled = MessagingUi::default();
		egui::Context::default()
			.run_ui(Default::default(), |ui| {
				ui.add_enabled_ui(false, |ui| disabled.camera_settings_content(ui, false));
			})
			.drop_without_applying_deltas();
		assert!(!disabled.voice_refresh_cameras);
		assert!(disabled.voice_camera_device_status.is_empty());
		for demo in [false, true] {
			for theme in [egui::Theme::Dark, egui::Theme::Light] {
				let ctx = egui::Context::default();
				ctx.set_theme(theme);
				let mut messaging = MessagingUi {
					voice_cameras: vec![(
						"synthetic-camera".into(),
						"Long synthetic camera name ".repeat(20),
					)],
					voice_camera_device: Some("synthetic-camera".into()),
					..Default::default()
				};
				for width in [240.0, 640.0] {
					for frame in 0..2 {
						let mut output = ctx.run_ui(
							egui::RawInput {
								screen_rect: Some(egui::Rect::from_min_size(
									egui::Pos2::ZERO,
									egui::vec2(width, 600.0),
								)),
								..Default::default()
							},
							|ui| {
								let available = ui.available_width();
								let content =
									ui.scope(|ui| messaging.camera_settings_content(ui, demo));
								assert!(content.response.rect.width() <= available + 1.0);
							},
						);
						output.textures_delta.clear();
						assert_eq!(
							messaging.voice_camera_device.as_deref(),
							Some("synthetic-camera")
						);
						assert_eq!(
							messaging.voice_refresh_cameras,
							!demo
								&& cfg!(any(
									target_os = "windows",
									target_os = "macos",
									target_os = "linux"
								)) && width == 240.0 && frame == 0
						);
						assert!(messaging.voice_camera_preview.is_none());
						messaging.voice_refresh_cameras = false;
					}
				}
			}
		}
	}

	#[test]
	fn camera_popup_selects_second_device_without_starting_capture() {
		fn labels(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
			match shape {
				egui::Shape::Text(text) => out.push((
					text.galley.job.text.clone(),
					text.galley.rect.translate(text.pos.to_vec2()),
				)),
				egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| labels(shape, out)),
				_ => {}
			}
		}
		let ctx = egui::Context::default();
		let frame = |messaging: &mut MessagingUi, events: Vec<egui::Event>| {
			let mut output = ctx.run_ui(
				egui::RawInput {
					screen_rect: Some(egui::Rect::from_min_size(
						egui::Pos2::ZERO,
						egui::vec2(640.0, 600.0),
					)),
					events,
					..Default::default()
				},
				|ui| {
					let trigger = ui.button(crate::i18n::translate("voice-labels-choose-camera"));
					messaging.camera_settings_popup(&trigger, true);
				},
			);
			output.textures_delta.clear();
			let mut text = vec![];
			for shape in output.shapes {
				labels(&shape.shape, &mut text);
			}
			text
		};
		let click = |messaging: &mut MessagingUi, pos: egui::Pos2| {
			for pressed in [true, false] {
				frame(
					messaging,
					vec![
						egui::Event::PointerMoved(pos),
						egui::Event::PointerButton {
							pos,
							button: egui::PointerButton::Primary,
							pressed,
							modifiers: egui::Modifiers::NONE,
						},
					],
				);
			}
		};
		let mut messaging = MessagingUi::default();
		for label in ["Choose camera", "System default", "USB Camera (preview)"] {
			frame(&mut messaging, vec![]);
			let text = frame(&mut messaging, vec![]);
			let pos = text
				.iter()
				.find(|(text, _)| text == label)
				.unwrap_or_else(|| panic!("Missing control: {label}"))
				.1
				.center();
			click(&mut messaging, pos);
		}
		assert_eq!(
			messaging.voice_camera_device.as_deref(),
			Some("synthetic-usb")
		);
		assert!(!messaging.voice_refresh_cameras);
		assert!(messaging.voice_camera_preview.is_none());
		assert!(
			frame(&mut messaging, vec![])
				.iter()
				.any(|(text, _)| text == "Camera device"),
			"Selecting a device keeps the parent camera popup open"
		);
	}

	#[test]
	fn existing_call_can_offer_recipient_action_after_prejoin_metadata_and_confirmation() {
		let mut state = test_support::existing_call_demo_state();
		state.demo = false;
		state.apply_voice(client_core::voice::Event::Call {
			channel: Id(22),
			ringing: Some(vec![]),
			participants: Some(vec![]),
			unavailable: false,
		});
		assert!(state.start_call(Id(22), false));
		let call = state.voice.active.as_mut().unwrap();
		let request = call.request;
		// Synthetic confirmed media phase; this test never starts a transport/device.
		call.phase = Phase::Waiting;
		let mut view = MessagingUi {
			voice_available: true,
			..Default::default()
		};
		view.ring_request = Some((Id(22), request, Id(2), false));
		let _ = state.voice.take_commands().count();
		view.apply_ring_request(&mut state);
		assert!(state.voice.take_commands().any(|command| command
			== client_core::voice::Command::Ring {
				channel: Id(22),
				recipient: Some(Id(2)),
				stop: false,
			}));
		assert!(inactive_recipient(&state, Id(22), Id(2)));
	}

	#[test]
	fn arrivals_spring_on_the_chime_and_ring_pulses_follow_the_ring() {
		assert!((arrival_scale(0.0) - WAITING_SCALE).abs() < 1e-6);
		let samples: Vec<f32> = (0..100)
			.map(|step| arrival_scale(step as f32 * 0.01))
			.collect();
		let (peak_at, peak) =
			samples
				.iter()
				.enumerate()
				.fold((0, 0.0_f32), |best, (index, scale)| {
					if *scale > best.1 {
						(index, *scale)
					} else {
						best
					}
				});
		assert!((1.01..1.03).contains(&peak), "a small overshoot: {peak}");
		assert!(
			(0.12..0.24).contains(&(peak_at as f32 * 0.01)),
			"peaks just after the second note"
		);
		assert!(
			samples[50..]
				.iter()
				.all(|scale| (scale - 1.0).abs() < 0.003),
			"settled by half a second"
		);

		let ctx = egui::Context::default();
		let beat = |since: f64| ring_ripples(&ctx, since);
		assert_eq!(beat(0.1), [None, None], "silent before the first burst");
		let first = RING_BURSTS[0] + f64::from(CUE_LATENCY);
		assert!(beat(first + 0.01)[0].is_some_and(|ripple| ripple < 0.05));
		let second = RING_BURSTS[1] + f64::from(CUE_LATENCY);
		assert!(beat(second + 0.01)[1].is_some_and(|ripple| ripple < 0.05));
		assert!(
			beat(first + OUTGOING_RING_CYCLE * 4.0 + 0.01)[0].is_some_and(|ripple| ripple < 0.05),
			"later cycles stay on the beat"
		);
		assert_eq!(beat(2.5), [None, None], "a pause before the next cycle");
	}

	#[test]
	fn hanging_up_on_an_unanswered_call_shows_no_join_stage() {
		// Discord's record outlives an unanswered call by a moment; the person rung must not
		// appear as if they had joined.
		let mut state = test_support::existing_call_demo_state();
		assert_eq!(
			join_stage(&state),
			Some(Id(22)),
			"a call whose members are not known yet still offers to join"
		);
		state.demo = false;
		for (members, shown) in [(vec![], false), (vec![Id(1)], false), (vec![Id(2)], true)] {
			state.apply_voice(client_core::voice::Event::Call {
				channel: Id(22),
				ringing: Some(vec![]),
				participants: Some(
					members
						.into_iter()
						.map(|user| Participant {
							user,
							muted: false,
							deafened: false,
							server_muted: false,
							server_deafened: false,
							video: false,
							streaming: false,
						})
						.collect(),
				),
				unavailable: false,
			});
			assert_eq!(join_stage(&state).is_some(), shown);
		}
	}

	#[test]
	fn dm_stage_includes_only_ringing_absent_recipients_without_speaking_or_mute_state() {
		let mut state = test_support::call_demo_state();
		let call = state.voice.active.as_mut().unwrap();
		let channel = call.channel;
		let request = call.request;
		call.participants.retain(|p| p.user == Id(1));
		assert_eq!(stage_participants(&state, channel).len(), 1);
		state.demo = false;
		state.apply_voice(client_core::voice::Event::Call {
			channel,
			ringing: Some(vec![Id(2)]),
			participants: None,
			unavailable: false,
		});
		let entries = stage_participants(&state, channel);
		assert_eq!(entries.len(), 2);
		// Discord seats the local participant last.
		assert_eq!(entries[1].participant.user, Id(1));
		let peer = entries
			.iter()
			.find(|entry| entry.participant.user == Id(2))
			.unwrap();
		assert!(inactive_recipient(&state, channel, Id(2)));
		assert!(!peer.participant.muted);
		let mut view = MessagingUi::default();
		view.voice_speaking.push(Id(2));
		assert!(!view.is_speaking(&state, channel, &peer.participant));
		state.voice.ring_error = Some((channel, "Synthetic recipient error"));
		assert!(
			view.stage_notices(&state, channel, true)
				.contains(&("Synthetic recipient error".into(), true))
		);
		// A ring chosen for an earlier attempt of this call is dropped.
		view.ring_request = Some((channel, request + 1, Id(2), false));
		let _ = state.voice.take_commands().count();
		view.apply_ring_request(&mut state);
		assert_eq!(state.voice.take_commands().count(), 0);
		assert!(view.ring_request.is_none());
	}
	#[test]
	fn solo_call_stages_show_both_local_previews_without_a_roster() {
		fn textures(shape: &egui::Shape, ids: &mut Vec<egui::TextureId>) {
			match shape {
				egui::Shape::Mesh(mesh) => ids.push(mesh.texture_id),
				egui::Shape::Rect(rect) => {
					if let Some(brush) = &rect.brush {
						ids.push(brush.fill_texture_id);
					}
				}
				egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| textures(shape, ids)),
				_ => {}
			}
		}
		for guild in [false, true] {
			for width in [320.0, 900.0] {
				for dark in [false, true] {
					let mut state = if guild {
						test_support::voice_demo_state()
					} else {
						test_support::call_demo_state()
					};
					state.voice.roster.clear();
					let call = state.voice.active.as_mut().unwrap();
					call.participants.clear();
					call.phase = Phase::Waiting;
					call.camera = true;
					let (channel, request) = (call.channel, call.request);
					let ctx = egui::Context::default();
					ctx.set_visuals(if dark {
						egui::Visuals::dark()
					} else {
						egui::Visuals::light()
					});
					let mut messaging = MessagingUi::default();
					let camera = ctx.load_texture(
						"synthetic-camera",
						egui::ColorImage::filled([4, 3], egui::Color32::RED),
						Default::default(),
					);
					let screen = ctx.load_texture(
						"synthetic-screen",
						egui::ColorImage::filled([16, 9], egui::Color32::BLUE),
						Default::default(),
					);
					let expected = [camera.id(), screen.id()];
					messaging.voice_camera_preview = Some(camera);
					messaging.screen.preview = Some(screen);
					messaging.screen.busy = true;
					messaging.screen.context = Some((state.generation, channel, request));
					assert_eq!(stage_participants(&state, channel).len(), 1);
					let mut commands = vec![];
					for phase in [Phase::Waiting, Phase::Connected] {
						state.voice.active.as_mut().unwrap().phase = phase;
						let mut output = ctx.run_ui(
							egui::RawInput {
								screen_rect: Some(egui::Rect::from_min_size(
									egui::Pos2::ZERO,
									egui::vec2(width, 800.0),
								)),
								..Default::default()
							},
							|ui| {
								if guild {
									messaging.voice_channel(ui, &mut state, channel, &mut commands);
								} else {
									messaging.call_bar(ui, &mut state, &mut commands, None);
								}
							},
						);
						let mut rendered = vec![];
						for shape in &output.shapes {
							textures(&shape.shape, &mut rendered);
						}
						output.textures_delta.clear();
						for id in expected {
							assert!(
								rendered.contains(&id),
								"guild={guild}, width={width}, phase={phase:?}, texture={id:?}"
							);
						}
					}
					assert!(
						commands.is_empty(),
						"Synthetic previews must never start media"
					);
					state.voice.active = None;
					assert!(stage_participants(&state, channel).is_empty());
				}
			}
		}
	}

	#[test]
	fn existing_dm_call_stage_joins_without_ringing_and_disables_unavailable_actions() {
		fn frame(
			ctx: &egui::Context,
			messaging: &mut MessagingUi,
			state: &mut State,
			width: f32,
			events: Vec<egui::Event>,
		) -> (Vec<(String, egui::Rect)>, Vec<Command>) {
			fn labels(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
				match shape {
					egui::Shape::Text(text) => out.push((
						text.galley.job.text.clone(),
						text.galley.rect.translate(text.pos.to_vec2()),
					)),
					egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| labels(shape, out)),
					_ => {}
				}
			}
			let mut commands = vec![];
			let mut output = ctx.run_ui(
				egui::RawInput {
					screen_rect: Some(egui::Rect::from_min_size(
						egui::Pos2::ZERO,
						egui::vec2(width, 480.0),
					)),
					events,
					..Default::default()
				},
				|ui| messaging.call_bar(ui, state, &mut commands, None),
			);
			output.textures_delta.clear();
			let mut text = vec![];
			for shape in output.shapes {
				labels(&shape.shape, &mut text);
			}
			// The join buttons are glyphs; their accessible names locate them.
			if let Some(update) = &output.platform_output.accesskit_update {
				text.extend(update.nodes.iter().filter_map(|(_, node)| {
					let bounds = node.bounds()?;
					Some((
						node.label()?.to_owned(),
						egui::Rect::from_min_max(
							egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
							egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
						),
					))
				}));
			}
			(text, commands)
		}
		for width in [320.0, 900.0] {
			for dark in [false, true] {
				for mode in 0..5 {
					let mut state = test_support::existing_call_demo_state();
					state.demo = mode == 1;
					state.gateway_connected = mode != 2;
					if mode == 4 {
						assert!(state.start_call(Id(25), false));
					}
					state
						.channels
						.iter_mut()
						.find(|c| c.id == Id(22))
						.unwrap()
						.name = "A long synthetic caller name ".repeat(8);
					let mut messaging = MessagingUi {
						voice_available: mode != 3,
						..Default::default()
					};
					let ctx = egui::Context::default();
					ctx.enable_accesskit();
					ctx.set_visuals(if dark {
						egui::Visuals::dark()
					} else {
						egui::Visuals::light()
					});
					frame(&ctx, &mut messaging, &mut state, width, vec![]);
					let (text, commands) = frame(&ctx, &mut messaging, &mut state, width, vec![]);
					assert!(
						commands.is_empty(),
						"Showing an ongoing call must never join"
					);
					let join = text
						.iter()
						.find(|(label, _)| label == "Join call")
						.expect("Visible Join call button")
						.1;
					assert!(
						join.left() >= 0.0 && join.right() <= width,
						"Join must fit narrow layouts"
					);
					assert!(text.iter().any(|(label, _)| label == "Join with video"));
					let mut sent = vec![];
					for pressed in [true, false] {
						let (_, commands) = frame(
							&ctx,
							&mut messaging,
							&mut state,
							width,
							vec![
								egui::Event::PointerMoved(join.center()),
								egui::Event::PointerButton {
									pos: join.center(),
									button: egui::PointerButton::Primary,
									pressed,
									modifiers: egui::Modifiers::NONE,
								},
							],
						);
						sent.extend(commands);
					}
					assert!(sent.is_empty(), "the controller sends call commands itself");
					let joined = state
						.voice
						.active
						.as_ref()
						.is_some_and(|call| call.channel == Id(22));
					assert_eq!(
						joined,
						mode == 0,
						"Demo, offline, voice-unavailable and busy states cannot join"
					);
					state.apply_voice(client_core::voice::Event::Deleted { channel: Id(22) });
					let (text, _) = frame(&ctx, &mut messaging, &mut state, width, vec![]);
					assert!(
						!text
							.iter()
							.any(|(label, _)| label == "Join call" || label == "Join with video")
					);
				}
			}
		}
	}

	#[test]
	fn voice_popup_keeps_device_selection_open_and_demo_controls_inert() {
		fn labels(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
			match shape {
				egui::Shape::Text(text) => out.push((
					text.galley.job.text.clone(),
					text.galley.rect.translate(text.pos.to_vec2()),
				)),
				egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| labels(shape, out)),
				_ => {}
			}
		}
		fn frame(
			ctx: &egui::Context,
			messaging: &mut MessagingUi,
			demo: bool,
			events: Vec<egui::Event>,
		) -> Vec<(String, egui::Rect)> {
			let mut output = ctx.run_ui(
				egui::RawInput {
					screen_rect: Some(egui::Rect::from_min_size(
						egui::Pos2::ZERO,
						egui::vec2(800.0, 800.0),
					)),
					events,
					..Default::default()
				},
				|ui| {
					let trigger = ui.button(crate::i18n::translate("voice-frame-open-voice"));
					messaging.voice_settings_popup(&trigger, demo, false, true);
				},
			);
			output.textures_delta.clear();
			let mut text = vec![];
			for shape in output.shapes {
				labels(&shape.shape, &mut text);
			}
			text
		}
		fn click(ctx: &egui::Context, messaging: &mut MessagingUi, demo: bool, pos: egui::Pos2) {
			for pressed in [true, false] {
				frame(
					ctx,
					messaging,
					demo,
					vec![
						egui::Event::PointerMoved(pos),
						egui::Event::PointerButton {
							pos,
							button: egui::PointerButton::Primary,
							pressed,
							modifiers: egui::Modifiers::NONE,
						},
					],
				);
			}
		}
		let position = |text: &[(String, egui::Rect)], label: &str| {
			text.iter()
				.find(|(value, _)| value == label)
				.unwrap_or_else(|| panic!("Missing visible control: {label}"))
				.1
				.center()
		};
		for demo in [false, true] {
			let ctx = egui::Context::default();
			let mut messaging = MessagingUi {
				voice_available: true,
				voice_inputs: vec![("synthetic-input".into(), "Synthetic headset".into())],
				voice_gain: crate::VoiceGain {
					input_percent: 140,
					output_percent: 80,
				},
				..Default::default()
			};
			frame(&ctx, &mut messaging, demo, vec![]);
			let text = frame(&ctx, &mut messaging, demo, vec![]);
			click(&ctx, &mut messaging, demo, position(&text, "Open voice"));
			let text = frame(&ctx, &mut messaging, demo, vec![]);
			click(
				&ctx,
				&mut messaging,
				demo,
				position(&text, "System default"),
			);
			let text = frame(&ctx, &mut messaging, demo, vec![]);
			if demo {
				assert!(
					!egui::Popup::is_any_open(&ctx),
					"Disabled devices must not open a picker"
				);
				click(
					&ctx,
					&mut messaging,
					demo,
					position(&text, "Refresh devices"),
				);
				let text = frame(&ctx, &mut messaging, demo, vec![]);
				click(&ctx, &mut messaging, demo, position(&text, "Reset levels"));
				assert_eq!(messaging.voice_input, None);
				assert!(!messaging.voice_refresh_devices);
				assert_eq!(messaging.voice_gain.input_percent, 140);
				assert_eq!(messaging.voice_gain.output_percent, 80);
			} else {
				assert!(
					egui::Popup::is_any_open(&ctx),
					"The device picker must survive its opening frame"
				);
				click(
					&ctx,
					&mut messaging,
					demo,
					position(&text, "Synthetic headset"),
				);
				assert_eq!(messaging.voice_input.as_deref(), Some("synthetic-input"));
				assert!(!egui::Popup::is_any_open(&ctx));
			}
			let text = frame(&ctx, &mut messaging, demo, vec![]);
			assert!(
				text.iter().any(|(value, _)| value == "All voice settings"),
				"Changing settings must keep the voice popup open"
			);
			click(&ctx, &mut messaging, demo, egui::pos2(760.0, 760.0));
			let text = frame(&ctx, &mut messaging, demo, vec![]);
			assert!(
				!text.iter().any(|(value, _)| value == "All voice settings"),
				"Clicking outside must close the popup"
			);
		}
	}

	#[test]
	fn gain_sliders_accept_keyboard_input_and_reset_with_the_session() {
		let mut messaging = MessagingUi::default();
		assert_eq!(messaging.voice_gain.input_percent, 100);
		assert_eq!(messaging.voice_gain.output_percent, 100);
		let ctx = egui::Context::default();
		let raw = || egui::RawInput {
			screen_rect: Some(egui::Rect::from_min_size(
				egui::Pos2::ZERO,
				egui::vec2(240.0, 260.0),
			)),
			..Default::default()
		};
		ctx.run_ui(raw(), |ui| {
			gain_controls(ui, &mut messaging.voice_gain)[0].request_focus();
		})
		.drop_without_applying_deltas();
		let mut input = raw();
		input.events.push(egui::Event::Key {
			key: egui::Key::ArrowRight,
			physical_key: None,
			pressed: true,
			repeat: false,
			modifiers: egui::Modifiers::NONE,
		});
		ctx.run_ui(input, |ui| {
			let controls = gain_controls(ui, &mut messaging.voice_gain);
			assert!(
				controls
					.iter()
					.all(|r| r.rect.right() <= ui.max_rect().right() + 1.0)
			);
		})
		.drop_without_applying_deltas();
		assert_eq!(messaging.voice_gain.input_percent, 101);
		assert_eq!(messaging.voice_gain.output_percent, 100);
		assert!(
			!messaging.voice_refresh_devices,
			"Gain does not enumerate devices"
		);
		messaging.voice_gain.input_percent = u16::MAX;
		messaging.voice_gain.output_percent = 0;
		ctx.run_ui(raw(), |ui| {
			gain_controls(ui, &mut messaging.voice_gain);
		})
		.drop_without_applying_deltas();
		assert_eq!(messaging.voice_gain.input_percent, 200);
		assert_eq!(messaging.voice_gain.output_percent, 0);
		messaging.clear();
		assert_eq!(messaging.voice_gain.input_percent, 100);
		assert_eq!(messaging.voice_gain.output_percent, 100);
	}

	#[test]
	fn guild_voice_requires_explicit_keyboard_join_and_demo_never_emits_media() {
		let mut state = test_support::demo_state();
		state.demo = false;
		assert!(matches!(
			state.select(Id(25)),
			Some(client_core::Command::History {
				channel: Id(25),
				..
			})
		));
		assert_eq!(state.selected, Some(Id(25)));
		assert!(state.voice.active.is_none());
		let mut messaging = MessagingUi {
			voice_available: true,
			..Default::default()
		};
		let context = egui::Context::default();
		let mut commands = vec![];
		context
			.run_ui(Default::default(), |ui| {
				messaging
					.call_button(ui, &mut state, Id(25), &mut commands)
					.request_focus();
			})
			.drop_without_applying_deltas();
		assert!(commands.is_empty());
		assert!(state.voice.active.is_none());
		let enter = egui::RawInput {
			events: vec![egui::Event::Key {
				key: egui::Key::Enter,
				physical_key: None,
				pressed: true,
				repeat: false,
				modifiers: egui::Modifiers::NONE,
			}],
			..Default::default()
		};
		context
			.run_ui(enter, |ui| {
				messaging.call_button(ui, &mut state, Id(25), &mut commands);
			})
			.drop_without_applying_deltas();
		assert!(commands.is_empty());
		let call = state.voice.active.as_mut().unwrap();
		assert_eq!(call.channel, Id(25));
		assert!(elapsed_label(call).is_none());
		call.connected_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(3663));
		call.phase = Phase::Waiting;
		assert_eq!(elapsed_label(call).as_deref(), Some("01:01:03"));
		call.channel_started_at =
			Some(std::time::Instant::now() - std::time::Duration::from_secs(7540));
		assert_eq!(elapsed_label(call).as_deref(), Some("02:05:40"));
		call.phase = Phase::Connecting;
		assert!(elapsed_label(call).is_none());
		state.demo = true;
		for width in [640.0, 1120.0] {
			let mut output = context.run_ui(
				egui::RawInput {
					screen_rect: Some(egui::Rect::from_min_size(
						egui::Pos2::ZERO,
						egui::vec2(width, 480.0),
					)),
					..Default::default()
				},
				|ui| {
					assert!(
						messaging
							.show(ui, &mut state)
							.iter()
							.all(|c| !matches!(c, Command::Voice(_) | Command::History { .. }))
					);
				},
			);
			output.textures_delta.clear();
		}
		assert!(messaging.take_avatar_requests().is_empty());
		state.voice.active = None;
		assert!(messaging.call_unavailable(&state, Id(25)).is_some());
		state.demo = false;
		messaging.voice_available = false;
		assert!(
			messaging
				.call_unavailable(&state, Id(25))
				.unwrap()
				.contains("Voice is unavailable")
		);
	}

	#[test]
	fn streaming_roster_hover_has_preview_and_watch_action() {
		let mut state = test_support::demo_state();
		state.voice.roster = vec![RosterEntry {
			guild: Id(10),
			channel: Id(25),
			participant: client_core::voice::Participant {
				user: Id(2),
				muted: false,
				deafened: false,
				server_muted: false,
				server_deafened: false,
				video: false,
				streaming: true,
			},
			member: None,
		}];
		let mut messaging = MessagingUi::default();
		let ctx = egui::Context::default();
		ctx.memory_mut(|memory| memory.set_everything_is_visible(true));
		let first = ctx.run_ui(
			egui::RawInput {
				screen_rect: Some(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(640.0, 420.0),
				)),
				..Default::default()
			},
			|ui| {
				ui.set_width(190.0);
				messaging.voice_participant(ui, &state, &state.voice.roster[0]);
			},
		);
		first.drop_without_applying_deltas();
		let mut output = ctx.run_ui(
			egui::RawInput {
				screen_rect: Some(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(640.0, 420.0),
				)),
				..Default::default()
			},
			|ui| {
				ui.set_width(190.0);
				messaging.voice_participant(ui, &state, &state.voice.roster[0]);
			},
		);
		fn labels(shape: &egui::Shape, text: &mut String) {
			match shape {
				egui::Shape::Text(label) => text.push_str(&label.galley.job.text),
				egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| labels(shape, text)),
				_ => {}
			}
		}
		let mut text = String::new();
		output
			.shapes
			.iter()
			.for_each(|shape| labels(&shape.shape, &mut text));
		output.textures_delta.clear();
		assert!(
			text.contains("Streaming Now") && text.contains("Watch Stream"),
			"{text}"
		);
		assert_eq!(
			messaging.stream_preview_request,
			Some((Id(10), Id(25), Id(2)))
		);
		messaging.stream_preview_request = None;
		for render_row in [true, false, true] {
			ctx.run_ui(Default::default(), |ui| {
				if render_row {
					messaging.voice_participant(ui, &state, &state.voice.roster[0]);
				}
			})
			.drop_without_applying_deltas();
			if !render_row {
				assert!(messaging.stream_preview_request.is_none());
			}
		}
		assert_eq!(
			messaging.stream_preview_request.take(),
			Some((Id(10), Id(25), Id(2))),
			"Reopening after the row disappears must refresh the still"
		);
		ctx.run_ui(Default::default(), |ui| {
			messaging.voice_participant(ui, &state, &state.voice.roster[0]);
		})
		.drop_without_applying_deltas();
		assert!(
			messaging.stream_preview_request.is_none(),
			"An uninterrupted hover requests only once"
		);
	}

	#[test]
	fn streaming_roster_click_still_opens_profile() {
		let state = test_support::voice_demo_state();
		let entry = &state.voice.roster[2];
		assert!(entry.participant.streaming);
		let mut messaging = MessagingUi::default();
		let ctx = egui::Context::default();
		let mut rect = egui::Rect::NOTHING;
		ctx.run_ui(Default::default(), |ui| {
			rect = ui
				.scope(|ui| messaging.voice_participant(ui, &state, entry))
				.response
				.rect;
		})
		.drop_without_applying_deltas();
		let pos = rect.left_center() + egui::vec2(40.0, 0.0);
		for pressed in [true, false] {
			ctx.run_ui(
				egui::RawInput {
					events: vec![
						egui::Event::PointerMoved(pos),
						egui::Event::PointerButton {
							pos,
							button: egui::PointerButton::Primary,
							pressed,
							modifiers: egui::Modifiers::NONE,
						},
					],
					..Default::default()
				},
				|ui| {
					ui.scope(|ui| messaging.voice_participant(ui, &state, entry));
				},
			)
			.drop_without_applying_deltas();
		}
		assert_eq!(
			messaging.profile.open_user().map(|user| user.id),
			Some(entry.participant.user)
		);
	}

	#[test]
	fn stream_preview_watch_clears_rejected_and_completed_intents() {
		for phase in [Phase::Connected, Phase::Waiting] {
			for present in [false, true] {
				let mut state = test_support::voice_demo_state();
				let call = state.voice.active.as_mut().unwrap();
				call.phase = phase;
				if !present {
					call.participants
						.retain(|participant| participant.user != Id(3));
				}
				let mut messaging = MessagingUi {
					stream_preview_watch: Some((Id(25), Id(3))),
					..Default::default()
				};
				let mut commands = vec![];
				messaging.apply_stream_preview_watch(&mut state, &mut commands);
				assert!(messaging.stream_preview_watch.is_none());
				assert_eq!(
					state.voice.active.as_ref().unwrap().watching,
					present.then_some(Id(3))
				);
				assert!(commands.is_empty());
				if !present {
					assert_eq!(
						state.status,
						"This stream is not available in the connected voice channel"
					);
				} else {
					state.status = "";
					messaging.stream_preview_watch = Some((Id(25), Id(3)));
					messaging.apply_stream_preview_watch(&mut state, &mut commands);
					assert!(messaging.stream_preview_watch.is_none());
					assert!(state.status.is_empty());
				}
			}
		}
	}

	#[test]
	fn viewing_an_incoming_call_never_answers_and_preview_cannot_call() {
		let mut state = State {
			demo: true,
			selected: Some(Id(1)),
			auth: AuthState::Authenticated,
			gateway_connected: true,
			channels: vec![model::Channel {
				last_message: None,
				id: Id(1),
				guild: None,
				parent_id: None,
				position: 0,
				name: "Synthetic DM".into(),
				kind: 1,
				recipients: vec![model::User {
					id: Id(2),
					name: "Synthetic peer".into(),
					avatar: None,
					webhook: false,
					kind: Default::default(),
					discriminator: 0,
					primary_guild: None,
					cosmetics: None,
				}],
				member_list_id: None,
				tags: None,
				message_count: None,
				icon: None,
			}],
			..Default::default()
		};
		state.voice.incoming = Some(Id(1));
		let mut messaging = MessagingUi {
			voice_available: true,
			..Default::default()
		};
		let context = egui::Context::default();
		let output = context.run_ui(Default::default(), |ui| {
			let commands = messaging.show(ui, &mut state);
			assert!(
				commands
					.iter()
					.all(|command| !matches!(command, Command::Voice(_)))
			);
		});
		output.drop_without_applying_deltas();
		assert!(state.voice.active.is_none());
		assert_eq!(state.voice.incoming, Some(Id(1)));
		assert!(messaging.call_unavailable(&state, Id(1)).is_some());
		state.demo = false;
		let output = context.run_ui(Default::default(), |ui| {
			let commands = messaging.show(ui, &mut state);
			assert!(
				commands
					.iter()
					.all(|command| !matches!(command, Command::Voice(_)))
			);
		});
		output.drop_without_applying_deltas();
		assert!(
			state.voice.active.is_none(),
			"Incoming calls require an explicit answer"
		);
		messaging.voice_available = false;
		assert!(
			messaging
				.call_unavailable(&state, Id(1))
				.unwrap()
				.contains("Voice is unavailable")
		);
		messaging.voice_available = true;
		assert!(messaging.call_unavailable(&state, Id(1)).is_none());
	}

	#[test]
	fn the_go_live_line_runs_round_the_rounded_edge_and_wraps() {
		let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(40.0, 30.0));
		let inner = rect.shrink(7.0);
		let start = rounded_outline(rect, 7.0, 0.0);
		assert_eq!(start, egui::pos2(17.0, 20.0));
		assert!(rounded_outline(rect, 7.0, -1.0).distance(start) < 1e-3);
		let mut last = start;
		for step in 1..=400 {
			let point = rounded_outline(rect, 7.0, step as f32 / 400.0);
			assert!(
				(inner.distance_to_pos(point) - 7.0).abs() < 1e-3,
				"{point:?}"
			);
			assert!(point.distance(last) < 0.5, "{point:?} jumps from {last:?}");
			last = point;
		}
	}

	#[test]
	fn go_live_shows_live_only_once_live_and_clears_on_the_stop_click() {
		let state = test_support::call_demo_state();
		let ctx = egui::Context::default();
		let mut screen = crate::screen::ScreenUi::default();
		let frame = |screen: &mut crate::screen::ScreenUi, time: f64| {
			let mut reveal = -1.0;
			let mut output = ctx.run_ui(
				egui::RawInput {
					time: Some(time),
					..Default::default()
				},
				|ui| {
					reveal = screen.live_reveal(ui.ctx());
					screen.show(ui.ctx(), &state);
				},
			);
			output.textures_delta.clear();
			reveal
		};
		screen.busy = true;
		assert_eq!(
			frame(&mut screen, 0.0),
			0.0,
			"a starting share shows no LIVE"
		);
		screen.live = true;
		assert_eq!(frame(&mut screen, 1.0), 0.0);
		assert_eq!(
			frame(&mut screen, 1.3),
			0.0,
			"LIVE waits for the stream sound and the closing ring"
		);
		assert!(frame(&mut screen, 1.5) > 0.5);
		assert_eq!(frame(&mut screen, 2.0), 1.0);
		screen.launch(&state);
		assert!(matches!(screen.request, Some(crate::screen::Request::Stop)));
		assert!(!screen.busy);
		assert_eq!(
			frame(&mut screen, 2.01),
			0.0,
			"stopping clears LIVE on the click"
		);
	}

	#[test]
	fn call_view_options_hide_non_video_people_and_the_own_camera() {
		let mut state = test_support::call_demo_state();
		let call = state.voice.active.as_mut().unwrap();
		call.camera = true;
		let channel = call.channel;
		let ctx = egui::Context::default();
		let mut messaging = MessagingUi {
			voice_camera_preview: Some(ctx.load_texture(
				"synthetic-camera",
				egui::ColorImage::filled([4, 3], egui::Color32::RED),
				Default::default(),
			)),
			..Default::default()
		};
		let entries = stage_participants(&state, channel);
		let everyone = messaging.stage_tiles(&state, channel, &entries).len();
		assert!(everyone >= 2);
		messaging.call_hide_non_video = true;
		let shown = messaging.stage_tiles(&state, channel, &entries);
		assert_eq!(shown.len(), 1, "only the camera tile stays");
		assert!(messaging.tile_has_video(&state, channel, &shown[0]));
		messaging.call_hide_own_camera = true;
		assert_eq!(
			messaging.stage_tiles(&state, channel, &entries).len(),
			everyone,
			"with no video showing, nobody is hidden"
		);
	}
}
