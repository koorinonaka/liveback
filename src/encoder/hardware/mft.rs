//! MFT plumbing shared by the hardware video encoder paths: enum filters,
//! media-type/codec-API configuration, the NV12 sample allocator and the DXGI
//! device-manager attachment. Split from `hardware.rs`, which keeps the
//! encoder session itself.

use windows::{
    core::{IUnknown, Interface},
    Win32::{
        Foundation::VARIANT_TRUE,
        Graphics::Direct3D11::{ID3D11Device, D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER},
        Media::MediaFoundation::{
            eAVEncCommonRateControlMode_Quality, eAVEncCommonRateControlMode_UnconstrainedVBR,
            CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonQuality,
            CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
            CODECAPI_AVLowLatencyMode, ICodecAPI, IMFDXGIDeviceManager, IMFTransform,
            IMFVideoSampleAllocatorEx, MFCreateAttributes, MFCreateDXGIDeviceManager,
            MFCreateVideoSampleAllocatorEx, MFMediaType_Video, MFNominalRange_16_235,
            MFVideoFormat_ARGB32, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
            MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
            MFT_MESSAGE_SET_D3D_MANAGER, MFT_REGISTER_TYPE_INFO, MF_MT_AVG_BITRATE,
            MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
            MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE,
            MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
            MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_SA_D3D11_BINDFLAGS,
        },
        System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4},
    },
};

use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaType, IMFSample, MFTEnumEx, MFT_ENUM_FLAG, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
};
use windows::Win32::System::Com::CoTaskMemFree;

use super::super::{
    bitrate_for, device_error, media_type_error, EncoderConfig, EncoderStartError,
    EncoderStartErrorKind,
};
use super::{EncoderInput, VideoCodec};

/// Which side of the transform a media type is for, and on the input side,
/// which picture format goes in (t261021-8388).
#[derive(Clone, Copy)]
pub(super) enum TypeSide {
    Input(EncoderInput),
    Output,
}

/// Frees an `MFTEnumEx` result: per its contract the caller must Release each
/// `IMFActivate` *and* `CoTaskMemFree` the array. Dropping each
/// `Option<IMFActivate>` in place does the Release; a bare `CoTaskMemFree`
/// alone leaked every activation object.
///
/// # Safety
/// `activations`/`count` must be exactly what `MFTEnumEx` returned, and no
/// element may have been moved out of the array.
unsafe fn release_mft_activates(activations: *mut Option<IMFActivate>, count: u32) {
    for index in 0..count as usize {
        std::ptr::drop_in_place(activations.add(index));
    }
    CoTaskMemFree(Some(activations.cast()));
}

/// Runs `MFTEnumEx` and hands the activation array to `pick`.
///
/// The one place that owns the `MFTEnumEx` allocation, so no caller can forget
/// to give it back -- the array is released whatever `pick` does with it.
///
/// # Safety
///
/// Media Foundation must be running on this thread (hold an `MfRuntime`).
pub(crate) unsafe fn with_mft_activates<T>(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<*const MFT_REGISTER_TYPE_INFO>,
    output: Option<*const MFT_REGISTER_TYPE_INFO>,
    pick: impl FnOnce(&[Option<IMFActivate>]) -> T,
) -> windows::core::Result<T> {
    let mut activations: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;
    MFTEnumEx(category, flags, input, output, &mut activations, &mut count)?;
    // `MFTEnumEx` reports "nothing matched" as S_OK with a null array and a
    // count of 0 -- which one test desk does for AV1. `from_raw_parts` rejects
    // a null pointer even for a zero-length slice, so a debug build aborts
    // there instead of returning an empty list.
    let entries: &[Option<IMFActivate>] = if activations.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(activations, count as usize)
    };
    let picked = pick(entries);
    release_mft_activates(activations, count);
    Ok(picked)
}

