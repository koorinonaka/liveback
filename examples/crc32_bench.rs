//! Measures what `ring_buffer::container::crc32` costs, and what share of a
//! whole-container `scan` that is.
//!
//! Two numbers, one process:
//!
//! 1. **micro** -- the library's `crc32` against the byte-at-a-time table it
//!    replaced, which this file keeps a verbatim copy of, run **interleaved**
//!    (old, new, old, new, ...) rather than as two blocks, so the machine's own
//!    drift lands on both sides. Both are checked against the CRC-32/ISO-HDLC
//!    check value and against each other on the real buffer before anything is
//!    timed -- a faster function that computes a different number is not a
//!    result.
//! 2. **scan** -- `scan()` over a synthetic container with **no checkpoint**, so
//!    every record is replayed and every payload CRC'd. That is the path the
//!    startup replay and `recover` take.
//!
//! The scan side cannot compare two implementations in one process: `scan` calls
//! whatever `crc32` the library was built with. So build this example twice from
//! the same commit (once with each `crc32` body), copy both exes out of
//! `target\`, and alternate runs. **The micro ratio labels the build** -- it is
//! ~1.0 when the library is still the table, and well above 1 when it is
//! `crc32fast` -- so the two exes cannot be mixed up after the fact.
//!
//! Gated behind `required-features = ["insight"]` like the other probes here, so
//! the quality gate (`cargo clippy --all-targets`, run with no features) never
//! compiles it. `.cargo/config.toml` pins an explicit target triple, so the exe
//! lands under that triple rather than in `target\release\`:
//!
//! ```powershell
//! cargo build --release --features insight --example crc32_bench
//! .\target\x86_64-pc-windows-msvc\release\examples\crc32_bench.exe
//! ```
//!
//! `BENCH_MB` sets the micro buffer (default 64), `BENCH_SEGMENTS` /
//! `BENCH_SEG_KB` the synthetic container (default 400 x 256 KB = 100 MB) and
//! `BENCH_ROUNDS` how many times each side runs (default 7; the median is
//! reported). The container goes to a throwaway directory under `%TEMP%` that
//! this process removes before it exits -- it never touches the user's buffer.

use std::hint::black_box;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use livia::ring_buffer::container::{scan, ContainerWriter};

/// The implementation `crc32` used before `crc32fast`, kept here verbatim so the
/// two can be run against each other in one process. Not exported: this is a
/// measuring stick, not a second implementation for the app to choose between.
fn table_crc32(bytes: &[u8]) -> u32 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 {
                    0xEDB8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            *entry = value;
        }
        table
    });
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc = table[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Bytes that do not compress to a single value and are not all distinct -- a
/// CRC does not care, but a buffer of zeroes invites a memset-shaped surprise.
fn filler(len: usize) -> Vec<u8> {
    let mut state = 0x243F_6A88_85A3_08D3u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn throughput_gbps(bytes: usize, elapsed: Duration) -> f64 {
    bytes as f64 / elapsed.as_secs_f64() / 1e9
}

fn main() -> io::Result<()> {
    let micro_mb = env_usize("BENCH_MB", 64);
    let segments = env_usize("BENCH_SEGMENTS", 400);
    let segment_kb = env_usize("BENCH_SEG_KB", 256);
    let rounds = env_usize("BENCH_ROUNDS", 7);

    // ---------- correctness, before any timing ----------

    // The standard check value for CRC-32/ISO-HDLC, the same one
    // `container::tests::crc32_matches_the_known_check_value` asserts.
    assert_eq!(
        livia::ring_buffer::container::crc32(b"123456789"),
        0xCBF4_3926
    );
    assert_eq!(table_crc32(b"123456789"), 0xCBF4_3926);

    let buffer = filler(micro_mb * 1024 * 1024);
    let lib_value = livia::ring_buffer::container::crc32(&buffer);
    let table_value = table_crc32(&buffer);
    assert_eq!(
        lib_value, table_value,
        "the library's crc32 and the table disagree on {micro_mb} MB -- \
         a speed number for a different checksum is worthless"
    );
    println!("check: both agree, {micro_mb} MB -> {lib_value:#010x}");

    // ---------- micro: interleaved, never two blocks ----------

    let mut table_samples = Vec::with_capacity(rounds);
    let mut lib_samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        black_box(table_crc32(black_box(&buffer)));
        table_samples.push(start.elapsed());

        let start = Instant::now();
        black_box(livia::ring_buffer::container::crc32(black_box(&buffer)));
        lib_samples.push(start.elapsed());
    }
    let table_median = median(table_samples);
    let lib_median = median(lib_samples);
    let bytes = buffer.len();
    println!(
        "micro  table {:>8.2?} ({:.2} GB/s)   lib {:>8.2?} ({:.2} GB/s)   ratio {:.2}x   n={rounds}",
        table_median,
        throughput_gbps(bytes, table_median),
        lib_median,
        throughput_gbps(bytes, lib_median),
        table_median.as_secs_f64() / lib_median.as_secs_f64(),
    );

    // ---------- scan: whole-container replay, no checkpoint ----------

    let dir = std::env::temp_dir().join(format!("lvb-crc32-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let result = scan_bench(&dir, segments, segment_kb, rounds);
    // Only this process's own throwaway directory, and it is removed whether or
    // not the measurement worked.
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn scan_bench(dir: &Path, segments: usize, segment_kb: usize, rounds: usize) -> io::Result<()> {
    let path: PathBuf = dir.join("bench.lvb");
    let body = filler(segment_kb * 1024);

    let mut writer = ContainerWriter::create(&path, "crc32-bench", 120)?;
    for index in 0..segments as u64 {
        let start_100ns = index as i64 * 10_000_000;
        writer.append_segment(index, start_100ns, start_100ns + 10_000_000, &[], &body)?;
    }
    // Deliberately dropped rather than `close()`d: `close` writes a checkpoint,
    // and a checkpoint is exactly what lets `scan` skip the replay. The replay
    // is the path being measured -- it is also the real case the startup
    // `repair_sessions` / `recover` path hits, where the crash came before any
    // checkpoint.
    drop(writer);

    let bytes = std::fs::read(&path)?;
    let mut samples = Vec::with_capacity(rounds);
    let mut replayed = 0usize;
    for _ in 0..rounds {
        let start = Instant::now();
        let scanned = scan(black_box(&bytes))?;
        samples.push(start.elapsed());
        replayed = scanned.records.len();
        black_box(scanned);
    }
    // The instrument, not the result: a checkpoint in the file would make `scan`
    // skip almost every record and report a throughput no disk or CPU can do.
    assert!(
        replayed >= segments,
        "scan replayed {replayed} records of {segments} -- it resumed from a \
         checkpoint instead of walking the file, so this number is not a scan"
    );
    let scan_median = median(samples);
    println!(
        "scan   {:>8.2?} over {} records / {:.1} MB ({:.2} GB/s)   n={rounds}",
        scan_median,
        segments,
        bytes.len() as f64 / 1024.0 / 1024.0,
        throughput_gbps(bytes.len(), scan_median),
    );
    Ok(())
}
