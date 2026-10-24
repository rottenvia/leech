//! Desktop playback of another participant's screen share. The choice is an explicit click
//! handled by the call controller; this runs the viewer transport for its watch plan. Frames
//! are decoded off the render thread and never written to disk.
use client_core::{
	State,
	voice::{self, Report},
};
use discord_voice::{Identity, Status};
use eframe::egui;
use std::sync::{Arc, Mutex};
use tokio::{runtime::Runtime, sync::watch, task::JoinHandle};

/// Latest stream picture and whether the UI still has to upload it.
type Slot = Option<(platform::video::Picture, bool)>;

/// Replace the latest picture in place: a plain copy of the decoder's planes into one reused
/// allocation. Colour conversion happens on the GPU when the UI uploads it.
fn store_frame(slot: &mut Slot, frame: discord_voice::RemoteFrame<'_>) -> bool {
	let picture = frame.picture;
	if !picture.is_valid() {
		return false;
	}
	let len = picture.data.len();
	let mut data = slot.take().map(|(old, _)| old.data).unwrap_or_default();
	if data.capacity() > 1024 * 1024 && data.capacity() > len.saturating_mul(4) {
		// Release a previous large resolution while keeping ordinary resize reuse.
		data = Vec::new();
	}
	data.clear();
	// Exact growth: never double a previous resolution's allocation beyond the frame.
	data.reserve_exact(len);
	data.extend_from_slice(picture.data);
	*slot = Some((picture.with_data(data), true));
	true
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Notice {
	Status(&'static str),
	Failed(&'static str),
}
struct Live {
	id: u64,
	task: JoinHandle<()>,
	events: watch::Receiver<Option<Notice>>,
	frames: Arc<Mutex<Slot>>,
	reported: Option<Report>,
}

#[derive(Default)]
pub(super) struct Watch {
	live: Option<Live>,
	/// GPU texture behind `ui.voice_stream_view`.
	texture: crate::gpu_video::VideoTexture,
}
impl Watch {
	pub fn stop(&mut self) {
		if let Some(live) = self.live.take() {
			live.task.abort();
		}
		// Logout stops polling, so release the GPU picture here; disposal waits a frame.
		self.texture = Default::default();
	}

	/// The offline demo watches inside the simulated Discord; no video is received. Only the
	/// `--demo-call-stream` fixture supplies a picture, which lasts while the share is watched.
	pub fn poll_simulated(&mut self, state: &State, ui: &mut ui::MessagingUi) {
		if !state.voice.watch_live() {
			ui.voice_stream_view = None;
		}
		ui.voice_stream_status = if ui.voice_stream_view.is_some() {
			"Watching the stream"
		} else if state.voice.watch_live() {
			"Offline preview · no stream is received"
		} else {
			state.voice.watch_status()
		};
	}

	pub fn poll(
		&mut self,
		runtime: &Runtime,
		state: &mut State,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
		identity: Option<Arc<Identity>>,
		audio: Option<std::sync::mpsc::SyncSender<discord_voice::Frame>>,
	) {
		let plan = state.voice.watch_plan();
		if self.live.as_ref().map(|live| live.id) != plan.map(|plan| plan.id) {
			self.stop();
			ui.voice_stream_view = None;
			if let Some(plan) = plan {
				let started = identity
					.ok_or("Connect the call before watching a stream")
					.and_then(|identity| {
						self.start(
							runtime,
							state,
							plan.id,
							plan.streamer.0,
							identity,
							audio,
							ctx,
						)
					});
				if let Err(error) = started {
					state.apply_voice(voice::Event::Media {
						id: plan.id,
						report: Report::Failed(error),
					});
				}
			}
		}
		if let Some(live) = &mut self.live {
			let mut report = match *live.events.borrow_and_update() {
				Some(Notice::Status(status)) => Some(Report::Status(status)),
				Some(Notice::Failed(error)) => Some(Report::Failed(error)),
				None => None,
			};
			if live.task.is_finished() && !matches!(report, Some(Report::Failed(_))) {
				report = Some(Report::Failed("The stream connection ended"));
			}
			// Uploading copies the planes into GPU staging, so it runs under the slot lock
			// and the decoder keeps refilling the same allocation.
			if let Ok(mut slot) = live.frames.try_lock()
				&& let Some((picture, fresh)) = slot.as_mut()
				&& std::mem::take(fresh)
				&& let Some(texture) = self.texture.upload(ctx, "remote-stream", picture.borrow())
			{
				ui.voice_stream_view = Some(texture);
				if !matches!(report, Some(Report::Failed(_))) {
					report = Some(Report::Live);
				}
			}
			// Each change is reported once; once video flows, only a failure follows.
			if let Some(report) = report
				&& live.reported != Some(report)
				&& !(live.reported == Some(Report::Live) && matches!(report, Report::Status(_)))
			{
				live.reported = Some(report);
				let id = live.id;
				state.apply_voice(voice::Event::Media { id, report });
			}
		} else {
			ui.voice_stream_view = None;
			self.texture = Default::default();
		}
		ui.voice_stream_status = state.voice.watch_status();
	}

	#[allow(clippy::too_many_arguments)] // One viewer: plan, identity, audio mix and wake.
	fn start(
		&mut self,
		runtime: &Runtime,
		state: &State,
		id: u64,
		streamer: u64,
		identity: Arc<Identity>,
		audio: Option<std::sync::mpsc::SyncSender<discord_voice::Frame>>,
		ctx: &egui::Context,
	) -> Result<(), &'static str> {
		let credentials = state
			.stream_credentials(id)
			.ok_or("Stream connection details expired")?;
		let frames = Arc::new(Mutex::new(None));
		let slot = frames.clone();
		let wake = ctx.clone();
		let sink: discord_voice::VideoSink = Arc::new(move |frame: discord_voice::RemoteFrame| {
			if frame.user != streamer {
				return;
			}
			if slot
				.lock()
				.is_ok_and(|mut slot| store_frame(&mut slot, frame))
			{
				wake.request_repaint();
			}
		});
		let (send, events) = watch::channel(None);
		let wake = ctx.clone();
		let task = runtime.spawn(async move {
			let (status_send, status_wake) = (send.clone(), wake.clone());
			let result =
				discord_voice::watch_stream(credentials, identity, sink, audio, move |event| {
					let status = match event {
						Status::Connecting => "Connecting to the stream…",
						Status::Discovering => "Checking the stream network…",
						Status::TransportReady | Status::Securing => "Securing the stream…",
						Status::WaitingForPeer => "Waiting for the streamer…",
						Status::Ready { .. } => "Stream secured · waiting for video",
						Status::RemoteAudio | Status::Speaking(_) | Status::CameraAvailable(_) => {
							return Ok(());
						}
					};
					status_send.send_replace(Some(Notice::Status(status)));
					status_wake.request_repaint();
					Ok(())
				})
				.await;
			let failure = result.err().unwrap_or("The stream connection ended");
			send.send_replace(Some(Notice::Failed(failure)));
			wake.request_repaint();
		});
		self.live = Some(Live {
			id,
			task,
			events,
			frames,
			reported: None,
		});
		Ok(())
	}
}
impl Drop for Watch {
	fn drop(&mut self) {
		self.stop();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Instant;

	fn frame(width: u32, height: u32, data: &[u8]) -> discord_voice::RemoteFrame<'_> {
		discord_voice::RemoteFrame {
			user: 7,
			picture: platform::video::Picture {
				width,
				height,
				layout: platform::video::Layout::I420,
				color: platform::video::Color::WEBRTC,
				rotation: 0,
				data,
			},
		}
	}

	fn i420(width: u32, height: u32, value: u8) -> Vec<u8> {
		vec![value; platform::video::Layout::I420.len(width, height).unwrap()]
	}

	#[test]
	fn watch_reuses_the_frame_buffer_and_flags_only_updates() {
		let mut slot = None;
		assert!(store_frame(&mut slot, frame(2, 2, &i420(2, 2, 1))));
		let planes = slot.as_ref().unwrap().0.data.as_ptr();
		for value in 0..20 {
			slot.as_mut().unwrap().1 = false;
			assert!(store_frame(&mut slot, frame(2, 2, &i420(2, 2, value))));
			let (picture, fresh) = slot.as_ref().unwrap();
			assert!(*fresh);
			assert_eq!(picture.data.as_ptr(), planes);
			assert_eq!(picture.data, i420(2, 2, value));
		}
		// A smaller picture reuses the allocation too.
		assert!(store_frame(&mut slot, frame(1, 1, &i420(1, 1, 9))));
		let (picture, _) = slot.as_ref().unwrap();
		assert_eq!(picture.data.as_ptr(), planes);
		assert_eq!((picture.width, picture.height), (1, 1));
	}

	#[test]
	fn watch_invalid_frames_preserve_the_last_valid_picture() {
		let mut slot = None;
		assert!(store_frame(&mut slot, frame(1, 1, &i420(1, 1, 4))));
		let planes = slot.as_ref().unwrap().0.data.as_ptr();
		for invalid in [
			frame(0, 1, &[]),
			frame(1, 0, &[]),
			frame(1, 1, &[0; 2]),
			frame(u32::MAX, u32::MAX, &[]),
		] {
			assert!(!store_frame(&mut slot, invalid));
			let (picture, _) = slot.as_ref().unwrap();
			assert_eq!(picture.data.as_ptr(), planes);
			assert_eq!((picture.width, picture.height), (1, 1));
		}
	}

	#[test]
	fn watch_releases_large_capacity_after_a_resolution_drop() {
		let mut slot = None;
		assert!(store_frame(
			&mut slot,
			frame(1920, 1080, &i420(1920, 1080, 0))
		));
		assert!(store_frame(&mut slot, frame(16, 16, &i420(16, 16, 0))));
		let (picture, _) = slot.take().unwrap();
		assert_eq!(picture.data.len(), 16 * 16 * 3 / 2);
		assert!(picture.data.capacity() <= 16 * 16 * 6);
	}

	#[test]
	fn watch_resolution_growth_does_not_double_the_capacity() {
		let mut slot = None;
		assert!(store_frame(
			&mut slot,
			frame(1600, 900, &i420(1600, 900, 0))
		));
		assert!(store_frame(
			&mut slot,
			frame(1920, 1080, &i420(1920, 1080, 0))
		));
		let (picture, _) = slot.take().unwrap();
		assert_eq!(picture.data.len(), 1920 * 1080 * 3 / 2);
		assert!(picture.data.capacity() <= 1920 * 1080 * 3 / 2);
	}

	/// Run this test executable directly under `/usr/bin/time -l` for process peak RSS.
	/// The synthetic 1080p I420 input is stored as the decoder sink does; no device or
	/// transport starts.
	#[test]
	#[ignore = "synthetic release CPU/RSS workload; run with --release --ignored --nocapture"]
	fn watch_frame_memory_workload() {
		let planes = i420(1920, 1080, 128);
		let mut samples = Vec::new();
		for run in 0..6 {
			let mut slot = None;
			let started = Instant::now();
			for _ in 0..120 {
				assert!(store_frame(&mut slot, frame(1920, 1080, &planes)));
				std::hint::black_box(&slot);
			}
			if run > 0 {
				samples.push(started.elapsed().as_secs_f64() * 1000.0);
			}
		}
		samples.sort_by(f64::total_cmp);
		println!(
			"watch_frame_memory_workload frames=120 samples_ms={samples:?} median_ms={:.3} frame_bytes={}",
			samples[2],
			planes.len()
		);
	}
}
