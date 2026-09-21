//! Pure state for the slint review timeline (task127). Ported from
//! `src/playback/timeline.ts`, `src/playback/thumbnails.ts` and the coordinate
//! maths embedded in `src/components/ReviewTimeline.tsx` as they stand after
//! round-2 (task118/119). Its unit tests (deleted with the WebView build) were the
//! source for the cases below.
//!
//! Everything here is in 100ns ticks on the session's absolute timeline; the
//! view maps a `Viewport` slice of it onto 0..100% of the track.

use crate::ring_buffer::{SessionManifest, TimelineGap};
use crate::ui_state::locale::Locale;

pub const HNS_PER_SECOND: i64 = 10_000_000;
/// Below ~5s of visible track the scrubber stops being useful as a scrubber.
pub const MIN_ZOOM_DURATION_100NS: i64 = 5 * HNS_PER_SECOND;
pub const ZOOM_STEP: f64 = 1.25;
pub const PAN_STEP_RATIO: f64 = 0.2;
/// Keeps the centered, fixed-width popovers from drawing off the track edge.
pub const HOVER_EDGE_CLAMP_PERCENT: f64 = 8.0;
/// Ordered fastest to slowest -- the menu draws rows in array order and reads
/// top-to-bottom, high to low. `<`/`>` step through and stop at the ends
/// (`shift_playback_rate` flips the sign so `>` still means faster). Round5
/// §4-C cut the 0.75 / 1.25 / 1.75 steps; task238 then dropped 4x -- at four
/// times speed a review is a blur, and the row was costing the menu a sixth of
/// its height for it.
pub const PLAYBACK_RATES: [f64; 5] = [2.0, 1.5, 1.0, 0.5, 0.25];
/// Index of 1.0x in `PLAYBACK_RATES` -- the fallback when a rate isn't listed.
const NORMAL_RATE_INDEX: usize = 2;
/// One frame at 60fps -- the minimum selectable range.
pub fn min_frame_100ns() -> i64 {
    (HNS_PER_SECOND + 59) / 60
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineSegment {
    pub index: u64,
    pub start_100ns: i64,
    pub end_100ns: i64,
    /// Shift applied to each audio track to seat its priming access unit
    /// (task204), one entry per track in track order (task1270). Empty, or a
    /// zero, for recordings made before priming existed.
    pub audio_offsets_100ns: Vec<i64>,
}

/// What a live session gained since the review screen last looked (task162).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistExtension {
    pub segments: Vec<TimelineSegment>,
    pub live_edge_100ns: i64,
}

/// The difference to push into a playing engine while its session is still
/// recording. `None` means nothing moved, which is the common case: the UI
/// drains at 100ms and segments finalize about every two seconds.
///
/// Only growth past the current tail is reported. A resend, a shorter list (the
/// front of a live session can still be pruned) and a snapshot of a *different*
/// session all produce `None` rather than something the engine would have to
/// reorder -- the playlist stays append-only, which is what keeps its binary
/// search valid.
pub fn playlist_extension(
    current: &TimelineSnapshot,
    latest: &TimelineSnapshot,
) -> Option<PlaylistExtension> {
    if current.session_id != latest.session_id {
        return None;
    }
    let tail = current.segments.last().map(|segment| segment.index);
    let segments: Vec<TimelineSegment> = latest
        .segments
        .iter()
        .filter(|segment| tail.is_none_or(|tail| segment.index > tail))
        .cloned()
        .collect();
    if segments.is_empty() && latest.live_edge_100ns <= current.live_edge_100ns {
        return None;
    }
    Some(PlaylistExtension {
        segments,
        live_edge_100ns: latest.live_edge_100ns,
    })
}

#[derive(Clone, Debug, Default)]
pub struct TimelineSnapshot {
    pub session_id: String,
    pub segments: Vec<TimelineSegment>,
    pub gaps: Vec<TimelineGap>,
    pub live_edge_100ns: i64,
    pub target_title: Option<String>,
    /// The executable behind the captured window (task2350). `None` for a
    /// monitor recording and for everything recorded before the container
    /// carried it.
    pub target_executable: Option<String>,
    /// That executable's full image path (2026-09-20). `None` for everything
    /// recorded before the container carried it.
    pub target_executable_path: Option<String>,
    /// The executables this session recorded audio for, in track order
    /// (task1250). Empty for every recording made before multi-track audio,
    /// which is what makes the mixer stay hidden for them.
    pub audio_tracks: Vec<String>,
}

impl TimelineSnapshot {
    /// `manifestToTimeline`: sorts segments by start, live edge = last end.
    pub fn from_manifest(manifest: &SessionManifest) -> Self {
        let mut segments: Vec<TimelineSegment> = manifest
            .segments
            .iter()
            .map(|segment| TimelineSegment {
                index: segment.index,
                start_100ns: segment.start_100ns,
                end_100ns: segment.end_100ns,
                audio_offsets_100ns: segment.audio_offsets_100ns.clone(),
            })
            .collect();
        segments.sort_by_key(|segment| segment.start_100ns);
        Self {
            session_id: manifest.session_id.clone(),
            live_edge_100ns: segments.last().map(|s| s.end_100ns).unwrap_or(0),
            segments,
            gaps: manifest.gaps.clone(),
            target_title: manifest.target_title.clone(),
            target_executable: manifest.target_executable.clone(),
            target_executable_path: manifest.target_executable_path.clone(),
            audio_tracks: manifest
                .audio_tracks
                .iter()
                .map(|track| track.executable_name.clone())
                .collect(),
        }
    }

