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

//! Parquet reader (docs/SPEC.md §1.1 `.parquet`, docs/ARCHITECTURE.md
//! §Parquet, docs/ROADMAP.md M7 "Parquet reader behind the `Reader` trait").
//!
//! Row-group access through `parquet`/`arrow`: [`inspect`] reads only the
//! time column (a projection, never the whole table) and summarizes it with
//! the same `time::summarize_ticks` the delimited-text path uses, so a
//! Parquet file and the equivalent CSV agree on sampling class, gap count,
//! and monotonicity by construction. [`map_dtype`] is SPEC §1.4's dtype
//! mapping: every Parquet/Arrow primitive keeps its native width — nothing is
//! upcast (Golden Rule 1). [`read_column`] materializes one column in that
//! native dtype.
//!
//! Not covered yet, each its own M7 item: nulls and dictionary-encoded
//! strings (a column containing either is *rejected loudly* with
//! [`GlydeError::Parquet`], never silently filled), partitioned directories,
//! and row-group statistics feeding the pyramid.

use super::report::{OpenSummary, SamplingClass};
use crate::series::{Dtype, SeriesValues};
use crate::time::{summarize_ticks, TimeUnit};
use crate::{GlydeError, Result};
use ::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use ::parquet::arrow::ProjectionMask;
use arrow::array::{Array, ArrayRef, AsArray};
use arrow::datatypes::{
    DataType, Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type,
    TimeUnit as ArrowUnit, UInt16Type, UInt32Type, UInt64Type, UInt8Type,
};
use std::fs::File;
use std::path::Path;
use tracing::info;

/// `.expected.json`'s `timestamp_format` label for a Parquet timestamp
/// column: the type is declared by the file, not inferred from text.
const PARQUET_TIMESTAMP_LABEL: &str = "parquet_timestamp";

/// Recognizes `.parquet` files.
pub struct ParquetReader;

impl super::Reader for ParquetReader {
    fn format_name(&self) -> &'static str {
        "parquet"
    }

    fn recognizes(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("parquet"))
    }
}

