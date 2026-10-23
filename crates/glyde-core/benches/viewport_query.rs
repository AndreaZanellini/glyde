// Copyright 2026 The Glyde Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Benchmark: viewport_query, at each pyramid level (docs/SPEC.md §5,
//! docs/QUALITY.md §3). `decimate_viewport` is what the UI thread calls once
//! per frame while panning/zooming, so its cost gates SPEC §5's "Pan/zoom
//! frame time: p99 ≤100ms".
//!
//! Per issue #61 decision 2, only the absolute SPEC §5 ceiling is
//! build-blocking here; the >15% regression comparison is a manual check on
//! the SPEC §5 reference machine using criterion's own local baseline
//! comparison (see `index_build.rs`'s module doc for the full rationale).
//!
//! `first_full_view_spilled` (issue #145) is the first decimation after a
//! spilled load: every channel's whole file at a Retina-wide viewport, read
//! through fresh mappings of the spill files and their on-disk pyramids.

use criterion::{criterion_group, criterion_main, Criterion};
use glyde_core::dsp::decimation::{build_pyramid, decimate_viewport, Pyramid, PYRAMID_FACTOR};
use glyde_core::index::disk_pyramid::DiskPyramid;
use glyde_core::ingest::Dataset;
use rayon::prelude::*;
use std::time::{Duration, Instant};

#[path = "support/mod.rs"]
mod support;

const SAMPLE_COUNT: usize = 8_000_000;
const PIXEL_COLUMNS: usize = 1600;
const FIXTURE_SEED: u64 = 0xC0DE;

/// SPEC §5 "Pan/zoom frame time": p99 ≤100ms. `decimate_viewport`'s cost is
/// SPEC §3.1's O(PYRAMID_FACTOR · log n) aggregation — expected microseconds
/// to low milliseconds even at 8M samples — so the ceiling is the SPEC
/// number itself, no extra CI-noise margin: a regression large enough to
/// threaten 100ms (e.g. an accidental linear scan) is nowhere near that
/// noise floor.
const VIEWPORT_QUERY_CEILING: Duration = Duration::from_millis(100);

/// The spilled fixture of `first_full_view_spilled`: rows, channels, and
/// the pixel columns of a 1440-point-wide plot at 2 px/pt.
const SPILLED_ROWS: usize = 8_000_000;
const SPILLED_CHANNELS: usize = 8;
const SPILLED_PIXEL_COLUMNS: usize = 2_880;

fn bench_viewport_query(c: &mut Criterion) {
    let (samples, timestamps) = support::synthetic_series(SAMPLE_COUNT, FIXTURE_SEED);
    let pyramid = build_pyramid(&samples, &timestamps);
    let full_span = SAMPLE_COUNT as i128;

    let mut group = c.benchmark_group("viewport_query");

    // One scenario per pyramid level: a range sized so that level is the
    // one `decimate_viewport` predominantly aggregates from (SPEC §3.1
    // "exact edges, aggregated interior").
    for level in 0..pyramid.len() {
        let span = (PYRAMID_FACTOR.pow(level as u32 + 1) * PIXEL_COLUMNS) as i128;
        let range = (0, span.min(full_span) - 1);

        let start = Instant::now();
        let _ = decimate_viewport(&pyramid, &samples, &timestamps, range, PIXEL_COLUMNS);
        let elapsed = start.elapsed();
        assert!(
            elapsed <= VIEWPORT_QUERY_CEILING,
            "viewport query at pyramid level {level} took {elapsed:?}, exceeding the \
             {VIEWPORT_QUERY_CEILING:?} build-blocking ceiling (SPEC §5: pan/zoom p99 ≤100ms)"
        );

        group.bench_function(format!("level_{level}"), |b| {
            b.iter(|| {
                decimate_viewport(
                    std::hint::black_box(&pyramid),
                    std::hint::black_box(&samples),
                    std::hint::black_box(&timestamps),
                    std::hint::black_box(range),
                    std::hint::black_box(PIXEL_COLUMNS),
                )
            })
        });
    }

    // SPEC §3.1 convergence guarantee: fewer raw samples than pixel columns
    // draws individual points instead of aggregating.
    let raw_range = (0, (PIXEL_COLUMNS as i128 / 2) - 1);
    group.bench_function("raw_convergence", |b| {
        b.iter(|| {
            decimate_viewport(
                std::hint::black_box(&pyramid),
                std::hint::black_box(&samples),
                std::hint::black_box(&timestamps),
                std::hint::black_box(raw_range),
                std::hint::black_box(PIXEL_COLUMNS),
            )
        })
    });

    group.finish();
}

