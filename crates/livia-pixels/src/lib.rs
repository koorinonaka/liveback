//! Pixel-format conversion for the playback path.
//!
//! This lives in its own package purely so the workspace can give it an
//! `opt-level` in dev builds. `run.bat` starts the app with `cargo run`, and a
//! per-pixel loop over a 1080p frame is far too slow unoptimized -- but a
//! profile override applies to every target in a package, and optimizing the
//! slint+skia `liveback` binary crashes rustc on this toolchain. A leaf crate
//! has neither problem (task200).
//!
//! It carries one dependency, `i-slint-core`, and that does not undo the above:
//! an `opt-level` override names a package, so the dependency still compiles
//! exactly as it does for the app. What lands here optimized is only this
//! crate's own code -- including the generic slint code it instantiates, which
//! is the point of [`image_from_rgba`] living here (task205).

use i_slint_core::graphics::{Image, Rgba8Pixel, SharedPixelBuffer};

/// BGRA (possibly bottom-up) -> tightly packed top-down RGBA.
///
/// `stride` is the source's row pitch in bytes; a negative value means the rows
/// are stored bottom-up, which is what Media Foundation's advanced Video
/// Processor used to hand out (task120). The basic one returns top-down, so the
/// sign is read rather than assumed either way (task200).
///
/// The bulk move is one `copy_from_slice` per row and the per-pixel work is a
/// two-byte swap plus alpha, which keeps this fast even at opt-level 0 -- an
/// iterator chain over `chunks_exact` does not inline there and cost 105ms per
/// 1080p frame against this version's ~5.9ms.
pub fn swizzle_bgra(source: &[u8], width: u32, height: u32, stride: i32) -> Vec<u8> {
    let mut rgba = vec![0u8; width as usize * 4 * height as usize];
    swizzle_bgra_into(&mut rgba, source, width, height, stride);
    rgba
}

/// [`swizzle_bgra`] into a buffer the caller owns, which the playback engine
/// recycles from frame to frame rather than faulting in a new one every time
/// (task1640). `rgba` must hold at least `width * height * 4` bytes; anything
/// shorter is left untouched.
///
/// Rows the source cannot supply are **zeroed** rather than skipped. The
/// allocating version could leave them alone because a fresh `Vec` is already
/// black there; a recycled one would show the previous frame instead.
pub fn swizzle_bgra_into(rgba: &mut [u8], source: &[u8], width: u32, height: u32, stride: i32) {
    let width = width as usize;
    let height = height as usize;
    let row_bytes = width * 4;
    let stride_abs = stride.unsigned_abs() as usize;
    let Some(rgba) = rgba.get_mut(..row_bytes * height) else {
        return;
    };
    for y in 0..height {
        let source_row = if stride < 0 { height - 1 - y } else { y };
        let Some(row) = source.get(source_row * stride_abs..source_row * stride_abs + row_bytes)
        else {
            rgba[y * row_bytes..].fill(0);
            break;
        };
        let target = &mut rgba[y * row_bytes..(y + 1) * row_bytes];
        target.copy_from_slice(row);
        let mut x = 0;
        while x < row_bytes {
            target.swap(x, x + 2);
            target[x + 3] = 0xFF;
            x += 4;
        }
    }
}

/// How many rows the Y plane of `source` was really built for, which is where
/// the interleaved UV plane starts -- derived from the buffer rather than
/// assumed to be `height`.
///
/// A decoder is free to align the Y plane to more rows than the frame
/// displays: Media Foundation's H.264 decoder pads to a multiple of 16, so it
/// hands 1080-line video back in a buffer sized for 1088 (measured: pitch
/// 1920, length 3,133,440) and 1032-line back in one sized for 1040 (length
/// 2,995,200). Reading chroma at row `height` then lands in the last rows of
/// luma padding instead. Those bytes are zero, which reads as U=V=0, which is
/// *bright green*: a green band across the top of the picture and every colour
/// below it pulled from a few rows too high. An NV12 buffer is
/// `y_stride * rows * 3/2`, so the row count falls out of its length.
///
/// Shared rather than re-derived per caller: `nv12_to_rgba_into` reads the
/// planes in place, `playback::segment_reader::nv12_planes` copies them out
/// packed for the GPU, and both need the same answer.
pub fn nv12_plane_rows(source_len: usize, y_stride: usize, height: usize) -> usize {
    (source_len * 2 / 3)
        .checked_div(y_stride)
        .unwrap_or(height)
        .max(height)
}

