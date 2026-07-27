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

//! Gap detection and sampling classification over a timestamp series
//! (docs/SPEC.md §2.2–2.3, docs/ROADMAP.md M2).
//!
//! Every statistic here is defined against the **median** of the whole Δt
//! distribution, and every one of them is computed in bounded memory over a
//! [`TickSource`] (issue #85) rather than over a materialized `Vec` of deltas:
//! on a multi-GB file those `Vec`s were themselves proportional to file size,
//! which SPEC §5's flat peak-RSS cap does not allow. See [`select_kth_key`]
//! for how an *exact* median is obtained without holding the distribution.

use std::ops::Range;

use tracing::info;

use super::monotonic::MonotonicityReport;
use super::ticks::{delta_count, for_each_delta, TickSource};
use crate::Result;

/// A detected gap between two consecutive samples in a timestamp series
/// (SPEC §2.2–2.3: `gap = Δt > 10 × median Δt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// Index of the last sample before the gap.
    pub before_index: usize,
    /// Index of the first sample after the gap (always `before_index + 1`).
    pub after_index: usize,
    /// The gap's Δt, in the same tick unit as the input timestamps.
    pub delta: i128,
}

/// SPEC §2.2 sampling classification, always shown in the inference bar
/// alongside the estimated sampling rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingClass {
    /// Robust CV (median absolute deviation / median) of Δt is at most 1% of
    /// the median Δt: full DSP is available.
    Uniform,
    /// Uniform within every contiguous segment separated by a gap: DSP is
    /// available per segment (SPEC §3.3).
    SegmentedUniform,
    /// Neither of the above: PSD is disabled with an explanation (SPEC §3.3).
    Irregular,
}

/// Everything SPEC §2.1–2.2 has to say about one timestamp axis, from a single
/// median computation and a single pass over Δt (issue #85).
///
/// `ingest::report` used to derive these three answers with three independent
/// scans — `detect_gaps`, then `classify_sampling` (which re-ran gap detection
/// internally), then `detect_monotonicity` — each of which recomputed the same
/// median. They cannot disagree now: there is one scan and one median.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeAxisStats {
    /// How many gaps (`Δt > 10 × median Δt`, SPEC §2.2–2.3) the axis has.
    pub gap_count: usize,
    /// SPEC §2.2's classification of the Δt distribution.
    pub sampling_class: SamplingClass,
    /// SPEC §2.1's non-monotonic and duplicate timestamp counts.
    pub monotonicity: MonotonicityReport,
}

/// SPEC §2.2: "jitter (robust CV of Δt) ≤ 1% of median Δt" is the
/// `Uniform`/`SegmentedUniform` threshold.
const UNIFORM_JITTER_TOLERANCE: f64 = 0.01;

/// SPEC §2.2–2.3: `gap = Δt > 10 × median Δt`.
const GAP_MULTIPLE_OF_MEDIAN: f64 = 10.0;

/// Buckets per refinement pass of [`select_kth_key`]. 2¹⁶ narrows the
/// surviving key range by a factor of 65536 per pass, so the full `u128` key
/// space is exhausted in at most eight passes, and a realistic Δt distribution
/// (whose whole spread is a few million ticks) in two or three.
///
/// The counters are the only memory an order statistic needs: 512 KiB,
/// independent of how many rows the file has.
const SELECT_BUCKETS: usize = 1 << 16;

/// A key sequence that can be replayed once per refinement pass of
/// [`select_kth_key`], never materialized. Keys are `u128` and ordered by
/// plain unsigned comparison, so a statistic over a signed or floating-point
/// distribution encodes its values order-preservingly first (see
/// [`delta_key`] and [`deviation_key`]).
trait KeyStream {
    fn for_each(&self, visit: &mut dyn FnMut(u128)) -> Result<()>;
}

/// Order-preserving `i128` → `u128` key: flipping the sign bit makes unsigned
/// comparison agree with signed comparison.
fn delta_key(delta: i128) -> u128 {
    (delta as u128) ^ (1u128 << 127)
}

/// Inverse of [`delta_key`].
fn delta_from_key(key: u128) -> i128 {
    (key ^ (1u128 << 127)) as i128
}

