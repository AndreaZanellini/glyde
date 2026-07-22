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

//! Proves the `Series`/`Dtype` model (docs/ROADMAP.md M2 "Series + dtype
//! model") against two torture-corpus shapes it must represent correctly:
//! case 51 (a constant series, SPEC §1.4's "constant series are valid
//! inputs") and case 47 (three boolean columns spelled three different ways
//! in the source text, SPEC §1.4's "`bool` ... series ... route to the
//! state timeline").
//!
//! No CSV reader exists yet (docs/ROADMAP.md M2, later item), so these
//! tests parse the corpus files' handful of rows directly rather than
//! going through `ingest::open()` — they exist to grade the `Series` model
//! itself, not ingestion. This intentionally does not reuse
//! `tests/support/mod.rs` (the corpus-manifest/golden-test harness): that
//! module's `CorpusCase`/`discover_cases` machinery is unused by a
//! model-only test like this one, and pulling in a whole unused surface
//! trips the `dead_code` lint (each integration-test file is its own
//! crate), so `corpus_dir()` is reproduced here in one line instead.

use glyde_core::series::{Dtype, Series, SeriesValues, ViewKind};
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testdata")
        .join("corpus")
}

fn data_rows(csv: &str) -> Vec<Vec<&str>> {
    csv.lines()
        .skip(1) // header
        .filter(|line| !line.is_empty())
        .map(|line| line.split(',').collect())
        .collect()
}

#[test]
fn corpus_case_51_constant_series_is_a_valid_constant_f64_series() {
    let path = corpus_dir().join("case-51-constant-series.csv");
    let csv = std::fs::read_to_string(&path).expect("read corpus case 51");

    let values: Vec<f64> = data_rows(&csv)
        .into_iter()
        .map(|row| row[1].parse().expect("value column parses as f64"))
        .collect();
    let series = Series::new("value", SeriesValues::F64(values));

    assert_eq!(series.len(), 4);
    assert_eq!(series.dtype(), Dtype::F64);
    assert_eq!(series.view_kind(), ViewKind::TimeDomain);
    assert!(
        series.is_constant(),
        "case 51 is named 'constant series' — every sample must compare equal"
    );
    assert!(
        series.anomalies().is_empty(),
        "a constant value is not itself an anomaly"
    );
}

#[test]
fn corpus_case_47_boolean_columns_parse_regardless_of_source_spelling() {
    let path = corpus_dir().join("case-47-boolean-column.csv");
    let csv = std::fs::read_to_string(&path).expect("read corpus case 47");
    let rows = data_rows(&csv);

    // The three columns spell booleans three different ways in the source
    // text (`true`/`false`, `0`/`1`, `TRUE`/`FALSE`); each must still parse
    // into the same `Dtype::Bool` model and route to the state timeline,
    // whatever the underlying spelling was.
    let flag_lower: Vec<bool> = rows.iter().map(|row| row[1] == "true").collect();
    let flag_numeric: Vec<bool> = rows.iter().map(|row| row[2] == "1").collect();
    let flag_upper: Vec<bool> = rows.iter().map(|row| row[3] == "TRUE").collect();

    assert_eq!(flag_lower, vec![true, false, true, false]);
    assert_eq!(flag_numeric, vec![false, true, false, true]);
    assert_eq!(flag_upper, vec![true, false, true, false]);

    for values in [flag_lower, flag_numeric, flag_upper] {
        let series = Series::new("flag", SeriesValues::Bool(values));

        assert_eq!(series.dtype(), Dtype::Bool);
        assert_eq!(
            series.view_kind(),
            ViewKind::StateTimeline,
            "SPEC §1.4: a bool series must never route to the numeric time-domain plot"
        );
    }
}