/// One `ProcessOutput` call on stream 0: allocates the output sample when the
/// MFT does not provide its own, and gives back what came out, if anything.
///
/// The error names the step that failed (`"GetOutputStreamInfo"`,
/// `"output sample"` or `"ProcessOutput"`), so each caller keeps its own
/// mapping -- the AAC encoder reads `MF_E_TRANSFORM_NEED_MORE_INPUT` from
/// `ProcessOutput` as "drained", the video encoder any error as a device
/// failure.
///
/// # Safety
///
/// `transform` must be a configured, streaming MFT.
pub(crate) unsafe fn process_output_once(
    transform: &IMFTransform,
) -> Result<Option<IMFSample>, (&'static str, windows::core::Error)> {
    let info = transform
        .GetOutputStreamInfo(0)
        .map_err(|error| ("GetOutputStreamInfo", error))?;
    let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
        Some(
            crate::encoder::mf_sample::empty(info.cbSize)
                .map_err(|error| ("output sample", error))?,
        )
    } else {
        None
    };
    let mut output = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: std::mem::ManuallyDrop::new(sample),
        ..Default::default()
    };
    let mut status = 0;
    let result = transform.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
    let sample = std::mem::ManuallyDrop::into_inner(std::ptr::read(&output.pSample));
    drop(std::mem::ManuallyDrop::into_inner(std::ptr::read(
        &output.pEvents,
    )));
    result.map_err(|error| ("ProcessOutput", error))?;
    Ok(sample)
}

/// The first type `transform` advertises on stream 0 with `subtype`, on the
/// input side or the output side, after `configure` has filled it in. `None`
/// when no advertised type has that subtype.
///
/// Only finds and configures: which side is set first is each MFT's own
/// requirement (video and audio differ), so the caller sets it.
///
/// # Safety
///
/// `transform` must be a live MFT.
pub(crate) unsafe fn first_available_type<E>(
    transform: &IMFTransform,
    input: bool,
    subtype: GUID,
    configure: impl FnOnce(&IMFMediaType) -> Result<(), E>,
) -> Result<Option<IMFMediaType>, E> {
    for index in 0..64 {
        let media_type = if input {
            transform.GetInputAvailableType(0, index)
        } else {
            transform.GetOutputAvailableType(0, index)
        };
        let Ok(media_type) = media_type else {
            break;
        };
        if media_type.GetGUID(&MF_MT_SUBTYPE).ok() != Some(subtype) {
            continue;
        }
        configure(&media_type)?;
        return Ok(Some(media_type));
    }
    Ok(None)
}

/// Every encoder here takes NV12 in, whatever it produces. Also the filter the
/// ARGB32 path enumerates with (t261021-8388): the NVIDIA MFTs list the same
/// encoder under both inputs, and ARGB32 is then negotiated on that encoder's
/// own type list -- so an MFT without ARGB32 is still found and records NV12.
/// The AV1 decoder count uses it too, so it stays NV12.
pub(super) fn nv12_input_filter() -> MFT_REGISTER_TYPE_INFO {
    MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    }
}

pub(super) fn output_filter_for(codec: VideoCodec) -> MFT_REGISTER_TYPE_INFO {
    MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: codec.subtype(),
    }
}

/// The advertised type for one side of the transform, configured for `config`.
///
/// The subtype is derived rather than passed: an [`EncoderInput`] going in and
/// `codec` coming out are the only combinations this encoder has, and a caller
/// that could name the two independently could name a mismatched pair -- which
/// compiles, finds nothing in the advertised list, and reports "MFT did not
/// advertise subtype" for what is really a configuration mistake (task1780).
pub(super) unsafe fn supported_video_type(
    transform: &IMFTransform,
    side: TypeSide,
    config: &EncoderConfig,
    codec: VideoCodec,
) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, String> {
    let subtype = match side {
        TypeSide::Input(EncoderInput::Nv12) => MFVideoFormat_NV12,
        TypeSide::Input(EncoderInput::Argb32) => MFVideoFormat_ARGB32,
        TypeSide::Output => codec.subtype(),
    };
    first_available_type(
        transform,
        matches!(side, TypeSide::Input(_)),
        subtype,
        |media_type| {
            configure_video_type(media_type, side, config, codec).map_err(|error| error.diagnostics)
        },
    )?
    .ok_or_else(|| format!("MFT did not advertise subtype {subtype:?}"))
}

