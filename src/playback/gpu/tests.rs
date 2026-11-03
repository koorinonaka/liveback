use std::time::{Duration, Instant};

use super::{
    nv12_bytes, settle, Converted, GpuScaler, ID3D11Resource, ID3D11Texture2D, Interface,
    Nv12Source, Readback, RingCycle, StageDevice, StagingCycle, ONE_SAMPLE, SHARE_RING,
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
        let pitch = mapped.RowPitch as usize;
        let luma_rows = height as usize;
        assert!(nv12.len() >= row_bytes * (luma_rows + luma_rows / 2));
        let into = mapped.pData as *mut u8;
        super::copy_rows(nv12.as_ptr(), row_bytes, into, pitch, row_bytes, luma_rows);
        // The chroma plane starts at the *surface's* height, not the
        // frame's. That gap is exactly the padding this fixture exists to
        // put in the way.
        super::copy_rows(
            nv12[luma_rows * row_bytes..].as_ptr(),
            row_bytes,
            into.add(padded_height as usize * pitch),
            pitch,
            row_bytes,
            luma_rows / 2,
        );
        gpu.context.Unmap(resource, slice);
    }
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
/// planes are no longer read per frame (t261016-97b7) but, when asked for,
/// have to come out of the slot the picture came out of, which is the
/// frame *before* the one being handed in.
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
    // t261016-97b7: playing, the planes are not read back per frame.
    assert!(
        planes.is_empty(),
        "a pipelined blit mapped the NV12 staging: that is the per-frame readback removed"
    );
    // The picture handed back is `first`, so asking now gets `first`'s
    // planes -- not `second`'s, which sit in the other slot in flight.
    let mut published = Vec::new();
    assert_eq!(gpu.read_published_nv12(&mut published), Some(source));
    assert_eq!(
        published, first,
        "the planes asked for are not the frame whose picture was handed back"
    );

    // The ring hazard: the next blit overwrites `first`'s slot with a third
    // frame (`first` again, so reading the in-flight slot now gives the
    // wrong answer `first`). The published picture is `second`'s by then,
    // and so must the planes be.
    assert!(gpu
        .blit(
            Nv12Source::Texture(&first_texture, 0),
            source,
            target,
            &mut rgba,
            Some(&mut planes),
            Readback::Pipelined,
        )
        .expect("the third pipelined blit reads the second one back"));
    assert_eq!(gpu.read_published_nv12(&mut published), Some(source));
    assert_eq!(
        published, second,
        "after the next blit the planes asked for have to follow the picture handed back"
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
        // Compared by the per-round minimum: load only ever pushes a round
        // up, so one round the scheduler evicted (or another test holding
        // the device) lands whole in a mean but never in a minimum.
        // t260925-421c: the mean went red 5/25 under a parallel `cargo test`.
        let (mut cpu_total, mut cpu_min) = (Duration::ZERO, Duration::MAX);
        let (mut gpu_total, mut gpu_min) = (Duration::ZERO, Duration::MAX);
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
            let cpu_round = started.elapsed();
            cpu_total += cpu_round;
            cpu_min = cpu_min.min(cpu_round);
            let started = Instant::now();
            // The stalling readback, which is the pair the CPU one is
            // comparable with: both numbers are one whole frame in and one
            // whole frame out.
            assert_eq!(
                gpu.convert_and_scale(&nv12, source, target, &mut gpu_rgba, Readback::Now),
                Converted::This
            );
            let gpu_round = started.elapsed();
            gpu_total += gpu_round;
            gpu_min = gpu_min.min(gpu_round);
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let (cpu, gpu_ms) = (ms(cpu_min), ms(gpu_min));
        let cpu_mean = ms(cpu_total) / f64::from(ROUNDS);
        let gpu_mean = ms(gpu_total) / f64::from(ROUNDS);
        eprintln!(
            "{}x{} -> {}x{}: CPU convert+scale min {cpu:.3} ms / mean {cpu_mean:.3} ms, \
             GPU min {gpu_ms:.3} ms / mean {gpu_mean:.3} ms",
            source.0, source.1, target.0, target.1
        );
        assert!(
            gpu_ms < cpu,
            "the GPU pair (min {gpu_ms:.3} ms) is not cheaper than the CPU pair \
             (min {cpu:.3} ms) at {}x{} -> {}x{}",
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
    // What a rebuild resets is pinned on a real device, in
    // `the_pipelined_readback_hands_back_the_previous_frame`.

    // The caller's drop points (a seek, a park, a crossing) leave the
    // pipeline standing, so the cycle has to be emptied explicitly --
    // otherwise the next frame is handed a picture whose planes the caller
    // has already recycled.
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

    // t261016-97b7: the published slot outlives a drop, and is forgotten
    // only by the blit that writes over it.
    cycle.published = Some(0);
    cycle.abandon();
    assert_eq!(cycle.published, Some(0));
    assert_eq!(cycle.advance(Readback::Pipelined), (0, None));
    assert_eq!(cycle.published, None, "slot 0 was just written over");
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
    // A new target size rebuilds the pipeline, and a rebuilt pipeline is a
    // fresh cycle: the frame it had in flight was the old size, so the
    // first frame after it is a first frame again.
    let resized = (32, 32);
    let mut resized_rgba = vec![0u8; resized.0 as usize * resized.1 as usize * 4];
    assert_eq!(
        gpu.convert_and_scale(
            &first,
            source,
            resized,
            &mut resized_rgba,
            Readback::Pipelined
        ),
        Converted::Warming,
        "the old-size frame in flight must not come back through the rebuilt pipeline"
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
/// t260917-66c2: the slot a frame is copied into is never one the stage was
/// handed in the last `SHARE_RING - 2` frames (slint may still be sampling
/// it), and each hand-out is the previous frame's own slot and fence value.
#[test]
fn t260917_66c2_the_ring_never_overwrites_a_slot_the_stage_was_just_handed() {
    let mut cycle = StagingCycle::default();
    let mut ring = RingCycle::default();
    let mut handed: Vec<usize> = Vec::new();
    for value in 1..=12u64 {
        let (write, read) = cycle.advance(Readback::Pipelined);
        let slot = ring.claim(write, value);
        assert!(
            !handed
                .iter()
                .rev()
                .take(SHARE_RING - 2)
                .any(|&shown| shown == slot),
            "frame {value} wrote slot {slot}, handed out recently: {handed:?}"
        );
        if let Some(index) = read {
            let (shown, shown_value) = ring.held[index].expect("a claimed slot");
            assert_eq!(shown_value, value - 1, "the hand-out is the previous frame");
            handed.push(shown);
        }
    }
    assert_eq!(handed.len(), 11);
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

/// The texture's bytes, read back through wgpu itself.
fn wgpu_bytes(
    device: &slint::wgpu_30::wgpu::Device,
    queue: &slint::wgpu_30::wgpu::Queue,
    texture: &slint::wgpu_30::wgpu::Texture,
) -> Vec<u8> {
    use slint::wgpu_30::wgpu;
    let size = texture.size();
    let row = (size.width * 4).div_ceil(256) * 256;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(row * size.height),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: None,
            },
        },
        size,
    );
    queue.submit([encoder.finish()]);
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, |mapped| mapped.expect("mapped"));
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("polled");
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .expect("the mapped range");
    let width = size.width as usize * 4;
    (0..size.height as usize)
        .flat_map(|y| mapped[y * row as usize..][..width].to_vec())
        .collect()
}

