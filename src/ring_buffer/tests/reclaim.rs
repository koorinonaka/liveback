//! Age/capacity reclaim and session-title tests.
use super::*;

const NANOS_PER_DAY: u128 = 24 * 60 * 60 * 1_000_000_000;

fn summary(age_days: u128, now_nanos: u128, total_bytes: u64) -> SessionSummary {
    let started = now_nanos - age_days * NANOS_PER_DAY;
    SessionSummary {
        session_id: format!("capture-{started:032x}"),
        start_100ns: None,
        end_100ns: None,
        segment_count: 1,
        total_bytes,
        closed: true,
        has_partial: false,
        recoverable: false,
        manifest_version: MANIFEST_VERSION,
        readable: true,
        target_title: None,
        thumbnail_index: None,
        note: None,
        protected: false,
        target_executable: None,
        target_executable_path: None,
        marker_count: 0,
    }
}

/// The ids alone, for the cases that are about *which* sessions go rather than
/// why (task700 gave the return value a reason as well).
fn ids(doomed: &[Doomed]) -> Vec<String> {
    doomed
        .iter()
        .map(|entry| entry.session_id.clone())
        .collect()
}

#[test]
fn sessions_to_reclaim_drops_sessions_past_the_lifetime() {
    let now = 1_000 * NANOS_PER_DAY;
    let fresh = summary(1, now, 10);
    let stale = summary(31, now, 10);
    // Capacity is generous: only age can select here.
    let doomed = sessions_to_reclaim(&[fresh.clone(), stale.clone()], 1_000, 30, now, &[]);
    assert_eq!(ids(&doomed), vec![stale.session_id]);
    assert_eq!(doomed[0].reason, ReclaimReason::TooOld);
}

#[test]
fn sessions_to_reclaim_drops_the_oldest_first_when_over_capacity() {
    let now = 1_000 * NANOS_PER_DAY;
    let newest = summary(1, now, 30);
    let middle = summary(2, now, 30);
    let oldest = summary(3, now, 30);
    // 60 bytes holds the two newest; the oldest has to go.
    let doomed = sessions_to_reclaim(
        &[middle.clone(), oldest.clone(), newest.clone()],
        60,
        365,
        now,
        &[],
    );
    assert_eq!(ids(&doomed), vec![oldest.session_id.clone()]);
    assert_eq!(doomed[0].reason, ReclaimReason::OverCapacity);

    // Tighter quota: oldest first in the returned order.
    let doomed = sessions_to_reclaim(&[middle.clone(), oldest.clone(), newest], 30, 365, now, &[]);
    assert_eq!(ids(&doomed), vec![oldest.session_id, middle.session_id]);
}

#[test]
fn sessions_to_reclaim_never_selects_protected_sessions_but_still_counts_their_bytes() {
    let now = 1_000 * NANOS_PER_DAY;
    let newest = summary(1, now, 30);
    let middle = summary(2, now, 30);
    let oldest = summary(3, now, 30);
    // The oldest is protected (loaded/recording): it survives even though
    // it is both the stalest and over quota, and the 30 bytes it holds
    // push `middle` out instead.
    let doomed = sessions_to_reclaim(
        &[newest.clone(), middle.clone(), oldest.clone()],
        60,
        1,
        now,
        std::slice::from_ref(&oldest.session_id),
    );
    assert_eq!(ids(&doomed), vec![middle.session_id]);
}

#[test]
fn sessions_to_reclaim_ages_only_sessions_whose_id_carries_a_timestamp() {
    let now = 1_000 * NANOS_PER_DAY;
    let mut odd = summary(99, now, 10);
    odd.session_id = "capture-recovered".into();
    // Unparseable id: too old to tell, so age can't select it...
    assert!(sessions_to_reclaim(&[odd.clone()], 1_000, 1, now, &[]).is_empty());
    // ...but capacity still can. 1 byte rather than 0: since round8 §3-2 a
    // capacity of 0 means 「なし」, so the smallest budget that is still in
    // force is one byte.
    assert_eq!(
        ids(&sessions_to_reclaim(&[odd.clone()], 1, 1_000, now, &[])),
        vec![odd.session_id]
    );
}

#[test]
fn normalize_target_title_trims_caps_and_treats_blank_as_unset() {
    assert_eq!(
        normalize_target_title("  My Game  "),
        Some("My Game".to_owned())
    );
    assert_eq!(normalize_target_title(""), None);
    assert_eq!(normalize_target_title("   \t "), None);
    // Counted in chars, not bytes, so multi-byte titles aren't cut short.
    let long = "あ".repeat(MAX_TARGET_TITLE_CHARS + 20);
    assert_eq!(
        normalize_target_title(&long).unwrap().chars().count(),
        MAX_TARGET_TITLE_CHARS
    );
}

