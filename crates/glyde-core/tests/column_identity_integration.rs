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

//! Issue #208, column identity: a column with an empty header got a nameless
//! legend entry, a trailing delimiter made a phantom empty lane, and two
//! columns with the same name could not be told apart. Through
//! `open_dataset`, the app's entry point, and both storage paths.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, Dataset, InferenceReport};
use std::path::{Path, PathBuf};

fn corpus(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corpus")
        .join(name)
}

fn names(dataset: &Dataset) -> Vec<&str> {
    dataset.columns.iter().map(|series| series.name()).collect()
}

fn open_both(name: &str) -> Vec<(InferenceReport, Dataset)> {
    let cache = tempfile::tempdir().expect("temp cache dir");
    [u64::MAX, 0]
        .into_iter()
        .map(|ram| {
            let (_, report, dataset) = ingest::open_dataset_with_budget(
                &corpus(name),
                RamBudget::from_total_ram_bytes(ram),
                cache.path(),
            )
            .expect("opens");
            (report, dataset)
        })
        .collect()
}

#[test]
fn a_column_with_an_empty_header_is_named_after_its_position() {
    for (report, dataset) in open_both("case-17-empty-columns.csv") {
        assert_eq!(names(&dataset), ["column_1", "pressure"]);
        assert_eq!(report.column_names, ["timestamp", "column_1", "pressure"]);
    }
}

#[test]
fn duplicate_names_are_told_apart() {
    for (report, dataset) in open_both("case-16-duplicate-column-names.csv") {
        assert_eq!(names(&dataset), ["value", "value (2)"]);
        assert_eq!(report.column_names, ["timestamp", "value", "value (2)"]);
    }
}

#[test]
fn a_column_with_no_value_at_all_is_not_drawn_but_is_reported() {
    for (report, dataset) in open_both("case-20-trailing-empty-lines.csv") {
        assert_eq!(names(&dataset), ["value", "pressure"], "no phantom lane");
        assert_eq!(report.unloaded_columns.len(), 1, "never silently dropped");
        let unloaded = &report.unloaded_columns[0];
        assert_eq!(unloaded.name, "column_3");
        assert!(unloaded.reason.contains("empty"), "{}", unloaded.reason);
    }
}
