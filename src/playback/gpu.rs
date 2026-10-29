//! NV12 -> RGBA and the downscale to the stage, in one `VideoProcessorBlt`
//! (t260913-2527).
//!
//! Playback used to pay both on the CPU: `livia_pixels::nv12_to_rgba` (1.29ms a
//! frame at 720p) then `FrameScaler` (2.55ms), while the *recording* side has
//! been doing the same colour conversion on the GPU at 140µs for as long as
//! there has been one (`capture::gpu::GpuNv12Converter`). This is that
//! asymmetry closed: `ID3D11VideoProcessor` does the colour conversion and a
//! pixel shader does the resample, both on the GPU in one submission.
//!
//! It is not free, because slint cannot take a D3D11 texture -- 1.17's only GPU
//! inputs are OpenGL and WGPU, and this app runs skia on a D3D11 surface -- and
//! because the playback decoder hands over system memory (no
//! `IMFDXGIDeviceManager` on that reader). So the frame goes up as NV12 and
//! comes back as RGBA. What makes it pay is that **the readback is the stage's
//! size, not the recording's**: measured by `examples/gpu_readback_probe.rs`,
//! 1922x1112 NV12 up is 0.211ms and 1213x682 RGBA down is 0.366ms.
//!
//! **The blit converts; it does not scale.** Step 2 of the task chose
//! `ID3D11VideoProcessor` partly because "the driver picks the minification
//! filter", and the measurement killed that half: black/white stripes minified
//! 0.625x -- the ratio windowed playback actually uses -- came back **102 counts
//! from mid-grey** through the processor's own scaling against **8** through
//! `FrameScaler`'s CatmullRom. That is a plain two-row bilinear tap, which is
//! precisely the aliasing task990 exists to remove, so using it would have been
//! a silent revert of task990 wearing a performance win as a disguise.
//!
//! (The earlier version of that test halved the image, and at exactly 0.5x every
//! filter that touches both rows answers 128. It proved nothing. A ratio test
//! has to be at a ratio the app uses.)
//!
//! So the blit runs 1:1 for the colour conversion -- validated, mean 0.47
//! against the CPU -- and a pixel shader does the resample with the same
//! CatmullRom kernel `fast_image_resize` uses. Both still happen on the GPU, in
//! one submission, with one readback.
//!
//! Everything here is best-effort. Construction failing, a format the processor
//! will not write, a blit that errors -- each falls back to the CPU pair, which
//! stays exactly as it was.

use std::sync::{Arc, Mutex, Once, PoisonError};

use crate::gpu::{create_pixel_shader, create_vertex_shader, FULLSCREEN_VERTEX_SHADER};

use windows::{
    core::{Interface, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, GENERIC_ALL, HANDLE, RECT},
        Graphics::{
            Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            Direct3D11::{
                ID3D11Buffer, ID3D11Device, ID3D11Device5, ID3D11DeviceContext,
                ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11PixelShader,
                ID3D11RenderTargetView, ID3D11Resource, ID3D11ShaderResourceView, ID3D11Texture2D,
                ID3D11VertexShader, ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice,
                ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
                ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
                D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET,
                D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
                D3D11_CPU_ACCESS_WRITE, D3D11_FENCE_FLAG_SHARED, D3D11_MAP_READ,
                D3D11_MAP_WRITE_DISCARD, D3D11_RESOURCE_MISC_SHARED,
                D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV,
                D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
                D3D11_USAGE_IMMUTABLE, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VIEWPORT,
                D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
            },
            Direct3D12::{ID3D12CommandQueue, ID3D12Device, ID3D12Fence, ID3D12Resource},
            Dxgi::Common::{
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_NV12,
                DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
            },
            Dxgi::{IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE},
        },
        Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager},
    },
};

/// The device and the per-size pipeline built on it.
///
/// One of these lives on the playback engine's thread and is never shared with
/// the capture side's. Ownership and lifetime, not thread safety: the capture
/// device is created inside `CaptureSurface::open` on the recording's own
/// worker thread and dies when that recording stops, while playback has to work
/// when nothing is recording at all. Sharing it would also queue playback's
/// blits behind the recording's on one immediate context.
pub(super) struct GpuScaler {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    /// The same device again, in the shape `IMFSourceReader` takes
    /// (t260915-e6c8). Playback's decoder decodes **onto this device**, so the
    /// texture the decoder writes and the video processor that reads it are on
    /// one device and no frame has to cross between two.
    manager: IMFDXGIDeviceManager,
    /// The resample pass. Size-independent -- everything that is not is in
    /// `Pipeline` -- so these are compiled once for the session.
    vertex_shader: ID3D11VertexShader,
    resample_shader: ID3D11PixelShader,
    /// Rebuilt when the recording's size or the stage's changes, which is a
    /// resize -- not something that happens per frame.
    pipeline: Option<Pipeline>,
    /// Whether the stage picture is left on the GPU for slint instead of read
    /// back (t260917-66c2). The caller sets it per frame
    /// ([`Self::set_share`]); a change rebuilds the pipeline.
    share: bool,
    /// Set for the session once the shared textures or the fence could not be
    /// made: this machine reads back, as it always did.
    share_failed: bool,
    /// Signalled after each copy into the shared ring; slint's queue waits on
    /// it before sampling. Made once, on the first shared pipeline.
    fence: Option<ShareFence>,
    /// The shared ring of a pipeline that was rebuilt, kept for the next shared
    /// pipeline of the same stage size (t260930-8bce). A new engine starts with
    /// sharing off for a frame, so without this every engine made a fresh ring
    /// -- and every ring slint's D3D12 device opens costs its size in dedicated
    /// GPU memory that never comes back, whoever lets go of it.
    spare_ring: Option<ShareRing>,
    /// The picture the last call handed back, when it is a shared one.
    picture: Option<SharedPicture>,
}

/// Says `$what` once per process, per message: every one of these failures is
/// a property of the machine and this path runs on the frame deadline, so the
/// same failure is not said every frame -- but a different one after it still
/// is. Until t261025-d24d the whole module shared one `Once`, and a "no D3D11
/// device" swallowed every later failure, "GPU convert+scale failed" included.
macro_rules! warn_once {
    ($what:literal, $error:expr $(,)?) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        let error: &windows::core::Error = $error;
        ONCE.call_once(|| tracing::warn!(event = "playback_gpu_scale_unavailable", %error, $what));
    }};
}

/// Shared textures in the ring the stage picture is copied into.
///
/// ponytail: no D3D12 -> D3D11 fence. The slot written is always the one
/// handed out `SHARE_RING - 1` frames ago, so slint has had that many frame
/// intervals to finish sampling it; a torn stage frame is the failure if it
/// ever does not. Signal a D3D12 fence from `AfterRendering` and wait on it here
/// if that shows up.
const SHARE_RING: usize = 4;

/// Shared textures [`StageDevice`] keeps open: the clip's ring and the review's.
const STAGE_CACHE: usize = 2 * SHARE_RING;

/// An NT handle, closed when the last holder lets go. `isize` rather than
/// `HANDLE` so a frame carrying one can cross to the UI thread.
#[derive(Debug)]
struct NtHandle(isize);

impl NtHandle {
    fn new(handle: HANDLE) -> Arc<Self> {
        Arc::new(Self(handle.0 as isize))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut _)
    }
}

impl Drop for NtHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle());
        }
    }
}

/// A stage-sized RGBA picture left on the GPU (t260917-66c2): a shared texture
/// of the playback device's, and the fence value that says its copy is done.
/// [`StageDevice::texture`] opens it on slint's device.
#[derive(Clone, Debug)]
pub struct SharedPicture {
    pub width: u32,
    pub height: u32,
    texture: Arc<NtHandle>,
    fence: Arc<NtHandle>,
    fence_value: u64,
}

struct ShareFence {
    fence: ID3D11Fence,
    context: ID3D11DeviceContext4,
    handle: Arc<NtHandle>,
    value: u64,
}

struct ShareRing {
    target: (u32, u32),
    slots: Vec<(ID3D11Texture2D, Arc<NtHandle>)>,
    cycle: RingCycle,
}

