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

//! A file with no time column, and the user's choice of time column (SPEC
//! §2.1 "Files without a time column", SPEC §1.2 "time column ... correctable
//! in one click").
//!
//! Before this, column 0 was the time index whenever it parsed as a number at
//! all — so a file of plain signals (`ax,ay,az`) plotted `ay` and `az`
//! against the *values* of `ax`, and `ax` itself vanished from the plot. Now
//! a numeric first column is only an index if it never runs backwards and
//! actually advances; otherwise Glyde generates a row index `0, 1, 2, …`,
//! keeps the first column as a signal, and says so (Golden Rule 2). The user
//! can also name the time column — or ask for the row index — outright.
//!
//! Every assertion goes through `open_dataset` (the entry point the app
//! calls), and the storage-sensitive ones through both storage paths.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{
    self, Confidence, GeneratedIndexReason, IngestOverrides, SamplingClass, TimeAxis,
    TimeColumnChoice, TimeIndexSource,
};
use glyde_core::series::SeriesValues;
use std::path::{Path, PathBuf};

fn corpus(file_name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testdata")
        .join("corpus")
        .join(file_name)
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write fixture");
    path
}

fn zero_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(0)
}

fn unlimited_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(u64::MAX)
}

fn progressive(axis: &TimeAxis) -> Vec<f64> {
    match axis {
        TimeAxis::Progressive { values } => values.as_slice().to_vec(),
        TimeAxis::Absolute { .. } => panic!("expected a progressive axis, got an absolute one"),
    }
}

fn names(dataset: &ingest::Dataset) -> Vec<&str> {
    dataset.columns.iter().map(|series| series.name()).collect()
}

/// Corpus case 59: three accelerometer channels, no time column at all.
fn case_59() -> PathBuf {
    corpus("case-59-no-time-column.csv")
}

#[test]
fn a_file_of_plain_signals_plots_every_column_against_a_generated_row_index() {
    let (summary, report, dataset) =
        ingest::open_dataset(&case_59()).expect("a file with no time column opens");

    let rows = summary.row_count as usize;
    let expected_index: Vec<f64> = (0..rows).map(|row| row as f64).collect();
    assert_eq!(progressive(&dataset.time), expected_index);

    // The first column is a signal, not an index: it is kept, with its own
    // values, in source order (Golden Rule 1).
    assert_eq!(names(&dataset), ["ax", "ay", "az"]);
    match dataset.columns[0].values() {
        SeriesValues::F64(values) => assert_eq!(values[..3], [0.12, -0.08, 0.31]),
        other => panic!("ax must stay a float signal, got {other:?}"),
    }

    // And the substitution is never silent (Golden Rule 2).
    assert_eq!(summary.sampling_class, SamplingClass::ProgressiveIndex);
    assert_eq!(report.time_column.value, None);
    assert_eq!(report.time_column.confidence, Confidence::Low);
    assert!(report.has_low_confidence_field());
    assert_eq!(
        report.time_index,
        TimeIndexSource::Generated(GeneratedIndexReason::NotMonotonic {
            column: "ax".to_string(),
            row: 1,
        })
    );
    assert_eq!(report.column_names, ["ax", "ay", "az"]);
}

#[test]
fn a_constant_first_column_is_not_a_time_index() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(
        dir.path(),
        "constant.csv",
        "mode,value\n3,1.0\n3,2.0\n3,1.5\n3,0.5\n",
    );

    let (_summary, report, dataset) = ingest::open_dataset(&path).expect("opens");

    assert_eq!(progressive(&dataset.time), [0.0, 1.0, 2.0, 3.0]);
    assert_eq!(names(&dataset), ["mode", "value"]);
    assert_eq!(
        report.time_index,
        TimeIndexSource::Generated(GeneratedIndexReason::Constant {
            column: "mode".to_string(),
        })
    );
    assert_eq!(report.time_column.confidence, Confidence::Low);
}

