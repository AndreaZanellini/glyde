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

use super::csv::{
    open_path_capturing_all_columns, open_path_capturing_all_columns_with_progress, ColumnText,
    CsvParseOutcome,
};
use super::infer::{infer_column, normalize_decimal_field};
use crate::dsp::decimation::{build_pyramid, Bucket};
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

    /// Converts this axis into the `i128` tick space
    /// [`crate::dsp::decimation::build_pyramid`]/
    /// [`crate::dsp::decimation::decimate_viewport`] operate on (issue #60
    /// decision, docs/ARCHITECTURE.md §The index).
    ///
    /// `Absolute` ticks are each [`Timestamp`]'s own `ticks` field verbatim —
    /// every timestamp in one axis was parsed against the same detected
    /// [`TimestampFormat`], hence shares one [`crate::time::TimeUnit`], so
    /// the values are already on one consistent scale. `Progressive` values
    /// have no calendar meaning or unit of their own, so they are mapped
    /// through [`progressive_value_to_tick`] fixed-point scaling — this
    /// preserves true x-distance between samples, so an unevenly-spaced
    /// progressive axis decimates the same way an absolute-time axis with
    /// identical physical spacing would, rather than aggregating by sample
    /// ordinal.
    ///
    /// `dsp::decimation` never interprets the ticks it is handed; only the
    /// caller (via [`progressive_tick_to_value`] for `Progressive`, or a
    /// `Timestamp`'s own `unit` for `Absolute`) knows how to convert them
    /// back to a display value.
    pub fn to_pyramid_ticks(&self) -> Vec<i128> {
        match self {
            TimeAxis::Absolute { timestamps, .. } => timestamps.iter().map(|t| t.ticks).collect(),
            TimeAxis::Progressive { values } => values
                .iter()
                .copied()
                .map(progressive_value_to_tick)
                .collect(),
        }
    }
}

/// Fixed-point scale applied to a [`TimeAxis::Progressive`] axis's `f64`
/// values to obtain pyramid ticks (issue #60 decision, recorded in
/// docs/ARCHITECTURE.md §The index). Matches the finest resolution already
/// carried by absolute timestamps (`TimeUnit::Nanoseconds`'s ×10⁹), chosen so
/// realistic progressive-index magnitudes and fractional precision survive
/// the round trip; values whose magnitude approaches `i128::MAX / 1e9`
/// (~1.7×10²⁹) or whose meaningful precision exceeds nine fractional digits
/// are outside what this scale can represent exactly. Progressive numeric
/// indices are not expected to reach either extreme in practice (SPEC
/// §2.1); this is an assumption flagged in `CHANGELOG.md`, not something
/// SPEC.md names.
pub const PROGRESSIVE_TICK_SCALE: f64 = 1e9;

/// Scales one [`TimeAxis::Progressive`] value into a pyramid tick (see
/// [`PROGRESSIVE_TICK_SCALE`]). Rounds to the nearest tick; the float-to-int
/// cast saturates rather than panicking on out-of-range input (Rust's
/// defined `as` semantics), which only matters for the extreme magnitudes
/// documented on [`PROGRESSIVE_TICK_SCALE`].
pub fn progressive_value_to_tick(value: f64) -> i128 {
    (value * PROGRESSIVE_TICK_SCALE).round() as i128
}

/// Inverse of [`progressive_value_to_tick`]: recovers a pyramid tick's
/// original `Progressive` axis value for display (e.g. an axis tick label or
/// cursor readout).
pub fn progressive_tick_to_value(tick: i128) -> f64 {
    tick as f64 / PROGRESSIVE_TICK_SCALE
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
    load_with_outcome(path).map(|(_outcome, dataset, _timestamp_format_ambiguous)| dataset)
}

/// [`load`], additionally returning the [`CsvParseOutcome`] the single parse
/// pass already produced (encoding, delimiter, decimal separator, row
/// counts) and whether the time column's timestamp format was an SPEC §2.1
/// ambiguity-rule fallback rather than a confident match (`false` for a
/// [`TimeAxis::Progressive`] index, which has no timestamp format to be
/// ambiguous about). `ingest::report::open_dataset` uses this so that
/// materializing a [`Dataset`] and reporting an [`super::OpenSummary`] /
/// [`super::InferenceReport`] for it share one parse of the file instead of
/// independent ones (issue #58: the app used to call `inspect()` and
/// `load()` back to back, each re-reading and re-decoding the whole file).
pub(crate) fn load_with_outcome(path: &Path) -> Result<(CsvParseOutcome, Dataset, bool)> {
    let (outcome, columns_text) = open_path_capturing_all_columns(path)?;
    let (dataset, timestamp_format_ambiguous) = build_dataset(&outcome, &columns_text)?;
    Ok((outcome, dataset, timestamp_format_ambiguous))
}

