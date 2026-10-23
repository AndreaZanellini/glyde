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

//! The spilled Parquet open (issue #155): a file whose typed columns do not
//! fit the RAM budget is read into the on-disk spill cache (SPEC §5.1's
//! affordable alternative, as for CSV, issue #75) instead of being refused.
//!
//! It is the in-memory open's twin, one storage over: the same footers, the
//! same record batches ([`read_column_batches`]), each column written in its
//! own dtype ([`SpilledValues`], never upcast) as its batches arrive, so no
//! column is ever held in memory. What the in-memory open decides from a
//! whole column is decided here from its spill, by the same code where the
//! decision is not storage-bound ([`time_candidate`], [`absolute_axis`],
//! [`time_axis_from_text`]) and by its exact counterpart where it is:
//!
//! - **Nulls** become the same gaps ([`SourceColumn::settle_missing`]): NaN
//!   and an empty label are written in place as the batches arrive; a
//!   boolean or integer column with nulls, whose own dtype has no gap, is
//!   rewritten once at the end — to text, or to `f64` with NaN gaps when
//!   every value converts exactly, else not loaded with the same reason.
//! - **NaN runs** (SPEC §1.3) are flagged as the samples stream past.
//! - **The `[Sort]`** (SPEC §2.1) is the on-disk sort of a spilled CSV.

use std::cell::Cell;
use std::ops::Range;

use super::super::dataset::{spilled_row_ordinal_axis, TimestampsSpillWriter};
use super::*;
use crate::index::spill::{SpillSet, SpillStringsWriter, SpillVec, SpillVecWriter};
use crate::series::{Anomalies, NanRunScan, Series, SpilledValues};

/// What a spilled open writes per row besides its columns: the absolute
/// axis's tick (16 bytes), unit (1) and UTC offset (8).
const AXIS_BYTES_PER_ROW: u64 = 16 + 1 + 8;

/// What the footers decided before a page is read, handed over from
/// [`super::open_checking_disk`].
pub(super) struct Footers<'a> {
    pub(super) sources: &'a [SourceFile],
    pub(super) schema: &'a Schema,
    pub(super) partition_keys: Vec<String>,
    pub(super) columns: Vec<SourceColumn>,
    pub(super) file_columns: usize,
    pub(super) loaded: Vec<usize>,
    pub(super) declared_rows: u64,
    pub(super) estimated_bytes: u64,
}

