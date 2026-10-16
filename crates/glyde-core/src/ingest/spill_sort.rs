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

//! SPEC §2.1's "[Sort]" for a spilled dataset (issue #110): the rows of a
//! file too large for memory, reordered by ascending timestamp on disk, in
//! bounded memory — the same order `dataset::sort_dataset_by_time` gives
//! the in-memory path (stable: equal timestamps keep their file order).
//!
//! A textbook external merge sort over whole rows:
//!
//! 1. Every row (its time value and one value per column, already typed) is
//!    read back sequentially through buffered cursors, never the mappings,
//!    and collected into runs of at most [`SORT_RUN_BYTES`], each sorted in
//!    memory by `(tick, source row)` and written to a run file.
//! 2. The unsorted dataset is then dropped, so its spill files are deleted
//!    before the sorted copy is written: the disk holds at most two copies
//!    of the data at any time (see `dataset::choose_storage`'s check).
//! 3. Runs are merged [`SORT_FAN_IN`] at a time (so a huge file never needs
//!    hundreds of open files) until one merge can feed the final writers —
//!    the same spill writers a load uses, so NaN runs are detected afresh
//!    in the new order exactly as on the way in.
//!
//! The source row number is part of the key, which makes every key unique:
//! the result does not depend on how the runs happened to be cut.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use super::dataset::{
    progressive_value_to_tick, ColumnSpillWriter, ColumnValue, Dataset, ProgressiveValues,
    TimeAxis, Timestamps,
};
use crate::budget::RamBudget;
use crate::index::spill::{SpillCursor, SpillSet, SpillStringsCursor, SpillVecWriter};
use crate::series::{Dtype, SeriesValues, SpilledValues};
use crate::time::TimestampFormat;
use crate::{GlydeError, Result};

/// The most row bytes held in memory while cutting runs: an eighth of the
/// RAM budget (SPEC §5's `min(25% RAM, 4 GB)` cap), within these bounds —
/// the run's sort index costs about as much again, and every other buffer
/// of the sort is a fixed handful of KB. A flat number at any file size.
pub(super) const SORT_RUN_BYTES: usize = 64 << 20;
pub(super) const MIN_SORT_RUN_BYTES: usize = 1 << 20;
/// The most runs merged at once (one open file and one read buffer each).
pub(super) const SORT_FAN_IN: usize = 64;
/// Read/write buffer of one run file.
const RUN_BUFFER_BYTES: usize = 256 * 1024;

/// Sorts `dataset`'s rows by ascending time (see the module docs), writing
/// the sorted copy as a new spill set in `cache_dir`. A dataset that is
/// already in order is returned as is.
pub(super) fn sort_spilled_by_time(
    dataset: Dataset,
    cache_dir: &Path,
    budget: RamBudget,
) -> Result<Dataset> {
    let run_bytes = usize::try_from(budget.cap_bytes() / 8)
        .unwrap_or(usize::MAX)
        .clamp(MIN_SORT_RUN_BYTES, SORT_RUN_BYTES);
    sort_spilled_by_time_with(dataset, cache_dir, run_bytes, SORT_FAN_IN)
}

