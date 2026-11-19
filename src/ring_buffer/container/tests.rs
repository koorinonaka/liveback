//! `ring_buffer::container` (task401). The format's whole point is surviving
//! a machine that stopped mid-write, so most of these are about damage.

use super::*;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

/// A temp directory that deletes itself when its binding goes out of scope
/// (task3230). Explicit cleanup at the end of a test is not enough: an
/// assertion failure unwinds past it, so exactly the runs worth keeping small
/// -- the failing ones -- were the ones leaving `livia-*` behind. `Drop` runs on
/// the unwind too.
///
/// This is a deliberate copy of `ring_buffer/tests.rs`'s guard rather than a
/// shared helper: that one is private, and `crates/livia-thumb` pulls
/// `container.rs` in by `#[path]` without ever compiling `ring_buffer/tests.rs`,
/// so there is no module both sides can import from.
///
/// Deliberately not `Clone`: two guards over one directory would each try to
/// remove it.
struct TmpDir {
    path: PathBuf,
}

impl Deref for TmpDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TmpDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        // Never panic here. Damage tests legitimately end with the directory
        // already gone, and a `.lvb` a writer still holds open (no
        // `FILE_SHARE_DELETE`, see `container.rs`) makes the removal a sharing
        // violation. Neither is a reason to turn a green test red.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn tmp_dir() -> TmpDir {
    // The sequence number is what makes the name unique, not the clock: two
    // parallel test threads in one process can read the same 100ns tick, and
    // before task3230 that had them sharing a directory -- whichever finished
    // first removed the other's fixture out from under it.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("livia-{}-{seq}-{id}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TmpDir { path }
}

fn body(index: u64, len: usize) -> Vec<u8> {
    // Distinct per segment and not compressible into a pattern the reader
    // could accidentally satisfy from the wrong offset.
    (0..len)
        .map(|i| ((index as usize * 31 + i * 7) % 251) as u8)
        .collect()
}

#[test]
fn crc32_matches_the_known_check_value() {
    // The standard check value for CRC-32/ISO-HDLC.
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
    assert_ne!(crc32(b"a"), crc32(b"b"));
}

/// task1410 put a sentinel in this header field: 0 means "never drop the
/// head". It has to survive the round trip, or a session reopened after a
/// crash would come back as a 0-minute ring and prune itself away.
#[test]
fn a_zero_retention_header_reads_back_as_zero() {
    // `dir` has to outlive the file: `tmp_dir().join(..)` would drop the guard
    // at the end of this statement and take the directory with it.
    let dir = tmp_dir();
    let path = dir.join("no-retention.lvb");
    ContainerWriter::create(&path, "capture-no-retention", 0)
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(
        ContainerReader::open(&path)
            .unwrap()
            .header()
            .retention_minutes,
        0
    );
}

#[test]
fn a_written_container_reads_back_byte_for_byte() {
    let dir = tmp_dir();
    let path = dir.join("round-trip.lvb");
    let bodies: Vec<Vec<u8>> = (0..8).map(|i| body(i, 300 + i as usize * 17)).collect();

    let mut writer = ContainerWriter::create(&path, "capture-roundtrip", 15).unwrap();
    for (index, bytes) in bodies.iter().enumerate() {
        let index = index as u64;
        writer
            .append_segment(
                index,
                index as i64 * 20_000_000,
                (index as i64 + 1) * 20_000_000,
                &[0],
                bytes,
            )
            .unwrap();
        writer
            .append_thumbnail(index, &[0xFF, 0xD8, index as u8])
            .unwrap();
        if index == 3 {
            writer.checkpoint().unwrap();
        }
    }
    writer
        .append_meta(&MetaEvent::MarkerAdded {
            time_100ns: 25_000_000,
            label: "here".into(),
            palette_index: Some(2),
            comment: None,
        })
        .unwrap();
    writer
        .append_meta(&MetaEvent::TitleSet {
            title: Some("Sample Game".into()),
        })
        .unwrap();
    writer.close().unwrap();

    let mut reader = ContainerReader::open(&path).unwrap();
    assert_eq!(reader.header().session_id, "capture-roundtrip");
    assert_eq!(reader.header().retention_minutes, 15);
    assert!(!reader.truncated());
    assert!(
        reader.snapshot().closed,
        "a closed container is not a candidate"
    );
    assert_eq!(reader.snapshot().segments.len(), 8);
    assert_eq!(reader.snapshot().title.as_deref(), Some("Sample Game"));
    assert_eq!(reader.snapshot().markers.len(), 1);

    for (index, expected) in bodies.iter().enumerate() {
        let index = index as u64;
        assert_eq!(
            &reader.read_segment(index).unwrap(),
            expected,
            "segment {index}"
        );
        assert_eq!(
            reader.read_thumbnail(index).unwrap(),
            vec![0xFF, 0xD8, index as u8]
        );
        let entry = reader
            .snapshot()
            .segments
            .iter()
            .find(|segment| segment.index == index)
            .unwrap();
        assert_eq!(entry.start_100ns, index as i64 * 20_000_000);
        assert_eq!(entry.end_100ns, (index as i64 + 1) * 20_000_000);
    }
}

/// The acceptance criterion that matters most: cut the file anywhere at all
/// and the reader still returns a consistent state. Every byte offset is
/// tried, not a sample -- the interesting ones are exactly the boundaries a
/// sampled sweep would step over (mid-header, mid-length-field, mid-payload).
#[test]
fn truncating_at_any_byte_leaves_a_consistent_reader() {
    let dir = tmp_dir();
    let path = dir.join("truncate.lvb");
    ContainerBuilder::new("capture-truncate")
        .segment(0, 0, 20_000_000, &body(0, 120))
        .segment(1, 20_000_000, 40_000_000, &body(1, 120))
        .checkpoint()
        .segment(2, 40_000_000, 60_000_000, &body(2, 120))
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 30_000_000,
            label: "m".into(),
            palette_index: None,
            comment: None,
        })
        .segment(3, 60_000_000, 80_000_000, &body(3, 120))
        .closed()
        .build(&path)
        .unwrap();

    let whole = std::fs::read(&path).unwrap();
    let full = scan(&whole).unwrap();
    assert_eq!(full.snapshot.segments.len(), 4);
    assert!(!full.truncated);

    for cut in 0..whole.len() {
        let bytes = &whole[..cut];
        match scan(bytes) {
            Ok(scan) => {
                // Whatever survived has to be internally consistent: every
                // segment's bytes are inside the range the scan vouched for,
                // and every one of them still reads as what was written.
                assert!(
                    scan.valid_end <= bytes.len() as u64,
                    "cut {cut}: valid_end past the end of the file"
                );
                for segment in &scan.snapshot.segments {
                    let end = segment.data.body.offset + segment.data.body.len;
                    assert!(
                        end <= scan.valid_end,
                        "cut {cut}: segment {} runs past valid_end",
                        segment.index
                    );
                    let actual = &bytes[segment.data.body.offset as usize..end as usize];
                    assert_eq!(
                        actual,
                        &body(segment.index, 120)[..],
                        "cut {cut}: segment {} came back wrong",
                        segment.index
                    );
                }
                // A prefix is never *more* than the whole file held.
                assert!(scan.snapshot.segments.len() <= full.snapshot.segments.len());
            }
            // Only a cut inside the fixed header may refuse to open at all --
            // there is no header to believe yet.
            Err(_) => assert!(
                cut < HEADER_LEN as usize,
                "cut {cut} failed to scan but the header was complete"
            ),
        }
    }
}

/// A flipped payload byte must never be served, wherever it lands. *Where* it
/// is caught depends on whether a checkpoint covers the record:
///
/// - past the newest checkpoint, the open scan re-reads the record and stops
///   there (logical truncate);
/// - before it, the scan trusts the checkpoint and skips the bytes -- which is
///   the saving a checkpoint exists for -- so the read is what has to catch it.
///
/// Both are exercised here. The second case is why `Located` carries the CRC.
#[test]
fn a_flipped_payload_byte_is_never_served() {
    let dir = tmp_dir();

    // --- past the checkpoint: the scan refuses the record.
    let unchecked = dir.join("rot-tail.lvb");
    ContainerBuilder::new("capture-rot")
        .segment(0, 0, 20_000_000, &body(0, 64))
        .checkpoint()
        .segment(1, 20_000_000, 40_000_000, &body(1, 64))
        .build(&unchecked)
        .unwrap();
    let intact = scan(&std::fs::read(&unchecked).unwrap()).unwrap();
    let first = intact
        .snapshot
        .segments
        .iter()
        .find(|segment| segment.index == 0)
        .unwrap()
        .data;
    let second = intact
        .snapshot
        .segments
        .iter()
        .find(|segment| segment.index == 1)
        .unwrap()
        .data
        .body;

    for offset in second.offset..second.offset + second.len {
        let path = dir.join(format!("rot-{offset}.lvb"));
        ContainerBuilder::new("capture-rot")
            .segment(0, 0, 20_000_000, &body(0, 64))
            .checkpoint()
            .segment(1, 20_000_000, 40_000_000, &body(1, 64))
            .damaged(Damage::FlipByteAt(offset))
            .build(&path)
            .unwrap();
        let scanned = scan(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            scanned.truncated,
            "a flipped byte at {offset} was not detected"
        );
        assert!(
            !scanned
                .snapshot
                .segments
                .iter()
                .any(|segment| segment.index == 1),
            "the corrupt segment at {offset} was served anyway"
        );
        // ...and the segment before it is untouched, at the same offset.
        assert_eq!(
            scanned
                .snapshot
                .segments
                .iter()
                .find(|segment| segment.index == 0)
                .unwrap()
                .data,
            first
        );
        let _ = std::fs::remove_file(&path);
    }

    // --- before the checkpoint: the scan lists it, the read rejects it.
    let covered = dir.join("rot-covered.lvb");
    ContainerBuilder::new("capture-rot")
        .segment(0, 0, 20_000_000, &body(0, 64))
        .segment(1, 20_000_000, 40_000_000, &body(1, 64))
        .closed()
        .build(&covered)
        .unwrap();
    let target = scan(&std::fs::read(&covered).unwrap())
        .unwrap()
        .snapshot
        .segments
        .iter()
        .find(|segment| segment.index == 1)
        .unwrap()
        .data
        .body;

    for offset in [
        target.offset,
        target.offset + target.len / 2,
        target.offset + target.len - 1,
    ] {
        let path = dir.join(format!("covered-{offset}.lvb"));
        ContainerBuilder::new("capture-rot")
            .segment(0, 0, 20_000_000, &body(0, 64))
            .segment(1, 20_000_000, 40_000_000, &body(1, 64))
            .closed()
            .damaged(Damage::FlipByteAt(offset))
            .build(&path)
            .unwrap();
        let mut reader = ContainerReader::open(&path).unwrap();
        // The checkpoint vouched for it, so it is still in the listing...
        assert!(reader
            .snapshot()
            .segments
            .iter()
            .any(|segment| segment.index == 1));
        // ...but its bytes are never handed out.
        let error = reader.read_segment(1).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "at {offset}");
        // The healthy neighbour still reads.
        assert_eq!(reader.read_segment(0).unwrap(), body(0, 64));
        let _ = std::fs::remove_file(&path);
    }
}

