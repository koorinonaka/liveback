use std::{
    mem::ManuallyDrop,
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};

use windows::{
    core::{implement, IUnknown, Interface, HRESULT},
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Media::Audio::{
            eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
            IActivateAudioInterfaceCompletionHandler,
            IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
            IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
            AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
            PROCESS_LOOPBACK_MODE, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVE_FORMAT_PCM,
        },
        System::{
            Com::{
                CoCreateInstance, CoTaskMemAlloc, CoTaskMemFree,
                StructuredStorage::{
                    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
                },
                BLOB, CLSCTX_ALL,
            },
            Threading::{CreateEventW, WaitForSingleObject},
            Variant::VT_BLOB,
        },
    },
};

use super::{
    device_position_gap_frames, frames_to_100ns, should_interpolate_gap, zero_pcm, AUDIO_CHANNELS,
    AUDIO_SAMPLE_RATE, MAX_SYNTHETIC_SILENCE_GAP_FRAMES,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPacket {
    pub timestamp_100ns: i64,
    pub frames: u32,
    /// Owned PCM copied before IAudioCaptureClient::ReleaseBuffer.
    pub pcm: Vec<u8>,
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationCompletion {
    completion: mpsc::Sender<Result<IAudioClient, windows::core::Error>>,
    returned: mpsc::Sender<()>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationCompletion_Impl {
    fn ActivateCompleted(
        &self,
        operation: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| unsafe {
            let mut hr = HRESULT(0);
            let mut object: Option<IUnknown> = None;
            operation.ok()?.GetActivateResult(&mut hr, &mut object)?;
            hr.ok()?;
            object
                .ok_or_else(windows::core::Error::from_win32)?
                .cast::<IAudioClient>()
        })();
        let _ = self.completion.send(result);
        // Keep caller-owned handler alive until callback has finished touching `self`.
        let _ = self.returned.send(());
        Ok(())
    }
}

/// Owns COM interfaces on capture worker. `stop` releases every interface before worker exits.
pub struct ProcessLoopbackAudioCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// Signaled by WASAPI roughly once per engine period (~10ms). Always created and
    /// registered in `start()` (event-driven mode is unconditional -- `Start()` fails with
    /// `AUDCLNT_E_EVENTHANDLE_NOT_SET` if `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` is set without
    /// one), but ordinary `poll()` callers are free to ignore it and never wait on it; only
    /// `run_event_driven_capture` does. Closed in `stop`.
    period_event: HANDLE,
    block_align: usize,
    format: PcmFormat,
    /// Cumulative device position (frames) just past the end of the last packet returned
    /// by `poll`, used to detect gaps the driver doesn't flag with
    /// `AUDCLNT_BUFFERFLAGS_SILENT`. `None` until the first packet after `start`/reinit,
    /// so a discontinuity is never computed across a capture restart.
    last_device_end_frames: Option<u64>,
    synthetic_silence_frames: u64,
    /// Next `synthetic_silence_frames` value at which to emit a cumulative `info!` log,
    /// so a long silent stretch is observable in `liveback.log` without a log line per
    /// packet (packets can arrive every ~10ms).
    next_synthetic_silence_log_threshold_frames: u64,
    oversized_gap_events: u32,
    /// Task088 Step2: wall-clock time of the previous `poll()` call, used to log the
    /// inter-call cadence when `LIVIA_TASK088_AUDIO_POLL_TRACE=1`. `None` until the
    /// first call after `start`/reinit.
    last_poll_instant: Option<Instant>,
}

/// Task088 Step2: `LIVIA_TASK088_AUDIO_POLL_TRACE=1` gate, read once. Off by default
/// so normal runs don't pay a per-poll `info!` cost; on, `poll()` logs per-call cadence and
/// per-packet raw `flags`/device position/qpc needed to fill in the branch table in
/// 088's Steps (buffer overrun vs. timestamp-origin vs. downstream duration derivation).
fn audio_poll_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("LIVIA_TASK088_AUDIO_POLL_TRACE").as_deref() == Ok("1"))
}

#[derive(Clone, Copy, Debug)]
struct PcmFormat {
    sample_rate: u32,
    channels: usize,
    bits_per_sample: u16,
}

