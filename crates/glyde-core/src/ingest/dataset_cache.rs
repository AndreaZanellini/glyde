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

//! Persistent whole-dataset cache (issue #106): reopening a known file skips
//! the text parse entirely, not only the derived caches.
//!
//! What is stored is everything a parse produces that the open needs, and
//! nothing derived from it: the [`CsvParseOutcome`] (encoding, delimiter,
//! decimal separator, their confidences, row tallies, skipped-row details),
//! the time-index decision, and the [`Dataset`] itself — typed time axis and
//! every column in its own dtype, strings byte for byte, with its anomalies.
//! The inference report and summary are then rebuilt from those exactly as
//! after a parse, so a reopen can never show a stale report (Golden Rule 2):
//! the report is not cached, its inputs are.
//!
//! Keyed like every other cache by source path + size + mtime + overrides
//! signature ([`CacheKey`]), so an edited file or a different correction is
//! a miss. It **fails closed**: a missing, foreign-version, truncated or
//! inconsistent file is a miss (logged at `warn` when it was there but
//! unusable), and the open parses the source as if there were no cache. A
//! cache is also not used when holding it would not fit the RAM budget
//! today. Only datasets opened in memory are cached; a file too large for
//! memory spills to scratch storage that is not kept (see issue #102).
//!
//! Format, one file per key: an 8-byte magic, a `u32` format version, 4
//! reserved bytes, a `u64` metadata length, the metadata as JSON, then each
//! array's raw native-endian bytes in the order the metadata lists them. The
//! metadata types below are private mirrors of the core types rather than
//! `serde` derives on the core types themselves, so the persisted format only
//! changes when this module (and its version) does.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::csv::{CsvParseOutcome, SkipReason, SkippedRowDetail};
use super::dataset::{
    Dataset, GeneratedIndexReason, ProgressiveValues, TimeAxis, TimeIndexInference, Timestamps,
};
use super::infer::{Confidence, DecimalSeparator, Delimiter};
use crate::budget::RamBudget;
use crate::index::level0::CacheKey;
use crate::series::{Anomalies, Series, SeriesValues};
use crate::time::{TimeUnit, Timestamp, TimestampFormat};
use crate::{GlydeError, Result};

const MAGIC: &[u8; 8] = b"GLYDEDS\0";
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 24;

/// Where a key's cache file lives under `cache_dir`.
fn cache_path(cache_dir: &Path, key: &CacheKey) -> PathBuf {
    cache_dir
        .join("datasets")
        .join(format!("{}.glyds", key.cache_stem()))
}

/// Everything a parse produced, as the open consumes it.
pub(crate) type Parsed = (CsvParseOutcome, Dataset, TimeIndexInference);

/// The cached parse for `key`, or `None` on any miss (see the module docs).
pub(crate) fn try_load(cache_dir: &Path, key: &CacheKey, budget: RamBudget) -> Option<Parsed> {
    let path = cache_path(cache_dir, key);
    let file = File::open(&path).ok()?;
    let len = file.metadata().ok()?.len();
    if !budget.affords(len) {
        info!(
            path = %path.display(),
            bytes = len,
            "dataset cache found but holding it would exceed the RAM budget; parsing instead"
        );
        return None;
    }
    match read(BufReader::new(file), len) {
        Ok(parsed) => {
            info!(
                path = %path.display(),
                rows = parsed.0.row_count,
                "dataset cache hit: reopening without reading the source file (issue #106)"
            );
            Some(parsed)
        }
        Err(reason) => {
            warn!(
                path = %path.display(),
                reason = %reason,
                "dataset cache unusable; parsing the source file instead"
            );
            None
        }
    }
}

