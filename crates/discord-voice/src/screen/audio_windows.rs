use crate::screen::{AudioChunk, MAX_AUDIO_SAMPLES};
use std::sync::{
	Arc,
	atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::mpsc::Sender;

#[cfg(target_os = "windows")]
pub(super) struct Audio {
	thread: Option<std::thread::JoinHandle<()>>,
	stop: Arc<AtomicBool>,
	failed: Arc<AtomicBool>,
}

#[cfg(target_os = "windows")]
impl Audio {
	/// Errors name the failing WASAPI step and its HRESULT, so a report from another PC
	/// says why.
	pub(super) fn start(
		send: Sender<AudioChunk>,
		ready: Arc<AtomicBool>,
		audio_epoch: Arc<AtomicU64>,
	) -> Result<Self, String> {
		// Its own stop flag: a failed audio worker must not end the video capture.
		let stop = Arc::new(AtomicBool::new(false));
		let failed = Arc::new(AtomicBool::new(false));
		let input = Samples {
			send,
			stop: stop.clone(),
			ready,
			epoch: audio_epoch,
			failed: failed.clone(),
		};
		let (started, result) = std::sync::mpsc::sync_channel(1);
		let thread = std::thread::Builder::new()
			.name("screen-audio".into())
			.spawn(move || {
				crate::timer::raise_audio_thread();
				if native::run(&input, started).is_err() {
					input.failed.store(true, Ordering::Release);
					input.stop.store(true, Ordering::Release);
				}
			})
			.map_err(|_| "worker thread could not start".to_owned())?;
		let audio = Self {
			thread: Some(thread),
			stop,
			failed,
		};
		match result.recv_timeout(std::time::Duration::from_secs(4)) {
			Ok(Ok(())) => Ok(audio),
			Ok(Err(error)) => Err(error),
			Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err("start timed out".into()),
			Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
				Err("worker stopped before starting".into())
			}
		}
	}

	pub(super) fn failed(&self) -> bool {
		self.failed.load(Ordering::Acquire)
			|| self
				.thread
				.as_ref()
				.is_some_and(std::thread::JoinHandle::is_finished)
	}
}

#[cfg(target_os = "windows")]
impl Drop for Audio {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Release);
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