    pub fn start_100ns(&self) -> i64 {
        self.segments.first().map(|s| s.start_100ns).unwrap_or(0)
    }

    /// `toTimelineView().duration100ns`.
    pub fn full_duration_100ns(&self) -> i64 {
        (self.live_edge_100ns - self.start_100ns()).max(0)
    }

    /// 前方スナップ: 指定時刻が gap/prune 範囲なら次に記録済みの時刻へ進める。
    pub fn snap_to_recorded_time(&self, time_100ns: i64) -> Option<i64> {
        if self
            .segments
            .iter()
            .any(|s| time_100ns >= s.start_100ns && time_100ns < s.end_100ns)
        {
            return Some(time_100ns);
        }
        self.segments
            .iter()
            .find(|s| s.start_100ns >= time_100ns)
            .map(|s| s.start_100ns)
    }

    pub fn clamp_range(&self, start: i64, end: i64, min_frame_100ns: i64) -> Option<(i64, i64)> {
        let start = self.snap_to_recorded_time(start)?;
        let end = self.snap_to_recorded_time(end)?;
        (end - start >= min_frame_100ns).then_some((start, end))
    }

    pub fn next_range(
        &self,
        current: Option<(i64, i64)>,
        follows_live: bool,
        min_frame_100ns: i64,
    ) -> (i64, i64) {
        let live_range = (self.start_100ns(), self.live_edge_100ns - 1);
        if follows_live {
            return live_range;
        }
        let (start, end) = current.unwrap_or((0, 0));
        self.clamp_range(start, end, min_frame_100ns)
            .unwrap_or(live_range)
    }

    /// `None` seconds = 全体.
    pub fn quick_range(&self, seconds: Option<f64>, min_frame_100ns: i64) -> Option<(i64, i64)> {
        let start = match seconds {
            None => self.start_100ns(),
            Some(seconds) => {
                self.live_edge_100ns - (seconds * HNS_PER_SECOND as f64).round() as i64
            }
        };
        self.clamp_range(start, self.live_edge_100ns - 1, min_frame_100ns)
    }

    /// `toTimelineView`'s marker filter: only markers that land on recorded
    /// time survive, unmapped -- the surviving `time_100ns` stays the exact
    /// backend identity.
    pub fn recorded_markers(&self, markers_100ns: &[i64]) -> Vec<i64> {
        markers_100ns
            .iter()
            .copied()
            .filter(|marker| self.snap_to_recorded_time(*marker).is_some())
            .collect()
    }

    /// `findCoveringSegment`: exact cover or nothing -- deliberately NOT the
    /// forward snap, so a hover inside a gap resolves to the placeholder
    /// instead of an unrelated later segment. Binary search: segments are
    /// sorted and never overlap, and a long session has hundreds of them to
    /// answer on every pointer move.
    pub fn find_covering_segment(&self, time_100ns: i64) -> Option<&TimelineSegment> {
        let segments = &self.segments;
        let mut low = 0isize;
        let mut high = segments.len() as isize - 1;
        while low <= high {
            let mid = ((low + high) >> 1) as usize;
            let segment = &segments[mid];
            if time_100ns < segment.start_100ns {
                high = mid as isize - 1;
            } else if time_100ns >= segment.end_100ns {
                low = mid as isize + 1;
            } else {
                return Some(segment);
            }
        }
        None
    }
}

// ---------- viewport (zoom/pan, task103) ----------

/// The slice of the session the track currently maps onto 0..100%. Non-zoomed
/// it is the whole recording and every formula reduces to the pre-zoom one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub start_100ns: i64,
    pub duration_100ns: i64,
}

impl Viewport {
    pub fn full(snapshot: &TimelineSnapshot) -> Self {
        Self {
            start_100ns: snapshot.start_100ns(),
            duration_100ns: snapshot.full_duration_100ns().max(1),
        }
    }

    pub fn end_100ns(&self) -> i64 {
        self.start_100ns + self.duration_100ns
    }

    pub fn percent(&self, value_100ns: i64) -> f64 {
        (value_100ns - self.start_100ns) as f64 / self.duration_100ns.max(1) as f64 * 100.0
    }

    /// For anything drawn as a box: clamped so a partly-overlapping element
    /// draws its visible part instead of overflowing the track.
    pub fn clamped_percent(&self, value_100ns: i64) -> f64 {
        self.percent(value_100ns).clamp(0.0, 100.0)
    }

    pub fn contains(&self, value_100ns: i64) -> bool {
        value_100ns >= self.start_100ns && value_100ns <= self.end_100ns()
    }

    pub fn overlaps(&self, start_100ns: i64, end_100ns: i64) -> bool {
        end_100ns >= self.start_100ns && start_100ns <= self.end_100ns()
    }

    /// Raw pointer ratio -> time, no snapping. Rounded because 100ns is the
    /// quantum everywhere else: without it a deep zoom lands on
    /// 719_999_999.98 and the clock reads a second early.
    pub fn time_from_ratio(&self, ratio: f64) -> i64 {
        let ratio = ratio.clamp(0.0, 1.0);
        self.start_100ns + (ratio * self.duration_100ns as f64).round() as i64
    }
}

