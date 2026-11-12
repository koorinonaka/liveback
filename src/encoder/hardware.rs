use std::sync::atomic::{AtomicUsize, Ordering};

use windows::{
    core::Interface,
    Win32::{
        Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
        Media::MediaFoundation::{
            CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate, IMFDXGIBuffer,
            IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample, IMFTransform,
            IMFVideoSampleAllocatorEx, METransformDrainComplete, METransformHaveOutput,
            METransformNeedInput, MFCreateDXGISurfaceBuffer, MFCreateSample, MFMediaType_Video,
            MFVideoFormat_AV1, MFVideoFormat_H264, MFT_CATEGORY_VIDEO_DECODER,
            MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_HARDWARE,
            MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
            MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_MESSAGE_SET_D3D_MANAGER, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT,
            MF_E_NO_EVENTS_AVAILABLE, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC_UNLOCK,
        },
    },
};

use super::nvenc::{NvencCodec, NvencParams, NvencSession};
use super::{
    device_error, validate_config, EncoderConfig, EncoderEvent, EncoderStartError,
    EncoderStartErrorKind, MfRuntime, SegmentMuxer,
};

mod credit;
mod mft;
pub use credit::{CreditWatch, CreditWindow};
pub use mft::attach_d3d11_manager;
use mft::{
    configure_codec_api, create_input_allocator, nv12_input_filter, output_filter_for,
    set_codec_u32, supported_video_type, TypeSide,
};
pub(crate) use mft::{first_available_type, process_output_once, with_mft_activates};

/// The picture format a hardware encoder session takes in (t261021-8388).
///
/// NV12 is what every session could always take: the capture path converts
/// its BGRA to it with `VideoProcessorBlt` first. ARGB32 hands the same BGRA
/// to the encoder as it is, so the encoder converts on its own engine and the
/// capture path only copies -- one 3D-queue job per frame instead of two,
/// which is where t261021-375d found the wait under a game's load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderInput {
    Nv12,
    Argb32,
}

impl EncoderInput {
    /// As the recording-start log line spells it.
    fn label(self) -> &'static str {
        match self {
            Self::Nv12 => "nv12",
            Self::Argb32 => "argb32",
        }
    }
}

/// Which codec a hardware encoder session produces.
///
/// The H.264 profile rides in the variant rather than in `EncoderConfig`
/// because it is H.264's concept and nothing else's: the export path's boundary
/// re-encoder has to reproduce the profile of the stream it splices into
/// (task1310), while AV1 has no knob to reproduce. Keeping it here also leaves
/// `EncoderConfig`'s two dozen construction sites alone, the same reasoning
/// `writing_into_memory` records for the muxer's destination.
///
/// Not a settings type. Recording's choice of codec is a `settings.rs` concern
/// and maps onto this at the boundary; a profile-carrying enum has no business
/// in a JSON file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264 { profile: u32 },
    Av1,
}

/// The boundary the doc comment above describes: `settings.rs` stores a plain
/// two-value choice, and the profile H.264 recording uses is this module's
/// business rather than the file's (task1760).
impl From<crate::settings::RecordingCodec> for VideoCodec {
    fn from(setting: crate::settings::RecordingCodec) -> Self {
        match setting {
            crate::settings::RecordingCodec::H264 => Self::H264_HIGH,
            crate::settings::RecordingCodec::Av1 => Self::Av1,
        }
    }
}

impl VideoCodec {
    /// What recording encodes as (task1000). Export asks for other profiles.
    pub const H264_HIGH: Self = Self::H264 {
        profile: H264_PROFILE_HIGH,
    };

    fn subtype(self) -> windows::core::GUID {
        match self {
            Self::H264 { .. } => MFVideoFormat_H264,
            Self::Av1 => MFVideoFormat_AV1,
        }
    }

    /// For diagnostics and the start-failure banner (task1760).
    fn label(self) -> &'static str {
        match self {
            Self::H264 { .. } => "H.264",
            Self::Av1 => "AV1",
        }
    }
}

