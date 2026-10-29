//! The target picker's state (task124).
//!
//! The behaviour is transcribed from the React implementation that round-2
//! finished: `src/pages/TargetsPage.tsx` for the grid and its two empty states,
//! `src/useAppState.ts` (`reloadTargets` / `selectAndStartCapture`) for
//! selection, and `src/i18n/ja.ts` for the strings.

use crate::capture::targets::{sort_previous_first, CaptureTarget, CaptureTargetKind};
use crate::capture::StartError;
use crate::ui_state::lifecycle;
use crate::ui_state::locale::Locale;

// The picker's wording (task1150 moved it into `tr!`). Kept next to the logic
// that decides when each one applies rather than in one central table -- the
// pairing of message to condition is the part that has to stay right (task114).
crate::tr! {
    /// DS CaptureScreen (t260927-711d): the one field, on both tabs.
    search_placeholder { ja: "タイトルやアプリ名で検索", en: "Search by title or app" }
    search_clear { ja: "検索をクリア", en: "Clear search" }
    /// The list re-reads itself once a second, so an empty grid has nothing
    /// for the user to *do* -- the refresh button it used to offer was a verb
    /// for a job that was already running (task143). Saying so is the answer.
    /// DS CaptureScreen 「状態」 (t260927-711d): an `EmptyState` with no button.
    empty_title { ja: "録画できるウィンドウがありません", en: "No windows to record" }
    empty_sub { ja: "一覧は自動で更新されます。", en: "The list updates on its own." }
    /// The poll itself failed: an error `EmptyState`, its reason under it.
    list_error_title { ja: "一覧を取得できませんでした", en: "Couldn't get the list" }
    /// That reason (t260928-08df): `capture::targets` answers in English for
    /// the log, and the flyout says this instead.
    list_error_reason {
        ja: "Windows からウィンドウの一覧を受け取れませんでした。",
        en: "Windows did not return the list of windows."
    }
    /// A start that did not happen (t260927-711d): the flyout has closed by
    /// then, so this is an error toast's title and the reason is its detail.
    start_failed_title { ja: "録画を開始できませんでした", en: "Couldn't start recording" }
    /// A window whose process the enumeration could not read. The capture side
    /// only flags the case (`UNAVAILABLE_REASON`); the wording is the picker's,
    /// same as the minimized one below -- it used to be a Japanese literal in
    /// `capture::targets`, which the `Locale::En` picker showed untranslated.
    process_unavailable {
        ja: "プロセス情報を取得できません",
        en: "This window's process could not be read"
    }
    /// A start's reasons (t260928-08df, DS CaptureScreen 「録り始められなか
    /// った」): the toast's second line. The window left, or its process was
    /// replaced under the handle, between the listing and the press.
    target_closed {
        ja: "対象のウィンドウが閉じられました。",
        en: "The window was closed."
    }
    start_internal {
        ja: "アプリ内部の問題で開始できませんでした。アプリを再起動してください。",
        en: "Something inside the app went wrong. Restart the app."
    }
    start_codec {
        ja: "設定の録画コーデックはこの PC では使えません。設定で変更してください。",
        en: "This PC cannot record with the codec in Settings. Change it there."
    }
    start_buffer_folder {
        ja: "録画バッファのフォルダを使えません。設定で保存先を確かめてください。",
        en: "The buffer folder cannot be used. Check its location in Settings."
    }
    /// task2430: a minimized window is listed but not pickable -- the encoder
    /// refuses the title-bar-sized client rect a minimized window reports, so
    /// the tile would only ever fail. Restoring it is the user's move.
    minimized_unavailable {
        ja: "最小化中は録画できません",
        en: "Cannot record while minimized"
    }
    unavailable { ja: "録画できません", en: "Cannot record" }

    /// The screen the desktop calls home, said in the tile's title rather
    /// than as a badge (DS TargetTile 「画面のタイトル」, t260927-711d).
    monitor_primary { ja: "メイン", en: "Main" }

    /// W-7 / W-8: the two halves of the picker's tab bar.
    tab_windows { ja: "ウィンドウ", en: "Windows" }
    tab_monitors { ja: "画面全体", en: "Screens" }
    /// W-12: the word the transport chip and the history row use for a capture
    /// in progress (task235). Not the picker's any more: its tile says REC.
    recording { ja: "LIVE", en: "LIVE" }
    /// The tag on a tile being recorded (DS `.lb-thumb__live`).
    rec { ja: "REC", en: "REC" }
    /// The plate in the middle of a hovered tile (DS TargetTile). The DS's
    /// words: recording is 録画, キャプチャ is only the rail item's name.
    capture_action { ja: "録画する", en: "Record" }


    /// The flyout's foot: what a pick records (DS CaptureScreen 中身).
    foot_windows {
        ja: "選んだウィンドウだけを録ります。通知や他のアプリは映りません。",
        en: "Only the window you pick is recorded. Notifications and other apps stay out."
    }
    foot_monitors {
        ja: "画面全体では、その画面に映るものがすべて録画されます。",
        en: "A whole screen records everything that shows on it."
    }
}