/// Order-preserving key for a **non-negative, non-NaN** `f64` — which every
/// absolute deviation in the MAD is: IEEE-754 orders non-negative floats
/// exactly as their bit patterns order as integers.
fn deviation_key(deviation: f64) -> u128 {
    deviation.to_bits() as u128
}

/// Inverse of [`deviation_key`].
fn deviation_from_key(key: u128) -> f64 {
    f64::from_bits(key as u64)
}

/// Every consecutive Δt of `range`, keyed for ordering.
struct DeltaKeys<'a, S: ?Sized> {
    source: &'a S,
    range: Range<usize>,
}

impl<S: TickSource + ?Sized> KeyStream for DeltaKeys<'_, S> {
    fn for_each(&self, visit: &mut dyn FnMut(u128)) -> Result<()> {
        for_each_delta(self.source, self.range.clone(), &mut |delta| {
            visit(delta_key(delta));
            Ok(())
        })
    }
}

/// Every consecutive Δt of `range` as its absolute deviation from
/// `median_delta` — the distribution the MAD is the median of — keyed for
/// ordering.
struct DeviationKeys<'a, S: ?Sized> {
    source: &'a S,
    range: Range<usize>,
    median_delta: f64,
}

impl<S: TickSource + ?Sized> KeyStream for DeviationKeys<'_, S> {
    fn for_each(&self, visit: &mut dyn FnMut(u128)) -> Result<()> {
        let median_delta = self.median_delta;
        for_each_delta(self.source, self.range.clone(), &mut |delta| {
            visit(deviation_key((delta as f64 - median_delta).abs()));
            Ok(())
        })
    }
}

/// The `k`-th smallest key (0-based) `keys` yields, **exactly**, in
/// `O(SELECT_BUCKETS)` memory and a bounded number of passes over the stream.
///
/// An exact median is the one statistic that cannot be computed in a single
/// bounded pass, and SPEC §2.2 defines its rules over exactly that. So this
/// narrows instead of sorting: one pass establishes the key range, then each
/// refinement pass histograms the surviving range into [`SELECT_BUCKETS`]
/// buckets and keeps the single bucket the `k`-th key falls in, together with
/// how many keys rank below it. The range shrinks by 2¹⁶ per pass and the
/// answer is reached when it holds one key value, so the result is the same
/// value a full sort would have selected — not an approximation of it.
///
/// `k` must be less than the number of keys the stream yields.
fn select_kth_key(keys: &dyn KeyStream, k: usize) -> Result<u128> {
    let mut lo = u128::MAX;
    let mut hi = 0u128;
    let mut count = 0usize;
    keys.for_each(&mut |key| {
        lo = lo.min(key);
        hi = hi.max(key);
        count += 1;
    })?;
    debug_assert!(
        k < count,
        "select_kth_key: rank {k} is outside a {count}-key distribution"
    );

    // Reused across passes so the whole selection allocates exactly once.
    let mut counts = vec![0usize; SELECT_BUCKETS];
    // How many keys rank strictly below the surviving range.
    let mut rank_below = 0usize;

    while lo < hi {
        // Ceil-divides the surviving span into at most `SELECT_BUCKETS`
        // buckets of `width` keys each, without the `hi - lo + 1` that would
        // overflow on the full key space.
        let width = (hi - lo) / SELECT_BUCKETS as u128 + 1;
        let (range_lo, range_hi) = (lo, hi);
        counts.iter_mut().for_each(|count| *count = 0);
        keys.for_each(&mut |key| {
            if key >= range_lo && key <= range_hi {
                counts[((key - range_lo) / width) as usize] += 1;
            }
        })?;

        let mut cumulative = rank_below;
        let mut narrowed = false;
        for (bucket, &count) in counts.iter().enumerate() {
            if cumulative + count > k {
                lo = range_lo + bucket as u128 * width;
                hi = range_hi.min(lo.saturating_add(width - 1));
                rank_below = cumulative;
                narrowed = true;
                break;
            }
            cumulative += count;
        }
        // Unreachable: `rank_below` plus the keys inside the surviving range is
        // the whole distribution, and `k` indexes into it. Bail out rather than
        // spin if that invariant is ever broken.
        debug_assert!(
            narrowed,
            "select_kth_key: rank {k} fell outside every bucket"
        );
        if !narrowed {
            break;
        }
    }

    Ok(lo)
}

