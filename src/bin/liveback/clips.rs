//! The クリップ page (task2980): the saved-clip folder, listed.
//!
//! Shape, and why it is not the history page's:
//!
//! - There is no index and no database. `livia::clips::list` re-scans the
//!   folder every time the pane is entered, which is what makes a file the user
//!   dropped in with Explorer appear -- and one they deleted disappear --
//!   without the app restarting. Same decision `ring_buffer::sessions` made.
//! - The size and the date come off `fs::metadata` in that scan. The comment,
//!   the duration and the thumbnail do not: each costs a shell round trip in
//!   the milliseconds, and paying that for a whole folder on every open is
//!   exactly the freeze the task warned about. They are fetched per row, for
//!   the rows on screen, and cached against the file's modification time.
//! - Those fetches get their own thread rather than riding the session worker's
//!   queue. `IShellItemImageFactory::GetImage` runs the media thumbnail
//!   provider, which can take hundreds of milliseconds on a file the shell has
//!   never seen; queued behind `ListSessions` / `Load` it would delay the things
//!   the user actually pressed for.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender};
use livia::clips::{ClipDetails, ClipEntry};
use livia::playback::{PlaybackCommand, PlaybackEngine};
use livia::settings::AppSettings;
use livia::ui_state::clip_player::{self, ClipPlayer, ClipPlayerState};
use livia::ui_state::clips::{
    self as clips_ui, ClickOutcome, ClipEdit, ClipGrouping, ClipPlacement, ClipSelection,
    PendingComment,
};
use livia::ui_state::playback as pb;
use livia::ui_state::sessions;
use livia::ui_state::timeline as tl;
use slint::{ComponentHandle, Image, Model, VecModel};

use super::review::paused_stage_image;
use super::settings_page::{commit_settings, settings_snapshot};
use super::tr_locale;
use super::{image_from_rgba, AppWindow, ClipLabels, ClipRow, ClipVm, Cmd, Icons, MenuEntry, Msg};

/// What a clip's own file says about it, once the worker has asked.
///
/// The duration was dropped on the floor between task3020 (§1-3 took the length
/// column off the screen, and a tile has nowhere to put it) and task3520, which
/// needs it again for a different reason: `PlaybackEngine::start_file` builds a
/// one-segment timeline out of it, so without a length the clip has no end to
/// seek within. It always cost nothing extra -- it is the same property-store
/// round trip the comment needs, and `livia::clips::details` never stopped
/// returning it.
#[derive(Default)]
struct Detail {
    comment: Option<String>,
    /// `None` for a file the shell reports no `System.Media.Duration` for.
    /// Distinct from "not asked yet", which is the whole `Detail` being absent
    /// from the cache -- the two must not be confused, or a click landing
    /// before the worker's answer would be reported as an unreadable clip.
    duration_100ns: Option<i64>,
    thumbnail: Option<Image>,
}

/// UI-thread state for the clip screen.
pub(super) struct Clips {
    entries: Vec<ClipEntry>,
    /// Cached per path, invalidated by the file's modification time -- so a
    /// clip whose comment task2990 rewrites is re-read rather than stale.
    ///
    /// ponytail: kept for the life of the process, like the history's
    /// thumbnails. A clip folder is tens of files, not the hundreds of sessions
    /// a ring buffer accumulates; add an LRU if that ever stops being true.
    details: HashMap<PathBuf, (SystemTime, Detail)>,
    /// Paths already handed to the worker, so a re-render does not re-request
    /// what is merely still in flight.
    requested: HashSet<PathBuf>,
    /// Comments committed but not yet read back off the file (t260916-568d).
    ///
    /// The write moves the file's modification time, which is the key
    /// `details` is filed under, so for the length of the round trip the row's
    /// own cache entry is not allowed to answer -- and the tile used to draw an
    /// empty comment over a discarded thumbnail until the re-read landed. The
    /// guess covers exactly that window; `detailed` drops it the moment a read
    /// that saw the write arrives, and `comment_write_failed` drops it when the
    /// write is refused, which is what puts the original text back.
    ///
    /// A map rather than one slot because nothing stops a second clip being
    /// edited while the first is still in flight. Two commits on the *same*
    /// clip do replace one another, and the first write's read can then end the
    /// second's guess a round trip early -- the row is stale for one re-read
    /// and self-corrects, which is not worth a generation counter.
    pending_comments: HashMap<PathBuf, PendingComment>,
    /// What the toolbar's search box says (task3820). Raw, exactly as typed:
    /// the trimming and the case folding are `clips_ui::filter`'s, so there is
    /// one place that decides what "matches" means.
    ///
    /// Not persisted, and deliberately not cleared when the pane is left --
    /// which the history screen's own search is not either (`history.rs`'s
    /// `on_sessions_search_changed` sets it and nothing unsets it). Both panes
    /// are built under an `if active-pane`, so leaving destroys the field and
    /// coming back draws an empty one over a filtered grid; that mismatch is
    /// history's too, and fixing it belongs where both can be fixed at once.
    query: String,
    /// Which *entry* the right-click menu belongs to. `None` closes it.
    ///
    /// An index into `entries`, not into the tiles on screen (task3820): with
    /// a filter up the two are different lists, and this one is what the
    /// delete resolves its file from. Storing the tile index here would make
    /// 「削除」 on the third tile recycle the third *file in the folder* --
    /// exactly the wrong-target mistake CLAUDE.md was written for. `render`
    /// maps it back to a tile index for the border.
    menu: Option<usize>,
    /// The open comment editor (task2990), or `None`.
    editing: Option<ClipEdit>,
    /// The rows on screen as `(first, count)`, reported by the `.slint` side.
    /// `None` until the first `visible-range-changed`, which does not fire for
    /// the initial values -- hence the assumed screenful below.
    visible_range: Option<(usize, usize)>,
    /// Which calendar span the grid's date headings cover (task3020). Week
    /// until the user says otherwise, and deliberately not persisted: it is
    /// which way you are looking at the folder, not a preference.
    grouping: ClipGrouping,
    /// The one clip drawn large in the grid (task3880, round20 §2-1 B案), or
    /// `None`.
    ///
    /// An index into `entries`, like `menu` and for the same reason (task3820):
    /// with a filter up, the tile the press came from and the file behind it
    /// are two different numbers, and this one has to survive a comment landing
    /// from the worker and re-deciding what the grid holds. `render` maps it
    /// back to a tile index on the way out.
    ///
    /// Raised and cleared by `row-expand-toggled`, which task3890 attached to
    /// a single click on a tile, to a click on the expanded tile, and to Esc.
    expanded: Option<usize>,
    /// The selection (t260916-5568), by *path*: the folder is a re-scan and a
    /// bulk delete resolves its files from here. `reconcile` prunes it to what
    /// is on the grid.
    selection: ClipSelection<PathBuf>,
    /// The left press in progress on the grid, from `tile-pressed` or
    /// `ground-pressed` to `pointer-released`.
    press: Option<ClipPress>,
    /// Clips waiting on the bulk delete's confirmation; empty means no dialog.
    confirm: Vec<PathBuf>,
    // ---- the in-place player (task3520) ----
    //
    // Two halves that have to agree: `player` is the transport as
    // `ui_state::clip_player` models it (tested there), `engine` is the live
    // decoder. The invariant is that they are `Some`/open together and that both
    // follow `expanded` -- which is why every path that can clear `expanded`
    // goes through `close_player`, not only the toggle.
    player: ClipPlayer,
    engine: Option<PlaybackEngine>,
    /// Where the *picture* is being drawn, in physical pixels -- the engine
    /// resamples to it (task990's rule, borrowed from the review stage).
    stage_target: Option<(u32, u32)>,
    /// The recording-sized picture behind the last frame shown -- not for a
    /// screenshot (nothing screenshots a clip) but so a paused clip can be
    /// resampled again when the surface changes size (t260914-39c4, the
    /// review's `last_frame` and `redraw_paused_stage`). Belongs to the engine
    /// that produced it: every place `engine` is dropped or replaced clears it,
    /// or clip A's picture is drawn on clip B's surface.
    last_frame: Option<(u32, u32, livia::playback::FullFrame)>,
    /// The last pointer sign of life over the surface, for the 3-second hide.
    last_activity: Instant,
    /// The last centre press, for the 400ms double-press that goes full screen
    /// (`pb::stage_double_press`).
    last_center_tap: Option<Instant>,
    /// Whether the press in progress was a plain centre tap -- i.e. whether the
    /// *release* should still toggle play. Cleared by the hold, which is a skim
    /// and not a transport command (task1010's rule on the review stage).
    center_tap_pending: bool,
    /// Set by the hold timer: the release owes the rate back, and a hold that
    /// began paused owes a pause as well.
    hold: Option<pb::StageHold>,
    /// The play/pause this side last sent, and when. `Pause` reaches the engine
    /// on another thread, and until the worker runs it the shared status still
    /// says `playing == true` -- so without this latch every frame in between
    /// writes that back over the `false` just set and the transport glyph
    /// flickers (task3290's finding, `pb::owns_playing`). The deadline in that
    /// function is what keeps a command the engine never takes from freezing
    /// the glyph.
    play_intent: Option<(bool, Instant)>,
    /// The seek this side last sent, still unanswered. `pb::owns_position`
    /// compares it against the engine's echo so the bar does not jump back to
    /// where the playhead was before the seek (task2910's rule).
    last_seek_sent: Option<i64>,
    /// Whether *this side* paused the clip because the window went off screen
    /// or was buried, and therefore owes it a resume (`pb::offscreen_transport`
    /// takes this as its third input, so a clip the user paused is not restarted
    /// by uncovering the window).
    paused_offscreen: bool,
    /// Session-lived, deliberately: the review screen persists its own volume
    /// in `AppSettings`, and a clip watched in the grid is not that transport.
    /// Nothing here is written to disk, so a clip plays at full volume the next
    /// time the app starts.
    volume_percent: i64,
    muted: bool,
    /// Repeat, session-lived for the same reason the volume above is (the
    /// user's 2026-09-16 ruling, t260916-0ee4): held across opening another
    /// clip while the app runs, never written to disk, and nothing to do with
    /// `Review::repeat` -- the two transports do not share a loop.
    repeat: bool,
    jobs: Sender<ClipJob>,
    /// The same channel the clip worker answers on, so a refusal this side
    /// decides (an unreadable length) reaches the toast the worker's own errors
    /// reach -- `Msg::ClipError`, handled in `liveback.rs`.
    msgs: Sender<Msg>,
    /// The session worker's command channel, for the one write this side makes
    /// itself -- a comment on the clip that is playing (t260915-1fc3) -- to ask
    /// for the same re-scan the clip worker sends after its own writes.
    cmds: Sender<Cmd>,
}

/// Four grid lines of four, for the window before the viewport has said
/// anything (task3020: the list became a four-column grid).
const ASSUMED_VISIBLE_ROWS: usize = 16;

/// How many columns the grid draws. Mirrors `ClipsPane.columns` in
/// `clips.slint`; §1-3 fixes it at four rather than deriving it from width, so
/// the two constants cannot drift apart on a resize.
const COLUMNS: usize = 4;

/// How many columns *and* lines the expanded tile covers (task3880).
///
/// Three, by the user's 2026-09-08 instruction recorded in
/// `.agents/docs/design/round20-design-response.md` §1 -- 「押したタイルが
/// 3列分の幅」, raised there from the two task3490 prototyped. It is not a
/// number this side gets to re-pick. With `COLUMNS` at 4 it falls out as
/// 回答 MD's own row-setting: the footprint takes three of the four columns
/// and the rest of the group stacks in the one column left of it.
const EXPAND_SPAN: usize = 3;

/// The context menu, in the order it is drawn (task3020 §1-4). The edit and the
/// delete are here rather than on the tile itself (task2990): a clip's plain
/// click already means something -- since task3890 it expands the tile, which
/// is what plays it (round20 design response §6) -- and the history screen
/// removed click-to-edit over the same collision (task184). The overlay's
/// pencil is the one thing on the tile that takes a click of its own, and it
/// sits above `touch` so the press never reaches the expand.
///
/// Four entries, unchanged: round20's 追補便 fixes the list, and since
/// task3530 retired the external-player route 「再生」 means the in-place
/// player -- the same expansion a click on the tile makes.
const MENU_ACTIONS: [ClipMenuAction; 4] = [
    ClipMenuAction::Open,
    ClipMenuAction::EditComment,
    ClipMenuAction::Reveal,
    ClipMenuAction::Delete,
];

/// The menu's entries when it acts on `targets` clips (t260916-5568):
/// 「再生」 and 「コメントを編集」 are about one clip and go when the menu acts
/// on a selection (design 4). The render and the click both ask this, so the
/// index the `.slint` side sends back maps through the list it was drawn from.
fn menu_actions(targets: usize) -> Vec<ClipMenuAction> {
    MENU_ACTIONS
        .iter()
        .copied()
        .filter(|action| {
            targets <= 1 || matches!(action, ClipMenuAction::Reveal | ClipMenuAction::Delete)
        })
        .collect()
}

/// A left press on the grid (t260916-5568).
struct ClipPress {
    /// The tile pressed, or `None` for bare ground.
    row: Option<i32>,
    origin: (f32, f32),
    /// The pane's cell size at the press, for the hit test.
    cell: (f32, f32),
    shift: bool,
    ctrl: bool,
    /// Past the drag threshold, or begun on bare ground: a marquee, and the
    /// release is not a click.
    dragging: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClipMenuAction {
    Open,
    Reveal,
    EditComment,
    Delete,
}

/// UI → clip worker.
pub(super) enum ClipJob {
    /// Comment, duration and thumbnail for one row, with the modification time
    /// the scan saw -- so the answer is filed under the file the question was
    /// asked about and not under whatever it has become since.
    Details(PathBuf, SystemTime),
    /// Explorer with the files selected -- one, or a whole selection
    /// (t260916-5568). A `ShellExecuteW`-class call that can block on a cold
    /// shell, so it does not run on the event loop.
    Reveal(Vec<PathBuf>),
    /// task2990. Both open the shell's own machinery -- the property store, and
    /// `SHFileOperationW` -- so both stay off the event loop with the rest.
    WriteComment(PathBuf, String),
    /// One clip or many (t260916-5568): one job, so one re-scan and one toast.
    Delete(Vec<PathBuf>),
}

impl Clips {
    pub(super) fn new(jobs: Sender<ClipJob>, msgs: Sender<Msg>, cmds: Sender<Cmd>) -> Self {
        Clips {
            entries: Vec::new(),
            details: HashMap::new(),
            requested: HashSet::new(),
            pending_comments: HashMap::new(),
            query: String::new(),
            menu: None,
            editing: None,
            visible_range: None,
            grouping: ClipGrouping::Week,
            expanded: None,
            selection: ClipSelection::default(),
            press: None,
            confirm: Vec::new(),
            player: ClipPlayer::default(),
            engine: None,
            stage_target: None,
            last_frame: None,
            last_activity: Instant::now(),
            last_center_tap: None,
            center_tap_pending: false,
            hold: None,
            play_intent: None,
            last_seek_sent: None,
            paused_offscreen: false,
            volume_percent: 100,
            muted: false,
            repeat: false,
            jobs,
            msgs,
            cmds,
        }
    }

