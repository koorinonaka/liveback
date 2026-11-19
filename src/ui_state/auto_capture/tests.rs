use super::*;
use crate::test_support::strings;

/// One window of one process. `executable_id` is lowercased by
/// `list_capture_targets`, so the fixtures are too.
fn window(id: &str, executable: &str, selectable: bool) -> CaptureTarget {
    CaptureTarget {
        executable_id: Some(executable.to_ascii_lowercase()),
        executable_name: Some(executable.to_owned()),
        selectable,
        ..crate::capture::tests::test_window(id)
    }
}

/// The stored entries the registration functions work on (task2560).
/// Metadata-less, like everything registered before that task.
fn apps(names: &[&str]) -> Vec<AutoCaptureApp> {
    names
        .iter()
        .map(|name| AutoCaptureApp::named(*name))
        .collect()
}

/// The processes behind a window list. Every window has a live process, so
/// this is the everyday coupling the picker hands in.
fn alive(targets: &[CaptureTarget]) -> HashSet<String> {
    targets
        .iter()
        .filter_map(|target| target.executable_id.clone())
        .collect()
}

/// A guard with no forbidden folders, for the tests that are not about it.
fn open_guard() -> FolderGuard {
    FolderGuard::default()
}

/// The real machine's shape, spelled out so the guard tests do not depend
/// on the machine they run on.
fn windows_guard() -> FolderGuard {
    FolderGuard {
        forbidden_trees: vec![PathBuf::from(r"C:\Windows")],
        forbidden_exact: vec![
            PathBuf::from(r"C:\Program Files"),
            PathBuf::from(r"C:\Program Files (x86)"),
        ],
    }
}

/// `poll` for the executable-only cases: no pids, no path resolution, an
/// open guard. Every existing task1970/2450/2550 case goes through here.
fn poll_names(
    watch: &mut AutoCaptureWatch,
    registered: &[String],
    running: &HashSet<String>,
    targets: &[CaptureTarget],
    capturing: bool,
) -> AutoCapturePlan {
    let pids = HashMap::new();
    let guard = open_guard();
    let inputs = AutoCaptureInputs {
        running,
        pids: &pids,
        resolve_path: &|_| None,
        guard: &guard,
    };
    watch.poll(registered, &[], &inputs, targets, capturing)
}

/// `poll` under that coupling -- for the cases that are not about the
/// difference between "no window listed" and "no process". The ones that
/// are call `poll_names` directly with a `running` set of their own.
fn poll_live(
    watch: &mut AutoCaptureWatch,
    registered: &[String],
    targets: &[CaptureTarget],
    capturing: bool,
) -> AutoCapturePlan {
    poll_names(watch, registered, &alive(targets), targets, capturing)
}

/// One process of a folder-rule fixture: a window, the pid behind it, and
/// the image path that pid resolves to.
struct Proc {
    window: CaptureTarget,
    path: PathBuf,
}

/// A window whose process id is its own, so several processes in one
/// fixture resolve to different paths (`window` hardcodes pid 1).
fn proc(id: &str, process_id: u32, path: &str, selectable: bool) -> Proc {
    let name = Path::new(path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut window = window(id, &name, selectable);
    window.process_id = process_id;
    Proc {
        window,
        path: PathBuf::from(path),
    }
}

/// The folder-rule poll: builds the name set, the name -> pid map and the
/// pid -> path resolver the way the picker's Toolhelp32 walk does.
fn poll_folders(
    watch: &mut AutoCaptureWatch,
    folders: &[AutoCaptureFolder],
    processes: &[&Proc],
    capturing: bool,
) -> AutoCapturePlan {
    poll_mixed(watch, &[], folders, processes, capturing, &open_guard())
}

/// The same with an executable list and an explicit guard, for the cases
/// about either.
fn poll_mixed(
    watch: &mut AutoCaptureWatch,
    registered: &[String],
    folders: &[AutoCaptureFolder],
    processes: &[&Proc],
    capturing: bool,
    guard: &FolderGuard,
) -> AutoCapturePlan {
    let mut running = HashSet::new();
    let mut pids: HashMap<String, Vec<u32>> = HashMap::new();
    let mut paths: HashMap<u32, PathBuf> = HashMap::new();
    let mut targets = Vec::new();
    for process in processes {
        let name = process.window.executable_id.clone().unwrap();
        running.insert(name.clone());
        pids.entry(name)
            .or_default()
            .push(process.window.process_id);
        paths.insert(process.window.process_id, process.path.clone());
        targets.push(process.window.clone());
    }
    let resolve = move |process_id: u32| paths.get(&process_id).cloned();
    let inputs = AutoCaptureInputs {
        running: &running,
        pids: &pids,
        resolve_path: &resolve,
        guard,
    };
    watch.poll(registered, folders, &inputs, &targets, capturing)
}

/// The whole point: the app was not there, now it is, so record it.
#[test]
fn a_registered_app_appearing_starts_its_window() {
    let list = strings(&["game.exe"]);
    let mut watch = AutoCaptureWatch::default();
    // Seed poll: only a text editor is up.
    let plan = poll_live(
        &mut watch,
        &list,
        &[window("1", "notepad.exe", true)],
        false,
    );
    assert_eq!(plan, AutoCapturePlan::default());

    let plan = poll_live(&mut watch, &list, &[window("2", "game.exe", true)], false);
    assert_eq!(plan.armed.as_deref(), Some("game.exe"));
    assert_eq!(plan.start.as_deref(), Some("2"));
}

/// An app that was already running when Liveback started has no edge. This
/// is why launching Liveback does not fire.
#[test]
fn an_app_already_running_at_the_first_poll_never_fires() {
    let list = strings(&["game.exe"]);
    let targets = [window("2", "game.exe", true)];
    let mut watch = AutoCaptureWatch::default();
    assert_eq!(
        poll_live(&mut watch, &list, &targets, false),
        AutoCapturePlan::default()
    );
    // Nor on any poll after it: the executable never left, so it never
    // arrives.
    assert_eq!(
        poll_live(&mut watch, &list, &targets, false),
        AutoCapturePlan::default()
    );
    assert_eq!(
        poll_live(&mut watch, &list, &targets, false),
        AutoCapturePlan::default()
    );
}

/// Once the recording is actually running the arming is spent, and simply
/// staying open is not another edge.
#[test]
fn a_running_recording_ends_the_arming() {
    let list = strings(&["game.exe"]);
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    let targets = [window("2", "game.exe", true)];
    assert!(poll_live(&mut watch, &list, &targets, false)
        .start
        .is_some());
    // The capture came up.
    let plan = poll_live(&mut watch, &list, &targets, true);
    assert_eq!(plan.start, None);
    assert_eq!(plan.skipped, vec!["game.exe".to_owned()]);
    assert_eq!(poll_live(&mut watch, &list, &targets, true).start, None);
}

/// The criterion that rules out level-triggering: stopping by hand has to
/// stay stopped while the game is still up.
#[test]
fn stopping_by_hand_does_not_restart_while_the_app_stays_open() {
    let list = strings(&["game.exe"]);
    let targets = [window("2", "game.exe", true)];
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    assert!(poll_live(&mut watch, &list, &targets, false)
        .start
        .is_some());
    poll_live(&mut watch, &list, &targets, true);
    // Stopped by hand; the game is still running.
    for _ in 0..3 {
        assert_eq!(poll_live(&mut watch, &list, &targets, false).start, None);
    }
    // Closing and relaunching is the next edge.
    poll_live(&mut watch, &list, &[], false);
    assert!(poll_live(&mut watch, &list, &targets, false)
        .start
        .is_some());
}

/// A start that `spawn_start_capture` refused leaves nothing recording, so
/// the next poll has to ask again rather than lose the launch -- but not
/// forever. A refusal that is permanent (the disk guard turning every start
/// down) has to stop being re-asked, or it is a failure banner and a log
/// line every second for as long as the app is open.
#[test]
fn a_start_that_never_takes_is_given_up_on() {
    let list = strings(&["game.exe"]);
    let targets = [window("2", "game.exe", true)];
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    for attempt in 1..MAX_START_ATTEMPTS {
        let plan = poll_live(&mut watch, &list, &targets, false);
        assert_eq!(plan.start.as_deref(), Some("2"));
        assert_eq!(plan.gave_up, None);
        if attempt > 1 {
            // A retry, not a fresh arming.
            assert_eq!(plan.armed, None);
        }
    }
    // The last one still goes out -- giving up cannot cancel a start that
    // is already on its way.
    let plan = poll_live(&mut watch, &list, &targets, false);
    assert_eq!(plan.start.as_deref(), Some("2"));
    assert_eq!(plan.gave_up.as_deref(), Some("game.exe"));
    // And then it is over: the app is still open, so there is no new edge.
    for _ in 0..3 {
        assert_eq!(
            poll_live(&mut watch, &list, &targets, false),
            AutoCapturePlan::default()
        );
    }
}

/// Giving up is per launch, not per install: closing the app and starting
/// it again is a fresh edge and a fresh budget.
#[test]
fn relaunching_after_a_give_up_arms_again_with_a_full_budget() {
    let list = strings(&["game.exe"]);
    let targets = [window("2", "game.exe", true)];
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    for _ in 0..MAX_START_ATTEMPTS {
        poll_live(&mut watch, &list, &targets, false);
    }
    poll_live(&mut watch, &list, &[], false);
    // The re-arming poll asks for its start too, so it is attempt 1.
    assert!(poll_live(&mut watch, &list, &targets, false)
        .armed
        .is_some());
    for _ in 2..MAX_START_ATTEMPTS {
        assert_eq!(poll_live(&mut watch, &list, &targets, false).gave_up, None);
    }
    assert!(poll_live(&mut watch, &list, &targets, false)
        .gave_up
        .is_some());
}

/// The splash-screen case: the process is up before anything capturable
/// exists. Arm on the executable, wait for the window, invent no delay.
#[test]
fn arming_waits_for_a_selectable_window() {
    let list = strings(&["game.exe"]);
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    let plan = poll_live(&mut watch, &list, &[window("2", "game.exe", false)], false);
    assert_eq!(plan.armed.as_deref(), Some("game.exe"));
    assert_eq!(plan.start, None);

    let plan = poll_live(&mut watch, &list, &[window("3", "game.exe", true)], false);
    assert_eq!(plan.start.as_deref(), Some("3"));
}

/// A slow splash must not spend the retry budget: nothing has been asked
/// for yet, so there is nothing to have refused.
#[test]
fn waiting_for_a_window_does_not_spend_the_retry_budget() {
    let list = strings(&["game.exe"]);
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    let splash = [window("2", "game.exe", false)];
    for _ in 0..(MAX_START_ATTEMPTS * 3) {
        assert_eq!(poll_live(&mut watch, &list, &splash, false).start, None);
    }
    let plan = poll_live(&mut watch, &list, &[window("3", "game.exe", true)], false);
    assert_eq!(plan.start.as_deref(), Some("3"));
    assert_eq!(plan.gave_up, None);
}

/// A launch that dies during startup must not leave the watch armed for
/// the next unrelated window. The death is the *process* going, which is
/// the half of this the window list cannot tell (task2450).
#[test]
fn an_app_that_disappears_before_it_is_capturable_disarms() {
    let list = strings(&["game.exe"]);
    let splash = [window("2", "game.exe", false)];
    let running = alive(&splash);
    let mut watch = AutoCaptureWatch::default();
    poll_names(&mut watch, &list, &HashSet::new(), &[], false);
    assert!(poll_names(&mut watch, &list, &running, &splash, false)
        .armed
        .is_some());
    // The process is gone, so nothing capturable is ever coming.
    assert_eq!(
        poll_names(&mut watch, &list, &HashSet::new(), &[], false),
        AutoCapturePlan::default()
    );
    // ...and it really was dropped: the app coming back arms afresh rather
    // than firing off the old arming.
    assert!(poll_names(&mut watch, &list, &running, &splash, false)
        .armed
        .is_some());
}

/// The other half: a window list that empties is *not* the process dying
/// (task2450). A minimized Store window leaves the picker's enumeration
/// with its app still up, and the arming has to survive that.
#[test]
fn an_empty_window_list_does_not_disarm_a_live_process() {
    let list = strings(&["game.exe"]);
    let splash = [window("2", "game.exe", false)];
    let ready = [window("3", "game.exe", true)];
    let running = alive(&splash);
    let mut watch = AutoCaptureWatch::default();
    poll_names(&mut watch, &list, &HashSet::new(), &[], false);
    assert!(poll_names(&mut watch, &list, &running, &splash, false)
        .armed
        .is_some());
    assert_eq!(
        poll_names(&mut watch, &list, &running, &[], false),
        AutoCapturePlan::default()
    );
    // Same arming, not a new one: `armed` is None and the start still goes.
    let plan = poll_names(&mut watch, &list, &running, &ready, false);
    assert_eq!(plan.armed, None);
    assert_eq!(plan.start.as_deref(), Some("3"));
}

/// Hand-edited `settings.json`, Windows filenames.
#[test]
fn the_executable_name_is_matched_case_insensitively() {
    let list = strings(&["  GAME.EXE  "]);
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    let plan = poll_live(&mut watch, &list, &[window("2", "Game.exe", true)], false);
    assert_eq!(plan.start.as_deref(), Some("2"));
}

/// Recording is single, so two launches in one second are decided by the
/// registration order and the loser is logged rather than queued.
#[test]
fn two_apps_in_one_poll_start_the_first_registered_one() {
    let list = strings(&["first.exe", "second.exe"]);
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    let plan = poll_live(
        &mut watch,
        &list,
        // Listed in the other order, to prove the registration order is
        // what decides rather than the window enumeration order.
        &[
            window("2", "second.exe", true),
            window("3", "first.exe", true),
        ],
        false,
    );
    assert_eq!(plan.armed.as_deref(), Some("first.exe"));
    assert_eq!(plan.skipped, vec!["second.exe".to_owned()]);
    assert_eq!(plan.start.as_deref(), Some("3"));
}

/// An app launching while something else is being recorded is passed over
/// for good, not queued behind it.
#[test]
fn a_launch_during_another_recording_is_skipped_and_not_queued() {
    let list = strings(&["game.exe"]);
    let targets = [window("2", "game.exe", true)];
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], true);
    let plan = poll_live(&mut watch, &list, &targets, true);
    assert_eq!(plan.armed, None);
    assert_eq!(plan.skipped, vec!["game.exe".to_owned()]);
    // The other recording ends; the missed launch stays missed.
    assert_eq!(poll_live(&mut watch, &list, &targets, false).start, None);
}

