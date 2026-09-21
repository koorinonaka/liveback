//! Scope timing for `--insight` runs (task205).
//!
//! The question this answers is "which of our own scopes is eating the time",
//! for a build the user starts from `run.bat`. Three things go into one file:
//!
//! - an **aggregate** line per scope every [`FLUSH_TICKS`] ticks (count, total,
//!   max, mean),
//! - a **slow** line the moment one call exceeds [`SLOW_THRESHOLD`], rate
//!   limited to [`SLOW_INTERVAL`] per scope,
//! - a **heartbeat** line every [`TICK`] with this process's CPU% and memory.
//!
//! Why aggregate rather than one line per span close: at 60fps across the dozen
//! or so instrumented scopes that is 200-400MB an hour, which is how
//! `logging.rs` came to carry a note about a 521MB file. Aggregates are ~2
//! lines a second whatever the frame rate, and the slow lines are what actually
//! catch the "a disk write landed on the capture thread" class of bug -- the
//! one an average would smear away.
//!
//! Nothing here is compiled unless the `insight` cargo feature is on, and even
//! then nothing runs unless the process was started with `--insight`. The
//! installer's `cargo build --release` therefore ships the binary it always
//! shipped.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::HashMap,
    fs::File,
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tracing::{span, Metadata};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

/// Spans this module measures carry this target. Everything else in the
/// process -- every `tracing::info!` the app already writes -- is invisible to
/// this layer, and these spans are in turn invisible to the file logger.
pub const TARGET: &str = "insight";

/// One call slower than this gets a line of its own. A 60fps frame budget is
/// 16.6ms, so a single scope crossing half of it is worth naming.
const SLOW_THRESHOLD: Duration = Duration::from_millis(8);
/// At most one slow line per scope per this interval. Without it a scope that
/// is *always* slow writes one line per frame, which is the flood the
/// aggregate design exists to avoid.
const SLOW_INTERVAL: Duration = Duration::from_secs(1);
/// Heartbeat cadence, and the granularity of the flush counter.
const TICK: Duration = Duration::from_secs(1);
/// Aggregates flush every this many ticks.
const FLUSH_TICKS: u32 = 5;
/// Threads named individually on the heartbeat's companion line. The process
/// runs dozens, nearly all idle, so the rest is one folded number.
const TOP_THREADS: usize = 6;
/// Scopes that measure a deliberate wait rather than work. They are still
/// counted -- the aggregate is how the pacing gets checked -- but they never
/// take a slow line: `playback_sleep` is 30-60ms every frame by design, and a
/// slow section that lists it every second stops being a list of stalls
/// (task210).
const NEVER_SLOW: [&str; 1] = ["playback_sleep"];