/// The reason there are two slots. Destroy either one and the container still
/// opens from the other.
#[test]
fn either_checkpoint_slot_can_be_lost() {
    let dir = tmp_dir();
    for (name, second) in [("slot-a", false), ("slot-b", true)] {
        let path = dir.join(format!("{name}.lvb"));
        ContainerBuilder::new("capture-slots")
            .segment(0, 0, 20_000_000, &body(0, 96))
            .checkpoint()
            .segment(1, 20_000_000, 40_000_000, &body(1, 96))
            .checkpoint()
            .segment(2, 40_000_000, 60_000_000, &body(2, 96))
            .closed()
            .damaged(Damage::CorruptSlot(second))
            .build(&path)
            .unwrap();

        let mut reader = ContainerReader::open(&path).unwrap();
        assert_eq!(
            reader.snapshot().segments.len(),
            3,
            "{name}: lost segments with one slot corrupt"
        );
        for index in 0..3u64 {
            assert_eq!(
                reader.read_segment(index).unwrap(),
                body(index, 96),
                "{name}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// A slot that passes its own CRC but names a record that is not a checkpoint:
/// the scan reports a truncated container even though the replay from the start
/// reads every record cleanly, and the checkpoint-only open refuses (so
/// `open_for_reading` falls back to the scan). The intact copy of the same file
/// is the control.
#[test]
fn a_slot_naming_a_non_checkpoint_truncates_the_scan_and_fails_the_checkpoint_open() {
    let dir = tmp_dir();
    let path = dir.join("misdirected-slot.lvb");
    ContainerBuilder::new("capture-rotcp")
        .segment(0, 0, 20_000_000, &body(0, 48))
        .checkpoint()
        .segment(1, 20_000_000, 40_000_000, &body(1, 48))
        .build(&path)
        .unwrap();
    let intact = std::fs::read(&path).unwrap();
    let scanned = scan(&intact).unwrap();
    assert!(!scanned.truncated);
    assert_eq!(scanned.snapshot.segments.len(), 2);
    assert!(ContainerReader::open_at_checkpoint(&path).is_ok());

    // Point the one slot at segment 1's record, with a valid slot CRC.
    let (header, slot_a, slot_b) = decode_header(&intact).unwrap();
    assert!(slot_b.is_none(), "one checkpoint, one slot");
    let segment = scanned.snapshot.segments[1].data.record;
    let misdirected = Slot {
        generation: slot_a.unwrap().generation,
        offset: segment.offset,
        len: segment.len,
    };
    let mut bytes = intact;
    bytes[..HEADER_LEN as usize].copy_from_slice(&encode_header(&header, Some(misdirected), None));
    std::fs::write(&path, &bytes).unwrap();

    let scanned = scan(&bytes).unwrap();
    assert!(scanned.truncated, "the unusable slot is a truncation");
    assert_eq!(
        scanned.snapshot.segments.len(),
        2,
        "the replay still reads both"
    );
    assert!(ContainerReader::open_at_checkpoint(&path).is_err());
    let _ = std::fs::remove_file(&path);
}

/// A container with no usable checkpoint at all still opens: the scan simply
/// replays from the first record. This is the state a crash before the first
/// checkpoint leaves.
#[test]
fn a_container_with_both_slots_gone_replays_from_the_start() {
    let dir = tmp_dir();
    let path = dir.join("no-slots.lvb");
    ContainerBuilder::new("capture-noslot")
        .segment(0, 0, 20_000_000, &body(0, 48))
        .segment(1, 20_000_000, 40_000_000, &body(1, 48))
        .checkpoint()
        .damaged(Damage::CorruptSlot(false))
        .build(&path)
        .unwrap();
    // Wipe the other one too.
    let mut bytes = std::fs::read(&path).unwrap();
    for byte in bytes.iter_mut().skip(SLOT_B_OFFSET).take(SLOT_LEN) {
        *byte ^= 0xFF;
    }
    std::fs::write(&path, &bytes).unwrap();

    let scanned = scan(&bytes).unwrap();
    assert_eq!(scanned.snapshot.segments.len(), 2);
    assert!(scanned.snapshot.markers.is_empty());
}

#[test]
fn an_unclosed_container_is_a_recovery_candidate_and_recovery_closes_it() {
    let dir = tmp_dir();
    let path = dir.join("recover.lvb");
    ContainerBuilder::new("capture-recover")
        .segment(0, 0, 20_000_000, &body(0, 200))
        .checkpoint()
        .segment(1, 20_000_000, 40_000_000, &body(1, 200))
        .build(&path)
        .unwrap();
    // Cut the tail mid-record, the way a crash during a segment write does.
    let len = std::fs::metadata(&path).unwrap().len();
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(len - 60).unwrap();
    drop(file);

    let reader = ContainerReader::open(&path).unwrap();
    assert!(!reader.snapshot().closed);
    assert!(reader.truncated());
    let survived = reader.snapshot().segments.len();
    drop(reader);

    let snapshot = recover(&path).unwrap();
    assert!(snapshot.closed);
    assert_eq!(snapshot.segments.len(), survived);

    // And it opens cleanly the second time: no truncation left to report.
    let mut reader = ContainerReader::open(&path).unwrap();
    assert!(reader.snapshot().closed);
    assert!(!reader.truncated(), "recovery left debris behind");
    assert_eq!(reader.read_segment(0).unwrap(), body(0, 200));
}

/// Pruning is logical, and the survivors do not move -- that immobility is
/// what lets the container hand out offsets that stay valid.
#[test]
fn pruning_frees_bytes_without_moving_what_is_left() {
    let dir = tmp_dir();
    let path = dir.join("prune.lvb");

    // Big enough that a hole is more than one cluster, or there is nothing
    // for the filesystem to give back.
    let chunk = 512 * 1024;
    let mut writer = ContainerWriter::create(&path, "capture-prune", 15).unwrap();
    for index in 0..8u64 {
        writer
            .append_segment(
                index,
                index as i64 * 20_000_000,
                (index as i64 + 1) * 20_000_000,
                &[0],
                &body(index, chunk),
            )
            .unwrap();
    }
    writer.checkpoint().unwrap();
    let before: Vec<Located> = writer
        .snapshot()
        .segments
        .iter()
        .map(|segment| segment.data)
        .collect();
    let sparse = writer.sparse();

    let punched = writer.prune(4).unwrap();
    writer.close().unwrap();

    assert!(punched.is_some(), "pruning four segments freed no range");

    let mut reader = ContainerReader::open(&path).unwrap();
    assert_eq!(reader.snapshot().segments.len(), 4);
    for (index, segment) in reader.snapshot().segments.iter().enumerate() {
        // Offsets are the originals: nothing was compacted.
        assert_eq!(
            segment.data,
            before[index + 4],
            "segment {} moved during prune",
            segment.index
        );
    }
    for index in 4..8u64 {
        assert_eq!(reader.read_segment(index).unwrap(), body(index, chunk));
    }
    assert!(
        reader.read_segment(0).is_err(),
        "a pruned segment still reads"
    );

    // Compared against the file's own length rather than a reading taken
    // before the prune: NTFS reports allocation lazily while a write handle is
    // open, so a "before" figure is not a number to trust. What a hole punch
    // means is precisely that these two have come apart.
    let logical = std::fs::metadata(&path).unwrap().len();
    let allocated_after = allocated_bytes(&path).unwrap();
    if sparse {
        assert!(
            allocated_after < logical,
            "hole punch freed nothing: allocated {allocated_after} of logical {logical}"
        );
        // The four survivors are half the file, so roughly half should remain
        // -- a loose bound, since clusters round.
        assert!(
            allocated_after < logical * 3 / 4,
            "hole punch freed less than expected: {allocated_after} of {logical}"
        );
    } else {
        // A volume without sparse support is a supported outcome, not a
        // failure -- the session is still logically shorter.
        eprintln!("volume is not sparse-capable; hole punch skipped");
    }
}

/// A prune whose hole punch cannot happen must still prune. Modelled by
/// punching a range on a handle that will not take it, which is the same
/// error path a non-sparse volume produces.
#[test]
fn a_failed_hole_punch_does_not_fail_the_prune() {
    let dir = tmp_dir();
    let path = dir.join("punch-fail.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-punchfail", 15).unwrap();
    for index in 0..4u64 {
        writer
            .append_segment(index, 0, 20_000_000, &[0], &body(index, 64))
            .unwrap();
    }

    // A range that starts past the end of the file: the FSCTL refuses it, and
    // `prune` must swallow that exactly as it swallows a non-sparse volume.
    let read_only = File::open(&path).unwrap();
    assert!(
        punch_hole(&read_only, u64::from(u32::MAX), 4096).is_err(),
        "expected this punch to fail so the test means something"
    );
    drop(read_only);

    writer.prune(2).unwrap();
    writer.close().unwrap();

    let reader = ContainerReader::open(&path).unwrap();
    assert_eq!(reader.snapshot().segments.len(), 2);
    assert!(reader.snapshot().closed);
}

/// t260925-e0d4: `--full` streams through `dump_from`; its output must be
/// `dump`'s over the whole file, byte for byte, healthy or damaged. Also what
/// the dump says: a healthy file names its header, records and markers, a cut
/// one reads as truncated but keeps its header, and a stranger is refused
/// without a panic.
#[test]
fn the_streamed_dump_matches_the_in_memory_dump() {
    let dir = tmp_dir();
    let good = dir.join("dump-from-good.lvb");
    ContainerBuilder::new("capture-dump-from")
        .segment(0, 0, 20_000_000, &body(0, 64))
        .thumbnail(0, &[0xFF, 0xD8, 0x00])
        .checkpoint()
        .segment(1, 20_000_000, 40_000_000, &body(1, 64))
        .closed()
        .build(&good)
        .unwrap();
    let len = std::fs::metadata(&good).unwrap().len();

    let cut = dir.join("dump-from-cut.lvb");
    std::fs::copy(&good, &cut).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&cut)
        .unwrap()
        .set_len(HEADER_LEN + (len - HEADER_LEN) / 2)
        .unwrap();

    let alien = dir.join("dump-from-alien.bin");
    std::fs::write(&alien, b"not a container at all").unwrap();

    let bad_magic = dir.join("dump-from-bad-magic.lvb");
    let mut bytes = std::fs::read(&good).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&bad_magic, &bytes).unwrap();

    for (path, expect) in [
        (&good, "truncated=false"),
        (&cut, "truncated=true"),
        (&alien, "header unreadable"),
        (&bad_magic, "header unreadable"),
    ] {
        let in_memory = dump(&std::fs::read(path).unwrap());
        let streamed = dump_from(&mut File::open(path).unwrap());
        assert!(
            in_memory.contains(expect),
            "{}: {in_memory}",
            path.display()
        );
        assert_eq!(streamed, in_memory, "{}", path.display());
    }

    // What the healthy dump names, on a file that also carries a marker.
    let marked = dir.join("dump-marked.lvb");
    ContainerBuilder::new("capture-dump")
        .segment(0, 0, 20_000_000, &body(0, 64))
        .thumbnail(0, &[0xFF, 0xD8, 0x00])
        .checkpoint()
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 5_000_000,
            label: "first".into(),
            palette_index: None,
            comment: None,
        })
        .closed()
        .build(&marked)
        .unwrap();
    let text = dump(&std::fs::read(&marked).unwrap());
    for line in [
        "magic=LVBF",
        "session=capture-dump",
        "truncated=false",
        "closed=true",
        "segment 0",
        "marker 5000000 first",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    let text = dump(&std::fs::read(&cut).unwrap());
    assert!(
        text.contains("magic=LVBF"),
        "a damaged file still has a header: {text}"
    );
}

/// t261009-0251: `comment` on the marker records is three-state -- absent
/// leaves it, `Some("")` clears it, `Some(x)` replaces it.
#[test]
fn a_marker_comment_folds_only_when_the_record_carries_one() {
    let mut snapshot = Snapshot::default();
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 10,
        label: "a".into(),
        palette_index: None,
        comment: Some("line 1\nline 2".into()),
    });
    assert_eq!(snapshot.markers[0].comment, "line 1\nline 2");
    // A plain rename leaves the comment.
    snapshot.apply(&MetaEvent::MarkerRenamed {
        time_100ns: 10,
        label: "b".into(),
        comment: None,
    });
    assert_eq!(snapshot.markers[0].label, "b");
    assert_eq!(snapshot.markers[0].comment, "line 1\nline 2");
    // A re-add without one leaves it too.
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 10,
        label: "b".into(),
        palette_index: None,
        comment: None,
    });
    assert_eq!(snapshot.markers[0].comment, "line 1\nline 2");
    snapshot.apply(&MetaEvent::MarkerRenamed {
        time_100ns: 10,
        label: "b".into(),
        comment: Some("other".into()),
    });
    assert_eq!(snapshot.markers[0].comment, "other");
    snapshot.apply(&MetaEvent::MarkerRenamed {
        time_100ns: 10,
        label: "b".into(),
        comment: Some(String::new()),
    });
    assert_eq!(snapshot.markers[0].comment, "");
}