/// Opens the sources the footers describe through the spill cache under
/// `cache_dir` — after checking the spill fits the free disk the query
/// reports, before a byte is written (issue #118).
pub(super) fn open_spilled(
    footers: Footers<'_>,
    request: OpenRequest<'_>,
    cache_dir: &Path,
    available_disk_bytes: impl FnOnce(&Path) -> Option<u64>,
) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    let Footers {
        sources,
        schema,
        partition_keys,
        columns,
        file_columns,
        loaded,
        declared_rows,
        estimated_bytes,
    } = footers;

    // The disk is a budget too: what earlier opens left behind is given
    // back first, then the spill must fit with headroom — twice over for the
    // sort, which holds two copies at its peak (as for CSV).
    crate::index::spill::wait_for_pending_deletions();
    crate::index::spill::sweep_orphaned_spill_files(cache_dir);
    let spill_bytes =
        estimated_bytes.saturating_add(declared_rows.saturating_mul(AXIS_BYTES_PER_ROW));
    let spill_bytes = if request.overrides.sort_by_time {
        spill_bytes.saturating_mul(2)
    } else {
        spill_bytes
    };
    match available_disk_bytes(cache_dir) {
        Some(available_bytes) => {
            crate::budget::check_disk_affordable(spill_bytes, available_bytes, cache_dir)?
        }
        None => warn!(
            cache_dir = %cache_dir.display(),
            spill_bytes,
            "could not determine the free space on the cache disk; spilling without checking it \
             (a full disk fails the open and its partial spill files are deleted)"
        ),
    }
    info!(
        estimated_bytes,
        cap_bytes = request.budget.cap_bytes(),
        spill_bytes,
        cache_dir = %cache_dir.display(),
        "opening the Parquet file through the on-disk spill cache: its typed columns do not fit \
         the RAM budget (SPEC §5.1)"
    );

    let set = SpillSet::create(cache_dir)?;
    let mut spills = columns
        .iter()
        .enumerate()
        .map(|(index, column)| match &column.state {
            ColumnState::Loading(values) => ColumnSpill::create(&set, index, values).map(Some),
            _ => Ok(None),
        })
        .collect::<Result<Vec<_>>>()?;

    // As in memory: each column decoded by its own reader on the `rayon`
    // pool, every column reading the sources in order.
    let mut row_count = 0u64;
    for source in sources {
        request.ensure_not_cancelled()?;
        let cancelled = request.cancelled;
        let mut targets: Vec<(usize, &str, &mut ColumnSpill)> = spills[..file_columns]
            .iter_mut()
            .enumerate()
            .filter(|(index, _)| loaded.contains(index))
            .filter_map(|(index, spill)| {
                spill
                    .as_mut()
                    .map(|spill| (index, columns[index].name.as_str(), spill))
            })
            .collect();
        use rayon::prelude::*;
        targets
            .par_iter_mut()
            .try_for_each(|(index, name, spill)| {
                read_column_batches(source, *index, name, cancelled, |array| spill.append(array))
            })?;
        let rows = usize::try_from(source.declared_rows()).unwrap_or(usize::MAX);
        for (spill, (_, value)) in spills[file_columns..].iter_mut().zip(&source.partition) {
            if let Some(spill) = spill {
                spill.append_label(value, rows)?;
            }
        }
        row_count += source.declared_rows();
    }
    request.ensure_not_cancelled()?;

    let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
    let mut states = columns
        .into_iter()
        .zip(spills)
        .map(|(column, spill)| match (spill, column.state) {
            (Some(spill), _) => spill.settle(&column.name),
            (None, ColumnState::Unloaded(reason)) => Ok(Spilled::Unloaded(reason)),
            (None, _) => Ok(Spilled::Taken),
        })
        .collect::<Result<Vec<_>>>()?;
    // A file column still loaded keeps its row groups' statistics, as in
    // memory (issue #162; the on-disk pyramid does not use them).
    let mut stats: Vec<Option<RowGroupStats>> = states
        .iter()
        .enumerate()
        .map(|(index, state)| match (state, schema.fields().get(index)) {
            (Spilled::Values { .. } | Spilled::Timestamp { .. }, Some(field)) => {
                row_group_stats(sources, field)
            }
            _ => None,
        })
        .collect();

    // The time index, as `build_dataset` chooses it.
    let overrides = request.overrides;
    let timestamps: Vec<bool> = states
        .iter()
        .map(|state| matches!(state, Spilled::Timestamp { .. }))
        .collect();
    let (candidate, check_monotonic) = time_candidate(&timestamps, overrides);
    let axis = OnDisk {
        set: &set,
        files: Cell::new(0),
    };
    let mut time = None;
    let mut timestamp_format_ambiguous = false;
    let mut generated = match candidate {
        None => Some(GeneratedIndexReason::Requested),
        Some(_) => None,
    };
    if let Some(index) = candidate {
        let name = names[index].clone();
        match std::mem::replace(&mut states[index], Spilled::Taken) {
            Spilled::Timestamp {
                first_missing: Some(row),
                ..
            } => {
                states[index] = Spilled::Unloaded(format!(
                    "it has no timestamp on row {row}, so it cannot place rows in time"
                ));
                generated = Some(GeneratedIndexReason::MissingTimestamps {
                    column: name,
                    row: row as u64,
                });
            }
            Spilled::Timestamp {
                unit, zone, ticks, ..
            } => {
                if overrides.timestamp_format.is_some() {
                    info!(
                        time_column = %name,
                        "the timestamp format correction does not apply to a typed Parquet \
                         timestamp column, whose type settles its format; ignored"
                    );
                }
                match absolute_axis(unit, &zone, &ticks, &axis)? {
                    Ok(built) => time = Some(built),
                    Err(reason) => {
                        states[index] = Spilled::Unloaded(reason);
                        generated = Some(GeneratedIndexReason::Unreadable { column: name });
                    }
                }
            }
            Spilled::Values { values, nan_runs }
                if values.is_numeric() || matches!(values, SpilledValues::String(_)) =>
            {
                match time_axis_from_text(
                    &values,
                    &axis,
                    &name,
                    overrides.timestamp_format,
                    check_monotonic,
                )? {
                    Ok((built, ambiguous)) => {
                        time = Some(built);
                        timestamp_format_ambiguous = ambiguous;
                    }
                    Err(reason) => {
                        states[index] = Spilled::Values { values, nan_runs };
                        generated = Some(reason);
                    }
                }
            }
            other => {
                states[index] = other;
                generated = Some(GeneratedIndexReason::Unreadable { column: name });
            }
        }
    }
    let time_column = if generated.is_none() { candidate } else { None };
    let time = match time {
        Some(time) => time,
        None => spilled_row_ordinal_axis(&set, &axis.stem("rowindex"), row_count as usize)?,
    };

    let mut series = Vec::new();
    for (index, state) in states.iter_mut().enumerate() {
        match std::mem::replace(state, Spilled::Taken) {
            Spilled::Timestamp { .. } => {
                *state = Spilled::Unloaded(
                    "it is a second timestamp column; pick it as the time column to plot \
                     against it"
                        .to_string(),
                );
            }
            Spilled::Values { values, nan_runs } => {
                if !nan_runs.is_empty() {
                    warn!(
                        run_count = nan_runs.len(),
                        "NaN run(s) flagged in a numeric column (SPEC §1.3)"
                    );
                }
                let built = Series::with_anomalies(
                    names[index].clone(),
                    SeriesValues::Spilled(values),
                    Anomalies {
                        nan_runs,
                        ..Anomalies::default()
                    },
                );
                series.push(match stats[index].take() {
                    Some(stats) => built.with_row_group_stats(stats),
                    None => built,
                });
            }
            other => *state = other,
        }
    }
    if series.is_empty() {
        return Err(GlydeError::SingleColumnFile);
    }

    let time_column_name = match time_column {
        Some(index) => names[index].clone(),
        None => super::super::GENERATED_TIME_COLUMN_NAME.to_string(),
    };
    let inference = TimeIndexInference {
        timestamp_format_ambiguous,
        time_column,
        generated,
    };
    log_time_index_decision(&time_column_name, &inference, row_count as usize);
    info!(
        row_count,
        column_count = series.len(),
        spill_dir = %set.dir().display(),
        "Parquet file materialized through the on-disk spill cache (SPEC §5.1)"
    );
    let mut dataset = Dataset {
        time,
        time_column_name,
        columns: series,
    };
    if overrides.sort_by_time {
        drop(set);
        dataset = super::super::spill_sort::sort_spilled_by_time(
            dataset,
            cache_dir,
            request.budget,
            request.cancelled,
        )?;
    }

    let unloaded_columns = names
        .into_iter()
        .zip(states)
        .filter_map(|(name, state)| match state {
            Spilled::Unloaded(reason) => Some(unloaded_column(name, reason)),
            _ => None,
        })
        .collect();
    finish_open(
        schema,
        partition_keys,
        row_count,
        unloaded_columns,
        dataset,
        inference,
    )
}