/// The scaler the last playback engine left behind (t260930-8bce), for the
/// next one to take instead of standing up a device of its own.
///
/// The stage picture's shared ring lives on the scaler's device, and each ring
/// slint's D3D12 device opens is charged for good: a scaler per engine -- one
/// per recording -- was 16-25 MB of dedicated GPU memory per recording that
/// never came back. Handing the one scaler from engine to engine keeps the ring,
/// so `StageDevice` finds the handles it already opened.
static PARKED: Mutex<Option<Parked>> = Mutex::new(None);

struct Parked(GpuScaler);

/// [`GpuScaler::park`]'s rule on a slot of the caller's (t260930-70ff): an
/// empty slot takes the scaler; a held one without a shared ring gives way to
/// an incoming one with a ring, since the ring is what slint's device has
/// opened for good. Otherwise the incoming scaler is dropped.
fn settle(slot: &mut Option<Parked>, incoming: GpuScaler) {
    let replace = match slot.as_ref() {
        None => true,
        Some(held) => !held.0.has_ring() && incoming.has_ring(),
    };
    let dropped_ring = if replace {
        slot.replace(Parked(incoming)).map(|held| held.0.has_ring())
    } else {
        Some(incoming.has_ring())
    };
    if let Some(had_ring) = dropped_ring {
        tracing::info!(
            event = "playback_gpu_scaler_dropped",
            had_ring,
            "playback GPU scaler"
        );
    }
}

// SAFETY: the scaler is moved, never shared -- the mutex hands it to exactly one
// engine thread at a time. The D3D11 device and the DXGI device manager are
// free-threaded, and the immediate context is `SetMultithreadProtected`
// (`create_device`) because the decoder already uses it from MF's threads.
unsafe impl Send for Parked {}

impl ShareRing {
    /// The picture held for staging-cycle index `index`.
    fn picture(
        &self,
        index: usize,
        (width, height): (u32, u32),
        fence: &Arc<NtHandle>,
    ) -> Option<SharedPicture> {
        let (slot, fence_value) = self.cycle.held[index]?;
        Some(SharedPicture {
            width,
            height,
            texture: self.slots[slot].1.clone(),
            fence: fence.clone(),
            fence_value,
        })
    }
}

/// Which ring slot each [`StagingCycle`] index's picture went into, and the
/// fence value it was signalled at. Pure, like `StagingCycle`, so the slot
/// rotation is testable without a device.
#[derive(Debug, Default, PartialEq, Eq)]
struct RingCycle {
    next: usize,
    held: [Option<(usize, u64)>; 2],
}

impl RingCycle {
    /// The slot this frame's picture is copied into, recorded against the
    /// staging-cycle index `write`.
    fn claim(&mut self, write: usize, fence_value: u64) -> usize {
        let slot = self.next;
        self.next = (slot + 1) % SHARE_RING;
        self.held[write] = Some((slot, fence_value));
        slot
    }
}

/// The same CatmullRom minification `livia_pixels::FrameScaler` does, as a
/// pixel shader.
///
/// `scale` is source/target per axis (always >= 1 here; the caller only takes
/// this path for a downscale) and it is what makes this a *resampler* rather
/// than a bilinear tap: the kernel's footprint is scaled with the ratio, so a
/// 1.6x minification averages about six source rows instead of two.
///
/// `SV_POSITION.xy` arrives as the destination pixel's centre, so the source
/// centre is just `position * scale`; a tap at source pixel `i` sits at
/// `i + 0.5` and its kernel argument is that distance divided by `scale`.
/// The weights are **normalized by their own sum** -- the discrete sum is not 1,
/// and skipping that shows up as a brightness shift the gradient test catches.
///
/// `Load`, not `Sample`: a linear sampler would filter underneath the filter.
/// The format is `_UNORM` rather than `_UNORM_SRGB` on purpose -- the CPU
/// CatmullRom convolves the gamma-encoded bytes, and the two only agree if this
/// one does too.
///
// ponytail: one pass with the 2D kernel product, which is ~4x the taps of the
// separable two-pass form at these ratios. Measured cheaper than the readback
// it shares a frame with; go separable if the ratio ever grows.
const RESAMPLE_PIXEL_SHADER: &[u8] = b"
Texture2D<float4> sourceTexture : register(t0);
cbuffer Resample : register(b0) { float2 scale; float2 sourceSize; };
float catmullRom(float x) {
  x = abs(x);
  float x2 = x * x;
  if (x < 1.0) return 1.5 * x2 * x - 2.5 * x2 + 1.0;
  if (x < 2.0) return -0.5 * x2 * x + 2.5 * x2 - 4.0 * x + 2.0;
  return 0.0;
}
float4 main(float4 position : SV_POSITION, float2 uv : TEXCOORD) : SV_TARGET {
  float2 centre = position.xy * scale;
  float2 first = floor(centre - 2.0 * scale - 0.5);
  float2 last = ceil(centre + 2.0 * scale - 0.5);
  float3 accumulated = float3(0.0, 0.0, 0.0);
  float total = 0.0;
  for (float y = first.y; y <= last.y; y += 1.0) {
    float wy = catmullRom((y + 0.5 - centre.y) / scale.y);
    for (float x = first.x; x <= last.x; x += 1.0) {
      float w = wy * catmullRom((x + 0.5 - centre.x) / scale.x);
      int2 at = int2(clamp(x, 0.0, sourceSize.x - 1.0), clamp(y, 0.0, sourceSize.y - 1.0));
      accumulated += w * sourceTexture.Load(int3(at, 0)).rgb;
      total += w;
    }
  }
  return float4(saturate(accumulated / total), 1.0);
}\0";

/// Everything that depends on one (source, target) pair.
///
/// The input and output *views* are built here rather than per blit on purpose.
/// `capture::gpu::GpuNv12Converter` creates them inside `convert_into` and had
/// to add a `ManuallyDrop` teardown to stop them leaking a device reference per
/// frame (task4280); a view that outlives the frame cannot leak per frame.
struct Pipeline {
    source: (u32, u32),
    target: (u32, u32),
    processor: ID3D11VideoProcessor,
    /// CPU-writable NV12, where the decoder's rows land.
    ///
    /// Separate from `input` because `CreateVideoProcessorInputView` refuses a
    /// `D3D11_USAGE_DYNAMIC` resource outright -- `E_INVALIDARG`, measured on
    /// this machine 2026-09-14 -- so the texture the CPU can write and the
    /// texture the processor can read cannot be the same one. The
    /// `CopyResource` between them never leaves the GPU.
    upload: ID3D11Texture2D,
    input: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    /// The blit's output -- recording-sized RGBA -- and the resample pass's
    /// input. Held only as its two views: each of them AddRefs the texture, so
    /// a third handle to it would be a field nothing reads.
    converted_view: ID3D11VideoProcessorOutputView,
    converted_resource: ID3D11ShaderResourceView,
    /// The resample pass's render target: stage-sized RGBA.
    output: ID3D11Texture2D,
    output_target: ID3D11RenderTargetView,
    /// `scale` and `sourceSize` for [`RESAMPLE_PIXEL_SHADER`].
    constants: ID3D11Buffer,
    /// Where the RGBA result is copied to be mapped for reading -- **two** of
    /// them (t260915-8ea7). Frame N's `CopyResource` goes into one while the
    /// other, holding N-1, is the one that gets mapped, so the `Map(READ)` waits
    /// for a blit that was submitted a tick ago instead of the one submitted
    /// three calls up this function.
    staging: [ID3D11Texture2D; 2],
    /// The NV12 twin of `staging`, on the same cycle index (t260915-e6c8).
    ///
    /// The hardware decoder hands over a texture, and `PlaybackFrame::full` --
    /// the recording-sized picture a screenshot saves -- still wants the NV12
    /// as bytes. Reading it back through the cycle costs a tick of latency and
    /// nothing on the frame deadline, where `IMF2DBuffer2::Lock2D` on a GPU
    /// surface costs the 12ms `.agents/docs/spike-seek-decode.md` rejected the
    /// whole idea over. Unused by the system-memory path, which already has the
    /// bytes.
    nv12_staging: [ID3D11Texture2D; 2],
    /// Which of the two is next to be written, and whether the other holds a
    /// picture nobody has read yet.
    cycle: StagingCycle,
    /// Where the RGBA picture goes instead of `staging` when it is shared
    /// (t260917-66c2). `staging` is then left unwritten.
    share: Option<ShareRing>,
}

