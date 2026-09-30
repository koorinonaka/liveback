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

    /// How full the ring is (t260928-2f77): the recorded span against the
    /// session's retention window, clamped to `0.0..=1.0`. At `1.0` the old
    /// end is being overwritten -- the DS Timeline's `overwriting`. A session
    /// with no retention limit (`NO_RETENTION_LIMIT`, 0) never fills. The
    /// byte cap is deliberately not part of this.
    pub fn ring_fill(&self, retention_minutes: u16) -> f64 {
        if retention_minutes == crate::ring_buffer::NO_RETENTION_LIMIT {
            return 0.0;
        }
        let window = i64::from(retention_minutes) * 60 * HNS_PER_SECOND;
        (self.full_duration_100ns() as f64 / window as f64).clamp(0.0, 1.0)
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
    current: Option<(i64, i64)>,
    min_frame_100ns: i64,
) -> Option<(i64, i64)> {
    match quick {
        // `next_range`, not `quick_range`: it is total, and this is also the
        // call a load makes, so a timeline that was empty when it was opened
        // gets exactly the range `load_review` would have given it.
        0 => Some(snapshot.next_range(None, true, min_frame_100ns)),
        seconds if seconds > 0 => snapshot.quick_range(Some(f64::from(seconds)), min_frame_100ns),
        // t260928-b301: the start is the user's, the end rides the live edge.
        QUICK_END_PINNED => current.and_then(|(start, _)| {
            snapshot.clamp_range(start, snapshot.live_edge_100ns - 1, min_frame_100ns)
        }),
        _ => None,
    }
}

/// `quick` for a hand-dragged range whose end handle was let go on the live
/// edge while recording (t260928-b301): the start stays where the user put
/// it and the end keeps following the recording (`range_after_growth`).
/// Pulling the end back off the edge makes it a plain `-1` again.
pub const QUICK_END_PINNED: i32 = -2;

/// Whether a range end let go at `end_100ns` counts as "on the live edge":
/// within one frame of the snapshot's end. `move_range_edge` stops the end at
/// `live_edge - 1`, so the handle pulled all the way right lands inside this.
pub fn end_reaches_live(snapshot: &TimelineSnapshot, end_100ns: i64, min_frame_100ns: i64) -> bool {
    snapshot.live_edge_100ns - end_100ns <= min_frame_100ns
}

/// The range an end-handle release leaves (t260928-faac): pinned -- the end
/// put back on the live edge as it is *now* -- when the handle's last move
/// left it on the edge as it was then (`end_on_live`, `end_reaches_live` at
/// that move). Judging at the move rather than at the release is the point:
/// a handle held still on the edge while segments land is still on the edge.
/// `None`: not pinned, the range stays as dragged.
pub fn release_pinned_range(
    snapshot: &TimelineSnapshot,
    range: (i64, i64),
    end_on_live: bool,
    min_frame_100ns: i64,
) -> Option<(i64, i64)> {
    if !end_on_live {
        return None;
    }
    range_after_growth(snapshot, QUICK_END_PINNED, Some(range), min_frame_100ns)
}

/// The live edge as drawn while recording (t260928-b301): the last finalized
/// segment's end plus the time since this side saw it land, capped at one
/// segment. A segment is 2 s, so the real edge moves in 2 s steps; this one
/// moves every drain and the next finalize replaces it with the real value.
///
/// Display only -- the bar's scale, the total clock and the live line. Seeks,
/// the selection and its guard stay on the real edge, so nothing can seek
/// into time that has no frame yet. Not recording: the real edge.
///
/// Before the first growth after a load, the clock runs from the load
/// (`since_load`, t260928-faac) so the bar grows from the first frame. The
/// load is no earlier than the loaded edge's segment landing, so this never
/// runs further ahead than the growth clock would -- and the first growth
/// replaces it with an edge a whole segment on, so it never steps back.
pub fn apparent_live_edge(
    live_edge_100ns: i64,
    recording: bool,
    since_growth: Option<std::time::Duration>,
    since_load: Option<std::time::Duration>,
) -> i64 {
    let Some(since) = since_growth.or(since_load).filter(|_| recording) else {
        return live_edge_100ns;
    };
    let ahead = i64::try_from(since.as_nanos() / 100).unwrap_or(i64::MAX);
    live_edge_100ns + ahead.min(crate::encoder::SEGMENT_DURATION_100NS)
}

