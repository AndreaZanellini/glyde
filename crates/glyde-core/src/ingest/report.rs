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
//! [`inspect`] examines column 0 as the time-index candidate, the same
//! automatic choice ingestion makes (SPEC §2.1): a numeric column that runs
//! backwards or never advances is a signal, not an index, and the summary then
//! reports the generated row index (corpus case 59). A time column anywhere
//! else is the user's one-click choice (`IngestOverrides::time_column`),
//! which `inspect` — a summary of the automatic open — does not take.

use super::csv::{open_path_capturing_column, CsvParseOutcome, SkippedRowDetail};
use super::dataset::{
    self, progressive_value_to_tick, Checkpoint, Dataset, GeneratedIndexReason, TimeAxis,
    TimeCandidateScan, TimeIndexInference,
};
use super::infer::Confidence;
use crate::time::{
    detect_monotonicity, detect_monotonicity_from, infer_timestamp_format, parse_timestamp,
    summarize_ticks, TimestampFormat,
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

/// SPEC §1.2 "Confidence is tracked per inference": one inferred value,
/// paired with how confidently it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct InferredField<T> {
    pub value: T,
    pub confidence: Confidence,
}

/// SPEC §2.1's timezone rule, summarized for the inference bar: "if the
/// source carries one, honor it and display it. If not, treat as naive local
/// time and label it as such." A per-row offset is already honored and
/// redisplayed by `crate::time::format_timestamp` (the axis and cursor
/// readout, `glyde-app::views::time`) — this is the one-line, whole-column
/// summary the inference bar shows instead, taken from the first parsed row.
#[derive(Debug, Clone, PartialEq)]
pub enum TimezoneLabel {
    /// The source carried an explicit UTC offset (SPEC §2.1
    /// [`TimestampFormat::Iso8601WithOffset`]), honored rather than
    /// discarded or converted. The `String` is a normalized `+HH:MM`
    /// display, e.g. `"+02:00"`.
    Honored(String),
    /// A numeric counter with a defined UTC epoch, even though individual
    /// values do not contain an explicit offset.
    UtcImplicit,
    /// No timezone in the source: SPEC §2.1's "treat as naive local time"
    /// default, made explicit rather than left for the user to assume.
    NaiveLocal,
}

/// A UTC offset in seconds as SPEC §2.1's `+HH:MM`/`-HH:MM` display text,
/// e.g. `7200` -> `"+02:00"`, `0` -> `"+00:00"`.
fn format_utc_offset(offset_seconds: i32) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let magnitude = offset_seconds.unsigned_abs();
    format!(
        "{sign}{:02}:{:02}",
        magnitude / 3600,
        (magnitude % 3600) / 60
    )
}

/// docs/ARCHITECTURE.md's `InferenceReport` (docs/ROADMAP.md M4 "surfaced to
/// the UI"): the SPEC §1.2 mandatory inference-bar fields — encoding,
/// delimiter, decimal separator, time column, timestamp format, sample
/// count, sampling classification — each paired with its own confidence
/// where SPEC §1.2/§2.1 define a real ambiguity signal for it.
/// `sample_count` and `sampling_class` are facts derived from the
/// already-parsed data, not guesses among competing readings, so they carry
/// no separate confidence field. `non_monotonic_count`, `duplicate_count`,
/// and `timezone` are SPEC §2.1's remaining timestamp affordances — likewise
/// facts about the already-parsed axis, not a competing reading to have a
/// confidence about.
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceReport {
    pub encoding: InferredField<String>,
    pub delimiter: InferredField<Option<String>>,
    pub decimal_separator: InferredField<Option<String>>,
    pub time_column: InferredField<Option<String>>,
    pub timestamp_format: InferredField<Option<String>>,
    pub sample_count: u64,
    pub sampling_class: SamplingClass,
    /// SPEC §1.3 "rows ... skipped, counted ... surfaced in the inference
    /// bar ('142 rows skipped — view details')" (docs/ROADMAP.md M4). Exact
    /// and unbounded, unlike `skipped_row_details` below.
    pub skipped_row_count: u64,
    /// A bounded sample of why rows were skipped (see
    /// `super::csv::MAX_SKIPPED_ROW_DETAILS`); use `skipped_row_count` for
    /// the exact total, and [`Self::skipped_row_details_truncated`] to know
    /// whether this list is a partial view of it.
    pub skipped_row_details: Vec<SkippedRowDetail>,
    /// SPEC §2.1: "non-monotonic timestamps: detected, counted, logged" —
    /// the inference bar's "[Sort]/[Keep as-is]" affordance shows when this
    /// is greater than zero.
    pub non_monotonic_count: u64,
    /// SPEC §2.1: "duplicate timestamps: preserved, flagged."
    pub duplicate_timestamp_count: u64,
    /// `None` when there is no absolute timestamp to have a timezone at all
    /// (a progressive numeric index, or the generated row index).
    pub timezone: Option<TimezoneLabel>,
    /// Where the time index came from: a column of the file, or the row
    /// index Glyde generated and why (SPEC §2.1 "Files without a time
    /// column"). `time_column` above is the corpus-facing summary of the same
    /// decision; this is what the inference bar explains and corrects.
    pub time_index: TimeIndexSource,
    /// Every column of the file, in header order — the time-column
    /// correction's choices (SPEC §1.2).
    pub column_names: Vec<String>,
}