/// Ctrl+wheel about the cursor. `None` = zoomed all the way out, drop the zoom
/// state entirely. `zoom_in` is deltaY < 0 in the React handler.
pub fn zoom_at(
    snapshot: &TimelineSnapshot,
    current: Viewport,
    cursor_ratio: f64,
    zoom_in: bool,
) -> Option<Viewport> {
    let full = Viewport::full(snapshot);
    let duration = current.duration_100ns.max(1) as f64;
    let anchor = current.start_100ns as f64 + cursor_ratio.clamp(0.0, 1.0) * duration;
    let next = (duration * if zoom_in { 1.0 / ZOOM_STEP } else { ZOOM_STEP })
        .max(MIN_ZOOM_DURATION_100NS as f64)
        .min(full.duration_100ns as f64);
    if next >= full.duration_100ns as f64 {
        return None;
    }
    // Keep `anchor` under the cursor: its offset into the viewport scales with
    // the viewport itself.
    let start = anchor - (anchor - current.start_100ns as f64) * (next / duration);
    let next = next.round() as i64;
    Some(Viewport {
        start_100ns: (start.round() as i64).clamp(
            full.start_100ns,
            full.start_100ns + full.duration_100ns - next,
        ),
        duration_100ns: next,
    })
}

/// Shift+wheel: one step is 20% of the visible span, clamped to the recording.
pub fn pan(snapshot: &TimelineSnapshot, current: Viewport, direction: i64) -> Viewport {
    let full = Viewport::full(snapshot);
    let step = (current.duration_100ns as f64 * PAN_STEP_RATIO) as i64 * direction.signum();
    Viewport {
        // `.max(...)`: a viewport wider than the full timeline (a stale zoom
        // against a snapshot that shrank) would invert the clamp bounds, and
        // `clamp` panics on min > max.
        start_100ns: (current.start_100ns + step).clamp(
            full.start_100ns,
            (full.start_100ns + full.duration_100ns - current.duration_100ns).max(full.start_100ns),
        ),
        duration_100ns: current.duration_100ns,
    }
}

/// Popover anchors: clamped into [8, 92] so a centered fixed-width popover
/// stays on the track.
pub fn popover_anchor_percent(percent: f64) -> f64 {
    percent.clamp(HOVER_EDGE_CLAMP_PERCENT, 100.0 - HOVER_EDGE_CLAMP_PERCENT)
}

// ---------- track model ----------

/// A span on the track in percent of its width. `width` keeps React's
/// `Math.max(0.5, ...)` floor so a 2s segment in a long session stays visible.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackBox {
    pub left: f32,
    pub width: f32,
}

fn track_box(viewport: &Viewport, start_100ns: i64, end_100ns: i64) -> TrackBox {
    let left = viewport.clamped_percent(start_100ns);
    let width = (viewport.clamped_percent(end_100ns) - left).max(0.5);
    TrackBox {
        left: left as f32,
        width: width as f32,
    }
}

/// Only what overlaps the viewport is emitted, so a zoomed track's element
/// count is bounded by what is visible.
pub fn segment_boxes(snapshot: &TimelineSnapshot, viewport: &Viewport) -> Vec<TrackBox> {
    snapshot
        .segments
        .iter()
        .filter(|s| viewport.overlaps(s.start_100ns, s.end_100ns))
        .map(|s| track_box(viewport, s.start_100ns, s.end_100ns))
        .collect()
}

pub fn gap_boxes(snapshot: &TimelineSnapshot, viewport: &Viewport) -> Vec<TrackBox> {
    snapshot
        .gaps
        .iter()
        .filter(|g| viewport.overlaps(g.start_100ns, g.end_100ns))
        .map(|g| track_box(viewport, g.start_100ns, g.end_100ns))
        .collect()
}

// ---------- gap marks (round5 §4-B) ----------

/// The hatch is gone: a gap is now the track's own background cut away, with a
/// 2px rule down each edge. Below 4px there is no room for two rules and the
/// space between them, so the pair collapses into one -- drawing them anyway
/// produced a 4px smear that read as a wide gap rather than a seam.
pub const GAP_MARK_RULE_PX: f32 = 2.0;
pub const GAP_MARK_MIN_WIDTH_PX: f32 = 4.0;

/// What to draw for one gap, given how wide it lands on this track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GapMark {
    /// Two rules with the cut-away span between them.
    Edges,
    /// One rule, centred on the gap.
    Single,
}

pub fn gap_mark(width_px: f32) -> GapMark {
    if width_px < GAP_MARK_MIN_WIDTH_PX {
        GapMark::Single
    } else {
        GapMark::Edges
    }
}

/// What the selection should become after a live session's timeline grew,
/// given which range preset is currently showing. `None` means "leave the
/// range alone" (task390).
///
/// `quick` is the panel's chip code: 0 is 全体, a positive value is 直近N
/// seconds, and -1 means the user dragged the in/out handles themselves.
/// Only that last one is left alone -- those edges are the user's, and
/// stretching them because a segment finalized would be taking them away.
///
/// The two that do follow matter because the selection is also what clamps
/// seeking: a live session opened at 0:01 kept a 0:01 range while it grew past
/// a minute, so clicking the right of the seekbar or pressing `End` landed
/// somewhere in the first second and the only way out was to press 全体に戻す.
pub fn range_after_growth(
    snapshot: &TimelineSnapshot,
    quick: i32,
    min_frame_100ns: i64,
) -> Option<(i64, i64)> {
    match quick {
        // `next_range`, not `quick_range`: it is total, and this is also the
        // call a load makes, so a timeline that was empty when it was opened
        // gets exactly the range `load_review` would have given it.
        0 => Some(snapshot.next_range(None, true, min_frame_100ns)),
        seconds if seconds > 0 => snapshot.quick_range(Some(f64::from(seconds)), min_frame_100ns),
        _ => None,
    }
}