/// t261101-6b34: the on-disk add (`sessions::add_marker`) writes `label: ""`
/// and no palette slot, so re-adding a marker at a position that already has
/// one must not wipe its label and colour on replay.
#[test]
fn a_bare_re_add_keeps_the_label_and_colour() {
    let mut snapshot = Snapshot::default();
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 10,
        label: "boss".into(),
        palette_index: Some(3),
        comment: None,
    });
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 10,
        label: String::new(),
        palette_index: None,
        comment: None,
    });
    assert_eq!(snapshot.markers.len(), 1);
    assert_eq!(snapshot.markers[0].label, "boss");
    assert_eq!(snapshot.markers[0].palette_index, Some(3));
}

/// t261009-0251: the wire shape. `None` writes no key, a record written
/// before the field existed reads as `None`, and a checkpoint entry without
/// one reads as empty.
#[test]
fn the_marker_comment_is_an_optional_key_on_the_wire() {
    let renamed = MetaEvent::MarkerRenamed {
        time_100ns: 5,
        label: "x".into(),
        comment: None,
    };
    let json = serde_json::to_string(&renamed).unwrap();
    assert!(!json.contains("comment"), "{json}");
    let with = MetaEvent::MarkerAdded {
        time_100ns: 5,
        label: "x".into(),
        palette_index: Some(1),
        comment: Some("a\nb".into()),
    };
    let json = serde_json::to_string(&with).unwrap();
    assert!(json.contains("\"comment\":\"a\\nb\""), "{json}");
    assert_eq!(serde_json::from_str::<MetaEvent>(&json).unwrap(), with);
    assert_eq!(
        serde_json::from_str::<MetaEvent>(r#"{"kind":"markerRenamed","time_100ns":5,"label":"x"}"#)
            .unwrap(),
        renamed
    );
    let entry = MarkerEntry {
        time_100ns: 5,
        label: "x".into(),
        palette_index: None,
        comment: "c".into(),
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert_eq!(serde_json::from_str::<MarkerEntry>(&json).unwrap(), entry);
    assert_eq!(
        serde_json::from_str::<MarkerEntry>(r#"{"time_100ns":5,"label":"x","palette_index":null}"#)
            .unwrap()
            .comment,
        ""
    );
}

/// t261009-0251 互換: a reader that predates `comment` -- the shapes below are
/// `MetaEvent` / `MarkerEntry` as of a7672089 -- reads every record of a
/// container that carries comments, and the file is not truncated.
#[test]
fn a_reader_without_comments_still_reads_a_commented_container() {
    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "camelCase")]
    #[allow(dead_code)]
    enum OldMetaEvent {
        MarkerAdded {
            time_100ns: i64,
            label: String,
            palette_index: Option<usize>,
        },
        MarkerRenamed {
            time_100ns: i64,
            label: String,
        },
    }
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct OldMarkerEntry {
        time_100ns: i64,
        label: String,
        palette_index: Option<usize>,
    }
    let events = [
        MetaEvent::MarkerAdded {
            time_100ns: 10,
            label: "a".into(),
            palette_index: Some(0),
            comment: Some("one\ntwo".into()),
        },
        MetaEvent::MarkerRenamed {
            time_100ns: 10,
            label: "a".into(),
            comment: Some(String::new()),
        },
    ];
    for event in &events {
        let json = serde_json::to_string(event).unwrap();
        assert!(
            json.contains("comment"),
            "control: the key is there: {json}"
        );
        serde_json::from_str::<OldMetaEvent>(&json).unwrap();
    }
    let entry = serde_json::to_string(&MarkerEntry {
        time_100ns: 10,
        label: "a".into(),
        palette_index: None,
        comment: "c".into(),
    })
    .unwrap();
    serde_json::from_str::<OldMarkerEntry>(&entry).unwrap();

    // The real reader over a real file: every record, nothing cut.
    let dir = tmp_dir();
    let path = dir.join("commented.lvb");
    let mut builder =
        ContainerBuilder::new("capture-commented").segment(0, 0, 20_000_000, &body(0, 64));
    for event in events {
        builder = builder.meta(event);
    }
    builder
        .checkpoint()
        .segment(1, 20_000_000, 40_000_000, &body(1, 64))
        .closed()
        .build(&path)
        .unwrap();
    let reader = ContainerReader::open(&path).unwrap();
    assert!(!reader.truncated());
    assert_eq!(reader.snapshot().segments.len(), 2);
    assert!(reader.snapshot().closed);
}

#[test]
fn meta_events_fold_into_the_state_a_session_shows() {
    let mut snapshot = Snapshot::default();
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 30,
        label: "b".into(),
        palette_index: Some(1),
        comment: None,
    });
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 10,
        label: "a".into(),
        palette_index: Some(0),
        comment: None,
    });
    // Sorted by position, whatever order they arrived in.
    assert_eq!(
        snapshot
            .markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![10, 30]
    );
    // The same position twice is one marker, not two.
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 10,
        label: "a2".into(),
        palette_index: Some(0),
        comment: None,
    });
    assert_eq!(snapshot.markers.len(), 2);
    assert_eq!(snapshot.markers[0].label, "a2");

    snapshot.apply(&MetaEvent::MarkerRenamed {
        time_100ns: 30,
        label: "renamed".into(),
        comment: None,
    });
    assert_eq!(snapshot.markers[1].label, "renamed");
    snapshot.apply(&MetaEvent::MarkerRemoved { time_100ns: 10 });
    assert_eq!(snapshot.markers.len(), 1);

    snapshot.apply(&MetaEvent::CommentSet {
        comment: Some("later".into()),
    });
    snapshot.apply(&MetaEvent::ProtectedSet { protected: true });
    assert_eq!(snapshot.comment.as_deref(), Some("later"));
    assert!(snapshot.protected);
    assert!(!snapshot.closed);
    snapshot.apply(&MetaEvent::Closed);
    assert!(snapshot.closed);

    // The audio track list folds like every other setter.
    assert!(snapshot.audio_tracks.is_empty(), "single track by default");
    snapshot.apply(&MetaEvent::AudioTracksSet {
        tracks: vec![
            AudioTrackInfo {
                executable_name: "game_dx11.exe".into(),
                microphone: None,
            },
            AudioTrackInfo {
                executable_name: "Discord.exe".into(),
                microphone: None,
            },
        ],
    });
    assert_eq!(snapshot.audio_tracks.len(), 2);
    assert_eq!(snapshot.audio_tracks[0].executable_name, "game_dx11.exe");
    // Last one wins, as with the title and the note.
    snapshot.apply(&MetaEvent::AudioTracksSet {
        tracks: vec![AudioTrackInfo {
            executable_name: "game_dx11.exe".into(),
            microphone: None,
        }],
    });
    assert_eq!(snapshot.audio_tracks.len(), 1);
}

/// Writes a healthy and a cut-short container where `--liveback-inspect` can
/// be pointed at them. Ignored because it leaves files behind on purpose:
/// checking the dump *command* (not just the function it calls) needs real
/// files, and later phases want a fixture pair to hand to their own tools.
#[test]
#[ignore = "task401: writes sample containers to LIVIA_CONTAINER_SAMPLE_DIR"]
fn writes_sample_containers_for_inspection() {
    let dir = PathBuf::from(
        std::env::var("LIVIA_CONTAINER_SAMPLE_DIR")
            .expect("set LIVIA_CONTAINER_SAMPLE_DIR to a directory"),
    );
    std::fs::create_dir_all(&dir).unwrap();

    let healthy = dir.join("sample-healthy.lvb");
    ContainerBuilder::new("capture-sample")
        .segment(0, 0, 20_000_000, &body(0, 4096))
        .thumbnail(0, &body(90, 512))
        .checkpoint()
        .segment(1, 20_000_000, 40_000_000, &body(1, 4096))
        .thumbnail(1, &body(91, 512))
        .meta(MetaEvent::TitleSet {
            title: Some("Sample Game".into()),
        })
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 12_000_000,
            label: "good bit".into(),
            palette_index: Some(3),
            comment: None,
        })
        // t261009-0251: comments on both marker records, then a checkpoint
        // and more records after them, for an older build's inspect to read.
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 30_000_000,
            label: "with a comment".into(),
            palette_index: Some(4),
            comment: Some("line one\nline two".into()),
        })
        .meta(MetaEvent::MarkerRenamed {
            time_100ns: 12_000_000,
            label: "good bit".into(),
            comment: Some("added later".into()),
        })
        .checkpoint()
        .segment(2, 40_000_000, 60_000_000, &body(2, 4096))
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 50_000_000,
            label: "after the comments".into(),
            palette_index: Some(5),
            comment: None,
        })
        .closed()
        .build(&healthy)
        .unwrap();

    // Cut inside the last segment, which is what a crash mid-write leaves.
    let cut = dir.join("sample-truncated.lvb");
    std::fs::copy(&healthy, &cut).unwrap();
    let len = std::fs::metadata(&cut).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&cut)
        .unwrap()
        .set_len(len - 3000)
        .unwrap();

    // t261009-0251: the same commented records with no checkpoint (nor the
    // one `closed` writes), so a reader has to walk every Meta record.
    let scanned = dir.join("sample-commented-scan.lvb");
    ContainerBuilder::new("capture-sample-scan")
        .segment(0, 0, 20_000_000, &body(0, 4096))
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 12_000_000,
            label: "good bit".into(),
            palette_index: Some(3),
            comment: Some("line one\nline two".into()),
        })
        .meta(MetaEvent::MarkerRenamed {
            time_100ns: 12_000_000,
            label: "good bit".into(),
            comment: Some(String::new()),
        })
        .segment(1, 20_000_000, 40_000_000, &body(1, 4096))
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 30_000_000,
            label: "after the comments".into(),
            palette_index: Some(4),
            comment: None,
        })
        .build(&scanned)
        .unwrap();

    println!("healthy={}", healthy.display());
    println!("scanned={}", scanned.display());
    println!("truncated={}", cut.display());
}

/// Plan §4's IO estimate, on this machine. Not a threshold -- a recorded
/// number, so the next person has something to compare against.
#[test]
#[ignore = "task401 volume measurement: 450 segments of real size"]
fn measures_a_full_retention_window() {
    let dir = tmp_dir();
    let path = dir.join("volume.lvb");
    // 15 minutes at two seconds a segment, each about what 1080p120 produces.
    let count = 450u64;
    let chunk = 1_500_000;
    let payload = body(1, chunk);

    let started = std::time::Instant::now();
    let mut writer = ContainerWriter::create(&path, "capture-volume", 15).unwrap();
    for index in 0..count {
        writer
            .append_segment(
                index,
                index as i64 * 20_000_000,
                (index as i64 + 1) * 20_000_000,
                &[0],
                &payload,
            )
            .unwrap();
        if index % 30 == 0 {
            writer.checkpoint().unwrap();
        }
    }
    writer.checkpoint().unwrap();
    let write_elapsed = started.elapsed();
    let logical = std::fs::metadata(&path).unwrap().len();
    let allocated_before = allocated_bytes(&path).unwrap();

    let punch_started = std::time::Instant::now();
    writer.prune(count / 2).unwrap();
    let punch_elapsed = punch_started.elapsed();
    writer.close().unwrap();
    let allocated_after = allocated_bytes(&path).unwrap();

    let open_started = std::time::Instant::now();
    let reader = ContainerReader::open(&path).unwrap();
    let open_elapsed = open_started.elapsed();

    println!("segments={count} chunk={chunk}");
    println!("write={write_elapsed:?} open={open_elapsed:?} punch={punch_elapsed:?}");
    println!(
        "logical={logical} allocated_before={allocated_before} allocated_after={allocated_after}"
    );
    println!("segments_after_prune={}", reader.snapshot().segments.len());
}

// ---------- task1250: per-track audio offsets ----------

