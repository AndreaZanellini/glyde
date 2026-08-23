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

//! Welch's averaged modified periodogram (docs/SPEC.md §3.2,
//! docs/ARCHITECTURE.md dsp/welch.rs).
//!
//! Locked by the golden tests in `crates/glyde-core/tests/golden/welch.rs`
//! (docs/QUALITY.md §2 Welch PSD). Never widen a golden test's tolerance or
//! change its expectations to make an implementation pass — if one looks
//! wrong, that is a `blocking-decision` issue, not an edit.
//!
//! `welch`/`welch_segmented` take only raw sample slices — there is no
//! pyramid/bucket type anywhere in this module's signatures. That is a
//! deliberate API-level enforcement of SPEC §3.2's "PSD is always computed on
//! raw samples ... never on decimated/pyramid data": the type system makes it
//! impossible to hand this module anything else.
//!
//! Still not covered here (docs/ROADMAP.md M5, separate items): streaming
//! accumulation for a selection too large for the memory budget, and the
//! `Irregular`-sampling product behavior (PSD disabled + offered sub-range).
//! Both `welch` and `welch_segmented` load their inputs fully into the FFT
//! buffer, matching every current golden test's fixture sizes.

use super::detrend::{self, Detrend};
use super::window::{self, Window};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// Smallest segment length the software's default ever picks (SPEC §3.2).
pub const MIN_SEGMENT_LEN: usize = 256;
/// Largest segment length the software's default ever picks (SPEC §3.2).
pub const MAX_SEGMENT_LEN: usize = 65536;
/// Default segment overlap fraction (SPEC §3.2: "50% overlap").
pub const DEFAULT_OVERLAP: f64 = 0.5;

/// The (at most three) user-facing controls behind the PSD settings
/// affordance (SPEC §3.2), plus the detrend method (documented, not exposed
/// as a fourth control).
#[derive(Debug, Clone)]
pub struct WelchConfig {
    pub window: Window,
    pub segment_len: usize,
    /// Fraction of a segment length that consecutive segments overlap by,
    /// e.g. `0.5` for 50%.
    pub overlap: f64,
    pub detrend: Detrend,
}

/// A one-sided power spectral density estimate (SPEC §3.2: units²/Hz).
#[derive(Debug, Clone)]
pub struct Psd {
    /// Bin center frequencies in Hz; `freqs.len() == segment_len / 2 + 1`.
    pub freqs: Vec<f64>,
    /// One-sided power at each bin, units²/Hz. DC and Nyquist are not
    /// doubled; every other bin is (SPEC §3.2).
    pub power: Vec<f64>,
    /// Frequency resolution `sample_rate_hz / segment_len`.
    pub delta_f: f64,
    /// Number of segments averaged into this estimate.
    pub segment_count: usize,
}

/// The software's default segment length: the largest power of two `<= N /
/// 8`, clamped to `[MIN_SEGMENT_LEN, MAX_SEGMENT_LEN]` (SPEC §3.2).
pub fn default_segment_length(sample_count: usize) -> usize {
    let target = sample_count / 8;
    let largest_pow2_leq_target = if target == 0 {
        1
    } else {
        1usize << (usize::BITS - 1 - target.leading_zeros())
    };
    largest_pow2_leq_target.clamp(MIN_SEGMENT_LEN, MAX_SEGMENT_LEN)
}

/// Welch's method on a single contiguous, uniformly-sampled run of
/// `samples` (SPEC §3.2, §3.3 `Uniform`). Never reads a pyramid/index — only
/// ever the raw samples passed in.
///
/// Splits `samples` into overlapping sub-segments of `config.segment_len`
/// (step `segment_len * (1 - overlap)`), detrends and windows each, and
/// averages their periodograms. If fewer samples are available than
/// `config.segment_len`, the whole slice is used as one shorter segment
/// (matching the length actually handed in, so `freqs`/`delta_f` describe
/// what was really computed rather than a padded fiction).
pub fn welch(samples: &[f64], sample_rate_hz: f64, config: &WelchConfig) -> Psd {
    let effective_len = config.segment_len.min(samples.len());
    if effective_len == 0 {
        return Psd {
            freqs: Vec::new(),
            power: Vec::new(),
            delta_f: sample_rate_hz / config.segment_len.max(1) as f64,
            segment_count: 0,
        };
    }

    let step = sub_segment_step(effective_len, config.overlap);
    let window_coeffs = window::coefficients(config.window, effective_len);
    let window_sum_sq: f64 = window_coeffs.iter().map(|w| w * w).sum();

    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(effective_len);

    let bin_count = effective_len / 2 + 1;
    let mut accumulated = vec![0.0; bin_count];
    let mut segment_count = 0usize;

    let mut start = 0usize;
    while start + effective_len <= samples.len() {
        let mut buffer: Vec<f64> = samples[start..start + effective_len].to_vec();
        detrend::apply(&mut buffer, config.detrend);
        for (sample, &w) in buffer.iter_mut().zip(window_coeffs.iter()) {
            *sample *= w;
        }

        let mut spectrum: Vec<Complex<f64>> =
            buffer.iter().map(|&x| Complex::new(x, 0.0)).collect();
        fft.process(&mut spectrum);

        for (bin, power) in accumulated.iter_mut().enumerate() {
            *power += spectrum[bin].norm_sqr();
        }

        segment_count += 1;
        start += step;
        if step == 0 {
            break;
        }
    }

    let scale_denominator = sample_rate_hz * window_sum_sq;
    let nyquist_bin = if effective_len % 2 == 0 {
        Some(effective_len / 2)
    } else {
        None
    };
    let power: Vec<f64> = accumulated
        .into_iter()
        .enumerate()
        .map(|(bin, sum)| {
            let mean_power = sum / segment_count.max(1) as f64;
            let one_sided_factor = if bin == 0 || Some(bin) == nyquist_bin {
                1.0
            } else {
                2.0
            };
            one_sided_factor * mean_power / scale_denominator
        })
        .collect();

    let delta_f = sample_rate_hz / effective_len as f64;
    let freqs = (0..bin_count).map(|bin| bin as f64 * delta_f).collect();

    Psd {
        freqs,
        power,
        delta_f,
        segment_count,
    }
}