/// The bug this edge source exists for (task2450). Minimizing a Store app
/// drops its window from the picker's enumeration -- its client rect goes
/// 0x0 -- while the process keeps running. Restoring it must not read as a
/// launch, which is what started a recording on the real machine.
#[test]
fn a_window_leaving_the_list_while_its_process_lives_is_not_a_launch() {
    let list = strings(&["mspaint.exe"]);
    let targets = [window("2", "mspaint.exe", true)];
    // Held fixed across all three polls: the app never exits.
    let running = alive(&targets);
    let mut watch = AutoCaptureWatch::default();
    poll_names(&mut watch, &list, &running, &targets, false);
    // Minimized: gone from the list, still very much running.
    assert_eq!(
        poll_names(&mut watch, &list, &running, &[], false),
        AutoCapturePlan::default()
    );
    // Restored.
    assert_eq!(
        poll_names(&mut watch, &list, &running, &targets, false),
        AutoCapturePlan::default()
    );
}

/// Why `running` is every process rather than only the registered ones:
/// a filtered set would leave a name just added out of the previous poll's
/// set, and registering an app that is already up would fire on the spot.
#[test]
fn registering_an_app_that_is_already_running_does_not_fire() {
    let targets = [
        window("2", "game.exe", true),
        window("3", "other.exe", true),
    ];
    let running = alive(&targets);
    let mut watch = AutoCaptureWatch::default();
    // Another app is registered, so the watch is seeded and past its
    // empty-list early return -- nothing to hide behind.
    let list = strings(&["other.exe"]);
    poll_names(&mut watch, &list, &running, &targets, false);
    poll_names(&mut watch, &list, &running, &targets, false);
    // `game.exe` joins the list while it is already up.
    let list = strings(&["other.exe", "game.exe"]);
    for _ in 0..3 {
        assert_eq!(
            poll_names(&mut watch, &list, &running, &targets, false),
            AutoCapturePlan::default()
        );
    }
}

/// Registering carries whatever metadata the caller resolved (task2560),
/// and unregistering ignores it -- the name alone decides both ways.
#[test]
fn a_registration_keeps_its_display_metadata() {
    let entry = AutoCaptureApp {
        display_name: Some("ペイント".to_owned()),
        name: "mspaint.exe".to_owned(),
        path: Some(r"C:\Windows\System32\mspaint.exe".to_owned()),
    };
    let list = toggle(&[], entry.clone());
    assert_eq!(list, vec![entry.clone()]);
    assert!(is_registered(&list, "mspaint.exe"));
    // The second toggle needs no metadata of its own to remove.
    assert!(toggle(&list, AutoCaptureApp::named("MSPAINT.EXE")).is_empty());
    assert!(unregister(&list, "mspaint.exe").is_empty());
}

/// Windows filenames fold case, so `Mspaint.exe` is not a second entry --
/// its tile has to read as registered and its click has to remove.
#[test]
fn a_registration_is_case_insensitive_in_both_directions() {
    let list = apps(&["Mspaint.exe"]);
    assert!(is_registered(&list, "mspaint.exe"));
    assert!(is_registered(&list, "MSPAINT.EXE"));
    assert!(toggle(&list, AutoCaptureApp::named("mspaint.exe")).is_empty());
    // And the same app cannot be added twice under another spelling.
    assert_eq!(
        toggle(
            &toggle(&[], AutoCaptureApp::named("game.exe")),
            AutoCaptureApp::named("GAME.EXE")
        ),
        Vec::<AutoCaptureApp>::new()
    );
}

/// A hand-edited file can hold both spellings; the settings row's × has to
/// clear the app, not one of its two lines.
#[test]
fn unregistering_drops_every_spelling_of_the_same_name() {
    let list = apps(&["Game.exe", "other.exe", "  GAME.EXE  "]);
    assert_eq!(unregister(&list, "game.exe"), apps(&["other.exe"]));
}

