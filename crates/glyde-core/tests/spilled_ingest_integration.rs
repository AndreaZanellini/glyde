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

//! Issue #75, Golden Rule 1 gate for the budget-driven spill path: opening a
//! file whose typed columns would not fit the RAM budget streams them to the
//! on-disk spill cache instead of materializing `Vec`-backed columns — and
//! the [`Dataset`](glyde_core::ingest::Dataset) that comes back must be
//! **indistinguishable** from the one the in-memory path produces for the
//! same file. Same values, same dtypes, same timestamps, same anomalies:
//! "this is a storage change, not a data change" (issue #75's decision
//! comment).
//!
//! Both paths are driven off the same fixture in the same test, with only the
//! [`RamBudget`] differing, so a divergence can only come from the storage
//! choice itself.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, TimeAxis};
use glyde_core::series::{Dtype, SeriesValues};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// A fixture exercising every dtype CSV ingestion can produce (SPEC §1.4 via
/// `ingest::infer::infer_column`: bool, i64, f64, string) plus an ISO 8601
/// time index that carries a real UTC offset, a NaN run, and a row whose
/// offset differs from its neighbours' (SPEC §2.1: per-row offsets are
/// honored, so the spilled axis must keep them per row too, not collapse them
/// to the first row's).
fn write_mixed_dtype_fixture(dir: &Path, row_count: usize) -> PathBuf {
    let path = dir.join("mixed.csv");
    let mut file = std::fs::File::create(&path).expect("create fixture");
    let mut text = String::from("timestamp,reading,count,flag,state\n");
    for i in 0..row_count {
        // Every 500th row carries a different UTC offset, and every 97th
        // reading is NaN (SPEC §1.3: preserved, never interpolated).
        let offset = if i % 500 == 0 { "+02:00" } else { "Z" };
        let reading = if i % 97 == 0 {
            "NaN".to_string()
        } else {
            format!("{:.4}", (i as f64) * 0.25 - 1000.0)
        };
        text.push_str(&format!(
            "2026-01-01T00:00:{:02}.{:03}{offset},{reading},{},{},{}\n",
            i % 60,
            i % 1000,
            i as i64 * 7 - 3,
            if i % 3 == 0 { "true" } else { "false" },
            if i % 2 == 0 { "running" } else { "idle" },
        ));
        if text.len() > 64 * 1024 {
            file.write_all(text.as_bytes()).expect("write fixture");
            text.clear();
        }
    }
    file.write_all(text.as_bytes()).expect("write fixture");
    file.flush().expect("flush fixture");
    path
}

/// A budget so large that no realistic file could ever exceed it — forces the
/// in-memory path.
fn unlimited_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(u64::MAX)
}

/// A zero budget — nothing is ever affordable in memory, so every file takes
/// the spill path. Using zero rather than a hand-tuned number keeps the test
/// independent of how the footprint estimate is calibrated.
fn zero_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(0)
}

#[test]
fn a_spilled_open_produces_exactly_the_same_dataset_as_an_in_memory_open() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_mixed_dtype_fixture(dir.path(), 4_000);

    let in_memory = ingest::load_with_budget(&path, unlimited_budget(), cache.path())
        .expect("in-memory open must succeed");
    let spilled = ingest::load_with_budget(&path, zero_budget(), cache.path())
        .expect("spilled open must succeed");

    assert!(
        !in_memory.is_spilled(),
        "an unlimited budget must keep the fast in-memory path"
    );
    assert!(
        spilled.is_spilled(),
        "a zero budget must force the on-disk spill path"
    );

    // Golden Rule 1: the two datasets must compare equal, field for field.
    assert_eq!(spilled.time_column_name, in_memory.time_column_name);
    assert_eq!(spilled.time, in_memory.time);
    assert_eq!(spilled.columns.len(), in_memory.columns.len());
    for (spilled_column, memory_column) in spilled.columns.iter().zip(&in_memory.columns) {
        assert_eq!(spilled_column.name(), memory_column.name());
        assert_eq!(spilled_column.dtype(), memory_column.dtype());
        assert_eq!(spilled_column.len(), memory_column.len());
        assert_eq!(spilled_column.anomalies(), memory_column.anomalies());
        match (
            spilled_column.values().as_f64_slice(),
            memory_column.values().as_f64_slice(),
        ) {
            // SPEC §1.3 keeps NaN samples, and `NaN == NaN` is false, so a
            // float column is compared by bit pattern — the strictest form of
            // "the same bytes came back" (Golden Rule 1).
            (Some(spilled), Some(memory)) => assert_eq!(
                spilled.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                memory.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                "column {} differs between storages",
                spilled_column.name()
            ),
            _ => assert_eq!(spilled_column.values(), memory_column.values()),
        }
    }
}