// WASAPI owns each packet until ReleaseBuffer. COM and capture objects stay on this worker.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod native {
	use super::*;
	use crate::diagnostics::{Metrics, Scope, Stage};
	use std::{
		mem::{ManuallyDrop, size_of},
		os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
		sync::mpsc::{SyncSender, sync_channel},
		time::{Duration, Instant},
	};
	use windows::{
		Win32::{
			Foundation::{E_FAIL, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
			Media::Audio::*,
			System::{
				Com::{
					BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
					StructuredStorage::{
						PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
					},
				},
				Threading::{CreateEventW, WaitForSingleObject},
				Variant::VT_BLOB,
			},
		},
		core::{Error, HRESULT, Interface, Ref, Result, implement},
	};

	#[implement(IActivateAudioInterfaceCompletionHandler)]
	struct Completion {
		send: SyncSender<()>,
		// Keep the activation blob alive even if cancellation wins the completion race.
		_params: Arc<AUDIOCLIENT_ACTIVATION_PARAMS>,
	}

	impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
		fn ActivateCompleted(
			&self,
			_: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
		) -> Result<()> {
			let _ = self.send.try_send(());
			Ok(())
		}
	}

	fn activation_params(pid: u32) -> AUDIOCLIENT_ACTIVATION_PARAMS {
		AUDIOCLIENT_ACTIVATION_PARAMS {
			ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
			Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
				ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
					TargetProcessId: pid,
					ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
				},
			},
		}
	}

	type Started = SyncSender<std::result::Result<(), String>>;

	/// Names the WASAPI step in an error while keeping its HRESULT.
	fn step(name: &'static str) -> impl Fn(Error) -> Error {
		move |error| Error::new(error.code(), name)
	}

	pub(super) fn run(input: &Samples, started: Started) -> Result<()> {
		// This dedicated thread initializes and releases all COM objects in one MTA.
		unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
		let mut started = Some(started);
		let result = (|| -> Result<()> {
			while !input.stop.load(Ordering::Acquire) && !input.send.is_closed() {
				capture(input, &mut started)?;
			}
			Ok(())
		})();
		unsafe { CoUninitialize() };
		if let (Err(error), Some(started)) = (&result, started) {
			let _ = started.try_send(Err(error.to_string()));
		}
		result
	}

	fn activate(input: &Samples) -> Result<IAudioClient> {
		let params = Arc::new(activation_params(std::process::id()));
		let (send, done) = sync_channel(1);
		let completion: IActivateAudioInterfaceCompletionHandler = Completion {
			send,
			_params: params.clone(),
		}
		.into();
		// The blob points into `params`; PROPVARIANT's Rust Drop would call
		// PropVariantClear and try to free that Arc allocation with CoTaskMemFree.
		// The completion handler owns `params` for the whole async activation.
		let variant = ManuallyDrop::new(PROPVARIANT {
			Anonymous: PROPVARIANT_0 {
				Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
					vt: VT_BLOB,
					Anonymous: PROPVARIANT_0_0_0 {
						blob: BLOB {
							cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
							pBlobData: Arc::as_ptr(&params).cast_mut().cast(),
						},
					},
					..Default::default()
				}),
			},
		});
		// The native operation retains the agile completion handler and its immutable blob.
		let operation = unsafe {
			ActivateAudioInterfaceAsync(
				VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
				&IAudioClient::IID,
				Some(&*variant),
				&completion,
			)
		}
		.map_err(step("ActivateAudioInterfaceAsync"))?;
		let deadline = Instant::now() + Duration::from_secs(3);
		loop {
			if input.stop.load(Ordering::Acquire) {
				return Err(E_FAIL.into());
			}
			if Instant::now() >= deadline {
				return Err(Error::new(E_FAIL, "activation timed out"));
			}
			if done.recv_timeout(Duration::from_millis(50)).is_ok() {
				break;
			}
		}
		let mut status = HRESULT(0);
		let mut activated = None;
		// Completion has fired; GetActivateResult returns an owned COM reference.
		unsafe { operation.GetActivateResult(&mut status, &mut activated) }
			.map_err(step("GetActivateResult"))?;
		drop(operation);
		status.ok().map_err(step("activation"))?;
		activated.ok_or(E_FAIL)?.cast()
	}

	/// Shared-mode stereo 48 kHz, as 32-bit float or 16-bit PCM.
	fn initialize(client: &IAudioClient, float: bool, convert: bool) -> Result<()> {
		let (tag, bits) = if float { (3, 32) } else { (1, 16) }; // WAVE_FORMAT_IEEE_FLOAT, _PCM
		let format = WAVEFORMATEX {
			wFormatTag: tag,
			nChannels: 2,
			nSamplesPerSec: 48_000,
			nAvgBytesPerSec: 48_000 * u32::from(bits / 4),
			nBlockAlign: bits / 4,
			wBitsPerSample: bits,
			cbSize: 0,
		};
		let mut flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
		if convert {
			flags |= AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
		}
		// A 20 ms buffer, as in Microsoft's application loopback sample.
		unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &format, None) }
			.map_err(step("Initialize"))
	}

	/// `(float, convert)` in order of preference. Native conversion gives stereo 48 kHz
	/// independently of physical output formats. System audio failed to start on a tester's
	/// Windows 11 23H2 PC, so the plain float and 16-bit PCM setups of Microsoft's sample
	/// follow in case that build refuses it.
	const SETUPS: [(bool, bool); 3] = [(true, true), (true, false), (false, false)];

	fn capture(input: &Samples, started: &mut Option<Started>) -> Result<()> {
		if input.stop.load(Ordering::Acquire) {
			return Err(E_FAIL.into());
		}
		let event = unsafe { CreateEventW(None, false, false, None)? };
		// Created before the clients so it stays open until their COM references are released.
		let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
		let mut refused = None;
		for (float, convert) in SETUPS {
			let client = activate(input)?;
			if input.stop.load(Ordering::Acquire) || input.send.is_closed() {
				return Err(E_FAIL.into());
			}
			if let Err(error) = initialize(&client, float, convert) {
				refused.get_or_insert(error);
				continue;
			}
			let capture: IAudioCaptureClient =
				unsafe { client.GetService() }.map_err(step("GetService"))?;
			unsafe { client.SetEventHandle(HANDLE(event.as_raw_handle())) }
				.map_err(step("SetEventHandle"))?;
			if input.stop.load(Ordering::Acquire) || input.send.is_closed() {
				return Err(E_FAIL.into());
			}
			let epoch = input.epoch.load(Ordering::Acquire);
			let mut metrics = Metrics::new(Scope::ScreenAudio);
			metrics.checkpoint();
			let start = metrics.start();
			unsafe { client.Start() }.map_err(step("Start"))?;
			metrics.finish(Stage::CaptureRestart, start);
			metrics.checkpoint();
			if let Some(started) = started.take() {
				let _ = started.try_send(Ok(()));
			}
			let result = packets(input, &capture, &event, epoch, float, &mut metrics);
			// Always stop before releasing the capture client and event, including packet errors.
			let stopped = unsafe { client.Stop() };
			return result.and(stopped);
		}
		Err(refused.unwrap_or_else(|| E_FAIL.into()))
	}

	fn packets(
		input: &Samples,
		capture: &IAudioCaptureClient,
		event: &OwnedHandle,
		epoch: u64,
		float: bool,
		metrics: &mut Metrics,
	) -> Result<()> {
		let mut scratch = [0.0; MAX_AUDIO_SAMPLES];
		while !input.stop.load(Ordering::Acquire) && !input.send.is_closed() {
			if input.epoch.load(Ordering::Acquire) != epoch {
				// Stop/Reset/Start can leave process loopback permanently empty on Windows.
				// Recreate it instead; the old client's immutable epoch prevents buffered
				// audio from being relabeled across the encryption transition.
				metrics.poll(true, 0, false, 0);
				metrics.checkpoint();
				return Ok(());
			}
			match unsafe { WaitForSingleObject(HANDLE(event.as_raw_handle()), 50) } {
				WAIT_TIMEOUT => {
					metrics.poll(false, 0, true, 0);
					continue;
				}
				WAIT_OBJECT_0 => {}
				_ => return Err(E_FAIL.into()),
			}
			let mut dropped = 0;
			// Bound work per wakeup as well as the bytes in each packet.
			for _ in 0..32 {
				if input.stop.load(Ordering::Acquire)
					|| unsafe { capture.GetNextPacketSize()? } == 0
				{
					break;
				}
				let start = metrics.start();
				let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0, 0);
				unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
				metrics.finish(Stage::CaptureRead, start);
				let count = (frames as usize).saturating_mul(2);
				let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
				let align = if float {
					align_of::<f32>()
				} else {
					align_of::<i16>()
				};
				let valid = silent || (!data.is_null() && data.align_offset(align) == 0);
				// A packet longer than one chunk is split rather than refused.
				for offset in (0..count).step_by(MAX_AUDIO_SAMPLES).take_while(|_| valid) {
					let chunk = &mut scratch[..(count - offset).min(MAX_AUDIO_SAMPLES)];
					// WASAPI guarantees frames of the initialized format until ReleaseBuffer.
					if silent {
						chunk.fill(0.0);
					} else if float {
						chunk.copy_from_slice(unsafe {
							std::slice::from_raw_parts(data.cast::<f32>().add(offset), chunk.len())
						});
					} else {
						let pcm = unsafe {
							std::slice::from_raw_parts(data.cast::<i16>().add(offset), chunk.len())
						};
						for (sample, &value) in chunk.iter_mut().zip(pcm) {
							*sample = f32::from(value) / 32768.0;
						}
					}
					let start = metrics.start();
					if input.push_at(chunk, epoch) {
						metrics.finish(Stage::CaptureQueue, start);
					} else {
						dropped += 1;
					}
				}
				unsafe { capture.ReleaseBuffer(frames)? };
				if !valid {
					return Err(E_FAIL.into());
				}
			}
			metrics.poll(false, dropped, false, 0);
		}
		Ok(())
	}

	#[cfg(test)]
	mod tests {
		use super::*;
		#[test]
		fn activation_excludes_our_entire_process_tree() {
			let params = activation_params(42);
			assert_eq!(
				params.ActivationType,
				AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK
			);
			let process = unsafe { params.Anonymous.ProcessLoopbackParams };
			assert_eq!(process.TargetProcessId, 42);
			assert_eq!(
				process.ProcessLoopbackMode,
				PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
			);
		}

		#[test]
		#[ignore = "needs the Windows audio service"]
		fn every_fallback_setup_initializes() {
			unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap() };
			let (send, _receive) = tokio::sync::mpsc::channel(1);
			let input = Samples {
				send,
				stop: Arc::default(),
				ready: Arc::default(),
				epoch: Arc::default(),
				failed: Arc::default(),
			};
			for (float, convert) in SETUPS {
				initialize(&activate(&input).unwrap(), float, convert).unwrap();
			}
		}
	}
}