    /// The scan's answer, from the session worker.
    pub(super) fn listed(&mut self, entries: Vec<ClipEntry>) {
        self.entries = entries;
        // A row that scrolled out of existence must not keep its old menu open
        // on whatever moved into its index.
        self.menu = None;
        // The expansion is an *entry* index and a re-scan re-sorts the folder,
        // so it is re-resolved by the path the player is actually decoding
        // (task3520). This is not hypothetical: writing a comment from the
        // playing surface commits the property store, which moves the file's
        // modification time, which re-lists the folder in a different order --
        // and an index kept across that would draw some other clip large while
        // this one played. `ClipEdit` resolves by path for the same reason.
        if let Some(playing) = self.player.current().map(|clip| clip.path.clone()) {
            self.expanded = self.entries.iter().position(|entry| entry.path == playing);
        }
        self.reconcile();
    }

    /// Which entries the search box leaves on the grid, in grid order
    /// (task3820).
    ///
    /// Recomputed rather than cached, and every tile index that arrives from
    /// the `.slint` side is resolved through it. A cached copy would be a
    /// second answer to "what is on screen" that has to be invalidated by the
    /// scan, by the query, *and* by a comment landing from the worker -- and
    /// the one place it went stale would be the place a delete resolved its
    /// file. The list is tens of files.
    fn visible(&self) -> Vec<usize> {
        clips_ui::filter(
            &self.query,
            self.entries.iter().map(|entry| {
                (
                    entry.name.as_str(),
                    // The tile's own answer, not the cache's: a comment the
                    // user has just committed is what the tile now says, so it
                    // is what a query has to match on. The task's Out of Scope
                    // names this -- a hit that changes because of the guess is
                    // the right hit.
                    self.tile_view(entry).comment,
                )
            }),
        )
    }

    /// The entry one tile stands for, or `None` if the grid moved under the
    /// index the `.slint` side sent.
    fn entry_index(&self, row: i32) -> Option<usize> {
        self.visible().get(usize::try_from(row).ok()?).copied()
    }

    /// The grid as `render` lays it out: which entries are on it, their date
    /// stamps, the expanded tile, and where every tile sits. Lifted out of
    /// `render` by t260916-5568 so the marquee's hit test places the tiles
    /// exactly the way they are drawn.
    fn layout(&self) -> (Vec<usize>, Vec<String>, Option<usize>, Vec<ClipPlacement>) {
        let now = super::history::now_date_text();
        let visible = self.visible();
        let stamps: Vec<String> = visible
            .iter()
            .filter_map(|index| self.entries.get(*index))
            // `created`, not `modified`: the grid's date and its week/month
            // headings are about when the clip was cut, and a comment write moves
            // the mtime (see `ClipEntry::created`).
            .map(|entry| super::history::clip_date_text(entry.created))
            .collect();
        // The expansion, in grid coordinates: `self.expanded` is an entry index
        // and `place` lays out the tiles that survived the filter (task3820's
        // distinction, the same one `menu_row` below crosses). An expanded entry
        // that is not on the filtered grid simply expands nothing.
        let expanded = self
            .expanded
            .and_then(|entry| visible.iter().position(|index| *index == entry));
        let placed = clips_ui::place(
            tr_locale(),
            self.grouping,
            &stamps,
            &now,
            COLUMNS,
            expanded,
            EXPAND_SPAN,
        );
        (visible, stamps, expanded, placed)
    }

    /// The paths on the grid, in grid order: the selection's whole universe
    /// (t260916-5568).
    fn visible_paths(&self) -> Vec<PathBuf> {
        self.visible()
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// The file one tile stands for.
    fn row_path(&self, row: i32) -> Option<PathBuf> {
        self.entry_index(row)
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.clone())
    }

    /// The tiles a marquee may catch, with their rectangles: the grid minus the
    /// one that is expanded (the user's 2026-09-19 ruling, t260920-c6eb). The
    /// expanded tile is drawn as the playback surface and carries no selection
    /// mark of its own, so a rectangle that swept it put a clip nobody could see
    /// was selected into the next Delete.
    ///
    /// Dropped **after** the zip, not before: `clips_ui::tile_rects` lays out
    /// every placement, so filtering `visible` first would pair each rectangle
    /// past the expansion with the wrong path.
    fn tile_rects(&self, cell: (f32, f32)) -> Vec<(PathBuf, clips_ui::Rect)> {
        let (visible, _, expanded, placed) = self.layout();
        let rects = clips_ui::tile_rects(&placed, cell.0, cell.1);
        visible
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.path.clone())
            .zip(rects)
            .enumerate()
            .filter(|(row, _)| Some(*row) != expanded)
            .map(|(_, tile)| tile)
            .collect()
    }

    /// The paths a selection gesture may reach, in grid order: `visible_paths`
    /// minus the expanded tile, for the same reason `tile_rects` drops it.
    /// Ctrl+A and a Shift range both run along this order, so leaving the
    /// expanded entry in it is what let them select it (t260920-c6eb).
    ///
    /// Not used by `retain`: a clip that was already selected when it was
    /// expanded stays selected -- this only stops a gesture from *adding* it.
    fn selectable_paths(&self) -> Vec<PathBuf> {
        let expanded = self
            .expanded
            .and_then(|entry| self.entries.get(entry))
            .map(|entry| entry.path.clone());
        self.visible_paths()
            .into_iter()
            .filter(|path| Some(path) != expanded.as_ref())
            .collect()
    }

    /// What the open menu over `entry` acts on (t260916-5568).
    fn menu_targets(&self, entry: usize) -> Vec<PathBuf> {
        self.entries
            .get(entry)
            .map(|clip| self.selection.menu_targets(&clip.path))
            .unwrap_or_default()
    }

    /// Drops an open menu or editor that no longer stands over its own file.
    ///
    /// Called by both things that can move a tile out from under one: a fresh
    /// scan, and a comment arriving from the worker -- because with a query up,
    /// a comment landing is what *decides* whether its clip is on the grid at
    /// all, and every tile below it then shifts by one.
    fn reconcile(&mut self) {
        let visible = self.visible();
        let paths: Vec<PathBuf> = visible
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.path.clone())
            .collect();
        // The *visible* paths, not the whole folder's: `ClipEdit::row` is the
        // tile the editor is drawn on, so that is the list it has to still
        // agree with (task2990's rule, in the coordinates task3820 gave it).
        if self
            .editing
            .as_ref()
            .is_some_and(|edit| !clips_ui::edit_survives(edit, &paths))
        {
            self.editing = None;
        }
        // The selection keeps only what is on the grid (t260916-5568, design
        // 7): a clip the scan or the search took off it must not ride along
        // into a delete whose target nobody can see.
        self.selection.retain(&paths);
        self.confirm.retain(|path| paths.contains(path));
        if self.menu.is_some_and(|entry| !visible.contains(&entry)) {
            self.menu = None;
        }
        // Same rule for the expansion (task3880): a tile drawn large that the
        // filter or the scan has taken off the grid has nothing left to be
        // large *about*, and leaving the index set would expand whatever slid
        // into it next.
        if self.expanded.is_some_and(|entry| !visible.contains(&entry)) {
            self.expanded = None;
        }
        // And the player with it (task3520): the surface is drawn *inside* the
        // expanded tile, so a tile that is no longer large has nowhere to put a
        // picture -- and an engine left running would keep the audio going over
        // a grid with nothing open on it. This is the second of the three places
        // `expanded` can be cleared; the other two are the toggle handler and
        // leaving the pane.
        if self.expanded.is_none() {
            self.drop_player();
        }
    }

    /// Stops and drops the engine. The window's full screen is *not* undone
    /// here -- that needs the `AppWindow`, and `render_player` does it on the
    /// way out, which is the one place every caller of this already passes
    /// through.
    fn drop_player(&mut self) {
        self.drop_engine();
        // `ClipPlayer::close` is only the model; the engine above was the audio.
        self.player.close();
        self.last_center_tap = None;
        self.center_tap_pending = false;
        self.paused_offscreen = false;
        self.hold = None;
    }

    /// Drops the decoder and nothing else (t260915-1fc3). Unlike `drop_player`
    /// the transport model stays open -- position, play state, and the path
    /// `listed` re-pins the expansion by -- so the clip can be reopened where it
    /// was once the file has been let go.
    fn drop_engine(&mut self) {
        // Dropping joins the worker thread (`PlaybackEngine::drop`), which is
        // what actually stops the audio and releases the file: the join
        // returning *is* the signal that the handle is gone.
        self.engine = None;
        self.last_frame = None;
        // Both answer to the engine that is gone: a seek it never acks would
        // keep `pb::owns_position` true for good and freeze the bar.
        self.last_seek_sent = None;
        self.play_intent = None;
    }

    /// Play or pause, remembered as intent so the pump does not undo it before
    /// the worker has taken it. Every route that changes the transport goes
    /// through here -- the button, the gestures, the offscreen park.
    fn set_playing(&mut self, playing: bool) {
        if let Some(clip) = self.player.current_mut() {
            clip.playing = playing;
        }
        self.play_intent = Some((playing, Instant::now()));
        self.send(if playing {
            PlaybackCommand::Play
        } else {
            PlaybackCommand::Pause
        });
    }

    /// One command to the engine, dropped on the floor when nothing is open --
    /// the space bar reaching a closed player is not a pause
    /// (`ClipPlayer::toggle_play`'s own rule, in the engine's vocabulary).
    fn send(&self, command: PlaybackCommand) {
        if let Some(engine) = self.engine.as_ref() {
            engine.send(command);
        }
    }

    /// Drops an open comment editor without writing anything (task2990
    /// follow-up).
    ///
    /// Leaving クリップ is the original caller: nothing tears the editor down
    /// when the pane is merely left -- the list is only re-scanned on the way
    /// back in (`opens_clips`), and for a file nobody touched while away
    /// `edit_survives` still says yes -- so a return would find the previous
    /// visit's editor still standing.
    ///
    /// Since task3430 it is also what every handler that re-renders the grid
    /// calls on the way in (`row_expand_toggled`, `row_menu_requested`,
    /// `menu_dismissed`, ...). Those used to commit the draft there; the
    /// user's ruling (2026-09-07) is that a click anywhere but the field
    /// discards, so they close it instead.
    pub(super) fn close_editor(&mut self) {
        self.editing = None;
    }

    /// One row's details, back from the worker.
    pub(super) fn detailed(
        &mut self,
        path: PathBuf,
        modified: SystemTime,
        details: ClipDetails,
        pixels: Option<(u32, u32, Vec<u8>)>,
    ) {
        self.requested.remove(&path);
        // A read filed under a different modification time is one taken after
        // the write, so it -- and not the guess -- is the truth from here. One
        // filed under the *same* time is the in-flight pre-write answer the
        // comment below is about, and leaves the guess standing (t260916-568d).
        if self
            .pending_comments
            .get(&path)
            .is_some_and(|pending| pending.superseded_by(modified))
        {
            self.pending_comments.remove(&path);
        }
        // Filed against the modification time the file had when the request was
        // *made*, not the one it has now. Task2990 made that difference matter:
        // a comment written while a detail read is in flight moves the file's
        // mtime, and filing the in-flight (pre-write) answer under the new one
        // would cache the old comment against the new file and leave the row
        // stale until something else re-scanned. Filed under the old mtime it
        // simply misses, and `queue_visible_details` asks again.
        self.details.insert(
            path,
            (
                modified,
                Detail {
                    comment: details.comment,
                    duration_100ns: details.duration_100ns,
                    thumbnail: pixels
                        .and_then(|(width, height, rgba)| image_from_rgba(width, height, &rgba)),
                },
            ),
        );
        // The comment that just landed is searchable text (task3820): with a
        // query up it can put this clip on the grid or take another off it, and
        // an open menu or editor two tiles down has just moved.
        self.reconcile();
    }

    /// The details read at the file's current modification time, or `None`.
    ///
    /// The strict test, and the one `queue_visible_details` has to keep using:
    /// it is what makes a written-to clip be asked about again. What the tile
    /// *draws* is `tile_view`, which is deliberately laxer.
    fn cached(&self, entry: &ClipEntry) -> Option<&Detail> {
        self.details
            .get(&entry.path)
            .filter(|(modified, _)| *modified == entry.modified)
            .map(|(_, detail)| detail)
    }

    /// What one tile draws for a clip (t260916-568d): the comment, and the
    /// details its picture comes from.
    ///
    /// The difference from `cached` is the whole fix. A cache entry whose
    /// modification time no longer matches the file is an update *in flight*,
    /// not an absence -- the picture it holds is still this clip's picture, and
    /// a comment write does not change a single pixel of it -- so the tile
    /// keeps drawing it until the re-read lands. The comment cannot be treated
    /// that way, because the write is precisely what changed it, and that is
    /// what `pending_comments` answers.
    fn tile_view(&self, entry: &ClipEntry) -> TileView<'_> {
        let detail = self.details.get(&entry.path).map(|(_, detail)| detail);
        TileView {
            comment: clips_ui::tile_comment(
                self.pending_comments
                    .get(&entry.path)
                    .map(|pending| pending.text.as_str()),
                detail.and_then(|detail| detail.comment.as_deref()),
            ),
            detail,
        }
    }

    /// Records the text a commit is about to write, so the tile shows it in the
    /// same render rather than after the round trip.
    ///
    /// Against the modification time the file has *now*, which is what tells an
    /// arriving read whether it saw the write.
    fn commit_optimistically(&mut self, path: &Path, modified: SystemTime, text: &str) {
        self.pending_comments.insert(
            path.to_path_buf(),
            PendingComment {
                text: text.to_owned(),
                at: modified,
            },
        );
    }

    /// A clip write was refused: drop the guesses, which puts every tile back
    /// on the text its file still holds. The toast is `Msg::ClipError`'s own
    /// job; this is the half that has to reach the grid.
    pub(super) fn comment_write_failed(&mut self) {
        self.pending_comments.clear();
    }
}