#[test]
fn spilled_columns_keep_their_native_dtype_and_exact_values() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_mixed_dtype_fixture(dir.path(), 600);

    let dataset =
        ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("spilled open");

    assert!(dataset.is_spilled());
    assert_eq!(dataset.time.len(), 600);
    assert_eq!(
        dataset
            .columns
            .iter()
            .map(|series| (series.name().to_string(), series.dtype()))
            .collect::<Vec<_>>(),
        vec![
            ("reading".to_string(), Dtype::F64),
            ("count".to_string(), Dtype::I64),
            ("flag".to_string(), Dtype::Bool),
            ("state".to_string(), Dtype::String),
        ],
        "the spill path must preserve each column's inferred dtype, not \
         flatten everything to f64 (Golden Rule 1)"
    );

    // Spot-check real values through the same `SeriesValues` API the
    // in-memory path exposes, so a caller cannot tell the two apart. Compared
    // by bit pattern, because SPEC §1.3's preserved NaN samples are part of
    // what must round-trip and `NaN == NaN` is false.
    let readings = dataset.columns[0]
        .values()
        .as_f64_slice()
        .expect("the reading column must be f64");
    let expected: Vec<f64> = (0..600)
        .map(|i| {
            if i % 97 == 0 {
                f64::NAN
            } else {
                (i as f64) * 0.25 - 1000.0
            }
        })
        .collect();
    assert_eq!(
        readings.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        "every spilled f64 sample, NaN included, must come back bit-identical"
    );

    // SPEC §1.3's NaN runs must still be flagged on a spilled column — the
    // anomaly container is derived from the samples, wherever they live.
    assert!(
        !dataset.columns[0].anomalies().nan_runs.is_empty(),
        "NaN runs must still be detected on a spilled column"
    );

    assert_eq!(
        dataset.columns[2].values(),
        &SeriesValues::Bool((0..600).map(|i| i % 3 == 0).collect::<Vec<bool>>())
    );
    assert_eq!(
        dataset.columns[3].values(),
        &SeriesValues::String(
            (0..600)
                .map(|i| if i % 2 == 0 { "running" } else { "idle" }.to_string())
                .collect::<Vec<String>>()
        )
    );
}

#[test]
fn a_spilled_i64_column_is_not_degraded_through_f64() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("big-integers.csv");
    let huge = (1i64 << 53) + 1; // beyond f64's exact-integer range
    let mut text = String::from("index,counter\n");
    for i in 0..64i64 {
        text.push_str(&format!("{i},{}\n", huge + i));
    }
    std::fs::write(&path, text).expect("write fixture");

    let dataset =
        ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("spilled open");

    assert!(dataset.is_spilled());
    assert_eq!(dataset.columns[0].dtype(), Dtype::I64);
    assert_eq!(
        dataset.columns[0].values(),
        &SeriesValues::I64((0..64i64).map(|i| huge + i).collect::<Vec<i64>>()),
        "an i64 beyond 2^53 must survive the spill round trip exactly — \
         routing it through f64 would silently degrade it (Golden Rule 1)"
    );
}

#[test]
fn a_spilled_absolute_axis_keeps_every_row_s_own_utc_offset() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_mixed_dtype_fixture(dir.path(), 1_200);

    let dataset =
        ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("spilled open");

    let TimeAxis::Absolute { timestamps, .. } = &dataset.time else {
        panic!("the fixture has an ISO 8601 absolute time index");
    };
    assert_eq!(timestamps.get(0).expect("row 0").offset_seconds, Some(7200));
    assert_eq!(timestamps.get(1).expect("row 1").offset_seconds, Some(0));
    assert_eq!(
        timestamps.get(500).expect("row 500").offset_seconds,
        Some(7200),
        "a per-row offset change must survive the spill, not be collapsed \
         to the first row's (SPEC §2.1)"
    );
}

#[test]
fn a_progressive_index_spills_and_round_trips() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("progressive.csv");
    let mut text = String::from("index,value\n");
    for i in 0..500 {
        text.push_str(&format!("{i},{}\n", i as f64 * 0.5));
    }
    std::fs::write(&path, text).expect("write fixture");

    let spilled =
        ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("spilled open");
    let in_memory =
        ingest::load_with_budget(&path, unlimited_budget(), cache.path()).expect("in-memory open");

    assert!(spilled.is_spilled());
    assert_eq!(spilled.time, in_memory.time);
    assert!(matches!(spilled.time, TimeAxis::Progressive { .. }));
    assert_eq!(spilled.columns[0].values(), in_memory.columns[0].values());
}

