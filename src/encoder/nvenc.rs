//! NVENC through NVIDIA's own encode API rather than its Media Foundation
//! transform (t261024-071a).
//!
//! The MFT takes one frame at a time (t261021-e402: in-flight 1 for AV1 and
//! H.264, with or without low latency), so its throughput is 1 / latency. Under
//! a game's GPU load the latency stretches to 20-50 ms and the MFT tops out
//! near 43 frames a second. This session keeps [`DEPTH`] frames in flight
//! instead, so a 23 ms latency still clears 120 a second.
//!
//! The library is loaded at run time (`nvEncodeAPI64.dll`, installed with the
//! driver). Anything that fails on the way to an open session -- no DLL, an
//! older API than the bindings, a device the driver will not open, a codec or
//! input format it does not list -- is an `Err`, and the caller falls back to
//! the MFT.

use std::ffi::c_void;

use windows::{
    core::{Interface, PCSTR, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, FreeLibrary, HANDLE, HMODULE, WAIT_OBJECT_0},
        Graphics::{
            Direct3D11::{
                ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
                D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE,
                D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
            },
            Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC},
        },
        System::{
            LibraryLoader::{GetProcAddress, LoadLibraryW},
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
};

#[rustfmt::skip]
mod sys;

/// Frames in flight: input textures and output bitstreams, one of each per slot.
pub(crate) const DEPTH: usize = 4;

/// Which bitstream the session produces. Mirrors the codecs recording uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NvencCodec {
    H264,
    Av1,
}

impl NvencCodec {
    fn guid(self) -> sys::GUID {
        match self {
            Self::H264 => sys::NV_ENC_CODEC_H264_GUID,
            Self::Av1 => sys::NV_ENC_CODEC_AV1_GUID,
        }
    }

    fn profile(self) -> sys::GUID {
        match self {
            Self::H264 => sys::NV_ENC_H264_PROFILE_HIGH_GUID,
            Self::Av1 => sys::NV_ENC_AV1_PROFILE_MAIN_GUID,
        }
    }
}

/// What the session is opened with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NvencParams {
    pub codec: NvencCodec,
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
    /// `NV_ENC_RC_PARAMS::targetQuality` under VBR (0 = let the driver pick).
    pub target_quality: u8,
    /// What the slot textures hold. Recording copies BGRA frames in; the
    /// export's boundary re-encode uploads decoded NV12 with [`NvencSession::fill_next_nv12`]
    /// (t261026-3ff2). The SPS does not depend on it.
    pub nv12_input: bool,
}

/// One encoded frame, in output order.
pub(crate) struct NvencPacket {
    pub data: Vec<u8>,
    pub pts_100ns: i64,
    pub duration_100ns: i64,
    pub keyframe: bool,
}

impl NvencPacket {
    /// What the muxer takes: the bitstream in a memory buffer, its time and
    /// length, and `CleanPoint` on IDR frames (what the MFT's samples carry).
    pub(crate) fn to_sample(
        &self,
    ) -> Result<windows::Win32::Media::MediaFoundation::IMFSample, String> {
        use windows::Win32::Media::MediaFoundation::*;
        fn map(what: &'static str) -> impl Fn(windows::core::Error) -> String {
            move |error| format!("{what}: {error}")
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(self.data.len().max(1) as u32)
                .map_err(map("MFCreateMemoryBuffer"))?;
            let mut target = std::ptr::null_mut();
            buffer
                .Lock(&mut target, None, None)
                .map_err(map("buffer Lock"))?;
            std::ptr::copy_nonoverlapping(self.data.as_ptr(), target, self.data.len());
            buffer.Unlock().map_err(map("buffer Unlock"))?;
            buffer
                .SetCurrentLength(self.data.len() as u32)
                .map_err(map("SetCurrentLength"))?;
            let sample = MFCreateSample().map_err(map("MFCreateSample"))?;
            sample.AddBuffer(&buffer).map_err(map("AddBuffer"))?;
            sample
                .SetSampleTime(self.pts_100ns)
                .map_err(map("SetSampleTime"))?;
            sample
                .SetSampleDuration(self.duration_100ns)
                .map_err(map("SetSampleDuration"))?;
            if self.keyframe {
                sample
                    .SetUINT32(&MFSampleExtension_CleanPoint, 1)
                    .map_err(map("CleanPoint"))?;
            }
            Ok(sample)
        }
    }
}