/// How many `HardwareVideoEncoder`s are alive right now (task4280).
///
/// One recording creates exactly one (`capture/worker.rs`, at start). An export
/// creates its own -- `export/part_writer.rs` probes candidates in a loop, so
/// how many is not a fixed number. So after N record->stop rounds **with no
/// export**, this reads 0 if teardown happens and N if it does not -- which is the measurement that splits the two fixes task4280
/// has to choose between:
///
/// - **stays at N after N rounds** -> the *Rust* struct is still held. Find the
///   holder; MFT shutdown messages would not help, nothing is calling them.
/// - **returns to 0 but dedicated GPU memory still steps per round** -> the
///   struct dies and the COM/driver side keeps the memory. That is the case the
///   task's step 4 addresses (`NOTIFY_END_OF_STREAM` -> `COMMAND_DRAIN` ->
///   `NOTIFY_END_STREAMING` -> `UninitializeSampleAllocator` before the fields
///   fall).
///
/// **Measured 2026-09-11 (task4280 step 3): the second branch.** Three
/// record->stop rounds read `alive=0` after every stop while dedicated GPU
/// memory went 151.3 -> 172.6 -> 193.9 MB.
///
/// **And the fix that branch pointed at did not work.** The `Drop` below now
/// sends the full MFT shutdown; all four steps return `S_OK` (the log says so
/// per round) and the per-round delta is still exactly +21.3 MB. So the encoder
/// object is **exonerated twice over** -- the struct dies, and shutting the MFT
/// down properly changes nothing. Do not re-suspect it; read
/// `.agents/tasks/evidence/4280-.../measurement-2026-09-11-postfix.md` instead.
///
/// The pair with `PlaybackEngine`'s counter (`crate::playback`) was meant to say
/// whether the ~20.5 MB/round fixed component sits on the encoder or the
/// decoder. Measured 2026-09-11: **neither.** Five engine swaps with no
/// recording (positive control in the log) moved dedicated GPU memory by 0.0 MB.
/// The holder is something else the recording path creates -- most likely the
/// per-recording `ID3D11Device` in `capture::worker::CaptureSurface`, whose own
/// counter (`SURFACES_ALIVE`) also reads 0 after every stop.
static ENCODERS_ALIVE: AtomicUsize = AtomicUsize::new(0);

pub struct HardwareVideoEncoder {
    backend: Backend,
    input: EncoderInput,
    frame_duration_100ns: i64,
    input_credit: u32,
    draining: bool,
    /// t261019-9ce9: the credit's history for the boundary and per-second
    /// lines.
    credit_watch: CreditWatch,
    /// Held by both backends: the MP4 sink the output goes to needs Media
    /// Foundation running, and an NVENC session alone does not start it
    /// (t261024-071a step 1.3: finalize crashed without it). Declared last so
    /// it drops last, after the backend it keeps running for.
    _runtime: MfRuntime,
}

/// Who encodes (t261024-071a). Recording takes NVENC's own API when the
/// driver offers it, because the MFT keeps only one frame in flight; every
/// other vendor stays on the MFT. The export's boundary re-encoder tries both
/// and keeps whichever reproduces the source's SPS (t261026-3ff2).
enum Backend {
    Mft(MftBackend),
    Nvenc(Box<NvencBackend>),
}

struct MftBackend {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    _manager: IMFDXGIDeviceManager,
    allocator: IMFVideoSampleAllocatorEx,
    /// True from `NOTIFY_BEGIN_STREAMING` until `Drop` sends
    /// `NOTIFY_END_STREAMING` (task4280 step 4).
    ///
    /// `draining` cannot carry this: it is `false` both before any drain and
    /// again after `METransformDrainComplete`, so it does not say whether the
    /// MFT was ever put into streaming state. The teardown below needs that
    /// distinction -- an MFT that never began streaming must not be sent
    /// `END_OF_STREAM`/`DRAIN`.
    streaming: bool,
    /// True once a drain ran to completion (`METransformDrainComplete` seen).
    /// `Drop` must not send a second `COMMAND_DRAIN` in that case.
    drain_completed: bool,
}

struct NvencBackend {
    session: NvencSession,
    /// `request_next_keyframe` takes `&self`; the next submit consumes this.
    force_keyframe: std::cell::Cell<bool>,
}

/// NVENC's `targetQuality` under VBR (0-51, lower is better), the counterpart
/// of the MFT's `QUALITY_LEVEL` (t261024-071a step 4). Chosen per codec so the
/// same moving picture comes out at about the MFT's bitrate: measured
/// 2026-10-25 on an NVIDIA GPU, 120 fps, per 1200 frames, the MFT at 18.9
/// (AV1) / 15.2 (H.264) Mbps against NVENC at 36 -> 17.7 and 32 -> 13.3,
/// 28 -> 20.7 (`evidence/t261024-071a-nvenc-depth/rate-results.txt`).
fn nvenc_target_quality(codec: NvencCodec) -> u8 {
    match codec {
        NvencCodec::Av1 => 36,
        NvencCodec::H264 => 31,
    }
}

