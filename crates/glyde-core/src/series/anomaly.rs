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
}
