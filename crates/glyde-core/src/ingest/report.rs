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
//! This is deliberately a smaller, single-purpose pipeline than
//! docs/ARCHITECTURE.md's [`InferenceReport`] (docs/ROADMAP.md M4 "surfaced
//! to the UI"): no per-field confidence, no dtype, no pyramid/index build. It
//! exists to satisfy QUALITY.md §1's corpus gate for the inference already
//! implemented; Parquet (M7) is a separate, later item. [`open_dataset`]
//! builds both [`OpenSummary`] and [`InferenceReport`] from the one parse it
//! already performs, since the two serve different callers (the corpus gate
//! vs. `glyde-app`'s UI) rather than one superseding the other.
//!
//! Every torture-corpus case names or positions its time index as the first
//! column; none exercises a file where the time column sits elsewhere, so
//! [`inspect`] always treats column 0 as the time index rather than
//! searching for a candidate among several. Worth revisiting if a real file
//! ever puts the time column anywhere else.

use super::csv::{open_path_capturing_column, CsvParseOutcome};
use super::dataset::{self, Checkpoint, Dataset, TimeAxis};
use super::infer::Confidence;
use crate::time::{infer_timestamp_format, parse_timestamp, summarize_ticks, TimestampFormat};
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

/// SPEC §1.2 "Confidence is tracked per inference": one inferred value,
/// paired with how confidently it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct InferredField<T> {
    pub value: T,
    pub confidence: Confidence,
}

/// docs/ARCHITECTURE.md's `InferenceReport` (docs/ROADMAP.md M4 "surfaced to
/// the UI"): the SPEC §1.2 mandatory inference-bar fields — encoding,
/// delimiter, decimal separator, time column, timestamp format, sample
/// count, sampling classification — each paired with its own confidence
/// where SPEC §1.2/§2.1 define a real ambiguity signal for it.
/// `sample_count` and `sampling_class` are facts derived from the
/// already-parsed data, not guesses among competing readings, so they carry
/// no separate confidence field.
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceReport {
    pub encoding: InferredField<String>,
    pub delimiter: InferredField<Option<String>>,
    pub decimal_separator: InferredField<Option<String>>,
    pub time_column: InferredField<Option<String>>,
    pub timestamp_format: InferredField<Option<String>>,
    pub sample_count: u64,
    pub sampling_class: SamplingClass,
}

impl InferenceReport {
    /// SPEC §1.2 "[the inference bar] opens expanded when any inference is
    /// low-confidence" (docs/ROADMAP.md M4). Lives in `glyde-core`, not
    /// `glyde-app`, per docs/ARCHITECTURE.md's Hard rule 2 — the app only
    /// renders this decision, it does not make it.
    pub fn has_low_confidence_field(&self) -> bool {
        self.encoding.confidence == Confidence::Low
            || self.delimiter.confidence == Confidence::Low
            || self.decimal_separator.confidence == Confidence::Low
            || self.time_column.confidence == Confidence::Low
            || self.timestamp_format.confidence == Confidence::Low
    }
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
///
/// This parses `path` on its own, independently of [`super::dataset::load`].
/// A caller that also needs the materialized [`Dataset`] (as
/// `glyde-app`'s indexer does) should call [`open_dataset`] instead, which
/// produces both from a single parse (issue #58).
pub fn inspect(path: &Path) -> Result<OpenSummary> {
    let (outcome, time_column_text) = open_path_capturing_column(path, 0)?;

    if outcome.column_names.len() < 2 {
        return Err(GlydeError::SingleColumnFile);
    }

    let time_column_name = outcome.column_names[0].clone();
    let time_fields: Vec<&str> = time_column_text.iter().collect();

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
            let stats = summarize_ticks(ticks.as_slice())?;
            (
                Some(time_column_name),
                Some(timestamp_format_label(format_inference.format).to_string()),
                stats.sampling_class.into(),
                stats.gap_count as u64,
                stats.monotonicity.non_monotonic_count as u64,
                stats.monotonicity.duplicate_count as u64,
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

/// Parses `path` once and returns the [`OpenSummary`] [`inspect`] reports,
/// the [`InferenceReport`] `glyde-app`'s UI surfaces (docs/ROADMAP.md M4),
/// and the materialized [`Dataset`] [`super::dataset::load`] produces (issue
/// #58: `glyde-app`'s indexer used to call `inspect` then `load` back to
/// back, each independently memory-mapping, decoding, and streaming the
/// whole file — twice the I/O and CPU work for one open). `Dataset::time`'s
/// already-parsed ticks feed the same sampling classification, gap
/// detection, and monotonicity checks `inspect` runs, so the two summaries
/// agree by construction rather than by re-derivation.
pub fn open_dataset(path: &Path) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let (outcome, dataset, timestamp_format_ambiguous) = dataset::load_with_outcome(path)?;
    build_summary_and_report(outcome, dataset, timestamp_format_ambiguous)
}

/// [`open_dataset`] against an explicit RAM budget and spill directory, so a
/// test or diagnostic can exercise a specific storage choice rather than
/// whatever the host machine's RAM selects (issue #75) — the same split
/// [`crate::ingest::load_with_budget`] provides for [`dataset::load`].
pub fn open_dataset_with_budget(
    path: &Path,
    budget: crate::budget::RamBudget,
    cache_dir: &Path,
) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let (outcome, dataset, timestamp_format_ambiguous) =
        dataset::load_with_outcome_with_budget(path, budget, cache_dir)?;
    build_summary_and_report(outcome, dataset, timestamp_format_ambiguous)
}

/// [`open_dataset`], additionally invoking `on_checkpoint` with a
/// [`Checkpoint`] as the background parse progresses (docs/ROADMAP.md M3
/// "Background progressive build emitting partial levels") — see
/// [`dataset::load_with_outcome_progressive`], which this wraps the same way
/// [`open_dataset`] wraps [`dataset::load_with_outcome`]. The
/// [`OpenSummary`]/[`InferenceReport`] pair is only ever built once, from the
/// final, complete parse: a checkpoint's own [`Checkpoint::dataset`] is
/// already a real, renderable `Dataset` on its own, and re-deriving a full
/// sampling-classification/gap-detection summary at every checkpoint (as
/// opposed to only computing the pyramid, which [`dataset::load_with_outcome_progressive`]
/// already does per checkpoint) is not something any caller needs yet — SPEC
/// §1.2's inference bar is driven by the completed open, not a moving target.
pub fn open_dataset_progressive(
    path: &Path,
    on_checkpoint: impl FnMut(Checkpoint),
) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let (outcome, dataset, timestamp_format_ambiguous) =
        dataset::load_with_outcome_progressive(path, on_checkpoint)?;
    build_summary_and_report(outcome, dataset, timestamp_format_ambiguous)
}