/// Issue #145: the first full view after a spilled load. Its decimation
/// reads, at every column edge of every channel, pages of the spill files
/// and of the on-disk pyramids that no frame has touched yet. Two cases:
/// - fresh mappings over a page cache that still holds the files, as right
///   after the load: gated at SPEC §5's frame ceiling;
/// - the same view after the OS dropped the files from its page cache, as
///   it does under memory pressure (the issue's 16 GB machine, busy, had
///   just written ~8 GB of spill and pyramid files), where every page
///   touched is a random disk read: measured, with the page-ins
///   it took, but not gated, since it is the disk's latency. Only on macOS
///   and Linux, which can be asked to drop a file's cached pages.
fn bench_first_full_view_spilled(c: &mut Criterion) {
    use std::io::Write as _;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("first_view.csv");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create fixture"));
    write!(file, "timestamp").expect("write fixture");
    for channel in 0..SPILLED_CHANNELS {
        write!(file, ",value{channel}").expect("write fixture");
    }
    writeln!(file).expect("write fixture");
    for i in 0..SPILLED_ROWS {
        write!(file, "{i}").expect("write fixture");
        for channel in 0..SPILLED_CHANNELS {
            let value = (i as f64 * 0.001 + channel as f64).sin();
            write!(file, ",{value:.6}").expect("write fixture");
        }
        writeln!(file).expect("write fixture");
    }
    file.flush().expect("flush fixture");
    drop(file);

    let spill = dir.path().join("spill");
    std::fs::create_dir_all(&spill).expect("spill dir");
    let dataset = glyde_core::ingest::load_with_budget(
        &path,
        glyde_core::budget::RamBudget::from_total_ram_bytes(0),
        &spill,
    )
    .expect("spilled fixture must open");
    assert!(
        dataset.is_spilled(),
        "a zero budget must force the spill path"
    );
    let pyramids = glyde_core::ingest::disk_pyramids_for_dataset(&dataset, &spill);
    assert!(
        pyramids.iter().all(Option::is_some),
        "every channel gets its on-disk pyramid"
    );
    let ticks = dataset.time.to_pyramid_ticks();
    let range = (ticks[0], ticks[ticks.len() - 1]);
    let full_view = || full_view_buckets(&dataset, &pyramids, &ticks, range);

    let start = Instant::now();
    let buckets = full_view();
    let elapsed = start.elapsed();
    assert_eq!(buckets, SPILLED_CHANNELS * SPILLED_PIXEL_COLUMNS);
    assert!(
        elapsed <= VIEWPORT_QUERY_CEILING,
        "the first full view after a spilled load ({SPILLED_CHANNELS} channels, \
         {SPILLED_ROWS} rows, {SPILLED_PIXEL_COLUMNS} columns) took {elapsed:?}, exceeding \
         the {VIEWPORT_QUERY_CEILING:?} build-blocking ceiling (SPEC §5: pan/zoom p99 ≤100ms)"
    );
    println!("first_full_view_spilled: first view after the load took {elapsed:?}");

    if !page_cache::drop_files_under(&spill) {
        println!(
            "first_full_view_spilled: this OS cannot drop a file's cached pages; cold case skipped"
        );
        return;
    }
    let faults = page_cache::major_faults();
    let start = Instant::now();
    full_view();
    let elapsed = start.elapsed();
    let edges = (SPILLED_PIXEL_COLUMNS + 1) * SPILLED_CHANNELS;
    let page_ins = page_cache::major_faults() - faults;
    println!(
        "first_full_view_spilled: cold page cache took {elapsed:?}, {page_ins} page-ins \
         ({:.2} per column edge per channel)",
        page_ins as f64 / edges as f64
    );

    let mut group = c.benchmark_group("viewport_query");
    group.sample_size(10);
    group.bench_function("first_full_view_spilled/cold page cache", |b| {
        b.iter_custom(|iterations| {
            (0..iterations)
                .map(|_| {
                    assert!(page_cache::drop_files_under(&spill));
                    let start = Instant::now();
                    std::hint::black_box(full_view());
                    start.elapsed()
                })
                .sum()
        })
    });
    group.finish();
}