/// True when this process was started with `--insight`. The request spelled it
/// `-insight`; both are accepted, and `run.bat` passes `--insight` because the
/// flag next to it in `main` is `--livia-export-helper`.
pub fn requested() -> bool {
    std::env::args_os().any(|arg| arg == "--insight" || arg == "-insight")
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct ScopeStats {
    count: u64,
    total: Duration,
    max: Duration,
}

/// Everything the instrumented threads touch. Split out from the writer and
/// the globals so its arithmetic can be tested without a subscriber, a file, or
/// a sleeping thread.
///
/// ponytail: one lock over the whole map. At the ~1-2k span closes a second
/// this instrumentation produces, contention is not the thing being measured;
/// shard it only if the lock itself shows up in a run.
#[derive(Default)]
struct Recorder {
    scopes: Mutex<HashMap<&'static str, ScopeStats>>,
    /// Last slow line per scope, for the rate limit.
    slow_gate: Mutex<HashMap<&'static str, Instant>>,
}

impl Recorder {
    /// Folds one closed scope in. Returns true when the caller should write a
    /// slow line for it -- the decision is here, the writing is not, so the
    /// gate stays testable and the lock stays short.
    fn record(&self, name: &'static str, elapsed: Duration, now: Instant) -> bool {
        if let Ok(mut scopes) = self.scopes.lock() {
            let entry = scopes.entry(name).or_default();
            entry.count += 1;
            entry.total += elapsed;
            entry.max = entry.max.max(elapsed);
        }
        elapsed >= SLOW_THRESHOLD && !NEVER_SLOW.contains(&name) && self.slow_line_is_due(name, now)
    }

    fn slow_line_is_due(&self, name: &'static str, now: Instant) -> bool {
        let Ok(mut gate) = self.slow_gate.lock() else {
            return false;
        };
        match gate.get(name) {
            Some(last) if now.duration_since(*last) < SLOW_INTERVAL => false,
            _ => {
                gate.insert(name, now);
                true
            }
        }
    }

    /// Takes the interval's stats, heaviest first. The ordering *is* the
    /// answer to "what is heavy", so nobody should have to sort a log by hand.
    fn drain(&self) -> Vec<(&'static str, ScopeStats)> {
        let Ok(mut scopes) = self.scopes.lock() else {
            return Vec::new();
        };
        let mut drained: Vec<_> = scopes.drain().collect();
        drop(scopes);
        drained.sort_by(|a, b| b.1.total.cmp(&a.1.total).then(a.0.cmp(b.0)));
        drained
    }
}

static RECORDER: OnceLock<Recorder> = OnceLock::new();
static WRITER: OnceLock<tracing_appender::non_blocking::NonBlocking> = OnceLock::new();
/// Start of the run, so every line carries a cheap monotonic `t=` instead of a
/// formatted date.
static STARTED: OnceLock<Instant> = OnceLock::new();

fn elapsed_secs() -> f64 {
    STARTED
        .get()
        .map_or(0.0, |started| started.elapsed().as_secs_f64())
}

/// Writes one line. `NonBlocking` is a channel send, not a file write, and it
/// is lossy by construction -- a full queue drops the line rather than parking
/// the capture thread, which is the entire reason these threads can be
/// measured from at all.
fn emit(line: std::fmt::Arguments<'_>) {
    if let Some(writer) = WRITER.get() {
        let _ = writeln!(writer.clone(), "{line}");
    }
}

/// Live bytes the Rust side has asked for. Not the process's committed memory
/// -- allocator arenas, D3D and Media Foundation pools, and Skia's textures
/// are all outside it, which is exactly what makes the two numbers useful
/// together: whatever a step in `private_mb` does *not* show up here as, is
/// something no Rust owner can be blamed for (task212).
static HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Decoded images the UI is holding, by owner. Set, not incremented -- the
/// caller passes its map's length, which cannot drift out of sync with it.
static UI_IMAGES: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

/// Named for [`UI_IMAGES`]. `as usize` on this is the index.
#[derive(Clone, Copy)]
pub enum Images {
    History = 0,
    Review = 1,
}

/// Records how many decoded images one owner holds. Behind
/// `insight_images!`, so a build without the feature never calls it.
pub fn set_images(owner: Images, count: usize) {
    UI_IMAGES[owner as usize].store(count, Ordering::Relaxed);
}

/// `System`, plus a running total. Installed as the global allocator only in
/// an `insight` build (see the `#[global_allocator]` below), so a shipped
/// binary allocates exactly as it did before.
///
/// Nothing in here may allocate -- an allocator that allocates recurses -- so
/// it is two atomics and nothing else. Formatting happens on the report
/// thread.
pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            HEAP_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            HEAP_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        HEAP_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            // Only on success: a failed realloc leaves the old block alive.
            HEAP_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            HEAP_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The timestamp stashed on a span when it is created.
struct Started(Instant);

pub struct InsightLayer;

impl<S> Layer<S> for InsightLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    /// Timing starts at creation rather than at `on_enter`: both shapes this is
    /// used through (`insight_scope!`, which creates and enters in one
    /// statement, and `#[instrument]`) create the span exactly where the
    /// measurement should start, and a creation cannot be missed or repeated
    /// the way an enter can.
    fn on_new_span(&self, _attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(Started(Instant::now()));
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(elapsed) = span
            .extensions()
            .get::<Started>()
            .map(|started| started.0.elapsed())
        else {
            return;
        };
        let Some(recorder) = RECORDER.get() else {
            return;
        };
        let name = span.metadata().name();
        if recorder.record(name, elapsed, Instant::now()) {
            emit(format_args!(
                "t={:.3} slow scope={name} ms={:.2}",
                elapsed_secs(),
                elapsed.as_secs_f64() * 1000.0
            ));
        }
    }
}

/// The predicate behind the layer's filter: our spans, and nothing else. Both
/// directions matter -- the file logger must not gain these spans, and this
/// layer must not gain the file logger's records -- which is what lets
/// `liveback.log` come out of the refactor unchanged.
fn is_insight_span(metadata: &Metadata<'_>) -> bool {
    metadata.is_span() && metadata.target() == TARGET
}

