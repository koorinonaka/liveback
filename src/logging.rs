//! The rolling file logger.
//!
//! The capture pipeline reports every failure through `tracing` -- the HRESULT
//! and stage behind a `CAP-DEV-001` stop live nowhere else -- and a
//! `windows_subsystem = "windows"` build has no console for them to reach, so
//! this is the only place those records survive.
//!
//! Until task131 this was installed from Tauri's `setup()` and asked
//! `app.path().app_log_dir()` for the directory. The path is spelled out here
//! now, deliberately unchanged, so logs keep landing beside the ones the
//! shipping build already wrote.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// What Tauri v2's `app_log_dir()` resolved to on Windows:
/// `%LOCALAPPDATA%\{identifier}\logs`.
const LOG_DIR: &str = "com.liveback.desktop\\logs";
/// Base name every log file starts with, day suffix and all.
const LOG_PREFIX: &str = "liveback.log";
/// How large one log file is allowed to get before the day's records move
/// on to a new part. Rotating by day alone could not bound this: on
/// 2026-08-14 a stuck playback engine wrote 521MB into a single day's file
/// in under three minutes, and a file that size is slow to grep and refuses
/// to open in most editors. The rate limit in `playback::fetch_gate` is what
/// stops the flood; this only keeps whatever does get written in pieces
/// something can still read.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

fn prune_logs(path: &std::path::Path) {
    let cutoff = SystemTime::now()
        .checked_sub(LOG_RETENTION)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.modified().is_ok_and(|modified| modified < cutoff) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Daily rolling with a size cap on top. The day dimension stays
/// `tracing_appender`'s -- it is the part that has to know about midnight,
/// and it already does. This wrapper only counts bytes and, past
/// `MAX_FILE_BYTES`, hands the records to a fresh appender under a
/// part-suffixed prefix: `liveback.log.2026-08-14`, then
/// `liveback.log.p1.2026-08-14`, and so on. Parts land in the same
/// directory under the same prefix, so `prune_logs` retires them with
/// everything else and nothing new deletes anything.
struct PartedDaily {
    dir: PathBuf,
    part: u32,
    written: u64,
    inner: tracing_appender::rolling::RollingFileAppender,
}

impl PartedDaily {
    fn new(dir: PathBuf) -> Self {
        // A restart appends to the day's existing file rather than
        // truncating it, so resume where the last run left off -- both which
        // part it had reached and how full that part already is. Starting
        // over at part 0 with an empty count would append another whole cap
        // to a file that had already been rotated away from.
        let (part, written) = resume_point(&dir);
        Self {
            written,
            inner: tracing_appender::rolling::daily(&dir, part_prefix(part)),
            dir,
            part,
        }
    }
}

/// Prefix for part `n`: part 0 keeps the plain `liveback.log` the
/// shipping build has always written, so nothing that greps for it by name
/// has to learn about parts to find the first one.
fn part_prefix(part: u32) -> String {
    if part == 0 {
        LOG_PREFIX.to_owned()
    } else {
        format!("{LOG_PREFIX}.p{part}")
    }
}

impl Write for PartedDaily {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Rotate before the write, not after: a record is never split
        // across two files, whatever its length.
        if self.written >= MAX_FILE_BYTES {
            self.part += 1;
            self.inner = tracing_appender::rolling::daily(&self.dir, part_prefix(self.part));
            self.written = 0;
        }
        let written = self.inner.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The part and byte count to carry over from whatever the last run left
/// behind: the most recently touched log file is the one this run is about
/// to append to, and its name says which part it is. An empty (or
/// unreadable) directory starts at part 0 with nothing written.
///
/// Reading mtime rather than parsing the day suffix is deliberate -- the
/// day belongs to `tracing_appender`, and the newest file is the current
/// day's by construction. A stale newest file (the app has not run since
/// yesterday) only means this run opens today's part 0 with a non-zero
/// count, which rotates one part early and costs nothing.
fn resume_point(dir: &Path) -> (u32, u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            let suffix = name.strip_prefix(LOG_PREFIX)?;
            let metadata = entry.metadata().ok().filter(fs::Metadata::is_file)?;
            Some((suffix.to_owned(), metadata))
        })
        .max_by_key(|(_, metadata)| metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH))
        .map_or((0, 0), |(suffix, metadata)| {
            (part_of(&suffix), metadata.len())
        })
}

