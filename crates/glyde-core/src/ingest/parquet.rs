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

//! The Parquet [`Reader`] (docs/SPEC.md §1.1, docs/ROADMAP.md M7, issue
//! #153).
//!
//! A Parquet file needs none of SPEC §1.2's text inference: the schema in
//! its footer already states every column's name and type. What is left is
//! *mapping* — each Arrow type onto the SPEC §1.4 dtype that holds it
//! without loss — and choosing the time index:
//!
//! - **Dtypes are preserved** (SPEC §1.4, Golden Rule 1): `BOOLEAN`,
//!   `INT8..INT64`, `UINT8..UINT64`, `FLOAT`, `DOUBLE` and UTF-8 strings land
//!   in the [`SeriesValues`] variant of the same width and signedness, never
//!   widened. A column of any other type (decimal, date, binary, nested, a
//!   16-bit float, …) is **not loaded**: it is logged at `warn` and listed in
//!   [`InferenceReport::unloaded_columns`], so the inference bar says so
//!   (Golden Rule 2). Nulls become gaps and dictionary-encoded strings a
//!   categorical series (see [`SourceColumn::settle_missing`]).
//! - **The time index** is the first typed `TIMESTAMP` column — its type
//!   says what it is, so there is no format to guess (reported as
//!   `parquet_timestamp`). Its unit (s/ms/µs/ns) is kept as the tick unit and
//!   its timezone is honored per row (SPEC §2.1); a column without one is
//!   naive local time. A file with no timestamp column falls back to SPEC
//!   §2.1's rule for column 0, and a column of numbers or text (column 0,
//!   or one the user picks) goes through **the same time-index path as a
//!   CSV column holding the same values** (issue #166): its values are read
//!   as text by the same timestamp-format scan, so epoch s/ms/µs/ns, LabVIEW
//!   and Excel serial numbers and date/time strings become absolute time,
//!   and the user's timestamp-format correction applies to it. A numeric
//!   column that never runs backwards and does advance but matches no
//!   format is a progressive index; anything else is plotted against a
//!   generated row index, reported low-confidence. A column the user picks
//!   in the inference bar is taken as given, exactly as for CSV.
//!
//! **Row-group access.** Only the columns that will be loaded are decoded,
//! each by its own reader projected onto it alone, in parallel on the
//! `rayon` pool, row group by row group in bounded record batches,
//! cancellable between batches. An integer column also keeps its row
//! groups' exact min/max, from which `index::row_groups` builds the
//! pyramid's upper levels where that is exact (issue #162). What
//! the typed columns will cost is known exactly from the footer before a
//! single page is decoded, so SPEC §5.1's affordability check runs first
//! and a file that does not fit is refused with the numbers. There is no
//! spilled path for Parquet yet, and no progress checkpoint: the plot
//! appears when the read completes.
//!
//! **Partitioned directories** (issue #159). A directory holding `.parquet`
//! files at any depth is one dataset: its files are read in path order and
//! concatenated, and every Hive `key=value/` directory above a file becomes
//! a text column, after the file's own, so a partition key is never dropped
//! silently. Entries named `_*` or `.*` (`_SUCCESS`, `_temporary/`, `.crc`)
//! are not data and are skipped, as Spark and Hive do. Every footer is read
//! before any page: the files must agree on their columns, types and
//! partition keys (a file that differs refuses the open, naming it — no
//! guessed merge), and the affordability check covers all of them.

use super::dataset::{
    assemble_dataset, log_time_index_decision, os_spill_dir, parse_progressive_value,
    row_ordinal_axis, Dataset, GeneratedIndexReason, ProgressiveValues, TimeAxis,
    TimeCandidateScan, TimeIndexInference, Timestamps,
};
use super::infer::{series_with_anomalies, Confidence};
use super::report::{
    build_summary_and_report, InferenceReport, InferredField, OpenRequest, OpenSummary,
    SourceFacts, UnloadedColumn,
};
use super::{IngestOverrides, Reader, TimeColumnChoice};
use crate::index::row_groups::{RowGroupBounds, RowGroupStats};
use crate::series::SeriesValues;
use crate::time::{
    parse_timestamp, TimeUnit, Timestamp, TimestampFormat, TimestampFormatInference,
    TimestampFormatScan,
};
use crate::{GlydeError, Result};
use arrow::array::{Array, ArrayRef, AsArray};
use arrow::datatypes::{
    DataType, Field, Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, Schema,
    TimeUnit as ArrowTimeUnit, TimestampMicrosecondType, TimestampMillisecondType,
    TimestampNanosecondType, TimestampSecondType, UInt16Type, UInt32Type, UInt64Type, UInt8Type,
};
use parquet::arrow::arrow_reader::statistics::StatisticsConverter;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::ProjectionMask;
use parquet::file::metadata::ParquetMetaData;
use std::fs::File;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Recognizes `.parquet` files and partitioned Parquet directories (SPEC
/// §1.1).
pub struct ParquetReader;

impl Reader for ParquetReader {
    fn format_name(&self) -> &'static str {
        "parquet"
    }

    /// A `.parquet` file, or a directory holding at least one at any depth
    /// (a partitioned dataset, issue #159).
    fn recognizes(&self, path: &Path) -> bool {
        if path.is_dir() {
            return list_directory(path).is_ok_and(|listing| !listing.parquet_files.is_empty());
        }
        has_parquet_extension(path)
    }

    /// The summary of an automatic open. Unlike delimited text there is no
    /// cheaper single-column pass worth a second pipeline: the footer already
    /// answers everything but the time axis statistics, so this is the open
    /// itself.
    fn inspect(&self, path: &Path) -> Result<OpenSummary> {
        let cache_dir = os_spill_dir();
        let request = OpenRequest {
            overrides: IngestOverrides::default(),
            budget: crate::budget::RamBudget::from_system(),
            cache_dir: cache_dir.as_deref(),
            cancelled: &super::csv::NEVER_CANCELLED,
            on_checkpoint: None,
        };
        open(path, request).map(|(summary, _report, _dataset)| summary)
    }

    fn open_dataset(
        &self,
        path: &Path,
        request: OpenRequest<'_>,
    ) -> Result<(OpenSummary, InferenceReport, Dataset)> {
        open(path, request)
    }
}

/// Rows per decoded record batch: the bound on what one read step holds
/// beyond the typed columns themselves, and how often cancellation is
/// checked.
const BATCH_ROWS: usize = 64 * 1024;

/// What the encoding / delimiter / decimal-separator fields report for a
/// format that has none of them (docs/QUALITY.md §1's corpus vocabulary).
const NOT_APPLICABLE: &str = "n/a";

fn invalid(path: &Path, reason: impl ToString) -> GlydeError {
    GlydeError::InvalidParquet {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

fn open(path: &Path, request: OpenRequest<'_>) -> Result<(OpenSummary, InferenceReport, Dataset)> {
    log_ignored_overrides(request.overrides);
    let sources = source_files(path)?;
    let schema = sources[0].metadata.schema().clone();
    let partition_keys: Vec<String> = sources[0]
        .partition
        .iter()
        .map(|(key, _)| key.clone())
        .collect();
    check_sources_agree(path, &sources)?;
    let declared_rows: u64 = sources.iter().map(SourceFile::declared_rows).sum();

    // Decide every column's fate from the footers alone. Partition keys
    // follow the file's own columns, as text.
    let mut columns: Vec<SourceColumn> = schema
        .fields()
        .iter()
        .map(|field| SourceColumn {
            name: field.name().clone(),
            nullable: field.is_nullable(),
            dictionary: matches!(field.data_type(), DataType::Dictionary(..)),
            state: match ColumnValues::for_type(field.data_type()) {
                Ok(values) => ColumnState::Loading(values),
                Err(reason) => ColumnState::Unloaded(reason),
            },
            validity: None,
            rows: 0,
            row_group_stats: None,
        })
        .chain(partition_keys.iter().map(|key| SourceColumn {
            name: key.clone(),
            nullable: false,
            dictionary: false,
            state: ColumnState::Loading(ColumnValues::String(Vec::new())),
            validity: None,
            rows: 0,
            row_group_stats: None,
        }))
        .collect();
    let file_columns = schema.fields().len();
    let loaded: Vec<usize> = columns[..file_columns]
        .iter()
        .enumerate()
        .filter(|(_, column)| matches!(column.state, ColumnState::Loading(_)))
        .map(|(index, _)| index)
        .collect();
    info!(
        path = %path.display(),
        files = sources.len(),
        rows = declared_rows,
        row_groups = sources.iter().map(|s| s.metadata.metadata().num_row_groups()).sum::<usize>(),
        columns = file_columns,
        loadable_columns = loaded.len(),
        partition_keys = ?partition_keys,
        "Parquet schema read from the footer"
    );

    // SPEC §5.1: affordability before acting, from every footer's exact
    // sizes, before a single page of any file is read.
    let estimated_bytes = sources
        .iter()
        .map(|source| {
            let rows = source.declared_rows();
            let labels: u64 = source
                .partition
                .iter()
                .map(|(_, value)| std::mem::size_of::<String>() as u64 + value.len() as u64)
                .sum();
            estimated_bytes(&columns[..file_columns], source.metadata.metadata(), rows)
                .saturating_add(rows.saturating_mul(labels))
        })
        .fold(0, u64::saturating_add);
    if !request.budget.affords(estimated_bytes) {
        warn!(
            estimated_bytes,
            cap_bytes = request.budget.cap_bytes(),
            "refusing to open a Parquet file whose columns do not fit the RAM budget (SPEC §5.1)"
        );
        return Err(GlydeError::BudgetExceeded {
            requested_bytes: estimated_bytes,
            cap_bytes: request.budget.cap_bytes(),
        });
    }
    info!(
        estimated_bytes,
        cap_bytes = request.budget.cap_bytes(),
        "opening the Parquet file in memory: its typed columns fit the RAM budget (SPEC §5.1)"
    );

    // The footers give the exact row count: reserve it once, rather than
    // letting each column's buffer double (and copy) its way there.
    let reserve_rows = usize::try_from(declared_rows).unwrap_or(0);
    for &index in &loaded {
        if let ColumnState::Loading(values) = &mut columns[index].state {
            values.reserve(reserve_rows);
        }
    }
    // Each column is decoded by its own reader, projected onto it alone, on
    // the `rayon` pool: decoding is CPU-bound, and the columns of a row group
    // are independent chunks of the file. Every column still reads the
    // sources in order, so its rows arrive in file order.
    let mut row_count = 0u64;
    for source in &sources {
        request.ensure_not_cancelled()?;
        let cancelled = request.cancelled;
        let mut targets: Vec<(usize, &mut SourceColumn)> = columns[..file_columns]
            .iter_mut()
            .enumerate()
            .filter(|(index, _)| loaded.contains(index))
            .collect();
        use rayon::prelude::*;
        targets
            .par_iter_mut()
            .try_for_each(|(index, column)| read_column(source, *index, column, cancelled))?;
        let rows = usize::try_from(source.declared_rows()).unwrap_or(usize::MAX);
        for (column, (_, value)) in columns[file_columns..].iter_mut().zip(&source.partition) {
            column.append_label(value, rows);
        }
        row_count += source.declared_rows();
    }
    request.ensure_not_cancelled()?;
    for column in &mut columns {
        column.settle_missing();
    }
    for (column, field) in columns.iter_mut().zip(schema.fields()) {
        if matches!(column.state, ColumnState::Loading(_)) {
            column.row_group_stats = row_group_stats(&sources, field);
        }
    }

    let (dataset, time_index) = build_dataset(&mut columns, row_count, request.overrides)?;
    let unloaded_columns = columns
        .into_iter()
        .filter_map(|column| match column.state {
            ColumnState::Unloaded(reason) => {
                warn!(
                    column = %column.name,
                    reason = %reason,
                    "Parquet column not loaded; listed in the inference bar"
                );
                Some(UnloadedColumn {
                    name: column.name,
                    reason,
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let facts = SourceFacts {
        column_names: schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .chain(partition_keys)
            .collect(),
        row_count,
        skipped_row_count: 0,
        skipped_row_details: Vec::new(),
        encoding: settled(NOT_APPLICABLE.to_string()),
        delimiter: settled(None),
        decimal_separator: settled(None),
        header_ambiguous: false,
        unloaded_columns,
    };
    build_summary_and_report(facts, dataset, time_index)
}

/// Appends every row of the file's column `index` (a root of its schema) to
/// `column`, in bounded record batches, checking `cancelled` between them.
fn read_column(
    source: &SourceFile,
    index: usize,
    column: &mut SourceColumn,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    let file = File::open(&source.path).map_err(|error| GlydeError::Io {
        path: source.path.clone(),
        source: error,
    })?;
    let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(file, source.metadata.clone());
    let projection = ProjectionMask::roots(builder.parquet_schema(), [index]);
    let batches = builder
        .with_projection(projection)
        .with_batch_size(BATCH_ROWS)
        .build()
        .map_err(|e| invalid(&source.path, e))?;
    let rows_before = column.rows;
    for batch in batches {
        super::dataset::ensure_not_cancelled(cancelled)?;
        let batch = batch.map_err(|e| invalid(&source.path, e))?;
        column.append(batch.column(0));
    }
    let read = (column.rows - rows_before) as u64;
    if read != source.declared_rows() {
        return Err(invalid(
            &source.path,
            format!(
                "its footer declares {} rows, but its column `{}` holds {read}",
                source.declared_rows(),
                column.name
            ),
        ));
    }
    Ok(())
}

/// One Parquet file of the source: the file itself, or one file of a
/// partitioned directory.
struct SourceFile {
    path: PathBuf,
    /// Its Hive partition keys and values (`date=2026-01-01/` gives
    /// `("date", "2026-01-01")`), outermost first. Empty for a single file.
    partition: Vec<(String, String)>,
    /// Its footer, read once and reused for the read itself.
    metadata: ArrowReaderMetadata,
}

impl SourceFile {
    fn declared_rows(&self) -> u64 {
        u64::try_from(self.metadata.metadata().file_metadata().num_rows()).unwrap_or(0)
    }
}

/// Whether a directory entry is one Spark and Hive leave out of a dataset:
/// `_SUCCESS`, `_metadata`, `_temporary/`, `.crc` checksums, hidden files.
fn is_ignored_entry(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with('_') || name.starts_with('.')
}

fn has_parquet_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("parquet"))
}

/// What a walk of a dataset directory found.
#[derive(Default)]
struct DirectoryListing {
    /// Every `.parquet` file, in path order.
    parquet_files: Vec<PathBuf>,
    /// Entries skipped by [`is_ignored_entry`] (a skipped directory counts
    /// once).
    ignored: usize,
    /// Files that are not Parquet (a stray CSV, a README).
    not_parquet: usize,
}

/// Walks `dir` at any depth. Symlinked directories are not followed, so a
/// link cycle cannot loop.
fn list_directory(dir: &Path) -> std::io::Result<DirectoryListing> {
    let mut listing = DirectoryListing::default();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            if is_ignored_entry(&entry.file_name()) {
                listing.ignored += 1;
            } else if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else if has_parquet_extension(&entry.path()) {
                listing.parquet_files.push(entry.path());
            } else {
                listing.not_parquet += 1;
            }
        }
    }
    // Path order, so `date=…` partitions arrive in date order.
    listing.parquet_files.sort();
    Ok(listing)
}

/// The Parquet files `path` stands for, each with its footer: `path` itself,
/// or every Parquet file of a dataset directory (docs/SPEC.md §1.1,
/// issue #159).
fn source_files(path: &Path) -> Result<Vec<SourceFile>> {
    let io_error = |source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    };
    let files = if path.is_dir() {
        let listing = list_directory(path).map_err(io_error)?;
        info!(
            path = %path.display(),
            parquet_files = listing.parquet_files.len(),
            ignored_entries = listing.ignored,
            "opening a Parquet dataset directory as one dataset, its files in path order; \
             entries named `_*` or `.*` are not data and are ignored, as Spark and Hive do"
        );
        if listing.not_parquet > 0 {
            warn!(
                path = %path.display(),
                files = listing.not_parquet,
                "files without a .parquet extension in the dataset directory are not read"
            );
        }
        if listing.parquet_files.is_empty() {
            return Err(invalid(path, "the folder holds no Parquet files"));
        }
        listing
            .parquet_files
            .into_iter()
            .map(|file| {
                let partition = hive_partition(path, &file);
                (file, partition)
            })
            .collect()
    } else {
        vec![(path.to_path_buf(), Vec::new())]
    };
    files
        .into_iter()
        .map(|(file_path, partition)| {
            let file = File::open(&file_path).map_err(|source| GlydeError::Io {
                path: file_path.clone(),
                source,
            })?;
            let metadata = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())
                .map_err(|e| invalid(&file_path, e))?;
            Ok(SourceFile {
                path: file_path,
                partition,
                metadata,
            })
        })
        .collect()
}

/// The exact per-row-group min/max of `field` across every source file, in
/// row order, for the pyramid (issue #162, `index::row_groups`), or `None`
/// (logged) when they cannot be trusted to equal a scan:
///
/// - Only integer columns: Parquet's float statistics leave NaN out without
///   counting it, and a bucket's `nan_count` must be exact.
/// - Statistics in the deprecated pre-2017 fields are not used: they were
///   written with signed ordering, wrong for unsigned columns.
/// - Statistics a writer marked inexact (truncated) are not used.
///
/// A row group with no statistics at all keeps `None` bounds; the pyramid
/// accepts that only for a group with no values.
fn row_group_stats(sources: &[SourceFile], field: &Field) -> Option<RowGroupStats> {
    let integer = matches!(
        field.data_type(),
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
    );
    if !integer {
        if matches!(field.data_type(), DataType::Float32 | DataType::Float64) {
            info!(
                column = %field.name(),
                "float column: its Parquet row-group statistics do not count NaN, so its \
                 pyramid is aggregated in full"
            );
        }
        return None;
    }
    let mut groups = Vec::new();
    for source in sources {
        match file_row_group_bounds(source, field) {
            Ok(bounds) => groups.extend(bounds),
            Err(reason) => {
                info!(
                    column = %field.name(),
                    file = %source.path.display(),
                    reason,
                    "Parquet row-group statistics not used for this column's pyramid"
                );
                return None;
            }
        }
    }
    if groups
        .iter()
        .all(|group| group.min.is_none() && group.max.is_none())
    {
        info!(
            column = %field.name(),
            "this Parquet column has no row-group statistics; its pyramid is aggregated in full"
        );
        return None;
    }
    Some(RowGroupStats::new(groups))
}

/// One file's row-group bounds for `field`, or why they cannot be used.
fn file_row_group_bounds(
    source: &SourceFile,
    field: &Field,
) -> std::result::Result<Vec<RowGroupBounds>, &'static str> {
    let metadata = source.metadata.metadata();
    let converter = StatisticsConverter::try_new(
        field.name(),
        source.metadata.schema(),
        source.metadata.parquet_schema(),
    )
    .map_err(|_| "the column's statistics could not be located")?;
    let leaf = converter
        .parquet_column_index()
        .ok_or("the column has no Parquet leaf to read statistics from")?;
    for row_group in metadata.row_groups() {
        if let Some(statistics) = row_group.column(leaf).statistics() {
            if statistics.is_min_max_deprecated() {
                return Err("its statistics use the deprecated pre-2017 fields");
            }
            if !statistics.min_is_exact() || !statistics.max_is_exact() {
                return Err("its statistics are marked inexact");
            }
        }
    }
    let unreadable = "its statistics could not be decoded";
    let mins = converter
        .row_group_mins(metadata.row_groups())
        .map_err(|_| unreadable)?;
    let maxes = converter
        .row_group_maxes(metadata.row_groups())
        .map_err(|_| unreadable)?;
    let (mins, maxes) = (integers_as_f64(&mins), integers_as_f64(&maxes));
    let (Some(mins), Some(maxes)) = (mins, maxes) else {
        return Err(unreadable);
    };
    Ok(metadata
        .row_groups()
        .iter()
        .zip(mins.into_iter().zip(maxes))
        .map(|(row_group, (min, max))| RowGroupBounds {
            rows: usize::try_from(row_group.num_rows()).unwrap_or(0),
            min,
            max,
        })
        .collect())
}