/// A v1 container, byte for byte, built by hand: the writer only emits v2 now,
/// so the old shape has to be forged to stay tested. Protected sessions outlive
/// every retention window, so this read path never gets to retire.
fn v1_container_bytes(audio_offset_100ns: i64, body: &[u8]) -> Vec<u8> {
    let dir = tmp_dir();
    let path = dir.join("forged-v1.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-v1", 15).unwrap();
    writer
        .append_segment(0, 0, 20_000_000, &[audio_offset_100ns], body)
        .unwrap();
    drop(writer);
    let mut bytes = std::fs::read(&path).unwrap();

    // Rewrite the one segment record into the v1 layout: drop the track count
    // and put the single offset back in the fourth slot.
    let scan = scan(&bytes).unwrap();
    let segment = scan
        .records
        .iter()
        .find(|r| r.kind == RecordKind::Segment)
        .unwrap();
    let payload_at = segment.payload.offset as usize;
    let payload_len = segment.payload.len as usize;
    let mut payload = bytes[payload_at..payload_at + payload_len].to_vec();
    // v2 prefix here is 40 bytes (one track); v1 is 32.
    payload.drain(24..32);
    payload[24..32].copy_from_slice(&audio_offset_100ns.to_le_bytes());
    let header_at = segment.offset as usize;
    // record header: kind | flags | payload_len(u32) | crc(u32) -- rewrite the
    // length and CRC, then splice the shorter payload in.
    let mut record = bytes[header_at..payload_at].to_vec();
    let len_at = record.len() - 8;
    record[len_at..len_at + 4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    record[len_at + 4..len_at + 8].copy_from_slice(&crc32(&payload).to_le_bytes());
    let tail = bytes[payload_at + payload_len..].to_vec();
    bytes.truncate(header_at);
    bytes.extend_from_slice(&record);
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&tail);
    // The header says v1, and both checkpoint slots are stale now, so the scan
    // replays from the top -- which is the path an old file takes anyway.
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    for slot in [SLOT_A_OFFSET, SLOT_B_OFFSET] {
        bytes[slot..slot + SLOT_LEN].fill(0);
    }
    bytes
}

#[test]
fn a_v1_segment_reads_as_a_single_track_offset() {
    let body = body(0, 512);
    let bytes = v1_container_bytes(4_200_000, &body);
    let scan = scan(&bytes).unwrap();
    assert_eq!(scan.header.format_version, 1);
    let segment = scan.snapshot.segments.first().expect("the v1 segment");
    assert_eq!(
        segment.audio_offsets_100ns,
        vec![4_200_000],
        "a v1 offset is one track, not none and not two"
    );
    // The body has to start 32 bytes in, not 40: getting this wrong would hand
    // playback eight bytes of prefix as if they were mp4.
    let at = segment.data.body.offset as usize;
    let len = segment.data.body.len as usize;
    assert_eq!(&bytes[at..at + len], &body[..]);
}

#[test]
fn v2_segments_round_trip_every_track_offset() {
    let dir = tmp_dir();
    let path = dir.join("multi-track.lvb");
    let offsets = [0i64, 21_333, -1, 999_999];
    let payload = body(3, 400);

    let mut writer = ContainerWriter::create(&path, "capture-multi", 15).unwrap();
    writer
        .append_segment(0, 0, 20_000_000, &offsets, &payload)
        .unwrap();
    // A second segment with a different track count in the same file: the
    // prefix length is per record, so nothing may assume it is fixed.
    writer
        .append_segment(1, 20_000_000, 40_000_000, &[7], &payload)
        .unwrap();
    writer.append_meta(&MetaEvent::Closed).unwrap();
    drop(writer);

    let mut reader = ContainerReader::open_for_reading(&path).unwrap();
    let snapshot = reader.snapshot();
    assert_eq!(snapshot.segments[0].audio_offsets_100ns, offsets.to_vec());
    assert_eq!(snapshot.segments[1].audio_offsets_100ns, vec![7]);
    assert_eq!(reader.read_segment(0).unwrap(), payload);
    assert_eq!(reader.read_segment(1).unwrap(), payload);
}