#[test]
fn a_monotonic_numeric_first_column_is_still_the_time_index() {
    // The negative control: corpus case 35's progressive index never runs
    // backwards, so it stays the index, reported confidently.
    let (_summary, report, dataset) =
        ingest::open_dataset(&corpus("case-35-progressive-integer-index.csv")).expect("opens");

    assert_eq!(names(&dataset), ["value"]);
    assert_eq!(
        report.time_index,
        TimeIndexSource::Column {
            index: 0,
            name: "sample".to_string(),
        }
    );
    assert_eq!(report.time_column.confidence, Confidence::High);
    assert!(!report.has_low_confidence_field());
}

#[test]
fn repeated_values_do_not_disqualify_a_numeric_time_index() {
    // SPEC §2.1: duplicate timestamps are "preserved, flagged" — a repeat is
    // not a step backwards.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "dup.csv", "t,v\n0,1\n1,2\n1,3\n2,4\n");

    let (_summary, report, dataset) = ingest::open_dataset(&path).expect("opens");

    assert_eq!(progressive(&dataset.time), [0.0, 1.0, 1.0, 2.0]);
    assert_eq!(names(&dataset), ["v"]);
    assert!(matches!(
        report.time_index,
        TimeIndexSource::Column { index: 0, .. }
    ));
}

#[test]
fn out_of_order_text_timestamps_keep_spec_2_1_sort_affordance() {
    // A date-and-time column is unmistakably a time column even when a row is
    // out of order — the monotonicity test is for numeric columns only, which
    // are otherwise indistinguishable from a signal. Corpus case 36 keeps its
    // SPEC §2.1 "[Sort] / [Keep as-is]" treatment.
    let (summary, report, dataset) =
        ingest::open_dataset(&corpus("case-36-non-monotonic-timestamps.csv")).expect("opens");

    assert!(matches!(dataset.time, TimeAxis::Absolute { .. }));
    assert_eq!(summary.non_monotonic_count, 1);
    assert_eq!(report.time_column.value.as_deref(), Some("timestamp"));
}

#[test]
fn out_of_order_epoch_seconds_are_treated_like_any_other_numeric_column() {
    // Documented consequence of the rule: an epoch column is just numbers, so
    // one that runs backwards reads as "no time column" — reported
    // low-confidence, with the column still plotted and one click away from
    // being picked as the time index.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(
        dir.path(),
        "epoch-back.csv",
        "epoch,v\n1767225600,1\n1767225601,2\n1767225600,3\n1767225602,4\n",
    );

    let (_summary, report, dataset) = ingest::open_dataset(&path).expect("opens");

    assert_eq!(progressive(&dataset.time), [0.0, 1.0, 2.0, 3.0]);
    assert_eq!(names(&dataset), ["epoch", "v"]);
    assert_eq!(
        report.time_index,
        TimeIndexSource::Generated(GeneratedIndexReason::NotMonotonic {
            column: "epoch".to_string(),
            row: 2,
        })
    );
}

#[test]
fn the_user_can_pick_any_column_as_the_time_index() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(
        dir.path(),
        "time-last.csv",
        "temperature,pressure,timestamp\n\
         20.5,1013.2,2026-01-01T00:00:00Z\n\
         20.4,1013.1,2026-01-01T00:00:01Z\n\
         20.6,1013.3,2026-01-01T00:00:02Z\n",
    );
    let overrides = IngestOverrides {
        time_column: Some(TimeColumnChoice::Column(2)),
        ..IngestOverrides::default()
    };

    let (summary, report, dataset) =
        ingest::open_dataset_with_overrides(&path, overrides).expect("opens");

    assert!(matches!(dataset.time, TimeAxis::Absolute { .. }));
    assert_eq!(dataset.time_column_name, "timestamp");
    assert_eq!(names(&dataset), ["temperature", "pressure"]);
    assert_eq!(summary.time_column.as_deref(), Some("timestamp"));
    assert_eq!(report.time_column.value.as_deref(), Some("timestamp"));
    // A deliberate choice is never a guess.
    assert_eq!(report.time_column.confidence, Confidence::High);
    assert_eq!(
        report.time_index,
        TimeIndexSource::Column {
            index: 2,
            name: "timestamp".to_string(),
        }
    );
}

