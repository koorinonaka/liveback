const TFDT_BOX_LEN: usize = 20;

mod parse;
mod rewrite;

pub(crate) use parse::summarize_tracks;
#[cfg(test)]
pub(crate) use parse::{dump_audio_sample_durations, dump_video_sample_durations};
#[cfg(test)]
pub(crate) use parse::{parse_tfhd, parse_trun, read_boxes, read_u32, Mp4BoxEntry};
// The tfdt rewrite on its own. Production goes through the finalize pair
// below instead (task1930), which is this plus the boundary padding; what is
// left here are the tests that pin the injection's own behavior.
#[cfg(test)]
pub(crate) use rewrite::inject_tfdt_if_missing_bytes;
// What the two finalize arms actually call (task1930): the tfdt rewrite above
// followed by the boundary padding below, in that order.
pub(crate) use rewrite::{finalize_fragmented_mp4, finalize_fragmented_mp4_bytes};
// Read-side, unlike everything above it: the repair both the playback reader
// and the export reader put a segment's bytes through before Media Foundation
// sees them (task1900 found it, task1920 shared it).
pub(crate) use rewrite::unstraddle_fragments;

#[cfg(test)]
mod tests;
