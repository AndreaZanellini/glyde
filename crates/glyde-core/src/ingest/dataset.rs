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

//! Materializing a delimited-text file as a typed [`Dataset`], under the
//! SPEC §5 RAM budget (issue #75).
//!
//! [`load`] wires together the same inference pieces `ingest::report::inspect`
//! uses for its summary — encoding, delimiter, decimal separator, time-index
//! detection — but, unlike `inspect`, actually materializes every column's
//! typed values instead of counts alone, so `glyde-app`'s time-domain view
//! has real samples to plot.
//!
//! **Two backing stores, chosen before a single row is read.** SPEC §5.1
//! requires Glyde to check affordability *before* acting, never after, and
//! caps peak RSS at a flat `min(25% RAM, 4 GB)` regardless of file size:
//!
//! - **In memory** (the fast path): the file is memory-mapped, every field's
//!   source text is captured into `super::csv`'s arena, and the typed
//!   columns are ordinary `Vec`s. Nothing about this changed — it is what
//!   every file small enough to afford it still does.
//! - **Spilled** (`crate::index::spill`): the file is read in bounded chunks
//!   — never mapped whole, since walking a mapping end to end makes every
//!   page resident — each row is typed as it arrives and appended straight
//!   to a per-column spill file, and the finished [`Dataset`] holds
//!   memory-mapped views of those files. No arena, no typed `Vec`, so peak
//!   memory does not grow with the file.
//!
//! The choice is a *storage* choice only (Golden Rule 1): the two produce
//! datasets that compare equal field for field — same values, same dtypes,
//! same timestamps, same anomalies — which
//! `tests/spilled_ingest_integration.rs` locks.

use super::csv::{
    open_path_capturing_all_columns, open_path_capturing_all_columns_with_progress, ColumnText,
    CsvParseOutcome, RowFields, Sniff,
};
use super::infer::{log_dtype_choice, normalize_decimal_field, ColumnDtypeChoice, ColumnDtypeScan};
use crate::budget::RamBudget;
use crate::dsp::decimation::{build_pyramid, Bucket};
use crate::index::level0::{self, CacheKey};
use crate::index::spill::{SpillStringsWriter, SpillVec, SpillVecWriter};
use crate::series::{Anomalies, Dtype, NanRunScan, Series, SeriesValues, SpilledValues};
use crate::time::{
    infer_timestamp_format, parse_timestamp, TimeUnit, Timestamp, TimestampFormat,
    TimestampFormatScan,
};
use crate::{GlydeError, Result};
use std::borrow::Cow;
use std::path::Path;
use tracing::{info, warn};

/// An [`TimeAxis::Absolute`] axis's timestamps, either on the heap (the
/// in-memory path) or memory-mapped from the spill cache (issue #75). Both
/// answer the same questions; no caller has to know which it holds.
///
/// The spilled form keeps `ticks`, `unit` and `offset_seconds` **per row**
/// rather than one shared value for the column: SPEC §2.1 honors whatever
/// UTC offset each source row carried (a file crossing a DST transition
/// really does change offset mid-column), and a sub-nanosecond fractional
/// second promotes that row alone to [`TimeUnit::Picoseconds`]. Collapsing
/// either to the first row's would degrade raw timestamps (Golden Rule 1).
#[derive(Debug, Clone)]
pub enum Timestamps {
    Memory(Vec<Timestamp>),
    Spilled {
        ticks: SpillVec<i128>,
        /// [`TimeUnit`] per row, as [`time_unit_code`].
        units: SpillVec<u8>,
        /// `offset_seconds` per row, with [`NO_UTC_OFFSET`] for `None`.
        offsets: SpillVec<i64>,
    },
}

/// The sentinel a spilled row carries when its source timestamp had no UTC
/// offset at all (SPEC §2.1: naive local time, never an invented `+00:00`).
/// Chosen outside every real offset — those fit in `i32` seconds.
const NO_UTC_OFFSET: i64 = i64::MIN;

fn time_unit_code(unit: TimeUnit) -> u8 {
    match unit {
        TimeUnit::Seconds => 0,
        TimeUnit::Milliseconds => 1,
        TimeUnit::Microseconds => 2,
        TimeUnit::Nanoseconds => 3,
        TimeUnit::Picoseconds => 4,
    }
}