fn parquet_error(path: &Path, reason: impl std::fmt::Display) -> GlydeError {
    GlydeError::Parquet {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

/// SPEC §1.4's dtype for an Arrow column type, or `None` when v1 has no
/// native representation for it. Timestamps are the time index's business
/// ([`timestamp_unit`]), not a series dtype.
pub fn map_dtype(data_type: &DataType) -> Option<Dtype> {
    Some(match data_type {
        DataType::Boolean => Dtype::Bool,
        DataType::Int8 => Dtype::I8,
        DataType::Int16 => Dtype::I16,
        DataType::Int32 => Dtype::I32,
        DataType::Int64 => Dtype::I64,
        DataType::UInt8 => Dtype::U8,
        DataType::UInt16 => Dtype::U16,
        DataType::UInt32 => Dtype::U32,
        DataType::UInt64 => Dtype::U64,
        DataType::Float32 => Dtype::F32,
        DataType::Float64 => Dtype::F64,
        DataType::Utf8 | DataType::LargeUtf8 => Dtype::String,
        _ => return None,
    })
}

/// The native tick unit of an Arrow timestamp column (SPEC §2.1: keep the
/// resolution the source declares).
pub fn timestamp_unit(data_type: &DataType) -> Option<TimeUnit> {
    match data_type {
        DataType::Timestamp(ArrowUnit::Second, _) => Some(TimeUnit::Seconds),
        DataType::Timestamp(ArrowUnit::Millisecond, _) => Some(TimeUnit::Milliseconds),
        DataType::Timestamp(ArrowUnit::Microsecond, _) => Some(TimeUnit::Microseconds),
        DataType::Timestamp(ArrowUnit::Nanosecond, _) => Some(TimeUnit::Nanoseconds),
        _ => None,
    }
}

fn builder(path: &Path) -> Result<ParquetRecordBatchReaderBuilder<File>> {
    let file = File::open(path).map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| parquet_error(path, e))
}

fn reject_nulls(path: &Path, name: &str, array: &dyn Array) -> Result<()> {
    if array.null_count() > 0 {
        return Err(parquet_error(
            path,
            format!(
                "column {name:?} contains {} null value(s); null handling is not supported yet",
                array.null_count()
            ),
        ));
    }
    Ok(())
}

/// Reads column `index` of `path` in its native dtype, batch by batch.
pub fn read_column(path: &Path, index: usize) -> Result<SeriesValues> {
    let builder = builder(path)?;
    let field = builder
        .schema()
        .fields()
        .get(index)
        .cloned()
        .ok_or_else(|| parquet_error(path, format!("no column at position {index}")))?;
    let dtype = map_dtype(field.data_type()).ok_or_else(|| {
        parquet_error(
            path,
            format!(
                "column {:?} has unsupported type {}",
                field.name(),
                field.data_type()
            ),
        )
    })?;
    let mask = ProjectionMask::roots(builder.parquet_schema(), [index]);
    let reader = builder
        .with_projection(mask)
        .build()
        .map_err(|e| parquet_error(path, e))?;

    macro_rules! collect {
        ($variant:ident, $ty:ty) => {{
            let mut out = Vec::new();
            for batch in reader {
                let batch = batch.map_err(|e| parquet_error(path, e))?;
                let array = batch.column(0);
                reject_nulls(path, field.name(), array.as_ref())?;
                out.extend_from_slice(array.as_primitive::<$ty>().values());
            }
            SeriesValues::$variant(out)
        }};
    }

    Ok(match dtype {
        Dtype::Bool => {
            let mut out = Vec::new();
            for batch in reader {
                let batch = batch.map_err(|e| parquet_error(path, e))?;
                let array = batch.column(0);
                reject_nulls(path, field.name(), array.as_ref())?;
                out.extend(array.as_boolean().iter().flatten());
            }
            SeriesValues::Bool(out)
        }
        Dtype::I8 => collect!(I8, Int8Type),
        Dtype::I16 => collect!(I16, Int16Type),
        Dtype::I32 => collect!(I32, Int32Type),
        Dtype::I64 => collect!(I64, Int64Type),
        Dtype::U8 => collect!(U8, UInt8Type),
        Dtype::U16 => collect!(U16, UInt16Type),
        Dtype::U32 => collect!(U32, UInt32Type),
        Dtype::U64 => collect!(U64, UInt64Type),
        Dtype::F32 => collect!(F32, Float32Type),
        Dtype::F64 => collect!(F64, Float64Type),
        Dtype::String => {
            let mut out = Vec::new();
            for batch in reader {
                let batch = batch.map_err(|e| parquet_error(path, e))?;
                let array = batch.column(0);
                reject_nulls(path, field.name(), array.as_ref())?;
                match array.data_type() {
                    DataType::LargeUtf8 => {
                        out.extend(array.as_string::<i64>().iter().flatten().map(str::to_owned))
                    }
                    _ => out.extend(array.as_string::<i32>().iter().flatten().map(str::to_owned)),
                }
            }
            SeriesValues::String(out)
        }
    })
}

/// A timestamp column's raw ticks (`i128`, native unit, never `f64`).
fn read_ticks(path: &Path, index: usize, name: &str) -> Result<Vec<i128>> {
    let builder = builder(path)?;
    let mask = ProjectionMask::roots(builder.parquet_schema(), [index]);
    let reader = builder
        .with_projection(mask)
        .build()
        .map_err(|e| parquet_error(path, e))?;
    let mut ticks = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| parquet_error(path, e))?;
        let array: &ArrayRef = batch.column(0);
        reject_nulls(path, name, array.as_ref())?;
        let raw = match array.data_type() {
            DataType::Timestamp(ArrowUnit::Second, _) => array
                .as_primitive::<arrow::datatypes::TimestampSecondType>()
                .values()
                .to_vec(),
            DataType::Timestamp(ArrowUnit::Millisecond, _) => array
                .as_primitive::<arrow::datatypes::TimestampMillisecondType>()
                .values()
                .to_vec(),
            DataType::Timestamp(ArrowUnit::Microsecond, _) => array
                .as_primitive::<arrow::datatypes::TimestampMicrosecondType>()
                .values()
                .to_vec(),
            _ => array
                .as_primitive::<arrow::datatypes::TimestampNanosecondType>()
                .values()
                .to_vec(),
        };
        ticks.extend(raw.into_iter().map(i128::from));
    }
    Ok(ticks)
}