/// Which staging texture this frame writes into and which one is read back.
///
/// Split out of [`Pipeline`] on purpose (t260915-8ea7): it is the whole of the
/// double buffer's logic and the only part of it that can be tested without a
/// D3D11 device.
#[derive(Debug, Default, PartialEq, Eq)]
struct StagingCycle {
    write: usize,
    /// Set once a blit has been written and not yet read back. False on a fresh
    /// pipeline -- the state the first frame after a rebuild is in.
    filled: bool,
    /// The `nv12_staging` slot holding the NV12 of the picture last handed
    /// back, for [`GpuScaler::read_published_nv12`] (t261016-97b7). Set by
    /// the caller after a readback that copied NV12; cleared here the moment a
    /// blit is about to overwrite that slot.
    published: Option<usize>,
}

impl StagingCycle {
    /// `(write, read)`: the index this frame's picture is copied into, and the
    /// index to map. `read` is `None` only on the first pipelined frame after a
    /// rebuild, where there is no earlier picture to hand back.
    fn advance(&mut self, readback: Readback) -> (usize, Option<usize>) {
        let write = self.write;
        if self.published == Some(write) {
            self.published = None;
        }
        let read = match readback {
            // The caller is paying the stall on purpose: read what was just
            // written, and leave nothing behind for the next call.
            Readback::Now => Some(write),
            Readback::Pipelined => self.filled.then_some(1 - write),
        };
        self.filled = matches!(readback, Readback::Pipelined);
        self.write = 1 - write;
        (write, read)
    }

    /// Forgets the picture in flight without reading it: the next pipelined
    /// frame is a first frame again.
    ///
    /// The caller's half of the same drop -- a seek, a park, a crossing -- keeps
    /// the *planes*, and the two have to be dropped together or the next call
    /// reads back a picture with nothing to pair it with.
    fn abandon(&mut self) {
        self.filled = false;
    }
}

/// Where this frame's NV12 already is.
///
/// The two are not interchangeable and the caller does not choose: it is
/// whatever the decoder that produced the sample hands over (t260915-e6c8).
enum Nv12Source<'a> {
    /// Tightly packed system memory -- the software decoder's output, and the
    /// only shape playback had before t260915-e6c8.
    Planes(&'a [u8]),
    /// A slice of the hardware decoder's own texture array. Nothing has been
    /// read back at this point, and the point of the whole change is that
    /// nothing has to be.
    Texture(&'a ID3D11Texture2D, u32),
}

/// When the caller wants the picture of the frame it is handing in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Readback {
    /// Next call. This is what takes the `Map(READ)` off the frame deadline,
    /// and it costs the preview one frame of latency.
    Pipelined,
    /// Now, stalling until the GPU has run this frame's blit. For the one frame
    /// per seek that has no next call to be read on: the transport may be
    /// paused, so a scrub would otherwise leave the stage on the frame before.
    Now,
}

/// What [`GpuScaler::convert_and_scale`] left in `rgba`.
///
/// Four states rather than the `bool` this replaced, because "no picture yet"
/// and "no picture ever" are opposite instructions: rounding the first to
/// `false` would run the CPU pair for a frame whose GPU blit is already in
/// flight, which is more work than the path started with, not less.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Converted {
    /// `rgba` holds the picture of the frame handed to the **previous** call.
    Previous,
    /// `rgba` holds this frame's own picture ([`Readback::Now`]).
    This,
    /// The blit was issued and nothing was read back: this frame's picture
    /// arrives with the next call. Nothing was written to `rgba`.
    Warming,
    /// Nothing was written and nothing is in flight; the caller must use the
    /// CPU pair.
    Failed,
}

