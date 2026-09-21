use std::sync::atomic::{AtomicUsize, Ordering};

use windows::{
    core::Interface,
    Win32::{
        Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
        Media::MediaFoundation::{
            CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate, IMFDXGIBuffer,
            IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample, IMFTransform,
            IMFVideoSampleAllocatorEx, METransformDrainComplete, METransformHaveOutput,
            METransformNeedInput, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video,
            MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264, MFT_CATEGORY_VIDEO_DECODER,
            MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_HARDWARE,
            MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
            MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
            MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFT_SET_TYPE_TEST_ONLY,
            MF_EVENT_FLAG_NO_WAIT, MF_E_NO_EVENTS_AVAILABLE, MF_SA_D3D11_AWARE,
            MF_TRANSFORM_ASYNC_UNLOCK,
        },
    },
};

use super::{
    device_error, validate_config, EncoderConfig, EncoderEvent, EncoderStartError,
    EncoderStartErrorKind, MfRuntime, SegmentMuxer,
};

mod mft;
pub use mft::attach_d3d11_manager;
pub(crate) use mft::release_mft_activates;
use mft::{
    configure_codec_api, create_nv12_allocator, nv12_input_filter, output_filter_for,
    set_codec_u32, supported_video_type,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HardwareEncoderCandidate {
    pub d3d11_aware: bool,
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
    _runtime: MfRuntime,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    _manager: IMFDXGIDeviceManager,
    allocator: IMFVideoSampleAllocatorEx,
    frame_duration_100ns: i64,
    input_credit: u32,
    draining: bool,
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

pub fn enumerate_hardware_h264_encoders(
    runtime: &MfRuntime,
) -> Result<Vec<HardwareEncoderCandidate>, EncoderStartError> {
    enumerate_hardware_encoders(runtime, VideoCodec::H264_HIGH)
}

/// Enumerates this machine's hardware encoders for `codec` and hands the
/// activation array to `pick`.
///
/// One place owns the `MFTEnumEx` allocation, so no caller can forget to give
/// it back -- the array is released whatever `pick` does with it.
///
/// # Safety
///
/// Media Foundation must be running on this thread (hold an [`MfRuntime`]).
unsafe fn with_hardware_encoders<T>(
    codec: VideoCodec,
    failure: &str,
    pick: impl FnOnce(&[Option<IMFActivate>]) -> T,
) -> Result<T, EncoderStartError> {
    let input_filter = nv12_input_filter();
    let output_filter = output_filter_for(codec);
    let mut activations: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input_filter),
        Some(&output_filter),
        &mut activations,
        &mut count,
    )
    .map_err(|error| EncoderStartError {
        kind: EncoderStartErrorKind::NoHardwareEncoder,
        diagnostics: format!("{failure}: {error}"),
    })?;
    // `MFTEnumEx` reports "nothing matched" as S_OK with a null array and a
    // count of 0 -- which this machine does for AV1. `from_raw_parts` rejects a
    // null pointer even for a zero-length slice, so a debug build aborts there
    // instead of returning an empty candidate list.
    let entries: &[Option<IMFActivate>] = if activations.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(activations, count as usize)
    };
    let picked = pick(entries);
    release_mft_activates(activations, count);
    Ok(picked)
}