/// The selection as drawn (t260928-970c): an end on the real live edge --
/// 全体, 直近N or `QUICK_END_PINNED`, which all keep it at `live_edge - 1` --
/// is shown on the apparent one, so the band and the panel's 終了 / 長さ grow
/// with the bar instead of in 2 s steps. An end short of the edge is the
/// user's and stays put. Display only: `review.range` itself stays real.
pub fn displayed_range(
    range: Option<(i64, i64)>,
    live_edge_100ns: i64,
    apparent_edge_100ns: i64,
) -> Option<(i64, i64)> {
    range.map(|(start, end)| {
        if end >= live_edge_100ns - 1 {
            (start, end.max(apparent_edge_100ns - 1))
        } else {
            (start, end)
        }
    })
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
///
/// 2026-09-25 (task4160): no production caller any more -- the panel's typed
/// edit and its `range-edited` callback were deleted; 96dc3fd^ is the shape to
/// restore from. Kept with its tests for that day.
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
///
/// 2026-09-25 (task4230): a second rule now reads the same coordinate the
/// other way. `range_guard`'s `hold_at_end` keeps playback running with the
/// playhead on (and past) `end` when the chip, `i`/`o` or the playhead button
/// put the end there. Both stand, deliberately not unified: a band drag
/// *seeks* (`range_follow`), so landing on `end` here is a side effect of the
/// push, and the user ruled on 2026-09-07 that it stays stopped; the family
/// does not seek, so its playhead is on `end` because the user chose that very
/// spot while watching (round19 §3, ruling 2026-09-09 案(B)). Same position,
/// different gesture -- which is why this function takes no latch.
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

/// The range a Shift+drag on the bare track sweeps out (t260928-88a2): the
/// press is one end and the pointer the other, whichever way it went, both cut
/// to the recording and snapped to recorded time by `clamp_range`. `None`
/// while the two are closer than `min_frame_100ns` -- the caller keeps what it
/// had, exactly as a refused handle move does.
pub fn sweep_range(
    snapshot: &TimelineSnapshot,
    anchor_100ns: i64,
    to_100ns: i64,
    min_frame_100ns: i64,
) -> Option<(i64, i64)> {
    let low = snapshot.start_100ns();
    // Exclusive, as everywhere else: the tick past the edge has no frame.
    let high = snapshot.live_edge_100ns - 1;
    let a = anchor_100ns.clamp(low, high);
    let b = to_100ns.clamp(low, high);
    snapshot.clamp_range(a.min(b), a.max(b), min_frame_100ns)
}

/// Half the window a marker row's 「前後 30 秒を範囲に」 opens (task3050). Fixed,
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

/// The range block's three presets (design round19 §2-3, drawn since
/// t260927-7d08 as the DS SegmentedControl 15 / 30 / 60 秒). Kept in the order
/// they are drawn, shortest first.
pub const RECENT_CHIP_SECONDS: [i64; 3] = [15, 30, 60];

/// What one of the range block's presets does (t260929-40a4). Index 0 is 全体,
/// the whole range `load_review` opens on (`quick == 0`, which grows with a
/// recording, task390); the rest are `RECENT_CHIP_SECONDS` in drawn order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangePreset {
    Whole,
    Recent(i64),
}

/// The preset a click on `index` names, or `None` for an index the control
/// should never hand back.
pub fn range_preset_at(index: i32) -> Option<RangePreset> {
    match index {
        0 => Some(RangePreset::Whole),
        _ => usize::try_from(index - 1)
            .ok()
            .and_then(|i| RECENT_CHIP_SECONDS.get(i).copied())
            .map(RangePreset::Recent),
    }
}

/// The lit preset, as an index into the drawn row (全体 first), or -1. 全体
/// reads as selected for exactly as long as the range is the whole one
/// (`quick == 0`) -- not for `QUICK_END_PINNED`, whose start the user chose.
/// Otherwise it is `selected_preset`, shifted past 全体.
pub fn range_preset_index(quick: i32, range: Option<(i64, i64)>, position_100ns: i64) -> i32 {
    if quick == 0 {
        return 0;
    }
    match selected_preset(range, position_100ns) {
        -1 => -1,
        index => index + 1,
    }
}