/// [`sort_spilled_by_time`] with explicit run size and fan-in, so a test can
/// force several runs and several merge passes on a small file.
pub(super) fn sort_spilled_by_time_with(
    dataset: Dataset,
    cache_dir: &Path,
    run_bytes: usize,
    fan_in: usize,
) -> Result<Dataset> {
    let Some(layout) = RowLayout::of(&dataset) else {
        warn!("sort-by-time requested on a dataset that is not fully spilled; left unsorted");
        return Ok(dataset);
    };
    if is_sorted(&dataset)? {
        info!("sort-by-time requested, but the timestamps are already in order");
        return Ok(dataset);
    }
    let rows = dataset.time.len();
    info!(
        rows,
        "sorting a spilled file by time on disk (SPEC §2.1 [Sort], issue #110)"
    );

    // --- 1. Cut sorted runs ---------------------------------------------
    let runs_set = SpillSet::create(cache_dir)?;
    let mut runs = cut_runs(&dataset, &layout, &runs_set, run_bytes)?;
    let time_column_name = dataset.time_column_name.clone();
    let names: Vec<String> = dataset
        .columns
        .iter()
        .map(|series| series.name().to_string())
        .collect();
    // --- 2. The unsorted files are no longer needed ----------------------
    drop(dataset);

    // --- 3. Merge -----------------------------------------------------------
    let mut pass = 0;
    while runs.len() > fan_in {
        pass += 1;
        info!(runs = runs.len(), pass, "merging sorted runs");
        let mut merged = Vec::new();
        for group in runs.chunks(fan_in) {
            let path = run_path(&runs_set, pass, merged.len());
            let mut out = BufWriter::with_capacity(RUN_BUFFER_BYTES, create(&path)?);
            let count = merge(group, |record| write_record(&mut out, &path, record))?;
            out.flush().map_err(io_error(&path))?;
            for run in group {
                let _ = std::fs::remove_file(&run.path);
            }
            merged.push(Run { path, count });
        }
        runs = merged;
    }

    let set = SpillSet::create(cache_dir)?;
    let mut writer = RowWriter::create(&set, &layout)?;
    let written = merge(&runs, |record| writer.push(&layout, record))?;
    debug_assert_eq!(written, rows);
    drop(runs_set);
    let sorted = writer.finish(&layout, time_column_name, names)?;
    info!(rows, "spilled file sorted by time");
    Ok(sorted)
}

/// How a row's time value and columns are laid out in a run record.
struct RowLayout {
    time: TimeLayout,
    columns: Vec<Dtype>,
}

#[derive(Clone, Copy)]
enum TimeLayout {
    /// Unit code (1 byte) and UTC offset (8 bytes) after the tick.
    Absolute(TimestampFormat),
    /// The progressive value (8 bytes).
    Progressive,
}