#[test]
fn set_session_title_persists_to_the_manifest_on_disk() {
    let root = tmp();
    let session = root.join("session-rename");
    let mut ring = RingBuffer::create(session.clone(), "session-rename".into(), 15, None).unwrap();
    fs::write(session.join("0.mp4"), b"x").unwrap();
    ring.add(seg(0)).unwrap();
    drop(ring);

    set_session_title(&root, "session-rename", Some("Renamed".into())).unwrap();
    assert_eq!(
        RingBuffer::open(session.clone())
            .unwrap()
            .manifest()
            .target_title,
        Some("Renamed".to_owned())
    );

    // An unset title clears the field rather than storing an empty string.
    set_session_title(&root, "session-rename", None).unwrap();
    assert_eq!(
        RingBuffer::open(session).unwrap().manifest().target_title,
        None
    );
}

#[test]
fn set_session_title_rejects_traversal_and_unknown_sessions() {
    let root = tmp();
    fs::create_dir_all(&root).unwrap();
    assert_eq!(
        set_session_title(&root, "../escape", Some("x".into()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        set_session_title(&root, "nope", Some("x".into()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

// ---- protection (task167) ----

/// Protection survives both limits, and its bytes still count: the two
/// unprotected sessions are what has to go to fit the budget, and the protected
/// one being over the age limit does not save the young ones.
#[test]
fn a_protected_session_is_never_the_one_reclaimed() {
    let now = 1_000 * NANOS_PER_DAY;
    let mut old_and_protected = summary(90, now, 60);
    old_and_protected.protected = true;
    let recent = summary(1, now, 60);
    let older = summary(2, now, 60);
    let sessions = [old_and_protected.clone(), recent.clone(), older.clone()];

    // Capacity: 100 bytes for three 60-byte sessions. The sweep walks
    // newest-first, so the newest fits and the next one does not; the protected
    // one is passed over wherever in that walk it falls.
    let doomed = ids(&sessions_to_reclaim(&sessions, 100, 3650, now, &[]));
    assert!(!doomed.contains(&old_and_protected.session_id));
    assert!(
        !doomed.contains(&recent.session_id),
        "the newest still fits"
    );
    assert_eq!(doomed, vec![older.session_id.clone()]);

    // Lifetime: 30 days would normally take the 90-day-old one first.
    let doomed = sessions_to_reclaim(&sessions, u64::MAX, 30, now, &[]);
    assert!(
        doomed.is_empty(),
        "the only session past the lifetime is protected"
    );
}

#[test]
fn protection_alone_can_exhaust_a_limit_and_says_so() {
    let now = 1_000 * NANOS_PER_DAY;
    let mut protected = summary(1, now, 200);
    protected.protected = true;
    let unprotected = summary(1, now, 500);

    // Under the limit: 200 bytes of protection inside a 1000-byte budget.
    assert!(!protected_over_limit(
        &[protected.clone(), unprotected.clone()],
        1_000
    ));
    // Over capacity on the protected sessions alone -- the unprotected 500 are
    // not counted, because the sweep can still reclaim those.
    assert!(protected_over_limit(&[protected, unprotected], 150));

    // Age never warns: outliving the lifetime is what protection is for.
    let mut ancient = summary(90, now, 1);
    ancient.protected = true;
    assert!(!protected_over_limit(&[ancient], u64::MAX));
}

#[test]
fn note_and_protection_round_trip_through_the_manifest_and_survive_each_other() {
    let root = std::env::temp_dir().join(format!("livia-note-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let session_id = "capture-00000000000000000000000000000001";
    let mut ring = RingBuffer::create(root.join(session_id), session_id.into(), 15, None).unwrap();
    ring.set_target_title(Some("Shooter Game".into())).unwrap();
    drop(ring);

    set_session_note(&root, session_id, Some("  best round  ".into())).unwrap();
    set_session_protected(&root, session_id, true).unwrap();

    let listed = list_sessions(&root).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].note.as_deref(), Some("best round"), "trimmed");
    assert!(listed[0].protected);
    assert_eq!(
        listed[0].target_title.as_deref(),
        Some("Shooter Game"),
        "editing one field must not drop the others"
    );

    // Emptying the note unsets it rather than storing whitespace.
    set_session_note(&root, session_id, Some("   ".into())).unwrap();
    assert_eq!(list_sessions(&root).unwrap()[0].note, None);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_manifest_written_before_these_fields_existed_still_loads() {
    let root = std::env::temp_dir().join(format!("livia-oldmanifest-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let session_id = "capture-00000000000000000000000000000002";
    let directory = root.join(session_id);
    fs::create_dir_all(&directory).unwrap();
    // Verbatim shape of a pre-task167 manifest: no `note`, no `protected`.
    fs::write(
        directory.join("manifest.json"),
        format!(
            r#"{{"version":{MANIFEST_VERSION},"sessionId":"{session_id}","closed":true,
                 "retentionMinutes":15,"segments":[],"gaps":[]}}"#
        ),
    )
    .unwrap();

    let listed = list_sessions(&root).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].note, None);
    assert!(!listed[0].protected);
    let manifest = RingBuffer::open(directory).unwrap();
    assert_eq!(manifest.manifest().note, None);
    assert!(!manifest.manifest().protected);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_bulk_toggle_attempts_every_session_and_reports_the_first_failure() {
    let root = std::env::temp_dir().join(format!("livia-bulk-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let ids: Vec<String> = (1..=2)
        .map(|n| format!("capture-0000000000000000000000000000000{n}"))
        .collect();
    for id in &ids {
        RingBuffer::create(root.join(id), id.clone(), 15, None).unwrap();
    }
    let mut all = ids.clone();
    all.push("capture-does-not-exist".into());

    let failed = set_sessions_protected(&root, &all, true);
    assert!(failed.is_err(), "the missing session is reported");
    let listed = list_sessions(&root).unwrap();
    assert_eq!(
        listed.iter().filter(|session| session.protected).count(),
        2,
        "one failure must not skip the sessions that could be written"
    );

    set_sessions_protected(&root, &ids, false).unwrap();
    assert!(list_sessions(&root)
        .unwrap()
        .iter()
        .all(|session| !session.protected));
    let _ = fs::remove_dir_all(&root);
}

/// Task700. The two reasons are told apart so only one of them reaches the
/// user: aging out is the retention setting doing what it says, while being
/// pushed out by the capacity budget is a recording disappearing for a reason
/// nobody chose. When both hold, age wins -- that session was going today
/// regardless, and calling it a capacity problem would put a toast in front of
/// a deletion the user configured.
#[test]
fn a_session_past_its_lifetime_is_reported_as_age_even_when_it_is_also_over_capacity() {
    let now = 1_000 * NANOS_PER_DAY;
    let newest = summary(1, now, 30);
    let ancient = summary(400, now, 30);

    // Both limits bite the ancient one: 30 bytes of budget is already spent by
    // `newest`, and it is well past a 30-day lifetime.
    let doomed = sessions_to_reclaim(&[newest.clone(), ancient.clone()], 30, 30, now, &[]);
    assert_eq!(ids(&doomed), vec![ancient.session_id.clone()]);
    assert_eq!(doomed[0].reason, ReclaimReason::TooOld);

    // Same pair, a lifetime nothing can reach: now it is capacity's doing.
    let doomed = sessions_to_reclaim(&[newest, ancient.clone()], 30, 100_000, now, &[]);
    assert_eq!(ids(&doomed), vec![ancient.session_id]);
    assert_eq!(doomed[0].reason, ReclaimReason::OverCapacity);
}

/// round8 §3-2: 0 is 「なし」 -- that limit is switched off, not set to zero.
/// Read literally, a zero lifetime says every recording is already past it and
/// a zero capacity says every byte is over budget: the buffer would empty
/// itself on the next sweep.
#[test]
fn a_limit_of_zero_is_no_limit_at_all() {
    let now = 1_000 * NANOS_PER_DAY;
    let ancient = summary(400, now, 10_000);
    let fresh = summary(1, now, 10_000);
    let sessions = [ancient.clone(), fresh.clone()];

    // 保持期間 = なし: the 400-day-old session stays, and only capacity decides.
    assert!(sessions_to_reclaim(&sessions, 100_000, 0, now, &[]).is_empty());
    assert_eq!(
        ids(&sessions_to_reclaim(&sessions, 15_000, 0, now, &[])),
        vec![ancient.session_id.clone()],
        "capacity is still in force with the lifetime switched off"
    );

    // 保持容量 = なし: age alone decides.
    assert_eq!(
        ids(&sessions_to_reclaim(&sessions, 0, 30, now, &[])),
        vec![ancient.session_id.clone()]
    );

    // Both なし: nothing is ever reclaimed.
    assert!(sessions_to_reclaim(&sessions, 0, 0, now, &[]).is_empty());

    // The protected-over-limit warning follows the same rule.
    let mut protected = ancient.clone();
    protected.protected = true;
    assert!(!protected_over_limit(&[protected.clone()], 0));
    assert!(protected_over_limit(&[protected], 1));
}