/// Which preset reads as selected, or -1 for none (t260927-7d08, DS
/// ReviewScreen). Only while the range is exactly that preset's length *and*
/// ends on the playhead -- what pressing it produced. Once the playhead walks
/// on, or a handle moves, or the clamp had to trim the range (the head of the
/// recording, a gap), it is no longer "the last N seconds" and nothing is lit.
pub fn selected_preset(range: Option<(i64, i64)>, position_100ns: i64) -> i32 {
    let Some((start, end)) = range else {
        return -1;
    };
    if end != position_100ns {
        return -1;
    }
    RECENT_CHIP_SECONDS
        .iter()
        .position(|seconds| end - start == seconds * HNS_PER_SECOND)
        .map_or(-1, |index| index as i32)
}

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

/// The DS `Timecode` (t260927-77b8): `m:ss.t`, the hour field only past an
/// hour or when `force_hours` keeps a column aligned with an over-an-hour
/// total, and a U+2212 minus in front of a negative value -- the remaining
/// time and the lag behind the live edge (`−2:18.7`). Never zero-padded in
/// front.
pub fn format_timecode(value_100ns: i64, force_hours: bool) -> String {
    let body = format_range_time(value_100ns.saturating_abs(), force_hours);
    if value_100ns < 0 {
        format!("\u{2212}{body}")
    } else {
        body
    }
}

/// A timecode's whole part and its `.t` tail: the DS draws the tail in
/// `ink-faint`, and slint cannot split a string, so the two travel apart.
pub fn split_timecode(text: &str) -> (&str, &str) {
    text.rfind('.').map_or((text, ""), |at| text.split_at(at))
}

/// The DS ruler's steps, in seconds (`playback.jsx` `STEPS`).
const RULER_STEPS_S: [i64; 11] = [5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600];

/// The ruler's `(major, minor)` step in seconds for a track showing
/// `span_100ns`: the major step is the smallest that leaves seven labels or
/// fewer, the minor the largest step that divides it and draws sixty ticks or
/// fewer (DS Timeline: 「7 本以下になる刻みを自動で選ぶ」).
pub fn ruler_steps(span_100ns: i64) -> (i64, i64) {
    let span = span_100ns.max(1) as f64 / HNS_PER_SECOND as f64;
    let major = RULER_STEPS_S
        .iter()
        .copied()
        .find(|step| span / *step as f64 <= 7.0)
        .unwrap_or(3600);
    let minor = RULER_STEPS_S
        .iter()
        .rev()
        .copied()
        .find(|step| *step < major && major % step == 0 && span / *step as f64 <= 60.0)
        .unwrap_or(major);
    (major, minor)
}

#[derive(Clone, Debug, PartialEq)]
pub struct RulerTick {
    pub percent: f64,
    pub major: bool,
    /// Major ticks only, and none within a second of the right edge, where
    /// the label would run off the track (and under the LIVE word).
    pub label: Option<String>,
}