/// What the button in the middle of a hovered tile does. Not a bool: the two
/// answers take different words, different colours and different callbacks, and
/// naming them keeps the `.slint` side from re-deriving the rule on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileAction {
    /// Nothing can be recorded here, so the tile offers no hover action.
    None,
    /// Start recording this target, or switch to it from another (task166).
    Capture,
    /// This *is* the recording. Pressing it stops.
    Stop,
}

impl TileAction {
    pub fn label(self, locale: Locale) -> &'static str {
        match self {
            TileAction::None => "",
            TileAction::Capture => capture_action(locale),
            // The plate on the tile already being recorded (task175): the
            // picker is where a recording is started, so it is where stopping
            // one belongs.
            TileAction::Stop => crate::ui_state::shell::control_stop_recording(locale),
        }
    }
}

/// What joins a screen's name to its resolution in W-10. The tile draws the two
/// halves in opposite corners (design §5), so the format and the split have to
/// agree on one separator rather than each guessing.
const MONITOR_SEPARATOR: &str = " — ";

/// W-10 (round5 §11): a screen's label in the picker. `number` is 1-based --
/// the enumeration order, which is what the user counts from.
pub fn monitor_display_name(locale: Locale, number: usize, width: i32, height: i32) -> String {
    let screen = match locale {
        Locale::Ja => "画面",
        Locale::En => "Screen",
    };
    format!("{screen} {number}{MONITOR_SEPARATOR}{width}×{height}")
}

/// W-10 taken apart again for the tile badge: the name goes bottom-left, the
/// resolution bottom-right. A title that is not a screen's comes back whole,
/// with no resolution.
pub fn monitor_name_and_resolution(title: &str) -> (&str, &str) {
    title.split_once(MONITOR_SEPARATOR).unwrap_or((title, ""))
}

/// Which half of the picker is showing (round5 §5). Windows and screens are
/// listed together and told apart here rather than polled separately: one list,
/// one order, and the tab is only a filter over it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PickerTab {
    #[default]
    Windows,
    Monitors,
}

impl PickerTab {
    /// The `SegmentTabs` position, which is all the `.slint` side knows.
    pub fn from_index(index: i32) -> Self {
        if index == 1 {
            Self::Monitors
        } else {
            Self::Windows
        }
    }

    pub fn index(self) -> i32 {
        match self {
            Self::Windows => 0,
            Self::Monitors => 1,
        }
    }

    fn accepts(self, kind: CaptureTargetKind) -> bool {
        matches!(
            (self, kind),
            (Self::Windows, CaptureTargetKind::Window)
                | (Self::Monitors, CaptureTargetKind::Monitor)
        )
    }
}