/// One spilled column once every source is read and its nulls settled.
enum Spilled {
    /// A series' values, with the NaN runs flagged while they were written.
    Values {
        values: SpilledValues,
        nan_runs: Vec<Range<usize>>,
    },
    /// A typed timestamp column: the time index, or not loaded.
    Timestamp {
        unit: TimeUnit,
        zone: Zone,
        ticks: SpillVec<i64>,
        /// Its first row without a timestamp, if any.
        first_missing: Option<usize>,
    },
    Unloaded(String),
    /// Moved into the time axis or the series.
    Taken,
}

impl Spilled {
    fn values(values: SpilledValues, nan_runs: Vec<Range<usize>>) -> Self {
        Spilled::Values { values, nan_runs }
    }
}

/// A column's spill writer, in its own dtype.
enum Writer {
    Bool(SpillVecWriter<u8>),
    I8(SpillVecWriter<i8>),
    I16(SpillVecWriter<i16>),
    I32(SpillVecWriter<i32>),
    I64(SpillVecWriter<i64>),
    U8(SpillVecWriter<u8>),
    U16(SpillVecWriter<u16>),
    U32(SpillVecWriter<u32>),
    U64(SpillVecWriter<u64>),
    F32(SpillVecWriter<f32>, NanRunScan),
    F64(SpillVecWriter<f64>, NanRunScan),
    String(SpillStringsWriter),
    Timestamp {
        unit: TimeUnit,
        zone: Zone,
        ticks: SpillVecWriter<i64>,
    },
}