/// The sample step between consecutive analysis windows for a given overlap
/// fraction (e.g. `0.5` for 50% overlap). Always at least `1`, so a
/// pathological `overlap >= 1.0` cannot stall the sliding window forever.
fn sub_segment_step(segment_len: usize, overlap: f64) -> usize {
    let overlap_samples = (segment_len as f64 * overlap.clamp(0.0, 1.0)).round() as usize;
    segment_len.saturating_sub(overlap_samples).max(1)
}

/// Welch's method across multiple contiguous segments separated by gaps
/// (SPEC §3.3 `SegmentedUniform`). No analysis window ever crosses a
/// segment boundary — each element of `segments` is Welch'd independently
/// (so `welch`'s own per-segment sliding window never sees samples from two
/// different physical segments) and the results are averaged, weighted by
/// segment length. Segments shorter than `config.segment_len` are excluded
/// from the average (the caller is responsible for reporting them, SPEC
/// §3.3).
pub fn welch_segmented(segments: &[&[f64]], sample_rate_hz: f64, config: &WelchConfig) -> Psd {
    let qualifying: Vec<&[f64]> = segments
        .iter()
        .copied()
        .filter(|seg| seg.len() >= config.segment_len)
        .collect();

    let Some(first) = qualifying.first() else {
        return Psd {
            freqs: Vec::new(),
            power: Vec::new(),
            delta_f: sample_rate_hz / config.segment_len.max(1) as f64,
            segment_count: 0,
        };
    };

    let reference = welch(first, sample_rate_hz, config);
    let mut weighted_power = vec![0.0; reference.power.len()];
    let mut total_weight = 0.0f64;
    let mut total_segment_count = 0usize;

    for seg in &qualifying {
        let psd = welch(seg, sample_rate_hz, config);
        let weight = seg.len() as f64;
        for (acc, &p) in weighted_power.iter_mut().zip(psd.power.iter()) {
            *acc += weight * p;
        }
        total_weight += weight;
        total_segment_count += psd.segment_count;
    }

    let power = weighted_power
        .into_iter()
        .map(|p| p / total_weight)
        .collect();

    Psd {
        freqs: reference.freqs,
        power,
        delta_f: reference.delta_f,
        segment_count: total_segment_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_segment_length_is_the_largest_power_of_two_leq_n_over_8_clamped() {
        // N/8 below the floor clamps up to MIN_SEGMENT_LEN.
        assert_eq!(default_segment_length(0), MIN_SEGMENT_LEN);
        assert_eq!(default_segment_length(1000), MIN_SEGMENT_LEN);
        // N/8 = 256 exactly.
        assert_eq!(default_segment_length(2048), 256);
        // N/8 = 300 -> largest power of two <= 300 is 256.
        assert_eq!(default_segment_length(2400), 256);
        // N/8 = 1023 -> largest power of two <= 1023 is 512.
        assert_eq!(default_segment_length(8184), 512);
        // N/8 far above MAX_SEGMENT_LEN clamps down.
        assert_eq!(default_segment_length(100_000_000), MAX_SEGMENT_LEN);
    }

    #[test]
    fn welch_segmented_excludes_segments_shorter_than_the_configured_segment_length() {
        const SAMPLE_RATE_HZ: f64 = 1000.0;
        const SEGMENT_LEN: usize = 64;

        let config = WelchConfig {
            window: Window::Rectangular,
            segment_len: SEGMENT_LEN,
            overlap: 0.0,
            detrend: Detrend::None,
        };

        let long_segment: Vec<f64> = (0..SEGMENT_LEN).map(|n| n as f64).collect();
        let too_short: Vec<f64> = vec![1.0; SEGMENT_LEN - 1];

        let with_short_segment =
            welch_segmented(&[&long_segment, &too_short], SAMPLE_RATE_HZ, &config);
        let without_short_segment = welch_segmented(&[&long_segment], SAMPLE_RATE_HZ, &config);

        assert_eq!(with_short_segment.power, without_short_segment.power);
        assert_eq!(
            with_short_segment.segment_count,
            without_short_segment.segment_count
        );
    }

    #[test]
    fn welch_segmented_with_no_qualifying_segments_returns_an_empty_estimate() {
        const SAMPLE_RATE_HZ: f64 = 1000.0;
        const SEGMENT_LEN: usize = 64;

        let config = WelchConfig {
            window: Window::Rectangular,
            segment_len: SEGMENT_LEN,
            overlap: 0.0,
            detrend: Detrend::None,
        };

        let too_short: Vec<f64> = vec![1.0; SEGMENT_LEN - 1];
        let psd = welch_segmented(&[&too_short], SAMPLE_RATE_HZ, &config);

        assert!(psd.power.is_empty());
        assert!(psd.freqs.is_empty());
        assert_eq!(psd.segment_count, 0);
    }

    #[test]
    fn welch_on_an_empty_slice_never_panics() {
        let config = WelchConfig {
            window: Window::Hann,
            segment_len: 64,
            overlap: 0.5,
            detrend: Detrend::Constant,
        };
        let psd = welch(&[], 1000.0, &config);
        assert!(psd.power.is_empty());
        assert_eq!(psd.segment_count, 0);
    }
}