/// An integer statistics array as the `f64` the pyramid holds, converted
/// exactly as the column's samples are (`as f64`, monotonic, so the min of
/// the converted values is the converted min).
fn integers_as_f64(array: &ArrayRef) -> Option<Vec<Option<f64>>> {
    fn convert<T: arrow::datatypes::ArrowPrimitiveType>(
        array: &ArrayRef,
        to_f64: impl Fn(T::Native) -> f64,
    ) -> Vec<Option<f64>> {
        array
            .as_primitive::<T>()
            .iter()
            .map(|v| v.map(&to_f64))
            .collect()
    }
    Some(match array.data_type() {
        DataType::Int8 => convert::<Int8Type>(array, f64::from),
        DataType::Int16 => convert::<Int16Type>(array, f64::from),
        DataType::Int32 => convert::<Int32Type>(array, f64::from),
        DataType::Int64 => convert::<Int64Type>(array, |n| n as f64),
        DataType::UInt8 => convert::<UInt8Type>(array, f64::from),
        DataType::UInt16 => convert::<UInt16Type>(array, f64::from),
        DataType::UInt32 => convert::<UInt32Type>(array, f64::from),
        DataType::UInt64 => convert::<UInt64Type>(array, |n| n as f64),
        _ => return None,
    })
}

/// The value Hive writes for a null partition value.
const HIVE_NULL_PARTITION: &str = "__HIVE_DEFAULT_PARTITION__";

/// The `key=value` directories between `root` and `file`, outermost first.
/// Values are percent-decoded as Hive escapes them (`10%3A00` is `10:00`),
/// and Hive's null partition becomes an empty label, as a null text value
/// does.
fn hive_partition(root: &Path, file: &Path) -> Vec<(String, String)> {
    let Some(parent) = file.parent() else {
        return Vec::new();
    };
    let Ok(relative) = parent.strip_prefix(root) else {
        return Vec::new();
    };
    relative
        .components()
        .filter_map(|component| {
            let component = component.as_os_str().to_str()?;
            let (key, value) = component.split_once('=')?;
            if key.is_empty() {
                return None;
            }
            let value = if value == HIVE_NULL_PARTITION {
                String::new()
            } else {
                percent_decode(value)
            };
            Some((percent_decode(key), value))
        })
        .collect()
}

/// `text` with every `%XX` escape decoded, or `text` unchanged when the
/// escapes do not decode to UTF-8 (the directory name is then the truest
/// label there is).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escape = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escape {
            Some(byte) => {
                decoded.push(byte);
                i += 3;
            }
            None => {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| text.to_string())
}

/// SPEC §1.1's directory is one dataset only when every file agrees: the
/// same columns of the same types, and the same partition keys. A file that
/// differs refuses the open, naming it — there is no guessed merge.
fn check_sources_agree(root: &Path, sources: &[SourceFile]) -> Result<()> {
    let Some((first, rest)) = sources.split_first() else {
        return Ok(());
    };
    let name = |source: &SourceFile| {
        source
            .path
            .strip_prefix(root)
            .unwrap_or(&source.path)
            .display()
            .to_string()
    };
    let keys = |source: &SourceFile| -> Vec<String> {
        source
            .partition
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    };
    let schema = first.metadata.schema();
    for key in keys(first) {
        if schema.field_with_name(&key).is_ok() {
            return Err(invalid(
                root,
                format!(
                    "its partition key `{key}` is also a column of `{}`, so Glyde cannot tell \
                     which one the dataset means",
                    name(first)
                ),
            ));
        }
    }
    for source in rest {
        if keys(source) != keys(first) {
            return Err(invalid(
                root,
                format!(
                    "`{}` has the partition keys [{}], but `{}` has [{}]; every file of a \
                     partitioned folder must have the same ones",
                    name(source),
                    keys(source).join(", "),
                    name(first),
                    keys(first).join(", ")
                ),
            ));
        }
        if let Some(difference) = schema_difference(source.metadata.schema(), schema) {
            return Err(invalid(
                root,
                format!(
                    "`{}` has a different schema from `{}` ({difference}); every file of the \
                     folder must have the same columns of the same types",
                    name(source),
                    name(first)
                ),
            ));
        }
    }
    Ok(())
}

/// How `schema` differs from `reference` in column names or types, if it
/// does. Nullability is not compared: whether a writer allowed nulls does not
/// change a value.
fn schema_difference(schema: &Schema, reference: &Schema) -> Option<String> {
    let describe =
        |field: &arrow::datatypes::Field| format!("`{}` ({})", field.name(), field.data_type());
    if schema.fields().len() != reference.fields().len() {
        return Some(format!(
            "{} columns instead of {}",
            schema.fields().len(),
            reference.fields().len()
        ));
    }
    schema
        .fields()
        .iter()
        .zip(reference.fields())
        .enumerate()
        .find(|(_, (field, other))| {
            field.name() != other.name() || field.data_type() != other.data_type()
        })
        .map(|(index, (field, other))| {
            format!(
                "column {} is {} instead of {}",
                index + 1,
                describe(field),
                describe(other)
            )
        })
}

/// A field Parquet's own schema settles: there was nothing to guess.
fn settled<T>(value: T) -> InferredField<T> {
    InferredField {
        value,
        confidence: Confidence::High,
    }
}

/// The text-format corrections have no meaning for a typed file; say so
/// rather than dropping them without a trace. (A timestamp-format
/// correction does apply, to a time column of numbers or text: see
/// [`time_axis_from_text`].)
fn log_ignored_overrides(overrides: IngestOverrides) {
    if overrides.delimiter.is_some()
        || overrides.decimal_separator.is_some()
        || overrides.encoding.is_some()
    {
        info!(
            "delimiter, decimal separator and encoding corrections do not apply to a Parquet \
             file; ignored"
        );
    }
}

/// Bytes per row budgeted for the text of a dictionary-encoded column's
/// labels. Its uncompressed size in the footer is the dictionary plus the
/// keys, far smaller than the decoded column, where every row holds its own
/// copy of its label. The footer cannot say how long the labels are, so a
/// state name's typical length is budgeted, with room to spare.
const DICTIONARY_LABEL_BYTES: u64 = 32;

/// What materializing every loadable column costs, from the footer: each
/// fixed-width column at its element width, a string column at its
/// uncompressed bytes plus one `String` per row (a dictionary column at
/// [`DICTIONARY_LABEL_BYTES`] per row on top), one validity byte per row for
/// a column that may hold nulls, and the time axis at one [`Timestamp`] per
/// row whichever column becomes it. A column whose nulls promote it to `f64`
/// (see [`SourceColumn::settle_missing`]) briefly holds both widths, so its
/// `f64` copy is budgeted too.
fn estimated_bytes(columns: &[SourceColumn], metadata: &ParquetMetaData, rows: u64) -> u64 {
    let schema = metadata.file_metadata().schema_descr();
    let mut text_bytes = vec![0u64; columns.len()];
    for row_group in metadata.row_groups() {
        for (leaf, chunk) in row_group.columns().iter().enumerate() {
            let root = schema.get_column_root_idx(leaf);
            if let Some(bytes) = text_bytes.get_mut(root) {
                *bytes += u64::try_from(chunk.uncompressed_size()).unwrap_or(0);
            }
        }
    }
    let axis = rows * std::mem::size_of::<Timestamp>() as u64;
    columns
        .iter()
        .zip(text_bytes)
        .map(|(column, text)| {
            let ColumnState::Loading(values) = &column.state else {
                return 0;
            };
            let values_bytes = match values {
                ColumnValues::String(_) if column.dictionary => {
                    text + rows * (std::mem::size_of::<String>() as u64 + DICTIONARY_LABEL_BYTES)
                }
                ColumnValues::String(_) => text + rows * std::mem::size_of::<String>() as u64,
                values if column.nullable && values.is_numeric() => {
                    rows * (values.element_bytes() + 8)
                }
                values => rows * values.element_bytes(),
            };
            values_bytes + if column.nullable { rows } else { 0 }
        })
        .fold(axis, u64::saturating_add)
}