#[test]
fn a_corrupt_track_count_stops_the_scan_instead_of_allocating() {
    let dir = tmp_dir();
    let path = dir.join("corrupt-count.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-corrupt", 15).unwrap();
    writer
        .append_segment(0, 0, 20_000_000, &[0], &body(0, 64))
        .unwrap();
    drop(writer);
    let mut bytes = std::fs::read(&path).unwrap();
    let scan_before = scan(&bytes).unwrap();
    let segment = scan_before
        .records
        .iter()
        .find(|r| r.kind == RecordKind::Segment)
        .unwrap();
    let count_at = segment.payload.offset as usize + 24;
    bytes[count_at..count_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    // The CRC will not match either, but the point is that a track count of
    // 2^64-1 is refused on its own terms rather than turned into a Vec.
    let scanned = scan(&bytes).unwrap();
    assert!(scanned.truncated);
    assert!(scanned.snapshot.segments.is_empty());
}

/// task2350. The record the auto-record toggle identifies its target by: a
/// container that never got one has to keep reading as `None`, or every
/// recording made before this existed would fail to open.
#[test]
fn the_target_executable_folds_and_survives_a_reopen() {
    let mut snapshot = Snapshot::default();
    assert_eq!(snapshot.target_executable, None, "absent by default");
    snapshot.apply(&MetaEvent::TargetExecutableSet {
        executable: Some("mspaint.exe".into()),
        path: None,
    });
    assert_eq!(snapshot.target_executable.as_deref(), Some("mspaint.exe"));
    // Last one wins, as with the title and the note.
    snapshot.apply(&MetaEvent::TargetExecutableSet {
        executable: None,
        path: None,
    });
    assert_eq!(snapshot.target_executable, None);

    let dir = tmp_dir();
    let path = dir.join("target-executable.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-exe", 15).unwrap();
    writer
        .append_meta(&MetaEvent::TargetExecutableSet {
            executable: Some("mspaint.exe".into()),
            path: Some(r"C:\Apps\mspaint.exe".into()),
        })
        .unwrap();
    writer.close().unwrap();
    let reopened = ContainerReader::open(&path).unwrap().snapshot().clone();
    assert_eq!(reopened.target_executable.as_deref(), Some("mspaint.exe"));
    // The image path rides the same record (2026-09-20).
    assert_eq!(
        reopened.target_executable_path.as_deref(),
        Some(r"C:\Apps\mspaint.exe")
    );

    let bare = dir.join("no-target-executable.lvb");
    ContainerWriter::create(&bare, "capture-bare", 15)
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(
        ContainerReader::open(&bare)
            .unwrap()
            .snapshot()
            .target_executable,
        None,
        "a container without the record still opens"
    );
}

/// The session comment's Meta record is stored as JSON, so its variant and
/// field names are on disk (t261101-cd40). Every container written so far
/// carries this exact form; it must keep reading, and new writes must keep it.
#[test]
fn a_stored_session_comment_record_keeps_its_on_disk_form() {
    let stored = r#"{"kind":"noteSet","note":"memo"}"#;
    let event: MetaEvent = serde_json::from_str(stored).unwrap();
    let mut snapshot = Snapshot::default();
    snapshot.apply(&event);
    assert_eq!(snapshot.comment.as_deref(), Some("memo"));
    assert_eq!(serde_json::to_string(&event).unwrap(), stored);
}

// ---------- inspect summary (task2890) ----------

#[test]
fn inspect_summary_answers_from_the_checkpoint_alone() {
    let dir = tmp_dir();
    let path = dir.join("summary.lvb");
    ContainerBuilder::new("session-summary")
        .retention_minutes(20)
        .segment(0, 100, 200, &body(0, 64))
        .segment(1, 200, 350, &body(1, 64))
        .meta(MetaEvent::TitleSet {
            title: Some("mspaint の検証".into()),
        })
        .meta(MetaEvent::CommentSet {
            comment: Some("throwaway".into()),
        })
        .meta(MetaEvent::ProtectedSet { protected: true })
        .closed()
        .build(&path)
        .unwrap();

    let summary = inspect_summary(&path).unwrap();
    let lines: Vec<&str> = summary.lines().collect();
    assert!(
        lines[0].starts_with("magic=LVBF version=")
            && lines[0].contains("session=session-summary")
            && lines[0].contains("retention=20min"),
        "{summary}"
    );
    assert!(summary.contains("title=mspaint の検証\n"), "{summary}");
    assert!(summary.contains("note=throwaway\n"), "{summary}");
    assert!(summary.contains("protected=true\n"), "{summary}");
    // The span comes from the first and last segment, not from a scan.
    assert!(
        summary.contains("segments=2 span=100..350 closed=true\n"),
        "{summary}"
    );
    assert!(
        summary.contains("audio tracks: (none recorded -- single track)\n"),
        "{summary}"
    );
    // No record listing: the default reading never scans.
    assert!(!summary.contains("segment 0"), "{summary}");
}

#[test]
fn inspect_summary_names_full_instead_of_scanning_without_a_checkpoint() {
    let dir = tmp_dir();
    let path = dir.join("no-checkpoint.lvb");
    // Records on disk, but the writer died before its first checkpoint. The
    // scan would find the segment; the default reading must not go looking.
    ContainerBuilder::new("session-crashed")
        .segment(0, 100, 200, &body(0, 64))
        .meta(MetaEvent::TitleSet {
            title: Some("never checkpointed".into()),
        })
        .build(&path)
        .unwrap();

    let summary = inspect_summary(&path).unwrap();
    assert!(summary.contains("slot A: empty or corrupt\n"), "{summary}");
    assert!(
        summary.contains("no usable checkpoint; rerun with --full to scan\n"),
        "{summary}"
    );
    // Nothing that only a scan could have produced.
    assert!(!summary.contains("title="), "{summary}");
    assert!(!summary.contains("segments="), "{summary}");
    // `--full` still sees it -- that is the point of keeping the two apart.
    let full = dump(&std::fs::read(&path).unwrap());
    assert!(full.contains("Segment"), "{full}");
}

/// What a full ring leaves allocated beyond its live records, after `passes`
/// indexer-shaped passes (t260913-e20f): append one segment, prune the oldest
/// once the ring holds `ring` of them, checkpoint. Returns the residue, the
/// last checkpoint's payload length, the path and the snapshot the writer
/// ended with. The writer is dropped unclosed, as a crash would leave it, and
/// only then measured: NTFS reports allocation lazily under a write handle.
fn steady_state_residue(
    dir: &Path,
    ring: u64,
    passes: u64,
    chunk: usize,
) -> Option<(i64, u64, PathBuf, Snapshot)> {
    let path = dir.join(format!("steady-{ring}-{passes}.lvb"));
    let mut writer = ContainerWriter::create(&path, "capture-steady", 15).unwrap();
    if !writer.sparse() {
        return None;
    }
    writer
        .append_meta(&MetaEvent::TitleSet {
            title: Some("steady".into()),
        })
        .unwrap();
    for index in 0..passes {
        writer
            .append_segment(
                index,
                index as i64 * 20_000_000,
                (index as i64 + 1) * 20_000_000,
                &[0],
                &body(index, chunk),
            )
            .unwrap();
        if index >= ring {
            writer.prune(index + 1 - ring).unwrap();
        }
        writer.checkpoint().unwrap();
    }
    let snapshot = writer.snapshot().clone();
    let checkpoint_len = encode_checkpoint(&snapshot).unwrap().len() as u64;
    let live: u64 = snapshot
        .segments
        .iter()
        .map(|segment| segment.data.record.len + RECORD_HEADER_LEN as u64)
        .sum();
    drop(writer);
    let allocated = allocated_bytes(&path).unwrap();
    Some((
        allocated as i64 - live as i64,
        checkpoint_len,
        path,
        snapshot,
    ))
}

/// The steady-state prune drops one segment per pass, and the `Pruned` meta
/// plus the two checkpoints written right after that segment's own pass sit
/// between it and the next survivor. Before t260913-e20f no span ever covered
/// them, so a recording kept one pair of whole-`Snapshot` checkpoints per pass
/// allocated forever -- growth proportional to the pass count.
#[test]
fn a_steady_state_prune_reclaims_the_checkpoints_it_passes() {
    let dir = tmp_dir();
    let ring = 100;
    let chunk = 16 * 1024;
    // Both runs start past 2 * ring: the debris a prune reaches was written a
    // ring's worth of passes earlier, and the ring's live tail holds a ring's
    // worth of checkpoints, so before that both are still growing.
    let Some((short, checkpoint_len, _, _)) = steady_state_residue(&dir, ring, 3 * ring, chunk)
    else {
        eprintln!("volume is not sparse-capable; hole punch skipped");
        return;
    };
    let (long, _, path, before) = steady_state_residue(&dir, ring, 3 * ring + 160, chunk).unwrap();
    let per_pass = (long - short) / 160;
    eprintln!(
        "ring={ring} checkpoint={checkpoint_len}B ({}B/segment) residue short={short} long={long} per_pass={per_pass}",
        checkpoint_len / ring
    );
    // Proportional would be ~2 checkpoints a pass; a plateau leaves at most
    // rounding at the edges of the holes.
    assert!(
        per_pass < checkpoint_len as i64 / 4,
        "residue grows {per_pass} B a pass against a {checkpoint_len} B checkpoint"
    );

    // The holes stay behind every reader: both open paths and recovery see
    // exactly what the writer last had.
    let cheap = ContainerReader::open_for_reading(&path).unwrap();
    assert_eq!(cheap.snapshot().segments, before.segments);
    assert_eq!(cheap.snapshot().title.as_deref(), Some("steady"));
    let mut full = ContainerReader::open(&path).unwrap();
    assert_eq!(full.snapshot().segments, before.segments);
    // The older slot is the fallback when the newer checkpoint is damaged, so
    // it must have survived too.
    let mut bytes = std::fs::read(&path).unwrap();
    let (_, slot_a, slot_b) = decode_header(&bytes).unwrap();
    let newest = [slot_a, slot_b]
        .into_iter()
        .flatten()
        .max_by_key(|slot| slot.generation)
        .unwrap();
    bytes[newest.offset as usize] ^= 0xFF;
    let fallback = scan(&bytes).unwrap();
    assert_eq!(fallback.snapshot.segments, before.segments);
    let recovered = recover(&path).unwrap();
    assert_eq!(recovered.segments, before.segments);
    assert_eq!(recovered.title.as_deref(), Some("steady"));
    let reopened = ContainerReader::open_for_reading(&path).unwrap();
    assert_eq!(reopened.snapshot().segments.len(), ring as usize);
    for segment in &before.segments {
        assert_eq!(
            full.read_segment(segment.index).unwrap(),
            body(segment.index, chunk)
        );
    }
}

/// The extended span runs up to the first survivor, which on a small ring can
/// take in a checkpoint a header slot still names. The older slot is the
/// fallback when the newer one is damaged, so the hole has to stop short of it
/// (t260913-e20f).
#[test]
fn the_hole_stops_short_of_a_slotted_checkpoint() {
    let dir = tmp_dir();
    let path = dir.join("slot-guard.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-slotguard", 15).unwrap();
    for index in 0..2u64 {
        writer
            .append_segment(index, 0, 20_000_000, &[0], &body(index, 64))
            .unwrap();
    }
    // Slot A: after seg1, so inside the [seg0 .. seg2) the prune reaches.
    writer.checkpoint().unwrap();
    for index in 2..4u64 {
        // Ending a checkpoint interval later, so the prune's own checkpoint
        // is due (t261012-fe57).
        writer
            .append_segment(index, 0, 400_000_000, &[0], &body(index, 64))
            .unwrap();
    }
    // Its own checkpoint lands in slot B.
    writer.prune(2).unwrap();
    assert_eq!(writer.checkpoint_generation(), 2, "the prune checkpointed");
    drop(writer);

    let mut bytes = std::fs::read(&path).unwrap();
    let (_, slot_a, slot_b) = decode_header(&bytes).unwrap();
    let newest = [slot_a, slot_b]
        .into_iter()
        .flatten()
        .max_by_key(|slot| slot.generation)
        .unwrap();
    bytes[newest.offset as usize] ^= 0xFF;
    let fallback = scan(&bytes).unwrap();
    let indices: Vec<u64> = fallback
        .snapshot
        .segments
        .iter()
        .map(|segment| segment.index)
        .collect();
    assert_eq!(indices, vec![2, 3]);
}

/// The span a prune computes runs past the dropped segment up to the next
/// survivor's record header, taking in what the pass wrote after it
/// (t260913-e20f). Checked on the span itself because the steady-state test's
/// aligned hole start would also sweep that debris up a pass later.
#[test]
fn the_prunable_span_runs_up_to_the_first_survivor() {
    let dir = tmp_dir();
    let path = dir.join("span-extends.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-spanextends", 15).unwrap();
    writer
        .append_segment(0, 0, 20_000_000, &[0], &body(0, 64))
        .unwrap();
    writer
        .append_meta(&MetaEvent::TitleSet {
            title: Some("debris".into()),
        })
        .unwrap();
    writer.checkpoint().unwrap();
    let survivor = writer
        .append_segment(1, 20_000_000, 40_000_000, &[0], &body(1, 64))
        .unwrap();
    let before = writer.snapshot().clone();
    let mut after = before.clone();
    after.apply(&MetaEvent::Pruned { keep_from_index: 1 });
    let span = after.prunable_span(&before).unwrap();
    assert_eq!(span.offset, before.segments[0].data.record.offset);
    assert_eq!(
        span.offset + span.len,
        survivor.located.record.offset - RECORD_HEADER_LEN as u64
    );
}

// ---------- t260913-fff8: prune cost is linear in the segment count ----------

/// `count` segments laid end to end the way a pass writes them, thumbnail
/// first -- no file behind them, only the entries `prune` walks.
fn synthetic_snapshot(count: u64) -> Snapshot {
    const THUMB: u64 = 200_000;
    const DATA: u64 = 3_000_000;
    let header = RECORD_HEADER_LEN as u64;
    let stride = 2 * header + THUMB + DATA;
    let segments = (0..count)
        .map(|index| {
            let thumb = HEADER_LEN + index * stride + header;
            let data = thumb + THUMB + header;
            let located = |offset, len| Located {
                record: Span { offset, len },
                body: Span { offset, len },
                crc: 0,
            };
            SegmentEntry {
                index,
                start_100ns: index as i64 * 20_000_000,
                end_100ns: (index as i64 + 1) * 20_000_000,
                audio_offsets_100ns: vec![0],
                data: located(data, DATA),
                thumbnail: Some(located(thumb, THUMB)),
                fragments: None,
            }
        })
        .collect();
    Snapshot {
        segments,
        ..Snapshot::default()
    }
}

/// A day of two-second segments (43,200) prunes one segment well inside the
/// two-second index pass it runs in. The drop used to be found by an n^2
/// `any` scan over a full clone of the snapshot.
#[test]
fn a_day_of_segments_prunes_in_linear_time() {
    for count in [3_600u64, 43_200] {
        let before = synthetic_snapshot(count);
        let mut after = before.clone();
        after.apply(&MetaEvent::Pruned { keep_from_index: 1 });
        let started = std::time::Instant::now();
        let span = after.prunable_span(&before).unwrap();
        println!("prunable_span count={count} {:?}", started.elapsed());
        assert_eq!(
            span.offset,
            before.segments[0].thumbnail.unwrap().record.offset
        );
    }

    let dir = tmp_dir();
    let path = dir.join("day.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-day", 15).unwrap();
    writer.snapshot = synthetic_snapshot(43_200);
    let started = std::time::Instant::now();
    writer.prune(1).unwrap();
    let elapsed = started.elapsed();
    println!("prune count=43200 {elapsed:?}");
    assert_eq!(writer.snapshot.segments.len(), 43_199);
    // Measured 275 ms after the fix (the checkpoint dominates), 3.6 s before.
    assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
}

/// Opening without a checkpoint holds one record's payload at a time, not the
/// file (t260913-1e2e). Two sizes, same bodies: the buffer peak is the same for
/// both, so it follows the largest record and not the file length. Both real
/// paths that replay -- `ContainerReader::open` and `recover` -- are checked.
#[test]
fn a_replay_holds_one_record_not_the_file() {
    let dir = tmp_dir();
    let mut peaks = Vec::new();
    for count in [8u64, 128] {
        let path = dir.join(format!("replay-{count}.lvb"));
        let mut builder = ContainerBuilder::new("capture-replay");
        for index in 0..count {
            let start = index as i64 * 20_000_000;
            builder = builder.segment(index, start, start + 20_000_000, &body(index, 4096));
        }
        builder.build(&path).unwrap();
        let file_len = std::fs::metadata(&path).unwrap().len() as usize;

        take_scan_buffer_peak();
        let reader = ContainerReader::open(&path).unwrap();
        assert_eq!(reader.snapshot().segments.len(), count as usize);
        let open_peak = take_scan_buffer_peak();
        recover(&path).unwrap();
        let recover_peak = take_scan_buffer_peak();

        println!(
            "count={count} file_len={file_len} open_peak={open_peak} recover_peak={recover_peak}"
        );
        assert!(open_peak > 0, "the probe never saw the buffer");
        assert_eq!(open_peak, recover_peak);
        assert!(open_peak <= MAX_PAYLOAD_LEN as usize + RECORD_HEADER_LEN);
        assert!(open_peak * 8 < file_len, "{open_peak} of {file_len}");
        peaks.push(open_peak);
    }
    assert_eq!(peaks[0], peaks[1], "the peak grew with the file");
}

/// The replay keeps the **last** record for an index (t260913-1e2e): a segment
/// re-appended after a prune replaces the earlier one. Also covers the
/// `Pruned` meta on the replay's own segment map, marker cutoff included.
#[test]
fn a_replayed_index_keeps_the_later_record() {
    let dir = tmp_dir();
    let path = dir.join("last-wins.lvb");
    ContainerBuilder::new("capture-last-wins")
        .segment(0, 0, 20_000_000, &body(0, 64))
        .segment(1, 20_000_000, 40_000_000, &body(1, 64))
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 10_000_000,
            label: "old".into(),
            palette_index: None,
            comment: None,
        })
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 30_000_000,
            label: "kept".into(),
            palette_index: None,
            comment: None,
        })
        .segment(5, 100_000_000, 120_000_000, &body(5, 64))
        .segment(5, 100_000_000, 120_000_000, &body(6, 64))
        .meta(MetaEvent::Pruned { keep_from_index: 1 })
        .build(&path)
        .unwrap();

    let mut reader = ContainerReader::open(&path).unwrap();
    assert_eq!(
        reader.checkpoint_end(),
        None,
        "must be the replay, not a checkpoint"
    );
    let indexes: Vec<u64> = reader.snapshot().segments.iter().map(|s| s.index).collect();
    assert_eq!(indexes, vec![1, 5]);
    assert_eq!(reader.read_segment(5).unwrap(), body(6, 64));
    let markers: Vec<&str> = reader
        .snapshot()
        .markers
        .iter()
        .map(|marker| marker.label.as_str())
        .collect();
    assert_eq!(markers, vec!["kept"]);
}

// ---------- t261012-fe57: checkpoint cadence ----------

/// What a full ring holds besides its live records, in the shape the indexer
/// writes it: a two-second segment and its thumbnail per pass, the oldest
/// pruned once the ring holds `ring`, then the pass's checkpoint. Two rings'
/// worth of passes, so every record left in the window was written in the
/// steady state.
///
/// `every_pass` is the cadence before t261012-fe57, kept callable as the
/// control: `prune` checkpointing for itself and the pass checkpointing again.
/// On the v2 build this same fixture (with `prune` + `checkpoint` and nothing
/// else) measured ring=150 13,429,288 B / 300 checkpoints and ring=300
/// 53,662,800 B / 600 checkpoints (evidence/t261012-fe57-checkpoint-cadence).
struct Cadence {
    /// Allocated bytes beyond the live segment and thumbnail records.
    residue: i64,
    /// `Checkpoint` records from the first survivor to the end of the file.
    checkpoints: usize,
    checkpoint_bytes: u64,
    /// The newest checkpoint's payload, i.e. one full-ring snapshot.
    last_checkpoint: u64,
}

