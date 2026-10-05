//! Deciding when a registered executable's launch starts a recording
//! (task1970).
//!
//! Pure so the trigger source stays replaceable: the caller is the picker's
//! existing 1s poll (`spawn_target_worker`), which hands in both the window
//! list it was reading anyway and a snapshot of the running processes. If
//! polling ever has to become `SetWinEventHook`, only the call site changes.
//!
//! The launch edge is read from *process liveness*, not from that window list
//! (task2450). A window can leave the picker's enumeration while its process
//! keeps running -- a minimized Store-app frame reports a 0x0 client rect and
//! is filtered out -- and reading that as an exit made merely restoring the
//! window look like a launch and started a recording. The window list still
//! decides *when* to fire: a live process with nothing capturable yet is a
//! splash screen.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::capture::targets::{CaptureTarget, CaptureTargetKind};
use crate::settings::{AutoCaptureApp, AutoCaptureFolder};
use crate::ui_state::locale::Locale;

crate::tr! {
    /// Shown when a launch started a recording without being asked to. The
    /// game is the foreground window at that moment, so in practice this is
    /// read on an OS toast rather than in the app (task1550's split).
    started_title { ja: "録画を自動で開始しました", en: "Recording started automatically" }
    /// The same toast for a start that follows on from a recording this
    /// feature ended itself: a handoff (round21 §4-1, task3840), where the
    /// launcher's recording was stopped and the game it started is recorded
    /// instead, or a rearm (t260928-5cb2), where the recorded window closed and
    /// the same process opened another. Either way "started automatically"
    /// would read as a second, unrelated recording. The first toast -- the one
    /// the launch itself produced -- is deliberately left alone: a launch that
    /// never gets further would otherwise begin recording with no notice.
    handed_over_title { ja: "録画を引き継ぎました", en: "Recording handed over" }
    /// The settings row that lists what is registered (task1980). Executables
    /// are still registered from the review panel's toggle -- a registration
    /// names the recorded app, which is what that screen knows. Folder rules
    /// (task3600) have no recording to name, so since task3610 this row also
    /// carries the one chooser that can make them. Since round21 §2-2
    /// (task3830) it says 「対象」 rather than 「アプリ」: half the list is
    /// folder rules, and a label naming only apps mis-describes it.
    /// t260927-fde8: the settings group's heading and index link.
    settings_label { ja: "自動録画", en: "Auto-record" }
    /// Under the label, in both states -- the row makes folder rules but not
    /// executable registrations, so it still has to say where the latter are
    /// made (round13 §1, revised by task3610; wording round21 §2-4).
    settings_sub {
        ja: "ここにあるアプリが起動すると、録画を自動で始めます。アプリは確認画面から、フォルダはここから追加します。",
        en: "Apps listed here start a recording when they launch. Add apps from review, folders from here."
    }
    /// The whole list when nothing is registered -- same sentence as the sub
    /// line, said as a state. It names the button by its label rather than by
    /// where it sits (round21 §2-4): the chooser is at the right end of the
    /// label line, not 「右上」.
    settings_empty {
        ja: "まだありません",
        en: "None yet"
    }
    /// The button at the right end of that row's label (task3610). The only
    /// way to make a folder rule: a folder has no recording behind it, so the
    /// review panel's toggle -- which names an executable -- cannot make one.
    settings_add_folder { ja: "フォルダを追加…", en: "Add a folder…" }
    /// t260927-fde8: a folder rule's second line.
    settings_folder_all { ja: "このフォルダの中のアプリすべて", en: "Every app inside this folder" }
    /// Refusing a picked folder that `folder_rule_allowed` turns down
    /// (task3610): a volume root, `%SystemRoot%` and everything under it, or
    /// `%ProgramFiles%` itself. Says which folders cannot be targets rather
    /// than only that this one failed -- the rule the guard enforces is not
    /// visible from the picker.
    ///
    /// **`%ProgramFiles%` itself is deliberately left out of the sentence**
    /// (round21 §2-5, task3830). Naming it would read as "nothing under
    /// Program Files can be a target", which is false -- a subfolder of it is
    /// an ordinary rule -- and would put people off the commonest place games
    /// install to. The guard is unchanged; only the wording narrowed.
    settings_folder_rejected {
        ja: "このフォルダは登録できません。ドライブ直下や Windows のシステムフォルダは対象にできません。",
        en: "That folder can't be registered. Drive roots and Windows system folders can't be targets."
    }
    /// The same guard, said about a rule that is already in `settings.json`
    /// (task3730): hand-writing one the picker would have refused puts a row on
    /// the settings list that `poll` silently ignores, and nothing on that row
    /// said so. The shape (state, then the fact in parentheses) is round21 §3's
    /// 4-2 for read-only fact lines.
    ///
    /// **This line and `settings_folder_rejected` state the guard differently,
    /// on purpose** (task3830). Until round21 the two shared a reason clause
    /// byte for byte; §2-5 then narrowed the toast to the two cases worth
    /// warning about up front and dropped `%ProgramFiles%` itself. On a row
    /// that borrowed sentence would be a lie: a hand-written
    /// `C:\Program Files` rule is neither a drive root nor a Windows system
    /// folder, yet it is exactly the row this line has to explain. 「配下の
    /// プロセスが多すぎます」 is true of all three cases the guard refuses, so
    /// the row keeps it. `the_two_guard_lines_say_the_guard_their_own_way`
    /// pins the divergence so neither drifts back onto the other by accident.
    ///
    /// **Settled, do not re-raise (2026-09-09).** Aligning this row to
    /// round21's narrowed vocabulary was proposed and *decided against* the
    /// same day: the user first chose to align it, was shown the paragraph
    /// above, and reversed to "keep the wording and write down why". So a
    /// later reader finding the two lines out of step is looking at the
    /// decision, not at drift -- 「ドライブ直下や Windows のシステムフォルダ」
    /// is simply false of a hand-written `C:\Program Files` rule, which is one
    /// of the three rows this line exists for. task3730's own record carries
    /// the same note.
    settings_folder_ignored {
        ja: "無視されています（配下のプロセスが多すぎます）",
        en: "Ignored: too many processes run from under it."
    }
    /// The × on each row of that list (task2410). Names the registration it
    /// takes away, not the recording: the row is a standing setting, and
    /// nothing is running when it is pressed.
    settings_remove_hint { ja: "自動録画から外す", en: "Remove from auto-record" }
    /// The picker tile's badge while the app is an auto-record target
    /// (round13 §1). State only -- the badge is not a control; registering
    /// happens on the review panel and unregistering also on the settings row.
    /// Since task3670 the state it reports is `covered_by`, so a process that
    /// qualifies only through a folder rule (task3600) wears the badge too --
    /// the badge says "this will record itself", not "this name is on a list".
    /// Which is also why a rule the guard turns down leaves the badge off
    /// (task3690): `poll` would not record that launch, so the badge would be
    /// promising something the feature never does.
    tile_registered { ja: "自動録画", en: "Auto-record" }
    /// The review panel's toggle row (round13 §1) -- the one place a
    /// registration is made, now that the picker tile only reports state.
    /// No tooltip (task2720): the sub line under it already says what staying
    /// on means, and a hover-only repeat of it was clipped anyway.
    ///
    /// t260927-7d08: the DS ReviewScreen's wording, on the panel's floor.
    review_toggle_label { ja: "起動したら自動で録画", en: "Record when it launches" }
}

