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
//! M1/M3). Written before `glyde_core::dsp::decimation::{build_pyramid,
//! decimate_viewport}` existed (`#[ignore]`d through M1/M2 while both were
//! `todo!()`); un-ignored now that docs/ROADMAP.md M3 lands the real bodies.
//! Never loosen an assertion here to make an implementation pass — if one
//! looks wrong, that is a `blocking-decision` issue, not an edit.

use glyde_core::dsp::decimation::{
    build_pyramid, build_pyramid_streaming, decimate_viewport, extend_pyramid, Bucket,
    PYRAMID_FACTOR,
};
use glyde_core::series::SampleSource;
use glyde_core::time::TickSource;
use glyde_core::Result;
use std::ops::Range;

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

/// docs/ROADMAP.md M3's progressive checkpoint schedule calls
/// [`build_pyramid`] again on every larger prefix as a file loads (issue
/// #90). [`extend_pyramid`] exists to avoid re-aggregating from scratch
/// each time; this test is its correctness contract: at every checkpoint,
/// extending the previous checkpoint's own pyramid with only the samples
/// added since must produce the *exact* same pyramid — bucket-for-bucket,
/// level-for-level — as calling [`build_pyramid`] fresh over the whole
/// prefix. Checkpoint sizes are deliberately irregular (not multiples of
/// [`PYRAMID_FACTOR`]) so the boundary case — a previously-partial level-0
/// bucket that new samples complete or extend — is exercised at every
/// level, not just the last one.
#[test]
fn extend_pyramid_matches_a_full_rebuild_at_every_irregular_checkpoint() {
    const CHECKPOINT_SIZES: &[usize] = &[
        1, 3, 7, 8, 9, 17, 63, 64, 65, 500, 511, 512, 513, 4096, 4097, 5000, 10_000,
    ];

    let mut rng = Xorshift64::new(0x1DEA5);
    let total = *CHECKPOINT_SIZES.last().expect("non-empty");
    let samples: Vec<f64> = (0..total).map(|_| rng.next_f64() * 1000.0).collect();
    let timestamps: Vec<i128> = (0..total as i128).collect();

    let mut previous_pyramid: Vec<Vec<Bucket>> = Vec::new();
    let mut previous_len = 0usize;

    for &checkpoint_len in CHECKPOINT_SIZES {
        let extended = extend_pyramid(
            previous_pyramid.clone(),
            previous_len,
            &samples[..checkpoint_len],
            &timestamps[..checkpoint_len],
        );
        let rebuilt = build_pyramid(&samples[..checkpoint_len], &timestamps[..checkpoint_len]);

        assert_eq!(
            extended, rebuilt,
            "extend_pyramid over a prefix of {checkpoint_len} samples (grown from a previous \
             checkpoint of {previous_len}) must exactly match build_pyramid over the same \
             prefix computed from scratch"
        );

        previous_pyramid = extended;
        previous_len = checkpoint_len;
    }
}

/// A pyramid with no previous checkpoint (`previous_len == 0`) must behave
/// exactly like [`build_pyramid`] — the first checkpoint of a progressive
/// load has nothing to extend from.
#[test]
fn extend_pyramid_with_no_previous_checkpoint_matches_build_pyramid() {
    const SAMPLE_COUNT: usize = 777;

    let mut rng = Xorshift64::new(0xFACADE);
    let samples: Vec<f64> = (0..SAMPLE_COUNT).map(|_| rng.next_f64() * 10.0).collect();
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).collect();

    let extended = extend_pyramid(Vec::new(), 0, &samples, &timestamps);
    let rebuilt = build_pyramid(&samples, &timestamps);

    assert_eq!(extended, rebuilt);
}

/// A [`SampleSource`]/[`TickSource`] pair that hands its data over in
/// deliberately awkward chunk sizes, so the streaming builder is exercised
/// across chunk boundaries that do *not* line up with [`PYRAMID_FACTOR`].
/// Stands in for a spilled column (`ingest::dataset`) without needing a file,
/// exactly as `time::ticks`' own `ChunkedTicks` does for the Δt scans.
struct ChunkedSamples {
    samples: Vec<f64>,
    chunk_len: usize,
}

impl SampleSource for ChunkedSamples {
    fn sample_count(&self) -> usize {
        self.samples.len()
    }

    fn visit_sample_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[f64]) -> Result<()>,
    ) -> Result<()> {
        visit_in_chunks(&self.samples, range, self.chunk_len, visit)
    }
}

struct ChunkedTicks {
    ticks: Vec<i128>,
    chunk_len: usize,
}

impl TickSource for ChunkedTicks {
    fn tick_count(&self) -> usize {
        self.ticks.len()
    }

    fn visit_tick_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[i128]) -> Result<()>,
    ) -> Result<()> {
        visit_in_chunks(&self.ticks, range, self.chunk_len, visit)
    }
}

fn visit_in_chunks<T>(
    values: &[T],
    range: Range<usize>,
    chunk_len: usize,
    visit: &mut dyn FnMut(&[T]) -> Result<()>,
) -> Result<()> {
    let start = range.start.min(values.len());
    let end = range.end.min(values.len());
    if start >= end {
        return Ok(());
    }
    for chunk in values[start..end].chunks(chunk_len.max(1)) {
        visit(chunk)?;
    }
    Ok(())
}