unsafe fn configure_video_type(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    side: TypeSide,
    config: &EncoderConfig,
    codec: VideoCodec,
) -> Result<(), EncoderStartError> {
    let size = ((config.output_size.width as u64) << 32) | config.output_size.height as u64;
    let rate = (u64::from(config.frame_rate)) << 32 | 1;
    media_type
        .SetUINT64(&MF_MT_FRAME_SIZE, size)
        .map_err(media_type_error)?;
    media_type
        .SetUINT64(&MF_MT_FRAME_RATE, rate)
        .map_err(media_type_error)?;
    media_type
        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
        .map_err(media_type_error)?;
    media_type
        .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1u64 << 32)
        .map_err(media_type_error)?;
    // Both the NV12 input type and the H.264 output type must agree on studio
    // (16-235) BT.709 range/matrix/transfer/primaries so the encoded SPS VUI
    // carries this signaling instead of leaving players to guess the range
    // (see Task 033; GpuNv12Converter already produces studio-range BT.709 YUV
    // to match).
    //
    // An ARGB32 input gets no range or matrix (t261021-8388): it is
    // full-range RGB with no YUV matrix, and the encoder's own conversion
    // takes both from the output type. Saying 16-235 here would tell it the
    // RGB itself is studio range.
    if !matches!(side, TypeSide::Input(EncoderInput::Argb32)) {
        media_type
            .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            .map_err(media_type_error)?;
        media_type
            .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
            .map_err(media_type_error)?;
    }
    media_type
        .SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
        .map_err(media_type_error)?;
    media_type
        .SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
        .map_err(media_type_error)?;
    if let TypeSide::Input(input) = side {
        let width = i64::from(config.output_size.width);
        let pixels = width * i64::from(config.output_size.height);
        let (stride, sample_size) = match input {
            EncoderInput::Nv12 => (width, pixels * 3 / 2),
            EncoderInput::Argb32 => (width * 4, pixels * 4),
        };
        media_type
            .SetUINT32(&MF_MT_DEFAULT_STRIDE, stride as u32)
            .map_err(media_type_error)?;
        media_type
            .SetUINT32(&MF_MT_SAMPLE_SIZE, sample_size as u32)
            .map_err(media_type_error)?;
        return Ok(());
    }
    // Everything below is the compressed output type, whichever codec it is.
    // Both rate control rungs want a bitrate: constant quality treats it as
    // advisory, the VBR fallback steers to it (`configure_rate_control`).
    //
    // `bitrate_for`'s ladder is sized for H.264 (task1000), so AV1 asks for less
    // of it: task1730 measured AV1 at 63.0% of H.264 over identical frames, and
    // handing AV1 the H.264 number would steer the VBR fallback to roughly
    // 1.5x what the picture needs -- spending the capacity win on nothing. The
    // ratio came from synthetic screen content and the number is advisory under
    // constant quality anyway, so being a little off costs little (task1780).
    let bitrate = match codec {
        VideoCodec::H264 { .. } => bitrate_for(config.output_size),
        VideoCodec::Av1 => bitrate_for(config.output_size) * 6 / 10,
    };
    media_type
        .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
        .map_err(media_type_error)?;
    if let VideoCodec::H264 { profile } = codec {
        // High for recording (task1000): CABAC and the 8x8 transform are worth
        // appreciably more than they cost at these bitrates, and screen content
        // -- flat regions with hard edges -- is what they help most. (High also
        // permits B-frames; see `configure_codec_api` for why nothing here may
        // be allowed to produce them.)
        //
        // A parameter rather than a constant since task1310: the export's
        // boundary re-encoder has to *reproduce an existing stream*, and a
        // recording made before task1000 is Baseline. Its SPS says so, and that
        // is what gets passed here -- recording itself still asks for High.
        media_type
            .SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
            .map_err(media_type_error)?;
    }
    Ok(())
}

