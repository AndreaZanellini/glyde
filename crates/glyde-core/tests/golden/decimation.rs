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

//! Decimation golden tests (docs/QUALITY.md §2 Decimation, docs/ROADMAP.md
//! M1/M3). Every test here is `#[ignore]`d because
//! `glyde_core::dsp::decimation::{build_pyramid, decimate_viewport}` are not
//! implemented yet (`todo!()` — docs/ROADMAP.md M3); `cargo test -- --ignored
//! --list` is the M1 maintainer proof that the full golden-test set exists.
//! Un-ignore each test only once the implementation makes it pass — never
//! loosen an assertion to make that happen.

use glyde_core::dsp::decimation::{build_pyramid, decimate_viewport, Bucket, PYRAMID_FACTOR};

/// A tiny deterministic PRNG (xorshift64*) so "random data" fixtures are
/// reproducible without adding a `rand` dependency to the workspace.
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Uniform in `[-1.0, 1.0)`.
    fn next_f64(&mut self) -> f64 {
        let bits = self.next_u64() >> 11; // 53 significant bits
        (bits as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

/// The oracle every pyramid bucket is checked against: a direct scan over
/// the raw samples in `[range.0, range.1]` (inclusive).
fn brute_force_min_max(samples: &[f64], timestamps: &[i128], range: (i128, i128)) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for (&ts, &value) in timestamps.iter().zip(samples.iter()) {
        if ts >= range.0 && ts <= range.1 {
            min = min.min(value);
            max = max.max(value);
        }
    }
    (min, max)
}

#[test]
#[ignore = "docs/ROADMAP.md M3: pyramid/decimation not implemented yet"]
fn spike_preservation_survives_decimation_to_800_columns() {
    const SAMPLE_COUNT: usize = 10_000_000;
    const SPIKE_INDEX: usize = 4_012_345;
    const PIXEL_COLUMNS: usize = 800;
    const SPIKE_AMPLITUDE: f64 = 1.0;

    let mut samples = vec![0.0_f64; SAMPLE_COUNT];
    samples[SPIKE_INDEX] = SPIKE_AMPLITUDE;
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).collect();

    let pyramid = build_pyramid(&samples, &timestamps);
    let columns = decimate_viewport(
        &pyramid,
        &samples,
        &timestamps,
        (0, (SAMPLE_COUNT - 1) as i128),
        PIXEL_COLUMNS,
    );

    assert_eq!(columns.len(), PIXEL_COLUMNS);

    let column_width = SAMPLE_COUNT / PIXEL_COLUMNS;
    let spike_column = SPIKE_INDEX / column_width;

    assert_eq!(
        columns[spike_column].max, SPIKE_AMPLITUDE,
        "the single-sample spike at index {SPIKE_INDEX} must survive into column {spike_column}; \
         a lost spike is a failed test (docs/QUALITY.md §2 Decimation)"
    );
    for (i, bucket) in columns.iter().enumerate() {
        if i != spike_column {
            assert_eq!(
                bucket.max, 0.0,
                "column {i} holds no spike sample and must show max == 0.0"
            );
        }
    }
}

#[test]
#[ignore = "docs/ROADMAP.md M3: pyramid/decimation not implemented yet"]
fn envelope_exactness_matches_brute_force_min_max_at_every_level() {
    // 8^4, so every pyramid level (span 8, 64, 512, 4096) divides the
    // fixture evenly and there is no partial/remainder bucket to reason
    // about — that is a separate, unspecified edge case.
    const SAMPLE_COUNT: usize = 4096;

    let mut rng = Xorshift64::new(0xC0FFEE);
    let samples: Vec<f64> = (0..SAMPLE_COUNT).map(|_| rng.next_f64() * 1000.0).collect();
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).collect();

    let pyramid = build_pyramid(&samples, &timestamps);
    assert!(
        !pyramid.is_empty(),
        "a 4096-sample fixture must produce at least one pyramid level"
    );

    let mut bucket_span = PYRAMID_FACTOR;
    for level in &pyramid {
        for (i, bucket) in level.iter().enumerate() {
            let start = (i * bucket_span) as i128;
            let end = ((i + 1) * bucket_span - 1) as i128;
            let (expected_min, expected_max) =
                brute_force_min_max(&samples, &timestamps, (start, end));

            assert_eq!(
                bucket.min, expected_min,
                "bucket span {bucket_span} index {i}: min must equal the brute-force min \
                 over the same raw sample range, exactly"
            );
            assert_eq!(
                bucket.max, expected_max,
                "bucket span {bucket_span} index {i}: max must equal the brute-force max \
                 over the same raw sample range, exactly"
            );
        }
        bucket_span *= PYRAMID_FACTOR;
    }
}