/// One column being spilled, batch by batch, with what its nulls will need
/// to be settled.
struct ColumnSpill {
    writer: Writer,
    set: SpillSet,
    stem: String,
    rows: usize,
    nulls: usize,
    first_null: Option<usize>,
    /// `1` per row with a value, for a boolean or integer column, whose own
    /// dtype has no gap: created at its first null, every row before it
    /// valid. A column without nulls never has one.
    validity: Option<SpillVecWriter<u8>>,
}

impl ColumnSpill {
    /// The writer for `values`' dtype (the empty in-memory accumulator the
    /// footer chose, [`ColumnValues::for_type`]).
    fn create(set: &SpillSet, index: usize, values: &ColumnValues) -> Result<Self> {
        let stem = format!("pq{index}");
        let writer = match values {
            ColumnValues::Bool(_) => Writer::Bool(SpillVecWriter::create(set, &stem)?),
            ColumnValues::I8(_) => Writer::I8(SpillVecWriter::create(set, &stem)?),
            ColumnValues::I16(_) => Writer::I16(SpillVecWriter::create(set, &stem)?),
            ColumnValues::I32(_) => Writer::I32(SpillVecWriter::create(set, &stem)?),
            ColumnValues::I64(_) => Writer::I64(SpillVecWriter::create(set, &stem)?),
            ColumnValues::U8(_) => Writer::U8(SpillVecWriter::create(set, &stem)?),
            ColumnValues::U16(_) => Writer::U16(SpillVecWriter::create(set, &stem)?),
            ColumnValues::U32(_) => Writer::U32(SpillVecWriter::create(set, &stem)?),
            ColumnValues::U64(_) => Writer::U64(SpillVecWriter::create(set, &stem)?),
            ColumnValues::F32(_) => {
                Writer::F32(SpillVecWriter::create(set, &stem)?, NanRunScan::default())
            }
            ColumnValues::F64(_) => {
                Writer::F64(SpillVecWriter::create(set, &stem)?, NanRunScan::default())
            }
            ColumnValues::String(_) => Writer::String(SpillStringsWriter::create(set, &stem)?),
            ColumnValues::Timestamp { unit, zone, .. } => Writer::Timestamp {
                unit: *unit,
                zone: *zone,
                ticks: SpillVecWriter::create(set, &stem)?,
            },
        };
        Ok(Self {
            writer,
            set: set.clone(),
            stem,
            rows: 0,
            nulls: 0,
            first_null: None,
            validity: None,
        })
    }

    /// Whether this column's nulls are settled from a validity record
    /// after the spill: a boolean or integer one.
    fn settles_from_validity(&self) -> bool {
        matches!(
            self.writer,
            Writer::Bool(_)
                | Writer::I8(_)
                | Writer::I16(_)
                | Writer::I32(_)
                | Writer::I64(_)
                | Writer::U8(_)
                | Writer::U16(_)
                | Writer::U32(_)
                | Writer::U64(_)
        )
    }