fn checkpoint_cadence(dir: &Path, ring: u64, every_pass: bool) -> Option<Cadence> {
    let path = dir.join(format!("cadence-{ring}-{every_pass}.lvb"));
    let mut writer = ContainerWriter::create(&path, "capture-cadence", 15).unwrap();
    if !writer.sparse() {
        return None;
    }
    for index in 0..2 * ring {
        let start = index as i64 * 20_000_000;
        writer
            .append_segment(index, start, start + 20_000_000, &[0], &body(index, 1024))
            .unwrap();
        writer.append_thumbnail(index, &body(index, 256)).unwrap();
        if index >= ring {
            writer.prune(index + 1 - ring).unwrap();
            if every_pass {
                writer.checkpoint().unwrap();
            }
        }
        if every_pass {
            writer.checkpoint().unwrap();
        } else {
            writer.checkpoint_if_due().unwrap();
        }
    }
    let snapshot = writer.snapshot().clone();
    let (_, slot_a, slot_b) = {
        let mut header = vec![0u8; HEADER_LEN as usize];
        (&writer.file).seek(SeekFrom::Start(0)).unwrap();
        (&writer.file).read_exact(&mut header).unwrap();
        decode_header(&header).unwrap()
    };
    drop(writer);
    let live: u64 = snapshot
        .segments
        .iter()
        .flat_map(|segment| [Some(segment.data), segment.thumbnail])
        .flatten()
        .map(|located| located.record.len + RECORD_HEADER_LEN as u64)
        .sum();
    let residue = allocated_bytes(&path).unwrap() as i64 - live as i64;
    // The front of the file is punched, so no scan starts there: walk the
    // window record by record from the first survivor's header.
    let first = snapshot.segments.first().unwrap();
    let from = first.thumbnail.map_or(first.data.record.offset, |thumb| {
        thumb.record.offset.min(first.data.record.offset)
    }) - RECORD_HEADER_LEN as u64;
    let mut window = Vec::new();
    let mut file = File::open(&path).unwrap();
    file.seek(SeekFrom::Start(from)).unwrap();
    file.read_to_end(&mut window).unwrap();
    let (mut checkpoints, mut checkpoint_bytes, mut cursor) = (0, 0, 0u64);
    while let Some((location, _)) = read_record_at(&window, cursor) {
        if location.kind == RecordKind::Checkpoint {
            checkpoints += 1;
            checkpoint_bytes += location.payload.len;
        }
        cursor = location.payload.offset + location.payload.len;
    }
    assert_eq!(cursor, window.len() as u64, "the walk reached the end");
    let newest = [slot_a, slot_b]
        .into_iter()
        .flatten()
        .max_by_key(|slot| slot.generation)
        .unwrap();
    Some(Cadence {
        residue,
        checkpoints,
        checkpoint_bytes,
        last_checkpoint: newest.len,
    })
}

/// A full two-second ring of 300 keeps one checkpoint per 30 s of media in its
/// window, not one or two per pass (t261012-fe57). The control is the old
/// cadence through the same fixture in the same build: it shows the walk does
/// see checkpoints, and that the saving is the cadence's alone.
#[test]
fn a_full_ring_keeps_one_checkpoint_per_interval() {
    /// This fixture's residue at ring=300 on the v2 build (JSON, two
    /// checkpoints a pass): the baseline the task is measured against.
    const V2_BUILD_RESIDUE: i64 = 53_662_800;
    let dir = tmp_dir();
    let Some(gated) = checkpoint_cadence(&dir, 300, false) else {
        eprintln!("volume is not sparse-capable; residue not measurable");
        return;
    };
    let control = checkpoint_cadence(&dir, 300, true).unwrap();
    for (name, cadence) in [("interval", &gated), ("every-pass", &control)] {
        println!(
            "ring=300 {name} residue={} checkpoints={} checkpoint_bytes={} last_checkpoint={}",
            cadence.residue, cadence.checkpoints, cadence.checkpoint_bytes, cadence.last_checkpoint
        );
    }
    assert!(
        control.checkpoints >= 600,
        "the walk sees the control's checkpoints: {}",
        control.checkpoints
    );
    assert!(
        gated.checkpoints <= 22,
        "{} checkpoints in a 600 s window",
        gated.checkpoints
    );
    assert!(
        gated.checkpoint_bytes * 20 <= control.checkpoint_bytes,
        "{} checkpoint bytes against the every-pass {}",
        gated.checkpoint_bytes,
        control.checkpoint_bytes
    );
    assert!(
        gated.residue * 20 <= V2_BUILD_RESIDUE,
        "residue {} against the v2 build's {V2_BUILD_RESIDUE}",
        gated.residue
    );
}

/// Step 1 / 7 of t261012-fe57: the residue at two ring sizes, for both
/// cadences, to show how each grows with the ring and to extrapolate to 3600 /
/// 7200 segments.
#[test]
#[ignore = "t261012-fe57 measurement; numbers in evidence/t261012-fe57-checkpoint-cadence"]
fn measures_checkpoint_residue_by_ring_size() {
    let dir = tmp_dir();
    for ring in [150, 300] {
        for every_pass in [true, false] {
            let Some(cadence) = checkpoint_cadence(&dir, ring, every_pass) else {
                eprintln!("volume is not sparse-capable");
                return;
            };
            println!(
                "ring={ring} every_pass={every_pass} residue={} checkpoints={} checkpoint_bytes={} last_checkpoint={}",
                cadence.residue, cadence.checkpoints, cadence.checkpoint_bytes, cadence.last_checkpoint
            );
        }
    }
}

/// The same 3600-segment snapshot (a two-hour ring of two-second segments) is
/// under 40 % of its JSON as v3's MessagePack (t261012-fe57). Fails if the
/// encoding becomes `to_vec_named`, which spells every field name again.
#[test]
fn a_messagepack_checkpoint_is_under_two_fifths_of_its_json() {
    let mut snapshot = synthetic_snapshot(3_600);
    // Real CRCs are full-width; `synthetic_snapshot`'s zeros would flatter
    // MessagePack, which writes a small number small.
    for segment in &mut snapshot.segments {
        segment.data.crc = crc32(&segment.index.to_le_bytes());
        if let Some(thumbnail) = segment.thumbnail.as_mut() {
            thumbnail.crc = !segment.data.crc;
        }
    }
    let msgpack = encode_checkpoint(&snapshot).unwrap().len();
    let json = serde_json::to_vec(&snapshot).unwrap().len();
    let named = rmp_serde::to_vec_named(&snapshot).unwrap().len();
    println!(
        "3600 segments: messagepack={msgpack} json={json} ({:.1}%) named={named} ({:.1}%)",
        msgpack as f64 * 100.0 / json as f64,
        named as f64 * 100.0 / json as f64
    );
    assert!(
        msgpack * 5 <= json * 2,
        "{msgpack} B against JSON's {json} B"
    );
    // Control: the named form is what this guards against, and it does fail.
    assert!(named * 5 > json * 2, "{named} B against JSON's {json} B");
}

/// v3 decodes everything it encodes (t261012-fe57). The array form reads by
/// position, so this is what fails if a skipped field stops being last or a
/// new field lands in the middle: the shapes below put an absent value next
/// to a present one wherever a struct has an optional field.
#[test]
fn v3_checkpoints_round_trip_every_shape() {
    let located = |offset: u64| Located {
        record: Span { offset, len: 40 },
        body: Span {
            offset: offset + 8,
            len: 32,
        },
        crc: 0xDEAD_BEEF,
    };
    let marker = |time_100ns, palette_index, comment: &str| MarkerEntry {
        time_100ns,
        label: format!("marker {time_100ns}"),
        palette_index,
        comment: comment.into(),
    };
    let snapshot = Snapshot {
        segments: vec![
            SegmentEntry {
                index: 0,
                start_100ns: -5,
                end_100ns: 20_000_000,
                audio_offsets_100ns: Vec::new(),
                data: located(4_106),
                thumbnail: None,
                fragments: None,
            },
            SegmentEntry {
                index: 7,
                start_100ns: 20_000_000,
                end_100ns: 40_000_000,
                audio_offsets_100ns: vec![21_333, -1, i64::MAX],
                data: located(9_000_000_000),
                thumbnail: Some(located(8_999_000_000)),
                fragments: fragment_table(&fmp4::body(1, &fmp4::probe_layout()).0),
            },
        ],
        markers: vec![
            marker(1, None, ""),
            marker(2, Some(3), "two\nlines"),
            marker(3, None, "comment, no colour"),
            marker(4, Some(0), ""),
        ],
        audio_tracks: vec![
            AudioTrackInfo {
                executable_name: "game.exe".into(),
                microphone: None,
            },
            AudioTrackInfo {
                executable_name: "マイク".into(),
                microphone: Some(String::new()),
            },
        ],
        title: Some("title".into()),
        target_executable: Some("game.exe".into()),
        target_executable_path: None,
        comment: None,
        protected: true,
        closed: false,
    };
    let bytes = encode_checkpoint(&snapshot).unwrap();
    assert_eq!(decode_checkpoint(FORMAT_VERSION, &bytes), Some(snapshot));

    // The pre-task1250 scalar reads as a one-track list in this encoding too:
    // `scalar_or_list` goes through `deserialize_any`, which MessagePack can
    // answer because its values carry their own type.
    #[derive(Serialize)]
    struct ScalarSegment {
        index: u64,
        start_100ns: i64,
        end_100ns: i64,
        audio_offset_100ns: i64,
        data: Located,
        thumbnail: Option<Located>,
    }
    let scalar = rmp_serde::to_vec(&ScalarSegment {
        index: 3,
        start_100ns: 0,
        end_100ns: 20_000_000,
        audio_offset_100ns: 4_200_000,
        data: located(5_000),
        thumbnail: None,
    })
    .unwrap();
    let read: SegmentEntry = rmp_serde::from_slice(&scalar).unwrap();
    assert_eq!(read.audio_offsets_100ns, vec![4_200_000]);
}

/// The header's version and each slot's checkpoint payload, newest first.
fn slot_payloads(bytes: &[u8]) -> (u32, Vec<&[u8]>) {
    let (header, slot_a, slot_b) = decode_header(bytes).unwrap();
    let mut slots: Vec<Slot> = [slot_a, slot_b].into_iter().flatten().collect();
    slots.sort_by_key(|slot| std::cmp::Reverse(slot.generation));
    let payloads = slots
        .iter()
        .map(|slot| {
            let (location, payload) =
                read_record_at(bytes, slot.offset - RECORD_HEADER_LEN as u64).unwrap();
            assert_eq!(location.kind, RecordKind::Checkpoint);
            payload
        })
        .collect();
    (header.format_version, payloads)
}

/// Scribbles over whichever header slot names the newer checkpoint.
fn corrupt_newest_slot(bytes: &mut [u8]) {
    let (_, slot_a, slot_b) = decode_header(bytes).unwrap();
    let generation = |slot: Option<Slot>| slot.map_or(0, |slot| slot.generation);
    let at = if generation(slot_a) > generation(slot_b) {
        SLOT_A_OFFSET
    } else {
        SLOT_B_OFFSET
    };
    for byte in &mut bytes[at..at + SLOT_LEN] {
        *byte ^= 0xFF;
    }
}

/// A new container is v3 and its checkpoints are MessagePack, not JSON
/// (t261012-fe57).
#[test]
fn a_new_container_is_v3_with_messagepack_checkpoints() {
    let dir = tmp_dir();
    let path = dir.join("v3.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-v3", 15).unwrap();
    writer
        .append_segment(0, 0, 20_000_000, &[0], &body(0, 64))
        .unwrap();
    writer.checkpoint().unwrap();
    writer.close().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let (version, payloads) = slot_payloads(&bytes);
    assert_eq!(version, 3);
    assert_eq!(payloads.len(), 2);
    for payload in payloads {
        let snapshot: Snapshot = rmp_serde::from_slice(payload).expect("MessagePack");
        assert_eq!(snapshot.segments.len(), 1);
        assert!(
            serde_json::from_slice::<Snapshot>(payload).is_err(),
            "not JSON"
        );
    }
}