/// Keeps the grid still while the modal is open (design §5). `sort_previous_first`
/// runs once, at the moment it opens; every poll after that reuses the order the
/// user is looking at -- targets that are still listed stay where they are, new
/// ones go on the end, and only the ones that vanished leave. Without this the
/// 1s poll re-sorted the grid the instant recording started and moved the tile
/// out from under the cursor.
///
// ponytail: O(n * m) by `position` + `remove`; the list is a few dozen windows,
// so an index map is not worth the allocation until it is thousands.
pub fn keep_previous_order(order: &[String], listed: Vec<CaptureTarget>) -> Vec<CaptureTarget> {
    let mut remaining = listed;
    let mut kept = Vec::with_capacity(remaining.len());
    for id in order {
        if let Some(at) = remaining.iter().position(|target| &target.id == id) {
            kept.push(remaining.remove(at));
        }
    }
    kept.extend(remaining);
    kept
}

/// DS CaptureScreen 「検索に合うものがない」, named for the tab it is on
/// (t260928-08df): the screens tab has no windows to not match.
pub fn search_empty_text(locale: Locale, tab: PickerTab, query: &str) -> String {
    let (ja, en) = match tab {
        PickerTab::Windows => ("ウィンドウ", "windows"),
        PickerTab::Monitors => ("画面", "displays"),
    };
    crate::ui_state::sessions::no_match_text(locale, ja, en, query)
}

/// The toast's second line for a start `CaptureController::start` refused
/// (t260928-08df): one plain sentence per [`StartError`] kind. The English
/// detail it carries is for the log, which the caller writes.
pub fn start_failure_text(locale: Locale, error: &StartError) -> String {
    match error {
        StartError::Internal(_) => start_internal(locale).to_owned(),
        StartError::Codec(_) => start_codec(locale).to_owned(),
        StartError::BufferFolder(_) => start_buffer_folder(locale).to_owned(),
        StartError::NoSpace(sentence) => sentence.clone(),
    }
}

/// A tile's title (DS TargetTile 「画面のタイトル」): a screen's number, and
/// メイン in words on the primary one -- no badge, and the resolution goes to
/// its own tag. A window's title is its own.
pub fn tile_title(locale: Locale, target: &CaptureTarget) -> String {
    if target.kind != CaptureTargetKind::Monitor {
        return target.title.clone();
    }
    let name = monitor_name_and_resolution(&target.title).0;
    if target.primary {
        format!("{name} · {}", monitor_primary(locale))
    } else {
        name.to_owned()
    }
}

/// The flyout's foot line and whether it is the warning (DS CaptureScreen
/// 「状態」): what a pick records on this tab, until
/// [`lifecycle::CONCURRENCY_WARNING_MIN`] or more are running -- then that
/// warning takes the line instead of a bar of its own.
pub fn foot_note(locale: Locale, tab: PickerTab, running: usize) -> (String, bool) {
    match lifecycle::concurrency_warning(locale, running) {
        Some(warning) => (warning, true),
        None => (
            match tab {
                PickerTab::Windows => foot_windows(locale),
                PickerTab::Monitors => foot_monitors(locale),
            }
            .to_owned(),
            false,
        ),
    }
}

/// Which of the two nothings the grid is showing. They take different verbs:
/// "no windows at all" is answered by refreshing, "your filter matched nothing"
/// by clearing it, and offering the wrong one sends the user down the wrong
/// road (task114 / round2 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetsEmpty {
    /// Windows exist; none of them matched the search.
    NoMatch,
    /// Nothing capturable at all.
    NoTargets,
}

/// A failed list poll and its reason (task143; t260927-711d). The poll re-reads
/// itself every second, so the next good list is what answers it. A *start*
/// that fails is not held here any more: the flyout closes on the click, so it
/// is said in a toast (DS CaptureScreen 「録り始められなかった」 -- 「次に開いた
/// ときまで残さない」), which is why this no longer has a close button or an
/// action half.
#[derive(Clone, Debug, Default)]
pub struct TargetsAlert {
    list_failure: Option<String>,
}

impl TargetsAlert {
    /// The reason, while the last poll failed.
    pub fn text(&self) -> Option<&str> {
        self.list_failure.as_deref()
    }

    /// `true` when this actually cleared a list failure (task a015). The drain
    /// raises `flags.targets` off what moved, not off a message having arrived,
    /// so every mutator on this type has to answer that question.
    pub fn listed_ok(&mut self) -> bool {
        self.list_failure.take().is_some()
    }