    /// Appends one batch's array, which has this column's type (every batch
    /// is decoded against the one footer schema).
    fn append(&mut self, array: &ArrayRef) -> Result<()> {
        // Logical nulls, as in memory: a dictionary column is null where its
        // key is, and where the label its key points at is.
        let nulls = array.logical_nulls().filter(|nulls| nulls.null_count() > 0);
        if let Some(nulls) = &nulls {
            if self.first_null.is_none() {
                self.first_null = nulls
                    .iter()
                    .position(|valid| !valid)
                    .map(|row| self.rows + row);
            }
            self.nulls += nulls.null_count();
            if self.validity.is_none() && self.settles_from_validity() {
                let mut validity =
                    SpillVecWriter::create(&self.set, &format!("{}valid", self.stem))?;
                for _ in 0..self.rows {
                    validity.push(1)?;
                }
                self.validity = Some(validity);
            }
        }
        if let Some(validity) = &mut self.validity {
            match &nulls {
                Some(nulls) => {
                    for valid in nulls.iter() {
                        validity.push(u8::from(valid))?;
                    }
                }
                None => {
                    for _ in 0..array.len() {
                        validity.push(1)?;
                    }
                }
            }
        }
        let valid = |row: usize| nulls.as_ref().is_none_or(|nulls| nulls.is_valid(row));
        match &mut self.writer {
            // What lands at a null row of these is a placeholder until
            // `settle` (Golden Rule 1: it is not a sample).
            Writer::Bool(values) => {
                for value in array.as_boolean().values().iter() {
                    values.push(u8::from(value))?;
                }
            }
            Writer::I8(values) => {
                values.extend_from_slice(array.as_primitive::<Int8Type>().values())?
            }
            Writer::I16(values) => {
                values.extend_from_slice(array.as_primitive::<Int16Type>().values())?
            }
            Writer::I32(values) => {
                values.extend_from_slice(array.as_primitive::<Int32Type>().values())?
            }
            Writer::I64(values) => {
                values.extend_from_slice(array.as_primitive::<Int64Type>().values())?
            }
            Writer::U8(values) => {
                values.extend_from_slice(array.as_primitive::<UInt8Type>().values())?
            }
            Writer::U16(values) => {
                values.extend_from_slice(array.as_primitive::<UInt16Type>().values())?
            }
            Writer::U32(values) => {
                values.extend_from_slice(array.as_primitive::<UInt32Type>().values())?
            }
            Writer::U64(values) => {
                values.extend_from_slice(array.as_primitive::<UInt64Type>().values())?
            }
            // A float's gap is NaN and a text's an empty label, written in
            // place — what `settle_missing` makes of them in memory.
            Writer::F32(values, nan_runs) => {
                for (row, &value) in array
                    .as_primitive::<Float32Type>()
                    .values()
                    .iter()
                    .enumerate()
                {
                    let value = if valid(row) { value } else { f32::NAN };
                    nan_runs.observe(f64::from(value));
                    values.push(value)?;
                }
            }
            Writer::F64(values, nan_runs) => {
                for (row, &value) in array
                    .as_primitive::<Float64Type>()
                    .values()
                    .iter()
                    .enumerate()
                {
                    let value = if valid(row) { value } else { f64::NAN };
                    nan_runs.observe(value);
                    values.push(value)?;
                }
            }
            Writer::String(values) => {
                let mut written = Ok(());
                for_each_text(array, |text| {
                    if written.is_ok() {
                        written = values.push(text);
                    }
                });
                written?;
            }
            Writer::Timestamp { ticks, .. } => match array.data_type() {
                DataType::Timestamp(ArrowTimeUnit::Second, _) => {
                    ticks.extend_from_slice(array.as_primitive::<TimestampSecondType>().values())?
                }
                DataType::Timestamp(ArrowTimeUnit::Millisecond, _) => ticks
                    .extend_from_slice(array.as_primitive::<TimestampMillisecondType>().values())?,
                DataType::Timestamp(ArrowTimeUnit::Microsecond, _) => ticks
                    .extend_from_slice(array.as_primitive::<TimestampMicrosecondType>().values())?,
                _ => ticks
                    .extend_from_slice(array.as_primitive::<TimestampNanosecondType>().values())?,
            },
        }
        self.rows += array.len();
        Ok(())
    }

