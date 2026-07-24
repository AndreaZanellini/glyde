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

/// SPEC §2.2: "jitter (robust CV of Δt) ≤ 1% of median Δt" is the
/// `Uniform`/`SegmentedUniform` threshold.
const UNIFORM_JITTER_TOLERANCE: f64 = 0.01;

/// The median of `values`, which must be non-empty. `values` is sorted in
/// place; statistics only, never fed back into a stored timestamp (Golden
/// Rule 1 governs raw sample data, not this derived classification metric).
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("Δt values are never NaN"));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// Consecutive Δt in `timestamps` (non-decreasing, all in the same tick
/// unit), as `f64` for the statistics in this module only — `Gap::delta`
/// itself is always the exact `i128` difference.
fn deltas(timestamps: &[i128]) -> Vec<i128> {
    timestamps
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect()
}

/// Scans consecutive Δt in `timestamps` (non-decreasing, all in the same
/// tick unit) and reports every gap where `Δt > 10 × median Δt` (SPEC
/// §2.2–2.3). Feeds both sampling classification (`SegmentedUniform` vs.
/// `Irregular`, SPEC §2.2) and the gap view (docs/ROADMAP.md M8).
pub fn detect_gaps(timestamps: &[i128]) -> Vec<Gap> {
    let deltas = deltas(timestamps);
    if deltas.is_empty() {
        return Vec::new();
    }

    let mut deltas_f64: Vec<f64> = deltas.iter().map(|&delta| delta as f64).collect();
    let threshold = 10.0 * median(&mut deltas_f64);

    deltas
        .iter()
        .enumerate()
        .filter(|&(_, &delta)| delta as f64 > threshold)
        .map(|(index, &delta)| Gap {
            before_index: index,
            after_index: index + 1,
            delta,
        })
        .collect()
}

/// Robust CV of `timestamps`' Δt (SPEC §2.2: MAD / median) is at most 1% of
/// the median Δt. Fewer than two samples, or a single delta, has no
/// dispersion to measure and is vacuously uniform; an all-equal-Δt segment
/// (median Δt of zero, e.g. duplicate timestamps) is uniform only if every
/// Δt is also zero.
fn is_uniform(timestamps: &[i128]) -> bool {
    let deltas = deltas(timestamps);
    if deltas.is_empty() {
        return true;
    }

    let mut deltas_f64: Vec<f64> = deltas.iter().map(|&delta| delta as f64).collect();
    let median_delta = median(&mut deltas_f64);
    if median_delta == 0.0 {
        return deltas_f64.iter().all(|&delta| delta == 0.0);
    }

    let mut deviations: Vec<f64> = deltas_f64
        .iter()
        .map(|&delta| (delta - median_delta).abs())
        .collect();
    let mad = median(&mut deviations);

    mad / median_delta.abs() <= UNIFORM_JITTER_TOLERANCE
}

/// Classifies `timestamps` (non-decreasing, all in the same tick unit) per
/// SPEC §2.2: `Uniform` if there is no gap and the robust CV of Δt is within
/// tolerance; `SegmentedUniform` if every contiguous segment between gaps is
/// itself uniform; `Irregular` otherwise. Fewer than two samples has no Δt
/// to classify and is vacuously `Uniform`.
pub fn classify_sampling(timestamps: &[i128]) -> SamplingClass {
    if timestamps.len() < 2 {
        return SamplingClass::Uniform;
    }

    let gaps = detect_gaps(timestamps);
    if gaps.is_empty() {
        return if is_uniform(timestamps) {
            SamplingClass::Uniform
        } else {
            SamplingClass::Irregular
        };
    }

    let mut boundaries: Vec<usize> = Vec::with_capacity(gaps.len() + 2);
    boundaries.push(0);
    boundaries.extend(gaps.iter().map(|gap| gap.after_index));
    boundaries.push(timestamps.len());

    let every_segment_uniform = boundaries
        .windows(2)
        .all(|pair| is_uniform(&timestamps[pair[0]..pair[1]]));

    if every_segment_uniform {
        SamplingClass::SegmentedUniform
    } else {
        SamplingClass::Irregular
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::corpus_fixture::corpus_column;
    use crate::time::{infer_timestamp_format, parse_timestamp};

    /// Parses every raw field of `column_name` in `file_name` under whatever
    /// format `infer_timestamp_format` picks, returning the `i128` ticks in
    /// row order — the shape `detect_gaps`/`classify_sampling` consume.
    fn corpus_ticks(file_name: &str, column_name: &str) -> Vec<i128> {
        let fields = corpus_column(file_name, column_name);
        let inference = infer_timestamp_format(&fields)
            .unwrap_or_else(|| panic!("{file_name}: must infer a timestamp format"));
        fields
            .iter()
            .map(|field| {
                parse_timestamp(field, inference.format)
                    .unwrap_or_else(|err| panic!("{file_name}: {field:?} must parse: {err}"))
                    .ticks
            })
            .collect()
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
        assert!(is_uniform(&[0, 0, 0, 0]));
        assert!(!is_uniform(&[0, 0, 0, 5]));
    }
}