/// What a tile draws for one clip, resolved together because the comment and
/// the picture answer to different rules (`Clips::tile_view`).
struct TileView<'a> {
    comment: Option<&'a str>,
    detail: Option<&'a Detail>,
}

// ---------------------------------------------------------------------------
// The in-place player (task3520, round20 §2-1 B案)
// ---------------------------------------------------------------------------
//
// Two coordinate systems meet here and must not be confused:
//
// - `Clips::expanded` and `Clips::menu` are **entry** indices, into the whole
//   folder as scanned. They survive a filter change and a comment landing.
// - `ClipsPane.expanded-row`, `ClipVm.row` and `ClipPlayerState::row` are
//   **tile** indices, into the filtered grid `render` draws. `render` maps one
//   to the other, once, and nothing else does.
//
// The engine is never keyed on either of them. It is keyed on the *path*, which
// is the only name a re-scan cannot move.

/// How long the clip's own length is worth reading on the event loop when the
/// worker has not answered yet -- i.e. a click landing in the first moments
/// after entering the pane. Measured at ~4ms a file by `livia::clips::details`'s
/// own note, once, against a click that would otherwise be refused for a clip
/// that is perfectly playable.
fn clip_duration(clips: &Clips, entry: &ClipEntry) -> Option<i64> {
    clips
        .cached(entry)
        .map(|detail| detail.duration_100ns)
        .unwrap_or_else(|| livia::clips::details(&entry.path).duration_100ns)
        .filter(|ticks| *ticks > 0)
}

/// Brings the engine into line with `Clips::expanded` after a press.
///
/// This is the whole of the lifecycle the task asks for in one place: the same
/// comparison answers "open", "close" and "switch to another tile", because
/// switching is just a close whose next state is `Some`. `ui_state::clips::
/// toggle_expanded` has already decided *which* tile by the time this runs.
fn sync_player(ui: &AppWindow, clips: &mut Clips) {
    let wanted = clips
        .expanded
        .and_then(|entry| clips.entries.get(entry))
        .map(|entry| entry.path.clone());
    let playing = clips.player.current().map(|clip| clip.path.clone());
    if wanted == playing {
        return;
    }
    clips.drop_player();
    let Some(path) = wanted else {
        return;
    };
    if !open_player(ui, clips, &path, 0, true) {
        // Nothing left to be large about: a tile expanded over a clip that
        // cannot be played is a blank rectangle with no way to read what went
        // wrong, and the toast below is the answer instead.
        clips.expanded = None;
    }
}

/// Starts decoding one clip. `false` means nothing was opened and the caller
/// has to take the expansion back down.
///
/// A tap opens at `0` playing; the reopen after a comment (t260915-1fc3)
/// passes back the position and play state the transport had.
fn open_player(
    ui: &AppWindow,
    clips: &mut Clips,
    path: &Path,
    initial_position_100ns: i64,
    playing: bool,
) -> bool {
    let Some(entry) = clips
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .cloned()
    else {
        return false;
    };
    // Refused *before* `ClipPlayer::open`, on purpose: `open` clamps a missing
    // length to 0, which would leave a transport whose bar cannot move and
    // whose every seek lands at the start -- a player that looks broken rather
    // than one that said no.
    let Some(duration_100ns) = clip_duration(clips, &entry) else {
        let _ = clips.msgs.send(Msg::ClipError(
            clips_ui::no_duration(tr_locale()).to_owned(),
        ));
        return false;
    };
    // The tile this clip is on right now. Only `ClipPlayerState` keeps it, and
    // only as a record of where it was opened -- everything that has to find
    // the clip again goes by path.
    let row = clips
        .visible()
        .iter()
        .position(|index| clips.entries.get(*index).map(|e| &e.path) == Some(&entry.path))
        .unwrap_or(0);

    // One outstanding wake-up at a time, exactly as the review's own engine is
    // rung (task200 follow-up): the mailbox is latest-wins, so a skipped ring
    // loses nothing and the UI's own speed becomes the rate limiter.
    let weak = ui.as_weak();
    let pending = Arc::new(AtomicBool::new(false));
    clips.player.open(row, entry.path.clone(), duration_100ns);
    // The model starts at 0; without this the bar would flash 0:00 until the
    // pump's first status read after a reopen.
    if let Some(clip) = clips.player.current_mut() {
        clip.position_100ns = initial_position_100ns.clamp(0, duration_100ns);
    }
    clips.engine = Some(PlaybackEngine::start_file(
        entry.path,
        duration_100ns,
        // The start for a tap; where the transport was for the reopen after a
        // comment (t260915-1fc3). task3540's 「スクラブ中のクリックはその位置から」
        // is the other caller this argument is kept for.
        initial_position_100ns,
        clips.volume_percent,
        clips.muted,
        // The surface's size (task990), on the engine before its worker decodes
        // the poster (t260915-c1ca). `None` the first time any clip is opened:
        // `ClipStage` rings `stage-size-changed` from its own `init`, and it
        // only exists once the tile is open, a frame after this. That poster
        // goes out at full resolution; every later open -- the reopen after a
        // comment (t260915-1fc3) included -- starts from the size the last
        // surface reported, which a resize ring corrects if it has changed.
        clips.stage_target,
        Box::new(move || {
            if pending.swap(true, Ordering::AcqRel) {
                return;
            }
            let weak = weak.clone();
            let pending = pending.clone();
            let _ = slint::invoke_from_event_loop(move || {
                pending.store(false, Ordering::Release);
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ClipVm>().invoke_frame_ready();
                }
            });
        }),
    ));
    clips.last_frame = None;
    if let Some(engine) = clips.engine.as_ref() {
        // t260913-3842: the clips player has no `StageGate` of its own, so
        // this is the only place the display's rate reaches its engine.
        engine
            .shared()
            .set_display_refresh_hz(super::desktop::display_refresh_hz());
    }
    // Opening a clip is asking to watch it (round20 section 6: a single click
    // plays). The worker starts paused (`Worker::new`), so without this the
    // tile opened on 0:00 behind a play glyph and waited for a second tap
    // (task3520's 2026-09-15 sweep, item 1). Through `set_playing` rather than
    // a bare `send`: it arms `play_intent`, so the pump does not copy the
    // worker's not-yet-updated `playing == false` back onto the glyph. The
    // command channel is unbounded, so this cannot block, and the worker takes
    // it as soon as it is up -- the review's own open does the same.
    // `playing` is `true` for every tap; only the reopen after a comment hands
    // a paused transport back paused, and a `Pause` to a worker that starts
    // paused is a no-op rather than a blip of audio.
    clips.set_playing(playing);
    // Once per open, not once per frame: the hover bands' 「±10秒」 never
    // changes while a clip is up, and `render_player` runs at the decode rate.
    ui.global::<ClipVm>()
        .set_zone_seconds(pb::stage_zone_seconds(tr_locale()).into());
    clips.last_activity = Instant::now();
    true
}

/// Pulls the engine's latest frame and status onto `ClipVm`.
///
/// The review's `pump_playback`, with everything this screen has no use for
/// left out: no `StageGate` (the surface is a tile, not a full-screen 1440p
/// stage -- add one if a clip ever costs the event loop what a session does),
/// no track models. It does keep a `last_frame`, as the review does -- not for a
/// screenshot (nothing screenshots a clip) but so a paused clip can be resampled
/// again when the surface changes size (t260914-39c4).
fn pump_player(ui: &AppWindow, clips: &mut Clips) {
    let Some(engine) = clips.engine.as_ref() else {
        return;
    };
    let shared = engine.shared();
    let vm = ui.global::<ClipVm>();
    let taken = shared.frame.lock().ok().and_then(|mut slot| slot.take());
    if let Some(frame) = taken {
        if let Some(image) = image_from_rgba(frame.width, frame.height, &frame.rgba) {
            vm.set_stage_frame(image);
            vm.set_stage_has_frame(true);
            // Measured off the recording, never off a frame the engine resized
            // for the surface: the surface's video box is sized by this aspect
            // and the resize target is measured off that box, so an aspect taken
            // from the resized frame closes a loop that never settles (task990).
            let (width, height) = frame
                .full
                .as_ref()
                .map_or((frame.width, frame.height), |(width, height, _)| {
                    (*width, *height)
                });
            vm.set_stage_aspect(pb::stage_aspect(width, height));
        }
        // The review's `pump_playback`, mirrored (t260914-39c4): the stage-sized
        // copy goes straight back to be resampled into again (task1690) --
        // `image_from_rgba` above already copied it into slint's own storage --
        // and the full-resolution picture is kept in place of the previous
        // one, which is the buffer that goes back to the engine (task1640,
        // through `recycle_full` since t260914-862b).
        let next = match frame.full {
            Some(full) => {
                shared.recycle_scaled(frame.rgba);
                full
            }
            None => (
                frame.width,
                frame.height,
                livia::playback::FullFrame::Rgba(frame.rgba),
            ),
        };
        if let Some((_, _, previous)) = clips.last_frame.replace(next) {
            shared.recycle_full(previous);
        }
    }
    let Ok(status) = shared.status.lock().map(|status| *status) else {
        return;
    };
    // The UI's last transport command wins over the engine's own `playing` only
    // while the latch says so; the moment it is spent -- the engine agreed, or
    // the deadline passed -- the engine has the glyph back (task3290).
    let held = clips.play_intent.and_then(|(intent, sent)| {
        pb::owns_playing(
            Some(intent),
            sent.elapsed().as_millis() as u64,
            status.playing,
        )
        .then_some(intent)
    });
    if held.is_none() {
        clips.play_intent = None;
    }
    let playing = held.unwrap_or(status.playing);
    let last_seek_sent = clips.last_seek_sent;
    // Asked before the fold below overwrites it: `clip.playing` still carries
    // the *previous* pump's answer, and that is the only thing here that tells
    // the engine's park at the end apart from a pause the user asked for
    // (t260916-0ee4).
    let wrap = pb::clip_repeat_wrap(
        clips.repeat,
        clips.player.current().is_some_and(|clip| clip.playing),
        playing,
        status.at_live_edge,
        pb::owns_position(false, last_seek_sent, status.acked_seek_100ns),
    );
    if let Some(clip) = clips.player.current_mut() {
        clip.playing = playing;
        // Until the engine echoes the seek this side sent, its `position_100ns`
        // still describes where the playhead was *before* it -- taking it would
        // snap the bar back under the pointer (task2910). No drag flag: the
        // seek bar sends a position per move, so each one is its own request.
        if !pb::owns_position(false, last_seek_sent, status.acked_seek_100ns) {
            clip.position_100ns = status.position_100ns.clamp(0, clip.duration_100ns);
        }
    }
    if status.acked_seek_100ns == last_seek_sent {
        clips.last_seek_sent = None;
    }
    // The loop point, and it sends no seek of its own: `Play` to a transport
    // parked on the last playable instant already restarts from the head
    // (`Worker::restarts_from_head`), which is the very path the play button
    // takes from there -- so the wrap is 「もう一度再生を押す」 and there is no
    // `last_seek_sent` to reconcile. Through `set_playing` because that arms
    // the intent latch: without it the glyph would flick back to play for the
    // frames between this command and the worker taking it (task3290's rule).
    if wrap {
        clips.set_playing(true);
    }
}

/// Pushes the transport onto `ClipVm`, and takes the window's full screen back
/// down when nothing is playing any more.
///
/// `row` is the **tile** the expansion is on, already mapped out of
/// `Clips::expanded` by `render` -- `ClipVm.row` is what decides which raised
/// copy in the grid mounts the surface, so it has to be a grid index.
fn render_player(ui: &AppWindow, clips: &Clips, row: Option<usize>) {
    let vm = ui.global::<ClipVm>();
    let Some(clip) = clips.player.current() else {
        // Full screen belongs to the clip being open (`ClipPlayer::close`'s own
        // rule), and the *window* half of it lives out here. Without this, Esc
        // would leave a borderless full-screen window with a grid in it.
        if vm.get_fullscreen() {
            ui.window().set_fullscreen(false);
            vm.set_fullscreen(false);
        }
        vm.set_open(false);
        vm.set_row(-1);
        vm.set_playing(false);
        vm.set_stage_has_frame(false);
        vm.set_controls_pinned(false);
        return;
    };
    vm.set_open(true);
    vm.set_row(row.and_then(|row| i32::try_from(row).ok()).unwrap_or(-1));
    vm.set_playing(clip.playing);
    vm.set_repeat_enabled(clips.repeat);
    // An hour of clip is unusual but not impossible, and the two halves of the
    // clock have to agree on their shape (`tl::format_position`'s contract).
    let force_hours = clip.duration_100ns >= 3600 * tl::HNS_PER_SECOND;
    vm.set_position_text(tl::format_position(clip.position_100ns, force_hours).into());
    vm.set_total_text(tl::format_position(clip.duration_100ns, force_hours).into());
    vm.set_played_percent((clip.played_ratio() * 100.0) as f32);
    vm.set_volume_ratio(clips.volume_percent as f32 / 100.0);
    vm.set_muted(clips.muted);
    // The pane's own reasons to keep the bar up are folded in here rather than
    // pushed into the `.slint`: an open comment editor is `Clips::editing`, and
    // an open context menu is `Clips::menu`.
    let pinned = vm.get_controls_pinned() || clips.editing.is_some() || clips.menu.is_some();
    vm.set_controls_visible(clip_player::controls_visible(
        clip.playing,
        clips.last_activity,
        Instant::now(),
        pinned,
    ));
}

