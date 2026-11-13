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

//! Issue #228, SPEC §1.4: "`i64` values beyond ±2⁵³ must not be converted to
//! `f64` for rendering without being flagged in the log and in the inference
//! bar." Corpus case 50 holds three such values in its `value` column; the
//! open must name that column in the report, whichever storage the RAM
//! budget picked, and leave the raw `i64` samples untouched.

use std::path::{Path, PathBuf};

use glyde_core::budget::RamBudget;
use glyde_core::index::spill::{SpillSet, SpillVecWriter};
use glyde_core::ingest::{open_dataset_with_budget, PrecisionLossColumn};
use glyde_core::series::{SeriesValues, SpilledValues};

fn case_50() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus/case-50-i64-beyond-2-53.csv")
}

/// 2⁵³: the largest magnitude an `f64` holds exactly.
const EXACT: i64 = 1 << 53;

#[test]
fn values_beyond_exact_f64_counts_only_magnitudes_above_2_pow_53() {
    let i64s = SeriesValues::I64(vec![
        0,
        EXACT,
        -EXACT,
        EXACT + 1,
        -(EXACT + 1),
        i64::MAX,
        i64::MIN,
    ]);
    assert_eq!(i64s.values_beyond_exact_f64(), 4);

    let u64s = SeriesValues::U64(vec![0, EXACT as u64, EXACT as u64 + 1, u64::MAX]);
    assert_eq!(u64s.values_beyond_exact_f64(), 2);

    // Narrower integers and floats are never rounded by the plot's `f64`.
    assert_eq!(
        SeriesValues::I32(vec![i32::MAX]).values_beyond_exact_f64(),
        0
    );
    assert_eq!(SeriesValues::F64(vec![1e300]).values_beyond_exact_f64(), 0);
    assert_eq!(SeriesValues::Bool(vec![true]).values_beyond_exact_f64(), 0);
}

#[test]
fn values_beyond_exact_f64_counts_spilled_columns_the_same_way() {
    let dir = tempfile::tempdir().expect("temp dir");
    let set = SpillSet::create(dir.path()).expect("spill set");

    let mut i64s = SpillVecWriter::create(&set, "i").expect("create");
    i64s.extend_from_slice(&[EXACT, EXACT + 1, i64::MIN])
        .expect("write");
    let i64s = SeriesValues::Spilled(SpilledValues::I64(i64s.finish().expect("finish")));
    assert_eq!(i64s.values_beyond_exact_f64(), 2);

    let mut u64s = SpillVecWriter::create(&set, "u").expect("create");
    u64s.extend_from_slice(&[1u64, u64::MAX]).expect("write");
    let u64s = SeriesValues::Spilled(SpilledValues::U64(u64s.finish().expect("finish")));
    assert_eq!(u64s.values_beyond_exact_f64(), 1);
}

fn assert_case_50_is_flagged(budget: RamBudget, storage: &str) {
    let cache = tempfile::tempdir().expect("temp dir");
    let (summary, report, dataset) =
        open_dataset_with_budget(&case_50(), budget, cache.path()).expect("case 50 opens");

    assert_eq!(
        report.precision_loss_columns,
        vec![PrecisionLossColumn {
            name: "value".to_string(),
            value_count: 3,
        }],
        "{storage}: the inference report must name the column"
    );
    assert_eq!(
        summary.precision_loss_columns,
        vec!["value".to_string()],
        "{storage}: the corpus summary must name the column"
    );

    // Golden Rule 1: flagging is all it does — the raw i64 samples are intact.
    let value = dataset
        .columns
        .iter()
        .find(|series| series.name() == "value")
        .expect("value column");
    assert_eq!(
        value.values().display_at(2).as_deref(),
        Some("9223372036854775807")
    );
    assert_eq!(
        value.values().display_at(0).as_deref(),
        Some("9007199254740993")
    );
}

#[test]
fn case_50_is_flagged_when_opened_in_memory() {
    assert_case_50_is_flagged(RamBudget::from_total_ram_bytes(u64::MAX), "in memory");
}

#[test]
fn case_50_is_flagged_when_spilled() {
    assert_case_50_is_flagged(RamBudget::from_total_ram_bytes(0), "spilled");
}

#[test]
fn a_file_within_the_exact_range_flags_nothing() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus/case-01-comma-clean.csv");
    let cache = tempfile::tempdir().expect("temp dir");
    let (summary, report, _dataset) = open_dataset_with_budget(
        &path,
        RamBudget::from_total_ram_bytes(u64::MAX),
        cache.path(),
    )
    .expect("case 1 opens");
    assert!(report.precision_loss_columns.is_empty());
    assert!(summary.precision_loss_columns.is_empty());
}