/// Enumerates this machine's hardware encoders for `codec` and hands the
/// activation array to `pick`.
///
/// [`with_mft_activates`] releases the array whatever `pick` does with it.
///
/// # Safety
///
/// Media Foundation must be running on this thread (hold an [`MfRuntime`]).
unsafe fn with_hardware_encoders<T>(
    codec: VideoCodec,
    failure: &str,
    pick: impl FnOnce(&[Option<IMFActivate>]) -> T,
) -> Result<T, EncoderStartError> {
    with_mft_activates(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&nv12_input_filter()),
        Some(&output_filter_for(codec)),
        pick,
    )
    .map_err(|error| EncoderStartError {
        kind: EncoderStartErrorKind::NoHardwareEncoder,
        diagnostics: format!("{failure}: {error}"),
    })
}

/// Whether any of this machine's hardware encoders for `codec` is D3D11-aware.
fn any_d3d11_aware_encoder(
    _runtime: &MfRuntime,
    codec: VideoCodec,
) -> Result<bool, EncoderStartError> {
    unsafe {
        with_hardware_encoders(codec, "MFTEnumEx hardware encoder failed", |entries| {
            entries
                .iter()
                .filter_map(|activate| activate.as_ref())
                .any(|activate| {
                    activate
                        .ActivateObject::<IMFTransform>()
                        .ok()
                        .and_then(|transform| transform.GetAttributes().ok())
                        .and_then(|attributes| attributes.GetUINT32(&MF_SA_D3D11_AWARE).ok())
                        .unwrap_or_default()
                        != 0
                })
        })
    }
}

/// Recording's start-time gate (task1760): this machine must be able to record
/// `codec` -- and, for AV1, to decode it again.
///
/// Re-enumerated on every recording rather than answered from
/// [`av1_recording_supported`]'s cache, because the two gates exist for
/// different reasons: the settings screen asks once to decide what to *offer*,
/// while this one is what catches a hand-edited `settings.json` or a driver
/// that changed under a running install.
pub fn ensure_recording_codec(codec: VideoCodec) -> Result<(), EncoderStartError> {
    let runtime = MfRuntime::start()?;
    if !any_d3d11_aware_encoder(&runtime, codec)? {
        return Err(EncoderStartError {
            kind: EncoderStartErrorKind::NoHardwareEncoder,
            diagnostics: format!(
                "no D3D11-aware hardware {} Media Foundation encoder is available",
                codec.label()
            ),
        });
    }
    // An AV1 encoder without an AV1 decoder is the one outcome task1760 exists
    // to prevent: recordings that succeed and then cannot be played back in
    // this app's own review screen.
    if matches!(codec, VideoCodec::Av1) && count_av1_decoders(&runtime)? == 0 {
        return Err(EncoderStartError {
            kind: EncoderStartErrorKind::UnsupportedFormat,
            diagnostics: "this machine can encode AV1 but has no AV1 video decoder, so the \
                recording could not be played back (install AV1 Video Extension)"
                .into(),
        });
    }
    Ok(())
}

/// How many Media Foundation transforms decode AV1 into NV12 here.
///
/// Not `MFT_ENUM_FLAG_HARDWARE`, unlike every encoder enumeration above: the
/// AV1 decoder a stock Windows install has is `AV1VideoExtension`, a software
/// MFT that the hardware flag filters out. Encoder and decoder are separately
/// installable, which is the whole reason this function exists.
///
/// Counts activations without calling `ActivateObject` -- existence is the
/// question, and instantiating decoders to answer it would cost a session per
/// call for nothing.
pub fn count_av1_decoders(_runtime: &MfRuntime) -> Result<usize, EncoderStartError> {
    unsafe {
        let input_filter = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_AV1,
        };
        with_mft_activates(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_ALL,
            Some(&input_filter),
            Some(&nv12_input_filter()),
            |entries| entries.len(),
        )
        .map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::UnsupportedFormat,
            diagnostics: format!("MFTEnumEx AV1 decoder failed: {error}"),
        })
    }
}

/// Whether the settings screen may offer AV1 at all (task1760): both halves
/// present, encoder *and* decoder.
///
/// Cached, because it is asked on every settings render and the answer only
/// changes when a driver or an OS extension does -- neither of which happens
/// inside one run of the app.
pub fn av1_recording_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| match ensure_recording_codec(VideoCodec::Av1) {
        Ok(()) => true,
        Err(error) => {
            tracing::info!(
                event = "av1_unavailable",
                diagnostics = %error.diagnostics,
                "AV1 recording is not offered on this machine"
            );
            false
        }
    })
}

