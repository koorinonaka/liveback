//! Downscaling decoded frames to the size they are actually drawn at (task990).
//!
//! slint hands the `Image` to skia, which samples it bilinearly with no
//! mipmaps. A 2560x1440 recording drawn into a ~1200px stage is therefore a
//! 0.47x bilinear minification -- it drops more than half the source rows
//! outright rather than averaging them, and the whole picture reads as soft.
//! Resampling to the target with a proper filter first is what fixes that; the
//! remaining subpixel residual (the stage's logical size times the scale factor
//! rarely lands on a whole pixel) is well under a pixel and not worth chasing.
//!
//! This runs on the playback engine's thread, never the UI's -- see the
//! `pump_playback` note about task205. The one exception is a resize while
//! paused, where no new frame is coming and the held one is resampled in place.
//!
//! The resampling itself is `livia_pixels::FrameScaler`, in that package rather
//! than this module for the reason its own docs give: the resize call is
//! generic, so it compiles at the *caller's* opt-level, and this crate's is 0.

pub use livia_pixels::FrameScaler;

use super::{FullFrame, PlaybackFrame};

/// The size a source frame should be resampled to before it is drawn, or `None`
/// when it should be left alone.
///
/// `target` is the box the stage laid out for the picture, and the scaler
/// **stretches whatever box it is handed** -- so the target is fitted to the
/// source's own aspect here rather than used as given. It used to be used as
/// given, on the assumption that it already carried the recording's aspect.
/// It does not on the first frame of a clip: the stage sizes its picture box
/// from `ClipVm.stage-aspect` / `ReviewVm`'s, and that number only arrives
/// *with a frame* (`clips.rs`'s `pump_player`), so until one does it is the
/// previous clip's -- or 16:9, straight after launch. One frame stretched into
/// last clip's shape and the next one into the right shape is the
/// 「クリップ再生するとサムネイルと位置がずれる」 the user reported on
/// 2026-09-16: the thumbnail under it is letterboxed correctly the whole time,
/// so the picture visibly jumps off it and back. Fitting here means a wrong
/// target costs a slightly small resample for one frame instead of a wrong
/// picture, and the geometry no longer depends on the aspect having arrived.
///
/// Past that, the decision is the same as it was: never upscale -- skia's
/// bilinear is fine for magnification and the pixels are not there to invent --
/// and never resample at 1:1.
pub fn scaled_dims(source: (u32, u32), target: (u32, u32)) -> Option<(u32, u32)> {
    if source.0 == 0 || source.1 == 0 || target.0 == 0 || target.1 == 0 {
        return None;
    }
    let scale = f64::min(
        f64::from(target.0) / f64::from(source.0),
        f64::from(target.1) / f64::from(source.1),
    );
    // `>= 1.0` covers both of the old guards at once: a box that does not fit
    // inside the source, and one that does not shrink it.
    if scale >= 1.0 {
        return None;
    }
    let fitted = (
        ((f64::from(source.0) * scale).round() as u32).max(1),
        ((f64::from(source.1) * scale).round() as u32).max(1),
    );
    (!near_one_to_one(source, fitted)).then_some(fitted)
}

/// A minification small enough to leave to skia (t260913-11b4).
///
/// The module's whole reason to exist is that skia's bilinear minification
/// drops rows instead of averaging them, which is ruinous at 0.47x. At 0.95x it
/// drops one row in twenty and the resample costs 2.5ms per frame -- the single
/// largest cost in playback. A 1922x1112 recording full screen on a 1920x1080
/// display is a 0.97x fit, which is exactly the case this spares.
///
/// Deliberately narrow. Windowed playback is nowhere near it (a ~1200px stage
/// for a 1922px recording is 0.63x) and still resamples.
fn near_one_to_one(source: (u32, u32), target: (u32, u32)) -> bool {
    const KEEP_AT_LEAST: u64 = 95;
    u64::from(target.0) * 100 >= u64::from(source.0) * KEEP_AT_LEAST
        && u64::from(target.1) * 100 >= u64::from(source.1) * KEEP_AT_LEAST
}

