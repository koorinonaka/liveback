//! The target picker's state (task124).
//!
//! The behaviour is transcribed from the React implementation that round-2
//! finished: `src/pages/TargetsPage.tsx` for the grid and its two empty states,
//! `src/useAppState.ts` (`reloadTargets` / `selectAndStartCapture`) for
//! selection, and `src/i18n/ja.ts` for the strings.

use crate::capture::targets::{sort_previous_first, CaptureTarget, CaptureTargetKind};
use crate::ui_state::locale::Locale;

// The picker's wording (task1150 moved it into `tr!`). Kept next to the logic
// that decides when each one applies rather than in one central table -- the
// pairing of message to condition is the part that has to stay right (task114).
crate::tr! {
    search_placeholder { ja: "対象を検索", en: "Search targets" }
    search_clear { ja: "検索をクリア", en: "Clear search" }
    alert_dismiss { ja: "閉じる", en: "Dismiss" }
    /// The list re-reads itself once a second, so an empty grid has nothing
    /// for the user to *do* -- the refresh button it used to offer was a verb
    /// for a job that was already running (task143). Saying so is the answer.
    empty {
        ja: "録画可能なウィンドウがありません。一覧は自動的に更新されます。",
        en: "No windows can be recorded. The list refreshes on its own."
    }
    error { ja: "対象一覧を取得できません。", en: "The target list could not be read." }
    selection_error {
        ja: "対象を選択できません。しばらくしてから再試行してください。",
        en: "That target could not be selected. Try again in a moment."
    }
    save_error {
        ja: "録画は開始されましたが、次回用の保存に失敗しました。",
        en: "Recording started, but it could not be remembered for next time."
    }
    minimized { ja: "最小化中", en: "Minimized" }
    /// task2430: a minimized window is listed but not pickable -- the encoder
    /// refuses the title-bar-sized client rect a minimized window reports, so
    /// the tile would only ever fail. Restoring it is the user's move.
    minimized_unavailable {
        ja: "最小化中は録画できません",
        en: "Cannot record while minimized"
    }
    unavailable { ja: "選択できません", en: "Unavailable" }

    /// W-11 (round5 §11): the badge on whichever screen the desktop calls home.
    monitor_primary { ja: "プライマリ", en: "Primary" }

    /// W-7 / W-8: the two halves of the picker's tab bar.
    tab_windows { ja: "ウィンドウ", en: "Windows" }
    tab_monitors { ja: "画面全体", en: "Screens" }
    /// W-12: the pill the tile being recorded carries. task235, round7 §2-15:
    /// the one word for a capture in progress. The picker tile, the transport
    /// chip and the history row all say it now -- the history's badge used to
    /// keep 録画中 on the grounds that a growing file is not the same as a
    /// picture on screen, and the round overruled that.
    recording { ja: "LIVE", en: "LIVE" }
    /// W-13: the button in the middle of a hovered tile. It replaced the old
    /// bottom-right 録画開始 pill, which said the same thing in a corner.
    capture_action { ja: "画面をキャプチャ", en: "Capture this screen" }
    /// The same slot on the tile already being recorded (task175). Round5 §3-B
    /// explicitly withdrew the title bar's stop button and §5 removed the
    /// target screen's alert; between them they left no way to stop a
    /// recording from inside the window at all, only the tray. The picker is
    /// where a recording is started and switched, so it is where stopping
    /// belongs -- the design's own model (Discord's share picker) puts "stop
    /// sharing" in the same place.
    stop_action { ja: "キャプチャを停止", en: "Stop capture" }

    /// The hover title over that button, from the mock (`.go` / `.go.stoprec`,
    /// round9 §1-1). The button says what it does in two words; the tooltip is
    /// where the sentence that answers "what happens to the recording I
    /// already have?" fits. Unconditional on purpose -- the mock puts no
    /// `data-when` on it, so it reads the same with zero captures running and
    /// the picker is not the place to invent a second wording.
    capture_action_title {
        ja: "録画中のものは止めずに、この対象のキャプチャを開始します",
        en: "Starts capturing this target without stopping what is already recording"
    }
    stop_action_title { ja: "この録画を停止", en: "Stop this recording" }
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
            TileAction::Stop => stop_action(locale),
        }
    }

    /// The `.slint` side compares an int: 0 none / 1 capture / 2 stop.
    pub fn index(self) -> i32 {
        match self {
            TileAction::None => 0,
            TileAction::Capture => 1,
            TileAction::Stop => 2,
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

/// `ja.targets.searchEmpty`.
pub fn search_empty_text(locale: Locale, query: &str) -> String {
    match locale {
        Locale::Ja => format!("「{query}」に一致する対象はありません。"),
        Locale::En => format!("Nothing matches “{query}”."),
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

/// The picker's banner, and how long each kind of trouble stays on it
/// (task143). The two have genuinely different lifetimes: the list re-reads
/// itself every second, so a *list* failure is answered by the next successful
/// read -- but an *action* failure ("could not select that window") was
/// answered by the very same tick, which meant the user had at most one second
/// to read the thing they had just caused.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Trouble {
    /// The list poll itself failed. Outlived by the next good list.
    List,
    /// Something the user asked for failed. Stays until they try again or say
    /// they have read it. Owned rather than `&'static str` (task157): the
    /// interesting message is the one `CaptureController::start` produced --
    /// "disk guard failed: ..." tells the user what to fix, "対象を選択できません"
    /// does not.
    Action(String),
}

#[derive(Clone, Debug, Default)]
pub struct TargetsAlert {
    current: Option<Trouble>,
}

impl TargetsAlert {
    pub fn text(&self, locale: Locale) -> Option<&str> {
        match &self.current {
            None => None,
            Some(Trouble::List) => Some(error(locale)),
            Some(Trouble::Action(message)) => Some(message),
        }
    }

    /// Only an action failure can be dismissed by hand, so the button is only
    /// worth drawing for one of the two.
    pub fn dismissible(&self) -> bool {
        matches!(self.current, Some(Trouble::Action(_)))
    }

    /// `true` when this actually cleared a list failure (task a015). The drain
    /// raises `flags.targets` off what moved, not off a message having arrived,
    /// so every mutator on this type has to answer that question.
    pub fn listed_ok(&mut self) -> bool {
        if self.current == Some(Trouble::List) {
            self.current = None;
            return true;
        }
        false
    }

    /// An action failure outranks a list failure: it is the one the user caused
    /// and the one they can act on. `true` when the banner changed -- a second
    /// consecutive list failure re-states what is already on screen.
    pub fn list_failed(&mut self) -> bool {
        if !self.dismissible() && self.current != Some(Trouble::List) {
            self.current = Some(Trouble::List);
            return true;
        }
        false
    }

    pub fn action_failed(&mut self, message: impl Into<String>) {
        self.current = Some(Trouble::Action(message.into()));
    }

    /// A fresh attempt supersedes whatever the last one said -- including a
    /// stale list failure, since the attempt is about to produce its own answer.
    pub fn start_attempted(&mut self) -> bool {
        self.current.take().is_some()
    }

    pub fn dismiss(&mut self) {
        if self.dismissible() {
            self.current = None;
        }
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
                                .is_some_and(|id| id.eq_ignore_ascii_case(executable))
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
        // `Msg::Capturing` rides the same 1s poll as the list, so this answers
        // the same question `set_targets` does (task a015). Dropping an
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

    /// Title or executable name, case-insensitive, same as the React filter --
    /// within the current tab. The search box is hidden on 画面全体 (there are
    /// at most a handful of screens), so a query left behind on the other tab
    /// must not silently empty this one.
    pub fn visible(&self) -> impl Iterator<Item = &CaptureTarget> {
        let query = if self.tab == PickerTab::Windows {
            self.search.to_lowercase()
        } else {
            String::new()
        };
        self.in_tab().filter(move |target| {
            target.title.to_lowercase().contains(&query)
                || target
                    .executable_name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&query))
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

/// Top-right pill: *why you cannot pick this one*. Distinct from
/// [`state_label`], which is merely a state the window happens to be in
/// (task114 / round2 §5) -- and from "recording is in progress", which is
/// neither and lives above the grid.
pub fn blocked_reason(locale: Locale, target: &CaptureTarget) -> Option<&str> {
    if target.selectable {
        return None;
    }
    // The enumeration's own wording wins where it has one (a window with no
    // readable process); minimized is the case it deliberately leaves to the UI
    // (task2430).
    if let Some(reason) = target.unavailable_reason.as_deref() {
        return Some(reason);
    }
    Some(if target.minimized {
        minimized_unavailable(locale)
    } else {
        unavailable(locale)
    })
}

/// Bottom-left pill.
pub fn state_label(locale: Locale, target: &CaptureTarget) -> Option<&'static str> {
    target.minimized.then_some(minimized(locale))
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
/// Its *message* is deliberately not part of the state, because the receiving
/// arm does not read it -- it clears the list and raises one fixed banner --
/// so two consecutive failures are one state however they are worded.
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
mod tests {
    use super::*;

    /// task t260911-66da: the 1s poll re-sent an identical list forever and
    /// the receiver paid for every one of them. Every transition the picker
    /// draws differently still has to get through -- including the one between
    /// an empty list and a failed one, which is why `Err` is a state rather
    /// than "no targets".
    #[test]
    fn a_list_that_did_not_move_is_not_sent_again() {
        let mut sent = SentList::default();
        let one = vec![target("1", "Overwatch", Some("game.exe"))];
        let two = vec![
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
        ];
        assert!(sent.changed(&Ok(one.clone())), "the first list");
        assert!(!sent.changed(&Ok(one.clone())), "the same list again");
        assert!(sent.changed(&Ok(two)), "a window opened");
        let nothing: Vec<CaptureTarget> = Vec::new();
        assert!(sent.changed(&Ok(nothing.clone())), "everything closed");
        assert!(!sent.changed(&Ok(nothing)), "still nothing");
        assert!(
            sent.changed(&Err("ウィンドウ一覧を取得できません".to_owned())),
            "an empty list and a failed list draw differently"
        );
        assert!(
            !sent.changed(&Err("違う文言".to_owned())),
            "one failure is one state whatever it is worded"
        );
        assert!(sent.changed(&Ok(one)), "the list came back");
    }

    /// A retitled window is what the receiver used to notice for itself.
    #[test]
    fn a_retitled_window_is_still_worth_sending() {
        let mut sent = SentList::default();
        assert!(sent.changed(&Ok(vec![target("1", "Overwatch", Some("game.exe"))])));
        assert!(
            sent.changed(&Ok(vec![target("1", "Overwatch — 2", Some("game.exe"))])),
            "the title is part of what a tile draws"
        );
    }

    /// The registration lists have to follow a removal made on the settings
    /// screen (task1980 / task3670) -- that is the feature the throttle could
    /// break, so the change has to be seen on the sending side instead.
    #[test]
    fn a_registration_is_sent_only_when_one_of_the_three_moved() {
        use crate::settings::{AutoCaptureApp, AutoCaptureFolder};

        let mut sent = SentRegistrations::default();
        let none: Option<String> = None;
        let no_apps: Vec<AutoCaptureApp> = Vec::new();
        let no_folders: Vec<AutoCaptureFolder> = Vec::new();
        assert!(
            sent.changed(&none, &no_apps, &no_folders),
            "the first snapshot, even an empty one"
        );
        assert!(
            !sent.changed(&none, &no_apps, &no_folders),
            "nothing registered, nothing to say"
        );

        let apps = vec![AutoCaptureApp {
            name: "game.exe".into(),
            ..AutoCaptureApp::default()
        }];
        assert!(sent.changed(&none, &apps, &no_folders), "registered");
        assert!(!sent.changed(&none, &apps, &no_folders), "same again");
        assert!(
            sent.changed(&none, &no_apps, &no_folders),
            "unregistered on the settings screen"
        );

        let folders = vec![AutoCaptureFolder {
            path: "C:\\Games".into(),
            ..AutoCaptureFolder::default()
        }];
        assert!(sent.changed(&none, &no_apps, &folders), "a folder rule");
        assert!(!sent.changed(&none, &no_apps, &folders), "same again");

        assert!(
            sent.changed(&Some("game.exe".to_owned()), &no_apps, &folders),
            "the last-used executable moved on its own"
        );
        assert!(
            !sent.changed(&Some("game.exe".to_owned()), &no_apps, &folders),
            "same again"
        );
    }

    /// `last_capture_executable` used to reach the preselect on every list
    /// poll. It has its own message now, so the preselect has to be reachable
    /// without one (task t260911-66da).
    #[test]
    fn the_last_used_executable_can_preselect_without_a_new_list() {
        let mut state = listed(vec![
            target("1", "Notepad", Some("notepad.exe")),
            target("2", "Overwatch", Some("game.exe")),
        ]);
        state.selected_id = None;
        assert!(state.reselect(Some("game.exe")), "the preselect moved");
        assert_eq!(state.selected_id(), Some("2"));
        assert!(!state.reselect(Some("game.exe")), "and stays where it is");
        // A selection that is still in the list outranks the preselect, the
        // same way `set_targets` has always had it.
        assert!(
            !state.reselect(Some("notepad.exe")),
            "the selection is kept"
        );
        assert_eq!(state.selected_id(), Some("2"));
    }

    /// W-10 / W-11 (task165). The number is the enumeration position the user
    /// counts from, so it is 1-based and the resolution is the monitor
    /// rectangle -- taskbar included, because the recording includes it.
    #[test]
    fn a_monitor_reads_as_its_number_and_its_resolution() {
        assert_eq!(
            monitor_display_name(Locale::Ja, 1, 2560, 1440),
            "画面 1 — 2560×1440"
        );
        assert_eq!(
            monitor_display_name(Locale::Ja, 2, 1920, 1080),
            "画面 2 — 1920×1080"
        );
        // The primary badge is its own label, not part of the name: the picker
        // draws it beside the title rather than inside it.
        assert!(
            !monitor_display_name(Locale::Ja, 1, 800, 600).contains(monitor_primary(Locale::Ja))
        );
    }

    /// The tile draws the two halves in opposite corners (task170), so the
    /// split has to survive the exact separator `monitor_display_name` writes.
    #[test]
    fn a_monitor_badge_takes_the_name_and_the_resolution_apart() {
        let name = monitor_display_name(Locale::Ja, 2, 1920, 1080);
        assert_eq!(monitor_name_and_resolution(&name), ("画面 2", "1920×1080"));
        // A window title is not a screen label and keeps all of itself.
        assert_eq!(
            monitor_name_and_resolution("Visual Studio Code"),
            ("Visual Studio Code", "")
        );
    }

    fn monitor(id: &str, number: usize) -> CaptureTarget {
        CaptureTarget {
            kind: CaptureTargetKind::Monitor,
            primary: number == 1,
            id: id.into(),
            window_handle: format!("0x{id}"),
            process_id: 0,
            title: monitor_display_name(Locale::Ja, number, 2560, 1440),
            executable_id: None,
            executable_name: None,
            minimized: false,
            selectable: true,
            unavailable_reason: None,
        }
    }

    /// The four rules the open modal's grid lives by (design §5).
    #[test]
    fn the_order_the_user_is_looking_at_survives_the_one_second_poll() {
        let listed = || {
            vec![
                target("1", "Overwatch", Some("game.exe")),
                target("2", "Notepad", Some("notepad.exe")),
                target("3", "Code", Some("code.exe")),
            ]
        };

        // Nothing on screen yet: the list arrives in the order it was given,
        // which is the order `sort_for_open` just decided.
        let first = keep_previous_order(&[], listed());
        assert_eq!(ids(&first), ["1", "2", "3"]);

        // Existing ids keep their positions even when the fresh list disagrees.
        let shuffled = vec![
            target("3", "Code", Some("code.exe")),
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
        ];
        let held = keep_previous_order(&order(&["1", "2", "3"]), shuffled);
        assert_eq!(ids(&held), ["1", "2", "3"]);

        // A window opened while the modal was up: it goes on the end, not
        // wherever the enumeration happened to put it.
        let mut grown = listed();
        grown.insert(0, target("4", "New", Some("new.exe")));
        let appended = keep_previous_order(&order(&["1", "2", "3"]), grown);
        assert_eq!(ids(&appended), ["1", "2", "3", "4"]);

        // A window that closed is the only thing allowed to move the rest.
        let mut shrunk = listed();
        shrunk.remove(1);
        let dropped = keep_previous_order(&order(&["1", "2", "3"]), shrunk);
        assert_eq!(ids(&dropped), ["1", "3"]);
    }

    fn order(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn ids(targets: &[CaptureTarget]) -> Vec<&str> {
        targets.iter().map(|target| target.id.as_str()).collect()
    }

    /// The bug this pair exists for: starting a recording rewrites
    /// `last_capture_executable`, and the next poll used to sort on it and pull
    /// the tile the user had just clicked to the front of the grid.
    #[test]
    fn only_opening_the_modal_reorders_the_grid() {
        let mut state = listed(vec![
            target("1", "Notepad", Some("notepad.exe")),
            target("2", "Overwatch", Some("game.exe")),
        ]);
        state.sort_for_open(Some("GAME.EXE"));
        assert_eq!(ids(&state.targets), ["2", "1"]);

        // The same list, polled again with the same preference: nothing moves,
        // because the modal is already open.
        state.refresh_targets(
            vec![
                target("1", "Notepad", Some("notepad.exe")),
                target("2", "Overwatch", Some("game.exe")),
            ],
            Some("game.exe"),
        );
        assert_eq!(ids(&state.targets), ["2", "1"]);
    }

    #[test]
    fn each_tab_only_counts_and_shows_its_own_kind() {
        let mut state = listed(vec![
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
            monitor("m1", 1),
        ]);
        assert_eq!(state.tab(), PickerTab::Windows);
        assert_eq!(state.total(), 2);
        assert_eq!(
            ids(&state.visible().cloned().collect::<Vec<_>>()),
            ["1", "2"]
        );

        state.set_tab(PickerTab::Monitors);
        assert_eq!(state.total(), 1);
        assert_eq!(state.visible_count(), 1);

        // A query typed on the other tab does not hide the screens: the search
        // box is not even drawn here.
        state.set_search("overwatch");
        assert_eq!(state.visible_count(), 1);
        state.set_tab(PickerTab::Windows);
        assert_eq!(state.visible_count(), 1);
    }

    #[test]
    fn the_tab_index_round_trips_through_the_segment_control() {
        assert_eq!(PickerTab::from_index(0), PickerTab::Windows);
        assert_eq!(PickerTab::from_index(1), PickerTab::Monitors);
        // Anything else is the default rather than a panic: the int comes from
        // the `.slint` side, which has no enum to be wrong about.
        assert_eq!(PickerTab::from_index(7), PickerTab::Windows);
        assert_eq!(PickerTab::Monitors.index(), 1);
    }

    fn target(id: &str, title: &str, executable: Option<&str>) -> CaptureTarget {
        CaptureTarget {
            kind: crate::capture::targets::CaptureTargetKind::Window,
            primary: false,
            id: id.into(),
            window_handle: format!("0x{id}"),
            process_id: 1,
            title: title.into(),
            executable_id: executable.map(str::to_owned),
            executable_name: executable.map(str::to_owned),
            minimized: false,
            selectable: executable.is_some(),
            unavailable_reason: None,
        }
    }

    /// task4290: the picker's poller re-captures a motionless window every
    /// 120ms and used to push each identical frame into the model, which is a
    /// redraw per capture. Only a frame that differs is worth sending.
    #[test]
    fn only_a_changed_thumbnail_frame_is_worth_sending() {
        let mut digests = ThumbnailDigests::default();
        let still = vec![7u8; 16];

        assert!(digests.changed("1", Some((2, 2, &still))), "first sighting");
        assert!(!digests.changed("1", Some((2, 2, &still))), "same picture");
        // Another target's identical picture is still that target's first.
        assert!(digests.changed("2", Some((2, 2, &still))), "other target");

        let moved = vec![9u8; 16];
        assert!(
            digests.changed("1", Some((2, 2, &moved))),
            "the window moved"
        );
        // Same bytes, different shape: a resized window is a change.
        assert!(digests.changed("1", Some((4, 1, &moved))), "resized");
    }

    /// A capture failure is a value, not a gap: `None` reaches the UI as
    /// `TileMedia::None`, so the trips in and out of it have to be sent or the
    /// failure glyph would never appear (and never clear).
    #[test]
    fn a_capture_failure_counts_as_a_change_in_both_directions() {
        let mut digests = ThumbnailDigests::default();
        let picture = vec![1u8; 4];

        assert!(digests.changed("1", Some((1, 1, &picture))));
        assert!(digests.changed("1", None), "picture -> failure");
        assert!(!digests.changed("1", None), "still failing");
        assert!(digests.changed("1", Some((1, 1, &picture))), "back");
    }

    /// The digests are pruned against the same list the UI prunes its thumbnail
    /// map against, so a target that leaves and comes back sends again --
    /// otherwise its tile would sit on the loading glyph for good.
    #[test]
    fn a_target_that_left_the_list_sends_its_frame_again_when_it_returns() {
        let mut digests = ThumbnailDigests::default();
        let picture = vec![3u8; 4];
        let one = target("1", "One", Some("one.exe"));
        let two = target("2", "Two", Some("two.exe"));

        assert!(digests.changed("1", Some((1, 1, &picture))));
        assert!(digests.changed("2", Some((1, 1, &picture))));

        digests.retain_listed(std::slice::from_ref(&two));
        assert!(digests.changed("1", Some((1, 1, &picture))), "1 came back");
        assert!(
            !digests.changed("2", Some((1, 1, &picture))),
            "2 never left"
        );

        digests.retain_listed(&[one, two]);
        assert!(!digests.changed("1", Some((1, 1, &picture))), "both kept");
    }

    fn listed(targets: Vec<CaptureTarget>) -> TargetsState {
        let mut state = TargetsState::default();
        state.set_targets(targets, None);
        state
    }

    #[test]
    fn search_matches_title_case_insensitively() {
        let mut state = listed(vec![
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
        ]);
        state.set_search("overWATCH");
        let visible: Vec<_> = state.visible().map(|target| target.id.as_str()).collect();
        assert_eq!(visible, ["1"]);
    }

    #[test]
    fn search_also_matches_the_executable_name() {
        let mut state = listed(vec![
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
        ]);
        state.set_search("notepad.EXE");
        let visible: Vec<_> = state.visible().map(|target| target.id.as_str()).collect();
        assert_eq!(visible, ["2"]);
    }

    #[test]
    fn a_target_without_an_executable_name_is_matched_on_its_title_alone() {
        let mut state = listed(vec![target("1", "Overwatch", None)]);
        state.set_search("watch");
        assert_eq!(state.visible_count(), 1);
        state.set_search("game");
        assert_eq!(state.visible_count(), 0);
    }

    #[test]
    fn empty_search_shows_everything() {
        let state = listed(vec![
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
        ]);
        assert_eq!(state.visible_count(), 2);
        assert_eq!(state.total(), 2);
        assert_eq!(state.empty(), None);
    }

    #[test]
    fn a_filter_that_matched_nothing_is_not_the_same_nothing_as_an_empty_list() {
        let mut state = listed(vec![target("1", "Overwatch", Some("game.exe"))]);
        state.set_search("nothing here");
        assert_eq!(state.empty(), Some(TargetsEmpty::NoMatch));
        assert_eq!(
            search_empty_text(Locale::Ja, "nothing here"),
            "「nothing here」に一致する対象はありません。"
        );

        let state = listed(vec![]);
        assert_eq!(state.empty(), Some(TargetsEmpty::NoTargets));
    }

    #[test]
    fn unselectable_targets_carry_their_own_reason_and_fall_back_to_a_generic_one() {
        let mut unavailable = target("1", "Overwatch", None);
        assert_eq!(
            blocked_reason(Locale::Ja, &unavailable),
            Some(super::unavailable(Locale::Ja))
        );
        unavailable.unavailable_reason = Some("プロセス情報を取得できません".into());
        // Even minimized: the enumeration only leaves the reason empty when
        // being minimized is the whole of it (task2430).
        unavailable.minimized = true;
        assert_eq!(
            blocked_reason(Locale::Ja, &unavailable),
            Some("プロセス情報を取得できません")
        );
        assert_eq!(
            blocked_reason(Locale::Ja, &target("2", "Notepad", Some("n.exe"))),
            None
        );
    }

    /// task2430. Minimized is both: the bottom pill still names the state,
    /// and -- since the encoder refuses the title-bar-sized client rect a
    /// minimized window reports -- it is now also why the tile cannot be
    /// picked. Every start path is gated on `selectable`, so clearing that one
    /// flag is what makes the tile inert; the pill is what keeps a dimmed tile
    /// from being unexplained.
    #[test]
    fn minimized_is_both_a_state_and_the_reason_you_cannot_pick_it() {
        let mut minimized = target("1", "Overwatch", Some("game.exe"));
        minimized.minimized = true;
        minimized.selectable = false;
        assert_eq!(
            state_label(Locale::Ja, &minimized),
            Some(super::minimized(Locale::Ja))
        );
        assert_eq!(
            blocked_reason(Locale::Ja, &minimized),
            Some(minimized_unavailable(Locale::Ja))
        );
        let state = listed(vec![minimized]);
        assert!(!state.can_start("1"));
        assert_eq!(state.tile_action("1"), TileAction::None);
    }

    /// The start path demanded an `executable_id` from every target, which a
    /// screen never has, so screen tiles were selectable but inert (task165).
    #[test]
    fn a_screen_starts_without_an_executable_identity_but_a_window_does_not() {
        assert!(!TargetsState::needs_executable(CaptureTargetKind::Monitor));
        assert!(TargetsState::needs_executable(CaptureTargetKind::Window));
    }

    #[test]
    fn only_selectable_targets_can_start_and_recording_freezes_only_its_own_tile() {
        let mut state = listed(vec![
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Ghost", None),
            target("3", "Notepad", Some("notepad.exe")),
        ]);
        assert!(state.can_start("1"));
        assert!(!state.can_start("2"), "no process identity, no capture");
        assert!(!state.can_start("missing"));

        // Recording tile 1: every other tile still starts one more beside it
        // (task2030 判断1), and the one already recording is the stop half of
        // the toggle rather than a second start.
        state.set_running(vec![("sess-a".into(), "0x1".into())]);
        assert!(!state.can_start("1"));
        assert_eq!(state.recording_session("1"), Some("sess-a"));
        assert!(state.can_start("3"));
        assert!(!state.can_start("2"), "still no process identity");

        // A second capture beside the first freezes neither of the others.
        state.set_running(vec![
            ("sess-a".into(), "0x1".into()),
            ("sess-b".into(), "0x3".into()),
        ]);
        assert_eq!(state.running_count(), 2);
        assert!(!state.can_start("1"));
        assert!(!state.can_start("3"));
        assert_eq!(state.recording_session("3"), Some("sess-b"));

        // A capture that ended leaves nothing inert behind.
        state.set_running(Vec::new());
        assert_eq!(state.recording_session("1"), None);
        assert!(!state.capturing());
        assert!(state.can_start("1"));
    }

    /// Task2030. The poll is ground truth: a click's optimism lasts exactly
    /// until the next one, whether or not the start it hoped for landed.
    #[test]
    fn a_clicked_tile_reads_as_recording_until_the_next_poll_says_otherwise() {
        let mut state = listed(vec![target("1", "Overwatch", Some("game.exe"))]);
        state.set_capturing_target("1");
        assert!(state.capturing());
        assert_eq!(state.tile_action("1"), TileAction::Stop);
        // ...but there is no session to stop yet, so the click has nothing to
        // send and the handler falls through rather than stopping the wrong one.
        assert_eq!(state.recording_session("1"), None);
        // A start that was refused: the poll comes back empty and the tile
        // offers to start again.
        state.set_running(Vec::new());
        assert_eq!(state.tile_action("1"), TileAction::Capture);
        assert!(state.can_start("1"));
    }

    /// The warning bar counts captures, and nothing else does (task2030 判断2).
    #[test]
    fn the_running_count_is_what_the_threshold_bar_reads() {
        let mut state = listed(vec![
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
            target("3", "Paint", Some("paint.exe")),
        ]);
        assert_eq!(state.running_count(), 0);
        state.set_running(vec![
            ("a".into(), "0x1".into()),
            ("b".into(), "0x2".into()),
            ("c".into(), "0x3".into()),
        ]);
        assert_eq!(state.running_count(), 3);
        // Still no refusal anywhere: task2080 removed the cap and task2030 adds
        // no count-based one back.
        state.set_running(Vec::new());
        assert!(state.can_start("1"));
    }

    #[test]
    fn a_refresh_preselects_the_last_used_executable() {
        let mut state = TargetsState::default();
        state.set_targets(
            vec![
                target("1", "Notepad", Some("notepad.exe")),
                target("2", "Overwatch", Some("game.exe")),
            ],
            Some("GAME.EXE"),
        );
        assert_eq!(state.selected_id(), Some("2"));
    }

    #[test]
    fn a_refresh_keeps_a_selection_that_is_still_in_the_list() {
        let mut state = TargetsState::default();
        state.set_targets(vec![target("1", "Notepad", Some("notepad.exe"))], None);
        state.select("1");
        state.set_targets(
            vec![
                target("1", "Notepad", Some("notepad.exe")),
                target("2", "Overwatch", Some("game.exe")),
            ],
            Some("game.exe"),
        );
        assert_eq!(state.selected_id(), Some("1"));
    }

    #[test]
    fn a_selection_that_left_the_list_does_not_survive_the_refresh() {
        let mut state = TargetsState::default();
        state.set_targets(vec![target("1", "Notepad", Some("notepad.exe"))], None);
        state.select("1");
        state.set_targets(vec![target("2", "Overwatch", Some("game.exe"))], None);
        assert_eq!(state.selected_id(), None);
    }

    #[test]
    fn a_list_failure_is_cleared_by_the_next_good_list() {
        let mut alert = TargetsAlert::default();
        assert_eq!(alert.text(Locale::Ja), None);
        alert.list_failed();
        assert_eq!(alert.text(Locale::Ja), Some(error(Locale::Ja)));
        assert!(!alert.dismissible());
        alert.listed_ok();
        assert_eq!(alert.text(Locale::Ja), None);
    }

    #[test]
    fn an_action_failure_survives_the_one_second_list_poll() {
        // The bug this rule exists for: `Msg::Targets(Ok)` used to clear the
        // banner, so a selection failure was on screen for at most one tick.
        let mut alert = TargetsAlert::default();
        alert.action_failed(selection_error(Locale::Ja));
        alert.listed_ok();
        assert_eq!(alert.text(Locale::Ja), Some(selection_error(Locale::Ja)));
        alert.listed_ok();
        assert_eq!(alert.text(Locale::Ja), Some(selection_error(Locale::Ja)));
    }

    #[test]
    fn an_action_failure_outranks_a_list_failure() {
        let mut alert = TargetsAlert::default();
        alert.action_failed(save_error(Locale::Ja));
        alert.list_failed();
        assert_eq!(alert.text(Locale::Ja), Some(save_error(Locale::Ja)));
    }

    #[test]
    fn only_a_new_attempt_or_the_close_button_takes_an_action_failure_down() {
        let mut alert = TargetsAlert::default();
        alert.action_failed(selection_error(Locale::Ja));
        assert!(alert.dismissible());
        alert.dismiss();
        assert_eq!(alert.text(Locale::Ja), None);

        alert.action_failed(selection_error(Locale::Ja));
        alert.start_attempted();
        assert_eq!(alert.text(Locale::Ja), None);
    }

    #[test]
    fn an_action_failure_carries_the_reason_it_was_given() {
        // task157: the banner says what actually went wrong, not a generic
        // stand-in, so a disk-guard failure is actionable.
        let mut alert = TargetsAlert::default();
        alert.action_failed("disk guard failed: 指定されたパスが見つかりません。 (0x80070003)");
        assert_eq!(
            alert.text(Locale::Ja),
            Some("disk guard failed: 指定されたパスが見つかりません。 (0x80070003)")
        );
        assert!(alert.dismissible());
        // A failure with no message of its own still falls back to the generic.
        alert.action_failed(selection_error(Locale::Ja));
        assert_eq!(alert.text(Locale::Ja), Some(selection_error(Locale::Ja)));
    }

    #[test]
    fn a_list_failure_has_no_close_button_because_dismissing_it_would_be_a_lie() {
        // It would come straight back on the next failed poll.
        let mut alert = TargetsAlert::default();
        alert.list_failed();
        alert.dismiss();
        assert_eq!(alert.text(Locale::Ja), Some(error(Locale::Ja)));
    }

    #[test]
    fn an_unselectable_window_is_never_preselected() {
        let mut unselectable = target("1", "Overwatch", Some("game.exe"));
        unselectable.selectable = false;
        let mut state = TargetsState::default();
        state.set_targets(vec![unselectable], Some("game.exe"));
        assert_eq!(state.selected_id(), None);
    }

    /// Round5 §5 + task175: the tile being recorded is the one that stops.
    #[test]
    fn only_the_recording_tile_offers_to_stop() {
        let mut state = listed(vec![
            target("1", "Overwatch", Some("game.exe")),
            target("2", "Ghost", None),
            target("3", "Notepad", Some("notepad.exe")),
        ]);

        // Nothing recording: every selectable tile starts one.
        assert_eq!(state.tile_action("1"), TileAction::Capture);
        assert_eq!(state.tile_action("3"), TileAction::Capture);
        // No process identity, so no action at all -- unchanged by task175.
        assert_eq!(state.tile_action("2"), TileAction::None);
        assert_eq!(state.tile_action("missing"), TileAction::None);

        state.set_running(vec![("sess-a".into(), "0x1".into())]);
        assert_eq!(state.tile_action("1"), TileAction::Stop);
        // The others start one more beside it (task2030 判断1); they do not stop.
        assert_eq!(state.tile_action("3"), TileAction::Capture);
        assert_eq!(state.tile_action("2"), TileAction::None);

        // Two running: two stop offers, and no tile is inert for the count.
        state.set_running(vec![
            ("sess-a".into(), "0x1".into()),
            ("sess-b".into(), "0x3".into()),
        ]);
        assert_eq!(state.tile_action("1"), TileAction::Stop);
        assert_eq!(state.tile_action("3"), TileAction::Stop);

        // And the offer goes away with the recording.
        state.set_running(Vec::new());
        assert_eq!(state.tile_action("1"), TileAction::Capture);
    }

    #[test]
    fn each_tile_action_carries_its_own_word() {
        assert_eq!(
            TileAction::Capture.label(Locale::Ja),
            capture_action(Locale::Ja)
        );
        assert_eq!(TileAction::Stop.label(Locale::Ja), stop_action(Locale::Ja));
        assert_eq!(TileAction::None.label(Locale::Ja), "");
        // The ints the `.slint` side compares against.
        assert_eq!(TileAction::None.index(), 0);
        assert_eq!(TileAction::Capture.index(), 1);
        assert_eq!(TileAction::Stop.index(), 2);
    }

    /// The byte string, not a shape: these two are the mock's own `title`
    /// attributes (`mock/liveback-mock.html` 921 / 928 / 948) and task2180
    /// forbids shortening or rewording them, so the test pins the text rather
    /// than asserting it is non-empty.
    #[test]
    fn the_hover_titles_are_the_mocks_own_sentences() {
        assert_eq!(
            capture_action_title(Locale::Ja),
            "録画中のものは止めずに、この対象のキャプチャを開始します"
        );
        assert_eq!(
            capture_action_title(Locale::En),
            "Starts capturing this target without stopping what is already recording"
        );
        assert_eq!(stop_action_title(Locale::Ja), "この録画を停止");
        assert_eq!(stop_action_title(Locale::En), "Stop this recording");
    }

    /// task a015. The list poll re-sends the same windows once a second, and
    /// the drain must be able to tell that apart from a window appearing.
    #[test]
    fn the_same_list_twice_is_not_a_change() {
        let mut state = TargetsState::default();
        let listed = vec![
            target("1", "Notepad", Some("notepad.exe")),
            monitor("m1", 1),
        ];
        let first = state.refresh_targets(listed.clone(), None);
        assert!(first.listed, "the first list is new by definition");
        let again = state.refresh_targets(listed, None);
        assert!(!again.any(), "an identical poll moves nothing: {again:?}");
        assert_eq!(again.first_diff, None);
    }

    /// A window opening is a change, and the instrument names which one -- a
    /// count alone cannot tell a new window from a title that ticked.
    #[test]
    fn a_window_appearing_is_a_change_and_names_itself() {
        let mut state = TargetsState::default();
        state.refresh_targets(vec![target("1", "Notepad", Some("notepad.exe"))], None);
        let change = state.refresh_targets(
            vec![
                target("1", "Notepad", Some("notepad.exe")),
                target("2", "Overwatch", Some("game.exe")),
            ],
            None,
        );
        assert!(change.listed);
        assert_eq!(change.first_diff.as_deref(), Some("Overwatch"));
    }

    /// The same window under a new title is a change: the tile draws the title.
    #[test]
    fn a_retitled_window_is_a_change() {
        let mut state = TargetsState::default();
        state.refresh_targets(vec![target("1", "Notepad", Some("notepad.exe"))], None);
        let change =
            state.refresh_targets(vec![target("1", "Notepad *", Some("notepad.exe"))], None);
        assert!(change.listed);
        assert_eq!(change.first_diff.as_deref(), Some("Notepad *"));
    }

    /// The order ruling, written down as a test (task a015 Step 4). The poll's
    /// own enumeration order is not stable, and `refresh_targets` normalises it
    /// back onto the order the user is looking at -- so a reordered poll of the
    /// same windows draws the identical grid and is not a change.
    #[test]
    fn a_reordered_poll_of_the_same_windows_is_not_a_change() {
        let mut state = TargetsState::default();
        let first = target("1", "Notepad", Some("notepad.exe"));
        let second = target("2", "Overwatch", Some("game.exe"));
        state.refresh_targets(vec![first.clone(), second.clone()], None);
        let change = state.refresh_targets(vec![second, first], None);
        assert!(
            !change.any(),
            "keep_previous_order puts them back: {change:?}"
        );
    }

    /// `set_targets` -- the open-modal path does reshuffle, so there the same
    /// windows in a new order *are* a different grid.
    #[test]
    fn a_reordered_list_through_set_targets_is_a_change() {
        let mut state = TargetsState::default();
        let first = target("1", "Notepad", Some("notepad.exe"));
        let second = target("2", "Overwatch", Some("game.exe"));
        state.set_targets(vec![first.clone(), second.clone()], None);
        assert!(state.set_targets(vec![second, first], None).listed);
    }

    /// The preselection moving is a change even when the list did not.
    #[test]
    fn the_preselection_moving_is_a_change_on_its_own() {
        let mut state = TargetsState::default();
        let listed = vec![target("1", "Overwatch", Some("game.exe"))];
        assert!(state.set_targets(listed.clone(), None).listed);
        let change = state.set_targets(listed, Some("game.exe"));
        assert!(!change.listed, "the same list");
        assert!(change.selection, "but a tile is preselected now");
    }

    /// `Msg::Capturing` rides the same 1s poll, so it answers the same question.
    #[test]
    fn an_unchanged_running_list_is_not_a_change() {
        let mut state = TargetsState::default();
        let running = vec![("sess-a".to_owned(), "0x1".to_owned())];
        assert!(state.set_running(running.clone()), "nothing to something");
        assert!(!state.set_running(running.clone()), "the same poll again");
        assert!(state.set_running(Vec::new()), "the recording stopped");
        // The optimistic tile from a click is dropped here, and that is a
        // change however still the running map is.
        state.set_capturing_target("1");
        assert!(state.set_running(Vec::new()), "pending_start was cleared");
        assert!(!state.set_running(Vec::new()));
    }

    /// The banner is the other half of what the picker pane draws.
    #[test]
    fn the_alert_reports_only_the_transitions() {
        let mut alert = TargetsAlert::default();
        assert!(!alert.listed_ok(), "nothing to clear");
        assert!(alert.list_failed(), "the banner appeared");
        assert!(!alert.list_failed(), "it was already saying that");
        assert!(alert.listed_ok(), "the banner went away");
        assert!(!alert.listed_ok());
        alert.action_failed("disk guard failed");
        assert!(!alert.list_failed(), "an action failure outranks it");
        assert!(!alert.listed_ok(), "and survives a good list");
        assert!(alert.start_attempted(), "a fresh attempt clears it");
        assert!(!alert.start_attempted());
    }
}
