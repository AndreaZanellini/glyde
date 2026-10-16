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

//! Issue #110: SPEC §2.1's "[Sort]" for non-monotonic timestamps must work on
//! a file too large for memory too, not only on the in-memory path. A
//! spilled open with [`IngestOverrides::sort_by_time`] sorts its spill files
//! on disk, in bounded memory, and must produce exactly what the in-memory
//! sort produces for the same file: same order (stable for equal
//! timestamps), same values in every dtype, same NaN-run anomalies — and
//! leave no intermediate files behind.

use std::path::{Path, PathBuf};
use std::time::Duration;

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, Dataset, IngestOverrides, TimeAxis, TimeColumnChoice};
use glyde_core::series::{Series, SeriesValues, SpilledValues};

const ROWS: usize = 3_000;

/// Non-monotonic timestamps (a scrambled permutation of 0..ROWS/2 seconds,
/// so every timestamp occurs twice and stability matters), with one column
/// of every dtype the spilled path writes.
fn write_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("scrambled.csv");
    let mut text = String::from("timestamp,value,count,flag,label\n");
    for i in 0..ROWS {
        let second = (i * 7_919) % (ROWS / 2);
        let value = if i % 97 == 0 || i % 97 == 1 {
            "NaN".to_string()
        } else {
            format!("{}", i as f64 * 0.25)
        };
        let label = match i % 5 {
            0 => String::new(),
            1 => "idle".to_string(),
            2 => format!("run-{i}"),
            3 => "défaut ⚠".to_string(),
            _ => "x".repeat(i % 40),
        };
        text.push_str(&format!(
            "2026-01-01T{:02}:{:02}:{:02}Z,{value},{},{},{label}\n",
            second / 3600,
            (second / 60) % 60,
            second % 60,
            i as i64 - 1_000,
            i % 3 == 0,
        ));
    }
    std::fs::write(&path, text).expect("write fixture");
    path
}

fn sorted() -> IngestOverrides {
    IngestOverrides {
        sort_by_time: true,
        ..IngestOverrides::default()
    }
}

fn load(path: &Path, overrides: IngestOverrides, ram: u64, cache: &Path) -> Dataset {
    ingest::load_with_overrides_and_budget(
        path,
        overrides,
        RamBudget::from_total_ram_bytes(ram),
        cache,
    )
    .expect("open")
}

/// F64 samples by bit pattern, so NaN compares equal to NaN.
fn f64_bits(series: &Series) -> Vec<u64> {
    let values: Vec<f64> = match series.values() {
        SeriesValues::F64(values) => values.clone(),
        SeriesValues::Spilled(SpilledValues::F64(values)) => values.as_slice().to_vec(),
        other => panic!("expected f64, got {other:?}"),
    };
    values.into_iter().map(f64::to_bits).collect()
}

fn assert_same_dataset(spilled: &Dataset, in_memory: &Dataset) {
    assert!(
        spilled.is_spilled(),
        "the fixture must take the spilled path"
    );
    assert!(!in_memory.is_spilled());
    assert_eq!(spilled.time, in_memory.time, "time axis");
    assert_eq!(spilled.time_column_name, in_memory.time_column_name);
    assert_eq!(spilled.columns.len(), in_memory.columns.len());
    for (a, b) in spilled.columns.iter().zip(&in_memory.columns) {
        assert_eq!(a.name(), b.name());
        assert_eq!(a.dtype(), b.dtype(), "{}", a.name());
        if a.dtype() == glyde_core::series::Dtype::F64 {
            assert_eq!(f64_bits(a), f64_bits(b), "{}", a.name());
        } else {
            assert_eq!(a.values(), b.values(), "{}", a.name());
        }
        assert_eq!(a.anomalies(), b.anomalies(), "{} anomalies", a.name());
    }
}

fn ticks_are_sorted(dataset: &Dataset) -> bool {
    let ticks = dataset.time.to_pyramid_ticks();
    ticks.windows(2).all(|pair| pair[0] <= pair[1])
}

#[test]
fn a_spilled_file_is_sorted_exactly_like_the_in_memory_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());

    let unsorted = load(&path, IngestOverrides::default(), 0, cache.path());
    assert!(
        !ticks_are_sorted(&unsorted),
        "the fixture must be scrambled"
    );
    drop(unsorted);

    let in_memory = load(&path, sorted(), u64::MAX, cache.path());
    assert!(ticks_are_sorted(&in_memory));
    let spilled = load(&path, sorted(), 0, cache.path());
    assert_same_dataset(&spilled, &in_memory);
}

// A numeric time column the user picked (a progressive index) sorts the
// same way.
#[test]
fn a_spilled_progressive_index_is_sorted_too() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("progressive.csv");
    let mut text = String::from("position,value,label\n");
    for i in 0..ROWS {
        let position = ((i * 31) % 1_000) as f64 * 0.5;
        text.push_str(&format!("{position},{},{}\n", i, i % 7));
    }
    std::fs::write(&path, text).expect("write fixture");
    let overrides = IngestOverrides {
        time_column: Some(TimeColumnChoice::Column(0)),
        ..sorted()
    };

    let in_memory = load(&path, overrides, u64::MAX, cache.path());
    assert!(matches!(in_memory.time, TimeAxis::Progressive { .. }));
    assert!(ticks_are_sorted(&in_memory));
    let spilled = load(&path, overrides, 0, cache.path());
    assert_same_dataset(&spilled, &in_memory);
}

fn spill_sets(cache_dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(cache_dir.join("spill")) {
        Ok(entries) => entries
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

// Sorting needs room for the unsorted columns and the sorted runs at once;
// once it is done, only the sorted dataset's own files remain, and they go
// when it is closed (issue #118).
#[test]
fn sorting_leaves_only_the_sorted_files_behind() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());

    let spilled = load(&path, sorted(), 0, cache.path());
    assert!(
        eventually(|| spill_sets(cache.path()).len() == 1),
        "only the sorted dataset's set may remain: {:?}",
        spill_sets(cache.path())
    );
    drop(spilled);
    assert!(eventually(|| spill_sets(cache.path()).is_empty()));
}

// Asking to sort a file that is already in order changes nothing.
#[test]
fn sorting_an_already_sorted_spilled_file_changes_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("ordered.csv");
    let mut text = String::from("timestamp,value\n");
    for i in 0..ROWS {
        text.push_str(&format!(
            "2026-01-01T{:02}:{:02}:{:02}Z,{i}\n",
            i / 3600,
            (i / 60) % 60,
            i % 60
        ));
    }
    std::fs::write(&path, text).expect("write fixture");

    let plain = load(&path, IngestOverrides::default(), 0, cache.path());
    let sorted = load(&path, sorted(), 0, cache.path());
    assert_eq!(sorted.time, plain.time);
    assert_eq!(sorted.columns[0].values(), plain.columns[0].values());
}
