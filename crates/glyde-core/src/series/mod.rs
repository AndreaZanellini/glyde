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

//! `Series`, the dtype model, and the anomaly container (docs/SPEC.md
//! §1.4, docs/ARCHITECTURE.md workspace layout `series/`, docs/ROADMAP.md
//! M2). This is a pure data model: it holds whatever a reader ingested in
//! its native dtype and whatever anomalies were flagged against it. It does
//! not parse, infer, or index anything itself — that is `ingest`'s and
//! `time`'s job, both of which produce a `Series` as their output.

mod anomaly;
mod dtype;
mod samples;
mod state;

pub use anomaly::{detect_nan_runs, Anomalies, NanRunScan};
pub use dtype::{Dtype, SeriesValues, SpilledValues, ViewKind};
pub use samples::{SampleSource, SeriesSamples, SAMPLE_CHUNK_LEN};
pub use state::{
    bool_state_bands, BoolBand, BoolBandBuilder, BoolLane, BOOL_OVERVIEW_CELLS,
    MAX_EXACT_BOOL_BANDS,
};

/// Apply `order[new_index] = old_index` without cloning a whole column.
/// Cycles need only one bit per row, even for string columns whose contents
/// may be much larger than the column's pointer array.
pub(crate) fn reorder_in_place<T>(values: &mut [T], order: &[usize]) {
    debug_assert_eq!(values.len(), order.len());
    let mut visited = vec![false; values.len()];
    for start in 0..values.len() {
        if visited[start] {
            continue;
        }
        let mut current = start;
        loop {
            visited[current] = true;
            let source = order[current];
            if source == start {
                break;
            }
            values.swap(current, source);
            current = source;
        }
    }
}

#[cfg(test)]
mod reorder_tests {
    use super::reorder_in_place;

    #[test]
    fn permutes_multiple_cycles_without_requiring_clone() {
        struct NonClone(u8);
        let mut values = [
            NonClone(0),
            NonClone(1),
            NonClone(2),
            NonClone(3),
            NonClone(4),
        ];
        reorder_in_place(&mut values, &[2, 0, 1, 4, 3]);
        assert_eq!(values.map(|value| value.0), [2, 0, 1, 4, 3]);
    }
}

/// One ingested column: its name, its values in their native dtype, and any
/// anomalies flagged against it. SPEC §1.4: constant and single-sample
/// series are valid `Series` values and must be able to render like any
/// other.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    name: String,
    values: SeriesValues,
    anomalies: Anomalies,
}

impl Series {
    /// A series with no anomalies flagged against it yet.
    pub fn new(name: impl Into<String>, values: SeriesValues) -> Self {
        Self {
            name: name.into(),
            values,
            anomalies: Anomalies::default(),
        }
    }

    /// A series together with the anomalies already flagged against it
    /// during ingestion.
    pub fn with_anomalies(
        name: impl Into<String>,
        values: SeriesValues,
        anomalies: Anomalies,
    ) -> Self {
        Self {
            name: name.into(),
            values,
            anomalies,
        }
    }

    /// The column name, taken verbatim from the source header (SPEC §4.1:
    /// unit suffixes like `°C` are part of this name, never parsed out).
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dtype(&self) -> Dtype {
        self.values.dtype()
    }

