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

//! Issue #189, SPEC §1.3 ("NaN / missing values: preserved as gaps in the
//! series, never interpolated"): a blank cell in an otherwise numeric CSV
//! column is a missing sample. The column stays numeric, and each blank becomes
//! a NaN gap that is flagged as a NaN run. A blank does not turn the column
//! into text.
//!
//! Corpus case 62 is opened through both storage paths (in memory and
//! spilled), so the rule holds on files of any size.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, Dataset};
use glyde_core::series::{Dtype, Series, SeriesValues};
use std::path::{Path, PathBuf};

fn case_62() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testdata")
        .join("corpus")
        .join("case-62-blank-cells-numeric-column.csv")
}

fn open(budget: RamBudget) -> Dataset {
    let cache = tempfile::tempdir().expect("temp cache dir");
    ingest::load_with_budget(&case_62(), budget, cache.path()).expect("case 62 must open")
}

fn column<'d>(dataset: &'d Dataset, name: &str) -> &'d Series {
    dataset
        .columns
        .iter()
        .find(|series| series.name() == name)
        .unwrap_or_else(|| panic!("case 62 has a {name} column"))
}

fn bits(values: &SeriesValues) -> Vec<u64> {
    values
        .as_f64_slice()
        .expect("a float column")
        .iter()
        .map(|v| v.to_bits())
        .collect()
}

fn assert_blank_cells_are_gaps(dataset: &Dataset) {
    // A float column with two blank cells (one empty, one only whitespace):
    // numeric, with one NaN run of length 2, as if the file said `NaN`
    // there (corpus case 43).
    let value = column(dataset, "value");
    assert_eq!(value.dtype(), Dtype::F64, "blanks must not make it text");
    assert_eq!(
        bits(value.values()),
        [10.0, f64::NAN, f64::NAN, 10.5, 10.6].map(f64::to_bits),
        "a blank cell is NaN, never 0 and never interpolated"
    );
    assert_eq!(value.anomalies().nan_runs, vec![1..3]);

    // An integer column with one blank: promoted to float, the same way a
    // `NaN` cell promotes it, because an integer has no missing value.
    let count = column(dataset, "count");
    assert_eq!(count.dtype(), Dtype::F64);
    assert_eq!(
        bits(count.values()),
        [1.0, 2.0, f64::NAN, 4.0, 5.0].map(f64::to_bits)
    );
    assert_eq!(count.anomalies().nan_runs, vec![2..3]);

    // A column that is blank on every row has no numeric evidence at all, so
    // it keeps its behavior from before issue #189.
    let unused = column(dataset, "unused");
    assert_eq!(unused.dtype(), Dtype::String);
}

#[test]
fn blank_cells_in_a_numeric_column_are_gaps_in_memory() {
    let dataset = open(RamBudget::from_total_ram_bytes(u64::MAX));
    assert!(!dataset.is_spilled());
    assert_blank_cells_are_gaps(&dataset);
}

#[test]
fn blank_cells_in_a_numeric_column_are_gaps_when_spilled() {
    let dataset = open(RamBudget::from_total_ram_bytes(0));
    assert!(dataset.is_spilled());
    assert_blank_cells_are_gaps(&dataset);
}