    /// `true` when what is on screen changed -- a second consecutive failure
    /// with the same reason re-states what is already there.
    pub fn list_failed(&mut self, reason: String) -> bool {
        if self.list_failure.as_deref() == Some(reason.as_str()) {
            return false;
        }
        self.list_failure = Some(reason);
        true
    }
}

/// What a list message actually moved (task a015).
///
/// The drain used to raise `flags.targets` for any message addressed to the
/// picker, so the 1s poll re-rendering an identical grid cost a `present` a
/// second (task4290 measured the remainder and could not fix it inside its own
/// scope). This is what the poll is asked instead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetsChange {
    /// The listed targets differ from the ones already held. Compared *after*
    /// [`keep_previous_order`] has run, which is the explicit ruling on order:
    /// a poll returning the same windows in a different enumeration order is
    /// **not** a change, because the grid it would redraw is identical.
    pub listed: bool,
    /// The preselected tile moved.
    pub selection: bool,
    /// The title of the first target that differs, when `listed`. The log line
    /// this feeds is the whole instrument, and a bare "changed" cannot tell a
    /// window opening from a title that merely ticked -- this can.
    pub first_diff: Option<String>,
}

impl TargetsChange {
    /// Whether anything the grid draws moved.
    pub fn any(&self) -> bool {
        self.listed || self.selection
    }
}

/// The title at the first position two lists differ at. A length difference
/// with a common prefix reports the first target past the shorter one.
fn first_difference(before: &[CaptureTarget], after: &[CaptureTarget]) -> Option<String> {
    let at = before
        .iter()
        .zip(after)
        .position(|(old, new)| old != new)
        .unwrap_or_else(|| before.len().min(after.len()));
    after
        .get(at)
        .or_else(|| before.get(at))
        .map(|target| target.title.clone())
}

#[derive(Debug, Default)]
pub struct TargetsState {
    targets: Vec<CaptureTarget>,
    search: String,
    selected_id: Option<String>,
    /// Every capture running right now as `(session id, target window handle)`,
    /// oldest start first (task2030). It used to be a `bool` plus one
    /// `capturing_id`, which said "at most one tile is recording" -- and the
    /// tile is a toggle now, so each one needs to know *which* capture it would
    /// stop. Keyed on the window handle rather than the tile id because that is
    /// what `CaptureController` keeps per capture; a tile id also carries the
    /// process id, which a restarted process changes.
    running: Vec<(String, String)>,
    /// A tile whose start has been asked for but has not come back on a poll
    /// yet (task166, task2030). One second of optimism: without it the tile
    /// keeps offering 「画面をキャプチャ」 until the next poll, and a second
    /// click there would start a *second* capture of the same window.
    pending_start: Option<String>,
    /// ウィンドウ or 画面全体 (task170). Windows and screens live in one list;
    /// this decides which of them the grid is currently showing.
    tab: PickerTab,
}

impl TargetsState {
    /// Replaces the list, mirroring `reloadTargets`: a selection that survived
    /// the refresh is kept, otherwise the last-used executable is preselected
    /// if one of its windows is selectable.
    pub fn set_targets(
        &mut self,
        targets: Vec<CaptureTarget>,
        last_executable: Option<&str>,
    ) -> TargetsChange {
        let listed = self.targets != targets;
        let first_diff = if listed {
            first_difference(&self.targets, &targets)
        } else {
            None
        };
        self.targets = targets;
        let selection = self.reselect(last_executable);
        TargetsChange {
            listed,
            selection,
            first_diff,
        }
    }