/// The input sample pool, NV12 or ARGB32 alike: the texture format follows
/// `input_type`.
pub(super) unsafe fn create_input_allocator(
    manager: &IMFDXGIDeviceManager,
    input_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> Result<IMFVideoSampleAllocatorEx, EncoderStartError> {
    let mut allocator = None;
    MFCreateVideoSampleAllocatorEx(
        &IMFVideoSampleAllocatorEx::IID,
        &mut allocator as *mut _ as *mut _,
    )
    .map_err(|error| device_error("MFCreateVideoSampleAllocatorEx", error))?;
    let allocator: IMFVideoSampleAllocatorEx = allocator.ok_or_else(|| {
        EncoderStartError::new(
            EncoderStartErrorKind::Device,
            "MFCreateVideoSampleAllocatorEx returned no allocator",
        )
    })?;
    allocator
        .SetDirectXManager(manager)
        .map_err(|error| device_error("allocator D3D manager", error))?;
    let mut attributes = None;
    MFCreateAttributes(&mut attributes, 2)
        .map_err(|error| device_error("allocator attributes", error))?;
    let attributes = attributes.ok_or_else(|| {
        EncoderStartError::new(
            EncoderStartErrorKind::Device,
            "allocator attribute creation returned nothing",
        )
    })?;
    attributes
        .SetUINT32(&MF_SA_D3D11_AWARE, 1)
        .map_err(media_type_error)?;
    attributes
        .SetUINT32(
            &MF_SA_D3D11_BINDFLAGS,
            (D3D11_BIND_RENDER_TARGET | D3D11_BIND_VIDEO_ENCODER).0 as u32,
        )
        .map_err(media_type_error)?;
    allocator
        .InitializeSampleAllocatorEx(4, 8, &attributes, input_type)
        .map_err(|error| device_error("input allocator initialization", error))?;
    Ok(allocator)
}

/// The codec-independent knobs every encoder session gets, whatever it encodes.
///
/// **Low latency is load-bearing, not a preference.** It is what suppresses
/// B-frames, and the muxer cannot take them: `write_video_sample_at` assumes
/// presentation order and writes no `ctts`, so a reordered sample would land at
/// the wrong time rather than be corrected. This applies to every codec here --
/// H.264 High permits B-frames, and AV1 encoders reorder too -- so neither this
/// nor the muxer's no-`ctts` assumption moves without the other (task1000,
/// task1780). The only test that would catch it going away for AV1 is
/// `av1_segments_reach_the_container_with_their_own_boxes`, which is `#[ignore]`d
/// for its encode session, so treat this comment as the guard.
///
/// GOP size is deliberately fatal on failure: it is the suspected fix for the
/// seek-time OBU warnings task1730 recorded on AV1, and a tolerated failure
/// would reintroduce them invisibly.
pub(super) unsafe fn configure_codec_api(
    transform: &IMFTransform,
    config: &EncoderConfig,
) -> Result<(), EncoderStartError> {
    let codec = transform
        .cast::<ICodecAPI>()
        .map_err(|error| device_error("hardware video encoder CodecAPI", error))?;
    // Half the configured frame rate, i.e. nominally 0.5s of GOP (task2620).
    // It does not land at 0.5s of wall clock: `AVEncMPVGOPSize` counts *encoded*
    // frames, and WGC feeds ~56fps against a configured 120, so 60 frames is
    // ~1.07s in practice (task2630 measured). That effective ~1s is the shipped
    // spec -- chasing wall-clock 0.5s would mean guessing an effective fps that
    // moves with load. What it buys: precise seek 65ms -> 34ms median, at
    // +5..23% video bitrate depending on content (task2530/2630). `.max(1)`
    // keeps a frame_rate of 1 from asking for a GOP of 0.
    set_codec_u32(
        &codec,
        &CODECAPI_AVEncMPVGOPSize,
        (u32::from(config.frame_rate) / 2).max(1),
    )
    .map_err(|mut error| {
        error.diagnostics = format!("GOP size: {}", error.diagnostics);
        error
    })?;
    set_codec_bool(&codec, &CODECAPI_AVLowLatencyMode).or_else(|primary| {
        set_codec_bool(&codec, &CODECAPI_AVEncCommonLowLatency).map_err(|fallback| {
            EncoderStartError {
                kind: EncoderStartErrorKind::UnsupportedFormat,
                diagnostics: format!(
                    "low latency unavailable: AVLowLatencyMode ({}) ; AVEncCommonLowLatency ({})",
                    primary.diagnostics, fallback.diagnostics
                ),
            }
        })
    })?;
    configure_rate_control(&codec);
    Ok(())
}

/// Constant quality on the CodecAPI's 0-100 scale, for the mode below. The one
/// knob this has: lower trades picture for capacity. It was 70; task1720 dropped
/// it to 60, and both values have been measured on the same machine.
///
/// What the knob buys depends entirely on the material. task1720 recorded a
/// synthetic flat-colour page and got only **8.4% smaller** (609.4 KB/s at 70
/// against 557.9 KB/s at 60) -- a scene whose flat colour and regular motion
/// leave little detail for a coarser quantizer to throw away. task1950 repeated the measurement on 2026-08-28 against real
/// content (real gameplay played back at 1920x1080: carved stone, particles,
/// the map and chat HUD at UI text size, camera panning throughout) and got
/// **23.3% smaller** -- 909.5 KB/s at 70 against 697.2 KB/s at 60, over 90 s
/// each at a matched 27.96 frames/s, with constant-quality rate control
/// confirmed accepted in both builds. Real recordings are also far more
/// expensive to begin with, which is where the extra saving comes from.
///
/// In neither measurement was any difference **visible** at 2x zoom, including
/// on the real-content pass at both a high-frequency HUD region and a
/// high-motion one: no blocking, no banding, no smeared texture. So 60 stands.
/// Evidence: `.agents/tasks/evidence/1950-quality-real-content/`.
const QUALITY_LEVEL: u32 = 60;

/// Picks the rate control the recording runs under, best first (task1000).
///
/// Constant quality is what the capacity win comes from: this records screens,
/// which hold still for long stretches, and a static picture at a fixed quality
/// costs almost nothing while motion still gets the bits it needs. Steering to
/// an average bitrate instead keeps paying for the still parts.
///
/// The rungs, in order: constant quality; failing that an unconstrained VBR
/// aimed at `MF_MT_AVG_BITRATE`, which at least lets quiet stretches fall below
/// it; failing that whatever the MFT does by default, which is what shipped
/// before this and is perfectly serviceable.
///
/// Deliberately not fatal, and deliberately not returning a `Result`. Vendor
/// support for the Quality mode is the shakiest of the modes, and it is a
/// preference about how a recording spends its bits -- a session that runs on
/// the driver's default rate control is enormously better than one that refuses
/// to start over it.
unsafe fn configure_rate_control(codec: &ICodecAPI) {
    // Mode before value: some MFTs reject `AVEncCommonQuality` unless the mode
    // it belongs to is already the active one.
    let quality = set_codec_u32(
        codec,
        &CODECAPI_AVEncCommonRateControlMode,
        eAVEncCommonRateControlMode_Quality.0 as u32,
    )
    .and_then(|()| set_codec_u32(codec, &CODECAPI_AVEncCommonQuality, QUALITY_LEVEL));
    let Err(quality_error) = quality else {
        return;
    };
    match set_codec_u32(
        codec,
        &CODECAPI_AVEncCommonRateControlMode,
        eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32,
    ) {
        Ok(()) => tracing::warn!(
            target: "task1000_rate_control",
            quality = %quality_error.diagnostics,
            "constant-quality rate control unavailable; aiming at an average bitrate instead"
        ),
        Err(vbr_error) => tracing::warn!(
            target: "task1000_rate_control",
            quality = %quality_error.diagnostics,
            vbr = %vbr_error.diagnostics,
            "no rate control mode could be set; leaving the encoder's own default"
        ),
    }
}

unsafe fn set_codec_bool(
    codec: &ICodecAPI,
    setting: &windows::core::GUID,
) -> Result<(), EncoderStartError> {
    codec
        .IsSupported(setting)
        .map_err(|error| device_error("required hardware video encoder CodecAPI setting", error))?;
    let mut variant = VARIANT::default();
    variant.Anonymous = VARIANT_0 {
        Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
            vt: VT_BOOL,
            wReserved1: 0,
            wReserved2: 0,
            wReserved3: 0,
            Anonymous: VARIANT_0_0_0 {
                boolVal: VARIANT_TRUE,
            },
        }),
    };
    codec
        .SetValue(setting, &variant)
        .map_err(|error| device_error("required hardware video encoder CodecAPI setting", error))
}

