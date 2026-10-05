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

//! Benchmark: welch. Budget is build-blocking (docs/SPEC.md §5, docs/QUALITY.md §3:
//! PSD of a 10M-sample selection ≤1s; Welch on 1M / 10M samples).
//!
//! Times the whole user-facing path, not just the FFTs: `dsp::psd::plan_psd_on`
//! classifying the selection's timestamps (SPEC §3.3) followed by
//! `dsp::psd::compute_psd` streaming the samples through Welch in bounded
//! chunks (SPEC §3.2), with the software's default settings.
//!
//! Per issue #61 decision 2, only the absolute SPEC §5 ceiling is
//! build-blocking here; the >15% regression comparison is a manual check on
//! the SPEC §5 reference machine using criterion's own local baseline
//! comparison (see `index_build.rs`'s module doc for the full rationale).

use criterion::{criterion_group, criterion_main, Criterion};
use glyde_core::budget::RamBudget;
use glyde_core::dsp::psd::{
    compute_psds, plan_psd_on, AxisScale, FrequencyUnit, PsdMemoryCap, PsdPlan, PsdSettings,
};
use std::time::{Duration, Instant};

#[path = "support/mod.rs"]
mod support;

const FIXTURE_SEED: u64 = 0x5EC7;

/// SPEC §5 "PSD of a 10 M-sample selection ≤ 1 s", used as the ceiling
/// itself with no extra margin: the measured path is a few hundred FFTs, and
/// a regression that threatens a second (e.g. reading the selection twice
/// per window) is far above CI-runner noise.
const PSD_10M_CEILING: Duration = Duration::from_secs(1);

const NANOS: AxisScale = AxisScale {
    ticks_per_unit: 1_000_000_000,
    frequency_unit: FrequencyUnit::Hertz,
};

/// `count` samples of noise plus a tone, on a 1 kHz nanosecond axis.
fn fixture(count: usize) -> (Vec<f64>, Vec<i128>) {
    let mut rng = support::Xorshift64::new(FIXTURE_SEED);
    let samples = (0..count)
        .map(|n| (n as f64 * 0.1).sin() + rng.next_f64())
        .collect();
    let ticks = (0..count as i128).map(|n| n * 1_000_000).collect();
    (samples, ticks)
}

fn plan(ticks: &[i128]) -> PsdPlan {
    plan_psd_on(
        ticks,
        NANOS,
        0..ticks.len(),
        &PsdSettings::default(),
        1,
        PsdMemoryCap::for_budget(&RamBudget::from_system()),
    )
    .expect("an in-memory axis cannot fail to scan")
    .expect("a uniform fixture always has a PSD")
}

fn plan_and_compute(samples: &[f64], ticks: &[i128]) -> usize {
    let plan = plan(ticks);
    compute_psds(ticks, &[samples], &plan, &|_| true)
        .expect("an in-memory source cannot fail")
        .expect("never cancelled")[0]
        .segment_count
}

fn bench_welch(c: &mut Criterion) {
    let mut group = c.benchmark_group("welch");
    group.sample_size(10);

    for (label, count) in [("1M", 1_000_000usize), ("10M", 10_000_000)] {
        let (samples, ticks) = fixture(count);

        if count == 10_000_000 {
            let start = Instant::now();
            let windows = plan_and_compute(&samples, &ticks);
            let elapsed = start.elapsed();
            assert!(windows > 0);
            assert!(
                elapsed <= PSD_10M_CEILING,
                "PSD of a 10M-sample selection took {elapsed:?}, exceeding the \
                 {PSD_10M_CEILING:?} build-blocking ceiling (SPEC §5)"
            );
        }

        group.bench_function(format!("plan_and_compute_{label}"), |b| {
            b.iter(|| {
                plan_and_compute(std::hint::black_box(&samples), std::hint::black_box(&ticks))
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_welch);
criterion_main!(benches);