/// Whether `executable` is already registered, compared the way `poll` matches
/// it: Windows filenames are case-insensitive, and the list can have been
/// hand-edited in `settings.json`. Only `name` decides -- the metadata task2560
/// added to an entry is display-only.
pub fn is_registered(registered: &[AutoCaptureApp], executable: &str) -> bool {
    let executable = executable.trim();
    !executable.is_empty()
        && registered
            .iter()
            .any(|entry| entry.name.trim().eq_ignore_ascii_case(executable))
}

/// Drops every entry naming `executable`, in any spelling -- a hand-edited file
/// can hold both `Game.exe` and `game.exe`, and removing one of them from the
/// settings row would leave the feature still armed.
pub fn unregister(registered: &[AutoCaptureApp], executable: &str) -> Vec<AutoCaptureApp> {
    let executable = executable.trim();
    registered
        .iter()
        .filter(|entry| !entry.name.trim().eq_ignore_ascii_case(executable))
        .cloned()
        .collect()
}

/// What the review panel's toggle row does: register if it is not, unregister
/// if it is. The caller hands in the whole entry -- name plus whatever display
/// metadata it resolved (task2560) -- and only the name decides which way the
/// toggle goes.
///
/// New entries go on the end and removal keeps the rest in place, because the
/// stored order is load-bearing -- `poll` breaks a two-launches-in-one-poll tie
/// by it. The name is stored as it is spelled on disk rather than lowercased:
/// matching folds case anyway, and the settings row is read by a person.
pub fn toggle(registered: &[AutoCaptureApp], entry: AutoCaptureApp) -> Vec<AutoCaptureApp> {
    if is_registered(registered, &entry.name) {
        return unregister(registered, &entry.name);
    }
    let name = entry.name.trim();
    let mut next = registered.to_vec();
    if !name.is_empty() {
        next.push(AutoCaptureApp {
            name: name.to_owned(),
            ..entry
        });
    }
    next
}

/// The body beside `started_title`, naming the executable as it is spelled on
/// disk rather than the lowercased id the matching uses.
pub fn started_body(locale: Locale, executable: &str) -> String {
    match locale {
        Locale::Ja => format!("{executable} の起動を検知しました。"),
        Locale::En => format!("Detected {executable} starting."),
    }
}

/// The body beside `handed_over_title` (round21 §4-1, task3840): the name of
/// the executable the recording moved *to*, which is the one thing the second
/// toast has to say that the first one did not. For a rearm (t260928-5cb2) it
/// is the same executable, which a bare name still reads right for.
///
/// Deliberately just the name rather than a sentence: the title already says
/// what happened, and the two toasts are read seconds apart.
pub fn handed_over_body(executable: &str) -> String {
    executable.trim().to_owned()
}

/// What the review panel's toggle row draws (round13 §1). Derived rather than
/// stored: the on/off is "is this session's executable in the list", and the
/// list is edited from the settings screen too.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewToggle {
    /// The row is on screen at all. False when the session names no executable
    /// -- a monitor recording, or one made before task2350 started writing the
    /// name into the container. Nothing to register, so nothing to offer.
    pub visible: bool,
    /// Whether *this executable* is on the registered list -- and nothing else
    /// (round21 §4-2, task3840). A folder rule covering the process no longer
    /// reads on here, though it still lights the picker's badge: the switch
    /// edits the executable list, so a folder rule showing as on would leave
    /// the row unable to be turned off.
    pub checked: bool,
    /// `mspaint.exe` (the executable alone, t260926-97dd), or -- when only a folder rule
    /// covers it (task3670, wording round21 §4-2) -- the read-only sentence
    /// saying a folder rule is why, and what switching the row on would add.
    /// A rule the guard turns down never gets named (task3690): nothing arms,
    /// so the line falls back to the executable sentence. Empty when `visible`
    /// is false.
    pub body: String,
    /// The executable as the row would register it, i.e. trimmed. Empty when
    /// `visible` is false. Stashed by the caller so the row can be re-derived
    /// when the list changes elsewhere, without re-reading the session.
    pub executable: String,
}