/// The [`OpenSummary`] of the Parquet file at `path` (the corpus gate's
/// view, docs/QUALITY.md §1). The time index is the first timestamp column;
/// a file with none gets SPEC §2.1's generated row index, like CSV does.
/// A file with no column besides the time index is
/// [`GlydeError::SingleColumnFile`], as for CSV.
pub fn inspect(path: &Path) -> Result<OpenSummary> {
    let builder = builder(path)?;
    let schema = builder.schema().clone();
    let row_count = builder.metadata().file_metadata().num_rows().max(0) as u64;
    drop(builder);

    if schema.fields().is_empty() || row_count == 0 {
        return Err(GlydeError::EmptyFile);
    }
    let time_index = schema
        .fields()
        .iter()
        .position(|f| timestamp_unit(f.data_type()).is_some());
    if schema.fields().len() == 1 && time_index.is_some() {
        return Err(GlydeError::SingleColumnFile);
    }
    for field in schema.fields() {
        let usable =
            map_dtype(field.data_type()).is_some() || timestamp_unit(field.data_type()).is_some();
        if !usable {
            info!(column = %field.name(), data_type = %field.data_type(), "unsupported Parquet column type");
        }
    }

    let (time_column, timestamp_format, sampling_class, gap_count, non_monotonic, duplicates) =
        match time_index {
            Some(index) => {
                let name = schema.field(index).name().clone();
                let ticks = read_ticks(path, index, &name)?;
                let stats = summarize_ticks(ticks.as_slice())?;
                info!(
                    column = %name,
                    rows = ticks.len(),
                    "parquet: time index is a timestamp column"
                );
                (
                    Some(name),
                    Some(PARQUET_TIMESTAMP_LABEL.to_string()),
                    stats.sampling_class.into(),
                    stats.gap_count as u64,
                    stats.monotonicity.non_monotonic_count as u64,
                    stats.monotonicity.duplicate_count as u64,
                )
            }
            None => {
                info!("parquet: no timestamp column; using the generated row index");
                (None, None, SamplingClass::ProgressiveIndex, 0, 0, 0)
            }
        };

    Ok(OpenSummary {
        encoding: "n/a".to_string(),
        delimiter: None,
        decimal_separator: None,
        time_column,
        timestamp_format,
        row_count,
        skipped_row_count: 0,
        sampling_class,
        gap_count,
        non_monotonic_count: non_monotonic,
        duplicate_timestamp_count: duplicates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::Reader as _;
    use ::parquet::arrow::ArrowWriter;
    use arrow::array::{
        BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, Int8Array,
        RecordBatch, StringArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array,
    };
    use arrow::datatypes::{Field, Schema};
    use std::sync::Arc;

    fn write(dir: &Path, name: &str, columns: Vec<(&str, ArrayRef)>) -> std::path::PathBuf {
        let fields: Vec<Field> = columns
            .iter()
            .map(|(n, a)| Field::new(*n, a.data_type().clone(), true))
            .collect();
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(
            schema.clone(),
            columns.into_iter().map(|(_, a)| a).collect(),
        )
        .unwrap();
        let path = dir.join(name);
        let mut writer = ArrowWriter::try_new(File::create(&path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        path
    }

    #[test]
    fn recognizes_parquet_case_insensitively_and_not_csv() {
        assert!(ParquetReader.recognizes(Path::new("a.parquet")));
        assert!(ParquetReader.recognizes(Path::new("a.PARQUET")));
        assert!(!ParquetReader.recognizes(Path::new("a.csv")));
        assert_eq!(ParquetReader.format_name(), "parquet");
    }

    #[test]
    fn dtype_mapping_keeps_every_native_width() {
        let cases = [
            (DataType::Boolean, Dtype::Bool),
            (DataType::Int8, Dtype::I8),
            (DataType::Int16, Dtype::I16),
            (DataType::Int32, Dtype::I32),
            (DataType::Int64, Dtype::I64),
            (DataType::UInt8, Dtype::U8),
            (DataType::UInt16, Dtype::U16),
            (DataType::UInt32, Dtype::U32),
            (DataType::UInt64, Dtype::U64),
            (DataType::Float32, Dtype::F32),
            (DataType::Float64, Dtype::F64),
            (DataType::Utf8, Dtype::String),
        ];
        for (arrow, expected) in cases {
            assert_eq!(map_dtype(&arrow), Some(expected), "{arrow}");
        }
        assert_eq!(map_dtype(&DataType::Date32), None);
    }

    #[test]
    fn timestamp_units_map_to_native_tick_units() {
        let unit = |u| timestamp_unit(&DataType::Timestamp(u, None));
        assert_eq!(unit(ArrowUnit::Second), Some(TimeUnit::Seconds));
        assert_eq!(unit(ArrowUnit::Millisecond), Some(TimeUnit::Milliseconds));
        assert_eq!(unit(ArrowUnit::Microsecond), Some(TimeUnit::Microseconds));
        assert_eq!(unit(ArrowUnit::Nanosecond), Some(TimeUnit::Nanoseconds));
        assert_eq!(timestamp_unit(&DataType::Int64), None);
    }

    #[test]
    fn read_column_round_trips_every_dtype_without_upcasting() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "all.parquet",
            vec![
                (
                    "b",
                    Arc::new(BooleanArray::from(vec![true, false])) as ArrayRef,
                ),
                ("i8", Arc::new(Int8Array::from(vec![-1, 2]))),
                ("i16", Arc::new(Int16Array::from(vec![-1, 2]))),
                ("i32", Arc::new(Int32Array::from(vec![-1, 2]))),
                ("i64", Arc::new(Int64Array::from(vec![i64::MIN, i64::MAX]))),
                ("u8", Arc::new(UInt8Array::from(vec![1, 255]))),
                ("u16", Arc::new(UInt16Array::from(vec![1, 65535]))),
                ("u32", Arc::new(UInt32Array::from(vec![1, u32::MAX]))),
                ("u64", Arc::new(UInt64Array::from(vec![1, u64::MAX]))),
                ("f32", Arc::new(Float32Array::from(vec![0.1, 0.2]))),
                ("f64", Arc::new(Float64Array::from(vec![0.1, f64::NAN]))),
                ("s", Arc::new(StringArray::from(vec!["on", "off"]))),
            ],
        );
        let get = |i| read_column(&path, i).unwrap();
        assert!(matches!(get(0), SeriesValues::Bool(v) if v == [true, false]));
        assert!(matches!(get(1), SeriesValues::I8(v) if v == [-1, 2]));
        assert!(matches!(get(2), SeriesValues::I16(v) if v == [-1, 2]));
        assert!(matches!(get(3), SeriesValues::I32(v) if v == [-1, 2]));
        assert!(matches!(get(4), SeriesValues::I64(v) if v == [i64::MIN, i64::MAX]));
        assert!(matches!(get(5), SeriesValues::U8(v) if v == [1, 255]));
        assert!(matches!(get(6), SeriesValues::U16(v) if v == [1, 65535]));
        assert!(matches!(get(7), SeriesValues::U32(v) if v == [1, u32::MAX]));
        assert!(matches!(get(8), SeriesValues::U64(v) if v == [1, u64::MAX]));
        assert!(matches!(get(9), SeriesValues::F32(v) if v == [0.1f32, 0.2f32]));
        assert!(matches!(get(10), SeriesValues::F64(v) if v[0] == 0.1 && v[1].is_nan()));
        assert!(matches!(get(11), SeriesValues::String(v) if v == ["on", "off"]));
    }

    #[test]
    fn a_column_with_nulls_is_rejected_loudly_not_filled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "nulls.parquet",
            vec![(
                "v",
                Arc::new(Float64Array::from(vec![Some(1.0), None])) as ArrayRef,
            )],
        );
        let err = read_column(&path, 0).expect_err("nulls must not be silently filled");
        assert!(matches!(err, GlydeError::Parquet { .. }), "{err:?}");
    }

    #[test]
    fn inspect_without_a_timestamp_column_uses_the_generated_row_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "plain.parquet",
            vec![(
                "v",
                Arc::new(Float64Array::from(vec![1.0, 2.0, 3.0])) as ArrayRef,
            )],
        );
        let summary = inspect(&path).unwrap();
        assert_eq!(summary.time_column, None);
        assert_eq!(summary.sampling_class, SamplingClass::ProgressiveIndex);
        assert_eq!(summary.row_count, 3);
    }

    #[test]
    fn inspect_of_a_timestamp_only_file_is_a_single_column_error() {
        use arrow::array::TimestampMillisecondArray;
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "t.parquet",
            vec![(
                "t",
                Arc::new(TimestampMillisecondArray::from(vec![0, 1000])) as ArrayRef,
            )],
        );
        assert!(matches!(inspect(&path), Err(GlydeError::SingleColumnFile)));
    }

    #[test]
    fn inspect_detects_a_gap_in_a_timestamp_column() {
        use arrow::array::TimestampMillisecondArray;
        let dir = tempfile::tempdir().unwrap();
        let mut ts: Vec<i64> = (0..20).map(|i| i * 1000).collect();
        ts.extend((0..20).map(|i| 1_000_000 + i * 1000));
        let n = ts.len();
        let path = write(
            dir.path(),
            "gap.parquet",
            vec![
                (
                    "t",
                    Arc::new(TimestampMillisecondArray::from(ts)) as ArrayRef,
                ),
                ("v", Arc::new(Float64Array::from(vec![0.0; n]))),
            ],
        );
        let summary = inspect(&path).unwrap();
        assert_eq!(summary.gap_count, 1);
        assert_eq!(summary.sampling_class, SamplingClass::SegmentedUniform);
    }
}