/// `rgba` resampled for a stage of `target` physical pixels, or `None` when
/// [`scaled_dims`] says there is nothing worth doing.
///
/// `spare` is a destination the UI has finished with (task1690); see
/// [`FrameScaler::downscale`] for what makes one usable.
pub fn downscale(
    scaler: &mut FrameScaler,
    rgba: &[u8],
    source: (u32, u32),
    target: (u32, u32),
    spare: Option<Vec<u8>>,
) -> Option<(u32, u32, Vec<u8>)> {
    let (width, height) = scaled_dims(source, target)?;
    let scaled = scaler
        .downscale(rgba, source, (width, height), spare)
        .or_else(|| {
            tracing::warn!(
                source_width = source.0,
                source_height = source.1,
                width,
                height,
                "playback: frame resize failed; drawing at full resolution"
            );
            None
        })?;
    Some((width, height, scaled))
}

/// The frame to publish for a stage of `target` physical pixels.
///
/// Resampled to the stage when that is worth doing, with the recording-sized
/// original carried along in [`PlaybackFrame::full`] -- screenshots save that,
/// and the stage aspect is measured from it. Otherwise the frame goes out as
/// decoded and `full` is `None`, which is the signal that `width`/`height`
/// already are the recording's.
pub fn frame_for_stage(
    scaler: &mut FrameScaler,
    rgba: Vec<u8>,
    (width, height): (u32, u32),
    target: Option<(u32, u32)>,
    spare: Option<Vec<u8>>,
) -> PlaybackFrame {
    match target.and_then(|target| downscale(scaler, &rgba, (width, height), target, spare)) {
        Some((scaled_width, scaled_height, scaled)) => PlaybackFrame {
            width: scaled_width,
            height: scaled_height,
            rgba: scaled,
            full: Some((width, height, FullFrame::Rgba(rgba))),
        },
        None => PlaybackFrame {
            width,
            height,
            rgba,
            full: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{frame_for_stage, scaled_dims, FrameScaler};

    fn opaque(width: u32, height: u32) -> Vec<u8> {
        vec![0x80; (width * height * 4) as usize]
    }

    /// What the UI is handed is the stage's size, and what it can still reach
    /// for a screenshot is the recording's.
    #[test]
    fn a_frame_for_a_smaller_stage_carries_the_recording_along() {
        let frame = frame_for_stage(
            &mut FrameScaler::default(),
            opaque(1920, 1080),
            (1920, 1080),
            Some((960, 540)),
            None,
        );
        assert_eq!((frame.width, frame.height), (960, 540));
        assert_eq!(frame.rgba.len(), 960 * 540 * 4);
        let (width, height, full) = frame.full.expect("the recording-sized frame is kept");
        assert_eq!((width, height), (1920, 1080));
        assert_eq!(full.rgba(width, height).len(), 1920 * 1080 * 4);
    }

    /// t260913-11b4. A 1922x1112 recording full screen on a 1920x1080 display
    /// fits to 1866x1080 -- a 0.97x minification whose resample costs 2.5ms a
    /// frame and buys almost nothing, so it is left to skia.
    #[test]
    fn a_stage_within_five_percent_of_the_recording_is_not_resampled() {
        assert_eq!(scaled_dims((1922, 1112), (1866, 1080)), None);
        assert_eq!(scaled_dims((1920, 1080), (1900, 1070)), None);
        // Exactly on the band's edge counts as near enough.
        assert_eq!(scaled_dims((2000, 1000), (1900, 950)), None);
    }

    /// The band is narrow on purpose: windowed playback -- a ~1200px stage for a
    /// 1922px recording -- is nowhere near it and still gets the good filter,
    /// which is the case task990 added this module for.
    #[test]
    fn a_stage_well_below_the_recording_still_resamples() {
        assert_eq!(scaled_dims((1922, 1112), (1213, 702)), Some((1213, 702)));
        assert_eq!(scaled_dims((2560, 1440), (1200, 675)), Some((1200, 675)));
        // Just outside the band.
        assert_eq!(scaled_dims((2000, 1000), (1880, 940)), Some((1880, 940)));
        // One side inside the band is not enough; both have to be -- and the
        // box that says so is fitted to the source's aspect first, so what
        // comes back is 0.8 on *both* sides rather than the stretched box.
        assert_eq!(scaled_dims((2000, 1000), (1990, 800)), Some((1600, 800)));
    }

    /// The first frame of a clip is resampled for a box laid out from the
    /// *previous* clip's aspect (16:9 straight after launch), because the real
    /// one arrives with a frame. The picture must still come back in the
    /// recording's own shape -- otherwise it draws in the wrong letterbox for
    /// one frame and jumps off the thumbnail under it (2026-09-16).
    #[test]
    fn a_target_box_of_the_wrong_aspect_never_stretches_the_picture() {
        // 1920x1032 (a window's client area) into 16:9: height decides.
        let (width, height) = scaled_dims((1920, 1032), (1213, 682)).expect("worth resampling");
        let ratio = f64::from(width) / f64::from(height);
        assert!((ratio - 1920.0 / 1032.0).abs() < 0.01, "{width}x{height}");
        assert!(
            height <= 682 && width <= 1213,
            "{width}x{height} fits the box"
        );
    }

    /// No stage size yet, or a stage at least as big as the recording: the
    /// frame goes out as decoded, and `full` stays empty to say so.
    #[test]
    fn a_frame_with_nothing_to_gain_is_published_as_decoded() {
        for target in [None, Some((1920, 1080)), Some((2560, 1440))] {
            let frame = frame_for_stage(
                &mut FrameScaler::default(),
                opaque(1920, 1080),
                (1920, 1080),
                target,
                None,
            );
            assert_eq!((frame.width, frame.height), (1920, 1080));
            assert_eq!(frame.rgba.len(), 1920 * 1080 * 4);
            assert!(frame.full.is_none(), "nothing was resampled for {target:?}");
        }
    }

    /// Task1690: the returned destination is the one that gets published, not
    /// a fresh allocation next to it -- which is the entire point of passing
    /// it in. Identity, since the content is `livia-pixels`' own test.
    #[test]
    fn the_stage_frame_is_written_into_the_returned_destination() {
        let spare = opaque(960, 540);
        let address = spare.as_ptr();
        let frame = frame_for_stage(
            &mut FrameScaler::default(),
            opaque(1920, 1080),
            (1920, 1080),
            Some((960, 540)),
            Some(spare),
        );
        assert_eq!(frame.rgba.as_ptr(), address, "the spare was written again");
    }

    #[test]
    fn a_stage_smaller_than_the_recording_is_scaled_to_the_stage() {
        assert_eq!(scaled_dims((2560, 1440), (1200, 675)), Some((1200, 675)));
        // This used to assert that a 1px shrink -- `(1920, 1080)` into
        // `(1919, 1079)` -- resamples. t260913-11b4 made it not: a minification
        // that close to 1:1 costs 2.5ms and changes almost nothing, so it is
        // left to skia. `a_stage_within_five_percent_of_the_recording_is_not_resampled`
        // is where that case lives now.
        // `(1800, 1000)` is not 16:9, so the box is fitted to the recording's
        // aspect before it is used: height decides, and 1080 -> 1000 puts the
        // width at 1778 rather than stretching the picture to 1800 (2026-09-16).
        assert_eq!(scaled_dims((1920, 1080), (1800, 1000)), Some((1778, 1000)));
    }

    /// Magnification stays skia's problem: the pixels are not there to invent,
    /// and resampling up before drawing only costs a copy.
    #[test]
    fn a_stage_at_or_above_the_recording_is_left_alone() {
        assert_eq!(scaled_dims((1280, 720), (1280, 720)), None);
        assert_eq!(scaled_dims((1280, 720), (1920, 1080)), None);
        assert_eq!(scaled_dims((1280, 720), (1281, 721)), None);
    }

    /// A stage mid-layout (or a window being restored) reports nothing to draw
    /// into; a zero-sized resize target is not a size, it is a crash.
    #[test]
    fn a_degenerate_target_is_left_alone() {
        assert_eq!(scaled_dims((1280, 720), (0, 0)), None);
        assert_eq!(scaled_dims((1280, 720), (640, 0)), None);
        assert_eq!(scaled_dims((1280, 720), (0, 360)), None);
    }

    /// Rounding can leave one axis a pixel over while the other shrinks. That
    /// is still an upscale on that axis, so it stays on the untouched path
    /// rather than stretching the frame.
    #[test]
    fn a_target_that_grows_either_axis_is_left_alone() {
        assert_eq!(scaled_dims((1280, 720), (1279, 721)), None);
        assert_eq!(scaled_dims((1280, 720), (1281, 719)), None);
    }
}