    /// The preselect half of [`set_targets`], on its own so a change to
    /// `last_capture_executable` can move the selection without a new list
    /// (task t260911-66da). That field reaches the picker on its own message
    /// now, and the list poll is no longer sent when nothing in the list moved
    /// -- so the once-a-second re-run this used to get is gone. Answers
    /// whether the selection moved.
    pub fn reselect(&mut self, last_executable: Option<&str>) -> bool {
        let selected_before = self.selected_id.clone();
        let kept = self
            .selected_id
            .as_deref()
            .is_some_and(|selected| self.targets.iter().any(|target| target.id == selected));
        if !kept {
            self.selected_id = last_executable.and_then(|executable| {
                self.targets
                    .iter()
                    .find(|target| {
                        target.selectable
                            && target
                                .executable_id
                                .as_deref()
                                .is_some_and(|id| crate::settings::same_executable(id, executable))
                    })
                    .map(|target| target.id.clone())
            });
        }
        self.selected_id != selected_before
    }

    /// A poll while the modal is open. Same bookkeeping as [`set_targets`],
    /// except the order the user is looking at survives it (design §5).
    pub fn refresh_targets(
        &mut self,
        targets: Vec<CaptureTarget>,
        last_executable: Option<&str>,
    ) -> TargetsChange {
        let order: Vec<String> = self
            .targets
            .iter()
            .map(|target| target.id.clone())
            .collect();
        self.set_targets(keep_previous_order(&order, targets), last_executable)
    }

    /// The modal just opened: the one moment the grid is allowed to reshuffle.
    pub fn sort_for_open(&mut self, last_executable: Option<&str>) {
        let targets = std::mem::take(&mut self.targets);
        self.targets = sort_previous_first(targets, last_executable);
    }

    /// Every target the poll listed, in list order -- not the tab's filtered
    /// view. One image path per process is resolved from this when a folder
    /// rule moves without the list having moved (task t260911-66da).
    pub fn all(&self) -> &[CaptureTarget] {
        &self.targets
    }

    pub fn set_tab(&mut self, tab: PickerTab) {
        self.tab = tab;
    }

    pub fn tab(&self) -> PickerTab {
        self.tab
    }

    pub fn set_search(&mut self, query: &str) {
        self.search = query.to_owned();
    }

    pub fn search(&self) -> &str {
        &self.search
    }

    /// What the 1s poll saw: `(session id, target window handle)` per running
    /// capture. Ground truth, so the optimistic tile from the last click is
    /// dropped here whether or not it made it into the list.
    pub fn set_running(&mut self, running: Vec<(String, String)>) -> bool {
        // `Msg::Capturing` is sent on every 1s tick of the picker worker; the
        // list is read on that tick only while the picker is visible, a rule
        // is registered or a capture runs (t261025-b28f). Either way this
        // answers the same question `set_targets` does (task a015). Dropping an
        // optimistic `pending_start` is a change even when the map did not
        // move: it is what the clicked tile is drawing.
        let changed = self.running != running || self.pending_start.is_some();
        self.running = running;
        self.pending_start = None;
        changed
    }

    /// A capture that has just been asked for, and on which tile (task166).
    pub fn set_capturing_target(&mut self, id: &str) {
        self.pending_start = Some(id.to_owned());
    }

    /// The capture this tile would stop, if it is recording one (task2030).
    pub fn recording_session(&self, id: &str) -> Option<&str> {
        let handle = self.find(id).map(|target| target.window_handle.as_str())?;
        self.running
            .iter()
            .find(|(_, running)| running == handle)
            .map(|(session_id, _)| session_id.as_str())
    }

    /// Whether this tile is being recorded, optimism included -- what the tile
    /// draws and what its click means, as opposed to what can be stopped.
    fn tile_recording(&self, id: &str) -> bool {
        self.pending_start.as_deref() == Some(id) || self.recording_session(id).is_some()
    }