    /// Appends `rows` copies of a partition key's value: every row of one
    /// file of a partitioned directory has its directory's value.
    fn append_label(&mut self, label: &str, rows: usize) -> Result<()> {
        if let Writer::String(values) = &mut self.writer {
            for _ in 0..rows {
                values.push(label)?;
            }
            self.rows += rows;
        }
        Ok(())
    }

    /// The finished column, its nulls turned into SPEC §1.3's gaps exactly
    /// as [`SourceColumn::settle_missing`] turns them in memory.
    fn settle(self, name: &str) -> Result<Spilled> {
        let ColumnSpill {
            writer,
            set,
            stem,
            nulls,
            first_null,
            validity,
            ..
        } = self;
        let validity = validity.map(SpillVecWriter::finish).transpose()?;
        let kept = |values: SpilledValues, nan_runs: Vec<Range<usize>>| {
            if nulls > 0 {
                log_gaps_kept(name, nulls);
            }
            Spilled::values(values, nan_runs)
        };
        let promoted = |result: WithGaps| match result {
            Ok((values, nan_runs)) => {
                log_gaps_converted(name, nulls, "f64 with NaN gaps");
                Spilled::values(SpilledValues::F64(values), nan_runs)
            }
            Err(reason) => Spilled::Unloaded(reason),
        };
        Ok(match writer {
            Writer::Timestamp { unit, zone, ticks } => Spilled::Timestamp {
                unit,
                zone,
                ticks: ticks.finish()?,
                first_missing: first_null,
            },
            Writer::F32(values, nan_runs) => {
                kept(SpilledValues::F32(values.finish()?), nan_runs.finish())
            }
            Writer::F64(values, nan_runs) => {
                kept(SpilledValues::F64(values.finish()?), nan_runs.finish())
            }
            Writer::String(values) => kept(SpilledValues::String(values.finish()?), Vec::new()),
            Writer::Bool(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::Bool(values), Vec::new()),
                    Some(validity) => {
                        let text = bools_as_text(&set, &stem, &values, validity)?;
                        log_gaps_converted(name, nulls, "text");
                        Spilled::values(SpilledValues::String(text), Vec::new())
                    }
                }
            }
            Writer::I8(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::I8(values), Vec::new()),
                    Some(validity) => promoted(with_gaps(&set, &stem, &values, validity, |n| {
                        Some(f64::from(n))
                    })?),
                }
            }
            Writer::I16(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::I16(values), Vec::new()),
                    Some(validity) => promoted(with_gaps(&set, &stem, &values, validity, |n| {
                        Some(f64::from(n))
                    })?),
                }
            }
            Writer::I32(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::I32(values), Vec::new()),
                    Some(validity) => promoted(with_gaps(&set, &stem, &values, validity, |n| {
                        Some(f64::from(n))
                    })?),
                }
            }
            Writer::I64(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::I64(values), Vec::new()),
                    Some(validity) => promoted(with_gaps(&set, &stem, &values, validity, |n| {
                        (n.unsigned_abs() <= MAX_EXACT_F64_INTEGER).then_some(n as f64)
                    })?),
                }
            }
            Writer::U8(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::U8(values), Vec::new()),
                    Some(validity) => promoted(with_gaps(&set, &stem, &values, validity, |n| {
                        Some(f64::from(n))
                    })?),
                }
            }
            Writer::U16(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::U16(values), Vec::new()),
                    Some(validity) => promoted(with_gaps(&set, &stem, &values, validity, |n| {
                        Some(f64::from(n))
                    })?),
                }
            }
            Writer::U32(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::U32(values), Vec::new()),
                    Some(validity) => promoted(with_gaps(&set, &stem, &values, validity, |n| {
                        Some(f64::from(n))
                    })?),
                }
            }
            Writer::U64(values) => {
                let values = values.finish()?;
                match &validity {
                    None => Spilled::values(SpilledValues::U64(values), Vec::new()),
                    Some(validity) => promoted(with_gaps(&set, &stem, &values, validity, |n| {
                        (n <= MAX_EXACT_F64_INTEGER).then_some(n as f64)
                    })?),
                }
            }
        })
    }
}

