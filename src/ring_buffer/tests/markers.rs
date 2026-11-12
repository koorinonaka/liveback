//! Manifest serde and marker round-trip tests.
use super::*;

#[test]
fn manifest_written_before_markers_existed_still_loads() {
    let json = r#"{
            "version": 2,
            "sessionId": "x",
            "closed": true,
            "retentionMinutes": 15,
            "segments": [],
            "gaps": []
        }"#;
    let manifest: SessionManifest = serde_json::from_str(json).unwrap();
    assert!(manifest.markers.is_empty());
    // Nor did the target title exist yet.
    assert_eq!(manifest.target_title, None);
}

#[test]
fn markers_round_trip_over_the_camel_case_wire_format() {
    // The frontend reads `time100ns`; a snake_case field here would arrive
    // as undefined and be silently dropped when positioning markers.
    let marker = MarkerRecord {
        time_100ns: 42,
        label: "boss".into(),
        color_index: Some(3),
        comment: String::new(),
    };
    let json = serde_json::to_string(&marker).unwrap();
    assert!(
        json.contains("\"time100ns\""),
        "unexpected wire format: {json}"
    );
    assert_eq!(serde_json::from_str::<MarkerRecord>(&json).unwrap(), marker);
    // A marker persisted before labels or colours were written keeps
    // deserializing; the colour it never had stays absent (task171), which is
    // what tells the review panel to derive one from its position.
    assert_eq!(
        serde_json::from_str::<MarkerRecord>(r#"{"time100ns":7}"#).unwrap(),
        MarkerRecord::at(7)
    );
}

/// t261009-0251: the comment round-trips, is left off the wire when empty, and
/// a marker written before it existed reads as having none.
#[test]
fn a_marker_comment_round_trips_and_defaults_to_none() {
    let marker = MarkerRecord {
        time_100ns: 42,
        label: "boss".into(),
        color_index: Some(1),
        comment: "slow here\n「まさか」".into(),
    };
    let json = serde_json::to_string(&marker).unwrap();
    assert!(json.contains("\"comment\""), "{json}");
    assert_eq!(serde_json::from_str::<MarkerRecord>(&json).unwrap(), marker);
    let bare = MarkerRecord {
        comment: String::new(),
        ..marker
    };
    let json = serde_json::to_string(&bare).unwrap();
    assert!(!json.contains("comment"), "{json}");
    assert_eq!(
        serde_json::from_str::<MarkerRecord>(r#"{"time100ns":42,"label":"boss","colorIndex":1}"#)
            .unwrap(),
        bare
    );
}

#[test]
fn normalize_comment_trims_unifies_line_breaks_and_caps() {
    assert_eq!(normalize_comment("  \r\n \t "), None);
    assert_eq!(normalize_comment(""), None);
    assert_eq!(
        normalize_comment("  a\r\n\r\nb \n").as_deref(),
        Some("a\n\nb")
    );
    let long = "あ".repeat(MAX_COMMENT_CHARS + 1);
    assert_eq!(
        normalize_comment(&long).unwrap().chars().count(),
        MAX_COMMENT_CHARS
    );
    let exact = "あ".repeat(MAX_COMMENT_CHARS);
    assert_eq!(normalize_comment(&exact).as_deref(), Some(exact.as_str()));
}

/// Round5 §4-B: the colour follows the order markers were *made* in, not the
/// order they end up sorted into, and it travels with the marker so a reload
/// draws the same bars in the same colours.
#[test]
fn merge_markers_assigns_palette_colours_in_creation_order() {
    let root = tmp();
    let mut ring = ring_at(&root, "s", 15);
    // Queued newest-first: the sort below puts 10 in front, but 30 was made
    // first and keeps colour 0.
    ring.merge_markers(vec![MarkerRecord::at(30), MarkerRecord::at(10)])
        .unwrap();
    let colours = |ring: &RingBuffer| {
        ring.manifest()
            .markers
            .iter()
            .map(|marker| (marker.time_100ns, marker.color_index))
            .collect::<Vec<_>>()
    };
    assert_eq!(colours(&ring), vec![(10, Some(1)), (30, Some(0))]);

    // Six more wrap the palette rather than running off its end.
    for time in [40, 50, 60, 70, 80, 90] {
        ring.merge_markers(vec![MarkerRecord::at(time)]).unwrap();
    }
    assert_eq!(
        colours(&ring),
        vec![
            (10, Some(1)),
            (30, Some(0)),
            (40, Some(2)),
            (50, Some(3)),
            (60, Some(4)),
            (70, Some(5)),
            (80, Some(0)),
            (90, Some(1)),
        ]
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn merge_markers_dedups_by_time_keeps_labels_and_sorts() {
    let root = tmp();
    let mut ring = stored_ring_at(&root, "s");
    ring.merge_markers(vec![MarkerRecord::at(30), MarkerRecord::at(10)])
        .unwrap();
    ring.update_marker(10, "named".into(), None).unwrap();
    // Re-queuing an instant already stored must neither duplicate it nor
    // wipe the label an edit already set.
    ring.merge_markers(vec![MarkerRecord::at(10), MarkerRecord::at(20)])
        .unwrap();
    assert_eq!(
        ring.manifest()
            .markers
            .iter()
            .map(|m| (m.time_100ns, m.label.as_str()))
            .collect::<Vec<_>>(),
        vec![(10, "named"), (20, ""), (30, "")]
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn update_and_delete_marker_reject_an_unknown_time() {
    let root = tmp();
    let mut ring = stored_ring_at(&root, "s");
    ring.merge_markers(vec![MarkerRecord::at(5)]).unwrap();
    assert!(ring.update_marker(999, "x".into(), None).is_err());
    assert!(ring.delete_marker(999).is_err());
    assert!(ring.delete_marker(5).is_ok());
    assert!(ring.manifest().markers.is_empty());
    let _ = fs::remove_dir_all(root);
}

/// t261102-24df: the on-disk arm refuses an unknown time the same way, and
/// appends nothing to the container while doing so.
#[test]
fn update_and_delete_marker_on_disk_reject_an_unknown_time() {
    let root = tmp();
    let id = "s";
    let path = root.join(id).with_extension(CONTAINER_EXTENSION);
    crate::ring_buffer::container::ContainerBuilder::new(id)
        .segment(0, 0, 20_000_000, b"abcd")
        .checkpoint()
        .closed()
        .build(&path)
        .unwrap();
    add_marker(&root, id, 5).unwrap();
    let len = fs::metadata(&path).unwrap().len();

    let errors = [
        update_marker(&root, id, 999, "x".into(), None).unwrap_err(),
        delete_marker(&root, id, 999).unwrap_err(),
    ];
    for error in errors {
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.to_string(), "no such marker");
    }
    assert_eq!(fs::metadata(&path).unwrap().len(), len, "nothing appended");

    delete_marker(&root, id, 5).unwrap();
    assert!(
        fs::metadata(&path).unwrap().len() > len,
        "the real delete lands"
    );
}

/// t261101-6b34: the in-memory prune and the replayed `Pruned` record keep the
/// same markers. The first kept segment can start before the retention cutoff
/// (it is kept because it *ends* after it); a marker in that stretch used to be
/// dropped in memory, kept on disk, and back after a restart.
#[test]
fn prune_keeps_the_same_markers_in_memory_as_on_disk() {
    const SEC: i64 = 10_000_000;
    let root = tmp();
    // Retention 15 min. Segment 1 ends at 903 s, so the cutoff is 3 s: segment 0
    // (ends at 2 s) goes, segment 1 (starts at 2 s) stays.
    let mut ring = ring_at(&root, "s", 15);
    let segments = [(0u64, 0, 2 * SEC), (1, 2 * SEC, 903 * SEC)];
    for (index, start_100ns, end_100ns) in segments {
        ring.add(SegmentRecord {
            index,
            path: format!("{index}.mp4").into(),
            start_100ns,
            end_100ns,
            bytes: 1,
            thumbnail_path: None,
            audio_offsets_100ns: vec![0],
        })
        .unwrap();
    }
    let marker_times = [SEC, 2 * SEC + SEC / 2, 10 * SEC];
    ring.merge_markers(
        marker_times
            .iter()
            .map(|&time_100ns| MarkerRecord::at(time_100ns))
            .collect(),
    )
    .unwrap();
    let keep_from_index = ring.prune().unwrap().expect("segment 0 was dropped");

    fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("disk.{CONTAINER_EXTENSION}"));
    let mut builder = container::ContainerBuilder::new("disk");
    for (index, start_100ns, end_100ns) in segments {
        builder = builder.segment(index, start_100ns, end_100ns, &[0xC0; 64]);
    }
    for time_100ns in marker_times {
        builder = builder.meta(container::MetaEvent::MarkerAdded {
            time_100ns,
            label: String::new(),
            palette_index: None,
            comment: None,
        });
    }
    builder
        .meta(container::MetaEvent::Pruned { keep_from_index })
        .build(&path)
        .unwrap();
    let reader = container::ContainerReader::open(&path).unwrap();
    let on_disk: Vec<i64> = reader
        .snapshot()
        .markers
        .iter()
        .map(|marker| marker.time_100ns)
        .collect();
    let in_memory: Vec<i64> = ring
        .manifest()
        .markers
        .iter()
        .map(|marker| marker.time_100ns)
        .collect();
    assert_eq!(on_disk, vec![2 * SEC + SEC / 2, 10 * SEC]);
    assert_eq!(in_memory, on_disk);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn prune_drops_markers_whose_video_is_gone() {
    let root = tmp();
    let mut ring = ring_at(&root, "s", 15);
    // Retention cutoff lands 15 minutes before the newest segment's end.
    let recent = 100i64 * 60 * 10_000_000;
    ring.add(SegmentRecord {
        index: 0,
        path: "0.mp4".into(),
        start_100ns: recent,
        end_100ns: recent + 20_000_000,
        bytes: 1,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    ring.merge_markers(vec![
        MarkerRecord {
            time_100ns: 0,
            label: "stale".into(),
            color_index: None,
            comment: String::new(),
        },
        MarkerRecord {
            time_100ns: recent,
            label: "kept".into(),
            color_index: None,
            comment: String::new(),
        },
    ])
    .unwrap();
    ring.prune().unwrap();
    assert_eq!(
        ring.manifest()
            .markers
            .iter()
            .map(|m| m.label.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    let _ = fs::remove_dir_all(root);
}