/// Where a dataset's time index came from (see [`InferenceReport::time_index`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeIndexSource {
    /// Read from the column at this 0-based header position.
    Column { index: usize, name: String },
    /// Generated by Glyde: `0, 1, 2, …` over the kept rows, with every
    /// column of the file plotted as a series.
    Generated(GeneratedIndexReason),
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

    /// True when more rows were skipped than `skipped_row_details` retains
    /// — SPEC §1.3's "view details" then also needs to say "showing the
    /// first N of M" rather than implying the list is exhaustive.
    pub fn skipped_row_details_truncated(&self) -> bool {
        self.skipped_row_count > self.skipped_row_details.len() as u64
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

    let time_column_name = outcome.column_names.first().cloned().unwrap_or_default();
    let time_fields: Vec<&str> = time_column_text.iter().collect();
    let format_inference = infer_timestamp_format(&time_fields);
    let mut candidate = TimeCandidateScan::default();
    for field in &time_fields {
        candidate.observe(field);
    }
    // SPEC §2.1 "Files without a time column", the same verdict ingestion
    // reaches: when column 0 is not a time index it is a series, and the
    // generated row index stands in.
    let generated = candidate.verdict(format_inference.is_some(), true, &time_column_name);
    let time_column_index = generated.is_none().then_some(0);
    if dataset::has_nothing_to_plot(
        outcome.column_names.len(),
        time_column_index,
        generated.as_ref(),
    ) {
        return Err(GlydeError::SingleColumnFile);
    }

    let (
        time_column,
        timestamp_format,
        sampling_class,
        gap_count,
        non_monotonic_count,
        duplicate_timestamp_count,
    ) = match format_inference.filter(|_| generated.is_none()) {
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
        // meaning, so there is no timestamp format and no gap concept (corpus
        // case 35). Its order is still counted, like `build_summary_and_report`
        // does — a generated row index is in order by construction.
        None if generated.is_none() => {
            let ticks: Vec<i128> = time_fields
                .iter()
                .map(|field| {
                    field
                        .trim()
                        .parse::<f64>()
                        .map(progressive_value_to_tick)
                        .unwrap_or_default()
                })
                .collect();
            let monotonicity = detect_monotonicity(&ticks);
            (
                None,
                None,
                SamplingClass::ProgressiveIndex,
                0,
                monotonicity.non_monotonic_count as u64,
                monotonicity.duplicate_count as u64,
            )
        }
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
    let (outcome, dataset, time_index) = dataset::load_with_outcome(path)?;
    build_summary_and_report(outcome, dataset, time_index)
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
    let (outcome, dataset, time_index) =
        dataset::load_with_outcome_with_budget(path, budget, cache_dir)?;
    build_summary_and_report(outcome, dataset, time_index)
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
    let (outcome, dataset, time_index) =
        dataset::load_with_outcome_progressive(path, on_checkpoint)?;
    build_summary_and_report(outcome, dataset, time_index)
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
    let (outcome, dataset, time_index) =
        dataset::load_with_outcome_with_overrides(path, overrides)?;
    build_summary_and_report(outcome, dataset, time_index)
}

/// [`open_dataset_progressive`] with [`super::IngestOverrides`] applied — the
/// entry point `glyde-app`'s background indexer actually calls, since its
/// normal open always reports progress (docs/ROADMAP.md M3).
pub fn open_dataset_progressive_with_overrides(
    path: &Path,
    overrides: super::IngestOverrides,
    on_checkpoint: impl FnMut(Checkpoint),
) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let (outcome, dataset, time_index) =
        dataset::load_with_outcome_progressive_with_overrides(path, overrides, on_checkpoint)?;
    build_summary_and_report(outcome, dataset, time_index)
}