/// A recording checkpoints once per interval now, so a crash leaves up to an
/// interval of segments behind its last checkpoint (t261012-fe57). Recovery
/// replays them: everything but the record the crash cut comes back, markers
/// on both sides of the checkpoint included.
#[test]
fn recovery_replays_the_segments_behind_the_last_checkpoint() {
    let dir = tmp_dir();
    let path = dir.join("behind.lvb");
    let marker = |time_100ns, label: &str| MetaEvent::MarkerAdded {
        time_100ns,
        label: label.into(),
        palette_index: None,
        comment: None,
    };
    let mut writer = ContainerWriter::create(&path, "capture-behind", 15).unwrap();
    for index in 0..10u64 {
        let start = index as i64 * 20_000_000;
        writer
            .append_segment(index, start, start + 20_000_000, &[0], &body(index, 256))
            .unwrap();
        if index == 3 {
            writer.append_meta(&marker(start, "before")).unwrap();
        }
        writer.checkpoint_if_due().unwrap();
    }
    writer.checkpoint().unwrap();
    // Twenty one-second segments: 20 s of media, inside the interval, so the
    // recording checkpoints none of them.
    for n in 0..20u64 {
        let index = 10 + n;
        let start = 200_000_000 + n as i64 * 10_000_000;
        writer
            .append_segment(index, start, start + 10_000_000, &[0], &body(index, 256))
            .unwrap();
        if n == 5 {
            writer.append_meta(&marker(start, "behind")).unwrap();
        }
        assert!(
            !writer.checkpoint_if_due().unwrap(),
            "segment {index} stays behind the checkpoint"
        );
    }
    let last = writer.snapshot().segments.last().unwrap().data;
    drop(writer);
    // The crash: the last segment's record cut halfway.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(last.record.offset + last.record.len / 2)
        .unwrap();

    let recovered = recover(&path).unwrap();
    let indexes: Vec<u64> = recovered.segments.iter().map(|s| s.index).collect();
    assert_eq!(indexes, (0..29).collect::<Vec<_>>());
    let labels: Vec<&str> = recovered.markers.iter().map(|m| m.label.as_str()).collect();
    assert_eq!(labels, vec!["before", "behind"]);
    assert!(recovered.closed);
    let mut reader = ContainerReader::open_for_reading(&path).unwrap();
    assert_eq!(reader.header().format_version, 3);
    assert_eq!(
        reader.checkpoint_end(),
        Some(reader.valid_end()),
        "recovery's checkpoint is the last record"
    );
    for index in 0..29 {
        assert_eq!(reader.read_segment(index).unwrap(), body(index, 256));
    }
}

/// A real v2 container, written by the v2 writer before this format existed
/// (t261012-fe57): three segments with two audio tracks, a thumbnail on the
/// first, a title, two markers (one commented), protected, closed; JSON
/// checkpoints in both slots.
const V2_FIXTURE: &[u8] = include_bytes!("fixtures/v2-closed.lvb");

/// `recover` is one of the two writers that go through `reopen`, so it
/// upgrades a v2 file (t261012-fe57). The upgrade's checkpoint and recovery's
/// own fill both slots with MessagePack, so with the newer slot damaged the
/// older one still opens at its checkpoint instead of by replay.
#[test]
fn recovering_a_v2_container_upgrades_both_slots() {
    let dir = tmp_dir();
    let path = dir.join("v2.lvb");
    std::fs::write(&path, V2_FIXTURE).unwrap();
    assert_eq!(slot_payloads(V2_FIXTURE).0, 2, "the fixture is v2");
    let before = ContainerReader::open_for_reading(&path)
        .unwrap()
        .snapshot()
        .clone();
    let indexes: Vec<u64> = before.segments.iter().map(|s| s.index).collect();
    assert_eq!(indexes, vec![0, 1, 2]);
    assert!(before.protected && before.closed);
    assert_eq!(before.title.as_deref(), Some("v2 fixture"));
    assert_eq!(before.markers[0].comment, "commented");

    assert_eq!(recover(&path).unwrap(), before);
    let mut bytes = std::fs::read(&path).unwrap();
    let (version, payloads) = slot_payloads(&bytes);
    assert_eq!(version, 3);
    assert_eq!(payloads.len(), 2);
    for payload in payloads {
        assert_eq!(decode_checkpoint(3, payload).as_ref(), Some(&before));
    }
    corrupt_newest_slot(&mut bytes);
    std::fs::write(&path, &bytes).unwrap();
    let mut reader = ContainerReader::open_for_reading(&path).unwrap();
    assert!(
        reader.checkpoint_end().is_some(),
        "opened at the older checkpoint, not by replay"
    );
    assert_eq!(reader.snapshot(), &before);
    assert_eq!(reader.read_segment(2).unwrap(), body(2, 64));
}

/// v1 is read-only (t261012-fe57): `reopen`, the one way into writing a file,
/// refuses it before a byte changes, so both the edit path and recovery leave
/// it as it was; and it still opens. `HEADER_LEN` as the valid end would cut
/// the whole file if the refusal came after the truncation.
#[test]
fn a_v1_container_refuses_writes_and_still_opens() {
    let dir = tmp_dir();
    let path = dir.join("v1.lvb");
    let body = body(0, 512);
    let original = v1_container_bytes(4_200_000, &body);
    std::fs::write(&path, &original).unwrap();
    let snapshot = scan(&original).unwrap().snapshot;
    let refused = ContainerWriter::reopen(&path, HEADER_LEN, snapshot).err();
    assert_eq!(
        refused.map(|error| error.kind()),
        Some(io::ErrorKind::Unsupported)
    );
    assert!(recover(&path).is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "not a byte changed"
    );
    let mut reader = ContainerReader::open(&path).unwrap();
    assert_eq!(reader.header().format_version, 1);
    assert_eq!(reader.read_segment(0).unwrap(), body);
}

// ---------- fragment table (t261017-3f94) ----------

/// A hand-built fragmented MP4 in the production writer's shape: `ftyp`,
/// `moov` (a 90kHz video `trak`, 48kHz audio `trak`s, `mvex`/`trex`), then
/// `moof`+`mdat` pairs whose every `moof` carries a `traf` per track, empty
/// ones included, with `default-base-is-moof` and a version-1 `tfdt` -- what
/// t261017-2334's probe read off the real `Mp4SegmentWriter`.
mod fmp4 {
    pub fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn full(version: u8, flags: u32, rest: &[u8]) -> Vec<u8> {
        let mut out = ((version as u32) << 24 | flags).to_be_bytes().to_vec();
        out.extend_from_slice(rest);
        out
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    pub const VIDEO_TIMESCALE: u32 = 90_000;
    pub const AUDIO_TIMESCALE: u32 = 48_000;
    pub const NON_SYNC: u32 = 0x0001_0000;

    fn trak(id: u32, timescale: u32, handler: &[u8; 4]) -> Vec<u8> {
        let tkhd = bx(b"tkhd", &full(0, 3, &words(&[0, 0, id, 0, 0])));
        let mdhd = bx(b"mdhd", &full(0, 0, &words(&[0, 0, timescale, 0, 0])));
        let mut hdlr = words(&[0]);
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0; 13]);
        let hdlr = bx(b"hdlr", &full(0, 0, &hdlr));
        bx(
            b"trak",
            &[tkhd, bx(b"mdia", &[mdhd, hdlr].concat())].concat(),
        )
    }

    /// `ftyp` + `moov` with track 1 video and `audio_tracks` audio tracks
    /// numbered from 2. `trex` says a video sample is a non-sync one unless
    /// told otherwise, as an encoder's would.
    pub fn head(audio_tracks: u32) -> Vec<u8> {
        let mut moov = trak(1, VIDEO_TIMESCALE, b"vide");
        let mut mvex = bx(b"trex", &full(0, 0, &words(&[1, 1, 0, 0, NON_SYNC])));
        for track in 0..audio_tracks {
            moov.extend(trak(2 + track, AUDIO_TIMESCALE, b"soun"));
            mvex.extend(bx(b"trex", &full(0, 0, &words(&[2 + track, 1, 0, 0, 0]))));
        }
        moov.extend(bx(b"mvex", &mvex));
        [bx(b"ftyp", b"iso6\0\0\0\0"), bx(b"moov", &moov)].concat()
    }

    /// One `traf`: `samples` samples from `decode_time`; `first_flags` set
    /// writes a `trun` `first_sample_flags`, which is how an encoder marks a
    /// keyframe against a non-sync `trex`.
    fn traf(track: u32, decode_time: u64, samples: u32, first_flags: Option<u32>) -> Vec<u8> {
        let tfhd = bx(b"tfhd", &full(0, 0x02_0000, &words(&[track])));
        let tfdt = bx(b"tfdt", &full(1, 0, &decode_time.to_be_bytes()));
        let mut trun = words(&[samples, 0]);
        if let Some(flags) = first_flags {
            trun.extend(words(&[flags]));
        }
        let trun = bx(
            b"trun",
            &full(0, 0x1 | if first_flags.is_some() { 0x4 } else { 0 }, &trun),
        );
        bx(b"traf", &[tfhd, tfdt, trun].concat())
    }

    /// What one `moof` carries: video `(decode time, keyframe)` and per audio
    /// track a decode time, `None` for a track with no samples here.
    pub struct Fragment {
        pub video: Option<(u64, bool)>,
        pub audio: Vec<Option<u64>>,
        pub mdat: usize,
    }

    pub fn fragment(sequence: u32, fragment: &Fragment) -> Vec<u8> {
        let mut moof = bx(b"mfhd", &full(0, 0, &words(&[sequence])));
        moof.extend(match fragment.video {
            Some((time, keyframe)) => traf(1, time, 60, keyframe.then_some(0)),
            None => traf(1, 0, 0, None),
        });
        for (track, time) in fragment.audio.iter().enumerate() {
            moof.extend(traf(
                2 + track as u32,
                time.unwrap_or(0),
                time.map_or(0, |_| 21),
                None,
            ));
        }
        let filler: Vec<u8> = (0..fragment.mdat)
            .map(|i| (i * 13 + sequence as usize) as u8)
            .collect();
        [bx(b"moof", &moof), bx(b"mdat", &filler)].concat()
    }

    /// The layout t261017-2334's probe measured on a 120fps, 2s segment: four
    /// GOPs of 0.5s, and the audio for a GOP's time muxed in front of it.
    pub fn probe_layout() -> Vec<Fragment> {
        let video = |seconds_x2: u64| Fragment {
            video: Some((seconds_x2 * VIDEO_TIMESCALE as u64 / 2, true)),
            audio: vec![None],
            mdat: 3_000,
        };
        let audio = |time: u64| Fragment {
            video: None,
            audio: vec![Some(time)],
            mdat: 11_000,
        };
        vec![
            video(0),
            audio(0),
            audio(21_504),
            video(1),
            video(2),
            audio(46_080),
            audio(69_632),
            video(3),
        ]
    }

    /// The body and, per fragment, its `(offset, len)` as laid out here --
    /// worked out while writing, not read back.
    pub fn body(
        audio_tracks: u32,
        fragments: &[Fragment],
    ) -> (Vec<u8>, usize, Vec<(usize, usize)>) {
        let mut out = head(audio_tracks);
        let head_len = out.len();
        let mut spans = Vec::new();
        for (sequence, each) in fragments.iter().enumerate() {
            let bytes = fragment(sequence as u32 + 1, each);
            spans.push((out.len(), bytes.len()));
            out.extend(bytes);
        }
        (out, head_len, spans)
    }
}

/// Every `moof` of a production-shaped body gets a row: where it is, its CRC,
/// and the keyframe / audio time it starts with, in 100ns from the timescales
/// in `moov`. A body that is not an fMP4 has no table, so filler-bodied
/// segments -- most of this file's tests -- keep reading whole.
#[test]
fn a_fragment_table_maps_each_moof_of_a_body() {
    let (segment, head_len, spans) = fmp4::body(1, &fmp4::probe_layout());
    let table = fragment_table(&segment).expect("a production-shaped body has a table");
    assert_eq!(table.head_len as usize, head_len);
    assert_eq!(table.head_crc, crc32(&segment[..head_len]));
    assert_eq!(table.rows.len(), 8);
    for (row, (offset, len)) in table.rows.iter().zip(&spans) {
        assert_eq!((row.offset as usize, row.len as usize), (*offset, *len));
        assert_eq!(row.crc, crc32(&segment[*offset..offset + len]));
    }
    assert_eq!(
        spans.last().map(|(o, l)| o + l),
        Some(segment.len()),
        "rows tile the body"
    );
    let keyframes: Vec<_> = table.rows.iter().map(|row| row.keyframe_100ns).collect();
    assert_eq!(
        keyframes,
        [
            Some(0),
            None,
            None,
            Some(5_000_000),
            Some(10_000_000),
            None,
            None,
            Some(15_000_000)
        ]
    );
    let audio: Vec<_> = table.rows.iter().map(|row| row.audio_100ns).collect();
    // 21504 / 48000 s = 4_480_000 * 100ns, 69632 / 48000 s = 14_506_666.
    assert_eq!(
        audio,
        [
            None,
            Some(0),
            Some(4_480_000),
            None,
            None,
            Some(9_600_000),
            Some(14_506_666),
            None
        ]
    );

    // A video fragment that starts on an inter frame is no place to cut.
    let mut layout = fmp4::probe_layout();
    layout[4].video = Some((90_000, false));
    let table = fragment_table(&fmp4::body(1, &layout).0).unwrap();
    assert_eq!(table.rows[4].keyframe_100ns, None);
    assert_eq!(
        table.rows[3].keyframe_100ns,
        Some(5_000_000),
        "its neighbour still is"
    );

    assert_eq!(fragment_table(&body(0, 512)), None);
    assert_eq!(fragment_table(b"the finalized mp4"), None);
    assert_eq!(fragment_table(&[]), None);
}