/// `executable` is the recorded target's name (`TimelineSnapshot::
/// target_executable`). `image_path` is that process's full image path when the
/// caller could resolve one, which is what lets a session covered only by a
/// folder rule say so in its second line (task3670); `None` answers from the
/// executable list alone. `guard` is handed straight to `covered_by`, so a rule
/// `poll` refuses is never named here either (task3690).
///
/// Round21 §4-2 (task3840) split the two halves of that answer: the *switch*
/// reads the executable list and only it, while the *line under it* reads the
/// whole of `covered_by`. Before, `checked` was `rule.is_some()`, so a process
/// covered only by a folder rule drew an on switch that switching off did
/// nothing to -- the display and the result of pressing it disagreed. The
/// picker's badge keeps the wider reading on purpose: its claim is "launching
/// this records it", which a folder rule does satisfy.
///
/// `visible` is unchanged by all of that: a session naming no executable still
/// has no row, whatever the folder rules say -- or whatever the guard says
/// about them.
pub fn review_toggle(
    locale: Locale,
    executable: Option<&str>,
    registered: &[AutoCaptureApp],
    folders: &[AutoCaptureFolder],
    guard: &FolderGuard,
    image_path: Option<&Path>,
) -> ReviewToggle {
    let executable = executable.unwrap_or_default().trim();
    if executable.is_empty() {
        return ReviewToggle::default();
    }
    let rule = covered_by(registered, folders, guard, executable, image_path);
    ReviewToggle {
        visible: true,
        checked: matches!(rule, Some(AutoCaptureRule::Executable)),
        body: review_toggle_body(locale, executable, rule.as_ref()),
        executable: executable.to_owned(),
    }
}

/// The row's second line, naming the executable as it is spelled on disk --
/// or, when a folder rule is the only thing covering it (task3670), saying so
/// instead. With the switch reading off in that case (round21 §4-2, task3840),
/// this line is the whole of what tells the user the app *is* an auto-record
/// target already; without it an off switch beside an app that records itself
/// on launch would be a plain contradiction. It also says what switching the
/// row on adds, since that is the one thing the switch's position cannot.
///
/// Only rules `covered_by` accepted reach here, so a rule the guard turns down
/// (task3690) arrives as `None` and takes the executable sentence -- nothing
/// arms, and reporting a rule that changes nothing would be the same lie the
/// badge was telling.
///
/// The rule is no longer named (round21 §4-2 fixed the wording without it):
/// the line says *that* a folder rule covers the app, and the settings screen
/// is where the rules themselves are read. That is also why this stopped
/// eliding a path -- there is no path in it any more.
pub fn review_toggle_body(
    locale: Locale,
    executable: &str,
    rule: Option<&AutoCaptureRule>,
) -> String {
    match rule {
        Some(AutoCaptureRule::Folder(_)) => match locale {
            Locale::Ja => {
                "フォルダの規則で自動録画の対象です。オンにすると、このアプリだけでも登録します。"
                    .to_owned()
            }
            Locale::En => {
                "Covered by a folder rule. Turning this on also registers this app on its own."
                    .to_owned()
            }
        },
        // t260927-7d08: the DS ReviewScreen's sentence, which is newer than
        // t260926-97dd's bare executable name.
        _ => match locale {
            // Ends in 「。」 like the folder rule's (t260928-bae5, review 07 F19).
            Locale::Ja => format!("{executable} が起動すると録画を始めます。"),
            Locale::En => format!("Starts recording when {executable} launches."),
        },
    }
}

/// The two runs a folder rule draws on the settings list (round21 §2-3,
/// task3830): `(tail, parent)` -- the last folder's name, which is what
/// identifies the rule, and the path leading to it. The row gives the tail the
/// weight the exe row gives a display name and the parent the muted role it
/// gives the exe name, so the half that names the rule is the half that stays
/// readable when the row runs out of width.
///
/// The parent keeps its trailing separator, matching the mock
/// (`D:\Games` reads as `Games` + `D:\`). It is sliced out of `rule` rather
/// than rebuilt, so the rule's own spelling survives -- a rule written
/// `d:/games/foo` keeps its forward slashes instead of being handed back half
/// converted.
///
/// A trailing separator is dropped before the split, the same normalisation
/// `folder_matches` gets from `Path::components`: `D:\Games\Foo\` and
/// `D:\Games\Foo` are one rule and must draw as one row. A rule with no folder
/// above it -- a volume root, or a `\` on its own -- has no parent to show, so
/// the whole rule stays in the tail and the parent is empty. `poll` ignores
/// those anyway (`folder_rule_ignored`); they reach the list only by
/// hand-editing `settings.json`.
pub fn folder_row_parts(rule: &str) -> (String, String) {
    let rule = rule.trim();
    let body = rule.trim_end_matches(['\\', '/']);
    match body.rfind(['\\', '/']) {
        // `body` is a prefix of `rule` and the separator is one ASCII byte, so
        // both slices land on char boundaries.
        Some(cut) if cut + 1 < body.len() => (body[cut + 1..].to_owned(), rule[..=cut].to_owned()),
        _ => (rule.to_owned(), String::new()),
    }
}

/// Whether `image_path` sits under the folder `rule` names (task3600).
///
/// Component-wise rather than a string prefix, so `D:\Games\Foo` does not match
/// `D:\Games\FooBar\x.exe`. Case folds ASCII-only, the way every other filename
/// comparison in this module does: Windows filenames fold case, and both sides
/// come off the same filesystem.
///
/// `Path::components` normalises a trailing separator away and yields the drive
/// letter or UNC share as one `Prefix` component, so `D:\Games\Foo\`,
/// `d:\games\foo` and `D:/Games/Foo` are one rule. Nothing is expanded or
/// resolved -- environment variables and junctions are out of scope -- so a
/// rule spelled `%ProgramFiles%\Foo` is simply a rule that matches nothing.
pub fn folder_matches(rule: &str, image_path: &Path) -> bool {
    let rule = rule.trim();
    if rule.is_empty() {
        return false;
    }
    let mut actual = image_path.components();
    for wanted in Path::new(rule).components() {
        match actual.next() {
            Some(have) if same_component(&wanted, &have) => {}
            _ => return false,
        }
    }
    true
}

fn same_component(left: &std::path::Component, right: &std::path::Component) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

/// Whether two folder rules name the same folder, compared the way
/// `folder_matches` compares: both directions, component-wise, case folded.
fn same_folder(left: &Path, right: &Path) -> bool {
    let left: Vec<_> = left.components().collect();
    let right: Vec<_> = right.components().collect();
    left.len() == right.len()
        && left
            .iter()
            .zip(&right)
            .all(|(one, other)| same_component(one, other))
}