/// Every channel of `dataset` decimated over `range`, in parallel as the
/// app's background decimation does; returns how many buckets that made.
fn full_view_buckets(
    dataset: &Dataset,
    pyramids: &[Option<DiskPyramid>],
    ticks: &[i128],
    range: (i128, i128),
) -> usize {
    dataset
        .columns
        .par_iter()
        .zip(pyramids)
        .map(|(series, pyramid)| {
            let samples = series.values().as_f64_slice().expect("an f64 channel");
            let pyramid = pyramid.as_ref().expect("a pyramid");
            pyramid
                .decimate(samples, ticks, range, SPILLED_PIXEL_COLUMNS)
                .len()
        })
        .sum()
}

/// Dropping files from the OS page cache, and counting the page-ins that
/// brought them back, for the cold case of `first_full_view_spilled`.
mod page_cache {
    use std::path::Path;

    #[cfg(unix)]
    #[repr(C)]
    struct Rusage {
        time: [i64; 4],
        counters: [i64; 14],
    }

    #[cfg(unix)]
    extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }

    /// This process's major page faults so far (each one a page read from
    /// disk), or 0 where it cannot be read.
    pub fn major_faults() -> i64 {
        #[cfg(unix)]
        {
            let mut usage = Rusage {
                time: [0; 4],
                counters: [0; 14],
            };
            // SAFETY: `Rusage` has `struct rusage`'s layout on 64-bit unix
            // (two `timeval`s, then 14 `long`s); RUSAGE_SELF is 0.
            if unsafe { getrusage(0, &mut usage) } == 0 {
                return usage.counters[5];
            }
        }
        0
    }

    /// Asks the OS to drop every file under `dir` from its page cache;
    /// `false` where it cannot be asked.
    pub fn drop_files_under(dir: &Path) -> bool {
        let mut supported = true;
        for entry in std::fs::read_dir(dir).expect("read spill dir") {
            let path = entry.expect("spill dir entry").path();
            if path.is_dir() {
                supported &= drop_files_under(&path);
            } else {
                supported &= drop_file(&path);
            }
        }
        supported
    }

    #[cfg(target_os = "macos")]
    fn drop_file(path: &Path) -> bool {
        extern "C" {
            fn msync(addr: *mut std::ffi::c_void, len: usize, flags: i32) -> i32;
        }
        const MS_INVALIDATE: i32 = 0x0002;
        let file = std::fs::File::open(path).expect("open spill file");
        file.sync_all().expect("sync spill file");
        if file.metadata().expect("stat spill file").len() == 0 {
            return true;
        }
        // SAFETY: a read-only map of a file nothing writes to while it lives.
        let map = unsafe { memmap2::Mmap::map(&file) }.expect("map spill file");
        // SAFETY: `map` is a live mapping of exactly `map.len()` bytes.
        let status = unsafe { msync(map.as_ptr() as *mut _, map.len(), MS_INVALIDATE) };
        assert_eq!(status, 0, "msync(MS_INVALIDATE) failed on {path:?}");
        true
    }

    #[cfg(target_os = "linux")]
    fn drop_file(path: &Path) -> bool {
        use std::os::fd::AsRawFd as _;
        extern "C" {
            fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
        }
        const POSIX_FADV_DONTNEED: i32 = 4;
        let file = std::fs::File::open(path).expect("open spill file");
        file.sync_all().expect("sync spill file");
        // SAFETY: a valid open descriptor; offset 0, length 0 is "the
        // whole file".
        let status = unsafe { posix_fadvise(file.as_raw_fd(), 0, 0, POSIX_FADV_DONTNEED) };
        assert_eq!(status, 0, "posix_fadvise(DONTNEED) failed on {path:?}");
        true
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn drop_file(_path: &Path) -> bool {
        false
    }
}

criterion_group!(benches, bench_viewport_query, bench_first_full_view_spilled);
criterion_main!(benches);