fn time_unit_from_code(code: u8) -> TimeUnit {
    match code {
        0 => TimeUnit::Seconds,
        1 => TimeUnit::Milliseconds,
        2 => TimeUnit::Microseconds,
        4 => TimeUnit::Picoseconds,
        // 3 is `Nanoseconds`, the unit every textual format produces and so
        // the only sensible reading of a byte this crate did not write.
        _ => TimeUnit::Nanoseconds,
    }
}

impl Timestamps {
    pub fn len(&self) -> usize {
        match self {
            Timestamps::Memory(timestamps) => timestamps.len(),
            Timestamps::Spilled { ticks, .. } => ticks.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `index`-th timestamp, reassembled from the spilled columns when
    /// this axis is spilled.
    pub fn get(&self, index: usize) -> Option<Timestamp> {
        match self {
            Timestamps::Memory(timestamps) => timestamps.get(index).copied(),
            Timestamps::Spilled {
                ticks,
                units,
                offsets,
            } => {
                let ticks = ticks.get(index)?;
                let unit = time_unit_from_code(units.get(index).unwrap_or(3));
                let offset = offsets.get(index).unwrap_or(NO_UTC_OFFSET);
                Some(Timestamp {
                    ticks,
                    unit,
                    offset_seconds: (offset != NO_UTC_OFFSET).then_some(offset as i32),
                })
            }
        }
    }

    /// Every timestamp, in row order.
    pub fn iter(&self) -> impl Iterator<Item = Timestamp> + '_ {
        (0..self.len()).filter_map(move |index| self.get(index))
    }

    /// The raw `i128` tick of every row — borrowed straight from the spill
    /// mapping when this axis is spilled, so the sampling/gap/monotonicity
    /// checks (SPEC §2.1–2.2) that consume `&[i128]` need no copy of it.
    pub fn ticks(&self) -> Cow<'_, [i128]> {
        match self {
            Timestamps::Memory(timestamps) => {
                Cow::Owned(timestamps.iter().map(|t| t.ticks).collect())
            }
            Timestamps::Spilled { ticks, .. } => Cow::Borrowed(ticks.as_slice()),
        }
    }

    fn is_spilled(&self) -> bool {
        matches!(self, Timestamps::Spilled { .. })
    }
}

impl From<Vec<Timestamp>> for Timestamps {
    fn from(timestamps: Vec<Timestamp>) -> Self {
        Timestamps::Memory(timestamps)
    }
}

/// Compared by value, never by storage — see [`SeriesValues`]'s own
/// `PartialEq`.
impl PartialEq for Timestamps {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Timestamps::Memory(a), Timestamps::Memory(b)) => a == b,
            _ => self.len() == other.len() && self.iter().eq(other.iter()),
        }
    }
}

/// A [`TimeAxis::Progressive`] axis's values, heap-backed or spilled — the
/// [`Timestamps`] counterpart for SPEC §2.1's progressive numeric index.
#[derive(Debug, Clone)]
pub enum ProgressiveValues {
    Memory(Vec<f64>),
    Spilled(SpillVec<f64>),
}

