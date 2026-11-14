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

//! Issue #193, Parquet: the RAM estimate the in-memory/spill decision is
//! taken on (SPEC §5.1) must cover what an in-memory open really holds at its
//! peak — the open itself **and** the derived caches the app builds right
//! after it (min/max pyramids, Level-0 caches), on a mixed-dtype file.
//!
//! Before the fix, a 269 MB Parquet file estimated at 1.76 GB peaked at
//! 4.0 GB in the app, 98% of the 4 GB cap.
//!
//! Its own test binary: `VmHWM` is process-wide. Linux-only, like
//! `tests/peak_rss.rs`.

#![cfg(target_os = "linux")]

#[path = "support/in_memory_estimate.rs"]
mod in_memory_estimate;

use arrow::array::{
    ArrayRef, BooleanArray, DictionaryArray, Float32Array, Float64Array, Int16Array,
    TimestampNanosecondArray, UInt32Array,
};
use arrow::datatypes::Int8Type;
use arrow::record_batch::RecordBatch;
use in_memory_estimate::assert_estimate_covers_peak;
use parquet::arrow::ArrowWriter;
use std::sync::Arc;

const ROWS: usize = 1_200_000;
const BATCH_ROWS: usize = 100_000;

/// Nanosecond timestamps, `f32`/`f64`/`i16`/`u32` channels, a flag and a
/// dictionary-encoded state: the dtype mix of issue #193's gearbox file,
/// written in bounded batches so writing it costs little memory itself.
fn write_fixture(path: &std::path::Path) {
    const STATES: [&str; 4] = ["IDLE", "RAMP", "PRODUCTION", "STOP"];
    let batch = |start: usize| -> RecordBatch {
        let rows = start..start + BATCH_ROWS;
        let f32s = |scale: f32| -> ArrayRef {
            Arc::new(Float32Array::from_iter_values(
                rows.clone().map(|i| (i % 4093) as f32 * scale),
            ))
        };
        let states: DictionaryArray<Int8Type> = rows
            .clone()
            .map(|i| STATES[(i / 50_000) % STATES.len()])
            .collect();
        RecordBatch::try_from_iter([
            (
                "timestamp",
                Arc::new(TimestampNanosecondArray::from_iter_values(
                    rows.clone()
                        .map(|i| 1_770_000_000_000_000_000 + i as i64 * 40_000),
                )) as ArrayRef,
            ),
            ("accel_a", f32s(0.001)),
            ("accel_b", f32s(0.002)),
            ("power", f32s(0.5)),
            (
                "speed",
                Arc::new(Float64Array::from_iter_values(
                    rows.clone().map(|i| 1500.0 + (i % 977) as f64 * 0.1),
                )) as ArrayRef,
            ),
            (
                "adc",
                Arc::new(Int16Array::from_iter_values(
                    rows.clone().map(|i| (i % 30_011) as i16),
                )) as ArrayRef,
            ),
            (
                "burst",
                Arc::new(UInt32Array::from_iter_values(
                    rows.clone().map(|i| (i / 50_000) as u32),
                )) as ArrayRef,
            ),
            (
                "overrange",
                Arc::new(BooleanArray::from_iter(
                    rows.clone().map(|i| Some(i % 997 == 0)),
                )) as ArrayRef,
            ),
            ("mode", Arc::new(states) as ArrayRef),
        ])
        .expect("valid fixture batch")
    };
    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = ArrowWriter::try_new(file, batch(0).schema(), None).expect("parquet writer");
    for start in (0..ROWS).step_by(BATCH_ROWS) {
        writer.write(&batch(start)).expect("write batch");
    }
    writer.close().expect("close writer");
}

#[test]
fn a_mixed_parquet_estimate_covers_the_peak_of_its_open_and_derived_caches() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("mixed.parquet");
    write_fixture(&path);
    assert_estimate_covers_peak(&path, ROWS);
}