/// The summary/report-building half of [`open_dataset`], shared with
/// [`open_dataset_progressive`] so the two never independently (and
/// possibly divergently) derive the same fields from a parsed [`Dataset`].
fn build_summary_and_report(
    outcome: CsvParseOutcome,
    dataset: Dataset,
    time_index: TimeIndexInference,
) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let (
        time_column,
        timestamp_format,
        sampling_class,
        gap_count,
        non_monotonic_count,
        duplicate_timestamp_count,
        timezone,
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
                Some(timezone_label(*format, timestamps)),
            )
        }
        // SPEC §2.1: a progressive numeric index has no absolute-time
        // meaning (corpus case 35) — same as `inspect`'s progressive arms
        // above. Its order is still counted: a column the user picked as the
        // time index may run backwards, and SPEC §2.1's "[Sort]" must then be
        // on offer. A generated row index is in order by construction.
        TimeAxis::Progressive { .. } if time_index.generated.is_none() => {
            let monotonicity = detect_monotonicity_from(&dataset.time)?;
            (
                None,
                None,
                SamplingClass::ProgressiveIndex,
                0,
                monotonicity.non_monotonic_count as u64,
                monotonicity.duplicate_count as u64,
                None,
            )
        }
        TimeAxis::Progressive { .. } => {
            (None, None, SamplingClass::ProgressiveIndex, 0, 0, 0, None)
        }
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
    //
    // SPEC §2.1 "Files without a time column" / issue #94: a generated row
    // index is the other way this field cannot be trusted — unless the user
    // asked for it. The file *has* a first column; Glyde judged it not to be
    // an index and substituted row numbers, so reporting "no time column" at
    // full confidence (the way corpus case 35's genuine progressive index is
    // reported) would be exactly the silent guess Golden Rule 2 forbids. Low
    // confidence here is what opens the inference bar expanded on first
    // render (SPEC §1.2) instead of leaving the substitution to the log.
    let generated_on_users_behalf = time_index
        .generated
        .as_ref()
        .is_some_and(|reason| *reason != GeneratedIndexReason::Requested);
    let time_column_confidence = if outcome.header_ambiguous || generated_on_users_behalf {
        Confidence::Low
    } else {
        Confidence::High
    };
    // A progressive index has no timestamp format to be ambiguous about, so
    // it is reported with full confidence rather than inheriting whatever
    // `timestamp_format_ambiguous` happened to default to — unless there is no
    // format precisely because none could be read (issue #94), which is a
    // missing answer rather than a non-question.
    let timestamp_format_confidence = match (&dataset.time, &time_index.generated) {
        (TimeAxis::Absolute { .. }, _) if time_index.timestamp_format_ambiguous => Confidence::Low,
        (_, Some(GeneratedIndexReason::Unreadable { .. })) => Confidence::Low,
        _ => Confidence::High,
    };
    let time_index_source = match (time_index.time_column, time_index.generated) {
        (_, Some(reason)) => TimeIndexSource::Generated(reason),
        (Some(index), None) => TimeIndexSource::Column {
            index,
            name: outcome.column_names[index].clone(),
        },
        // Unreachable: ingestion reports a column or a reason, never neither.
        (None, None) => TimeIndexSource::Generated(GeneratedIndexReason::Requested),
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
        skipped_row_count: outcome.skipped_row_count,
        skipped_row_details: outcome.skipped_row_details,
        non_monotonic_count,
        duplicate_timestamp_count,
        timezone,
        time_index: time_index_source,
        column_names: outcome.column_names,
    };

    Ok((summary, report, dataset))
}

