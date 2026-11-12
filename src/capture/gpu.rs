use std::sync::atomic::{AtomicU64, Ordering};

use windows::{
    core::Interface,
    Win32::Graphics::{
        Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        Direct3D11::{
            ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
            ID3D11RenderTargetView, ID3D11Resource, ID3D11SamplerState, ID3D11ShaderResourceView,
            ID3D11Texture2D, ID3D11VertexShader, ID3D11VideoContext, ID3D11VideoContext1,
            ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
            D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
            D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ,
            D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
            D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_IMMUTABLE, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
            D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
            D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
            D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VIEWPORT, D3D11_VPIV_DIMENSION_TEXTURE2D,
            D3D11_VPOV_DIMENSION_TEXTURE2D,
        },
        Dxgi::Common::{
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
        },
        Dxgi::DXGI_ERROR_WAS_STILL_DRAWING,
    },
};

use super::CaptureSize;
use crate::gpu::{create_pixel_shader, create_vertex_shader, FULLSCREEN_VERTEX_SHADER};

// The non-HDR WGC frame pool is created with DirectXPixelFormat::B8G8R8A8UIntNormalized
// (worker.rs, CaptureSurface::open), whose contents are already sRGB-gamma-encoded.
// CreateShaderResourceView is created with a `None` desc, so it inherits that UNORM
// format and the GPU does not linearize on sample. The values this shader receives are
// therefore already gamma-encoded: do not re-apply a gamma curve here, or highlights
// blow out (double gamma).
const SDR_PIXEL_SHADER: &[u8] = b"
Texture2D sourceTexture : register(t0); SamplerState sourceSampler : register(s0);
float4 main(float4 position : SV_POSITION, float2 uv : TEXCOORD) : SV_TARGET {
  return float4(sourceTexture.Sample(sourceSampler, uv).rgb, 1.0);
}\0";
/// The size every segment thumbnail is written at.
///
/// 480x270, not the 160x90 this used to be (task244): the history's tile grid
/// gives each thumbnail ~220px at four columns and more on a wide window, so a
/// 160px source was being scaled *up* -- which is what "the thumbnails are
/// rough" was. Lives here rather than beside the JPEG encoder because since
/// task2820 the GPU is what resizes to it.
pub(in crate::capture) const THUMBNAIL_SIZE: CaptureSize = CaptureSize {
    width: 480,
    height: 270,
};

/// The segment thumbnail's downscale (task2820). 4x4 taps of a linear
/// sampler, so sixteen samples average sixty-four source texels: at the 8x
/// reduction a maximised 4K window takes to 480x270 that is the whole
/// footprint of one destination pixel.
///
/// A plain bilinear `Sample` would read 2x2 texels out of that 8x8 and alias
/// hard -- which is what the CPU `imageops::resize(Triangle)` this replaces
/// never did. At gentler ratios (a 1080p window) the taps reach slightly past
/// the footprint and blur a little instead; invisible at 480x270, and the
/// right way to be wrong.
const THUMBNAIL_BOX_PIXEL_SHADER: &[u8] = b"
Texture2D sourceTexture : register(t0); SamplerState sourceSampler : register(s0);
cbuffer BoxFilter : register(b0) { float2 tapStep; float2 boxPadding; };
float4 main(float4 position : SV_POSITION, float2 uv : TEXCOORD) : SV_TARGET {
  float3 accumulated = float3(0.0, 0.0, 0.0);
  [unroll] for (int y = -2; y < 2; ++y) {
    [unroll] for (int x = -2; x < 2; ++x) {
      accumulated += sourceTexture.Sample(sourceSampler, uv + (float2(x, y) + 0.5) * tapStep).rgb;
    }
  }
  return float4(accumulated / 16.0, 1.0);
}\0";

// Windows HDR composition uses scRGB: sRGB/Rec.709-primary linear values where 1.0 is
// the SDR reference white (80 nits). What WGC hands over for an HDR-composited desktop
// is therefore `linear_sRGB * W_c`, where `W_c` is the monitor's SDR content white level
// at the moment the capture session opened -- task1820 measured that the composition
// scale is *frozen at session start* and does not move when the brightness slider does
// (evidence: `.agents/tasks/evidence/1820-white-level-refresh/`).
//
// The exact inverse of that is one division. `whitePoint` is the same `W_c`, queried
// at capture start (`worker::setup::monitor_sdr_white_point`) and again only when the
// monitor's HDR state flips mid-recording (t260917-7faa; a slider move alone is not
// followed, per task1820), and uploaded through
// the constant buffer below, so `c / w` puts an SDR pixel back on the sRGB value it
// started as and the encode is lossless in the round trip.
//
// This used to be an extended-Reinhard tone curve, which is what a *display* mapping of
// genuine HDR content wants -- but the captured desktop is overwhelmingly SDR composited
// into HDR, and Reinhard lifts its midtones badly: at w=4.1 (this machine's 328-nit SDR
// white level) sRGB 120 came back as 178 and sRGB 36 as 75, and the whole recording read
// as washed out (task2750, the "white blowout" report). Task1790's acceptance only asked
// that bright steps stay *separated*, which the lifted curve satisfied, so the error
// survived that fix. Do not reintroduce a rolloff here without re-deriving it against
// the frozen composition scale.
//
// Above `w` the result is hard-clipped rather than rolled off (user decision, task2750):
// SDR sources stay exactly identity, and genuine HDR highlights saturate at 255. The
// gamma is the piecewise sRGB OETF, not `pow(x, 1/2.2)`, so the round trip is exact in
// the shadows too -- the two curves diverge by several codes below sRGB 20.
const HDR_PIXEL_SHADER: &[u8] = b"
Texture2D sourceTexture : register(t0); SamplerState sourceSampler : register(s0);
cbuffer ToneMap : register(b0) { float whitePoint; float3 padding; };
float3 srgbEncode(float3 x) {
  return x <= 0.0031308 ? 12.92 * x : 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}