/// [`open_dataset`] with [`super::IngestOverrides`] applied (docs/ROADMAP.md
/// M4 "One-click correction of each field → triggers a re-index",
/// docs/SPEC.md §1.2): each `Some` field in `overrides` bypasses its
/// inference step and is reported at full confidence, since a deliberate
/// user correction is never a guess (Golden Rule 2). `glyde-app`'s inference
/// bar calls this to re-open the current file after a one-click correction.
pub fn open_dataset_with_overrides(
    path: &Path,
    overrides: super::IngestOverrides,
) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let (outcome, dataset, timestamp_format_ambiguous) =
        dataset::load_with_outcome_with_overrides(path, overrides)?;
    build_summary_and_report(outcome, dataset, timestamp_format_ambiguous)
}

/// [`open_dataset_progressive`] with [`super::IngestOverrides`] applied — the
/// entry point `glyde-app`'s background indexer actually calls, since its
/// normal open always reports progress (docs/ROADMAP.md M3).
pub fn open_dataset_progressive_with_overrides(
    path: &Path,
    overrides: super::IngestOverrides,
    on_checkpoint: impl FnMut(Checkpoint),
) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let (outcome, dataset, timestamp_format_ambiguous) =
        dataset::load_with_outcome_progressive_with_overrides(path, overrides, on_checkpoint)?;
    build_summary_and_report(outcome, dataset, timestamp_format_ambiguous)
}