/// t260917-66c2: a shared picture opened on a DX12 wgpu device -- slint's
/// kind -- through `OpenSharedHandle`, `texture_from_raw` and the fence
/// wait carries exactly the bytes the readback path reads for the same
/// frame: colour and orientation unchanged. Two different frames in two
/// slots, so each picture is shown to be its own frame's and not whatever
/// the ring held last.
#[test]
fn t260917_66c2_a_shared_picture_opens_on_wgpu_with_the_readback_bytes() {
    use slint::wgpu_30::wgpu;
    let (source, target) = ((320, 180), (200, 113));
    let frames = [gradient(source.0, source.1), stripes(source.0, source.1)];
    let mut reading = GpuScaler::new().expect("a GPU scaler on this machine");
    let expected: Vec<Vec<u8>> = frames
        .iter()
        .map(|nv12| scale(&mut reading, nv12, source, target))
        .collect();
    assert_ne!(expected[0], expected[1], "the two frames differ");

    let mut sharing = GpuScaler::new().expect("a GPU scaler on this machine");
    sharing.set_share(true);
    let pictures: Vec<_> = frames
        .iter()
        .map(|nv12| {
            assert!(sharing
                .blit(
                    Nv12Source::Planes(nv12),
                    source,
                    target,
                    &mut [],
                    None,
                    Readback::Now
                )
                .expect("the shared blit"));
            sharing.take_picture().expect("a shared picture")
        })
        .collect();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("a DX12 adapter");
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .expect("a DX12 device");
    let mut stage = StageDevice::new(&device, &queue).expect("wgpu on DX12");
    for (picture, expected) in pictures.iter().zip(&expected) {
        let texture = stage.texture(picture).expect("opened on the wgpu device");
        assert_eq!(
            (texture.size().width, texture.size().height),
            target,
            "the stage size"
        );
        assert!(
            wgpu_bytes(&device, &queue, &texture) == *expected,
            "the shared picture is byte for byte the readback's"
        );
    }
}