/// An integer column with nulls as `f64` with NaN gaps, and its NaN runs —
/// or why it cannot be turned into one exactly.
type WithGaps = std::result::Result<(SpillVec<f64>, Vec<Range<usize>>), String>;

/// A boolean column with nulls as text, as a CSV boolean column with blank
/// cells is: `true`, `false`, or an empty label at a null.
fn bools_as_text(
    set: &SpillSet,
    stem: &str,
    values: &SpillVec<u8>,
    validity: &SpillVec<u8>,
) -> Result<crate::index::spill::SpillStrings> {
    let mut text = SpillStringsWriter::create(set, &format!("{stem}text"))?;
    let (mut values, mut validity) = (values.cursor()?, validity.cursor()?);
    while let (Some(value), Some(valid)) = (values.next_value()?, validity.next_value()?) {
        text.push(match (valid != 0, value != 0) {
            (false, _) => "",
            (true, true) => "true",
            (true, false) => "false",
        })?;
    }
    text.finish()
}

/// An integer column with nulls as `f64` with NaN gaps — when `exact` turns
/// every present value into the `f64` it equals; otherwise the reason it is
/// not loaded (SPEC §1.4: never upcast with precision loss). The NaN runs
/// are flagged on the way.
fn with_gaps<T: bytemuck::Pod>(
    set: &SpillSet,
    stem: &str,
    values: &SpillVec<T>,
    validity: &SpillVec<u8>,
    exact: impl Fn(T) -> Option<f64>,
) -> Result<WithGaps> {
    let (mut cursor, mut valid) = (values.cursor()?, validity.cursor()?);
    while let (Some(value), Some(present)) = (cursor.next_value()?, valid.next_value()?) {
        if present != 0 && exact(value).is_none() {
            return Ok(Err(TOO_LARGE_FOR_GAPS.to_string()));
        }
    }
    let mut converted = SpillVecWriter::create(set, &format!("{stem}gaps"))?;
    let mut nan_runs = NanRunScan::default();
    let (mut cursor, mut valid) = (values.cursor()?, validity.cursor()?);
    while let (Some(value), Some(present)) = (cursor.next_value()?, valid.next_value()?) {
        let value = match exact(value) {
            Some(value) if present != 0 => value,
            _ => f64::NAN,
        };
        nan_runs.observe(value);
        converted.push(value)?;
    }
    Ok(Ok((converted.finish()?, nan_runs.finish())))
}

/// The spill cache as an [`AxisSink`]: each axis it builds is a set of spill
/// files of the open's own set, under a stem of its own.
struct OnDisk<'a> {
    set: &'a SpillSet,
    files: Cell<usize>,
}

impl OnDisk<'_> {
    /// A stem no other axis file of this open has.
    fn stem(&self, what: &str) -> String {
        let next = self.files.get();
        self.files.set(next + 1);
        format!("pqaxis{next}{what}")
    }
}

impl AxisSink for OnDisk<'_> {
    type Progressive = SpillVecWriter<f64>;
    type Absolute = TimestampsSpillWriter;

    fn progressive(&self, _rows: usize) -> Result<SpillVecWriter<f64>> {
        SpillVecWriter::create(self.set, &self.stem("progressive"))
    }

    fn absolute(&self, _rows: usize) -> Result<TimestampsSpillWriter> {
        TimestampsSpillWriter::create(self.set, &self.stem("ts"))
    }
}

impl AxisWriter<f64> for SpillVecWriter<f64> {
    type Output = ProgressiveValues;

    fn push(&mut self, value: f64) -> Result<()> {
        SpillVecWriter::push(self, value)
    }

    fn finish(self) -> Result<ProgressiveValues> {
        Ok(ProgressiveValues::Spilled(SpillVecWriter::finish(self)?))
    }
}

impl AxisWriter<Timestamp> for TimestampsSpillWriter {
    type Output = Timestamps;

    fn push(&mut self, value: Timestamp) -> Result<()> {
        TimestampsSpillWriter::push(self, &value)
    }