/// One top-level column of the file's schema, in schema order.
struct SourceColumn {
    name: String,
    /// Whether the schema allows nulls in it (sizes the budget estimate).
    nullable: bool,
    /// Whether its values are dictionary-encoded (sizes the budget estimate).
    dictionary: bool,
    state: ColumnState,
    /// `false` at every row that is null, created at the first null seen —
    /// `None` for a column without any (the common case costs nothing).
    validity: Option<Vec<bool>>,
    /// Rows appended so far.
    rows: usize,
    /// Its exact row-group min/max, for the pyramid (issue #162): an integer
    /// column's only.
    row_group_stats: Option<RowGroupStats>,
}

enum ColumnState {
    /// Being read, or read.
    Loading(ColumnValues),
    /// Read, and moved into the dataset (as the time axis or a series).
    Taken,
    /// Not in the dataset, with the plain-language reason the inference bar
    /// shows.
    Unloaded(String),
}

impl SourceColumn {
    fn append(&mut self, array: &ArrayRef) {
        let ColumnState::Loading(values) = &mut self.state else {
            return;
        };
        // Logical nulls: a dictionary column is null where its key is, and
        // where the label its key points at is.
        match array.logical_nulls().filter(|nulls| nulls.null_count() > 0) {
            Some(nulls) => {
                let rows = self.rows;
                let validity = self.validity.get_or_insert_with(|| vec![true; rows]);
                validity.extend(nulls.iter());
            }
            None => {
                if let Some(validity) = &mut self.validity {
                    validity.resize(self.rows + array.len(), true);
                }
            }
        }
        self.rows += array.len();
        // The value buffer under a null holds whatever the writer left there;
        // `settle_missing` overwrites it before anything reads it (Golden
        // Rule 1: it is not a sample).
        values.append(array);
    }

    /// Appends `rows` copies of a partition key's value: every row of one
    /// file of a partitioned directory has its directory's value.
    fn append_label(&mut self, label: &str, rows: usize) {
        if let ColumnState::Loading(ColumnValues::String(values)) = &mut self.state {
            values.extend(std::iter::repeat_n(label, rows).map(str::to_string));
            self.rows += rows;
        }
    }

    /// The first row with no value, if any.
    fn first_missing_row(&self) -> Option<usize> {
        self.validity.as_ref()?.iter().position(|valid| !valid)
    }

    /// Turns every null into SPEC §1.3's gap, in the column's own dtype when
    /// it has one (docs/ROADMAP.md M7, issue #157):
    ///
    /// - `float`/`double`: NaN, which the series flags as a NaN run.
    /// - Integers: no integer is a gap, so the column becomes `f64` with NaN
    ///   gaps — only when every value converts exactly (SPEC §1.4: never
    ///   upcast with precision loss). An `int64`/`uint64` beyond ±2⁵³ is not
    ///   loaded instead, with the reason.
    /// - Text: an empty label, which is what a blank cell is in a CSV text
    ///   column (and what the state timeline already shows for one).
    /// - Booleans: text `true`/`false`/empty, which is what a CSV boolean
    ///   column with blank cells becomes.
    /// - Timestamps: left as they are; a timestamp column with nulls cannot
    ///   be the time axis ([`build_dataset`] reports it), and is not a series.
    fn settle_missing(&mut self) {
        let Some(validity) = self.validity.take() else {
            return;
        };
        let missing = validity.iter().filter(|valid| !**valid).count();
        let ColumnState::Loading(values) = &mut self.state else {
            return;
        };
        let promoted = match values {
            ColumnValues::F32(v) => {
                mark_missing(v, &validity, f32::NAN);
                None
            }
            ColumnValues::F64(v) => {
                mark_missing(v, &validity, f64::NAN);
                None
            }
            ColumnValues::String(v) => {
                mark_missing(v, &validity, String::new());
                None
            }
            ColumnValues::Bool(v) => Some(Ok(ColumnValues::String(
                v.iter()
                    .zip(&validity)
                    .map(|(&value, &valid)| match (valid, value) {
                        (false, _) => String::new(),
                        (true, true) => "true".to_string(),
                        (true, false) => "false".to_string(),
                    })
                    .collect(),
            ))),
            ColumnValues::Timestamp { .. } => {
                self.validity = Some(validity);
                return;
            }
            numeric => Some(exact_f64_with_gaps(numeric, &validity).map(ColumnValues::F64)),
        };
        match promoted {
            None => info!(
                column = %self.name,
                missing,
                "Parquet nulls kept as gaps in the column's own dtype (SPEC §1.3)"
            ),
            Some(Ok(values)) => {
                info!(
                    column = %self.name,
                    missing,
                    to = if matches!(values, ColumnValues::F64(_)) { "f64 with NaN gaps" } else { "text" },
                    "Parquet column with nulls converted so its gaps can be shown (SPEC §1.3); \
                     every present value is unchanged"
                );
                self.state = ColumnState::Loading(values);
            }
            Some(Err(reason)) => self.state = ColumnState::Unloaded(reason),
        }
    }

    /// The typed values, moving them out of this column.
    fn take(&mut self) -> Option<ColumnValues> {
        match std::mem::replace(&mut self.state, ColumnState::Taken) {
            ColumnState::Loading(values) => Some(values),
            other => {
                self.state = other;
                None
            }
        }
    }
}

/// A column's typed values as they accumulate across record batches — the
/// SPEC §1.4 dtype the Arrow type maps to, plus a timestamp column's own
/// unit and timezone.
enum ColumnValues {
    Bool(Vec<bool>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    String(Vec<String>),
    Timestamp {
        unit: TimeUnit,
        zone: Zone,
        ticks: Vec<i64>,
    },
}

impl ColumnValues {
    /// The empty accumulator for an Arrow type, or why the type cannot be
    /// loaded.
    fn for_type(data_type: &DataType) -> std::result::Result<Self, String> {
        Ok(match data_type {
            DataType::Boolean => ColumnValues::Bool(Vec::new()),
            DataType::Int8 => ColumnValues::I8(Vec::new()),
            DataType::Int16 => ColumnValues::I16(Vec::new()),
            DataType::Int32 => ColumnValues::I32(Vec::new()),
            DataType::Int64 => ColumnValues::I64(Vec::new()),
            DataType::UInt8 => ColumnValues::U8(Vec::new()),
            DataType::UInt16 => ColumnValues::U16(Vec::new()),
            DataType::UInt32 => ColumnValues::U32(Vec::new()),
            DataType::UInt64 => ColumnValues::U64(Vec::new()),
            DataType::Float32 => ColumnValues::F32(Vec::new()),
            DataType::Float64 => ColumnValues::F64(Vec::new()),
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => {
                ColumnValues::String(Vec::new())
            }
            DataType::Timestamp(unit, timezone) => ColumnValues::Timestamp {
                unit: match unit {
                    ArrowTimeUnit::Second => TimeUnit::Seconds,
                    ArrowTimeUnit::Millisecond => TimeUnit::Milliseconds,
                    ArrowTimeUnit::Microsecond => TimeUnit::Microseconds,
                    ArrowTimeUnit::Nanosecond => TimeUnit::Nanoseconds,
                },
                zone: Zone::from_arrow(timezone.as_deref()),
                ticks: Vec::new(),
            },
            // A categorical column (SPEC §1.4): decoded to one label per row,
            // the same string series a CSV text column is.
            DataType::Dictionary(_, labels)
                if matches!(labels.as_ref(), DataType::Utf8 | DataType::LargeUtf8) =>
            {
                ColumnValues::String(Vec::new())
            }
            other => return Err(format!("its type ({other}) is not one Glyde can plot")),
        })
    }

    /// Makes room for `rows` more rows (the budget check already covered
    /// them).
    fn reserve(&mut self, rows: usize) {
        match self {
            ColumnValues::Bool(v) => v.reserve(rows),
            ColumnValues::I8(v) => v.reserve(rows),
            ColumnValues::I16(v) => v.reserve(rows),
            ColumnValues::I32(v) => v.reserve(rows),
            ColumnValues::I64(v) => v.reserve(rows),
            ColumnValues::U8(v) => v.reserve(rows),
            ColumnValues::U16(v) => v.reserve(rows),
            ColumnValues::U32(v) => v.reserve(rows),
            ColumnValues::U64(v) => v.reserve(rows),
            ColumnValues::F32(v) => v.reserve(rows),
            ColumnValues::F64(v) => v.reserve(rows),
            ColumnValues::String(v) => v.reserve(rows),
            ColumnValues::Timestamp { ticks, .. } => ticks.reserve(rows),
        }
    }

    /// Bytes one row of this column costs in memory (a string column's text
    /// is sized separately, see [`estimated_bytes`]).
    fn element_bytes(&self) -> u64 {
        match self {
            ColumnValues::Bool(_) | ColumnValues::I8(_) | ColumnValues::U8(_) => 1,
            ColumnValues::I16(_) | ColumnValues::U16(_) => 2,
            ColumnValues::I32(_) | ColumnValues::U32(_) | ColumnValues::F32(_) => 4,
            ColumnValues::I64(_) | ColumnValues::U64(_) | ColumnValues::F64(_) => 8,
            ColumnValues::String(_) => std::mem::size_of::<String>() as u64,
            ColumnValues::Timestamp { .. } => 8,
        }
    }

    /// Appends one batch's array, which has this column's type (the reader
    /// decodes every batch against the one footer schema). What lands at a
    /// null row is a placeholder until [`SourceColumn::settle_missing`].
    fn append(&mut self, array: &ArrayRef) {
        match self {
            ColumnValues::Bool(values) => values.extend(array.as_boolean().values().iter()),
            ColumnValues::I8(values) => values.extend(array.as_primitive::<Int8Type>().values()),
            ColumnValues::I16(values) => values.extend(array.as_primitive::<Int16Type>().values()),
            ColumnValues::I32(values) => values.extend(array.as_primitive::<Int32Type>().values()),
            ColumnValues::I64(values) => values.extend(array.as_primitive::<Int64Type>().values()),
            ColumnValues::U8(values) => values.extend(array.as_primitive::<UInt8Type>().values()),
            ColumnValues::U16(values) => values.extend(array.as_primitive::<UInt16Type>().values()),
            ColumnValues::U32(values) => values.extend(array.as_primitive::<UInt32Type>().values()),
            ColumnValues::U64(values) => values.extend(array.as_primitive::<UInt64Type>().values()),
            ColumnValues::F32(values) => {
                values.extend(array.as_primitive::<Float32Type>().values())
            }
            ColumnValues::F64(values) => {
                values.extend(array.as_primitive::<Float64Type>().values())
            }
            ColumnValues::String(values) => match array.data_type() {
                DataType::Dictionary(..) => append_dictionary_labels(values, array),
                DataType::LargeUtf8 => values.extend(
                    array
                        .as_string::<i64>()
                        .iter()
                        .map(|value| value.unwrap_or_default().to_string()),
                ),
                DataType::Utf8View => values.extend(
                    array
                        .as_string_view()
                        .iter()
                        .map(|value| value.unwrap_or_default().to_string()),
                ),
                _ => values.extend(
                    array
                        .as_string::<i32>()
                        .iter()
                        .map(|value| value.unwrap_or_default().to_string()),
                ),
            },
            ColumnValues::Timestamp { ticks, .. } => match array.data_type() {
                DataType::Timestamp(ArrowTimeUnit::Second, _) => {
                    ticks.extend(array.as_primitive::<TimestampSecondType>().values())
                }
                DataType::Timestamp(ArrowTimeUnit::Millisecond, _) => {
                    ticks.extend(array.as_primitive::<TimestampMillisecondType>().values())
                }
                DataType::Timestamp(ArrowTimeUnit::Microsecond, _) => {
                    ticks.extend(array.as_primitive::<TimestampMicrosecondType>().values())
                }
                _ => ticks.extend(array.as_primitive::<TimestampNanosecondType>().values()),
            },
        }
    }