fn same_guid(a: &sys::GUID, b: &sys::GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

fn check(status: sys::NVENCSTATUS, what: &str) -> Result<(), String> {
    if status == sys::_NVENCSTATUS_NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what} returned NVENCSTATUS {status}"))
    }
}

struct Slot {
    texture: ID3D11Texture2D,
    registered: sys::NV_ENC_REGISTERED_PTR,
    bitstream: sys::NV_ENC_OUTPUT_PTR,
    event: HANDLE,
    /// The mapped input while the slot is in flight.
    mapped: sys::NV_ENC_INPUT_PTR,
}

pub(crate) struct NvencSession {
    library: HMODULE,
    api: sys::NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    params: NvencParams,
    slots: Vec<Slot>,
    /// Slot indices in submit order; the front is the oldest in flight.
    in_flight: std::collections::VecDeque<usize>,
    free: Vec<usize>,
    /// The highest version the installed driver reports, `major << 4 | minor`.
    driver_version: u32,
    /// NV12 input only: the CPU-writable texture a frame is uploaded through,
    /// and the context that copies it into a slot.
    staging: Option<(ID3D11DeviceContext, ID3D11Texture2D)>,
}

// The raw pointers belong to the encode session, which the capture worker owns
// and drives from one thread at a time.
unsafe impl Send for NvencSession {}

impl NvencSession {
    /// Loads the library, opens a session on `device` and allocates [`DEPTH`]
    /// slots. Every failure is an `Err` the caller treats as "use the MFT".
    pub(crate) fn open(device: &ID3D11Device, params: NvencParams) -> Result<Self, String> {
        unsafe {
            let library = LoadLibraryW(PCWSTR::from_raw(
                windows::core::w!("nvEncodeAPI64.dll").as_ptr(),
            ))
            .map_err(|error| format!("LoadLibrary nvEncodeAPI64.dll: {error}"))?;
            let mut session = Self {
                library,
                api: std::mem::zeroed(),
                encoder: std::ptr::null_mut(),
                params,
                slots: Vec::new(),
                in_flight: std::collections::VecDeque::new(),
                free: Vec::new(),
                driver_version: 0,
                staging: None,
            };
            // From here `Drop` cleans up whatever got made.
            session.start(device)?;
            Ok(session)
        }
    }