/// The ruler over the part of the session `viewport` shows. Times count from
/// `origin_100ns` (the session's start), so a zoomed track still reads in the
/// same clock as the transport and the hover time.
pub fn ruler_ticks(viewport: &Viewport, origin_100ns: i64) -> Vec<RulerTick> {
    let (major, minor) = ruler_steps(viewport.duration_100ns);
    let minor_100ns = minor * HNS_PER_SECOND;
    let from = (viewport.start_100ns - origin_100ns).max(0);
    let to = viewport.end_100ns() - origin_100ns;
    let first = (from + minor_100ns - 1) / minor_100ns;
    (first..)
        .map(|index| index * minor_100ns)
        .take_while(|time| *time <= to)
        .map(|time| {
            let is_major = (time / HNS_PER_SECOND) % major == 0;
            RulerTick {
                percent: viewport.percent(time + origin_100ns),
                major: is_major,
                label: (is_major && time < to - HNS_PER_SECOND).then(|| format_duration(time)),
            }
        })
        .collect()
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
    /// W-14 (round5 §11): the name cell of a marker nobody has named. The old
    /// `ラベルを入力` belonged to a popover with a text field already open;
    /// the panel's row is not in edit mode until you double-click it, so it
    /// asks.
    marker_label_placeholder { ja: "名前を付ける", en: "Give it a name" }
    /// A pin on the track with no name (DS Timeline 「名前なし」,
    /// t260928-ac18). The panel row keeps the placeholder above: that one is
    /// a field you can type into, this one is only a label.
    marker_unnamed { ja: "名前なし", en: "Unnamed" }
    marker_delete { ja: "削除", en: "Delete" }
    marker_save_error {
        ja: "マーカーの名前を保存できませんでした",
        en: "The marker's name could not be saved"
    }
    marker_delete_error {
        ja: "マーカーを削除できませんでした",
        en: "The marker could not be deleted"
    }
    marker_add_error {
        ja: "マーカーを追加できませんでした",
        en: "The marker could not be added"
    }
    /// task3060 (design round18 §4-7): renamed from 「静止画を保存」. Sitting under
    /// the range block it read as "a still *of the selection*"; it has always
    /// written the frame on screen (`save_screenshot_png`), so the label says so.
    ///
    /// t260927-7d08: 「静止画」, the DS ReviewScreen's word -- it now sits beside
    /// 「書き出し…」 under the primary, and the range block is no longer directly
    /// above it to be misread against.
    screenshot { ja: "静止画", en: "Still" }
    screenshot_no_frame {
        ja: "表示中のフレームがありません。",
        en: "There is no frame on screen."
    }
    /// t260927-7d08: 「書き出し…」 (DS) -- the ellipsis says a dialog follows.
    export_selection { ja: "書き出し…", en: "Export…" }
    /// Task2970: the primary of the pair. It opens no file dialog -- that is
    /// the whole difference from the row below it, so the wording names the
    /// destination rather than the act of writing a file.
    clip_save { ja: "クリップに保存", en: "Save as a clip" }
    /// The comment field under the range (t260927-7d08, DS ReviewScreen): it is
    /// always there, so what done/3180 carried in a trailing hint -- what to
    /// type, that it is optional, Enter -- is the label, this hint and the
    /// primary's Keycap now.
    clip_comment_label { ja: "コメント", en: "Comment" }
    clip_comment_placeholder { ja: "コメントを付けられます", en: "Add a comment" }
    clip_comment_hint { ja: "空欄でも保存できます", en: "You can leave it empty" }
    /// The export progress bar's cancel (DS ProgressBar: 「中止」).
    export_abort { ja: "中止", en: "Stop" }

    /// The panel's two tabs (t260927-7d08, DS ReviewScreen).
    panel_export_tab { ja: "書き出し", en: "Export" }
    panel_marker_section { ja: "マーカー", en: "Markers" }
    /// The export tab's overline and its hint (DS ReviewScreen).
    range_overline { ja: "範囲", en: "Range" }
    range_hint { ja: "再生位置から遡る", en: "Back from the playhead" }
    /// The overline over the track mixer, when there are two tracks or more.
    panel_audio_section { ja: "音声", en: "Audio" }
    /// W-5: the marker list before there is anything in it.
    markers_empty { ja: "マーカーはまだありません", en: "No markers yet" }
    /// Beside the hotkey's Keycap under it (DS MarkerList).
    markers_empty_note { ja: "ゲームの画面にいても押せます。", en: "It works from inside the game, too." }
    marker_rename_hint { ja: "名前を変更", en: "Rename" }
    marker_delete_hint { ja: "マーカーを削除", en: "Delete the marker" }
    /// The delete is a 1-second hold (t260928-bae5 F4, DS 原則 5); a short press
    /// says so instead of deleting.
    marker_hold_hint { ja: "長押しで削除", en: "Hold to delete" }
    range_start_label { ja: "開始", en: "Start" }
    range_end_label { ja: "終了", en: "End" }
    range_length_label { ja: "長さ", en: "Length" }
    /// t260928-88a2: 開始 / 終了's 「再生位置へ合わせる」, 7d08^'s words.
    range_playhead_start { ja: "開始を再生位置に合わせる", en: "Set the start to the playhead" }
    range_playhead_end { ja: "終了を再生位置に合わせる", en: "Set the end to the playhead" }
    /// task3050: the marker row's first button.
    marker_range_hint { ja: "前後 30 秒を範囲に", en: "Select 30s around this marker" }
    /// What the range block shows with nothing selected.
    range_unset { ja: "—", en: "—" }
    panel_toggle { ja: "詳細パネル", en: "Detail panel" }

    /// round7 §2-7: the captured window is minimized. The recording is fine --
    /// the video is black by design (§2-E1) and the audio keeps running -- so
    /// the message says what is happening rather than warning about it.
    minimized_title { ja: "対象が最小化されています", en: "The target is minimized" }
    // t260927-77b8: the DS Stage README's words, one paragraph.
    minimized_body {
        ja: "映像は黒で記録され、音声は続いています。ウィンドウを戻すと映像も戻ります。",
        en: "The picture is being recorded black and the audio keeps running. Restore the window and the picture comes back."
    }

    /// round7 §2-8: the pill in the top right corner of a full screen playback.
    fullscreen_exit { ja: "全画面を終了", en: "Leave full screen" }

    /// W-1 / W-2 (round5 §11, rewritten by round7 §2-6): the review screen
    /// with nothing loaded. It states the fact plainly and says what turns it
    /// into a picture; W-3 / W-4 went with the two cards that used to stand
    /// here.
    empty_stage_title { ja: "まだ何も録画していません", en: "Nothing has been recorded yet" }
    // t260927-9865: the DS EmptyReviewScreen's words, one paragraph, and the
    // primary button that opens the capture flyout. A break after the first
    // sentence (t260928-ac18): the second otherwise split mid-word.
    empty_stage_body {
        ja: "録画する対象を選ぶと、ここに映像が流れはじめます。\n録画しながら、いつでも巻き戻して見返せます。",
        en: "Choose what to record and the picture starts flowing here. You can rewind and look back at any time while recording."
    }
    empty_stage_action { ja: "対象を選ぶ", en: "Choose a target" }
    /// The zoom readout's prefix.
    zoom_level_prefix { ja: "表示範囲", en: "Showing" }
}

