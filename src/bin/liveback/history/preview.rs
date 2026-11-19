//! The hover preview (2026-10-12): a row the pointer rests on loops the first
//! six seconds of its recording, muted, on its own picture.
//!
//! One preview at a time. The pointer moving off the row, onto another, or the
//! page going away cancels it; the thread checks the flag between frames.
//! Shaped like the clip storyboard (task3540) -- a thread per hover, frames
//! rung to the UI as they land -- but it borrows nothing from the review
//! engine, whose state outlives its pane (`clip_player`'s header says why).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use livia::capture::CaptureController;
use livia::ui_state::clip_player::STORYBOARD_DWELL;
use livia::ui_state::sessions::{opening_segments, HOVER_PREVIEW_SPAN_100NS};
use livia::ui_state::shell::Pane;
use slint::ComponentHandle;

use super::super::{image_from_rgba, AppWindow, SessionsVm};
use super::History;

/// The largest frame handed to the UI. Twice the 160x90 thumbnail, which is
/// enough for a tile at 150%; a list row's 96x54 face is smaller still.
const FRAME_MAX: (u32, u32) = (320, 180);

/// 15fps: a preview does not need the recording's own rate, and every frame
/// kept is a copy on the UI thread and 230KB held for the loop.
const FRAME_STEP_100NS: i64 = 666_666;

/// A decoded frame: width, height, RGBA.
type Frame = (u32, u32, Vec<u8>);

pub(super) fn wire(ui: &AppWindow, history: &Rc<RefCell<History>>, controller: &CaptureController) {
    let weak = ui.as_weak();
    let history = history.clone();
    let controller = controller.clone();
    ui.global::<SessionsVm>().on_preview_hover(move |row| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mut history = history.borrow_mut();
        if let Some(cancel) = history.preview.take() {
            cancel.store(true, Ordering::Release);
        }
        ui.global::<SessionsVm>().set_preview_row(-1);
        let Some(id) = history.row_id(row) else {
            return;
        };
        if !history.state.find(&id).is_some_and(|s| s.readable) {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        history.preview = Some(cancel.clone());
        let weak = weak.clone();
        let controller = controller.clone();
        // The dwell: a pointer sweeping across the list cancels each row
        // before its timer fires, so nothing is read.
        slint::Timer::single_shot(STORYBOARD_DWELL, move || {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            std::thread::spawn(move || preview_thread(controller, id, row, cancel, weak));
        });
    });
}

fn preview_thread(
    controller: CaptureController,
    session_id: String,
    row: i32,
    cancel: Arc<AtomicBool>,
    weak: slint::Weak<AppWindow>,
) {
    livia::com::init_mta();
    let Ok(_runtime) = livia::encoder::MfRuntime::start() else {
        return;
    };
    let Some(segments) = read_opening(&controller, &session_id) else {
        return;
    };
    let show = |at: i64, start: Instant, frame: &Frame| -> bool {
        let due = start + Duration::from_nanos(at.max(0) as u64 * 100);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        let (width, height, rgba) = frame.clone();
        let weak = weak.clone();
        let cancel = cancel.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let vm = ui.global::<SessionsVm>();
            // The pane is an `if` in app.slint: leaving it drops the rows
            // without a hover change, so the frame is what notices.
            if ui.get_active_pane() != Pane::Sessions.index() {
                cancel.store(true, Ordering::Release);
                vm.set_preview_row(-1);
                return;
            }
            vm.set_preview_frame(image_from_rgba(width, height, &rgba).unwrap_or_default());
            vm.set_preview_row(row);
        });
        true
    };

    // First pass: decode at the recording's pace, keeping each frame shown.
    // ponytail: the loop holds every kept frame, ~90 x 230KB = ~20MB while one
    // row is hovered; decode again each pass if that ever matters.
    let mut frames: Vec<(i64, Frame)> = Vec::new();
    let start = Instant::now();
    for (offset, bytes) in segments {
        let reader = livia::playback::segment_byte_stream(&bytes)
            .and_then(|stream| livia::clips::FrameReader::from_byte_stream(&stream));
        let mut reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                tracing::debug!(%error, "hover preview: segment would not open");
                break;
            }
        };
        let mut base = None;
        let mut from = i64::MIN;
        while let Some((timestamp, width, height, rgba)) = reader.next_frame(from, FRAME_MAX) {
            // Segment timestamps can be absolute (hours in); the manifest's
            // offset places the segment, its first frame places the rest.
            let at = offset + timestamp - *base.get_or_insert(timestamp);
            if at >= HOVER_PREVIEW_SPAN_100NS {
                break;
            }
            from = timestamp + FRAME_STEP_100NS;
            let frame = (width, height, rgba);
            if !show(at, start, &frame) {
                return;
            }
            frames.push((at, frame));
        }
    }
    if frames.is_empty() {
        return;
    }
    // Then the loop, from memory. A recording shorter than the span loops
    // what it has, one step after its last frame.
    let length = frames.last().map_or(0, |(at, _)| at + FRAME_STEP_100NS);
    loop {
        let start = Instant::now();
        for (at, frame) in &frames {
            if !show(*at, start, frame) {
                return;
            }
        }
        let due = start + Duration::from_nanos(length as u64 * 100);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
}

/// The opening segments' bytes with their offsets. The lease is held only for
/// the reads: a lease makes `discard_session` refuse the session, and the
/// pointer resting on a row must not be what stops it from being discarded.
fn read_opening(controller: &CaptureController, session_id: &str) -> Option<Vec<(i64, Vec<u8>)>> {
    let manifest = controller.timeline_for(session_id)?;
    let opening = opening_segments(&manifest.segments, HOVER_PREVIEW_SPAN_100NS);
    if opening.is_empty() {
        return None;
    }
    let indexes = opening.iter().map(|(index, _)| *index).collect();
    let lease = match controller.acquire_review_lease(Some(session_id.to_owned()), indexes) {
        Ok(lease) => lease,
        Err(error) => {
            tracing::debug!(%error, "hover preview: no lease");
            return None;
        }
    };
    let read = opening
        .iter()
        .map(|(index, offset)| {
            controller
                .read_review_segment(&lease, *index)
                .map(|bytes| (*offset, bytes))
        })
        .collect::<Result<Vec<_>, _>>();
    controller.release_review_lease(&lease);
    read.map_err(|error| tracing::debug!(%error, "hover preview: read failed"))
        .ok()
}