    unsafe fn start(&mut self, device: &ID3D11Device) -> Result<(), String> {
        let max_version = GetProcAddress(
            self.library,
            PCSTR(c"NvEncodeAPIGetMaxSupportedVersion".as_ptr().cast()),
        )
        .ok_or("nvEncodeAPI64.dll has no NvEncodeAPIGetMaxSupportedVersion")?;
        let max_version: sys::GetMaxSupportedVersion = std::mem::transmute(max_version);
        check(
            max_version(&mut self.driver_version),
            "NvEncodeAPIGetMaxSupportedVersion",
        )?;
        let wanted = (sys::NVENCAPI_MAJOR_VERSION << 4) | sys::NVENCAPI_MINOR_VERSION;
        if self.driver_version < wanted {
            return Err(format!(
                "driver NVENC API {}.{} is older than the bindings' {}.{}",
                self.driver_version >> 4,
                self.driver_version & 0xf,
                sys::NVENCAPI_MAJOR_VERSION,
                sys::NVENCAPI_MINOR_VERSION
            ));
        }
        let create = GetProcAddress(
            self.library,
            PCSTR(c"NvEncodeAPICreateInstance".as_ptr().cast()),
        )
        .ok_or("nvEncodeAPI64.dll has no NvEncodeAPICreateInstance")?;
        let create: sys::CreateInstance = std::mem::transmute(create);
        self.api.version = sys::NV_ENCODE_API_FUNCTION_LIST_VER;
        check(create(&mut self.api), "NvEncodeAPICreateInstance")?;

        let mut open: sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
        open.version = sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        open.deviceType = sys::_NV_ENC_DEVICE_TYPE_NV_ENC_DEVICE_TYPE_DIRECTX;
        open.device = device.as_raw();
        open.apiVersion = sys::NVENCAPI_VERSION;
        check(
            (self
                .api
                .nvEncOpenEncodeSessionEx
                .ok_or("no nvEncOpenEncodeSessionEx")?)(&mut open, &mut self.encoder),
            "nvEncOpenEncodeSessionEx",
        )?;

        self.require_codec_and_input()?;
        self.initialize()?;
        for index in 0..DEPTH {
            let slot = self.make_slot(device)?;
            self.slots.push(slot);
            self.free.push(DEPTH - 1 - index);
        }
        if self.params.nv12_input {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.params.width,
                Height: self.params.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: 0,
            };
            let mut texture = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|error| format!("NVENC NV12 staging texture: {error}"))?;
            let context = device
                .GetImmediateContext()
                .map_err(|error| format!("NVENC immediate context: {error}"))?;
            self.staging = Some((context, texture.ok_or("NVENC NV12 staging texture: none")?));
        }
        Ok(())
    }

    unsafe fn require_codec_and_input(&self) -> Result<(), String> {
        let codec = self.params.codec.guid();
        let mut count = 0;
        check(
            (self
                .api
                .nvEncGetEncodeGUIDCount
                .ok_or("no nvEncGetEncodeGUIDCount")?)(self.encoder, &mut count),
            "nvEncGetEncodeGUIDCount",
        )?;
        let mut guids = vec![std::mem::zeroed::<sys::GUID>(); count as usize];
        let mut got = 0;
        check(
            (self
                .api
                .nvEncGetEncodeGUIDs
                .ok_or("no nvEncGetEncodeGUIDs")?)(
                self.encoder,
                guids.as_mut_ptr(),
                count,
                &mut got,
            ),
            "nvEncGetEncodeGUIDs",
        )?;
        if !guids[..got as usize].iter().any(|g| same_guid(g, &codec)) {
            return Err(format!("NVENC does not list {:?}", self.params.codec));
        }
        let mut count = 0;
        check(
            (self
                .api
                .nvEncGetInputFormatCount
                .ok_or("no nvEncGetInputFormatCount")?)(self.encoder, codec, &mut count),
            "nvEncGetInputFormatCount",
        )?;
        let mut formats = vec![0 as sys::NV_ENC_BUFFER_FORMAT; count as usize];
        let mut got = 0;
        check(
            (self
                .api
                .nvEncGetInputFormats
                .ok_or("no nvEncGetInputFormats")?)(
                self.encoder,
                codec,
                formats.as_mut_ptr(),
                count,
                &mut got,
            ),
            "nvEncGetInputFormats",
        )?;
        if !formats[..got as usize].contains(&self.buffer_format()) {
            return Err(format!(
                "NVENC does not take {} input for {:?}",
                if self.params.nv12_input {
                    "NV12"
                } else {
                    "ARGB"
                },
                self.params.codec
            ));
        }
        Ok(())
    }

    fn buffer_format(&self) -> sys::NV_ENC_BUFFER_FORMAT {
        if self.params.nv12_input {
            sys::_NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_NV12
        } else {
            sys::_NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_ARGB
        }
    }

    /// The recording conditions `mft::configure_codec_api` sets on the MFT, in
    /// NVENC's terms: no B frames (the muxer writes no `ctts`, task1000 /
    /// task1780), GOP = frame_rate / 2 (task2620), parameter sets on every IDR
    /// so each segment plays alone, studio-range BT.709 (task033).
    unsafe fn initialize(&mut self) -> Result<(), String> {
        let p = self.params;
        let codec = p.codec.guid();
        let preset = sys::NV_ENC_PRESET_P4_GUID;
        let tuning = sys::NV_ENC_TUNING_INFO_NV_ENC_TUNING_INFO_LOW_LATENCY;
        let mut preset_config: sys::NV_ENC_PRESET_CONFIG = std::mem::zeroed();
        preset_config.version = sys::NV_ENC_PRESET_CONFIG_VER;
        preset_config.presetCfg.version = sys::NV_ENC_CONFIG_VER;
        check(
            (self
                .api
                .nvEncGetEncodePresetConfigEx
                .ok_or("no nvEncGetEncodePresetConfigEx")?)(
                self.encoder,
                codec,
                preset,
                tuning,
                &mut preset_config,
            ),
            "nvEncGetEncodePresetConfigEx",
        )?;
        let mut config = preset_config.presetCfg;
        config.version = sys::NV_ENC_CONFIG_VER;
        config.profileGUID = p.codec.profile();
        let gop = (p.frame_rate / 2).max(1);
        config.gopLength = gop;
        config.frameIntervalP = 1;
        config.rcParams.rateControlMode = sys::_NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_VBR;
        config.rcParams.averageBitRate = 0;
        // The driver's default ceiling under VBR held AV1 at 30.8 Mbps whatever the
        // target quality (t261024-071a step 4), while the MFT's heavy game
        // segments run 112-140 Mbps (28-35 MB per 2 s). Above that, so quality
        // decides, as it does for the MFT.
        config.rcParams.maxBitRate = 240_000_000;
        config.rcParams.targetQuality = p.target_quality;
        match p.codec {
            NvencCodec::H264 => {
                let h264 = &mut config.encodeCodecConfig.h264Config;
                h264.idrPeriod = gop;
                h264.set_repeatSPSPPS(1);
                let vui = &mut h264.h264VUIParameters;
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFormat = sys::_NV_ENC_VUI_VIDEO_FORMAT_NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                vui.videoFullRangeFlag = 0;
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries =
                    sys::_NV_ENC_VUI_COLOR_PRIMARIES_NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                vui.transferCharacteristics = sys::_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                vui.colourMatrix = sys::_NV_ENC_VUI_MATRIX_COEFFS_NV_ENC_VUI_MATRIX_COEFFS_BT709;
            }
            NvencCodec::Av1 => {
                let av1 = &mut config.encodeCodecConfig.av1Config;
                av1.idrPeriod = gop;
                av1.set_repeatSeqHdr(1);
                av1.colorPrimaries =
                    sys::_NV_ENC_VUI_COLOR_PRIMARIES_NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                av1.transferCharacteristics = sys::_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                av1.matrixCoefficients =
                    sys::_NV_ENC_VUI_MATRIX_COEFFS_NV_ENC_VUI_MATRIX_COEFFS_BT709;
                av1.colorRange = 0;
            }
        }
        let mut init: sys::NV_ENC_INITIALIZE_PARAMS = std::mem::zeroed();
        init.version = sys::NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = codec;
        init.presetGUID = preset;
        init.tuningInfo = tuning;
        init.encodeWidth = p.width;
        init.encodeHeight = p.height;
        init.darWidth = p.width;
        init.darHeight = p.height;
        init.maxEncodeWidth = p.width;
        init.maxEncodeHeight = p.height;
        init.frameRateNum = p.frame_rate;
        init.frameRateDen = 1;
        init.enableEncodeAsync = 1;
        init.enablePTD = 1;
        init.encodeConfig = &mut config;
        check(
            (self
                .api
                .nvEncInitializeEncoder
                .ok_or("no nvEncInitializeEncoder")?)(self.encoder, &mut init),
            "nvEncInitializeEncoder",
        )
    }

    unsafe fn make_slot(&self, device: &ID3D11Device) -> Result<Slot, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.params.width,
            Height: self.params.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: if self.params.nv12_input {
                DXGI_FORMAT_NV12
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: if self.params.nv12_input {
                D3D11_BIND_RENDER_TARGET.0 as u32
            } else {
                (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32
            },
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| format!("NVENC input texture: {error}"))?;
        let texture = texture.ok_or("NVENC input texture: none")?;

        let mut register: sys::NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
        register.version = sys::NV_ENC_REGISTER_RESOURCE_VER;
        register.resourceType = sys::_NV_ENC_INPUT_RESOURCE_TYPE_NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        register.width = self.params.width;
        register.height = self.params.height;
        register.resourceToRegister = texture.as_raw();
        register.bufferFormat = self.buffer_format();
        register.bufferUsage = sys::_NV_ENC_BUFFER_USAGE_NV_ENC_INPUT_IMAGE;
        check(
            (self
                .api
                .nvEncRegisterResource
                .ok_or("no nvEncRegisterResource")?)(self.encoder, &mut register),
            "nvEncRegisterResource",
        )?;

        let mut bitstream: sys::NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
        bitstream.version = sys::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        let status =
            (self
                .api
                .nvEncCreateBitstreamBuffer
                .ok_or("no nvEncCreateBitstreamBuffer")?)(self.encoder, &mut bitstream);
        let mut slot = Slot {
            texture,
            registered: register.registeredResource,
            bitstream: std::ptr::null_mut(),
            event: HANDLE::default(),
            mapped: std::ptr::null_mut(),
        };
        if let Err(error) = check(status, "nvEncCreateBitstreamBuffer") {
            self.release_slot(&mut slot);
            return Err(error);
        }
        slot.bitstream = bitstream.bitstreamBuffer;
        match CreateEventW(None, false, false, None) {
            Ok(event) => slot.event = event,
            Err(error) => {
                self.release_slot(&mut slot);
                return Err(format!("CreateEvent: {error}"));
            }
        }
        let mut event: sys::NV_ENC_EVENT_PARAMS = std::mem::zeroed();
        event.version = sys::NV_ENC_EVENT_PARAMS_VER;
        event.completionEvent = slot.event.0;
        if let Err(error) = check(
            (self
                .api
                .nvEncRegisterAsyncEvent
                .ok_or("no nvEncRegisterAsyncEvent")?)(self.encoder, &mut event),
            "nvEncRegisterAsyncEvent",
        ) {
            let _ = CloseHandle(slot.event);
            slot.event = HANDLE::default();
            self.release_slot(&mut slot);
            return Err(error);
        }
        Ok(slot)
    }

    /// Undoes `make_slot` in reverse. Each step tolerates the ones before it
    /// never having happened.
    unsafe fn release_slot(&self, slot: &mut Slot) {
        if !slot.mapped.is_null() {
            if let Some(unmap) = self.api.nvEncUnmapInputResource {
                unmap(self.encoder, slot.mapped);
            }
            slot.mapped = std::ptr::null_mut();
        }
        if !slot.event.is_invalid() && !slot.event.0.is_null() {
            if let Some(unregister) = self.api.nvEncUnregisterAsyncEvent {
                let mut event: sys::NV_ENC_EVENT_PARAMS = std::mem::zeroed();
                event.version = sys::NV_ENC_EVENT_PARAMS_VER;
                event.completionEvent = slot.event.0;
                unregister(self.encoder, &mut event);
            }
            let _ = CloseHandle(slot.event);
            slot.event = HANDLE::default();
        }
        if !slot.bitstream.is_null() {
            if let Some(destroy) = self.api.nvEncDestroyBitstreamBuffer {
                destroy(self.encoder, slot.bitstream);
            }
            slot.bitstream = std::ptr::null_mut();
        }
        if !slot.registered.is_null() {
            if let Some(unregister) = self.api.nvEncUnregisterResource {
                unregister(self.encoder, slot.registered);
            }
            slot.registered = std::ptr::null_mut();
        }
    }

    /// The stream's parameter sets as the encoder emits them in band: H.264 SPS +
    /// PPS in Annex B, AV1 the sequence header OBU. The same bytes the MFT puts in
    /// `MF_MT_MPEG_SEQUENCE_HEADER`.
    pub(crate) fn sequence_header(&self) -> Result<Vec<u8>, String> {
        let mut buffer = vec![0u8; 1024];
        let mut size = 0u32;
        unsafe {
            let mut payload: sys::NV_ENC_SEQUENCE_PARAM_PAYLOAD = std::mem::zeroed();
            payload.version = sys::NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
            payload.inBufferSize = buffer.len() as u32;
            payload.spsppsBuffer = buffer.as_mut_ptr().cast();
            payload.outSPSPPSPayloadSize = &mut size;
            check(
                (self
                    .api
                    .nvEncGetSequenceParams
                    .ok_or("no nvEncGetSequenceParams")?)(
                    self.encoder, &mut payload
                ),
                "nvEncGetSequenceParams",
            )?;
        }
        buffer.truncate(size as usize);
        Ok(buffer)
    }

    /// The type the MP4 sink is created with: the attributes the MFT's
    /// `GetOutputCurrentType` carries for the same stream (dumped 2026-10-25,
    /// t261024-071a step 1.3), with the bitrate left out -- VBR on target
    /// quality has no fixed one.
    pub(crate) fn output_media_type(
        &self,
    ) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, String> {
        use windows::Win32::Media::MediaFoundation::*;
        let p = self.params;
        let header = self.sequence_header()?;
        let set = |what: &str, result: windows::core::Result<()>| {
            result.map_err(|error| format!("output type {what}: {error}"))
        };
        unsafe {
            let t = MFCreateMediaType().map_err(|error| format!("MFCreateMediaType: {error}"))?;
            set("major", t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video))?;
            set(
                "subtype",
                t.SetGUID(
                    &MF_MT_SUBTYPE,
                    match p.codec {
                        NvencCodec::H264 => &MFVideoFormat_H264,
                        NvencCodec::Av1 => &MFVideoFormat_AV1,
                    },
                ),
            )?;
            set(
                "frame size",
                t.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(p.width) << 32) | u64::from(p.height),
                ),
            )?;
            set(
                "frame rate",
                t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(p.frame_rate) << 32) | 1),
            )?;
            set(
                "aspect",
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1),
            )?;
            set(
                "interlace",
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32),
            )?;
            set(
                "profile",
                t.SetUINT32(
                    &MF_MT_MPEG2_PROFILE,
                    match p.codec {
                        NvencCodec::H264 => super::hardware::H264_PROFILE_HIGH,
                        NvencCodec::Av1 => 1,
                    },
                ),
            )?;
            set(
                "range",
                t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32),
            )?;
            set(
                "primaries",
                t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32),
            )?;
            set(
                "transfer",
                t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32),
            )?;
            set(
                "matrix",
                t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32),
            )?;
            set(
                "sequence header",
                t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &header),
            )?;
            set("level", t.SetUINT32(&MF_MT_MPEG2_LEVEL, u32::MAX))?;
            set("fixed size", t.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 0))?;
            set(
                "independent",
                t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1),
            )?;
            set("bitrate", t.SetUINT32(&MF_MT_AVG_BITRATE, 12_000_000))?;
            Ok(t)
        }
    }

    /// The highest API version the installed driver reports, as `major.minor`.
    pub(crate) fn driver_version(&self) -> (u32, u32) {
        (self.driver_version >> 4, self.driver_version & 0xf)
    }

    /// How many slots are free to take a frame now.
    pub(crate) fn free_slots(&self) -> usize {
        self.free.len()
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// The texture of the slot the next [`Self::submit`] will encode, if one is
    /// free. The caller copies its BGRA frame into it first.
    pub(crate) fn next_texture(&self) -> Option<&ID3D11Texture2D> {
        self.free.last().map(|&index| &self.slots[index].texture)
    }

    /// NV12 input: copies one tightly packed NV12 frame (`width` bytes a row,
    /// `height` luma rows then `height / 2` chroma rows) into the next free
    /// slot's texture and returns it for [`Self::submit`].
    pub(crate) fn fill_next_nv12(&self, frame: &[u8]) -> Result<ID3D11Texture2D, String> {
        let (context, staging) = self
            .staging
            .as_ref()
            .ok_or("NVENC session was not opened for NV12 input")?;
        let texture = self
            .next_texture()
            .ok_or("NVENC NV12 fill with no free slot")?;
        let width = self.params.width as usize;
        let rows = self.params.height as usize * 3 / 2;
        if frame.len() < width * rows {
            return Err(format!(
                "NV12 frame is {} bytes, {width}x{rows} needs {}",
                frame.len(),
                width * rows
            ));
        }
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .map_err(|error| format!("NVENC NV12 staging Map: {error}"))?;
            // The chroma plane starts `height` rows of `RowPitch` in.
            let pitch = mapped.RowPitch as usize;
            for row in 0..rows {
                std::ptr::copy_nonoverlapping(
                    frame.as_ptr().add(row * width),
                    mapped.pData.cast::<u8>().add(row * pitch),
                    width,
                );
            }
            context.Unmap(staging, 0);
            context.CopyResource(texture, staging);
        }
        Ok(texture.clone())
    }

    /// Encodes the frame the caller copied into `texture` -- one of the free
    /// slots' textures, as [`Self::next_texture`] handed it out -- without
    /// waiting. Looked up by identity rather than taken from the top of the free
    /// list, so a `poll` between handing the texture out and submitting it
    /// cannot make this encode a different slot.
    pub(crate) fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        pts_100ns: i64,
        force_idr: bool,
    ) -> Result<(), String> {
        let position = self
            .free
            .iter()
            .position(|&index| self.slots[index].texture.as_raw() == texture.as_raw())
            .ok_or("NVENC submit of a texture that is not a free slot")?;
        let index = self.free[position];
        unsafe {
            let slot = &mut self.slots[index];
            let mut map: sys::NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
            map.version = sys::NV_ENC_MAP_INPUT_RESOURCE_VER;
            map.registeredResource = slot.registered;
            check(
                (self
                    .api
                    .nvEncMapInputResource
                    .ok_or("no nvEncMapInputResource")?)(self.encoder, &mut map),
                "nvEncMapInputResource",
            )?;
            slot.mapped = map.mappedResource;
            let duration = 10_000_000 / i64::from(self.params.frame_rate);
            let mut pic: sys::NV_ENC_PIC_PARAMS = std::mem::zeroed();
            pic.version = sys::NV_ENC_PIC_PARAMS_VER;
            pic.inputWidth = self.params.width;
            pic.inputHeight = self.params.height;
            pic.inputPitch = self.params.width;
            pic.inputBuffer = map.mappedResource;
            pic.bufferFmt = map.mappedBufferFmt;
            pic.outputBitstream = slot.bitstream;
            pic.completionEvent = slot.event.0;
            pic.pictureStruct = sys::_NV_ENC_PIC_STRUCT_NV_ENC_PIC_STRUCT_FRAME;
            pic.inputTimeStamp = pts_100ns as u64;
            pic.inputDuration = duration as u64;
            if force_idr {
                pic.encodePicFlags = (sys::_NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_FORCEIDR
                    | sys::_NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_OUTPUT_SPSPPS)
                    as u32;
            }
            let status = (self.api.nvEncEncodePicture.ok_or("no nvEncEncodePicture")?)(
                self.encoder,
                &mut pic,
            );
            // With no B frames and no lookahead every picture is output; NEED_MORE_INPUT would mean
            // reordering, which the muxer cannot carry.
            if let Err(error) = check(status, "nvEncEncodePicture") {
                if let Some(unmap) = self.api.nvEncUnmapInputResource {
                    unmap(self.encoder, slot.mapped);
                }
                slot.mapped = std::ptr::null_mut();
                return Err(error);
            }
        }
        self.free.remove(position);
        self.in_flight.push_back(index);
        Ok(())
    }

    /// Collects every finished frame, oldest first, without waiting. Stops at
    /// the first one still encoding, so output order is submit order.
    pub(crate) fn poll(&mut self) -> Result<Vec<NvencPacket>, String> {
        let mut packets = Vec::new();
        while let Some(&index) = self.in_flight.front() {
            unsafe {
                if WaitForSingleObject(self.slots[index].event, 0) != WAIT_OBJECT_0 {
                    break;
                }
                packets.push(self.take(index)?);
            }
            self.in_flight.pop_front();
            self.free.push(index);
        }
        Ok(packets)
    }

    unsafe fn take(&mut self, index: usize) -> Result<NvencPacket, String> {
        let slot = &mut self.slots[index];
        let mut lock: sys::NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
        lock.version = sys::NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = slot.bitstream;
        check(
            (self.api.nvEncLockBitstream.ok_or("no nvEncLockBitstream")?)(self.encoder, &mut lock),
            "nvEncLockBitstream",
        )?;
        let data = std::slice::from_raw_parts(
            lock.bitstreamBufferPtr as *const u8,
            lock.bitstreamSizeInBytes as usize,
        )
        .to_vec();
        let packet = NvencPacket {
            data,
            pts_100ns: lock.outputTimeStamp as i64,
            duration_100ns: lock.outputDuration as i64,
            keyframe: lock.pictureType == sys::_NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_IDR,
        };
        (self
            .api
            .nvEncUnlockBitstream
            .ok_or("no nvEncUnlockBitstream")?)(self.encoder, slot.bitstream);
        (self
            .api
            .nvEncUnmapInputResource
            .ok_or("no nvEncUnmapInputResource")?)(self.encoder, slot.mapped);
        slot.mapped = std::ptr::null_mut();
        Ok(packet)
    }
}

impl Drop for NvencSession {
    fn drop(&mut self) {
        unsafe {
            if !self.encoder.is_null() {
                // Frames still encoding: wait for them, briefly, so the
                // bitstreams are not destroyed under the encoder.
                for &index in &self.in_flight {
                    let _ = WaitForSingleObject(self.slots[index].event, 200);
                }
                let mut slots = std::mem::take(&mut self.slots);
                for slot in &mut slots {
                    self.release_slot(slot);
                }
                if let Some(destroy) = self.api.nvEncDestroyEncoder {
                    destroy(self.encoder);
                }
                self.encoder = std::ptr::null_mut();
            }
            let _ = FreeLibrary(self.library);
        }
    }
}