/// Whether two rule strings are the same rule, for matching a plan's
/// `AutoCaptureRule::Folder` back to the entry it came from.
pub fn same_rule(left: &str, right: &str) -> bool {
    same_folder(Path::new(left.trim()), Path::new(right.trim()))
}

/// The folders a rule may not name (ユーザー裁定3, task3600).
///
/// Injected rather than read from the environment inside `poll`, so the
/// decision is a pure function with fixed tests instead of one that changes
/// with the machine it runs on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderGuard {
    /// Rejected together with everything under them -- `%SystemRoot%`. A rule
    /// anywhere inside Windows would arm on service hosts and shell processes,
    /// i.e. record the user unasked.
    pub forbidden_trees: Vec<PathBuf>,
    /// Rejected only when named exactly -- `%ProgramFiles%` and its x86 twin.
    /// Their subfolders stay allowed: that is where games install.
    pub forbidden_exact: Vec<PathBuf>,
}

impl FolderGuard {
    /// The real machine's folders. The fallbacks are the standard paths, used
    /// when a variable is missing or empty -- a guard that silently forgot
    /// about Windows would be worse than one that guesses right.
    pub fn from_env() -> Self {
        fn dir(key: &str, fallback: &str) -> PathBuf {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map_or_else(|| PathBuf::from(fallback), PathBuf::from)
        }
        Self {
            forbidden_trees: vec![dir("SystemRoot", r"C:\Windows")],
            forbidden_exact: vec![
                dir("ProgramFiles", r"C:\Program Files"),
                dir("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ],
        }
    }

    /// The one `from_env` the display sites share, built on first use and kept
    /// for the life of the process.
    ///
    /// Same invariant `picker.rs` builds its poll-side guard on: the machine's
    /// Windows and Program Files directories do not move while the app runs,
    /// and `src/` never calls `std::env::set_var` / `remove_var`, so nothing
    /// can make the cached answer stale. Caching matters because the picker
    /// asks per tile per render, which is no place for an environment lookup.
    ///
    /// The pure functions still take `&FolderGuard` (see the type's doc) --
    /// this is only how the binary gets the one it passes them, so the tests
    /// keep their own fixed guards.
    pub fn machine() -> &'static Self {
        static MACHINE: std::sync::OnceLock<FolderGuard> = std::sync::OnceLock::new();
        MACHINE.get_or_init(Self::from_env)
    }
}

/// Whether a hand-written folder rule is one this feature will act on.
///
/// Turned down: a bare volume root (`C:\`, or a UNC share with nothing under
/// it), `%SystemRoot%` and everything below it, and `%ProgramFiles%` /
/// `%ProgramFiles(x86)%` themselves -- but not their subfolders. Anything wider
/// than a folder is a rule to record whatever the machine happens to run.
///
/// task3610 makes this the picker's refusal; here it is what makes `poll`
/// ignore a rule that was typed into `settings.json` anyway, and say so once.
pub fn folder_rule_allowed_with(rule: &str, guard: &FolderGuard) -> bool {
    let rule = rule.trim();
    if rule.is_empty() {
        return false;
    }
    let path = Path::new(rule);
    // No named folder inside the volume means the rule *is* the volume.
    if !path
        .components()
        .any(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }
    !guard
        .forbidden_trees
        .iter()
        .any(|tree| folder_matches(&tree.to_string_lossy(), path))
        && !guard
            .forbidden_exact
            .iter()
            .any(|exact| same_folder(exact, path))
}

/// Whether the settings list should say this rule is being ignored
/// (task3730) -- the same question `folder_rule_allowed_with` answers, asked
/// from the display's side.
///
/// A thin skin rather than a second derivation: the guard's contents stay the
/// one decision in `folder_rule_allowed_with`, and this only spares every call
/// site the negation (task3690 turned down spreading the judgement itself).
/// Folder rules only -- an executable registration has no guard, so its row
/// never carries the line.
pub fn folder_rule_ignored(rule: &str, guard: &FolderGuard) -> bool {
    !folder_rule_allowed_with(rule, guard)
}

/// `folder_rule_allowed_with` against the machine's own folders.
pub fn folder_rule_allowed(rule: &str) -> bool {
    folder_rule_allowed_with(rule, FolderGuard::machine())
}

/// Whether `path` is already a rule, compared the way `folder_matches`
/// compares -- `D:\Games\Foo\`, `d:/games/foo` and `D:\Games\Foo` are one rule,
/// so picking a folder twice cannot put it on the list twice.
pub fn folder_registered(folders: &[AutoCaptureFolder], path: &str) -> bool {
    let path = path.trim();
    !path.is_empty() && folders.iter().any(|folder| same_rule(&folder.path, path))
}

/// What the settings screen's 「フォルダを追加」 does (task3610): append the
/// picked folder unless it is already a rule.
///
/// New rules go on the end and the rest keep their places, for the same reason
/// `toggle` keeps executable order: the stored order is what the list draws.
/// The path is stored exactly as the picker spelled it -- matching folds case
/// and normalises separators anyway, and this row is read by a person.
///
/// Deliberately silent on a duplicate rather than reporting one: the rule the
/// user asked for is on the list either way, which is what the screen shows.
/// The guard (`folder_rule_allowed`) is the caller's, not this function's --
/// refusing needs a message, and the message is the caller's toast.
pub fn add_folder(folders: &[AutoCaptureFolder], path: &str) -> Vec<AutoCaptureFolder> {
    let path = path.trim();
    let mut next = folders.to_vec();
    if !path.is_empty() && !folder_registered(folders, path) {
        next.push(AutoCaptureFolder::at(path));
    }
    next
}

/// Drops every rule naming the same folder as `path`, in any spelling -- a
/// hand-edited file can hold `D:\Games\Foo` and `d:/games/foo/`, and removing
/// one of them from the settings row would leave the feature still armed. Same
/// reasoning as `unregister` for executables.
pub fn unregister_folder(folders: &[AutoCaptureFolder], path: &str) -> Vec<AutoCaptureFolder> {
    let path = path.trim();
    folders
        .iter()
        .filter(|folder| !same_rule(&folder.path, path))
        .cloned()
        .collect()
}

/// Why an arming happened, carried on the plan so the caller never has to
/// re-derive it from the name (task3600): a name armed by a folder rule is not
/// in `auto_capture_executables` at all, so the name alone cannot say it.
///
/// The same answer the display sites read (task3670): `covered_by` returns this
/// for the picker tile's badge and the review panel's toggle, so "why is this
/// an auto-record target" is one enum and one function rather than a second
/// derivation that could drift from `poll`'s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoCaptureRule {
    /// Registered by executable name (task1970).
    Executable,
    /// Covered by this folder rule, spelled as the settings file spells it.
    Folder(String),
}