/// BT.709 studio-range NV12 -> tightly packed top-down RGBA.
///
/// This is the colour conversion Media Foundation's Video Processor used to do
/// on the way to RGB32. Doing it here instead is what makes it cheap: asking
/// the Source Reader for NV12 takes `ReadSample` from ~8ms a frame to well
/// under 2ms, because the decoder hands over its native output and nothing
/// converts it (task200 measured all three configurations; task206 acts on it).
///
/// The coefficients are BT.709 with a **studio-range** input, matching what the
/// capture side encodes: `GpuNv12Converter` sets the output colour space to
/// `YCBCR_STUDIO_G22_LEFT_P709`, and `verify_nv12_conversion_uses_studio_range_bt709`
/// is the test that pins it. Reading those same samples as full range would
/// wash the picture out by about 7% at both ends, which is exactly the kind of
/// drift that survives every unit test and only shows up side by side.
///
/// `y_stride` is the Y plane's row pitch in bytes; the interleaved UV plane
/// follows it at the same pitch and half the height, which is NV12's layout.
/// A source too short for the size it claims yields what could be read and
/// leaves the rest black rather than panicking mid-frame.
pub fn nv12_to_rgba(source: &[u8], width: u32, height: u32, y_stride: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; width as usize * 4 * height as usize];
    nv12_to_rgba_into(&mut rgba, source, width, height, y_stride);
    rgba
}