float4 main(float4 position : SV_POSITION, float2 uv : TEXCOORD) : SV_TARGET {
  float3 c = max(sourceTexture.Sample(sourceSampler, uv).rgb, 0.0) / whitePoint;
  return float4(srgbEncode(saturate(c)), 1.0);
}\0";

/// scRGB linear white point to fall back to, and never to go below: 1.0 is the
/// SDR reference white (80 nits), and the extra 2% is Task 034's headroom, kept
/// across task1790 and task2750 so the fallback path's numbers stay comparable
/// with every measurement taken before them.
///
/// It is a floor, not a correction: a monitor that really does report the plain
/// 80-nit reference white gets divided by 1.02 rather than 1.0, which darkens
/// the encode by ~2% (sRGB 255 lands at 253). That is deliberate headroom and
/// well under the +-2 the acceptance allows, but it is the one input for which
/// this path is not bit-exact -- worth knowing before reading a swatch table.
pub(super) const HDR_WHITE_POINT_FLOOR: f32 = 1.02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FitRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub(crate) fn aspect_fit(input: CaptureSize, output: CaptureSize) -> FitRect {
    if input.width <= 0 || input.height <= 0 || output.width <= 0 || output.height <= 0 {
        return FitRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let input_ratio = input.width as f64 / input.height as f64;
    let output_ratio = output.width as f64 / output.height as f64;
    let (width, height) = if input_ratio > output_ratio {
        (
            output.width,
            (output.width as f64 / input_ratio).round() as i32,
        )
    } else {
        (
            (output.height as f64 * input_ratio).round() as i32,
            output.height,
        )
    };
    FitRect {
        x: (output.width - width) / 2,
        y: (output.height - height) / 2,
        width,
        height,
    }
}

/// The viewport `TransformPipeline::transform` draws the source into.
///
/// Normally `aspect_fit`. The exception is a source at most one pixel larger
/// than the output on each axis, which is exactly what an odd-sized window
/// leaves behind once `worker.rs` rounds the output down to even for NV12
/// (task980): fitting that resamples every pixel through the linear sampler for
/// the sake of a 1px difference and the whole frame goes soft. Drawing it 1:1
/// and letting the rasterizer clip the last row/column against the render
/// target keeps texel-to-pixel alignment exact -- a viewport is allowed to
/// exceed the render target, D3D11 clips it -- and costs one row of pixels the
/// fit path was squeezing in anyway.
pub(crate) fn crop_or_fit(input: CaptureSize, output: CaptureSize) -> FitRect {
    let croppable = |input: i32, output: i32| output > 0 && (0..=1).contains(&(input - output));
    if croppable(input.width, output.width) && croppable(input.height, output.height) {
        return FitRect {
            x: 0,
            y: 0,
            width: input.width,
            height: input.height,
        };
    }
    aspect_fit(input, output)
}

/// Rows/columns at each edge of a WGC frame that carry no picture: the part of
/// a maximized window Windows hangs past the monitor work area (task2770).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Inset {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The viewport that draws the whole `input` so that only its part inside
/// `inset` lands in the output -- placed exactly where `crop_or_fit` would put
/// a source of that visible size, then grown back out by the inset (which puts
/// the origin past the render target's edge; D3D11 clips it). A zero inset is
/// `crop_or_fit` unchanged.
pub(crate) fn source_viewport(input: CaptureSize, inset: Inset, output: CaptureSize) -> FitRect {
    let visible = CaptureSize {
        width: input.width - inset.left - inset.right,
        height: input.height - inset.top - inset.bottom,
    };
    if visible.width <= 0 || visible.height <= 0 {
        return crop_or_fit(input, output);
    }
    let fit = crop_or_fit(visible, output);
    let scale_x = fit.width as f64 / visible.width as f64;
    let scale_y = fit.height as f64 / visible.height as f64;
    FitRect {
        x: fit.x - (inset.left as f64 * scale_x).round() as i32,
        y: fit.y - (inset.top as f64 * scale_y).round() as i32,
        width: (input.width as f64 * scale_x).round() as i32,
        height: (input.height as f64 * scale_y).round() as i32,
    }
}

/// Decimates arrivals down to the recording's frame rate.
///
/// Phase accumulator, not a gap-from-last test. Accepting whenever the gap since
/// the last accepted frame reaches the interval sounds equivalent and is not: it
/// yields `arrival / ceil(interval / arrival_period)`, so it can only land on a
/// divisor of the arrival rate. Measured 2026-09-13 (t260913-578a): arrivals at
/// 139.93/s against a 120 target came out at **69.97/s** -- exactly half -- because
/// every second arrival fell 7.15ms after the last accepted one and the test wanted
/// 8.33ms. The bug was invisible while WGC's own `MinUpdateInterval` default pinned
/// arrival below every target (55.97/s); lifting that cap in `worker.rs` exposed it.
///
/// Holding a deadline instead spends the leftover phase on the next frame, so the
/// *average* lands on the target from any arrival rate: 139.93 -> 119.94,
/// 279.86 -> 119.94, and an arrival slower than the target passes through untouched.
pub struct FrameThrottle {
    /// The earliest timestamp the next frame may be accepted at. `None` until the
    /// first frame, which is always accepted.
    next_due_100ns: Option<i64>,
    interval_100ns: i64,
}

impl FrameThrottle {
    pub fn new(frame_rate: u8) -> Result<Self, &'static str> {
        // 120 joined the list in task164; `settings::FRAME_RATES` is the other
        // half of this pair and a test asserts the two agree.
        if !matches!(frame_rate, 30 | 60 | 120) {
            return Err("frame rate must be 30, 60 or 120");
        }
        Ok(Self {
            next_due_100ns: None,
            interval_100ns: 10_000_000 / i64::from(frame_rate),
        })
    }

    pub fn accepts(&mut self, timestamp_100ns: i64) -> bool {
        if self.next_due_100ns.is_some_and(|due| timestamp_100ns < due) {
            return false;
        }
        // `max(due, ts - interval)` is what keeps a stall from paying itself back as
        // a burst: a static window delivers nothing for seconds (WGC only composes
        // on change), and a deadline left far in the past would then wave through
        // every arrival until it caught up. At most one frame rides early.
        let base = self.next_due_100ns.map_or(timestamp_100ns, |due| {
            due.max(timestamp_100ns - self.interval_100ns)
        });
        self.next_due_100ns = Some(base + self.interval_100ns);
        true
    }
}

