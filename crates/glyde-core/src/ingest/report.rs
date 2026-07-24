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

//! The torture-corpus open-vs-`.expected.json` comparison shape
//! (docs/QUALITY.md §1, docs/ROADMAP.md M2 "Activate corpus open→compare
//! gate for all cases handled so far"). [`OpenSummary`] mirrors the
//! `.expected.json` schema field for field; [`inspect`] is the pipeline that
//! produces one from a real delimited-text file by wiring together every
//! inference piece the roadmap has landed so far: encoding → delimiter →
//! header → decimal separator (reported only; no value parsed for this
//! summary depends on it) → the first column as the time index →
//! `time::infer_timestamp_format` → sampling classification, gap detection,
//! and monotonicity.
//!
//! This is deliberately a smaller, single-purpose pipeline, not
//! docs/ARCHITECTURE.md's future `InferenceReport` (docs/ROADMAP.md M4): no
//! per-field confidence, no dtype, no pyramid/index build. It exists to
//! satisfy QUALITY.md §1's corpus gate for the inference already implemented;
//! Parquet (M7) and the richer M4 report are separate, later items.
//!
//! Every torture-corpus case names or positions its time index as the first
//! column; none exercises a file where the time column sits elsewhere, so
//! [`inspect`] always treats column 0 as the time index rather than
//! searching for a candidate among several. Worth revisiting if a real file
//! ever puts the time column anywhere else.

use super::csv::open_path_capturing_column;
use crate::time::{
    classify_sampling, detect_gaps, detect_monotonicity, infer_timestamp_format, parse_timestamp,
    TimestampFormat,
};
use crate::{GlydeError, Result};
use std::path::Path;

/// docs/QUALITY.md §1's "sampling class" field, extended with
/// [`SamplingClass::ProgressiveIndex`] for SPEC §2.1's "progressive numeric"
/// index kind (corpus case 35) — a valid index with no absolute-time
/// meaning. `time::SamplingClass` has no such variant because it classifies
/// the *distribution of Δt*, a concept that only applies once a column has
/// already been recognized as an absolute timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SamplingClass {
    Uniform,
    SegmentedUniform,
    Irregular,
    ProgressiveIndex,
}

impl From<crate::time::SamplingClass> for SamplingClass {
    fn from(class: crate::time::SamplingClass) -> Self {
        match class {
            crate::time::SamplingClass::Uniform => SamplingClass::Uniform,
            crate::time::SamplingClass::SegmentedUniform => SamplingClass::SegmentedUniform,
            crate::time::SamplingClass::Irregular => SamplingClass::Irregular,
        }
    }
}

/// What a correct open of a file produces, mirroring docs/QUALITY.md §1's
/// `.expected.json` schema field for field.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct OpenSummary {
    pub encoding: String,
    pub delimiter: Option<String>,
    pub decimal_separator: Option<String>,
    pub time_column: Option<String>,
    pub timestamp_format: Option<String>,
    pub row_count: u64,
    pub skipped_row_count: u64,
    pub sampling_class: SamplingClass,
    pub gap_count: u64,
    /// SPEC §2.1: "non-monotonic timestamps: detected, counted, logged."
    /// Defaults to 0 so corpus cases unrelated to this check need no
    /// `.expected.json` update.
    #[serde(default)]
    pub non_monotonic_count: u64,
    /// SPEC §2.1: "duplicate timestamps: preserved, flagged." Defaults to 0
    /// for the same reason as `non_monotonic_count`.
    #[serde(default)]
    pub duplicate_timestamp_count: u64,
}

/// The `.expected.json` vocabulary name for `format` (docs/QUALITY.md §1's
/// `timestamp_format` field) — naming invented in the M1 PR that committed
/// the time-index corpus fixtures, before any of this code existed.
fn timestamp_format_label(format: TimestampFormat) -> &'static str {
    match format {
        TimestampFormat::Iso8601WithOffset => "iso8601",
        TimestampFormat::Iso8601Naive => "iso8601_naive",
        TimestampFormat::DateTimeSpace => "datetime_space",
        TimestampFormat::DayFirst => "dd_mm_yyyy",
        TimestampFormat::MonthFirst => "mm_dd_yyyy",
        TimestampFormat::EpochSeconds => "epoch_s",
        TimestampFormat::EpochMillis => "epoch_ms",
        TimestampFormat::EpochMicros => "epoch_us",
        TimestampFormat::EpochNanos => "epoch_ns",
        TimestampFormat::LabViewEpoch => "labview_epoch",
        TimestampFormat::ExcelSerial => "excel_serial",
    }
}

/// Wires together every inference piece docs/ROADMAP.md M2 has landed so far
/// into one [`OpenSummary`], for a delimited-text (`.csv`/`.tsv`/`.txt`) file
/// at `path`. A single-column file has only a time index and no data series
/// to plot, and is rejected as [`GlydeError::SingleColumnFile`] (corpus case
/// 18) rather than silently "succeeding" with nothing to show.
pub fn inspect(path: &Path) -> Result<OpenSummary> {
    let (outcome, time_fields) = open_path_capturing_column(path, 0)?;

    if outcome.column_names.len() < 2 {
        return Err(GlydeError::SingleColumnFile);
    }

    let time_column_name = outcome.column_names[0].clone();

    let (
        time_column,
        timestamp_format,
        sampling_class,
        gap_count,
        non_monotonic_count,
        duplicate_timestamp_count,
    ) = match infer_timestamp_format(&time_fields) {
        Some(format_inference) => {
            let mut ticks = Vec::with_capacity(time_fields.len());
            for field in &time_fields {
                ticks.push(parse_timestamp(field, format_inference.format)?.ticks);
            }
            let sampling_class: SamplingClass = classify_sampling(&ticks).into();
            let gap_count = detect_gaps(&ticks).len() as u64;
            let monotonicity = detect_monotonicity(&ticks);
            (
                Some(time_column_name),
                Some(timestamp_format_label(format_inference.format).to_string()),
                sampling_class,
                gap_count,
                monotonicity.non_monotonic_count as u64,
                monotonicity.duplicate_count as u64,
            )
        }
        // SPEC §2.1: a progressive numeric index has no absolute-time
        // meaning, so there is no timestamp format, no gap concept, and no
        // monotonicity check to run against it (corpus case 35).
        None => (None, None, SamplingClass::ProgressiveIndex, 0, 0, 0),
    };

    Ok(OpenSummary {
        encoding: outcome.encoding_label,
        delimiter: Some(outcome.delimiter.as_str().to_string()),
        decimal_separator: Some(outcome.decimal_separator.as_str().to_string()),
        time_column,
        timestamp_format,
        row_count: outcome.row_count,
        skipped_row_count: outcome.skipped_row_count,
        sampling_class,
        gap_count,
        non_monotonic_count,
        duplicate_timestamp_count,
    })
}