/// `eAVEncH264VProfile_High`, which is also H.264's own `profile_idc` for
/// High. Named here so the export path can name the others (task1310).
pub(super) const H264_PROFILE_HIGH: u32 = 100;

impl HardwareVideoEncoder {
    /// H.264 High, the recording default (task1000). Tests only: the app
    /// picks the codec through `create_for_recording` / `create_with_codec`.
    #[cfg(test)]
    pub fn create(
        config: &EncoderConfig,
        device: &ID3D11Device,
    ) -> Result<Self, EncoderStartError> {
        Self::create_with_codec(config, device, VideoCodec::H264_HIGH)
    }

    /// The same, at a named codec (task1310, task1740).
    ///
    /// Two callers want something other than the recording default. The
    /// export's boundary re-encoder has to reproduce the profile of the stream
    /// it is splicing into, and a recording made before task1000 is Baseline:
    /// its SPS says so, and `VideoCodec::H264 { profile }` carries that
    /// `profile_idc` -- 66, 77, 100 -- straight through. AV1 recording asks for
    /// `VideoCodec::Av1`, which has no profile to reproduce.
    pub fn create_with_codec(
        config: &EncoderConfig,
        device: &ID3D11Device,
        codec: VideoCodec,
    ) -> Result<Self, EncoderStartError> {
        Self::create_taking(config, device, codec, &[EncoderInput::Nv12])
    }

    /// Recording's session. NVENC's own API first (t261024-071a): it keeps
    /// several frames in flight where the MFT keeps one. When that is not on
    /// offer -- no `nvEncodeAPI64.dll`, an older driver API, a device it will
    /// not open, a codec or profile it does not take -- the MFT, ARGB32 in
    /// where it takes it and NV12 otherwise (t261021-8388). Which input the
    /// session took is [`Self::input`], and the caller has to feed that.
    pub fn create_for_recording(
        config: &EncoderConfig,
        device: &ID3D11Device,
        codec: VideoCodec,
    ) -> Result<Self, EncoderStartError> {
        match Self::create_nvenc(config, device, codec, EncoderInput::Argb32) {
            Ok(encoder) => return Ok(encoder),
            Err(reason) => tracing::info!(
                event = "nvenc_unavailable",
                codec = ?codec,
                reason = %reason,
                "NVENC API not used for this recording; falling back to the MFT"
            ),
        }
        Self::create_taking(
            config,
            device,
            codec,
            &[EncoderInput::Argb32, EncoderInput::Nv12],
        )
    }

    /// NVENC's own API at `input`: `Argb32` for recording, `Nv12` for the
    /// export's boundary re-encode, which has to reproduce an NVENC
    /// recording's SPS / PPS byte for byte and cannot from the MFT
    /// (t261026-3ff2: level, ref frames, VUI flags all differ). `Nv12` takes
    /// packed memory samples in [`Self::process_input_sample`].
    pub(crate) fn create_nvenc(
        config: &EncoderConfig,
        device: &ID3D11Device,
        codec: VideoCodec,
        input: EncoderInput,
    ) -> Result<Self, String> {
        validate_config(config).map_err(|error| error.diagnostics)?;
        let nvenc_codec = match codec {
            VideoCodec::Av1 => NvencCodec::Av1,
            VideoCodec::H264 {
                profile: H264_PROFILE_HIGH,
            } => NvencCodec::H264,
            VideoCodec::H264 { profile } => {
                return Err(format!(
                    "H.264 profile {profile} is not recorded through NVENC"
                ))
            }
        };
        let runtime = MfRuntime::start().map_err(|error| error.diagnostics)?;
        let session = NvencSession::open(
            device,
            NvencParams {
                codec: nvenc_codec,
                width: config.output_size.width as u32,
                height: config.output_size.height as u32,
                frame_rate: u32::from(config.frame_rate),
                target_quality: nvenc_target_quality(nvenc_codec),
                nv12_input: input == EncoderInput::Nv12,
            },
        )?;
        let alive = ENCODERS_ALIVE.fetch_add(1, Ordering::Relaxed) + 1;
        let (major, minor) = session.driver_version();
        tracing::info!(
            event = "hardware_encoder_created",
            codec = ?codec,
            input = input.label(),
            backend = "nvenc",
            depth = super::nvenc::DEPTH,
            driver_api = %format!("{major}.{minor}"),
            alive,
            "hardware video encoder created"
        );
        Ok(Self {
            _runtime: runtime,
            input_credit: session.free_slots() as u32,
            backend: Backend::Nvenc(Box::new(NvencBackend {
                session,
                force_keyframe: std::cell::Cell::new(false),
            })),
            input,
            frame_duration_100ns: 10_000_000 / i64::from(config.frame_rate),
            draining: false,
            credit_watch: CreditWatch::new(std::time::Instant::now()),
        })
    }