/// Leaving the クリップ pane (task3520). The editor's own teardown reason
/// (`Clips::close_editor`) with the player's added: a clip decoding for a screen
/// nobody is looking at is the same waste `offscreen_transport` exists to stop,
/// and here there is nothing to come back to -- the expansion is not persisted.
pub(super) fn left_pane(ui: &AppWindow, clips: &mut Clips) {
    clips.close_editor();
    if clips.expanded.is_none() && clips.engine.is_none() {
        return;
    }
    clips.expanded = None;
    clips.drop_player();
    render_player(ui, clips, None);
}

/// The window went off screen or was covered while a clip was playing, and came
/// back (task2800's rule, borrowed whole through `pb::offscreen_transport`).
///
/// Unlike the review there is no `paused_offscreen` companion to keep: a clip
/// that the *user* paused is `playing == false`, and `offscreen_transport`
/// already refuses to resume something it did not pause -- which is what the
/// third argument carries.
pub(super) fn offscreen_tick(ui: &AppWindow, clips: &mut Clips, on_screen: bool) {
    let Some(clip) = clips.player.current() else {
        return;
    };
    let (playing, owed) = (clip.playing, clips.paused_offscreen);
    match pb::offscreen_transport(on_screen, playing, owed) {
        pb::OffscreenTransport::Pause => {
            clips.paused_offscreen = true;
            clips.set_playing(false);
        }
        pb::OffscreenTransport::Resume => {
            clips.paused_offscreen = false;
            clips.set_playing(true);
        }
        pb::OffscreenTransport::Leave => return,
    }
    render_player(ui, clips, mounted_row(ui));
}

/// The tile the surface is mounted on, read back off `ClipVm` rather than
/// re-derived from `Clips::expanded`.
///
/// `render` is the one place that decides it, and the hot paths -- a decoded
/// frame, a pointer moving over the picture -- must not re-run the filter to
/// repeat an answer that has not changed. `-1` (nothing open) falls out as
/// `None` from the `usize` conversion.
fn mounted_row(ui: &AppWindow) -> Option<usize> {
    usize::try_from(ui.global::<ClipVm>().get_row()).ok()
}

/// Play/pause, from any of the four things that ask for it: the transport
/// button, a centre tap, the second press of a double click undoing the first,
/// and the release of a hold that began paused.
fn toggle_play(ui: &AppWindow, clips: &mut Clips) {
    let Some(playing) = clips.player.toggle_play() else {
        return;
    };
    // A deliberate transport command settles the offscreen debt: a clip the
    // user paused must not be restarted by uncovering the window.
    clips.paused_offscreen = false;
    clips.set_playing(playing);
    clips.last_activity = Instant::now();
    render_player(ui, clips, mounted_row(ui));
}

/// Flips the mute flag and tells the engine. Shared by the bar's button and the
/// `m` key (t260916-956e) so the two cannot drift; volume is not persisted on
/// this screen the way the review's is, so there is no settings round trip.
fn toggle_mute(ui: &AppWindow, clips: &mut Clips) {
    clips.muted = !clips.muted;
    let (percent, muted) = (clips.volume_percent, clips.muted);
    clips.send(PlaybackCommand::SetVolume { percent, muted });
    clips.last_activity = Instant::now();
    render_player(ui, clips, mounted_row(ui));
}

/// Moves the playhead, clamped to the clip. The tick is remembered as
/// `last_seek_sent` so the pump can tell the engine's stale position from its
/// answer to *this* request (task2910's rule).
fn seek_to(clips: &mut Clips, target_100ns: i64) {
    let Some(clip) = clips.player.current_mut() else {
        return;
    };
    let target = target_100ns.clamp(0, clip.duration_100ns);
    clip.position_100ns = target;
    clips.last_seek_sent = Some(target);
    clips.send(PlaybackCommand::Seek {
        target_100ns: target,
        precise: true,
    });
    clips.last_activity = Instant::now();
}

/// Borderless full screen for the clip surface.
///
/// Deliberately *not* `review_wiring::set_fullscreen`: that one also writes
/// `ReviewVm.fullscreen` and re-derives the review's right panel, neither of
/// which this screen has. `app.slint`'s `any-fullscreen` already ORs the two
/// flags, so the chrome folds away either way (task3510).
fn set_clip_fullscreen(ui: &AppWindow, on: bool) {
    ui.window().set_fullscreen(on);
    ui.global::<ClipVm>().set_fullscreen(on);
}

/// Saves an open comment editor into the file it was opened on (task2990).
///
/// `draft` is the field's own final text, and Enter is now the only exit that
/// produces one: task3430 turned the blur that used to arrive here into a
/// cancel, so there is no longer a teardown path that has to fall back to the
/// draft kept on every keystroke.
///
/// The write goes to `edit.path`, and only while that path is still in the
/// list. The list is a re-scan with no index behind it, so writing a comment
/// onto whatever slid into the editor's row is the mistaken-target mistake
/// CLAUDE.md exists for -- resolving by path is what rules it out.
fn commit_open_edit(ui: &AppWindow, clips: &mut Clips, draft: String) {
    let Some(edit) = clips.editing.take() else {
        return;
    };
    // The modification time comes out of the same lookup that rules the write
    // in: it is the key the row's details are filed under, and the guess below
    // is measured against it.
    let Some(modified) = clips
        .entries
        .iter()
        .find(|entry| entry.path == edit.path)
        .map(|entry| entry.modified)
    else {
        return;
    };
    // Unchanged text writes nothing: the store's `Commit` moves the file's
    // modification time, which is the key the row's own details are cached
    // under. An editor opened and closed must not cost a re-read of everything.
    //
    // Measured against what the field was *seeded* with, not against what the
    // row says now. A row whose details are still in flight opens its editor
    // empty, and the comment can land from the worker while it is open --
    // against the live value, closing that editor untouched would read as
    // "emptied" and write `""` over a comment the user never saw.
    if !clips_ui::needs_write(&draft, Some(&edit.original)) {
        return;
    }
    // The clip that is playing cannot be written while its engine holds it, and
    // no share mode on the reading side fixes that: the shell's mp4 property
    // handler opens the store `GPS_READWRITE` exclusively (t260914-b680: 16/16
    // `MFCreateFile` combinations refused, and a single plain read handle alone
    // still fails with 0x80070020). So that one clip is written between
    // dropping the engine -- whose `Drop` joins, i.e. the file is let go when it
    // returns -- and reopening it, synchronously on this thread for the reason
    // the review's clip write gives (task2960: `Commit` under 1ms, ~45ms handler
    // load once per process). Every other clip keeps the worker route.
    // Before either route, and before the render either of them leads to: the
    // write moves the modification time the row's details are filed under, so
    // without this the tile would draw an empty comment over a discarded
    // thumbnail until the re-read landed (t260916-568d).
    clips.commit_optimistically(&edit.path, modified, &draft);
    let under_player = clips
        .player
        .current()
        .filter(|clip| clips.engine.is_some() && clip.path == edit.path)
        .cloned();
    match under_player {
        Some(saved) => write_under_player(ui, clips, &edit.path, &draft, saved),
        None => {
            let _ = clips.jobs.send(ClipJob::WriteComment(edit.path, draft));
        }
    }
}

/// The playing-clip half of `commit_open_edit` (t260915-1fc3): let go, write,
/// reopen where the transport was. Reopened whether or not the write went
/// through -- a refused comment must not also cost the clip being watched --
/// and answered the way `ClipJob::WriteComment` answers: a re-scan on success,
/// the toast on failure.
fn write_under_player(
    ui: &AppWindow,
    clips: &mut Clips,
    path: &Path,
    comment: &str,
    saved: ClipPlayerState,
) {
    let started = Instant::now();
    clips.drop_engine();
    let written = livia::clips::write_comment(path, comment);
    let reopened = open_player(ui, clips, path, saved.position_100ns, saved.playing);
    if reopened {
        if let Some(clip) = clips.player.current_mut() {
            clip.fullscreen = saved.fullscreen;
        }
    } else {
        // `open_player` refused before touching the model, so the transport
        // would be left open over no engine; close both, as `sync_player` does.
        clips.drop_player();
        clips.expanded = None;
    }
    tracing::info!(
        took_ms = started.elapsed().as_millis() as u64,
        written = written.is_ok(),
        reopened,
        position_100ns = saved.position_100ns,
        playing = saved.playing,
        "clip_comment_under_player"
    );
    match written {
        Ok(()) => {
            let _ = clips.cmds.send(Cmd::ListClips);
        }
        Err(error) => {
            let _ = clips.msgs.send(Msg::ClipError(error));
        }
    }
}

/// Asks the worker for what the rows on screen need, and only those -- unless
/// a search is up, in which case it asks about the whole folder.
///
/// The window exists because a detail read is a shell round trip in the tens of
/// milliseconds and a folder need not be paid for all at once. A query changes
/// what the answer is *for*: the comment is what is being searched, so a clip
/// whose comment has not been read cannot match on it, and asking only about
/// the window would make the result set grow as the user scrolled -- a search
/// that finds more of the same thing the longer you look at it. So a non-empty
/// query asks about every clip (task3820).
///
/// The cost is the thumbnail that rides along on the same job, which is the
/// ceiling `Clips::details` already contemplates for a folder the user has
/// scrolled through: reached sooner, not raised.
fn queue_visible_details(clips: &mut Clips) {
    let searching = !clips.query.trim().is_empty();
    let range = if searching {
        0..clips.entries.len()
    } else {
        let (first, count) = clips.visible_range.unwrap_or((0, ASSUMED_VISIBLE_ROWS));
        // The history screen's own windowing, reused rather than re-derived: it
        // already clamps an overscanned range to the list. Sound only while the
        // grid is unfiltered, which is exactly when this branch runs -- with no
        // query the tiles are the entries, one for one.
        sessions::visible_thumbnail_range(first, count, clips.entries.len())
    };
    let wanted: Vec<(PathBuf, SystemTime)> = clips.entries[range]
        .iter()
        .filter(|entry| clips.cached(entry).is_none() && !clips.requested.contains(&entry.path))
        .map(|entry| (entry.path.clone(), entry.modified))
        .collect();
    for (path, modified) in wanted {
        clips.requested.insert(path.clone());
        let _ = clips.jobs.send(ClipJob::Details(path, modified));
    }
}

/// Off the event loop: the property store and the thumbnail provider both take
/// milliseconds at best, and the shell calls behind 開く / 表示 can block while
/// Explorer starts.
///
/// It holds the command channel as well as the message one because a write and
/// a delete both have to be followed by a fresh scan, and only the worker knows
/// when its own shell call finished. Re-listing is the whole of the
/// invalidation: the folder is the list (there is no index), so a deleted clip
/// stops being listed and an edited one comes back with a new modification time
/// that misses the detail cache.
pub(super) fn spawn_clip_worker(rx: Receiver<ClipJob>, tx: Sender<Msg>, cmd: Sender<Cmd>) {
    std::thread::spawn(move || {
        // The shell wants an initialized apartment; `RPC_E_CHANGED_MODE` would
        // only mean someone got here first, which is fine.
        // SAFETY: no arguments, and the result is deliberately dropped.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        while let Ok(job) = rx.recv() {
            match job {
                ClipJob::Details(path, modified) => {
                    let details = livia::clips::details(&path);
                    let pixels = thumbnail_rgba(&path);
                    let _ = tx.send(Msg::ClipDetails(path, modified, details, pixels));
                }
                ClipJob::Reveal(paths) => {
                    // Every clip lives in the one clip folder, so the first
                    // one's parent is the window to open; anything that is not
                    // in it is left out rather than opening a second window.
                    let folder = paths.first().and_then(|path| path.parent());
                    let revealed = folder.and_then(|folder| {
                        let items: Vec<&Path> = paths
                            .iter()
                            .map(PathBuf::as_path)
                            .filter(|path| path.parent() == Some(folder))
                            .collect();
                        super::desktop::reveal_items(folder, &items).ok()
                    });
                    if revealed.is_none() {
                        let _ = tx.send(Msg::ClipError(
                            sessions::open_directory_error(tr_locale()).to_owned(),
                        ));
                    }
                }
                ClipJob::WriteComment(path, comment) => {
                    // The failure the task names -- a file another process has
                    // open, or one marked read-only -- surfaces here as the
                    // store refusing to open read-write. The row is left
                    // showing what the file still says, because nothing was
                    // written.
                    match livia::clips::write_comment(&path, &comment) {
                        Ok(()) => {
                            let _ = cmd.send(Cmd::ListClips);
                        }
                        Err(error) => {
                            let _ = tx.send(Msg::ClipError(error));
                        }
                    }
                }
                ClipJob::Delete(paths) => {
                    // `ring_buffer::remove_container`, not a second
                    // `SHFileOperationW`: it is already the recycle call plus
                    // the check that the name really left the directory, which
                    // is what stops a delete the OS accepted-but-deferred from
                    // being reported as done (task1520). A clip is one file,
                    // exactly the shape it takes for a container session.
                    //
                    // One file at a time, all of them tried: a clip another app
                    // holds open fails alone and the rest still go. The first
                    // failure is the one reported.
                    let count = paths.len();
                    let mut removed = 0usize;
                    let mut failure = None;
                    for path in &paths {
                        match livia::ring_buffer::remove_container(
                            path,
                            livia::ring_buffer::Disposal::Recycle,
                        ) {
                            Ok(()) => removed += 1,
                            Err(error) => {
                                failure.get_or_insert(error);
                            }
                        }
                    }
                    if removed > 0 {
                        let _ = cmd.send(Cmd::ListClips);
                    }
                    let _ = match failure {
                        None => tx.send(Msg::ClipNotice(if count == 1 {
                            clips_ui::deleted(tr_locale()).to_owned()
                        } else {
                            clips_ui::bulk_deleted(tr_locale(), count)
                        })),
                        Some(error) => tx.send(Msg::ClipError(format!(
                            "{}: {error}",
                            clips_ui::delete_failed(tr_locale())
                        ))),
                    };
                }
            }
        }
    });
}