/// Writes `parsed` as `key`'s cache. A failure is logged and otherwise
/// ignored: the cache only ever saves time, the open already succeeded.
pub(crate) fn store(
    cache_dir: &Path,
    key: &CacheKey,
    parsed: (&CsvParseOutcome, &Dataset, &TimeIndexInference),
) {
    if parsed.1.is_spilled() {
        return;
    }
    let path = cache_path(cache_dir, key);
    match write(&path, parsed) {
        Ok(()) => info!(path = %path.display(), "dataset cached for instant reopen (issue #106)"),
        Err(error) => warn!(
            path = %path.display(),
            error = %error,
            "could not write the dataset cache; the next open of this file parses it again"
        ),
    }
}

// --- Metadata ---------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Meta {
    outcome: OutcomeMeta,
    inference: InferenceMeta,
    time_column_name: String,
    rows: usize,
    /// `Some(format)` for an absolute axis, `None` for a progressive one.
    time_format: Option<String>,
    columns: Vec<ColumnMeta>,
}

#[derive(Serialize, Deserialize)]
struct OutcomeMeta {
    column_names: Vec<String>,
    row_count: u64,
    skipped_row_count: u64,
    skipped_rows: Vec<SkippedMeta>,
    encoding_label: String,
    delimiter: String,
    decimal_separator: String,
    encoding_confidence: bool,
    delimiter_confidence: bool,
    decimal_separator_confidence: bool,
    header_ambiguous: bool,
}

#[derive(Serialize, Deserialize)]
struct SkippedMeta {
    line_number: usize,
    /// `Some((expected, found))` for a field-count mismatch.
    field_count: Option<(usize, usize)>,
    message: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct InferenceMeta {
    timestamp_format_ambiguous: bool,
    time_column: Option<usize>,
    generated: Option<GeneratedMeta>,
}

#[derive(Serialize, Deserialize)]
enum GeneratedMeta {
    Requested,
    NotMonotonic { column: String, row: u64 },
    Constant { column: String },
    MostlyRepeated { column: String, changes: u64 },
    Unreadable { column: String },
    MissingTimestamps { column: String, row: u64 },
}

#[derive(Serialize, Deserialize)]
struct ColumnMeta {
    name: String,
    dtype: String,
    nan_runs: Vec<(usize, usize)>,
    outliers: Vec<usize>,
    skipped_rows: Vec<usize>,
}

/// One row of an absolute time axis, laid out without padding.
#[derive(Clone, Copy)]
#[repr(C)]
struct PackedTimestamp {
    ticks: i128,
    offset_seconds: i64,
    /// [`unit_code`]; `i64::MIN` in `offset_seconds` means no offset.
    unit: u64,
}

// SAFETY: `repr(C)`, every field a plain integer, and no padding: 16 + 8 + 8
// bytes at offsets 0, 16 and 24, total 32 — a multiple of the 16-byte
// alignment. Any bit pattern is a valid value, and all-zero is one too.
unsafe impl Zeroable for PackedTimestamp {}
unsafe impl Pod for PackedTimestamp {}
const _: () = assert!(std::mem::size_of::<PackedTimestamp>() == 32);

const NO_OFFSET: i64 = i64::MIN;

fn confidence_high(confidence: Confidence) -> bool {
    confidence == Confidence::High
}

fn confidence(high: bool) -> Confidence {
    if high {
        Confidence::High
    } else {
        Confidence::Low
    }
}

const DELIMITERS: [Delimiter; 5] = [
    Delimiter::Comma,
    Delimiter::Semicolon,
    Delimiter::Tab,
    Delimiter::Pipe,
    Delimiter::Whitespace,
];

const FORMATS: [(TimestampFormat, &str); 12] = [
    (TimestampFormat::Iso8601WithOffset, "iso8601-offset"),
    (TimestampFormat::Iso8601Naive, "iso8601-naive"),
    (TimestampFormat::DateTimeSpace, "datetime-space"),
    (TimestampFormat::DayFirst, "day-first"),
    (TimestampFormat::MonthFirst, "month-first"),
    (TimestampFormat::EpochSeconds, "epoch-s"),
    (TimestampFormat::EpochMillis, "epoch-ms"),
    (TimestampFormat::EpochMicros, "epoch-us"),
    (TimestampFormat::EpochNanos, "epoch-ns"),
    (TimestampFormat::LabViewEpoch, "labview"),
    (TimestampFormat::ExcelSerial, "excel"),
    (TimestampFormat::ParquetTimestamp, "parquet"),
];

const UNITS: [TimeUnit; 5] = [
    TimeUnit::Seconds,
    TimeUnit::Milliseconds,
    TimeUnit::Microseconds,
    TimeUnit::Nanoseconds,
    TimeUnit::Picoseconds,
];

fn unit_code(unit: TimeUnit) -> u64 {
    UNITS.iter().position(|&u| u == unit).unwrap_or(0) as u64
}

fn format_name(format: TimestampFormat) -> &'static str {
    FORMATS
        .iter()
        .find(|(f, _)| *f == format)
        .map_or("unknown", |(_, name)| name)
}

