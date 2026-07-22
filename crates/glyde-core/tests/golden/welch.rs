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

//! Welch PSD golden tests (docs/QUALITY.md §2 Welch PSD, docs/ROADMAP.md
//! M1/M5). Every test here is `#[ignore]`d because
//! `glyde_core::dsp::welch::{welch, welch_segmented}` are `todo!()` stubs
//! (docs/ROADMAP.md M5); `cargo test -- --ignored --list` is the M1
//! maintainer proof that the full golden-test set exists. Un-ignore each
//! test only once the implementation makes it pass — never loosen an
//! assertion to make that happen.
//!
//! Expected values below are derived from the textbook DFT definition
//! (Golden Rule 4), not copied from any implementation: for a real signal
//! made only of exact-bin frequency components (no leakage), the rectangular
//! window's DFT of a length-`N` sinusoid `A * sin(2*pi*k*n/N)` has magnitude
//! `A*N/2` at bin `k` and `0` elsewhere, so the one-sided PSD SPEC §3.2
//! defines (`2 * |X[k]|^2 / (fs * N)` for every bin except DC/Nyquist, which
//! are not doubled) evaluates to `A^2 * N / (2 * fs)` at that bin.

use glyde_core::dsp::detrend::Detrend;
use glyde_core::dsp::welch::{welch, welch_segmented, WelchConfig};
use glyde_core::dsp::window::Window;
use std::f64::consts::PI;