/// The same choice, made from the gap's share of the track rather than from
/// pixels, so a track whose width has not been reported yet is a case of its
/// own instead of a very narrow track (task380).
///
/// The track tells Rust its width through a `changed width` callback, which
/// says nothing on the first paint -- so `track_width_px` is 0 while a
/// just-loaded session draws for the first time, and every gap on it, however
/// wide, measured under `GAP_MARK_MIN_WIDTH_PX` and collapsed to one rule. A
/// 33-minute session with a 23-minute hole showed a 2px line until the window
/// was resized.
///
/// Zero means "not known yet", and the honest answer there is `Edges`: it
/// draws the gap at the width the percentage already gives, and the only cost
/// of being wrong is that a hair-thin gap shows its cut-away for one frame
/// before the real width arrives. Collapsing is the answer that cannot be
/// corrected by looking at it.
pub fn gap_mark_on_track(width_percent: f32, track_width_px: f32) -> GapMark {
    if track_width_px <= 0.0 {
        GapMark::Edges
    } else {
        gap_mark(width_percent / 100.0 * track_width_px)
    }
}

// ---------- markers (round5 §4-B) ----------

/// Which palette slot a marker wears. A marker written since task171 carries
/// its own index, assigned in creation order, so it survives a reload.
///
/// A marker written before then has none, and falls back to its position in the
/// list -- stable across restarts because the list order is, though deleting an
/// older legacy marker will shift the ones after it. New markers are immune to
/// that, which is why the stored index exists at all.
pub fn marker_palette_index(stored: Option<u8>, position: usize) -> usize {
    match stored {
        Some(index) => index as usize % crate::MARKER_PALETTE_LEN,
        None => position % crate::MARKER_PALETTE_LEN,
    }
}

// ---------- selection range (round5 §4-D) ----------

/// The three numbers the right panel's range block shows. Separate strings, not
/// one line: the block draws 開始 / 終了 in two cells with 長さ on its own row
/// underneath, and a single formatted string could not be laid out that way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeValues {
    pub start: String,
    pub end: String,
    pub length: String,
}

/// `None` = no selection, which the panel shows as em dashes rather than
/// zeroes: `0:00` would read as a selection that happens to be empty.
pub fn range_values(
    range: Option<(i64, i64)>,
    origin_100ns: i64,
    force_hours: bool,
) -> RangeValues {
    let Some((start, end)) = range else {
        // Not localized in practice -- an em dash is an em dash -- but it goes
        // through the table so a locale that wants something else can say so.
        let unset = range_unset(Locale::Ja);
        return RangeValues {
            start: unset.into(),
            end: unset.into(),
            length: unset.into(),
        };
    };
    RangeValues {
        start: format_range_time(start - origin_100ns, force_hours),
        end: format_range_time(end - origin_100ns, force_hours),
        length: format_range_time((end - start).max(0), force_hours),
    }
}

/// The panel's own clock (round8 §2). It carries tenths where the transport's
/// does not: ±10s from a boundary that already sits mid-second, and a typed
/// `mm:ss.s`, both land off a whole second -- rounded to seconds the display
/// would sit still while the range moved under it.
pub fn format_range_time(value_100ns: i64, force_hours: bool) -> String {
    let value = value_100ns.max(0);
    let tenths = value / (HNS_PER_SECOND / 10);
    let total_seconds = tenths / 10;
    let hours = total_seconds / 3600;
    if hours > 0 || force_hours {
        format!(
            "{}:{:02}:{:02}.{}",
            hours,
            total_seconds % 3600 / 60,
            total_seconds % 60,
            tenths % 10
        )
    } else {
        format!(
            "{}:{:02}.{}",
            total_seconds / 60,
            total_seconds % 60,
            tenths % 10
        )
    }
}

/// The inverse, for the panel's typed edit. `m:ss`, `m:ss.s` and `h:mm:ss.s`
/// all parse; anything else is `None` and the caller puts the old text back
/// (round8 §2: 不正値は復元). Deliberately strict about the shape -- a bare
/// `90` could mean 90 seconds or 90 minutes, and guessing would be worse than
/// refusing.
pub fn parse_range_time(text: &str) -> Option<i64> {
    let text = text.trim();
    let mut parts = text.split(':');
    let (hours, minutes, seconds) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(minutes), Some(seconds), None, None) => (0i64, minutes, seconds),
        (Some(hours), Some(minutes), Some(seconds), None) => (
            hours.parse::<i64>().ok().filter(|h| *h >= 0)?,
            minutes,
            seconds,
        ),
        _ => return None,
    };
    let minutes: i64 = minutes.parse().ok().filter(|m| *m >= 0)?;
    // `.parse::<f64>()` would take `1e2`, `inf` and a leading `+`. The seconds
    // field is two digits and an optional tenth, nothing else.
    let (whole, tenths) = match seconds.split_once('.') {
        None => (seconds, 0i64),
        Some((whole, frac)) => {
            if frac.is_empty() || !frac.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            // Past the tenth the display cannot show it, so it is truncated
            // rather than refused -- pasting a `.35` from somewhere else
            // should land on `.3`, not put the old value back.
            (whole, i64::from(frac.as_bytes()[0] - b'0'))
        }
    };
    if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let seconds: i64 = whole.parse().ok()?;
    if seconds >= 60 || (hours > 0 && minutes >= 60) {
        return None;
    }
    Some(((hours * 3600 + minutes * 60 + seconds) * 10 + tenths) * (HNS_PER_SECOND / 10))
}