impl RowLayout {
    fn of(dataset: &Dataset) -> Option<RowLayout> {
        let time = match &dataset.time {
            TimeAxis::Absolute {
                timestamps: Timestamps::Spilled { .. },
                format,
            } => TimeLayout::Absolute(*format),
            TimeAxis::Progressive {
                values: ProgressiveValues::Spilled(_),
            } => TimeLayout::Progressive,
            _ => return None,
        };
        let columns = dataset
            .columns
            .iter()
            .map(|series| match series.values() {
                SeriesValues::Spilled(values) => Some(values.dtype()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        Some(RowLayout { time, columns })
    }
}

/// Whether the time axis is already non-decreasing, read through a cursor.
fn is_sorted(dataset: &Dataset) -> Result<bool> {
    let mut previous = i128::MIN;
    let mut time = TimeCursor::new(dataset)?;
    let mut payload = Vec::new();
    while let Some(tick) = time.next_into(&mut payload)? {
        if tick < previous {
            return Ok(false);
        }
        previous = tick;
    }
    Ok(true)
}

/// Reads the time axis row by row: the sort key, with the row's time
/// payload appended to a buffer.
///
/// `Absolute` is boxed: three cursors to `Progressive`'s one, each holding
/// a buffer handle and a `PathBuf` (`clippy::large_enum_variant`, as for
/// `dataset::TimeAxisSpillWriter`).
enum TimeCursor {
    Absolute(Box<AbsoluteCursors>),
    Progressive(SpillCursor<f64>),
}

struct AbsoluteCursors {
    ticks: SpillCursor<i128>,
    units: SpillCursor<u8>,
    offsets: SpillCursor<i64>,
}

impl TimeCursor {
    fn new(dataset: &Dataset) -> Result<TimeCursor> {
        match &dataset.time {
            TimeAxis::Absolute {
                timestamps:
                    Timestamps::Spilled {
                        ticks,
                        units,
                        offsets,
                    },
                ..
            } => Ok(TimeCursor::Absolute(Box::new(AbsoluteCursors {
                ticks: ticks.cursor()?,
                units: units.cursor()?,
                offsets: offsets.cursor()?,
            }))),
            TimeAxis::Progressive {
                values: ProgressiveValues::Spilled(values),
            } => Ok(TimeCursor::Progressive(values.cursor()?)),
            _ => Err(not_spilled()),
        }
    }

    /// The next row's tick, appending its time payload to `payload`.
    fn next_into(&mut self, payload: &mut Vec<u8>) -> Result<Option<i128>> {
        match self {
            TimeCursor::Absolute(cursors) => {
                let AbsoluteCursors {
                    ticks,
                    units,
                    offsets,
                } = &mut **cursors;
                let Some(tick) = ticks.next_value()? else {
                    return Ok(None);
                };
                let unit = units.next_value()?.ok_or_else(short_column)?;
                let offset = offsets.next_value()?.ok_or_else(short_column)?;
                payload.push(unit);
                payload.extend_from_slice(&offset.to_le_bytes());
                Ok(Some(tick))
            }
            TimeCursor::Progressive(values) => {
                let Some(value) = values.next_value()? else {
                    return Ok(None);
                };
                payload.extend_from_slice(&value.to_le_bytes());
                Ok(Some(progressive_value_to_tick(value)))
            }
        }
    }
}

/// Reads one data column row by row, appending each value to a buffer.
enum ColumnCursor {
    Bool(SpillCursor<u8>),
    I64(SpillCursor<i64>),
    F64(SpillCursor<f64>),
    String(SpillStringsCursor, Vec<u8>),
}

impl ColumnCursor {
    fn new(values: &SpilledValues) -> Result<ColumnCursor> {
        Ok(match values {
            SpilledValues::Bool(values) => ColumnCursor::Bool(values.cursor()?),
            SpilledValues::I64(values) => ColumnCursor::I64(values.cursor()?),
            SpilledValues::F64(values) => ColumnCursor::F64(values.cursor()?),
            SpilledValues::String(values) => ColumnCursor::String(values.cursor()?, Vec::new()),
        })
    }

    fn append_next(&mut self, payload: &mut Vec<u8>) -> Result<()> {
        match self {
            ColumnCursor::Bool(values) => {
                payload.push(values.next_value()?.ok_or_else(short_column)?);
            }
            ColumnCursor::I64(values) => {
                let value = values.next_value()?.ok_or_else(short_column)?;
                payload.extend_from_slice(&value.to_le_bytes());
            }
            ColumnCursor::F64(values) => {
                let value = values.next_value()?.ok_or_else(short_column)?;
                payload.extend_from_slice(&value.to_le_bytes());
            }
            ColumnCursor::String(values, field) => {
                if !values.next_into(field)? {
                    return Err(short_column());
                }
                payload.extend_from_slice(&(field.len() as u32).to_le_bytes());
                payload.extend_from_slice(field);
            }
        }
        Ok(())
    }
}

/// One row on its way through the sort: its key and its encoded values.
struct Record {
    tick: i128,
    row: u64,
    payload: Vec<u8>,
}

/// A sorted run file and how many records it holds.
struct Run {
    path: PathBuf,
    count: usize,
}

fn run_path(set: &SpillSet, pass: usize, index: usize) -> PathBuf {
    set.dir().join(format!("sort-{pass}-{index}.run"))
}

/// Step 1: reads every row in file order and writes it into sorted runs of
/// at most `run_bytes` of row data each.
fn cut_runs(
    dataset: &Dataset,
    layout: &RowLayout,
    set: &SpillSet,
    run_bytes: usize,
) -> Result<Vec<Run>> {
    let mut time = TimeCursor::new(dataset)?;
    let mut columns = dataset
        .columns
        .iter()
        .map(|series| match series.values() {
            SeriesValues::Spilled(values) => ColumnCursor::new(values),
            _ => Err(not_spilled()),
        })
        .collect::<Result<Vec<_>>>()?;
    debug_assert_eq!(columns.len(), layout.columns.len());

    let mut runs = Vec::new();
    let mut bytes = Vec::new();
    // (tick, row, start, end) of each row in `bytes`.
    let mut index: Vec<(i128, u64, usize, usize)> = Vec::new();
    let mut row = 0u64;
    loop {
        let start = bytes.len();
        let Some(tick) = time.next_into(&mut bytes)? else {
            break;
        };
        for column in &mut columns {
            column.append_next(&mut bytes)?;
        }
        index.push((tick, row, start, bytes.len()));
        row += 1;
        if bytes.len() >= run_bytes {
            runs.push(write_run(set, runs.len(), &mut bytes, &mut index)?);
        }
    }
    if !index.is_empty() {
        runs.push(write_run(set, runs.len(), &mut bytes, &mut index)?);
    }
    info!(runs = runs.len(), rows = row, "sorted runs written");
    Ok(runs)
}

fn write_run(
    set: &SpillSet,
    number: usize,
    bytes: &mut Vec<u8>,
    index: &mut Vec<(i128, u64, usize, usize)>,
) -> Result<Run> {
    index.sort_unstable_by_key(|&(tick, row, _, _)| (tick, row));
    let path = run_path(set, 0, number);
    let mut out = BufWriter::with_capacity(RUN_BUFFER_BYTES, create(&path)?);
    for &(tick, row, start, end) in index.iter() {
        write_raw(&mut out, &path, tick, row, &bytes[start..end])?;
    }
    out.flush().map_err(io_error(&path))?;
    let count = index.len();
    bytes.clear();
    index.clear();
    Ok(Run { path, count })
}

fn write_record(out: &mut impl Write, path: &Path, record: &Record) -> Result<()> {
    write_raw(out, path, record.tick, record.row, &record.payload)
}

/// A record on disk: tick (16 bytes), source row (8), payload length (4),
/// payload — all little-endian.
fn write_raw(
    out: &mut impl Write,
    path: &Path,
    tick: i128,
    row: u64,
    payload: &[u8],
) -> Result<()> {
    let header = [
        &tick.to_le_bytes()[..],
        &row.to_le_bytes()[..],
        &(payload.len() as u32).to_le_bytes()[..],
    ];
    for part in header {
        out.write_all(part).map_err(io_error(path))?;
    }
    out.write_all(payload).map_err(io_error(path))
}

/// Reads one run file's records back in order.
struct RunReader {
    reader: BufReader<File>,
    path: PathBuf,
    remaining: usize,
}

impl RunReader {
    fn open(run: &Run) -> Result<RunReader> {
        let file = File::open(&run.path).map_err(io_error(&run.path))?;
        Ok(RunReader {
            reader: BufReader::with_capacity(RUN_BUFFER_BYTES, file),
            path: run.path.clone(),
            remaining: run.count,
        })
    }

    fn next_record(&mut self) -> Result<Option<Record>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut header = [0u8; 28];
        self.reader
            .read_exact(&mut header)
            .map_err(io_error(&self.path))?;
        let tick = i128::from_le_bytes(header[0..16].try_into().expect("16 bytes"));
        let row = u64::from_le_bytes(header[16..24].try_into().expect("8 bytes"));
        let len = u32::from_le_bytes(header[24..28].try_into().expect("4 bytes")) as usize;
        let mut payload = vec![0u8; len];
        self.reader
            .read_exact(&mut payload)
            .map_err(io_error(&self.path))?;
        self.remaining -= 1;
        Ok(Some(Record { tick, row, payload }))
    }
}

/// K-way merges `runs` in `(tick, row)` order, handing each record to
/// `sink`. Returns how many records were merged.
fn merge(runs: &[Run], mut sink: impl FnMut(&Record) -> Result<()>) -> Result<usize> {
    let mut readers = runs
        .iter()
        .map(RunReader::open)
        .collect::<Result<Vec<_>>>()?;
    let mut heads: Vec<Option<Record>> = Vec::with_capacity(readers.len());
    let mut heap = BinaryHeap::new();
    for (index, reader) in readers.iter_mut().enumerate() {
        let head = reader.next_record()?;
        if let Some(record) = &head {
            heap.push(Reverse((record.tick, record.row, index)));
        }
        heads.push(head);
    }
    let mut merged = 0;
    while let Some(Reverse((_, _, index))) = heap.pop() {
        let record = heads[index].take().expect("a queued run has a head");
        sink(&record)?;
        merged += 1;
        if let Some(next) = readers[index].next_record()? {
            heap.push(Reverse((next.tick, next.row, index)));
            heads[index] = Some(next);
        }
    }
    Ok(merged)
}

/// Writes merged rows into a fresh spilled dataset.
struct RowWriter {
    time: TimeWriter,
    columns: Vec<ColumnSpillWriter>,
}

/// `Absolute` is boxed for the same reason as [`TimeCursor::Absolute`].
enum TimeWriter {
    Absolute(Box<AbsoluteWriters>),
    Progressive(SpillVecWriter<f64>),
}

struct AbsoluteWriters {
    ticks: SpillVecWriter<i128>,
    units: SpillVecWriter<u8>,
    offsets: SpillVecWriter<i64>,
}

impl RowWriter {
    fn create(set: &SpillSet, layout: &RowLayout) -> Result<RowWriter> {
        let time = match layout.time {
            TimeLayout::Absolute(_) => TimeWriter::Absolute(Box::new(AbsoluteWriters {
                ticks: SpillVecWriter::create(set, "ts")?,
                units: SpillVecWriter::create(set, "tsunit")?,
                offsets: SpillVecWriter::create(set, "tsoffset")?,
            })),
            TimeLayout::Progressive => {
                TimeWriter::Progressive(SpillVecWriter::create(set, "tsprogressive")?)
            }
        };
        let columns = layout
            .columns
            .iter()
            .enumerate()
            .map(|(index, &dtype)| ColumnSpillWriter::create(set, &format!("c{index}"), dtype))
            .collect::<Result<_>>()?;
        Ok(RowWriter { time, columns })
    }

    fn push(&mut self, layout: &RowLayout, record: &Record) -> Result<()> {
        let mut payload = PayloadReader(&record.payload);
        match &mut self.time {
            TimeWriter::Absolute(writers) => {
                let AbsoluteWriters {
                    ticks,
                    units,
                    offsets,
                } = &mut **writers;
                ticks.push(record.tick)?;
                units.push(payload.take::<1>()?[0])?;
                offsets.push(i64::from_le_bytes(payload.take::<8>()?))?;
            }
            TimeWriter::Progressive(values) => {
                values.push(f64::from_le_bytes(payload.take::<8>()?))?;
            }
        }
        for (writer, dtype) in self.columns.iter_mut().zip(&layout.columns) {
            let value = match dtype {
                Dtype::Bool => ColumnValue::Bool(payload.take::<1>()?[0] != 0),
                Dtype::I64 => ColumnValue::I64(i64::from_le_bytes(payload.take::<8>()?)),
                Dtype::F64 => ColumnValue::F64(f64::from_le_bytes(payload.take::<8>()?)),
                _ => {
                    let len = u32::from_le_bytes(payload.take::<4>()?) as usize;
                    let bytes = payload.take_slice(len)?;
                    // Written from a `&str` by `ColumnCursor::append_next`.
                    ColumnValue::Str(std::str::from_utf8(bytes).map_err(|_| corrupt_record())?)
                }
            };
            writer.push_value(value)?;
        }
        Ok(())
    }

    fn finish(
        self,
        layout: &RowLayout,
        time_column_name: String,
        names: Vec<String>,
    ) -> Result<Dataset> {
        let time = match (self.time, layout.time) {
            (TimeWriter::Absolute(writers), TimeLayout::Absolute(format)) => {
                let AbsoluteWriters {
                    ticks,
                    units,
                    offsets,
                } = *writers;
                TimeAxis::Absolute {
                    timestamps: Timestamps::Spilled {
                        ticks: ticks.finish()?,
                        units: units.finish()?,
                        offsets: offsets.finish()?,
                    },
                    format,
                }
            }
            (TimeWriter::Progressive(values), _) => TimeAxis::Progressive {
                values: ProgressiveValues::Spilled(values.finish()?),
            },
            // Unreachable: both come from the same `layout.time`.
            (TimeWriter::Absolute(_), TimeLayout::Progressive) => return Err(corrupt_record()),
        };
        let columns = self
            .columns
            .into_iter()
            .zip(names)
            .map(|(writer, name)| writer.finish(name))
            .collect::<Result<Vec<_>>>()?;
        Ok(Dataset {
            time,
            time_column_name,
            columns,
        })
    }
}

/// Decodes a record payload front to back.
struct PayloadReader<'a>(&'a [u8]);

impl PayloadReader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self.take_slice(N)?;
        Ok(bytes.try_into().expect("N bytes"))
    }

    fn take_slice(&mut self, len: usize) -> Result<&[u8]> {
        if self.0.len() < len {
            return Err(corrupt_record());
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }
}

