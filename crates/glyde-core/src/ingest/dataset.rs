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
    CsvParseOutcome, RowFields, Sniff, FIRST_PROGRESS_CHECKPOINT_ROWS,
};
use super::infer::{
    log_dtype_choice, normalize_decimal_field, ColumnDtypeChoice, ColumnDtypeScan, ColumnInference,
};
use super::{IngestOverrides, TimeColumnChoice};
use crate::budget::RamBudget;
use crate::dsp::decimation::{build_pyramid, build_pyramid_streaming, extend_pyramid, Bucket};
use crate::index::level0::{self, CacheKey, Level0Cache};
use crate::index::pyramid;
use crate::index::spill::{SpillStringsWriter, SpillVec, SpillVecWriter};
use crate::series::{Anomalies, Dtype, NanRunScan, Series, SeriesValues, SpilledValues};
use crate::time::{
    parse_timestamp, TickSource, TimeUnit, Timestamp, TimestampFormat, TimestampFormatInference,
    TimestampFormatScan, TICK_CHUNK_LEN,
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

    /// [`SeriesValues::reorder`]'s counterpart for the time axis itself
    /// (SPEC §2.1's "[Sort]" affordance): `order[i]` is the original index of
    /// the timestamp that should end up at position `i`. Only defined for
    /// [`Timestamps::Memory`] — see [`sort_dataset_by_time`]'s doc comment.
    fn reorder(&mut self, order: &[usize]) {
        match self {
            Timestamps::Memory(timestamps) => {
                crate::series::reorder_in_place(timestamps, order);
            }
            Timestamps::Spilled { .. } => {
                debug_assert!(
                    false,
                    "Timestamps::reorder must never be called on a spilled axis"
                );
            }
        }
    }
}

/// How SPEC §2.1–2.2's statistics read this axis (issue #85): in bounded
/// chunks, replayable, whichever storage backs it.
///
/// [`Timestamps::ticks`] is still the right way to hand a *whole* tick column
/// to a caller that needs one contiguously (`TimeAxis::to_pyramid_ticks`).
/// This is for the multi-pass statistics in [`crate::time`], where a spilled
/// column is read back through a fixed-size buffer instead, so a scan of it
/// does not make the mapping resident and cost memory proportional to file
/// size.
impl TickSource for Timestamps {
    fn tick_count(&self) -> usize {
        self.len()
    }