/// Slides the whole range to a new start, keeping its length (round8 §2: the
/// band's middle drags at a fixed length). Runs out of road at either end of
/// the recording rather than shrinking, and refuses a move whose landing place
/// is not recorded time.
pub fn pan_range(
    snapshot: &TimelineSnapshot,
    range: (i64, i64),
    to_start_100ns: i64,
    min_frame_100ns: i64,
) -> Option<(i64, i64)> {
    let length = range.1 - range.0;
    let low = snapshot.start_100ns();
    let high = snapshot.live_edge_100ns - 1;
    if high - low < length {
        return None;
    }
    let start = to_start_100ns.clamp(low, high - length);
    snapshot.clamp_range(start, start + length, min_frame_100ns)
}

/// Which end of the range an edit moves. The panel's third row is a readout,
/// not a boundary, so there are only two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeEdge {
    Start,
    End,
}

/// Where the playhead has to stand after a range edit, or `None` to leave it
/// where it is (task3160).
///
/// `edge` says which gesture asked: `Some(Start)` / `Some(End)` is a boundary
/// being moved -- by a handle drag, a stepper or a typed time -- and follows
/// only 通り過ぎたとき, i.e. when `position_100ns` has fallen outside the new
/// range. `None` is the whole band being panned, which always shows its start:
/// a move carries the selection somewhere else entirely, so the useful frame
/// is its first one whether or not the playhead happened to stay inside.
///
/// Inclusive on both boundaries: standing exactly on `start` or `end` is
/// inside. That is what makes a continuing drag follow -- each move puts the
/// playhead *on* the boundary, and the next move leaves it just outside again.
pub fn range_follow(
    range: (i64, i64),
    position_100ns: i64,
    edge: Option<RangeEdge>,
) -> Option<i64> {
    let (start, end) = range;
    let Some(edge) = edge else {
        return Some(start);
    };
    if (start..=end).contains(&position_100ns) {
        return None;
    }
    Some(match edge {
        RangeEdge::Start => start,
        RangeEdge::End => end,
    })
}

/// Whether playback may pick up again where a range edit left the playhead
/// (task3390).
///
/// task3160 answered this from the *gesture*: 終了端 never resumed, the other
/// two always did. The user asked on 2026-09-07 for an end edit that leaves the
/// playhead inside the selection to keep playing, and once the question is
/// asked about the position [`range_follow`] settled on instead, all three
/// gestures collapse into one rule -- 終了に達していないなら再開する:
///
/// - 開始端: outside means the playhead is pulled to `start`, short of the end.
/// - 移動: always parked on `start`, short of the end.
/// - 終了端 with the playhead still inside: it does not move, so it is short of
///   the end unless it was already standing on it.
/// - 終了端 with the playhead pushed out: it lands *on* `end`, and resuming
///   there would only flicker -- the next drained frame's
///   [`crate::ui_state::playback::range_guard`] answers `PauseAtEnd` and stops
///   it again. That is the one case task3160's 「終了位置で止まったまま」 was
///   really about, and it survives unchanged.
///
/// Note the asymmetry with [`range_follow`], which counts `end` as *inside* so
/// that a continuing drag keeps following: for resuming, standing on `end` is
/// the end reached.
///
/// A degenerate (empty) selection resumes, for the same reason `range_guard`
/// returns `None` for one: it has no inside, so nothing would stop playback
/// anyway. Callers always hold a real range here -- the argument is taken so
/// the case is testable rather than assumed.
pub fn may_resume_after_range_edit(range: (i64, i64), position_100ns: i64) -> bool {
    let (start, end) = range;
    if start >= end {
        return true;
    }
    position_100ns < end
}

/// Moves one boundary and keeps the pair legal: inside the recording, at least
/// `min_frame_100ns` apart, and snapped to recorded time. `None` means the
/// move was impossible and the caller should leave the range alone (round8 §2:
/// 開始<終了を常にクランプ).
pub fn move_range_edge(
    snapshot: &TimelineSnapshot,
    range: (i64, i64),
    edge: RangeEdge,
    to_100ns: i64,
    min_frame_100ns: i64,
) -> Option<(i64, i64)> {
    let (start, end) = range;
    let low = snapshot.start_100ns();
    // Exclusive, as everywhere else: the tick past the edge has no frame.
    let high = snapshot.live_edge_100ns - 1;
    if high - low < min_frame_100ns {
        return None;
    }
    let moved = match edge {
        RangeEdge::Start => (
            to_100ns.clamp(low, (end - min_frame_100ns).max(low)),
            end.max(low + min_frame_100ns),
        ),
        RangeEdge::End => (
            start.min(high - min_frame_100ns),
            to_100ns.clamp((start + min_frame_100ns).min(high), high),
        ),
    };
    snapshot.clamp_range(moved.0, moved.1, min_frame_100ns)
}

/// Half the window a marker row's 「前後30秒を範囲に」 opens (task3050). Fixed,
/// with no setting behind it: the playhead buttons and `i`/`o` are how a rough
/// window gets tightened, so a number to tune would be a third way to say the
/// same thing.
pub const MARKER_RANGE_HALF_SECONDS: i64 = 30;

/// The range a marker asks for: `MARKER_RANGE_HALF_SECONDS` either side of it,
/// with the ends of the recording winning. A recording shorter than the window
/// yields the whole of it rather than nothing.
pub fn marker_range(
    snapshot: &TimelineSnapshot,
    marker_100ns: i64,
    min_frame_100ns: i64,
) -> Option<(i64, i64)> {
    let low = snapshot.start_100ns();
    let high = snapshot.live_edge_100ns - 1;
    let half = MARKER_RANGE_HALF_SECONDS * HNS_PER_SECOND;
    snapshot.clamp_range(
        (marker_100ns - half).max(low),
        (marker_100ns + half).min(high),
        min_frame_100ns,
    )
}

