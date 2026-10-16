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

//! Streaming Welch golden tests (docs/ROADMAP.md M5 "Streaming Welch", SPEC
//! §3.2: "compute Welch streaming (accumulate segment periodograms while
//! reading), never by loading everything"). Written before
//! `welch_source`/`welch_segmented_source` existed.
//!
//! The reference here is the already-golden in-memory `welch` /
//! `welch_segmented` (`golden/welch.rs`): a streaming estimate is the *same*
//! estimate, read differently, so it must match bit for bit — not within a
//! tolerance — however the source happens to chunk its samples. The one
//! deliberate difference is a missing (non-finite) sample: the textbook
//! estimate of a series with a hole in it is the average of the periodograms
//! of the windows that do not touch the hole, so no analysis window ever
//! spans one (the same rule SPEC §3.3 sets for a timestamp gap).

use std::ops::Range;

use glyde_core::dsp::detrend::Detrend;
use glyde_core::dsp::welch::WelchConfig;
use glyde_core::dsp::welch::{welch, welch_segmented, welch_segmented_source, welch_source};
use glyde_core::dsp::window::Window;
use glyde_core::series::SampleSource;
use std::f64::consts::PI;

/// A source that hands its samples over in deliberately awkward chunks — a
/// prime length that never lines up with a window or a step — so a
/// chunk-boundary bug in the accumulator cannot hide.
struct OddChunks {
    samples: Vec<f64>,
    chunk_len: usize,
}

impl SampleSource for OddChunks {
    fn sample_count(&self) -> usize {
        self.samples.len()
    }

    fn visit_sample_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[f64]) -> glyde_core::Result<()>,
    ) -> glyde_core::Result<()> {
        let end = range.end.min(self.samples.len());
        let start = range.start.min(end);
        for chunk in self.samples[start..end].chunks(self.chunk_len) {
            visit(chunk)?;
        }
        Ok(())
    }
}

