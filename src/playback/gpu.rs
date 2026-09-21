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

use std::sync::Once;

use crate::capture::gpu::{create_pixel_shader, create_vertex_shader, FULLSCREEN_VERTEX_SHADER};

use windows::{
    core::Interface,
    Win32::{
        Foundation::{HMODULE, RECT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
                ID3D11Multithread, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11Resource,
                ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, ID3D11VideoContext,
                ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
                ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
                D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET,
                D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
                D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD,
                D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
                D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
                D3D11_USAGE_IMMUTABLE, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VIEWPORT,
                D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
            },
            Dxgi::Common::{
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_NV12,
                DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
            },
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
}

impl StagingCycle {
    /// `(write, read)`: the index this frame's picture is copied into, and the
    /// index to map. `read` is `None` only on the first pipelined frame after a
    /// rebuild, where there is no earlier picture to hand back.
    fn advance(&mut self, readback: Readback) -> (usize, Option<usize>) {
        let write = self.write;
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
pub(super) enum Nv12Source<'a> {
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
            .map_err(|error| warn_once("playback: no D3D11 device for the GPU scaler", &error))
            .ok()?;
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|error| warn_once("playback: no ID3D11VideoDevice", &error))
            .ok()?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|error| warn_once("playback: no ID3D11VideoContext", &error))
            .ok()?;
        let (vertex_shader, resample_shader) = unsafe {
            (
                create_vertex_shader(&device, FULLSCREEN_VERTEX_SHADER),
                create_pixel_shader(&device, RESAMPLE_PIXEL_SHADER),
            )
        };
        let vertex_shader = vertex_shader
            .map_err(|error| warn_once("playback: the full-screen vertex shader", &error))
            .ok()?;
        let resample_shader = resample_shader
            .map_err(|error| warn_once("playback: the CatmullRom resample shader", &error))
            .ok()?;
        let manager = create_manager(&device)
            .map_err(|error| warn_once("playback: no DXGI device manager", &error))
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
        })
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
    /// `planes` is filled with the NV12 **of the frame `rgba` is the picture
    /// of** -- so on [`Readback::Pipelined`] both are the previous frame's, and
    /// the caller pairs them the same way it always did. That is what keeps
    /// `PlaybackFrame::full` (the recording-sized screenshot) working without
    /// the synchronous readback this change exists to avoid.
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
        if rgba.len() != target.0 as usize * target.1 as usize * 4 {
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
                warn_once(
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
        self.ensure_pipeline(source, target)?;
        // Before the shared borrow below, and after `ensure_pipeline`: a rebuilt
        // pipeline starts its cycle empty, which is what makes a resize hand the
        // caller `Warming` instead of a picture of the old size.
        let (write, read) = self
            .pipeline
            .as_mut()
            .expect("ensure_pipeline leaves one behind")
            .cycle
            .advance(readback);
        let pipeline = self
            .pipeline
            .as_ref()
            .expect("ensure_pipeline leaves one behind");
        unsafe {
            match nv12 {
                Nv12Source::Planes(bytes) => {
                    upload_nv12(&self.context, &pipeline.upload, bytes, source)
                        .map_err(at("Map(WRITE_DISCARD) of the NV12 upload texture"))?;
                    self.context.CopyResource(
                        &pipeline.input.cast::<ID3D11Resource>()?,
                        &pipeline.upload.cast::<ID3D11Resource>()?,
                    );
                }
                Nv12Source::Texture(texture, slice) => {
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
            self.resample(pipeline, target);
            copy_to_staging(&self.context, &pipeline.output, &pipeline.staging[write])
                .map_err(at("CopyResource into the RGBA staging texture"))?;
            if planes.is_some() {
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
            match read {
                Some(index) => {
                    read_staging(&self.context, &pipeline.staging[index], rgba)
                        .map_err(at("Map(READ) of the RGBA staging texture"))?;
                    if let Some(planes) = planes {
                        read_nv12_staging(
                            &self.context,
                            &pipeline.nv12_staging[index],
                            planes,
                            source,
                        )
                        .map_err(at("Map(READ) of the NV12 staging texture"))?;
                    }
                    Ok(true)
                }
                None => Ok(false),
            }
        }
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
        let read = if rgba.len() == pipeline.target.0 as usize * pipeline.target.1 as usize * 4 {
            let index = 1 - pipeline.cycle.write;
            unsafe { read_staging(&self.context, &pipeline.staging[index], rgba) }
                .and_then(|()| match planes {
                    // The hardware-decoder path holds no planes of its own
                    // (t260915-e6c8), so the frame this hands back would be
                    // published with an empty `full` unless its NV12 comes out
                    // of the same slot the picture just did.
                    Some(planes) => unsafe {
                        read_nv12_staging(
                            &self.context,
                            &pipeline.nv12_staging[index],
                            planes,
                            pipeline.source,
                        )
                    },
                    None => Ok(()),
                })
                .map_err(|error| warn_once("playback: reading back the frame in flight", &error))
                .is_ok()
        } else {
            false
        };
        self.discard_in_flight();
        read
    }

    fn ensure_pipeline(
        &mut self,
        source: (u32, u32),
        target: (u32, u32),
    ) -> windows::core::Result<()> {
        if self
            .pipeline
            .as_ref()
            .is_some_and(|held| held.source == source && held.target == target)
        {
            return Ok(());
        }
        self.pipeline = None;
        self.pipeline = Some(self.build_pipeline(source, target)?);
        Ok(())
    }

    fn build_pipeline(
        &self,
        source: (u32, u32),
        target: (u32, u32),
    ) -> windows::core::Result<Pipeline> {
        unsafe {
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
                    &enumerator,
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

            // Recording-sized RGBA: the blit's target and the shader's source.
            let converted = self
                .texture(rgba_converted(source))
                .map_err(at("CreateTexture2D(RGBA converted)"))?;
            let mut converted_view = None;
            self.video_device
                .CreateVideoProcessorOutputView(
                    &converted.cast::<ID3D11Resource>()?,
                    &enumerator,
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
            let converted_resource =
                converted_resource.ok_or_else(windows::core::Error::from_win32)?;

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
            })
        }
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
    for row in 0..rows {
        let from = &nv12[row * width..][..width];
        let into = (mapped.pData as *mut u8).add(row * mapped.RowPitch as usize);
        std::ptr::copy_nonoverlapping(from.as_ptr(), into, width);
    }
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
    context.Map(&resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    let width = width as usize;
    let rows = height as usize + height as usize / 2;
    planes.clear();
    planes.resize(width * rows, 0);
    for row in 0..rows {
        let from = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
        let into = &mut planes[row * width..][..width];
        std::ptr::copy_nonoverlapping(from, into.as_mut_ptr(), width);
    }
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
    context.Map(&staging_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    let mut description = D3D11_TEXTURE2D_DESC::default();
    staging.GetDesc(&mut description);
    let row_bytes = description.Width as usize * 4;
    for row in 0..description.Height as usize {
        let from = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
        let into = &mut rgba[row * row_bytes..][..row_bytes];
        std::ptr::copy_nonoverlapping(from, into.as_mut_ptr(), row_bytes);
    }
    context.Unmap(&staging_resource, 0);
    Ok(())
}

fn create_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            // `VIDEO_SUPPORT` since t260915-e6c8: this device is now also the
            // one the Source Reader's decoder runs on, and a device created
            // without it is refused as a DXVA decode device.
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or_else(windows::core::Error::from_win32)?;
    let context = context.ok_or_else(windows::core::Error::from_win32)?;
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

/// Said once per process. Every one of these failures is a property of the
/// machine, and this path runs on the frame deadline.
fn warn_once(what: &str, error: &windows::core::Error) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| tracing::warn!(event = "playback_gpu_scale_unavailable", %error, "{what}"));
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        nv12_bytes, Converted, GpuScaler, ID3D11Resource, ID3D11Texture2D, Interface, Nv12Source,
        Readback, StagingCycle, ONE_SAMPLE,
    };
    use livia_pixels::{nv12_to_rgba, FrameScaler};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

    /// A source frame whose luma is a diagonal gradient and whose chroma sweeps
    /// the other way: every byte of both planes is exercised, so a wrong colour
    /// matrix or a swapped U/V shows up as a large mean difference rather than
    /// hiding in a flat field.
    fn gradient(width: u32, height: u32) -> Vec<u8> {
        let mut nv12 = vec![0u8; nv12_bytes((width, height))];
        let (width, height) = (width as usize, height as usize);
        for y in 0..height {
            for x in 0..width {
                // Studio range: keep the luma inside 16..=235 so the comparison
                // is about the conversion, not about how each side clamps.
                nv12[y * width + x] = 16 + (((x + y) * 219 / (width + height)) as u8);
            }
        }
        let uv = width * height;
        for row in 0..height / 2 {
            for x in 0..width / 2 {
                nv12[uv + row * width + x * 2] = (x * 255 / (width / 2).max(1)) as u8;
                nv12[uv + row * width + x * 2 + 1] = (row * 255 / (height / 2).max(1)) as u8;
            }
        }
        nv12
    }

    /// Alternating black and white rows -- the hardest thing to minify, and the
    /// content task990's CatmullRom pass exists for.
    fn stripes(width: u32, height: u32) -> Vec<u8> {
        let mut nv12 = vec![128u8; nv12_bytes((width, height))];
        let (width, height) = (width as usize, height as usize);
        for y in 0..height {
            let luma = if y % 2 == 0 { 16 } else { 235 };
            nv12[y * width..][..width].fill(luma);
        }
        nv12
    }

    /// The blit itself rather than `convert_and_scale`, so a driver refusing
    /// something reports its own `HRESULT` here instead of arriving as a bare
    /// `false` and a `warn!` nobody in a test run reads.
    ///
    /// `Readback::Now` because these tests compare *this* frame's pixels: the
    /// pipelined path answers with the frame before, which is what the
    /// `t260915_8ea7` tests below are for.
    fn scale(gpu: &mut GpuScaler, nv12: &[u8], source: (u32, u32), target: (u32, u32)) -> Vec<u8> {
        let mut rgba = vec![0u8; target.0 as usize * target.1 as usize * 4];
        assert!(
            gpu.blit(
                Nv12Source::Planes(nv12),
                source,
                target,
                &mut rgba,
                None,
                Readback::Now
            )
            .expect("the video processor converted and scaled the frame"),
            "an immediate readback always writes the destination"
        );
        rgba
    }

    /// The decoder's own shape: an NV12 surface **taller than the frame**, with
    /// the frame in its top-left corner. 1088 rows for a 1080-row frame is the
    /// ordinary case, and it is the reason the texture path copies a box rather
    /// than the whole resource.
    ///
    /// Staging rather than `UpdateSubresource`: a planar `DEFAULT` texture has
    /// no single system-memory layout to hand over, and a staging texture is a
    /// legal `CopySubresourceRegion` source -- which is the call under test.
    fn padded_nv12(
        gpu: &GpuScaler,
        nv12: &[u8],
        size: (u32, u32),
        padded_height: u32,
    ) -> ID3D11Texture2D {
        padded_nv12_array(gpu, &[nv12], size, padded_height)
    }

    /// [`padded_nv12`] as a *texture array*, one frame per slice -- the other
    /// half of the decoder's shape (t260917-aca5): the decoder hands over
    /// `(texture, slice)`, and the slice is the `SrcSubresource` of the copy.
    ///
    /// With one mip level a staging texture's subresource index *is* its array
    /// slice, so `Map(slice)` writes exactly the surface a
    /// `CopySubresourceRegion(.., slice, ..)` reads.
    fn padded_nv12_array(
        gpu: &GpuScaler,
        slices: &[&[u8]],
        (width, height): (u32, u32),
        padded_height: u32,
    ) -> ID3D11Texture2D {
        let texture = gpu
            .texture(D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: padded_height,
                MipLevels: 1,
                ArraySize: slices.len() as u32,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: ONE_SAMPLE,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: 0,
            })
            .expect("a padded NV12 staging texture");
        let resource: ID3D11Resource = texture.cast().expect("the texture as a resource");
        for (slice, nv12) in slices.iter().enumerate() {
            write_padded_slice(
                gpu,
                &resource,
                slice as u32,
                nv12,
                (width, height),
                padded_height,
            );
        }
        texture
    }

    fn write_padded_slice(
        gpu: &GpuScaler,
        resource: &ID3D11Resource,
        slice: u32,
        nv12: &[u8],
        (width, height): (u32, u32),
        padded_height: u32,
    ) {
        unsafe {
            let mut mapped = Default::default();
            gpu.context
                .Map(resource, slice, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .expect("Map(WRITE) of the fixture surface");
            let row_bytes = width as usize;
            for row in 0..height as usize {
                let from = &nv12[row * row_bytes..][..row_bytes];
                let into = (mapped.pData as *mut u8).add(row * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(from.as_ptr(), into, row_bytes);
            }
            // The chroma plane starts at the *surface's* height, not the
            // frame's. That gap is exactly the padding this fixture exists to
            // put in the way.
            for row in 0..height as usize / 2 {
                let from = &nv12[(height as usize + row) * row_bytes..][..row_bytes];
                let into = (mapped.pData as *mut u8)
                    .add((padded_height as usize + row) * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(from.as_ptr(), into, row_bytes);
            }
            gpu.context.Unmap(resource, slice);
        }
    }

    /// t260915-e6c8: a frame that never left the GPU has to come out the same
    /// as the same frame handed over as bytes, and its NV12 has to come back
    /// for `PlaybackFrame::full`.
    ///
    /// Both failures this guards against are silent. Copying the whole resource
    /// instead of the frame's rectangle puts the padding's rows into the
    /// picture (and on a size mismatch `CopyResource` does nothing at all,
    /// leaving the previous frame on the stage); reading the NV12 back at the
    /// wrong plane offset gives a picture-perfect frame whose screenshot is
    /// green. The planes path over the same bytes is the control for the first,
    /// and the fixture's own bytes for the second.
    #[test]
    fn a_decoder_texture_blits_to_the_same_picture_as_its_planes() {
        let Some(mut gpu) = GpuScaler::new() else {
            return;
        };
        let source = (64u32, 48u32);
        let target = (40u32, 30u32);
        let nv12 = stripes(source.0, source.1);
        let from_planes = scale(&mut gpu, &nv12, source, target);

        // The decoy is the whole reason the asserts below can fail. A copy that
        // does nothing -- which is what `CopyResource` does on a size mismatch,
        // silently -- leaves the pipeline's input texture holding whatever the
        // last blit put there, so measuring the texture path against a control
        // that *also* wrote those exact bytes would pass either way. Measured:
        // with this line removed, replacing the frame's box with a
        // whole-resource copy still passed.
        let decoy = gradient(source.0, source.1);
        let _ = scale(&mut gpu, &decoy, source, target);

        let texture = padded_nv12(&gpu, &nv12, source, source.1 + 8);
        let mut rgba = vec![0u8; target.0 as usize * target.1 as usize * 4];
        let mut planes = Vec::new();
        assert!(
            gpu.blit(
                Nv12Source::Texture(&texture, 0),
                source,
                target,
                &mut rgba,
                Some(&mut planes),
                Readback::Now,
            )
            .expect("the video processor took the decoder's texture"),
            "an immediate readback always writes the destination"
        );

        let (worst, average) = difference(&from_planes, &rgba);
        assert!(
            worst <= 1,
            "the texture path drew a different picture from the planes path \
             (worst {worst}, average {average:.3})"
        );
        assert_eq!(
            planes, nv12,
            "the NV12 read back for `PlaybackFrame::full` is not the frame that went in"
        );
    }

    /// t260917-aca5: the slice index a decoder hands over is the copy's
    /// `SrcSubresource`, and until this test every caller that exercised it
    /// passed `0` -- where a copy that ignores the index is indistinguishable
    /// from one that reads it.
    ///
    /// So the frame sits in slice 1 and a *different* picture (the decoy) in
    /// slice 0. Ignoring the index draws the decoy, which is also why the
    /// pipeline's input is primed with the decoy's bytes first: a copy that did
    /// nothing at all must not pass either. The `assert_ne!` against the decoy
    /// is the proof proper -- the other two could in principle pass on a
    /// fixture that put the same picture in both slices.
    #[test]
    fn a_non_zero_array_slice_blits_the_picture_from_that_slice() {
        let Some(mut gpu) = GpuScaler::new() else {
            eprintln!("no D3D11 video device on this machine; the slice index is untested");
            return;
        };
        let source = (64u32, 48u32);
        let target = (40u32, 30u32);
        let frame = stripes(source.0, source.1);
        let decoy = gradient(source.0, source.1);
        let from_decoy = scale(&mut gpu, &decoy, source, target);
        let from_planes = scale(&mut gpu, &frame, source, target);
        // Leave the decoy, not the frame, in the pipeline's input.
        let _ = scale(&mut gpu, &decoy, source, target);

        let texture = padded_nv12_array(&gpu, &[&decoy, &frame], source, source.1 + 8);
        let mut rgba = vec![0u8; target.0 as usize * target.1 as usize * 4];
        let mut planes = Vec::new();
        assert!(
            gpu.blit(
                Nv12Source::Texture(&texture, 1),
                source,
                target,
                &mut rgba,
                Some(&mut planes),
                Readback::Now,
            )
            .expect("the video processor took slice 1 of the texture array"),
            "an immediate readback always writes the destination"
        );

        assert_ne!(
            rgba, from_decoy,
            "slice 1 drew slice 0's picture: the copy is not reading the slice it was given"
        );
        let (worst, average) = difference(&from_planes, &rgba);
        assert!(
            worst <= 1,
            "slice 1 drew a different picture from the same frame's planes \
             (worst {worst}, average {average:.3})"
        );
        assert_eq!(
            planes, frame,
            "the NV12 read back out of slice 1 is not the frame that went into slice 1"
        );
    }

    /// The pipelined half of the texture path (t260915-e6c8), which is the
    /// whole reason the NV12 staging is a *pair* rather than one texture.
    ///
    /// `Readback::Now` -- what the test above uses -- cannot see this at all:
    /// there `read == write`, so reading the wrong slot is indistinguishable
    /// from reading the right one. Production runs `Pipelined`, where the
    /// planes have to come out of the slot the picture comes out of, which is
    /// the frame *before* the one being handed in.
    ///
    /// The decoy is load-bearing again: the second blit's own input is
    /// `second`, so a texture copy that did nothing would leave `second` in the
    /// pipeline's input and the readback would quietly be `second` rather than
    /// `first`.
    #[test]
    fn the_pipelined_texture_readback_hands_back_the_previous_frames_planes() {
        let Some(mut gpu) = GpuScaler::new() else {
            return;
        };
        let source = (64u32, 48u32);
        let target = (40u32, 30u32);
        let first = stripes(source.0, source.1);
        let second = gradient(source.0, source.1);
        let expected = scale(&mut gpu, &first, source, target);
        // Leaves `second` in the pipeline's input, and (being `Readback::Now`)
        // leaves the cycle empty -- so the blit below is a first pipelined
        // frame again.
        let _ = scale(&mut gpu, &second, source, target);

        let first_texture = padded_nv12(&gpu, &first, source, source.1 + 8);
        let second_texture = padded_nv12(&gpu, &second, source, source.1 + 8);
        let mut rgba = vec![0u8; target.0 as usize * target.1 as usize * 4];
        let mut planes = Vec::new();

        assert!(
            !gpu.blit(
                Nv12Source::Texture(&first_texture, 0),
                source,
                target,
                &mut rgba,
                Some(&mut planes),
                Readback::Pipelined,
            )
            .expect("the first pipelined blit is submitted"),
            "the first pipelined frame after a rebuild has nothing to read back"
        );
        assert!(
            planes.is_empty(),
            "nothing was read back, so nothing should have been written to the planes"
        );

        assert!(
            gpu.blit(
                Nv12Source::Texture(&second_texture, 0),
                source,
                target,
                &mut rgba,
                Some(&mut planes),
                Readback::Pipelined,
            )
            .expect("the second pipelined blit reads the first one back"),
            "the second pipelined frame hands back the first"
        );
        let (worst, average) = difference(&expected, &rgba);
        assert!(
            worst <= 1,
            "the pipelined readback handed back the wrong frame's picture \
             (worst {worst}, average {average:.3})"
        );
        assert_eq!(
            planes, first,
            "the pipelined readback handed back the wrong frame's planes: they have to come out \
             of the slot the picture came out of"
        );
    }

    fn difference(left: &[u8], right: &[u8]) -> (u32, f64) {
        let mut worst = 0u32;
        let mut total = 0u64;
        for (a, b) in left.iter().zip(right) {
            let delta = u32::from(a.abs_diff(*b));
            worst = worst.max(delta);
            total += u64::from(delta);
        }
        (worst, total as f64 / left.len() as f64)
    }

    /// AC3. The GPU's picture against the CPU's, in numbers rather than by eye:
    /// the same NV12 through `nv12_to_rgba` + `FrameScaler` and through the
    /// video processor, compared pixel by pixel.
    ///
    /// The two are not required to be identical -- the driver picks its own
    /// minification filter, which is the point of using it -- but a wrong colour
    /// space or a swapped plane moves the *mean* by tens of counts, and that is
    /// what this catches. Skipped, loudly, where there is no device.
    #[test]
    fn the_gpu_picture_matches_the_cpu_one() {
        let Some(mut gpu) = GpuScaler::new() else {
            eprintln!("no D3D11 video device on this machine; GPU scaling untested");
            return;
        };
        // Even on both axes: an NV12 texture cannot be anything else.
        let source = (640, 360);
        let target = (320, 180);
        let nv12 = gradient(source.0, source.1);
        let cpu = FrameScaler::default()
            .downscale(
                &nv12_to_rgba(&nv12, source.0, source.1, source.0),
                source,
                target,
                None,
            )
            .expect("the CPU scaler handles a 0.5x downscale");
        let actual = scale(&mut gpu, &nv12, source, target);
        let (worst, mean) = difference(&cpu, &actual);
        eprintln!("GPU vs CPU picture: max {worst}, mean {mean:.3}");
        // Generous on the max -- two different resampling filters disagree most
        // at an edge -- and tight on the mean, which is where a colour-space
        // mistake lands. A full-range read of studio-range input moves the mean
        // by ~9 counts.
        assert!(
            mean < 3.0,
            "GPU and CPU pictures differ by mean {mean:.2} (max {worst}); \
             a mean this large is a colour-space mistake, not a filter difference"
        );
    }

    /// The other half of AC3: whether the filter the driver picks is a real
    /// resampler or a plain bilinear tap, at the ratio windowed playback
    /// actually uses.
    ///
    /// **The ratio is the whole test.** At exactly 0.5x every filter that touches
    /// both rows answers 128, so a halved image proves nothing. 64 -> 40 is
    /// 0.625x, next to the 0.63x Context names for a ~1200px stage on a 1922px
    /// recording: 1.6 input rows per output row, so a filter whose kernel is
    /// scaled to the ratio still averages ~6 rows and lands near mid-grey, while
    /// bilinear samples two adjacent rows at a phase that walks and comes back
    /// oscillating between the extremes.
    ///
    /// The CPU's own CatmullRom runs in the same test as the positive control:
    /// "near mid-grey" only means anything next to what the filter this replaces
    /// answers on the same input.
    #[test]
    fn the_gpu_downscale_resamples_rather_than_sampling_two_rows() {
        let Some(mut gpu) = GpuScaler::new() else {
            eprintln!("no D3D11 video device on this machine; GPU scaling untested");
            return;
        };
        // 0.625x, and one below 0.5x so the tap loop runs with `scale > 2` --
        // the bound it computes is the part a fixed-width kernel gets wrong.
        for (source, target) in [((64, 64), (40, 40)), ((100, 100), (40, 40))] {
            let nv12 = stripes(source.0, source.1);
            let cpu = FrameScaler::default()
                .downscale(
                    &nv12_to_rgba(&nv12, source.0, source.1, source.0),
                    source,
                    target,
                    None,
                )
                .expect("the CPU scaler handles the downscale");
            let gpu_rgba = scale(&mut gpu, &nv12, source, target);
            // The interior only: the edge rows of any convolution see the clamp.
            let row_bytes = target.0 as usize * 4;
            let interior = |rgba: &[u8]| {
                rgba[row_bytes * 4..row_bytes * (target.1 as usize - 4)]
                    .chunks_exact(4)
                    .map(|pixel| u32::from(pixel[0]).abs_diff(128))
                    .max()
                    .expect("the stage has pixels")
            };
            let (cpu_worst, gpu_worst) = (interior(&cpu), interior(&gpu_rgba));
            let ratio = f64::from(target.0) / f64::from(source.0);
            eprintln!(
                "{ratio:.3}x black/white stripes, counts from mid-grey: \
                 CPU CatmullRom {cpu_worst}, GPU {gpu_worst}"
            );
            // Bilinear on this input swings most of the way to the extremes: the
            // video processor's own scaler measured 102 here before the shader
            // replaced it. The bound is set against that failure, not against the
            // CPU's number, so a filter that merely differs still passes.
            assert!(
                gpu_worst < 64,
                "{ratio:.3}x black/white stripes came back {gpu_worst} counts from mid-grey \
                 (the CPU's CatmullRom: {cpu_worst}); the resample is sampling rows rather \
                 than resampling them, which is the softness task990 removed"
            );
        }
    }

    /// AC1/AC2's component measurement, off the app: the same NV12 frame
    /// through the CPU pair (`nv12_to_rgba` + `FrameScaler`, both compiled at
    /// `opt-level = 2` even in a dev build -- see the workspace Cargo.toml) and
    /// through the blit, **interleaved** so a busy machine cannot be mistaken
    /// for a difference between the two.
    ///
    /// The numbers printed here are the ones the task's before/after is built
    /// on; what `--insight` adds on top is the rest of the tick.
    #[test]
    fn the_gpu_pair_costs_less_than_the_cpu_pair() {
        let Some(mut gpu) = GpuScaler::new() else {
            eprintln!("no D3D11 video device on this machine; GPU scaling unmeasured");
            return;
        };
        // The user's recording into a windowed stage, and the 720p bench
        // source into the same stage -- the two the task's table is about.
        for (source, target) in [((1922, 1112), (1213, 682)), ((1280, 720), (1213, 682))] {
            let nv12 = gradient(source.0, source.1);
            let mut scaler = FrameScaler::default();
            let mut gpu_rgba = vec![0u8; target.0 as usize * target.1 as usize * 4];
            let mut cpu_total = Duration::ZERO;
            let mut gpu_total = Duration::ZERO;
            const ROUNDS: u32 = 20;
            // One of each untimed: the first blit builds the pipeline and the
            // first resize grows the convolution buffers.
            let _ = gpu.convert_and_scale(&nv12, source, target, &mut gpu_rgba, Readback::Now);
            let mut spare = scaler.downscale(
                &nv12_to_rgba(&nv12, source.0, source.1, source.0),
                source,
                target,
                None,
            );
            for _ in 0..ROUNDS {
                let started = Instant::now();
                let rgba = nv12_to_rgba(&nv12, source.0, source.1, source.0);
                spare = scaler.downscale(&rgba, source, target, spare);
                cpu_total += started.elapsed();
                let started = Instant::now();
                // The stalling readback, which is the pair the CPU one is
                // comparable with: both numbers are one whole frame in and one
                // whole frame out.
                assert_eq!(
                    gpu.convert_and_scale(&nv12, source, target, &mut gpu_rgba, Readback::Now),
                    Converted::This
                );
                gpu_total += started.elapsed();
            }
            let cpu = cpu_total.as_secs_f64() * 1000.0 / f64::from(ROUNDS);
            let gpu_ms = gpu_total.as_secs_f64() * 1000.0 / f64::from(ROUNDS);
            eprintln!(
                "{}x{} -> {}x{}: CPU convert+scale {cpu:.3} ms, GPU {gpu_ms:.3} ms",
                source.0, source.1, target.0, target.1
            );
            assert!(
                gpu_ms < cpu,
                "the GPU pair ({gpu_ms:.3} ms) is not cheaper than the CPU pair ({cpu:.3} ms) \
                 at {}x{} -> {}x{}",
                source.0,
                source.1,
                target.0,
                target.1
            );
        }
    }

    /// A destination of the wrong size is refused rather than half-written --
    /// the caller falls back to the CPU pair, which is AC4's mechanism.
    #[test]
    fn a_mismatched_destination_is_refused() {
        let Some(mut gpu) = GpuScaler::new() else {
            eprintln!("no D3D11 video device on this machine; GPU scaling untested");
            return;
        };
        let source = (64, 64);
        let nv12 = stripes(source.0, source.1);
        let mut too_small = vec![0u8; 32 * 32 * 4 - 4];
        assert_eq!(
            gpu.convert_and_scale(&nv12, source, (32, 32), &mut too_small, Readback::Pipelined),
            Converted::Failed
        );
        let mut short_source = vec![0u8; 32 * 32 * 4];
        assert_eq!(
            gpu.convert_and_scale(
                &nv12[..8],
                source,
                (32, 32),
                &mut short_source,
                Readback::Pipelined
            ),
            Converted::Failed
        );
    }

    /// t260915-8ea7's state machine, without a device: which texture is written,
    /// which is read, and what a rebuild resets.
    #[test]
    fn the_staging_cycle_reads_the_texture_the_previous_frame_wrote() {
        let mut cycle = StagingCycle::default();
        // The first pipelined frame has nothing behind it.
        assert_eq!(cycle.advance(Readback::Pipelined), (0, None));
        // From then on every frame reads the one before it, and never writes
        // into a texture that has not been read.
        assert_eq!(cycle.advance(Readback::Pipelined), (1, Some(0)));
        assert_eq!(cycle.advance(Readback::Pipelined), (0, Some(1)));
        assert_eq!(cycle.advance(Readback::Pipelined), (1, Some(0)));
        // A rebuilt pipeline is a fresh cycle -- the frame it had in flight was
        // the old size, and the first frame after it is a first frame again.
        assert_eq!(
            StagingCycle::default().advance(Readback::Pipelined),
            (0, None)
        );
    }

    /// The caller's drop points (a seek, a park, a crossing) leave the pipeline
    /// standing, so the cycle has to be emptied explicitly -- otherwise the next
    /// frame is handed a picture whose planes the caller has already recycled.
    #[test]
    fn an_abandoned_frame_makes_the_next_one_a_first_frame() {
        let mut cycle = StagingCycle::default();
        assert_eq!(cycle.advance(Readback::Pipelined), (0, None));
        assert_eq!(cycle.advance(Readback::Pipelined), (1, Some(0)));
        cycle.abandon();
        assert_eq!(
            cycle.advance(Readback::Pipelined),
            (0, None),
            "the frame in flight was dropped, so there is nothing to read back"
        );
        // And the cycle keeps working afterwards rather than warming forever.
        assert_eq!(cycle.advance(Readback::Pipelined), (1, Some(0)));
    }

    /// The seek poster's readback: this frame's own texture, and nothing left
    /// in flight for the next call to pair with the wrong planes.
    #[test]
    fn an_immediate_readback_maps_what_it_just_wrote_and_leaves_nothing_behind() {
        let mut cycle = StagingCycle::default();
        let (write, read) = cycle.advance(Readback::Now);
        assert_eq!(read, Some(write));
        assert_eq!(cycle.advance(Readback::Pipelined), (1, None));
        // And it abandons a frame that was in flight rather than handing it
        // back late: a seek's poster is read on the spot, and the picture from
        // before the seek is exactly what must not be published.
        let mut cycle = StagingCycle::default();
        assert_eq!(cycle.advance(Readback::Pipelined), (0, None));
        let (write, read) = cycle.advance(Readback::Now);
        assert_eq!((write, read), (1, Some(1)));
        assert_eq!(cycle.advance(Readback::Pipelined), (0, None));
    }

    /// The pipeline end to end on a real device: the second pipelined call hands
    /// back the **first** frame's picture, not its own.
    ///
    /// The two frames are deliberately different pictures (a gradient and
    /// stripes), and the control is the same frame through `Readback::Now` in
    /// the same run -- "it matches A" only means something next to "it does not
    /// match B".
    #[test]
    fn the_pipelined_readback_hands_back_the_previous_frame() {
        let Some(mut gpu) = GpuScaler::new() else {
            eprintln!("no D3D11 video device on this machine; GPU scaling untested");
            return;
        };
        let source = (64, 64);
        let target = (40, 40);
        let (first, second) = (gradient(source.0, source.1), stripes(source.0, source.1));
        let immediate_first = scale(&mut gpu, &first, source, target);
        let immediate_second = scale(&mut gpu, &second, source, target);
        assert_ne!(
            immediate_first, immediate_second,
            "the two frames have to differ for this test to be able to fail"
        );
        let mut rgba = vec![0u8; target.0 as usize * target.1 as usize * 4];
        // `Readback::Now` above left the cycle empty, so this is a first frame.
        assert_eq!(
            gpu.convert_and_scale(&first, source, target, &mut rgba, Readback::Pipelined),
            Converted::Warming,
            "the first pipelined frame has no earlier picture to hand back"
        );
        assert!(
            rgba.iter().all(|byte| *byte == 0),
            "a warming call must not write the destination"
        );
        assert_eq!(
            gpu.convert_and_scale(&second, source, target, &mut rgba, Readback::Pipelined),
            Converted::Previous
        );
        let (worst, mean) = difference(&immediate_first, &rgba);
        assert_eq!(
            worst, 0,
            "the pipelined readback handed back something other than the previous frame \
             (mean difference {mean:.3} from it)"
        );
        // The crossing / seek / park case, end to end: the caller drops the
        // frame in flight and the next one must not be told about it.
        gpu.discard_in_flight();
        assert_eq!(
            gpu.convert_and_scale(&first, source, target, &mut rgba, Readback::Pipelined),
            Converted::Warming,
            "a discarded frame must not come back on the next call; the caller has \
             already recycled the planes it would have to be published with"
        );
        assert_eq!(
            gpu.convert_and_scale(&second, source, target, &mut rgba, Readback::Pipelined),
            Converted::Previous,
            "and the pipeline keeps running rather than warming forever"
        );
    }

    /// `pause()`'s half of the same cycle: the picture in flight comes back
    /// without a new blit, and comes back once (t260915-7088).
    ///
    /// Same controls as the test above -- two deliberately different pictures,
    /// each rendered immediately for comparison -- because "it wrote something"
    /// is not "it wrote B". The negative half (a second call, and a scaler that
    /// never built a pipeline) sits in the same test next to the positive: an
    /// assert that nothing came back passes for free on a harness that never
    /// ran.
    #[test]
    fn reading_the_frame_in_flight_hands_it_back_once() {
        let Some(mut gpu) = GpuScaler::new() else {
            eprintln!("no D3D11 video device on this machine; GPU scaling untested");
            return;
        };
        let source = (64, 64);
        let target = (40, 40);
        let bytes = target.0 as usize * target.1 as usize * 4;
        let (first, second) = (gradient(source.0, source.1), stripes(source.0, source.1));
        let immediate_first = scale(&mut gpu, &first, source, target);
        let immediate_second = scale(&mut gpu, &second, source, target);
        assert_ne!(
            immediate_first, immediate_second,
            "the two frames have to differ for this test to be able to fail"
        );

        // The state a paused tick leaves behind: A handed out, B in flight.
        let mut handed_out = vec![0u8; bytes];
        assert_eq!(
            gpu.convert_and_scale(&first, source, target, &mut handed_out, Readback::Pipelined),
            Converted::Warming
        );
        assert_eq!(
            gpu.convert_and_scale(
                &second,
                source,
                target,
                &mut handed_out,
                Readback::Pipelined
            ),
            Converted::Previous
        );
        assert_eq!(
            difference(&immediate_first, &handed_out).0,
            0,
            "the setup did not leave B in flight"
        );

        let mut flushed = vec![0u8; bytes];
        assert!(
            gpu.read_in_flight(&mut flushed, None),
            "B's blit is in flight and has to be readable without submitting another"
        );
        let (worst, mean) = difference(&immediate_second, &flushed);
        assert_eq!(
            worst, 0,
            "the flush handed back something other than the frame in flight \
             (mean difference {mean:.3} from it)"
        );
        assert_ne!(
            difference(&immediate_first, &flushed).0,
            0,
            "and it is not A, the picture already handed out"
        );

        // Once: the caller paired the planes with it on the first call, so a
        // second picture would have nothing to come out with.
        let sentinel = vec![0x5au8; bytes];
        let mut again = sentinel.clone();
        assert!(
            !gpu.read_in_flight(&mut again, None),
            "the read emptied the cycle, so there is nothing left in flight"
        );
        assert_eq!(
            again, sentinel,
            "a refused read must not write the destination"
        );
        assert_eq!(
            gpu.convert_and_scale(&first, source, target, &mut handed_out, Readback::Pipelined),
            Converted::Warming,
            "and the next pipelined frame is a first frame again"
        );

        // A scaler that has never blitted has no pipeline to read from.
        let mut fresh = GpuScaler::new().expect("a second scaler on a machine that gave one");
        assert!(!fresh.read_in_flight(&mut again, None));
        assert_eq!(again, sentinel);
    }
}