/// The stored order decides which of two simultaneous launches wins, so
/// adding appends and removing leaves the rest where they were.
#[test]
fn the_registration_order_survives_an_add_and_a_remove() {
    let list = apps(&["first.exe", "second.exe"]);
    let list = toggle(&list, AutoCaptureApp::named("third.exe"));
    assert_eq!(list, apps(&["first.exe", "second.exe", "third.exe"]));
    assert_eq!(
        unregister(&list, "second.exe"),
        apps(&["first.exe", "third.exe"])
    );
}

/// `poll` skips empty entries, so writing one is only a line in the
/// settings row that does nothing.
#[test]
fn a_blank_name_is_never_registered() {
    assert!(toggle(&[], AutoCaptureApp::named("   ")).is_empty());
    assert!(!is_registered(&apps(&["  "]), "  "));
}

/// The review row's whole derivation, both switch positions (round13 §1).
#[test]
fn the_review_toggle_reads_the_list_for_the_recorded_executable() {
    let off = review_toggle(
        Locale::Ja,
        Some("mspaint.exe"),
        &[],
        &[],
        &open_guard(),
        None,
    );
    assert!(off.visible);
    assert!(!off.checked);
    assert_eq!(
        off.body,
        "オンにすると、mspaint.exe の起動で録画を始めます。"
    );
    assert_eq!(off.executable, "mspaint.exe");

    let on = review_toggle(
        Locale::Ja,
        Some("mspaint.exe"),
        &apps(&["mspaint.exe"]),
        &[],
        &open_guard(),
        None,
    );
    assert!(on.checked);
    // Registered: the line stops offering and states what happens (t261104-185f).
    assert_eq!(on.body, "mspaint.exe が起動すると録画を始めます。");

    // A hand-edited `settings.json` spelling it differently still has the
    // row reading on -- otherwise switching it would add a second entry.
    let hand_edited = apps(&["  Mspaint.exe  "]);
    for (locale, executable) in [(Locale::Ja, "mspaint.exe"), (Locale::En, "MSPAINT.EXE")] {
        assert!(
            review_toggle(
                locale,
                Some(executable),
                &hand_edited,
                &[],
                &open_guard(),
                None
            )
            .checked,
            "{executable}"
        );
    }
}

/// Monitor recordings, and everything recorded before task2350 wrote the
/// name into the container, have no executable. No row: there is nothing to
/// register, and a switch over an unknown app would be a lie. Unchanged by
/// task3670: a folder rule covering the path does not conjure a row.
#[test]
fn the_review_toggle_is_hidden_without_a_recorded_executable() {
    for missing in [None, Some(""), Some("   ")] {
        assert_eq!(
            review_toggle(
                Locale::Ja,
                missing,
                &apps(&["mspaint.exe"]),
                &[],
                &open_guard(),
                None
            ),
            ReviewToggle::default()
        );
        assert_eq!(
            review_toggle(
                Locale::Ja,
                missing,
                &[],
                &folders(&[r"D:\Games\Foo"]),
                &open_guard(),
                Some(Path::new(r"D:\Games\Foo\mspaint.exe")),
            ),
            ReviewToggle::default()
        );
    }
}

// ---- task3670: the display sites read folder rules too ----

/// The whole answer both display sites read, one row per case.
#[test]
fn covered_by_answers_the_executable_list_first_then_the_folders() {
    let list = apps(&["game.exe"]);
    let rules = folders(&[r"D:\Games\Foo"]);
    let under = Path::new(r"D:\Games\Foo\bin\game.exe");
    let elsewhere = Path::new(r"C:\Users\me\game.exe");

    // On the executable list: that answer, whatever the folders say.
    assert_eq!(
        covered_by(&list, &[], &open_guard(), "game.exe", None),
        Some(AutoCaptureRule::Executable)
    );
    assert_eq!(
        covered_by(&list, &rules, &open_guard(), "game.exe", Some(under)),
        Some(AutoCaptureRule::Executable)
    );
    // Not on it, but the path is under a rule: the rule, spelled as the
    // settings file spells it.
    assert_eq!(
        covered_by(&[], &rules, &open_guard(), "game.exe", Some(under)),
        Some(AutoCaptureRule::Folder(r"D:\Games\Foo".to_owned()))
    );
    // Not on it and no path to match: the pre-task3670 answer.
    assert_eq!(
        covered_by(&[], &rules, &open_guard(), "game.exe", None),
        None
    );
    // Not on it and the path is under no rule.
    assert_eq!(
        covered_by(&[], &rules, &open_guard(), "game.exe", Some(elsewhere)),
        None
    );
    // Neither list.
    assert_eq!(
        covered_by(&[], &[], &open_guard(), "game.exe", Some(under)),
        None
    );
}

/// The same folding `folder_matches` and `same_rule` do -- one rule however
/// either side spells it -- and the same component boundary, so a sibling
/// folder sharing a prefix of letters is not covered.
#[test]
fn covered_by_folds_a_rule_the_way_the_watch_folds_it() {
    for rule in [r"D:\Games\Foo\", r"d:/games/foo", r"  D:\GAMES\FOO  "] {
        let rules = folders(&[rule]);
        assert_eq!(
            covered_by(
                &[],
                &rules,
                &open_guard(),
                "game.exe",
                Some(Path::new(r"d:\games\Foo\game.exe"))
            ),
            Some(AutoCaptureRule::Folder(rule.to_owned())),
            "{rule}"
        );
    }
    // `D:\Games\Foo` does not reach `D:\Games\FooBar` (component-wise).
    assert_eq!(
        covered_by(
            &[],
            &folders(&[r"D:\Games\Foo"]),
            &open_guard(),
            "game.exe",
            Some(Path::new(r"D:\Games\FooBar\game.exe"))
        ),
        None
    );
}

/// Two rules covering one path are decided by registration order, the same
/// way `matching_folder` decides for `poll`.
#[test]
fn covered_by_reports_the_first_matching_rule() {
    assert_eq!(
        covered_by(
            &[],
            &folders(&[r"D:\Games", r"D:\Games\Foo"]),
            &open_guard(),
            "game.exe",
            Some(Path::new(r"D:\Games\Foo\game.exe"))
        ),
        Some(AutoCaptureRule::Folder(r"D:\Games".to_owned()))
    );
}

/// Round21 §4-2 (task3840), the required case: covered *only* by a folder
/// rule, the switch stays off -- it reports the executable list, which this
/// app is not on -- and the line under it carries the whole fact, including
/// what switching it on would add.
///
/// This deliberately reverses task3670's toggle-side expectation. The badge
/// side of that task is untouched and still holds: `covered_by` keeps
/// answering `Folder`, which is what the picker reads.
#[test]
fn the_review_toggle_reads_off_for_a_folder_rule_and_says_why() {
    let rules = folders(&[r"D:\Games\Foo"]);
    let path = Path::new(r"D:\Games\Foo\game.exe");

    let ja = review_toggle(
        Locale::Ja,
        Some("game.exe"),
        &[],
        &rules,
        &open_guard(),
        Some(path),
    );
    assert!(ja.visible);
    assert!(!ja.checked);
    assert_eq!(
        ja.body,
        "フォルダの規則で自動録画の対象です。オンにすると、このアプリだけでも登録します。"
    );
    // The name is still what the switch would register.
    assert_eq!(ja.executable, "game.exe");
    // The rule itself is the settings screen's business, not this line's.
    assert!(!ja.body.contains("Games"), "{}", ja.body);

    let en = review_toggle(
        Locale::En,
        Some("game.exe"),
        &[],
        &rules,
        &open_guard(),
        Some(path),
    );
    assert!(en.visible);
    assert!(!en.checked);
    assert_eq!(
        en.body,
        "Covered by a folder rule. Turning this on also registers this app on its own."
    );
    assert!(!en.body.contains("Games"), "{}", en.body);

    // The badge's half of the same question is unchanged -- that is what
    // makes the two displays deliberately different rather than drifted.
    assert_eq!(
        covered_by(&[], &rules, &open_guard(), "game.exe", Some(path)),
        Some(AutoCaptureRule::Folder(r"D:\Games\Foo".to_owned()))
    );

    // Registered by name as well: on, with the legacy line, because that is
    // the rule that answers (the order `poll` arms in).
    let both = review_toggle(
        Locale::Ja,
        Some("game.exe"),
        &apps(&["game.exe"]),
        &rules,
        &open_guard(),
        Some(path),
    );
    assert!(both.checked);
    assert_eq!(both.body, "game.exe が起動すると録画を始めます。");

    // Nothing running to resolve a path from: unchanged from before.
    let unresolved = review_toggle(
        Locale::Ja,
        Some("game.exe"),
        &[],
        &rules,
        &open_guard(),
        None,
    );
    assert!(unresolved.visible && !unresolved.checked);
    assert_eq!(
        unresolved.body,
        "オンにすると、game.exe の起動で録画を始めます。"
    );
}

// ---- task3690: the display sites read the guard too ----

/// The defect: `poll` drops a rule the guard turns down (`active_folders`),
/// so the badge and the toggle have to drop it as well. The second half of
/// this asserts the guard is what changes the answer -- with an open guard
/// the same rules still cover the path, which is exactly what the display
/// used to report while `poll` recorded nothing.
#[test]
fn covered_by_ignores_a_rule_the_guard_turns_down() {
    let rules = folders(&[r"C:\Windows", r"C:\Windows\System32"]);
    let path = Path::new(r"C:\Windows\System32\notepad.exe");
    assert_eq!(
        covered_by(&[], &rules, &windows_guard(), "notepad.exe", Some(path)),
        None
    );
    assert_eq!(
        covered_by(&[], &rules, &open_guard(), "notepad.exe", Some(path)),
        Some(AutoCaptureRule::Folder(r"C:\Windows".to_owned()))
    );
}