/// The picture the shell has of this clip, as RGBA.
///
/// `SIIGBF_THUMBNAILONLY` is load-bearing: without it the shell happily falls
/// back to the generic mp4 file-type icon, so every row would show the same
/// picture and none of them would be of the video. Failing is ordinary -- a
/// file the provider will not open, a codec with no extractor -- and the row
/// draws its placeholder.
fn thumbnail_rgba(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY,
    };

    // Sized for the tile the grid draws now: task3020 turned this screen from
    // task2980's list of rows into a 4-column grid, so `ui/clips.slint`'s
    // `cell-width` is (pane - 12px pad*2 - 12px - 3*14px gap) / 4 -- about
    // 255px logical on a 1100px pane, and about 430px on a maximized window.
    // At 150% that is 383px physical, at 200% it is 510px, and a maximized
    // window at 150% asks for ~646px. 256x144 was therefore being *upscaled*,
    // which is the blur task3340 was filed for.
    //
    // Measured on this machine (task3340, evidence/3340-clip-thumbnail): the
    // shell honours the exact size asked for -- 512 came back 512, 768 came
    // back 768 -- and costs the same either way (~85-95ms on a file it has
    // never seen, 6-25ms warm), so the size is chosen for the biggest tile the
    // window can draw rather than for the cheapest decode.
    // `SIIGBF_BIGGERSIZEOK` is deliberately *not* set: it made the same call
    // return 1280x720 cold and 768x432 warm, i.e. a size that varies with the
    // shell's cache state, and the extra pixels have nowhere to go.
    // The tile still scales what it gets (`image-fit: cover`), so this stays a
    // fixed request -- deriving it from the live pane width would re-fetch
    // every thumbnail on every resize.
    //
    // ponytail: 768x432 RGBA is 1.3MB a tile. Only the tiles on screen (plus
    // the two rows `sessions::visible_thumbnail_range` overscans) are ever
    // requested, but `Clips::details` then keeps every one it has fetched for
    // the life of the process -- so the ceiling is the whole folder once the
    // user has scrolled through it: ~40MB at 30 clips, ~130MB at 100. If a
    // folder ever grows that far, shrink the decode before caching it (or give
    // `details` the LRU its own comment already contemplates).
    const REQUESTED: SIZE = SIZE { cx: 768, cy: 432 };

    // SAFETY: the path string outlives the call, and the factory is a COM
    // interface obtained and dropped inside this function. The HBITMAP it hands
    // back is owned by this caller, and freed below on both paths.
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).ok()?;
        let bitmap = factory.GetImage(REQUESTED, SIIGBF_THUMBNAILONLY).ok()?;
        let pixels = super::picker::colour_plane_rgba(bitmap);
        // What the shell actually handed back, which is not always what was
        // asked for: a clip that is not 16:9 fits inside the requested box
        // rather than filling it, and a provider is free to answer smaller
        // than what it was handed. Kept at
        // `debug!` so the size a real clip resolves to can be read off a run
        // without re-instrumenting (task3340).
        if let Some((width, height, _)) = pixels.as_ref() {
            tracing::debug!(
                requested_cx = REQUESTED.cx,
                requested_cy = REQUESTED.cy,
                width,
                height,
                "clip thumbnail returned by the shell"
            );
        }
        if let Err(error) = DeleteObject(HGDIOBJ(bitmap.0)).ok() {
            tracing::warn!(%error, "could not free a clip thumbnail bitmap");
        }
        pixels
    }
}

/// The strings the grid needs that are not per-tile (task3020).
pub(super) fn push_labels(ui: &AppWindow) {
    let labels = ui.global::<ClipLabels>();
    labels.set_add_comment(clips_ui::add_comment(tr_locale()).into());
    labels.set_week(clips_ui::group_week(tr_locale()).into());
    labels.set_month(clips_ui::group_month(tr_locale()).into());
    labels.set_search_placeholder(clips_ui::search_placeholder(tr_locale()).into());
    labels.set_confirm_cancel(sessions::discard_cancel(tr_locale()).into());
}

/// Rebuilds the tiles and every label around them.
pub(super) fn render(ui: &AppWindow, clips: &Clips, model: &VecModel<ClipRow>) {
    // The grid's whole geometry, decided here (task3020): a date heading spans
    // the full width and pushes everything under it down, so a tile's y needs
    // two running counts that a slint `for` cannot carry.
    let now = super::history::now_date_text();
    // The search box's answer, once: it decides the tiles, the headings, the
    // footer's count, and which of the two nothings is on screen (task3820).
    // Filtering *here*, before `place`, is what makes "only the groups with a
    // hit keep their heading" fall out for free -- `place` is handed the clips
    // that survived and breaks them into weeks exactly as it always did. Doing
    // it on the `.slint` side with `visible: false` would leave the line
    // numbers and the heading counts describing a grid that is no longer there.
    let (visible, stamps, expanded, placed) = clips.layout();
    // Counted from the placement rather than from the last tile's own line:
    // with a footprint the last tile fills a hole beside the expanded one, and
    // the pane's `content-height` would stop the grid short of its bottom.
    let grid_lines = clips_ui::line_count(&placed);

    let rows: Vec<ClipRow> = visible
        .iter()
        .filter_map(|index| clips.entries.get(*index))
        .zip(stamps.iter().zip(placed))
        .map(|(entry, (stamp, place))| {
            let view = clips.tile_view(entry);
            let detail = view.detail;
            ClipRow {
                name: entry.name.as_str().into(),
                // The history list's own relative wording, so a clip and the
                // session it was cut out of read their dates the same way.
                date: sessions::row_date_text(tr_locale(), stamp, &now).into(),
                comment: view.comment.unwrap_or_default().into(),
                thumbnail: detail
                    .and_then(|detail| detail.thumbnail.clone())
                    .unwrap_or_default(),
                has_thumbnail: detail.is_some_and(|detail| detail.thumbnail.is_some()),
                column: place.column,
                line: place.line,
                heading_offset: place.heading_offset,
                heading: place.heading.as_str().into(),
                span: place.span,
                selected: clips.selection.is_selected(&entry.path),
            }
        })
        .collect();

    if model.row_count() == rows.len() {
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
    } else {
        model.set_vec(rows);
    }

    // task3030: `list` creates the folder itself now, so there is no longer a
    // "folder missing" screen to show separately -- 0 entries is always
    // 「クリップがありません」, whether the folder was just created or has
    // simply never received a save.
    ui.set_clips_empty_text(
        if clips.entries.is_empty() {
            clips_ui::empty(tr_locale())
        } else {
            ""
        }
        .into(),
    );
    // The second nothing, on its own property (task3820). Not folded into
    // `empty-text`: that one is what the toolbar is gated on, so a query that
    // stopped matching would take the search field down with the grid and
    // leave no way to clear the text that emptied it. The picker draws the
    // same distinction with `empty-is-filter`.
    ui.set_clips_filter_empty_text(
        if !clips.entries.is_empty() && visible.is_empty() {
            clips_ui::filter_empty(tr_locale())
        } else {
            ""
        }
        .into(),
    );
    // Counted over what is on the grid, not over the folder (task3820). The
    // wording is 「N 件を表示」 -- "showing N" -- so under a filter the honest N
    // is the number being shown. Round20 §9 does not rule on this; it is this
    // task's own call, and it goes out on the next design letter.
    ui.set_clips_footer_text(
        sessions::footer_visible_text(
            tr_locale(),
            visible.len(),
            visible
                .iter()
                .filter_map(|index| clips.entries.get(*index))
                .map(|entry| entry.bytes)
                .sum(),
        )
        .into(),
    );

    let icons = ui.global::<Icons>();
    let entries: Vec<MenuEntry> = match clips.menu {
        None => Vec::new(),
        Some(entry) => menu_actions(clips.menu_targets(entry).len())
            .iter()
            .map(|action| MenuEntry {
                label: match action {
                    ClipMenuAction::Open => clips_ui::menu_open(tr_locale()),
                    ClipMenuAction::Reveal => clips_ui::menu_reveal(tr_locale()),
                    ClipMenuAction::EditComment => clips_ui::menu_edit_comment(tr_locale()),
                    ClipMenuAction::Delete => clips_ui::menu_delete(tr_locale()),
                }
                .into(),
                icon: match action {
                    ClipMenuAction::Open => icons.get_play(),
                    ClipMenuAction::Reveal => icons.get_folder(),
                    ClipMenuAction::EditComment => icons.get_pencil(),
                    ClipMenuAction::Delete => icons.get_discard(),
                },
                hint: match action {
                    ClipMenuAction::Open => clips_ui::open_hint(tr_locale()),
                    _ => "",
                }
                .into(),
                // The red marks the entry that removes something. Behind it
                // is a confirmation for one clip or many, like the history
                // screen's 破棄 -- the user's 2026-09-19 ruling, which retired
                // task2990's dialog-free single delete (`ask_delete`).
                separator_above: matches!(action, ClipMenuAction::Delete),
                danger: matches!(action, ClipMenuAction::Delete),
                // The clip menu has no refused entry today; it carries the
                // field because `MenuEntry` is shared with the history
                // screen, whose 破棄 is disabled over a recording
                // (t260920-bb09).
                disabled: false,
            })
            .collect(),
    };
    ui.set_clips_menu_open(!entries.is_empty());
    ui.set_clips_menu_entries(Rc::new(VecModel::from(entries)).into());
    // Which tile the menu is acting on, so it can say so with a border
    // (task3350). Derived from the same `clips.menu` the entries above are, so
    // it goes back to -1 on its own: every place that clears the menu
    // (`menu_dismissed`, `menu_selected`, `listed`, and the re-target in
    // `row_menu_requested`) ends in this render.
    // Back into grid coordinates: `clips.menu` is an entry index, and the
    // border is drawn on a tile (task3820). A menu whose entry is not on the
    // filtered grid resolves to -1 -- which cannot normally happen, since
    // `reconcile` closes one first, but the border must not land on a
    // neighbour if it ever does.
    ui.set_clips_menu_row(
        clips
            .menu
            .and_then(|entry| visible.iter().position(|index| *index == entry))
            .and_then(|row| i32::try_from(row).ok())
            .unwrap_or(-1),
    );
    ui.set_clips_editing_row(
        clips
            .editing
            .as_ref()
            .and_then(|edit| i32::try_from(edit.row).ok())
            .unwrap_or(-1),
    );
    ui.set_clips_month_grouping(clips.grouping == ClipGrouping::Month);
    // task3880. -1 is "nothing expanded".
    ui.set_clips_expanded_row(
        expanded
            .and_then(|row| i32::try_from(row).ok())
            .unwrap_or(-1),
    );
    ui.set_clips_grid_lines(grid_lines);
    // The selection (t260916-5568): the count Esc and Delete gate on, and the
    // bulk delete's confirmation.
    ui.set_clips_selection_count(i32::try_from(clips.selection.len()).unwrap_or(i32::MAX));
    let confirming: Vec<&ClipEntry> = clips
        .confirm
        .iter()
        .filter_map(|path| clips.entries.iter().find(|entry| &entry.path == path))
        .collect();
    // One clip reads as the history's single discard does, two or more as its
    // bulk discard (follow-up of t260916-5568, 2026-09-19).
    let (title, lead, ok) = if confirming.is_empty() {
        (String::new(), String::new(), String::new())
    } else if confirming.len() == 1 {
        (
            clips_ui::delete_confirm_title(tr_locale()).to_owned(),
            clips_ui::delete_lead(tr_locale()).to_owned(),
            clips_ui::delete_confirm(tr_locale()).to_owned(),
        )
    } else {
        let bytes: u64 = confirming.iter().map(|entry| entry.bytes).sum();
        (
            clips_ui::bulk_delete_confirm_title(tr_locale(), confirming.len()),
            clips_ui::bulk_delete_lead(tr_locale(), &sessions::format_bytes(bytes)),
            clips_ui::bulk_delete_confirm(tr_locale(), confirming.len()),
        )
    };
    ui.set_clips_confirm_title(title.into());
    ui.set_clips_confirm_lead(lead.into());
    ui.set_clips_confirm_ok(ok.into());
    // task3520, last: the surface is drawn inside the raised copy of the tile
    // `expanded` names, so `ClipVm.row` has to be the same grid index the
    // property above just took.
    render_player(ui, clips, expanded);
}

/// Writes the selection onto the tiles whose flag changed, and nothing else
/// (t260916-5568). A marquee calls this on every move, and a full `render`
/// per move would rebuild every row -- history's task189 rule.
fn apply_selection(ui: &AppWindow, clips: &Clips, model: &VecModel<ClipRow>) {
    let paths = clips.visible_paths();
    if model.row_count() != paths.len() {
        render(ui, clips, model);
        return;
    }
    for (index, path) in paths.iter().enumerate() {
        let selected = clips.selection.is_selected(path);
        if let Some(mut row) = model.row_data(index) {
            if row.selected != selected {
                row.selected = selected;
                model.set_row_data(index, row);
            }
        }
    }
    ui.set_clips_selection_count(i32::try_from(clips.selection.len()).unwrap_or(i32::MAX));
}

/// A plain click's expansion, the same move `row-expand-toggled` makes.
fn toggle_row(ui: &AppWindow, clips: &mut Clips, row: i32) {
    clips.close_editor();
    clips.menu = None;
    let pressed = clips.entry_index(row);
    clips.expanded = clips_ui::toggle_expanded(clips.expanded, pressed);
    sync_player(ui, clips);
}

/// Every clip delete's first step -- the menu's, and the Delete key's, for one
/// clip or many: it opens the confirmation and deletes nothing. The dialog's
/// 「削除する」 is what reaches `delete_clips` (`confirm_accepted`).
///
/// The user's 2026-09-19 ruling (follow-up of t260916-5568): 「どちらも確認
/// あった方がある、uxは揃えたい」 -- the history asks for one session and for
/// several, so the clips do too.
fn ask_delete(clips: &mut Clips, targets: Vec<PathBuf>) {
    if targets.is_empty() {
        return;
    }
    // Closed first: the editor is drawn by row index, and an accepted delete
    // moves every row under it.
    clips.menu = None;
    clips.editing = None;
    clips.confirm = targets;
}