    /// How many captures are running. What the threshold warning bar counts
    /// (task2030 判断2); nothing refuses a start on it.
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn capturing(&self) -> bool {
        !self.running.is_empty() || self.pending_start.is_some()
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    pub fn select(&mut self, id: &str) {
        self.selected_id = Some(id.to_owned());
    }

    /// Everything the current tab could show, before the search narrows it.
    pub fn total(&self) -> usize {
        self.in_tab().count()
    }

    fn in_tab(&self) -> impl Iterator<Item = &CaptureTarget> {
        let tab = self.tab;
        self.targets
            .iter()
            .filter(move |target| tab.accepts(target.kind))
    }

    /// Everything of this tab's kind, whatever the search: the count its tab
    /// carries (DS Tabs `count`).
    pub fn tab_count(&self, tab: PickerTab) -> usize {
        self.targets
            .iter()
            .filter(|target| tab.accepts(target.kind))
            .count()
    }

    /// Title or executable name, case-insensitive, same as the React filter --
    /// within the current tab. Both tabs search since t260927-711d (決めたこと
    /// 3): a screen answers to its number and, on the primary one, to メイン in
    /// either language, since the locale is not known here.
    pub fn visible(&self) -> impl Iterator<Item = &CaptureTarget> {
        let query = self.search.to_lowercase();
        self.in_tab().filter(move |target| {
            target.title.to_lowercase().contains(&query)
                || target
                    .executable_name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&query))
                || (target.primary
                    && [Locale::Ja, Locale::En]
                        .into_iter()
                        .any(|locale| monitor_primary(locale).to_lowercase().contains(&query)))
        })
    }

    pub fn visible_count(&self) -> usize {
        self.visible().count()
    }

    pub fn empty(&self) -> Option<TargetsEmpty> {
        if self.visible_count() > 0 {
            None
        } else if self.total() > 0 {
            Some(TargetsEmpty::NoMatch)
        } else {
            Some(TargetsEmpty::NoTargets)
        }
    }

    pub fn find(&self, id: &str) -> Option<&CaptureTarget> {
        self.targets.iter().find(|target| target.id == id)
    }

    /// A click on this tile starts one more capture (task2030 判断1). Recording
    /// never freezes the grid: a click on another target adds to what is running
    /// rather than replacing it, which is why there is no count in here. The
    /// tile already recording is the toggle's other half -- its click stops that
    /// capture, so it cannot also start one.
    pub fn can_start(&self, id: &str) -> bool {
        !self.tile_recording(id) && self.find(id).is_some_and(|target| target.selectable)
    }

    /// Whether starting this target needs an executable identity.
    ///
    /// A window does: `executable_id` is what tells a restarted process from the
    /// one that was enumerated, and `is_capture_target_valid` re-checks it just
    /// before the capture starts. A screen has neither -- there is no process
    /// behind an HMONITOR. The start path used to demand the id from every
    /// target, so a click on a screen tile silently did nothing (task165).
    pub fn needs_executable(kind: CaptureTargetKind) -> bool {
        !matches!(kind, CaptureTargetKind::Monitor)
    }

    /// What the hovered tile's centre button offers (task175, task2030 判断1).
    /// The tile is a toggle: a recording tile stops that one capture, every
    /// other selectable tile starts one more beside whatever is running, and an
    /// unselectable tile offers nothing at all.
    pub fn tile_action(&self, id: &str) -> TileAction {
        if !self.find(id).is_some_and(|target| target.selectable) {
            return TileAction::None;
        }
        if self.tile_recording(id) {
            TileAction::Stop
        } else {
            TileAction::Capture
        }
    }
}

/// *Why you cannot pick this one*, in the middle of the sunk tile (DS
/// TargetTile 「選べない」). The only word a minimized window gets now: the
/// 最小化中 badge beside it went with t260927-711d.
pub fn blocked_reason(locale: Locale, target: &CaptureTarget) -> Option<&str> {
    if target.selectable {
        return None;
    }
    // A window with no readable process is worded here too: the enumeration
    // only says *that* it happened. Minimized is the case it always left to the
    // UI (task2430).
    if target.unavailable_reason.is_some() {
        return Some(process_unavailable(locale));
    }
    Some(if target.minimized {
        minimized_unavailable(locale)
    } else {
        unavailable(locale)
    })
}

/// Which captured thumbnail frames are worth handing to the UI (task4290).
///
/// The picker's poller re-captures every listed target while the modal is open,
/// and each send replaces that tile's `Image` even when the picture is byte for
/// byte the one already on screen -- which is what kept a picker full of
/// motionless windows asking for frames. Refreshing in place is the intended
/// behaviour, so the fix is to send what *changed* rather than to stop
/// capturing: a moving target still updates every round.
#[derive(Debug, Default)]
pub struct ThumbnailDigests {
    seen: std::collections::HashMap<String, u64>,
}