type SpanFilter = tracing_subscriber::filter::FilterFn<fn(&Metadata<'_>) -> bool>;

pub fn only_insight_spans() -> SpanFilter {
    tracing_subscriber::filter::filter_fn(is_insight_span as fn(&Metadata<'_>) -> bool)
}

/// Creates the run's file and starts the reporting thread. `None` when the file
/// cannot be opened, in which case the caller installs no layer at all and the
/// app runs exactly as it would without the flag.
///
/// One file per run (`insight.<epoch seconds>.log`) rather than a share of the
/// rolling `liveback.log`: a run is the unit anyone analysing this compares,
/// and `logging`'s pruning retires whatever it finds in the directory by age,
/// so the 7-day retention already covers these without knowing about them.
pub fn install(log_dir: &Path) -> Option<InsightLayer> {
    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let file = File::create(log_dir.join(format!("insight.{epoch_secs}.log"))).ok()?;
    // Matches `logging::configure_logging`: the guard has to outlive every
    // write and there is nowhere to park it. The cost is the last queued lines
    // at process exit, which for a sampling log is not worth a shutdown path.
    let (writer, guard) = tracing_appender::non_blocking(file);
    std::mem::forget(guard);
    if WRITER.set(writer).is_err() {
        return None;
    }
    let _ = STARTED.set(Instant::now());
    RECORDER.get_or_init(Recorder::default);

    let cpus = thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    emit(format_args!(
        "t=0.000 start epoch_secs={epoch_secs} profile={} cpus={cpus} slow_ms={} flush_s={}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        SLOW_THRESHOLD.as_millis(),
        TICK.as_secs() * u64::from(FLUSH_TICKS),
    ));

    thread::Builder::new()
        .name("insight-report".into())
        .spawn(move || report_loop(cpus))
        .ok()?;

    Some(InsightLayer)
}

/// The heaviest `top` threads, with everything else folded into one number.
/// Split out from the Win32 walk so the arithmetic is testable without a
/// snapshot handle -- and folded at all because printing one line per thread
/// every second is the flood the aggregate design exists to avoid.
fn heaviest(mut threads: Vec<(String, f64)>, top: usize) -> (Vec<(String, f64)>, f64) {
    threads.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let rest = threads.split_off(threads.len().min(top));
    (threads, rest.iter().map(|(_, pct)| pct).sum())
}

fn report_loop(cpus: usize) {
    let mut cpu = process::CpuMeter::new();
    let mut threads = process::ThreadMeter::new();
    let mut tick: u32 = 0;
    loop {
        thread::sleep(TICK);
        tick = tick.wrapping_add(1);

        if let Some(sample) = cpu.sample(cpus) {
            emit(format_args!(
                "t={:.3} heartbeat cpu_pct={:.1} working_set_mb={:.1} private_mb={:.1}",
                elapsed_secs(),
                sample.cpu_pct,
                sample.working_set_mb,
                sample.private_mb
            ));
        }

        // The other half of the heartbeat's decomposition: how much of the
        // memory is the Rust side's, and how many decoded images the UI is
        // sitting on (task212). Read every tick because the question is where
        // a step lands, and the steps are seconds wide.
        const MB: f64 = 1024.0 * 1024.0;
        emit(format_args!(
            "t={:.3} buffers rust_heap_mb={:.1} ui_images=history:{},review:{}",
            elapsed_secs(),
            HEAP_BYTES.load(Ordering::Relaxed) as f64 / MB,
            UI_IMAGES[Images::History as usize].load(Ordering::Relaxed),
            UI_IMAGES[Images::Review as usize].load(Ordering::Relaxed),
        ));

        // The heartbeat's decomposition: `cpu_pct` counts every thread, and
        // the scopes above cover only a quarter of it (task211), so this is
        // what names the rest -- Slint's renderer, the WGC and Media
        // Foundation pools, none of which this file can put a scope inside of.
        if let Some(per_thread) = threads.sample(cpus) {
            let total: f64 = per_thread.iter().map(|(_, pct)| pct).sum();
            let (top, other) = heaviest(per_thread, TOP_THREADS);
            let top = top
                .iter()
                .map(|(name, pct)| format!("{name}:{pct:.1}"))
                .collect::<Vec<_>>()
                .join(",");
            emit(format_args!(
                "t={:.3} threads total_pct={total:.1} top={top} other={other:.1}",
                elapsed_secs(),
            ));
        }

        if tick.is_multiple_of(FLUSH_TICKS) {
            if let Some(recorder) = RECORDER.get() {
                let now = elapsed_secs();
                for (name, stats) in recorder.drain() {
                    let mean_us = stats.total.as_secs_f64() * 1e6 / stats.count.max(1) as f64;
                    emit(format_args!(
                        "t={now:.3} scope={name} n={} total_ms={:.2} max_ms={:.2} mean_us={mean_us:.1}",
                        stats.count,
                        stats.total.as_secs_f64() * 1000.0,
                        stats.max.as_secs_f64() * 1000.0,
                    ));
                }
            }
        }
    }
}

/// This process's own CPU and memory. The app's goal is to cost the capture
/// target as little as possible; nothing here measures that target, so this is
/// the proxy -- if our CPU share climbs, theirs is what paid for it.
mod process {
    use std::{collections::HashMap, time::Instant};
    use windows::Win32::{
        Foundation::{CloseHandle, LocalFree, FILETIME, HLOCAL},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{
                GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, GetThreadDescription,
                GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
            },
        },
    };

    pub struct Sample {
        pub cpu_pct: f64,
        pub working_set_mb: f64,
        pub private_mb: f64,
    }

    pub struct CpuMeter {
        last: Option<(Instant, u64)>,
    }

    fn ticks(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    impl CpuMeter {
        pub fn new() -> Self {
            Self { last: None }
        }

        /// `None` on the first call -- a rate needs two samples -- and whenever
        /// Windows declines to answer.
        pub fn sample(&mut self, cpus: usize) -> Option<Sample> {
            let process = unsafe { GetCurrentProcess() };
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            unsafe {
                GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user).ok()?;
            }
            let busy = ticks(kernel) + ticks(user);
            let now = Instant::now();
            let previous = self.last.replace((now, busy));

            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?,
                ..Default::default()
            };
            unsafe {
                GetProcessMemoryInfo(process, &mut counters, counters.cb).ok()?;
            }

            let (then, busy_then) = previous?;
            // Both sides in 100ns units: process time as Windows reports it,
            // wall time from the monotonic clock.
            let wall = now.duration_since(then).as_nanos() / 100;
            if wall == 0 {
                return None;
            }
            let busy_delta = busy.saturating_sub(busy_then) as f64;
            const MB: f64 = 1024.0 * 1024.0;
            Some(Sample {
                cpu_pct: busy_delta / wall as f64 / cpus as f64 * 100.0,
                working_set_mb: counters.WorkingSetSize as f64 / MB,
                private_mb: counters.PagefileUsage as f64 / MB,
            })
        }
    }

    /// The same rate, per thread. Every 100ns `CpuMeter` reports belongs to
    /// some thread, so this decomposes the heartbeat completely -- including
    /// the threads nothing in this process can wrap a scope around.
    pub struct ThreadMeter {
        busy: HashMap<u32, u64>,
        at: Option<Instant>,
    }

    impl ThreadMeter {
        pub fn new() -> Self {
            Self {
                busy: HashMap::new(),
                at: None,
            }
        }

        /// Each thread's share since the last call, unsorted. `None` on the
        /// first call, which only primes the previous values.
        ///
        /// A thread first seen this call is skipped rather than counted from
        /// zero: thread ids are reused, and a reused id would otherwise report
        /// its predecessor's whole lifetime as one tick's work.
        pub fn sample(&mut self, cpus: usize) -> Option<Vec<(String, f64)>> {
            let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.ok()?;
            let pid = unsafe { GetCurrentProcessId() };
            let mut entry = THREADENTRY32 {
                dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).ok()?,
                ..Default::default()
            };
            let mut busy_now = HashMap::new();
            let mut deltas = Vec::new();
            let mut walking = unsafe { Thread32First(snapshot, &mut entry) };
            while walking.is_ok() {
                if entry.th32OwnerProcessID == pid {
                    if let Some((busy, name)) = thread_busy(entry.th32ThreadID) {
                        if let Some(before) = self.busy.get(&entry.th32ThreadID) {
                            deltas.push((name, busy.saturating_sub(*before)));
                        }
                        busy_now.insert(entry.th32ThreadID, busy);
                    }
                }
                walking = unsafe { Thread32Next(snapshot, &mut entry) };
            }
            unsafe {
                let _ = CloseHandle(snapshot);
            }

            let now = Instant::now();
            let previous = self.at.replace(now);
            self.busy = busy_now;
            let wall = now.duration_since(previous?).as_nanos() / 100;
            if wall == 0 {
                return None;
            }
            Some(
                deltas
                    .into_iter()
                    .map(|(name, delta)| (name, delta as f64 / wall as f64 / cpus as f64 * 100.0))
                    .collect(),
            )
        }
    }

    /// One thread's busy ticks and its name. The name is whatever
    /// `SetThreadDescription` was given -- which `std::thread::Builder::name`
    /// sets, so our own threads arrive labelled. The Media Foundation, WGC and
    /// driver pools are nameless and fall back to their id; their total is the
    /// answer even when their names are not.
    fn thread_busy(id: u32) -> Option<(u64, String)> {
        let thread = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, id) }.ok()?;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let times =
            unsafe { GetThreadTimes(thread, &mut creation, &mut exit, &mut kernel, &mut user) };
        let name = unsafe { GetThreadDescription(thread) }
            .ok()
            .and_then(|description| {
                let name = unsafe { description.to_string() }.ok();
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(description.0.cast())));
                }
                name.filter(|name| !name.is_empty())
            });
        unsafe {
            let _ = CloseHandle(thread);
        }
        times.ok()?;
        Some((
            ticks(kernel) + ticks(user),
            // The line is comma separated and space delimited, and a thread
            // name is whatever its author typed.
            name.map_or_else(|| format!("tid{id}"), |name| name.replace([',', ' '], "_")),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Folding is the whole reason this is not one line per call: count, total
    /// and max all have to survive being collapsed, and the drain has to hand
    /// them back heaviest first.
    #[test]
    fn closes_fold_into_count_total_and_max_heaviest_first() {
        let recorder = Recorder::default();
        let now = Instant::now();
        for millis in [2u64, 9, 4] {
            recorder.record("convert", Duration::from_millis(millis), now);
        }
        recorder.record("decode", Duration::from_millis(1), now);

        let drained = recorder.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].0, "convert", "the heaviest scope leads");
        assert_eq!(
            drained[0].1,
            ScopeStats {
                count: 3,
                total: Duration::from_millis(15),
                max: Duration::from_millis(9),
            },
            "the peak is the point, not just the mean"
        );
        assert!(
            recorder.drain().is_empty(),
            "a drained interval starts the next one empty"
        );
    }

    /// A scope that is slow on every frame must not write a line per frame --
    /// that is the flood the aggregate design exists to avoid.
    #[test]
    fn the_slow_gate_passes_once_per_interval_per_scope() {
        let recorder = Recorder::default();
        let t0 = Instant::now();
        let slow = SLOW_THRESHOLD + Duration::from_millis(1);

        assert!(
            recorder.record("convert", slow, t0),
            "the first slow call reports"
        );
        assert!(
            !recorder.record("convert", slow, t0 + Duration::from_millis(200)),
            "a second one inside the interval is suppressed"
        );
        assert!(
            recorder.record("decode", slow, t0 + Duration::from_millis(200)),
            "the limit is per scope, not global"
        );
        assert!(
            recorder.record(
                "convert",
                slow,
                t0 + SLOW_INTERVAL + Duration::from_millis(1)
            ),
            "past the interval it reports again"
        );
        assert_eq!(
            recorder.drain().len(),
            2,
            "suppressing a slow line still counts the call"
        );
    }

    /// A deliberate wait is counted but never reported as slow. Without this
    /// the sleep that paces playback -- tens of milliseconds, every frame, on
    /// purpose -- would fill the slow section it exists to keep readable.
    #[test]
    fn a_deliberate_wait_is_counted_but_never_slow() {
        let recorder = Recorder::default();
        let now = Instant::now();
        let long = SLOW_THRESHOLD * 4;

        assert!(
            !recorder.record(NEVER_SLOW[0], long, now),
            "a waiting scope never takes a slow line"
        );
        let drained = recorder.drain();
        assert_eq!(
            drained.first().map(|(name, stats)| (*name, stats.count)),
            Some((NEVER_SLOW[0], 1)),
            "but it is still counted, so the pacing can be checked"
        );
    }

    /// The gate must not fire for ordinary calls, or every frame would be an
    /// outlier and the file would say nothing.
    #[test]
    fn a_fast_call_is_never_a_slow_line() {
        let recorder = Recorder::default();
        let now = Instant::now();
        assert!(!recorder.record("convert", SLOW_THRESHOLD - Duration::from_micros(1), now));
        assert!(
            recorder.record("convert", SLOW_THRESHOLD, now),
            "the threshold itself reports"
        );
    }

    /// End to end through the real subscriber: `insight_scope!` produces a
    /// span this layer times, and an ordinary span of the same level does not
    /// reach it. The filter is the only thing keeping the two log files from
    /// bleeding into each other, so it is worth exercising rather than
    /// asserting about.
    #[test]
    fn the_filter_times_our_spans_and_ignores_everything_else() {
        use tracing_subscriber::{layer::SubscriberExt, Layer as _};

        let recorder = RECORDER.get_or_init(Recorder::default);
        let _ = recorder.drain();

        let subscriber =
            tracing_subscriber::registry().with(InsightLayer.with_filter(only_insight_spans()));
        tracing::subscriber::with_default(subscriber, || {
            crate::insight_scope!("measured");
            {
                let _other = tracing::debug_span!("unmeasured").entered();
            }
        });

        let drained = recorder.drain();
        assert_eq!(
            drained.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            vec!["measured"],
            "only the insight-target span is timed"
        );
        assert_eq!(drained[0].1.count, 1);
    }

    /// The per-thread line has to add up: a fold that loses time would point
    /// at the wrong thread, and the whole point of the line is that its total
    /// can be checked against the heartbeat's `cpu_pct` (task211).
    #[test]
    fn the_heaviest_threads_lead_and_the_rest_are_kept_in_the_fold() {
        let threads = vec![
            ("tid900".to_string(), 0.5),
            ("capture".to_string(), 3.0),
            ("thumbnail-writer".to_string(), 1.0),
            ("tid901".to_string(), 0.25),
        ];
        let total: f64 = threads.iter().map(|(_, pct)| pct).sum();

        let (top, other) = heaviest(threads, 2);
        assert_eq!(
            top.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["capture", "thumbnail-writer"],
            "the heaviest threads are the ones named"
        );
        assert!(
            (top.iter().map(|(_, pct)| pct).sum::<f64>() + other - total).abs() < 1e-9,
            "top + other must still be the process's whole share"
        );
    }

    /// The Win32 walk itself: enumerate, read, name, free, twice. Nothing in
    /// the pure test above touches `OpenThread`/`GetThreadDescription`, and a
    /// mistake in the handle or the `LocalFree` there would surface only as a
    /// crash inside a real `--insight` run.
    ///
    /// Only reachable under `--features insight`, so `cargo test` does not run
    /// it; `cargo test --features insight insight::` does.
    #[test]
    fn the_thread_walk_accounts_for_this_test_process() {
        let mut meter = process::ThreadMeter::new();
        assert!(
            meter.sample(1).is_none(),
            "the first call only primes the previous values"
        );
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(50) {
            std::hint::black_box((0..1000u64).fold(1u64, u64::wrapping_add));
        }
        let threads = meter.sample(1).expect("a second sample has a rate");
        assert!(!threads.is_empty(), "this process has threads");
        assert!(
            threads.iter().map(|(_, pct)| pct).sum::<f64>() > 0.0,
            "a busy-looping test process spends measurable CPU"
        );
    }

    /// A one-legged counter would make the whole memory line a lie: a missed
    /// `dealloc` reads as a leak that is not there, a missed `alloc` hides one
    /// that is. Allocate something too big to be confused with the noise of
    /// other test threads, then drop it (task212).
    ///
    /// `--features insight` only, like the thread walk above.
    #[test]
    fn allocating_and_dropping_leaves_the_heap_counter_where_it_started() {
        const BIG: usize = 64 * 1024 * 1024;
        let before = HEAP_BYTES.load(Ordering::Relaxed);

        let block = vec![0u8; BIG];
        let held = HEAP_BYTES.load(Ordering::Relaxed);
        assert!(
            held >= before + BIG,
            "a live allocation is counted: {before} -> {held}"
        );

        drop(block);
        let after = HEAP_BYTES.load(Ordering::Relaxed);
        assert!(
            after < before + BIG,
            "dropping gives it back: {held} -> {after}"
        );
    }

    /// `--insight` decides whether any of this runs, so the parse has to be
    /// exact: an argument that merely contains the word is not the flag.
    #[test]
    fn the_flag_is_matched_whole() {
        let matches = |arg: &str| {
            let arg = std::ffi::OsString::from(arg);
            arg == "--insight" || arg == "-insight"
        };
        assert!(matches("--insight"));
        assert!(matches("-insight"), "the spelling the request used");
        assert!(!matches("--insightful"));
        assert!(!matches("insight"));
        assert!(!matches("--no-insight"));
    }
}