    /// `inputs` best first; the first one the session's `SetInputType` takes
    /// wins.
    pub(crate) fn create_taking(
        config: &EncoderConfig,
        device: &ID3D11Device,
        codec: VideoCodec,
        inputs: &[EncoderInput],
    ) -> Result<Self, EncoderStartError> {
        validate_config(config)?;
        let runtime = MfRuntime::start()?;
        unsafe {
            let selected = with_hardware_encoders(codec, "MFTEnumEx failed", |entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_ref())
                    .find_map(|activate| {
                        let transform = activate.ActivateObject::<IMFTransform>().ok()?;
                        let attributes = transform.GetAttributes().ok()?;
                        if attributes.GetUINT32(&MF_SA_D3D11_AWARE).ok()? == 0 {
                            return None;
                        }
                        attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).ok()?;
                        let manager = attach_d3d11_manager(&transform, device).ok()?;
                        configure_codec_api(&transform, config).ok()?;
                        let output =
                            supported_video_type(&transform, TypeSide::Output, config, codec)
                                .ok()?;
                        transform.SetOutputType(0, &output, 0).ok()?;
                        let (input, input_type) = inputs.iter().find_map(|&input| {
                            let input_type = supported_video_type(
                                &transform,
                                TypeSide::Input(input),
                                config,
                                codec,
                            )
                            .ok()?;
                            transform.SetInputType(0, &input_type, 0).ok()?;
                            Some((input, input_type))
                        })?;
                        let events = transform.cast::<IMFMediaEventGenerator>().ok()?;
                        transform
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                            .ok()?;
                        transform
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                            .ok()?;
                        let allocator = create_input_allocator(&manager, &input_type).ok()?;
                        Some((transform, events, manager, allocator, input))
                    })
            })?;
            let (transform, events, manager, allocator, input) =
                selected.ok_or_else(|| EncoderStartError {
                    kind: EncoderStartErrorKind::UnsupportedFormat,
                    // Naming the codec matters: this is also what an exhausted
                    // encode-session pool looks like from here, and a message
                    // that says H.264 while creating AV1 sends the reader the
                    // wrong way (task1740).
                    diagnostics: format!(
                        "no D3D11-aware hardware MFT session accepted configured {inputs:?} in / {codec:?} out"
                    ),
                })?;
            let alive = ENCODERS_ALIVE.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(
                event = "hardware_encoder_created",
                codec = ?codec,
                input = input.label(),
                backend = "mft",
                alive,
                "hardware video encoder created"
            );
            Ok(Self {
                _runtime: runtime,
                backend: Backend::Mft(MftBackend {
                    transform,
                    events,
                    _manager: manager,
                    allocator,
                    // `NOTIFY_BEGIN_STREAMING` was sent above, inside the
                    // candidate loop, before this value was built.
                    streaming: true,
                    drain_completed: false,
                }),
                input,
                frame_duration_100ns: 10_000_000 / i64::from(config.frame_rate),
                input_credit: 0,
                draining: false,
                credit_watch: CreditWatch::new(std::time::Instant::now()),
            })
        }
    }

    /// Poll events first. `ProcessInput` is legal only while this count is non-zero.
    pub fn poll(&mut self) -> Result<Vec<IMFSample>, EncoderStartError> {
        let mut output = Vec::new();
        let mut need_inputs = 0;
        match &mut self.backend {
            Backend::Mft(mft) => unsafe {
                loop {
                    let event = match mft.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                        Ok(event) => event,
                        Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                        Err(error) => {
                            return Err(device_error("hardware video encoder GetEvent", error))
                        }
                    };
                    match event
                        .GetType()
                        .map_err(|error| device_error("hardware video encoder event type", error))?
                    {
                        kind if kind == METransformNeedInput.0 as u32 => {
                            self.input_credit = self.input_credit.saturating_add(1);
                            need_inputs += 1;
                        }
                        kind if kind == METransformHaveOutput.0 as u32 => {
                            if let Some(sample) = mft.process_output_for_event()? {
                                if let Ok(pts) = sample.GetSampleTime() {
                                    self.credit_watch.on_output(std::time::Instant::now(), pts);
                                }
                                output.push(sample);
                            }
                        }
                        kind if kind == METransformDrainComplete.0 as u32 => {
                            self.draining = false;
                            mft.drain_completed = true;
                        }
                        _ => {}
                    }
                }
            },
            Backend::Nvenc(nvenc) => {
                let before = nvenc.session.free_slots();
                for packet in nvenc.session.poll().map_err(nvenc_error)? {
                    self.credit_watch
                        .on_output(std::time::Instant::now(), packet.pts_100ns);
                    output.push(packet.to_sample().map_err(nvenc_error)?);
                }
                // A freed slot is NVENC's `METransformNeedInput`.
                need_inputs = (nvenc.session.free_slots() - before) as u32;
                self.input_credit = nvenc.session.free_slots() as u32;
                if self.draining && nvenc.session.in_flight() == 0 {
                    // No B frames and no lookahead: every submitted frame has
                    // come back, so the drain is complete.
                    self.draining = false;
                }
            }
        }
        self.credit_watch
            .on_poll(std::time::Instant::now(), need_inputs, self.input_credit);
        Ok(output)
    }

    pub fn has_input_credit(&self) -> bool {
        self.input_credit != 0 && !self.draining
    }

    pub fn input_credit(&self) -> u32 {
        self.input_credit
    }

    /// The credit's history (t261019-9ce9), for the capture loop to open and
    /// close its windows on.
    pub fn credit_watch(&mut self) -> &mut CreditWatch {
        &mut self.credit_watch
    }

    /// What [`Self::allocate_input_sample`]'s textures hold, and so what the
    /// caller has to put in them.
    pub fn input(&self) -> EncoderInput {
        self.input
    }

    /// NVENC: the next free slot's registered texture, wrapped as an
    /// `IMFSample` so [`Self::input_sample_texture`] reads it the same way.
    /// The next [`Self::process_input_sample`] encodes that slot.
    pub fn allocate_input_sample(&self) -> Result<IMFSample, EncoderStartError> {
        match &self.backend {
            Backend::Mft(mft) => unsafe {
                mft.allocator
                    .AllocateSample()
                    .map_err(|error| device_error("input sample allocation", error))
            },
            Backend::Nvenc(nvenc) => unsafe {
                let texture = nvenc.session.next_texture().ok_or_else(|| {
                    EncoderStartError::new(
                        EncoderStartErrorKind::Device,
                        "NVENC input sample allocated with no free slot",
                    )
                })?;
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
                    .map_err(|error| device_error("NVENC input buffer", error))?;
                let sample =
                    MFCreateSample().map_err(|error| device_error("NVENC input sample", error))?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|error| device_error("NVENC input AddBuffer", error))?;
                Ok(sample)
            },
        }
    }

    pub fn input_sample_texture(sample: &IMFSample) -> Result<ID3D11Texture2D, EncoderStartError> {
        unsafe {
            let buffer = sample
                .GetBufferByIndex(0)
                .map_err(|error| device_error("allocator sample buffer", error))?;
            let dxgi = buffer
                .cast::<IMFDXGIBuffer>()
                .map_err(|error| device_error("allocator DXGI buffer", error))?;
            let mut resource = None;
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut resource as *mut _ as *mut _)
                .map_err(|error| device_error("allocator texture", error))?;
            resource.ok_or_else(|| {
                EncoderStartError::new(
                    EncoderStartErrorKind::Device,
                    "allocator returned no D3D11 texture",
                )
            })
        }
    }

    pub fn process_input_sample(
        &mut self,
        sample: &IMFSample,
        timestamp_100ns: i64,
    ) -> Result<(), EncoderStartError> {
        crate::insight_scope!("h264_submit");
        if !self.has_input_credit() {
            return Err(EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics:
                    "hardware video encoder input submitted without METransformNeedInput credit"
                        .into(),
            });
        }
        match &mut self.backend {
            Backend::Mft(mft) => unsafe {
                sample
                    .SetSampleTime(timestamp_100ns)
                    .map_err(|error| device_error("input sample timestamp", error))?;
                sample
                    .SetSampleDuration(self.frame_duration_100ns)
                    .map_err(|error| device_error("input sample duration", error))?;
                mft.transform
                    .ProcessInput(0, sample, 0)
                    .map_err(|error| device_error("hardware video encoder ProcessInput", error))?;
            },
            Backend::Nvenc(nvenc) => {
                // Argb32: `sample` wraps the slot texture `allocate_input_sample`
                // handed out; the session encodes that slot. Nv12: `sample` is
                // a packed memory frame, uploaded into the next slot here.
                let texture = if self.input == EncoderInput::Nv12 {
                    super::mf_sample::with_bytes(sample, |bytes| {
                        nvenc.session.fill_next_nv12(bytes)
                    })
                    .map_err(|error| device_error("NVENC NV12 input", error))?
                    .map_err(nvenc_error)?
                } else {
                    Self::input_sample_texture(sample)?
                };
                let force = nvenc.force_keyframe.replace(false);
                nvenc
                    .session
                    .submit(&texture, timestamp_100ns, force)
                    .map_err(nvenc_error)?;
            }
        }
        self.input_credit -= 1;
        self.credit_watch.on_submit(
            std::time::Instant::now(),
            timestamp_100ns,
            self.input_credit,
        );
        Ok(())
    }

    pub fn begin_drain(&mut self) -> Result<(), EncoderStartError> {
        if self.draining {
            return Ok(());
        }
        match &self.backend {
            Backend::Mft(mft) => unsafe {
                mft.transform
                    .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                    .map_err(|error| device_error("hardware video encoder drain", error))?;
                self.draining = true;
            },
            // Completes in `poll` once every frame in flight has come back.
            Backend::Nvenc(nvenc) => self.draining = nvenc.session.in_flight() != 0,
        }
        Ok(())
    }

    pub fn drain_complete(&self) -> bool {
        !self.draining
    }

    pub fn request_next_keyframe(&self) -> Result<(), EncoderStartError> {
        match &self.backend {
            Backend::Mft(mft) => unsafe {
                let codec = mft
                    .transform
                    .cast::<ICodecAPI>()
                    .map_err(|error| device_error("hardware video encoder CodecAPI", error))?;
                set_codec_u32(&codec, &CODECAPI_AVEncVideoForceKeyFrame, 1)
            },
            Backend::Nvenc(nvenc) => {
                nvenc.force_keyframe.set(true);
                Ok(())
            }
        }
    }

    pub fn output_media_type(
        &self,
    ) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, EncoderStartError> {
        match &self.backend {
            Backend::Mft(mft) => unsafe {
                mft.transform.GetOutputCurrentType(0).map_err(|error| {
                    device_error("hardware video encoder output media type", error)
                })
            },
            Backend::Nvenc(nvenc) => nvenc.session.output_media_type().map_err(nvenc_error),
        }
    }

    pub fn poll_to_muxer(
        &mut self,
        muxer: &mut SegmentMuxer,
    ) -> Result<Vec<EncoderEvent>, EncoderStartError> {
        // Drain plus mux: the encoder's output side, which is where a stalled
        // segment writer shows up as capture-thread time (task205).
        crate::insight_scope!("h264_poll_to_muxer");
        // What the sink refused last turn is owed to it before anything new, and
        // this is the call the capture loop makes every turn -- including the
        // turns where `poll` has nothing, which is most of them once the
        // encoder has stalled (t260911-e7f3).
        let mut events = muxer.drain_pending_video()?;
        for sample in self.poll()? {
            events.extend(muxer.push_video_sample(&sample)?);
        }
        Ok(events)
    }
}