fn enumerate_hardware_encoders(
    _runtime: &MfRuntime,
    codec: VideoCodec,
) -> Result<Vec<HardwareEncoderCandidate>, EncoderStartError> {
    unsafe {
        with_hardware_encoders(codec, "MFTEnumEx hardware encoder failed", |entries| {
            entries
                .iter()
                .filter_map(|activate| activate.as_ref())
                .map(|activate| {
                    let d3d11_aware = activate
                        .ActivateObject::<IMFTransform>()
                        .ok()
                        .and_then(|transform| transform.GetAttributes().ok())
                        .and_then(|attributes| attributes.GetUINT32(&MF_SA_D3D11_AWARE).ok())
                        .unwrap_or_default()
                        != 0;
                    HardwareEncoderCandidate { d3d11_aware }
                })
                .collect()
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
    let candidates = enumerate_hardware_encoders(&runtime, codec)?;
    if !candidates.iter().any(|candidate| candidate.d3d11_aware) {
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
        let output_filter = nv12_input_filter();
        let mut activations: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_ALL,
            Some(&input_filter),
            Some(&output_filter),
            &mut activations,
            &mut count,
        )
        .map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::UnsupportedFormat,
            diagnostics: format!("MFTEnumEx AV1 decoder failed: {error}"),
        })?;
        release_mft_activates(activations, count);
        Ok(count as usize)
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

pub fn negotiate_hardware_h264(
    config: &EncoderConfig,
    device: &ID3D11Device,
) -> Result<(), EncoderStartError> {
    validate_config(config)?;
    let _runtime = MfRuntime::start()?;
    unsafe {
        let mut failures = Vec::new();
        let accepted =
            with_hardware_encoders(VideoCodec::H264_HIGH, "MFTEnumEx failed", |entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_ref())
                    .any(|activate| {
                        let Ok(transform) = activate.ActivateObject::<IMFTransform>() else {
                            failures.push("ActivateObject failed".to_owned());
                            return false;
                        };
                        let aware = transform
                            .GetAttributes()
                            .ok()
                            .and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).ok())
                            .unwrap_or_default()
                            != 0;
                        if !aware {
                            failures.push("candidate is not D3D11-aware".to_owned());
                            return false;
                        }
                        if let Ok(attributes) = transform.GetAttributes() {
                            if let Err(error) = attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                            {
                                failures.push(format!("MF_TRANSFORM_ASYNC_UNLOCK: {error}"));
                                return false;
                            }
                        }
                        let _manager = match attach_d3d11_manager(&transform, device) {
                            Ok(manager) => manager,
                            Err(error) => {
                                failures.push(format!("D3D manager: {}", error.diagnostics));
                                return false;
                            }
                        };
                        if let Err(error) = configure_codec_api(&transform, config) {
                            failures.push(format!("CodecAPI: {}", error.diagnostics));
                            return false;
                        }
                        let output = match supported_video_type(
                            &transform,
                            false,
                            config,
                            VideoCodec::H264_HIGH,
                        ) {
                            Ok(media_type) => media_type,
                            Err(error) => {
                                failures.push(format!("GetOutputAvailableType: {error}"));
                                return false;
                            }
                        };
                        if let Err(error) =
                            transform.SetOutputType(0, &output, MFT_SET_TYPE_TEST_ONLY.0 as u32)
                        {
                            failures.push(format!("SetOutputType(test): {error}"));
                            return false;
                        }
                        if let Err(error) = transform.SetOutputType(0, &output, 0) {
                            failures.push(format!("SetOutputType: {error}"));
                            return false;
                        }
                        let input = match supported_video_type(
                            &transform,
                            true,
                            config,
                            VideoCodec::H264_HIGH,
                        ) {
                            Ok(media_type) => media_type,
                            Err(error) => {
                                failures.push(format!("GetInputAvailableType: {error}"));
                                return false;
                            }
                        };
                        if let Err(error) =
                            transform.SetInputType(0, &input, MFT_SET_TYPE_TEST_ONLY.0 as u32)
                        {
                            failures.push(format!("SetInputType(test): {error}"));
                            return false;
                        }
                        if let Err(error) = transform.SetInputType(0, &input, 0) {
                            failures.push(format!("SetInputType: {error}"));
                            return false;
                        }
                        true
                    })
            })?;
        if accepted {
            Ok(())
        } else {
            Err(EncoderStartError {
                kind: EncoderStartErrorKind::UnsupportedFormat,
                diagnostics: format!(
                    "no D3D11-aware hardware H.264 MFT accepted NV12/H.264 media types: {}",
                    failures.join("; ")
                ),
            })
        }
    }
}

/// `eAVEncH264VProfile_High`, which is also H.264's own `profile_idc` for
/// High. Named here so the export path can name the others (task1310).
pub const H264_PROFILE_HIGH: u32 = 100;

