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

//! Full in-memory materialization of a small delimited-text file
//! (docs/ROADMAP.md M2 "Time-domain view v1": render raw samples,
//! pre-pyramid). [`load`] wires together the same inference pieces
//! `ingest::report::inspect` uses for its summary — encoding, delimiter,
//! decimal separator, time-index detection — but, unlike `inspect`, actually
//! materializes every column's typed values instead of counts alone, so
//! `glyde-app`'s time-domain view has real samples to plot.
//!
//! This is deliberately the "small files" half of docs/ARCHITECTURE.md's
//! "Level 0 = raw samples" index layer: it loads the whole file into memory
//! in one pass, with none of the RAM-budget affordability check or
//! chunked/streaming reads SPEC §5.1 requires for arbitrary-size files —
//! that is docs/ROADMAP.md M3's job (see [`super::csv::CsvParseOutcome`]'s
//! own doc comment, which flags the same deferral for row data in general).

use super::csv::open_path_capturing_all_columns;
use super::infer::{infer_column, normalize_decimal_field};
use crate::series::Series;
use crate::time::{infer_timestamp_format, parse_timestamp, Timestamp, TimestampFormat};
use crate::{GlydeError, Result};
use std::path::Path;

/// The time axis of a materialized [`Dataset`] (SPEC §2.1's two accepted
/// index kinds).
#[derive(Debug, Clone, PartialEq)]
pub enum TimeAxis {
    /// An absolute timestamp column, kept together with the
    /// [`TimestampFormat`] it was detected as so a caller can redisplay each
    /// [`Timestamp`] the same way the source wrote it (e.g. round-tripping
    /// an honored UTC offset, SPEC §2.1).
    Absolute {
        timestamps: Vec<Timestamp>,
        format: TimestampFormat,
    },
    /// A monotonic integer/float sequence with no absolute-time meaning
    /// (SPEC §2.1 "progressive numeric").
    Progressive { values: Vec<f64> },
}