pub(super) unsafe fn set_codec_u32(
    codec: &ICodecAPI,
    setting: &windows::core::GUID,
    value: u32,
) -> Result<(), EncoderStartError> {
    codec
        .IsSupported(setting)
        .map_err(|error| device_error("required hardware video encoder CodecAPI setting", error))?;
    let mut variant = VARIANT::default();
    variant.Anonymous = VARIANT_0 {
        Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
            vt: VT_UI4,
            wReserved1: 0,
            wReserved2: 0,
            wReserved3: 0,
            Anonymous: VARIANT_0_0_0 { ulVal: value },
        }),
    };
    codec
        .SetValue(setting, &variant)
        .map_err(|error| device_error("hardware video encoder CodecAPI setting", error))
}

pub fn attach_d3d11_manager(
    transform: &IMFTransform,
    device: &ID3D11Device,
) -> Result<IMFDXGIDeviceManager, EncoderStartError> {
    unsafe {
        let mut token = 0;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)
            .map_err(|error| device_error("MFCreateDXGIDeviceManager", error))?;
        let manager = manager.ok_or_else(|| {
            EncoderStartError::new(
                EncoderStartErrorKind::Device,
                "no DXGI device manager returned",
            )
        })?;
        let device_unknown = device
            .cast::<IUnknown>()
            .map_err(|error| device_error("D3D device cast", error))?;
        manager
            .ResetDevice(&device_unknown, token)
            .map_err(|error| device_error("ResetDevice", error))?;
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: format!("MFT rejected D3D manager: {error}"),
            })?;
        Ok(manager)
    }
}