    /// Which of the three SPEC §4 visualizations this series routes to.
    pub fn view_kind(&self) -> ViewKind {
        self.dtype().view_kind()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// SPEC §1.4: constant series (every sample equal, including the
    /// vacuous single-sample case) are valid inputs, not an edge case to
    /// special-case away.
    pub fn is_constant(&self) -> bool {
        self.values.is_constant()
    }

    pub fn values(&self) -> &SeriesValues {
        &self.values
    }

    pub fn anomalies(&self) -> &Anomalies {
        &self.anomalies
    }

    /// [`SeriesValues::reorder`], plus keeping the anomaly record consistent
    /// with the new sample order (SPEC §2.1's "[Sort]" affordance). NaN runs
    /// are *positional* (SPEC §1.3: `[start, end)` sample-index ranges), so
    /// they are recomputed against the reordered samples rather than
    /// permuted themselves — permuting index ranges is equivalent to
    /// recomputing them and would be more code for no benefit. Every other
    /// anomaly kind is unaffected by reordering the *retained* rows:
    /// `skipped_rows` names *source* row numbers that were never
    /// materialized into this series at all, and `outliers` is not populated
    /// by anything yet (docs/ROADMAP.md M8).
    pub(crate) fn reorder(&mut self, order: &[usize]) {
        self.values.reorder(order);
        if let Some(samples) = self.values.as_f64_slice() {
            self.anomalies.nan_runs = detect_nan_runs(samples);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_series_has_no_anomalies() {
        let series = Series::new("value", SeriesValues::F64(vec![1.0, 2.0, 3.0]));

        assert!(series.anomalies().is_empty());
    }

    #[test]
    fn series_exposes_its_name_dtype_and_length() {
        let series = Series::new("temperature", SeriesValues::F64(vec![1.0, 2.0]));

        assert_eq!(series.name(), "temperature");
        assert_eq!(series.dtype(), Dtype::F64);
        assert_eq!(series.len(), 2);
        assert!(!series.is_empty());
    }

    #[test]
    fn bool_and_string_series_route_to_the_state_timeline() {
        let flag = Series::new("flag", SeriesValues::Bool(vec![true, false]));
        let state = Series::new(
            "state",
            SeriesValues::String(vec!["running".to_string(), "idle".to_string()]),
        );

        assert_eq!(flag.view_kind(), ViewKind::StateTimeline);
        assert_eq!(state.view_kind(), ViewKind::StateTimeline);
    }

    #[test]
    fn numeric_series_routes_to_the_time_domain() {
        let series = Series::new("value", SeriesValues::F64(vec![1.0, 2.0]));

        assert_eq!(series.view_kind(), ViewKind::TimeDomain);
    }

    #[test]
    fn a_constant_series_is_a_valid_input_and_reports_itself_as_constant() {
        let series = Series::new("value", SeriesValues::F64(vec![42.0, 42.0, 42.0, 42.0]));

        assert!(series.is_constant());
        assert!(series.anomalies().is_empty());
        assert_eq!(series.len(), 4);
    }

    #[test]
    fn a_single_sample_series_is_valid_and_vacuously_constant() {
        let series = Series::new("value", SeriesValues::F64(vec![42.0]));

        assert!(series.is_constant());
        assert_eq!(series.len(), 1);
    }

    // SPEC §2.1's "[Sort]" affordance: `reorder` must permute samples like
    // `SeriesValues::reorder`, and — for a float column — recompute NaN runs
    // against the new order rather than leaving them pointing at the old
    // positions.
    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn reorder_permutes_samples_and_recomputes_nan_runs_for_a_float_series() {
        // Original order: [1.0, NaN, NaN, 4.0]; nan_runs = [1..3]. With
        // order = [3, 0, 1, 2], new[i] = old[order[i]] reads as
        // [4.0, 1.0, NaN, NaN] — the NaN run moves from [1..3) to [2..4),
        // which only a real recomputation (not a carried-over stale range)
        // produces.
        let mut series = Series::with_anomalies(
            "value",
            SeriesValues::F64(vec![1.0, f64::NAN, f64::NAN, 4.0]),
            Anomalies {
                nan_runs: vec![1..3],
                ..Anomalies::default()
            },
        );

        series.reorder(&[3, 0, 1, 2]);

        let SeriesValues::F64(values) = series.values() else {
            panic!("expected an F64 series");
        };
        assert_eq!(values[0], 4.0);
        assert_eq!(values[1], 1.0);
        assert!(values[2].is_nan());
        assert!(values[3].is_nan());
        assert_eq!(series.anomalies().nan_runs, vec![2..4]);
    }

    #[test]
    fn reorder_leaves_skipped_rows_untouched() {
        let mut series = Series::with_anomalies(
            "value",
            SeriesValues::F64(vec![1.0, 2.0, 3.0]),
            Anomalies {
                skipped_rows: vec![7, 12],
                ..Anomalies::default()
            },
        );

        series.reorder(&[2, 0, 1]);

        assert_eq!(series.anomalies().skipped_rows, vec![7, 12]);
    }

    #[test]
    fn with_anomalies_attaches_the_given_anomaly_container() {
        let anomalies = Anomalies {
            skipped_rows: vec![2],
            ..Anomalies::default()
        };
        let series = Series::with_anomalies(
            "value",
            SeriesValues::F64(vec![1.0, 2.0]),
            anomalies.clone(),
        );

        assert_eq!(series.anomalies(), &anomalies);
    }
}