// Frame-delivery ground truth. Born as task029's temporary probe
// (task029-capture-video-frame-delivery-stall-silent-zero-fps: fps stuck at 0.0
// with silent per-frame `?` failures); **that root cause is resolved** -- task035
// found fps 0.0 to be Windows Graphics Capture behaving as specified (a window
// whose content does not change is never re-composited, so no frame arrives:
// measured arrived=1 over 26.2s static vs arrived=1241 over 22s moving), and the
// separate CAP-DEV-001 encoder failure to be odd client dimensions, since fixed.
//
// It stays anyway. Task035 explicitly chose to keep it as product code
// (`.agents/tasks/done/035-diagnose-capture-startup-h264-encoder-failures.md`,
// Summary) because it is the only thing that answers "are frames arriving at
// all?" without a rebuild, and it dropped the hardcoded user path so the counters
// go out through `tracing` alone. Task202's segment-boundary probe then built on
// it: `throttled` / `no_credit` are read as before/after deltas in
// `capture/worker.rs` (`target: "task202_boundary"`), so this is no longer a
// task029-only structure.
//
// Removable when frame arrival and drop causes are observable from somewhere
// else -- the insight counters would be the natural home. Until then, deleting it
// blinds both task029-class ("fps is 0") and task202-class ("frames missing
// across a segment boundary") diagnosis. Kept deliberately (task1650).
pub(super) struct FrameDebug {
    pub(super) arrived: AtomicU64,
    pub(super) throttled: AtomicU64,
    pub(super) accepted: AtomicU64,
    pub(super) err_try_get_frame: AtomicU64,
    pub(super) err_system_relative_time: AtomicU64,
    pub(super) err_surface: AtomicU64,
    pub(super) err_cast: AtomicU64,
    pub(super) err_get_interface: AtomicU64,
    pub(super) err_recreate: AtomicU64,
    pub(super) no_credit: AtomicU64,
    /// Frames the handler produced but could not hand downstream because
    /// `FRAME_QUEUE_CAPACITY` was already occupied (task1710). `try_send`
    /// returning `Full` was previously indistinguishable from success at the
    /// call site, which is what made "is arrival throttled by the consumer, or
    /// is the consumer merely keeping up?" unanswerable.
    pub(super) queue_full: AtomicU64,
}

impl FrameDebug {
    pub(super) fn new(hdr_input: bool, size: (i32, i32)) -> Self {
        let debug = Self {
            arrived: AtomicU64::new(0),
            throttled: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            err_try_get_frame: AtomicU64::new(0),
            err_system_relative_time: AtomicU64::new(0),
            err_surface: AtomicU64::new(0),
            err_cast: AtomicU64::new(0),
            err_get_interface: AtomicU64::new(0),
            err_recreate: AtomicU64::new(0),
            no_credit: AtomicU64::new(0),
            queue_full: AtomicU64::new(0),
        };
        debug.log(&format!(
            "session_start hdr_input={hdr_input} initial_size={}x{}",
            size.0, size.1
        ));
        debug
    }