/// [`nv12_to_rgba`] into a buffer the caller owns -- the recycled-buffer half of
/// the pair, same contract as [`swizzle_bgra_into`]: at least
/// `width * height * 4` bytes, and every row the source falls short of is zeroed
/// rather than left holding whatever the last frame wrote there (task1640).
pub fn nv12_to_rgba_into(rgba: &mut [u8], source: &[u8], width: u32, height: u32, y_stride: u32) {
    let width = width as usize;
    let height = height as usize;
    let y_stride = y_stride as usize;
    let row_bytes = width * 4;
    let Some(rgba) = rgba.get_mut(..row_bytes * height) else {
        return;
    };
    let uv_base = y_stride * nv12_plane_rows(source.len(), y_stride, height);
    // Chroma covers pairs of columns, so an odd width still needs a whole
    // final pair to read from.
    let uv_bytes = width.div_ceil(2) * 2;

    // Row pairs, then whole rows, then pairs of pixels -- the slices are taken
    // once per row and everything inside walks them, which is what keeps the
    // bounds checks out of a loop that runs two million times a frame. The
    // three chroma terms are computed once per 2x2 block and spent four times.
    for pair in 0..height.div_ceil(2) {
        let uv_at = uv_base + pair * y_stride;
        let Some(uv) = source.get(uv_at..uv_at + uv_bytes) else {
            rgba[pair * 2 * row_bytes..].fill(0);
            return;
        };
        for row in (pair * 2)..((pair * 2 + 2).min(height)) {
            let luma_at = row * y_stride;
            let Some(luma) = source.get(luma_at..luma_at + width) else {
                rgba[row * row_bytes..].fill(0);
                return;
            };
            let out = &mut rgba[row * row_bytes..(row + 1) * row_bytes];
            // `chunks_mut(8)` is two pixels, and the final chunk is half that
            // on an odd width -- which is exactly the tail behaviour wanted,
            // with no separate case to get wrong.
            for ((pixels, lumas), chroma) in out
                .chunks_mut(8)
                .zip(luma.chunks(2))
                .zip(uv.chunks_exact(2))
            {
                // BT.709, limited -> full range, in 8.8 fixed point:
                //   R = 1.164*(Y-16)                 + 1.793*(V-128)
                //   G = 1.164*(Y-16) - 0.213*(U-128) - 0.533*(V-128)
                //   B = 1.164*(Y-16) + 2.112*(U-128)
                let d = i32::from(chroma[0]) - 128;
                let e = i32::from(chroma[1]) - 128;
                let r_add = 459 * e + 128;
                let g_add = -55 * d - 136 * e + 128;
                let b_add = 541 * d + 128;
                for (pixel, &luma) in pixels.chunks_exact_mut(4).zip(lumas) {
                    let c = 298 * (i32::from(luma) - 16);
                    pixel[0] = clamp_u8((c + r_add) >> 8);
                    pixel[1] = clamp_u8((c + g_add) >> 8);
                    pixel[2] = clamp_u8((c + b_add) >> 8);
                    pixel[3] = 0xFF;
                }
            }
        }
    }
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Packed RGBA -> a slint [`Image`], for the UI to hand straight to a widget.
///
/// Slint builds a `SharedPixelBuffer` one element at a time (`SharedVector`'s
/// `From<&[T]>`), so this generic instantiation costs 21ms a 1920x1032 frame
/// when it lands in an unoptimized crate -- and the UI thread pays it once per
/// presented frame. Instantiating it *here* is the whole trick: a
/// monomorphization is compiled into the crate that asks for it, so this
/// package's `opt-level` is the one that applies (task205).
///
/// `None` for a zero-sized or short buffer. Both are load-bearing:
/// `SharedPixelBuffer::new(0, 0)` hands back a dangling pointer that aborts the
/// process on the copy, and a length mismatch would read out of bounds.
pub fn image_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Image> {
    if width == 0 || height == 0 || rgba.is_empty() {
        return None;
    }
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }
    // `clone_from_slice` rather than `new` + `copy_from_slice`: the latter fills
    // the buffer with default pixels first, which is a second pass of the same
    // per-element loop (38.6ms against 21.3ms unoptimized).
    Some(Image::from_rgba8(
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba, width, height),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an NV12 buffer of one flat colour, with `pad` bytes of row
    /// padding, so the tests exercise the stride path rather than assuming
    /// `y_stride == width`.
    fn flat_nv12(width: usize, height: usize, pad: usize, y: u8, u: u8, v: u8) -> (Vec<u8>, u32) {
        let stride = width + pad;
        let mut buffer = vec![0u8; stride * height + stride * height.div_ceil(2)];
        for row in 0..height {
            buffer[row * stride..row * stride + width].fill(y);
        }
        let uv_base = stride * height;
        for row in 0..height.div_ceil(2) {
            let start = uv_base + row * stride;
            for pair in 0..width.div_ceil(2) {
                buffer[start + pair * 2] = u;
                buffer[start + pair * 2 + 1] = v;
            }
        }
        (buffer, stride as u32)
    }

    fn assert_close(actual: &[u8], expected: [u8; 3], what: &str) {
        for (channel, (got, want)) in actual.iter().zip(expected).enumerate() {
            assert!(
                i32::from(*got).abs_diff(i32::from(want)) <= 1,
                "{what}: channel {channel} was {got}, expected {want} (+/-1); \
                 the whole pixel was {actual:?}"
            );
        }
        assert_eq!(actual[3], 0xFF, "{what}: alpha must be opaque");
    }

    /// The colours the capture side actually writes. Studio range means black
    /// is Y=16 and white is Y=235; reading those as full range would lift
    /// black off zero and clip white, which is the silent drift this pins.
    #[test]
    fn nv12_decodes_bt709_studio_range_primaries() {
        // (name, Y, U, V, expected RGB) -- BT.709 studio range.
        let cases: [(&str, u8, u8, u8, [u8; 3]); 5] = [
            ("black", 16, 128, 128, [0, 0, 0]),
            ("white", 235, 128, 128, [255, 255, 255]),
            ("red", 63, 102, 240, [255, 0, 0]),
            ("green", 173, 42, 26, [0, 255, 0]),
            ("blue", 32, 240, 118, [0, 0, 255]),
        ];
        for (name, y, u, v, expected) in cases {
            let (source, stride) = flat_nv12(4, 4, 0, y, u, v);
            let rgba = nv12_to_rgba(&source, 4, 4, stride);
            assert_eq!(rgba.len(), 4 * 4 * 4);
            // Every pixel of a flat frame must agree, not just the first.
            for pixel in rgba.chunks_exact(4) {
                assert_close(pixel, expected, name);
            }
        }
    }

    /// Row padding is the normal case out of Media Foundation, and reading
    /// past it would slide every row sideways -- a diagonal smear rather than
    /// an obviously wrong colour.
    #[test]
    fn nv12_honours_row_padding() {
        let (source, stride) = flat_nv12(6, 4, 26, 235, 128, 128);
        assert_eq!(stride, 32, "the padded stride is what gets exercised");
        let rgba = nv12_to_rgba(&source, 6, 4, stride);
        for pixel in rgba.chunks_exact(4) {
            assert_close(pixel, [255, 255, 255], "padded white");
        }
    }

    /// Odd sizes have a half-populated final block in both directions. The
    /// output must still be exactly `width * height * 4` and fully written.
    #[test]
    fn nv12_handles_odd_width_and_height() {
        let (source, stride) = flat_nv12(5, 3, 1, 235, 128, 128);
        let rgba = nv12_to_rgba(&source, 5, 3, stride);
        assert_eq!(rgba.len(), 5 * 3 * 4);
        for (index, pixel) in rgba.chunks_exact(4).enumerate() {
            assert_close(pixel, [255, 255, 255], &format!("odd pixel {index}"));
        }
    }

    /// Chroma is shared across a 2x2 block but luma is not: a frame that is
    /// bright on one row and dark on the next must come back that way, which
    /// is what catches the block loop writing the same row twice.
    #[test]
    fn nv12_keeps_per_pixel_luma_inside_a_block() {
        let (mut source, stride) = flat_nv12(2, 2, 0, 16, 128, 128);
        source[0] = 235; // top-left only
        let rgba = nv12_to_rgba(&source, 2, 2, stride);
        assert_close(&rgba[0..4], [255, 255, 255], "top-left is white");
        for (index, pixel) in rgba.chunks_exact(4).enumerate().skip(1) {
            assert_close(pixel, [0, 0, 0], &format!("pixel {index} stays black"));
        }
    }

    /// The decoder aligns its Y plane to more rows than the frame displays --
    /// 1080 lines arrive in a buffer built for 1088 -- so the UV plane does
    /// not start at `y_stride * height`. Assuming it does reads the luma
    /// padding as chroma, and zeroed padding is U=V=0, which is bright green:
    /// a green band across the top and every colour below it taken from a few
    /// rows too high. Caught on a real recording, pinned here.
    #[test]
    fn nv12_finds_the_uv_plane_when_the_y_plane_is_padded_with_rows() {
        let (width, height, plane_rows) = (4usize, 4usize, 8usize);
        let stride = width;
        // Y rows for the visible frame, then padding rows, then chroma.
        let mut source = vec![0u8; stride * plane_rows + stride * plane_rows / 2];
        source[..stride * height].fill(235);
        let uv_base = stride * plane_rows;
        source[uv_base..].fill(128);

        let rgba = nv12_to_rgba(&source, width as u32, height as u32, stride as u32);
        for (index, pixel) in rgba.chunks_exact(4).enumerate() {
            assert_close(
                pixel,
                [255, 255, 255],
                &format!("padded-plane pixel {index}"),
            );
        }
    }

    /// A truncated buffer must leave the rest black instead of panicking
    /// mid-frame: a short read is a decoder hiccup, not a reason to take the
    /// playback thread down.
    #[test]
    fn nv12_tolerates_a_short_source() {
        let (source, stride) = flat_nv12(4, 4, 0, 235, 128, 128);
        let rgba = nv12_to_rgba(&source[..8], 4, 4, stride);
        assert_eq!(rgba.len(), 4 * 4 * 4, "the frame is still fully sized");
    }

    /// The pixel order and the bottom-up handling, pinned exactly. A 2x2
    /// bottom-up BGRA source: source row 0 is the *bottom* display row.
    #[test]
    fn swizzle_flips_bottom_up_rows_and_reorders_channels() {
        // Stride 12 (one padding pixel per row), two rows: source row 0 =
        // pixels A B, source row 1 = pixels C D. Bottom-up => C D over A B.
        #[rustfmt::skip]
        let source: Vec<u8> = vec![
            1, 2, 3, 9,   4, 5, 6, 9,   0, 0, 0, 0, // A(b=1,g=2,r=3) B(b=4,g=5,r=6)
            7, 8, 9, 9,  10, 11, 12, 9, 0, 0, 0, 0, // C D
        ];
        let rgba = swizzle_bgra(&source, 2, 2, -12);
        #[rustfmt::skip]
        assert_eq!(rgba, vec![
            9, 8, 7, 255,  12, 11, 10, 255, // C D
            3, 2, 1, 255,   6,  5,  4, 255, // A B
        ]);
        // Top-down keeps the source order.
        let rgba = swizzle_bgra(&source, 2, 2, 12);
        assert_eq!(&rgba[..4], &[3, 2, 1, 255]);
        assert_eq!(&rgba[8..12], &[9, 8, 7, 255]);
    }

    /// The pixels have to survive the trip into slint's own buffer, and the
    /// two guards have to hold: a zero size aborts the process further down,
    /// and a short buffer would read past the end.
    #[test]
    fn image_from_rgba_round_trips_pixels_and_rejects_bad_sizes() {
        #[rustfmt::skip]
        let rgba: Vec<u8> = vec![
            1, 2, 3, 255,   4, 5, 6, 255,
            7, 8, 9, 255,  10, 11, 12, 255,
        ];
        let image = image_from_rgba(2, 2, &rgba).expect("a well-formed frame converts");
        assert_eq!((image.size().width, image.size().height), (2, 2));
        let back = image.to_rgba8().expect("the buffer reads back");
        assert_eq!(back.as_bytes(), rgba.as_slice());

        assert!(image_from_rgba(0, 0, &rgba).is_none(), "zero size");
        assert!(image_from_rgba(2, 2, &rgba[..8]).is_none(), "short buffer");
        assert!(image_from_rgba(2, 2, &[]).is_none(), "empty buffer");
    }

    /// A short source (never seen in practice, but the slice lookup tolerates
    /// it) still yields a full-size, black-padded frame rather than panicking.
    #[test]
    fn a_truncated_source_yields_a_padded_frame() {
        let rgba = swizzle_bgra(&[1, 2, 3, 4], 2, 2, 8);
        assert_eq!(rgba.len(), 2 * 2 * 4);
        assert_eq!(&rgba[4..], &[0; 12]);
    }
}

use fast_image_resize::images::{Image as ResizeImage, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

/// Resamples decoded frames down to the size the stage draws them at (task990).
///
/// This lives here for the same reason [`image_from_rgba`] does: `Resizer::resize`
/// is generic over its image views, so the instantiation is compiled into
/// whichever crate calls it. Called from `livia` it compiled at opt-level 0 and
/// a 2560x1440 -> 1200x675 pass measured 604ms a frame -- against a 16.7ms
/// budget at 60fps -- with the dependency itself already optimized. The same
/// call from this package is the whole fix: 5.3ms.
///
/// CatmullRom rather than Lanczos3, measured on that same frame at 5.3ms
/// against 7.4ms. Both leave mipmap-less bilinear minification far behind,
/// which is the entire point; what the extra 2.1ms would buy is a little more
/// acutance and a little more ringing, and it does not fit next to
/// `nv12_to_rgba` in a 60fps frame.
///
/// The resizer is held rather than built per call: it owns the scratch buffers
/// the convolution runs through, which would otherwise grow from empty sixty
/// times a second.
pub struct FrameScaler {
    resizer: Resizer,
    options: ResizeOptions,
}

impl Default for FrameScaler {
    fn default() -> Self {
        Self {
            resizer: Resizer::new(),
            // Frames arrive from `swizzle_bgra`/`nv12_to_rgba` fully opaque, so
            // the alpha premultiply/demultiply either side of the convolution
            // would be two full-image passes that cannot change a pixel.
            options: ResizeOptions::new()
                .resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom))
                .use_alpha(false),
        }
    }
}