/// SPEC §2.1's timezone summary for an absolute-timestamp axis: `Honored`
/// with the first parsed row's UTC offset for
/// [`TimestampFormat::Iso8601WithOffset`], `UtcImplicit` for counters with a
/// defined UTC epoch, and `NaiveLocal` for dates without an offset (including
/// Excel serial dates, which specify a calendar origin but no timezone).
fn timezone_label(
    format: TimestampFormat,
    timestamps: &super::dataset::Timestamps,
) -> TimezoneLabel {
    match format {
        TimestampFormat::EpochSeconds
        | TimestampFormat::EpochMillis
        | TimestampFormat::EpochMicros
        | TimestampFormat::EpochNanos
        | TimestampFormat::LabViewEpoch => return TimezoneLabel::UtcImplicit,
        TimestampFormat::Iso8601WithOffset => {}
        TimestampFormat::Iso8601Naive
        | TimestampFormat::DateTimeSpace
        | TimestampFormat::DayFirst
        | TimestampFormat::MonthFirst
        | TimestampFormat::ExcelSerial => return TimezoneLabel::NaiveLocal,
    }
    match timestamps
        .iter()
        .next()
        .and_then(|timestamp| timestamp.offset_seconds)
    {
        Some(offset_seconds) => TimezoneLabel::Honored(format_utc_offset(offset_seconds)),
        None => TimezoneLabel::NaiveLocal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::dataset::load;
    use crate::ingest::IngestOverrides;
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

    // docs/ROADMAP.md M4 "Skipped-rows detail surface", SPEC §1.3: the
    // `InferenceReport` the UI actually renders must carry both the exact
    // count and the bounded detail sample, not just `OpenSummary`'s count.
    #[test]
    fn open_dataset_reports_skipped_row_details_for_ragged_rows() {
        let path = corpus_path("case-21-ragged-rows.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 21 must open");

        assert_eq!(report.skipped_row_count, 2);
        assert_eq!(report.skipped_row_details.len(), 2);
        assert_eq!(report.skipped_row_details[0].line_number, 3);
        assert_eq!(report.skipped_row_details[1].line_number, 4);
        assert!(!report.skipped_row_details_truncated());
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

    // SPEC §2.1's "non-monotonic ... counted" and "duplicate ... flagged"
    // must reach `InferenceReport`, not stop at `OpenSummary` (the same gap
    // the timestamp-format-ambiguity review follow-up above closed for that
    // field).
    #[test]
    fn inference_report_carries_the_non_monotonic_count_for_corpus_case_36() {
        let path = corpus_path("case-36-non-monotonic-timestamps.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 36 must open");

        assert_eq!(report.non_monotonic_count, 1);
        assert_eq!(report.duplicate_timestamp_count, 0);
    }

    #[test]
    fn inference_report_carries_the_duplicate_count_for_corpus_case_37() {
        let path = corpus_path("case-37-duplicate-timestamps.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 37 must open");

        assert_eq!(report.non_monotonic_count, 0);
        assert_eq!(report.duplicate_timestamp_count, 1);
    }

    // SPEC §2.1: "if the source carries [a timezone], honor it and display
    // it" — corpus case 24's `+02:00` offset must reach `InferenceReport`.
    #[test]
    fn inference_report_honors_the_timezone_for_corpus_case_24() {
        let path = corpus_path("case-24-iso8601-with-timezone.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 24 must open");

        assert_eq!(
            report.timezone,
            Some(TimezoneLabel::Honored("+02:00".to_string()))
        );
    }

    // SPEC §2.1: "if not, treat as naive local time and label it as such" —
    // corpus case 25 has no offset at all.
    #[test]
    fn inference_report_reports_naive_local_for_corpus_case_25() {
        let path = corpus_path("case-25-iso8601-naive.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 25 must open");

        assert_eq!(report.timezone, Some(TimezoneLabel::NaiveLocal));
    }

    #[test]
    fn numeric_utc_epochs_are_not_labeled_naive_local() {
        for file in [
            "case-29-epoch-seconds.csv",
            "case-30-epoch-milliseconds.csv",
            "case-31-epoch-microseconds.csv",
            "case-32-epoch-nanoseconds.csv",
            "case-34-labview-epoch.csv",
        ] {
            let (_, report, _) = open_dataset(&corpus_path(file)).expect(file);
            assert_eq!(report.timezone, Some(TimezoneLabel::UtcImplicit), "{file}");
        }
    }

    #[test]
    fn excel_serial_date_has_no_utc_timezone() {
        let (_, report, _) = open_dataset(&corpus_path("case-33-excel-serial-dates.csv"))
            .expect("case 33 must open");
        assert_eq!(report.timezone, Some(TimezoneLabel::NaiveLocal));
    }

    // A progressive numeric index (corpus case 35) has no timezone concept
    // at all — `None`, not a guessed `NaiveLocal`.
    #[test]
    fn inference_report_timezone_is_none_for_a_progressive_index() {
        let path = corpus_path("case-35-progressive-integer-index.csv");

        let (_summary, report, _dataset) = open_dataset(&path).expect("case 35 must open");

        assert_eq!(report.timezone, None);
    }

    // SPEC §2.1's "[Sort]" affordance, end to end: opening corpus case 36
    // with `IngestOverrides::sort_by_time` set must reorder both the time
    // axis and its paired `value` column, and the resulting report must show
    // the axis as monotonic again.
    #[test]
    fn sort_by_time_override_reorders_rows_and_clears_the_non_monotonic_count() {
        let path = corpus_path("case-36-non-monotonic-timestamps.csv");
        let overrides = IngestOverrides {
            sort_by_time: true,
            ..IngestOverrides::default()
        };

        let (_summary, report, dataset) =
            open_dataset_with_overrides(&path, overrides).expect("case 36 must open sorted");

        assert_eq!(report.non_monotonic_count, 0);
        // Case 36's two out-of-order rows share the exact same timestamp
        // (both `00:00:01`); before sorting they are not adjacent (a
        // `00:00:02` row sits between them), so `MonotonicityReport`'s
        // consecutive-pair definition counts that as one backward step, not
        // a duplicate. Sorting makes them adjacent, and a genuine duplicate
        // that scrambled order was hiding is exactly what SPEC §2.1's
        // "duplicate timestamps: preserved, flagged" should now surface —
        // sorting must never silently drop it.
        assert_eq!(report.duplicate_timestamp_count, 1);

        let TimeAxis::Absolute { timestamps, .. } = &dataset.time else {
            panic!("case 36's time index is an absolute timestamp column");
        };
        let ticks: Vec<i128> = timestamps.iter().map(|timestamp| timestamp.ticks).collect();
        assert!(
            ticks.windows(2).all(|pair| pair[0] <= pair[1]),
            "ticks must be non-decreasing after sorting: {ticks:?}"
        );

        let crate::series::SeriesValues::F64(values) = dataset.columns[0].values() else {
            panic!("case 36's value column is f64");
        };
        // Original rows (timestamp, value): (0,10.0) (1,10.1) (2,10.2)
        // (1,10.3) (3,10.4) (4,10.5). A stable sort by ascending timestamp
        // keeps the two t=1 rows in their original relative order (10.1
        // before 10.3), so `value` reorders to this exact sequence.
        assert_eq!(values, &vec![10.0, 10.1, 10.3, 10.2, 10.4, 10.5]);
    }

    // Without the override, case 36 must open unsorted exactly as before —
    // `sort_by_time` defaults to `false` and never changes behavior on its
    // own.
    #[test]
    fn without_the_override_corpus_case_36_stays_unsorted() {
        let path = corpus_path("case-36-non-monotonic-timestamps.csv");

        let (_summary, report, dataset) = open_dataset(&path).expect("case 36 must open");

        assert_eq!(report.non_monotonic_count, 1);
        let crate::series::SeriesValues::F64(values) = dataset.columns[0].values() else {
            panic!("case 36's value column is f64");
        };
        assert_eq!(values, &vec![10.0, 10.1, 10.2, 10.3, 10.4, 10.5]);
    }
}