struct Samples {
	send: Sender<AudioChunk>,
	stop: Arc<AtomicBool>,
	ready: Arc<AtomicBool>,
	epoch: Arc<AtomicU64>,
	failed: Arc<AtomicBool>,
}

impl Samples {
	#[cfg(test)]
	fn push(&self, data: &[f32]) {
		let epoch = self.epoch.load(Ordering::Acquire);
		self.push_at(data, epoch);
	}

	fn push_at(&self, data: &[f32], epoch: u64) -> bool {
		if self.stop.load(Ordering::Acquire)
			|| !self.ready.load(Ordering::Acquire)
			|| epoch != self.epoch.load(Ordering::Acquire)
		{
			return false;
		}
		if data.len() > MAX_AUDIO_SAMPLES || !data.len().is_multiple_of(2) {
			self.failed.store(true, Ordering::Release);
			self.stop.store(true, Ordering::Release);
			return false;
		}
		if data.is_empty() {
			return false;
		}
		// Reserve first: a stalled transport must not allocate for dropped packets.
		if let Ok(permit) = self.send.try_reserve() {
			let samples = data
				.iter()
				.map(|&sample| {
					if sample.is_finite() {
						sample.clamp(-1.0, 1.0)
					} else {
						0.0
					}
				})
				.collect();
			if !self.stop.load(Ordering::Acquire)
				&& self.ready.load(Ordering::Acquire)
				&& epoch == self.epoch.load(Ordering::Acquire)
			{
				permit.send(AudioChunk { samples, epoch });
				return true;
			}
		}
		false
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn loopback_samples_are_gated_sanitized_and_bounded() {
		let (send, mut receive) = tokio::sync::mpsc::channel(1);
		let samples = Samples {
			send,
			stop: Arc::new(AtomicBool::new(false)),
			ready: Arc::new(AtomicBool::new(false)),
			epoch: Arc::new(AtomicU64::new(7)),
			failed: Arc::new(AtomicBool::new(false)),
		};
		assert!(!samples.push_at(&[0.25, -0.25], 7));
		assert!(receive.try_recv().is_err());
		samples.ready.store(true, Ordering::Release);
		assert!(samples.push_at(&[f32::NAN, f32::INFINITY, -2.0, 2.0], 7));
		assert!(!samples.push_at(&[0.5, -0.5], 7)); // Full queue drops the new chunk.
		samples.epoch.store(8, Ordering::Release);
		let chunk = receive.try_recv().unwrap();
		assert_eq!(chunk.samples, vec![0.0, 0.0, -1.0, 1.0]);
		assert_eq!(chunk.epoch, 7); // Queued samples retain their capture generation.
		assert!(receive.try_recv().is_err());
		assert!(!samples.push_at(&[0.25, -0.25], 7)); // A native wakeup across rekey is stale.
		assert!(receive.try_recv().is_err());
		samples.push(&vec![0.0; MAX_AUDIO_SAMPLES]);
		let chunk = receive.try_recv().unwrap();
		assert_eq!(chunk.samples.len(), MAX_AUDIO_SAMPLES);
		assert_eq!(chunk.epoch, 8);
		samples.stop.store(true, Ordering::Release);
		samples.push(&[0.25, -0.25]);
		assert!(receive.try_recv().is_err());
		samples.stop.store(false, Ordering::Release);
		samples.push(&vec![0.0; MAX_AUDIO_SAMPLES + 2]);
		assert!(samples.failed.load(Ordering::Acquire));
		assert!(samples.stop.load(Ordering::Acquire));
		assert!(receive.try_recv().is_err());
	}
}