#[test]
fn the_user_can_ask_for_the_row_index_even_when_a_time_column_was_detected() {
    // The escape hatch for the case no heuristic can catch: a first column
    // that is a monotonic *signal* (a counter, a cumulative quantity).
    let path = corpus("case-01-comma-clean.csv");
    let overrides = IngestOverrides {
        time_column: Some(TimeColumnChoice::RowIndex),
        ..IngestOverrides::default()
    };

    let (summary, report, dataset) =
        ingest::open_dataset_with_overrides(&path, overrides).expect("opens");

    let rows = summary.row_count as usize;
    assert_eq!(
        progressive(&dataset.time),
        (0..rows).map(|row| row as f64).collect::<Vec<_>>()
    );
    // The former time column is kept as a column of its own.
    assert_eq!(dataset.columns.len(), report.column_names.len());
    assert_eq!(dataset.columns[0].name(), report.column_names[0]);
    assert_eq!(
        report.time_index,
        TimeIndexSource::Generated(GeneratedIndexReason::Requested)
    );
    assert_eq!(report.time_column.confidence, Confidence::High);
    assert!(!report.has_low_confidence_field());
}

#[test]
fn a_picked_numeric_column_that_runs_backwards_is_honored_and_flagged() {
    // The user's choice is never second-guessed by the monotonicity rule, but
    // SPEC §2.1's non-monotonic report (and its [Sort] affordance) applies.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "picked.csv", "a,b\n1,0\n2,1\n3,3\n4,2\n");
    let overrides = IngestOverrides {
        time_column: Some(TimeColumnChoice::Column(1)),
        ..IngestOverrides::default()
    };

    let (summary, report, dataset) =
        ingest::open_dataset_with_overrides(&path, overrides).expect("opens");

    assert_eq!(progressive(&dataset.time), [0.0, 1.0, 3.0, 2.0]);
    assert_eq!(names(&dataset), ["a"]);
    assert_eq!(summary.non_monotonic_count, 1);
    assert_eq!(report.non_monotonic_count, 1);
}

#[test]
fn sorting_by_a_picked_column_reorders_every_series_with_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "sort.csv", "a,b\n10,0\n20,1\n30,3\n40,2\n");
    let overrides = IngestOverrides {
        time_column: Some(TimeColumnChoice::Column(1)),
        sort_by_time: true,
        ..IngestOverrides::default()
    };

    let dataset = ingest::load_with_overrides(&path, overrides).expect("opens");

    assert_eq!(progressive(&dataset.time), [0.0, 1.0, 2.0, 3.0]);
    assert_eq!(
        *dataset.columns[0].values(),
        SeriesValues::I64(vec![10, 20, 40, 30])
    );
}

#[test]
fn a_picked_column_that_is_not_a_time_index_falls_back_visibly() {
    // Picking a text column that matches no timestamp format cannot produce
    // an index; the row index stands in, and the column stays a series.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(
        dir.path(),
        "states.csv",
        "t,state,v\n0,idle,1.0\n1,run,2.0\n2,idle,3.0\n",
    );
    let overrides = IngestOverrides {
        time_column: Some(TimeColumnChoice::Column(1)),
        ..IngestOverrides::default()
    };

    let (_summary, report, dataset) =
        ingest::open_dataset_with_overrides(&path, overrides).expect("opens");

    assert_eq!(progressive(&dataset.time), [0.0, 1.0, 2.0]);
    assert_eq!(names(&dataset), ["t", "state", "v"]);
    assert_eq!(
        report.time_index,
        TimeIndexSource::Generated(GeneratedIndexReason::Unreadable {
            column: "state".to_string(),
        })
    );
    assert_eq!(report.time_column.confidence, Confidence::Low);
}

#[test]
fn an_out_of_range_column_choice_falls_back_to_automatic_detection() {
    // A stale choice (e.g. left over from before a delimiter correction
    // changed the column count) must never fail the open.
    let path = corpus("case-01-comma-clean.csv");
    let overrides = IngestOverrides {
        time_column: Some(TimeColumnChoice::Column(99)),
        ..IngestOverrides::default()
    };

    let (_summary, _report, picked) =
        ingest::open_dataset_with_overrides(&path, overrides).expect("opens");
    let (_summary, _report, automatic) = ingest::open_dataset(&path).expect("opens");

    assert_eq!(picked, automatic);
}