impl FrameScaler {
    /// `rgba` resampled to `target`, or `None` if the resize fails -- which
    /// leaves the caller holding the full-resolution frame it already had.
    ///
    /// Whether resampling is worth doing at all is the caller's call; see
    /// `livia::playback::scale::scaled_dims`.
    ///
    /// `spare` is a destination an earlier frame was resampled into and whose
    /// last reader has since dropped it (task1690, on task1640's pattern): the
    /// convolution writes every destination pixel, so a buffer of exactly the
    /// right size is written again in place rather than faulted in fresh. Any
    /// other size -- the stage was resized since -- is dropped and a new one
    /// allocated, the same contract `playback::take_fitting` states.
    pub fn downscale(
        &mut self,
        rgba: &[u8],
        source: (u32, u32),
        target: (u32, u32),
        spare: Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        let bytes = target.0 as usize * target.1 as usize * 4;
        let mut buffer = match spare {
            Some(buffer) if buffer.len() == bytes => buffer,
            _ => vec![0u8; bytes],
        };
        let source_view = ImageRef::new(source.0, source.1, rgba, PixelType::U8x4).ok()?;
        let mut scaled =
            ResizeImage::from_slice_u8(target.0, target.1, &mut buffer, PixelType::U8x4).ok()?;
        self.resizer
            .resize(&source_view, &mut scaled, &self.options)
            .ok()?;
        drop(scaled);
        Some(buffer)
    }
}