/// The median of the `count` keys `keys` yields, decoded to the `f64` space
/// SPEC §2.2's thresholds are expressed in.
///
/// The definition is the textbook one and matches the reference implementation
/// the tests compare against (`tests::sorted_median`) value for value: the
/// middle key of an odd-sized distribution, the average of the two middle keys
/// of an even-sized one.
fn median_key(keys: &dyn KeyStream, count: usize, decode: fn(u128) -> f64) -> Result<f64> {
    let upper = decode(select_kth_key(keys, count / 2)?);
    if count % 2 == 1 {
        Ok(upper)
    } else {
        let lower = decode(select_kth_key(keys, count / 2 - 1)?);
        Ok((lower + upper) / 2.0)
    }
}

/// Decodes a Δt key into the `f64` the statistics are computed in. `i128 as
/// f64` rounds monotonically, so the k-th smallest of the rounded Δt is the
/// rounded k-th smallest Δt — which is why selecting on exact `i128` keys and
/// converting afterwards gives the same median as rounding first and sorting
/// would (Golden Rule 1: the exact `i128` Δt is what `Gap::delta` reports;
/// `f64` is only ever the statistics' arithmetic).
fn delta_key_as_f64(key: u128) -> f64 {
    delta_from_key(key) as f64
}

/// SPEC §2.2's "median Δt" over `range`, or `None` when `range` holds fewer
/// than two ticks and so has no Δt at all.
fn median_delta<S: TickSource + ?Sized>(source: &S, range: Range<usize>) -> Result<Option<f64>> {
    let count = delta_count(&range, source.tick_count());
    if count == 0 {
        return Ok(None);
    }
    let keys = DeltaKeys { source, range };
    median_key(&keys, count, delta_key_as_f64).map(Some)
}

/// Robust CV of `range`'s Δt (SPEC §2.2: MAD / median) is at most 1% of the
/// median Δt. Fewer than two samples, or a single delta, has no dispersion to
/// measure and is vacuously uniform; an all-equal-Δt segment (median Δt of
/// zero, e.g. duplicate timestamps) is uniform only if every Δt is also zero.
fn is_uniform<S: TickSource + ?Sized>(source: &S, range: Range<usize>) -> Result<bool> {
    let Some(median_delta) = median_delta(source, range.clone())? else {
        return Ok(true);
    };

    if median_delta == 0.0 {
        let mut every_delta_zero = true;
        for_each_delta(source, range, &mut |delta| {
            if delta != 0 {
                every_delta_zero = false;
            }
            Ok(())
        })?;
        return Ok(every_delta_zero);
    }

    let count = delta_count(&range, source.tick_count());
    let deviations = DeviationKeys {
        source,
        range,
        median_delta,
    };
    let mad = median_key(&deviations, count, deviation_from_key)?;

    Ok(mad / median_delta.abs() <= UNIFORM_JITTER_TOLERANCE)
}

/// SPEC §2.2–2.3's gap rule itself, in one place: is this Δt more than ten
/// times the median? `threshold` is always [`GAP_MULTIPLE_OF_MEDIAN`] × the
/// median Δt of the series the Δt came from.
fn is_gap(delta: i128, threshold: f64) -> bool {
    delta as f64 > threshold
}

/// Hands every gap in `range` — with indices relative to the source's own row
/// numbering — to `visit`, in row order, without ever holding more than one of
/// them.
fn for_each_gap<S: TickSource + ?Sized>(
    source: &S,
    range: Range<usize>,
    threshold: f64,
    visit: &mut dyn FnMut(Gap) -> Result<()>,
) -> Result<()> {
    let mut before_index = range.start;
    for_each_delta(source, range, &mut |delta| {
        let index = before_index;
        before_index += 1;
        if is_gap(delta, threshold) {
            visit(Gap {
                before_index: index,
                after_index: index + 1,
                delta,
            })
        } else {
            Ok(())
        }
    })
}

