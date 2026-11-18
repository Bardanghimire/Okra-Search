//! Latency benchmark for the file-search backends, measured from an ordinary (unelevated)
//! process the way the launcher uses them.
//!
//!     cargo run --release -p okra-core --example bench_backends
//!
//! Backends: the elevated NTFS helper (raw pipe round trip), the Everything SDK (when its DLL
//! can be loaded) and the built-in walker index (through the file provider, so it includes the
//! provider's scoring). Environment variables:
//!     FS_RUNS=100               timed runs per query (after 5 warm-up runs)
//!     FS_PACE_MS=0              pause before every call, not timed (try 2 to mimic typing)
//!     FS_EVERYTHING_DLL=path    Everything64.dll to use
//!     FS_EVERYTHING_SCOPE=path  folder prefix added to Everything queries, to roughly match the
//!                               helper's scoped dataset, e.g. C:\Users\barda\
//!     FS_BENCH_CSV=path         also write the results as CSV
use okra_core::providers::everything::EverythingSdk;
use okra_core::providers::files::{ntfs_helper_search, FilesProvider};
use okra_core::{Config, FileBackendKind, Provider};
use std::path::PathBuf;
use std::time::Instant;

const QUERIES: &[&str] = &[
    "ca",
    "car",
    "carg",
    "cargo",
    "Cargo.toml",
    "ipc.rs",
    ".rs",
    "okra",
    "readme",
    "zzqxjkvw",
];
const WARMUP: usize = 5;
const RESULT_LIMIT: usize = 60;

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

fn bench(
    label: &str,
    runs: usize,
    pace_ms: u64,
    csv: &mut Vec<String>,
    f: &mut dyn FnMut(&str) -> Option<usize>,
) {
    let pause = || {
        if pace_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(pace_ms));
        }
    };
    println!("\n== {label}");
    println!(
        "{:<12} {:>8} {:>10} {:>10} {:>10} {:>10} {:>9}",
        "query", "results", "first ms", "median ms", "p95 ms", "max ms", "failed"
    );
    for q in QUERIES.iter().copied() {
        let start = Instant::now();
        let first = f(q);
        let first_ms = start.elapsed().as_secs_f64() * 1000.0;
        let mut count = first;
        for _ in 0..WARMUP {
            pause();
            if let Some(n) = f(q) {
                count = count.or(Some(n));
            }
        }
        // Only successful calls are timed. A failed call (backend unavailable or busy) returns
        // almost instantly and would otherwise make the backend look far faster than it is.
        let mut times = Vec::with_capacity(runs);
        let mut failed = 0usize;
        for _ in 0..runs {
            pause();
            let t = Instant::now();
            let result = f(q);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            match result {
                Some(n) => {
                    count = count.or(Some(n));
                    times.push(ms);
                }
                None => failed += 1,
            }
        }
        let Some(count) = count.filter(|_| !times.is_empty()) else {
            println!(
                "{q:<12} {:>8} {:>10} {:>10} {:>10} {:>10} {failed:>4}/{runs}",
                "n/a", "-", "-", "-", "-"
            );
            continue;
        };
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = percentile(&times, 0.5);
        let p95 = percentile(&times, 0.95);
        let max = *times.last().unwrap();
        println!("{q:<12} {count:>8} {first_ms:>10.3} {median:>10.3} {p95:>10.3} {max:>10.3} {failed:>4}/{runs}");
        csv.push(format!(
            "{label},{q},{count},{first_ms:.3},{median:.3},{p95:.3},{max:.3},{failed},{runs}"
        ));
    }
}

fn main() {
    let runs: usize = std::env::var("FS_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(100);
    let pace_ms: u64 = std::env::var("FS_PACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut csv =
        vec!["backend,query,results,first_ms,median_ms,p95_ms,max_ms,failed,runs".to_string()];
    println!("Okra file-backend benchmark: {runs} timed runs per query, {WARMUP} warm-up runs, {pace_ms} ms pause between calls, up to {RESULT_LIMIT} results");
    println!("Use a --release build; debug builds are much slower and are not comparable.");

    // 1. Elevated NTFS helper, raw pipe round trip.
    let helper = FilesProvider::new(&Config::default()).helper_status();
    println!("\n{}", helper.describe());
    if helper.is_serving() {
        bench("NTFS helper (pipe)", runs, pace_ms, &mut csv, &mut |q| {
            ntfs_helper_search(q, RESULT_LIMIT).map(|r| r.len())
        });
    } else {
        println!("Skipping the NTFS helper: it is not serving searches.");
    }

    // 2. Everything SDK, when available.
    let dll: Option<PathBuf> = std::env::var_os("FS_EVERYTHING_DLL").map(PathBuf::from);
    match EverythingSdk::load(dll.as_deref()) {
        Some(sdk) => {
            let scope = std::env::var("FS_EVERYTHING_SCOPE").ok();
            bench("Everything (SDK)", runs, pace_ms, &mut csv, &mut |q| {
                let full = match &scope {
                    Some(prefix) => format!("{prefix} {q}"),
                    None => q.to_string(),
                };
                sdk.search(&full, RESULT_LIMIT as u32).map(|r| r.len())
            });
        }
        None => println!(
            "\nSkipping Everything: Everything64.dll was not found (set FS_EVERYTHING_DLL)."
        ),
    }

    // 3. Built-in walker index, through the provider (includes scoring).
    let cfg = Config {
        file_backend: FileBackendKind::Builtin,
        ..Config::default()
    };
    println!("\nBuilding the walker index over {:?} ...", cfg.file_roots);
    let start = Instant::now();
    let walker = FilesProvider::new(&cfg);
    // The walker fills its index in the background, so wait until it answers a known query.
    let mut walker_ready = false;
    while start.elapsed().as_secs() < 60 {
        if !walker.query("cargo", 20).is_empty() {
            walker_ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if walker_ready {
        println!(
            "walker index answered its first query after {:.0} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
    } else {
        println!("walker index returned nothing for \"cargo\" after 60 s; its numbers below are not meaningful");
    }
    bench(
        "Built-in walker (provider)",
        runs,
        pace_ms,
        &mut csv,
        &mut |q| Some(walker.query(q, 20).len()),
    );

    if let Ok(path) = std::env::var("FS_BENCH_CSV") {
        match std::fs::write(&path, csv.join("\n") + "\n") {
            Ok(()) => println!("\nWrote {path}"),
            Err(error) => println!("\nCould not write {path}: {error}"),
        }
    }
}