#[test]
#[ignore = "docs/ROADMAP.md M3: pyramid/decimation not implemented yet"]
fn pyramid_consistency_level_k_plus_1_is_the_exact_aggregation_of_level_k() {
    const SAMPLE_COUNT: usize = 4096;

    let mut rng = Xorshift64::new(0xBADC0DE);
    let samples: Vec<f64> = (0..SAMPLE_COUNT).map(|_| rng.next_f64() * 1000.0).collect();
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).collect();

    let pyramid = build_pyramid(&samples, &timestamps);
    assert!(
        pyramid.len() >= 2,
        "a 4096-sample fixture must produce at least two pyramid levels to check consistency \
         between them"
    );

    for level in 0..pyramid.len() - 1 {
        let (children_level, parent_level) = (&pyramid[level], &pyramid[level + 1]);

        for (i, parent) in parent_level.iter().enumerate() {
            let children = &children_level[i * PYRAMID_FACTOR..(i + 1) * PYRAMID_FACTOR];

            let expected_min = children
                .iter()
                .map(|b: &Bucket| b.min)
                .fold(f64::INFINITY, f64::min);
            let expected_max = children
                .iter()
                .map(|b: &Bucket| b.max)
                .fold(f64::NEG_INFINITY, f64::max);
            let expected_first_ts = children
                .first()
                .expect("factor-8 group is non-empty")
                .first_ts;
            let expected_last_ts = children
                .last()
                .expect("factor-8 group is non-empty")
                .last_ts;
            let expected_nan_count: u64 = children.iter().map(|b| b.nan_count).sum();

            assert_eq!(
                parent.min,
                expected_min,
                "level {} bucket {i}: min must be the exact min-of-mins of its 8 level-{level} \
                 children, never a re-aggregation of resampled data",
                level + 1
            );
            assert_eq!(
                parent.max,
                expected_max,
                "level {} bucket {i}: max must be the exact max-of-maxes of its 8 level-{level} \
                 children",
                level + 1
            );
            assert_eq!(
                parent.first_ts,
                expected_first_ts,
                "level {} bucket {i}: first_ts",
                level + 1
            );
            assert_eq!(
                parent.last_ts,
                expected_last_ts,
                "level {} bucket {i}: last_ts",
                level + 1
            );
            assert_eq!(
                parent.nan_count,
                expected_nan_count,
                "level {} bucket {i}: nan_count",
                level + 1
            );
        }
    }
}

#[test]
#[ignore = "docs/ROADMAP.md M3: pyramid/decimation not implemented yet"]
fn convergence_to_raw_samples_when_the_viewport_has_fewer_samples_than_pixels() {
    const SAMPLE_COUNT: usize = 50;
    const PIXEL_COLUMNS: usize = 200;

    let mut rng = Xorshift64::new(0x5EED);
    let samples: Vec<f64> = (0..SAMPLE_COUNT).map(|_| rng.next_f64() * 10.0).collect();
    // Real time gaps between samples (not one tick apart), so this fixture
    // genuinely exercises "fewer samples than pixels" rather than a
    // dense/coincidental range.
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).map(|i| i * 1000).collect();

    let pyramid = build_pyramid(&samples, &timestamps);
    let range = (timestamps[0], timestamps[SAMPLE_COUNT - 1]);
    let columns = decimate_viewport(&pyramid, &samples, &timestamps, range, PIXEL_COLUMNS);

    assert_eq!(
        columns.len(),
        SAMPLE_COUNT,
        "a viewport with fewer raw samples ({SAMPLE_COUNT}) than pixel columns ({PIXEL_COLUMNS}) \
         must render one point per raw sample, never aggregate them away (SPEC §3.1 convergence \
         guarantee)"
    );
    for (i, bucket) in columns.iter().enumerate() {
        assert_eq!(
            bucket.min, samples[i],
            "sample {i} must render its exact raw value, not a resampled one"
        );
        assert_eq!(bucket.max, samples[i]);
        assert_eq!(bucket.first_ts, timestamps[i]);
        assert_eq!(bucket.last_ts, timestamps[i]);
    }
}

#[test]
#[ignore = "docs/ROADMAP.md M3: pyramid/decimation not implemented yet"]
fn no_aliasing_1khz_sine_at_100khz_decimated_to_500_columns_shows_a_constant_envelope() {
    const SAMPLE_RATE_HZ: f64 = 100_000.0;
    const SIGNAL_FREQ_HZ: f64 = 1_000.0;
    const SAMPLES_PER_CYCLE: usize = 100; // SAMPLE_RATE_HZ / SIGNAL_FREQ_HZ
    const AMPLITUDE: f64 = 1.0;
    const PIXEL_COLUMNS: usize = 500;
    // One full cycle per column, so every column's raw samples span exactly
    // one period and must contain a point within one sample-step of the true
    // +amplitude peak and -amplitude trough, regardless of phase.
    const SAMPLE_COUNT: usize = PIXEL_COLUMNS * SAMPLES_PER_CYCLE;
    const ENVELOPE_TOLERANCE: f64 = 1e-6;

    let samples: Vec<f64> = (0..SAMPLE_COUNT)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE_HZ;
            AMPLITUDE * (2.0 * std::f64::consts::PI * SIGNAL_FREQ_HZ * t).sin()
        })
        .collect();
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT)
        .map(|i| (i as f64 / SAMPLE_RATE_HZ * 1e9).round() as i128)
        .collect();

    let pyramid = build_pyramid(&samples, &timestamps);
    let range = (timestamps[0], timestamps[SAMPLE_COUNT - 1]);
    let columns = decimate_viewport(&pyramid, &samples, &timestamps, range, PIXEL_COLUMNS);

    assert_eq!(columns.len(), PIXEL_COLUMNS);

    // A naive point-subsample (rather than true per-column min/max) would
    // pick one of 100 samples per column and, depending on phase, could
    // regularly miss the peak/trough — a beat/moiré pattern where the
    // rendered amplitude appears to swell and shrink across columns. Real
    // min/max decimation must show the full ±amplitude band in every column.
    for (i, bucket) in columns.iter().enumerate() {
        assert!(
            bucket.max > AMPLITUDE - ENVELOPE_TOLERANCE,
            "column {i} max {} must reach the true +amplitude envelope, no aliasing beat",
            bucket.max
        );
        assert!(
            bucket.min < -AMPLITUDE + ENVELOPE_TOLERANCE,
            "column {i} min {} must reach the true -amplitude envelope, no aliasing beat",
            bucket.min
        );
    }
}