#[test]
fn open_dataset_reports_the_same_summary_whichever_storage_it_chose() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_mixed_dtype_fixture(dir.path(), 2_000);

    let (spilled_summary, spilled_report, spilled_dataset) =
        ingest::open_dataset_with_budget(&path, zero_budget(), cache.path())
            .expect("spilled open must succeed");
    let (memory_summary, memory_report, memory_dataset) =
        ingest::open_dataset_with_budget(&path, unlimited_budget(), cache.path())
            .expect("in-memory open must succeed");

    assert!(spilled_dataset.is_spilled());
    assert!(!memory_dataset.is_spilled());
    assert_eq!(
        spilled_summary, memory_summary,
        "the corpus-gate summary must not depend on where the samples were stored"
    );
    assert_eq!(spilled_report, memory_report);
}

// SPEC §5's "first meaningful plot, **any file size**: ≤ 2 s (progressive:
// render what is indexed, keep indexing in background)" applies to exactly the
// files that get spilled — the large ones. A spilled open must therefore still
// report progress checkpoints, not go quiet until the whole read finishes.
//
// Caught in CI by the `first_plot` bench, which spills on the 7.5 GB macos-14
// runner (cap 1.88 GB) where it did not on a 16 GB dev machine, and panicked
// on "a 1 GB fixture must cross at least one progress checkpoint".
#[test]
fn a_spilled_open_still_reports_progress_checkpoints() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    // More than the 20,000-row first checkpoint, so the doubling schedule fires
    // at least twice.
    let path = write_mixed_dtype_fixture(dir.path(), 50_000);

    let mut checkpoints: Vec<(u64, usize)> = Vec::new();
    let dataset =
        ingest::load_progressive_with_budget(&path, zero_budget(), cache.path(), |checkpoint| {
            checkpoints.push((checkpoint.rows_read, checkpoint.dataset.time.len()));
        })
        .expect("spilled progressive open must succeed");

    assert!(dataset.is_spilled());
    assert_eq!(dataset.time.len(), 50_000);
    assert_eq!(
        checkpoints
            .iter()
            .map(|&(rows, _)| rows)
            .collect::<Vec<_>>(),
        vec![20_000, 40_000],
        "a spilled open must checkpoint on the same doubling schedule as an \
         in-memory one"
    );
    for &(rows_read, dataset_len) in &checkpoints {
        assert_eq!(
            dataset_len, rows_read as usize,
            "a checkpoint's dataset must hold exactly the rows it reports"
        );
    }
}

// The preview that makes the checkpoints above possible is bounded: past
// `PREVIEW_MAX_ROWS` it stops growing, so a genuinely huge file cannot smuggle
// an unbounded `Vec` back in through the progress path. A 250k-row fixture
// crosses the 200k cap, so the 160k checkpoint must be the last one even
// though the file has far more rows.
#[test]
fn the_spilled_progressive_preview_stops_growing_at_its_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_mixed_dtype_fixture(dir.path(), 250_000);

    let mut checkpoint_rows: Vec<u64> = Vec::new();
    let dataset =
        ingest::load_progressive_with_budget(&path, zero_budget(), cache.path(), |checkpoint| {
            checkpoint_rows.push(checkpoint.rows_read);
        })
        .expect("spilled progressive open must succeed");

    assert_eq!(dataset.time.len(), 250_000);
    assert_eq!(
        checkpoint_rows,
        vec![20_000, 40_000, 80_000, 160_000],
        "the preview must retire at its row cap instead of following the \
         doubling schedule all the way up with the file"
    );
}

#[test]
fn a_single_column_file_is_still_a_clean_error_on_the_spill_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("single.csv");
    std::fs::write(&path, "timestamp\n2026-01-01T00:00:00Z\n").expect("write fixture");

    let err = ingest::load_with_budget(&path, zero_budget(), cache.path())
        .expect_err("a single-column file must be rejected on either path");

    assert!(matches!(err, glyde_core::GlydeError::SingleColumnFile));
}

#[test]
fn ragged_rows_are_skipped_identically_on_both_paths() {
    let corpus =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus/case-21-ragged-rows.csv");
    let cache = tempfile::tempdir().expect("temp cache dir");

    let spilled =
        ingest::load_with_budget(&corpus, zero_budget(), cache.path()).expect("spilled open");
    let in_memory = ingest::load_with_budget(&corpus, unlimited_budget(), cache.path())
        .expect("in-memory open");

    assert_eq!(spilled.time.len(), 3, "SPEC §1.3 ragged-row salvage");
    assert_eq!(spilled.time, in_memory.time);
    for (a, b) in spilled.columns.iter().zip(&in_memory.columns) {
        assert_eq!(a.values(), b.values());
    }
}