/// The streaming builder's whole correctness contract (issue #88): reading a
/// column in bounded chunks — which is what lets a spilled, memory-mapped
/// column be aggregated without making every page of it resident — must
/// produce the *exact* same pyramid as [`build_pyramid`] over the same data
/// held whole in memory. Bucket-for-bucket, level-for-level, for every
/// combination of sample-chunk and tick-chunk size, including sizes that are
/// coprime with [`PYRAMID_FACTOR`] and with each other, so a bucket's contents
/// can never depend on how the read happened to be split up.
#[test]
fn build_pyramid_streaming_matches_build_pyramid_at_every_chunk_size() {
    // Deliberately awkward: primes, one, exactly PYRAMID_FACTOR, one either
    // side of it, and sizes larger than the data itself.
    const CHUNK_SIZES: &[usize] = &[1, 2, 3, 5, 7, 8, 9, 13, 64, 65, 1_000, 10_000];
    const SAMPLE_COUNT: usize = 5_000;

    let mut rng = Xorshift64::new(0xB0117);
    let samples: Vec<f64> = (0..SAMPLE_COUNT).map(|_| rng.next_f64() * 1000.0).collect();
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).map(|i| i * 7).collect();

    let expected = build_pyramid(&samples, &timestamps);
    assert!(
        expected.len() > 1,
        "the fixture must produce a multi-level pyramid, not just level 0"
    );

    for &sample_chunk in CHUNK_SIZES {
        for &tick_chunk in CHUNK_SIZES {
            let streamed = build_pyramid_streaming(
                &ChunkedSamples {
                    samples: samples.clone(),
                    chunk_len: sample_chunk,
                },
                &ChunkedTicks {
                    ticks: timestamps.clone(),
                    chunk_len: tick_chunk,
                },
            )
            .expect("an in-memory chunked source never fails");

            assert_eq!(
                streamed, expected,
                "streaming {SAMPLE_COUNT} samples {sample_chunk} at a time against ticks read \
                 {tick_chunk} at a time must produce exactly the pyramid build_pyramid produces \
                 over the same data held whole"
            );
        }
    }
}

/// NaN handling is part of the bucket definition (`nan_count` counted, the
/// min/max envelope left to the real samples), so the streaming path must
/// reproduce it exactly — including a bucket that is *entirely* NaN, whose
/// envelope stays `+∞`/`−∞` and must survive being merged across a chunk
/// boundary rather than being collapsed to a real number.
#[test]
fn build_pyramid_streaming_preserves_nan_counts_and_envelopes_across_chunk_boundaries() {
    const SAMPLE_COUNT: usize = 400;

    // Every sample of buckets 2 and 5 is NaN; elsewhere every third sample is.
    let samples: Vec<f64> = (0..SAMPLE_COUNT)
        .map(|i| {
            let bucket = i / PYRAMID_FACTOR;
            if bucket == 2 || bucket == 5 || i % 3 == 0 {
                f64::NAN
            } else {
                i as f64
            }
        })
        .collect();
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).collect();

    let expected = build_pyramid(&samples, &timestamps);

    for chunk_len in [1usize, 3, 5, 8, 17] {
        let streamed = build_pyramid_streaming(
            &ChunkedSamples {
                samples: samples.clone(),
                chunk_len,
            },
            &ChunkedTicks {
                ticks: timestamps.clone(),
                chunk_len,
            },
        )
        .expect("an in-memory chunked source never fails");

        assert_eq!(streamed.len(), expected.len());
        for (level, (streamed_level, expected_level)) in
            streamed.iter().zip(expected.iter()).enumerate()
        {
            assert_eq!(streamed_level.len(), expected_level.len());
            for (index, (got, want)) in streamed_level.iter().zip(expected_level.iter()).enumerate()
            {
                assert_eq!(
                    got.min.to_bits(),
                    want.min.to_bits(),
                    "level {level} bucket {index} min differs when streamed {chunk_len} at a time"
                );
                assert_eq!(
                    got.max.to_bits(),
                    want.max.to_bits(),
                    "level {level} bucket {index} max differs when streamed {chunk_len} at a time"
                );
                assert_eq!(got.nan_count, want.nan_count);
                assert_eq!(got.first_ts, want.first_ts);
                assert_eq!(got.last_ts, want.last_ts);
            }
        }
    }
}

/// The degenerate shapes SPEC §1.4 requires to render: an empty column has no
/// pyramid at all, and a single sample is one single-sample level-0 bucket —
/// both exactly as [`build_pyramid`] already reports them.
#[test]
fn build_pyramid_streaming_matches_build_pyramid_on_empty_and_single_sample_columns() {
    for sample_count in [0usize, 1, 2, 7, 8] {
        let samples: Vec<f64> = (0..sample_count).map(|i| i as f64).collect();
        let timestamps: Vec<i128> = (0..sample_count as i128).collect();

        let streamed = build_pyramid_streaming(
            &ChunkedSamples {
                samples: samples.clone(),
                chunk_len: 1,
            },
            &ChunkedTicks {
                ticks: timestamps.clone(),
                chunk_len: 1,
            },
        )
        .expect("an in-memory chunked source never fails");

        assert_eq!(
            streamed,
            build_pyramid(&samples, &timestamps),
            "a {sample_count}-sample column must stream to the same pyramid it builds to"
        );
    }
}

/// A slice is itself a [`SampleSource`]/[`TickSource`], so the streaming entry
/// point is usable on the small-file path too — and must agree there as well,
/// which is what makes it safe for `ingest` to route *any* dataset through it.
#[test]
fn build_pyramid_streaming_over_plain_slices_matches_build_pyramid() {
    const SAMPLE_COUNT: usize = 3_000;

    let mut rng = Xorshift64::new(0x5115);
    let samples: Vec<f64> = (0..SAMPLE_COUNT).map(|_| rng.next_f64() * 42.0).collect();
    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).map(|i| i * 3 + 100).collect();

    let streamed = build_pyramid_streaming(samples.as_slice(), timestamps.as_slice())
        .expect("an in-memory slice source never fails");

    assert_eq!(streamed, build_pyramid(&samples, &timestamps));
}