fn nvenc_error(diagnostics: String) -> EncoderStartError {
    EncoderStartError {
        kind: EncoderStartErrorKind::Device,
        diagnostics,
    }
}

impl MftBackend {
    fn process_output_for_event(&self) -> Result<Option<IMFSample>, EncoderStartError> {
        unsafe { process_output_once(&self.transform) }.map_err(|(step, error)| match step {
            "ProcessOutput" => device_error("hardware video encoder ProcessOutput", error),
            step => device_error(step, error),
        })
    }

    /// The MFT half of the teardown `Drop` below logs.
    fn shut_down(&mut self, steps: &mut Vec<&'static str>, failures: &mut Vec<String>) {
        unsafe {
            if self.streaming {
                let mut send =
                    |name: &'static str, message| match self.transform.ProcessMessage(message, 0) {
                        Ok(()) => steps.push(name),
                        Err(error) => failures.push(format!("{name}={error}")),
                    };
                send("end_of_stream", MFT_MESSAGE_NOTIFY_END_OF_STREAM);
                if !self.drain_completed {
                    send("drain", MFT_MESSAGE_COMMAND_DRAIN);
                }
                send("end_streaming", MFT_MESSAGE_NOTIFY_END_STREAMING);
                // A null manager: the documented way to make the transform
                // release the DXGI device manager it was handed at creation.
                match self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0)
                {
                    Ok(()) => steps.push("detach_d3d_manager"),
                    Err(error) => failures.push(format!("detach_d3d_manager={error}")),
                }
                self.streaming = false;
            }
            match self.allocator.UninitializeSampleAllocator() {
                Ok(()) => steps.push("uninitialize_allocator"),
                Err(error) => failures.push(format!("uninitialize_allocator={error}")),
            }
        }
    }
}

