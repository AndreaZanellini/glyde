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

//! Benchmark: first_plot. Budget is build-blocking (docs/SPEC.md §5,
//! docs/ROADMAP.md M3 "Background progressive build emitting partial
//! levels"): "first meaningful plot, any file size: ≤2s (progressive: render
//! what is indexed, keep indexing in background)".
//!
//! This measures wall-clock from calling
//! [`ingest::open_dataset_progressive`] on a large fixture to its *first*
//! progress checkpoint — the moment a real, renderable [`ingest::Dataset`]
//! first becomes available, which is what lets `glyde-app`'s indexer thread
//! send the UI its first [`IndexingMessage::Progress`] (see
//! `crates/glyde-app/src/plumbing/mod.rs`).
//!
//! `open_dataset_progressive` itself is a single blocking call that only
//! returns once the *whole* file has been parsed (the checkpoint callback
//! fires early, mid-call, but the call doesn't return until completion) —
//! `glyde-app`'s real indexer thread gets around this by running it on a
//! background thread and only waiting on the channel for the first message.
//! An earlier version of this bench did the same (spawned a thread per
//! criterion sample and didn't join it), reasoning that only the
//! already-measured [`Duration`] mattered — but criterion's `b.iter` times
//! the *wall-clock* cost of calling the closure, not its return value, so
//! the fast-returning closure let criterion's warmup phase alone spawn
//! dozens of concurrent full-file-parsing threads, each holding a full
//! parsed `Dataset` until it finished — a 30+ GB RSS spike that got the
//! process OOM-killed. This version stays deliberately simple and safe
//! instead: one call, no threading, run to completion, same as
//! `index_build`'s established pattern below. The hard gate below still
//! asserts on the correct, early value; the criterion statistical section's
//! reported "time" is dominated by full-parse completion, not first-
//! checkpoint latency — expected and harmless, since (per `index_build`'s
//! own module doc) that section is a local regression aid only, never a CI
//! gate.

use criterion::{criterion_group, criterion_main, Criterion};
use glyde_core::ingest;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[path = "support/mod.rs"]
mod support;

/// SPEC §5: "first meaningful plot, any file size: ≤2s". Reaching the first
/// checkpoint only requires reading a small prefix of the file (20,000 kept
/// rows, `ingest::csv`'s progress schedule — see
/// `crates/glyde-core/src/ingest/csv.rs`), regardless of the file's total
/// size, so this ceiling is tight relative to the whole-file budgets the
/// other benches enforce — plenty of margin remains for CI-runner noise
/// (issue #61's documented pattern) without weakening what this bench
/// actually proves.
const FIRST_PLOT_CEILING: Duration = Duration::from_secs(10);

const FIXTURE_BYTES: u64 = 1024 * 1024 * 1024;
const FIXTURE_COLUMNS: usize = 8;
const FIXTURE_SEED: u64 = 0xF1257;

fn bench_first_plot(c: &mut Criterion) {
    let fixture = support::fixture_dir().join("bench_first_plot_1gb.csv");
    support::ensure_csv_fixture(&fixture, FIXTURE_BYTES, FIXTURE_COLUMNS, FIXTURE_SEED)
        .expect("failed to generate the first_plot bench fixture");

    // Absolute-ceiling gate: one clean timed run to the first checkpoint,
    // asserted directly against the SPEC §5 budget (docs/QUALITY.md §3:
    // "exceeding a budget fails the build").
    let elapsed = time_to_first_checkpoint(&fixture);
    assert!(
        elapsed <= FIRST_PLOT_CEILING,
        "first progress checkpoint on a 1 GB CSV took {elapsed:?}, exceeding the \
         {FIRST_PLOT_CEILING:?} build-blocking ceiling (SPEC §5: first meaningful \
         plot, any file size, ≤2s)"
    );

    // Statistical measurement, for local regression comparisons only (see
    // `index_build`'s module doc for why this is not a CI-computed
    // regression gate; see this file's module doc for why the reported
    // "time" here reflects full-parse cost, not first-checkpoint latency).
    // Sample size lowered from criterion's default since each iteration
    // parses a full 1GB fixture.
    let mut group = c.benchmark_group("first_plot");
    group.sample_size(10);
    group.bench_function("csv_1gb_first_checkpoint", |b| {
        b.iter(|| time_to_first_checkpoint(std::hint::black_box(&fixture)))
    });
    group.finish();
}

/// Runs [`ingest::open_dataset_progressive`] on `fixture` — a single,
/// synchronous, un-threaded call (see the module doc for why this bench
/// deliberately avoids spawning a thread per call) — and returns the
/// wall-clock time from the call until its first checkpoint fired, measured
/// via [`Instant`]s taken before the call and inside the checkpoint
/// callback. The call itself keeps running after that (to completion, like
/// any full parse) before this function can return the already-measured
/// value.
fn time_to_first_checkpoint(fixture: &std::path::Path) -> Duration {
    let start = Instant::now();
    let (tx, rx) = mpsc::channel();

    ingest::open_dataset_progressive(fixture, |checkpoint| {
        let _ = tx.send((Instant::now(), checkpoint.rows_read));
    })
    .expect("first_plot bench fixture must open");

    let (first_checkpoint_at, rows_read) = rx
        .try_iter()
        .next()
        .expect("a 1 GB fixture must cross at least one progress checkpoint");
    assert!(
        rows_read > 0,
        "a checkpoint must report a nonzero row count"
    );

    first_checkpoint_at - start
}

criterion_group!(benches, bench_first_plot);
criterion_main!(benches);