#[cfg(test)]
mod scaler_tests {
    use super::FrameScaler;

    fn gradient(width: u32, height: u32) -> Vec<u8> {
        (0..width * height * 4)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>()
    }

    /// Task1690: a recycled destination must produce byte-for-byte what a fresh
    /// one does. Poisoned with `0xFF` first, so a destination pixel the
    /// convolution failed to write would show up as the previous frame's
    /// leftovers rather than silently passing.
    #[test]
    fn a_recycled_destination_is_written_end_to_end() {
        let source = gradient(64, 48);
        let mut scaler = FrameScaler::default();
        let fresh = scaler
            .downscale(&source, (64, 48), (25, 19), None)
            .expect("a fresh destination resizes");
        assert_eq!(fresh.len(), 25 * 19 * 4);

        let poisoned = vec![0xFFu8; 25 * 19 * 4];
        let recycled = scaler
            .downscale(&source, (64, 48), (25, 19), Some(poisoned))
            .expect("a recycled destination resizes");
        assert_eq!(recycled, fresh, "no pixel of the poison survives");
    }

    /// A spare from a differently sized stage is refused rather than written
    /// short: the result is always exactly the target, whatever came in.
    #[test]
    fn a_misfit_spare_is_replaced_rather_than_written_short() {
        let source = gradient(64, 48);
        let mut scaler = FrameScaler::default();
        for spare in [vec![0xFFu8; 4], vec![0xFFu8; 64 * 48 * 4]] {
            let scaled = scaler
                .downscale(&source, (64, 48), (25, 19), Some(spare))
                .expect("a misfit spare still resizes");
            assert_eq!(scaled.len(), 25 * 19 * 4);
        }
    }
}