/// Why `executable` is an auto-record target at all -- `None` when it is not
/// (task3670).
///
/// The one derivation behind both display sites: the picker tile's badge and
/// the review panel's toggle. Both used to read `is_registered`, i.e. the
/// executable list alone, so a process qualifying only through a folder rule
/// (task3600) was drawn as unregistered while `poll` was happily recording it.
///
/// The executable list wins over a folder rule that also covers the process,
/// the same order `poll` arms
/// in: a name registered on its own is the more specific of the two statements.
///
/// `image_path` is the running process's full image path. `None` -- nothing by
/// that name is running, so there is no path to match -- answers from the
/// executable list alone, which is what this was before task3670. The name
/// decides only that half: a folder rule is matched on the path the way
/// `poll`'s `matching_folder` matches it, and only after `guard` has thrown out
/// the rules `poll` refuses to arm on (task3690).
///
/// That second half is why `guard` is here at all. `poll` acts on
/// `active_folders`, i.e. the rules `folder_rule_allowed_with` lets through;
/// reading the raw list here made a rule typed straight into `settings.json`
/// -- `C:\Windows`, a volume root -- light the picker badge and the review
/// toggle for a launch `poll` would never record. The display said yes and the
/// feature said no.
///
/// Filter before the match, never after: a refused rule sitting *ahead* of an
/// allowed one that covers the same path must not swallow the answer. Finding
/// first and checking the guard afterwards would return `None` there, which is
/// the opposite of what `poll` does.
///
/// The guard is injected rather than read from the environment for the reason
/// `FolderGuard`'s own doc gives; the binary passes `FolderGuard::machine()`.
pub fn covered_by(
    registered: &[AutoCaptureApp],
    folders: &[AutoCaptureFolder],
    guard: &FolderGuard,
    executable: &str,
    image_path: Option<&Path>,
) -> Option<AutoCaptureRule> {
    if is_registered(registered, executable) {
        return Some(AutoCaptureRule::Executable);
    }
    let image_path = image_path?;
    // Registration order decides when two rules both cover it, matching
    // `matching_folder`. The rule is reported as the settings file spells it.
    folders
        .iter()
        .filter(|folder| folder_rule_allowed_with(&folder.path, guard))
        .find(|folder| folder_matches(&folder.path, image_path))
        .map(|folder| AutoCaptureRule::Folder(folder.path.clone()))
}

/// The process side of one poll, plus the guard the folder rules are read
/// through. Bundled because `poll` would otherwise sit over clippy's argument
/// budget, and because these four always travel together.
pub struct AutoCaptureInputs<'a> {
    /// Every live process's executable name, lowercased the way `executable_id`
    /// is (`capture::targets::running_processes`).
    pub running: &'a HashSet<String>,
    /// Lowercased name -> the process ids behind it, from that same walk.
    /// Read only for names that are new this poll.
    pub pids: &'a HashMap<String, Vec<u32>>,
    /// Full image path for a process id. Called only for names that are new
    /// this poll and only while a folder rule is registered, which is what
    /// keeps the per-second cost where task1970 left it.
    pub resolve_path: &'a dyn Fn(u32) -> Option<PathBuf>,
    /// Folders a rule may not name.
    pub guard: &'a FolderGuard,
}

/// The first registered folder covering any process behind `name`. Registration
/// order decides when two rules both cover it, matching how two registered
/// executables in one poll are decided.
fn matching_folder<'a>(
    folders: &[&'a AutoCaptureFolder],
    name: &str,
    inputs: &AutoCaptureInputs<'_>,
) -> Option<&'a AutoCaptureFolder> {
    inputs.pids.get(name)?.iter().find_map(|process_id| {
        let path = (inputs.resolve_path)(*process_id)?;
        folders
            .iter()
            .copied()
            .find(|folder| folder_matches(&folder.path, &path))
    })
}

/// What one poll decided. Everything but `start` and `handoff` exists for the
/// log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoCapturePlan {
    /// A registered executable appeared this poll and is now waiting for a
    /// window worth capturing. Lowercased, as the ids are.
    pub armed: Option<String>,
    /// Registered executables that appeared but were passed over: another one
    /// was already armed, or a recording is running.
    pub skipped: Vec<String>,
    /// The target id to start, i.e. the same string a picker tile click
    /// carries. Repeats every poll until the recording is running or the
    /// retry budget runs out.
    pub start: Option<String>,
    /// The arming was dropped because `MAX_START_ATTEMPTS` starts in a row all
    /// left nothing recording. For the log: the start is refusing for a reason
    /// the retry cannot fix.
    pub gave_up: Option<String>,
    /// Which rule `armed` / `start` came from (task3600). `None` when neither
    /// fired this poll -- a bare retry poll says nothing new, so it stays
    /// `default()` for the tests that assert on that.
    pub rule: Option<AutoCaptureRule>,
    /// A process that appeared under the *running* auto-recording's own folder
    /// rule: the launcher handed over to the game it started (ユーザー裁定c).
    /// The caller stops that recording and rearms on this name -- and only when
    /// the recording really is one this feature started, which is the half of
    /// "never stop a manual recording" the watch cannot see.
    pub handoff: Option<String>,
    /// This `start` is the far side of a handoff (round21 §4-1, task3840) or a
    /// rearm (t260928-5cb2): the recording it replaces was ended by this
    /// feature -- its own `handoff`, or its window closing -- so the toast says 「録画を引き継ぎました」 rather than announcing a second
    /// automatic start. Only ever true alongside `start`, and it stays true
    /// across the retry polls of that same start, because the caller announces
    /// on `Started` rather than on the ask.
    pub handed_over: bool,
    /// Folder rules the guard turned down, reported once each. A rule typed
    /// into `settings.json` by hand has to be visible in the log without being
    /// repeated every second for as long as the app is open.
    pub rejected_folders: Vec<String>,
}