/// The typed-conversion half of [`load_with_outcome`]: every column's raw
/// captured text, already fully read by [`super::csv`], into a [`Dataset`].
/// Split out so [`load_with_outcome_progressive`] can run the exact same
/// conversion against a growing prefix of `columns_text` at each checkpoint,
/// rather than a second, drifting implementation of the same logic
/// (docs/ROADMAP.md M3 "Background progressive build emitting partial
/// levels").
fn build_dataset(
    outcome: &CsvParseOutcome,
    columns_text: &[ColumnText],
) -> Result<(Dataset, bool)> {
    if outcome.column_names.len() < 2 {
        return Err(GlydeError::SingleColumnFile);
    }

    let time_column_name = outcome.column_names[0].clone();
    let time_fields: Vec<&str> = columns_text[0].iter().collect();

    let (time, timestamp_format_ambiguous) =
        match infer_timestamp_format(&time_fields) {
            Some(format_inference) => {
                let mut timestamps = Vec::with_capacity(time_fields.len());
                for field in &time_fields {
                    timestamps.push(parse_timestamp(field, format_inference.format)?);
                }
                (
                    TimeAxis::Absolute {
                        timestamps,
                        format: format_inference.format,
                    },
                    format_inference.ambiguous,
                )
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
                            input: field.to_string(),
                        }
                    })?;
                    values.push(value);
                }
                (TimeAxis::Progressive { values }, false)
            }
        };

    let columns = outcome.column_names[1..]
        .iter()
        .zip(&columns_text[1..])
        .map(|(name, column_text)| {
            // `normalize_decimal_field` keeps `Cow::Borrowed` when the field
            // needs no rewrite (the common dot-decimal case): `infer_column`
            // is generic over `AsRef<str>`, so this never forces an owned
            // copy just to satisfy its signature (issue #62).
            let normalized: Vec<std::borrow::Cow<'_, str>> = column_text
                .iter()
                .map(|field| normalize_decimal_field(field, outcome.decimal_separator))
                .collect();
            infer_column(name.clone(), &normalized).series
        })
        .collect();

    Ok((
        Dataset {
            time,
            time_column_name,
            columns,
        },
        timestamp_format_ambiguous,
    ))
}

/// One progress update from [`load_with_outcome_progressive`]: a real
/// [`Dataset`] built from the rows read so far, plus that dataset's own
/// min/max pyramid for every numeric column (docs/ROADMAP.md M3 "Background
/// progressive build emitting partial levels", docs/ARCHITECTURE.md
/// §pipeline: "first level ready → first plot"). `pyramids` is parallel to
/// `dataset.columns` — `None` at an index whose column is `Bool`/`String`
/// (state-timeline dtypes have no numeric pyramid, SPEC §4.3), `Some` for
/// every numeric one, built by the same golden-tested
/// [`build_pyramid`](crate::dsp::decimation::build_pyramid) the final,
/// complete dataset would use — a checkpoint's pyramid is a real, exact
/// pyramid over the samples read so far, never an approximation.
pub struct Checkpoint {
    pub dataset: Dataset,
    pub pyramids: Vec<Option<Vec<Vec<Bucket>>>>,
    pub rows_read: u64,
}

/// [`load_with_outcome`], additionally invoking `on_checkpoint` with a
/// [`Checkpoint`] at each progress update `super::csv`'s row-count-doubling
/// schedule fires (docs/ROADMAP.md M3 "Background progressive build emitting
/// partial levels", SPEC §5 "first meaningful plot ... ≤ 2s ... render what
/// is indexed, keep indexing in background"). Every checkpoint is a real
/// [`Dataset`] built by the same [`build_dataset`] conversion the final
/// result uses — never a resampled or approximated preview — so a caller can
/// render it exactly like a completed open, just with fewer rows.
///
/// A checkpoint whose prefix does not itself build into a valid `Dataset`
/// (e.g. too few progressive-index rows parsed as `f64` for
/// [`infer_timestamp_format`] to have committed to an absolute format yet) is
/// not treated as a hard error — only the final, complete parse's result is
/// ever returned as one; a transient mid-stream checkpoint failure just skips
/// that one progress update, logged at `warn` (docs/CLAUDE.md "never `panic!`
/// on malformed user data", applied here to a checkpoint's own internal
/// consistency rather than the source file).
pub(crate) fn load_with_outcome_progressive(
    path: &Path,
    mut on_checkpoint: impl FnMut(Checkpoint),
) -> Result<(CsvParseOutcome, Dataset, bool)> {
    let (outcome, columns_text) = open_path_capturing_all_columns_with_progress(
        path,
        |partial_outcome, partial_columns| match build_dataset(partial_outcome, partial_columns) {
            Ok((dataset, _ambiguous)) => {
                let pyramids = pyramids_for(&dataset);
                on_checkpoint(Checkpoint {
                    rows_read: partial_outcome.row_count,
                    pyramids,
                    dataset,
                });
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    rows_read = partial_outcome.row_count,
                    "progressive checkpoint could not be materialized as a dataset yet, skipping this progress update"
                );
            }
        },
    )?;

    let (dataset, timestamp_format_ambiguous) = build_dataset(&outcome, &columns_text)?;
    Ok((outcome, dataset, timestamp_format_ambiguous))
}

