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

//! Issue #94, SPEC §1.3 "never abort the load": a time column that matches no
//! known timestamp format and is not numeric either used to take the whole
//! file down with `GlydeError::NonNumericTimeIndex` — no plot, and no
//! inference bar to correct anything from. It now opens against a row-ordinal
//! index, reported low-confidence so the substitution is visible rather than
//! silent (Golden Rule 2).
//!
//! `tests/corpus_open_compare.rs` cannot cover this on its own: it drives
//! `ingest::inspect`, which never parses the time column's *values* and so
//! already "succeeded" on such a file while `open_dataset` — the entry point
//! the app actually calls — failed. That divergence is exactly why the bug
//! survived a green corpus gate, so the assertions here go through
//! `open_dataset`, and through both storage paths.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, Confidence, SamplingClass, TimeAxis};
use std::path::{Path, PathBuf};

/// Corpus case 58: `01-Jan-2026 00:00:00` — a real timestamp shape (`DD-Mon-YYYY`)
/// that SPEC §2.1's "must support at minimum" list does not include, so
/// inference legitimately finds no format for it.
fn case_58() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testdata")
        .join("corpus")
        .join("case-58-unrecognized-time-format.csv")
}

/// A budget of 0 makes every file unaffordable in memory, so `choose_storage`
/// takes the spill path (issue #75) — the mirror of `spilled_ingest_integration`'s
/// own helper.
fn zero_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(0)
}

fn unlimited_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(u64::MAX)
}

#[test]
fn an_unreadable_time_column_opens_against_a_row_ordinal_index() {
    let (summary, report, dataset) = ingest::open_dataset(&case_58())
        .expect("SPEC §1.3: an unreadable time column never aborts");

    // The index is the row ordinal, not a mangled reading of the source
    // column: 0, 1, 2, … for every row, in source order.
    match &dataset.time {
        TimeAxis::Progressive { values } => {
            assert_eq!(values.as_slice(), &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        }
        TimeAxis::Absolute { .. } => {
            panic!("case 58's time column matches no format in SPEC §2.1's list")
        }
    }

    // The data columns are untouched — this is an index substitution, never a
    // change to the samples themselves (Golden Rule 1).
    assert_eq!(dataset.columns.len(), 2);
    assert_eq!(summary.row_count, 6);
    assert_eq!(summary.skipped_row_count, 0);
    assert_eq!(summary.sampling_class, SamplingClass::ProgressiveIndex);

    // …and the substitution is visible. Both time fields report low
    // confidence, which is what opens the inference bar expanded on first
    // render (SPEC §1.2).
    assert_eq!(report.time_column.value, None);
    assert_eq!(report.time_column.confidence, Confidence::Low);
    assert_eq!(report.timestamp_format.value, None);
    assert_eq!(report.timestamp_format.confidence, Confidence::Low);
    assert!(
        report.has_low_confidence_field(),
        "a file whose time column Glyde could not read must never present as \
         a confident, unambiguous open"
    );
}

#[test]
fn a_genuine_progressive_index_is_still_reported_confidently() {
    // The negative control for the assertion above: corpus case 35's index is
    // a real numeric column, so it reports `time_column: None` too — but at
    // full confidence. Without this, "always report the time fields low" would
    // pass the test above while destroying the signal it exists to carry.
    let case_35 = case_58().with_file_name("case-35-progressive-integer-index.csv");
    let (_summary, report, dataset) = ingest::open_dataset(&case_35).expect("case 35 opens");

    assert!(matches!(dataset.time, TimeAxis::Progressive { .. }));
    assert_eq!(report.time_column.confidence, Confidence::High);
    assert_eq!(report.timestamp_format.confidence, Confidence::High);
    assert!(!report.has_low_confidence_field());
}

#[test]
fn the_row_ordinal_fallback_is_identical_on_the_spilled_path() {
    // Issue #75 split ingestion into two storage paths; issue #94's fallback
    // has to be a property of ingestion, not of whichever path the machine's
    // RAM happened to choose.
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = case_58();

    let spilled =
        ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("spilled open");
    let in_memory =
        ingest::load_with_budget(&path, unlimited_budget(), cache.path()).expect("in-memory open");

    assert!(
        spilled.is_spilled(),
        "the zero budget must force the spill path"
    );
    assert!(!in_memory.is_spilled());
    assert_eq!(spilled.time, in_memory.time);
    assert_eq!(
        spilled.columns[0].values(),
        in_memory.columns[0].values(),
        "the samples must not depend on the storage path"
    );

    match &spilled.time {
        TimeAxis::Progressive { values } => {
            assert_eq!(values.as_slice(), &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        }
        TimeAxis::Absolute { .. } => panic!("the spilled path must take the same fallback"),
    }
}

#[test]
fn one_unreadable_row_is_enough_to_demote_the_whole_column() {
    // The documented consequence of deciding this at column level (issue #94's
    // Option A): a column that is numeric except for a single "N/A" is not a
    // usable index, so the whole column is replaced rather than one row being
    // silently dropped from a dataset the user believes is complete. Every row
    // survives — which is the point — but the index is ordinals, not the
    // column's own numbers.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("one-bad-cell.csv");
    let mut text = String::from("index,value\n");
    for row in 0..10 {
        if row == 7 {
            text.push_str("N/A,0.7\n");
        } else {
            text.push_str(&format!("{},{}\n", row * 10, row as f64 * 0.1));
        }
    }
    std::fs::write(&path, text).expect("write fixture");

    let (summary, report, dataset) = ingest::open_dataset(&path).expect("the file still opens");

    assert_eq!(summary.row_count, 10, "no row is dropped by the fallback");
    assert_eq!(dataset.time.len(), 10);
    match &dataset.time {
        TimeAxis::Progressive { values } => {
            assert_eq!(values.as_slice().first(), Some(&0.0));
            assert_eq!(values.as_slice().last(), Some(&9.0));
        }
        TimeAxis::Absolute { .. } => panic!("`N/A` is not a timestamp"),
    }
    assert_eq!(report.time_column.confidence, Confidence::Low);
}