    pub(super) fn log(&self, line: &str) {
        // Formerly `task029_frame_debug` (renamed task1700): the probe also feeds
        // task202's boundary diagnosis, so the name no longer points at one task.
        // Logs written before the rename still carry the old name.
        // INFO, not WARN: these are probe readings, not faults, and at WARN
        // they buried the real warnings (158 of the 10-15 log's 197).
        tracing::info!(event = "capture_frame_debug", "{line}");
    }

    fn summary(&self) -> String {
        format!(
            "arrived={} throttled={} accepted={} err_try_get_frame={} err_system_relative_time={} err_surface={} err_cast={} err_get_interface={} err_recreate={} no_credit={} queue_full={}",
            self.arrived.load(Ordering::Relaxed),
            self.throttled.load(Ordering::Relaxed),
            self.accepted.load(Ordering::Relaxed),
            self.err_try_get_frame.load(Ordering::Relaxed),
            self.err_system_relative_time.load(Ordering::Relaxed),
            self.err_surface.load(Ordering::Relaxed),
            self.err_cast.load(Ordering::Relaxed),
            self.err_get_interface.load(Ordering::Relaxed),
            self.err_recreate.load(Ordering::Relaxed),
            self.no_credit.load(Ordering::Relaxed),
            self.queue_full.load(Ordering::Relaxed),
        )
    }
}

impl Drop for FrameDebug {
    fn drop(&mut self) {
        let summary = self.summary();
        self.log(&format!("session_end {summary}"));
    }
}

pub(super) struct TransformPipeline {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex_shader: ID3D11VertexShader,
    sdr_shader: ID3D11PixelShader,
    hdr_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    /// The HDR shader's `ToneMap` cbuffer, holding the white point for this
    /// session's monitor. Immutable and never updated per frame: when the
    /// monitor's HDR state flips mid-recording, `set_hdr_white_point` replaces
    /// the whole buffer.
    tone_map_constants: ID3D11Buffer,
    pub(super) output: ID3D11Texture2D,
    output_view: ID3D11RenderTargetView,
    output_size: CaptureSize,
    /// The segment thumbnail's own pipeline (task2820): shader, render target,
    /// tap spacing and the one staging texture the readback maps.
    ///
    /// All fixed size and built once, because the thumbnail is always
    /// [`THUMBNAIL_SIZE`] however the window is resized -- which is the point.
    /// Before this, the readback copied the *full* output (a maximised 4K
    /// window is ~32MB) into a staging texture created fresh on every call,
    /// and the CPU then resized it down.
    thumbnail_shader: ID3D11PixelShader,
    thumbnail_constants: ID3D11Buffer,
    thumbnail_output: ID3D11Texture2D,
    thumbnail_view: ID3D11RenderTargetView,
    thumbnail_staging: ID3D11Texture2D,
}

pub(crate) struct GpuNv12Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    fence: ConvertFence,
}

/// How many conversions `ConvertFence` follows at once. One frame is in the
/// encoder at a time (t261021-e402), so a handful is plenty; past this the
/// oldest is given up unmeasured rather than the queue growing.
const CONVERT_FENCE_MAX: usize = 16;

/// When the GPU finished each frame's conversion (t261021-375d).
///
/// The encoder's submit-to-output time stretches to 20-50ms under a game's GPU
/// load while the encoder alone takes ~2ms. A fence signalled right after the
/// `VideoProcessorBlt` splits that time into "the conversion was still queued"
/// and "NVENC had it". A fence, not an event query: the query's result was
/// retired 1.5-2ms *after* the encoder had already returned the frame that
/// conversion fed (probe, `evidence/t261021-375d-encode-wait/fence-probe.rs`),
/// while the fence's value is written by the GPU as it passes and came first
/// on 119/119 frames. Read without waiting from the pump, so the precision is
/// the pump interval (~4ms at 120fps). Measurement only: a device without
/// fences, or a failed signal, turns it off for the rest of the recording.
struct ConvertFence {
    context: ID3D11DeviceContext,
    signal: Option<(
        windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext4,
        windows::Win32::Graphics::Direct3D11::ID3D11Fence,
    )>,
    next: u64,
    pending: std::collections::VecDeque<(u64, i64)>,
}