    fn finish(self) -> Result<Timestamps> {
        TimestampsSpillWriter::finish(self)
    }
}

impl TickValues for SpillVec<i64> {
    fn len(&self) -> usize {
        SpillVec::len(self)
    }

    fn for_each_tick(&self, visit: &mut dyn FnMut(i64)) -> Result<()> {
        self.read_chunks(0..SpillVec::len(self), &mut |chunk| {
            chunk.iter().for_each(|&tick| visit(tick));
            Ok(())
        })
    }
}

impl CandidateValues for SpilledValues {
    fn is_numeric(&self) -> bool {
        !matches!(self, SpilledValues::Bool(_) | SpilledValues::String(_))
    }

    fn len(&self) -> usize {
        SpilledValues::len(self)
    }

    fn for_each_f64(&self, visit: &mut dyn FnMut(f64)) -> Result<()> {
        match self {
            SpilledValues::I8(v) => each_as_f64(v, f64::from, visit),
            SpilledValues::I16(v) => each_as_f64(v, f64::from, visit),
            SpilledValues::I32(v) => each_as_f64(v, f64::from, visit),
            SpilledValues::I64(v) => each_as_f64(v, |n| n as f64, visit),
            SpilledValues::U8(v) => each_as_f64(v, f64::from, visit),
            SpilledValues::U16(v) => each_as_f64(v, f64::from, visit),
            SpilledValues::U32(v) => each_as_f64(v, f64::from, visit),
            SpilledValues::U64(v) => each_as_f64(v, |n| n as f64, visit),
            SpilledValues::F32(v) => each_as_f64(v, f64::from, visit),
            SpilledValues::F64(v) => each_as_f64(v, |n| n, visit),
            // Not numeric: never asked (see `is_numeric`).
            SpilledValues::Bool(_) | SpilledValues::String(_) => Ok(()),
        }
    }

    fn for_each_field(&self, visit: &mut dyn FnMut(&str)) -> Result<()> {
        match self {
            SpilledValues::I8(v) => render(v, visit),
            SpilledValues::I16(v) => render(v, visit),
            SpilledValues::I32(v) => render(v, visit),
            SpilledValues::I64(v) => render(v, visit),
            SpilledValues::U8(v) => render(v, visit),
            SpilledValues::U16(v) => render(v, visit),
            SpilledValues::U32(v) => render(v, visit),
            SpilledValues::U64(v) => render(v, visit),
            SpilledValues::F32(v) => render(v, visit),
            SpilledValues::F64(v) => render(v, visit),
            SpilledValues::String(v) => {
                let mut cursor = v.cursor()?;
                let mut field = Vec::new();
                while cursor.next_into(&mut field)? {
                    // Written from a `&str` by `ColumnSpill::append`.
                    visit(std::str::from_utf8(&field).unwrap_or_default());
                }
                Ok(())
            }
            SpilledValues::Bool(_) => Ok(()),
        }
    }
}

/// Each of `values` as `f64`, read back through bounded chunks.
fn each_as_f64<T: bytemuck::Pod>(
    values: &SpillVec<T>,
    convert: impl Fn(T) -> f64,
    visit: &mut dyn FnMut(f64),
) -> Result<()> {
    values.read_chunks(0..values.len(), &mut |chunk| {
        chunk.iter().for_each(|&value| visit(convert(value)));
        Ok(())
    })
}

/// Each of `values` as the text [`ColumnValues::for_each_field`] gives the
/// same number in memory: its `Display`, one reused buffer.
fn render<T: bytemuck::Pod + std::fmt::Display>(
    values: &SpillVec<T>,
    visit: &mut dyn FnMut(&str),
) -> Result<()> {
    use std::fmt::Write;
    let mut buffer = String::new();
    values.read_chunks(0..values.len(), &mut |chunk| {
        for value in chunk {
            buffer.clear();
            // Writing to a `String` cannot fail.
            let _ = write!(buffer, "{value}");
            visit(&buffer);
        }
        Ok(())
    })
}
