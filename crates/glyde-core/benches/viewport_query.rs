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

use criterion::{criterion_group, criterion_main, Criterion};
use glyde_core::dsp::decimation::{build_pyramid, decimate_viewport, PYRAMID_FACTOR};
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

criterion_group!(benches, bench_viewport_query);
criterion_main!(benches);
