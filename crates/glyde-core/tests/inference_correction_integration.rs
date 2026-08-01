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

//! docs/ROADMAP.md M4 "One-click correction of each field → triggers a
//! re-index" (docs/SPEC.md §1.2 "Each is correctable in one click; correcting
//! triggers a re-index"): [`glyde_core::ingest::IngestOverrides`] lets a
//! caller pin the delimiter, decimal separator, and/or timestamp format that
//! would otherwise be inferred, and a full re-open with it must produce a
//! genuinely different [`Dataset`] — not just a different report field — so a
//! click in `glyde-app`'s inference bar visibly changes the plot (the
//! roadmap item's own maintainer test: "swap delimiter / decimal / date
//! order → plot updates").
//!
//! Every case here is deliberately built so the override changes *materialized
//! values or dtypes*, not only a confidence label: proving the override
//! actually reaches the parse, not just the report.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, DecimalSeparator, Delimiter, IngestOverrides, SamplingClass};
use glyde_core::series::{Dtype, SeriesValues};
use glyde_core::time::TimestampFormat;
use glyde_core::GlydeError;
use std::path::Path;

fn unlimited_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(u64::MAX)
}

fn zero_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(0)
}

/// A comma-decimal value column behind a semicolon delimiter: `infer_column`
/// naturally parses "1,234"/"2,345" as `f64` (1.234, 2.345) because
/// `infer_decimal_separator` sees comma-decimal fields and nothing
/// dot-decimal to compete with them. Forcing `DecimalSeparator::Dot` must
/// leave the literal comma in place, so `.parse::<f64>()` fails and the
/// column falls back to `Dtype::String` — Golden Rule 1: a field the override
/// cannot parse is preserved verbatim, never silently coerced.
#[test]
fn decimal_separator_override_changes_a_column_s_dtype_and_values() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("comma-decimal.csv");
    std::fs::write(
        &path,
        "timestamp;value\n\
         2026-01-01T00:00:00Z;1,234\n\
         2026-01-01T00:00:01Z;2,345\n\
         2026-01-01T00:00:02Z;3,456\n",
    )
    .expect("write fixture");

    let auto = ingest::load_with_overrides_and_budget(
        &path,
        IngestOverrides::default(),
        unlimited_budget(),
        cache.path(),
    )
    .expect("auto-inferred open must succeed");
    assert_eq!(auto.columns[0].dtype(), Dtype::F64);
    assert_eq!(
        auto.columns[0].values(),
        &SeriesValues::F64(vec![1.234, 2.345, 3.456])
    );

    let overridden = ingest::load_with_overrides_and_budget(
        &path,
        IngestOverrides {
            decimal_separator: Some(DecimalSeparator::Dot),
            ..Default::default()
        },
        unlimited_budget(),
        cache.path(),
    )
    .expect("overridden open must still succeed, as a String column");
    assert_eq!(
        overridden.columns[0].dtype(),
        Dtype::String,
        "a Dot override must leave the literal comma unrewritten, so the \
         numeric parse fails and the raw text is kept rather than corrupted"
    );
    assert_eq!(
        overridden.columns[0].values(),
        &SeriesValues::String(vec![
            "1,234".to_string(),
            "2,345".to_string(),
            "3,456".to_string()
        ])
    );
}

/// A clean, unambiguous comma-delimited file. Overriding the delimiter to one
/// that does not appear anywhere in the file must make every line tokenize
/// to a single field, hitting the same [`GlydeError::SingleColumnFile`] a
/// genuinely single-column file would — proof the override reaches the
/// delimiter-inference step, not just relabels the auto-inferred one, and
/// that a bad manual choice fails cleanly instead of guessing something new.
#[test]
fn delimiter_override_that_does_not_occur_in_the_file_reports_a_clean_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("comma.csv");
    std::fs::write(
        &path,
        "timestamp,value\n\
         2026-01-01T00:00:00Z,1.5\n\
         2026-01-01T00:00:01Z,2.5\n",
    )
    .expect("write fixture");

    let auto = ingest::load_with_overrides_and_budget(
        &path,
        IngestOverrides::default(),
        unlimited_budget(),
        cache.path(),
    )
    .expect("auto-inferred open must succeed");
    assert_eq!(auto.columns.len(), 1);

    let err = ingest::load_with_overrides_and_budget(
        &path,
        IngestOverrides {
            delimiter: Some(Delimiter::Semicolon),
            ..Default::default()
        },
        unlimited_budget(),
        cache.path(),
    )
    .expect_err("a delimiter absent from the file must collapse every row to one field");
    assert!(matches!(err, GlydeError::SingleColumnFile));
}