fn bad(reason: impl Into<String>) -> GlydeError {
    GlydeError::CorruptCache {
        path: PathBuf::from("<dataset cache>"),
        reason: reason.into(),
    }
}

// --- Write -------------------------------------------------------------------

fn write(
    path: &Path,
    (outcome, dataset, inference): (&CsvParseOutcome, &Dataset, &TimeIndexInference),
) -> Result<()> {
    let io = |source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    };
    let dir = path.parent().expect("cache files live in a directory");
    std::fs::create_dir_all(dir).map_err(io)?;

    let rows = dataset.time.len();
    let time_format = match &dataset.time {
        TimeAxis::Absolute { format, .. } => Some(format_name(*format).to_string()),
        TimeAxis::Progressive { .. } => None,
    };
    let columns = dataset
        .columns
        .iter()
        .map(|series| {
            let anomalies = series.anomalies();
            Ok(ColumnMeta {
                name: series.name().to_string(),
                dtype: dtype_name(series.values())?.to_string(),
                nan_runs: anomalies
                    .nan_runs
                    .iter()
                    .map(|r| (r.start, r.end))
                    .collect(),
                outliers: anomalies.outliers.clone(),
                skipped_rows: anomalies.skipped_rows.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let meta = Meta {
        outcome: OutcomeMeta {
            column_names: outcome.column_names.clone(),
            row_count: outcome.row_count,
            skipped_row_count: outcome.skipped_row_count,
            skipped_rows: outcome
                .skipped_row_details
                .iter()
                .map(|detail| match &detail.reason {
                    SkipReason::FieldCountMismatch { expected, found } => SkippedMeta {
                        line_number: detail.line_number,
                        field_count: Some((*expected, *found)),
                        message: None,
                    },
                    SkipReason::Unparseable { message } => SkippedMeta {
                        line_number: detail.line_number,
                        field_count: None,
                        message: Some(message.clone()),
                    },
                })
                .collect(),
            encoding_label: outcome.encoding_label.clone(),
            delimiter: outcome.delimiter.as_str().to_string(),
            decimal_separator: outcome.decimal_separator.as_str().to_string(),
            encoding_confidence: confidence_high(outcome.encoding_confidence),
            delimiter_confidence: confidence_high(outcome.delimiter_confidence),
            decimal_separator_confidence: confidence_high(outcome.decimal_separator_confidence),
            header_ambiguous: outcome.header_ambiguous,
        },
        inference: InferenceMeta {
            timestamp_format_ambiguous: inference.timestamp_format_ambiguous,
            time_column: inference.time_column,
            generated: inference
                .generated
                .as_ref()
                .map(|reason| match reason.clone() {
                    GeneratedIndexReason::Requested => GeneratedMeta::Requested,
                    GeneratedIndexReason::NotMonotonic { column, row } => {
                        GeneratedMeta::NotMonotonic { column, row }
                    }
                    GeneratedIndexReason::Constant { column } => GeneratedMeta::Constant { column },
                    GeneratedIndexReason::MostlyRepeated { column, changes } => {
                        GeneratedMeta::MostlyRepeated { column, changes }
                    }
                    GeneratedIndexReason::Unreadable { column } => {
                        GeneratedMeta::Unreadable { column }
                    }
                    GeneratedIndexReason::MissingTimestamps { column, row } => {
                        GeneratedMeta::MissingTimestamps { column, row }
                    }
                }),
        },
        time_column_name: dataset.time_column_name.clone(),
        rows,
        time_format,
        columns,
    };
    let meta = serde_json::to_vec(&meta).map_err(|err| bad(err.to_string()))?;

    let tmp = path.with_extension(format!("glyds.{}.tmp", std::process::id()));
    let mut out = BufWriter::new(File::create(&tmp).map_err(io)?);
    let result = (|| -> std::io::Result<()> {
        out.write_all(MAGIC)?;
        out.write_all(&FORMAT_VERSION.to_ne_bytes())?;
        out.write_all(&[0; 4])?;
        out.write_all(&(meta.len() as u64).to_ne_bytes())?;
        out.write_all(&meta)?;
        match &dataset.time {
            TimeAxis::Absolute {
                timestamps: Timestamps::Memory(timestamps),
                ..
            } => {
                for chunk in timestamps.chunks(4096) {
                    let packed: Vec<PackedTimestamp> = chunk
                        .iter()
                        .map(|t| PackedTimestamp {
                            ticks: t.ticks,
                            offset_seconds: t.offset_seconds.map_or(NO_OFFSET, i64::from),
                            unit: unit_code(t.unit),
                        })
                        .collect();
                    out.write_all(bytemuck::cast_slice(&packed))?;
                }
            }
            TimeAxis::Progressive {
                values: ProgressiveValues::Memory(values),
            } => out.write_all(bytemuck::cast_slice(values))?,
            _ => return Err(std::io::Error::other("a spilled time axis is not cached")),
        }
        for series in &dataset.columns {
            write_values(&mut out, series.values())?;
        }
        out.flush()?;
        out.get_ref().sync_all()
    })();
    drop(out);
    if let Err(source) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(source));
    }
    std::fs::rename(&tmp, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        io(source)
    })
}