    /// The SPEC §1.4 series values, or `None` for a timestamp column, which
    /// only exists as a time axis.
    fn into_series_values(self) -> Option<SeriesValues> {
        Some(match self {
            ColumnValues::Bool(v) => SeriesValues::Bool(v),
            ColumnValues::I8(v) => SeriesValues::I8(v),
            ColumnValues::I16(v) => SeriesValues::I16(v),
            ColumnValues::I32(v) => SeriesValues::I32(v),
            ColumnValues::I64(v) => SeriesValues::I64(v),
            ColumnValues::U8(v) => SeriesValues::U8(v),
            ColumnValues::U16(v) => SeriesValues::U16(v),
            ColumnValues::U32(v) => SeriesValues::U32(v),
            ColumnValues::U64(v) => SeriesValues::U64(v),
            ColumnValues::F32(v) => SeriesValues::F32(v),
            ColumnValues::F64(v) => SeriesValues::F64(v),
            ColumnValues::String(v) => SeriesValues::String(v),
            ColumnValues::Timestamp { .. } => return None,
        })
    }
}

/// Overwrites every null row's placeholder with `gap`.
fn mark_missing<T: Clone>(values: &mut [T], validity: &[bool], gap: T) {
    for (value, &valid) in values.iter_mut().zip(validity) {
        if !valid {
            *value = gap.clone();
        }
    }
}

/// The largest integer magnitude every smaller one of which an `f64` holds
/// exactly (2⁵³).
const MAX_EXACT_F64_INTEGER: u64 = 1 << 53;

/// An integer column with nulls as `f64` with NaN gaps, or why it cannot be:
/// a present value beyond ±2⁵³ would be rounded, and SPEC §1.4 forbids an
/// upcast that loses precision.
fn exact_f64_with_gaps(
    values: &ColumnValues,
    validity: &[bool],
) -> std::result::Result<Vec<f64>, String> {
    let exact = |magnitude: u64| magnitude <= MAX_EXACT_F64_INTEGER;
    let fits = match values {
        ColumnValues::I64(v) => v
            .iter()
            .zip(validity)
            .all(|(n, &valid)| !valid || exact(n.unsigned_abs())),
        ColumnValues::U64(v) => v
            .iter()
            .zip(validity)
            .all(|(&n, &valid)| !valid || exact(n)),
        _ => true,
    };
    if !fits {
        return Err(
            "it has missing values, and its integers are too large to show as decimals exactly \
             (beyond ±2^53), so Glyde cannot mark the gaps without changing a value"
                .to_string(),
        );
    }
    let mut converted = values.to_f64();
    mark_missing(&mut converted, validity, f64::NAN);
    Ok(converted)
}

/// Decodes a dictionary-encoded UTF-8 batch to one label per row: the
/// categorical column as the plain text column the rest of Glyde reads.
fn append_dictionary_labels(values: &mut Vec<String>, array: &ArrayRef) {
    let dictionary = array.as_any_dictionary();
    let labels = dictionary.values();
    let label = |key: usize| -> String {
        if key >= labels.len() || labels.is_null(key) {
            return String::new();
        }
        match labels.data_type() {
            DataType::LargeUtf8 => labels.as_string::<i64>().value(key).to_string(),
            _ => labels.as_string::<i32>().value(key).to_string(),
        }
    };
    values.extend(
        dictionary
            .normalized_keys()
            .into_iter()
            .enumerate()
            .map(|(row, key)| {
                if array.is_null(row) {
                    String::new()
                } else {
                    label(key)
                }
            }),
    );
}

/// A timestamp column's timezone (SPEC §2.1: "if the source carries one,
/// honor it and display it. If not, treat as naive local time").
#[derive(Debug, Clone, Copy, PartialEq)]
enum Zone {
    /// No timezone: the values are wall-clock time, naive local.
    Naive,
    /// A fixed UTC offset in seconds (`"UTC"`, `"+02:00"`, …).
    Fixed(i32),
    /// An IANA zone: the offset is resolved per row, so a column crossing a
    /// DST transition keeps each row's own offset.
    Named(chrono_tz::Tz),
}

impl Zone {
    fn from_arrow(timezone: Option<&str>) -> Self {
        let Some(timezone) = timezone else {
            return Zone::Naive;
        };
        if let Some(offset) = parse_fixed_offset(timezone) {
            return Zone::Fixed(offset);
        }
        match timezone.parse::<chrono_tz::Tz>() {
            Ok(zone) => Zone::Named(zone),
            Err(_) => {
                // Arrow stores a timezone-aware column as UTC instants, so the
                // instants themselves are still exact; only the local
                // redisplay is unknown. Showing them in UTC is true, inventing
                // a zone would not be.
                warn!(
                    timezone,
                    "unrecognized Parquet timestamp timezone; showing its instants in UTC"
                );
                Zone::Fixed(0)
            }
        }
    }

