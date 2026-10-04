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
//! `welch_source`/`welch_segmented_source` are the streaming entry points
//! (SPEC §3.2, docs/ROADMAP.md M5 "Streaming Welch"): they read a
//! [`SampleSource`] in its own bounded chunks and hold one analysis window at
//! a time, never the selection. `welch`/`welch_segmented` are the same code
//! fed from a resident slice. Choosing *what* to compute on — which samples,
//! which segments, whether the selection may have a PSD at all — is
//! `dsp::psd`'s job, not this module's.

use std::ops::Range;
use std::sync::Arc;

use super::detrend::{self, Detrend};
use super::window::{self, Window};
use crate::series::SampleSource;
use crate::Result;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Smallest segment length the software's default ever picks (SPEC §3.2).
pub const MIN_SEGMENT_LEN: usize = 256;
/// Largest segment length the software's default ever picks (SPEC §3.2).
pub const MAX_SEGMENT_LEN: usize = 65536;
/// Default segment overlap fraction (SPEC §3.2: "50% overlap").
pub const DEFAULT_OVERLAP: f64 = 0.5;

/// The (at most three) user-facing controls behind the PSD settings
/// affordance (SPEC §3.2), plus the detrend method (documented, not exposed
/// as a fourth control).
#[derive(Debug, Clone, PartialEq)]
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
    /// Number of analysis windows averaged into this estimate.
    pub segment_count: usize,
    /// How many non-finite (missing) samples the selection held. None of them
    /// ever reached an FFT: every window that would have spanned one was
    /// skipped (see [`welch_source`]).
    pub non_finite_count: usize,
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
///
/// The in-memory face of [`welch_source`]: both feed the same
/// [`WelchAccumulator`], so there is exactly one Welch implementation
/// (docs/ARCHITECTURE.md Hard rule 4).
pub fn welch(samples: &[f64], sample_rate_hz: f64, config: &WelchConfig) -> Psd {
    welch_source(
        samples,
        0..samples.len(),
        sample_rate_hz,
        config,
        &mut |_| true,
    )
    .expect("reading an in-memory slice cannot fail")
    .expect("an estimate that is never cancelled always completes")
}

/// Welch's method over `range` of `source`, streaming (SPEC §3.2: "accumulate
/// segment periodograms while reading, never by loading everything"): the
/// samples are read in the source's own bounded chunks, and the only memory
/// held is one analysis window's worth of buffers plus the running sum of
/// periodograms — independent of how many samples the selection holds.
///
/// `keep_going` is called after every chunk with the number of samples read
/// so far; returning `false` cancels the estimate, which then returns
/// `Ok(None)` — never a spectrum of whatever part happened to be read.
///
/// A non-finite sample (a missing value, SPEC §1.3) is never fed to an FFT:
/// the window being filled is discarded and the next one starts right after
/// the hole, so no analysis window ever spans one. How many there were is
/// reported in [`Psd::non_finite_count`].
pub fn welch_source<S: SampleSource + ?Sized>(
    source: &S,
    range: Range<usize>,
    sample_rate_hz: f64,
    config: &WelchConfig,
    keep_going: &mut dyn FnMut(usize) -> bool,
) -> Result<Option<Psd>> {
    let end = range.end.min(source.sample_count());
    let start = range.start.min(end);
    let selected = end - start;
    let effective_len = config.segment_len.min(selected);
    if effective_len == 0 {
        return Ok(Some(Psd {
            freqs: Vec::new(),
            power: Vec::new(),
            delta_f: sample_rate_hz / config.segment_len.max(1) as f64,
            segment_count: 0,
            non_finite_count: 0,
        }));
    }

    let mut accumulator = WelchAccumulator::new(effective_len, config);
    let mut read = 0usize;
    let mut cancelled = false;
    source.visit_sample_chunks(start..end, &mut |chunk| {
        if cancelled {
            return Ok(());
        }
        accumulator.push(chunk);
        read += chunk.len();
        cancelled = !keep_going(read);
        Ok(())
    })?;
    if cancelled {
        return Ok(None);
    }
    Ok(Some(accumulator.finish(sample_rate_hz)))
}

/// The running state of one Welch estimate: the window being filled, the
/// FFT plan and its scratch, and the sum of every completed window's
/// periodogram. Its footprint is a fixed multiple of the window length, so
/// it is the same whether it is fed ten thousand samples or ten billion.
struct WelchAccumulator {
    len: usize,
    step: usize,
    detrend: Detrend,
    window_coeffs: Vec<f64>,
    window_sum_sq: f64,
    fft: Arc<dyn Fft<f64>>,
    /// The samples of the window currently being filled, in order.
    pending: Vec<f64>,
    // Reuse every scratch buffer across windows. A large selection may have
    // hundreds of overlapping windows; allocating per window would add
    // avoidable latency to the PSD path.
    buffer: Vec<f64>,
    spectrum: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
    accumulated: Vec<f64>,
    segment_count: usize,
    non_finite_count: usize,
}

impl WelchAccumulator {
    fn new(len: usize, config: &WelchConfig) -> Self {
        let window_coeffs = window::coefficients(config.window, len);
        let window_sum_sq = window_coeffs.iter().map(|w| w * w).sum();
        let fft = FftPlanner::<f64>::new().plan_fft_forward(len);
        let scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        Self {
            len,
            step: sub_segment_step(len, config.overlap),
            detrend: config.detrend,
            window_coeffs,
            window_sum_sq,
            fft,
            pending: Vec::with_capacity(len),
            buffer: vec![0.0; len],
            spectrum: vec![Complex::new(0.0, 0.0); len],
            scratch,
            accumulated: vec![0.0; len / 2 + 1],
            segment_count: 0,
            non_finite_count: 0,
        }
    }

