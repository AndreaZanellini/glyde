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

//! Issue #216: a `;`-delimited file whose cells are all comma-decimal
//! numbers (a relative-time first column written the European way, no
//! timestamp and no integer column) failed to open with "single-column
//! file": header detection took every row for a header label, so the file
//! had no data row and no column at all. Through `open_dataset`, the entry
//! point the app calls, and through both storage paths.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, Confidence, TimeAxis};
use glyde_core::series::SeriesValues;

const FILE: &str = "time;pressure;temperature\n\
                    0,0;1,5;20,25\n\
                    0,5;1,6;20,5\n\
                    1,0;-1,7;20,75\n\
                    1,5;1,8;21,0\n";

fn write_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("comma-decimals-only.csv");
    std::fs::write(&path, FILE).expect("write fixture");
    path
}

#[test]
fn a_semicolon_file_of_comma_decimals_only_opens_with_its_header() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write_fixture(dir.path());

    let (summary, report, dataset) =
        ingest::open_dataset(&path).expect("a file of comma-decimal numbers opens");

    assert_eq!(report.delimiter.value.as_deref(), Some(";"));
    assert_eq!(report.decimal_separator.value.as_deref(), Some(","));
    assert_eq!(
        report.column_names,
        ["time", "pressure", "temperature"],
        "the first line is the header"
    );
    assert_eq!(summary.row_count, 4, "every data row is kept");
    assert_eq!(summary.skipped_row_count, 0);
    assert!(matches!(dataset.time, TimeAxis::Progressive { .. }));

    let value_of = |name: &str| {
        let series = dataset
            .columns
            .iter()
            .find(|series| series.name() == name)
            .unwrap_or_else(|| panic!("column {name} is loaded"));
        match series.values() {
            SeriesValues::F64(values) => values.clone(),
            other => panic!("{name} must be numeric, got {other:?}"),
        }
    };
    assert_eq!(value_of("pressure"), [1.5, 1.6, -1.7, 1.8]);
    assert_eq!(value_of("temperature"), [20.25, 20.5, 20.75, 21.0]);
    assert_eq!(report.delimiter.confidence, Confidence::High);
}

#[test]
fn the_same_file_opens_identically_on_the_spilled_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write_fixture(dir.path());
    let cache = tempfile::tempdir().expect("temp cache dir");

    let spilled = ingest::load_with_budget(&path, RamBudget::from_total_ram_bytes(0), cache.path())
        .expect("spilled open");
    let in_memory = ingest::load_with_budget(
        &path,
        RamBudget::from_total_ram_bytes(u64::MAX),
        cache.path(),
    )
    .expect("in-memory open");

    assert!(
        spilled.is_spilled(),
        "the zero budget forces the spill path"
    );
    assert_eq!(spilled.time, in_memory.time);
    let names = |dataset: &ingest::Dataset| {
        dataset
            .columns
            .iter()
            .map(|series| series.name().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&spilled), names(&in_memory));
    for (spilled, in_memory) in spilled.columns.iter().zip(&in_memory.columns) {
        assert_eq!(spilled.values(), in_memory.values());
    }
}

// Found with #216: a file in which no line looks like data (here, text
// only) has no column after header inference. The spilled open indexed
// into an empty batch and panicked; both paths now report it as a file
// with nothing to plot, never a crash (SPEC §1.3).
#[test]
fn a_file_with_no_data_row_is_an_error_on_both_paths_never_a_panic() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("labels-only.csv");
    std::fs::write(&path, "alpha;beta\ngamma;delta\nepsilon;zeta\n").expect("write fixture");
    let cache = tempfile::tempdir().expect("temp cache dir");

    for budget in [0, u64::MAX] {
        let result =
            ingest::load_with_budget(&path, RamBudget::from_total_ram_bytes(budget), cache.path());
        assert!(
            matches!(result, Err(glyde_core::GlydeError::SingleColumnFile)),
            "budget {budget}: {result:?}"
        );
    }
}