    /// The UTC offset of the instant `ticks` (in `unit` since the Unix epoch).
    fn offset_seconds(self, ticks: i64, unit: TimeUnit) -> Option<i32> {
        use chrono::{Offset, TimeZone};
        match self {
            Zone::Naive => None,
            Zone::Fixed(offset) => Some(offset),
            Zone::Named(zone) => {
                let per_second = unit.ticks_per_second() as i64;
                let seconds = ticks.div_euclid(per_second);
                let instant = chrono::DateTime::from_timestamp(seconds, 0)?;
                Some(
                    zone.offset_from_utc_datetime(&instant.naive_utc())
                        .fix()
                        .local_minus_utc(),
                )
            }
        }
    }
}

/// `"UTC"`, `"Z"`, `"+HH:MM"`, `"-HHMM"`, `"+HH"` as seconds east of UTC.
fn parse_fixed_offset(timezone: &str) -> Option<i32> {
    if timezone.eq_ignore_ascii_case("utc") || timezone == "Z" {
        return Some(0);
    }
    let (sign, rest) = match timezone.as_bytes().first()? {
        b'+' => (1, &timezone[1..]),
        b'-' => (-1, &timezone[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (hours, minutes) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0),
        4 => (
            digits[..2].parse::<i32>().ok()?,
            digits[2..].parse::<i32>().ok()?,
        ),
        _ => return None,
    };
    (hours < 24 && minutes < 60).then_some(sign * (hours * 3600 + minutes * 60))
}

/// Whether `ticks` of `unit` since the Unix epoch is an instant `chrono` can
/// display (within about ±262,000 years).
fn displayable(ticks: i128, unit: TimeUnit) -> bool {
    i64::try_from(ticks.div_euclid(unit.ticks_per_second()))
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .is_some()
}

/// The time axis from a timestamp column's ticks, or why it cannot be one:
/// an instant outside the calendar range `chrono` can display (beyond about
/// ±262,000 years) is not a time Glyde can place on an axis, and every
/// formatter downstream relies on an axis timestamp being displayable.
fn absolute_axis(
    unit: TimeUnit,
    zone: Zone,
    ticks: Vec<i64>,
) -> std::result::Result<TimeAxis, String> {
    let displayable = |tick: i64| displayable(i128::from(tick), unit);
    // The displayable instants are one contiguous range, so checking the
    // extremes checks every row; the row is only looked for to name it.
    let (min, max) = ticks.iter().fold((i64::MAX, i64::MIN), |(min, max), &t| {
        (min.min(t), max.max(t))
    });
    if !ticks.is_empty() && !(displayable(min) && displayable(max)) {
        let row = ticks
            .iter()
            .position(|&tick| !displayable(tick))
            .unwrap_or_default();
        return Err(format!(
            "its timestamp on row {row} is outside the dates Glyde can display"
        ));
    }
    let timestamps = match zone {
        Zone::Naive => ticks
            .into_iter()
            .map(|tick| Timestamp::new(i128::from(tick), unit))
            .collect(),
        Zone::Fixed(offset) => ticks
            .into_iter()
            .map(|tick| Timestamp::with_offset(i128::from(tick), unit, offset))
            .collect(),
        Zone::Named(_) => ticks
            .into_iter()
            .map(|tick| match zone.offset_seconds(tick, unit) {
                Some(offset) => Timestamp::with_offset(i128::from(tick), unit, offset),
                None => Timestamp::new(i128::from(tick), unit),
            })
            .collect(),
    };
    Ok(TimeAxis::Absolute {
        timestamps: Timestamps::Memory(timestamps),
        format: TimestampFormat::ParquetTimestamp,
    })
}

/// Chooses the time index (see the module docs) and assembles the dataset
/// from every loaded column. A timestamp column that is not the time index
/// is moved to the unloaded list: v1 has no series dtype for it.
fn build_dataset(
    columns: &mut [SourceColumn],
    row_count: u64,
    overrides: IngestOverrides,
) -> Result<(Dataset, TimeIndexInference)> {
    let is_timestamp = |column: &SourceColumn| {
        matches!(
            column.state,
            ColumnState::Loading(ColumnValues::Timestamp { .. })
        )
    };
    let automatic = || {
        columns
            .iter()
            .position(is_timestamp)
            .or((!columns.is_empty()).then_some(0))
    };
    let (candidate, check_monotonic) = match overrides.time_column {
        None => (automatic(), true),
        Some(TimeColumnChoice::RowIndex) => (None, true),
        Some(TimeColumnChoice::Column(index)) if index < columns.len() => (Some(index), false),
        Some(TimeColumnChoice::Column(index)) => {
            warn!(
                picked_column = index,
                column_count = columns.len(),
                "the picked time column does not exist in this file; detecting the time column \
                 automatically instead"
            );
            (automatic(), true)
        }
    };

    let mut time = None;
    let mut timestamp_format_ambiguous = false;
    let mut generated = match candidate {
        None => Some(GeneratedIndexReason::Requested),
        Some(_) => None,
    };
    if let Some(index) = candidate {
        let name = columns[index].name.clone();
        match &columns[index].state {
            ColumnState::Loading(ColumnValues::Timestamp { .. })
                if columns[index].first_missing_row().is_some() =>
            {
                let row = columns[index].first_missing_row().unwrap_or_default() as u64;
                columns[index].state = ColumnState::Unloaded(format!(
                    "it has no timestamp on row {row}, so it cannot place rows in time"
                ));
                generated = Some(GeneratedIndexReason::MissingTimestamps { column: name, row });
            }
            ColumnState::Loading(ColumnValues::Timestamp { .. }) => {
                if overrides.timestamp_format.is_some() {
                    info!(
                        time_column = %name,
                        "the timestamp format correction does not apply to a typed Parquet \
                         timestamp column, whose type settles its format; ignored"
                    );
                }
                if let Some(ColumnValues::Timestamp { unit, zone, ticks }) = columns[index].take() {
                    match absolute_axis(unit, zone, ticks) {
                        Ok(axis) => time = Some(axis),
                        Err(reason) => {
                            columns[index].state = ColumnState::Unloaded(reason);
                            generated = Some(GeneratedIndexReason::Unreadable { column: name });
                        }
                    }
                }
            }
            ColumnState::Loading(values) if values.is_numeric() || values.is_text() => {
                match time_axis_from_text(
                    values,
                    &name,
                    overrides.timestamp_format,
                    check_monotonic,
                ) {
                    Ok((axis, ambiguous)) => {
                        columns[index].take();
                        time = Some(axis);
                        timestamp_format_ambiguous = ambiguous;
                    }
                    Err(reason) => generated = Some(reason),
                }
            }
            _ => generated = Some(GeneratedIndexReason::Unreadable { column: name }),
        }
    }
    let time_column = if generated.is_none() { candidate } else { None };
    let time = time.unwrap_or_else(|| row_ordinal_axis(row_count as usize));

    let mut pending = Vec::new();
    for column in columns.iter_mut() {
        if is_timestamp(column) {
            column.state = ColumnState::Unloaded(
                "it is a second timestamp column; pick it as the time column to plot against it"
                    .to_string(),
            );
            continue;
        }
        if let Some(values) = column.take().and_then(ColumnValues::into_series_values) {
            pending.push((column.name.clone(), values, column.row_group_stats.take()));
        }
    }
    // Each series' anomaly scan is independent: on the `rayon` pool, in
    // column order.
    use rayon::prelude::*;
    let series: Vec<_> = pending
        .into_par_iter()
        .map(|(name, values, stats)| {
            let built = series_with_anomalies(name, values, true);
            match stats {
                Some(stats) => built.with_row_group_stats(stats),
                None => built,
            }
        })
        .collect();
    if series.is_empty() {
        return Err(GlydeError::SingleColumnFile);
    }

    let time_column_name = match time_column {
        Some(index) => columns[index].name.clone(),
        None => super::GENERATED_TIME_COLUMN_NAME.to_string(),
    };
    let inference = TimeIndexInference {
        timestamp_format_ambiguous,
        time_column,
        generated,
    };
    log_time_index_decision(&time_column_name, &inference, row_count as usize);
    Ok((
        assemble_dataset(time, time_column_name, series, overrides),
        inference,
    ))
}

/// SPEC §2.1's decision for a time column of numbers or text, made exactly
/// as the CSV path (`dataset::TimeAxisBuilder`) makes it for a column
/// holding the same values as text (issue #166): the same
/// [`TimestampFormatScan`] (or the user's format correction) chooses the
/// format, the same [`TimeCandidateScan`] verdict decides whether the
/// column is a time index at all, and [`parse_timestamp`] reads each value.
/// A test opens every case both ways and asserts the same decision.
///
/// A number is read as the text [`ColumnValues::for_each_field`] gives it,
/// its shortest exact decimal form: a whole-valued float reads as the
/// integer it equals. CSV text can carry a `.0` that the scan takes as
/// evidence of a LabVIEW timestamp; a typed float carries no such text, so
/// an `INT64` and a `DOUBLE` column of the same values reach the same
/// decision.
///
/// Returns the axis and whether its format is SPEC §2.1's low-confidence
/// pick, or why the column is not a time index. A format correction the
/// column cannot be read in falls back to the row index with the reason
/// rather than failing the open (SPEC §1.3 "never abort the load").
fn time_axis_from_text(
    values: &ColumnValues,
    column: &str,
    format_override: Option<TimestampFormat>,
    check_monotonic: bool,
) -> std::result::Result<(TimeAxis, bool), GeneratedIndexReason> {
    let unreadable = || GeneratedIndexReason::Unreadable {
        column: column.to_string(),
    };
    let mut candidate = TimeCandidateScan::default();
    let mut scan = TimestampFormatScan::default();
    // A number's own value is the exact one; parsing its text back would
    // only round-trip it.
    let as_f64 = values.is_numeric().then(|| values.to_f64());
    if let Some(numbers) = &as_f64 {
        for &value in numbers {
            candidate.observe_value(value);
        }
    }
    if format_override.is_none() || as_f64.is_none() {
        values.for_each_field(|field| {
            if format_override.is_none() {
                scan.observe(field);
            }
            if as_f64.is_none() {
                candidate.observe(field);
            }
        });
    }
    let inference = match format_override {
        Some(format) => Some(TimestampFormatInference {
            format,
            ambiguous: false,
        }),
        None => scan.finish(),
    };
    if let Some(reason) = candidate.verdict(inference.is_some(), check_monotonic, column) {
        return Err(reason);
    }

    let Some(inference) = inference else {
        // Every value is a number (the verdict saw to that) and none is a
        // timestamp: SPEC §2.1's progressive index.
        let progressive = match as_f64 {
            Some(numbers) => numbers,
            None => {
                let mut numbers = Vec::new();
                let mut failed = false;
                values.for_each_field(|field| match parse_progressive_value(field) {
                    Ok(value) => numbers.push(value),
                    Err(_) => failed = true,
                });
                if failed {
                    return Err(unreadable());
                }
                numbers
            }
        };
        return Ok((
            TimeAxis::Progressive {
                values: ProgressiveValues::Memory(progressive),
            },
            false,
        ));
    };

    // The `f64` copy only ever becomes a progressive axis: free it before
    // the timestamps are built, so the open never holds both (PR #167
    // review).
    drop(as_f64);
    let mut timestamps = Vec::with_capacity(values.len());
    let mut first_failure = None;
    values.for_each_field(|field| {
        if first_failure.is_some() {
            return;
        }
        match parse_timestamp(field, inference.format) {
            Ok(timestamp) if displayable(timestamp.ticks, timestamp.unit) => {
                timestamps.push(timestamp)
            }
            _ => first_failure = Some((timestamps.len(), field.to_string())),
        }
    });
    if let Some((row, field)) = first_failure {
        warn!(
            time_column = %column,
            format = ?inference.format,
            row,
            field = %field,
            "the time column cannot be read in this timestamp format (a value fails to parse \
             or is outside the dates Glyde can display); plotting against a generated row \
             index instead (SPEC §1.3 \"never abort the load\")"
        );
        return Err(unreadable());
    }
    info!(
        time_column = %column,
        format = ?inference.format,
        corrected = format_override.is_some(),
        low_confidence = inference.ambiguous,
        "time column of numbers or text read as absolute time, as the same values in a CSV \
         (SPEC §2.1)"
    );
    Ok((
        TimeAxis::Absolute {
            timestamps: Timestamps::Memory(timestamps),
            format: inference.format,
        },
        inference.ambiguous,
    ))
}

impl ColumnValues {
    /// Whether this is a text column, which SPEC §2.1 reads for a
    /// timestamp format as it would a CSV column's text.
    fn is_text(&self) -> bool {
        matches!(self, ColumnValues::String(_))
    }

    /// The number of rows.
    fn len(&self) -> usize {
        match self {
            ColumnValues::Bool(v) => v.len(),
            ColumnValues::I8(v) => v.len(),
            ColumnValues::I16(v) => v.len(),
            ColumnValues::I32(v) => v.len(),
            ColumnValues::I64(v) => v.len(),
            ColumnValues::U8(v) => v.len(),
            ColumnValues::U16(v) => v.len(),
            ColumnValues::U32(v) => v.len(),
            ColumnValues::U64(v) => v.len(),
            ColumnValues::F32(v) => v.len(),
            ColumnValues::F64(v) => v.len(),
            ColumnValues::String(v) => v.len(),
            ColumnValues::Timestamp { ticks, .. } => ticks.len(),
        }
    }

    /// Hands `f` each row's value as the text a CSV column would hold: a
    /// string as is, a number in its shortest exact decimal form (Rust's
    /// `Display`, never an exponent). One buffer is reused for every row, so
    /// the column is never held a second time as text. Booleans and
    /// timestamps are never asked (see [`time_axis_from_text`]'s caller).
    fn for_each_field(&self, mut f: impl FnMut(&str)) {
        fn render<T: std::fmt::Display>(values: &[T], f: &mut impl FnMut(&str)) {
            use std::fmt::Write;
            let mut buffer = String::new();
            for value in values {
                buffer.clear();
                // Writing to a `String` cannot fail.
                let _ = write!(buffer, "{value}");
                f(&buffer);
            }
        }
        match self {
            ColumnValues::I8(v) => render(v, &mut f),
            ColumnValues::I16(v) => render(v, &mut f),
            ColumnValues::I32(v) => render(v, &mut f),
            ColumnValues::I64(v) => render(v, &mut f),
            ColumnValues::U8(v) => render(v, &mut f),
            ColumnValues::U16(v) => render(v, &mut f),
            ColumnValues::U32(v) => render(v, &mut f),
            ColumnValues::U64(v) => render(v, &mut f),
            ColumnValues::F32(v) => render(v, &mut f),
            ColumnValues::F64(v) => render(v, &mut f),
            ColumnValues::String(v) => v.iter().for_each(|text| f(text)),
            ColumnValues::Bool(_) | ColumnValues::Timestamp { .. } => {}
        }
    }

    /// Whether this is a number column, the only kind SPEC §2.1 lets stand
    /// in as a progressive time index.
    fn is_numeric(&self) -> bool {
        !matches!(
            self,
            ColumnValues::Bool(_) | ColumnValues::String(_) | ColumnValues::Timestamp { .. }
        )
    }

    /// A numeric column's values as `f64`, for SPEC §2.1's time-index test
    /// and, if it passes, the progressive axis (the same `f64` a CSV
    /// progressive index holds).
    fn to_f64(&self) -> Vec<f64> {
        match self {
            ColumnValues::I8(v) => v.iter().map(|&n| f64::from(n)).collect(),
            ColumnValues::I16(v) => v.iter().map(|&n| f64::from(n)).collect(),
            ColumnValues::I32(v) => v.iter().map(|&n| f64::from(n)).collect(),
            ColumnValues::I64(v) => v.iter().map(|&n| n as f64).collect(),
            ColumnValues::U8(v) => v.iter().map(|&n| f64::from(n)).collect(),
            ColumnValues::U16(v) => v.iter().map(|&n| f64::from(n)).collect(),
            ColumnValues::U32(v) => v.iter().map(|&n| f64::from(n)).collect(),
            ColumnValues::U64(v) => v.iter().map(|&n| n as f64).collect(),
            ColumnValues::F32(v) => v.iter().map(|&n| f64::from(n)).collect(),
            ColumnValues::F64(v) => v.clone(),
            // Not numeric: never asked (see `is_numeric`).
            ColumnValues::Bool(_) | ColumnValues::String(_) | ColumnValues::Timestamp { .. } => {
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::RamBudget;
    use crate::ingest::{SamplingClass, TimeIndexSource, TimezoneLabel};
    use crate::series::Dtype;
    use arrow::array::{
        BooleanArray, Date32Array, Decimal128Array, DictionaryArray, Float32Array, Float64Array,
        Int16Array, Int32Array, Int64Array, Int8Array, LargeStringArray, StringArray,
        TimestampMicrosecondArray, TimestampMillisecondArray, TimestampSecondArray, UInt16Array,
        UInt32Array, UInt64Array, UInt8Array,
    };
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    const SECOND_US: i64 = 1_000_000;
    /// 2026-03-01T08:00:00Z, the first row of corpus case 53.
    const START_US: i64 = 1_772_352_000_000_000;

    fn corpus_path(file_name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/corpus")
            .join(file_name)
    }

    fn write(dir: &tempfile::TempDir, columns: Vec<(&str, ArrayRef)>) -> PathBuf {
        write_with(dir, columns, None)
    }

    fn write_with(
        dir: &tempfile::TempDir,
        columns: Vec<(&str, ArrayRef)>,
        properties: Option<WriterProperties>,
    ) -> PathBuf {
        let path = dir.path().join("test.parquet");
        write_at(&path, columns, properties);
        path
    }

    /// Writes one Parquet file at `path`, creating its directories.
    fn write_at(
        path: &Path,
        columns: Vec<(&str, ArrayRef)>,
        properties: Option<WriterProperties>,
    ) -> PathBuf {
        let batch = RecordBatch::try_from_iter(columns).expect("valid test batch");
        std::fs::create_dir_all(path.parent().unwrap()).expect("create test dirs");
        let path = path.to_path_buf();
        let file = File::create(&path).expect("create test file");
        let mut writer =
            ArrowWriter::try_new(file, batch.schema(), properties).expect("parquet writer");
        writer.write(&batch).expect("write batch");
        writer.close().expect("close writer");
        path
    }

    fn seconds(count: i64) -> ArrayRef {
        Arc::new(
            TimestampMicrosecondArray::from(
                (0..count)
                    .map(|i| START_US + i * SECOND_US)
                    .collect::<Vec<_>>(),
            )
            .with_timezone("UTC"),
        )
    }

    fn open_with(
        path: &Path,
        overrides: IngestOverrides,
    ) -> Result<(OpenSummary, InferenceReport, Dataset)> {
        open(
            path,
            OpenRequest {
                overrides,
                budget: RamBudget::from_total_ram_bytes(16 << 30),
                cache_dir: None,
                cancelled: &AtomicBool::new(false),
                on_checkpoint: None,
            },
        )
    }

    fn open_default(path: &Path) -> (OpenSummary, InferenceReport, Dataset) {
        open_with(path, IngestOverrides::default()).expect("test file must open")
    }

    fn timestamps(dataset: &Dataset) -> Vec<Timestamp> {
        match &dataset.time {
            TimeAxis::Absolute { timestamps, format } => {
                assert_eq!(*format, TimestampFormat::ParquetTimestamp);
                timestamps.iter().collect()
            }
            other => panic!("expected an absolute axis, got {other:?}"),
        }
    }

    #[test]
    fn corpus_53_opens_with_its_typed_timestamp_column_as_the_time_index() {
        let (summary, report, dataset) =
            open_default(&corpus_path("case-53-clean-parquet-timestamp.parquet"));

        assert_eq!(summary.row_count, 6);
        assert_eq!(summary.time_column.as_deref(), Some("timestamp"));
        assert_eq!(
            summary.timestamp_format.as_deref(),
            Some("parquet_timestamp")
        );
        assert_eq!(summary.sampling_class, SamplingClass::Uniform);
        assert_eq!(summary.encoding, "n/a");
        assert_eq!(summary.delimiter, None);
        assert_eq!(summary.decimal_separator, None);
        assert_eq!(
            report.timezone,
            Some(TimezoneLabel::Honored("+00:00".to_string()))
        );
        assert!(!report.has_low_confidence_field());
        assert!(report.unloaded_columns.is_empty());

        let ticks: Vec<Timestamp> = timestamps(&dataset);
        assert_eq!(ticks.len(), 6);
        assert_eq!(
            ticks[0],
            Timestamp::with_offset(i128::from(START_US), TimeUnit::Microseconds, 0)
        );
        assert_eq!(
            ticks[5].ticks,
            i128::from(START_US + 5 * SECOND_US),
            "ticks stay in the file's own unit"
        );
        assert_eq!(dataset.columns.len(), 1);
        assert_eq!(dataset.columns[0].name(), "value");
        assert_eq!(
            dataset.columns[0].values(),
            &SeriesValues::F64(vec![10.0, 10.1, 10.2, 10.3, 10.4, 10.5])
        );
    }

    #[test]
    fn every_supported_arrow_type_keeps_its_own_dtype_and_exact_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("t", seconds(2)),
                ("bool", Arc::new(BooleanArray::from(vec![true, false]))),
                ("i8", Arc::new(Int8Array::from(vec![i8::MIN, i8::MAX]))),
                ("i16", Arc::new(Int16Array::from(vec![i16::MIN, i16::MAX]))),
                ("i32", Arc::new(Int32Array::from(vec![i32::MIN, i32::MAX]))),
                ("i64", Arc::new(Int64Array::from(vec![i64::MIN, i64::MAX]))),
                ("u8", Arc::new(UInt8Array::from(vec![0, u8::MAX]))),
                ("u16", Arc::new(UInt16Array::from(vec![0, u16::MAX]))),
                ("u32", Arc::new(UInt32Array::from(vec![0, u32::MAX]))),
                ("u64", Arc::new(UInt64Array::from(vec![0, u64::MAX]))),
                ("f32", Arc::new(Float32Array::from(vec![0.1f32, -2.5]))),
                ("f64", Arc::new(Float64Array::from(vec![0.1, f64::MAX]))),
                (
                    "utf8",
                    Arc::new(StringArray::from(vec!["idle", "°C µm/s²"])),
                ),
                ("large", Arc::new(LargeStringArray::from(vec!["a", "b"]))),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        assert!(report.unloaded_columns.is_empty());
        let by_name = |name: &str| {
            dataset
                .columns
                .iter()
                .find(|series| series.name() == name)
                .unwrap_or_else(|| panic!("{name} must be loaded"))
                .values()
                .clone()
        };
        assert_eq!(by_name("bool"), SeriesValues::Bool(vec![true, false]));
        assert_eq!(by_name("i8"), SeriesValues::I8(vec![i8::MIN, i8::MAX]));
        assert_eq!(by_name("i16"), SeriesValues::I16(vec![i16::MIN, i16::MAX]));
        assert_eq!(by_name("i32"), SeriesValues::I32(vec![i32::MIN, i32::MAX]));
        assert_eq!(by_name("i64"), SeriesValues::I64(vec![i64::MIN, i64::MAX]));
        assert_eq!(by_name("u8"), SeriesValues::U8(vec![0, u8::MAX]));
        assert_eq!(by_name("u16"), SeriesValues::U16(vec![0, u16::MAX]));
        assert_eq!(by_name("u32"), SeriesValues::U32(vec![0, u32::MAX]));
        assert_eq!(by_name("u64"), SeriesValues::U64(vec![0, u64::MAX]));
        assert_eq!(by_name("f32"), SeriesValues::F32(vec![0.1, -2.5]));
        assert_eq!(by_name("f64"), SeriesValues::F64(vec![0.1, f64::MAX]));
        assert_eq!(
            by_name("utf8"),
            SeriesValues::String(vec!["idle".into(), "°C µm/s²".into()])
        );
        assert_eq!(
            by_name("large"),
            SeriesValues::String(vec!["a".into(), "b".into()])
        );
        let dtypes: Vec<Dtype> = dataset.columns.iter().map(|s| s.dtype()).collect();
        assert_eq!(dtypes[0], Dtype::Bool);
        assert_eq!(dataset.columns.len(), 13, "every column but the time index");
    }

    #[test]
    fn columns_glyde_cannot_hold_are_listed_with_a_reason_never_dropped_silently() {
        let dir = tempfile::tempdir().unwrap();
        let numeric_dictionary: DictionaryArray<arrow::datatypes::Int8Type> = DictionaryArray::new(
            arrow::array::Int8Array::from(vec![0, 1, 0]),
            Arc::new(Int32Array::from(vec![7, 9])),
        );
        let path = write(
            &dir,
            vec![
                ("t", seconds(3)),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0]))),
                (
                    "price",
                    Arc::new(
                        Decimal128Array::from(vec![100, 200, 300])
                            .with_precision_and_scale(10, 2)
                            .unwrap(),
                    ),
                ),
                ("day", Arc::new(Date32Array::from(vec![1, 2, 3]))),
                ("codes", Arc::new(numeric_dictionary)),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        let names: Vec<&str> = dataset.columns.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["value"]);
        let unloaded: Vec<(&str, &str)> = report
            .unloaded_columns
            .iter()
            .map(|c| (c.name.as_str(), c.reason.as_str()))
            .collect();
        assert_eq!(unloaded.len(), 3, "{unloaded:?}");
        assert!(unloaded[0].0 == "price" && unloaded[0].1.contains("Decimal128(10, 2)"));
        assert!(unloaded[1].0 == "day" && unloaded[1].1.contains("Date32"));
        assert!(unloaded[2].0 == "codes" && unloaded[2].1.contains("Dictionary(Int8, Int32)"));
        assert_eq!(report.column_names, ["t", "value", "price", "day", "codes"]);
    }

    fn series<'a>(dataset: &'a Dataset, name: &str) -> &'a crate::series::Series {
        dataset
            .columns
            .iter()
            .find(|series| series.name() == name)
            .unwrap_or_else(|| panic!("{name} must be loaded"))
    }

    /// SPEC §1.3: a null is a gap — NaN in the series, flagged as a NaN run,
    /// never filled with whatever the value buffer held under it.
    #[test]
    fn corpus_54_float_nulls_become_flagged_nan_gaps() {
        let (summary, report, dataset) =
            open_default(&corpus_path("case-54-parquet-with-nulls.parquet"));

        assert_eq!(summary.row_count, 6);
        assert!(report.unloaded_columns.is_empty());
        let value = series(&dataset, "value");
        let SeriesValues::F64(samples) = value.values() else {
            panic!("a double column stays f64, got {:?}", value.dtype());
        };
        let shown: Vec<Option<f64>> = samples
            .iter()
            .map(|v| (!v.is_nan()).then_some(*v))
            .collect();
        assert_eq!(
            shown,
            [Some(10.0), Some(10.1), None, Some(10.3), None, Some(10.5)]
        );
        assert_eq!(value.anomalies().nan_runs, [2..3, 4..5]);
    }

    #[test]
    fn corpus_56_dictionary_strings_become_a_categorical_series() {
        let (summary, report, dataset) =
            open_default(&corpus_path("case-56-parquet-dictionary-strings.parquet"));

        assert_eq!(summary.row_count, 6);
        assert!(report.unloaded_columns.is_empty());
        let state = series(&dataset, "state");
        assert_eq!(
            state.values(),
            &SeriesValues::String(
                ["idle", "idle", "running", "running", "running", "fault"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(state.view_kind(), crate::series::ViewKind::StateTimeline);
    }

    #[test]
    fn nulls_in_every_other_supported_type_follow_the_csv_rules() {
        let dir = tempfile::tempdir().unwrap();
        let labels: DictionaryArray<arrow::datatypes::Int32Type> =
            vec![Some("on"), None, Some("off")].into_iter().collect();
        let path = write(
            &dir,
            vec![
                ("t", seconds(3)),
                (
                    "f32",
                    Arc::new(Float32Array::from(vec![Some(1.5), None, Some(3.0)])),
                ),
                (
                    "i32",
                    Arc::new(Int32Array::from(vec![Some(1), None, Some(3)])),
                ),
                (
                    "u64",
                    Arc::new(UInt64Array::from(vec![Some(1 << 53), None, Some(0)])),
                ),
                (
                    "big",
                    Arc::new(Int64Array::from(vec![Some((1 << 53) + 1), None, Some(0)])),
                ),
                (
                    "flag",
                    Arc::new(BooleanArray::from(vec![Some(true), None, Some(false)])),
                ),
                (
                    "text",
                    Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])),
                ),
                ("labels", Arc::new(labels)),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        let gaps = |name: &str| -> Vec<Option<f64>> {
            let values = series(&dataset, name).values();
            assert_eq!(values.dtype(), Dtype::F64, "{name} is promoted to f64");
            values
                .to_f64_vec()
                .unwrap()
                .into_iter()
                .map(|v| (!v.is_nan()).then_some(v))
                .collect()
        };
        let SeriesValues::F32(f32s) = series(&dataset, "f32").values() else {
            panic!("a float column stays f32");
        };
        assert!(f32s[0] == 1.5 && f32s[1].is_nan() && f32s[2] == 3.0);
        assert_eq!(series(&dataset, "f32").anomalies().nan_runs, vec![(1..2)]);
        assert_eq!(gaps("i32"), [Some(1.0), None, Some(3.0)]);
        assert_eq!(gaps("u64"), [Some((1u64 << 53) as f64), None, Some(0.0)]);
        assert_eq!(series(&dataset, "i32").anomalies().nan_runs, vec![(1..2)]);
        let text = |values: [&str; 3]| SeriesValues::String(values.map(String::from).to_vec());
        assert_eq!(
            series(&dataset, "flag").values(),
            &text(["true", "", "false"])
        );
        assert_eq!(series(&dataset, "text").values(), &text(["a", "", "c"]));
        assert_eq!(
            series(&dataset, "labels").values(),
            &text(["on", "", "off"])
        );

        assert_eq!(report.unloaded_columns.len(), 1);
        assert_eq!(report.unloaded_columns[0].name, "big");
        assert!(
            report.unloaded_columns[0].reason.contains("exactly"),
            "{}",
            report.unloaded_columns[0].reason
        );
    }

    #[test]
    fn a_large_utf8_dictionary_decodes_too() {
        let dir = tempfile::tempdir().unwrap();
        let labels = DictionaryArray::<arrow::datatypes::Int16Type>::new(
            arrow::array::Int16Array::from(vec![1, 0, 1]),
            Arc::new(LargeStringArray::from(vec!["low", "high"])),
        );
        let path = write(
            &dir,
            vec![
                ("t", seconds(3)),
                ("labels", Arc::new(labels)),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0]))),
            ],
        );

        let (_summary, _report, dataset) = open_default(&path);

        assert_eq!(
            series(&dataset, "labels").values(),
            &SeriesValues::String(vec!["high".into(), "low".into(), "high".into()])
        );
    }

    #[test]
    fn a_timestamp_column_with_missing_values_cannot_be_the_time_axis() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                (
                    "t",
                    Arc::new(
                        TimestampMicrosecondArray::from(vec![Some(START_US), None, Some(START_US)])
                            .with_timezone("UTC"),
                    ) as ArrayRef,
                ),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0]))),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        assert_eq!(
            report.time_index,
            TimeIndexSource::Generated(GeneratedIndexReason::MissingTimestamps {
                column: "t".to_string(),
                row: 1
            })
        );
        assert!(report.has_low_confidence_field());
        assert_eq!(dataset.columns.len(), 1);
        assert_eq!(report.unloaded_columns[0].name, "t");
    }

    #[test]
    fn a_second_timestamp_column_is_listed_as_unloaded_not_plotted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("t", seconds(2)),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0]))),
                ("logged_at", seconds(2)),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        assert_eq!(dataset.time_column_name, "t");
        assert_eq!(dataset.columns.len(), 1);
        assert_eq!(report.unloaded_columns.len(), 1);
        assert_eq!(report.unloaded_columns[0].name, "logged_at");
        assert!(report.unloaded_columns[0]
            .reason
            .contains("second timestamp column"));
    }

    #[test]
    fn the_first_timestamp_column_is_the_index_wherever_it_sits() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("value", Arc::new(Float64Array::from(vec![5.0, 1.0]))),
                ("t", seconds(2)),
            ],
        );

        let (summary, report, dataset) = open_default(&path);

        assert_eq!(summary.time_column.as_deref(), Some("t"));
        assert_eq!(
            report.time_index,
            TimeIndexSource::Column {
                index: 1,
                name: "t".to_string()
            }
        );
        assert_eq!(dataset.columns[0].name(), "value");
    }

    #[test]
    fn without_a_timestamp_column_an_advancing_numeric_first_column_is_a_progressive_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("sample", Arc::new(Int32Array::from(vec![0, 1, 2, 3]))),
                (
                    "value",
                    Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0, 4.0])),
                ),
            ],
        );

        let (summary, report, dataset) = open_default(&path);

        assert_eq!(summary.sampling_class, SamplingClass::ProgressiveIndex);
        assert_eq!(dataset.time_column_name, "sample");
        assert_eq!(
            dataset.time,
            TimeAxis::Progressive {
                values: ProgressiveValues::Memory(vec![0.0, 1.0, 2.0, 3.0])
            }
        );
        assert!(!report.has_low_confidence_field());
    }

    #[test]
    fn without_a_timestamp_column_a_numeric_first_column_that_drops_is_a_series() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("accel", Arc::new(Float64Array::from(vec![0.5, 0.7, 0.2]))),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0]))),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        assert_eq!(
            report.time_index,
            TimeIndexSource::Generated(GeneratedIndexReason::NotMonotonic {
                column: "accel".to_string(),
                row: 2
            })
        );
        assert!(report.has_low_confidence_field());
        let names: Vec<&str> = dataset.columns.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["accel", "value"]);
    }

    #[test]
    fn without_a_timestamp_column_a_text_first_column_is_unreadable_as_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("label", Arc::new(StringArray::from(vec!["a", "b"]))),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0]))),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        assert_eq!(
            report.time_index,
            TimeIndexSource::Generated(GeneratedIndexReason::Unreadable {
                column: "label".to_string()
            })
        );
        assert_eq!(dataset.columns.len(), 2);
    }

    #[test]
    fn picking_the_row_index_plots_against_rows_and_reports_the_timestamp_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("t", seconds(2)),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0]))),
            ],
        );

        let (_summary, report, dataset) = open_with(
            &path,
            IngestOverrides {
                time_column: Some(TimeColumnChoice::RowIndex),
                ..IngestOverrides::default()
            },
        )
        .unwrap();

        assert_eq!(
            report.time_index,
            TimeIndexSource::Generated(GeneratedIndexReason::Requested)
        );
        assert_eq!(dataset.columns.len(), 1);
        assert_eq!(report.unloaded_columns[0].name, "t");
    }

    #[test]
    fn a_picked_numeric_column_is_taken_as_given_even_if_it_runs_backwards() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("t", seconds(3)),
                ("depth", Arc::new(Float64Array::from(vec![3.0, 1.0, 2.0]))),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0]))),
            ],
        );

        let (summary, report, dataset) = open_with(
            &path,
            IngestOverrides {
                time_column: Some(TimeColumnChoice::Column(1)),
                ..IngestOverrides::default()
            },
        )
        .unwrap();

        assert_eq!(dataset.time_column_name, "depth");
        assert_eq!(summary.non_monotonic_count, 1);
        assert_eq!(report.unloaded_columns[0].name, "t");
    }

    /// 2026-03-01T08:00:00Z in whole seconds, corpus case 53's first row.
    const START_S: i64 = START_US / SECOND_US;
    /// The same instant as LabVIEW seconds (since 1904-01-01T00:00:00Z).
    const START_LABVIEW_S: i64 = START_S + 2_082_844_800;

    /// Opens the CSV `t,value` file holding `fields` as its time column, the
    /// text a Parquet value of the same column reads as.
    fn open_csv_twin(
        dir: &tempfile::TempDir,
        fields: &[String],
        overrides: IngestOverrides,
    ) -> (OpenSummary, InferenceReport, Dataset) {
        let mut text = String::from("t,value\n");
        for (row, field) in fields.iter().enumerate() {
            text.push_str(&format!("{field},{row}.5\n"));
        }
        let path = dir.path().join("twin.csv");
        std::fs::write(&path, text).expect("write CSV twin");
        crate::ingest::open_dataset_with_overrides(&path, overrides).expect("CSV twin opens")
    }

    /// The SPEC §2.1 decision a time column reached: everything the
    /// inference bar and the plot take from it.
    fn time_decision(
        summary: &OpenSummary,
        report: &InferenceReport,
        dataset: &Dataset,
    ) -> impl std::fmt::Debug + PartialEq {
        (
            report.time_index.clone(),
            report.timestamp_format.value.clone(),
            report.timestamp_format.confidence,
            report.timezone.clone(),
            summary.sampling_class,
            dataset.time.clone(),
        )
    }

    /// Asserts that a Parquet file whose time column is `column` reaches
    /// exactly the time-index decision a CSV holding the same values as
    /// text does (docs/ROADMAP.md M7, issue #166).
    fn assert_same_time_index_as_csv(column: ArrayRef, overrides: IngestOverrides) {
        let rows = column.len();
        let fields: Vec<String> = (0..rows)
            .map(|row| arrow::util::display::array_value_to_string(&column, row).unwrap())
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("t", column),
                (
                    "value",
                    Arc::new(Float64Array::from(
                        (0..rows).map(|row| row as f64 + 0.5).collect::<Vec<_>>(),
                    )),
                ),
            ],
        );

        let (summary, report, dataset) = open_with(&path, overrides).expect("Parquet opens");
        let (csv_summary, csv_report, csv_dataset) = open_csv_twin(&dir, &fields, overrides);

        assert_eq!(
            time_decision(&summary, &report, &dataset),
            time_decision(&csv_summary, &csv_report, &csv_dataset),
            "Parquet and CSV disagree on the time column {fields:?}"
        );
    }

    #[test]
    fn number_time_columns_reach_the_same_time_index_as_the_same_values_in_a_csv() {
        let int64 = |values: Vec<i64>| -> ArrayRef { Arc::new(Int64Array::from(values)) };
        let float64 = |values: Vec<f64>| -> ArrayRef { Arc::new(Float64Array::from(values)) };
        let steps = |start: i64, step: i64| (0..6).map(|i| start + i * step).collect::<Vec<_>>();
        let columns: Vec<(&str, ArrayRef)> = vec![
            ("epoch seconds", int64(steps(START_S, 1))),
            ("epoch milliseconds", int64(steps(START_S * 1_000, 1_000))),
            ("epoch microseconds", int64(steps(START_US, SECOND_US))),
            (
                "epoch nanoseconds",
                int64(steps(START_US * 1_000, SECOND_US * 1_000)),
            ),
            (
                "unsigned epoch seconds",
                Arc::new(UInt32Array::from(
                    (0..6).map(|i| START_S as u32 + i).collect::<Vec<_>>(),
                )),
            ),
            (
                "float epoch seconds with a fraction",
                float64((0..6).map(|i| START_S as f64 + 0.25 + i as f64).collect()),
            ),
            (
                "whole epoch seconds also in the LabVIEW window (ambiguous)",
                int64(steps(START_LABVIEW_S, 1)),
            ),
            (
                "Excel serial days",
                float64((0..6).map(|i| 46_082.5 + f64::from(i) * 0.25).collect()),
            ),
            ("a small progressive index", int64(steps(0, 1))),
            (
                "epoch milliseconds that drop",
                int64(vec![
                    START_S * 1_000,
                    START_S * 1_000 + 1_000,
                    START_S * 1_000 - 5_000,
                    START_S * 1_000 + 2_000,
                ]),
            ),
            ("epoch seconds stuck on one value", int64(vec![START_S; 4])),
        ];
        for (case, column) in columns {
            eprintln!("case: {case}");
            assert_same_time_index_as_csv(column, IngestOverrides::default());
        }
    }

    #[test]
    fn text_time_columns_reach_the_same_time_index_as_the_same_text_in_a_csv() {
        let text = |values: &[&str]| -> ArrayRef { Arc::new(StringArray::from(values.to_vec())) };
        let columns: Vec<(&str, ArrayRef)> = vec![
            (
                "ISO 8601 with an offset",
                text(&[
                    "2026-03-01T08:00:00+01:00",
                    "2026-03-01T08:00:01+01:00",
                    "2026-03-01T08:00:02+01:00",
                ]),
            ),
            (
                "date and time with a space",
                text(&[
                    "2026-03-01 08:00:00",
                    "2026-03-01 08:00:01",
                    "2026-03-01 08:00:02",
                ]),
            ),
            (
                "day/month ambiguous in every row",
                text(&[
                    "01/02/2026 08:00:00",
                    "01/02/2026 08:00:01",
                    "01/02/2026 08:00:02",
                ]),
            ),
            (
                "dates out of order stay a time index",
                text(&[
                    "2026-03-01T08:00:02",
                    "2026-03-01T08:00:00",
                    "2026-03-01T08:00:01",
                ]),
            ),
            (
                "epoch seconds written as text",
                text(&["1772352000", "1772352001", "1772352002"]),
            ),
            ("text that is no time", text(&["a", "b", "c"])),
        ];
        for (case, column) in columns {
            eprintln!("case: {case}");
            assert_same_time_index_as_csv(column, IngestOverrides::default());
        }
    }

    #[test]
    fn a_timestamp_format_correction_applies_to_a_number_time_column_as_for_a_csv() {
        let ambiguous: ArrayRef = Arc::new(Int64Array::from(
            (0..4).map(|i| START_LABVIEW_S + i).collect::<Vec<_>>(),
        ));
        let corrected = IngestOverrides {
            timestamp_format: Some(TimestampFormat::LabViewEpoch),
            ..IngestOverrides::default()
        };
        assert_same_time_index_as_csv(ambiguous.clone(), corrected);

        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, vec![("t", ambiguous), ("value", values(4, 0.0))]);
        let (_summary, report, dataset) = open_with(&path, corrected).unwrap();
        assert_eq!(
            report.timestamp_format.value.as_deref(),
            Some("labview_epoch")
        );
        assert_eq!(report.timestamp_format.confidence, Confidence::High);
        match &dataset.time {
            TimeAxis::Absolute { timestamps, format } => {
                assert_eq!(*format, TimestampFormat::LabViewEpoch);
                let first = timestamps.iter().next().unwrap();
                assert_eq!(
                    first.ticks * 1_000_000 / first.unit.ticks_per_second(),
                    i128::from(START_US)
                );
            }
            other => panic!("expected an absolute axis, got {other:?}"),
        }
    }

    #[test]
    fn an_epoch_milliseconds_first_column_is_absolute_time_in_utc() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                (
                    "time_ms",
                    Arc::new(Int64Array::from(
                        (0..4)
                            .map(|i| START_S * 1_000 + i * 500)
                            .collect::<Vec<_>>(),
                    )),
                ),
                ("value", values(4, 0.0)),
            ],
        );

        let (summary, report, dataset) = open_default(&path);

        assert_eq!(dataset.time_column_name, "time_ms");
        assert_eq!(summary.timestamp_format.as_deref(), Some("epoch_ms"));
        assert_eq!(summary.sampling_class, SamplingClass::Uniform);
        assert_eq!(report.timezone, Some(TimezoneLabel::UtcImplicit));
        assert!(!report.has_low_confidence_field());
        let TimeAxis::Absolute { timestamps, .. } = &dataset.time else {
            panic!("expected an absolute axis, got {:?}", dataset.time);
        };
        let ticks: Vec<i128> = timestamps.iter().map(|t| t.ticks).collect();
        let first = i128::from(START_S * 1_000);
        assert_eq!(ticks, [first, first + 500, first + 1_000, first + 1_500]);
        // The column is the axis, not also a series.
        assert_eq!(dataset.columns.len(), 1);
    }

    #[test]
    fn a_whole_valued_float_time_column_reads_as_the_same_integers_would() {
        let open_column = |column: ArrayRef| {
            let dir = tempfile::tempdir().unwrap();
            let path = write(&dir, vec![("t", column), ("value", values(3, 0.0))]);
            let (summary, report, dataset) = open_default(&path);
            (
                summary.timestamp_format,
                report.timestamp_format.confidence,
                dataset.time,
            )
        };
        for start in [START_S, START_LABVIEW_S] {
            let integers = (0..3).map(|i| start + i).collect::<Vec<_>>();
            let floats = integers.iter().map(|&n| n as f64).collect::<Vec<_>>();
            assert_eq!(
                open_column(Arc::new(Float64Array::from(floats))),
                open_column(Arc::new(Int64Array::from(integers))),
            );
        }
    }

    #[test]
    fn a_picked_epoch_column_becomes_the_absolute_time_axis() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("value", values(3, 0.0)),
                (
                    "epoch",
                    Arc::new(Int64Array::from(
                        (0..3).map(|i| START_S + i).collect::<Vec<_>>(),
                    )),
                ),
            ],
        );

        let (summary, _report, dataset) = open_with(
            &path,
            IngestOverrides {
                time_column: Some(TimeColumnChoice::Column(1)),
                ..IngestOverrides::default()
            },
        )
        .unwrap();

        assert_eq!(dataset.time_column_name, "epoch");
        assert_eq!(summary.timestamp_format.as_deref(), Some("epoch_s"));
    }

    #[test]
    fn a_timestamp_format_correction_does_not_apply_to_a_typed_timestamp_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, vec![("t", seconds(3)), ("value", values(3, 0.0))]);

        let (summary, _report, dataset) = open_with(
            &path,
            IngestOverrides {
                timestamp_format: Some(TimestampFormat::EpochSeconds),
                ..IngestOverrides::default()
            },
        )
        .unwrap();

        assert_eq!(
            summary.timestamp_format.as_deref(),
            Some("parquet_timestamp")
        );
        assert_eq!(timestamps(&dataset).len(), 3);
    }

    #[test]
    fn a_timestamp_format_correction_the_column_cannot_be_read_in_falls_back_to_the_row_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                (
                    "label",
                    Arc::new(StringArray::from(vec!["a", "b", "c"])) as ArrayRef,
                ),
                ("value", values(3, 0.0)),
            ],
        );

        let (_summary, report, dataset) = open_with(
            &path,
            IngestOverrides {
                timestamp_format: Some(TimestampFormat::Iso8601Naive),
                ..IngestOverrides::default()
            },
        )
        .expect("an unreadable correction never aborts the open");

        assert_eq!(
            report.time_index,
            TimeIndexSource::Generated(GeneratedIndexReason::Unreadable {
                column: "label".to_string()
            })
        );
        assert_eq!(dataset.columns.len(), 2);
    }

    #[test]
    fn sort_by_time_reorders_every_column_in_lockstep() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                (
                    "t",
                    Arc::new(TimestampSecondArray::from(vec![3, 1, 2])) as ArrayRef,
                ),
                (
                    "value",
                    Arc::new(Float64Array::from(vec![30.0, 10.0, 20.0])),
                ),
            ],
        );

        let (_summary, _report, dataset) = open_with(
            &path,
            IngestOverrides {
                sort_by_time: true,
                ..IngestOverrides::default()
            },
        )
        .unwrap();

        let ticks: Vec<i128> = timestamps(&dataset).iter().map(|t| t.ticks).collect();
        assert_eq!(ticks, [1, 2, 3]);
        assert_eq!(
            dataset.columns[0].values(),
            &SeriesValues::F64(vec![10.0, 20.0, 30.0])
        );
    }

    #[test]
    fn a_named_timezone_is_honored_per_row_across_a_dst_change() {
        let dir = tempfile::tempdir().unwrap();
        // 2026-03-29T00:30:00Z and 02:30Z: Rome switches from +01:00 to
        // +02:00 at 01:00Z that night.
        let before = 1_774_744_200_000;
        let path = write(
            &dir,
            vec![
                (
                    "t",
                    Arc::new(
                        TimestampMillisecondArray::from(vec![before, before + 7_200_000])
                            .with_timezone("Europe/Rome"),
                    ) as ArrayRef,
                ),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0]))),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        let offsets: Vec<Option<i32>> = timestamps(&dataset)
            .iter()
            .map(|t| t.offset_seconds)
            .collect();
        assert_eq!(offsets, [Some(3600), Some(7200)]);
        assert_eq!(
            report.timezone,
            Some(TimezoneLabel::Honored("+01:00".to_string()))
        );
        assert_eq!(
            crate::time::format_timestamp(
                &timestamps(&dataset)[1],
                TimestampFormat::ParquetTimestamp
            ),
            "2026-03-29T04:30:00+02:00"
        );
    }

    #[test]
    fn a_timestamp_without_a_timezone_is_naive_local_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                (
                    "t",
                    Arc::new(TimestampSecondArray::from(vec![0, 1])) as ArrayRef,
                ),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0]))),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        assert_eq!(report.timezone, Some(TimezoneLabel::NaiveLocal));
        let first = timestamps(&dataset)[0];
        assert_eq!(first, Timestamp::new(0, TimeUnit::Seconds));
        assert_eq!(
            crate::time::format_timestamp(&first, TimestampFormat::ParquetTimestamp),
            "1970-01-01T00:00:00"
        );
    }

    #[test]
    fn rows_from_every_row_group_arrive_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let rows = 10_000;
        let path = write_with(
            &dir,
            vec![
                ("t", seconds(rows)),
                (
                    "value",
                    Arc::new(Float64Array::from(
                        (0..rows).map(|i| i as f64).collect::<Vec<_>>(),
                    )),
                ),
            ],
            Some(
                WriterProperties::builder()
                    .set_max_row_group_size(1_000)
                    .build(),
            ),
        );

        let (summary, _report, dataset) = open_default(&path);

        assert_eq!(summary.row_count, rows as u64);
        assert_eq!(
            dataset.columns[0].values(),
            &SeriesValues::F64((0..rows).map(|i| i as f64).collect())
        );
        assert_eq!(summary.non_monotonic_count, 0);
    }

    /// The PSD reads each timestamp's own unit: a 1 kHz recording stored as
    /// Parquet microseconds must show a 50 Hz tone at 50 Hz, not at a
    /// frequency scaled by the unit.
    #[test]
    fn a_50_hz_tone_in_a_microsecond_parquet_file_peaks_at_50_hz() {
        use crate::dsp::psd::{
            compute_psds, plan_psd, PsdMemoryCap, PsdSettings, PSD_MEMORY_CAP_BYTES,
        };
        let dir = tempfile::tempdir().unwrap();
        let rows = 8_192;
        let tone: Vec<f32> = (0..rows)
            .map(|i| (2.0 * std::f64::consts::PI * 50.0 * i as f64 / 1_000.0).sin() as f32)
            .collect();
        let path = write(
            &dir,
            vec![
                (
                    "t",
                    Arc::new(
                        TimestampMicrosecondArray::from(
                            (0..rows as i64)
                                .map(|i| START_US + i * 1_000)
                                .collect::<Vec<_>>(),
                        )
                        .with_timezone("UTC"),
                    ) as ArrayRef,
                ),
                ("tone", Arc::new(Float32Array::from(tone))),
            ],
        );
        let (_summary, _report, dataset) = open_default(&path);

        let plan = plan_psd(
            &dataset.time,
            0..dataset.time.len(),
            &PsdSettings::default(),
            1,
            PsdMemoryCap::from_bytes(PSD_MEMORY_CAP_BYTES),
        )
        .unwrap()
        .expect("a uniform 1 kHz axis has a PSD");
        let samples = dataset.columns[0].values().sample_source().unwrap();
        let spectra = compute_psds(&dataset.time, &[samples], &plan, &|_| true)
            .unwrap()
            .unwrap();

        let psd = &spectra[0];
        let peak = (0..psd.power.len())
            .max_by(|&a, &b| psd.power[a].total_cmp(&psd.power[b]))
            .unwrap();
        assert!(
            (psd.freqs[peak] - 50.0).abs() <= psd.delta_f,
            "peak at {} Hz",
            psd.freqs[peak]
        );
    }

    #[test]
    fn a_file_that_does_not_fit_the_budget_is_refused_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                ("t", seconds(1_000)),
                ("value", Arc::new(Float64Array::from(vec![0.0; 1_000]))),
            ],
        );

        let err = open(
            &path,
            OpenRequest {
                overrides: IngestOverrides::default(),
                budget: RamBudget::from_total_ram_bytes(4 * 1024),
                cache_dir: None,
                cancelled: &AtomicBool::new(false),
                on_checkpoint: None,
            },
        )
        .expect_err("1000 rows cannot fit a 1 KB budget");

        match err {
            GlydeError::BudgetExceeded {
                requested_bytes,
                cap_bytes,
            } => {
                assert_eq!(cap_bytes, 1024);
                assert!(requested_bytes >= 1_000 * 8, "{requested_bytes}");
            }
            other => panic!("expected BudgetExceeded, got {other:?}"),
        }
    }

    #[test]
    fn a_cancelled_open_stops_with_cancelled() {
        let err = open(
            &corpus_path("case-53-clean-parquet-timestamp.parquet"),
            OpenRequest {
                overrides: IngestOverrides::default(),
                budget: RamBudget::from_total_ram_bytes(16 << 30),
                cache_dir: None,
                cancelled: &AtomicBool::new(true),
                on_checkpoint: None,
            },
        )
        .expect_err("a raised cancel flag must stop the open");

        assert!(matches!(err, GlydeError::Cancelled));
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.parquet");
        std::fs::write(&path, b"PAR1 this is not a parquet footer").unwrap();

        let err = open_with(&path, IngestOverrides::default()).unwrap_err();

        assert!(matches!(err, GlydeError::InvalidParquet { .. }), "{err:?}");
    }

    #[test]
    fn timestamps_beyond_displayable_dates_fall_back_to_the_row_index_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            vec![
                (
                    "t",
                    Arc::new(TimestampSecondArray::from(vec![0, i64::MAX])) as ArrayRef,
                ),
                ("value", Arc::new(Float64Array::from(vec![1.0, 2.0]))),
            ],
        );

        let (_summary, report, dataset) = open_default(&path);

        assert_eq!(
            report.time_index,
            TimeIndexSource::Generated(GeneratedIndexReason::Unreadable {
                column: "t".to_string()
            })
        );
        assert_eq!(dataset.columns.len(), 1);
        assert_eq!(report.unloaded_columns[0].name, "t");
        assert!(report.unloaded_columns[0].reason.contains("row 1"));
    }

    #[test]
    fn a_lone_timestamp_column_leaves_nothing_to_plot() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, vec![("t", seconds(3))]);

        let err = open_with(&path, IngestOverrides::default()).unwrap_err();

        assert!(matches!(err, GlydeError::SingleColumnFile));
    }

    fn values(count: i64, first: f64) -> ArrayRef {
        Arc::new(Float64Array::from(
            (0..count).map(|i| first + i as f64).collect::<Vec<_>>(),
        ))
    }

    #[test]
    fn corpus_55_partitions_open_as_one_dataset_in_partition_order() {
        let path = corpus_path("case-55-partitioned-parquet-directory");
        assert!(ParquetReader.recognizes(&path));

        let (summary, report, dataset) = open_default(&path);

        assert_eq!(summary.row_count, 6);
        assert_eq!(summary.sampling_class, SamplingClass::SegmentedUniform);
        assert_eq!(summary.gap_count, 1);
        assert_eq!(report.column_names, ["timestamp", "value", "date"]);
        assert_eq!(
            series(&dataset, "value").values(),
            &SeriesValues::F64(vec![1.0, 1.1, 1.2, 2.0, 2.1, 2.2])
        );
        assert_eq!(
            series(&dataset, "date").values(),
            &SeriesValues::String(
                [
                    "2026-01-01",
                    "2026-01-01",
                    "2026-01-01",
                    "2026-01-03",
                    "2026-01-03",
                    "2026-01-03"
                ]
                .map(String::from)
                .to_vec()
            )
        );
    }

    #[test]
    fn a_directory_is_claimed_only_when_it_holds_parquet_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!ParquetReader.recognizes(dir.path()), "an empty directory");
        std::fs::write(dir.path().join("readings.csv"), "t,v\n").unwrap();
        assert!(!ParquetReader.recognizes(dir.path()), "only a CSV inside");
        write_at(
            &dir.path().join("deep/x=1/part-0.parquet"),
            vec![("t", seconds(2)), ("v", values(2, 0.0))],
            None,
        );
        assert!(
            ParquetReader.recognizes(dir.path()),
            "a nested Parquet file"
        );
    }

    #[test]
    fn files_of_a_directory_are_concatenated_in_path_order_and_markers_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        // Written out of order on purpose: path order, not creation order.
        write_at(
            &dir.path().join("part-1.parquet"),
            vec![("t", seconds(2)), ("v", values(2, 10.0))],
            None,
        );
        write_at(
            &dir.path().join("part-0.parquet"),
            vec![("t", seconds(2)), ("v", values(2, 0.0))],
            None,
        );
        std::fs::write(dir.path().join("_SUCCESS"), b"").unwrap();
        std::fs::write(dir.path().join(".part-0.parquet.crc"), b"junk").unwrap();
        write_at(
            &dir.path().join("_temporary/part-9.parquet"),
            vec![("t", seconds(1)), ("v", values(1, 99.0))],
            None,
        );

        let (summary, report, dataset) = open_default(dir.path());

        assert_eq!(
            series(&dataset, "v").values(),
            &SeriesValues::F64(vec![0.0, 1.0, 10.0, 11.0])
        );
        assert_eq!(report.column_names, ["t", "v"], "no partition keys here");
        // Both files restart at the same instant: reported, never reordered.
        assert_eq!(summary.non_monotonic_count, 1);
    }

    #[test]
    fn nested_hive_partitions_become_one_text_column_per_key() {
        let dir = tempfile::tempdir().unwrap();
        for (site, day) in [("north", "1"), ("south", "2")] {
            write_at(
                &dir.path()
                    .join(format!("site={site}/day={day}/part-0.parquet")),
                vec![("t", seconds(1)), ("v", values(1, 0.0))],
                None,
            );
        }

        let (_summary, report, dataset) = open_default(dir.path());

        assert_eq!(report.column_names, ["t", "v", "site", "day"]);
        let text = |v: [&str; 2]| SeriesValues::String(v.map(String::from).to_vec());
        assert_eq!(series(&dataset, "site").values(), &text(["north", "south"]));
        assert_eq!(series(&dataset, "day").values(), &text(["1", "2"]));
    }

    #[test]
    fn a_file_with_a_different_schema_refuses_the_open_and_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write_at(
            &dir.path().join("a.parquet"),
            vec![("t", seconds(1)), ("v", values(1, 0.0))],
            None,
        );
        write_at(
            &dir.path().join("b.parquet"),
            vec![("t", seconds(1)), ("pressure", values(1, 0.0))],
            None,
        );

        let err = open_with(dir.path(), IngestOverrides::default()).unwrap_err();

        match err {
            GlydeError::InvalidParquet { reason, .. } => {
                assert!(reason.contains("b.parquet"), "{reason}");
                assert!(reason.contains("a.parquet"), "{reason}");
            }
            other => panic!("expected InvalidParquet, got {other:?}"),
        }
    }

    #[test]
    fn files_with_different_partition_keys_refuse_the_open() {
        let dir = tempfile::tempdir().unwrap();
        write_at(
            &dir.path().join("date=1/part-0.parquet"),
            vec![("t", seconds(1)), ("v", values(1, 0.0))],
            None,
        );
        write_at(
            &dir.path().join("loose.parquet"),
            vec![("t", seconds(1)), ("v", values(1, 0.0))],
            None,
        );

        let err = open_with(dir.path(), IngestOverrides::default()).unwrap_err();

        assert!(
            matches!(&err, GlydeError::InvalidParquet { reason, .. } if reason.contains("partition")),
            "{err:?}"
        );
    }

    #[test]
    fn the_budget_check_covers_every_file_of_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        for part in 0..4 {
            write_at(
                &dir.path().join(format!("part-{part}.parquet")),
                vec![("t", seconds(1_000)), ("v", values(1_000, 0.0))],
                None,
            );
        }
        let one_file = open(
            &dir.path().join("part-0.parquet"),
            OpenRequest {
                overrides: IngestOverrides::default(),
                budget: RamBudget::from_total_ram_bytes(16 << 30),
                cache_dir: None,
                cancelled: &AtomicBool::new(false),
                on_checkpoint: None,
            },
        );
        assert!(one_file.is_ok());

        // A budget one file fits but four files do not.
        let err = open(
            dir.path(),
            OpenRequest {
                overrides: IngestOverrides::default(),
                budget: RamBudget::from_total_ram_bytes(4 * 50_000),
                cache_dir: None,
                cancelled: &AtomicBool::new(false),
                on_checkpoint: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, GlydeError::BudgetExceeded { .. }), "{err:?}");
    }

    #[test]
    fn hive_values_are_percent_decoded_and_the_null_partition_is_an_empty_label() {
        let dir = tempfile::tempdir().unwrap();
        for at in ["10%3A00", "__HIVE_DEFAULT_PARTITION__"] {
            write_at(
                &dir.path().join(format!("at={at}/part-0.parquet")),
                vec![("t", seconds(1)), ("v", values(1, 0.0))],
                None,
            );
        }

        let (_summary, _report, dataset) = open_default(dir.path());

        // `_` sorts after digits: the null partition comes second.
        assert_eq!(
            series(&dataset, "at").values(),
            &SeriesValues::String(vec!["10:00".to_string(), String::new()])
        );
        assert_eq!(percent_decode("a%2Fb%"), "a/b%", "a stray % is kept");
        assert_eq!(percent_decode("%FF"), "%FF", "not UTF-8: left as written");
    }

    #[test]
    fn a_partition_key_that_is_also_a_column_refuses_the_open() {
        let dir = tempfile::tempdir().unwrap();
        write_at(
            &dir.path().join("v=1/part-0.parquet"),
            vec![("t", seconds(1)), ("v", values(1, 0.0))],
            None,
        );

        let err = open_with(dir.path(), IngestOverrides::default()).unwrap_err();

        assert!(
            matches!(&err, GlydeError::InvalidParquet { reason, .. } if reason.contains("`v`")),
            "{err:?}"
        );
    }

    fn small_row_groups(rows: usize) -> WriterProperties {
        WriterProperties::builder()
            .set_max_row_group_size(rows)
            .build()
    }

    /// Issue #162: an integer column carries its row groups' exact min/max,
    /// and the pyramid built with them is the pyramid a scan builds.
    #[test]
    fn integer_columns_carry_their_row_group_statistics_into_the_pyramid() {
        let dir = tempfile::tempdir().unwrap();
        let rows = 64 * 20 + 5;
        let counts: ArrayRef = Arc::new(Int64Array::from(
            (0..rows as i64)
                .map(|i| (i % 7 != 3).then_some((i * 7919) % 1013 - 500))
                .collect::<Vec<_>>(),
        ));
        let path = write_with(
            &dir,
            vec![
                ("t", seconds(rows as i64)),
                ("count", counts),
                ("level", values(rows as i64, 0.5)),
            ],
            Some(small_row_groups(64)),
        );

        let (_summary, _report, dataset) = open_default(&path);

        let count = series(&dataset, "count");
        let stats = count.row_group_stats().expect("an integer column has them");
        assert_eq!(stats.groups().len(), 21);
        assert_eq!(stats.groups()[0].rows, 64);
        assert_eq!(stats.groups()[20].rows, 5);
        assert!(
            series(&dataset, "level").row_group_stats().is_none(),
            "float statistics do not count NaN, so a float column has none"
        );

        let ticks = dataset.time.to_pyramid_ticks();
        let samples = count.values().to_f64_vec().unwrap();
        let (_, first_level) = crate::index::row_groups::build_pyramid_with_row_groups(
            &samples,
            &ticks,
            &count.anomalies().nan_runs,
            stats,
        );
        assert_eq!(first_level, Ok(1), "64-row groups line up with level 1");
        let pyramids = crate::ingest::pyramids_for_dataset(&dataset);
        let index = dataset
            .columns
            .iter()
            .position(|c| c.name() == "count")
            .unwrap();
        assert_eq!(
            pyramids[index].as_ref().unwrap(),
            &crate::dsp::decimation::build_pyramid(&samples, &ticks)
        );
    }

    #[test]
    fn row_group_statistics_of_a_directory_run_across_its_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        for (part, first) in [(0, 0), (1, 100)] {
            let counts: ArrayRef =
                Arc::new(Int32Array::from((first..first + 128).collect::<Vec<i32>>()));
            write_at(
                &dir.path().join(format!("part-{part}.parquet")),
                vec![("t", seconds(128)), ("count", counts)],
                Some(small_row_groups(64)),
            );
        }

        let (_summary, _report, dataset) = open_default(dir.path());

        let groups = series(&dataset, "count")
            .row_group_stats()
            .unwrap()
            .groups();
        let bounds: Vec<_> = groups.iter().map(|g| (g.rows, g.min, g.max)).collect();
        assert_eq!(
            bounds,
            [
                (64, Some(0.0), Some(63.0)),
                (64, Some(64.0), Some(127.0)),
                (64, Some(100.0), Some(163.0)),
                (64, Some(164.0), Some(227.0)),
            ]
        );
    }

    #[test]
    fn a_file_written_without_statistics_has_none_to_carry() {
        let dir = tempfile::tempdir().unwrap();
        let counts: ArrayRef = Arc::new(Int32Array::from((0..128).collect::<Vec<i32>>()));
        let properties = WriterProperties::builder()
            .set_max_row_group_size(64)
            .set_statistics_enabled(parquet::file::properties::EnabledStatistics::None)
            .build();
        let path = write_with(
            &dir,
            vec![("t", seconds(128)), ("count", counts)],
            Some(properties),
        );

        let (_summary, _report, dataset) = open_default(&path);

        assert!(series(&dataset, "count").row_group_stats().is_none());
    }

    #[test]
    fn fixed_offsets_parse_in_every_arrow_spelling() {
        assert_eq!(parse_fixed_offset("UTC"), Some(0));
        assert_eq!(parse_fixed_offset("Z"), Some(0));
        assert_eq!(parse_fixed_offset("+02:00"), Some(7200));
        assert_eq!(parse_fixed_offset("-0530"), Some(-19_800));
        assert_eq!(parse_fixed_offset("+09"), Some(32_400));
        assert_eq!(parse_fixed_offset("+25:00"), None);
        assert_eq!(parse_fixed_offset("Europe/Rome"), None);
    }
}