fn row(keyframe: Option<i64>, audio: Option<i64>) -> fragments::FragmentRow {
    fragments::FragmentRow {
        offset: 0,
        len: 0,
        crc: 0,
        keyframe_100ns: keyframe,
        audio_100ns: audio,
    }
}

/// The cut rule against the three cuts t261017-2334's probe verified by
/// decoding: the last row whose audio starts by the GOP, never past the GOP's
/// own row. Cutting at the video `moof` instead (row 3, 4, 7) is what dropped
/// the GOP's first 460ms of audio, and in the last GOP left a file MF would not
/// open.
#[test]
fn a_cut_keeps_the_audio_muxed_ahead_of_its_gop() {
    let table = fragment_table(&fmp4::body(1, &fmp4::probe_layout()).0).unwrap();
    // The second element is where the cut reader serves from: the GOP plus
    // the slack for MF's rounding of it.
    let slack = fragments::CUT_SLACK_100NS;
    assert_eq!(table.cut(7_500_000), Some((2, 5_000_000 + slack)));
    assert_eq!(table.cut(5_000_000 + slack), Some((2, 5_000_000 + slack)));
    assert_eq!(
        table.cut(5_000_000),
        None,
        "inside the slack the GOP before serves it, and that is the first"
    );
    assert_eq!(
        table.cut(12_500_000),
        Some((4, 10_000_000 + slack)),
        "audio at 0.96s is in time"
    );
    assert_eq!(table.cut(17_500_000), Some((6, 15_000_000 + slack)));
    assert_eq!(table.cut(19_999_999), Some((6, 15_000_000 + slack)));
    // The first GOP has nothing in front of it to skip.
    assert_eq!(table.cut(0), None);
    assert_eq!(table.cut(4_999_999), None);

    // No audio at all: the GOP's own row.
    let video_only = FragmentTable {
        head_len: 0,
        head_crc: 0,
        rows: vec![row(Some(0), None), row(Some(5_000_000), None)],
    };
    assert_eq!(video_only.cut(6_000_000), Some((1, 5_000_000 + slack)));

    // Two audio tracks, and row 2 carries only one of them (`i64::MAX`): it
    // can never be the cut, or the other track would start late.
    let partial = FragmentTable {
        head_len: 0,
        head_crc: 0,
        rows: vec![
            row(Some(0), None),
            row(None, Some(0)),
            row(None, Some(i64::MAX)),
            row(Some(5_000_000), None),
        ],
    };
    assert_eq!(partial.cut(6_000_000), Some((1, 5_000_000 + slack)));
    // Audio that starts after the GOP everywhere: no cut keeps it.
    let late = FragmentTable {
        head_len: 0,
        head_crc: 0,
        rows: vec![row(Some(0), None), row(Some(5_000_000), Some(6_000_000))],
    };
    assert_eq!(late.cut(7_000_000), None);

    // Why each `None` reads whole (t261018-fc75's `cut_skip`).
    assert_eq!(table.cut_or_skip(0), Err("first_gop"));
    assert_eq!(table.cut_or_skip(5_000_000), Err("first_gop"));
    assert_eq!(late.cut_or_skip(7_000_000), Err("no_audio"));
    // Audio only in the first row: the cut would land on the head.
    let head_audio = FragmentTable {
        head_len: 0,
        head_crc: 0,
        rows: vec![row(Some(0), Some(0)), row(Some(5_000_000), Some(6_000_000))],
    };
    assert_eq!(head_audio.cut_or_skip(7_000_000), Err("no_audio"));
}

/// A cut read hands out exactly the head plus `body[cut..]`, each piece
/// checked against its own CRC: a flipped byte in the head or in a kept
/// fragment fails it, a flipped byte in a fragment it skipped does not -- the
/// partial read promises the bytes it serves, not the record (the record CRC
/// still catches it on a whole read, which is the contrast).
#[test]
fn a_cut_read_serves_the_head_and_the_kept_fragments_verified() {
    let dir = tmp_dir();
    let path = dir.join("cut.lvb");
    let (segment, head_len, spans) = fmp4::body(1, &fmp4::probe_layout());
    let mut writer = ContainerWriter::create(&path, "capture-cut", 15).unwrap();
    writer
        .append_segment(0, 0, 20_000_000, &[0], &body(9, 300))
        .unwrap();
    let appended = writer
        .append_segment(1, 20_000_000, 40_000_000, &[0], &segment)
        .unwrap();
    writer.close().unwrap();
    let located = appended.located;
    let table = appended.fragments.expect("the writer built the table");

    let (bytes, from) = read_located_from(&path, located, &table, 7_500_000)
        .unwrap()
        .expect("a seek into the second GOP is cut");
    assert_eq!(from, 5_000_000 + fragments::CUT_SLACK_100NS);
    let cut = spans[2].0;
    assert_eq!(bytes, [&segment[..head_len], &segment[cut..]].concat());
    assert!(bytes.len() < located.record.len as usize);
    assert_eq!(
        read_located_from(&path, located, &table, 1_000_000).unwrap(),
        None
    );

    let original = std::fs::read(&path).unwrap();
    let flipped = |at_in_body: usize| {
        let damaged = dir.join(format!("flip-{at_in_body}.lvb"));
        let mut bytes = original.clone();
        bytes[located.body.offset as usize + at_in_body] ^= 0x40;
        std::fs::write(&damaged, bytes).unwrap();
        damaged
    };
    let head = flipped(head_len / 2);
    let kept = flipped(spans[5].0 + spans[5].1 / 2);
    let skipped = flipped(spans[1].0 + spans[1].1 / 2);
    for (name, damaged) in [("head", &head), ("kept fragment", &kept)] {
        let error = read_located_from(damaged, located, &table, 7_500_000).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{name}");
    }
    assert_eq!(
        read_located_from(&skipped, located, &table, 7_500_000)
            .unwrap()
            .map(|(bytes, _)| bytes.len()),
        Some(bytes.len()),
        "a fragment the cut skips is not read"
    );
    assert!(
        read_located_at(&skipped, located).is_err(),
        "the flip is real: the whole record no longer verifies"
    );
}

/// The field is the last of `SegmentEntry` and `#[serde(default)]`, so a
/// checkpoint written before it -- one element short per segment -- still
/// decodes, with no table; and one written with it round-trips.
#[test]
fn a_checkpoint_from_before_the_fragment_table_still_reads() {
    #[derive(Serialize)]
    struct BeforeTheTable {
        index: u64,
        start_100ns: i64,
        end_100ns: i64,
        audio_offsets_100ns: Vec<i64>,
        data: Located,
        thumbnail: Option<Located>,
    }
    let data = Located {
        record: Span {
            offset: 4096,
            len: 900,
        },
        body: Span {
            offset: 4136,
            len: 860,
        },
        crc: 7,
    };
    let old = rmp_serde::to_vec(&BeforeTheTable {
        index: 3,
        start_100ns: 0,
        end_100ns: 20_000_000,
        audio_offsets_100ns: vec![0],
        data,
        thumbnail: None,
    })
    .unwrap();
    let read: SegmentEntry = rmp_serde::from_slice(&old).expect("an old entry decodes");
    assert_eq!((read.index, read.data, &read.fragments), (3, data, &None));

    let entry = SegmentEntry {
        fragments: fragment_table(&fmp4::body(1, &fmp4::probe_layout()).0),
        ..read
    };
    assert!(entry.fragments.is_some());
    let again: SegmentEntry = rmp_serde::from_slice(&rmp_serde::to_vec(&entry).unwrap()).unwrap();
    assert_eq!(again, entry);
}

/// A segment the newest checkpoint does not cover gets its table back from
/// the replay, which reads the payload anyway.
#[test]
fn a_scan_without_a_checkpoint_rebuilds_the_fragment_table() {
    let dir = tmp_dir();
    let path = dir.join("replay.lvb");
    let (segment, _, _) = fmp4::body(1, &fmp4::probe_layout());
    let mut writer = ContainerWriter::create(&path, "capture-replay", 15).unwrap();
    let appended = writer
        .append_segment(0, 0, 20_000_000, &[0], &segment)
        .unwrap();
    writer
        .append_segment(1, 20_000_000, 40_000_000, &[0], &body(1, 64))
        .unwrap();
    // No checkpoint and no close: both records are past the checkpoint.
    let scanned = scan(&std::fs::read(&path).unwrap()).unwrap().snapshot;
    drop(writer);
    assert!(appended.fragments.is_some());
    assert_eq!(scanned.segments[0].fragments, appended.fragments);
    assert_eq!(
        scanned.segments[1].fragments, None,
        "a filler body has none"
    );
}

/// t261017-3f94 Step 8: what the table costs a checkpoint, per segment, at
/// the largest numbers a real one carries -- 8 rows (4 GOPs and 4 audio
/// `moof`s in 2s at 120fps), offsets and lengths in the megabytes, every time
/// and CRC needing a full 32-bit MessagePack integer.
///
/// cargo test --lib the_fragment_table_costs_a_checkpoint -- --nocapture
#[test]
fn the_fragment_table_costs_a_checkpoint_at_most_256_bytes_a_segment() {
    let worst = FragmentTable {
        head_len: 1_211,
        head_crc: 0xDEAD_BEEF,
        rows: (0..8u32)
            .map(|at| fragments::FragmentRow {
                offset: 1_211 + at * 3_500_000,
                len: 3_500_000,
                crc: 0xF000_0000 | at,
                keyframe_100ns: (at % 2 == 0).then_some(at as i64 * 2_500_000 + 70_000),
                audio_100ns: (at % 2 == 1).then_some(at as i64 * 2_500_000 + 70_000),
            })
            .collect(),
    };
    let segments = 100u64;
    let without = Snapshot {
        segments: (0..segments)
            .map(|index| SegmentEntry {
                index,
                start_100ns: index as i64 * 20_000_000,
                end_100ns: (index as i64 + 1) * 20_000_000,
                audio_offsets_100ns: vec![0],
                data: Located::default(),
                thumbnail: None,
                fragments: None,
            })
            .collect(),
        ..Snapshot::default()
    };
    let mut with = without.clone();
    for segment in &mut with.segments {
        segment.fragments = Some(worst.clone());
    }
    let before = encode_checkpoint(&without).unwrap().len();
    let after = encode_checkpoint(&with).unwrap().len();
    let per_segment = (after - before) as f64 / segments as f64;
    println!("t261017-3f94 checkpoint {before} -> {after} bytes for {segments} segments: +{per_segment:.1} bytes a segment");
    assert!(per_segment <= 256.0, "{per_segment} bytes a segment");
}

/// A located record whose bytes the file no longer holds (a tail cut under
/// it) is refused as out of range before anything is allocated or read -- by
/// the whole read and by the cut read alike (t261025-00ad: the one
/// `check_span` both go through). `InvalidInput`, not the `UnexpectedEof` a
/// read past the end would give, is what says the bound was checked first.
#[test]
fn a_span_past_the_end_of_the_file_is_refused_as_out_of_range() {
    let dir = tmp_dir();
    let path = dir.join("short.lvb");
    let (segment, _, _) = fmp4::body(1, &fmp4::probe_layout());
    let mut writer = ContainerWriter::create(&path, "capture-short", 15).unwrap();
    let appended = writer
        .append_segment(0, 0, 20_000_000, &[0], &segment)
        .unwrap();
    writer.close().unwrap();
    let located = appended.located;
    let table = appended.fragments.expect("the writer built the table");
    // The contrast: the intact file serves both reads.
    assert!(read_located_at(&path, located).is_ok());
    assert!(read_located_from(&path, located, &table, 7_500_000)
        .unwrap()
        .is_some());

    let short = dir.join("short-cut.lvb");
    let bytes = std::fs::read(&path).unwrap();
    let end = (located.body.offset + located.body.len) as usize;
    std::fs::write(&short, &bytes[..end - 1]).unwrap();
    assert_eq!(
        read_located_at(&short, located).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        read_located_from(&short, located, &table, 7_500_000)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}