/// The one implementation behind [`detect_gaps`], [`classify_sampling`] and
/// [`summarize_ticks`]: one median, then one pass over Δt that counts gaps,
/// accumulates SPEC §2.1's monotonicity counts, and checks each
/// gap-delimited segment for uniformity as soon as that segment closes — so
/// neither the gaps nor the segment boundaries are ever collected.
///
/// Does not log the monotonicity anomalies it counted; that is the caller's
/// choice (see [`summarize_ticks`]).
fn scan_distribution<S: TickSource + ?Sized>(source: &S) -> Result<TimeAxisStats> {
    let tick_count = source.tick_count();
    let mut monotonicity = MonotonicityReport::default();

    // Fewer than two samples has no Δt to classify and is vacuously
    // `Uniform`, with nothing to compare for monotonicity either.
    if tick_count < 2 {
        return Ok(TimeAxisStats {
            gap_count: 0,
            sampling_class: SamplingClass::Uniform,
            monotonicity,
        });
    }

    let median_delta = median_delta(source, 0..tick_count)?
        .expect("two or more ticks always have at least one Δt");
    let threshold = GAP_MULTIPLE_OF_MEDIAN * median_delta;

    // One pass over Δt answers all three questions: how many gaps, whether
    // each gap-delimited segment is uniform (checked the moment the segment
    // closes, so no boundary list is ever built), and SPEC §2.1's monotonicity
    // counts, which are the signs of these very same Δt.
    let mut gap_count = 0usize;
    let mut segment_start = 0usize;
    let mut every_segment_uniform = true;
    let mut after_index = 1usize;
    for_each_delta(source, 0..tick_count, &mut |delta| {
        monotonicity.observe_delta(delta);
        let segment_end = after_index;
        after_index += 1;
        if is_gap(delta, threshold) {
            gap_count += 1;
            // Short-circuits exactly like the `all()` this replaces: once one
            // segment is not uniform the classification is settled.
            if every_segment_uniform {
                every_segment_uniform = is_uniform(source, segment_start..segment_end)?;
            }
            segment_start = segment_end;
        }
        Ok(())
    })?;

    let sampling_class = if gap_count == 0 {
        if is_uniform(source, 0..tick_count)? {
            SamplingClass::Uniform
        } else {
            SamplingClass::Irregular
        }
    } else {
        if every_segment_uniform {
            every_segment_uniform = is_uniform(source, segment_start..tick_count)?;
        }
        if every_segment_uniform {
            SamplingClass::SegmentedUniform
        } else {
            SamplingClass::Irregular
        }
    };

    if gap_count > 0 {
        info!(
            gap_count,
            threshold_ticks = threshold,
            "gaps detected (Δt > 10× median Δt, SPEC §2.2–2.3)"
        );
    }
    info!(
        sampling_class = ?sampling_class,
        gap_count,
        "sampling classified (SPEC §2.2)"
    );

    Ok(TimeAxisStats {
        gap_count,
        sampling_class,
        monotonicity,
    })
}

/// Everything SPEC §2.1–2.2 says about `source`, in bounded memory — the
/// entry point `ingest::report` uses for both an in-memory and a spilled time
/// axis (issue #85). Logs every decision it took, exactly as the individual
/// [`detect_gaps`]/[`classify_sampling`]/[`super::detect_monotonicity`]
/// entry points do.
pub fn summarize_ticks<S: TickSource + ?Sized>(source: &S) -> Result<TimeAxisStats> {
    let stats = scan_distribution(source)?;
    stats.monotonicity.log_anomalies();
    Ok(stats)
}

/// Scans consecutive Δt in `timestamps` (non-decreasing, all in the same tick
/// unit) and reports every gap where `Δt > 10 × median Δt` (SPEC
/// §2.2–2.3). Feeds both sampling classification (`SegmentedUniform` vs.
/// `Irregular`, SPEC §2.2) and the gap view (docs/ROADMAP.md M8).
///
/// Returns one [`Gap`] per gap found, so a caller that only needs the *count*
/// of them over a multi-GB file should use [`summarize_ticks`] instead, which
/// counts without collecting.
pub fn detect_gaps(timestamps: &[i128]) -> Vec<Gap> {
    let Some(median_delta) =
        median_delta(timestamps, 0..timestamps.len()).expect(SLICE_SCAN_IS_INFALLIBLE)
    else {
        return Vec::new();
    };
    let threshold = GAP_MULTIPLE_OF_MEDIAN * median_delta;

    let mut gaps = Vec::new();
    for_each_gap(timestamps, 0..timestamps.len(), threshold, &mut |gap| {
        gaps.push(gap);
        Ok(())
    })
    .expect(SLICE_SCAN_IS_INFALLIBLE);

    if !gaps.is_empty() {
        info!(
            gap_count = gaps.len(),
            threshold_ticks = threshold,
            "gaps detected (Δt > 10× median Δt, SPEC §2.2–2.3)"
        );
    }

    gaps
}