/// Corpus case 28 is every torture-corpus row genuinely ambiguous between
/// `DD/MM` and `MM/DD` (no field > 12 in either position); SPEC §2.1's
/// ambiguity rule defaults to `DayFirst` at low confidence. Overriding to
/// `MonthFirst` (the roadmap's exact maintainer test: "one click swaps
/// `DD/MM` ↔ `MM/DD`") must reparse the same raw text under the other
/// reading, producing different tick values, not just a relabeled format.
#[test]
fn timestamp_format_override_swaps_day_first_for_month_first_and_changes_ticks() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corpus/case-28-fully-ambiguous-dates.csv");

    let (day_first_summary, day_first_report, day_first) =
        ingest::open_dataset_with_overrides(&corpus, IngestOverrides::default())
            .expect("default open must succeed");
    assert_eq!(
        day_first_summary.timestamp_format.as_deref(),
        Some("dd_mm_yyyy")
    );
    assert_eq!(
        day_first_report.timestamp_format.confidence,
        ingest::Confidence::Low,
        "SPEC §2.1's fully-ambiguous fallback must still be reported low-confidence"
    );

    let (month_first_summary, month_first_report, month_first) =
        ingest::open_dataset_with_overrides(
            &corpus,
            IngestOverrides {
                timestamp_format: Some(TimestampFormat::MonthFirst),
                ..Default::default()
            },
        )
        .expect("month-first override must still open the same file");
    assert_eq!(
        month_first_summary.timestamp_format.as_deref(),
        Some("mm_dd_yyyy")
    );
    assert_eq!(
        month_first_report.timestamp_format.confidence,
        ingest::Confidence::High,
        "a deliberate override is a settled choice, never a low-confidence guess (Golden Rule 2)"
    );

    let ingest::TimeAxis::Absolute {
        timestamps: day_first_ticks,
        ..
    } = &day_first.time
    else {
        panic!("case 28 has an absolute timestamp index");
    };
    let ingest::TimeAxis::Absolute {
        timestamps: month_first_ticks,
        ..
    } = &month_first.time
    else {
        panic!("case 28 has an absolute timestamp index");
    };
    assert_ne!(
        day_first_ticks, month_first_ticks,
        "swapping day/month reading of the same ambiguous raw text must change the \
         parsed instants, exactly like the roadmap's \"plot updates\" maintainer test"
    );
}

/// The override must reach the spilled path exactly the same as the
/// in-memory one (issue #75's "same values, same dtypes" invariant already
/// proven for un-overridden opens in `spilled_ingest_integration.rs`), so a
/// user's correction is not silently limited to small files.
#[test]
fn overrides_produce_the_same_dataset_on_both_storage_paths() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("comma-decimal.csv");
    let mut text = String::from("timestamp;value\n");
    for i in 0..2_000 {
        text.push_str(&format!(
            "2026-01-01T00:00:{:02}.{:03}Z;{},{}\n",
            i % 60,
            i % 1000,
            i,
            i % 10
        ));
    }
    std::fs::write(&path, text).expect("write fixture");

    let overrides = IngestOverrides {
        decimal_separator: Some(DecimalSeparator::Comma),
        ..Default::default()
    };

    let in_memory =
        ingest::load_with_overrides_and_budget(&path, overrides, unlimited_budget(), cache.path())
            .expect("in-memory override open must succeed");
    let spilled =
        ingest::load_with_overrides_and_budget(&path, overrides, zero_budget(), cache.path())
            .expect("spilled override open must succeed");

    assert!(!in_memory.is_spilled());
    assert!(spilled.is_spilled());
    assert_eq!(spilled.columns[0].dtype(), in_memory.columns[0].dtype());
    assert_eq!(spilled.columns[0].values(), in_memory.columns[0].values());
}

/// A plain progressive-index file has no ambiguity to correct, but the
/// override plumbing must not be limited to files that already carry an
/// absolute timestamp (docs/ROADMAP.md M4 applies to every field, delimiter
/// and decimal separator included, independent of the time-index kind).
#[test]
fn delimiter_and_decimal_overrides_still_work_on_a_progressive_index_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("progressive-semicolon.csv");
    std::fs::write(
        &path,
        "index;value\n\
         0;1,5\n\
         1;2,5\n\
         2;3,5\n",
    )
    .expect("write fixture");

    let dataset = ingest::load_with_overrides_and_budget(
        &path,
        IngestOverrides {
            delimiter: Some(Delimiter::Semicolon),
            decimal_separator: Some(DecimalSeparator::Comma),
            ..Default::default()
        },
        unlimited_budget(),
        cache.path(),
    )
    .expect("progressive-index file must still open under an override");

    assert!(matches!(dataset.time, ingest::TimeAxis::Progressive { .. }));
    assert_eq!(
        dataset.columns[0].values(),
        &SeriesValues::F64(vec![1.5, 2.5, 3.5])
    );
}

/// [`ingest::open_dataset_progressive_with_overrides`] must apply the same
/// override as the non-progressive entry point — the app's real open path
/// always goes through the progressive variant (`plumbing::run_index_job`),
/// so this is the one M4's UI actually calls.
#[test]
fn progressive_open_applies_the_same_override_as_a_plain_open() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("comma-decimal.csv");
    std::fs::write(
        &path,
        "timestamp;value\n\
         2026-01-01T00:00:00Z;1,5\n\
         2026-01-01T00:00:01Z;2,5\n\
         2026-01-01T00:00:02Z;3,5\n",
    )
    .expect("write fixture");

    let overrides = IngestOverrides {
        decimal_separator: Some(DecimalSeparator::Comma),
        ..Default::default()
    };

    let mut checkpoints = 0usize;
    let (_summary, report, dataset) =
        ingest::open_dataset_progressive_with_overrides(&path, overrides, |_checkpoint| {
            checkpoints += 1;
        })
        .expect("progressive override open must succeed");

    assert_eq!(
        dataset.columns[0].values(),
        &SeriesValues::F64(vec![1.5, 2.5, 3.5])
    );
    assert_eq!(
        report.decimal_separator.confidence,
        ingest::Confidence::High
    );
    assert_eq!(report.sampling_class, SamplingClass::Uniform);
}