fn noisy_tone(len: usize, seed: u64) -> Vec<f64> {
    let mut state = seed.max(1);
    (0..len)
        .map(|n| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let noise = ((state >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0;
            3.0 * (2.0 * PI * 0.0625 * n as f64).sin() + noise + 7.0
        })
        .collect()
}

fn hann_config(segment_len: usize) -> WelchConfig {
    WelchConfig {
        window: Window::Hann,
        segment_len,
        overlap: 0.5,
        detrend: Detrend::Constant,
    }
}

fn never_cancel(_done: usize) -> bool {
    true
}

#[test]
fn streaming_welch_over_a_chunked_source_is_bit_identical_to_the_in_memory_estimate() {
    const SAMPLE_RATE_HZ: f64 = 250.0;
    let samples = noisy_tone(100_003, 0xA11CE);
    let config = hann_config(1024);
    let source = OddChunks {
        samples: samples.clone(),
        chunk_len: 37,
    };

    let in_memory = welch(&samples, SAMPLE_RATE_HZ, &config);
    let streamed = welch_source(
        &source,
        0..samples.len(),
        SAMPLE_RATE_HZ,
        &config,
        &mut never_cancel,
    )
    .expect("an in-memory source cannot fail")
    .expect("never cancelled");

    assert_eq!(streamed.freqs, in_memory.freqs);
    assert_eq!(
        streamed.power, in_memory.power,
        "streaming must accumulate exactly the same periodograms in the same order"
    );
    assert_eq!(streamed.segment_count, in_memory.segment_count);
    assert_eq!(streamed.delta_f, in_memory.delta_f);
}

#[test]
fn streaming_welch_over_a_sub_range_matches_welch_on_that_slice_alone() {
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    let samples = noisy_tone(50_000, 0xBEEF);
    let config = hann_config(2048);
    let source = OddChunks {
        samples: samples.clone(),
        chunk_len: 4099,
    };
    let selection = 12_345..41_000;

    let expected = welch(&samples[selection.clone()], SAMPLE_RATE_HZ, &config);
    let streamed = welch_source(
        &source,
        selection,
        SAMPLE_RATE_HZ,
        &config,
        &mut never_cancel,
    )
    .unwrap()
    .unwrap();

    assert_eq!(streamed.power, expected.power);
    assert_eq!(streamed.segment_count, expected.segment_count);
}

#[test]
fn streaming_welch_shorter_than_one_window_uses_the_whole_selection_as_one_window() {
    const SAMPLE_RATE_HZ: f64 = 100.0;
    let samples = noisy_tone(300, 7);
    let config = hann_config(1024);
    let source = OddChunks {
        samples: samples.clone(),
        chunk_len: 11,
    };

    let expected = welch(&samples, SAMPLE_RATE_HZ, &config);
    let streamed = welch_source(&source, 0..300, SAMPLE_RATE_HZ, &config, &mut never_cancel)
        .unwrap()
        .unwrap();

    assert_eq!(streamed.freqs.len(), 300 / 2 + 1);
    assert_eq!(streamed.power, expected.power);
    assert_eq!(streamed.segment_count, 1);
}

#[test]
fn streaming_segmented_welch_matches_the_in_memory_length_weighted_average() {
    const SAMPLE_RATE_HZ: f64 = 500.0;
    let samples = noisy_tone(30_000, 0x5E6);
    let config = hann_config(1024);
    let source = OddChunks {
        samples: samples.clone(),
        chunk_len: 997,
    };
    // Three physical segments of different lengths, plus one shorter than a
    // window, which both paths must exclude identically.
    let ranges = [0..9_000, 9_000..9_500, 10_000..21_000, 22_000..30_000];
    let slices: Vec<&[f64]> = ranges.iter().map(|r| &samples[r.clone()]).collect();

    let expected = welch_segmented(&slices, SAMPLE_RATE_HZ, &config);
    let streamed =
        welch_segmented_source(&source, &ranges, SAMPLE_RATE_HZ, &config, &mut never_cancel)
            .unwrap()
            .unwrap();

    assert_eq!(streamed.freqs, expected.freqs);
    assert_eq!(streamed.power, expected.power);
    assert_eq!(streamed.segment_count, expected.segment_count);
}

#[test]
fn no_analysis_window_ever_spans_a_missing_sample() {
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 1024;
    const HOLE: usize = 1500;
    let mut samples = noisy_tone(3 * SEGMENT_LEN, 0xD00D);
    samples[HOLE] = f64::NAN;
    let config = WelchConfig {
        window: Window::Rectangular,
        segment_len: SEGMENT_LEN,
        overlap: 0.0,
        detrend: Detrend::Constant,
    };
    let source = OddChunks {
        samples: samples.clone(),
        chunk_len: 101,
    };

    let streamed = welch_source(
        &source,
        0..samples.len(),
        SAMPLE_RATE_HZ,
        &config,
        &mut never_cancel,
    )
    .unwrap()
    .unwrap();

    // Textbook: the windows that fit without touching the hole are
    // [0, 1024) and, restarting right after it, [1501, 2525); the next would
    // need samples up to 3549 and the series ends at 3072. The estimate is
    // the plain average of those two periodograms.
    let first = welch(&samples[0..SEGMENT_LEN], SAMPLE_RATE_HZ, &config);
    let second = welch(
        &samples[HOLE + 1..HOLE + 1 + SEGMENT_LEN],
        SAMPLE_RATE_HZ,
        &config,
    );

    assert_eq!(streamed.segment_count, 2);
    assert_eq!(streamed.non_finite_count, 1);
    for (bin, &actual) in streamed.power.iter().enumerate() {
        let expected = (first.power[bin] + second.power[bin]) / 2.0;
        assert!(
            actual.is_finite(),
            "bin {bin}: a missing sample must never leak into the estimate as NaN"
        );
        assert!(
            (actual - expected).abs() <= 1e-12 * expected.abs().max(1e-300),
            "bin {bin}: {actual} != {expected}, the average of the two windows that avoid the hole"
        );
    }
}

#[test]
fn a_cancelled_streaming_estimate_returns_nothing_rather_than_a_partial_spectrum() {
    let samples = noisy_tone(200_000, 3);
    let source = OddChunks {
        samples,
        chunk_len: 10_000,
    };
    let mut calls = 0usize;
    let result = welch_source(
        &source,
        0..200_000,
        1000.0,
        &hann_config(1024),
        &mut |_done| {
            calls += 1;
            calls < 3
        },
    )
    .unwrap();

    assert!(
        result.is_none(),
        "a cancelled run must not be mistaken for a PSD of the samples it happened to reach"
    );
}
