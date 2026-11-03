//! The recorded session as a list of segments on its absolute timeline, in
//! 100ns ticks. Engine types: playback, the MCP server and the review screen
//! (`ui_state::timeline`, which re-exports them) all read the same snapshot.

use crate::ring_buffer::{SessionManifest, TimelineGap};

pub const HNS_PER_SECOND: i64 = 10_000_000;
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
