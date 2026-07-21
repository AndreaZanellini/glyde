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

//! Torture-corpus loader and open-vs-expected comparison harness
//! (docs/QUALITY.md §1, docs/ROADMAP.md M1).
//!
//! Each corpus case is a data file (or, for partitioned Parquet, a directory)
//! in `testdata/corpus/` paired with a `<name>.expected.json` describing what
//! a correct `open()` must produce. [`OpenSummary`] is that schema; it is
//! also the shape a future `glyde_core::ingest::open()` (docs/ROADMAP.md M2)
//! will return, so [`compare`] can gate real ingestion once that lands.

use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// SPEC §2.2-2.3 sampling classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SamplingClass {
    Uniform,
    SegmentedUniform,
    Irregular,
    /// A progressive integer index with no absolute time (corpus case 35).
    ProgressiveIndex,
}

/// What a correct `open()` of a corpus file produces. Mirrors QUALITY.md §1's
/// list verbatim: "inferred encoding/delimiter/decimal/time column/format,
/// row count, skipped-row count, sampling class, gap count."
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OpenSummary {
    pub encoding: String,
    pub delimiter: Option<String>,
    pub decimal_separator: Option<String>,
    pub time_column: Option<String>,
    pub timestamp_format: Option<String>,
    pub row_count: u64,
    pub skipped_row_count: u64,
    pub sampling_class: SamplingClass,
    pub gap_count: u64,
}

/// A single field disagreement between an actual open() and its expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub field: &'static str,
    pub expected: String,
    pub actual: String,
}

/// Compares an actual open() result against its `.expected.json`, field by
/// field, returning every disagreement (empty = match).
pub fn compare(actual: &OpenSummary, expected: &OpenSummary) -> Vec<Mismatch> {
    let mut mismatches = Vec::new();

    macro_rules! check {
        ($field:ident) => {
            if actual.$field != expected.$field {
                mismatches.push(Mismatch {
                    field: stringify!($field),
                    expected: format!("{:?}", expected.$field),
                    actual: format!("{:?}", actual.$field),
                });
            }
        };
    }

    check!(encoding);
    check!(delimiter);
    check!(decimal_separator);
    check!(time_column);
    check!(timestamp_format);
    check!(row_count);
    check!(skipped_row_count);
    check!(sampling_class);
    check!(gap_count);

    mismatches
}

/// A corpus case: its `.expected.json` and the sibling data file/directory
/// sharing its stem.
#[derive(Debug, Clone)]
pub struct CorpusCase {
    pub name: String,
    pub expected_path: PathBuf,
    pub data_path: PathBuf,
}

/// `testdata/corpus/`, resolved from this crate's manifest dir so the test
/// works regardless of the invoking working directory.
pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testdata")
        .join("corpus")
}

/// Finds every `<name>.expected.json` directly under `dir` and pairs it with
/// the sibling file/directory named `<name>` or `<name>.<ext>`. Missing data
/// files are reported as a case whose `data_path` does not exist, so callers
/// can produce a precise failure message rather than a silent skip.
pub fn discover_cases(dir: &Path) -> std::io::Result<Vec<CorpusCase>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();

    let mut cases = Vec::new();
    for expected_path in &entries {
        let Some(file_name) = expected_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = file_name.strip_suffix(".expected.json") else {
            continue;
        };

        let data_path = entries
            .iter()
            .find(|candidate| {
                *candidate != expected_path
                    && candidate
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n == stem || n.starts_with(&format!("{stem}.")))
            })
            .cloned()
            .unwrap_or_else(|| dir.join(stem));

        cases.push(CorpusCase {
            name: stem.to_string(),
            expected_path: expected_path.clone(),
            data_path,
        });
    }

    Ok(cases)
}

/// Parses a `.expected.json` file against the [`OpenSummary`] schema.
pub fn load_expected(path: &Path) -> Result<OpenSummary, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OpenSummary {
        OpenSummary {
            encoding: "utf-8".to_string(),
            delimiter: Some(",".to_string()),
            decimal_separator: Some(".".to_string()),
            time_column: Some("timestamp".to_string()),
            timestamp_format: Some("iso8601".to_string()),
            row_count: 100,
            skipped_row_count: 0,
            sampling_class: SamplingClass::Uniform,
            gap_count: 0,
        }
    }

    #[test]
    fn compare_reports_no_mismatches_for_identical_summaries() {
        assert_eq!(compare(&sample(), &sample()), Vec::new());
    }

    #[test]
    fn compare_reports_every_disagreeing_field() {
        let actual = OpenSummary {
            delimiter: Some(";".to_string()),
            row_count: 99,
            sampling_class: SamplingClass::Irregular,
            ..sample()
        };

        let mismatches = compare(&actual, &sample());
        let fields: Vec<&str> = mismatches.iter().map(|m| m.field).collect();

        assert_eq!(fields, vec!["delimiter", "row_count", "sampling_class"]);
    }

    #[test]
    fn load_expected_parses_a_well_formed_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("case.expected.json");
        fs::write(
            &path,
            r#"{
                "encoding": "utf-8",
                "delimiter": ",",
                "decimal_separator": ".",
                "time_column": "timestamp",
                "timestamp_format": "iso8601",
                "row_count": 10,
                "skipped_row_count": 0,
                "sampling_class": "uniform",
                "gap_count": 0
            }"#,
        )
        .expect("write expected.json");

        let expected = load_expected(&path).expect("valid expected.json must parse");
        assert_eq!(expected.encoding, "utf-8");
        assert_eq!(expected.sampling_class, SamplingClass::Uniform);
    }

    #[test]
    fn load_expected_rejects_malformed_json() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("case.expected.json");
        fs::write(&path, "{ not json").expect("write malformed json");

        assert!(load_expected(&path).is_err());
    }

    #[test]
    fn discover_cases_pairs_expected_json_with_its_sibling_data_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        fs::write(dir.path().join("case1.csv"), "a,b\n1,2\n").expect("write data file");
        fs::write(dir.path().join("case1.expected.json"), "{}").expect("write expected.json");

        let cases = discover_cases(dir.path()).expect("read corpus dir");

        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "case1");
        assert!(cases[0].data_path.ends_with("case1.csv"));
    }

    #[test]
    fn discover_cases_pairs_expected_json_with_a_sibling_directory() {
        let dir = tempfile::tempdir().expect("create temp dir");
        fs::create_dir(dir.path().join("case2")).expect("create partitioned dir");
        fs::write(dir.path().join("case2.expected.json"), "{}").expect("write expected.json");

        let cases = discover_cases(dir.path()).expect("read corpus dir");

        assert_eq!(cases.len(), 1);
        assert!(cases[0].data_path.ends_with("case2"));
        assert!(cases[0].data_path.is_dir());
    }

    #[test]
    fn discover_cases_reports_a_missing_data_file_instead_of_silently_skipping() {
        let dir = tempfile::tempdir().expect("create temp dir");
        fs::write(dir.path().join("orphan.expected.json"), "{}").expect("write expected.json");

        let cases = discover_cases(dir.path()).expect("read corpus dir");

        assert_eq!(cases.len(), 1);
        assert!(!cases[0].data_path.exists());
    }

    #[test]
    fn discover_cases_on_a_missing_directory_returns_empty() {
        let cases = discover_cases(Path::new("/nonexistent/glyde-corpus-dir"))
            .expect("a missing corpus dir is not an error");
        assert!(cases.is_empty());
    }
}
