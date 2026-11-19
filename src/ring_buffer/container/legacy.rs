//! Every `.lvb` format older than the one `container.rs` reads and writes
//! (t261012-fe57). The main module handles v3 only; an older file is read
//! through here, and the first write to it is what upgrades it, here.
//!
//! Nothing converts files in bulk -- no installer step, no startup pass, no
//! command. Listing, opening and playing an old session read it as it is and
//! never write to it; only an edit (or a recovery) brings it forward.
//!
//! A future format change adds its own "version n -> n+1" step to this file,
//! run after the ones before it, and **no step is ever removed**: 1.0.0 and
//! 0.9.0 cannot update in place, so someone will always be skipping versions,
//! and dropping a step would leave their protected old sessions unopenable.

use std::io;

use super::{ContainerWriter, Snapshot, V1_SEGMENT_PREFIX_LEN};

/// v1 and v2 checkpoints: the snapshot as JSON. The serde attributes that let
/// a pre-task1250 checkpoint read (`SegmentEntry`'s `audio_offset_100ns` alias
/// and `scalar_or_list`) stay on the shared struct, because v3 decodes the
/// same struct from MessagePack.
pub(super) fn decode_json_checkpoint(payload: &[u8]) -> Option<Snapshot> {
    serde_json::from_slice(payload).ok()
}

/// v1's segment prefix, `index | start | end | audio_offset`, with the body
/// always at 32. v2 turned the fourth field into a track count.
pub(super) fn parse_v1_segment_prefix(payload: &[u8]) -> Option<(u64, i64, i64, Vec<i64>, usize)> {
    if payload.len() < V1_SEGMENT_PREFIX_LEN {
        return None;
    }
    let index = u64::from_le_bytes(payload[0..8].try_into().ok()?);
    let start = i64::from_le_bytes(payload[8..16].try_into().ok()?);
    let end = i64::from_le_bytes(payload[16..24].try_into().ok()?);
    let offset = i64::from_le_bytes(payload[24..32].try_into().ok()?);
    Some((index, start, end, vec![offset], V1_SEGMENT_PREFIX_LEN))
}

/// v1 is read-only: it has no upgrade step, so a write to one is refused
/// before anything on disk changes. None are known to exist (0 of 11 on the
/// machine with the user's recordings, 2026-10-12), and every public build
/// from 0.9.0 on wrote v2.
pub(super) fn refuse_unwritable(format_version: u32) -> io::Result<()> {
    if format_version < 2 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("container format version {format_version} is too old to edit"),
        ));
    }
    Ok(())
}

/// v2 -> v3. The segment prefix did not change, only the checkpoint encoding,
/// so the step is one MessagePack checkpoint of the snapshot the caller read,
/// with the header saying v3 -- both written by that `checkpoint`, record first
/// and header second, the order every checkpoint keeps.
///
/// The caller writes its own checkpoint right after, into the other slot, so
/// both slots end up naming MessagePack. Left with one JSON slot, a damaged
/// newest slot would send the reader to a JSON payload it decodes as v3, and
/// on to a full replay.
pub(super) fn upgrade_v2_to_v3(writer: &mut ContainerWriter) -> io::Result<()> {
    writer.header.format_version = 3;
    writer.checkpoint()?;
    tracing::info!(
        event = "container_upgraded",
        session_id = %writer.header.session_id,
        from = 2,
        to = 3,
        "an edited container was upgraded to format v3"
    );
    Ok(())
}
