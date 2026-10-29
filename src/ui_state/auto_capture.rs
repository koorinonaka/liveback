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
use crate::settings::{executable_key, same_executable, AutoCaptureApp, AutoCaptureFolder};
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
            .any(|entry| same_executable(&entry.name, executable))
}

/// Drops every entry naming `executable`, in any spelling -- a hand-edited file
/// can hold both `Game.exe` and `game.exe`, and removing one of them from the
/// settings row would leave the feature still armed.
pub fn unregister(registered: &[AutoCaptureApp], executable: &str) -> Vec<AutoCaptureApp> {
    let executable = executable.trim();
    registered
        .iter()
        .filter(|entry| !same_executable(&entry.name, executable))
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
        let active_folders = self.active_folders(folders, inputs, &mut plan);
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
        self.arm_newcomers(
            registered,
            &active_folders,
            &previous,
            inputs,
            capturing,
            &mut plan,
        );
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
        self.fire(targets, &mut plan);
        plan
    }

    /// Step (b) of `poll`: the folder rules the guard allows. A rule it turns
    /// down is reported in `plan.rejected_folders` once, not every poll.
    fn active_folders<'f>(
        &mut self,
        folders: &'f [AutoCaptureFolder],
        inputs: &AutoCaptureInputs<'_>,
        plan: &mut AutoCapturePlan,
    ) -> Vec<&'f AutoCaptureFolder> {
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
        active_folders
    }

    /// Steps (e) and (f) of `poll`: arm on the edge of a registered executable,
    /// then of a process out of an allowed folder, or record why not.
    fn arm_newcomers(
        &mut self,
        registered: &[String],
        active_folders: &[&AutoCaptureFolder],
        previous: &HashSet<String>,
        inputs: &AutoCaptureInputs<'_>,
        capturing: bool,
        plan: &mut AutoCapturePlan,
    ) {
        // Names an executable registration already decided, so a folder rule
        // covering the same process cannot decide it a second time. This is
        // also the "exe 個別指定がフォルダ規則に優先" rule for arming.
        let mut decided: HashSet<String> = HashSet::new();
        for entry in registered {
            // Windows filenames are case-insensitive and this list is
            // hand-edited, so `Discord.exe` has to match `discord.exe`.
            let executable = executable_key(entry);
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
                .difference(previous)
                .filter(|name| !decided.contains(*name))
                .cloned()
                .collect();
            newcomers.sort_unstable();
            for name in newcomers {
                let Some(folder) = matching_folder(active_folders, &name, inputs) else {
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
    }

    /// Step (h) of `poll`: the armed executable's window, as this poll's
    /// start, against the retry budget.
    fn fire(&mut self, targets: &[CaptureTarget], plan: &mut AutoCapturePlan) {
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
    }

    /// Removes `executable` from `seen`, so the next `poll` reads the process
    /// as newly launched and manufactures the ordinary launch edge -- arming,
    /// splash wait, retry budget, disarm-on-death and all. WGC cannot rebind
    /// a running session, so a recording that has to follow a process to
    /// another window is a fresh start, and a fresh start wants a fresh edge.
    pub fn forget(&mut self, executable: &str) {
        self.seen.remove(&executable_key(executable));
    }

    /// `forget`, plus the note that the edge it manufactures continues a
    /// recording this feature ended itself -- so the start it leads to carries
    /// `handed_over` and its toast says the recording moved rather than that a
    /// second one began. The picker's two rearms both use it: the handoff
    /// (round21 §4-1, task3840) and the bound window closing while its process
    /// lives on (SourceClosed, task2550; marked since t260928-5cb2).
    pub fn forget_handed_over(&mut self, executable: &str) {
        let executable = executable_key(executable);
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
            executable: executable_key(executable),
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

/// The metadata upgrade of the auto-record list (task2560 / task2590): an entry
/// with a path and a display name is kept; one with a path but no name gets
/// its name from `describe` (the file's version info); one with no path is
/// `resolve`d from a running process, kept as it was when that finds nothing.
/// `None` when nothing changed -- the steady state, which writes nothing. The
/// I/O behind the two closures is the caller's.
pub fn upgrade_plan(
    entries: &[AutoCaptureApp],
    mut describe: impl FnMut(&str) -> Option<String>,
    mut resolve: impl FnMut(&str) -> AutoCaptureApp,
) -> Option<Vec<AutoCaptureApp>> {
    let mut changed = false;
    let upgraded = entries
        .iter()
        .map(|entry| {
            if let Some(path) = &entry.path {
                if entry.display_name.is_some() {
                    return entry.clone();
                }
                let Some(name) = describe(path) else {
                    return entry.clone();
                };
                changed = true;
                return AutoCaptureApp {
                    display_name: Some(name),
                    ..entry.clone()
                };
            }
            let resolved = resolve(&entry.name);
            if resolved.path.is_none() {
                return entry.clone();
            }
            changed = true;
            resolved
        })
        .collect();
    changed.then_some(upgraded)
}

#[cfg(test)]
mod tests;