impl TimeAxis {
    pub fn len(&self) -> usize {
        match self {
            TimeAxis::Absolute { timestamps, .. } => timestamps.len(),
            TimeAxis::Progressive { values } => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A fully materialized small delimited-text file: its time axis plus every
/// other column, typed (SPEC §1.4). `columns` is in source header order,
/// excluding the time-index column.
#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    pub time: TimeAxis,
    pub time_column_name: String,
    pub columns: Vec<Series>,
}

/// Loads every row and column of the delimited-text file at `path` into
/// memory (see the module docs for the "small files" scope this is bounded
/// to). Column 0 is always the time index, the same convention
/// `ingest::report::inspect` uses (docs/QUALITY.md's torture corpus never
/// puts the time column anywhere else). A single-column file has no data
/// series to plot and is rejected as [`GlydeError::SingleColumnFile`],
/// exactly like `inspect`.
pub fn load(path: &Path) -> Result<Dataset> {
    let (outcome, mut columns_text) = open_path_capturing_all_columns(path)?;

    if outcome.column_names.len() < 2 {
        return Err(GlydeError::SingleColumnFile);
    }

    let time_column_name = outcome.column_names[0].clone();
    let time_fields = columns_text.remove(0);

    let time =
        match infer_timestamp_format(&time_fields) {
            Some(format_inference) => {
                let mut timestamps = Vec::with_capacity(time_fields.len());
                for field in &time_fields {
                    timestamps.push(parse_timestamp(field, format_inference.format)?);
                }
                TimeAxis::Absolute {
                    timestamps,
                    format: format_inference.format,
                }
            }
            // SPEC §2.1: no recognized absolute-timestamp format matched every
            // field, so this is a progressive numeric index (corpus case 35) —
            // unless it isn't even that, which is a real error, not a silent
            // empty plot.
            None => {
                let mut values = Vec::with_capacity(time_fields.len());
                for field in &time_fields {
                    let value = field.trim().parse::<f64>().map_err(|_| {
                        GlydeError::NonNumericTimeIndex {
                            input: field.clone(),
                        }
                    })?;
                    values.push(value);
                }
                TimeAxis::Progressive { values }
            }
        };

    let columns = outcome.column_names[1..]
        .iter()
        .zip(columns_text)
        .map(|(name, fields)| {
            let normalized: Vec<String> = fields
                .iter()
                .map(|field| normalize_decimal_field(field, outcome.decimal_separator).into_owned())
                .collect();
            infer_column(name.clone(), &normalized).series
        })
        .collect();

    Ok(Dataset {
        time,
        time_column_name,
        columns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::series::{Dtype, SeriesValues};
    use std::path::{Path, PathBuf};

    fn corpus_path(file_name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("testdata")
            .join("corpus")
            .join(file_name)
    }

    // Corpus case 1: a clean comma-delimited, dot-decimal file with an ISO
    // 8601 (`Z`-suffixed) time index. Every data column must materialize as
    // real `f64` samples, aligned one-to-one with the time axis.
    #[test]
    fn corpus_case_01_loads_a_clean_csv_into_a_dataset() {
        let dataset = load(&corpus_path("case-01-comma-clean.csv")).expect("case 1 must load");

        assert_eq!(dataset.time_column_name, "timestamp");
        assert_eq!(dataset.time.len(), 6);
        match &dataset.time {
            TimeAxis::Absolute { timestamps, format } => {
                assert_eq!(*format, TimestampFormat::Iso8601WithOffset);
                assert_eq!(timestamps.len(), 6);
            }
            TimeAxis::Progressive { .. } => panic!("case 1 has an absolute timestamp index"),
        }

        assert_eq!(dataset.columns.len(), 2);
        assert_eq!(dataset.columns[0].name(), "value");
        assert_eq!(
            dataset.columns[0].values(),
            &SeriesValues::F64(vec![1.5, 1.6, 1.7, 1.8, 1.9, 2.0])
        );
        assert_eq!(dataset.columns[1].name(), "pressure");
        assert_eq!(dataset.columns[1].dtype(), Dtype::F64);
        assert_eq!(dataset.columns[1].len(), 6);
    }

    // Corpus case 2: semicolon-delimited, comma-decimal (SPEC §1.2.4's
    // `1,5;2,3` trap). Proves the decimal-separator normalization is wired
    // in: without it, every value column would silently fall back to
    // `Dtype::String` instead of `F64`.
    #[test]
    fn corpus_case_02_comma_decimal_columns_infer_as_f64_not_string() {
        let dataset =
            load(&corpus_path("case-02-semicolon-comma-decimal.csv")).expect("case 2 must load");

        assert_eq!(
            dataset.columns[0].values(),
            &SeriesValues::F64(vec![1.5, 1.6, 1.7, 1.8, 1.9, 2.0])
        );
        assert_eq!(
            dataset.columns[1].values(),
            &SeriesValues::F64(vec![101.3, 101.4, 101.5, 101.6, 101.7, 101.8])
        );
    }

    // Corpus case 21: two of five data rows are ragged and must be skipped
    // (SPEC §1.3) — the time axis and every data column must end up the
    // same, shorter length, still aligned row-for-row.
    #[test]
    fn corpus_case_21_ragged_rows_are_skipped_and_stay_aligned() {
        let dataset = load(&corpus_path("case-21-ragged-rows.csv")).expect("case 21 must load");

        assert_eq!(dataset.time.len(), 3);
        assert_eq!(dataset.columns[0].len(), 3);
        assert_eq!(dataset.columns[1].len(), 3);
        assert_eq!(
            dataset.columns[0].values(),
            &SeriesValues::F64(vec![1.0, 1.3, 1.4])
        );
    }

    // Corpus case 35: a plain progressive integer index (no absolute-time
    // meaning) — must load as `TimeAxis::Progressive`, not fail or be
    // mistaken for a timestamp.
    #[test]
    fn corpus_case_35_progressive_index_loads_as_progressive_values() {
        let dataset =
            load(&corpus_path("case-35-progressive-integer-index.csv")).expect("case 35 must load");

        match &dataset.time {
            TimeAxis::Progressive { values } => {
                assert_eq!(values, &vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
            }
            TimeAxis::Absolute { .. } => panic!("case 35 has no absolute timestamp"),
        }
    }

    // Corpus case 18: only the time-index column, no data series to plot —
    // must fail cleanly (SPEC/QUALITY.md §1.18), never panic or silently
    // succeed with an empty dataset.
    #[test]
    fn corpus_case_18_single_column_file_is_a_clean_error() {
        let err = load(&corpus_path("case-18-single-column.csv"))
            .expect_err("a single-column file must be rejected");

        assert!(matches!(err, GlydeError::SingleColumnFile));
    }

    #[test]
    fn load_reports_a_missing_file_instead_of_panicking() {
        let err = load(Path::new("/nonexistent/glyde-dataset-test.csv"))
            .expect_err("a missing file must be a reported error");

        assert!(matches!(err, GlydeError::Io { .. }));
    }
}