/// The three 直近N chips above 「クリップに保存」 (design round19 §2-3). Kept in
/// the order they are drawn, shortest first, which is also the mock's.
pub const RECENT_CHIP_SECONDS: [i64; 3] = [15, 30, 60];

/// A chip's range: the end is where the playhead is standing, the start is `N`
/// seconds before it. Deliberately unclamped by the recording -- the caller
/// runs it through `TimelineSnapshot::clamp_range`, which is the one place that
/// knows about gaps, the live edge and the minimum frame. That split is also
/// what lets the boundary cases below be stated at all: a playhead less than
/// `N` seconds in gives `0..position`, and a playhead at 0 gives a zero-length
/// range that the clamp then refuses.
///
/// The end is the playhead rather than the session's tail: 「見ながらここまで」
/// is the grammar round19 §2-3 asks these to join, and it is the same one the
/// range block's 「終了を再生位置に合わせる」 already uses.
pub fn recent_range(position_100ns: i64, seconds: i64) -> (i64, i64) {
    (
        (position_100ns - seconds * HNS_PER_SECOND).max(0),
        position_100ns,
    )
}

// ---------- playback rate ----------

/// `<`/`>`: steps and stops at the ends, so a held `>` can't wrap back to
/// 0.25x mid-review. `direction > 0` means faster; the array is fastest-first,
/// so faster is a smaller index.
pub fn shift_playback_rate(current: f64, direction: i64) -> f64 {
    let index = rate_index(current);
    let next = (index as i64 - direction.signum()).clamp(0, PLAYBACK_RATES.len() as i64 - 1);
    PLAYBACK_RATES[next as usize]
}

pub fn rate_index(rate: f64) -> usize {
    PLAYBACK_RATES
        .iter()
        .position(|candidate| *candidate == rate)
        .unwrap_or(NORMAL_RATE_INDEX)
}

/// Menu rows for the rate chip's popup, in `PLAYBACK_RATES` order.
pub fn rate_labels() -> Vec<String> {
    PLAYBACK_RATES
        .iter()
        .copied()
        .map(playback_rate_text)
        .collect()
}

// ---------- formatting / strings ----------

/// `formatDuration`: `M:SS` under an hour, `H:MM:SS` from one hour on,
/// floored, never negative.
pub fn format_duration(value_100ns: i64) -> String {
    format_position(value_100ns, false)
}

/// `force_hours` keeps the position column aligned with an over-an-hour
/// total (YouTube prints `0:05:00 / 1:20:00`, not `5:00 / 1:20:00`).
pub fn format_position(value_100ns: i64, force_hours: bool) -> String {
    let total_seconds = (value_100ns / HNS_PER_SECOND).max(0);
    let hours = total_seconds / 3600;
    if hours > 0 || force_hours {
        format!(
            "{}:{:02}:{:02}",
            hours,
            total_seconds % 3600 / 60,
            total_seconds % 60
        )
    } else {
        format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
    }
}

/// Countdown shown when the clock is toggled to remaining time.
pub fn format_remaining(total_100ns: i64, position_100ns: i64, force_hours: bool) -> String {
    format!(
        "−{}",
        format_position((total_100ns - position_100ns).max(0), force_hours)
    )
}

/// `0`..`9` jump to that tenth of the recording, like YouTube's digit keys.
pub fn digit_jump_100ns(snapshot: &TimelineSnapshot, digit: u32) -> i64 {
    snapshot.start_100ns() + snapshot.full_duration_100ns() * digit.min(9) as i64 / 10
}

/// Where the transport's two jumps land (task240): the selection's own edges
/// when a range is set, and the whole session's otherwise. Moving the playhead
/// only -- neither jump touches the range it is reading.
pub fn range_jump_targets(snapshot: &TimelineSnapshot, range: Option<(i64, i64)>) -> (i64, i64) {
    match range {
        Some((start, end)) => (start, end),
        // The live edge, not the tick past it: `live_edge_100ns` is exclusive
        // and seeking to it lands on nothing recorded.
        None => (snapshot.start_100ns(), snapshot.live_edge_100ns - 1),
    }
}