/// The picker's own guard on a poll's ready-made `handoff` answer (task3600
/// follow-up).
///
/// The picker calls `poll` with `capturing = active || stopping`, so while a
/// stop is still draining (~1s), `poll` sees `capturing == true` the same as
/// it would for an ordinary running recording -- `holding` has not cleared
/// yet either way, because that only happens once `capturing` goes false.
/// `AutoCaptureWatch` has no way to tell the two apart from the inside: a
/// process that merely happens to launch under the same registered folder
/// during that drain reads as ユーザー裁定c's handoff, and acting on it would
/// re-arm a recording of the newcomer on top of a session that is already on
/// its way down because the user asked to stop it.
///
/// `stopping` is known only on the picker side (the watch is never told), so
/// the guard lives here rather than inside `poll`.
pub fn handoff_target(handoff: Option<&str>, stopping: bool) -> Option<&str> {
    if stopping {
        None
    } else {
        handoff
    }
}

/// How many polls in a row may ask for a start before the launch is written
/// off. One poll is one second, so this is roughly five seconds of retrying.
///
/// There is a budget at all because some refusals are permanent: the disk
/// guard turning the start down does so again a second later, and without a
/// bound that is a `capture_start_failed` and a banner every second for as
/// long as the application stays open. A refusal that *is* transient (the
/// splash window swapped out from under the revalidation) is gone well inside
/// five tries. After the give-up the executable has to leave and come back --
/// the same edge every other arming needs.
const MAX_START_ATTEMPTS: u32 = 5;

/// Edge detection over the set of running executables.
///
/// Level-triggered ("registered app running and nothing recording → start")
/// breaks two ways: a recording the user stopped by hand restarts a second
/// later, and merely launching Liveback fires. So the trigger is the
/// *transition* from absent to present, which also means an app that was
/// already running when Liveback started never fires -- the first poll only
/// seeds the set. That is the specified behaviour, not an oversight.
///
/// Arming and firing are separate because a game's first window is not
/// necessarily capturable (splash screens are created, destroyed and recreated
/// during startup). The appearance of the *executable* arms; the appearance of
/// a `selectable` window of it fires. No invented delay.
#[derive(Debug, Default)]
pub struct AutoCaptureWatch {
    /// Executable ids of every process alive on the previous poll. All of
    /// them, not just the registered ones, so adding a registration for an app
    /// that is already running does not manufacture an edge. Processes rather
    /// than the poll's windows: a window can leave the picker's list with its
    /// process still up, and that is not an exit (task2450).
    seen: HashSet<String>,
    /// The registered executable waiting for a capturable window.
    armed: Option<String>,
    /// Which rule armed it (task3600), so the plan can hand the caller the
    /// answer rather than have it re-derived from a name that may not be in the
    /// executable list at all.
    armed_rule: Option<AutoCaptureRule>,
    /// The rule behind the recording that is currently running, set when an
    /// arming of this watch's was consumed by it. Stays `None` for a recording
    /// started by hand -- which is what keeps the handoff off manual
    /// recordings.
    holding: Option<AutoCaptureRule>,
    /// One handoff per recording. The newcomer enters `seen` on the same poll
    /// so it has no second edge of its own, but a third process out of the same
    /// folder must not chain another stop onto the same session.
    handed_off: bool,
    /// The name `forget_handed_over` was called with, i.e. the newcomer a
    /// handoff or a rearm is arming on (task3840, t260928-5cb2). Carried across the rearm because
    /// `handed_off` cannot be: the handoff stops the recording, and clearing
    /// `capturing` is exactly what resets `handed_off` -- on the same poll
    /// that arms the newcomer. This is what turns the *next* start into
    /// `plan.handed_over`.
    handoff_newcomer: Option<String>,
    /// Folder rules already reported as rejected, so the log line is written
    /// once rather than every second. Cleared with the rest of the state when
    /// both lists empty, so removing and re-adding a bad rule reports it again.
    rejected: HashSet<String>,
    /// Starts asked for since the current arming. Only polls that actually
    /// emitted one count, so a long splash screen does not spend the budget.
    attempts: u32,
    /// Whether `seen` has ever been filled. The first poll is a seed.
    seeded: bool,
}