fn dtype_name(values: &SeriesValues) -> Result<&'static str> {
    Ok(match values {
        SeriesValues::Bool(_) => "bool",
        SeriesValues::I8(_) => "i8",
        SeriesValues::I16(_) => "i16",
        SeriesValues::I32(_) => "i32",
        SeriesValues::I64(_) => "i64",
        SeriesValues::U8(_) => "u8",
        SeriesValues::U16(_) => "u16",
        SeriesValues::U32(_) => "u32",
        SeriesValues::U64(_) => "u64",
        SeriesValues::F32(_) => "f32",
        SeriesValues::F64(_) => "f64",
        SeriesValues::String(_) => "string",
        SeriesValues::Spilled(_) => return Err(bad("a spilled column is not cached")),
    })
}

fn write_values(out: &mut impl Write, values: &SeriesValues) -> std::io::Result<()> {
    match values {
        SeriesValues::Bool(v) => {
            let bytes: Vec<u8> = v.iter().map(|&b| u8::from(b)).collect();
            out.write_all(&bytes)
        }
        SeriesValues::I8(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::I16(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::I32(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::I64(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::U8(v) => out.write_all(v),
        SeriesValues::U16(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::U32(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::U64(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::F32(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::F64(v) => out.write_all(bytemuck::cast_slice(v)),
        SeriesValues::String(v) => {
            let mut end = 0u64;
            let ends: Vec<u64> = v
                .iter()
                .map(|s| {
                    end += s.len() as u64;
                    end
                })
                .collect();
            out.write_all(bytemuck::cast_slice(&ends))?;
            for s in v {
                out.write_all(s.as_bytes())?;
            }
            Ok(())
        }
        SeriesValues::Spilled(_) => Err(std::io::Error::other("a spilled column is not cached")),
    }
}

// --- Read --------------------------------------------------------------------

/// Reads exactly the bytes the file must contain, tracking how many remain
/// so a short or overlong file is caught, never misread.
struct Reader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Reader<R> {
    fn bytes(&mut self, buf: &mut [u8]) -> Result<()> {
        if (buf.len() as u64) > self.remaining {
            return Err(bad("the file is shorter than its metadata says"));
        }
        self.inner
            .read_exact(buf)
            .map_err(|err| bad(err.to_string()))?;
        self.remaining -= buf.len() as u64;
        Ok(())
    }

    fn array<T: Pod>(&mut self, len: usize) -> Result<Vec<T>> {
        let bytes = len
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| bad("array length overflows"))?;
        if bytes as u64 > self.remaining {
            return Err(bad("the file is shorter than its metadata says"));
        }
        let mut values = vec![T::zeroed(); len];
        self.bytes(bytemuck::cast_slice_mut(&mut values))?;
        Ok(values)
    }
}

fn read(inner: impl Read, file_len: u64) -> Result<Parsed> {
    let mut r = Reader {
        inner,
        remaining: file_len,
    };
    let mut header = [0u8; HEADER_LEN];
    r.bytes(&mut header)?;
    if &header[0..8] != MAGIC {
        return Err(bad("not a dataset cache file"));
    }
    let version = u32::from_ne_bytes(header[8..12].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION {
        return Err(bad(format!(
            "format version {version}, expected {FORMAT_VERSION}"
        )));
    }
    let meta_len = u64::from_ne_bytes(header[16..24].try_into().expect("8 bytes"));
    if meta_len > r.remaining {
        return Err(bad("metadata longer than the file"));
    }
    let mut meta = vec![0u8; meta_len as usize];
    r.bytes(&mut meta)?;
    let meta: Meta = serde_json::from_slice(&meta).map_err(|err| bad(err.to_string()))?;
    let rows = meta.rows;

    let time = match &meta.time_format {
        Some(name) => {
            let format = FORMATS
                .iter()
                .find(|(_, n)| n == name)
                .map(|(f, _)| *f)
                .ok_or_else(|| bad(format!("unknown timestamp format {name}")))?;
            let mut timestamps = Vec::with_capacity(rows);
            let mut left = rows;
            while left > 0 {
                let take = left.min(4096);
                for packed in r.array::<PackedTimestamp>(take)? {
                    let unit = *UNITS
                        .get(packed.unit as usize)
                        .ok_or_else(|| bad("unknown time unit"))?;
                    let offset_seconds = if packed.offset_seconds == NO_OFFSET {
                        None
                    } else {
                        Some(i32::try_from(packed.offset_seconds).map_err(|_| bad("offset"))?)
                    };
                    timestamps.push(Timestamp {
                        ticks: packed.ticks,
                        unit,
                        offset_seconds,
                    });
                }
                left -= take;
            }
            TimeAxis::Absolute {
                timestamps: Timestamps::Memory(timestamps),
                format,
            }
        }
        None => TimeAxis::Progressive {
            values: ProgressiveValues::Memory(r.array::<f64>(rows)?),
        },
    };

    let columns = meta
        .columns
        .iter()
        .map(|column| {
            let values = read_values(&mut r, &column.dtype, rows)?;
            let anomalies = Anomalies {
                nan_runs: column
                    .nan_runs
                    .iter()
                    .map(|&(start, end)| Range { start, end })
                    .collect(),
                outliers: column.outliers.clone(),
                skipped_rows: column.skipped_rows.clone(),
            };
            Ok(Series::with_anomalies(
                column.name.clone(),
                values,
                anomalies,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    if r.remaining != 0 {
        return Err(bad("trailing bytes after the last column"));
    }

    let outcome = &meta.outcome;
    let delimiter = DELIMITERS
        .into_iter()
        .find(|d| d.as_str() == outcome.delimiter)
        .ok_or_else(|| bad("unknown delimiter"))?;
    let decimal_separator = [DecimalSeparator::Dot, DecimalSeparator::Comma]
        .into_iter()
        .find(|d| d.as_str() == outcome.decimal_separator)
        .ok_or_else(|| bad("unknown decimal separator"))?;
    let outcome_out = CsvParseOutcome {
        column_names: outcome.column_names.clone(),
        row_count: outcome.row_count,
        skipped_row_count: outcome.skipped_row_count,
        skipped_row_details: outcome
            .skipped_rows
            .iter()
            .map(|skipped| SkippedRowDetail {
                line_number: skipped.line_number,
                reason: match (&skipped.field_count, &skipped.message) {
                    (Some((expected, found)), _) => SkipReason::FieldCountMismatch {
                        expected: *expected,
                        found: *found,
                    },
                    (None, message) => SkipReason::Unparseable {
                        message: message.clone().unwrap_or_default(),
                    },
                },
            })
            .collect(),
        encoding_label: outcome.encoding_label.clone(),
        delimiter,
        decimal_separator,
        encoding_confidence: confidence(outcome.encoding_confidence),
        delimiter_confidence: confidence(outcome.delimiter_confidence),
        decimal_separator_confidence: confidence(outcome.decimal_separator_confidence),
        header_ambiguous: outcome.header_ambiguous,
    };
    let inference = TimeIndexInference {
        timestamp_format_ambiguous: meta.inference.timestamp_format_ambiguous,
        time_column: meta.inference.time_column,
        generated: meta.inference.generated.map(|generated| match generated {
            GeneratedMeta::Requested => GeneratedIndexReason::Requested,
            GeneratedMeta::NotMonotonic { column, row } => {
                GeneratedIndexReason::NotMonotonic { column, row }
            }
            GeneratedMeta::Constant { column } => GeneratedIndexReason::Constant { column },
            GeneratedMeta::MostlyRepeated { column, changes } => {
                GeneratedIndexReason::MostlyRepeated { column, changes }
            }
            GeneratedMeta::Unreadable { column } => GeneratedIndexReason::Unreadable { column },
            GeneratedMeta::MissingTimestamps { column, row } => {
                GeneratedIndexReason::MissingTimestamps { column, row }
            }
        }),
    };
    let dataset = Dataset {
        time,
        time_column_name: meta.time_column_name,
        columns,
    };
    Ok((outcome_out, dataset, inference))
}

fn read_values(r: &mut Reader<impl Read>, dtype: &str, rows: usize) -> Result<SeriesValues> {
    Ok(match dtype {
        "bool" => SeriesValues::Bool(r.array::<u8>(rows)?.into_iter().map(|b| b != 0).collect()),
        "i8" => SeriesValues::I8(r.array(rows)?),
        "i16" => SeriesValues::I16(r.array(rows)?),
        "i32" => SeriesValues::I32(r.array(rows)?),
        "i64" => SeriesValues::I64(r.array(rows)?),
        "u8" => SeriesValues::U8(r.array(rows)?),
        "u16" => SeriesValues::U16(r.array(rows)?),
        "u32" => SeriesValues::U32(r.array(rows)?),
        "u64" => SeriesValues::U64(r.array(rows)?),
        "f32" => SeriesValues::F32(r.array(rows)?),
        "f64" => SeriesValues::F64(r.array(rows)?),
        "string" => {
            let ends: Vec<u64> = r.array(rows)?;
            let mut previous = 0u64;
            let mut strings = Vec::with_capacity(rows);
            for end in ends {
                let len = end
                    .checked_sub(previous)
                    .ok_or_else(|| bad("string offsets go backwards"))?;
                previous = end;
                let mut bytes = vec![0u8; usize::try_from(len).map_err(|_| bad("length"))?];
                r.bytes(&mut bytes)?;
                strings.push(String::from_utf8(bytes).map_err(|_| bad("string is not UTF-8"))?);
            }
            SeriesValues::String(strings)
        }
        other => return Err(bad(format!("unknown dtype {other}"))),
    })
}