/// A tiny deterministic PRNG (xorshift64*), matching
/// `tests/golden/decimation.rs`, so "white noise" fixtures are reproducible
/// without adding a `rand` dependency to the workspace.
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
        let bits = self.next_u64() >> 11;
        (bits as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

fn sinusoid(bin: usize, segment_len: usize, amplitude: f64) -> Vec<f64> {
    (0..segment_len)
        .map(|n| amplitude * (2.0 * PI * bin as f64 * n as f64 / segment_len as f64).sin())
        .collect()
}

#[test]
#[ignore = "docs/ROADMAP.md M5: Welch not implemented yet"]
fn sinusoid_peak_lands_at_the_exact_bin_with_correct_amplitude() {
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 1024;
    const BIN_INDEX: usize = 40; // arbitrary, not DC (0) or Nyquist (512)
    const AMPLITUDE: f64 = 1.0;

    let samples = sinusoid(BIN_INDEX, SEGMENT_LEN, AMPLITUDE);
    let config = WelchConfig {
        window: Window::Rectangular,
        segment_len: SEGMENT_LEN,
        overlap: 0.0,
        detrend: Detrend::None,
    };
    let psd = welch(&samples, SAMPLE_RATE_HZ, &config);

    let (peak_index, &peak_power) = psd
        .power
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .expect("psd has at least one bin");
    assert_eq!(
        peak_index, BIN_INDEX,
        "the sinusoid's power must peak exactly at its own frequency bin, never a neighbor"
    );

    let expected_peak = AMPLITUDE.powi(2) * SEGMENT_LEN as f64 / (2.0 * SAMPLE_RATE_HZ);
    let relative_err = (peak_power - expected_peak).abs() / expected_peak;
    assert!(
        relative_err < 1e-6,
        "peak power {peak_power} must match the textbook periodogram value {expected_peak} \
         (relative err {relative_err})"
    );

    let recovered_amplitude = (2.0 * peak_power * psd.delta_f).sqrt();
    assert!(
        (recovered_amplitude - AMPLITUDE).abs() < 1e-3,
        "recovered amplitude {recovered_amplitude} must match the injected amplitude \
         {AMPLITUDE} within tolerance"
    );
}

#[test]
#[ignore = "docs/ROADMAP.md M5: Welch not implemented yet"]
fn parseval_power_conservation_with_a_hann_window_matches_signal_variance() {
    // Large N so the sampling error of the variance estimate (~sqrt(2*ENBW/N)
    // for a Hann-windowed estimate) is well under the assertion tolerance —
    // this is a statistical invariant, not an exact identity, so the margin
    // must comfortably absorb one fixed-seed realization's sampling noise.
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 65536;
    const AMPLITUDE: f64 = 2.0; // samples uniform in [-AMPLITUDE, AMPLITUDE)
    const RELATIVE_TOLERANCE: f64 = 0.05;

    let mut rng = Xorshift64::new(0x51DE5EED);
    let samples: Vec<f64> = (0..SEGMENT_LEN)
        .map(|_| rng.next_f64() * AMPLITUDE)
        .collect();

    let config = WelchConfig {
        window: Window::Hann,
        segment_len: SEGMENT_LEN,
        overlap: 0.0,
        detrend: Detrend::None,
    };
    let psd = welch(&samples, SAMPLE_RATE_HZ, &config);

    let total_power: f64 = psd.power.iter().sum::<f64>() * psd.delta_f;
    let expected_variance = AMPLITUDE.powi(2) / 3.0; // Var[Uniform(-A, A)]
    let relative_err = (total_power - expected_variance).abs() / expected_variance;

    assert!(
        relative_err < RELATIVE_TOLERANCE,
        "sum(PSD * delta_f) = {total_power} must recover the signal's variance {expected_variance} \
         within {RELATIVE_TOLERANCE} relative tolerance (relative err {relative_err}); a \
         window-normalization bug shows up here as a systematic, seed-independent bias"
    );
}

#[test]
#[ignore = "docs/ROADMAP.md M5: Welch not implemented yet"]
fn one_sided_scaling_doubles_every_bin_except_dc_and_nyquist() {
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 1024;
    const NYQUIST_BIN: usize = SEGMENT_LEN / 2;
    const MID_BIN: usize = 40;
    const DC_LEVEL: f64 = 3.0;
    const NYQUIST_AMPLITUDE: f64 = 0.5;
    const MID_AMPLITUDE: f64 = 1.0;

    // DC, Nyquist (the n-alternating +/-1 term), and a mid-band sinusoid are
    // mutually orthogonal over exact DFT bins, so each contributes to
    // exactly one bin with no leakage between them (Golden Rule 4: matches
    // the textbook DFT definition, not an implementation-derived tolerance).
    let samples: Vec<f64> = (0..SEGMENT_LEN)
        .map(|n| {
            let alternating = if n % 2 == 0 { 1.0 } else { -1.0 };
            DC_LEVEL
                + NYQUIST_AMPLITUDE * alternating
                + MID_AMPLITUDE * (2.0 * PI * MID_BIN as f64 * n as f64 / SEGMENT_LEN as f64).sin()
        })
        .collect();

    let config = WelchConfig {
        window: Window::Rectangular,
        segment_len: SEGMENT_LEN,
        overlap: 0.0,
        detrend: Detrend::None,
    };
    let psd = welch(&samples, SAMPLE_RATE_HZ, &config);

    let expected_dc = DC_LEVEL.powi(2) * SEGMENT_LEN as f64 / SAMPLE_RATE_HZ;
    let expected_nyquist = NYQUIST_AMPLITUDE.powi(2) * SEGMENT_LEN as f64 / SAMPLE_RATE_HZ;
    let expected_mid = MID_AMPLITUDE.powi(2) * SEGMENT_LEN as f64 / (2.0 * SAMPLE_RATE_HZ);

    let assert_close = |name: &str, actual: f64, expected: f64| {
        let relative_err = (actual - expected).abs() / expected;
        assert!(
            relative_err < 1e-6,
            "{name} bin power {actual} must equal {expected} (relative err {relative_err})"
        );
    };

    assert_close("DC (must not be doubled)", psd.power[0], expected_dc);
    assert_close(
        "Nyquist (must not be doubled)",
        psd.power[NYQUIST_BIN],
        expected_nyquist,
    );
    assert_close(
        "mid-band (must be doubled)",
        psd.power[MID_BIN],
        expected_mid,
    );
}

#[test]
#[ignore = "docs/ROADMAP.md M5: Welch not implemented yet"]
fn window_normalization_reports_the_same_total_power_across_windows() {
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 8192;
    const AMPLITUDE: f64 = 1.5;
    const RELATIVE_TOLERANCE: f64 = 0.05;

    let mut rng = Xorshift64::new(0xB16B00B5);
    let samples: Vec<f64> = (0..SEGMENT_LEN)
        .map(|_| rng.next_f64() * AMPLITUDE)
        .collect();

    let total_power_for = |window: Window| {
        let config = WelchConfig {
            window,
            segment_len: SEGMENT_LEN,
            overlap: 0.0,
            detrend: Detrend::None,
        };
        let psd = welch(&samples, SAMPLE_RATE_HZ, &config);
        psd.power.iter().sum::<f64>() * psd.delta_f
    };

    let rectangular = total_power_for(Window::Rectangular);
    let hann = total_power_for(Window::Hann);
    let hamming = total_power_for(Window::Hamming);

    for (name, total) in [("Hann", hann), ("Hamming", hamming)] {
        let relative_err = (total - rectangular).abs() / rectangular;
        assert!(
            relative_err < RELATIVE_TOLERANCE,
            "{name} total power {total} must match rectangular's {rectangular} within \
             {RELATIVE_TOLERANCE} relative tolerance (relative err {relative_err}) — the same \
             signal must not appear to carry more or less power just because a different \
             window was picked"
        );
    }
}

#[test]
#[ignore = "docs/ROADMAP.md M5: Welch not implemented yet"]
fn detrend_removes_the_leakage_skirt_from_a_large_dc_offset() {
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 1024;
    const DC_OFFSET: f64 = 1000.0;
    const NOISE_AMPLITUDE: f64 = 0.01;
    // Comfortably above the true noise floor (~1e-7 for this fixture) but
    // orders of magnitude below what a Hann-windowed 1000.0 DC offset leaks
    // into nearby bins if left un-detrended (~1e2-1e3, from Hann's ~-31 dB
    // sidelobes on a `DC_LEVEL^2 * N / fs` main lobe).
    const LEAKAGE_THRESHOLD: f64 = 1.0;

    let mut rng = Xorshift64::new(0xDE7DE5D);
    let samples: Vec<f64> = (0..SEGMENT_LEN)
        .map(|_| DC_OFFSET + rng.next_f64() * NOISE_AMPLITUDE)
        .collect();

    let config = WelchConfig {
        window: Window::Hann,
        segment_len: SEGMENT_LEN,
        overlap: 0.0,
        detrend: Detrend::Constant,
    };
    let psd = welch(&samples, SAMPLE_RATE_HZ, &config);

    for bin in 1..=20 {
        assert!(
            psd.power[bin] < LEAKAGE_THRESHOLD,
            "bin {bin} power {} must stay below the leakage threshold {LEAKAGE_THRESHOLD} once \
             the {DC_OFFSET} DC offset is detrended away; a bug that skips detrending would leak \
             the Hann window's sidelobes of the DC term into this bin",
            psd.power[bin]
        );
    }
}

#[test]
#[ignore = "docs/ROADMAP.md M5: Welch not implemented yet"]
fn two_tones_resolve_at_their_exact_frequencies_with_correct_relative_levels() {
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 1024;
    const BIN_A: usize = 30;
    const BIN_B: usize = 90;
    const AMPLITUDE_A: f64 = 1.0;
    const AMPLITUDE_B: f64 = 0.25;

    let samples: Vec<f64> = (0..SEGMENT_LEN)
        .map(|n| {
            let t = n as f64 / SEGMENT_LEN as f64;
            AMPLITUDE_A * (2.0 * PI * BIN_A as f64 * t).sin()
                + AMPLITUDE_B * (2.0 * PI * BIN_B as f64 * t).sin()
        })
        .collect();

    let config = WelchConfig {
        window: Window::Rectangular,
        segment_len: SEGMENT_LEN,
        overlap: 0.0,
        detrend: Detrend::None,
    };
    let psd = welch(&samples, SAMPLE_RATE_HZ, &config);

    let expected_a = AMPLITUDE_A.powi(2) * SEGMENT_LEN as f64 / (2.0 * SAMPLE_RATE_HZ);
    let expected_b = AMPLITUDE_B.powi(2) * SEGMENT_LEN as f64 / (2.0 * SAMPLE_RATE_HZ);

    for (bin, expected, label) in [(BIN_A, expected_a, "A"), (BIN_B, expected_b, "B")] {
        let relative_err = (psd.power[bin] - expected).abs() / expected;
        assert!(
            relative_err < 1e-6,
            "tone {label} at bin {bin}: power {} must equal {expected} (relative err \
             {relative_err})",
            psd.power[bin]
        );
    }

    let expected_ratio = expected_a / expected_b;
    let actual_ratio = psd.power[BIN_A] / psd.power[BIN_B];
    assert!(
        (actual_ratio - expected_ratio).abs() / expected_ratio < 1e-6,
        "power ratio between the two tones ({actual_ratio}) must match the amplitude-squared \
         ratio ({expected_ratio})"
    );
}

#[test]
#[ignore = "docs/ROADMAP.md M5: Welch not implemented yet"]
fn segmented_welch_averages_per_segment_periodograms_and_never_crosses_a_gap() {
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 1024;
    const BIN_INDEX: usize = 40;
    const AMPLITUDE: f64 = 1.0;
    // A huge, alternating offset per physical segment. Any analysis window
    // that ever spans two segments (e.g. a naive concatenation Welch'd with
    // 50% overlap, which slides a window across the SEGMENT_LEN boundary)
    // sees a step of 2 * DC_OFFSET and produces broadband high-frequency
    // energy that a correct segmented implementation — which never lets a
    // window cross a physical gap — must not show.
    const DC_OFFSET: f64 = 500.0;
    // Near Nyquist: far from BIN_INDEX and from any energy a clean detrended
    // sinusoid would carry, so it is a clean broadband-leakage detector.
    const HIGH_FREQ_BIN: usize = SEGMENT_LEN / 2 - 5;

    let make_segment = |sign: f64| -> Vec<f64> {
        sinusoid(BIN_INDEX, SEGMENT_LEN, AMPLITUDE)
            .into_iter()
            .map(|v| v + sign * DC_OFFSET)
            .collect()
    };
    let seg0 = make_segment(1.0);
    let seg1 = make_segment(-1.0);
    let seg2 = make_segment(1.0);

    let config = WelchConfig {
        window: Window::Hann,
        segment_len: SEGMENT_LEN,
        overlap: 0.5,
        detrend: Detrend::Constant,
    };

    let segmented = welch_segmented(&[&seg0, &seg1, &seg2], SAMPLE_RATE_HZ, &config);

    // Reference: each segment Welch'd in isolation (so no window can ever
    // cross a boundary in this reference either), then a plain average since
    // all three segments share the same length (length-weighted == plain
    // average here).
    let individual: Vec<_> = [&seg0, &seg1, &seg2]
        .iter()
        .map(|seg| welch(seg, SAMPLE_RATE_HZ, &config))
        .collect();
    let expected_power: Vec<f64> = (0..individual[0].power.len())
        .map(|bin| {
            individual.iter().map(|psd| psd.power[bin]).sum::<f64>() / individual.len() as f64
        })
        .collect();

    for (bin, (&actual, &expected)) in segmented
        .power
        .iter()
        .zip(expected_power.iter())
        .enumerate()
    {
        let relative_err = if expected.abs() > 1e-12 {
            (actual - expected).abs() / expected.abs()
        } else {
            (actual - expected).abs()
        };
        assert!(
            relative_err < 1e-6,
            "bin {bin}: segmented power {actual} must equal the length-weighted average of the \
             per-segment periodograms {expected} (relative err {relative_err})"
        );
    }

    // Now prove the "never crosses a gap" guarantee by construction: a naive
    // concatenation, Welch'd as if it were one uniform run, DOES cross the
    // boundaries (its 50%-overlapping windows straddle every SEGMENT_LEN
    // step) and must show far more high-frequency energy than the correctly
    // segmented result.
    let mut concatenated = seg0.clone();
    concatenated.extend_from_slice(&seg1);
    concatenated.extend_from_slice(&seg2);
    let naive = welch(&concatenated, SAMPLE_RATE_HZ, &config);

    assert!(
        naive.power[HIGH_FREQ_BIN] > 100.0 * segmented.power[HIGH_FREQ_BIN].max(1e-12),
        "naive concatenation leaks {} into bin {HIGH_FREQ_BIN} from crossing the segment \
         boundaries, but the correctly segmented result only has {} there — a segmented Welch \
         that (incorrectly) let a window span a gap would not show this gap between the two",
        naive.power[HIGH_FREQ_BIN],
        segmented.power[HIGH_FREQ_BIN]
    );
}

#[test]
#[ignore = "docs/ROADMAP.md M5: Welch not implemented yet"]
fn raw_data_guarantee_zoomed_in_and_zoomed_out_selections_are_bit_identical() {
    // `welch`'s signature only accepts a raw `&[f64]` — there is no
    // pyramid/bucket type it could read instead (SPEC §3.2: "PSD is always
    // computed on raw samples ... never on decimated/pyramid data"). This
    // test is the runtime companion to that type-level guarantee: the exact
    // same values, reached through two independent code paths (a slice into
    // a large "whole file" buffer vs. a freshly copied "just this selection"
    // buffer, standing in for the zoomed-out and zoomed-in cases), must
    // produce bit-identical results.
    const SAMPLE_RATE_HZ: f64 = 1000.0;
    const SEGMENT_LEN: usize = 4096;
    const FILE_SAMPLE_COUNT: usize = 200_000;
    const SELECTION_START: usize = 50_000;

    let mut rng = Xorshift64::new(0xFEEDFACE);
    let whole_file: Vec<f64> = (0..FILE_SAMPLE_COUNT).map(|_| rng.next_f64()).collect();
    let zoomed_out_selection = &whole_file[SELECTION_START..SELECTION_START + SEGMENT_LEN];
    // An independently-allocated copy of the identical values, as if the UI
    // had zoomed all the way in and re-read just this range from raw storage.
    let zoomed_in_selection: Vec<f64> = zoomed_out_selection.to_vec();

    let config = WelchConfig {
        window: Window::Hann,
        segment_len: SEGMENT_LEN,
        overlap: 0.5,
        detrend: Detrend::Constant,
    };

    let from_zoomed_out = welch(zoomed_out_selection, SAMPLE_RATE_HZ, &config);
    let from_zoomed_in = welch(&zoomed_in_selection, SAMPLE_RATE_HZ, &config);

    assert_eq!(
        from_zoomed_out.freqs, from_zoomed_in.freqs,
        "identical raw selections must produce identical bin frequencies"
    );
    assert_eq!(
        from_zoomed_out.power, from_zoomed_in.power,
        "identical raw selections must produce bit-identical power, regardless of whether the \
         data was reached via a full-file slice or an independently copied buffer — PSD must \
         never depend on anything but the raw samples handed to it"
    );
}
