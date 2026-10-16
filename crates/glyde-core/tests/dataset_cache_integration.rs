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

//! Issue #106: reopening a known file must not re-parse its text. The second
//! open of an unchanged file, under the same overrides, is served from the
//! whole-dataset cache — without reading the source at all — and yields the
//! very same summary, inference report and dataset. Anything else (another
//! correction, a damaged or foreign cache, a file too big for memory) falls
//! back to a real parse: the cache fails closed.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{
    self, Dataset, DecimalSeparator, InferenceReport, IngestOverrides, OpenSummary,
};
use glyde_core::series::SeriesValues;

/// Everything worth preserving at once: a semicolon delimiter and comma
/// decimals, UTC offsets, skipped (ragged) rows, a NaN run, and string,
/// bool, i64 and f64 columns.
fn write_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("rich.csv");
    let mut text = String::from("timestamp;value;count;flag;label\n");
    for i in 0..500 {
        let value = if (100..103).contains(&i) {
            "NaN".to_string()
        } else {
            format!("{},{}", i, i % 10)
        };
        text.push_str(&format!(
            "2026-01-01T00:{:02}:{:02}+02:00;{value};{};{};{}\n",
            i / 60,
            i % 60,
            i as i64 * 3 - 7,
            if i % 2 == 0 { "true" } else { "false" },
            ["idle", "run", "défaut ⚠", ""][i % 4],
        ));
        if i == 250 {
            text.push_str("garbage;row\n");
        }
    }
    std::fs::write(&path, text).expect("write fixture");
    path
}

fn open(
    path: &Path,
    overrides: IngestOverrides,
    ram: u64,
    cache: &Path,
) -> glyde_core::Result<(OpenSummary, InferenceReport, Dataset)> {
    ingest::open_dataset_progressive_in(
        path,
        overrides,
        RamBudget::from_total_ram_bytes(ram),
        cache,
        &AtomicBool::new(false),
        |_| {},
    )
}

fn cache_files(cache: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(cache.join("datasets"))
        .map(|entries| entries.map(|entry| entry.expect("entry").path()).collect())
        .unwrap_or_default()
}

/// Every f64 sample by bit pattern, so NaN compares equal to NaN.
fn f64_bits(dataset: &Dataset) -> Vec<Vec<u64>> {
    dataset
        .columns
        .iter()
        .filter_map(|series| match series.values() {
            SeriesValues::F64(values) => Some(values.iter().map(|v| v.to_bits()).collect()),
            _ => None,
        })
        .collect()
}

fn assert_same_open(
    a: &(OpenSummary, InferenceReport, Dataset),
    b: &(OpenSummary, InferenceReport, Dataset),
) {
    assert_eq!(a.0, b.0, "summary");
    assert_eq!(a.1, b.1, "inference report");
    assert_eq!(a.2.time, b.2.time, "time axis");
    assert_eq!(a.2.time_column_name, b.2.time_column_name);
    assert_eq!(f64_bits(&a.2), f64_bits(&b.2), "f64 samples");
    for (x, y) in a.2.columns.iter().zip(&b.2.columns) {
        assert_eq!(x.name(), y.name());
        assert_eq!(x.dtype(), y.dtype());
        assert_eq!(x.anomalies(), y.anomalies(), "{} anomalies", x.name());
        if x.dtype() != glyde_core::series::Dtype::F64 {
            assert_eq!(x.values(), y.values(), "{}", x.name());
        }
    }
}

#[test]
fn a_reopen_is_identical_to_the_first_open() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());

    let first = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect("open");
    // The fixture really exercises what the cache must keep.
    assert_eq!(first.1.decimal_separator.value.as_deref(), Some(","));
    assert!(
        first.1.skipped_row_count > 0,
        "a skipped row is part of the report"
    );
    assert!(!first.2.columns.iter().all(|s| s.anomalies().is_empty()));
    assert_eq!(
        cache_files(cache.path()).len(),
        1,
        "the first open writes the cache"
    );

    let second = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect("reopen");
    assert_same_open(&first, &second);
}

// The heart of the issue: the second open does not read the source text.
// An unreadable source (metadata still visible, contents not) can only
// reopen from the cache; a different correction of it must try to parse it,
// and so fails.
#[cfg(unix)]
#[test]
fn a_reopen_does_not_read_the_source_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());
    let first = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect("open");

    let original = std::fs::metadata(&path).expect("stat").permissions();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    if std::fs::read(&path).is_ok() {
        // Running as root: permissions cannot make the file unreadable, so
        // this test cannot tell a cache hit from a parse.
        std::fs::set_permissions(&path, original).expect("restore");
        return;
    }
    let reopened = open(&path, IngestOverrides::default(), u64::MAX, cache.path());
    let corrected = open(
        &path,
        IngestOverrides {
            decimal_separator: Some(DecimalSeparator::Dot),
            ..IngestOverrides::default()
        },
        u64::MAX,
        cache.path(),
    );
    std::fs::set_permissions(&path, original).expect("restore");

    assert_same_open(
        &first,
        &reopened.expect("a cached reopen needs no read access"),
    );
    assert!(
        corrected.is_err(),
        "a different correction is a different reading, so it must parse the source"
    );
}

#[test]
fn a_damaged_or_foreign_cache_falls_back_to_parsing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());
    let first = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect("open");
    let file = cache_files(cache.path()).pop().expect("cache file");
    let good = std::fs::read(&file).expect("read cache");

    let mut truncated = good.clone();
    truncated.truncate(good.len() - 5);
    let mut overlong = good.clone();
    overlong.push(0);
    let mut future = good.clone();
    future[8..12].copy_from_slice(&99u32.to_ne_bytes());
    let mut garbled = good.clone();
    let middle = good.len() / 2;
    garbled[24..middle.min(64)].fill(b'#');

    for (what, bytes) in [
        ("truncated", truncated),
        ("overlong", overlong),
        ("future version", future),
        ("garbled metadata", garbled),
    ] {
        std::fs::write(&file, bytes).expect("damage cache");
        let reopened = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect(what);
        assert_same_open(&first, &reopened);
        assert_eq!(
            std::fs::read(&file).expect("read cache"),
            good,
            "{what}: the fallback parse rewrites a good cache"
        );
    }
}

#[test]
fn a_file_too_large_for_memory_is_not_cached() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());

    let (_, _, dataset) = open(&path, IngestOverrides::default(), 0, cache.path()).expect("open");
    assert!(dataset.is_spilled());
    assert!(cache_files(cache.path()).is_empty());
}