impl ThumbnailDigests {
    /// `true` when this frame differs from the last one recorded for `id`, a
    /// first sighting included. A capture failure (`None`) is a value like any
    /// other: the swap between a failure and a picture has to count as a
    /// change, or the tile's failure glyph would never appear.
    pub fn changed(&mut self, id: &str, frame: Option<(u32, u32, &[u8])>) -> bool {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        frame.hash(&mut hasher);
        let digest = hasher.finish();
        if self.seen.get(id) == Some(&digest) {
            return false;
        }
        self.seen.insert(id.to_owned(), digest);
        true
    }

    /// Forget what the target list no longer carries. The UI prunes its own
    /// thumbnail map against the very same list, so the two forget together --
    /// a target that comes back sends its first frame again instead of being
    /// held back as "unchanged" against a tile whose picture is gone (which
    /// would leave the loading glyph, and its animation, up for good).
    pub fn retain_listed(&mut self, listed: &[CaptureTarget]) {
        self.seen
            .retain(|id, _| listed.iter().any(|target| target.id == *id));
    }
}

/// What the list poll last told the picker about the settings a tile's badge
/// draws (task t260911-66da). Until this task `Msg::Targets(Ok)` re-read the
/// three fields itself on every poll -- the only path by which a removal made
/// on the settings screen reached the picker -- and that is exactly what kept
/// the list from being sent only when it changed. The poll already locks
/// `AppSettings` once a second for the auto-capture watch, so reading them
/// there costs nothing and the decision moves to the sender.
#[derive(Debug, Default)]
pub struct SentRegistrations {
    sent: Option<(
        Option<String>,
        Vec<crate::settings::AutoCaptureApp>,
        Vec<crate::settings::AutoCaptureFolder>,
    )>,
}

impl SentRegistrations {
    /// `true` when one of the three moved, a first poll included: the picker
    /// starts out holding the defaults, so the first snapshot is a change even
    /// when the file registers nothing.
    pub fn changed(
        &mut self,
        last_capture_executable: &Option<String>,
        apps: &[crate::settings::AutoCaptureApp],
        folders: &[crate::settings::AutoCaptureFolder],
    ) -> bool {
        let same = self
            .sent
            .as_ref()
            .is_some_and(|(sent_executable, sent_apps, sent_folders)| {
                sent_executable == last_capture_executable
                    && sent_apps.as_slice() == apps
                    && sent_folders.as_slice() == folders
            });
        if same {
            return false;
        }
        self.sent = Some((
            last_capture_executable.clone(),
            apps.to_vec(),
            folders.to_vec(),
        ));
        true
    }
}

/// The list half of the same idea (task t260911-66da). The poll re-sent an
/// identical list once a second forever, and each one cost the receiver a
/// settings lock, a whole-list comparison and -- with a folder rule
/// registered -- one `OpenProcess` per process.
///
/// A failure is a state of its own rather than "no targets": an empty list and
/// a failed list draw different things (`listed_ok` against `list_failed`).
/// Its *message* is deliberately not part of the state. The receiving arm does
/// read it -- since t260927-711d it is the error state's second line -- but the
/// poll words every failure with the one `list_error_reason` sentence
/// (t260928-08df), so two consecutive failures are one state either way.
#[derive(Debug, Default)]
pub struct SentList {
    sent: Option<Result<Vec<CaptureTarget>, ()>>,
}

impl SentList {
    pub fn changed(&mut self, listed: &Result<Vec<CaptureTarget>, String>) -> bool {
        let same = match (&self.sent, listed) {
            (Some(Ok(sent)), Ok(targets)) => sent == targets,
            (Some(Err(())), Err(_)) => true,
            _ => false,
        };
        if same {
            return false;
        }
        self.sent = Some(match listed {
            Ok(targets) => Ok(targets.clone()),
            Err(_) => Err(()),
        });
        true
    }
}

#[cfg(test)]
mod tests;
