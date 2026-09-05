//! Benchmark library generator + indexing timing (Option A: JPEG-only, hermetic).
//!
//! Generates deterministic seeded JPEGs (no `ffmpeg`/`heif-convert` needed),
//! bulk-indexes them cold, then re-indexes warm. Never touches `medialibrary/`.
//!
//! Run:
//! ```sh
//! cargo test --test bench_index -- --nocapture            # tiny smoke (CI)
//! cargo test --test bench_index -- --ignored --nocapture  # 200 + 5k
//! cargo run -- --root benchdata/bench-5k index            # warm re-index check
//! rm -rf benchdata/bench-5k                               # manual cleanup
//! ```
//!
//! Env overrides: `BENCH_PHOTOS`, `BENCH_ALBUMS`, `BENCH_SEED`, `BENCH_ROOT`
//! (custom persistent root for the large preset; defaults to
//! `<repo>/benchdata/bench-5k`). Timing goes to stderr (`--nocapture` to
//! see it); CI asserts counts only, never wall time.
//!
//! NOTE: uses `index::index_library` (no `CliProgress`) on purpose —
//! `CliProgress::new` installs a process-once `ctrlc` handler and would fail
//! on repeat calls inside one test process.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use common::write_bench_jpeg;
use hallward::{catalog, index, library};
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const MARKER: &str = "bench.marker";
const MARKER_TEXT: &str = "hallward benchmark library (generated) - safe to delete\n";

/// (width, height) for file `i`: 70% 1600px, 20% 4000px, 10% 640px.
fn dims_for_slot(i: usize) -> (u32, u32) {
    match i % 10 {
        0..=6 => (1600, 1067),
        7..=8 => (4000, 2667),
        _ => (640, 480),
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Write `photos` deterministic JPEGs across `albums` albums under `root`.
/// Generation time is the caller's concern (kept out of index timing).
/// Files are independent (seeded per index), so they encode in parallel.
fn generate(root: &Path, photos: usize, albums: usize, seed: u64, dims: fn(usize) -> (u32, u32)) {
    assert!(albums > 0, "bench needs at least one album");
    let done = AtomicUsize::new(0);
    (0..photos).into_par_iter().for_each(|i| {
        let album = i % albums;
        let rel = format!("2024/Album-{album:02}/img-{i:05}.jpg");
        let (w, h) = dims(i);
        write_bench_jpeg(root, &rel, w, h, seed ^ i as u64);
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(500) || n == photos {
            eprintln!("bench: generated {n}/{photos} files");
        }
    });
}

/// Resolve the persistent root for the large preset: `BENCH_ROOT` wins,
/// otherwise `<repo>/benchdata/<name>`. Refuses to use a directory holding a
/// foreign (non-bench) catalog.
fn persistent_root(name: &str) -> PathBuf {
    if let Some(root) = std::env::var_os("BENCH_ROOT") {
        let root = PathBuf::from(root);
        claim_bench_dir(&root);
        return root;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benchdata")
        .join(name);
    claim_bench_dir(&root);
    root
}

fn claim_bench_dir(root: &Path) {
    fs::create_dir_all(root).expect("create bench root");
    let catalog = root.join(library::ALBUM_DIR).join("catalog.sqlite");
    if catalog.exists() && !root.join(MARKER).exists() {
        panic!(
            "refusing to benchmark in {}: catalog exists without {MARKER} (not a bench library?)",
            root.display()
        );
    }
    fs::write(root.join(MARKER), MARKER_TEXT).expect("write bench marker");
}

fn remove_catalog(root: &Path) {
    let album = root.join(library::ALBUM_DIR);
    if album.exists() {
        fs::remove_dir_all(&album).expect("clear bench catalog for cold run");
    }
}

fn count(root: &Path) -> i64 {
    let conn = catalog::open(root, false).expect("open bench catalog");
    catalog::count(&conn).expect("count bench catalog")
}

/// Generate + cold index + warm re-index, asserting counts and printing times.
fn run_bench(
    root: &Path,
    photos: usize,
    albums: usize,
    seed: u64,
    label: &str,
    dims: fn(usize) -> (u32, u32),
) {
    let gen_start = Instant::now();
    generate(root, photos, albums, seed, dims);
    eprintln!(
        "bench [{label}]: generated {photos} files in {albums} albums in {:.1}s",
        gen_start.elapsed().as_secs_f64()
    );

    let cold_start = Instant::now();
    let cold = index::index_library(root).expect("cold index");
    let cold_secs = cold_start.elapsed().as_secs_f64();
    assert_eq!(
        cold.added_or_updated, photos,
        "cold index should add every generated file"
    );
    assert_eq!(count(root), photos as i64);

    let warm_start = Instant::now();
    let warm = index::index_library(root).expect("warm index");
    let warm_secs = warm_start.elapsed().as_secs_f64();
    assert_eq!(warm.skipped, photos, "warm re-index should skip every file");

    eprintln!(
        "bench [{label}]: {photos} files, {albums} albums, seed {seed}, \
         cold {cold_secs:.1}s (added {}), warm {warm_secs:.1}s (skipped {})",
        cold.added_or_updated, warm.skipped,
    );
}

fn temp_library() -> tempfile::TempDir {
    // `hallward-bench` prefix (no leading dot): `index_library` prunes hidden
    // dirs during its walk, like production (`~/Pictures/...`).
    let dir = tempfile::Builder::new()
        .prefix("hallward-bench")
        .tempdir()
        .expect("tempdir for bench library");
    catalog::open(dir.path(), true).expect("create bench catalog");
    dir
}

/// Tiny smoke so `cargo test` stays fast. Realistic sizes live in ignored presets.
#[test]
fn bench_smoke() {
    let dir = temp_library();
    run_bench(dir.path(), 4, 2, 42, "smoke", |_| (64, 48));
}

/// 200-file JPEG mix (1600/4000/640). Local timing only.
#[test]
#[ignore]
fn bench_small() {
    let photos = env_usize("BENCH_PHOTOS", 200);
    let albums = env_usize("BENCH_ALBUMS", 10);
    let seed = env_u64("BENCH_SEED", 42);
    let dir = temp_library();
    run_bench(dir.path(), photos, albums, seed, "small", dims_for_slot);
}

/// Large preset: 5k photos in `benchdata/bench-5k` (gitignored, kept after
/// the run for repeated `index` timings). Local only.
#[test]
#[ignore]
fn bench_large() {
    let photos = env_usize("BENCH_PHOTOS", 5000);
    let albums = env_usize("BENCH_ALBUMS", 20);
    let seed = env_u64("BENCH_SEED", 42);
    let root = persistent_root("bench-5k");
    remove_catalog(&root);
    catalog::open(&root, true).expect("create bench catalog");
    run_bench(&root, photos, albums, seed, "large", dims_for_slot);
}
