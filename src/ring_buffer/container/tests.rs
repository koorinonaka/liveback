//! `ring_buffer::container` (task401). The format's whole point is surviving
//! a machine that stopped mid-write, so most of these are about damage.

use super::*;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

/// A temp directory that deletes itself when its binding goes out of scope
/// (task3230). Explicit cleanup at the end of a test is not enough: an
/// assertion failure unwinds past it, so exactly the runs worth keeping small
/// -- the failing ones -- were the ones leaving `lvb-*` behind. `Drop` runs on
/// the unwind too.
///
/// This is a deliberate copy of `ring_buffer/tests.rs`'s guard rather than a
/// shared helper: that one is private, and `crates/lvb-thumb` pulls
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
    let path = std::env::temp_dir().join(format!("lvb-{}-{seq}-{id}", std::process::id()));
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
        })
        .unwrap();
    writer
        .append_meta(&MetaEvent::TitleSet {
            title: Some("FINAL FANTASY XIV".into()),
        })
        .unwrap();
    writer.close().unwrap();

    let mut reader = ContainerReader::open(&path).unwrap();
    assert_eq!(reader.header().session_id, "capture-roundtrip");
    assert_eq!(reader.header().retention_minutes, 15);
    assert!(!reader.truncated());
    assert!(
        !reader.needs_recovery(),
        "a closed container is not a candidate"
    );
    assert_eq!(reader.snapshot().segments.len(), 8);
    assert_eq!(
        reader.snapshot().title.as_deref(),
        Some("FINAL FANTASY XIV")
    );
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
    assert!(reader.needs_recovery());
    assert!(reader.truncated());
    let survived = reader.snapshot().segments.len();
    drop(reader);

    let snapshot = recover(&path).unwrap();
    assert!(snapshot.closed);
    assert_eq!(snapshot.segments.len(), survived);

    // And it opens cleanly the second time: no truncation left to report.
    let mut reader = ContainerReader::open(&path).unwrap();
    assert!(!reader.needs_recovery());
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
    assert!(!reader.needs_recovery());
}

#[test]
fn the_dump_reads_both_a_healthy_and_a_broken_container() {
    let dir = tmp_dir();
    let good = dir.join("dump-good.lvb");
    ContainerBuilder::new("capture-dump")
        .segment(0, 0, 20_000_000, &body(0, 64))
        .thumbnail(0, &[0xFF, 0xD8, 0x00])
        .checkpoint()
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 5_000_000,
            label: "note".into(),
            palette_index: None,
        })
        .closed()
        .build(&good)
        .unwrap();

    let text = dump(&std::fs::read(&good).unwrap());
    assert!(text.contains("magic=LVBF"));
    assert!(text.contains("session=capture-dump"));
    assert!(text.contains("truncated=false"));
    assert!(text.contains("closed=true"));
    assert!(text.contains("segment 0"));
    assert!(text.contains("marker 5000000 note"));

    // Broken: cut halfway through, which is the case a dump exists for.
    let broken = dir.join("dump-broken.lvb");
    std::fs::copy(&good, &broken).unwrap();
    let len = std::fs::metadata(&broken).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&broken)
        .unwrap()
        .set_len(HEADER_LEN + (len - HEADER_LEN) / 2)
        .unwrap();
    let text = dump(&std::fs::read(&broken).unwrap());
    assert!(text.contains("truncated=true"), "{text}");
    assert!(
        text.contains("magic=LVBF"),
        "a damaged file still has a header"
    );

    // And a file that is not one of ours at all says so instead of panicking.
    let alien = dir.join("alien.bin");
    std::fs::write(&alien, b"not a container at all").unwrap();
    assert!(dump(&std::fs::read(&alien).unwrap()).contains("header unreadable"));
}

#[test]
fn meta_events_fold_into_the_state_a_session_shows() {
    let mut snapshot = Snapshot::default();
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 30,
        label: "b".into(),
        palette_index: Some(1),
    });
    snapshot.apply(&MetaEvent::MarkerAdded {
        time_100ns: 10,
        label: "a".into(),
        palette_index: Some(0),
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
    });
    assert_eq!(snapshot.markers.len(), 2);
    assert_eq!(snapshot.markers[0].label, "a2");

    snapshot.apply(&MetaEvent::MarkerRenamed {
        time_100ns: 30,
        label: "renamed".into(),
    });
    assert_eq!(snapshot.markers[1].label, "renamed");
    snapshot.apply(&MetaEvent::MarkerRemoved { time_100ns: 10 });
    assert_eq!(snapshot.markers.len(), 1);

    snapshot.apply(&MetaEvent::NoteSet {
        note: Some("later".into()),
    });
    snapshot.apply(&MetaEvent::ProtectedSet { protected: true });
    assert_eq!(snapshot.note.as_deref(), Some("later"));
    assert!(snapshot.protected);
    assert!(!snapshot.closed);
    snapshot.apply(&MetaEvent::Closed);
    assert!(snapshot.closed);
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
            title: Some("FINAL FANTASY XIV".into()),
        })
        .meta(MetaEvent::MarkerAdded {
            time_100ns: 12_000_000,
            label: "good bit".into(),
            palette_index: Some(3),
        })
        .segment(2, 40_000_000, 60_000_000, &body(2, 4096))
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

    println!("healthy={}", healthy.display());
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

#[test]
fn the_audio_track_list_folds_like_every_other_setter() {
    let mut snapshot = Snapshot::default();
    assert!(snapshot.audio_tracks.is_empty(), "single track by default");
    snapshot.apply(&MetaEvent::AudioTracksSet {
        tracks: vec![
            AudioTrackInfo {
                executable_name: "ffxiv_dx11.exe".into(),
            },
            AudioTrackInfo {
                executable_name: "Discord.exe".into(),
            },
        ],
    });
    assert_eq!(snapshot.audio_tracks.len(), 2);
    assert_eq!(snapshot.audio_tracks[0].executable_name, "ffxiv_dx11.exe");
    // Last one wins, as with the title and the note.
    snapshot.apply(&MetaEvent::AudioTracksSet {
        tracks: vec![AudioTrackInfo {
            executable_name: "ffxiv_dx11.exe".into(),
        }],
    });
    assert_eq!(snapshot.audio_tracks.len(), 1);
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
        .meta(MetaEvent::NoteSet {
            note: Some("throwaway".into()),
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