impl ConvertFence {
    fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Self {
        use windows::Win32::Graphics::Direct3D11::{
            ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, D3D11_FENCE_FLAG_NONE,
        };
        let signal = (|| {
            let device5: ID3D11Device5 = device.cast().ok()?;
            let context4: ID3D11DeviceContext4 = context.cast().ok()?;
            let mut fence: Option<ID3D11Fence> = None;
            unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) }.ok()?;
            Some((context4, fence?))
        })();
        Self {
            context: context.clone(),
            signal,
            next: 0,
            pending: std::collections::VecDeque::new(),
        }
    }

    fn mark(&mut self, pts_100ns: i64) {
        let Some((context4, fence)) = &self.signal else {
            return;
        };
        self.next += 1;
        if unsafe { context4.Signal(fence, self.next) }.is_err() {
            self.signal = None;
            self.pending.clear();
            return;
        }
        // `convert_into` already flushed the Blt; this sends the signal after
        // it, or it would wait for the next frame's flush.
        {
            // Shares the name with `convert_into`'s flush (t261025-cec6).
            crate::insight_scope!("gpu_flush");
            unsafe { self.context.Flush() };
        }
        if self.pending.len() == CONVERT_FENCE_MAX {
            self.pending.pop_front();
        }
        self.pending.push_back((self.next, pts_100ns));
    }

    /// Calls `done` with each frame whose conversion the GPU has finished, in
    /// submit order.
    fn collect(&mut self, mut done: impl FnMut(i64)) {
        let Some((_, fence)) = &self.signal else {
            return;
        };
        let completed = unsafe { fence.GetCompletedValue() };
        while let Some(&(value, pts_100ns)) = self.pending.front() {
            if value > completed {
                return;
            }
            self.pending.pop_front();
            done(pts_100ns);
        }
    }
}

impl GpuNv12Converter {
    pub(super) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        size: CaptureSize,
    ) -> windows::core::Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast()?;
            let video_context: ID3D11VideoContext = context.cast()?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: size.width as u32,
                InputHeight: size.height as u32,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: size.width as u32,
                OutputHeight: size.height as u32,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;
            // The capture target is Windows 11 22H2+ only (see README common
            // constraints), where ID3D11VideoContext1 (available since the Windows
            // 8.1 Platform Update) is always present, so there is no down-level
            // fallback path here: RGB input is full-range BT.709, YUV output must
            // be studio/limited-range BT.709 so the encoded stream doesn't rely on
            // player-side range guessing (see Task 033).
            let video_context1: ID3D11VideoContext1 = video_context.cast()?;
            video_context1.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            video_context1.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            Ok(Self {
                video_device,
                video_context,
                enumerator,
                processor,
                fence: ConvertFence::new(device, context),
            })
        }
    }

    /// Follows the conversion `convert_into` just queued for `pts_100ns` until
    /// the GPU finishes it (t261021-375d).
    pub(crate) fn mark_converted(&mut self, pts_100ns: i64) {
        self.fence.mark(pts_100ns);
    }

    /// Reports each followed conversion the GPU has finished, without waiting.
    pub(crate) fn collect_converted(&mut self, done: impl FnMut(i64)) {
        self.fence.collect(done);
    }

    /// The ARGB32 encoder input's stand-in for [`Self::convert_into`]
    /// (t261021-8388): the encoder converts to YUV itself, so the frame only
    /// has to land in its sample. `source` and `target` are the same size and
    /// both BGRA, which is what the encoder's ARGB32 allocator hands out.
    pub(crate) fn copy_into(
        &self,
        source: &ID3D11Texture2D,
        target: &ID3D11Texture2D,
    ) -> windows::core::Result<()> {
        crate::insight_scope!("gpu_copy_argb32");
        unsafe {
            let immediate: ID3D11DeviceContext = self.video_context.cast()?;
            immediate.CopyResource(target, source);
            immediate.Flush();
        }
        Ok(())
    }

    pub(crate) fn convert_into(
        &self,
        source: &ID3D11Texture2D,
        target: &ID3D11Texture2D,
    ) -> windows::core::Result<()> {
        crate::insight_scope!("gpu_convert_nv12");
        unsafe {
            // t261025-cec6: both views are rebuilt every frame; this scope
            // and the one in `transform` share the name `gpu_view_create`.
            let (input_view, output_view) = {
                crate::insight_scope!("gpu_view_create");
                let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                };
                let mut input_view = None;
                self.video_device.CreateVideoProcessorInputView(
                    &source.cast::<ID3D11Resource>()?,
                    &self.enumerator,
                    &input_desc,
                    Some(&mut input_view),
                )?;
                let input_view = input_view.ok_or_else(windows::core::Error::from_win32)?;
                let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut output_view = None;
                self.video_device.CreateVideoProcessorOutputView(
                    &target.cast::<ID3D11Resource>()?,
                    &self.enumerator,
                    &output_view_desc,
                    Some(&mut output_view),
                )?;
                let output_view = output_view.ok_or_else(windows::core::Error::from_win32)?;
                (input_view, output_view)
            };
            self.video_context.VideoProcessorSetStreamFrameFormat(
                &self.processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
                ..Default::default()
            };
            let blt = self.video_context.VideoProcessorBlt(
                &self.processor,
                &output_view,
                0,
                std::slice::from_mut(&mut stream),
            );
            // `pInputSurface` is `ManuallyDrop`, so the input view is never
            // released unless it is dropped here -- on the error path too.
            // Leaked, it was one view per frame, each holding a reference to
            // the capture device, so no recording's D3D11 device (and the
            // textures and pools on it) was ever destroyed: +21.3 MB of
            // dedicated GPU memory per stopped recording (task4280, measured
            // 6651 device references after a 6647-frame recording).
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            blt?;
            let immediate: ID3D11DeviceContext = self.video_context.cast()?;
            crate::insight_scope!("gpu_flush");
            immediate.Flush();
            Ok(())
        }
    }
}