/// Classifies `timestamps` (non-decreasing, all in the same tick unit) per
/// SPEC §2.2: `Uniform` if there is no gap and the robust CV of Δt is within
/// tolerance; `SegmentedUniform` if every contiguous segment between gaps is
/// itself uniform; `Irregular` otherwise. Fewer than two samples has no Δt
/// to classify and is vacuously `Uniform`.
pub fn classify_sampling(timestamps: &[i128]) -> SamplingClass {
    scan_distribution(timestamps)
        .expect(SLICE_SCAN_IS_INFALLIBLE)
        .sampling_class
}

/// Only a [`TickSource`] that reads from disk can fail; scanning a slice
/// already in memory cannot, so the slice-taking entry points here have no
/// error to report.
pub(super) const SLICE_SCAN_IS_INFALLIBLE: &str = "scanning an in-memory tick slice cannot fail";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::corpus_fixture::corpus_ticks;
    use crate::time::ticks::test_support::ChunkedTicks;
    use proptest::prelude::*;

    /// The straightforward median: sort the whole distribution and take the
    /// middle. This is the reference [`median_key`]'s bounded selection is
    /// proven against below — kept as a test-only oracle rather than a second
    /// production path, so there is still exactly one median implementation in
    /// the shipped code (docs/ARCHITECTURE.md Hard rule 4).
    fn sorted_median(values: &mut [f64]) -> f64 {
        values.sort_by(|a, b| a.partial_cmp(b).expect("Δt values are never NaN"));
        let n = values.len();
        if n % 2 == 1 {
            values[n / 2]
        } else {
            (values[n / 2 - 1] + values[n / 2]) / 2.0
        }
    }

    fn bounded_median_delta(ticks: &[i128]) -> Option<f64> {
        median_delta(ticks, 0..ticks.len()).expect(SLICE_SCAN_IS_INFALLIBLE)
    }

    fn sorted_median_delta(ticks: &[i128]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let mut deltas: Vec<f64> = ticks
            .windows(2)
            .map(|pair| (pair[1] - pair[0]) as f64)
            .collect();
        Some(sorted_median(&mut deltas))
    }

    // Corpus case 38 (docs/QUALITY.md §1.38): six samples nominally 1s apart
    // with a few milliseconds of jitter on each Δt. SPEC §2.2's 1%-of-median
    // robust-CV tolerance must absorb this jitter as `Uniform`, with no gap.
    #[test]
    fn corpus_case_38_timestamps_with_jitter_classify_as_uniform() {
        let ticks = corpus_ticks("case-38-timestamps-with-jitter.csv", "timestamp");

        assert_eq!(detect_gaps(&ticks), Vec::new());
        assert_eq!(classify_sampling(&ticks), SamplingClass::Uniform);
    }

    // Corpus case 39 (docs/QUALITY.md §1.39): an event log with Δt of
    // 2s/1s/12s/1s/24s. The median Δt (2s) makes only the trailing 24s delta
    // a gap (> 10 * median = 20s); the pre-gap run (2s/1s/12s/1s) is itself
    // far too jittery to be `Uniform` (robust CV well over 1%), so the whole
    // series is `Irregular` — SPEC §2.2's `SegmentedUniform` requires every
    // segment to be uniform, not just the presence of a gap.
    #[test]
    fn corpus_case_39_irregular_event_log_classifies_as_irregular_with_one_gap() {
        let ticks = corpus_ticks("case-39-irregular-event-log.csv", "timestamp");

        assert_eq!(detect_gaps(&ticks).len(), 1);
        assert_eq!(classify_sampling(&ticks), SamplingClass::Irregular);
    }

    // Corpus case 40 (docs/QUALITY.md §1.40): three uniform 3-sample bursts
    // (1s apart within a burst) separated by two ~2-day gaps. Each burst is
    // internally uniform, so SPEC §2.2 classifies the whole series
    // `SegmentedUniform` with exactly the two inter-burst gaps detected.
    #[test]
    fn corpus_case_40_segmented_three_bursts_classifies_as_segmented_uniform_with_two_gaps() {
        let ticks = corpus_ticks("case-40-segmented-three-bursts.csv", "timestamp");

        assert_eq!(detect_gaps(&ticks).len(), 2);
        assert_eq!(classify_sampling(&ticks), SamplingClass::SegmentedUniform);
    }

    #[test]
    fn fewer_than_two_samples_is_vacuously_uniform_with_no_gaps() {
        assert_eq!(detect_gaps(&[]), Vec::new());
        assert_eq!(classify_sampling(&[]), SamplingClass::Uniform);
        assert_eq!(detect_gaps(&[42]), Vec::new());
        assert_eq!(classify_sampling(&[42]), SamplingClass::Uniform);
    }

    #[test]
    fn duplicate_timestamps_with_a_zero_median_delta_are_uniform_only_if_every_delta_is_zero() {
        let uniform: &[i128] = &[0, 0, 0, 0];
        let not_uniform: &[i128] = &[0, 0, 0, 5];

        assert!(is_uniform(uniform, 0..uniform.len()).expect(SLICE_SCAN_IS_INFALLIBLE));
        assert!(!is_uniform(not_uniform, 0..not_uniform.len()).expect(SLICE_SCAN_IS_INFALLIBLE));
    }

    // Issue #85: the whole point of the bounded selection is that it is
    // *exact*. These are the cases where an approximate median would still
    // look plausible — an even count (two middle values averaged), negative
    // Δt from a non-monotonic series, and a distribution wide enough to need
    // several refinement passes.
    #[test]
    fn the_bounded_median_matches_a_full_sort_on_hand_checked_distributions() {
        // Odd delta count: 1, 3, 5 -> 3.
        assert_eq!(bounded_median_delta(&[0, 1, 4, 9]), Some(3.0));
        // Even delta count: 1, 2, 3, 10 -> (2 + 3) / 2.
        assert_eq!(bounded_median_delta(&[0, 1, 3, 6, 16]), Some(2.5));
        // A non-monotonic series' negative deltas order below positive ones:
        // -10, -5, 5 -> -5.
        assert_eq!(bounded_median_delta(&[0, -10, -15, -10]), Some(-5.0));
        // A single Δt is its own median.
        assert_eq!(bounded_median_delta(&[7, 19]), Some(12.0));
        // Wide enough that the surviving key range needs more than one
        // refinement pass to collapse.
        assert_eq!(
            bounded_median_delta(&[0, 1, 1 + i128::from(u32::MAX), 2 + i128::from(u64::MAX)]),
            Some(f64::from(u32::MAX))
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Issue #85's correctness requirement: the bounded-memory median must
        /// be the value a full sort would have selected, for any series — not
        /// an approximation of it. Ticks are drawn from a range wide enough to
        /// force several refinement passes and are *not* constrained to be
        /// monotonic, so negative Δt are covered too.
        #[test]
        fn the_bounded_median_equals_the_sorted_median_for_any_series(
            ticks in prop::collection::vec(-1_000_000_000_000i128..1_000_000_000_000, 0..200),
        ) {
            prop_assert_eq!(bounded_median_delta(&ticks), sorted_median_delta(&ticks));
        }

        /// The same guarantee for the statistics built on top of the median:
        /// a source that hands its ticks over in tiny chunks (standing in for
        /// a spilled axis, which reads them back a buffer at a time) must
        /// classify and count exactly like the whole slice does.
        #[test]
        fn chunked_and_whole_slice_sources_agree_on_every_statistic(
            ticks in prop::collection::vec(-1_000i128..1_000, 0..120),
            chunk_len in 1usize..7,
        ) {
            let chunked = ChunkedTicks { ticks: ticks.clone(), chunk_len };

            let whole = summarize_ticks(ticks.as_slice()).expect(SLICE_SCAN_IS_INFALLIBLE);
            let chunked = summarize_ticks(&chunked).expect(SLICE_SCAN_IS_INFALLIBLE);

            prop_assert_eq!(whole, chunked);
            prop_assert_eq!(whole.gap_count, detect_gaps(&ticks).len());
            prop_assert_eq!(whole.sampling_class, classify_sampling(&ticks));
            prop_assert_eq!(whole.monotonicity, crate::time::detect_monotonicity(&ticks));
        }
    }
}