impl ProgressiveValues {
    pub fn as_slice(&self) -> &[f64] {
        match self {
            ProgressiveValues::Memory(values) => values,
            ProgressiveValues::Spilled(values) => values.as_slice(),
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    fn is_spilled(&self) -> bool {
        matches!(self, ProgressiveValues::Spilled(_))
    }
}

impl From<Vec<f64>> for ProgressiveValues {
    fn from(values: Vec<f64>) -> Self {
        ProgressiveValues::Memory(values)
    }
}

impl PartialEq for ProgressiveValues {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

/// The time axis of a materialized [`Dataset`] (SPEC §2.1's two accepted
/// index kinds).
#[derive(Debug, Clone, PartialEq)]
pub enum TimeAxis {
    /// An absolute timestamp column, kept together with the
    /// [`TimestampFormat`] it was detected as so a caller can redisplay each
    /// [`Timestamp`] the same way the source wrote it (e.g. round-tripping
    /// an honored UTC offset, SPEC §2.1).
    Absolute {
        timestamps: Timestamps,
        format: TimestampFormat,
    },
    /// A monotonic integer/float sequence with no absolute-time meaning
    /// (SPEC §2.1 "progressive numeric").
    Progressive { values: ProgressiveValues },
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

    /// Whether this axis's samples live in the on-disk spill cache rather
    /// than on the heap.
    pub fn is_spilled(&self) -> bool {
        match self {
            TimeAxis::Absolute { timestamps, .. } => timestamps.is_spilled(),
            TimeAxis::Progressive { values } => values.is_spilled(),
        }
    }

    /// Converts this axis into the `i128` tick space
    /// [`crate::dsp::decimation::build_pyramid`]/
    /// [`crate::dsp::decimation::decimate_viewport`] operate on (issue #60
    /// decision, docs/ARCHITECTURE.md §The index).
    ///
    /// `Absolute` ticks are each [`Timestamp`]'s own `ticks` field verbatim —
    /// every timestamp in one axis was parsed against the same detected
    /// [`TimestampFormat`], hence shares one [`crate::time::TimeUnit`], so
    /// the values are already on one consistent scale, and a spilled axis
    /// borrows them straight from its mapping with no copy at all.
    /// `Progressive` values have no calendar meaning or unit of their own,
    /// so they are mapped through [`progressive_value_to_tick`] fixed-point
    /// scaling — this preserves true x-distance between samples, so an
    /// unevenly-spaced progressive axis decimates the same way an
    /// absolute-time axis with identical physical spacing would, rather than
    /// aggregating by sample ordinal.
    ///
    /// `dsp::decimation` never interprets the ticks it is handed; only the
    /// caller (via [`progressive_tick_to_value`] for `Progressive`, or a
    /// `Timestamp`'s own `unit` for `Absolute`) knows how to convert them
    /// back to a display value.
    pub fn to_pyramid_ticks(&self) -> Cow<'_, [i128]> {
        match self {
            TimeAxis::Absolute { timestamps, .. } => timestamps.ticks(),
            TimeAxis::Progressive { values } => Cow::Owned(
                values
                    .as_slice()
                    .iter()
                    .copied()
                    .map(progressive_value_to_tick)
                    .collect(),
            ),
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

/// A fully materialized delimited-text file: its time axis plus every other
/// column, typed (SPEC §1.4). `columns` is in source header order, excluding
/// the time-index column. Whether the samples live on the heap or in the
/// on-disk spill cache is a storage detail — see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    pub time: TimeAxis,
    pub time_column_name: String,
    pub columns: Vec<Series>,
}

impl Dataset {
    /// Whether this dataset's samples live in the on-disk spill cache
    /// (issue #75) rather than on the heap — the storage the RAM budget
    /// picked for it, exposed so a test or a diagnostic can assert which
    /// path an open actually took.
    pub fn is_spilled(&self) -> bool {
        self.time.is_spilled()
            || self
                .columns
                .iter()
                .any(|series| matches!(series.values(), SeriesValues::Spilled(_)))
    }
}

/// Loads every row and column of the delimited-text file at `path`, choosing
/// between the in-memory and spilled backing stores by the machine's RAM
/// budget (see the module docs). Column 0 is always the time index, the same
/// convention `ingest::report::inspect` uses (docs/QUALITY.md's torture
/// corpus never puts the time column anywhere else). A single-column file
/// has no data series to plot and is rejected as
/// [`GlydeError::SingleColumnFile`], exactly like `inspect`.
pub fn load(path: &Path) -> Result<Dataset> {
    load_with_outcome(path).map(|(_outcome, dataset, _timestamp_format_ambiguous)| dataset)
}

/// [`load`] against an explicit budget and spill directory, for tests and
/// diagnostics that need to exercise a specific storage choice rather than
/// whatever the host machine's RAM happens to select — the same split
/// [`RamBudget::from_total_ram_bytes`] exists for.
pub fn load_with_budget(path: &Path, budget: RamBudget, cache_dir: &Path) -> Result<Dataset> {
    load_with_outcome_using(path, budget, Some(cache_dir))
        .map(|(_outcome, dataset, _ambiguous)| dataset)
}

/// [`load`], additionally returning the [`CsvParseOutcome`] the parse
/// already produced (encoding, delimiter, decimal separator, row counts) and
/// whether the time column's timestamp format was an SPEC §2.1
/// ambiguity-rule fallback rather than a confident match (`false` for a
/// [`TimeAxis::Progressive`] index, which has no timestamp format to be
/// ambiguous about). `ingest::report::open_dataset` uses this so that
/// materializing a [`Dataset`] and reporting an [`super::OpenSummary`] /
/// [`super::InferenceReport`] for it share one read of the file instead of
/// independent ones (issue #58: the app used to call `inspect()` and
/// `load()` back to back, each re-reading and re-decoding the whole file).
pub(crate) fn load_with_outcome(path: &Path) -> Result<(CsvParseOutcome, Dataset, bool)> {
    let cache_dir = os_spill_dir();
    load_with_outcome_using(path, RamBudget::from_system(), cache_dir.as_deref())
}

/// [`load_with_outcome`] against an explicit budget and spill directory.
pub(crate) fn load_with_outcome_with_budget(
    path: &Path,
    budget: RamBudget,
    cache_dir: &Path,
) -> Result<(CsvParseOutcome, Dataset, bool)> {
    load_with_outcome_using(path, budget, Some(cache_dir))
}

/// The OS-standard spill directory, or `None` when this machine has no
/// resolvable cache directory at all. Not an error on its own: a file that
/// fits the budget never needs it (docs/ARCHITECTURE.md §The index: "the
/// cache is an optimization, never a requirement to open a file").
fn os_spill_dir() -> Option<std::path::PathBuf> {
    match level0::os_cache_dir() {
        Ok(dir) => Some(dir),
        Err(err) => {
            warn!(
                error = %err,
                "no OS cache directory available; a file too large for the RAM budget cannot be \
                 spilled and will be refused rather than attempted (SPEC §5.1)"
            );
            None
        }
    }
}

/// Which backing store an open should use, decided from the file's shape
/// before any row is read (SPEC §5.1 "checks affordability before acting,
/// never after").
enum Storage {
    InMemory,
    Spill(Box<Sniff>),
}

/// Runs the SPEC §5.1 affordability check for `path` and reports the storage
/// it selected at `info` (Golden Rule 2: a decision taken on the user's
/// behalf is never silent).
fn choose_storage(path: &Path, budget: RamBudget, cache_dir: Option<&Path>) -> Result<Storage> {
    let file_bytes = std::fs::metadata(path)
        .map_err(|source| GlydeError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();

    let sniff = super::csv::sniff_path(path)?;
    let footprint = sniff.footprint(file_bytes);

    if budget.affords(footprint.estimated_bytes) {
        info!(
            file_bytes,
            estimated_bytes = footprint.estimated_bytes,
            cap_bytes = budget.cap_bytes(),
            estimated_row_count = footprint.estimated_row_count,
            column_count = footprint.column_count,
            "opening in memory: the typed columns fit the RAM budget (SPEC §5.1)"
        );
        return Ok(Storage::InMemory);
    }

    if cache_dir.is_none() {
        return Err(GlydeError::BudgetExceeded {
            requested_bytes: footprint.estimated_bytes,
            cap_bytes: budget.cap_bytes(),
        });
    }

    info!(
        file_bytes,
        estimated_bytes = footprint.estimated_bytes,
        cap_bytes = budget.cap_bytes(),
        estimated_row_count = footprint.estimated_row_count,
        column_count = footprint.column_count,
        "spilling to the on-disk cache: materializing this file in memory would exceed the RAM \
         budget (SPEC §5.1)"
    );
    Ok(Storage::Spill(Box::new(sniff)))
}

fn load_with_outcome_using(
    path: &Path,
    budget: RamBudget,
    cache_dir: Option<&Path>,
) -> Result<(CsvParseOutcome, Dataset, bool)> {
    match choose_storage(path, budget, cache_dir)? {
        Storage::InMemory => {
            let (outcome, columns_text) = open_path_capturing_all_columns(path)?;
            let (dataset, ambiguous) = build_dataset(&outcome, &columns_text)?;
            Ok((outcome, dataset, ambiguous))
        }
        Storage::Spill(sniff) => {
            let cache_dir = cache_dir.expect("choose_storage refuses to spill without a cache dir");
            load_spilled(path, &sniff, cache_dir)
        }
    }
}

/// The typed-conversion half of the in-memory path: every column's raw
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

    let (time, timestamp_format_ambiguous) = match infer_timestamp_format(&time_fields) {
        Some(format_inference) => {
            let mut timestamps = Vec::with_capacity(time_fields.len());
            for field in &time_fields {
                timestamps.push(parse_timestamp(field, format_inference.format)?);
            }
            (
                TimeAxis::Absolute {
                    timestamps: Timestamps::Memory(timestamps),
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
                values.push(parse_progressive_value(field)?);
            }
            (
                TimeAxis::Progressive {
                    values: ProgressiveValues::Memory(values),
                },
                false,
            )
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
            let normalized: Vec<Cow<'_, str>> = column_text
                .iter()
                .map(|field| normalize_decimal_field(field, outcome.decimal_separator))
                .collect();
            super::infer::infer_column(name.clone(), &normalized).series
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

/// SPEC §2.1's progressive numeric index: a plain number with no
/// absolute-time meaning. A field that is not even that is a real error, not
/// a silent empty plot.
fn parse_progressive_value(field: &str) -> Result<f64> {
    field
        .trim()
        .parse::<f64>()
        .map_err(|_| GlydeError::NonNumericTimeIndex {
            input: field.to_string(),
        })
}

// ---------------------------------------------------------------------------
// The spilled path (issue #75)
// ---------------------------------------------------------------------------

/// Reads `path` in bounded chunks and materializes it as a [`Dataset`] whose
/// samples live in `cache_dir`'s spill files rather than on the heap.
///
/// Two passes over the file, neither retaining anything:
///
/// 1. **Scan.** Every row is fed to `time::TimestampFormatScan` and one
///    `infer::ColumnDtypeScan` per data column — the same canonical
///    inference the in-memory path runs, driven incrementally instead of
///    over a whole captured column. This settles SPEC §2.1's timestamp
///    format and SPEC §1.4's dtypes over *every* row, so a file's inferred
///    shape never depends on how large it happens to be.
/// 2. **Write.** Every row is typed under that decision and appended to its
///    column's spill file.
///
/// Two passes rather than one because a dtype is only known once the last
/// row has been seen (SPEC §1.4: a single non-numeric cell keeps the whole
/// column as text), and re-reading the source is cheaper — and far more
/// faithful — than spilling every field's text first only to re-type it.
fn load_spilled(
    path: &Path,
    sniff: &Sniff,
    cache_dir: &Path,
) -> Result<(CsvParseOutcome, Dataset, bool)> {
    let column_names = sniff.column_names().to_vec();
    if column_names.len() < 2 {
        return Err(GlydeError::SingleColumnFile);
    }
    let decimal_separator = sniff.decimal_separator;
    let data_column_count = column_names.len() - 1;

    // --- Pass 1: infer, retaining nothing -----------------------------------
    let mut time_scan = TimestampFormatScan::default();
    let mut dtype_scans: Vec<ColumnDtypeScan> = (0..data_column_count)
        .map(|_| ColumnDtypeScan::default())
        .collect();
    super::csv::stream_path(path, sniff, &mut |row: RowFields<'_>| {
        time_scan.observe(row.get(0).unwrap_or_default());
        for (index, scan) in dtype_scans.iter_mut().enumerate() {
            let field = row.get(index + 1).unwrap_or_default();
            scan.observe(&normalize_decimal_field(field, decimal_separator));
        }
        Ok(())
    })?;

    let timestamp_format = time_scan.finish();
    let choices: Vec<ColumnDtypeChoice> = dtype_scans.iter().map(ColumnDtypeScan::finish).collect();

    // --- Pass 2: type every row straight into its spill file ----------------
    let stem = CacheKey::for_path(path)?.cache_stem();
    let mut time_writer = TimeAxisSpillWriter::create(
        cache_dir,
        &stem,
        timestamp_format.map(|inference| inference.format),
    )?;
    let mut column_writers: Vec<ColumnSpillWriter> = choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            ColumnSpillWriter::create(cache_dir, &format!("{stem}.c{index}"), choice.dtype)
        })
        .collect::<Result<_>>()?;

    let outcome = super::csv::stream_path(path, sniff, &mut |row: RowFields<'_>| {
        time_writer.push(row.get(0).unwrap_or_default())?;
        for (index, writer) in column_writers.iter_mut().enumerate() {
            let field = row.get(index + 1).unwrap_or_default();
            writer.push(&normalize_decimal_field(field, decimal_separator))?;
        }
        Ok(())
    })?;

    let time = time_writer.finish()?;
    let columns = column_writers
        .into_iter()
        .zip(&column_names[1..])
        .zip(&choices)
        .map(|((writer, name), choice)| {
            log_dtype_choice(*choice, outcome.row_count as usize);
            writer.finish(name.clone())
        })
        .collect::<Result<Vec<Series>>>()?;

    info!(
        row_count = outcome.row_count,
        column_count = column_names.len(),
        cache_dir = %cache_dir.display(),
        "file materialized through the on-disk spill cache (SPEC §5.1)"
    );

    Ok((
        outcome,
        Dataset {
            time,
            time_column_name: column_names[0].clone(),
            columns,
        },
        timestamp_format.is_some_and(|inference| inference.ambiguous),
    ))
}

/// Writes a [`TimeAxis`] to the spill cache one row at a time.
enum TimeAxisSpillWriter {
    Absolute {
        ticks: SpillVecWriter<i128>,
        units: SpillVecWriter<u8>,
        offsets: SpillVecWriter<i64>,
        format: TimestampFormat,
    },
    Progressive(SpillVecWriter<f64>),
}

impl TimeAxisSpillWriter {
    fn create(cache_dir: &Path, stem: &str, format: Option<TimestampFormat>) -> Result<Self> {
        match format {
            Some(format) => Ok(TimeAxisSpillWriter::Absolute {
                ticks: SpillVecWriter::create(cache_dir, &format!("{stem}.ts"))?,
                units: SpillVecWriter::create(cache_dir, &format!("{stem}.tsunit"))?,
                offsets: SpillVecWriter::create(cache_dir, &format!("{stem}.tsoffset"))?,
                format,
            }),
            None => Ok(TimeAxisSpillWriter::Progressive(SpillVecWriter::create(
                cache_dir,
                &format!("{stem}.tsprogressive"),
            )?)),
        }
    }

    fn push(&mut self, field: &str) -> Result<()> {
        match self {
            TimeAxisSpillWriter::Absolute {
                ticks,
                units,
                offsets,
                format,
            } => {
                let timestamp = parse_timestamp(field, *format)?;
                ticks.push(timestamp.ticks)?;
                units.push(time_unit_code(timestamp.unit))?;
                offsets.push(
                    timestamp
                        .offset_seconds
                        .map_or(NO_UTC_OFFSET, |offset| offset as i64),
                )
            }
            TimeAxisSpillWriter::Progressive(values) => {
                values.push(parse_progressive_value(field)?)
            }
        }
    }

    fn finish(self) -> Result<TimeAxis> {
        match self {
            TimeAxisSpillWriter::Absolute {
                ticks,
                units,
                offsets,
                format,
            } => Ok(TimeAxis::Absolute {
                timestamps: Timestamps::Spilled {
                    ticks: ticks.finish()?,
                    units: units.finish()?,
                    offsets: offsets.finish()?,
                },
                format,
            }),
            TimeAxisSpillWriter::Progressive(values) => Ok(TimeAxis::Progressive {
                values: ProgressiveValues::Spilled(values.finish()?),
            }),
        }
    }
}

/// Writes one data column to the spill cache in its inferred dtype, one
/// sample at a time. A field that does not parse under the dtype the scan
/// pass settled on cannot happen — the scan saw the same field — so the
/// fallbacks here mirror `infer_column`'s own.
enum ColumnSpillWriter {
    Bool(SpillVecWriter<u8>),
    I64(SpillVecWriter<i64>),
    /// SPEC §1.3's NaN runs are flagged as the samples stream past, so the
    /// finished column never has to be read back to find them.
    F64(SpillVecWriter<f64>, NanRunScan),
    String(SpillStringsWriter),
}

impl ColumnSpillWriter {
    fn create(cache_dir: &Path, stem: &str, dtype: Dtype) -> Result<Self> {
        Ok(match dtype {
            Dtype::Bool => ColumnSpillWriter::Bool(SpillVecWriter::create(cache_dir, stem)?),
            Dtype::I64 => ColumnSpillWriter::I64(SpillVecWriter::create(cache_dir, stem)?),
            Dtype::F64 => ColumnSpillWriter::F64(
                SpillVecWriter::create(cache_dir, stem)?,
                NanRunScan::default(),
            ),
            // Every other dtype is unreachable from `ColumnDtypeScan`, which
            // only ever settles on bool/i64/f64/string (SPEC §1.4 via
            // `infer_column`); a Parquet reader's narrower widths land with
            // docs/ROADMAP.md M7 and will extend both together.
            _ => ColumnSpillWriter::String(SpillStringsWriter::create(cache_dir, stem)?),
        })
    }

    fn push(&mut self, field: &str) -> Result<()> {
        match self {
            ColumnSpillWriter::Bool(values) => values.push(u8::from(
                super::infer::parse_bool_field(field).unwrap_or_default(),
            )),
            ColumnSpillWriter::I64(values) => {
                values.push(field.trim().parse::<i64>().unwrap_or_default())
            }
            ColumnSpillWriter::F64(values, nan_runs) => {
                let value = field.trim().parse::<f64>().unwrap_or_default();
                nan_runs.observe(value);
                values.push(value)
            }
            ColumnSpillWriter::String(values) => values.push(field),
        }
    }

    fn finish(self, name: String) -> Result<Series> {
        Ok(match self {
            ColumnSpillWriter::Bool(values) => Series::new(
                name,
                SeriesValues::Spilled(SpilledValues::Bool(values.finish()?)),
            ),
            ColumnSpillWriter::I64(values) => Series::new(
                name,
                SeriesValues::Spilled(SpilledValues::I64(values.finish()?)),
            ),
            ColumnSpillWriter::F64(values, nan_runs) => {
                let nan_runs = nan_runs.finish();
                if !nan_runs.is_empty() {
                    warn!(
                        run_count = nan_runs.len(),
                        "NaN run(s) flagged in a numeric column (SPEC §1.3)"
                    );
                }
                Series::with_anomalies(
                    name,
                    SeriesValues::Spilled(SpilledValues::F64(values.finish()?)),
                    Anomalies {
                        nan_runs,
                        ..Anomalies::default()
                    },
                )
            }
            ColumnSpillWriter::String(values) => Series::new(
                name,
                SeriesValues::Spilled(SpilledValues::String(values.finish()?)),
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// Progressive (background) loading
// ---------------------------------------------------------------------------

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
///
/// **Checkpoints are an in-memory-path feature.** A file large enough to be
/// spilled reports no progress updates: a checkpoint hands out a `Dataset`
/// over samples that are still being written, and re-mapping a growing spill
/// file is not portable (Windows will not rename a mapped file, which is how
/// every spill file is published). Such a file still opens — it just shows a
/// spinner rather than a filling plot until the read completes. Tracked as
/// its own follow-up; wiring the *pyramid* into the UI is issue #80.
pub(crate) fn load_with_outcome_progressive(
    path: &Path,
    mut on_checkpoint: impl FnMut(Checkpoint),
) -> Result<(CsvParseOutcome, Dataset, bool)> {
    let cache_dir = os_spill_dir();
    match choose_storage(path, RamBudget::from_system(), cache_dir.as_deref())? {
        Storage::Spill(sniff) => {
            let cache_dir = cache_dir
                .as_deref()
                .expect("choose_storage refuses to spill without a cache dir");
            info!(
                "progress checkpoints are skipped for a spilled open; the plot appears once the \
                 read completes"
            );
            load_spilled(path, &sniff, cache_dir)
        }
        Storage::InMemory => {
            let (outcome, columns_text) = open_path_capturing_all_columns_with_progress(
                path,
                |partial_outcome, partial_columns| match build_dataset(
                    partial_outcome,
                    partial_columns,
                ) {
                    Ok((dataset, _ambiguous)) => {
                        let pyramids = pyramids_for(&dataset);
                        on_checkpoint(Checkpoint {
                            rows_read: partial_outcome.row_count,
                            pyramids,
                            dataset,
                        });
                    }
                    Err(err) => {
                        warn!(
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
    }
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
        .map(|series| match series.values().as_f64_slice() {
            // Already `f64`: hand `build_pyramid` the samples themselves
            // rather than a freshly allocated copy of them.
            Some(samples) => Some(build_pyramid(samples, &ticks)),
            None => series
                .values()
                .to_f64_vec()
                .map(|samples| build_pyramid(&samples, &ticks)),
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
                assert_eq!(values.as_slice(), [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
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
            ]
            .into(),
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
            values: vec![0.0, 1.0, 2.5, -3.25].into(),
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
                        values: values.as_slice()[..checkpoint.rows_read as usize]
                            .to_vec()
                            .into(),
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