impl TransformPipeline {
    /// `hdr_white_point` is the scRGB linear value the HDR tone curve maps to
    /// output white -- the capture monitor's SDR content white level divided by
    /// 80 nits (see `HDR_WHITE_POINT_FLOOR`). Ignored for SDR sources.
    pub(super) fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        output_size: CaptureSize,
        hdr_white_point: f32,
    ) -> windows::core::Result<Self> {
        unsafe {
            let vertex_shader =
                create_vertex_shader(&device, FULLSCREEN_VERTEX_SHADER).map_err(|error| {
                    windows::core::Error::new(error.code(), "fullscreen vertex shader setup failed")
                })?;
            let sdr_shader = create_pixel_shader(&device, SDR_PIXEL_SHADER).map_err(|error| {
                windows::core::Error::new(error.code(), "SDR pixel shader setup failed")
            })?;
            let hdr_shader = create_pixel_shader(&device, HDR_PIXEL_SHADER).map_err(|error| {
                windows::core::Error::new(error.code(), "HDR pixel shader setup failed")
            })?;
            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(|error| {
                    windows::core::Error::new(error.code(), "sampler creation failed")
                })?;
            let tone_map_constants =
                create_tone_map_buffer(&device, hdr_white_point).map_err(|error| {
                    windows::core::Error::new(error.code(), "tone map constant buffer failed")
                })?;
            let (output, output_view) =
                create_output_texture(&device, output_size).map_err(|error| {
                    windows::core::Error::new(error.code(), "BGRA8 output texture creation failed")
                })?;
            let thumbnail_shader = create_pixel_shader(&device, THUMBNAIL_BOX_PIXEL_SHADER)
                .map_err(|error| {
                    windows::core::Error::new(error.code(), "thumbnail pixel shader setup failed")
                })?;
            let thumbnail_constants =
                create_box_filter_buffer(&device, THUMBNAIL_SIZE).map_err(|error| {
                    windows::core::Error::new(error.code(), "box filter constant buffer failed")
                })?;
            let (thumbnail_output, thumbnail_view) = create_output_texture(&device, THUMBNAIL_SIZE)
                .map_err(|error| {
                    windows::core::Error::new(error.code(), "thumbnail texture creation failed")
                })?;
            let thumbnail_staging =
                create_staging_texture(&device, &thumbnail_output).map_err(|error| {
                    windows::core::Error::new(error.code(), "thumbnail staging creation failed")
                })?;
            Ok(Self {
                device,
                context,
                vertex_shader,
                sdr_shader,
                hdr_shader,
                sampler: sampler.ok_or_else(windows::core::Error::from_win32)?,
                tone_map_constants,
                output,
                output_view,
                output_size,
                thumbnail_shader,
                thumbnail_constants,
                thumbnail_output,
                thumbnail_view,
                thumbnail_staging,
            })
        }
    }

    pub(super) fn transform(
        &self,
        source: &ID3D11Texture2D,
        input_size: CaptureSize,
        inset: Inset,
        hdr: bool,
    ) -> windows::core::Result<ID3D11Texture2D> {
        crate::insight_scope!("gpu_transform");
        unsafe {
            let source_view = {
                crate::insight_scope!("gpu_view_create");
                self.source_view(source)?
            };
            self.context
                .ClearRenderTargetView(&self.output_view, &[0.0, 0.0, 0.0, 1.0]);
            let fit = source_viewport(input_size, inset, self.output_size);
            self.draw_fullscreen(
                &self.output_view,
                fit,
                if hdr {
                    &self.hdr_shader
                } else {
                    &self.sdr_shader
                },
                &self.tone_map_constants,
                source_view,
            );
            Ok(self.output.clone())
        }
    }

    unsafe fn source_view(
        &self,
        source: &ID3D11Texture2D,
    ) -> windows::core::Result<ID3D11ShaderResourceView> {
        let mut view = None;
        self.device
            .CreateShaderResourceView(source, None, Some(&mut view))?;
        view.ok_or_else(windows::core::Error::from_win32)
    }

    /// The one draw both passes make: `source` through `shader` with
    /// `constants`, as a full-screen triangle into `viewport` of `target`.
    /// The source is unbound again after the draw, so the texture is not still
    /// bound as a shader resource when the next pass renders into it.
    unsafe fn draw_fullscreen(
        &self,
        target: &ID3D11RenderTargetView,
        viewport: FitRect,
        shader: &ID3D11PixelShader,
        constants: &ID3D11Buffer,
        source: ID3D11ShaderResourceView,
    ) {
        self.context.OMSetRenderTargets(
            Some(&[Some(target.clone())]),
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
        );
        self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
            TopLeftX: viewport.x as f32,
            TopLeftY: viewport.y as f32,
            Width: viewport.width as f32,
            Height: viewport.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]));
        self.context
            .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        self.context.VSSetShader(&self.vertex_shader, None);
        self.context.PSSetShader(shader, None);
        self.context
            .PSSetConstantBuffers(0, Some(&[Some(constants.clone())]));
        self.context
            .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        self.context.PSSetShaderResources(0, Some(&[Some(source)]));
        self.context.Draw(3, 0);
        self.context.PSSetShaderResources(0, Some(&[None]));
    }

    /// Replaces the tone curve's white point while a recording runs, when the
    /// capture monitor's HDR state flips under it (t260917-7faa). The buffer is immutable, so this makes a new one; it
    /// happens only on a display settings change.
    pub(super) fn set_hdr_white_point(
        &mut self,
        hdr_white_point: f32,
    ) -> windows::core::Result<()> {
        self.tone_map_constants = unsafe { create_tone_map_buffer(&self.device, hdr_white_point)? };
        Ok(())
    }

    /// The texture the last `transform` drew into. There is only ever one --
    /// `transform` hands back `self.output` every time -- so this is literally
    /// the frame still on the pipeline, which the frame-hold watchdog re-encodes
    /// when WGC has stopped delivering (task163).
    pub(super) fn last_output(&self) -> ID3D11Texture2D {
        self.output.clone()
    }

    /// Draws `source` down to [`THUMBNAIL_SIZE`] on the GPU and reads *that*
    /// back, as tightly-packed top-down BGRA (task2820).
    ///
    /// Replaces reading the full-size frame back and resizing it on the CPU:
    /// the copy goes from ~32MB to 518,400 bytes, and
    /// `targets::bgra_to_resized_jpeg` now finds its input already at the
    /// target size and takes its "same size, nothing to do" path
    /// (`targets.rs`), so the CPU resize disappears entirely.
    ///
    /// Deliberately *not* aspect-preserving, matching what the CPU resize did:
    /// the history grid draws these tiles full-bleed, and letterboxing them
    /// would change every thumbnail on screen for a task about cost.
    ///
    /// Only queues the draw and the copy into the staging texture
    /// (t261017-613d); `collect_thumbnail` reads it. The `Map` used to wait
    /// here for the GPU to reach the copy -- queue depth, not bytes, 24-44ms
    /// at a 1s boundary under a game's GPU load (t261016-3fbd) -- on the one
    /// thread that feeds the encoder. One staging texture: a copy queued
    /// before the previous one was collected replaces it, newest wins.
    pub(super) fn queue_thumbnail(&self, source: &ID3D11Texture2D) -> windows::core::Result<()> {
        crate::insight_scope!("gpu_read_bgra");
        unsafe {
            let source_view = self.source_view(source)?;
            self.draw_fullscreen(
                &self.thumbnail_view,
                FitRect {
                    x: 0,
                    y: 0,
                    width: THUMBNAIL_SIZE.width,
                    height: THUMBNAIL_SIZE.height,
                },
                &self.thumbnail_shader,
                &self.thumbnail_constants,
                source_view,
            );
            self.context
                .CopyResource(&self.thumbnail_staging, &self.thumbnail_output);
            // `Map(DO_NOT_WAIT)` does not submit the command buffer itself; an
            // unflushed copy could report "still drawing" until something else
            // happened to flush.
            self.context.Flush();
        }
        Ok(())
    }

    /// Reads the copy `queue_thumbnail` left in the staging texture. With
    /// `wait` false, `Ok(None)` means the GPU has not reached it yet -- try
    /// again later. `wait` true blocks until it has (the stop drain, so the
    /// last segment keeps its thumbnail).
    pub(super) fn collect_thumbnail(
        &self,
        wait: bool,
    ) -> windows::core::Result<Option<(u32, u32, Vec<u8>)>> {
        unsafe { read_staging(&self.context, &self.thumbnail_staging, THUMBNAIL_SIZE, wait) }
    }

    /// Paints the output black and returns it (task163). What the watchdog
    /// holds while the target is minimized: a frozen last frame would read as
    /// "still running", and the recording is deliberately still going.
    pub(super) fn blank(&self) -> ID3D11Texture2D {
        unsafe {
            self.context
                .ClearRenderTargetView(&self.output_view, &[0.0, 0.0, 0.0, 1.0]);
        }
        self.output.clone()
    }
}

