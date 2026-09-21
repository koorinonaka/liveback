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
}

#[test]
fn markers_round_trip_over_the_camel_case_wire_format() {
    // The frontend reads `time100ns`; a snake_case field here would arrive
    // as undefined and be silently dropped when positioning markers.
    let marker = MarkerRecord {
        time_100ns: 42,
        label: "boss".into(),
        color_index: Some(3),
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
        MarkerRecord {
            time_100ns: 7,
            label: String::new(),
            color_index: None,
        }
    );
}

/// Round5 §4-B: the colour follows the order markers were *made* in, not the
/// order they end up sorted into, and it is written to the manifest so a
/// reload draws the same bars in the same colours.
#[test]
fn merge_markers_assigns_palette_colours_in_creation_order() {
    let root = tmp();
    let mut ring = RingBuffer::create(root.to_path_buf(), "s".into(), 15, None).unwrap();
    // Queued newest-first: the sort below puts 10 in front, but 30 was made
    // first and keeps colour 0.
    ring.merge_markers(vec![
        MarkerRecord {
            time_100ns: 30,
            label: String::new(),
            color_index: None,
        },
        MarkerRecord {
            time_100ns: 10,
            label: String::new(),
            color_index: None,
        },
    ])
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
        ring.merge_markers(vec![MarkerRecord {
            time_100ns: time,
            label: String::new(),
            color_index: None,
        }])
        .unwrap();
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

    // And it is on disk, not merely in memory.
    let reopened = RingBuffer::open(root.to_path_buf()).unwrap();
    assert_eq!(colours(&reopened), colours(&ring));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn merge_markers_dedups_by_time_keeps_labels_and_sorts() {
    let root = tmp();
    let mut ring = RingBuffer::create(root.to_path_buf(), "s".into(), 15, None).unwrap();
    ring.merge_markers(vec![
        MarkerRecord {
            time_100ns: 30,
            label: String::new(),
            color_index: None,
        },
        MarkerRecord {
            time_100ns: 10,
            label: String::new(),
            color_index: None,
        },
    ])
    .unwrap();
    ring.update_marker(10, "named".into()).unwrap();
    // Re-queuing an instant already stored must neither duplicate it nor
    // wipe the label an edit already set.
    ring.merge_markers(vec![
        MarkerRecord {
            time_100ns: 10,
            label: String::new(),
            color_index: None,
        },
        MarkerRecord {
            time_100ns: 20,
            label: String::new(),
            color_index: None,
        },
    ])
    .unwrap();
    assert_eq!(
        ring.manifest()
            .markers
            .iter()
            .map(|m| (m.time_100ns, m.label.as_str()))
            .collect::<Vec<_>>(),
        vec![(10, "named"), (20, ""), (30, "")]
    );
    // Survives a reload: the merge persisted rather than only mutating.
    let reopened = RingBuffer::open(root.to_path_buf()).unwrap();
    assert_eq!(reopened.manifest().markers.len(), 3);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn update_and_delete_marker_reject_an_unknown_time() {
    let root = tmp();
    let mut ring = RingBuffer::create(root.to_path_buf(), "s".into(), 15, None).unwrap();
    ring.merge_markers(vec![MarkerRecord {
        time_100ns: 5,
        label: String::new(),
        color_index: None,
    }])
    .unwrap();
    assert!(ring.update_marker(999, "x".into()).is_err());
    assert!(ring.delete_marker(999).is_err());
    assert!(ring.delete_marker(5).is_ok());
    assert!(ring.manifest().markers.is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn prune_drops_markers_whose_video_is_gone() {
    let root = tmp();
    let mut ring = RingBuffer::create(root.to_path_buf(), "s".into(), 15, None).unwrap();
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
        },
        MarkerRecord {
            time_100ns: recent,
            label: "kept".into(),
            color_index: None,
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

#[test]
fn manifest_without_target_title_field_deserializes_to_none() {
    let json = r#"{
            "version": 2,
            "sessionId": "x",
            "closed": true,
            "retentionMinutes": 15,
            "segments": [],
            "gaps": []
        }"#;
    let manifest: SessionManifest = serde_json::from_str(json).unwrap();
    assert_eq!(manifest.target_title, None);
}
#[test]
fn segment_without_thumbnail_path_field_deserializes_to_none() {
    let json = r#"{
            "version": 2,
            "sessionId": "x",
            "closed": true,
            "retentionMinutes": 15,
            "segments": [{
                "index": 0,
                "path": "0.mp4",
                "start100ns": 0,
                "end100ns": 20000000,
                "bytes": 1
            }],
            "gaps": []
        }"#;
    let manifest: SessionManifest = serde_json::from_str(json).unwrap();
    assert_eq!(manifest.segments[0].thumbnail_path, None);
}