/// The part number in what follows `LOG_PREFIX` in a log file's name:
/// `.p3.2026-08-14` is part 3, `.2026-08-14` is part 0. Anything that does
/// not parse is part 0 too -- a name this does not recognise is not one
/// this module wrote.
fn part_of(suffix: &str) -> u32 {
    suffix
        .strip_prefix(".p")
        .and_then(|rest| rest.split('.').next())
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

pub fn log_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join(LOG_DIR))
}

/// Installs the subscriber and keeps the writer's worker alive for the rest of
/// the process. Leaked rather than handed back: the guard has to outlive every
/// `tracing` call, and the caller has nowhere sensible to park it.
///
/// Assembled from layers since task205, where `--insight` gained a second
/// destination for a second kind of record. The file layer below must keep
/// writing exactly what `tracing_subscriber::fmt()` wrote before that: the
/// builder's own default max level is `INFO` (`fmt::Subscriber::
/// DEFAULT_MAX_LEVEL`), which a bare `registry()` does not have, so it is
/// spelled out rather than inherited.
pub fn configure_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

    let Some(log_dir) = log_dir() else {
        return;
    };
    let _ = fs::create_dir_all(&log_dir);
    prune_logs(&log_dir);

    // Before `log_dir` is handed to the appender below, and after the prune --
    // so the run's own file is not a candidate for its own retention sweep.
    #[cfg(feature = "insight")]
    let insight_layer = crate::insight::requested()
        .then(|| crate::insight::install(&log_dir))
        .flatten()
        .map(|layer| layer.with_filter(crate::insight::only_insight_spans()));

    let (writer, guard) = tracing_appender::non_blocking(PartedDaily::new(log_dir));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);

    let subscriber = tracing_subscriber::registry().with(file_layer);

    // The insight layer only exists when the feature is compiled in *and* the
    // run asked for it, and it takes only its own spans -- so the file above
    // never sees them, and this never sees the file's records.
    #[cfg(feature = "insight")]
    let subscriber = subscriber.with(insight_layer);

    let _ = subscriber.try_init();
    std::mem::forget(guard);
    tracing::info!(event = "application_started", "Liveback started");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Part 0 keeps the name the shipping build has always written, and
    /// every later part round-trips through the name it is given.
    #[test]
    fn part_names_round_trip_through_the_day_suffix() {
        assert_eq!(part_prefix(0), LOG_PREFIX);
        for part in [1, 2, 17, 300] {
            let name = format!("{}.2026-08-14", part_prefix(part));
            let suffix = name.strip_prefix(LOG_PREFIX).expect("prefixed");
            assert_eq!(part_of(suffix), part, "{name} did not round-trip");
        }
        assert_eq!(part_of(".2026-08-14"), 0, "the unsuffixed day file is p0");
    }

    /// Names this module did not write must not be read as a part number
    /// and silently resume some arbitrary part.
    #[test]
    fn an_unrecognised_suffix_is_part_zero() {
        assert_eq!(part_of(".pxx.2026-08-14"), 0);
        assert_eq!(part_of(".old"), 0);
        assert_eq!(part_of(""), 0);
    }

    /// A restart picks up the part the last run had rotated to, and how
    /// full it already was -- otherwise it would append another whole
    /// `MAX_FILE_BYTES` to a file that had already been rotated away from.
    #[test]
    fn a_restart_resumes_the_newest_part_and_its_length() {
        let dir = std::env::temp_dir().join("livia-logging-resume");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp log dir");
        assert_eq!(
            resume_point(&dir),
            (0, 0),
            "an empty directory starts fresh"
        );

        fs::write(dir.join("liveback.log.2026-08-14"), vec![b'x'; 64]).unwrap();
        assert_eq!(resume_point(&dir), (0, 64));

        // Written second, so it is the newest by mtime whatever the
        // filesystem's timestamp resolution turns out to be.
        std::thread::sleep(Duration::from_millis(20));
        fs::write(dir.join("liveback.log.p2.2026-08-14"), vec![b'x'; 9]).unwrap();
        assert_eq!(resume_point(&dir), (2, 9));

        let _ = fs::remove_dir_all(&dir);
    }
}