/// t260930-8bce: a pipeline rebuilt with sharing off and then on again --
/// what every new engine does on its first frames -- hands out the same
/// shared textures, so slint's device never opens a second ring. A new
/// stage size is the one thing that makes a new ring.
#[test]
fn t260930_8bce_a_rebuilt_pipeline_keeps_its_shared_ring() {
    let source = (320, 180);
    let nv12 = gradient(source.0, source.1);
    let Some(mut gpu) = GpuScaler::new() else {
        return;
    };
    let shared = |gpu: &mut GpuScaler, target: (u32, u32)| {
        gpu.set_share(true);
        assert!(gpu
            .blit(
                Nv12Source::Planes(&nv12),
                source,
                target,
                &mut [],
                None,
                Readback::Now
            )
            .expect("the shared blit"));
        gpu.take_picture().expect("a shared picture").texture
    };
    let first = shared(&mut gpu, (200, 113));
    gpu.set_share(false);
    let mut rgba = vec![0u8; 200 * 113 * 4];
    assert_eq!(
        gpu.convert_and_scale(&nv12, source, (200, 113), &mut rgba, Readback::Now),
        Converted::This
    );
    let again = shared(&mut gpu, (200, 113));
    let ring = gpu
        .pipeline
        .as_ref()
        .and_then(|held| held.share.as_ref())
        .expect("a ring");
    assert!(
        ring.slots
            .iter()
            .any(|(_, handle)| std::sync::Arc::ptr_eq(handle, &first)),
        "the first picture's texture is still one of the ring's"
    );
    assert!(
        ring.slots
            .iter()
            .any(|(_, handle)| std::sync::Arc::ptr_eq(handle, &again)),
        "and so is the one after the rebuild"
    );
    let resized = shared(&mut gpu, (160, 90));
    let ring = gpu
        .pipeline
        .as_ref()
        .and_then(|held| held.share.as_ref())
        .expect("a ring");
    assert!(
        !ring
            .slots
            .iter()
            .any(|(_, handle)| std::sync::Arc::ptr_eq(handle, &first)),
        "a new stage size makes a new ring"
    );
    assert!(ring
        .slots
        .iter()
        .any(|(_, handle)| std::sync::Arc::ptr_eq(handle, &resized)));
}

/// t260930-70ff: whichever order two overlapping engines park in, the slot
/// ends up holding the scaler whose pipeline carries the shared ring.
#[test]
fn t260930_70ff_the_slot_keeps_the_scaler_with_a_shared_ring() {
    let source = (320, 180);
    let nv12 = gradient(source.0, source.1);
    let pair = || {
        let plain = GpuScaler::new()?;
        let mut sharing = GpuScaler::new()?;
        sharing.set_share(true);
        assert!(sharing
            .blit(
                Nv12Source::Planes(&nv12),
                source,
                (200, 113),
                &mut [],
                None,
                Readback::Now
            )
            .expect("the shared blit"));
        sharing.take_picture().expect("a shared picture");
        Some((plain, sharing))
    };
    let Some((plain, sharing)) = pair() else {
        return;
    };
    assert!(sharing.has_ring() && !plain.has_ring(), "the fixture");
    let mut slot = None;
    settle(&mut slot, plain);
    settle(&mut slot, sharing);
    assert!(
        slot.as_ref().is_some_and(|held| held.0.has_ring()),
        "a ring-less scaler parked first gives way"
    );
    let (plain, sharing) = pair().expect("made once already");
    let mut slot = None;
    settle(&mut slot, sharing);
    settle(&mut slot, plain);
    assert!(
        slot.as_ref().is_some_and(|held| held.0.has_ring()),
        "a ring-bearing scaler parked first stays"
    );
}

/// t261025-d24d: `warn_once!` is once per message, not once per module. Two
/// different failures, each hit twice, say two lines -- the second failure is
/// not swallowed by the first, and neither repeats.
#[test]
fn each_gpu_warning_is_said_once_by_itself() {
    #[derive(Clone, Default)]
    struct Lines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // Two failure sites, as `GpuScaler::new` and `run` are. The messages are
    // this test's own: the `Once` behind each lives for the whole process.
    fn fail(error: &windows::core::Error) {
        warn_once!("t261025-d24d: the first failure", error);
        warn_once!("t261025-d24d: the second failure", error);
    }
    // A second live dispatcher, as `container_sessions` explains: otherwise a
    // callsite another test thread registered first can be cached as disabled.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    let lines = Lines::default();
    let sink = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    let error = windows::core::Error::from(windows::Win32::Foundation::E_FAIL);
    tracing::subscriber::with_default(subscriber, || {
        fail(&error);
        fail(&error);
    });
    let logged = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let said: Vec<&str> = logged
        .lines()
        .filter(|line| line.contains("t261025-d24d"))
        .collect();
    assert_eq!(said.len(), 2, "one line per message: {said:#?}");
    assert!(said[0].contains("the first failure"), "{said:#?}");
    assert!(said[1].contains("the second failure"), "{said:#?}");
}