#[test]
fn a_single_signal_column_opens_against_the_row_index() {
    // A one-channel export used to be refused as `SingleColumnFile` (its only
    // column was taken as the index); it is plottable now.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "one.csv", "signal\n0.5\n-0.2\n0.9\n0.1\n");

    let (_summary, _report, dataset) = ingest::open_dataset(&path).expect("opens");

    assert_eq!(progressive(&dataset.time), [0.0, 1.0, 2.0, 3.0]);
    assert_eq!(names(&dataset), ["signal"]);
}

#[test]
fn a_single_timestamp_column_is_still_refused() {
    // Corpus case 18: only a time index, nothing to plot against it.
    let err =
        ingest::open_dataset(&corpus("case-18-single-column.csv")).expect_err("nothing to plot");
    assert!(matches!(err, glyde_core::GlydeError::SingleColumnFile));
}

#[test]
fn no_time_column_is_identical_on_both_storage_paths() {
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = case_59();

    let spilled =
        ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("spilled open");
    let in_memory =
        ingest::load_with_budget(&path, unlimited_budget(), cache.path()).expect("in-memory open");

    assert!(spilled.is_spilled());
    assert!(!in_memory.is_spilled());
    assert_eq!(spilled.time, in_memory.time);
    assert_eq!(names(&spilled), names(&in_memory));
    for (spilled, in_memory) in spilled.columns.iter().zip(&in_memory.columns) {
        assert_eq!(spilled.values(), in_memory.values());
    }
}

#[test]
fn a_picked_column_is_identical_on_both_storage_paths() {
    let cache = tempfile::tempdir().expect("temp cache dir");
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(
        dir.path(),
        "time-middle.csv",
        "a,timestamp,b\n\
         1.5,2026-01-01T00:00:00Z,x\n\
         2.5,2026-01-01T00:00:01Z,y\n\
         0.5,2026-01-01T00:00:02Z,x\n",
    );
    for choice in [TimeColumnChoice::Column(1), TimeColumnChoice::RowIndex] {
        let overrides = IngestOverrides {
            time_column: Some(choice),
            ..IngestOverrides::default()
        };
        let spilled =
            ingest::load_with_overrides_and_budget(&path, overrides, zero_budget(), cache.path())
                .expect("spilled open");
        let in_memory = ingest::load_with_overrides_and_budget(
            &path,
            overrides,
            unlimited_budget(),
            cache.path(),
        )
        .expect("in-memory open");

        assert!(spilled.is_spilled());
        assert_eq!(spilled.time, in_memory.time, "{choice:?}");
        assert_eq!(spilled.time_column_name, in_memory.time_column_name);
        assert_eq!(names(&spilled), names(&in_memory), "{choice:?}");
        for (spilled, in_memory) in spilled.columns.iter().zip(&in_memory.columns) {
            assert_eq!(spilled.values(), in_memory.values(), "{choice:?}");
        }
    }
}

#[test]
fn a_column_that_runs_backwards_only_after_the_first_checkpoint_is_demoted_cleanly() {
    // A progressive open decides on the rows read so far. Here the first
    // column is a perfect index for 60 000 rows — past several checkpoints —
    // and only then steps backwards. The completed open must be exactly what
    // a one-shot load produces, and every checkpoint's pyramids must belong
    // to that checkpoint's own dataset, never to a stale layout.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut text = String::from("counter,value\n");
    for row in 0..60_000u32 {
        text.push_str(&format!("{row},{}\n", f64::from(row % 7)));
    }
    text.push_str("5,1.0\n");
    for row in 0..100u32 {
        text.push_str(&format!("{},{}\n", 60_000 + row, f64::from(row)));
    }
    let path = write(dir.path(), "late.csv", &text);

    let mut checkpoints = 0usize;
    let progressive_dataset = ingest::load_progressive(&path, |checkpoint| {
        checkpoints += 1;
        assert_eq!(
            checkpoint.pyramids,
            ingest::pyramids_for_dataset(&checkpoint.dataset),
            "a checkpoint's pyramids must be built over its own dataset"
        );
    })
    .expect("opens");
    let one_shot = ingest::load(&path).expect("opens");

    assert!(
        checkpoints >= 2,
        "the fixture must span several checkpoints"
    );
    assert_eq!(progressive_dataset, one_shot);
    assert_eq!(names(&one_shot), ["counter", "value"]);
    assert_eq!(one_shot.time.len(), 60_101);
}