/// Sends `paths` to the recycle bin as one job -- one re-scan and one toast
/// however many there are (t260916-5568).
///
/// The clip being played goes down first: the engine is reading the file, and
/// the history screen unloads its review before a discard for the same reason
/// (task1520) -- the file is about to go, so its picture cannot stay.
fn delete_clips(ui: &AppWindow, clips: &mut Clips, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let playing = clips.player.current().map(|clip| clip.path.clone());
    if playing.is_some_and(|playing| paths.contains(&playing)) {
        clips.expanded = None;
        sync_player(ui, clips);
    }
    clips.editing = None;
    let _ = clips.jobs.send(ClipJob::Delete(paths));
}

/// Opens the comment editor on one tile, from either of its two entry points
/// (task3020 §1-4): the overlay's pencil and the context menu.
///
/// Seeded with what the tile is showing, which is the comment read out of the
/// file itself -- and with "" for one whose details have not landed yet, so the
/// editor still opens on a cold grid.
/// `row` is a tile on the grid, which under a search is not the same number as
/// the entry behind it (task3820) -- so the file is resolved through
/// `entry_index`, while `ClipEdit::row` keeps the tile index the `.slint` side
/// draws the editor on.
fn open_editor(clips: &mut Clips, row: usize) {
    let Some(index) = i32::try_from(row)
        .ok()
        .and_then(|row| clips.entry_index(row))
    else {
        return;
    };
    let Some(path) = clips.entries.get(index).map(|entry| entry.path.clone()) else {
        return;
    };
    // The tile's own answer, not the cache's (t260916-568d): reopening the
    // editor on a clip whose write is still in flight would otherwise seed an
    // empty field, and `needs_write` would then read a no-op Enter as clearing
    // the comment the user had just typed.
    let original = clips
        .entries
        .get(index)
        .map(|entry| clips.tile_view(entry))
        .and_then(|view| view.comment)
        .unwrap_or_default()
        .to_owned();
    clips.editing = Some(ClipEdit {
        row,
        path,
        original,
    });
}

/// The scan itself, run on the session worker: it already owns the settings the
/// clip folder is configured in, and reading a directory's metadata is the same
/// class of work as `ListSessions`.
pub(super) fn scan(settings: &Mutex<AppSettings>, tx: &Sender<Msg>) {
    let directory = settings
        .lock()
        .ok()
        .and_then(|current| livia::clips::clip_directory(&current).ok())
        .unwrap_or_default();
    let _ = tx.send(Msg::Clips {
        entries: livia::clips::list(&directory),
    });
}