/// The tap spacing [`THUMBNAIL_BOX_PIXEL_SHADER`] reads, in source UV.
///
/// One destination pixel covers `1/width` of the source in UV whatever the
/// source resolution is, because the draw maps the whole source across the
/// whole target. Four taps spanning that footprint sit a quarter of it apart,
/// which is the `1/(4*size)` below.
unsafe fn create_box_filter_buffer(
    device: &ID3D11Device,
    target: CaptureSize,
) -> windows::core::Result<ID3D11Buffer> {
    create_constants(
        device,
        [
            1.0 / (4.0 * target.width.max(1) as f32),
            1.0 / (4.0 * target.height.max(1) as f32),
            0.0,
            0.0,
        ],
    )
}

/// An immutable 16-byte constant buffer, the minimum D3D11 allows: one
/// `float4` of payload.
unsafe fn create_constants(
    device: &ID3D11Device,
    constants: [f32; 4],
) -> windows::core::Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of_val(&constants) as u32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        ..Default::default()
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: constants.as_ptr().cast(),
        ..Default::default()
    };
    let mut buffer = None;
    device.CreateBuffer(&desc, Some(&initial), Some(&mut buffer))?;
    buffer.ok_or_else(windows::core::Error::from_win32)
}

/// A CPU-readable twin of `texture`, made once and mapped over and over.
pub(super) unsafe fn create_staging_texture(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> windows::core::Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    texture.GetDesc(&mut desc);
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
    };
    let mut staging = None;
    device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
    staging.ok_or_else(windows::core::Error::from_win32)
}

