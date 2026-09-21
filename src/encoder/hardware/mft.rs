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
            MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
            MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, MFT_MESSAGE_SET_D3D_MANAGER,
            MFT_REGISTER_TYPE_INFO, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
            MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
            MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
            MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_SA_D3D11_BINDFLAGS,
        },
        System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4},
    },
};

use windows::Win32::Media::MediaFoundation::IMFActivate;
use windows::Win32::System::Com::CoTaskMemFree;

use super::super::{
    bitrate_for, device_error, media_type_error, EncoderConfig, EncoderStartError,
    EncoderStartErrorKind,
};
use super::VideoCodec;

/// Frees an `MFTEnumEx` result: per its contract the caller must Release each
/// `IMFActivate` *and* `CoTaskMemFree` the array. Dropping each
/// `Option<IMFActivate>` in place does the Release; a bare `CoTaskMemFree`
/// alone leaked every activation object.
///
/// # Safety
/// `activations`/`count` must be exactly what `MFTEnumEx` returned, and no
/// element may have been moved out of the array.
pub(crate) unsafe fn release_mft_activates(activations: *mut Option<IMFActivate>, count: u32) {
    for index in 0..count as usize {
        std::ptr::drop_in_place(activations.add(index));
    }
    CoTaskMemFree(Some(activations.cast()));
}

/// Every encoder here takes NV12 in, whatever it produces.
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
/// The subtype is derived rather than passed: NV12 going in and `codec` coming
/// out is the only combination this encoder has, and a caller that could name
/// the two independently could name a mismatched pair -- which compiles, finds
/// nothing in the advertised list, and reports "MFT did not advertise subtype"
/// for what is really a configuration mistake (task1780).
pub(super) unsafe fn supported_video_type(
    transform: &IMFTransform,
    input: bool,
    config: &EncoderConfig,
    codec: VideoCodec,
) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, String> {
    let subtype = if input {
        MFVideoFormat_NV12
    } else {
        codec.subtype()
    };
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
        configure_video_type(&media_type, input, config, codec)
            .map_err(|error| error.diagnostics)?;
        return Ok(media_type);
    }
    Err(format!("MFT did not advertise subtype {subtype:?}"))
}

unsafe fn configure_video_type(
    media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    input: bool,
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
    media_type
        .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
        .map_err(media_type_error)?;
    media_type
        .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
        .map_err(media_type_error)?;
    media_type
        .SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
        .map_err(media_type_error)?;
    media_type
        .SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
        .map_err(media_type_error)?;
    if input {
        media_type
            .SetUINT32(&MF_MT_DEFAULT_STRIDE, config.output_size.width as u32)
            .map_err(media_type_error)?;
        media_type
            .SetUINT32(
                &MF_MT_SAMPLE_SIZE,
                (i64::from(config.output_size.width) * i64::from(config.output_size.height) * 3 / 2)
                    as u32,
            )
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

pub(super) unsafe fn create_nv12_allocator(
    manager: &IMFDXGIDeviceManager,
    input_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
) -> Result<IMFVideoSampleAllocatorEx, EncoderStartError> {
    let mut allocator = None;
    MFCreateVideoSampleAllocatorEx(
        &IMFVideoSampleAllocatorEx::IID,
        &mut allocator as *mut _ as *mut _,
    )
    .map_err(|error| device_error("MFCreateVideoSampleAllocatorEx", error))?;
    let allocator: IMFVideoSampleAllocatorEx = allocator.ok_or_else(|| EncoderStartError {
        kind: EncoderStartErrorKind::Device,
        diagnostics: "MFCreateVideoSampleAllocatorEx returned no allocator".into(),
    })?;
    allocator
        .SetDirectXManager(manager)
        .map_err(|error| device_error("allocator D3D manager", error))?;
    let mut attributes = None;
    MFCreateAttributes(&mut attributes, 2)
        .map_err(|error| device_error("allocator attributes", error))?;
    let attributes = attributes.ok_or_else(|| EncoderStartError {
        kind: EncoderStartErrorKind::Device,
        diagnostics: "allocator attribute creation returned nothing".into(),
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
        .map_err(|error| device_error("NV12 allocator initialization", error))?;
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
/// content (FFXIV gameplay played back at 1920x1080: carved stone, particles,
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
        MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::Device,
            diagnostics: format!("MFCreateDXGIDeviceManager failed: {error}"),
        })?;
        let manager = manager.ok_or_else(|| EncoderStartError {
            kind: EncoderStartErrorKind::Device,
            diagnostics: "no DXGI device manager returned".into(),
        })?;
        let device_unknown = device
            .cast::<IUnknown>()
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: format!("D3D device cast failed: {error}"),
            })?;
        manager
            .ResetDevice(&device_unknown, token)
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: format!("ResetDevice failed: {error}"),
            })?;
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: format!("MFT rejected D3D manager: {error}"),
            })?;
        Ok(manager)
    }
}