/// The summary/report-building half of [`open_dataset`], shared with
/// [`open_dataset_progressive`] so the two never independently (and
/// possibly divergently) derive the same fields from a parsed [`Dataset`].
fn build_summary_and_report(
    outcome: CsvParseOutcome,
    dataset: Dataset,
    timestamp_format_ambiguous: bool,
) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let (
        time_column,
        timestamp_format,
        sampling_class,
        gap_count,
        non_monotonic_count,
        duplicate_timestamp_count,
    ) = match &dataset.time {
        TimeAxis::Absolute { timestamps, format } => {
            // Read as a `TickSource`, never as one whole slice (issue #85): a
            // spilled axis hands its ticks over a buffer at a time, so the
            // summary of a 10 GB file costs the same memory as the summary of a
            // 10 MB one — SPEC §5's peak-RSS cap is a flat number, not a
            // fraction of file size.
            let stats = summarize_ticks(timestamps)?;
            (
                Some(dataset.time_column_name.clone()),
                Some(timestamp_format_label(*format).to_string()),
                stats.sampling_class.into(),
                stats.gap_count as u64,
                stats.monotonicity.non_monotonic_count as u64,
                stats.monotonicity.duplicate_count as u64,
            )
        }
        // SPEC §2.1: a progressive numeric index has no absolute-time
        // meaning (corpus case 35) — same as `inspect`'s `None` arm above.
        TimeAxis::Progressive { .. } => (None, None, SamplingClass::ProgressiveIndex, 0, 0, 0),
    };

    let summary = OpenSummary {
        encoding: outcome.encoding_label.clone(),
        delimiter: Some(outcome.delimiter.as_str().to_string()),
        decimal_separator: Some(outcome.decimal_separator.as_str().to_string()),
        time_column: time_column.clone(),
        timestamp_format: timestamp_format.clone(),
        row_count: outcome.row_count,
        skipped_row_count: outcome.skipped_row_count,
        sampling_class,
        gap_count,
        non_monotonic_count,
        duplicate_timestamp_count,
    };

    // SPEC §2.1: a column's *name* is only as trustworthy as the header
    // detection that produced it (`HeaderInference::ambiguous`) — an
    // ambiguous header means `time_column`'s value is itself a guess.
    let time_column_confidence = if outcome.header_ambiguous {
        Confidence::Low
    } else {
        Confidence::High
    };
    // A progressive index has no timestamp format to be ambiguous about, so
    // it is reported with full confidence rather than inheriting whatever
    // `timestamp_format_ambiguous` happened to default to.
    let timestamp_format_confidence = match &dataset.time {
        TimeAxis::Absolute { .. } if timestamp_format_ambiguous => Confidence::Low,
        _ => Confidence::High,
    };

    let report = InferenceReport {
        encoding: InferredField {
            value: outcome.encoding_label,
            confidence: outcome.encoding_confidence,
        },
        delimiter: InferredField {
            value: Some(outcome.delimiter.as_str().to_string()),
            confidence: outcome.delimiter_confidence,
        },
        decimal_separator: InferredField {
            value: Some(outcome.decimal_separator.as_str().to_string()),
            confidence: outcome.decimal_separator_confidence,
        },
        time_column: InferredField {
            value: time_column,
            confidence: time_column_confidence,
        },
        timestamp_format: InferredField {
            value: timestamp_format,
            confidence: timestamp_format_confidence,
        },
        sample_count: outcome.row_count,
        sampling_class,
    };

    Ok((summary, report, dataset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::dataset::load;
    use std::path::PathBuf;

    fn corpus_path(file_name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("testdata")
            .join("corpus")
            .join(file_name)
    }

    // Issue #58: `open_dataset` must agree with calling `inspect` and `load`
    // separately, for both an absolute-timestamp file (case 1) and a
    // progressive-index one (case 35) — the two code paths must never
    // silently drift apart now that they share one parse.
    #[test]
    fn open_dataset_agrees_with_inspect_and_load_for_an_absolute_timestamp_file() {
        let path = corpus_path("case-01-comma-clean.csv");

        let (summary, _report, dataset) = open_dataset(&path).expect("case 1 must open");
        let expected_summary = inspect(&path).expect("case 1 must inspect");
        let expected_dataset = load(&path).expect("case 1 must load");

        assert_eq!(summary, expected_summary);
        assert_eq!(dataset, expected_dataset);
    }

    #[test]
    fn open_dataset_agrees_with_inspect_and_load_for_a_progressive_index_file() {
        let path = corpus_path("case-35-progressive-integer-index.csv");

        let (summary, _report, dataset) = open_dataset(&path).expect("case 35 must open");
        let expected_summary = inspect(&path).expect("case 35 must inspect");
        let expected_dataset = load(&path).expect("case 35 must load");

        assert_eq!(summary, expected_summary);
        assert_eq!(dataset, expected_dataset);
    }

    // Corpus case 21: ragged rows are skipped on both paths; the unified
    // parse must still land on the same summary and dataset as before.
    #[test]
    fn open_dataset_agrees_with_inspect_and_load_for_ragged_rows() {
        let path = corpus_path("case-21-ragged-rows.csv");

        let (summary, _report, dataset) = open_dataset(&path).expect("case 21 must open");
        let expected_summary = inspect(&path).expect("case 21 must inspect");
        let expected_dataset = load(&path).expect("case 21 must load");

        assert_eq!(summary, expected_summary);
        assert_eq!(dataset, expected_dataset);
    }

    // docs/ROADMAP.md M4 "InferenceReport surfaced to the UI ... proven by:
    // report-struct snapshot". A stable, real-fixture snapshot of every
    // field `InferenceReport` carries, so an unintended change to any of
    // them (a field silently dropped, a confidence rule silently changed)
    // shows up as a diff a reviewer must explicitly accept.
    #[test]
    fn inference_report_snapshot_for_a_clean_comma_file() {
        let path = corpus_path("case-01-comma-clean.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 1 must open");

        insta::assert_debug_snapshot!("inference_report_case_01_comma_clean", report);
    }

    // Corpus case 8 exercises a real, low-confidence encoding guess
    // (windows-1252 via `chardetng`, not a BOM or the tolerant-UTF-8 fast
    // path) so the snapshot captures a `Confidence::Low` field, not only the
    // all-`High` shape of a clean file.
    #[test]
    fn inference_report_snapshot_for_a_low_confidence_encoding_file() {
        let path = corpus_path("case-08-latin1-degree-micro.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 8 must open");

        assert_eq!(report.encoding.confidence, Confidence::Low);
        insta::assert_debug_snapshot!("inference_report_case_08_low_confidence_encoding", report);
    }

    // Corpus case 28: every row's date is genuinely ambiguous (no field > 12
    // in either slash position), so SPEC §2.1's ambiguity rule falls back to
    // the ISO-leaning `DD/MM` default — this is the exact case
    // `TimestampFormatInference::ambiguous` exists to flag, and it must
    // reach `InferenceReport::timestamp_format`, not stop at `OpenSummary`
    // (review follow-up on PR #70: this path had only lower-level
    // `TimestampFormatInference` unit-test coverage before).
    #[test]
    fn inference_report_reports_low_confidence_timestamp_format_for_fully_ambiguous_dates() {
        let path = corpus_path("case-28-fully-ambiguous-dates.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 28 must open");

        assert_eq!(
            report.timestamp_format.value,
            Some("dd_mm_yyyy".to_string())
        );
        assert_eq!(report.timestamp_format.confidence, Confidence::Low);
        // Every other field in this file is unambiguous; only the date
        // format itself is a guess.
        assert_eq!(report.encoding.confidence, Confidence::High);
        assert_eq!(report.delimiter.confidence, Confidence::High);
        assert_eq!(report.time_column.confidence, Confidence::High);
    }

    // No corpus case (the fixed 56-case set) exercises a header preamble
    // that never matches the data rows' field count (`HeaderInference::
    // ambiguous`), so this builds one inline via a real temp file — the same
    // review follow-up as above, for `InferenceReport::time_column`. A
    // single-field preamble line ("notes") above two-field ISO-timestamp
    // data rows can never match the data field count, which is exactly
    // `infer_header`'s `ambiguous` condition.
    #[test]
    fn inference_report_reports_low_confidence_time_column_for_an_ambiguous_header() {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        std::io::Write::write_all(
            &mut file,
            b"notes\n\
              2026-01-01T00:00:00Z,1.5\n\
              2026-01-01T00:00:01Z,1.6\n\
              2026-01-01T00:00:02Z,1.7\n",
        )
        .expect("write temp file");

        let (_summary, report, dataset) =
            open_dataset(file.path()).expect("ambiguous-header file must open");

        assert_eq!(dataset.time_column_name, "column_0");
        assert_eq!(report.time_column.value, Some("column_0".to_string()));
        assert_eq!(report.time_column.confidence, Confidence::Low);
        // The timestamp values themselves are unambiguous ISO 8601 — only
        // the column *name* is a guess here.
        assert_eq!(report.timestamp_format.confidence, Confidence::High);
    }

    #[test]
    fn open_dataset_reports_a_missing_file_instead_of_panicking() {
        let err = open_dataset(Path::new("/nonexistent/glyde-report-test.csv"))
            .expect_err("a missing file must be a reported error");

        assert!(matches!(err, GlydeError::Io { .. }));
    }

    // SPEC §1.2 / docs/ROADMAP.md M4's inference-bar expand trigger: a fully
    // unambiguous file must never claim low confidence anywhere.
    #[test]
    fn has_low_confidence_field_is_false_for_an_unambiguous_file() {
        let path = corpus_path("case-01-comma-clean.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 1 must open");

        assert!(!report.has_low_confidence_field());
    }

    // Case 28's only low-confidence field is `timestamp_format`; that alone
    // must be enough to flip the whole-report flag the inference bar reads.
    #[test]
    fn has_low_confidence_field_is_true_when_one_field_is_low() {
        let path = corpus_path("case-28-fully-ambiguous-dates.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 28 must open");

        assert!(report.has_low_confidence_field());
    }
}