impl HardwareVideoEncoder {
    /// The recording path: H.264 High (task1000).
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
                        let output = supported_video_type(&transform, false, config, codec).ok()?;
                        transform.SetOutputType(0, &output, 0).ok()?;
                        let input = supported_video_type(&transform, true, config, codec).ok()?;
                        transform.SetInputType(0, &input, 0).ok()?;
                        let events = transform.cast::<IMFMediaEventGenerator>().ok()?;
                        transform
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                            .ok()?;
                        transform
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                            .ok()?;
                        let allocator = create_nv12_allocator(&manager, &input).ok()?;
                        Some((transform, events, manager, allocator))
                    })
            })?;
            let (transform, events, manager, allocator) =
                selected.ok_or_else(|| EncoderStartError {
                    kind: EncoderStartErrorKind::UnsupportedFormat,
                    // Naming the codec matters: this is also what an exhausted
                    // encode-session pool looks like from here, and a message
                    // that says H.264 while creating AV1 sends the reader the
                    // wrong way (task1740).
                    diagnostics: format!(
                        "no D3D11-aware hardware MFT session accepted configured NV12 in / {codec:?} out"
                    ),
                })?;
            let alive = ENCODERS_ALIVE.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(
                event = "hardware_encoder_created",
                codec = ?codec,
                alive,
                "hardware video encoder created"
            );
            Ok(Self {
                _runtime: runtime,
                transform,
                events,
                _manager: manager,
                allocator,
                frame_duration_100ns: 10_000_000 / i64::from(config.frame_rate),
                input_credit: 0,
                draining: false,
                // `NOTIFY_BEGIN_STREAMING` was sent above, inside the candidate
                // loop, before this value was built.
                streaming: true,
                drain_completed: false,
            })
        }
    }

    /// Poll events first. `ProcessInput` is legal only while this count is non-zero.
    pub fn poll(&mut self) -> Result<Vec<IMFSample>, EncoderStartError> {
        let mut output = Vec::new();
        unsafe {
            loop {
                let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
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
                        self.input_credit = self.input_credit.saturating_add(1)
                    }
                    kind if kind == METransformHaveOutput.0 as u32 => {
                        if let Some(sample) = self.process_output_for_event()? {
                            output.push(sample);
                        }
                    }
                    kind if kind == METransformDrainComplete.0 as u32 => {
                        self.draining = false;
                        self.drain_completed = true;
                    }
                    _ => {}
                }
            }
        }
        Ok(output)
    }

    pub fn has_input_credit(&self) -> bool {
        self.input_credit != 0 && !self.draining
    }

    pub fn allocate_input_sample(&self) -> Result<IMFSample, EncoderStartError> {
        unsafe {
            self.allocator
                .AllocateSample()
                .map_err(|error| device_error("NV12 sample allocation", error))
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
            resource.ok_or_else(|| EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: "allocator returned no D3D11 texture".into(),
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
        unsafe {
            sample
                .SetSampleTime(timestamp_100ns)
                .map_err(|error| device_error("input sample timestamp", error))?;
            sample
                .SetSampleDuration(self.frame_duration_100ns)
                .map_err(|error| device_error("input sample duration", error))?;
            self.transform
                .ProcessInput(0, sample, 0)
                .map_err(|error| device_error("hardware video encoder ProcessInput", error))?;
            self.input_credit -= 1;
            Ok(())
        }
    }

    pub fn begin_drain(&mut self) -> Result<(), EncoderStartError> {
        if self.draining {
            return Ok(());
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .map_err(|error| device_error("hardware video encoder drain", error))?;
            self.draining = true;
            Ok(())
        }
    }

    pub fn drain_complete(&self) -> bool {
        !self.draining
    }

    pub fn request_next_keyframe(&self) -> Result<(), EncoderStartError> {
        unsafe {
            let codec = self
                .transform
                .cast::<ICodecAPI>()
                .map_err(|error| device_error("hardware video encoder CodecAPI", error))?;
            set_codec_u32(&codec, &CODECAPI_AVEncVideoForceKeyFrame, 1)
        }
    }

    pub fn output_media_type(
        &self,
    ) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, EncoderStartError> {
        unsafe {
            self.transform
                .GetOutputCurrentType(0)
                .map_err(|error| device_error("hardware video encoder output media type", error))
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

    fn process_output_for_event(&self) -> Result<Option<IMFSample>, EncoderStartError> {
        unsafe {
            let info =
                self.transform
                    .GetOutputStreamInfo(0)
                    .map_err(|error| EncoderStartError {
                        kind: EncoderStartErrorKind::Device,
                        diagnostics: format!("GetOutputStreamInfo failed: {error}"),
                    })?;
            let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                let sample: IMFSample = MFCreateSample().map_err(|error| EncoderStartError {
                    kind: EncoderStartErrorKind::Device,
                    diagnostics: format!("MFCreateSample failed: {error}"),
                })?;
                let buffer = MFCreateMemoryBuffer(info.cbSize.max(1)).map_err(|error| {
                    EncoderStartError {
                        kind: EncoderStartErrorKind::Device,
                        diagnostics: format!("MFCreateMemoryBuffer failed: {error}"),
                    }
                })?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|error| EncoderStartError {
                        kind: EncoderStartErrorKind::Device,
                        diagnostics: format!("output AddBuffer failed: {error}"),
                    })?;
                Some(sample)
            } else {
                None
            };
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(sample),
                ..Default::default()
            };
            let mut status = 0;
            let result =
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
            let sample = std::mem::ManuallyDrop::into_inner(std::ptr::read(&output.pSample));
            drop(std::mem::ManuallyDrop::into_inner(std::ptr::read(
                &output.pEvents,
            )));
            result.map_err(|error| device_error("hardware video encoder ProcessOutput", error))?;
            Ok(sample)
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
impl Drop for HardwareVideoEncoder {
    fn drop(&mut self) {
        let alive = ENCODERS_ALIVE.fetch_sub(1, Ordering::Relaxed) - 1;
        let mut steps: Vec<&'static str> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
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
        // task4280: after the teardown, before the fields drop. Expect 2 (the
        // transform plus the `events` cast of it, one object); more names a
        // holder outside this struct. 0 = probe off.
        let transform_refcount = if crate::capture::leak_probe::enabled() {
            unsafe { crate::capture::leak_probe::com_refcount(&self.transform) }
        } else {
            0
        };
        tracing::info!(
            event = "hardware_encoder_dropped",
            alive,
            transform_refcount,
            draining = self.draining,
            drain_completed = self.drain_completed,
            teardown = steps.join(","),
            teardown_failed = failures.join(","),
            "hardware video encoder dropped"
        );
    }
}
