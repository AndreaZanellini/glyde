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

//! Benchmark: progressive_pyramid. Budget is build-blocking (docs/SPEC.md
//! §5, issue #90): as a file loads, `super::csv`'s row-count-doubling
//! progressive checkpoint schedule must not spend more than a documented
//! ceiling building pyramids in total, no matter how many checkpoints fire.
//!
//! This benches [`extend_pyramid`] directly, called repeatedly on the same
//! doubling schedule `ingest::dataset::PyramidCursor` drives in the real
//! ingestion path (`FIRST_PROGRESS_CHECKPOINT_ROWS = 20_000`, then ×2, ×2,
//! …) — not the whole CSV-parsing pipeline, since parsing dominates a full
//! `ingest::load_progressive` call's wall time and would drown out the
//! pyramid-specific cost this bench exists to gate.
//!
//! Also reports, for local comparison only, what the pre-#90 behavior (a
//! full [`build_pyramid`] rebuild at every checkpoint) would have cost on
//! the same schedule — never a CI gate itself (the two are measured in the
//! same process back to back, which is exactly the kind of noisy
//! same-run comparison docs/QUALITY.md §3.2 reserves for the manual
//! reference-machine ritual, not a hard assertion), but it makes a local
//! `cargo bench` run's win legible without cross-referencing this file
//! against git history.

use criterion::{criterion_group, criterion_main, Criterion};
use glyde_core::dsp::decimation::{build_pyramid, extend_pyramid, Bucket};
use std::time::{Duration, Instant};

/// Mirrors `ingest::csv::FIRST_PROGRESS_CHECKPOINT_ROWS` (`pub(crate)`, so
/// not reachable from this external bench crate) — the row-count-doubling
/// schedule's first checkpoint size.
const FIRST_CHECKPOINT_ROWS: usize = 20_000;

const TOTAL_SAMPLES: usize = 4_000_000;

/// Generous CI-noise margin (see `index_build.rs`'s module doc for why:
/// shared/virtualized runners are slower than the SPEC §5 reference
/// machine). Extending a pyramid across the whole doubling schedule does
/// roughly the same total work as one `build_pyramid` call over the final
/// sample count (each sample is bucketed once, not once per checkpoint), so
/// this ceiling is generous relative to that, not tight.
const CEILING: Duration = Duration::from_secs(5);

/// The doubling schedule `super::csv` drives: `FIRST_CHECKPOINT_ROWS`, then
/// ×2 each time, up to (and including) `total` as the final checkpoint.
fn checkpoint_schedule(total: usize) -> Vec<usize> {
    let mut sizes = Vec::new();
    let mut next = FIRST_CHECKPOINT_ROWS;
    while next < total {
        sizes.push(next);
        next = next.saturating_mul(2);
    }
    sizes.push(total);
    sizes
}

fn synthetic_samples(total: usize) -> (Vec<f64>, Vec<i128>) {
    // A tiny deterministic PRNG (xorshift64*), matching
    // `tests/golden/decimation.rs`'s fixture generator, so this bench needs
    // no extra dependency.
    let mut state: u64 = 0xC0FFEE_u64;
    let mut next_f64 = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bits = state >> 11;
        (bits as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    };

    let samples: Vec<f64> = (0..total).map(|_| next_f64() * 1000.0).collect();
    let timestamps: Vec<i128> = (0..total as i128).collect();
    (samples, timestamps)
}

/// Runs the doubling checkpoint schedule via [`extend_pyramid`], as
/// `ingest::dataset::PyramidCursor` does in the real ingestion path.
fn run_incremental(samples: &[f64], timestamps: &[i128], schedule: &[usize]) -> Duration {
    let start = Instant::now();
    let mut pyramid: Vec<Vec<Bucket>> = Vec::new();
    let mut previous_len = 0usize;
    for &len in schedule {
        pyramid = extend_pyramid(pyramid, previous_len, &samples[..len], &timestamps[..len]);
        previous_len = len;
    }
    start.elapsed()
}

/// The pre-#90 behavior: a full [`build_pyramid`] rebuild at every
/// checkpoint (local-comparison only, see module doc).
fn run_full_rebuild(samples: &[f64], timestamps: &[i128], schedule: &[usize]) -> Duration {
    let start = Instant::now();
    for &len in schedule {
        std::hint::black_box(build_pyramid(&samples[..len], &timestamps[..len]));
    }
    start.elapsed()
}

fn bench_progressive_pyramid(c: &mut Criterion) {
    let (samples, timestamps) = synthetic_samples(TOTAL_SAMPLES);
    let schedule = checkpoint_schedule(TOTAL_SAMPLES);

    // Absolute-ceiling gate: one clean timed run of the real code path
    // (docs/QUALITY.md §3: "exceeding a budget fails the build").
    let elapsed = run_incremental(&samples, &timestamps, &schedule);
    assert!(
        elapsed <= CEILING,
        "extending a pyramid across the full {} progress checkpoints from {TOTAL_SAMPLES} \
         samples took {elapsed:?}, exceeding the {CEILING:?} build-blocking ceiling (issue #90)",
        schedule.len()
    );

    // Local comparison only, never a CI gate (see module doc).
    let full_rebuild_elapsed = run_full_rebuild(&samples, &timestamps, &schedule);
    eprintln!(
        "progressive_pyramid: incremental {elapsed:?} vs. full-rebuild-per-checkpoint \
         {full_rebuild_elapsed:?} across {} checkpoints, {TOTAL_SAMPLES} samples",
        schedule.len()
    );

    // Statistical measurement, for local regression comparisons only (see
    // `index_build.rs`'s module doc for why this is not a CI-computed
    // regression gate).
    let mut group = c.benchmark_group("progressive_pyramid");
    group.sample_size(10);
    group.bench_function("incremental_extend_across_doubling_schedule", |b| {
        b.iter(|| run_incremental(&samples, &timestamps, std::hint::black_box(&schedule)))
    });
    group.finish();
}

criterion_group!(benches, bench_progressive_pyramid);
criterion_main!(benches);