/// [`load`], additionally reporting progress like [`load_with_outcome_progressive`].
pub fn load_progressive(path: &Path, on_checkpoint: impl FnMut(Checkpoint)) -> Result<Dataset> {
    load_with_outcome_progressive(path, on_checkpoint)
        .map(|(_outcome, dataset, _ambiguous)| dataset)
}

/// `dataset`'s own min/max pyramid, one entry per column in `dataset.columns`
/// order (see [`Checkpoint::pyramids`]).
fn pyramids_for(dataset: &Dataset) -> Vec<Option<Vec<Vec<Bucket>>>> {
    let ticks = dataset.time.to_pyramid_ticks();
    dataset
        .columns
        .iter()
        .map(|series| {
            series
                .values()
                .to_f64_vec()
                .map(|samples| build_pyramid(&samples, &ticks))
        })
        .collect()
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

    // Issue #60 decision (solution B): `Absolute` ticks pass through
    // untouched — every timestamp in an axis already shares one `TimeUnit`
    // from the detected format, so there is nothing to scale.
    #[test]
    fn to_pyramid_ticks_on_absolute_axis_returns_each_timestamps_own_ticks() {
        use crate::time::TimeUnit;

        let time = TimeAxis::Absolute {
            timestamps: vec![
                Timestamp::new(0, TimeUnit::Nanoseconds),
                Timestamp::new(1_500_000_000, TimeUnit::Nanoseconds),
                Timestamp::new(3_000_000_000, TimeUnit::Nanoseconds),
            ],
            format: TimestampFormat::EpochNanos,
        };

        assert_eq!(
            time.to_pyramid_ticks(),
            vec![0, 1_500_000_000, 3_000_000_000]
        );
    }

    // Issue #60 decision: `Progressive` values are scaled by
    // `PROGRESSIVE_TICK_SCALE` (×1e9) so the pyramid aggregates by true
    // x-distance, not by sample ordinal.
    #[test]
    fn to_pyramid_ticks_on_progressive_axis_scales_by_the_fixed_point_factor() {
        let time = TimeAxis::Progressive {
            values: vec![0.0, 1.0, 2.5, -3.25],
        };

        assert_eq!(
            time.to_pyramid_ticks(),
            vec![0, 1_000_000_000, 2_500_000_000, -3_250_000_000]
        );
    }

    // Corpus case 35's real progressive values, run through the same
    // conversion the future pyramid-building call site will use — ticks
    // must stay non-decreasing (`build_pyramid`/`decimate_viewport`'s
    // precondition on their `timestamps` argument).
    #[test]
    fn to_pyramid_ticks_on_progressive_axis_preserves_monotonicity() {
        let dataset =
            load(&corpus_path("case-35-progressive-integer-index.csv")).expect("case 35 must load");

        let ticks = dataset.time.to_pyramid_ticks();
        assert_eq!(
            ticks,
            vec![
                0,
                1_000_000_000,
                2_000_000_000,
                3_000_000_000,
                4_000_000_000,
                5_000_000_000
            ]
        );
        assert!(
            ticks.windows(2).all(|pair| pair[0] <= pair[1]),
            "a monotonic progressive axis must scale into a non-decreasing tick sequence"
        );
    }

    // `progressive_tick_to_value` must exactly invert
    // `progressive_value_to_tick` for values representable at the fixed
    // ×1e9 resolution (issue #60's documented scale limits).
    #[test]
    fn progressive_value_and_tick_round_trip() {
        for value in [0.0, 1.0, -1.0, 0.1, 123.456, -9_999.5, 1e6, -1e-6] {
            let tick = progressive_value_to_tick(value);
            let recovered = progressive_tick_to_value(tick);
            assert!(
                (recovered - value).abs() < 1e-6,
                "value {value} round-tripped to {recovered} through tick {tick}"
            );
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

    /// A synthetic progressive-index CSV large enough to cross
    /// `super::csv`'s first progress checkpoint at least once.
    fn many_rows_temp_csv(row_count: u64) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        let mut text = String::from("index,value\n");
        for i in 0..row_count {
            text.push_str(&format!("{i},{}\n", i as f64 * 0.5));
        }
        std::io::Write::write_all(&mut file, text.as_bytes()).expect("write temp file");
        file
    }

    // docs/ROADMAP.md M3 "Background progressive build emitting partial
    // levels": every checkpoint's dataset must be a true prefix of the final
    // dataset — same values, just fewer rows — and the final result returned
    // by `load_with_outcome_progressive` must equal plain `load_with_outcome`
    // on the same file (progress reporting must never change the outcome).
    #[test]
    fn load_with_outcome_progressive_checkpoints_are_true_prefixes_of_the_final_dataset() {
        let file = many_rows_temp_csv(70_000);
        let mut checkpoints: Vec<Checkpoint> = Vec::new();

        let (_outcome, final_dataset, _ambiguous) =
            load_with_outcome_progressive(file.path(), |checkpoint| checkpoints.push(checkpoint))
                .expect("progressive load must succeed");
        let (_outcome2, expected_dataset, _ambiguous2) =
            load_with_outcome(file.path()).expect("non-progressive load must succeed");

        assert_eq!(
            final_dataset, expected_dataset,
            "progress reporting must not change the final dataset"
        );
        assert!(
            checkpoints.len() >= 2,
            "a 70k-row fixture must cross the first two checkpoints (20k, 40k)"
        );

        for checkpoint in &checkpoints {
            assert_eq!(checkpoint.dataset.time.len(), checkpoint.rows_read as usize);
            assert_eq!(
                checkpoint.dataset.time,
                {
                    let TimeAxis::Progressive { values } = &final_dataset.time else {
                        panic!("expected a progressive index");
                    };
                    TimeAxis::Progressive {
                        values: values[..checkpoint.rows_read as usize].to_vec(),
                    }
                },
                "a checkpoint's time axis must be an exact prefix of the final one"
            );
            assert_eq!(
                checkpoint.dataset.columns.len(),
                final_dataset.columns.len()
            );
            for (checkpoint_col, final_col) in checkpoint
                .dataset
                .columns
                .iter()
                .zip(&final_dataset.columns)
            {
                let SeriesValues::F64(checkpoint_values) = checkpoint_col.values() else {
                    panic!("expected f64 columns");
                };
                let SeriesValues::F64(final_values) = final_col.values() else {
                    panic!("expected f64 columns");
                };
                assert_eq!(checkpoint_values, &final_values[..checkpoint_values.len()]);
            }
        }
    }

    // The pyramid attached to each checkpoint must be exactly what
    // `build_pyramid` would compute directly over that checkpoint's own
    // samples — a real, exact aggregation of the rows read so far, not an
    // approximation (docs/ROADMAP.md M3 "emitting partial levels").
    #[test]
    fn load_with_outcome_progressive_checkpoint_pyramids_match_build_pyramid_on_the_same_prefix() {
        let file = many_rows_temp_csv(70_000);
        let mut checkpoints: Vec<Checkpoint> = Vec::new();

        load_with_outcome_progressive(file.path(), |checkpoint| checkpoints.push(checkpoint))
            .expect("progressive load must succeed");

        assert!(!checkpoints.is_empty());
        for checkpoint in &checkpoints {
            let ticks = checkpoint.dataset.time.to_pyramid_ticks();
            assert_eq!(checkpoint.pyramids.len(), checkpoint.dataset.columns.len());
            for (pyramid, column) in checkpoint.pyramids.iter().zip(&checkpoint.dataset.columns) {
                let samples = column.values().to_f64_vec().expect("numeric column");
                let expected = crate::dsp::decimation::build_pyramid(&samples, &ticks);
                assert_eq!(pyramid.as_ref(), Some(&expected));
            }
        }
    }

    // A file too small to ever cross the first checkpoint must still load
    // correctly via the progressive path, simply never invoking the
    // callback.
    #[test]
    fn load_with_outcome_progressive_never_checkpoints_a_small_file() {
        let file = many_rows_temp_csv(5);
        let mut checkpoint_count = 0;

        let (_outcome, dataset, _ambiguous) =
            load_with_outcome_progressive(file.path(), |_checkpoint| checkpoint_count += 1)
                .expect("progressive load of a small file must succeed");

        assert_eq!(checkpoint_count, 0);
        assert_eq!(dataset.time.len(), 5);
    }

    // `load_progressive` is the public entry point mirroring `load`; proves
    // it returns the same dataset `load` would for the same file, with
    // checkpoints observed along the way.
    #[test]
    fn load_progressive_agrees_with_load_and_reports_checkpoints() {
        let file = many_rows_temp_csv(70_000);
        let mut checkpoint_count = 0;

        let dataset = load_progressive(file.path(), |_checkpoint| checkpoint_count += 1)
            .expect("load_progressive must succeed");
        let expected = load(file.path()).expect("load must succeed");

        assert_eq!(dataset, expected);
        assert!(checkpoint_count >= 1);
    }
}