/// What Escape does on the review screen (task1060). One key, one thing, and
/// the most local thing first: the window's own shape, then the zoom.
///
/// t260927-7d08: the 確定ステップ that used to come first is gone (user ruling
/// 2026-09-27, DS ReviewScreen) -- the range is always editable and the comment
/// field is always there, so there is no mode left to step out of. Escape in
/// the comment field itself only hands the keyboard back to the pane
/// (`ui/review.slint`) and never reaches here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapeAction {
    LeaveFullscreen,
    ClearZoom,
    /// Not ours: the key is handed back so whatever else wants it can have it.
    Nothing,
}

pub fn escape_action(fullscreen: bool, zoomed: bool) -> EscapeAction {
    if fullscreen {
        EscapeAction::LeaveFullscreen
    } else if zoomed {
        EscapeAction::ClearZoom
    } else {
        EscapeAction::Nothing
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
pub const EMPTY_STAGE_BODY: &str =
    "録画する対象を選ぶと、ここに映像が流れはじめます。\n録画しながら、いつでも巻き戻して見返せます。";

/// One range preset's word (t260927-7d08, DS SegmentedControl: 「数値の選択肢は
/// 単位まで書く」). Built rather than tabled: the three differ only in a number,
/// and three `tr!` keys would be three places to keep in step with
/// `RECENT_CHIP_SECONDS`.
pub fn recent_chip_label(locale: Locale, seconds: i64) -> String {
    match locale {
        Locale::Ja => format!("{seconds} 秒"),
        Locale::En => format!("{seconds}s"),
    }
}

/// The range block's preset row as drawn (t260929-40a4): 全体 first, then the
/// `RECENT_CHIP_SECONDS` chips. Index for index what `range_preset_at` reads.
pub fn range_preset_labels(locale: Locale) -> Vec<String> {
    let whole = match locale {
        Locale::Ja => "全体",
        Locale::En => "All",
    };
    std::iter::once(whole.to_owned())
        .chain(
            RECENT_CHIP_SECONDS
                .iter()
                .map(|seconds| recent_chip_label(locale, *seconds)),
        )
        .collect()
}

pub fn zoom_level_text(locale: Locale, duration_100ns: i64) -> String {
    format!(
        "{} {}",
        zoom_level_prefix(locale),
        format_duration(duration_100ns)
    )
}

/// `×` in front, not behind (task238): the multiplier is what the row is
/// *about*, and a leading `×` puts every row's digits in the same column once
/// the text is right-aligned. The multiplication sign rather than an ASCII
/// `x` since t260927-77b8 (TransportBar 決めたこと 4: 「×1.5」).
pub fn playback_rate_text(rate: f64) -> String {
    if rate.fract() == 0.0 {
        format!("×{}", rate as i64)
    } else {
        format!("×{rate}")
    }
}

/// The empty review screen's drifting scene (t260927-9865, DS `TimeDrift`):
/// the back belt travels 1600px in 80s, the front belt (and its ruler) in
/// 44s, and the dot at the edge of now swells out over 2.4s and back.
pub const DRIFT_BACK_PERIOD: std::time::Duration = std::time::Duration::from_secs(80);
pub const DRIFT_FRONT_PERIOD: std::time::Duration = std::time::Duration::from_secs(44);
/// One way of the pulse (the DS's `lb-now 2.4s ... alternate`).
pub const DRIFT_PULSE_HALF: std::time::Duration = std::time::Duration::from_millis(2400);

/// Each belt's travel as a fraction of one 1600px lap, and the pulse's 0..1
/// swell. All three come from one running time, so a late timer tick moves
/// them further rather than making them slower.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftPhases {
    pub back: f32,
    pub front: f32,
    pub pulse: f32,
}

pub fn drift_phases(run: std::time::Duration) -> DriftPhases {
    let lap = |period: std::time::Duration| {
        (run.as_nanos() % period.as_nanos()) as f64 / period.as_nanos() as f64
    };
    // Up over one half, back down over the next, on `easing-standard`; the
    // CSS `alternate` plays the curve backwards on the way down.
    let swell = 2.0 * lap(2 * DRIFT_PULSE_HALF);
    let pulse = ease_standard(if swell <= 1.0 { swell } else { 2.0 - swell });
    DriftPhases {
        back: lap(DRIFT_BACK_PERIOD) as f32,
        front: lap(DRIFT_FRONT_PERIOD) as f32,
        pulse: pulse as f32,
    }
}

/// `easing-standard`, `cubic-bezier(0.2, 0, 0, 1)` (tokens.slint): the curve's
/// height where it reaches `p` across, by bisection -- x is monotonic on
/// [0, 1]. Here rather than in the .slint, where a function this size is
/// inlined at every call (KNOWLEDGE「関数は呼び出し箇所へ展開される」).
fn ease_standard(p: f64) -> f64 {
    if p <= 0.0 {
        return 0.0;
    }
    if p >= 1.0 {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
    for _ in 0..40 {
        let t = (lo + hi) / 2.0;
        let x = 0.6 * (1.0 - t) * (1.0 - t) * t + t * t * t;
        if x < p {
            lo = t;
        } else {
            hi = t;
        }
    }
    let t = (lo + hi) / 2.0;
    3.0 * t * t - 2.0 * t * t * t
}

/// Whether the drift should advance at all: only while the empty review
/// screen is the pane on show, the window is on screen (not minimized, not
/// stashed in the tray) and the user has not asked for less motion. Anything
/// else writes nothing, so the window presents nothing.
pub fn drift_runs(empty_review_shown: bool, window_on_screen: bool, reduce_motion: bool) -> bool {
    empty_review_shown && window_on_screen && !reduce_motion
}

/// Running time that only accumulates while the drift runs, so a pause picks
/// up where it stopped instead of jumping ahead by however long it was hidden.
#[derive(Debug, Default)]
pub struct DriftClock {
    run: std::time::Duration,
    since: Option<std::time::Instant>,
}

impl DriftClock {
    /// One tick. Returns the running time so far; while stopped it holds.
    pub fn tick(&mut self, now: std::time::Instant, running: bool) -> std::time::Duration {
        if running {
            if let Some(since) = self.since {
                self.run += now.saturating_duration_since(since);
            }
            self.since = Some(now);
        } else {
            self.since = None;
        }
        self.run
    }
}

#[cfg(test)]
mod tests;