/// Filter before the match, not after. `%ProgramFiles%` itself is refused
/// while its subfolders are not (that is where games install), so a
/// hand-written `C:\Program Files` ahead of a real rule must not swallow
/// the answer -- finding first and checking the guard afterwards would
/// report `None` here and disagree with `poll`.
#[test]
fn covered_by_reports_the_first_rule_the_guard_allows() {
    assert_eq!(
        covered_by(
            &[],
            &folders(&[r"C:\Program Files", r"C:\Program Files\Foo"]),
            &windows_guard(),
            "game.exe",
            Some(Path::new(r"C:\Program Files\Foo\bin\game.exe"))
        ),
        Some(AutoCaptureRule::Folder(r"C:\Program Files\Foo".to_owned()))
    );
}

/// The guard reads folder rules and nothing else. A name on the executable
/// list is armed by `poll` wherever it lives, so the badge stays on even
/// when the only rule also covering it is refused.
#[test]
fn the_executable_list_still_wins_over_a_guarded_rule() {
    assert_eq!(
        covered_by(
            &apps(&["game.exe"]),
            &folders(&[r"C:\Windows"]),
            &windows_guard(),
            "game.exe",
            Some(Path::new(r"C:\Windows\game.exe"))
        ),
        Some(AutoCaptureRule::Executable)
    );
}