impl AutoCaptureWatch {
    /// `inputs.running` is every live process's executable name, lowercased
    /// (`capture::targets::running_processes`) -- the edge is read from it.
    /// `targets` only decides which window a fired start names. `capturing` is
    /// "a recording is running or on its way down", read from the controller on
    /// the same poll.
    ///
    /// `folders` (task3600) is matched on the image path instead of the name,
    /// but the edge stays name-based: keying `seen` on paths would break the
    /// documented rule above that an app already running when a registration
    /// arrives is not an edge. Only names that are new this poll are resolved
    /// to a path, so the added per-second cost is a handful of `OpenProcess`
    /// calls -- orders below what `list_capture_targets` already spends on the
    /// same tick.
    pub fn poll(
        &mut self,
        registered: &[String],
        folders: &[AutoCaptureFolder],
        inputs: &AutoCaptureInputs<'_>,
        targets: &[CaptureTarget],
        capturing: bool,
    ) -> AutoCapturePlan {
        let mut plan = AutoCapturePlan::default();
        // Nothing registered either way: no set to build and no state to carry,
        // so an install that never uses the feature pays nothing for it.
        // Dropping the state also re-seeds when the first registration arrives,
        // which is what keeps an already-running app from counting as an edge.
        if registered.is_empty() && folders.is_empty() {
            *self = Self::default();
            return plan;
        }
        // Read before the seed return, so a rule the guard turns down is
        // reported on the first poll that sees it rather than the second.
        let mut active_folders: Vec<&AutoCaptureFolder> = Vec::new();
        for folder in folders {
            if folder_rule_allowed_with(&folder.path, inputs.guard) {
                active_folders.push(folder);
            } else if self
                .rejected
                .insert(folder.path.trim().to_ascii_lowercase())
            {
                plan.rejected_folders.push(folder.path.clone());
            }
        }
        let previous = std::mem::replace(&mut self.seen, inputs.running.clone());
        if !std::mem::replace(&mut self.seeded, true) {
            return plan;
        }
        // The process died before it ever offered a capturable window. Its
        // windows merely leaving the list is not that -- see the module doc.
        if self
            .armed
            .as_deref()
            .is_some_and(|executable| !self.seen.contains(executable))
        {
            self.armed = None;
            self.armed_rule = None;
            // The handed-over process died before it ever offered a window, so
            // there is no second recording to call a handoff (task3840).
            self.handoff_newcomer = None;
        }
        // Names an executable registration already decided, so a folder rule
        // covering the same process cannot decide it a second time. This is
        // also the "exe 個別指定がフォルダ規則に優先" rule for arming.
        let mut decided: HashSet<String> = HashSet::new();
        for entry in registered {
            // Windows filenames are case-insensitive and this list is
            // hand-edited, so `Discord.exe` has to match `discord.exe`.
            let executable = entry.trim().to_ascii_lowercase();
            if executable.is_empty()
                || !self.seen.contains(&executable)
                || previous.contains(&executable)
            {
                continue;
            }
            if !decided.insert(executable.clone()) {
                continue;
            }
            // One recording at a time, so two launches in one poll window are
            // decided by the order the user registered them in.
            if self.armed.is_none() && !capturing {
                self.armed = Some(executable.clone());
                self.armed_rule = Some(AutoCaptureRule::Executable);
                self.attempts = 0;
                plan.armed = Some(executable);
                plan.rule = Some(AutoCaptureRule::Executable);
            } else {
                plan.skipped.push(executable);
            }
        }
        // Folder rules (task3600), after the executable list so a name in both
        // is decided by its own entry.
        if !active_folders.is_empty() {
            // `difference` walks a hash set, so two launches in one poll would
            // otherwise be ordered by hash. Sorted, that tie is stable and
            // testable; which *rule* wins is still the registration order.
            let mut newcomers: Vec<String> = self
                .seen
                .difference(&previous)
                .filter(|name| !decided.contains(*name))
                .cloned()
                .collect();
            newcomers.sort_unstable();
            for name in newcomers {
                let Some(folder) = matching_folder(&active_folders, &name, inputs) else {
                    continue;
                };
                let rule = AutoCaptureRule::Folder(folder.path.clone());
                if self.armed.is_none() && !capturing {
                    self.armed = Some(name.clone());
                    self.armed_rule = Some(rule.clone());
                    self.attempts = 0;
                    plan.armed = Some(name);
                    plan.rule = Some(rule);
                } else if capturing
                    && !self.handed_off
                    && matches!(
                        &self.holding,
                        Some(AutoCaptureRule::Folder(held)) if same_rule(held, &folder.path)
                    )
                {
                    // ユーザー裁定c: the running recording was started by this
                    // same folder rule -- a launcher -- and the process that
                    // just appeared lives in that folder too, so the recording
                    // follows the newcomer. Only ever inside one rule, and only
                    // over a recording an arming of ours was consumed by; a
                    // recording started by hand leaves `holding` empty.
                    self.handed_off = true;
                    plan.handoff = Some(name);
                } else {
                    plan.skipped.push(name);
                }
            }
        }
        // A recording already running is a hard skip rather than a wait: an app
        // launched during someone else's recording must not grab the encoder
        // the moment that recording ends.
        //
        // This is also what ends the arming after a successful start, and with
        // it the manual-stop case: stopping by hand leaves the executable in
        // `seen`, so there is no new edge until the app is closed and
        // relaunched.
        if capturing {
            if let Some(executable) = self.armed.take() {
                // The recording that consumed the arming is the one a handoff
                // may later replace.
                self.holding = self.armed_rule.take();
                // The handed-over start took: the toast has been asked for, and
                // this recording is an ordinary one from here on -- a third
                // process out of the same folder gets its own answer (task3840).
                self.handoff_newcomer = None;
                plan.skipped.push(executable);
            }
            return plan;
        }
        // Nothing is running any more, so there is nothing to hand over and the
        // next recording gets its own handoff budget.
        self.holding = None;
        self.handed_off = false;
        // Still armed: fire, and stay armed. The start goes through
        // `spawn_start_capture`, which revalidates the handle and can refuse
        // (a splash window swapped out from under it); giving up at the first
        // refusal would lose the launch. The retry is simply the next poll, and
        // the UI's `starting` guard is what stops a slow start from being asked
        // for twice.
        //
        // Bounded, though: see `MAX_START_ATTEMPTS`. A refusal that never stops
        // being one would otherwise be re-asked, and re-reported, every second.
        if let Some(executable) = self.armed.as_deref() {
            plan.start = targets
                .iter()
                .find(|target| {
                    target.kind == CaptureTargetKind::Window
                        && target.selectable
                        && target.executable_id.as_deref() == Some(executable)
                })
                .map(|target| target.id.clone());
            if plan.start.is_some() {
                // Told on every start rather than only the arming poll: the
                // caller logs which rule armed it, and a start can be
                // a retry (task3600). Same reason `handed_over` repeats
                // (task3840): the announcement happens on `Started`, which may
                // be several retries later than the arming poll.
                plan.rule = self.armed_rule.clone();
                plan.handed_over = self.handoff_newcomer.as_deref() == Some(executable);
                self.attempts += 1;
                if self.attempts >= MAX_START_ATTEMPTS {
                    // This poll's start still goes out -- it is the last one.
                    // Dropping the arming does not cancel a start already in
                    // flight, so a slow-but-working start is unharmed.
                    plan.gave_up = self.armed.take();
                    self.armed_rule = None;
                    // Nothing is going to start now, so the next launch of this
                    // name is a launch, not a handoff (task3840).
                    self.handoff_newcomer = None;
                }
            }
        }
        plan
    }