/// `staging` -> tightly-packed top-down BGRA bytes, or `None` while the GPU
/// has not reached the copy and `wait` is false.
///
/// With `wait` the `Map` is where a readback waits for the GPU; the row loop
/// only exists because `RowPitch` is padded and the JPEG encoder wants it
/// packed.
unsafe fn read_staging(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    size: CaptureSize,
    wait: bool,
) -> windows::core::Result<Option<(u32, u32, Vec<u8>)>> {
    let (width, height) = (size.width.max(1) as u32, size.height.max(1) as u32);
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let flags = if wait {
        0
    } else {
        D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32
    };
    match context.Map(staging, 0, D3D11_MAP_READ, flags, Some(&mut mapped)) {
        Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => return Ok(None),
        result => result?,
    }
    let row_bytes = (width as usize) * 4;
    let mut bgra = vec![0u8; row_bytes * height as usize];
    let row_pitch = mapped.RowPitch as usize;
    for row in 0..height as usize {
        let src = mapped.pData.cast::<u8>().add(row * row_pitch);
        let dst = bgra.as_mut_ptr().add(row * row_bytes);
        std::ptr::copy_nonoverlapping(src, dst, row_bytes);
    }
    context.Unmap(staging, 0);
    Ok(Some((width, height, bgra)))
}

/// The HDR shader's `cbuffer ToneMap`. One `float` of payload, padded to the
/// 16-byte minimum a D3D11 constant buffer requires. The floor is applied here
/// rather than in the shader so a bad query can never hand the divide a `w` at
/// or below zero -- `c / 0` is an infinity that saturates the whole frame to
/// white, which is exactly the failure this capture path exists to avoid.
unsafe fn create_tone_map_buffer(
    device: &ID3D11Device,
    white_point: f32,
) -> windows::core::Result<ID3D11Buffer> {
    create_constants(
        device,
        [white_point.max(HDR_WHITE_POINT_FLOOR), 0.0, 0.0, 0.0],
    )
}

unsafe fn create_output_texture(
    device: &ID3D11Device,
    size: CaptureSize,
) -> windows::core::Result<(ID3D11Texture2D, ID3D11RenderTargetView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.width.max(1) as u32,
        Height: size.height.max(1) as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
        ..Default::default()
    };
    let mut output = None;
    device.CreateTexture2D(&desc, None, Some(&mut output))?;
    let output = output.ok_or_else(windows::core::Error::from_win32)?;
    let mut output_view = None;
    device.CreateRenderTargetView(&output, None, Some(&mut output_view))?;
    Ok((
        output,
        output_view.ok_or_else(windows::core::Error::from_win32)?,
    ))
}

/// Reads a whole BGRA8 `ID3D11Texture2D` back to CPU memory as tightly-packed
/// top-down BGRA bytes (no `RowPitch` padding).
///
/// Test-only since task2820. Segment thumbnails used to come through here and
/// were then resized on the CPU; they now go through
/// `TransformPipeline::queue_thumbnail` / `collect_thumbnail`, which read back the already-shrunk
/// picture instead. What is left is the transform tests' way of looking at
/// output pixels, which genuinely wants every one of them.
///
/// `transform`'s output is always `DXGI_FORMAT_B8G8R8A8_UNORM` regardless of
/// HDR/SDR input (the pixel shader tone-maps HDR down to it; see
/// `create_output_texture`), so there is no separate HDR handling here.
#[cfg(test)]
pub(super) fn read_bgra_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> windows::core::Result<(u32, u32, Vec<u8>)> {
    // A GPU->CPU readback on the capture thread: the shape of work that
    // stalls a pipeline, so it is measured separately (task205).
    crate::insight_scope!("gpu_read_bgra");
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
        let staging = create_staging_texture(device, texture)?;
        context.CopyResource(&staging, texture);
        let size = CaptureSize {
            width: desc.Width as i32,
            height: desc.Height as i32,
        };
        read_staging(context, &staging, size, true)?.ok_or_else(windows::core::Error::from_win32)
    }
}

/// The 4 bytes at (`x`, `y`) of tightly packed BGRA rows `width` wide.
#[cfg(test)]
pub(super) fn pixel_at(bgra: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * width + x) * 4) as usize;
    [bgra[at], bgra[at + 1], bgra[at + 2], bgra[at + 3]]
}