/// An open guard is not "no checking": `folder_rule_allowed_with` refuses a
/// rule naming no folder at all -- a volume root, a bare UNC share -- on
/// its own, before it ever looks at the guard's lists. `folder_matches`
/// would happily say `C:\` covers everything on the volume, which is what
/// makes this worth pinning down.
#[test]
fn a_volume_root_is_refused_even_by_an_open_guard() {
    let path = Path::new(r"C:\Games\game.exe");
    assert!(folder_matches(r"C:\", path));
    assert_eq!(
        covered_by(
            &[],
            &folders(&[r"C:\"]),
            &open_guard(),
            "game.exe",
            Some(path)
        ),
        None
    );
}

/// The toggle side of the same defect: off, and the line under it falls
/// back to the executable sentence rather than naming a rule that arms
/// nothing. `visible` is untouched -- the session still names an app.
#[test]
fn the_review_toggle_reads_off_for_a_guarded_rule() {
    let rules = folders(&[r"C:\Windows"]);
    let path = Path::new(r"C:\Windows\game.exe");

    let ja = review_toggle(
        Locale::Ja,
        Some("game.exe"),
        &[],
        &rules,
        &windows_guard(),
        Some(path),
    );
    assert!(ja.visible);
    assert!(!ja.checked);
    assert_eq!(ja.body, "オンにすると、game.exe の起動で録画を始めます。");
    assert!(!ja.body.contains("Windows"), "{}", ja.body);

    let en = review_toggle(
        Locale::En,
        Some("game.exe"),
        &[],
        &rules,
        &windows_guard(),
        Some(path),
    );
    assert!(en.visible);
    assert!(!en.checked);
    assert_eq!(
        en.body,
        "Turn on to start recording when game.exe launches."
    );
    assert!(!en.body.contains("Windows"), "{}", en.body);
}

/// The rearm primitive (task2550): forgetting the executable makes the
/// next poll read the still-running process as a fresh launch, so the
/// next selectable window of it starts a new recording. Spelled with the
/// on-disk casing to prove `forget` folds case like everything else here.
#[test]
fn forgetting_a_running_app_manufactures_a_new_edge() {
    let list = strings(&["game.exe"]);
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    let first = [window("2", "game.exe", true)];
    assert!(poll_live(&mut watch, &list, &first, false).start.is_some());
    poll_live(&mut watch, &list, &first, true);
    // The bound window closed (SourceClosed) but the process lives, and
    // another window of it is up. Without the forget this is the
    // manual-stop shape: no edge, nothing starts.
    let second = [window("3", "game.exe", true)];
    let running = alive(&second);
    assert_eq!(
        poll_names(&mut watch, &list, &running, &second, false),
        AutoCapturePlan::default()
    );
    watch.forget("Game.exe");
    let plan = poll_names(&mut watch, &list, &running, &second, false);
    assert_eq!(plan.armed.as_deref(), Some("game.exe"));
    assert_eq!(plan.start.as_deref(), Some("3"));
}

/// A forget whose process then dies starts nothing: the edge needs the
/// process present on the next poll, and it is not. This is the app
/// simply exiting after its last window closed.
#[test]
fn forgetting_a_dead_app_starts_nothing() {
    let list = strings(&["game.exe"]);
    let targets = [window("2", "game.exe", true)];
    let mut watch = AutoCaptureWatch::default();
    poll_live(&mut watch, &list, &[], false);
    assert!(poll_live(&mut watch, &list, &targets, false)
        .start
        .is_some());
    poll_live(&mut watch, &list, &targets, true);
    watch.forget("game.exe");
    // The process is gone by the next poll.
    for _ in 0..3 {
        assert_eq!(
            poll_names(&mut watch, &list, &HashSet::new(), &[], false),
            AutoCapturePlan::default()
        );
    }
}

/// `forget` is surgical: the other registered app was already running,
/// stays in `seen`, and must not fire off someone else's rearm.
#[test]
fn forgetting_one_app_leaves_the_others_seen() {
    let list = strings(&["first.exe", "second.exe"]);
    let targets = [
        window("2", "first.exe", true),
        window("3", "second.exe", true),
    ];
    let running = alive(&targets);
    let mut watch = AutoCaptureWatch::default();
    // Seed with both already up: no edges.
    poll_names(&mut watch, &list, &running, &targets, false);
    watch.forget("first.exe");
    let plan = poll_names(&mut watch, &list, &running, &targets, false);
    assert_eq!(plan.armed.as_deref(), Some("first.exe"));
    assert_eq!(plan.start.as_deref(), Some("2"));
    // `second.exe` had no edge -- not even a skip.
    assert!(plan.skipped.is_empty());
}

/// Nothing registered is the shipped default: no decisions, and no set
/// built out of a list that is polled every second regardless.
#[test]
fn an_empty_registration_list_decides_nothing() {
    let mut watch = AutoCaptureWatch::default();
    let targets = [window("2", "game.exe", true)];
    assert_eq!(
        poll_live(&mut watch, &[], &targets, false),
        AutoCapturePlan::default()
    );
    assert!(!watch.seeded);
    // Registering an app that is already running still needs an edge.
    let list = strings(&["game.exe"]);
    assert_eq!(
        poll_live(&mut watch, &list, &targets, false),
        AutoCapturePlan::default()
    );
    assert_eq!(
        poll_live(&mut watch, &list, &targets, false),
        AutoCapturePlan::default()
    );
}

// ---- task3600: folder rules ----

fn folders(paths: &[&str]) -> Vec<AutoCaptureFolder> {
    paths
        .iter()
        .map(|path| AutoCaptureFolder::at(*path))
        .collect()
}

/// The boundary the whole rule turns on: `starts_with` on the *string*
/// would put `D:\Games\FooBar` under `D:\Games\Foo`.
#[test]
fn a_folder_rule_matches_by_component_not_by_string_prefix() {
    let rule = r"D:\Games\Foo";
    assert!(folder_matches(rule, Path::new(r"D:\Games\Foo\game.exe")));
    // Subfolders, to any depth.
    assert!(folder_matches(
        rule,
        Path::new(r"D:\Games\Foo\bin\x64\game.exe")
    ));
    // The sibling that shares a prefix of letters but not of components.
    assert!(!folder_matches(
        rule,
        Path::new(r"D:\Games\FooBar\game.exe")
    ));
    // A shorter path cannot be under a longer rule.
    assert!(!folder_matches(rule, Path::new(r"D:\Games\game.exe")));
    // Another drive, another tree.
    assert!(!folder_matches(rule, Path::new(r"E:\Games\Foo\game.exe")));
    assert!(!folder_matches(rule, Path::new(r"D:\Other\Foo\game.exe")));
    // Nothing is expanded (out of scope, and said so in the task): a rule
    // written with an environment variable is a rule that matches nothing
    // rather than one that silently covers Program Files.
    assert!(!folder_matches(
        r"%ProgramFiles%\Foo",
        Path::new(r"C:\Program Files\Foo\game.exe")
    ));
}

/// Windows filenames fold case and a hand-written rule is spelled however
/// the person felt -- drive letter, folder names, trailing separator and
/// separator flavour all included.
#[test]
fn a_folder_rule_folds_case_and_ignores_a_trailing_separator() {
    let path = Path::new(r"D:\Games\Foo\game.exe");
    for rule in [
        r"D:\Games\Foo",
        r"d:\games\foo",
        r"D:\GAMES\FOO\",
        r"D:/Games/Foo",
        "  D:\\Games\\Foo  ",
    ] {
        assert!(folder_matches(rule, path), "{rule}");
    }
    // A blank rule is not a rule that matches everything.
    for blank in ["", "   "] {
        assert!(!folder_matches(blank, path), "{blank:?}");
    }
}

/// A network install is a folder like any other, and its share is one
/// component pair that has to fold case too.
#[test]
fn a_unc_folder_rule_matches_its_own_share() {
    let rule = r"\\nas\games\Foo";
    assert!(folder_matches(
        rule,
        Path::new(r"\\NAS\Games\Foo\bin\game.exe")
    ));
    assert!(!folder_matches(
        rule,
        Path::new(r"\\nas\games\FooBar\game.exe")
    ));
    // A different share is a different rule even with the same tail.
    assert!(!folder_matches(
        rule,
        Path::new(r"\\nas\other\Foo\game.exe")
    ));
    // ...and a local path is never under a share.
    assert!(!folder_matches(rule, Path::new(r"D:\games\Foo\game.exe")));
}

/// ユーザー裁定3: a rule wider than a folder would record whatever the
/// machine happens to run, so the guard turns it down.
#[test]
fn the_guard_turns_down_volume_roots_and_windows() {
    let guard = windows_guard();
    for rejected in [
        "",
        "   ",
        r"C:\",
        "C:",
        r"D:\",
        r"\\nas\games",
        r"C:\Windows",
        r"c:\windows\system32",
        r"C:\WINDOWS\",
        r"C:\Program Files",
        r"c:\program files\",
        r"C:\Program Files (x86)",
    ] {
        assert!(
            !folder_rule_allowed_with(rejected, &guard),
            "{rejected:?} should be refused"
        );
    }
    for allowed in [
        r"D:\Games\Foo",
        // Under Program Files is where games install -- only the folder
        // itself is refused.
        r"C:\Program Files\Foo",
        r"C:\Program Files (x86)\Steam\steamapps",
        // Not the Windows folder, just a name that starts the same way.
        r"C:\WindowsApps\Foo",
        r"\\nas\games\Foo",
    ] {
        assert!(
            folder_rule_allowed_with(allowed, &guard),
            "{allowed:?} should be allowed"
        );
    }
    // the settings list marks exactly the rules the poll ignores.
    // What the settings row asks: the rules `poll` throws away are exactly
    // the ones the list has to mark. Volume roots go regardless of the guard
    // (`a_volume_root_is_refused_even_by_an_open_guard`'s point, from the
    // display's side), and a folder under Program Files is a normal rule.
    {
        let guard = windows_guard();
        for ignored in [
            r"C:\Windows",
            r"C:\Windows\System32",
            r"C:\Windows\System32\drivers\etc",
            r"C:\",
            r"C:\Program Files",
            "",
        ] {
            assert!(
                folder_rule_ignored(ignored, &guard),
                "{ignored:?} should be marked ignored"
            );
        }
        for kept in [
            r"C:\Program Files\Foo",
            r"D:\Games\Foo",
            r"C:\Program Files (x86)\Steam\steamapps",
        ] {
            assert!(
                !folder_rule_ignored(kept, &guard),
                "{kept:?} should not be marked"
            );
        }
        // An open guard is not "nothing is ignored": the volume root falls out
        // of the rule's own shape, not out of the guard's lists.
        assert!(folder_rule_ignored(r"C:\", &open_guard()));
        assert!(!folder_rule_ignored(r"C:\Windows", &open_guard()));
    }
}

// ---------- the settings screen's folder rows (task3610) ----------

/// The 「フォルダを追加」 button's whole job. Appending rather than
/// inserting keeps the stored order the list draws.
#[test]
fn adding_a_folder_puts_it_on_the_end() {
    let list = folders(&[r"D:\Games\Foo"]);
    let next = add_folder(&list, r"E:\Games\Bar");
    assert_eq!(next, folders(&[r"D:\Games\Foo", r"E:\Games\Bar"]));
}

/// Picking the same folder twice -- in any spelling the picker or a
/// hand-edited file can produce -- must not put it on the list twice.
#[test]
fn adding_a_folder_that_is_already_a_rule_changes_nothing() {
    let list = folders(&[r"D:\Games\Foo"]);
    for spelling in [
        r"D:\Games\Foo",
        r"d:\games\foo",
        r"D:/Games/Foo/",
        "  D:\\Games\\Foo  ",
    ] {
        assert!(
            folder_registered(&list, spelling),
            "{spelling:?} should already be registered"
        );
        assert_eq!(add_folder(&list, spelling), list, "{spelling:?}");
    }
    assert!(!folder_registered(&list, r"D:\Games\FooBar"));
    assert!(!folder_registered(&list, "   "));
    assert_eq!(add_folder(&list, "   "), list);
}

/// The × on a folder row reports the path string, and the removal has to
/// take every spelling of that folder with it -- the executable list's
/// `unregister` reasoning, for the same reason: a survivor stays armed.
#[test]
fn removing_a_folder_takes_every_spelling_of_it() {
    let list = folders(&[r"D:\Games\Foo", r"d:/games/foo/", r"D:\Games\Bar"]);
    assert_eq!(
        unregister_folder(&list, r"D:\GAMES\Foo"),
        folders(&[r"D:\Games\Bar"])
    );
}

/// round21 §1-2 / §2-4 / §2-5 settled these four word for word, and the
/// screen is the only other place they can be checked. Pinned exactly
/// rather than by `contains`: a paraphrase that still contains
/// 「フォルダ」 would pass a `contains` check and ship the wrong sentence
/// (task3830).
#[test]
fn the_target_row_says_what_round21_settled() {
    // t260927-fde8: the DS SettingsScreen's 自動録画 group.
    assert_eq!(settings_label(Locale::Ja), "自動録画");
    assert_eq!(settings_label(Locale::En), "Auto-record");
    assert_eq!(
            settings_sub(Locale::Ja),
            "ここにあるアプリが起動すると、録画を自動で始めます。アプリは確認画面から、フォルダはここから追加します。"
        );
    assert_eq!(
            settings_sub(Locale::En),
            "Apps listed here start a recording when they launch. Add apps from review, folders from here."
        );
    assert_eq!(settings_empty(Locale::Ja), "まだありません");
    assert_eq!(settings_empty(Locale::En), "None yet");
    // The button moved to the right end of the label line in task3610; the
    // empty state kept saying 「右上の」 until round21 §2-4 rewrote it.
    assert!(!settings_empty(Locale::Ja).contains("右上"));
    assert!(!settings_empty(Locale::En).contains("above"));
    assert_eq!(
            settings_folder_rejected(Locale::Ja),
            "このフォルダは登録できません。ドライブ直下や Windows のシステムフォルダは対象にできません。"
        );
    assert_eq!(
        settings_folder_rejected(Locale::En),
        "That folder can't be registered. Drive roots and Windows system folders can't be targets."
    );
}

/// The two runs of a folder row. The tail is what the row promotes, so the
/// split has to survive the spellings `settings.json` can hold by hand:
/// a volume root with nothing above it, a trailing separator, a UNC share,
/// and a rule one level deep whose parent *is* the root.
#[test]
fn a_folder_row_promotes_the_last_folder_and_keeps_the_parent_ahead_of_it() {
    // A volume root has no folder to promote: the whole rule is the tail.
    assert_eq!(folder_row_parts(r"D:\"), (r"D:\".to_owned(), String::new()));
    // A trailing separator is not a second, nameless level (`folder_matches`
    // normalises it away too), and the parent keeps its own separator.
    assert_eq!(
        folder_row_parts(r"D:\Games\Foo\"),
        ("Foo".to_owned(), r"D:\Games\".to_owned())
    );
    assert_eq!(
        folder_row_parts(r"D:\Games\Foo"),
        folder_row_parts(r"D:\Games\Foo\")
    );
    assert_eq!(
        folder_row_parts(r"\\server\share\Foo"),
        ("Foo".to_owned(), r"\\server\share\".to_owned())
    );
    // One level deep: the parent is the root, drawn as the mock draws it.
    assert_eq!(
        folder_row_parts(r"D:\Foo"),
        ("Foo".to_owned(), r"D:\".to_owned())
    );
    // The rule's own spelling survives -- sliced out, not rebuilt.
    assert_eq!(
        folder_row_parts("d:/games/foo"),
        ("foo".to_owned(), "d:/games/".to_owned())
    );
    assert_eq!(folder_row_parts("  "), (String::new(), String::new()));
}

// ---------- the settings list's ignored line (task3730) ----------

/// One guard, two audiences. task3730 pinned the row's reason clause byte
/// for byte against the toast's; round21 §2-5 then narrowed the toast to
/// the two cases worth warning about before the fact and dropped
/// `%ProgramFiles%` itself, which the row cannot drop -- a hand-written
/// `C:\Program Files` rule is one of the rows this line has to explain.
/// So the equality is gone on purpose (task3830) and what is pinned now is
/// the divergence: the row keeps its own all-three-cases reason, the toast
/// keeps the narrowed one, and neither drifts onto the other unnoticed.
#[test]
fn the_two_guard_lines_say_the_guard_their_own_way() {
    for locale in [Locale::Ja, Locale::En] {
        assert!(!settings_folder_ignored(locale).is_empty());
    }
    // ja: 「…（理由）」 -- the reason is what follows the opening paren.
    let (ignored_head, ignored_reason) = settings_folder_ignored(Locale::Ja)
        .split_once('（')
        .expect("the ja line states its reason in parentheses");
    assert_eq!(ignored_head, "無視されています");
    assert_eq!(ignored_reason, "配下のプロセスが多すぎます）");
    // en: "<state>: <reason>".
    let (ignored_head, ignored_reason) = settings_folder_ignored(Locale::En)
        .split_once(": ")
        .expect("the en line states its reason after a colon");
    assert_eq!(ignored_head, "Ignored");
    assert_eq!(ignored_reason, "too many processes run from under it.");
    // The toast names the two refusals a person meets from the picker, and
    // never Program Files -- whose *subfolders* are ordinary rules.
    assert!(settings_folder_rejected(Locale::Ja).contains("ドライブ直下"));
    assert!(settings_folder_rejected(Locale::Ja).contains("システムフォルダ"));
    assert!(settings_folder_rejected(Locale::En).contains("Drive roots"));
    assert!(settings_folder_rejected(Locale::En).contains("system folders"));
    for locale in [Locale::Ja, Locale::En] {
        assert!(!settings_folder_rejected(locale).contains("Program Files"));
    }
}

/// The whole point of the feature: a process nobody registered by name is
/// recorded because of where it lives.
#[test]
fn a_process_under_a_registered_folder_arms_without_being_named() {
    let list = folders(&[r"D:\Games\Foo"]);
    let editor = proc("1", 10, r"C:\Windows\notepad.exe", true);
    let game = proc("2", 11, r"D:\Games\Foo\bin\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    assert_eq!(
        poll_folders(&mut watch, &list, &[&editor], false),
        AutoCapturePlan::default()
    );
    let plan = poll_folders(&mut watch, &list, &[&editor, &game], false);
    assert_eq!(plan.armed.as_deref(), Some("game.exe"));
    assert_eq!(plan.start.as_deref(), Some("2"));
    assert_eq!(
        plan.rule,
        Some(AutoCaptureRule::Folder(r"D:\Games\Foo".to_owned()))
    );
}

/// The same executable name outside the rule is not the same install.
#[test]
fn the_same_name_outside_the_folder_starts_nothing() {
    let list = folders(&[r"D:\Games\Foo"]);
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let elsewhere = proc("2", 11, r"D:\Games\FooBar\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_folders(&mut watch, &list, &[&seed], false);
    assert_eq!(
        poll_folders(&mut watch, &list, &[&seed, &elsewhere], false),
        AutoCapturePlan::default()
    );
}

/// A rule the guard turned down is ignored, and said so once -- not once a
/// second for as long as the app is open.
#[test]
fn a_guarded_rule_is_ignored_and_logged_once() {
    let list = folders(&[r"C:\Windows"]);
    let guard = windows_guard();
    let seed = proc("1", 10, r"D:\Games\Foo\launcher.exe", true);
    let inside = proc("2", 11, r"C:\Windows\System32\calc.exe", true);
    let mut watch = AutoCaptureWatch::default();
    let plan = poll_mixed(&mut watch, &[], &list, &[&seed], false, &guard);
    assert_eq!(plan.rejected_folders, vec![r"C:\Windows".to_owned()]);
    // Silent from here on, and the rule decides nothing.
    for _ in 0..3 {
        let plan = poll_mixed(&mut watch, &[], &list, &[&seed, &inside], false, &guard);
        assert!(plan.rejected_folders.is_empty());
        assert_eq!(plan.armed, None);
    }
    // Emptying both lists drops the state, so re-adding the bad rule is
    // reported again rather than silently swallowed.
    poll_mixed(&mut watch, &[], &[], &[&seed], false, &guard);
    let plan = poll_mixed(&mut watch, &[], &list, &[&seed], false, &guard);
    assert_eq!(plan.rejected_folders, vec![r"C:\Windows".to_owned()]);
}

/// ユーザー裁定c, the case the whole handoff exists for: the launcher is
/// recorded, the game it starts lives in the same registered folder, so the
/// recording moves to the game instead of staying on the launcher.
#[test]
fn a_launcher_hands_its_recording_to_the_game_it_starts() {
    let list = folders(&[r"D:\Games\Foo"]);
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let launcher = proc("2", 11, r"D:\Games\Foo\launcher.exe", true);
    let game = proc("3", 12, r"D:\Games\Foo\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_folders(&mut watch, &list, &[&seed], false);
    // The launcher launches and is recorded.
    let plan = poll_folders(&mut watch, &list, &[&seed, &launcher], false);
    assert_eq!(plan.start.as_deref(), Some("2"));
    poll_folders(&mut watch, &list, &[&seed, &launcher], true);
    // The game appears while that recording is up. Without the handoff this
    // is the hard skip, and the game is never recorded.
    let plan = poll_folders(&mut watch, &list, &[&seed, &launcher, &game], true);
    assert_eq!(plan.handoff.as_deref(), Some("game.exe"));
    assert!(plan.skipped.is_empty());
    // The caller stops the recording and applies the rearm; the game is a
    // fresh launch from the next poll's point of view.
    watch.forget("game.exe");
    let plan = poll_folders(&mut watch, &list, &[&seed, &launcher, &game], false);
    assert_eq!(plan.armed.as_deref(), Some("game.exe"));
    assert_eq!(plan.start.as_deref(), Some("3"));
    // The plain rearm says nothing about a handoff: this is the same call
    // the SourceClosed rearm makes (task3840).
    assert!(!plan.handed_over);
}

// ---- task3840: the second toast of a handoff says so ----

/// Round21 §4-1: the start the handoff's rearm leads to is marked, so its
/// toast reads 「録画を引き継ぎました」 instead of announcing a second
/// automatic start. The mark survives the retry polls, because the caller
/// announces on `Started` rather than on the ask -- and it is gone by the
/// time the *next* launch of the same app comes round.
#[test]
fn a_handed_over_rearm_marks_the_start_it_leads_to() {
    let list = folders(&[r"D:\Games\Foo"]);
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let launcher = proc("2", 11, r"D:\Games\Foo\launcher.exe", true);
    let game = proc("3", 12, r"D:\Games\Foo\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_folders(&mut watch, &list, &[&seed], false);
    // The launcher's own recording: an ordinary start, ordinary toast.
    let plan = poll_folders(&mut watch, &list, &[&seed, &launcher], false);
    assert_eq!(plan.start.as_deref(), Some("2"));
    assert!(!plan.handed_over);
    poll_folders(&mut watch, &list, &[&seed, &launcher], true);
    let all = [&seed, &launcher, &game];
    assert_eq!(
        poll_folders(&mut watch, &list, &all, true)
            .handoff
            .as_deref(),
        Some("game.exe")
    );
    // The picker stops the recording and applies *this* rearm.
    watch.forget_handed_over("Game.exe");
    let plan = poll_folders(&mut watch, &list, &all, false);
    assert_eq!(plan.start.as_deref(), Some("3"));
    assert!(plan.handed_over);
    // A retry of the same start is still the same handoff.
    let retry = poll_folders(&mut watch, &list, &all, false);
    assert_eq!(retry.start.as_deref(), Some("3"));
    assert!(retry.handed_over);
    // The start took. The game closes and is launched again by hand: that
    // is a launch, not a handoff.
    poll_folders(&mut watch, &list, &all, true);
    poll_folders(&mut watch, &list, &[&seed, &launcher], false);
    let relaunch = poll_folders(&mut watch, &list, &all, false);
    assert_eq!(relaunch.start.as_deref(), Some("3"));
    assert!(!relaunch.handed_over);
}

/// The mark is spent on the recording it was made for, not on whichever app
/// happens to arm next: a handed-over process that dies during its splash
/// wait leaves nothing marked behind.
#[test]
fn a_handed_over_process_that_dies_leaves_no_mark() {
    let list = folders(&[r"D:\Games\Foo"]);
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let game = proc("3", 12, r"D:\Games\Foo\game.exe", false);
    let sibling = proc("4", 13, r"D:\Games\Foo\other.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_folders(&mut watch, &list, &[&seed], false);
    // Armed but not started: no capturable window yet.
    watch.forget_handed_over("game.exe");
    let plan = poll_folders(&mut watch, &list, &[&seed, &game], false);
    assert_eq!(plan.armed.as_deref(), Some("game.exe"));
    assert_eq!(plan.start, None);
    // It dies there, and a different app under the same rule launches.
    poll_folders(&mut watch, &list, &[&seed], false);
    let plan = poll_folders(&mut watch, &list, &[&seed, &sibling], false);
    assert_eq!(plan.start.as_deref(), Some("4"));
    assert!(!plan.handed_over);
}

/// The picker-side guard (task3600 follow-up): a `handoff` the watch
/// already decided on is withheld while the picker knows it is in the
/// middle of draining a stop, and passed through otherwise.
#[test]
fn the_handoff_target_is_withheld_while_stopping() {
    assert_eq!(handoff_target(Some("game.exe"), true), None);
    assert_eq!(handoff_target(Some("game.exe"), false), Some("game.exe"));
    assert_eq!(handoff_target(None, false), None);
    assert_eq!(handoff_target(None, true), None);
}

/// Ping-pong guard: one handoff per recording. A third process out of the
/// same folder must not chain another stop onto the same session.
#[test]
fn a_folder_hands_off_only_once_per_recording() {
    let list = folders(&[r"D:\Games\Foo"]);
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let launcher = proc("2", 11, r"D:\Games\Foo\launcher.exe", true);
    let game = proc("3", 12, r"D:\Games\Foo\game.exe", true);
    let helper = proc("4", 13, r"D:\Games\Foo\crashpad.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_folders(&mut watch, &list, &[&seed], false);
    poll_folders(&mut watch, &list, &[&seed, &launcher], false);
    poll_folders(&mut watch, &list, &[&seed, &launcher], true);
    assert!(
        poll_folders(&mut watch, &list, &[&seed, &launcher, &game], true)
            .handoff
            .is_some()
    );
    // Still draining, and another sibling shows up.
    let plan = poll_folders(&mut watch, &list, &[&seed, &launcher, &game, &helper], true);
    assert_eq!(plan.handoff, None);
    assert_eq!(plan.skipped, vec!["crashpad.exe".to_owned()]);
}

/// The rule that protects the user: a recording they started themselves is
/// never handed away, however many processes appear under a registered
/// folder. The watch armed nothing, so it holds no rule to hand over.
#[test]
fn a_manually_started_recording_is_never_handed_off() {
    let list = folders(&[r"D:\Games\Foo"]);
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let game = proc("2", 11, r"D:\Games\Foo\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    // Seeded while the user's own recording is already running.
    poll_folders(&mut watch, &list, &[&seed], true);
    let plan = poll_folders(&mut watch, &list, &[&seed, &game], true);
    assert_eq!(plan.handoff, None);
    assert_eq!(plan.skipped, vec!["game.exe".to_owned()]);
}

/// Handoff is a within-one-rule behaviour: a launch under a *different*
/// registered folder is the ordinary hard skip, not a reason to take the
/// encoder off the recording that is running.
#[test]
fn a_launch_under_another_folder_rule_does_not_hand_off() {
    let list = folders(&[r"D:\Games\Foo", r"D:\Games\Bar"]);
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let first = proc("2", 11, r"D:\Games\Foo\game.exe", true);
    let second = proc("3", 12, r"D:\Games\Bar\other.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_folders(&mut watch, &list, &[&seed], false);
    poll_folders(&mut watch, &list, &[&seed, &first], false);
    poll_folders(&mut watch, &list, &[&seed, &first], true);
    let plan = poll_folders(&mut watch, &list, &[&seed, &first, &second], true);
    assert_eq!(plan.handoff, None);
    assert_eq!(plan.skipped, vec!["other.exe".to_owned()]);
}

/// ...and neither does a registration by name: the handoff belongs to the
/// folder rule, which is the only place a second process is expected to be
/// the same thing as the first.
#[test]
fn an_executable_registration_never_hands_off() {
    let names = strings(&["launcher.exe"]);
    let list = folders(&[r"D:\Games\Foo"]);
    let guard = open_guard();
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let launcher = proc("2", 11, r"D:\Games\Foo\launcher.exe", true);
    let game = proc("3", 12, r"D:\Games\Foo\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_mixed(&mut watch, &names, &list, &[&seed], false, &guard);
    // Armed by its name, so `holding` is the executable rule.
    let plan = poll_mixed(
        &mut watch,
        &names,
        &list,
        &[&seed, &launcher],
        false,
        &guard,
    );
    assert_eq!(plan.rule, Some(AutoCaptureRule::Executable));
    poll_mixed(&mut watch, &names, &list, &[&seed, &launcher], true, &guard);
    let plan = poll_mixed(
        &mut watch,
        &names,
        &list,
        &[&seed, &launcher, &game],
        true,
        &guard,
    );
    assert_eq!(plan.handoff, None);
    assert_eq!(plan.skipped, vec!["game.exe".to_owned()]);
}

/// A name in both lists is decided by its own entry, so the plan's rule --
/// which is what the arming log reports -- says `Executable`.
#[test]
fn an_executable_registration_wins_over_a_folder_covering_it() {
    let names = strings(&["game.exe"]);
    let list = folders(&[r"D:\Games\Foo"]);
    let guard = open_guard();
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let game = proc("2", 11, r"D:\Games\Foo\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_mixed(&mut watch, &names, &list, &[&seed], false, &guard);
    let plan = poll_mixed(&mut watch, &names, &list, &[&seed, &game], false, &guard);
    assert_eq!(plan.armed.as_deref(), Some("game.exe"));
    assert_eq!(plan.rule, Some(AutoCaptureRule::Executable));
    // ...and it is decided once, not once per list.
    assert!(plan.skipped.is_empty());
}

/// Two rules covering one process are decided by the order they are stored
/// in, the same way two registered executables in one poll are.
#[test]
fn the_first_matching_folder_rule_is_the_one_reported() {
    let list = folders(&[r"D:\Games", r"D:\Games\Foo"]);
    let seed = proc("1", 10, r"C:\Users\me\editor.exe", true);
    let game = proc("2", 11, r"D:\Games\Foo\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    poll_folders(&mut watch, &list, &[&seed], false);
    let plan = poll_folders(&mut watch, &list, &[&seed, &game], false);
    assert_eq!(
        plan.rule,
        Some(AutoCaptureRule::Folder(r"D:\Games".to_owned()))
    );
}

/// The existing rules still hold for a folder arming: an app already
/// running when the rule arrives has no edge, and a process that dies
/// before it is capturable disarms.
#[test]
fn a_folder_arming_keeps_the_edge_rules() {
    let list = folders(&[r"D:\Games\Foo"]);
    let game = proc("2", 11, r"D:\Games\Foo\game.exe", true);
    let mut watch = AutoCaptureWatch::default();
    // Already up at the seed poll, and on every poll after it.
    for _ in 0..3 {
        assert_eq!(
            poll_folders(&mut watch, &list, &[&game], false),
            AutoCapturePlan::default()
        );
    }
    // A splash that dies before it is capturable leaves nothing armed.
    let splash = proc("3", 12, r"D:\Games\Foo\boot.exe", false);
    assert!(poll_folders(&mut watch, &list, &[&game, &splash], false)
        .armed
        .is_some());
    assert_eq!(
        poll_folders(&mut watch, &list, &[&game], false),
        AutoCapturePlan::default()
    );
}

#[test]
fn a_rearm_hold_drops_the_stop_and_recycles_a_launch_recording_once_replaced() {
    let mut hold = RearmHold::new(7, "App.exe", Some("session-a".to_owned()));
    assert_eq!(hold.executable, "app.exe");
    assert_eq!(hold.tick(false, false), RearmHoldOutcome::Waiting);
    assert_eq!(
        hold.tick(true, false),
        RearmHoldOutcome::Replaced {
            discard: Some("session-a".to_owned())
        }
    );
}

#[test]
fn a_rearm_hold_never_recycles_a_recording_that_was_itself_a_rearm() {
    let mut hold = RearmHold::new(7, "app.exe", None);
    assert_eq!(
        hold.tick(true, false),
        RearmHoldOutcome::Replaced { discard: None }
    );
}

#[test]
fn a_rearm_hold_releases_the_stop_when_the_rearm_is_abandoned() {
    let mut hold = RearmHold::new(7, "app.exe", Some("session-a".to_owned()));
    assert_eq!(hold.tick(false, true), RearmHoldOutcome::Released);
}

#[test]
fn a_rearm_hold_releases_the_stop_after_its_budget() {
    let mut hold = RearmHold::new(7, "app.exe", Some("session-a".to_owned()));
    for _ in 0..REARM_HOLD_POLLS {
        assert_eq!(hold.tick(false, false), RearmHoldOutcome::Waiting);
    }
    assert_eq!(hold.tick(false, false), RearmHoldOutcome::Released);
}

/// A process whose path cannot be read (an elevated one, or one that exited
/// between the walk and the open) is simply not matched -- never guessed at
/// by name.
#[test]
fn a_process_whose_path_will_not_resolve_arms_nothing() {
    let list = folders(&[r"D:\Games\Foo"]);
    let running: HashSet<String> = ["editor.exe".to_owned(), "game.exe".to_owned()]
        .into_iter()
        .collect();
    let mut pids: HashMap<String, Vec<u32>> = HashMap::new();
    pids.insert("editor.exe".to_owned(), vec![10]);
    pids.insert("game.exe".to_owned(), vec![11]);
    let guard = open_guard();
    let mut watch = AutoCaptureWatch::default();
    let seed: HashSet<String> = ["editor.exe".to_owned()].into_iter().collect();
    let poll = |watch: &mut AutoCaptureWatch, running: &HashSet<String>| {
        let inputs = AutoCaptureInputs {
            running,
            pids: &pids,
            resolve_path: &|_| None,
            guard: &guard,
        };
        watch.poll(&[], &list, &inputs, &[], false)
    };
    poll(&mut watch, &seed);
    assert_eq!(poll(&mut watch, &running), AutoCapturePlan::default());
}

/// One poll's worth of input to `AutoStartTracker` (t261101-566d): what the
/// picker reads, then the plan its `AutoCaptureWatch` returned.
#[derive(Default)]
struct TrackerPoll<'a> {
    running: &'a [(&'a str, &'a str)],
    /// `None` for a lifecycle that is not a stop; `Some((seq, code))`.
    stopped: Option<(u64, &'a str)>,
    plan: AutoCapturePlan,
    /// The (target id, window handle) `plan.start` resolves to.
    start_window: Option<(&'a str, &'a str)>,
}

/// Runs one poll the way `spawn_target_worker` does, with `app.exe`
/// registered and alive. Returns every action in order and `held`.
fn tracker_poll(tracker: &mut AutoStartTracker, poll: TrackerPoll) -> (Vec<Action>, bool) {
    let running: Vec<(String, String)> = poll
        .running
        .iter()
        .map(|(session, handle)| ((*session).to_owned(), (*handle).to_owned()))
        .collect();
    let processes = RunningProcesses {
        names: ["app.exe".to_owned()].into_iter().collect(),
        pids: HashMap::new(),
    };
    let registered = apps(&["app.exe"]);
    let (state, seq, code) = match poll.stopped {
        Some((seq, code)) => ("stopped", seq, Some(code)),
        None => ("recording", 0, None),
    };
    let mut actions = tracker.before_plan(&PollSeen {
        running: &running,
        state,
        seq,
        diagnostic_code: code,
        processes: Some(&processes),
        registered: &registered,
        stopping: false,
    });
    let start_target = poll
        .start_window
        .map(|(id, handle)| (id.to_owned(), handle.to_owned(), "app.exe".to_owned()));
    let (after, held) = tracker.after_plan(&poll.plan, start_target, false, Some(&processes));
    actions.extend(after);
    (actions, held)
}

fn start_plan(id: &str, handed_over: bool) -> AutoCapturePlan {
    AutoCapturePlan {
        start: Some(id.to_owned()),
        handed_over,
        ..AutoCapturePlan::default()
    }
}

/// The first two polls every scenario below shares: a launch edge starts
/// `win-a`, and the next poll sees it running as `s-a`.
fn auto_started(tracker: &mut AutoStartTracker) {
    tracker_poll(
        tracker,
        TrackerPoll {
            plan: start_plan("win-a", false),
            start_window: Some(("win-a", "h-a")),
            ..TrackerPoll::default()
        },
    );
    tracker_poll(
        tracker,
        TrackerPoll {
            running: &[("s-a", "h-a")],
            ..TrackerPoll::default()
        },
    );
}

/// task2550 + t260928-5cb2 on a real application: a launch edge starts the
/// window, the window closes under the live process, the rearm is applied at
/// once (nothing is draining), the stop is held, and the rearm's recording
/// swallows it and recycles the launch's first window.
#[test]
fn a_launch_window_closing_rearms_and_the_replacement_swallows_its_stop() {
    let mut tracker = AutoStartTracker::default();
    let (actions, held) = tracker_poll(
        &mut tracker,
        TrackerPoll {
            plan: start_plan("win-a", false),
            start_window: Some(("win-a", "h-a")),
            ..TrackerPoll::default()
        },
    );
    assert_eq!(
        actions,
        vec![Action::StartCapture {
            id: "win-a".to_owned(),
            handed_over: false
        }]
    );
    assert!(!held);
    let (actions, held) = tracker_poll(
        &mut tracker,
        TrackerPoll {
            running: &[("s-a", "h-a")],
            ..TrackerPoll::default()
        },
    );
    assert_eq!((actions, held), (vec![], false));
    // The bound window closed: CAP-TGT-001 with the process alive.
    let (actions, held) = tracker_poll(
        &mut tracker,
        TrackerPoll {
            stopped: Some((1, "CAP-TGT-001")),
            ..TrackerPoll::default()
        },
    );
    assert_eq!(
        actions,
        vec![Action::ForgetHandedOver("app.exe".to_owned())]
    );
    assert!(held, "the stop waits on its rearm");
    // The watch reads the forgotten name as a fresh launch.
    let (actions, held) = tracker_poll(
        &mut tracker,
        TrackerPoll {
            stopped: Some((1, "CAP-TGT-001")),
            plan: start_plan("win-b", true),
            start_window: Some(("win-b", "h-b")),
            ..TrackerPoll::default()
        },
    );
    assert_eq!(
        actions,
        vec![Action::StartCapture {
            id: "win-b".to_owned(),
            handed_over: true
        }]
    );
    assert!(held);
    let (actions, held) = tracker_poll(
        &mut tracker,
        TrackerPoll {
            running: &[("s-b", "h-b")],
            stopped: Some((1, "CAP-TGT-001")),
            ..TrackerPoll::default()
        },
    );
    assert_eq!(
        actions,
        vec![Action::RecycleSession {
            session: "s-a".to_owned(),
            executable: "app.exe".to_owned()
        }]
    );
    assert!(held, "swallowed, never reported");
}

/// task2550: a recording the user started from a tile is not this feature's,
/// so its window closing under a registered, live process rearms nothing and
/// holds nothing. The auto-started control in the same test does both.
#[test]
fn a_manually_started_recording_never_rearms() {
    let closed = |tracker: &mut AutoStartTracker| {
        tracker_poll(
            tracker,
            TrackerPoll {
                stopped: Some((1, "CAP-TGT-001")),
                ..TrackerPoll::default()
            },
        )
    };
    let mut manual = AutoStartTracker::default();
    tracker_poll(
        &mut manual,
        TrackerPoll {
            running: &[("s-a", "h-a")],
            ..TrackerPoll::default()
        },
    );
    assert_eq!(closed(&mut manual), (vec![], false));

    let mut auto = AutoStartTracker::default();
    auto_started(&mut auto);
    assert_eq!(
        closed(&mut auto),
        (vec![Action::ForgetHandedOver("app.exe".to_owned())], true)
    );
}

/// t260928-5cb2: the controller keeps re-reading the swallowed stop until the
/// next event, and each re-read stays held -- but the next stop, one `seq`
/// later, is a stop of its own and is reported. The rearm's recording was not
/// a launch's first window, so closing it recycles nothing.
#[test]
fn a_swallowed_stop_stays_unreported_and_the_next_one_is_not_swallowed() {
    let mut tracker = AutoStartTracker::default();
    auto_started(&mut tracker);
    let polls = [
        TrackerPoll {
            stopped: Some((1, "CAP-TGT-001")),
            ..TrackerPoll::default()
        },
        TrackerPoll {
            stopped: Some((1, "CAP-TGT-001")),
            plan: start_plan("win-b", true),
            start_window: Some(("win-b", "h-b")),
            ..TrackerPoll::default()
        },
        TrackerPoll {
            running: &[("s-b", "h-b")],
            stopped: Some((1, "CAP-TGT-001")),
            ..TrackerPoll::default()
        },
    ];
    for poll in polls {
        tracker_poll(&mut tracker, poll);
    }
    for _ in 0..3 {
        let (actions, held) = tracker_poll(
            &mut tracker,
            TrackerPoll {
                running: &[("s-b", "h-b")],
                stopped: Some((1, "CAP-TGT-001")),
                ..TrackerPoll::default()
            },
        );
        assert_eq!((actions, held), (vec![], true));
    }
    // The rearm's window closes too: a new stop, seq 2. It rearms again, but
    // its hold carries no session to recycle.
    let (actions, held) = tracker_poll(
        &mut tracker,
        TrackerPoll {
            stopped: Some((2, "CAP-TGT-001")),
            ..TrackerPoll::default()
        },
    );
    assert_eq!(
        actions,
        vec![Action::ForgetHandedOver("app.exe".to_owned())]
    );
    assert!(held);
    tracker_poll(
        &mut tracker,
        TrackerPoll {
            stopped: Some((2, "CAP-TGT-001")),
            plan: start_plan("win-c", true),
            start_window: Some(("win-c", "h-c")),
            ..TrackerPoll::default()
        },
    );
    let (actions, held) = tracker_poll(
        &mut tracker,
        TrackerPoll {
            running: &[("s-c", "h-c")],
            stopped: Some((2, "CAP-TGT-001")),
            ..TrackerPoll::default()
        },
    );
    assert_eq!((actions, held), (vec![], true));
    // The user stops that one by hand: seq 3 is reported.
    let (actions, held) = tracker_poll(
        &mut tracker,
        TrackerPoll {
            stopped: Some((3, "CAP-EXIT-001")),
            ..TrackerPoll::default()
        },
    );
    assert_eq!((actions, held), (vec![], false));
}

/// ユーザー裁定c (task3600): a newcomer under the running auto-recording's
/// folder stops that session and rearms on the newcomer once it drains; the
/// same plan over a manual recording stops nothing.
#[test]
fn a_handoff_stops_only_an_auto_started_session() {
    let handoff = AutoCapturePlan {
        handoff: Some("app.exe".to_owned()),
        ..AutoCapturePlan::default()
    };
    let mut manual = AutoStartTracker::default();
    let (actions, _) = tracker_poll(
        &mut manual,
        TrackerPoll {
            running: &[("s-a", "h-a")],
            plan: handoff.clone(),
            ..TrackerPoll::default()
        },
    );
    assert_eq!(actions, vec![]);

    let mut auto = AutoStartTracker::default();
    auto_started(&mut auto);
    let (actions, _) = tracker_poll(
        &mut auto,
        TrackerPoll {
            running: &[("s-a", "h-a")],
            plan: handoff,
            ..TrackerPoll::default()
        },
    );
    assert_eq!(actions, vec![Action::StopSession("s-a".to_owned())]);
    let (actions, _) = tracker_poll(
        &mut auto,
        TrackerPoll {
            stopped: Some((1, "CAP-EXIT-001")),
            ..TrackerPoll::default()
        },
    );
    assert_eq!(
        actions,
        vec![Action::ForgetHandedOver("app.exe".to_owned())]
    );
}