fn create(path: &Path) -> Result<File> {
    File::create(path).map_err(io_error(path))
}

fn io_error(path: &Path) -> impl Fn(std::io::Error) -> GlydeError + '_ {
    move |source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn internal(message: &str) -> GlydeError {
    GlydeError::Io {
        path: PathBuf::from("<spill sort>"),
        source: std::io::Error::other(message.to_string()),
    }
}

fn not_spilled() -> GlydeError {
    internal("the dataset is not spilled")
}

fn short_column() -> GlydeError {
    internal("a spilled column is shorter than the time axis")
}

fn corrupt_record() -> GlydeError {
    internal("a sort run record does not match the row layout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::RamBudget;
    use crate::ingest::{load_with_budget, load_with_overrides_and_budget, IngestOverrides};

    /// Scrambled timestamps (each one twice) with a string column whose
    /// fields vary from empty to longer than a whole tiny run.
    fn fixture(dir: &Path) -> PathBuf {
        let path = dir.join("scrambled.csv");
        let mut text = String::from("timestamp,value,label\n");
        for i in 0..2_000usize {
            let second = (i * 613) % 1_000;
            text.push_str(&format!(
                "2026-01-01T00:{:02}:{:02}Z,{},{}\n",
                second / 60,
                second % 60,
                i,
                "é".repeat(i % 300)
            ));
        }
        std::fs::write(&path, text).expect("write fixture");
        path
    }

    fn sorted_in_memory(path: &Path, cache: &Path) -> Dataset {
        load_with_overrides_and_budget(
            path,
            IngestOverrides {
                sort_by_time: true,
                ..IngestOverrides::default()
            },
            RamBudget::from_total_ram_bytes(u64::MAX),
            cache,
        )
        .expect("open")
    }

    #[test]
    fn many_tiny_runs_and_many_merge_passes_give_the_same_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = tempfile::tempdir().expect("temp cache dir");
        let path = fixture(dir.path());
        let reference = sorted_in_memory(&path, cache.path());

        for (run_bytes, fan_in) in [(256, 2), (4_096, 3), (1, 64)] {
            let spilled = load_with_budget(&path, RamBudget::from_total_ram_bytes(0), cache.path())
                .expect("open");
            assert!(spilled.is_spilled());
            let sorted =
                sort_spilled_by_time_with(spilled, cache.path(), run_bytes, fan_in).expect("sort");
            assert!(sorted.is_spilled());
            assert_eq!(
                sorted.time, reference.time,
                "run {run_bytes}, fan-in {fan_in}"
            );
            for (a, b) in sorted.columns.iter().zip(&reference.columns) {
                assert_eq!(
                    a.values(),
                    b.values(),
                    "{} ({run_bytes}, {fan_in})",
                    a.name()
                );
            }
        }
    }

    #[test]
    fn an_in_memory_dataset_is_left_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = tempfile::tempdir().expect("temp cache dir");
        let path = fixture(dir.path());
        let in_memory = load_with_budget(
            &path,
            RamBudget::from_total_ram_bytes(u64::MAX),
            cache.path(),
        )
        .expect("open");
        let time = in_memory.time.clone();
        let result =
            sort_spilled_by_time(in_memory, cache.path(), RamBudget::from_total_ram_bytes(0))
                .expect("no error");
        assert_eq!(result.time, time, "not spilled: never touched");
    }
}