#[test]
fn a_spilled_file_whose_first_column_is_all_ones_plots_it_against_the_row_index() {
    let cache = tempfile::tempdir().expect("temp cache dir");
    let dir = tempfile::tempdir().expect("temp dir");
    let mut text = String::from("flag,value\n");
    for row in 0..50_000u32 {
        text.push_str(&format!("1,{}\n", f64::from(row % 13) * 0.5));
    }
    let path = write(dir.path(), "ones.csv", &text);

    let (_summary, report, spilled) =
        ingest::open_dataset_with_budget(&path, zero_budget(), cache.path()).expect("spilled open");
    let in_memory =
        ingest::load_with_budget(&path, unlimited_budget(), cache.path()).expect("in-memory open");

    assert!(spilled.is_spilled());
    assert_eq!(
        report.time_index,
        TimeIndexSource::Generated(GeneratedIndexReason::Constant {
            column: "flag".to_string(),
        })
    );
    assert_eq!(report.time_column.confidence, Confidence::Low);
    assert_eq!(spilled.time.len(), 50_000);
    assert_eq!(spilled.time, in_memory.time);
    assert_eq!(names(&spilled), ["flag", "value"]);
    println!("flag dtype = {:?}", spilled.columns[0].dtype());
    for (spilled, in_memory) in spilled.columns.iter().zip(&in_memory.columns) {
        assert_eq!(spilled.values(), in_memory.values());
    }
}

#[test]
fn a_staircase_first_column_is_a_signal_not_a_time_index() {
    // The shape of a real test-bench export: `OP` (operating point) holds
    // 1, then 2, then 3 for thousands of rows each, and `time in s` restarts
    // at 0 for every operating point. `OP` never decreases and does advance,
    // but it changes on a handful of rows out of thousands — every sample of
    // one operating point would land on the same x. Neither column is a
    // whole-file time axis; the row index is, with both kept as series.
    let cache = tempfile::tempdir().expect("temp cache dir");
    let dir = tempfile::tempdir().expect("temp dir");
    let mut text = String::from("OP,time in s,i_a in A\n");
    for op in 1..=3u32 {
        for row in 0..10_000u32 {
            text.push_str(&format!(
                "{op},{},{}\n",
                f64::from(row) * 1e-6,
                f64::from(row % 17) * 0.25
            ));
        }
    }
    let path = write(dir.path(), "staircase.csv", &text);

    for budget in [unlimited_budget(), zero_budget()] {
        let (summary, report, dataset) =
            ingest::open_dataset_with_budget(&path, budget, cache.path()).expect("opens");

        assert_eq!(progressive(&dataset.time).len(), 30_000);
        assert_eq!(names(&dataset), ["OP", "time in s", "i_a in A"]);
        assert_eq!(summary.sampling_class, SamplingClass::ProgressiveIndex);
        assert_eq!(
            report.time_index,
            TimeIndexSource::Generated(GeneratedIndexReason::MostlyRepeated {
                column: "OP".to_string(),
                changes: 2,
            })
        );
        assert_eq!(report.time_column.confidence, Confidence::Low);
    }
}

#[test]
fn an_index_that_advances_on_most_rows_is_still_a_time_index() {
    // Duplicates alone are not disqualifying (SPEC §2.1 "preserved,
    // flagged"): an index that repeats now and then still advances on most
    // rows.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(
        dir.path(),
        "some-dups.csv",
        "t,v\n0,1\n1,2\n1,3\n2,4\n3,5\n3,6\n4,7\n",
    );

    let (_summary, report, _dataset) = ingest::open_dataset(&path).expect("opens");

    assert!(matches!(
        report.time_index,
        TimeIndexSource::Column { index: 0, .. }
    ));
}
