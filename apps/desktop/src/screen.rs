//! Desktop capture for this device's screen share. The call controller decides when a share
//! exists; this starts and stops the capture worker and stream transport to match its plan.
use client_core::{
	State,
	screen::{self, Settings, SourceId},
	voice::{self, Report, ShareState},
};
use discord_voice::{Identity, Status, screen::Worker};
use eframe::egui;
use std::sync::{Arc, mpsc};
use tokio::{runtime::Runtime, sync::watch, task::JoinHandle};

/// Capture workers allowed to finish closing in the background at once.
const MAX_RETIRING: usize = 4;

/// Names why a share ended, under the same opt-in variable as the voice reports.
fn note(event: &str, reason: &str) {
	if std::env::var_os("LEECH_VOICE_DIAGNOSTICS").is_some_and(|value| value == "1") {
		eprintln!("[Leech voice Screen] {event}={reason}");
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Notice {
	Status(&'static str),
	Live,
	Failed(&'static str),
}
struct Live {
	id: u64,
	worker: Worker,
	task: JoinHandle<()>,
	events: watch::Receiver<Option<Notice>>,
	reported: Option<Notice>,
}
struct SourceScan {
	generation: u64,
	context: Option<(u64, model::Id, u64)>,
	receive: mpsc::Receiver<Result<Vec<screen::Source>, &'static str>>,
}

#[derive(Default)]
pub(super) struct Screen {
	live: Option<Live>,
	retiring: Vec<mpsc::Receiver<Result<(), &'static str>>>,
	source_scan: Option<SourceScan>,
	/// Source discovery status, shown while the picker is open and no share runs.
	status: &'static str,
}
impl Screen {
	pub fn stop(&mut self) {
		self.retire();
		self.status = "";
	}
	fn retire(&mut self) {
		if let Some(live) = self.live.take() {
			live.task.abort();
			if self.retiring.len() < MAX_RETIRING {
				self.retiring.push(live.worker.shutdown());
			}
		}
	}

	/// Start and stop requests from the picker go to the call controller.
	fn requests(&mut self, state: &mut State, ui: &mut ui::MessagingUi) {
		match ui.screen.request.take() {
			Some(ui::screen::Request::Start(settings)) => {
				if !state.share_screen(settings) {
					self.status = "Screen sharing requires the current connected call";
				}
			}
			Some(ui::screen::Request::Stop) => {
				note("share_stopped", "requested");
				state.stop_sharing();
			}
			None => {}
		}
	}

	fn show_state(&self, state: &State, ui: &mut ui::MessagingUi) {
		let share = state.voice.share_state();
		ui.screen.busy = share != ShareState::Off;
		ui.screen.status = match share {
			ShareState::Off if !state.voice.share_status().is_empty() => state.voice.share_status(),
			ShareState::Off => self.status,
			ShareState::Stopping => "Stopping screen sharing…",
			ShareState::Starting | ShareState::Live => state.voice.share_status(),
		};
	}

	/// The offline demo shares into the simulated Discord; nothing is captured.
	pub fn poll_simulated(&mut self, state: &mut State, ui: &mut ui::MessagingUi) {
		ui.screen.supported = true;
		if std::mem::take(&mut ui.screen.refresh_requested) {
			ui.screen.sources = vec![
				screen::Source {
					id: SourceId::Display(1),
					name: "Display 1".into(),
				},
				screen::Source {
					id: SourceId::Window(2),
					name: "Leech".into(),
				},
			];
			ui.screen.selected = ui.screen.selected.or(Some(SourceId::Display(1)));
			self.status = "Choose a screen or window";
		}
		self.requests(state, ui);
		ui.screen.preview = None;
		ui.screen.capture_status = None;
		self.show_state(state, ui);
	}

	pub fn poll(
		&mut self,
		runtime: &Runtime,
		state: &mut State,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
		identity: Option<Arc<Identity>>,
	) {
		self.retiring
			.retain(|done| matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
		ui.screen.supported = discord_voice::screen::supported();
		if !ui.screen.open && ui.screen.request.is_none() && state.voice.share_plan().is_none() {
			ui.screen.sources.clear();
			ui.screen.selected = None;
		}
		self.poll_sources(state, ui, ctx);
		self.requests(state, ui);

		let plan = state.voice.share_plan();
		if self.live.as_ref().map(|live| live.id) != plan.map(|plan| plan.id) {
			self.retire();
			if let Some(plan) = plan {
				let started = identity
					.ok_or("Screen sharing requires the connected call")
					.and_then(|identity| {
						self.start(runtime, state, plan.id, plan.settings, identity, ctx)
					});
				if let Err(error) = started {
					note("share_start_failed", error);
					state.apply_voice(voice::Event::Media {
						id: plan.id,
						report: Report::Failed(error),
					});
				}
			}
		}
		if let Some(live) = &mut self.live {
			let mut notice = *live.events.borrow_and_update();
			if !matches!(notice, Some(Notice::Failed(_))) {
				if let Some(result) = live.worker.result() {
					notice = Some(Notice::Failed(
						result.err().unwrap_or("Screen capture stopped"),
					));
				} else if live.task.is_finished() {
					notice = Some(Notice::Failed("Screen-share connection ended"));
				}
			}
			live.worker.set_preview_visible(
				ui.screen.preview.is_none()
					|| (state.selected == state.voice.active.as_ref().map(|call| call.channel)
						&& !ctx.input(|input| input.viewport().minimized.unwrap_or(false))),
			);
			if let Some(frame) = live.worker.take_preview() {
				let image = egui::ColorImage::from_rgba_unmultiplied(
					[frame.width() as usize, frame.height() as usize],
					frame.as_raw(),
				);
				if let Some(texture) = &mut ui.screen.preview {
					texture.set(image, egui::TextureOptions::LINEAR);
				} else {
					ui.screen.preview =
						Some(ctx.load_texture("local-screen", image, egui::TextureOptions::LINEAR));
					let cue = model::notification_preferences::Sound::ScreenShareOn;
					if ui.notification_options.allows(cue) {
						ui.notification_preview = Some(cue);
						ctx.request_repaint();
					}
				}
			}
			ui.screen.capture_status = live.worker.capture_status();
			if notice.is_some() && notice != live.reported {
				live.reported = notice;
				let id = live.id;
				let report = match notice {
					Some(Notice::Status(status)) => Report::Status(status),
					Some(Notice::Live) => Report::Live,
					Some(Notice::Failed(error)) => {
						note("share_stopped", error);
						Report::Failed(error)
					}
					None => unreachable!("checked above"),
				};
				state.apply_voice(voice::Event::Media { id, report });
			}
		} else {
			ui.screen.preview = None;
			ui.screen.capture_status = None;
		}
		self.show_state(state, ui);
	}

	fn poll_sources(&mut self, state: &State, ui: &mut ui::MessagingUi, ctx: &egui::Context) {
		if ui.screen.refresh_requested {
			ui.screen.refresh_requested = false;
			if self.source_scan.is_none() && ui.screen.supported {
				let (send, receive) = mpsc::sync_channel(1);
				let wake = ctx.clone();
				self.status = "Looking for screens and windows…";
				match std::thread::Builder::new()
					.name("screen-sources".into())
					.spawn(move || {
						let _ = send.send(discord_voice::screen::sources());
						wake.request_repaint();
					}) {
					Ok(_) => {
						self.source_scan = Some(SourceScan {
							generation: state.generation,
							context: ui.screen.context,
							receive,
						})
					}
					Err(_) => self.status = "Could not start screen source discovery",
				}
			} else if !ui.screen.supported {
				self.status = "Screen sharing is unavailable on this platform";
			}
		}
		let Some(scan) = &self.source_scan else {
			return;
		};
		match scan.receive.try_recv() {
			Ok(_)
				if !ui.screen.open
					|| scan.generation != state.generation
					|| scan.context != ui.screen.context =>
			{
				self.source_scan = None;
				if ui.screen.open {
					ui.screen.refresh_requested = true;
					ctx.request_repaint();
				}
			}
			Ok(Ok(sources)) => {
				ui.screen.sources = sources;
				ui.screen.selected = ui
					.screen
					.selected
					.filter(|selected| {
						ui.screen
							.sources
							.iter()
							.any(|source| source.id == *selected)
					})
					.or_else(|| ui.screen.sources.first().map(|source| source.id));
				self.status = if ui.screen.sources.is_empty() {
					"No shareable screens or windows were found"
				} else if cfg!(target_os = "linux") {
					"Choose the system picker or, on X11, explicitly share the entire desktop"
				} else {
					"Choose a screen or window"
				};
				self.source_scan = None;
			}
			Ok(Err(error)) => {
				self.status = error;
				self.source_scan = None;
			}
			Err(mpsc::TryRecvError::Disconnected) => {
				self.status = "Screen source discovery stopped";
				self.source_scan = None;
			}
			Err(mpsc::TryRecvError::Empty) => {}
		}
	}

	fn start(
		&mut self,
		runtime: &Runtime,
		state: &State,
		id: u64,
		settings: Settings,
		identity: Arc<Identity>,
		ctx: &egui::Context,
	) -> Result<(), &'static str> {
		let credentials = state
			.stream_credentials(id)
			.ok_or("Screen-share connection details expired")?;
		let wake = ctx.clone();
		let (worker, video) = Worker::start(settings, move || wake.request_repaint())?;
		let (send, events) = watch::channel(None);
		let wake = ctx.clone();
		let task = runtime.spawn(async move {
			let (status_send, status_wake) = (send.clone(), wake.clone());
			let result = discord_voice::run_stream(credentials, identity, video, move |event| {
				let notice = match event {
					Status::Connecting => Notice::Status("Connecting screen-share transport…"),
					Status::Discovering => Notice::Status("Checking screen-share network…"),
					Status::TransportReady | Status::Securing => {
						Notice::Status("Securing screen video…")
					}
					// Alone in the call the share is already live; nobody is watching yet.
					Status::WaitingForPeer | Status::Ready { .. } => Notice::Live,
					Status::RemoteAudio | Status::Speaking(_) | Status::CameraAvailable(_) => {
						return Ok(());
					}
				};
				status_send.send_replace(Some(notice));
				status_wake.request_repaint();
				Ok(())
			})
			.await;
			let failure = match result {
				Ok(()) => "Screen-share connection ended",
				Err(error) => error,
			};
			send.send_replace(Some(Notice::Failed(failure)));
			wake.request_repaint();
		});
		self.live = Some(Live {
			id,
			worker,
			task,
			events,
			reported: None,
		});
		Ok(())
	}
}