impl GpuScaler {
    /// A scaler on a fresh hardware device, or `None` when this machine will not
    /// give one -- in which case playback keeps doing both steps on the CPU.
    pub(super) fn new() -> Option<Self> {
        let (device, context) = create_device()
            .map_err(|error| warn_once!("playback: no D3D11 device for the GPU scaler", &error))
            .ok()?;
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|error| warn_once!("playback: no ID3D11VideoDevice", &error))
            .ok()?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|error| warn_once!("playback: no ID3D11VideoContext", &error))
            .ok()?;
        let (vertex_shader, resample_shader) = unsafe {
            (
                create_vertex_shader(&device, FULLSCREEN_VERTEX_SHADER),
                create_pixel_shader(&device, RESAMPLE_PIXEL_SHADER),
            )
        };
        let vertex_shader = vertex_shader
            .map_err(|error| warn_once!("playback: the full-screen vertex shader", &error))
            .ok()?;
        let resample_shader = resample_shader
            .map_err(|error| warn_once!("playback: the CatmullRom resample shader", &error))
            .ok()?;
        let manager = create_manager(&device)
            .map_err(|error| warn_once!("playback: no DXGI device manager", &error))
            .ok()?;
        Some(Self {
            device,
            context,
            video_device,
            video_context,
            manager,
            vertex_shader,
            resample_shader,
            pipeline: None,
            share: false,
            share_failed: false,
            fence: None,
            spare_ring: None,
            picture: None,
        })
    }

    /// The scaler a finished engine parked, or a new one (t260930-8bce).
    pub(super) fn take_or_new() -> Option<Self> {
        let parked = PARKED.lock().unwrap_or_else(PoisonError::into_inner).take();
        tracing::info!(
            event = "playback_gpu_scaler_taken",
            reused = parked.is_some(),
            "playback GPU scaler"
        );
        parked.map(|parked| parked.0).or_else(Self::new)
    }

    /// Leaves this scaler for the next engine's [`Self::take_or_new`], with
    /// nothing of this engine's in flight.
    ///
    // ponytail: one slot, and a scaler with a shared ring wins it
    // (t260930-70ff). Engines overlap -- the review engine stays alive under
    // the clip screen -- so a clip's scaler parks before the review's does;
    // dropping the review's ring there cost 15.7-26.0 MB per review -> clip ->
    // review round, flat once the ring-bearer wins. Switching inside the clip
    // screen measured flat either way. Two ring-bearing scalers at once (not
    // seen) still drops the incoming one; a slot per stage size if it happens.
    pub(super) fn park(mut self) {
        self.discard_in_flight();
        self.picture = None;
        settle(
            &mut PARKED.lock().unwrap_or_else(PoisonError::into_inner),
            self,
        );
    }

    /// Whether this scaler carries a shared ring: in its pipeline (an engine
    /// that was sharing when it parked) or kept aside from a rebuild.
    fn has_ring(&self) -> bool {
        self.spare_ring.is_some()
            || self
                .pipeline
                .as_ref()
                .is_some_and(|held| held.share.is_some())
    }

    /// The device manager to open a segment's Source Reader with, so the
    /// decoder lands its frames on this scaler's own device (t260915-e6c8).
    ///
    /// It only exists where the scaler does, which is what keeps the two in
    /// step: a machine with no video device gets neither, and its playback
    /// stays exactly the software path it always was.
    pub(super) fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }

    /// Asks for the picture to be left on the GPU (t260917-66c2). Refused for
    /// good once sharing failed on this device.
    pub(super) fn set_share(&mut self, want: bool) {
        self.share = want && !self.share_failed;
    }

    /// Whether the next call hands its picture back through
    /// [`Self::take_picture`] instead of `rgba` -- which may then be empty.
    pub(super) fn sharing(&self) -> bool {
        self.share
    }

    /// The shared picture the last call (or [`Self::read_in_flight`]) handed
    /// back, in place of `rgba`.
    pub(super) fn take_picture(&mut self) -> Option<SharedPicture> {
        self.picture.take()
    }

    /// `nv12` (tightly packed, `source.0` bytes a row) converted and scaled into
    /// `rgba`, which must hold `target.0 * target.1 * 4` bytes.
    ///
    /// Which frame's picture that is -- and whether there is one at all -- is
    /// [`Converted`]; only [`Converted::Failed`] means the caller must use the
    /// CPU pair.
    pub(super) fn convert_and_scale(
        &mut self,
        nv12: &[u8],
        source: (u32, u32),
        target: (u32, u32),
        rgba: &mut [u8],
        readback: Readback,
    ) -> Converted {
        if nv12.len() < nv12_bytes(source) {
            return Converted::Failed;
        }
        self.run(
            Nv12Source::Planes(nv12),
            source,
            target,
            rgba,
            None,
            readback,
        )
    }

    /// [`Self::convert_and_scale`] over the hardware decoder's own texture
    /// (t260915-e6c8), which is what a reader opened with [`Self::manager`]
    /// hands back.
    ///
    /// On [`Readback::Now`] `planes` is filled with this frame's NV12. On
    /// [`Readback::Pipelined`] it is left untouched (t261016-97b7): the NV12 of
    /// the picture handed back stays in its staging slot, and
    /// [`Self::read_published_nv12`] reads it only when something asks.
    pub(super) fn convert_texture_and_scale(
        &mut self,
        (texture, slice): (&ID3D11Texture2D, u32),
        source: (u32, u32),
        target: (u32, u32),
        rgba: &mut [u8],
        planes: &mut Vec<u8>,
        readback: Readback,
    ) -> Converted {
        self.run(
            Nv12Source::Texture(texture, slice),
            source,
            target,
            rgba,
            Some(planes),
            readback,
        )
    }

    fn run(
        &mut self,
        nv12: Nv12Source<'_>,
        source: (u32, u32),
        target: (u32, u32),
        rgba: &mut [u8],
        planes: Option<&mut Vec<u8>>,
        readback: Readback,
    ) -> Converted {
        crate::insight_scope!("playback_gpu_convert_scale");
        self.picture = None;
        if !self.share && rgba.len() != super::rgba_bytes(target.0, target.1) {
            return Converted::Failed;
        }
        match self.blit(nv12, source, target, rgba, planes, readback) {
            Ok(true) => match readback {
                Readback::Pipelined => Converted::Previous,
                Readback::Now => Converted::This,
            },
            Ok(false) => Converted::Warming,
            Err(error) => {
                // Once: a driver that refuses one frame refuses every frame, and
                // this runs on the frame deadline.
                warn_once!(
                    "playback: GPU convert+scale failed; using the CPU path",
                    &error,
                );
                // A pipeline that just failed is not trusted for the next frame
                // either; the rebuild is what recovers a lost device. It also
                // empties the cycle, so the caller is told `Warming` next time
                // and drops whatever it was holding for the frame in flight.
                self.pipeline = None;
                Converted::Failed
            }
        }
    }

    /// `true` when `rgba` was written. `false` is the first pipelined frame of a
    /// pipeline: submitted, nothing to read back yet.
    fn blit(
        &mut self,
        nv12: Nv12Source<'_>,
        source: (u32, u32),
        target: (u32, u32),
        rgba: &mut [u8],
        planes: Option<&mut Vec<u8>>,
        readback: Readback,
    ) -> windows::core::Result<bool> {
        // Sections of the `playback_gpu_convert_scale` total (t260922-c15f).
        // Each is its own block: `insight_scope!` is a `let`, so a second one in
        // the same block would shadow the first without closing it.
        {
            crate::insight_scope!("playback_gpu_ensure_pipeline");
            self.ensure_pipeline(source, target)?;
        }
        // Before the shared borrow below, and after `ensure_pipeline`: a rebuilt
        // pipeline starts its cycle empty, which is what makes a resize hand the
        // caller `Warming` instead of a picture of the old size.
        let fence_value = match (&self.pipeline, self.fence.as_mut()) {
            (Some(Pipeline { share: Some(_), .. }), Some(fence)) => {
                fence.value += 1;
                Some(fence.value)
            }
            _ => None,
        };
        let held = self
            .pipeline
            .as_mut()
            .expect("ensure_pipeline leaves one behind");
        let (write, read) = held.cycle.advance(readback);
        let slot = held
            .share
            .as_mut()
            .zip(fence_value)
            .map(|(ring, value)| ring.cycle.claim(write, value));
        let pipeline = self
            .pipeline
            .as_ref()
            .expect("ensure_pipeline leaves one behind");
        unsafe {
            self.upload(pipeline, nv12, source)?;
            self.convert(pipeline, target)?;
            self.copy_out(pipeline, (write, slot), fence_value, planes.is_some())?;
            match read {
                Some(index) => {
                    let copied_nv12 = planes.is_some();
                    // Only a picture that stays on the stage reads its NV12 now
                    // (t261016-97b7): per playing frame the map + row copy was
                    // 78% of this scope, for planes only a screenshot or a
                    // paused redraw ever reads. Playing, they stay in the slot
                    // until `read_published_nv12` asks or the next blit
                    // overwrites them.
                    let planes = planes.filter(|_| matches!(readback, Readback::Now));
                    self.picture = read_back(
                        &self.context,
                        pipeline,
                        self.fence.as_ref(),
                        index,
                        rgba,
                        planes,
                    )?;
                    if copied_nv12 {
                        if let Some(held) = self.pipeline.as_mut() {
                            held.cycle.published = Some(index);
                        }
                    }
                    Ok(true)
                }
                None => Ok(false),
            }
        }
    }

    /// The blit's first step: this frame's NV12 into the pipeline's input
    /// texture, from the CPU's planes or out of the decoder's own texture.
    unsafe fn upload(
        &self,
        pipeline: &Pipeline,
        nv12: Nv12Source<'_>,
        source: (u32, u32),
    ) -> windows::core::Result<()> {
        match nv12 {
            Nv12Source::Planes(bytes) => {
                crate::insight_scope!("playback_gpu_upload_planes");
                upload_nv12(&self.context, &pipeline.upload, bytes, source)
                    .map_err(at("Map(WRITE_DISCARD) of the NV12 upload texture"))?;
                self.context.CopyResource(
                    &pipeline.input.cast::<ID3D11Resource>()?,
                    &pipeline.upload.cast::<ID3D11Resource>()?,
                );
            }
            Nv12Source::Texture(texture, slice) => {
                crate::insight_scope!("playback_gpu_upload_texture");
                // A box copy rather than `CopyResource`: the decoder's
                // surfaces are padded to its own alignment (1088 rows for a
                // 1080-row frame is the ordinary case) and `CopyResource`
                // refuses a size mismatch outright, where this takes the
                // frame's own rectangle out of whatever it was given.
                self.context.CopySubresourceRegion(
                    &pipeline.input.cast::<ID3D11Resource>()?,
                    0,
                    0,
                    0,
                    0,
                    &texture.cast::<ID3D11Resource>()?,
                    slice,
                    Some(&D3D11_BOX {
                        left: 0,
                        top: 0,
                        front: 0,
                        right: source.0,
                        bottom: source.1,
                        back: 1,
                    }),
                );
            }
        }
        Ok(())
    }

    /// The second: NV12 to RGBA on the video processor, then resampled to the
    /// stage size.
    unsafe fn convert(&self, pipeline: &Pipeline, target: (u32, u32)) -> windows::core::Result<()> {
        {
            crate::insight_scope!("playback_gpu_blt");
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(pipeline.input_view.clone())),
                ..Default::default()
            };
            let blt = self.video_context.VideoProcessorBlt(
                &pipeline.processor,
                &pipeline.converted_view,
                0,
                std::slice::from_mut(&mut stream),
            );
            // `pInputSurface` is `ManuallyDrop`, so the clone above is released
            // only here -- on the error path too (task4280's leak, in the shape
            // this module can still reach it: the view itself is cached, the
            // reference count taken for the blit is not).
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            blt.map_err(at("VideoProcessorBlt"))?;
        }
        {
            crate::insight_scope!("playback_gpu_resample");
            self.resample(pipeline, target);
        }
        Ok(())
    }

    /// The third: the picture into the shared slot (signalled) or the RGBA
    /// staging texture of cycle slot `write`, the NV12 beside it when `nv12`,
    /// and the work flushed to the GPU.
    unsafe fn copy_out(
        &self,
        pipeline: &Pipeline,
        (write, slot): (usize, Option<usize>),
        fence_value: Option<u64>,
        nv12: bool,
    ) -> windows::core::Result<()> {
        match (slot, &pipeline.share, &self.fence, fence_value) {
            (Some(slot), Some(ring), Some(fence), Some(value)) => {
                crate::insight_scope!("playback_gpu_copy_to_share");
                copy_to_staging(&self.context, &pipeline.output, &ring.slots[slot].0)
                    .map_err(at("CopyResource into the shared RGBA texture"))?;
                fence
                    .context
                    .Signal(&fence.fence, value)
                    .map_err(at("ID3D11DeviceContext4::Signal"))?;
            }
            _ => {
                crate::insight_scope!("playback_gpu_copy_to_staging");
                copy_to_staging(&self.context, &pipeline.output, &pipeline.staging[write])
                    .map_err(at("CopyResource into the RGBA staging texture"))?;
            }
        }
        if nv12 {
            crate::insight_scope!("playback_gpu_copy_to_staging_nv12");
            // The same submission as the picture, into the same cycle slot:
            // that is what makes the bytes read back below belong to the
            // frame whose picture is read back beside them.
            copy_to_staging(
                &self.context,
                &pipeline.input,
                &pipeline.nv12_staging[write],
            )
            .map_err(at("CopyResource into the NV12 staging texture"))?;
        }
        {
            crate::insight_scope!("playback_gpu_flush");
            // Send this frame's work to the GPU now, so it runs while the
            // caller waits for the next frame instead of starting only when
            // the next call's `Map(READ)` flushes it (t260927-00d0).
            self.context.Flush();
        }
        Ok(())
    }

    /// The full-screen triangle that runs [`RESAMPLE_PIXEL_SHADER`] over the
    /// converted frame. No sampler: the shader `Load`s.
    unsafe fn resample(&self, pipeline: &Pipeline, target: (u32, u32)) {
        self.context.OMSetRenderTargets(
            Some(&[Some(pipeline.output_target.clone())]),
            None::<&windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView>,
        );
        self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: target.0 as f32,
            Height: target.1 as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]));
        self.context
            .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        self.context.VSSetShader(&self.vertex_shader, None);
        self.context.PSSetShader(&self.resample_shader, None);
        self.context
            .PSSetConstantBuffers(0, Some(&[Some(pipeline.constants.clone())]));
        self.context
            .PSSetShaderResources(0, Some(&[Some(pipeline.converted_resource.clone())]));
        self.context.Draw(3, 0);
        // Unbound before the next blit writes into `converted` again: a texture
        // still bound as a shader resource cannot be a render target.
        self.context.PSSetShaderResources(0, Some(&[None]));
    }

    /// Forgets the picture whose blit is in flight (t260915-8ea7).
    ///
    /// For the caller's own drop points -- a seek, a park, a crossing -- where
    /// the pipeline itself is perfectly fine and only the frame is unwanted:
    /// without this the next call would report a picture the caller no longer
    /// has planes for.
    pub(super) fn discard_in_flight(&mut self) {
        if let Some(pipeline) = self.pipeline.as_mut() {
            pipeline.cycle.abandon();
        }
    }

    /// The picture whose blit is in flight, read back **without submitting a
    /// new blit** (t260915-7088).
    ///
    /// `true` when `rgba` was written; `false` when there is nothing in flight
    /// -- no pipeline at all, or a first frame after a rebuild -- and on a
    /// failed `Map(READ)`.
    ///
    /// The read empties the cycle whichever way it went, exactly like
    /// [`discard_in_flight`](Self::discard_in_flight): the caller has taken the
    /// planes out to pair with this picture, so a frame left in flight would
    /// come back on the next call with nothing to pair it with. A second call
    /// therefore returns `false`.
    ///
    /// For `pause()`, the one caller that stops **on** the frame in flight
    /// instead of moving the position off it. Every other drop point -- a seek,
    /// a park, a crossing -- wants `discard_in_flight`.
    pub(super) fn read_in_flight(&mut self, rgba: &mut [u8], planes: Option<&mut Vec<u8>>) -> bool {
        let Some(pipeline) = self.pipeline.as_ref() else {
            return false;
        };
        if !pipeline.cycle.filled {
            return false;
        }
        // The same guard `convert_and_scale` puts in front of a blit:
        // `read_staging` copies whole rows and a short destination would panic.
        let index = 1 - pipeline.cycle.write;
        let copied_nv12 = planes.is_some();
        let shared = pipeline.share.is_some() && self.fence.is_some();
        // The hardware-decoder path holds no planes of its own (t260915-e6c8),
        // so the frame this hands back would be published with an empty `full`
        // unless its NV12 comes out of the same slot the picture just did.
        let read = (shared
            || rgba.len() == super::rgba_bytes(pipeline.target.0, pipeline.target.1))
            && match unsafe {
                read_back(
                    &self.context,
                    pipeline,
                    self.fence.as_ref(),
                    index,
                    rgba,
                    planes,
                )
            } {
                Ok(picture) => {
                    if picture.is_some() {
                        self.picture = picture;
                    }
                    true
                }
                Err(error) => {
                    warn_once!("playback: reading back the frame in flight", &error);
                    false
                }
            };
        if let Some(held) = self.pipeline.as_mut().filter(|_| read && copied_nv12) {
            held.cycle.published = Some(index);
        }
        self.discard_in_flight();
        read
    }

    /// The NV12 of the picture last handed back, read out of the staging slot
    /// it is still sitting in (t261016-97b7); the recording size it is
    /// measured in, or `None` when no slot holds it any more -- a rebuild, or
    /// the system-memory path, which never copies NV12 into the ring.
    ///
    /// For a screenshot while playing, where the pipelined blit left the planes
    /// unread. No blit is submitted, so the cycle is untouched; the copy into
    /// that slot went out with the picture's own blit, so the `Map` does not
    /// wait on anything newer.
    pub(super) fn read_published_nv12(&mut self, planes: &mut Vec<u8>) -> Option<(u32, u32)> {
        let pipeline = self.pipeline.as_ref()?;
        let index = pipeline.cycle.published?;
        unsafe {
            read_nv12_staging(
                &self.context,
                &pipeline.nv12_staging[index],
                planes,
                pipeline.source,
            )
        }
        .map_err(|error| warn_once!("playback: reading back the published frame's NV12", &error))
        .ok()?;
        Some(pipeline.source)
    }

    fn ensure_pipeline(
        &mut self,
        source: (u32, u32),
        target: (u32, u32),
    ) -> windows::core::Result<()> {
        let share = self.share;
        if self.pipeline.as_ref().is_some_and(|held| {
            held.source == source && held.target == target && held.share.is_some() == share
        }) {
            return Ok(());
        }
        if let Some(ring) = self.pipeline.take().and_then(|held| held.share) {
            self.spare_ring = Some(ring);
        }
        let mut pipeline = self.build_pipeline(source, target)?;
        if share {
            // A machine that cannot share says so once and reads back from
            // then on; this frame goes to the CPU pair with nothing in `rgba`.
            let spare = self.spare_ring.take().filter(|ring| ring.target == target);
            let ring = self.ensure_fence().and_then(|()| match spare {
                // `next` is kept: the slot it names is still the one handed
                // out longest ago, which is what the ring's no-fence rule needs.
                Some(mut ring) => {
                    ring.cycle.held = Default::default();
                    Ok(ring)
                }
                None => self.share_ring(target),
            });
            match ring {
                Ok(ring) => pipeline.share = Some(ring),
                Err(error) => {
                    static SHARE_ONCE: Once = Once::new();
                    SHARE_ONCE.call_once(|| {
                        tracing::warn!(
                            event = "playback_gpu_share_unavailable",
                            %error,
                            "the stage picture cannot be shared with slint; reading it back"
                        )
                    });
                    self.share_failed = true;
                    self.share = false;
                    return Err(error);
                }
            }
        }
        self.pipeline = Some(pipeline);
        Ok(())
    }

    /// The fence slint's queue waits on, shared as an NT handle.
    fn ensure_fence(&mut self) -> windows::core::Result<()> {
        if self.fence.is_some() {
            return Ok(());
        }
        unsafe {
            let device: ID3D11Device5 = self.device.cast()?;
            let mut fence: Option<ID3D11Fence> = None;
            device
                .CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)
                .map_err(at("CreateFence(SHARED)"))?;
            let fence = fence.ok_or_else(windows::core::Error::from_win32)?;
            let handle = fence
                .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                .map_err(at("ID3D11Fence::CreateSharedHandle"))?;
            self.fence = Some(ShareFence {
                fence,
                context: self.context.cast()?,
                handle: NtHandle::new(handle),
                value: 0,
            });
        }
        Ok(())
    }

    /// [`SHARE_RING`] stage-sized RGBA textures slint's D3D12 device can open.
    /// `RENDER_TARGET` because `slint::Image::try_from` refuses a texture
    /// without `RENDER_ATTACHMENT` (t260917-bd02).
    fn share_ring(&self, target: (u32, u32)) -> windows::core::Result<ShareRing> {
        let description = D3D11_TEXTURE2D_DESC {
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                as u32,
            ..rgba_output(target)
        };
        let mut slots = Vec::with_capacity(SHARE_RING);
        for _ in 0..SHARE_RING {
            let texture = self
                .texture(description)
                .map_err(at("CreateTexture2D(RGBA shared)"))?;
            let handle = unsafe {
                texture.cast::<IDXGIResource1>()?.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    PCWSTR::null(),
                )
            }
            .map_err(at("IDXGIResource1::CreateSharedHandle"))?;
            slots.push((texture, NtHandle::new(handle)));
        }
        Ok(ShareRing {
            target,
            slots,
            cycle: RingCycle::default(),
        })
    }

    fn build_pipeline(
        &self,
        source: (u32, u32),
        target: (u32, u32),
    ) -> windows::core::Result<Pipeline> {
        unsafe {
            let (enumerator, processor) = self.video_processor(source)?;
            let (upload, input, input_view) = self.input_textures(source, &enumerator)?;

            let (converted_view, converted_resource) =
                self.converted_textures(source, &enumerator)?;

            let output = self
                .texture(rgba_output(target))
                .map_err(at("CreateTexture2D(RGBA output)"))?;
            let mut output_target = None;
            self.device
                .CreateRenderTargetView(&output, None, Some(&mut output_target))
                .map_err(at("CreateRenderTargetView(output)"))?;
            let output_target = output_target.ok_or_else(windows::core::Error::from_win32)?;
            let constants = self
                .resample_constants(source, target)
                .map_err(at("CreateBuffer(Resample constants)"))?;

            let staging = [
                self.texture(rgba_staging(target))
                    .map_err(at("CreateTexture2D(RGBA staging)"))?,
                self.texture(rgba_staging(target))
                    .map_err(at("CreateTexture2D(RGBA staging)"))?,
            ];
            // Built whether or not this session's decoder is the hardware one:
            // 2 x 3.1MB at 1080p against a pipeline that would have to be torn
            // down and rebuilt the first time a segment came back in the other
            // shape.
            let nv12_staging = [
                self.texture(nv12_staging_desc(source))
                    .map_err(at("CreateTexture2D(NV12 staging)"))?,
                self.texture(nv12_staging_desc(source))
                    .map_err(at("CreateTexture2D(NV12 staging)"))?,
            ];
            Ok(Pipeline {
                source,
                target,
                processor,
                upload,
                input,
                input_view,
                converted_view,
                converted_resource,
                output,
                output_target,
                constants,
                staging,
                nv12_staging,
                cycle: StagingCycle::default(),
                share: None,
            })
        }
    }

    /// `build_pipeline`'s first step: a video processor that converts the
    /// recording-sized NV12 to RGBA at the same size, studio-range BT.709 in.
    unsafe fn video_processor(
        &self,
        source: (u32, u32),
    ) -> windows::core::Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)> {
        let description = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: RATE,
            InputWidth: source.0,
            InputHeight: source.1,
            OutputFrameRate: RATE,
            // Same size in and out: the processor only converts colour here.
            // Letting it scale is what the module header measured at 102
            // counts off mid-grey.
            OutputWidth: source.0,
            OutputHeight: source.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = self
            .video_device
            .CreateVideoProcessorEnumerator(&description)
            .map_err(at("CreateVideoProcessorEnumerator"))?;
        // RGBA rather than BGRA because slint is handed RGBA: taking the
        // processor's BGRA and swizzling it back would put a full-frame CPU
        // pass into the path this task exists to empty.
        let support = enumerator
            .CheckVideoProcessorFormat(DXGI_FORMAT_R8G8B8A8_UNORM)
            .map_err(at("CheckVideoProcessorFormat"))?;
        if support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_NOTIMPL,
                "the video processor will not write R8G8B8A8",
            ));
        }
        let processor = self
            .video_device
            .CreateVideoProcessor(&enumerator, 0)
            .map_err(at("CreateVideoProcessor"))?;
        // The mirror of the capture side (`GpuNv12Converter::new`): it
        // *writes* studio-range BT.709, `livia_pixels::nv12_to_rgba` reads
        // it back as studio-range BT.709, and so does this.
        let video_context1: ID3D11VideoContext1 = self.video_context.cast()?;
        video_context1.VideoProcessorSetStreamColorSpace1(
            &processor,
            0,
            DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
        );
        video_context1.VideoProcessorSetOutputColorSpace1(
            &processor,
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
        );
        // Whole frame in, whole frame out, same size: the shader scales.
        self.video_context.VideoProcessorSetStreamSourceRect(
            &processor,
            0,
            true,
            Some(&rect(source)),
        );
        self.video_context.VideoProcessorSetStreamDestRect(
            &processor,
            0,
            true,
            Some(&rect(source)),
        );
        self.video_context.VideoProcessorSetStreamFrameFormat(
            &processor,
            0,
            D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        );
        Ok((enumerator, processor))
    }

    /// The second: the NV12 upload texture, the processor's input texture and
    /// its input view.
    unsafe fn input_textures(
        &self,
        source: (u32, u32),
        enumerator: &ID3D11VideoProcessorEnumerator,
    ) -> windows::core::Result<(
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11VideoProcessorInputView,
    )> {
        let upload = self
            .texture(nv12_upload(source))
            .map_err(at("CreateTexture2D(NV12 upload)"))?;
        let input = self
            .texture(nv12_input(source))
            .map_err(at("CreateTexture2D(NV12 input)"))?;
        let mut input_view = None;
        self.video_device
            .CreateVideoProcessorInputView(
                &input.cast::<ID3D11Resource>()?,
                enumerator,
                &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                },
                Some(&mut input_view),
            )
            .map_err(at("CreateVideoProcessorInputView"))?;
        let input_view = input_view.ok_or_else(windows::core::Error::from_win32)?;
        Ok((upload, input, input_view))
    }

    /// The third: the recording-sized RGBA the processor writes and the
    /// resample shader reads, as its two views.
    unsafe fn converted_textures(
        &self,
        source: (u32, u32),
        enumerator: &ID3D11VideoProcessorEnumerator,
    ) -> windows::core::Result<(ID3D11VideoProcessorOutputView, ID3D11ShaderResourceView)> {
        // Recording-sized RGBA: the blit's target and the shader's source.
        let converted = self
            .texture(rgba_converted(source))
            .map_err(at("CreateTexture2D(RGBA converted)"))?;
        let mut converted_view = None;
        self.video_device
            .CreateVideoProcessorOutputView(
                &converted.cast::<ID3D11Resource>()?,
                enumerator,
                &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                },
                Some(&mut converted_view),
            )
            .map_err(at("CreateVideoProcessorOutputView"))?;
        let converted_view = converted_view.ok_or_else(windows::core::Error::from_win32)?;
        let mut converted_resource = None;
        self.device
            .CreateShaderResourceView(&converted, None, Some(&mut converted_resource))
            .map_err(at("CreateShaderResourceView(converted)"))?;
        let converted_resource = converted_resource.ok_or_else(windows::core::Error::from_win32)?;
        Ok((converted_view, converted_resource))
    }

    /// `cbuffer Resample`: `scale` (source/target, per axis) and the source's
    /// size for the tap clamp. Four floats, which is the 16-byte minimum a D3D11
    /// constant buffer takes, so there is no padding to get wrong.
    fn resample_constants(
        &self,
        source: (u32, u32),
        target: (u32, u32),
    ) -> windows::core::Result<ID3D11Buffer> {
        let constants = [
            source.0 as f32 / target.0.max(1) as f32,
            source.1 as f32 / target.1.max(1) as f32,
            source.0 as f32,
            source.1 as f32,
        ];
        let description = D3D11_BUFFER_DESC {
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
        unsafe {
            self.device
                .CreateBuffer(&description, Some(&initial), Some(&mut buffer))?
        };
        buffer.ok_or_else(windows::core::Error::from_win32)
    }

    fn texture(&self, description: D3D11_TEXTURE2D_DESC) -> windows::core::Result<ID3D11Texture2D> {
        let mut texture = None;
        unsafe {
            self.device
                .CreateTexture2D(&description, None, Some(&mut texture))?
        };
        texture.ok_or_else(windows::core::Error::from_win32)
    }
}