    /// Feeds the next samples, in order, completing every window they fill.
    fn push(&mut self, mut samples: &[f64]) {
        while !samples.is_empty() {
            let wanted = self.len - self.pending.len();
            let take = wanted.min(samples.len());
            let (head, rest) = samples.split_at(take);
            samples = rest;
            match head.iter().rposition(|sample| !sample.is_finite()) {
                Some(last_hole) => {
                    // Every window touching a hole is discarded; the next
                    // one starts on the sample right after the last hole.
                    self.non_finite_count += head.iter().filter(|s| !s.is_finite()).count();
                    self.pending.clear();
                    self.pending.extend_from_slice(&head[last_hole + 1..]);
                }
                None => self.pending.extend_from_slice(head),
            }
            if self.pending.len() == self.len {
                self.complete_window();
                self.pending.drain(..self.step);
            }
        }
    }

    fn complete_window(&mut self) {
        self.buffer.copy_from_slice(&self.pending);
        detrend::apply(&mut self.buffer, self.detrend);
        for ((frequency_sample, &sample), &w) in self
            .spectrum
            .iter_mut()
            .zip(self.buffer.iter())
            .zip(&self.window_coeffs)
        {
            *frequency_sample = Complex::new(sample * w, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        for (bin, power) in self.accumulated.iter_mut().enumerate() {
            *power += self.spectrum[bin].norm_sqr();
        }
        self.segment_count += 1;
    }

    /// The averaged, one-sided, density-scaled estimate (SPEC §3.2).
    fn finish(self, sample_rate_hz: f64) -> Psd {
        let scale_denominator = sample_rate_hz * self.window_sum_sq;
        let nyquist_bin = if self.len.is_multiple_of(2) {
            Some(self.len / 2)
        } else {
            None
        };
        let segment_count = self.segment_count;
        let power: Vec<f64> = self
            .accumulated
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

        let delta_f = sample_rate_hz / self.len as f64;
        let freqs = (0..power.len()).map(|bin| bin as f64 * delta_f).collect();

        Psd {
            freqs,
            power,
            delta_f,
            segment_count,
            non_finite_count: self.non_finite_count,
        }
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
    let mut average = LengthWeightedAverage::default();
    for seg in segments
        .iter()
        .filter(|seg| seg.len() >= config.segment_len)
    {
        average.add(seg.len(), welch(seg, sample_rate_hz, config));
    }
    average.finish(sample_rate_hz, config)
}

/// [`welch_segmented`] over `segments` — ranges of one `source`, each a
/// contiguous, gap-free run — streaming each exactly as [`welch_source`]
/// does and folding it into the average as soon as it completes, so memory
/// stays one window's worth however many segments there are. `keep_going`
/// sees the running total of samples read across all segments; cancelling
/// returns `Ok(None)`.
pub fn welch_segmented_source<S: SampleSource + ?Sized>(
    source: &S,
    segments: &[Range<usize>],
    sample_rate_hz: f64,
    config: &WelchConfig,
    keep_going: &mut dyn FnMut(usize) -> bool,
) -> Result<Option<Psd>> {
    let mut average = LengthWeightedAverage::default();
    let mut read_before = 0usize;
    for segment in segments.iter().filter(|r| r.len() >= config.segment_len) {
        let estimate = welch_source(
            source,
            segment.clone(),
            sample_rate_hz,
            config,
            &mut |read| keep_going(read_before + read),
        )?;
        let Some(estimate) = estimate else {
            return Ok(None);
        };
        read_before += segment.len();
        average.add(segment.len(), estimate);
    }
    Ok(Some(average.finish(sample_rate_hz, config)))
}

/// SPEC §3.3: per-segment estimates averaged, each weighted by its segment's
/// length in samples, folded in one at a time. The one place this average is
/// computed, for both the in-memory and the streaming segmented entry points.
#[derive(Default)]
struct LengthWeightedAverage {
    reference: Option<Psd>,
    weighted_power: Vec<f64>,
    total_weight: f64,
    segment_count: usize,
    non_finite_count: usize,
}

impl LengthWeightedAverage {
    fn add(&mut self, len: usize, psd: Psd) {
        let weight = len as f64;
        if self.reference.is_none() {
            self.weighted_power = psd.power.iter().map(|&power| power * weight).collect();
        } else {
            for (acc, &p) in self.weighted_power.iter_mut().zip(psd.power.iter()) {
                *acc += weight * p;
            }
        }
        self.total_weight += weight;
        self.segment_count += psd.segment_count;
        self.non_finite_count += psd.non_finite_count;
        if self.reference.is_none() {
            self.reference = Some(Psd {
                power: Vec::new(),
                ..psd
            });
        }
    }

    fn finish(self, sample_rate_hz: f64, config: &WelchConfig) -> Psd {
        let Some(reference) = self.reference else {
            return Psd {
                freqs: Vec::new(),
                power: Vec::new(),
                delta_f: sample_rate_hz / config.segment_len.max(1) as f64,
                segment_count: 0,
                non_finite_count: 0,
            };
        };
        let total_weight = self.total_weight;
        Psd {
            freqs: reference.freqs,
            power: self
                .weighted_power
                .into_iter()
                .map(|p| p / total_weight)
                .collect(),
            delta_f: reference.delta_f,
            segment_count: self.segment_count,
            non_finite_count: self.non_finite_count,
        }
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