    fn visit_tick_chunks(
        &self,
        range: std::ops::Range<usize>,
        visit: &mut dyn FnMut(&[i128]) -> Result<()>,
    ) -> Result<()> {
        match self {
            // The heap path's ticks each live inside a `Timestamp`, so they are
            // gathered into one reused chunk buffer rather than borrowed. This
            // path is only taken for a file that already fit the RAM budget, so
            // the copy is bounded by construction.
            Timestamps::Memory(timestamps) => {
                let start = range.start.min(timestamps.len());
                let end = range.end.min(timestamps.len());
                if start >= end {
                    return Ok(());
                }
                let mut chunk = Vec::with_capacity(TICK_CHUNK_LEN.min(end - start));
                for timestamps in timestamps[start..end].chunks(TICK_CHUNK_LEN) {
                    chunk.clear();
                    chunk.extend(timestamps.iter().map(|timestamp| timestamp.ticks));
                    visit(&chunk)?;
                }
                Ok(())
            }
            Timestamps::Spilled { ticks, .. } => ticks.read_chunks(range, visit),
        }
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

    /// [`Timestamps::reorder`]'s counterpart for a [`TimeAxis::Progressive`]
    /// axis. Only defined for [`ProgressiveValues::Memory`].
    fn reorder(&mut self, order: &[usize]) {
        match self {
            ProgressiveValues::Memory(values) => {
                crate::series::reorder_in_place(values, order);
            }
            ProgressiveValues::Spilled(_) => {
                debug_assert!(
                    false,
                    "ProgressiveValues::reorder must never be called on a spilled axis"
                );
            }
        }
    }

    /// Hands `range`'s values to `visit` in bounded chunks, in row order —
    /// the [`TickSource`] treatment applied to a progressive axis's own `f64`
    /// values, so [`TimeAxis`]'s tick source never has to materialize a
    /// spilled progressive column to scale it (issue #88).
    fn visit_value_chunks(
        &self,
        range: std::ops::Range<usize>,
        visit: &mut dyn FnMut(&[f64]) -> Result<()>,
    ) -> Result<()> {
        match self {
            ProgressiveValues::Memory(values) => {
                let start = range.start.min(values.len());
                let end = range.end.min(values.len());
                if start >= end {
                    return Ok(());
                }
                for chunk in values[start..end].chunks(TICK_CHUNK_LEN) {
                    visit(chunk)?;
                }
                Ok(())
            }
            ProgressiveValues::Spilled(values) => values.read_chunks(range, visit),
        }
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

    /// [`Timestamps::reorder`] / [`ProgressiveValues::reorder`], dispatched
    /// on whichever variant this axis holds (SPEC §2.1's "[Sort]"
    /// affordance — see [`sort_dataset_by_time`]).
    fn reorder(&mut self, order: &[usize]) {
        match self {
            TimeAxis::Absolute { timestamps, .. } => timestamps.reorder(order),
            TimeAxis::Progressive { values } => values.reorder(order),
        }
    }
}

/// The bounded-memory counterpart of [`TimeAxis::to_pyramid_ticks`] (issue
/// #88): the same ticks, in the same order, but handed over in fixed-size
/// chunks so a spilled axis is never made resident whole — including a
/// `Progressive` axis, whose fixed-point scaling `to_pyramid_ticks` can only
/// deliver by allocating a whole second column.
impl TickSource for TimeAxis {
    fn tick_count(&self) -> usize {
        self.len()
    }

    fn visit_tick_chunks(
        &self,
        range: std::ops::Range<usize>,
        visit: &mut dyn FnMut(&[i128]) -> Result<()>,
    ) -> Result<()> {
        match self {
            TimeAxis::Absolute { timestamps, .. } => timestamps.visit_tick_chunks(range, visit),
            TimeAxis::Progressive { values } => {
                let mut ticks = Vec::with_capacity(TICK_CHUNK_LEN);
                values.visit_value_chunks(range, &mut |chunk| {
                    ticks.clear();
                    ticks.extend(chunk.iter().copied().map(progressive_value_to_tick));
                    visit(&ticks)
                })
            }
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
/// budget (see the module docs). Column 0 is the time-index candidate, the
/// same convention `ingest::report::inspect` uses — unless it is a numeric
/// column that runs backwards or never advances, or no index at all, in which
/// case it is a series and a row index is generated ([`TimeCandidateScan`],
/// SPEC §2.1 "Files without a time column"). A file left with nothing to plot
/// is rejected as [`GlydeError::SingleColumnFile`], exactly like `inspect`.
pub fn load(path: &Path) -> Result<Dataset> {
    load_with_outcome(path).map(|(_outcome, dataset, _timestamp_format_ambiguous)| dataset)
}

/// [`load`] against an explicit budget and spill directory, for tests and
/// diagnostics that need to exercise a specific storage choice rather than
/// whatever the host machine's RAM happens to select — the same split
/// [`RamBudget::from_total_ram_bytes`] exists for.
pub fn load_with_budget(path: &Path, budget: RamBudget, cache_dir: &Path) -> Result<Dataset> {
    load_with_outcome_using(path, budget, Some(cache_dir), IngestOverrides::default())
        .map(|(_outcome, dataset, _ambiguous)| dataset)
}

/// [`load`] with [`IngestOverrides`] applied (docs/ROADMAP.md M4 "One-click
/// correction of each field → triggers a re-index"): each `Some` field bypasses
/// its inference step and settles the parse outright.
pub fn load_with_overrides(path: &Path, overrides: IngestOverrides) -> Result<Dataset> {
    load_with_outcome_with_overrides(path, overrides).map(|(_outcome, dataset, _ambiguous)| dataset)
}

/// [`load_with_overrides`] against an explicit budget and spill directory,
/// for tests that need to exercise a specific storage choice under an
/// override rather than whatever the host machine's RAM happens to select.
pub fn load_with_overrides_and_budget(
    path: &Path,
    overrides: IngestOverrides,
    budget: RamBudget,
    cache_dir: &Path,
) -> Result<Dataset> {
    load_with_outcome_using(path, budget, Some(cache_dir), overrides)
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
pub(crate) fn load_with_outcome(
    path: &Path,
) -> Result<(CsvParseOutcome, Dataset, TimeIndexInference)> {
    let cache_dir = os_spill_dir();
    load_with_outcome_using(
        path,
        RamBudget::from_system(),
        cache_dir.as_deref(),
        IngestOverrides::default(),
    )
}

/// [`load_with_outcome`] against an explicit budget and spill directory.
pub(crate) fn load_with_outcome_with_budget(
    path: &Path,
    budget: RamBudget,
    cache_dir: &Path,
) -> Result<(CsvParseOutcome, Dataset, TimeIndexInference)> {
    load_with_outcome_using(path, budget, Some(cache_dir), IngestOverrides::default())
}

/// [`load_with_outcome`] with [`IngestOverrides`] applied — `ingest::report::
/// open_dataset_with_overrides` uses this the same way `open_dataset` uses
/// [`load_with_outcome`].
pub(crate) fn load_with_outcome_with_overrides(
    path: &Path,
    overrides: IngestOverrides,
) -> Result<(CsvParseOutcome, Dataset, TimeIndexInference)> {
    let cache_dir = os_spill_dir();
    load_with_outcome_using(
        path,
        RamBudget::from_system(),
        cache_dir.as_deref(),
        overrides,
    )
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
fn choose_storage(
    path: &Path,
    budget: RamBudget,
    cache_dir: Option<&Path>,
    overrides: IngestOverrides,
) -> Result<Storage> {
    let file_bytes = std::fs::metadata(path)
        .map_err(|source| GlydeError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();

    let sniff = super::csv::sniff_path(path, overrides)?;
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
    if overrides.sort_by_time {
        warn!(
            file_bytes,
            "sort-by-time was requested, but this file must spill to the on-disk cache (SPEC \
             §5.1) — a spilled column cannot be permuted in place, so it opens unsorted \
             (SPEC §2.1's [Sort] affordance is not available for a spilled file)"
        );
    }
    Ok(Storage::Spill(Box::new(sniff)))
}

fn load_with_outcome_using(
    path: &Path,
    budget: RamBudget,
    cache_dir: Option<&Path>,
    overrides: IngestOverrides,
) -> Result<(CsvParseOutcome, Dataset, TimeIndexInference)> {
    match choose_storage(path, budget, cache_dir, overrides)? {
        Storage::InMemory => {
            let (outcome, columns_text) = open_path_capturing_all_columns(path, overrides)?;
            let (dataset, ambiguous) = build_dataset(&outcome, &columns_text, overrides)?;
            Ok((outcome, dataset, ambiguous))
        }
        Storage::Spill(sniff) => {
            let cache_dir = cache_dir.expect("choose_storage refuses to spill without a cache dir");
            load_spilled(path, &sniff, cache_dir, None, overrides)
        }
    }
}

/// The typed-conversion half of the in-memory path: every column's raw
/// captured text, already fully read by [`super::csv`], into a [`Dataset`].
/// A thin wrapper over [`DatasetBuilder`], which
/// [`load_with_outcome_progressive`] drives incrementally across checkpoints
/// — one implementation for both, so a checkpoint can never drift from the
/// final dataset (docs/ROADMAP.md M3 "Background progressive build emitting
/// partial levels").
fn build_dataset(
    outcome: &CsvParseOutcome,
    columns_text: &[ColumnText],
    overrides: IngestOverrides,
) -> Result<(Dataset, TimeIndexInference)> {
    DatasetBuilder::default().finish(outcome, columns_text, overrides)
}

/// The column SPEC §2.1's time-index detection examines: column 0 unless the
/// user picked another one (or none at all — `None`). A picked position past
/// the last column is a stale choice, never a reason to fail the open, so it
/// falls back to automatic detection with a `warn`.
fn time_column_candidate(overrides: IngestOverrides, column_count: usize) -> Option<usize> {
    match overrides.time_column {
        None => (column_count > 0).then_some(0),
        Some(TimeColumnChoice::RowIndex) => None,
        Some(TimeColumnChoice::Column(index)) if index < column_count => Some(index),
        Some(TimeColumnChoice::Column(index)) => {
            warn!(
                picked_column = index,
                column_count,
                "the picked time column does not exist in this file; detecting the time column \
                 automatically instead"
            );
            (column_count > 0).then_some(0)
        }
    }
}

/// Whether SPEC §2.1's monotonicity test applies to the candidate: only when
/// Glyde is choosing on the user's behalf. A deliberately picked column is
/// taken as given (its out-of-order rows are reported, never overruled).
fn checks_monotonicity(overrides: IngestOverrides, column_count: usize) -> bool {
    !matches!(
        overrides.time_column,
        Some(TimeColumnChoice::Column(index)) if index < column_count
    )
}

/// The data columns of a file with `column_count` columns whose time index
/// is `time_column` (`None`: a generated row index, so every column is
/// data), in source header order.
fn data_column_indices(column_count: usize, time_column: Option<usize>) -> Vec<usize> {
    (0..column_count)
        .filter(|&index| Some(index) != time_column)
        .collect()
}

/// Whether a file of `column_count` columns, with this time-index decision,
/// leaves nothing to plot ([`GlydeError::SingleColumnFile`]): no data column
/// at all (corpus case 18: a lone timestamp column), or a lone column that is
/// neither timestamps nor numbers. The latter is far more often a wrong
/// delimiter collapsing every row into one field than a dataset, and failing
/// cleanly is what lets a bad delimiter correction be noticed rather than
/// opened as one giant text series.
pub(crate) fn has_nothing_to_plot(
    column_count: usize,
    time_column: Option<usize>,
    generated: Option<&GeneratedIndexReason>,
) -> bool {
    data_column_indices(column_count, time_column).is_empty()
        || (column_count == 1 && matches!(generated, Some(GeneratedIndexReason::Unreadable { .. })))
}

/// [`Dataset::time_column_name`] for a time index read from `time_column`,
/// or [`GENERATED_TIME_COLUMN_NAME`] when there is none.
fn time_column_name(column_names: &[String], time_column: Option<usize>) -> String {
    time_column.map_or_else(
        || GENERATED_TIME_COLUMN_NAME.to_string(),
        |index| column_names[index].clone(),
    )
}

/// Types captured column text into a [`Dataset`] incrementally (issue #114).
///
/// Before #114, every progressive checkpoint re-derived its dataset from
/// the whole prefix read so far, and the final dataset was derived once more
/// from scratch: with checkpoints at 20k, 40k, 80k, … rows that is roughly
/// twice the typing work of the file itself, and typing (number and
/// timestamp parsing) — not tokenizing — is where an in-memory open spends
/// most of its time. The builder keeps each column's [`ColumnInference`] and
/// the time axis's parsed timestamps between calls, so every row is typed
/// once: a checkpoint only types the rows that arrived since the previous
/// one and hands out a copy, and the final call types the tail and hands
/// over the vectors themselves.
///
/// Columns are independent, so they are typed in parallel on the `rayon`
/// compute pool (docs/ARCHITECTURE.md §Threading model), the time axis
/// alongside them.
#[derive(Default)]
struct DatasetBuilder {
    time: TimeAxisBuilder,
    /// One per source column, in header order. The time-index candidate's
    /// entry is only extended once the candidate turns out *not* to be a
    /// time index (SPEC §2.1 "Files without a time column") — until then its
    /// text is typed by `time` alone, so the common case types it once.
    columns: Vec<ColumnInference>,
    /// The time column the previous [`Self::advance`] settled on (`Some(None)`:
    /// the generated row index), or `None` before the first call.
    time_column: Option<Option<usize>>,
    /// Set when an `advance` settles on a different time column than the one
    /// before it — a candidate can be demoted when a later row runs
    /// backwards. Everything derived from earlier checkpoints (a pyramid
    /// cursor, above all) then describes a different axis and column set.
    layout_changed: bool,
}

impl DatasetBuilder {
    /// A checkpoint's dataset over every row captured so far; the builder
    /// keeps its state for the next call.
    fn snapshot(
        &mut self,
        outcome: &CsvParseOutcome,
        columns_text: &[ColumnText],
        overrides: IngestOverrides,
    ) -> Result<(Dataset, TimeIndexInference)> {
        let (time, inference) = self.advance(outcome, columns_text, overrides)?;
        let time = time.unwrap_or_else(|| self.time.absolute_snapshot());
        let decimal_separator = outcome.decimal_separator;
        let columns = data_column_indices(outcome.column_names.len(), inference.time_column)
            .into_iter()
            .map(|index| {
                let text = &columns_text[index];
                self.columns[index].snapshot(outcome.column_names[index].clone(), |row| {
                    normalize_decimal_field(text.field(row), decimal_separator)
                })
            })
            .collect();
        let name = time_column_name(&outcome.column_names, inference.time_column);
        Ok((assemble_dataset(time, name, columns, overrides), inference))
    }

    /// Whether the time column changed since this was last asked (see
    /// [`Self::layout_changed`]), clearing the flag.
    fn take_layout_changed(&mut self) -> bool {
        std::mem::take(&mut self.layout_changed)
    }

    /// The final dataset, handing over the typed vectors rather than copying
    /// them.
    fn finish(
        mut self,
        outcome: &CsvParseOutcome,
        columns_text: &[ColumnText],
        overrides: IngestOverrides,
    ) -> Result<(Dataset, TimeIndexInference)> {
        let (time, inference) = self.advance(outcome, columns_text, overrides)?;
        let time = time.unwrap_or_else(|| self.time.take_absolute());
        let decimal_separator = outcome.decimal_separator;
        let name = time_column_name(&outcome.column_names, inference.time_column);
        log_time_index_decision(
            &name,
            &inference,
            columns_text.first().map_or(0, ColumnText::len),
        );
        let columns = std::mem::take(&mut self.columns)
            .into_iter()
            .enumerate()
            .filter(|&(index, _)| Some(index) != inference.time_column)
            .map(|(index, column)| {
                let text = &columns_text[index];
                column
                    .finish(outcome.column_names[index].clone(), |row| {
                        normalize_decimal_field(text.field(row), decimal_separator)
                    })
                    .series
            })
            .collect();
        Ok((assemble_dataset(time, name, columns, overrides), inference))
    }

    /// Types every row not yet typed, in every column. Returns the time axis
    /// only when it is *not* the incrementally-built absolute axis (a
    /// progressive index or the generated row index, both cheap and rare);
    /// otherwise the caller reads it from [`Self::time`].
    fn advance(
        &mut self,
        outcome: &CsvParseOutcome,
        columns_text: &[ColumnText],
        overrides: IngestOverrides,
    ) -> Result<(Option<TimeAxis>, TimeIndexInference)> {
        let column_count = outcome.column_names.len();
        let row_count = columns_text.first().map_or(0, ColumnText::len);
        let candidate = time_column_candidate(overrides, column_count);
        let check_monotonic = checks_monotonicity(overrides, column_count);
        self.columns
            .resize_with(column_count, ColumnInference::default);
        let decimal_separator = outcome.decimal_separator;
        let (time, ()) = rayon::join(
            || match candidate {
                Some(index) => self.time.advance(
                    &columns_text[index],
                    &outcome.column_names[index],
                    overrides,
                    check_monotonic,
                ),
                None => Ok(TimeAdvance {
                    axis: Some(row_ordinal_axis(row_count)),
                    generated: Some(GeneratedIndexReason::Requested),
                    timestamp_format_ambiguous: false,
                }),
            },
            || {
                use rayon::prelude::*;
                self.columns
                    .par_iter_mut()
                    .zip(columns_text)
                    .enumerate()
                    .filter(|&(index, _)| Some(index) != candidate)
                    .for_each(|(_, (column, text))| {
                        column.extend(text.len(), |row| {
                            normalize_decimal_field(text.field(row), decimal_separator)
                        })
                    });
            },
        );
        let time = time?;

        let time_column = if time.generated.is_some() {
            None
        } else {
            candidate
        };
        if let (None, Some(index)) = (time_column, candidate) {
            // Demoted: the candidate is a series after all, typed like any
            // other (from row 0 if this is the first call that knows it).
            let text = &columns_text[index];
            self.columns[index].extend(text.len(), |row| {
                normalize_decimal_field(text.field(row), decimal_separator)
            });
        }
        if self
            .time_column
            .is_some_and(|previous| previous != time_column)
        {
            self.layout_changed = true;
        }
        self.time_column = Some(time_column);

        if has_nothing_to_plot(column_count, time_column, time.generated.as_ref()) {
            return Err(GlydeError::SingleColumnFile);
        }
        Ok((
            time.axis,
            TimeIndexInference {
                timestamp_format_ambiguous: time.timestamp_format_ambiguous,
                time_column,
                generated: time.generated,
            },
        ))
    }
}

/// What one [`TimeAxisBuilder::advance`] settled.
struct TimeAdvance {
    /// `None` when the axis is the incrementally-built absolute one, read
    /// from the builder instead.
    axis: Option<TimeAxis>,
    /// `Some` when the candidate is not a time index and `axis` is the
    /// generated row index.
    generated: Option<GeneratedIndexReason>,
    timestamp_format_ambiguous: bool,
}

/// [`DatasetBuilder`]'s time-axis half: SPEC §2.1's timestamp-format scan and
/// the time-candidate verdict, fed incrementally, and the timestamps parsed
/// so far under the format the scan currently settles on. Should a later row
/// change that decision, the timestamps are re-parsed from the first row
/// under the new format, so the result is always exactly what a from-scratch
/// pass over the same rows produces.
#[derive(Default)]
struct TimeAxisBuilder {
    scan: TimestampFormatScan,
    candidate: TimeCandidateScan,
    observed: usize,
    parsed: Vec<Timestamp>,
    parsed_format: Option<TimestampFormat>,
}

impl TimeAxisBuilder {
    fn advance(
        &mut self,
        text: &ColumnText,
        column_name: &str,
        overrides: IngestOverrides,
        check_monotonic: bool,
    ) -> Result<TimeAdvance> {
        let total = text.len();
        for row in self.observed..total {
            let field = text.field(row);
            if overrides.timestamp_format.is_none() {
                self.scan.observe(field);
            }
            self.candidate.observe(field);
        }
        self.observed = total;
        let inference = match overrides.timestamp_format {
            // A user override settles the format outright, never scanned —
            // the same as `resolve_timestamp_format`.
            Some(format) => Some(TimestampFormatInference {
                format,
                ambiguous: false,
            }),
            None => self.scan.clone().finish(),
        };

        // SPEC §2.1 "Files without a time column": a numeric column that
        // runs backwards or never advances is a signal, and one that is
        // neither timestamps nor numbers is no index at all (issue #94) —
        // either way the row index stands in for it.
        if let Some(reason) =
            self.candidate
                .verdict(inference.is_some(), check_monotonic, column_name)
        {
            self.parsed = Vec::new();
            self.parsed_format = None;
            return Ok(TimeAdvance {
                axis: Some(row_ordinal_axis(total)),
                generated: Some(reason),
                timestamp_format_ambiguous: false,
            });
        }

        match inference {
            Some(format_inference) => {
                if self.parsed_format != Some(format_inference.format) {
                    self.parsed.clear();
                    self.parsed_format = Some(format_inference.format);
                }
                // Exact, for the same reason as `ColumnInference::extend`.
                self.parsed
                    .reserve_exact(total.saturating_sub(self.parsed.len()));
                for row in self.parsed.len()..total {
                    self.parsed
                        .push(parse_timestamp(text.field(row), format_inference.format)?);
                }
                Ok(TimeAdvance {
                    axis: None,
                    generated: None,
                    timestamp_format_ambiguous: format_inference.ambiguous,
                })
            }
            // SPEC §2.1: no recognized absolute-timestamp format matched every
            // field, but every field is a number (the verdict above saw to
            // that), so this is a progressive numeric index (corpus case 35).
            None => {
                self.parsed = Vec::new();
                self.parsed_format = None;
                let values = (0..total)
                    .map(|row| parse_progressive_value(text.field(row)))
                    .collect::<Result<Vec<f64>>>()?;
                Ok(TimeAdvance {
                    axis: Some(TimeAxis::Progressive {
                        values: ProgressiveValues::Memory(values),
                    }),
                    generated: None,
                    timestamp_format_ambiguous: false,
                })
            }
        }
    }

    fn absolute_snapshot(&self) -> TimeAxis {
        TimeAxis::Absolute {
            timestamps: Timestamps::Memory(self.parsed.clone()),
            format: self
                .parsed_format
                .expect("advance returned no axis, so it parsed an absolute one"),
        }
    }

    fn take_absolute(&mut self) -> TimeAxis {
        TimeAxis::Absolute {
            timestamps: Timestamps::Memory(std::mem::take(&mut self.parsed)),
            format: self
                .parsed_format
                .expect("advance returned no axis, so it parsed an absolute one"),
        }
    }
}

fn assemble_dataset(
    time: TimeAxis,
    time_column_name: String,
    columns: Vec<Series>,
    overrides: IngestOverrides,
) -> Dataset {
    let mut dataset = Dataset {
        time,
        time_column_name,
        columns,
    };
    if overrides.sort_by_time {
        sort_dataset_by_time(&mut dataset);
    }
    dataset
}

/// SPEC §2.1's "[Sort]" affordance: reorders `dataset`'s time axis and every
/// column so timestamps become non-decreasing, applying one permutation to
/// all of them in lockstep so each sample stays paired with its own row. A
/// tie-preserving ordering, so rows that already share a tick value (SPEC §2.1's
/// "duplicate timestamps ... preserved") keep their original relative order
/// among themselves rather than being shuffled.
///
/// Only called from [`build_dataset`], the in-memory conversion path — a
/// spilled column's on-disk file is append-only and cannot be permuted in
/// place, so [`choose_storage`] logs a warning and opens unsorted instead of
/// reaching this at all when [`IngestOverrides::sort_by_time`] is set on a
/// file that must spill.
fn sort_dataset_by_time(dataset: &mut Dataset) {
    let tick_count = dataset.time.len();
    if tick_count < 2 {
        return;
    }

    let ticks = dataset.time.to_pyramid_ticks();
    let mut order: Vec<usize> = (0..tick_count).collect();
    // Include the source index in the comparison: equal timestamps retain
    // their original order while the sort itself needs no merge buffer.
    order.sort_unstable_by(|&a, &b| ticks[a].cmp(&ticks[b]).then(a.cmp(&b)));

    dataset.time.reorder(&order);
    for series in &mut dataset.columns {
        series.reorder(&order);
    }
}

/// What ingestion settled about the time index beyond the axis itself — the
/// things `super::report` needs to describe an open in SPEC §1.2's
/// inference bar but cannot re-derive from a finished [`Dataset`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TimeIndexInference {
    /// SPEC §2.1's day-vs-month ambiguity rule fired: the format is the
    /// documented default, not a discriminated match, so the inference bar
    /// reports it low-confidence with a one-click swap.
    pub(crate) timestamp_format_ambiguous: bool,
    /// The 0-based header position of the column the time index was read
    /// from, or `None` for the generated row index.
    pub(crate) time_column: Option<usize>,
    /// Why the row index was generated, when it was. The load still succeeds
    /// either way — SPEC §1.3 "never abort the load" — and every reason but
    /// [`GeneratedIndexReason::Requested`] is reported low-confidence so the
    /// substitution is never silent (Golden Rule 2).
    pub(crate) generated: Option<GeneratedIndexReason>,
}

/// [`Dataset::time_column_name`] when the time index is generated rather
/// than read from a column.
pub const GENERATED_TIME_COLUMN_NAME: &str = "row index";

/// Why a file is plotted against a generated row index `0, 1, 2, …` instead
/// of one of its own columns (SPEC §2.1 "Files without a time column").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratedIndexReason {
    /// The user asked for the row index (SPEC §1.2's time-column correction).
    Requested,
    /// The candidate column is numeric, but its value drops at row index
    /// `row` (0-based, the same position the generated axis gives that row),
    /// so it is a signal, not a time index. NaN counts as a drop: a time
    /// index has no missing values.
    NotMonotonic { column: String, row: u64 },
    /// The candidate column is numeric but holds one value throughout — it
    /// never advances, so it cannot place samples in time.
    Constant { column: String },
    /// The candidate column is numeric and never decreases, but it stays on
    /// the same value for most rows, changing on only `changes` of them — a
    /// staircase such as an operating-point or test-step number. Plotted
    /// against it, every sample of one step would land on the same x.
    MostlyRepeated { column: String, changes: u64 },
    /// The candidate column is neither timestamps in a supported format nor
    /// numbers (issue #94).
    Unreadable { column: String },
}

/// CLAUDE.md "every ingestion decision is logged": the time index is the
/// first one the plot depends on, and generating one the file does not
/// contain is the loudest decision ingestion can take on the user's behalf.
fn log_time_index_decision(time_column_name: &str, inference: &TimeIndexInference, rows: usize) {
    match &inference.generated {
        None => info!(
            time_column = %time_column_name,
            position = inference.time_column,
            "time index read from a column of the file (SPEC §2.1)"
        ),
        Some(GeneratedIndexReason::Requested) => info!(
            rows,
            "no time column, as requested: plotting every column against the row index"
        ),
        Some(GeneratedIndexReason::NotMonotonic { column, row }) => warn!(
            candidate = %column,
            first_drop_at_row = row,
            rows,
            "no time column detected: the first column is numeric but runs backwards, so it is \
             plotted as a signal against a generated row index (SPEC §2.1), reported \
             low-confidence in the inference bar"
        ),
        Some(GeneratedIndexReason::MostlyRepeated { column, changes }) => warn!(
            candidate = %column,
            changes,
            rows,
            "no time column detected: the first column never decreases but stays on the same \
             value for most rows (a staircase, not a time axis), so it is plotted as a signal \
             against a generated row index (SPEC §2.1), reported low-confidence in the \
             inference bar"
        ),
        Some(GeneratedIndexReason::Constant { column }) => warn!(
            candidate = %column,
            rows,
            "no time column detected: the first column holds a single value throughout, so it is \
             plotted as a signal against a generated row index (SPEC §2.1), reported \
             low-confidence in the inference bar"
        ),
        Some(GeneratedIndexReason::Unreadable { column }) => warn!(
            candidate = %column,
            rows,
            "the time column matches no timestamp format (SPEC §2.1) and is not numeric either; \
             it is kept as a series and the file is plotted against a generated row index \
             instead of being refused (SPEC §1.3 \"never abort the load\"), reported \
             low-confidence in the inference bar"
        ),
    }
}

/// SPEC §2.1's progressive numeric index: a plain number with no
/// absolute-time meaning. A field that is not even that is not a time index
/// value at all — see [`TimeCandidateScan`].
fn parse_progressive_value(field: &str) -> Result<f64> {
    field
        .trim()
        .parse::<f64>()
        .map_err(|_| GlydeError::NonNumericTimeIndex {
            input: field.to_string(),
        })
}

/// Whether a candidate column can be the time index, decided from its text
/// one field at a time, so the in-memory, spilled and progressive paths all
/// reach the same verdict without holding the column (SPEC §2.1 "Files
/// without a time column").
///
/// A column of dates and times is unmistakably a time column, even with rows
/// out of order (SPEC §2.1 reports those with [Sort]/[Keep as-is]). A
/// *numeric* column is not: an epoch counter and an accelerometer channel
/// look alike field by field. What tells them apart is order — a time index
/// never runs backwards and does advance — so a numeric candidate that drops
/// (or is constant, or a staircase that stays on one value for most rows) is
/// a signal and the row index stands in for it. The test
/// is on the parsed `f64`, the same value for every numeric format (epoch,
/// LabVIEW, Excel serial, progressive), so it never depends on which format
/// the column also happens to match.
#[derive(Debug, Clone, Default)]
pub(crate) struct TimeCandidateScan {
    rows: usize,
    /// Fields that are not numbers (issue #94).
    non_numeric: usize,
    previous: Option<f64>,
    first_drop: Option<usize>,
    /// How many steps (row to the next) increased the value.
    advances: u64,
}

impl TimeCandidateScan {
    pub(crate) fn observe(&mut self, field: &str) {
        let row = self.rows;
        self.rows += 1;
        let Ok(value) = parse_progressive_value(field) else {
            self.non_numeric += 1;
            return;
        };
        if let Some(previous) = self.previous {
            // Incomparable (a NaN on either side) counts as a drop too.
            match value.partial_cmp(&previous) {
                Some(std::cmp::Ordering::Greater) => self.advances += 1,
                Some(std::cmp::Ordering::Equal) => {}
                Some(std::cmp::Ordering::Less) | None => {
                    self.first_drop.get_or_insert(row);
                }
            }
        }
        self.previous = Some(value);
    }

    /// `None` when the candidate is a time index; otherwise why it is not.
    /// `matches_timestamp_format`: whether every field also parses under the
    /// (inferred or user-chosen) timestamp format. `check_monotonic` is off
    /// for a column the user picked deliberately.
    pub(crate) fn verdict(
        &self,
        matches_timestamp_format: bool,
        check_monotonic: bool,
        column: &str,
    ) -> Option<GeneratedIndexReason> {
        if self.non_numeric > 0 {
            return (!matches_timestamp_format).then(|| GeneratedIndexReason::Unreadable {
                column: column.to_string(),
            });
        }
        if !check_monotonic {
            return None;
        }
        if let Some(row) = self.first_drop {
            return Some(GeneratedIndexReason::NotMonotonic {
                column: column.to_string(),
                row: row as u64,
            });
        }
        if self.rows < 2 {
            return None;
        }
        if self.advances == 0 {
            return Some(GeneratedIndexReason::Constant {
                column: column.to_string(),
            });
        }
        // A time index advances on most rows; duplicates are occasional
        // anomalies (SPEC §2.1 "preserved, flagged"). One that advances on
        // fewer than half of its steps is a staircase — every row of one step
        // would share one x. The threshold is an assumption flagged in
        // `CHANGELOG.md`, not a SPEC number.
        let steps = (self.rows - 1) as u64;
        (self.advances * 2 < steps).then(|| GeneratedIndexReason::MostlyRepeated {
            column: column.to_string(),
            changes: self.advances,
        })
    }
}

/// The generated row index (SPEC §2.1 "Files without a time column", issue
/// #94): row 0, 1, 2, … of the rows kept after SPEC §1.3's skipping, as a
/// progressive numeric axis, so a file without a usable time column still
/// opens, still plots, and still keeps its rows in source order.
fn row_ordinal_axis(row_count: usize) -> TimeAxis {
    TimeAxis::Progressive {
        values: ProgressiveValues::Memory((0..row_count).map(|row| row as f64).collect()),
    }
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
    on_checkpoint: Option<&mut dyn FnMut(Checkpoint)>,
    overrides: IngestOverrides,
) -> Result<(CsvParseOutcome, Dataset, TimeIndexInference)> {
    let column_names = sniff.column_names().to_vec();
    let column_count = column_names.len();
    let decimal_separator = sniff.decimal_separator;
    let candidate = time_column_candidate(overrides, column_count);
    let check_monotonic = checks_monotonicity(overrides, column_count);

    // --- Pass 1: infer, retaining nothing -----------------------------------
    let mut time_scan = TimestampFormatScan::default();
    // SPEC §2.1 "Files without a time column" / issue #94: the same verdict
    // the in-memory path reaches, asked incrementally so a spilled open
    // reaches it without holding the column.
    let mut candidate_scan = TimeCandidateScan::default();
    // One dtype scan per column, the time candidate's included: whether the
    // candidate is a time index is only known once pass 1 ends, and if it is
    // not, it is written as a series in pass 2.
    let mut dtype_scans: Vec<ColumnDtypeScan> = (0..column_count)
        .map(|_| ColumnDtypeScan::default())
        .collect();
    //
    // Rows arrive in bounded column-major batches (`stream_in_batches`) and
    // each batch's columns are scanned in parallel on the `rayon` pool
    // (issue #114): every column's scan is independent and sees its own
    // fields in row order, so the result is the row-at-a-time scan's
    // exactly — and this pass is what a spilled open's first plot waits on.
    stream_in_batches(path, sniff, column_count, |batch| {
        rayon::join(
            || {
                if let Some(index) = candidate {
                    for time_field in batch[index].iter() {
                        time_scan.observe(time_field);
                        candidate_scan.observe(time_field);
                    }
                }
            },
            || {
                use rayon::prelude::*;
                dtype_scans
                    .par_iter_mut()
                    .zip(batch)
                    .for_each(|(scan, text)| {
                        for field in text.iter() {
                            scan.observe(&normalize_decimal_field(field, decimal_separator));
                        }
                    });
            },
        );
        Ok(())
    })?;

    // docs/ROADMAP.md M4: a user's timestamp-format override settles the
    // question outright, the same as `resolve_timestamp_format` does for the
    // in-memory path — never ambiguous, since it is a deliberate choice.
    let timestamp_format = match overrides.timestamp_format {
        Some(format) => Some(TimestampFormatInference {
            format,
            ambiguous: false,
        }),
        None => time_scan.finish(),
    };
    let generated = match candidate {
        Some(index) => candidate_scan.verdict(
            timestamp_format.is_some(),
            check_monotonic,
            &column_names[index],
        ),
        None => Some(GeneratedIndexReason::Requested),
    };
    let time_column = if generated.is_some() { None } else { candidate };
    let timestamp_format = timestamp_format.filter(|_| time_column.is_some());
    if has_nothing_to_plot(column_count, time_column, generated.as_ref()) {
        return Err(GlydeError::SingleColumnFile);
    }
    let data_columns = data_column_indices(column_count, time_column);
    let choices: Vec<ColumnDtypeChoice> = data_columns
        .iter()
        .map(|&index| dtype_scans[index].finish())
        .collect();
    let time_column_name = time_column_name(&column_names, time_column);
    let inference = TimeIndexInference {
        timestamp_format_ambiguous: timestamp_format.is_some_and(|inference| inference.ambiguous),
        time_column,
        generated,
    };

    // --- Pass 2: type every row straight into its spill file ----------------
    // `.with_overrides_signature`: these spill files are always freshly
    // written on this call (never read-and-reused within it), so nothing
    // reachable today collides across two different overrides for the same
    // path — but scoping the stem consistently with the pyramid cache keeps
    // that true if a later reopen ever starts reading them back (issue #92's
    // Level 0 read-through wiring, not yet wired into the open path) rather
    // than leaving the same class of staleness bug latent for that PR to
    // rediscover.
    let stem = CacheKey::for_path(path)?
        .with_overrides_signature(super::overrides_signature(overrides))
        .cache_stem();
    let mut time_writer = TimeAxisSpillWriter::create(
        cache_dir,
        &stem,
        timestamp_format.map(|inference| inference.format),
        inference.generated.is_some(),
    )?;
    let mut column_writers: Vec<ColumnSpillWriter> = choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            ColumnSpillWriter::create(cache_dir, &format!("{stem}.c{index}"), choice.dtype)
        })
        .collect::<Result<_>>()?;
    let data_column_names: Vec<String> = data_columns
        .iter()
        .map(|&index| column_names[index].clone())
        .collect();

    let mut preview = SpillPreview::new(
        on_checkpoint,
        time_column_name.clone(),
        data_column_names.clone(),
        &choices,
        timestamp_format.map(|inference| inference.format),
    );

    // Batched like pass 1 (issue #114): each column's writer — and its
    // preview vector — sees its own fields in row order, on the `rayon`
    // pool, and the preview's checkpoints are emitted afterwards over
    // exactly the row prefixes the row-at-a-time loop emitted them at.
    let outcome = stream_in_batches(path, sniff, column_count, |batch| {
        write_spill_batch(
            batch,
            time_column,
            &data_columns,
            &mut time_writer,
            &mut column_writers,
            decimal_separator,
            &mut preview,
        )
    })?;
    drop(preview);

    log_time_index_decision(&time_column_name, &inference, outcome.row_count as usize);
    let time = time_writer.finish()?;
    let columns = column_writers
        .into_iter()
        .zip(data_column_names)
        .zip(&choices)
        .map(|((writer, name), choice)| {
            log_dtype_choice(*choice, outcome.row_count as usize);
            writer.finish(name)
        })
        .collect::<Result<Vec<Series>>>()?;

    info!(
        row_count = outcome.row_count,
        column_count,
        cache_dir = %cache_dir.display(),
        "file materialized through the on-disk spill cache (SPEC §5.1)"
    );

    Ok((
        outcome,
        Dataset {
            time,
            time_column_name,
            columns,
        },
        inference,
    ))
}

/// Streams every row of `path` (see [`super::csv::stream_path`]) to
/// `process` in column-major batches of up to [`SCAN_BATCH_ROWS`] rows —
/// one [`ColumnText`] per column — in row order, returning the parse's
/// outcome (issue #114).
///
/// Tokenizing runs on a scoped reader thread while `process` handles the
/// previous batch on the calling thread, so a spilled pass costs roughly the
/// slower of the two rather than their sum. At most
/// [`SPILL_BATCHES_IN_FLIGHT`] filled batches wait between them and drained
/// buffers are handed back for reuse, so memory stays a few MB at any file
/// size (SPEC §5.1). `process` — and with it every writer, the preview and
/// its checkpoint callback — never leaves the calling thread.
///
/// A `process` failure stops the reader and is returned in preference to
/// anything the reader then reports: it concerns earlier rows, and is the
/// error the row-at-a-time loop this replaces would have stopped on.
fn stream_in_batches(
    path: &Path,
    sniff: &Sniff,
    column_count: usize,
    mut process: impl FnMut(&[ColumnText]) -> Result<()>,
) -> Result<CsvParseOutcome> {
    let new_batch =
        || -> Vec<ColumnText> { (0..column_count).map(|_| ColumnText::default()).collect() };
    let (filled_tx, filled_rx) =
        std::sync::mpsc::sync_channel::<Vec<ColumnText>>(SPILL_BATCHES_IN_FLIGHT);
    let (drained_tx, drained_rx) = std::sync::mpsc::channel::<Vec<ColumnText>>();

    std::thread::scope(|scope| {
        let reader = scope.spawn(move || {
            let consumer_stopped = || GlydeError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other("the spill writer stopped reading rows"),
            };
            let mut batch = new_batch();
            let outcome = super::csv::stream_path(path, sniff, &mut |row: RowFields<'_>| {
                for (index, column) in batch.iter_mut().enumerate() {
                    column.push(row.get(index).unwrap_or_default());
                }
                if batch[0].len() >= SCAN_BATCH_ROWS {
                    let next = drained_rx.try_recv().unwrap_or_else(|_| new_batch());
                    filled_tx
                        .send(std::mem::replace(&mut batch, next))
                        .map_err(|_| consumer_stopped())?;
                }
                Ok(())
            })?;
            if batch[0].len() > 0 {
                filled_tx.send(batch).map_err(|_| consumer_stopped())?;
            }
            Ok(outcome)
        });

        let processed = loop {
            let Ok(mut batch) = filled_rx.recv() else {
                break Ok(());
            };
            if let Err(error) = process(&batch) {
                break Err(error);
            }
            batch.iter_mut().for_each(ColumnText::clear);
            let _ = drained_tx.send(batch);
        };
        // Unblocks a reader waiting on a full channel after a failure.
        drop(filled_rx);
        let read = reader
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        processed.and(read)
    })
}

/// How many filled batches [`stream_in_batches`] lets the reader get ahead
/// of the writer by.
const SPILL_BATCHES_IN_FLIGHT: usize = 2;

/// Types one batch of rows into the spill writers (pass 2 of
/// [`load_spilled`]) — the time axis and every data column in parallel —
/// feeding the progressive preview as it goes.
///
/// Fails exactly as the row-at-a-time loop did: with the error of the
/// earliest failing row, and within that row the time column's before any
/// data column's and a lower column index's before a higher one's — after
/// the preview has counted (and checkpointed) every row before it.
fn write_spill_batch(
    batch: &[ColumnText],
    time_column: Option<usize>,
    data_columns: &[usize],
    time_writer: &mut TimeAxisSpillWriter,
    column_writers: &mut [ColumnSpillWriter],
    decimal_separator: super::infer::DecimalSeparator,
    preview: &mut SpillPreview<'_>,
) -> Result<()> {
    let rows = batch.first().map_or(0, ColumnText::len);
    let collect = preview.is_active();
    // A generated row index reads no field at all (`TimeAxisSpillWriter::RowOrdinal`).
    let time_text = time_column.map(|index| &batch[index]);
    let data_text: Vec<&ColumnText> = data_columns.iter().map(|&index| &batch[index]).collect();
    let SpillPreview {
        timestamps,
        progressive,
        columns: preview_columns,
        ..
    } = &mut *preview;

    // Each task reports its first failure as (row, order, error), where
    // `order` ranks the time column before every data column.
    let (time_failure, column_failures) = rayon::join(
        || -> Option<(usize, usize, GlydeError)> {
            for row in 0..rows {
                match time_writer.push(time_text.map_or("", |text| text.field(row))) {
                    Ok(value) if collect => match value {
                        TimeValue::Absolute(timestamp) => timestamps.push(timestamp),
                        TimeValue::Progressive(value) => progressive.push(value),
                    },
                    Ok(_) => {}
                    Err(error) => return Some((row, 0, error)),
                }
            }
            None
        },
        || -> Vec<Option<(usize, usize, GlydeError)>> {
            use rayon::prelude::*;
            column_writers
                .par_iter_mut()
                .zip(preview_columns.par_iter_mut())
                .zip(data_text.par_iter())
                .enumerate()
                .map(|(index, ((writer, preview_column), text))| {
                    for row in 0..rows {
                        let normalized =
                            normalize_decimal_field(text.field(row), decimal_separator);
                        match writer.push(&normalized) {
                            Ok(value) if collect => preview_column.push(value),
                            Ok(_) => {}
                            Err(error) => return Some((row, index + 1, error)),
                        }
                    }
                    None
                })
                .collect()
        },
    );

    let first_failure = std::iter::once(time_failure)
        .chain(column_failures)
        .flatten()
        .min_by_key(|&(row, order, _)| (row, order));
    let completed_rows = first_failure.as_ref().map_or(rows, |&(row, _, _)| row);
    preview.advance_rows(completed_rows as u64);
    match first_failure {
        Some((_, _, error)) => Err(error),
        None => Ok(()),
    }
}

/// Rows per batch in the spilled path's inference scan: large enough that
/// handing a batch's columns to the `rayon` pool costs nothing next to
/// scanning them, small enough to stay a few MB at any file size (SPEC §5.1).
const SCAN_BATCH_ROWS: usize = 16_384;

/// Writes a [`TimeAxis`] to the spill cache one row at a time.
///
/// The `Absolute` payload is boxed: it carries three writers to the
/// `Progressive` arm's one, and each writer owns two `PathBuf`s, so inline it
/// would make every `Progressive` axis pay for storage it never uses
/// (`clippy::large_enum_variant`).
enum TimeAxisSpillWriter {
    Absolute(Box<AbsoluteAxisSpillWriter>),
    Progressive(SpillVecWriter<f64>),
    /// The generated row index (SPEC §2.1 "Files without a time column",
    /// issue #94): the row ordinal is written in place of a time column.
    /// Structurally a `Progressive` axis — the difference is that the values
    /// come from the row counter rather than from the file, which is what the
    /// inference bar flags.
    RowOrdinal {
        values: SpillVecWriter<f64>,
        next_row: u64,
    },
}

/// [`TimeAxisSpillWriter::Absolute`]'s payload: SPEC §2.1's ticks, per-row
/// [`TimeUnit`] and per-row UTC offset, each in its own spill file.
struct AbsoluteAxisSpillWriter {
    ticks: SpillVecWriter<i128>,
    units: SpillVecWriter<u8>,
    offsets: SpillVecWriter<i64>,
    format: TimestampFormat,
}

impl TimeAxisSpillWriter {
    fn create(
        cache_dir: &Path,
        stem: &str,
        format: Option<TimestampFormat>,
        generated_index: bool,
    ) -> Result<Self> {
        match format {
            Some(format) => Ok(TimeAxisSpillWriter::Absolute(Box::new(
                AbsoluteAxisSpillWriter {
                    ticks: SpillVecWriter::create(cache_dir, &format!("{stem}.ts"))?,
                    units: SpillVecWriter::create(cache_dir, &format!("{stem}.tsunit"))?,
                    offsets: SpillVecWriter::create(cache_dir, &format!("{stem}.tsoffset"))?,
                    format,
                },
            ))),
            None if generated_index => Ok(TimeAxisSpillWriter::RowOrdinal {
                values: SpillVecWriter::create(cache_dir, &format!("{stem}.tsprogressive"))?,
                next_row: 0,
            }),
            None => Ok(TimeAxisSpillWriter::Progressive(SpillVecWriter::create(
                cache_dir,
                &format!("{stem}.tsprogressive"),
            )?)),
        }
    }

    /// Types one row's time field and appends it. Returns the typed value so
    /// [`SpillPreview`] can record it without parsing the same field twice.
    fn push(&mut self, field: &str) -> Result<TimeValue> {
        match self {
            TimeAxisSpillWriter::Absolute(writer) => {
                let timestamp = parse_timestamp(field, writer.format)?;
                writer.ticks.push(timestamp.ticks)?;
                writer.units.push(time_unit_code(timestamp.unit))?;
                writer.offsets.push(
                    timestamp
                        .offset_seconds
                        .map_or(NO_UTC_OFFSET, |offset| offset as i64),
                )?;
                Ok(TimeValue::Absolute(timestamp))
            }
            TimeAxisSpillWriter::Progressive(values) => {
                let value = parse_progressive_value(field)?;
                values.push(value)?;
                Ok(TimeValue::Progressive(value))
            }
            // The field is deliberately not read — pass 1 already established
            // there is no time column to read it from, and substituting the
            // row ordinal is exactly the decision the inference bar reports.
            TimeAxisSpillWriter::RowOrdinal { values, next_row } => {
                let value = *next_row as f64;
                values.push(value)?;
                *next_row += 1;
                Ok(TimeValue::Progressive(value))
            }
        }
    }

    fn finish(self) -> Result<TimeAxis> {
        match self {
            TimeAxisSpillWriter::Absolute(writer) => Ok(TimeAxis::Absolute {
                timestamps: Timestamps::Spilled {
                    ticks: writer.ticks.finish()?,
                    units: writer.units.finish()?,
                    offsets: writer.offsets.finish()?,
                },
                format: writer.format,
            }),
            TimeAxisSpillWriter::Progressive(values)
            | TimeAxisSpillWriter::RowOrdinal { values, .. } => Ok(TimeAxis::Progressive {
                values: ProgressiveValues::Spilled(values.finish()?),
            }),
        }
    }
}

/// One row's typed time value, handed back by [`TimeAxisSpillWriter::push`].
#[derive(Debug, Clone, Copy)]
enum TimeValue {
    Absolute(Timestamp),
    Progressive(f64),
}

/// One row's typed data value, handed back by [`ColumnSpillWriter::push`],
/// borrowing the source text for the string case so the common numeric path
/// costs no allocation.
enum ColumnValue<'a> {
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(&'a str),
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

    /// Types one row's field and appends it. Returns the typed value so
    /// [`SpillPreview`] can record it without parsing the same field twice.
    fn push<'f>(&mut self, field: &'f str) -> Result<ColumnValue<'f>> {
        match self {
            ColumnSpillWriter::Bool(values) => {
                let value = super::infer::parse_bool_field(field).unwrap_or_default();
                values.push(u8::from(value))?;
                Ok(ColumnValue::Bool(value))
            }
            ColumnSpillWriter::I64(values) => {
                let value = field.trim().parse::<i64>().unwrap_or_default();
                values.push(value)?;
                Ok(ColumnValue::I64(value))
            }
            ColumnSpillWriter::F64(values, nan_runs) => {
                let value = field.trim().parse::<f64>().unwrap_or_default();
                nan_runs.observe(value);
                values.push(value)?;
                Ok(ColumnValue::F64(value))
            }
            ColumnSpillWriter::String(values) => {
                values.push(field)?;
                Ok(ColumnValue::Str(field))
            }
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

/// How many rows of a spilled open the progressive preview keeps in memory.
///
/// SPEC §5 requires a first meaningful plot within 2 s *for any file size*, so
/// a spilled open cannot simply skip progress reporting — that is the file
/// size where it matters most. It also cannot hand out a `Dataset` over spill
/// files that are still being written: they are published by an atomic rename,
/// and Windows will not rename a mapped file.
///
/// The way out is that a *preview* does not need every row. The preview keeps
/// the first rows' typed values in ordinary heap `Vec`s — the same shape the
/// in-memory path produces, so `views::time` needs no special case — and stops
/// growing at this cap, which bounds it at roughly 20 MB for a typical
/// ten-column file no matter how large the source is. The complete, spilled
/// dataset replaces it when the read finishes.
///
/// Sized so the doubling checkpoint schedule fires several times before the
/// cap (20k, 40k, 80k, 160k kept rows), which is what makes the plot visibly
/// fill rather than appear once.
const PREVIEW_MAX_ROWS: u64 = 200_000;

/// The spilled path's progressive preview (see [`PREVIEW_MAX_ROWS`]).
/// Accumulates typed values already computed by the spill writers — it never
/// re-parses a field — and emits a [`Checkpoint`] on the same
/// row-count-doubling schedule `super::csv` uses for the in-memory path, so a
/// caller cannot tell which storage produced a given progress update.
struct SpillPreview<'a> {
    on_checkpoint: Option<&'a mut dyn FnMut(Checkpoint)>,
    time_column_name: String,
    column_names: Vec<String>,
    format: Option<TimestampFormat>,
    timestamps: Vec<Timestamp>,
    progressive: Vec<f64>,
    columns: Vec<PreviewColumn>,
    rows: u64,
    next_checkpoint_rows: u64,
    pyramid_cursor: PyramidCursor,
}

/// One preview column's heap-backed values, in the dtype the scan pass
/// settled on.
enum PreviewColumn {
    Bool(Vec<bool>),
    I64(Vec<i64>),
    F64(Vec<f64>),
    String(Vec<String>),
}

impl PreviewColumn {
    fn new(dtype: Dtype) -> Self {
        match dtype {
            Dtype::Bool => PreviewColumn::Bool(Vec::new()),
            Dtype::I64 => PreviewColumn::I64(Vec::new()),
            Dtype::F64 => PreviewColumn::F64(Vec::new()),
            _ => PreviewColumn::String(Vec::new()),
        }
    }

    fn push(&mut self, value: ColumnValue<'_>) {
        match (self, value) {
            (PreviewColumn::Bool(values), ColumnValue::Bool(value)) => values.push(value),
            (PreviewColumn::I64(values), ColumnValue::I64(value)) => values.push(value),
            (PreviewColumn::F64(values), ColumnValue::F64(value)) => values.push(value),
            (PreviewColumn::String(values), ColumnValue::Str(value)) => {
                values.push(value.to_string())
            }
            // Unreachable: the writer and the preview were built from the same
            // `ColumnDtypeChoice`, so their variants always agree.
            _ => {}
        }
    }

    /// The first `rows` values as a [`Series`].
    fn to_series(&self, name: &str, rows: usize) -> Series {
        match self {
            PreviewColumn::Bool(values) => {
                Series::new(name, SeriesValues::Bool(values[..rows].to_vec()))
            }
            PreviewColumn::I64(values) => {
                Series::new(name, SeriesValues::I64(values[..rows].to_vec()))
            }
            PreviewColumn::F64(values) => Series::with_anomalies(
                name,
                SeriesValues::F64(values[..rows].to_vec()),
                Anomalies {
                    nan_runs: crate::series::detect_nan_runs(&values[..rows]),
                    ..Anomalies::default()
                },
            ),
            PreviewColumn::String(values) => {
                Series::new(name, SeriesValues::String(values[..rows].to_vec()))
            }
        }
    }

    fn release(&mut self) {
        match self {
            PreviewColumn::Bool(values) => {
                values.clear();
                values.shrink_to_fit();
            }
            PreviewColumn::I64(values) => {
                values.clear();
                values.shrink_to_fit();
            }
            PreviewColumn::F64(values) => {
                values.clear();
                values.shrink_to_fit();
            }
            PreviewColumn::String(values) => {
                values.clear();
                values.shrink_to_fit();
            }
        }
    }
}

impl<'a> SpillPreview<'a> {
    fn new(
        on_checkpoint: Option<&'a mut dyn FnMut(Checkpoint)>,
        time_column_name: String,
        column_names: Vec<String>,
        choices: &[ColumnDtypeChoice],
        format: Option<TimestampFormat>,
    ) -> Self {
        Self {
            on_checkpoint,
            time_column_name,
            column_names,
            format,
            timestamps: Vec::new(),
            progressive: Vec::new(),
            columns: choices
                .iter()
                .map(|choice| PreviewColumn::new(choice.dtype))
                .collect(),
            rows: 0,
            next_checkpoint_rows: FIRST_PROGRESS_CHECKPOINT_ROWS,
            pyramid_cursor: PyramidCursor::default(),
        }
    }

    fn is_active(&self) -> bool {
        self.on_checkpoint.is_some()
    }

    /// Counts `completed` more rows whose values every preview vector
    /// already holds (a batch may have pushed a few rows past them — see
    /// [`write_spill_batch`]), emitting each checkpoint the row-at-a-time
    /// count would have crossed, over exactly its own row prefix, and
    /// retiring the preview at [`PREVIEW_MAX_ROWS`].
    fn advance_rows(&mut self, completed: u64) {
        if !self.is_active() {
            return;
        }
        let target = self.rows + completed;
        while self.next_checkpoint_rows <= target.min(PREVIEW_MAX_ROWS) {
            self.emit(self.next_checkpoint_rows as usize);
            self.next_checkpoint_rows = self.next_checkpoint_rows.saturating_mul(2);
        }
        self.rows = target;
        if self.rows >= PREVIEW_MAX_ROWS {
            self.retire();
        }
    }

    /// A checkpoint over the first `rows` rows.
    fn emit(&mut self, rows: usize) {
        let dataset = Dataset {
            time: match self.format {
                Some(format) => TimeAxis::Absolute {
                    timestamps: Timestamps::Memory(self.timestamps[..rows].to_vec()),
                    format,
                },
                None => TimeAxis::Progressive {
                    values: ProgressiveValues::Memory(self.progressive[..rows].to_vec()),
                },
            },
            time_column_name: self.time_column_name.clone(),
            columns: self
                .columns
                .iter()
                .zip(&self.column_names)
                .map(|(column, name)| column.to_series(name, rows))
                .collect(),
        };
        let pyramids = self.pyramid_cursor.update(&dataset);
        if let Some(on_checkpoint) = self.on_checkpoint.as_deref_mut() {
            on_checkpoint(Checkpoint {
                dataset,
                pyramids,
                rows_read: rows as u64,
                // Issue #87: this preview exists *because* the file is being
                // streamed to disk, so every checkpoint it emits says so.
                spilled: true,
            });
        }
    }

    fn retire(&mut self) {
        info!(
            rows = self.rows,
            "progressive preview reached its row cap; the remaining rows stream straight to \
             the spill cache and the complete plot appears when the read finishes"
        );
        self.on_checkpoint = None;
        self.timestamps = Vec::new();
        self.progressive = Vec::new();
        for column in &mut self.columns {
            column.release();
        }
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
    /// Whether this open is streaming the file's samples to the on-disk cache
    /// instead of holding them in memory (issue #75's `choose_storage`
    /// decision, taken before a single row is read).
    ///
    /// A checkpoint's own `dataset` cannot answer this: a spilled open
    /// checkpoints from a bounded *in-memory* preview (see [`SpillPreview`]),
    /// so `Dataset::is_spilled` is `false` on every progress update and only
    /// becomes `true` on the completed one. Issue #87: SPEC §5.1 owes the user
    /// "a clear explanation and the affordable alternative" for that decision,
    /// and the moment it needs explaining is while the open is slow — which is
    /// exactly when only a checkpoint is available to carry it.
    pub spilled: bool,
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
/// **Both storage paths checkpoint**, on the same doubling schedule — SPEC §5's
/// "first meaningful plot, **any file size**: ≤ 2 s" applies to spilled files
/// most of all. The in-memory path checkpoints from the growing captured text;
/// the spilled path cannot hand out a `Dataset` over spill files that are still
/// being written (they are published by an atomic rename, and Windows will not
/// rename a mapped file), so it keeps a bounded in-memory preview of the first
/// rows instead — see [`PREVIEW_MAX_ROWS`]. Past that cap a spilled open stops
/// checkpointing and the complete plot appears when the read finishes.
pub(crate) fn load_with_outcome_progressive(
    path: &Path,
    on_checkpoint: impl FnMut(Checkpoint),
) -> Result<(CsvParseOutcome, Dataset, TimeIndexInference)> {
    load_with_outcome_progressive_using(path, IngestOverrides::default(), on_checkpoint)
}

/// [`load_with_outcome_progressive`] with [`IngestOverrides`] applied
/// (docs/ROADMAP.md M4) — `ingest::report::open_dataset_progressive_with_overrides`
/// uses this the same way `open_dataset_progressive` uses
/// [`load_with_outcome_progressive`].
pub(crate) fn load_with_outcome_progressive_with_overrides(
    path: &Path,
    overrides: IngestOverrides,
    on_checkpoint: impl FnMut(Checkpoint),
) -> Result<(CsvParseOutcome, Dataset, TimeIndexInference)> {
    load_with_outcome_progressive_using(path, overrides, on_checkpoint)
}

fn load_with_outcome_progressive_using(
    path: &Path,
    overrides: IngestOverrides,
    mut on_checkpoint: impl FnMut(Checkpoint),
) -> Result<(CsvParseOutcome, Dataset, TimeIndexInference)> {
    let cache_dir = os_spill_dir();
    match choose_storage(
        path,
        RamBudget::from_system(),
        cache_dir.as_deref(),
        overrides,
    )? {
        Storage::Spill(sniff) => {
            let cache_dir = cache_dir
                .as_deref()
                .expect("choose_storage refuses to spill without a cache dir");
            load_spilled(path, &sniff, cache_dir, Some(&mut on_checkpoint), overrides)
        }
        Storage::InMemory => {
            let mut pyramid_cursor = PyramidCursor::default();
            let mut builder = DatasetBuilder::default();
            let (outcome, columns_text) = open_path_capturing_all_columns_with_progress(
                path,
                overrides,
                |partial_outcome, partial_columns| match builder.snapshot(
                    partial_outcome,
                    partial_columns,
                    overrides,
                ) {
                    Ok((dataset, _ambiguous)) => {
                        if builder.take_layout_changed() {
                            // The time column was demoted: the axis and the
                            // column set the cursor extends are gone.
                            pyramid_cursor = PyramidCursor::default();
                        }
                        let pyramids = pyramid_cursor.update(&dataset);
                        on_checkpoint(Checkpoint {
                            rows_read: partial_outcome.row_count,
                            pyramids,
                            dataset,
                            spilled: false,
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
            let (dataset, timestamp_format_ambiguous) =
                builder.finish(&outcome, &columns_text, overrides)?;
            Ok((outcome, dataset, timestamp_format_ambiguous))
        }
    }
}

/// [`load`], additionally reporting progress like [`load_with_outcome_progressive`].
pub fn load_progressive(path: &Path, on_checkpoint: impl FnMut(Checkpoint)) -> Result<Dataset> {
    load_with_outcome_progressive(path, on_checkpoint)
        .map(|(_outcome, dataset, _ambiguous)| dataset)
}

/// [`load_progressive`] against an explicit budget and spill directory, so a
/// test can exercise progress reporting on a chosen storage path rather than
/// whichever one the host machine's RAM selects (issue #75) — the same split
/// [`load_with_budget`] provides for [`load`].
pub fn load_progressive_with_budget(
    path: &Path,
    budget: RamBudget,
    cache_dir: &Path,
    mut on_checkpoint: impl FnMut(Checkpoint),
) -> Result<Dataset> {
    let overrides = IngestOverrides::default();
    match choose_storage(path, budget, Some(cache_dir), overrides)? {
        Storage::Spill(sniff) => {
            load_spilled(path, &sniff, cache_dir, Some(&mut on_checkpoint), overrides)
                .map(|(_outcome, dataset, _ambiguous)| dataset)
        }
        Storage::InMemory => {
            let mut pyramid_cursor = PyramidCursor::default();
            let mut builder = DatasetBuilder::default();
            let (outcome, columns_text) = open_path_capturing_all_columns_with_progress(
                path,
                overrides,
                |partial, columns| {
                    if let Ok((dataset, _ambiguous)) = builder.snapshot(partial, columns, overrides)
                    {
                        if builder.take_layout_changed() {
                            pyramid_cursor = PyramidCursor::default();
                        }
                        let pyramids = pyramid_cursor.update(&dataset);
                        on_checkpoint(Checkpoint {
                            rows_read: partial.row_count,
                            pyramids,
                            dataset,
                            spilled: false,
                        });
                    }
                },
            )?;
            builder
                .finish(&outcome, &columns_text, overrides)
                .map(|(dataset, _ambiguous)| dataset)
        }
    }
}

/// Per-column pyramid state carried between successive progressive
/// checkpoints (docs/ROADMAP.md M3, issue #90), so a later, larger
/// checkpoint's pyramid is built by *extending* the previous checkpoint's
/// own pyramid ([`extend_pyramid`]) instead of re-aggregating every sample
/// read so far from scratch every time — `super::csv`'s row-count-doubling
/// checkpoint schedule made that `O(n log n)` of pure waste across a full
/// progressive load.
///
/// A non-`f64` numeric column also needs its `f64` conversion — normally
/// [`crate::series::SeriesValues::to_f64_vec`] — recomputed at every
/// checkpoint. This cursor grows that conversion incrementally instead
/// (only the newly arrived rows, via
/// [`crate::series::SeriesValues::f64_at_checked`], which — unlike
/// [`crate::series::SeriesValues::f64_at`] — still emits SPEC §1.4's
/// `i64`/`u64` precision-loss log, since each row is only ever converted
/// once here rather than every frame), so a checkpoint never re-walks rows
/// it already converted.
///
/// Reset (`Default::default()`) once per progressive load; never shared
/// across two different files or two different columns.
#[derive(Default)]
struct PyramidCursor {
    columns: Vec<Option<PyramidCursorColumn>>,
}

/// One column's incremental state: the pyramid built so far, the sample
/// count it covers, and — for a non-`f64` numeric column only — the
/// running `f64` conversion `extend_pyramid` is fed. `f64_cache` is always
/// `None` for an `f64` column, which is read straight off
/// [`crate::series::SeriesValues::as_f64_slice`] and never copied.
struct PyramidCursorColumn {
    pyramid: Vec<Vec<Bucket>>,
    len: usize,
    f64_cache: Option<Vec<f64>>,
}

impl PyramidCursor {
    /// Updates every column's pyramid for `dataset`'s current (larger) set
    /// of rows, in `dataset.columns` order — the same shape and, bucket for
    /// bucket, the same values [`pyramids_for_dataset(dataset)`] would
    /// produce (`tests::load_with_outcome_progressive_checkpoint_pyramids_match_build_pyramid_on_the_same_prefix`
    /// locks the equivalence), just without re-walking rows this cursor has
    /// already seen.
    fn update(&mut self, dataset: &Dataset) -> Vec<Option<Vec<Vec<Bucket>>>> {
        let ticks = dataset.time.to_pyramid_ticks();
        if self.columns.len() != dataset.columns.len() {
            self.columns.resize_with(dataset.columns.len(), || None);
        }

        // One column's pyramid never reads another's, so they extend in
        // parallel on the `rayon` compute pool (issue #114).
        use rayon::prelude::*;
        dataset
            .columns
            .par_iter()
            .zip(self.columns.par_iter_mut())
            .map(|(series, state)| Self::update_column(series.values(), &ticks, state))
            .collect()
    }

    fn update_column(
        values: &SeriesValues,
        ticks: &[i128],
        state: &mut Option<PyramidCursorColumn>,
    ) -> Option<Vec<Vec<Bucket>>> {
        if matches!(values.dtype(), Dtype::Bool | Dtype::String) {
            *state = None;
            return None;
        }

        if let Some(samples) = values.as_f64_slice() {
            let pyramid = match state.take().filter(|s| s.len <= samples.len()) {
                Some(PyramidCursorColumn { pyramid, len, .. }) => {
                    extend_pyramid(pyramid, len, samples, ticks)
                }
                None => build_pyramid(samples, ticks),
            };
            *state = Some(PyramidCursorColumn {
                pyramid: pyramid.clone(),
                len: samples.len(),
                f64_cache: None,
            });
            return Some(pyramid);
        }

        // Non-`f64` numeric column: grow the cached conversion by exactly
        // the rows added since the last checkpoint, never re-converting
        // the whole column.
        let total = values.len();
        let previous = state
            .take()
            .filter(|s| s.f64_cache.as_ref().is_some_and(|c| c.len() <= total));
        let mut cache = previous
            .as_ref()
            .and_then(|s| s.f64_cache.clone())
            .unwrap_or_default();
        for i in cache.len()..total {
            cache.push(
                values
                    .f64_at_checked(i)
                    .expect("dtype checked non-bool/string above"),
            );
        }

        let pyramid = match previous {
            Some(PyramidCursorColumn { pyramid, len, .. }) => {
                extend_pyramid(pyramid, len, &cache, ticks)
            }
            None => build_pyramid(&cache, ticks),
        };
        *state = Some(PyramidCursorColumn {
            pyramid: pyramid.clone(),
            len: cache.len(),
            f64_cache: Some(cache),
        });
        Some(pyramid)
    }
}

/// `dataset`'s own min/max pyramid, one entry per column in `dataset.columns`
/// order (see [`Checkpoint::pyramids`]). `None` at an index whose column is
/// non-numeric; `Some` for every numeric one, built by the same golden-tested
/// [`build_pyramid`] the pyramid used internally by [`load_with_outcome_progressive`]
/// checkpoints uses. Public so `glyde-app` can build a final pyramid for a
/// completed dataset once progressive checkpointing has stopped
/// (docs/ROADMAP.md M3, issue #80).
///
/// A [`Dataset::is_spilled`] dataset is read through
/// [`build_pyramid_streaming`] instead (issue #88): its columns are
/// memory-mapped files, and the whole-column slices the in-memory path hands
/// [`build_pyramid`] would make every page of them resident — memory
/// proportional to file size, against SPEC §5's flat cap. The two paths
/// produce identical pyramids (locked by `tests/golden/decimation.rs` for the
/// builders and `tests/spilled_pyramid_integration.rs` for this dispatch);
/// only the residency differs. A spilled column whose read fails yields
/// `None` for that column — a missing pyramid degrades rendering to an
/// un-pyramided viewport scan, it never fails the open
/// (docs/ARCHITECTURE.md §Error philosophy).
pub fn pyramids_for_dataset(dataset: &Dataset) -> Vec<Option<Vec<Vec<Bucket>>>> {
    if dataset.is_spilled() {
        return dataset
            .columns
            .iter()
            .enumerate()
            .map(|(index, series)| streaming_pyramid_for_column(dataset, index, series))
            .collect();
    }

    let ticks = dataset.time.to_pyramid_ticks();
    use rayon::prelude::*;
    dataset
        .columns
        .par_iter()
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

/// One spilled column's pyramid, built by streaming both its samples and the
/// time axis's ticks (issue #88). `None` for a non-numeric column, and for a
/// column whose spill file could not be read — logged, never escalated.
fn streaming_pyramid_for_column(
    dataset: &Dataset,
    index: usize,
    series: &Series,
) -> Option<Vec<Vec<Bucket>>> {
    let samples = series.values().sample_source()?;
    match build_pyramid_streaming(&samples, &dataset.time) {
        Ok(pyramid) => Some(pyramid),
        Err(err) => {
            warn!(
                column = index,
                name = series.name(),
                error = %err,
                "could not read this spilled column back to build its pyramid; the view will \
                 fall back to an un-pyramided viewport scan (issue #88)"
            );
            None
        }
    }
}

/// [`pyramids_for_dataset`], but reusing a previous completed open's cached
/// pyramid for each numeric column when `path` names an unchanged file
/// (path + size + mtime match what was cached) *and* `overrides` matches
/// what produced the cached entry, and writing the cache after building on a
/// miss — so a second, completed open of the same file skips the pyramid
/// aggregation entirely instead of redoing it (issue #81; docs/ROADMAP.md M3
/// "reopening rebuilds the pyramid from cached Level 0 rather than loading it
/// too"). Each column gets its own cache entry, keyed by
/// [`CacheKey::with_column`] and [`CacheKey::with_overrides_signature`], so a
/// multi-column file's columns never overwrite each other's cache, and a
/// one-click correction (docs/ROADMAP.md M4) of the same, byte-for-byte
/// unchanged file never collides with the pre-correction cache entry either.
///
/// A cache miss or a cache read/write failure both fall back to an ordinary
/// uncached [`build_pyramid`] rather than failing the open — a cache is an
/// optimization, never a requirement to open a file
/// (docs/ARCHITECTURE.md §The index).
///
/// Resolves the OS-standard cache directory itself; see
/// [`pyramids_for_dataset_cached_with_cache_dir`] for a version that takes
/// an explicit one (tests; a system with no OS cache directory falls back
/// to the same uncached path).
///
/// A [`Dataset::is_spilled`] dataset takes the bounded-memory build on a
/// cache miss, exactly as [`pyramids_for_dataset`] does (issue #88); the
/// cached result is the same either way, since both builders produce the same
/// pyramid.
///
/// **This or [`derived_caches_for_dataset_cached`]?** That one is what
/// `glyde-app` calls for a completed, non-spilled open: it produces this
/// pyramid *and* the Level-0 raw-sample cache, converting each column at most
/// once between them. This one is the pyramid alone — the right choice when
/// there is no Level-0 cache to build, which today means the spilled path
/// (where a Level-0 cache would be a whole second copy of the column, issue
/// #102) and the tests that exercise pyramid caching in isolation.
pub fn pyramids_for_dataset_cached(
    path: &Path,
    dataset: &Dataset,
    overrides: IngestOverrides,
) -> Vec<Option<Vec<Vec<Bucket>>>> {
    match os_spill_dir() {
        Some(cache_dir) => {
            pyramids_for_dataset_cached_with_cache_dir(path, dataset, &cache_dir, overrides)
        }
        None => pyramids_for_dataset(dataset),
    }
}

/// [`pyramids_for_dataset_cached`] against an explicit cache directory, for
/// tests that need a temp directory rather than the real OS cache dir — the
/// same split [`load_with_budget`] provides for [`load`].
pub fn pyramids_for_dataset_cached_with_cache_dir(
    path: &Path,
    dataset: &Dataset,
    cache_dir: &Path,
    overrides: IngestOverrides,
) -> Vec<Option<Vec<Vec<Bucket>>>> {
    let key = match CacheKey::for_path(path) {
        Ok(key) => key.with_overrides_signature(super::overrides_signature(overrides)),
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "could not read this file's metadata to key its pyramid cache; pyramid will be \
                 rebuilt and not cached (issue #81)"
            );
            return pyramids_for_dataset(dataset);
        }
    };

    // Only the in-memory path wants the ticks as one slice; materializing them
    // for a spilled dataset is exactly the residency this avoids (issue #88).
    let spilled = dataset.is_spilled();
    let ticks = if spilled {
        Cow::Borrowed(&[] as &[i128])
    } else {
        dataset.time.to_pyramid_ticks()
    };
    dataset
        .columns
        .iter()
        .enumerate()
        .map(|(index, series)| {
            let column_key = key.with_column(index);

            // A spilled column never gets materialized to be cached: both the
            // build and the fallback read it in bounded chunks (issue #88).
            if spilled {
                let samples = series.values().sample_source()?;
                return match pyramid::build_or_open_streaming(
                    cache_dir,
                    &column_key,
                    &samples,
                    &dataset.time,
                ) {
                    Ok(levels) => Some(levels),
                    Err(err) => {
                        warn!(
                            path = %path.display(),
                            column = index,
                            error = %err,
                            "pyramid cache read/write failed for this spilled column; falling \
                             back to an uncached streaming build (issue #81)"
                        );
                        streaming_pyramid_for_column(dataset, index, series)
                    }
                };
            }

            let samples: Cow<'_, [f64]> = match series.values().as_f64_slice() {
                // Already `f64`: hand the pyramid cache the samples
                // themselves rather than a freshly allocated copy of them.
                Some(samples) => Cow::Borrowed(samples),
                None => Cow::Owned(series.values().to_f64_vec()?),
            };
            match pyramid::build_or_open(cache_dir, &column_key, &samples, &ticks) {
                Ok(levels) => Some(levels),
                Err(err) => {
                    warn!(
                        path = %path.display(),
                        column = index,
                        error = %err,
                        "pyramid cache read/write failed for this column; falling back to an \
                         uncached build (issue #81)"
                    );
                    Some(build_pyramid(&samples, &ticks))
                }
            }
        })
        .collect()
}

/// One entry per column in `Dataset::columns` order: that column's min/max
/// pyramid, or `None` when it is non-numeric (see [`Checkpoint::pyramids`]).
pub type ColumnPyramids = Vec<Option<Vec<Vec<Bucket>>>>;

/// One entry per column in `Dataset::columns` order: that column's raw
/// `(timestamp, value)` pairs as a memory-mapped [`Level0Cache`], or `None`
/// when it is non-numeric or its cache could not be read or written.
pub type ColumnLevel0Caches = Vec<Option<Level0Cache>>;

/// Both derived caches of one completed open, in `Dataset::columns` order —
/// what [`derived_caches_for_dataset_cached`] returns.
pub type DerivedCaches = (ColumnPyramids, ColumnLevel0Caches);

/// Both of a completed, non-spilled open's derived caches — every column's
/// min/max pyramid and its raw `(timestamp, value)` pairs — served from (and
/// written to) the on-disk caches (issue #92, split from #81: "Level-0 typed
/// spill cache … reopen is instant"). `None` at an index whose column is
/// non-numeric, or whose cache could not be read or written: a cache is an
/// optimization, never a requirement to open a file (docs/ARCHITECTURE.md
/// §The index), and `glyde-app` falls back to the in-memory dataset for
/// either case, so a miss here never blocks rendering.
///
/// **The two are built together, in this order, on purpose.** Both need the
/// column as `&[f64]`, which for any non-`f64` dtype means a
/// [`crate::series::SeriesValues::to_f64_vec`] conversion of the whole column
/// — per element, with SPEC §1.4's precision-loss check on every `i64`/`u64`
/// value. Building them through two independent passes would pay that twice
/// per open, and pay it again on every *reopen* despite both caches hitting.
/// So, per column:
///
/// 1. Level 0 is resolved first, and only *converts* if its cache misses.
/// 2. The pyramid is then built from the Level-0 cache's own memory-mapped
///    `samples()`/`timestamps()` — which are exactly the converted values,
///    already on disk — rather than from a second conversion. This is what
///    docs/ARCHITECTURE.md §"Where Level 0 actually lives" describes: "the
///    large-file path memory-maps the Level-0 cache and hands these functions
///    a real slice over the mapped bytes".
///
/// The result: a reopen of an unchanged file converts nothing at all, and a
/// first open converts each column exactly once, never twice.
///
/// Each column gets its own cache entry, keyed by [`CacheKey::with_column`]
/// and [`CacheKey::with_overrides_signature`], so a multi-column file's
/// columns never overwrite each other's cache and a one-click correction
/// (docs/ROADMAP.md M4) of the same, byte-for-byte unchanged file never
/// collides with the pre-correction entry.
///
/// Resolves the OS-standard cache directory itself; see
/// [`derived_caches_for_dataset_cached_with_cache_dir`] for a version taking
/// an explicit one (tests; a system with no OS cache directory falls back to
/// an uncached pyramid build and `None` for every Level-0 entry).
///
/// Callers must not call this over a [`Dataset::is_spilled`] dataset. Reading
/// one is bounded as of issue #88, but both caches would still *produce*
/// something proportional to file size — the pyramid ~9 bytes per sample per
/// column in RAM, the Level-0 cache a whole second copy of the column on disk
/// and mapped — against SPEC §5's flat cap (issue #102).
pub fn derived_caches_for_dataset_cached(
    path: &Path,
    dataset: &Dataset,
    overrides: IngestOverrides,
) -> DerivedCaches {
    match os_spill_dir() {
        Some(cache_dir) => {
            derived_caches_for_dataset_cached_with_cache_dir(path, dataset, &cache_dir, overrides)
        }
        None => (
            pyramids_for_dataset(dataset),
            (0..dataset.columns.len()).map(|_| None).collect(),
        ),
    }
}

/// [`derived_caches_for_dataset_cached`] against an explicit cache directory,
/// for tests that need a temp directory rather than the real OS cache dir —
/// the same split [`load_with_budget`] provides for [`load`].
pub fn derived_caches_for_dataset_cached_with_cache_dir(
    path: &Path,
    dataset: &Dataset,
    cache_dir: &Path,
    overrides: IngestOverrides,
) -> DerivedCaches {
    let key = match CacheKey::for_path(path) {
        Ok(key) => key.with_overrides_signature(super::overrides_signature(overrides)),
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "could not read this file's metadata to key its derived caches; the pyramid will \
                 be rebuilt and not cached, and the raw-sample view will read from the in-memory \
                 dataset (issues #81, #92)"
            );
            return (
                pyramids_for_dataset(dataset),
                (0..dataset.columns.len()).map(|_| None).collect(),
            );
        }
    };

    let ticks = dataset.time.to_pyramid_ticks();
    // Columns are independent — each has its own cache files — so they are
    // built on the `rayon` compute pool (issue #114). The result keeps
    // `dataset.columns` order.
    use rayon::prelude::*;
    let per_column: Vec<_> = dataset
        .columns
        .par_iter()
        .enumerate()
        .map(|(index, series)| {
            derived_caches_for_column(
                path,
                cache_dir,
                &key.with_column(index),
                index,
                series,
                &ticks,
            )
        })
        .collect();
    per_column.into_iter().unzip()
}

/// One column's pyramid and Level-0 cache, converting the column to `f64` at
/// most once — see [`derived_caches_for_dataset_cached`] for why the order
/// matters.
fn derived_caches_for_column(
    path: &Path,
    cache_dir: &Path,
    column_key: &CacheKey,
    index: usize,
    series: &Series,
    ticks: &[i128],
) -> (Option<Vec<Vec<Bucket>>>, Option<Level0Cache>) {
    // Step 1: Level 0, without converting anything if it is already cached.
    let level0 = match level0::try_open(cache_dir, column_key) {
        Ok(Some(cache)) => Some(cache),
        Ok(None) => None,
        Err(err) => {
            warn!(
                path = %path.display(),
                column = index,
                error = %err,
                "level 0 cache could not be read for this column; rebuilding it (issue #92)"
            );
            None
        }
    };

    // A cache hit means the converted samples are already on disk and mapped:
    // the pyramid is built from those, so nothing is converted twice, and a
    // reopen converts nothing at all.
    if let Some(level0) = level0 {
        let pyramid = build_or_open_pyramid(
            path,
            cache_dir,
            column_key,
            index,
            level0.samples(),
            level0.timestamps(),
        );
        return (Some(pyramid), Some(level0));
    }

    // Step 2: a miss. Convert once (or borrow, when the column is already
    // `f64`), write Level 0, then build the pyramid from what Level 0 mapped
    // back rather than from the conversion again.
    let samples: Cow<'_, [f64]> = match series.values().as_f64_slice() {
        Some(samples) => Cow::Borrowed(samples),
        None => match series.values().to_f64_vec() {
            Some(samples) => Cow::Owned(samples),
            // Non-numeric: no pyramid and no Level 0, exactly as before.
            None => return (None, None),
        },
    };

    match level0::build(cache_dir, column_key, &samples, ticks) {
        Ok(level0) => {
            let pyramid = build_or_open_pyramid(
                path,
                cache_dir,
                column_key,
                index,
                level0.samples(),
                level0.timestamps(),
            );
            (Some(pyramid), Some(level0))
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                column = index,
                error = %err,
                "level 0 cache could not be written for this column; the raw-sample view will \
                 read from the in-memory dataset instead (issue #92)"
            );
            let pyramid =
                build_or_open_pyramid(path, cache_dir, column_key, index, &samples, ticks);
            (Some(pyramid), None)
        }
    }
}

/// [`pyramid::build_or_open`] with this module's "a cache failure is a warn,
/// not an error" handling — shared by every cached pyramid path here so the
/// fallback is written once.
fn build_or_open_pyramid(
    path: &Path,
    cache_dir: &Path,
    column_key: &CacheKey,
    index: usize,
    samples: &[f64],
    ticks: &[i128],
) -> Vec<Vec<Bucket>> {
    match pyramid::build_or_open(cache_dir, column_key, samples, ticks) {
        Ok(levels) => levels,
        Err(err) => {
            warn!(
                path = %path.display(),
                column = index,
                error = %err,
                "pyramid cache read/write failed for this column; falling back to an uncached \
                 build (issue #81)"
            );
            build_pyramid(samples, ticks)
        }
    }
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

    fn verdict_of(fields: &[&str], check_monotonic: bool) -> Option<GeneratedIndexReason> {
        let mut scan = TimeCandidateScan::default();
        for field in fields {
            scan.observe(field);
        }
        let matches_format = crate::time::infer_timestamp_format(fields).is_some();
        scan.verdict(matches_format, check_monotonic, "c")
    }

    // SPEC §2.1 "Files without a time column": the column-level verdict
    // itself, isolated from any file.
    #[test]
    fn a_non_decreasing_numeric_column_is_a_time_index() {
        assert_eq!(verdict_of(&["0", "10", " 20 ", "20", "30.5"], true), None);
    }

    #[test]
    fn a_numeric_column_that_drops_is_a_signal_and_names_the_first_drop() {
        assert_eq!(
            verdict_of(&["0", "1", "2", "1.5", "0"], true),
            Some(GeneratedIndexReason::NotMonotonic {
                column: "c".to_string(),
                row: 3,
            })
        );
    }

    #[test]
    fn a_nan_in_a_numeric_candidate_counts_as_a_drop() {
        assert_eq!(
            verdict_of(&["0", "NaN", "2"], true),
            Some(GeneratedIndexReason::NotMonotonic {
                column: "c".to_string(),
                row: 1,
            })
        );
    }

    #[test]
    fn a_constant_numeric_column_is_a_signal_but_a_single_row_is_not_judged() {
        assert_eq!(
            verdict_of(&["4", "4", "4"], true),
            Some(GeneratedIndexReason::Constant {
                column: "c".to_string(),
            })
        );
        assert_eq!(verdict_of(&["4"], true), None);
    }

    #[test]
    fn a_column_that_changes_on_few_rows_is_a_signal() {
        assert_eq!(
            verdict_of(&["1", "1", "1", "2", "2", "2"], true),
            Some(GeneratedIndexReason::MostlyRepeated {
                column: "c".to_string(),
                changes: 1,
            })
        );
        // Exactly half of the steps advancing is enough.
        assert_eq!(verdict_of(&["0", "1", "1", "2", "2"], true), None);
    }

    #[test]
    fn a_picked_numeric_column_is_never_overruled_for_running_backwards() {
        assert_eq!(verdict_of(&["3", "1", "2"], false), None);
        assert_eq!(verdict_of(&["3", "3"], false), None);
    }

    #[test]
    fn out_of_order_text_timestamps_are_still_a_time_index() {
        let fields = [
            "2026-01-01T00:00:02Z",
            "2026-01-01T00:00:01Z",
            "2026-01-01T00:00:03Z",
        ];
        assert_eq!(verdict_of(&fields, true), None);
    }

    #[test]
    fn a_column_of_neither_timestamps_nor_numbers_is_unreadable() {
        let expected = Some(GeneratedIndexReason::Unreadable {
            column: "c".to_string(),
        });
        assert_eq!(verdict_of(&["0", "10", "N/A", "30"], true), expected);
        // …even when the user picked it: there is nothing to index by.
        assert_eq!(verdict_of(&["idle", "run"], false), expected);
    }

    #[test]
    fn the_row_ordinal_axis_is_one_value_per_row_in_source_order() {
        match row_ordinal_axis(4) {
            TimeAxis::Progressive { values } => {
                assert_eq!(values.as_slice(), &[0.0, 1.0, 2.0, 3.0]);
            }
            TimeAxis::Absolute { .. } => panic!("the fallback index has no absolute meaning"),
        }
        // A zero-row file still produces an axis rather than panicking on an
        // empty range (SPEC §1.3: malformed input never panics).
        assert_eq!(row_ordinal_axis(0).len(), 0);
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

    /// [`many_rows_temp_csv`], but `value` is written as a plain integer
    /// (no decimal point) so it infers as `i64`, not `f64` — the only way to
    /// drive `PyramidCursor`'s non-`f64` incremental `f64` cache branch
    /// (issue #90), which `many_rows_temp_csv`'s always-`f64` column never
    /// reaches (that column takes the zero-copy `as_f64_slice` branch
    /// instead).
    fn many_rows_temp_csv_with_i64_column(row_count: u64) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        let mut text = String::from("index,value\n");
        for i in 0..row_count {
            text.push_str(&format!("{i},{}\n", i as i64 * 3 - 1_000_000));
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

    /// Issue #114: checkpoints are typed incrementally (`DatasetBuilder`),
    /// so a decision that changes between checkpoints — a time column that
    /// stops matching its timestamp format, a column widening from integer
    /// to float, or from bool to integer to string — must still produce, at
    /// every checkpoint, exactly the dataset a from-scratch load of that same
    /// prefix produces, and a final dataset equal to a non-progressive load.
    #[test]
    fn incremental_checkpoints_match_a_from_scratch_load_of_the_same_prefix() {
        let row_count = 90_000usize;
        let header = "time,a,b\n";
        let rows: Vec<String> = (0..row_count)
            .map(|i| {
                // EpochSeconds for the first 25k rows; an all-zero fraction
                // afterwards matches no absolute format, so the column turns
                // into a progressive index between the 20k and 40k checkpoints.
                let time = if i < 25_000 {
                    format!("{}", 1_700_000_000 + i)
                } else {
                    format!("{}.0", 1_700_000_000 + i)
                };
                let a = if i < 30_000 {
                    format!("{}", i as i64 - 500)
                } else {
                    format!("{}.25", i)
                };
                let b = if i < 35_000 {
                    format!("{}", i % 2)
                } else if i < 60_000 {
                    format!("{}", i * 3)
                } else if i == 60_000 {
                    "x".to_string()
                } else {
                    format!("{i}")
                };
                format!("{time},{a},{b}\n")
            })
            .collect();
        let write = |count: usize| {
            let mut file = tempfile::NamedTempFile::new().expect("create temp file");
            let mut text = String::from(header);
            for row in &rows[..count] {
                text.push_str(row);
            }
            std::io::Write::write_all(&mut file, text.as_bytes()).expect("write temp file");
            file
        };

        let full = write(row_count);
        let mut checkpoints: Vec<Checkpoint> = Vec::new();
        let (_outcome, final_dataset, final_inference) =
            load_with_outcome_progressive(full.path(), |checkpoint| checkpoints.push(checkpoint))
                .expect("progressive load must succeed");
        let (_outcome, expected, expected_inference) =
            load_with_outcome(full.path()).expect("non-progressive load must succeed");
        assert_eq!(final_dataset, expected);
        assert_eq!(final_inference, expected_inference);

        let rows_read: Vec<u64> = checkpoints.iter().map(|c| c.rows_read).collect();
        assert_eq!(rows_read, vec![20_000, 40_000, 80_000]);
        let dtypes = |dataset: &Dataset| -> Vec<Dtype> {
            dataset.columns.iter().map(Series::dtype).collect()
        };
        assert_eq!(
            dtypes(&checkpoints[0].dataset),
            vec![Dtype::I64, Dtype::Bool]
        );
        assert_eq!(
            dtypes(&checkpoints[1].dataset),
            vec![Dtype::F64, Dtype::I64]
        );
        assert_eq!(
            dtypes(&checkpoints[2].dataset),
            vec![Dtype::F64, Dtype::String]
        );
        assert!(matches!(
            checkpoints[0].dataset.time,
            TimeAxis::Absolute { .. }
        ));
        assert!(matches!(
            checkpoints[1].dataset.time,
            TimeAxis::Progressive { .. }
        ));

        for checkpoint in &checkpoints {
            let prefix = write(checkpoint.rows_read as usize);
            let (_outcome, from_scratch, _inference) =
                load_with_outcome(prefix.path()).expect("prefix load must succeed");
            assert_eq!(
                checkpoint.dataset, from_scratch,
                "checkpoint at {} rows must equal a from-scratch load of that prefix",
                checkpoint.rows_read
            );
        }
    }

    /// Issue #114: the spilled path types and writes rows in parallel
    /// batches, but its progressive preview must still checkpoint at the
    /// same row counts, over the same rows, as the in-memory path — across
    /// batch boundaries (`SCAN_BATCH_ROWS`) and up to the preview cap
    /// (`PREVIEW_MAX_ROWS`) — and the finished spilled dataset must equal
    /// the in-memory one.
    #[test]
    fn batched_spilled_checkpoints_match_the_in_memory_checkpoints() {
        let row_count = 230_000u64;
        let mut file = tempfile::NamedTempFile::new().expect("create temp file");
        let mut text = String::from("time,a,b,flag\n");
        for i in 0..row_count {
            text.push_str(&format!(
                "2024-01-01T00:00:00.{:06}Z,{},{}.5,{}\n",
                i % 1_000_000,
                i as i64 - 7,
                i * 3,
                i % 2
            ));
        }
        std::io::Write::write_all(&mut file, text.as_bytes()).expect("write temp file");
        let cache_dir = tempfile::tempdir().expect("cache dir");

        let collect = |budget: RamBudget| {
            let mut checkpoints: Vec<(u64, Dataset)> = Vec::new();
            let dataset =
                load_progressive_with_budget(file.path(), budget, cache_dir.path(), |c| {
                    checkpoints.push((c.rows_read, c.dataset));
                })
                .expect("load must succeed");
            (dataset, checkpoints)
        };
        let (in_memory, memory_checkpoints) =
            collect(RamBudget::from_total_ram_bytes(64 * 1024 * 1024 * 1024));
        let (spilled, spilled_checkpoints) = collect(RamBudget::from_total_ram_bytes(1));
        assert!(!in_memory.is_spilled());
        assert!(spilled.is_spilled());
        assert_eq!(spilled, in_memory);

        let spilled_rows: Vec<u64> = spilled_checkpoints.iter().map(|(rows, _)| *rows).collect();
        assert_eq!(spilled_rows, vec![20_000, 40_000, 80_000, 160_000]);
        for (rows, spilled_checkpoint) in &spilled_checkpoints {
            let (_, memory_checkpoint) = memory_checkpoints
                .iter()
                .find(|(memory_rows, _)| memory_rows == rows)
                .expect("the in-memory load checkpoints at the same row counts");
            assert_eq!(
                spilled_checkpoint, memory_checkpoint,
                "checkpoint at {rows} rows"
            );
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

    // `PyramidCursor::update_column` has a separate code path for a
    // non-`f64` numeric column — an incrementally-grown `f64_cache` — that
    // the `f64`-column test above never exercises (an `f64` column takes
    // the zero-copy `as_f64_slice` branch instead). Same property, same
    // fixture shape, but through `many_rows_temp_csv_with_i64_column` so the
    // cache branch is what actually runs.
    #[test]
    fn load_with_outcome_progressive_checkpoint_pyramids_match_build_pyramid_for_a_non_f64_column()
    {
        let file = many_rows_temp_csv_with_i64_column(70_000);
        let mut checkpoints: Vec<Checkpoint> = Vec::new();

        load_with_outcome_progressive(file.path(), |checkpoint| checkpoints.push(checkpoint))
            .expect("progressive load must succeed");

        assert!(!checkpoints.is_empty());
        for checkpoint in &checkpoints {
            assert_eq!(checkpoint.dataset.columns.len(), 1);
            let column = &checkpoint.dataset.columns[0];
            assert!(
                matches!(column.values(), SeriesValues::I64(_)),
                "fixture's value column must infer as i64, not f64, to actually drive the \
                 non-f64 incremental cache branch"
            );

            let ticks = checkpoint.dataset.time.to_pyramid_ticks();
            let samples = column.values().to_f64_vec().expect("numeric column");
            let expected = crate::dsp::decimation::build_pyramid(&samples, &ticks);
            assert_eq!(checkpoint.pyramids[0].as_ref(), Some(&expected));
        }
    }

    // Same property as above, over the spilled `SpillPreview` checkpoint
    // path (a zero RAM budget forces every file to spill regardless of
    // size — `spilled_ingest_integration.rs`'s established pattern), which
    // has its own, separate `PyramidCursor` instance.
    #[test]
    fn a_spilled_progressive_load_checkpoint_pyramids_match_build_pyramid_for_a_non_f64_column() {
        let file = many_rows_temp_csv_with_i64_column(70_000);
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let mut checkpoints: Vec<Checkpoint> = Vec::new();

        load_progressive_with_budget(
            file.path(),
            RamBudget::from_total_ram_bytes(0),
            cache_dir.path(),
            |checkpoint| checkpoints.push(checkpoint),
        )
        .expect("spilled progressive load must succeed");

        assert!(
            !checkpoints.is_empty(),
            "a 70k-row fixture under a zero RAM budget must still checkpoint via the \
             spilled preview path"
        );
        for checkpoint in &checkpoints {
            assert_eq!(checkpoint.dataset.columns.len(), 1);
            let column = &checkpoint.dataset.columns[0];
            assert!(
                matches!(column.values(), SeriesValues::I64(_)),
                "fixture's value column must infer as i64 on the spilled preview path too"
            );

            let ticks = checkpoint.dataset.time.to_pyramid_ticks();
            let samples = column.values().to_f64_vec().expect("numeric column");
            let expected = crate::dsp::decimation::build_pyramid(&samples, &ticks);
            assert_eq!(checkpoint.pyramids[0].as_ref(), Some(&expected));
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