/// NV12's two planes: `height` luma rows then `height / 2` interleaved chroma
/// rows, all `width` bytes wide when tightly packed.
pub(super) fn nv12_bytes((width, height): (u32, u32)) -> usize {
    width as usize * height as usize * 3 / 2
}

/// 60fps in both directions. The content description wants a frame rate and
/// nothing here is deinterlacing or rate-converting, so it only has to be
/// self-consistent.
const RATE: DXGI_RATIONAL = DXGI_RATIONAL {
    Numerator: 60,
    Denominator: 1,
};

fn rect((width, height): (u32, u32)) -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    }
}

fn nv12_upload((width, height): (u32, u32)) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: ONE_SAMPLE,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    }
}

fn nv12_input((width, height): (u32, u32)) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: ONE_SAMPLE,
        Usage: D3D11_USAGE_DEFAULT,
        // `D3D11_BIND_DECODER`, not `SHADER_RESOURCE`: the video processor's
        // input view is a decoder-family view, and a texture bound for shader
        // reads is refused with `E_INVALIDARG` (measured 2026-09-14, both with
        // and without `D3D11_USAGE_DYNAMIC`). The capture side gets away with
        // `RENDER_TARGET` on its input because that one is BGRA.
        BindFlags: D3D11_BIND_DECODER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

