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

//! The anomaly container (docs/SPEC.md §1.3, §4.1): every absorbed anomaly
//! against a series is counted and kept so the UI can surface it in the
//! anomaly gutter — never silently dropped, never corrected in place.

use std::ops::Range;

/// Anomalies flagged against a single [`super::Series`]. Every field is a
/// *record*, not a correction: SPEC §1.3 requires NaN runs and outliers to
/// stay in the plotted data and skipped rows to be counted, not guessed at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Anomalies {
    /// Contiguous runs of missing/NaN samples, as `[start, end)` sample
    /// index ranges. SPEC §1.3: rendered as a visible discontinuity, never
    /// interpolated.
    pub nan_runs: Vec<Range<usize>>,
    /// Sample indices flagged by the robust MAD-based outlier rule (SPEC
    /// §1.3). Flagged only — the value stays in the series and is still
    /// plotted; this list only drives the anomaly-gutter marker.
    pub outliers: Vec<usize>,
    /// Source row indices dropped during ingestion (wrong field count,
    /// unparseable value, truncated tail — SPEC §1.3). Every drop is
    /// counted here; there is no silent skip.
    pub skipped_rows: Vec<usize>,
}

impl Anomalies {
    /// True when nothing was flagged against the series at all.
    pub fn is_empty(&self) -> bool {
        self.nan_runs.is_empty() && self.outliers.is_empty() && self.skipped_rows.is_empty()
    }
}

/// Contiguous runs of `NaN` samples in `values`, as half-open `[start, end)`
/// sample-index ranges (SPEC §1.3: preserved as a visible discontinuity,
/// never interpolated). Adjacent `NaN` samples merge into one run rather
/// than being reported one index at a time (corpus case 43: three
/// consecutive `NaN` readings are one gap, not three).
pub fn detect_nan_runs(values: &[f64]) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut run_start: Option<usize> = None;

    for (index, value) in values.iter().enumerate() {
        if value.is_nan() {
            run_start.get_or_insert(index);
        } else if let Some(start) = run_start.take() {
            runs.push(start..index);
        }
    }
    if let Some(start) = run_start {
        runs.push(start..values.len());
    }

    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_anomalies_is_empty() {
        assert!(Anomalies::default().is_empty());
    }

    #[test]
    fn a_single_flagged_outlier_makes_it_non_empty() {
        let anomalies = Anomalies {
            outliers: vec![3],
            ..Anomalies::default()
        };

        assert!(!anomalies.is_empty());
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn a_single_nan_run_makes_it_non_empty() {
        let anomalies = Anomalies {
            nan_runs: vec![2..5],
            ..Anomalies::default()
        };

        assert!(!anomalies.is_empty());
    }

    #[test]
    fn a_single_skipped_row_makes_it_non_empty() {
        let anomalies = Anomalies {
            skipped_rows: vec![0],
            ..Anomalies::default()
        };

        assert!(!anomalies.is_empty());
    }

    // Corpus case 43 (QUALITY.md §1.43): three consecutive NaN readings
    // among otherwise-clean values must be reported as one run, not three.
    #[test]
    fn detect_nan_runs_merges_a_consecutive_run_into_one_range() {
        let values = [10.0, 10.1, f64::NAN, f64::NAN, f64::NAN, 10.5, 10.6];

        assert_eq!(detect_nan_runs(&values), vec![2..5]);
    }

    #[test]
    fn detect_nan_runs_reports_two_separate_runs_as_two_ranges() {
        let values = [f64::NAN, 1.0, f64::NAN, f64::NAN, 2.0];

        assert_eq!(detect_nan_runs(&values), vec![0..1, 2..4]);
    }

    #[test]
    fn detect_nan_runs_is_empty_for_a_run_free_series() {
        assert!(detect_nan_runs(&[1.0, 2.0, 3.0]).is_empty());
    }

    // Infinity is a valid float value (corpus case 44), never a NaN run.
    #[test]
    fn detect_nan_runs_does_not_flag_infinities() {
        assert!(detect_nan_runs(&[1.0, f64::INFINITY, f64::NEG_INFINITY, 1.0]).is_empty());
    }
}