impl ProcessLoopbackAudioCapture {
    pub fn start(process_id: u32) -> windows::core::Result<Self> {
        if process_id == 0 {
            return Err(windows::core::Error::from_win32());
        }
        Self::start_process_loopback(
            process_id,
            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        )
    }

    /// Every application's audio *except* this process's own tree (task1430).
    ///
    /// The same virtual-device activation `start` uses, with the mode flipped
    /// and the target pointed at ourselves. Excluding Liveback is the whole
    /// reason this is one mixed track rather than two: the API can exclude
    /// exactly one process tree, and the one that must never be recorded is the
    /// one playing the recording back for review.
    pub fn start_excluding_self() -> windows::core::Result<Self> {
        Self::start_process_loopback(
            std::process::id(),
            PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        )
    }

    /// The Application Loopback activation both process-scoped paths share:
    /// only the mode constant and the target pid differ between them.
    fn start_process_loopback(
        target_process_id: u32,
        mode: PROCESS_LOOPBACK_MODE,
    ) -> windows::core::Result<Self> {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: target_process_id,
                    ProcessLoopbackMode: mode,
                },
            },
        };
        let blob_size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
        let blob_memory = unsafe { CoTaskMemAlloc(blob_size) }.cast::<u8>();
        if blob_memory.is_null() {
            return Err(windows::core::Error::from_win32());
        }
        // The blob is owned by the PROPVARIANT built below: its `Drop` runs
        // `PropVariantClear`, which `CoTaskMemFree`s a VT_BLOB's `pBlobData`.
        // Freeing it here as well is a double free (STATUS_HEAP_CORRUPTION on
        // every `start()`), so there is nothing to release on this path.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (&params as *const AUDIOCLIENT_ACTIVATION_PARAMS).cast::<u8>(),
                blob_memory,
                blob_size,
            );
        }
        let blob = BLOB {
            cbSize: blob_size as u32,
            pBlobData: blob_memory,
        };
        let variant = PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 { blob },
                }),
            },
        };
        let (sender, receiver) = mpsc::channel();
        let (returned_sender, returned_receiver) = mpsc::channel();
        let callback: IActivateAudioInterfaceCompletionHandler = ActivationCompletion {
            completion: sender,
            returned: returned_sender,
        }
        .into();
        let _operation = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&variant),
                &callback,
            )?
        };
        let client = receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| windows::core::Error::from_win32())??;
        returned_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| windows::core::Error::from_win32())?;
        // Application Loopback intentionally does not implement GetMixFormat on some
        // supported Windows builds. Use its documented PCM capture format explicitly.
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: AUDIO_CHANNELS,
            nSamplesPerSec: AUDIO_SAMPLE_RATE,
            nAvgBytesPerSec: AUDIO_SAMPLE_RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        unsafe { Self::initialize(client, &format) }
    }

    /// The whole system's mix instead of one process's (task165). Monitor
    /// capture has no process to point at -- a screen is whatever happens to be
    /// on it -- so it records the default render endpoint in loopback mode.
    /// Shared mode there accepts only the device's own mix format, which is why
    /// this asks `GetMixFormat` where the process path hardcodes 16-bit PCM.
    pub fn start_system() -> windows::core::Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            if mix.is_null() {
                return Err(windows::core::Error::from_win32());
            }
            // `&*mix`, never a copy: the endpoint's mix is a
            // `WAVEFORMATEXTENSIBLE`, whose `cbSize: 22` promises 22 more bytes
            // after the `WAVEFORMATEX` header. Copying just the header left
            // `Initialize` reading stack past it and refusing the format with
            // `E_INVALIDARG` -- which the caller reads as "no audio here" and
            // records the screen without a sound track (task198).
            let result = Self::initialize(client, &*mix);
            CoTaskMemFree(Some(mix.cast()));
            result
        }
    }

    /// The tail both start paths share: initialize in shared loopback mode with
    /// the caller's format, take the capture service, and arm the period event.
    unsafe fn initialize(
        client: IAudioClient,
        format: &WAVEFORMATEX,
    ) -> windows::core::Result<Self> {
        {
            let block_align = format.nBlockAlign as usize;
            let pcm_format = PcmFormat {
                sample_rate: format.nSamplesPerSec,
                channels: format.nChannels as usize,
                bits_per_sample: format.wBitsPerSample,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                format,
                None,
            )?;
            let capture = client.GetService::<IAudioCaptureClient>()?;
            let period_event = CreateEventW(None, false, false, None)?;
            // Close the event on failure: `stop()` (the only other CloseHandle
            // site) is unreachable if `Self` is never constructed.
            if let Err(error) = client
                .SetEventHandle(period_event)
                .and_then(|()| client.Start())
            {
                let _ = CloseHandle(period_event);
                return Err(error);
            }
            Ok(Self {
                client,
                period_event,
                capture,
                block_align,
                format: pcm_format,
                last_device_end_frames: None,
                synthetic_silence_frames: 0,
                next_synthetic_silence_log_threshold_frames: AUDIO_SAMPLE_RATE as u64,
                oversized_gap_events: 0,
                last_poll_instant: None,
            })
        }
    }

    /// Converts shared-mode mix PCM to fixed 48 kHz stereo float. `SourceBuffer.buffered`
    /// on the playback side is the *intersection* of the audio and video tracks, so a
    /// segment with zero audio fragments makes the whole segment's buffered range empty
    /// even though video is complete there, hard-stalling playback. `poll` therefore
    /// always hands this fixed-size zero-filled PCM for silent/gap frames (see
    /// `AUDCLNT_BUFFERFLAGS_SILENT` handling and gap interpolation there) instead of
    /// omitting them, so every segment gets at least one audio fragment.
    pub fn normalize_to_f32(&self, packet: &AudioPacket) -> Vec<f32> {
        if packet.pcm.is_empty() || self.format.channels == 0 {
            return Vec::new();
        }
        let source_frames = packet.frames as usize;
        let output_frames = ((source_frames as u64 * AUDIO_SAMPLE_RATE as u64)
            / self.format.sample_rate.max(1) as u64) as usize;
        let mut output = Vec::with_capacity(output_frames * 2);
        for index in 0..output_frames {
            let source =
                (index as u64 * self.format.sample_rate as u64 / AUDIO_SAMPLE_RATE as u64) as usize;
            let sample = |channel: usize| -> f32 {
                let channel = channel.min(self.format.channels - 1);
                let offset = (source.min(source_frames.saturating_sub(1)) * self.block_align)
                    + channel * (self.format.bits_per_sample as usize / 8);
                match self.format.bits_per_sample {
                    32 if offset + 4 <= packet.pcm.len() => {
                        f32::from_le_bytes(packet.pcm[offset..offset + 4].try_into().unwrap())
                    }
                    16 if offset + 2 <= packet.pcm.len() => {
                        i16::from_le_bytes(packet.pcm[offset..offset + 2].try_into().unwrap())
                            as f32
                            / i16::MAX as f32
                    }
                    _ => 0.0,
                }
            };
            output.push(sample(0));
            output.push(sample(1));
        }
        output
    }

    pub fn poll(&mut self) -> windows::core::Result<Vec<AudioPacket>> {
        // WASAPI hands packets over roughly every 10ms; anything slow here
        // shows up as a gap in the recording rather than as UI lag (task205).
        crate::insight_scope!("audio_poll");
        let trace = audio_poll_trace_enabled();
        let now = Instant::now();
        let since_last_poll_ms = trace
            .then(|| {
                self.last_poll_instant
                    .map(|previous| now.duration_since(previous).as_secs_f64() * 1000.0)
            })
            .flatten();
        if trace {
            self.last_poll_instant = Some(now);
        }
        let mut packets = Vec::new();
        let mut total_frames_this_call: u64 = 0;
        unsafe {
            loop {
                let count = self.capture.GetNextPacketSize()?;
                if count == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                let mut device: u64 = 0;
                let mut qpc: u64 = 0;
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut device),
                    Some(&mut qpc),
                )?;
                if trace {
                    tracing::info!(
                        timestamp_100ns = qpc,
                        frames,
                        device_position = device,
                        raw_flags = flags,
                        data_discontinuity = flags & 0x1 != 0,
                        "task088 audio poll packet"
                    );
                }
                total_frames_this_call += frames as u64;

                // IAudioCaptureClient can skip device frames between packets (buffer
                // glitches, packet coalescing) without flagging the missing span as
                // silent. Fill the gap with synthetic silence so the AAC timeline stays
                // contiguous; see `normalize_to_f32` for why a hole is worse than silence.
                if let Some(gap_frames) =
                    device_position_gap_frames(self.last_device_end_frames, device)
                {
                    if should_interpolate_gap(gap_frames) {
                        tracing::debug!(
                            gap_frames,
                            "interpolated synthetic silence for audio device position discontinuity"
                        );
                        self.record_synthetic_silence_frames(gap_frames);
                        packets.push(AudioPacket {
                            timestamp_100ns: qpc as i64
                                - frames_to_100ns(gap_frames, self.format.sample_rate),
                            frames: gap_frames as u32,
                            pcm: zero_pcm(gap_frames as u32, self.block_align),
                        });
                    } else {
                        self.oversized_gap_events += 1;
                        tracing::warn!(
                            gap_frames,
                            max_synthetic_gap_frames = MAX_SYNTHETIC_SILENCE_GAP_FRAMES,
                            "audio device position discontinuity exceeds synthetic silence interpolation cap; leaving gap unfilled"
                        );
                    }
                }

                let silent = flags & 0x2 != 0;
                let pcm = if silent || data.is_null() {
                    zero_pcm(frames, self.block_align)
                } else {
                    let byte_count = frames as usize * self.block_align;
                    std::slice::from_raw_parts(data, byte_count).to_vec()
                };
                self.capture.ReleaseBuffer(frames)?;
                if silent {
                    self.record_synthetic_silence_frames(frames as u64);
                }
                self.last_device_end_frames = Some(device + frames as u64);
                packets.push(AudioPacket {
                    timestamp_100ns: qpc as i64,
                    frames,
                    pcm,
                });
            }
        }
        if trace {
            tracing::info!(
                ?since_last_poll_ms,
                packets_this_call = packets.len(),
                total_frames_this_call,
                "task088 audio poll call"
            );
        }
        Ok(packets)
    }

    /// Accumulates `frames` of synthetic silence (interpolated gap or
    /// `AUDCLNT_BUFFERFLAGS_SILENT`), logging a running total at `info!` roughly once per
    /// `AUDIO_SAMPLE_RATE` frames so a long silent stretch is visible in `liveback.log`
    /// without a line per packet (packets can arrive every ~10ms).
    fn record_synthetic_silence_frames(&mut self, frames: u64) {
        self.synthetic_silence_frames += frames;
        if self.synthetic_silence_frames >= self.next_synthetic_silence_log_threshold_frames {
            tracing::info!(
                cumulative_synthetic_silence_frames = self.synthetic_silence_frames,
                "synthetic silence emitted to keep audio track continuous"
            );
            self.next_synthetic_silence_log_threshold_frames =
                self.synthetic_silence_frames + AUDIO_SAMPLE_RATE as u64;
        }
    }

    /// Cumulative frames of synthetic silence emitted so far (gap interpolation plus
    /// `AUDCLNT_BUFFERFLAGS_SILENT` packets). Runtime observability goes through the
    /// `tracing` calls in `poll` (see `MAX_SYNTHETIC_SILENCE_GAP_FRAMES`); this accessor
    /// exists so tests can assert on the accumulated count directly.
    #[cfg(test)]
    pub fn synthetic_silence_frames(&self) -> u64 {
        self.synthetic_silence_frames
    }

    /// Count of device-position discontinuities that exceeded
    /// `MAX_SYNTHETIC_SILENCE_GAP_FRAMES` and were left unfilled.
    #[cfg(test)]
    pub fn oversized_gap_events(&self) -> u32 {
        self.oversized_gap_events
    }

    pub fn stop(self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
            let _ = CloseHandle(self.period_event);
        }
    }

    /// Blocks up to `timeout_ms` for the next WASAPI period event, returning whether it fired
    /// (as opposed to timing out). A timeout is expected and used by
    /// `run_event_driven_capture` to periodically re-check its stop signal, not an error.
    pub(super) fn wait_for_period_event(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.period_event, timeout_ms) == WAIT_OBJECT_0 }
    }
}