crate::tr! {
    add_marker { ja: "マーカー追加", en: "Add marker" }
    /// W-14 (round5 §11): the name cell of a marker nobody has named. The old
    /// `ラベルを入力` belonged to a popover with a text field already open;
    /// the panel's row is not in edit mode until you double-click it, so it
    /// asks.
    marker_label_placeholder { ja: "名前を付ける", en: "Give it a name" }
    marker_delete { ja: "削除", en: "Delete" }
    marker_save_error {
        ja: "マーカーのラベルを保存できませんでした。",
        en: "The marker's label could not be saved."
    }
    marker_delete_error {
        ja: "マーカーを削除できませんでした。",
        en: "The marker could not be deleted."
    }
    marker_add_error {
        ja: "マーカーを追加できませんでした。",
        en: "The marker could not be added."
    }
    /// task3060 (design round18 §4-7): renamed from 「静止画を保存」. Sitting under
    /// the range block it read as "a still *of the selection*"; it has always
    /// written the frame on screen (`save_screenshot_png`), so the label says so.
    screenshot { ja: "表示中のフレームを保存", en: "Save the frame on screen" }
    screenshot_no_frame {
        ja: "表示中のフレームがありません。",
        en: "There is no frame on screen."
    }
    export_selection { ja: "選択範囲を書き出す", en: "Export the selection" }
    /// Task2970: the primary of the pair. It opens no file dialog -- that is
    /// the whole difference from the row below it, so the wording names the
    /// destination rather than the act of writing a file.
    clip_save { ja: "クリップに保存", en: "Save as a clip" }
    /// The hint inside the comment field the button turns into. Not a
    /// placeholder: `InlineEdit` draws `hint` as a trailing label beside the
    /// input (`ui/controls.slint`), so whatever stands here is subtracted from
    /// the typing width for as long as it is shown. task3180 therefore has
    /// `ui/review.slint` blank it on the first keystroke -- the guidance is
    /// only needed while the field is empty, and the field gets its full width
    /// back the moment there is something to read in it.
    ///
    /// It has to carry three of the four things the user cannot otherwise see
    /// (what to type / that it is optional / Enter / Esc); the fourth, the
    /// context that this is 「クリップに保存」, used to come from the section
    /// heading, which `ui/review.slint` swapped to the button's own label while
    /// the field was open; task3410 fixed that heading at 「動画の出力」, so the
    /// context the field carries is now just the section's, not the button's.
    /// Measured against the 251px panel column at 9.5px:
    /// ja 176.1px, en 197.7px, budget 233px (251 - 12 padding - 6 spacing).
    clip_comment_hint {
        ja: "コメント（任意） Enterで保存 Escで取消",
        en: "Comment (optional) Enter saves, Esc cancels"
    }

    /// W-16 (round5 §11): the right panel's first section. It replaced 書き出し
    /// -- the section is the operations that *decide* a range, and the export
    /// is what comes out the far end of them.
    panel_range_section { ja: "範囲選択", en: "Selection" }
    /// task3060 (design round18 §4-7): the two buttons that write a video file
    /// out of the selection, plus the progress row that replaces them. Split
    /// off from the still below so the two are not read as one group.
    ///
    /// task3410 (user, 2026-09-07) widened the wording from 「選択範囲の書き出し」.
    /// The heading has to cover everything the section can be showing: the
    /// primary that writes straight to the clip folder, the secondary that goes
    /// through the save dialog (task1050), the progress row, *and* the comment
    /// field the primary turns into. It used to be swapped out for the clip
    /// button's label while that field was open (task3180); the heading is
    /// fixed now, so it has to read as true in every one of those states --
    /// hence the act ("the video output") rather than the object of it. It also
    /// ends the collision with `export_selection`, which had the same en string.
    panel_export_section { ja: "動画の出力", en: "Video export" }
    /// task3060: the still, which is about the current frame and not the range.
    panel_still_section { ja: "静止画", en: "Still image" }
    panel_marker_section { ja: "マーカー", en: "Markers" }
    /// W-5: the marker list before there is anything in it.
    markers_empty { ja: "マーカーはまだありません", en: "No markers yet" }
    range_start_label { ja: "開始", en: "Start" }
    range_end_label { ja: "終了", en: "End" }
    range_length_label { ja: "長さ", en: "Length" }
    /// round8 §2: the steppers and the reset are icon-only, so the wording
    /// that used to be on the quick chips lives in their tooltips instead.
    range_step_start_back { ja: "開始を 10 秒戻す", en: "Move the start back 10s" }
    range_step_start_forward { ja: "開始を 10 秒進める", en: "Move the start on 10s" }
    range_step_end_back { ja: "終了を 10 秒戻す", en: "Move the end back 10s" }
    range_step_end_forward { ja: "終了を 10 秒進める", en: "Move the end on 10s" }
    /// task3050 (design round18 §4-6): the third button in the tail column,
    /// which copies the playhead onto that boundary. `i` / `o` are the same
    /// action from the keyboard.
    range_playhead_start { ja: "開始を再生位置に合わせる", en: "Set the start to the playhead" }
    range_playhead_end { ja: "終了を再生位置に合わせる", en: "Set the end to the playhead" }
    /// task3050: the marker row's second button.
    marker_range_hint { ja: "前後30秒を範囲に", en: "Select 30s around this marker" }
    /// The way back, and the only preset that survived the chips.
    range_reset_hint { ja: "全選択", en: "Select all" }
    // `range_edit_label` (「範囲を編集」) and `range_edit_hint` stood here for
    // task1060's toggle. task3860 (round19 §1) folded that mode into the
    // 確定ステップ, so there is no control left to label and no tip left to
    // explain it -- the step's own hint carries what the user needs to know.
    /// What the range block shows with nothing selected.
    range_unset { ja: "—", en: "—" }
    panel_toggle { ja: "詳細パネル", en: "Detail panel" }

    /// round7 §2-7: the captured window is minimized. The recording is fine --
    /// the video is black by design (§2-E1) and the audio keeps running -- so
    /// the message says what is happening rather than warning about it.
    minimized_title { ja: "対象が最小化されています", en: "The target is minimized" }
    minimized_body {
        ja: "映像は黒で記録され、音声は継続しています\nウィンドウを復元すると映像が戻ります",
        en: "The picture is being recorded black and the audio keeps running\nRestore the window and the picture comes back"
    }

    /// round7 §2-8: the pill in the top right corner of a full screen playback.
    fullscreen_exit { ja: "全画面を終了", en: "Leave full screen" }

    /// W-1 / W-2 (round5 §11, rewritten by round7 §2-6): the review screen
    /// with nothing loaded. It states the fact plainly and says what turns it
    /// into a picture; W-3 / W-4 went with the two cards that used to stand
    /// here.
    empty_stage_title { ja: "まだ何も録画していません", en: "Nothing has been recorded yet" }
    empty_stage_body {
        ja: "対象を選ぶと、ここに映像が流れはじめます",
        en: "Pick a target and the picture starts here"
    }
    /// The zoom readout's prefix.
    zoom_level_prefix { ja: "表示範囲", en: "Showing" }
}