/// The blit's target: a render target the resample shader can also read.
fn rgba_converted((width, height): (u32, u32)) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..rgba_output((width, height))
    }
}

fn rgba_output((width, height): (u32, u32)) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: ONE_SAMPLE,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

fn rgba_staging((width, height): (u32, u32)) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: ONE_SAMPLE,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    }
}

/// Where the decoder's own NV12 is copied to be read back for
/// `PlaybackFrame::full` (t260915-e6c8). The RGBA staging texture in every
/// respect but the format and the size it is measured in -- the recording's,
/// not the stage's.
fn nv12_staging_desc((width, height): (u32, u32)) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Format: DXGI_FORMAT_NV12,
        ..rgba_staging((width, height))
    }
}

const ONE_SAMPLE: DXGI_SAMPLE_DESC = DXGI_SAMPLE_DESC {
    Count: 1,
    Quality: 0,
};

/// `Map(WRITE_DISCARD)` plus a row copy: the decoder's NV12 onto the GPU without
/// a staging round trip. The mapped pitch is the GPU's, not the source's.
unsafe fn upload_nv12(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    nv12: &[u8],
    (width, height): (u32, u32),
) -> windows::core::Result<()> {
    let resource: ID3D11Resource = texture.cast()?;
    let mut mapped = Default::default();
    context.Map(&resource, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
    let width = width as usize;
    let rows = height as usize + height as usize / 2;
    assert!(nv12.len() >= width * rows, "the NV12 planes are short");
    copy_rows(
        nv12.as_ptr(),
        width,
        mapped.pData as *mut u8,
        mapped.RowPitch as usize,
        width,
        rows,
    );
    context.Unmap(&resource, 0);
    Ok(())
}

/// The twin of [`upload_nv12`] in the other direction: a mapped NV12 staging
/// texture's rows into a tightly packed buffer (t260915-e6c8).
///
/// `planes` is resized rather than required to be the right length, because the
/// caller's spare comes from a pool that may have been filled at another size.
/// A mapped NV12 resource is `height` rows of luma followed by `height / 2`
/// rows of interleaved chroma, all at the same pitch -- exactly the layout
/// [`upload_nv12`] writes.
unsafe fn read_nv12_staging(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    planes: &mut Vec<u8>,
    (width, height): (u32, u32),
) -> windows::core::Result<()> {
    let resource: ID3D11Resource = staging.cast()?;
    let mut mapped = Default::default();
    {
        crate::insight_scope!("playback_gpu_readback_nv12_map");
        context.Map(&resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    crate::insight_scope!("playback_gpu_readback_nv12_copy");
    let width = width as usize;
    let rows = height as usize + height as usize / 2;
    planes.clear();
    planes.resize(width * rows, 0);
    copy_rows(
        mapped.pData as *const u8,
        mapped.RowPitch as usize,
        planes.as_mut_ptr(),
        width,
        width,
        rows,
    );
    context.Unmap(&resource, 0);
    Ok(())
}

/// The rendered frame into the staging texture it will be mapped from. No
/// `Map`, so nothing here waits for the GPU: this is the half of the old
/// `read_rgba` that stays on the frame's own tick (t260915-8ea7).
unsafe fn copy_to_staging(
    context: &ID3D11DeviceContext,
    output: &ID3D11Texture2D,
    staging: &ID3D11Texture2D,
) -> windows::core::Result<()> {
    let output: ID3D11Resource = output.cast()?;
    let staging_resource: ID3D11Resource = staging.cast()?;
    context.CopyResource(&staging_resource, &output);
    Ok(())
}

/// Reads cycle slot `index` back, for the blit and for `read_in_flight`
/// alike: the picture -- the shared slot's handle when the ring is up, else the
/// RGBA staging bytes into `rgba` -- and, when `planes` is given, the NV12
/// staged in the same slot beside it.
unsafe fn read_back(
    context: &ID3D11DeviceContext,
    pipeline: &Pipeline,
    fence: Option<&ShareFence>,
    index: usize,
    rgba: &mut [u8],
    planes: Option<&mut Vec<u8>>,
) -> windows::core::Result<Option<SharedPicture>> {
    let picture = match (&pipeline.share, fence) {
        (Some(ring), Some(fence)) => Some(
            ring.picture(index, pipeline.target, &fence.handle)
                .ok_or_else(|| {
                    windows::core::Error::new(
                        windows::Win32::Foundation::E_UNEXPECTED,
                        "no shared slot holds the picture read back",
                    )
                })?,
        ),
        _ => {
            read_staging(context, &pipeline.staging[index], rgba)
                .map_err(at("Map(READ) of the RGBA staging texture"))?;
            None
        }
    };
    if let Some(planes) = planes {
        read_nv12_staging(
            context,
            &pipeline.nv12_staging[index],
            planes,
            pipeline.source,
        )
        .map_err(at("Map(READ) of the NV12 staging texture"))?;
    }
    Ok(picture)
}

/// Map a staging texture and copy the rows out.
///
/// The `Map` is the wait, and which texture is handed in is the whole of this
/// task: measured 2026-09-14, 1213x682 in a dev build, it was 0.6-1.1ms against
/// 0.5-0.7ms for the row copy below, 0.2-0.9ms for the upload and ~15µs to
/// submit the blit -- the single largest item in the GPU path, and
/// `examples/gpu_readback_probe.rs` measured 0.366ms for the same readback with
/// nothing pending. That difference is the stall. Handed the texture the
/// *previous* call wrote, the GPU has had a whole tick to run that blit.
unsafe fn read_staging(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    rgba: &mut [u8],
) -> windows::core::Result<()> {
    let staging_resource: ID3D11Resource = staging.cast()?;
    let mut mapped = Default::default();
    // Wait and copy apart (t260922-c15f): the `Map` returns only once the GPU
    // has finished the blit that wrote this texture; the rows after it are CPU
    // memory traffic out of the staging heap.
    {
        crate::insight_scope!("playback_gpu_readback_map");
        context.Map(&staging_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    crate::insight_scope!("playback_gpu_readback_copy");
    let mut description = D3D11_TEXTURE2D_DESC::default();
    staging.GetDesc(&mut description);
    let row_bytes = description.Width as usize * 4;
    let rows = description.Height as usize;
    assert!(
        rgba.len() >= row_bytes * rows,
        "the RGBA destination is short"
    );
    copy_rows(
        mapped.pData as *const u8,
        mapped.RowPitch as usize,
        rgba.as_mut_ptr(),
        row_bytes,
        row_bytes,
        rows,
    );
    context.Unmap(&staging_resource, 0);
    Ok(())
}

/// `rows` rows of `width` bytes from `from` into `into`, each side at its own
/// pitch: the one row loop between a mapped D3D11 resource (the GPU's
/// `RowPitch`) and a tightly packed buffer, in either direction.
///
/// Safety: both sides must hold `rows` rows at their pitch -- the callers
/// check the packed side, and `Map` vouches for the mapped one.
pub(super) unsafe fn copy_rows(
    from: *const u8,
    from_pitch: usize,
    into: *mut u8,
    into_pitch: usize,
    width: usize,
    rows: usize,
) {
    for row in 0..rows {
        std::ptr::copy_nonoverlapping(
            from.add(row * from_pitch),
            into.add(row * into_pitch),
            width,
        );
    }
}

fn create_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    // `VIDEO_SUPPORT` (in `DEVICE_FLAGS`) since t260915-e6c8: this device is
    // also the one the Source Reader's decoder runs on, and a device created
    // without it is refused as a DXVA decode device.
    let (device, context) = crate::gpu::create_device(crate::gpu::DEVICE_FLAGS)?;
    // The decoder runs on a thread of Media Foundation's choosing while this
    // module blits from the playback worker's. Without this the two race on one
    // immediate context, and what a driver does about that is undefined rather
    // than slow.
    unsafe {
        let _ = context
            .cast::<ID3D11Multithread>()?
            .SetMultithreadProtected(true);
    }
    Ok((device, context))
}

/// The device as `IMFSourceReader` takes it (t260915-e6c8). The same three
/// calls `encoder::hardware::mft::attach_d3d11_manager` makes, without the MFT
/// half -- a Source Reader is handed the manager as an attribute rather than a
/// message.
fn create_manager(device: &ID3D11Device) -> windows::core::Result<IMFDXGIDeviceManager> {
    unsafe {
        let mut token = 0;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or_else(windows::core::Error::from_win32)?;
        manager.ResetDevice(&device.cast::<windows::core::IUnknown>()?, token)?;
        Ok(manager)
    }
}

/// Names the call that failed, keeping its `HRESULT`. Every failure in this
/// module is `E_INVALIDARG` or a device error from one of a dozen D3D11 calls,
/// and the code alone does not say which -- on a machine that is not this one,
/// the `warn!` is the whole diagnosis.
fn at(what: &'static str) -> impl FnOnce(windows::core::Error) -> windows::core::Error {
    move |error| windows::core::Error::new(error.code(), what)
}

/// slint's own D3D12 device, able to open a [`SharedPicture`] as a
/// `wgpu::Texture` for `slint::Image::try_from` (t260917-66c2).
///
/// The route t260917-bd02 chose: the D3D11 texture's NT handle opened on the
/// D3D12 device wgpu runs on, wrapped by `wgpu-hal` and handed to wgpu. The
/// opened textures are kept per handle, so steady playback opens each ring slot
/// once; holding the handle's `Arc` is what stops a closed handle's value being
/// reused under a stale entry.
pub struct StageDevice {
    device: slint::wgpu_30::wgpu::Device,
    d3d12: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: Option<(Arc<NtHandle>, ID3D12Fence)>,
    textures: Vec<(Arc<NtHandle>, slint::wgpu_30::wgpu::Texture)>,
}

impl StageDevice {
    /// `None` unless wgpu is on its DX12 backend.
    pub fn new(
        device: &slint::wgpu_30::wgpu::Device,
        queue: &slint::wgpu_30::wgpu::Queue,
    ) -> Option<Self> {
        use slint::wgpu_30::wgpu::hal::api::Dx12;
        unsafe {
            let d3d12 = foreign(device.as_hal::<Dx12>()?.raw_device())?;
            let command_queue = foreign(queue.as_hal::<Dx12>()?.as_raw())?;
            Some(Self {
                device: device.clone(),
                d3d12,
                queue: command_queue,
                fence: None,
                textures: Vec::new(),
            })
        }
    }

    /// The picture as a texture on slint's device, with slint's queue told to
    /// wait for the copy that wrote it.
    pub fn texture(
        &mut self,
        picture: &SharedPicture,
    ) -> windows::core::Result<slint::wgpu_30::wgpu::Texture> {
        use slint::wgpu_30::wgpu;
        unsafe {
            if !self
                .fence
                .as_ref()
                .is_some_and(|(handle, _)| Arc::ptr_eq(handle, &picture.fence))
            {
                let mut fence: Option<ID3D12Fence> = None;
                self.d3d12
                    .OpenSharedHandle(picture.fence.handle(), &mut fence)
                    .map_err(at("OpenSharedHandle(fence)"))?;
                let fence = fence.ok_or_else(windows::core::Error::from_win32)?;
                self.fence = Some((picture.fence.clone(), fence));
            }
            let held = self
                .textures
                .iter()
                .find(|(handle, _)| Arc::ptr_eq(handle, &picture.texture))
                .map(|(_, texture)| texture.clone());
            let texture = match held {
                Some(texture) => texture,
                None => {
                    let mut resource: Option<ID3D12Resource> = None;
                    self.d3d12
                        .OpenSharedHandle(picture.texture.handle(), &mut resource)
                        .map_err(at("OpenSharedHandle(texture)"))?;
                    let resource = resource.ok_or_else(windows::core::Error::from_win32)?;
                    let size = wgpu::Extent3d {
                        width: picture.width,
                        height: picture.height,
                        depth_or_array_layers: 1,
                    };
                    let raw = wgpu::hal::dx12::Device::texture_from_raw(
                        into_foreign(resource),
                        wgpu::TextureFormat::Rgba8Unorm,
                        wgpu::TextureDimension::D2,
                        size,
                        1,
                        1,
                    );
                    let texture = self.device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                        raw,
                        &wgpu::TextureDescriptor {
                            label: Some("liveback stage frame"),
                            size,
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: wgpu::TextureFormat::Rgba8Unorm,
                            usage: wgpu::TextureUsages::TEXTURE_BINDING
                                | wgpu::TextureUsages::RENDER_ATTACHMENT
                                | wgpu::TextureUsages::COPY_SRC,
                            view_formats: &[],
                        },
                        // COMMON, which is where an opened shared resource
                        // starts and what skia's import claims too.
                        wgpu::TextureUses::PRESENT,
                    );
                    // Two rings, oldest out first (t261025-a7cb): the review's
                    // engine stays alive under the clip screen, so switching
                    // between the two hands back slots this device opened
                    // before -- and every open costs the slot's size in
                    // dedicated memory that never returns (t260930-8bce).
                    // Clearing on one full ring re-opened all four on every
                    // switch.
                    if self.textures.len() >= STAGE_CACHE {
                        self.textures.remove(0);
                    }
                    self.textures
                        .push((picture.texture.clone(), texture.clone()));
                    texture
                }
            };
            let (_, fence) = self.fence.as_ref().expect("opened above");
            self.queue
                .Wait(fence, picture.fence_value)
                .map_err(at("ID3D12CommandQueue::Wait"))?;
            Ok(texture)
        }
    }
}

/// The same COM pointer seen through this crate's `windows` (0.61) instead of
/// the 0.62 that `wgpu-hal` is built with -- the round trip
/// `i-slint-renderer-skia`'s `dx12.rs` makes the other way. Both are one
/// `NonNull` over the same vtable.
unsafe fn foreign<F, T: Interface>(from: &F) -> Option<T> {
    assert_eq!(
        std::mem::size_of::<F>(),
        std::mem::size_of::<*mut std::ffi::c_void>()
    );
    let raw: *mut std::ffi::c_void = std::mem::transmute_copy(from);
    T::from_raw_borrowed(&raw).cloned()
}

/// [`foreign`] in the other direction, moving the reference.
unsafe fn into_foreign<T>(from: impl Interface) -> T {
    assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*mut std::ffi::c_void>()
    );
    let raw = from.into_raw();
    std::mem::transmute_copy(&raw)
}

#[cfg(test)]
mod tests;