pub(super) fn wire(
    ui: &AppWindow,
    clips: &Rc<std::cell::RefCell<Clips>>,
    clip_rows: &Rc<VecModel<ClipRow>>,
    settings: &Arc<Mutex<AppSettings>>,
) {
    // The saved 週/月 (the user, 2026-09-20), read once and written back by
    // `on_clips_grouping_changed` below -- the same shape the history screen's
    // view mode now has.
    clips.borrow_mut().grouping = if settings_snapshot(settings).clip_month_grouping {
        ClipGrouping::Month
    } else {
        ClipGrouping::Week
    };
    // Each of these re-renders, which destroys an open editor before its own
    // blur can fire -- so each ends the edit first, exactly as every
    // `session_handler!` on the history screen does. That used to be a commit;
    // since task3430 it is a discard, because reaching any of these handlers
    // means the click landed somewhere that is not the open field.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_row_menu_requested(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.close_editor();
            // The tile pressed, resolved to its entry (task3820). Stored that
            // way round because the menu outlives the grid it was opened on:
            // between the right-click and the 削除 the worker can land a
            // comment that changes what the filter keeps.
            clips.menu = clips.entry_index(row);
            // Explorer's rule, the history screen's since task1120: inside
            // the selection the menu acts on all of it; outside it, the tile
            // replaces the selection first (t260916-5568, design 4).
            if let Some(path) = clips.row_path(row) {
                clips.selection.select_for_menu(&path);
            }
            render(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_menu_dismissed(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.close_editor();
            clips.menu = None;
            render(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_menu_selected(move |entry| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            // Mapped through the same list `render` drew (t260916-5568): a
            // menu over a selection has two entries, not four.
            let targets = clips
                .menu
                .map(|entry| clips.menu_targets(entry))
                .unwrap_or_default();
            let chosen = usize::try_from(entry)
                .ok()
                .and_then(|index| menu_actions(targets.len()).get(index).copied());
            // The row is resolved to a path here, once, while the menu still
            // says which row it belongs to -- and it is the path, never the
            // index, that the delete and the edit carry from here on. The list
            // is a re-scan with no index behind it, so an index outlives its
            // row by exactly nothing (CLAUDE.md's rule for a destructive
            // click, in the form it takes with no click left to make).
            // `(tile, file)`: the file comes from the entry the menu was opened
            // on, the tile is where 「コメントを編集」 has to draw its field --
            // and under a search those are two different numbers (task3820).
            let visible = clips.visible();
            let target = clips.menu.and_then(|entry| {
                let row = visible.iter().position(|index| *index == entry)?;
                clips
                    .entries
                    .get(entry)
                    .map(|clip: &ClipEntry| (row, clip.path.clone()))
            });
            // Kept before the menu is dropped: 「再生」 needs the *entry*, which
            // is what `expanded` is indexed by, and `target` above has already
            // traded it for the tile it happens to sit on.
            let entry = clips.menu;
            clips.menu = None;
            if let (Some(action), Some((row, path))) = (chosen, target) {
                match action {
                    ClipMenuAction::Open => {
                        // task3530 retired the external player, so 「再生」 is
                        // the in-place one -- the same move a click on the
                        // tile makes (task3890), minus the toggle: a menu
                        // entry that reads 「再生」 always plays and never
                        // collapses, so this assigns rather than toggling.
                        // The editor goes first for the reason the delete
                        // below drops it: the expansion moves every tile under
                        // this one and the field is drawn by tile index.
                        clips.close_editor();
                        clips.expanded = entry;
                        sync_player(&ui, &mut clips);
                        // The one state the assignment above cannot reach:
                        // `sync_player` compares paths and returns early when
                        // the engine is already on this clip, so a tile that
                        // was expanded *and paused* saw the menu entry do
                        // nothing (task3530 follow-up). Resumed through
                        // `toggle_play`, the same door the Space / `k` key uses
                        // (t260916-956e) -- the offscreen debt and the intent
                        // latch are its, not a second copy here. Guarded on
                        // `is_paused`, because a menu entry that reads 「再生」
                        // must never pause: a freshly opened clip is already
                        // playing (`open_player`) and a refused one has nothing
                        // open, and neither answers `true`.
                        if clips.player.is_paused() {
                            toggle_play(&ui, &mut clips);
                        }
                    }
                    ClipMenuAction::Reveal => {
                        let _ = clips.jobs.send(ClipJob::Reveal(targets));
                    }
                    ClipMenuAction::EditComment => open_editor(&mut clips, row),
                    ClipMenuAction::Delete => {
                        // One or many, it asks first (the user's 2026-09-19
                        // ruling, which retires task2990's dialog-free single
                        // delete). One is the tile the menu was opened on.
                        let targets = if targets.len() > 1 {
                            targets
                        } else {
                            vec![path]
                        };
                        ask_delete(&mut clips, targets);
                    }
                }
            }
            render(&ui, &clips, &rows);
        });
    }

    // The press that expands or collapses one tile (task3880). Only the
    // placement changes, so this is `render` and nothing else -- no scan, no
    // detail fetch, no seek. The `viewport-y` nudge that follows an expansion
    // is the pane's own, because its inputs are pixels this side never sees.
    //
    // Three gestures reach this since task3890 -- a click on a tile, a click
    // on the expanded tile, and Esc -- and none of them decides the new state:
    // each sends the tile it pressed and the rule is
    // `clips_ui::toggle_expanded`, where it can be tested. A gesture layer
    // cannot.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_row_expand_toggled(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.close_editor();
            // And the menu, for the same reason the editor goes (ユーザー報告
            // 2026-09-14): a menu opened over *another* tile while this one was
            // expanded is still mounted at its old pixel position, and the
            // collapse moves every tile out from under it. Unconditional, like
            // `close_editor` above -- expanding is the same move as collapsing.
            clips.menu = None;
            // Through `entry_index`, never the raw tile number: under a search
            // the two are different lists (task3820).
            let pressed = clips.entry_index(row);
            clips.expanded = clips_ui::toggle_expanded(clips.expanded, pressed);
            // task3520: the engine follows whatever `toggle_expanded` just
            // decided, which is how 「他タイルのクリック＝そのタイルの再生に
            // 切り替え」 (round20 §2-3) arrives with no branch of its own -- a
            // switch is a close whose next state happens to be `Some`.
            // Before the render, because a clip that refuses to open takes the
            // expansion back down with it.
            sync_player(&ui, &mut clips);
            render(&ui, &clips, &rows);
        });
    }

    // The overlay's pencil (task3020): the second way into the same editor.
    // The tile's own click expands it (task3890), so the pencil is still what
    // a click on the comment has to go through -- it sits above the tile's
    // `touch`, which is what keeps the press off the expand.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_row_edit_requested(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            // Whatever was open elsewhere is saved first, exactly as every
            // other re-rendering handler here does.
            clips.close_editor();
            if let Ok(row) = usize::try_from(row) {
                open_editor(&mut clips, row);
            }
            render(&ui, &clips, &rows);
        });
    }

    // The toolbar's search box (task3820), on every keystroke.
    //
    // The editor and the menu both close, exactly as the history screen's own
    // search handler closes them: both are drawn by tile index, and a filter
    // that changes which clip is on which tile would leave the comment field
    // mounted over somebody else's clip -- and the next Enter would write into
    // it. (The file behind the editor is checked as well, in `reconcile`;
    // closing here is the cheaper answer and there is no path that reaches this
    // handler *while* typing in the tile's own field.)
    //
    // Nothing is re-scanned: the filter runs over the list already in hand.
    // `queue_visible_details` is called because a query changes *what* has to
    // be fetched -- see its own comment.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_search_changed(move |query| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.query = query.to_string();
            // What the query hid leaves the selection (t260916-5568, design 7).
            let order = clips.visible_paths();
            clips.selection.retain(&order);
            clips.confirm.retain(|path| order.contains(path));
            clips.close_editor();
            clips.menu = None;
            queue_visible_details(&mut clips);
            render(&ui, &clips, &rows);
        });
    }

    // 週 / 月. Only the headings change, so nothing is re-scanned and nothing
    // is re-fetched -- `render` re-runs the placement and that is the whole of
    // it.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        let grouping_settings = settings.clone();
        ui.on_clips_grouping_changed(move |month| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.close_editor();
            clips.grouping = if month {
                ClipGrouping::Month
            } else {
                ClipGrouping::Week
            };
            commit_settings(&grouping_settings, |current| {
                current.clip_month_grouping = month;
            });
            render(&ui, &clips, &rows);
        });
    }

    // The editor's three exits. Enter arrives as `committed`; Esc and blur
    // both as `cancelled` (task3430's contract, inherited whole from the
    // history screen's `InlineEdit` -- it was 194-D's, with blur on the other
    // side, until the user's 2026-09-07 ruling).
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_comment_committed(move |value| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            commit_open_edit(&ui, &mut clips, value.to_string());
            render(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_edit_cancelled(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.close_editor();
            render(&ui, &clips, &rows);
        });
    }

    {
        let clips_rc = clips.clone();
        ui.on_clips_visible_range_changed(move |first, count| {
            let mut clips = clips_rc.borrow_mut();
            clips.visible_range = Some((first.max(0) as usize, count.max(0) as usize));
            queue_visible_details(&mut clips);
        });
    }

    // ---- the selection (t260916-5568) ----
    //
    // A press, its moves and its release. Rust decides which of click and
    // marquee it was (`ClipPress::dragging`), so the `.slint` side reports
    // and draws and nothing more.
    {
        let clips_rc = clips.clone();
        ui.on_clips_tile_pressed(move |row, x, y, cell_w, cell_h, shift, ctrl| {
            clips_rc.borrow_mut().press = Some(ClipPress {
                row: Some(row),
                origin: (x, y),
                cell: (cell_w, cell_h),
                shift,
                ctrl,
                dragging: false,
            });
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_ground_pressed(move |x, y, cell_w, cell_h, shift, ctrl| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.press = Some(ClipPress {
                row: None,
                origin: (x, y),
                cell: (cell_w, cell_h),
                shift,
                ctrl,
                dragging: true,
            });
            // Bare ground selects nothing, so without Ctrl the press alone
            // clears -- Explorer's empty-space click (design 2).
            clips.selection.begin_marquee(ctrl);
            apply_selection(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_pointer_moved(move |x, y| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            let Some((origin, cell, ctrl, dragging)) = clips
                .press
                .as_ref()
                .map(|press| (press.origin, press.cell, press.ctrl, press.dragging))
            else {
                return;
            };
            if !dragging {
                if !clips_ui::past_drag_threshold(origin, (x, y)) {
                    return;
                }
                if let Some(press) = clips.press.as_mut() {
                    press.dragging = true;
                }
                clips.selection.begin_marquee(ctrl);
            }
            let tiles = clips.tile_rects(cell);
            clips.selection.marquee(
                &tiles,
                clips_ui::Rect::from_corners(origin.0, origin.1, x, y),
            );
            ui.set_clips_marquee_active(true);
            apply_selection(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_pointer_released(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            ui.set_clips_marquee_active(false);
            let Some(press) = clips.press.take() else {
                return;
            };
            if press.dragging {
                return;
            }
            let Some(row) = press.row else { return };
            let Some(path) = clips.row_path(row) else {
                return;
            };
            let order = clips.selectable_paths();
            match clips
                .selection
                .click(&path, press.shift, press.ctrl, &order)
            {
                // Plain: the selection is already cleared, and the tile plays
                // exactly as a click always made it (round20 §6).
                ClickOutcome::Expand => toggle_row(&ui, &mut clips, row),
                ClickOutcome::Selected => clips.menu = None,
            }
            render(&ui, &clips, &rows);
        });
    }

    // Delete: asks first however many are selected, as the history's Delete
    // does (the user's 2026-09-19 ruling; t260916-5568 had let one go with no
    // dialog).
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_delete_selected(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            let targets = clips.selection.selected().to_vec();
            if targets.is_empty() {
                return;
            }
            ask_delete(&mut clips, targets);
            render(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_select_all(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            let order = clips.selectable_paths();
            clips.selection.select_all(&order);
            apply_selection(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_selection_cleared(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.selection.clear();
            apply_selection(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_confirm_accepted(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            let targets = std::mem::take(&mut clips.confirm);
            delete_clips(&ui, &mut clips, targets);
            render(&ui, &clips, &rows);
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let rows = clip_rows.clone();
        ui.on_clips_confirm_cancelled(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.confirm.clear();
            render(&ui, &clips, &rows);
        });
    }

    wire_player(ui, clips);
}

/// `ClipVm`'s own callbacks (task3520): the frame pump, the stage gestures and
/// the five controls round20 §2-2 allows.
///
/// Split from `wire` because the two halves answer different things -- the grid
/// above, one clip's transport here -- and because every handler in this one
/// needs the hold timer.
fn wire_player(ui: &AppWindow, clips: &Rc<std::cell::RefCell<Clips>>) {
    // A slint timer, so it runs on the event loop and needs no synchronization
    // with the handlers that arm it.
    let hold_timer = Rc::new(slint::Timer::default());

    // The engine rings this through the event loop after every publish.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>().on_frame_ready(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            pump_player(&ui, &mut clips);
            render_player(&ui, &clips, mounted_row(&ui));
        });
    }

    // How big the picture is being drawn, from the surface's own layout.
    //
    // A paused clip is resampled again here, on the review's own path
    // (`paused_stage_image`, t260914-39c4): paused, no next frame comes to do
    // it, and the picture stayed sized for the old surface -- measurably soft
    // after a resize (task3520's 2026-09-15 sweep). Playing, the engine's next
    // frame arrives resampled already, so this does not draw twice.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>()
            .on_stage_size_changed(move |width, height| {
                let Some(ui) = weak.upgrade() else { return };
                let target = pb::stage_target(width, height, ui.window().scale_factor());
                let mut clips = clips_rc.borrow_mut();
                if clips.stage_target == target {
                    return;
                }
                clips.stage_target = target;
                // The same line the review stage logs (t260914-a025): this
                // caller used to be silent, so a log-only reading never saw it.
                super::shell::log_stage_resize(ui.window(), "clips", target);
                if let Some(engine) = clips.engine.as_ref() {
                    engine.shared().set_display_target(target);
                }
                // `clip.playing` is what `render_player` puts on the glyph --
                // the engine's status behind the `play_intent` latch, already
                // folded together by `pump_player`.
                if clips.player.is_paused() {
                    if let Some(image) = paused_stage_image(clips.last_frame.as_ref(), target) {
                        ui.global::<ClipVm>().set_stage_frame(image);
                    }
                }
            });
    }

    // Any pointer sign of life over the picture: what wakes the bar back up.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>().on_activity(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.last_activity = Instant::now();
            render_player(&ui, &clips, mounted_row(&ui));
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>().on_play_toggled(move || {
            let Some(ui) = weak.upgrade() else { return };
            toggle_play(&ui, &mut clips_rc.borrow_mut());
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>().on_seek(move |ratio| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            // Through `ClipPlayer`, which is where the clamping and the NaN a
            // zero-width bar can produce are already handled and tested.
            let Some(target) = clips
                .player
                .current()
                .map(|clip| clip.seek_target(f64::from(ratio)))
            else {
                return;
            };
            seek_to(&mut clips, target);
            render_player(&ui, &clips, mounted_row(&ui));
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>().on_volume_changed(move |ratio| {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.volume_percent = (f64::from(ratio).clamp(0.0, 1.0) * 100.0).round() as i64;
            // Moving the slider is how a muted clip is unmuted, the same way
            // the review's is (round5 §4-C): the gesture says "this loud".
            clips.muted = false;
            let (percent, muted) = (clips.volume_percent, clips.muted);
            clips.send(PlaybackCommand::SetVolume { percent, muted });
            clips.last_activity = Instant::now();
            render_player(&ui, &clips, mounted_row(&ui));
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>().on_mute_toggled(move || {
            let Some(ui) = weak.upgrade() else { return };
            toggle_mute(&ui, &mut clips_rc.borrow_mut());
        });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>().on_repeat_toggled(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut clips = clips_rc.borrow_mut();
            clips.repeat = !clips.repeat;
            // Nothing is sent to the engine here. Repeat only ever decides what
            // `pump_player` does at the park, so turning it on while a clip is
            // already parked on its end leaves that clip parked -- the loop
            // starts from the next play. Turning it off mid-clip likewise only
            // changes what happens when the end arrives.
            //
            // A press on the row is a sign of life like every other control's,
            // or the 3-second hide could take the row away under the pointer
            // that just used it.
            clips.last_activity = Instant::now();
            render_player(&ui, &clips, mounted_row(&ui));
        });
    }

    {
        let weak = ui.as_weak();
        ui.global::<ClipVm>().on_fullscreen_toggled(move || {
            let Some(ui) = weak.upgrade() else { return };
            let on = !ui.window().is_fullscreen();
            set_clip_fullscreen(&ui, on);
        });
    }

    // The ✕ on the surface. Out through the same door Esc and the click outside
    // already use (round20 §2-3): pressing the row that is open collapses it,
    // and the collapse is what drops the engine.
    {
        let weak = ui.as_weak();
        ui.global::<ClipVm>().on_close_requested(move || {
            let Some(ui) = weak.upgrade() else { return };
            let row = ui.global::<ClipVm>().get_row();
            if row >= 0 {
                ui.invoke_clips_row_expand_toggled(row);
            }
        });
    }

    // The pane's `FocusScope` asks here for every key Esc did not want
    // (t260916-956e). Answering `false` is what lets the press carry on being
    // whatever else it was -- a character in the comment field never reaches
    // this at all, because the `TextInput` holds the focus.
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        ui.global::<ClipVm>().on_key_pressed_cb(move |text| {
            let Some(ui) = weak.upgrade() else {
                return false;
            };
            let Some(action) = clip_player::clip_key_action(text.as_str()) else {
                return false;
            };
            // Nothing is mounted: the keys belong to the player, not the grid.
            if !clips_rc.borrow().player.is_open() {
                return false;
            }
            match action {
                clip_player::ClipKeyAction::TogglePlay => {
                    toggle_play(&ui, &mut clips_rc.borrow_mut())
                }
                clip_player::ClipKeyAction::ToggleMute => {
                    toggle_mute(&ui, &mut clips_rc.borrow_mut())
                }
                // Outside any borrow: this one reaches back into the window.
                clip_player::ClipKeyAction::ToggleFullscreen => {
                    set_clip_fullscreen(&ui, !ui.window().is_fullscreen());
                }
                // The side zones' tap, by the same two lines it runs in
                // `on_stage_pressed` -- so the step stays stated once, in
                // `pb::tap_delta_100ns`.
                clip_player::ClipKeyAction::Seek(zone) => {
                    let mut clips = clips_rc.borrow_mut();
                    let target = clips
                        .player
                        .current()
                        .map(|clip| clip.position_100ns + pb::tap_delta_100ns(zone));
                    if let Some(target) = target {
                        seek_to(&mut clips, target);
                    }
                    render_player(&ui, &clips, mounted_row(&ui));
                }
            }
            true
        });
    }

    // ---------- the stage gestures ----------
    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let hold = hold_timer.clone();
        ui.global::<ClipVm>()
            .on_stage_pressed(move |x, y, width, height| {
                let Some(ui) = weak.upgrade() else { return };
                // Outside the picture is the letterbox, which is not the video:
                // no gesture starts there (task1010).
                let Some(ratio) = pb::stage_hit(x, y, width, height) else {
                    return;
                };
                let zone = pb::stage_zone(ratio);
                {
                    let mut clips = clips_rc.borrow_mut();
                    clips.last_activity = Instant::now();
                    let since = clips
                        .last_center_tap
                        .map(|at| at.elapsed().as_millis() as u64);
                    if pb::stage_double_press(since, zone) {
                        // The second press of a double click. Full screen goes
                        // now rather than on the release slint would have
                        // reported it on, and the tap that opened the pair is
                        // undone, so the whole gesture leaves the transport
                        // where it found it.
                        clips.last_center_tap = None;
                        clips.center_tap_pending = false;
                        clips.hold = None;
                        toggle_play(&ui, &mut clips);
                        drop(clips);
                        hold.stop();
                        set_clip_fullscreen(&ui, !ui.window().is_fullscreen());
                        return;
                    }
                    clips.hold = None;
                    clips.last_center_tap = None;
                    clips.center_tap_pending = zone == pb::StageZone::Center;
                    if zone != pb::StageZone::Center {
                        // ±10s, on the press: the side zones have no second
                        // meaning waiting on a release.
                        let target = clips
                            .player
                            .current()
                            .map(|clip| clip.position_100ns + pb::tap_delta_100ns(zone));
                        if let Some(target) = target {
                            seek_to(&mut clips, target);
                        }
                        render_player(&ui, &clips, mounted_row(&ui));
                        return;
                    }
                    clips.last_center_tap = Some(Instant::now());
                }
                // Centre hold = x2 for as long as it is held (task650). One
                // shot, not a repeat: this is continuous playback, so there is
                // nothing to step.
                let weak = ui.as_weak();
                let clips_rc = clips_rc.clone();
                hold.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(pb::STAGE_HOLD_DELAY_MS),
                    move || {
                        let Some(ui) = weak.upgrade() else { return };
                        let mut clips = clips_rc.borrow_mut();
                        let Some(playing) = clips.player.current().map(|clip| clip.playing) else {
                            return;
                        };
                        // Rate 1.0 as the "current": nothing on this screen can
                        // set another one, and the restore is still read out of
                        // the struct so a future rate control needs no change
                        // here.
                        let held = pb::stage_hold_rate(playing, 1.0);
                        clips.hold = Some(held);
                        // Held, so the release cannot fall through to the
                        // play/pause tap: a long press is a skim, not a
                        // transport command.
                        clips.center_tap_pending = false;
                        clips.send(PlaybackCommand::SetRate(held.applied));
                        // A hold that started paused runs anyway and stops
                        // again on release (task1010).
                        if held.repause {
                            clips.set_playing(true);
                        }
                        render_player(&ui, &clips, mounted_row(&ui));
                    },
                );
            });
    }

    {
        let weak = ui.as_weak();
        let clips_rc = clips.clone();
        let hold = hold_timer.clone();
        ui.global::<ClipVm>().on_stage_released(move || {
            let Some(ui) = weak.upgrade() else { return };
            hold.stop();
            let mut clips = clips_rc.borrow_mut();
            if let Some(held) = clips.hold.take() {
                // The rate goes back to whatever it was, and a hold that began
                // paused hands back a paused clip at wherever it reached
                // (task1010).
                clips.send(PlaybackCommand::SetRate(held.restore));
                if held.repause {
                    clips.set_playing(false);
                }
            } else if clips.center_tap_pending {
                toggle_play(&ui, &mut clips);
            }
            clips.center_tap_pending = false;
            render_player(&ui, &clips, mounted_row(&ui));
        });
    }
}

/// Called after a listing or a detail lands: the rows changed, so the window
/// may now want details it did not before.
pub(super) fn refresh(ui: &AppWindow, clips: &mut Clips, model: &VecModel<ClipRow>) {
    queue_visible_details(clips);
    render(ui, clips, model);
}

#[cfg(test)]
mod tests {
    use super::*;
    use livia::playback::FullFrame;

    fn frame(width: u32, height: u32) -> (u32, u32, FullFrame) {
        let rgba = vec![128u8; (width * height * 4) as usize];
        (width, height, FullFrame::Rgba(rgba))
    }

    /// t260914-39c4: the held picture belongs to the engine that drew it. A
    /// `last_frame` that outlived `drop_player` would be what the next clip's
    /// paused-resize redraw puts on its surface.
    #[test]
    fn dropping_the_player_forgets_the_held_frame() {
        let (jobs, _jobs_rx) = crossbeam_channel::unbounded();
        let (msgs, _msgs_rx) = crossbeam_channel::unbounded();
        let (cmds, _cmds_rx) = crossbeam_channel::unbounded();
        let mut clips = Clips::new(jobs, msgs, cmds);
        clips.last_frame = Some(frame(4, 4));
        // Control: the frame is there to be forgotten.
        assert!(clips.last_frame.is_some());
        clips.drop_player();
        assert!(clips.last_frame.is_none());
    }

    /// The shared body of the paused redraw: resampled to the surface when there
    /// is one, the recording's own size before the surface has reported, and
    /// nothing at all without a held frame.
    #[test]
    fn a_paused_stage_image_is_sized_for_the_surface() {
        let held = frame(64, 36);
        let size = |image: Image| (image.size().width, image.size().height);
        assert_eq!(
            paused_stage_image(Some(&held), Some((16, 9))).map(size),
            Some((16, 9))
        );
        assert_eq!(
            paused_stage_image(Some(&held), None).map(size),
            Some((64, 36))
        );
        assert!(paused_stage_image(None, Some((16, 9))).is_none());
    }

    /// t260916-5568: a menu over a selection keeps only what makes sense for
    /// several files, and a menu over one clip keeps all four.
    #[test]
    fn a_menu_over_a_selection_offers_only_reveal_and_delete() {
        assert!(menu_actions(1) == MENU_ACTIONS.to_vec());
        assert!(menu_actions(0) == MENU_ACTIONS.to_vec());
        assert!(menu_actions(2) == vec![ClipMenuAction::Reveal, ClipMenuAction::Delete]);
    }

    /// Six clips with entry 1 expanded, which `place` lays out as a 3x3 block
    /// over columns 1..3 of lines 0..2, leaving entry 0 above entry 2 in
    /// column 0. Returns the clips, the cell size, and every placement's
    /// rectangle including the expanded one's -- `Clips::tile_rects` now drops
    /// that, which is the thing under test, so the geometry has to come from
    /// the pure function.
    fn expanded_grid() -> (Clips, (f32, f32), Vec<clips_ui::Rect>) {
        let mut clips = headless_clips();
        clips.listed(
            (1..=6)
                .map(|n| entry(&format!("panel-{n:02}.mp4")))
                .collect(),
        );
        clips.expanded = Some(1);
        let cell = (100.0, 50.0);
        let (_, _, _, placed) = clips.layout();
        let rects = clips_ui::tile_rects(&placed, cell.0, cell.1);
        (clips, cell, rects)
    }

    fn picked(selection: &ClipSelection<PathBuf>) -> Vec<String> {
        selection
            .selected()
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }

    /// The user's 2026-09-19 ruling (t260920-c6eb): a rectangle that sweeps the
    /// expanded tile takes its neighbour and not the expansion. The expanded
    /// tile is the playback surface and draws no selection mark, so before this
    /// it joined the next Delete with nothing on screen to say so -- measured on
    /// the real machine at 「2 件のクリップを削除しますか？」 with one tile framed
    /// (`evidence/t260920-c6eb-.../shots/b1-pre-03-marquee-done.png`).
    #[test]
    fn a_marquee_across_the_expanded_tile_takes_only_its_neighbour() {
        let (clips, cell, rects) = expanded_grid();
        let big = rects[1];
        let neighbour = rects[2];
        // from inside the neighbour to inside the expanded block, along the
        // neighbour's own line so no other tile is touched
        let drag = clips_ui::Rect::from_corners(
            neighbour.x + 1.0,
            neighbour.y + 1.0,
            big.x + 5.0,
            neighbour.y + 5.0,
        );
        // Control: the drag really does reach into the expansion's rectangle --
        // without this the assert below would pass for the wrong reason.
        assert!(big.overlaps(&drag));
        assert!(neighbour.overlaps(&drag));

        let mut selection = ClipSelection::default();
        selection.begin_marquee(false);
        selection.marquee(&clips.tile_rects(cell), drag);
        assert_eq!(picked(&selection), ["panel-03.mp4"]);
    }

    /// The positive control for the test above: with nothing expanded, a
    /// rectangle laid between two neighbouring tiles takes both. What changed
    /// is the expansion, not the marquee.
    #[test]
    fn the_same_marquee_takes_both_tiles_once_the_expansion_is_closed() {
        let (mut clips, cell, _) = expanded_grid();
        clips.expanded = None;
        let (_, _, _, placed) = clips.layout();
        let rects = clips_ui::tile_rects(&placed, cell.0, cell.1);
        let drag = clips_ui::Rect::from_corners(
            rects[1].x + 1.0,
            rects[1].y + 1.0,
            rects[2].x + 5.0,
            rects[1].y + 5.0,
        );
        let mut selection = ClipSelection::default();
        selection.begin_marquee(false);
        selection.marquee(&clips.tile_rects(cell), drag);
        assert_eq!(picked(&selection), ["panel-02.mp4", "panel-03.mp4"]);
    }

    /// Ctrl+A runs along `selectable_paths`, so the expanded tile is not in
    /// what it selects -- and is again as soon as the expansion closes.
    #[test]
    fn ctrl_a_leaves_the_expanded_tile_out() {
        let (mut clips, _, _) = expanded_grid();
        let mut selection = ClipSelection::default();
        selection.select_all(&clips.selectable_paths());
        assert_eq!(
            picked(&selection),
            [
                "panel-01.mp4",
                "panel-03.mp4",
                "panel-04.mp4",
                "panel-05.mp4",
                "panel-06.mp4"
            ]
        );
        clips.expanded = None;
        selection.select_all(&clips.selectable_paths());
        assert_eq!(picked(&selection).len(), 6);
    }

    /// A Shift range spanning the expanded entry skips it: the range is taken
    /// along the same order Ctrl+A uses, and the expanded entry is not in it.
    #[test]
    fn a_shift_range_spanning_the_expanded_tile_skips_it() {
        let (mut clips, _, _) = expanded_grid();
        let order = clips.selectable_paths();
        let first = clips.visible_paths()[0].clone();
        let fourth = clips.visible_paths()[3].clone();
        let mut selection = ClipSelection::default();
        selection.click(&first, false, true, &order);
        selection.click(&fourth, true, false, &order);
        assert_eq!(
            picked(&selection),
            ["panel-01.mp4", "panel-03.mp4", "panel-04.mp4"]
        );

        // Control: the same two ends with nothing expanded take the entry in
        // between as well.
        clips.expanded = None;
        let order = clips.selectable_paths();
        let mut selection = ClipSelection::default();
        selection.click(&first, false, true, &order);
        selection.click(&fourth, true, false, &order);
        assert_eq!(
            picked(&selection),
            [
                "panel-01.mp4",
                "panel-02.mp4",
                "panel-03.mp4",
                "panel-04.mp4"
            ]
        );
    }

    /// t260916-5568 design 7: a clip that left the folder leaves the
    /// selection with it, so a later bulk delete cannot name it.
    #[test]
    fn a_rescan_drops_a_vanished_clip_from_the_selection() {
        let mut clips = headless_clips();
        clips.listed(vec![entry("a.mp4"), entry("b.mp4")]);
        let order = clips.visible_paths();
        clips.selection.select_all(&order);
        clips.confirm = order.clone();
        assert_eq!(clips.selection.len(), 2);
        clips.listed(vec![entry("b.mp4")]);
        assert_eq!(clips.selection.selected(), &order[1..]);
        assert_eq!(clips.confirm, order[1..].to_vec());
    }

    /// The user's 2026-09-19 ruling (follow-up of t260916-5568): one clip
    /// asks, exactly as two do -- the confirmation opens and no delete job is
    /// sent until it is accepted.
    #[test]
    fn deleting_one_clip_asks_first_like_deleting_two() {
        let (jobs, jobs_rx) = crossbeam_channel::unbounded();
        let (msgs, _msgs_rx) = crossbeam_channel::unbounded();
        let (cmds, _cmds_rx) = crossbeam_channel::unbounded();
        let mut clips = Clips::new(jobs, msgs, cmds);
        clips.listed(vec![entry("a.mp4"), entry("b.mp4")]);
        let order = clips.visible_paths();
        for targets in [order[..1].to_vec(), order.clone()] {
            clips.confirm.clear();
            ask_delete(&mut clips, targets.clone());
            assert_eq!(clips.confirm, targets);
            assert!(jobs_rx.try_recv().is_err(), "no delete before the dialog");
        }
        // Nothing selected asks nothing.
        clips.confirm.clear();
        ask_delete(&mut clips, Vec::new());
        assert!(clips.confirm.is_empty());
    }

    fn headless_clips() -> Clips {
        let (jobs, _jobs_rx) = crossbeam_channel::unbounded();
        let (msgs, _msgs_rx) = crossbeam_channel::unbounded();
        let (cmds, _cmds_rx) = crossbeam_channel::unbounded();
        Clips::new(jobs, msgs, cmds)
    }

    fn entry(name: &str) -> ClipEntry {
        ClipEntry {
            path: PathBuf::from(format!(r"C:\clips\{name}")),
            name: name.to_owned(),
            created: SystemTime::UNIX_EPOCH,
            modified: SystemTime::UNIX_EPOCH,
            bytes: 1,
        }
    }

    /// The same clip, re-listed after a write moved its modification time.
    fn entry_rewritten(name: &str) -> ClipEntry {
        ClipEntry {
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            ..entry(name)
        }
    }

    /// A detail read for `name`, filed under `modified`, carrying a comment and
    /// a 2x2 picture -- the picture is what the tile must not lose.
    fn detail_for(clips: &mut Clips, name: &str, modified: SystemTime, comment: &str) {
        clips.detailed(
            entry(name).path,
            modified,
            ClipDetails {
                comment: Some(comment.to_owned()),
                duration_100ns: Some(10),
            },
            Some((2, 2, vec![255u8; 16])),
        );
    }

    /// What `render` would put in the row, without a window: the comment it
    /// draws and whether `ClipTile`'s `if root.entry.has-thumbnail: Image`
    /// survives.
    fn drawn(clips: &Clips, entry: &ClipEntry) -> (String, bool) {
        let view = clips.tile_view(entry);
        (
            view.comment.unwrap_or_default().to_owned(),
            view.detail.is_some_and(|detail| detail.thumbnail.is_some()),
        )
    }

    /// t260916-568d. The whole sequence a comment save goes through, in the
    /// order the event loop sees it. Before this task the two asserts in the
    /// middle read `("", false)`: the tile drew the placeholder over bare
    /// `chrome-inset` (measured on the real app: 4 frames of it on the worker
    /// route, ~550ms of the old comment on the playing route).
    #[test]
    fn a_saved_comment_is_on_the_tile_before_the_file_answers() {
        let mut clips = headless_clips();
        clips.listed(vec![entry("a.mp4"), entry("b.mp4")]);
        detail_for(&mut clips, "a.mp4", SystemTime::UNIX_EPOCH, "old");
        detail_for(&mut clips, "b.mp4", SystemTime::UNIX_EPOCH, "untouched");
        // Control: the row is fully drawn before anything is written.
        assert_eq!(drawn(&clips, &entry("a.mp4")), ("old".into(), true));

        // The commit, as `commit_open_edit` makes it.
        clips.commit_optimistically(&entry("a.mp4").path, SystemTime::UNIX_EPOCH, "new");
        assert_eq!(
            drawn(&clips, &entry("a.mp4")),
            ("new".into(), true),
            "the same render already says `new`, and keeps the picture"
        );

        // The re-scan the write triggers: the file's modification time has
        // moved, so the cache entry no longer matches it.
        clips.listed(vec![entry_rewritten("a.mp4"), entry("b.mp4")]);
        assert_eq!(
            drawn(&clips, &entry_rewritten("a.mp4")),
            ("new".into(), true),
            "a cache miss is an update in flight, not an empty row"
        );
        assert_eq!(
            drawn(&clips, &entry("b.mp4")),
            ("untouched".into(), true),
            "the other clip is resolved by path and did not move"
        );

        // The detail read that saw the write: it is the truth from here on.
        detail_for(
            &mut clips,
            "a.mp4",
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            "new",
        );
        assert!(clips.pending_comments.is_empty(), "the guess is spent");
        assert_eq!(
            drawn(&clips, &entry_rewritten("a.mp4")),
            ("new".into(), true)
        );
    }

    /// t260916-568d. Two things that must not end the guess early, and one that
    /// must: the pre-write read task2990 files under the old modification time
    /// misses, a refused write rolls back, and the strict `cached` test the
    /// re-request depends on still misses.
    #[test]
    fn only_a_read_that_saw_the_write_ends_the_guess() {
        let mut clips = headless_clips();
        clips.listed(vec![entry("a.mp4")]);
        detail_for(&mut clips, "a.mp4", SystemTime::UNIX_EPOCH, "old");
        clips.commit_optimistically(&entry("a.mp4").path, SystemTime::UNIX_EPOCH, "new");

        // A detail read that was in flight when the write went out comes back
        // filed under the pre-write time. It never saw `new`.
        detail_for(&mut clips, "a.mp4", SystemTime::UNIX_EPOCH, "old");
        assert_eq!(drawn(&clips, &entry("a.mp4")).0, "new");

        // `queue_visible_details` must still see a miss once the file moves, or
        // the re-read that ends the guess is never asked for.
        clips.listed(vec![entry_rewritten("a.mp4")]);
        assert!(clips.cached(&entry_rewritten("a.mp4")).is_none());

        // The write is refused: the tile goes back to the file's own text.
        clips.comment_write_failed();
        assert_eq!(
            drawn(&clips, &entry_rewritten("a.mp4")),
            ("old".into(), true)
        );
    }

    /// t260916-568d. The editor is seeded from what the tile says, so a second
    /// Enter with nothing typed is not an edit that empties the comment.
    #[test]
    fn reopening_the_editor_mid_write_seeds_the_comment_just_typed() {
        let mut clips = headless_clips();
        clips.listed(vec![entry("a.mp4")]);
        detail_for(&mut clips, "a.mp4", SystemTime::UNIX_EPOCH, "old");
        clips.commit_optimistically(&entry("a.mp4").path, SystemTime::UNIX_EPOCH, "new");
        clips.listed(vec![entry_rewritten("a.mp4")]);

        let seed = clips
            .tile_view(&entry_rewritten("a.mp4"))
            .comment
            .unwrap_or_default();
        assert_eq!(seed, "new");
        assert!(!clips_ui::needs_write(seed, Some(seed)));
        // Control: the cache alone would have seeded the field empty, and that
        // same Enter would have written the comment away.
        assert!(clips.cached(&entry_rewritten("a.mp4")).is_none());
        assert!(clips_ui::needs_write("", Some("new")));
    }

    /// t260915-1fc3: the comment is written between dropping the engine and
    /// reopening it, so the transport has to outlive the engine -- and with it
    /// what the bar would otherwise have lost.
    #[test]
    fn dropping_only_the_engine_keeps_the_transport() {
        let mut clips = headless_clips();
        clips.player.open(0, entry("a.mp4").path, 100);
        clips.last_frame = Some(frame(4, 4));
        clips.last_seek_sent = Some(5);
        clips.play_intent = Some((true, Instant::now()));
        clips.drop_engine();
        assert!(clips.player.is_open(), "the transport survives");
        assert!(clips.last_frame.is_none());
        assert_eq!(clips.last_seek_sent, None, "no dead engine will ack it");
        assert!(clips.play_intent.is_none());
        // Control: `drop_player` is the one that closes the transport.
        clips.drop_player();
        assert!(!clips.player.is_open());
    }

    /// t260915-1fc3 AC3. Already true before that task (task3520's `listed`):
    /// a re-scan that reorders the folder keeps the expansion on the clip the
    /// transport holds, which is why `drop_engine` must leave the transport.
    #[test]
    fn a_rescan_keeps_the_expansion_on_the_clip_being_played() {
        let mut clips = headless_clips();
        clips.player.open(0, entry("b.mp4").path, 100);
        clips.listed(vec![entry("a.mp4"), entry("b.mp4")]);
        assert_eq!(clips.expanded, Some(1));
        clips.drop_engine();
        clips.listed(vec![entry("b.mp4"), entry("a.mp4")]);
        assert_eq!(clips.expanded, Some(0), "followed b.mp4 across the reorder");
        // Control: with no transport to read, the same reorder leaves the index
        // where it was -- now over the other clip.
        clips.player.close();
        clips.expanded = Some(1);
        clips.listed(vec![entry("a.mp4"), entry("b.mp4")]);
        assert_eq!(clips.expanded, Some(1));
    }
}