    /// Removes `executable` from `seen`, so the next `poll` reads the process
    /// as newly launched and manufactures the ordinary launch edge -- arming,
    /// splash wait, retry budget, disarm-on-death and all. WGC cannot rebind
    /// a running session, so a recording that has to follow a process to
    /// another window is a fresh start, and a fresh start wants a fresh edge.
    pub fn forget(&mut self, executable: &str) {
        self.seen.remove(&executable.trim().to_ascii_lowercase());
    }

    /// `forget`, plus the note that the edge it manufactures continues a
    /// recording this feature ended itself -- so the start it leads to carries
    /// `handed_over` and its toast says the recording moved rather than that a
    /// second one began. The picker's two rearms both use it: the handoff
    /// (round21 §4-1, task3840) and the bound window closing while its process
    /// lives on (SourceClosed, task2550; marked since t260928-5cb2).
    pub fn forget_handed_over(&mut self, executable: &str) {
        let executable = executable.trim().to_ascii_lowercase();
        self.seen.remove(&executable);
        self.handoff_newcomer = Some(executable);
    }
}

/// How many polls a SourceClosed stop report is held for its rearm to start
/// (t260928-5cb2). One poll is one second. Rearm-to-start measured on a real
/// application was 2.26 s and 2.33 s, so five leaves room without making a
/// genuine stop wait long when the process neither exits nor opens a window.
pub const REARM_HOLD_POLLS: u32 = 5;

/// A SourceClosed stop whose report waits on the rearm it scheduled
/// (t260928-5cb2).
///
/// A process whose recorded window closes while it keeps running is either
/// moving to another window -- a splash screen giving way to the application
/// -- or on its way out, window first. Reported at once, the first reads as a
/// stop and an unrelated start a second apart; suppressed outright, the second
/// -- closing the application -- goes silent. So the report is held until the
/// rearm resolves: a replacement recording running drops it (the handover
/// toast says what happened), anything else releases it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RearmHold {
    /// The lifecycle event being held.
    pub seq: u64,
    /// Lowercased, as the ids are.
    pub executable: String,
    /// The stopped recording's session, when it was the first one its launch
    /// produced: that one showed what the process put up before the window it
    /// moved to -- a splash screen -- and goes to the recycle bin once the
    /// replacement is running. `None` for a recording that was itself a
    /// rearm's, so an application window closing never costs its recording.
    pub first_of_launch: Option<String>,
    polls: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RearmHoldOutcome {
    Waiting,
    /// The replacement recording is running: never report the stop, and
    /// recycle `discard` if there is one.
    Replaced {
        discard: Option<String>,
    },
    /// Report the stop after all.
    Released,
}

impl RearmHold {
    pub fn new(seq: u64, executable: &str, first_of_launch: Option<String>) -> Self {
        Self {
            seq,
            executable: executable.trim().to_ascii_lowercase(),
            first_of_launch,
            polls: 0,
        }
    }

    /// One poll. `replacement_running`: a recording of this executable,
    /// started after the hold, is in the active list. `abandoned`: the rearm
    /// cannot start any more -- the process exited, or the arming gave up.
    pub fn tick(&mut self, replacement_running: bool, abandoned: bool) -> RearmHoldOutcome {
        if replacement_running {
            return RearmHoldOutcome::Replaced {
                discard: self.first_of_launch.take(),
            };
        }
        self.polls += 1;
        if abandoned || self.polls > REARM_HOLD_POLLS {
            RearmHoldOutcome::Released
        } else {
            RearmHoldOutcome::Waiting
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One window of one process. `executable_id` is lowercased by
    /// `list_capture_targets`, so the fixtures are too.
    fn window(id: &str, executable: &str, selectable: bool) -> CaptureTarget {
        CaptureTarget {
            kind: CaptureTargetKind::Window,
            primary: false,
            id: id.to_owned(),
            window_handle: format!("0x{id}"),
            process_id: 1,
            title: id.to_owned(),
            executable_id: Some(executable.to_ascii_lowercase()),
            executable_name: Some(executable.to_owned()),
            minimized: false,
            selectable,
            unavailable_reason: None,
        }
    }

    fn registered(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["  GAME.EXE  "]);
        let mut watch = AutoCaptureWatch::default();
        poll_live(&mut watch, &list, &[], false);
        let plan = poll_live(&mut watch, &list, &[window("2", "Game.exe", true)], false);
        assert_eq!(plan.start.as_deref(), Some("2"));
    }

    /// Recording is single, so two launches in one second are decided by the
    /// registration order and the loser is logged rather than queued.
    #[test]
    fn two_apps_in_one_poll_start_the_first_registered_one() {
        let list = registered(&["first.exe", "second.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["mspaint.exe"]);
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
        let list = registered(&["other.exe"]);
        poll_names(&mut watch, &list, &running, &targets, false);
        poll_names(&mut watch, &list, &running, &targets, false);
        // `game.exe` joins the list while it is already up.
        let list = registered(&["other.exe", "game.exe"]);
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
        assert_eq!(off.body, "mspaint.exe が起動すると録画を始めます。");
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
        assert_eq!(unresolved.body, "game.exe が起動すると録画を始めます。");
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
        assert_eq!(ja.body, "game.exe が起動すると録画を始めます。");
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
        assert_eq!(en.body, "Starts recording when game.exe launches.");
        assert!(!en.body.contains("Windows"), "{}", en.body);
    }

    /// The rearm primitive (task2550): forgetting the executable makes the
    /// next poll read the still-running process as a fresh launch, so the
    /// next selectable window of it starts a new recording. Spelled with the
    /// on-disk casing to prove `forget` folds case like everything else here.
    #[test]
    fn forgetting_a_running_app_manufactures_a_new_edge() {
        let list = registered(&["game.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let list = registered(&["first.exe", "second.exe"]);
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
        let list = registered(&["game.exe"]);
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
        let names = registered(&["launcher.exe"]);
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
        let names = registered(&["game.exe"]);
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
}