/// Shuts the MFT down before the interface pointers fall (task4280 step 4).
///
/// **This is a correctness fix, not the VRAM fix.** Measured 2026-09-11: with
/// every step below returning `S_OK`, dedicated GPU memory still climbed +21.3 MB
/// per record->stop round, identical to the build that only logged here. It is
/// kept because an async MFT that is never told the stream ended is wrong on its
/// own terms, and because the next reader needs the negative result to be in the
/// code rather than only in a task file. **The leak's holder is elsewhere** --
/// see `ENCODERS_ALIVE` and
/// `.agents/tasks/evidence/4280-.../measurement-2026-09-11-postfix.md`.
///
/// **Why it was written.** Step 3's measurement (2026-09-11,
/// `.agents/tasks/evidence/4280-.../measurement-2026-09-11-prefix.md`) read
/// `ENCODERS_ALIVE` back to **0** after every record->stop round while dedicated
/// GPU memory still climbed +21.3 MB per round. That is the second branch of
/// `ENCODERS_ALIVE`'s decision table: the Rust struct dies and the COM/driver
/// side keeps the memory. Dropping the four interface pointers is not enough
/// because an async MFT that was never told the stream ended keeps its own
/// references -- to the DXGI device manager (and through it the capture side's
/// `ID3D11Device`) and to the encode session -- and the sample allocator keeps
/// its NV12 surfaces until it is explicitly uninitialized.
///
/// **Order matters** (Media Foundation's async MFT contract):
/// `NOTIFY_END_OF_STREAM` -> `COMMAND_DRAIN` -> `NOTIFY_END_STREAMING` ->
/// `SET_D3D_MANAGER(0)` -> `UninitializeSampleAllocator`. `SET_D3D_MANAGER` with
/// a null manager is what makes the MFT let go of the device manager; without it
/// the transform can outlive this struct holding the capture device.
///
/// **What this deliberately does not do**: wait for the drain to complete. The
/// completion arrives as `METransformDrainComplete` on the event generator,
/// which is pumped by `poll()` on the capture worker thread -- there is no pump
/// running inside `drop`. `COMMAND_DRAIN` is only sent when a drain was never
/// started, so the normal stop path (which drains and waits) reaches here with
/// `drain_completed == true` and sends no second drain.
///
/// Every step logs its HRESULT at `info!` rather than propagating: a `Drop` that
/// fails must not panic, and the observable behaviour of recording must not
/// change (task4280 Out of Scope).
///
/// The NVENC backend's session tears itself down in its own `Drop`
/// (`nvenc.rs`); only the counter and the line are shared.
impl Drop for HardwareVideoEncoder {
    fn drop(&mut self) {
        let alive = ENCODERS_ALIVE.fetch_sub(1, Ordering::Relaxed) - 1;
        let mut steps: Vec<&'static str> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        // task4280: after the teardown, before the fields drop. Expect 2 (the
        // transform plus the `events` cast of it, one object); more names a
        // holder outside this struct. 0 = probe off, or the NVENC backend.
        let transform_refcount = match &mut self.backend {
            Backend::Mft(mft) => {
                mft.shut_down(&mut steps, &mut failures);
                if crate::capture::leak_probe::enabled() {
                    unsafe { crate::capture::leak_probe::com_refcount(&mft.transform) }
                } else {
                    0
                }
            }
            Backend::Nvenc(_) => 0,
        };
        tracing::info!(
            event = "hardware_encoder_dropped",
            alive,
            transform_refcount,
            draining = self.draining,
            drain_completed = matches!(&self.backend, Backend::Mft(mft) if mft.drain_completed),
            teardown = steps.join(","),
            teardown_failed = failures.join(","),
            "hardware video encoder dropped"
        );
    }
}