/// One stepper press, in seconds.
pub const RANGE_STEP_SECONDS: i64 = 10;

/// What Escape does on the review screen (task1060). One key, one thing, and
/// the most local thing first: a mode the user stepped into is nearer to hand
/// than the window's own shape.
///
/// task3860 changed which mode that is, not the order. 「範囲を編集」 is gone
/// and the 確定ステップ took its place, so the first branch now reads
/// `clip_editing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapeAction {
    /// End the 確定ステップ. **A fallback, not the usual path**: while the
    /// step is open its comment field holds key focus and slint's `TextInput`
    /// takes Escape first (`ui/controls.slint`), so this arm is reached only
    /// if the key ever arrives with the field gone or unfocused.
    CloseConfirmStep,
    LeaveFullscreen,
    ClearZoom,
    /// Not ours: the key is handed back so whatever else wants it can have it.
    Nothing,
}

pub fn escape_action(clip_editing: bool, fullscreen: bool, zoomed: bool) -> EscapeAction {
    if clip_editing {
        EscapeAction::CloseConfirmStep
    } else if fullscreen {
        EscapeAction::LeaveFullscreen
    } else if zoomed {
        EscapeAction::ClearZoom
    } else {
        EscapeAction::Nothing
    }
}

/// What ending the 確定ステップ leaves behind (task3860, round19 §1).
///
/// Both exits land here -- Esc or a blur through `clip-cancelled`, and Enter
/// through `clip-committed` -- because both end the step, and the two must
/// leave the same state or the range would depend on how the user left.
///
/// **The range is passed through untouched.** That is the one caveat round19
/// carried: cancelling the step drops the typed comment and *keeps* the range
/// the user dragged inside it. The reasons are the ruling's own -- the range
/// is screen state rather than part of "the save", it is shared with 指定
/// フォルダに書き出し and the still, and with the toggle gone the step is the
/// only place a drag can reach a boundary, so winding it back would delete
/// the 「ドラッグで整えてから書き出す」 path outright. There is no saved copy
/// to restore from anywhere in this crate, and this function is where a future
/// one would have to be written -- which is the point of routing both exits
/// through it.
///
/// The drag latch is the other half, inherited from task1060's toggle. Leaving
/// the step mid-drag is the one way the band's gesture ends without an `up` or
/// a `cancel`: `sel-touch.enabled` goes false with the pointer still held, and
/// slint's `TouchArea` returns `EventIgnored` on a disabled area *before* it
/// handles Released or Exit, so the release is swallowed. Left set,
/// `range_dragging` would tell `owns_position` the UI owns the position for
/// ever and freeze the playhead on screen while playback ran on. Deliberately
/// no resume: the transport stays where the drag left it, which is the honest
/// state for a gesture that was abandoned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfirmStepEnd {
    pub range: Option<(i64, i64)>,
    pub range_dragging: bool,
    pub resume_after_range_drag: bool,
}

pub fn end_confirm_step(range: Option<(i64, i64)>) -> ConfirmStepEnd {
    ConfirmStepEnd {
        range,
        range_dragging: false,
        resume_after_range_drag: false,
    }
}

/// round7 §2-7: the captured window is minimized. The recording is fine -- the
/// video is black by design (§2-E1) and the audio keeps running -- so the
/// message says what is happening rather than warning about it.
pub const MINIMIZED_TITLE: &str = "対象が最小化されています";
pub const MINIMIZED_BODY: &str = "映像は黒で記録され、音声は継続しています
ウィンドウを復元すると映像が戻ります";

/// round7 §2-8: the pill in the top right corner of a full screen playback.
pub const FULLSCREEN_EXIT: &str = "全画面を終了";

/// W-1 / W-2 (round5 §11, rewritten by round7 §2-6): the review screen with
/// nothing loaded. It states the fact plainly and says what turns it into a
/// picture; W-3 / W-4 went with the two cards that used to stand here.
pub const EMPTY_STAGE_TITLE: &str = "まだ何も録画していません";
pub const EMPTY_STAGE_BODY: &str = "対象を選ぶと、ここに映像が流れはじめます";

/// One 直近N chip's word (design round19 §2-3). Built rather than tabled: the
/// three chips differ only in a number, and three `tr!` keys that share a stem
/// would be three places to keep in step with `RECENT_CHIP_SECONDS`.
///
/// The mock carries the ja side only (`.rchip`, `mock/liveback-mock.html:901`),
/// so the en is this side's: the same 「直近」-as-`Last` reading the history
/// screen's wording already uses, with the unit letter the panel's other
/// English strings use for seconds (`Move the start back 10s`).
pub fn recent_chip_label(locale: Locale, seconds: i64) -> String {
    match locale {
        Locale::Ja => format!("直近{seconds}秒"),
        Locale::En => format!("Last {seconds}s"),
    }
}

pub fn zoom_level_text(locale: Locale, duration_100ns: i64) -> String {
    format!(
        "{} {}",
        zoom_level_prefix(locale),
        format_duration(duration_100ns)
    )
}

/// `x` in front, not behind (task238): the multiplier is what the row is
/// *about*, and a leading `x` puts every row's digits in the same column once
/// the text is right-aligned.
pub fn playback_rate_text(rate: f64) -> String {
    if rate.fract() == 0.0 {
        format!("x{}", rate as i64)
    } else {
        format!("x{rate}")
    }
}

#[cfg(test)]
mod tests;
