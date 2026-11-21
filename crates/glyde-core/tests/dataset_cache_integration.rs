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
use glyde_core::ingest::{self, Dataset, InferenceReport, IngestOverrides, OpenSummary};
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
    use glyde_core::ingest::DecimalSeparator;
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

/// Splits a dataset cache file into its metadata (as JSON) and the raw
/// arrays after it, so a test can rewrite one metadata field and keep the
/// rest of the file byte for byte.
fn split_cache(bytes: &[u8]) -> (Vec<u8>, serde_json::Value, Vec<u8>) {
    let meta_len = u64::from_ne_bytes(bytes[16..24].try_into().expect("8 bytes")) as usize;
    let header = bytes[..24].to_vec();
    let meta = serde_json::from_slice(&bytes[24..24 + meta_len]).expect("metadata is JSON");
    (header, meta, bytes[24 + meta_len..].to_vec())
}

fn join_cache(mut header: Vec<u8>, meta: &serde_json::Value, payload: &[u8]) -> Vec<u8> {
    let meta = serde_json::to_vec(meta).expect("serialize metadata");
    header[16..24].copy_from_slice(&(meta.len() as u64).to_ne_bytes());
    header.extend_from_slice(&meta);
    header.extend_from_slice(payload);
    header
}

// Issue #233: the cache lives in a user-writable directory, so its metadata
// is untrusted input like any source file. A count or index that does not
// fit the arrays the file actually holds must be a cache miss, never an
// unbounded allocation or an out-of-range index later on (a panic is an
// abort in a release build).
#[test]
fn a_cache_whose_metadata_disagrees_with_its_arrays_falls_back_to_parsing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());
    let first = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect("open");
    let file = cache_files(cache.path()).pop().expect("cache file");
    let good = std::fs::read(&file).expect("read cache");
    let (header, meta, payload) = split_cache(&good);
    let rows = meta["rows"].as_u64().expect("rows");
    assert!(meta["time_format"].is_string(), "an absolute time axis");

    type Tamper = fn(&mut serde_json::Value, u64);
    let tampers: [(&str, Tamper); 10] = [
        ("an enormous row count", |m, _| {
            m["rows"] = (1u64 << 60).into()
        }),
        ("a row count beyond usize", |m, _| {
            m["rows"] = u64::MAX.into()
        }),
        ("a NaN run past the last row", |m, rows| {
            m["columns"][0]["nan_runs"] = serde_json::json!([[0, rows + 10]])
        }),
        ("a NaN run that ends before it starts", |m, _| {
            m["columns"][0]["nan_runs"] = serde_json::json!([[5, 2]])
        }),
        ("an outlier past the last row", |m, rows| {
            m["columns"][0]["outliers"] = serde_json::json!([rows + 10])
        }),
        ("a skipped row past the last row", |m, rows| {
            m["columns"][0]["skipped_rows"] = serde_json::json!([rows + 10])
        }),
        ("more skipped rows than the outcome skipped", |m, _| {
            let skipped = m["outcome"]["skipped_row_count"].as_u64().expect("count");
            m["columns"][0]["skipped_rows"] = (0..=skipped).collect::<Vec<u64>>().into()
        }),
        ("a time column that does not exist", |m, _| {
            m["inference"]["time_column"] = 99.into()
        }),
        ("fewer column names than the source had", |m, _| {
            m["outcome"]["column_names"] = serde_json::json!(["timestamp"])
        }),
        ("more data columns than column names", |m, _| {
            let extra = m["columns"][0].clone();
            let columns = m["columns"].as_array_mut().expect("columns");
            for _ in 0..8 {
                columns.push(extra.clone());
            }
        }),
    ];
    for (what, tamper) in tampers {
        let mut tampered = meta.clone();
        tamper(&mut tampered, rows);
        std::fs::write(&file, join_cache(header.clone(), &tampered, &payload))
            .expect("tamper cache");
        let reopened = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect(what);
        assert_same_open(&first, &reopened);
        assert_eq!(
            std::fs::read(&file).expect("read cache"),
            good,
            "{what}: the fallback parse rewrites a good cache"
        );
    }
}

// Issue #277 (found by the `cache_reopen` fuzz target): a string column is
// stored as one end offset per row, then the UTF-8 bytes. An end offset that
// points far past the end of the file sized the string's buffer before the
// file length was checked, so a damaged cache aborted the app on a
// multi-exabyte allocation. It must be a cache miss like any other damage.
#[test]
fn a_cache_whose_string_offsets_point_past_the_file_falls_back_to_parsing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());
    let first = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect("open");
    let file = cache_files(cache.path()).pop().expect("cache file");
    let good = std::fs::read(&file).expect("read cache");
    let (header, meta, payload) = split_cache(&good);
    let last = meta["columns"]
        .as_array()
        .expect("columns")
        .last()
        .expect("a column");
    assert_eq!(last["dtype"], "string", "the fixture's last column is text");

    // The file ends with the last string column: its end offsets, then its
    // bytes. The last offset is the byte count that follows it.
    let last_end_at = (0..payload.len() - 8)
        .rev()
        .find(|&at| {
            let end = u64::from_ne_bytes(payload[at..at + 8].try_into().expect("8 bytes"));
            at as u64 + 8 + end == payload.len() as u64
        })
        .expect("the last string end offset");

    for (what, end) in [
        ("a string end far past the file", 1u64 << 60),
        ("a string end just past the file", payload.len() as u64),
    ] {
        let mut tampered = payload.clone();
        tampered[last_end_at..last_end_at + 8].copy_from_slice(&end.to_ne_bytes());
        std::fs::write(&file, join_cache(header.clone(), &meta, &tampered)).expect("tamper cache");
        let reopened = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect(what);
        assert_same_open(&first, &reopened);
        assert_eq!(
            std::fs::read(&file).expect("read cache"),
            good,
            "{what}: the fallback parse rewrites a good cache"
        );
    }
}

// Issue #277 (found by the `cache_reopen` fuzz target): no parser produces a
// tick beyond ±(i128::MAX / 2), and the gap statistics subtract neighbouring
// ticks, so a cache holding an extreme tick overflowed that subtraction.
// Such a cache is damaged, never a dataset to summarize.
#[test]
fn a_cache_whose_timestamps_lie_beyond_any_parse_falls_back_to_parsing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());
    let first = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect("open");
    let file = cache_files(cache.path()).pop().expect("cache file");
    let good = std::fs::read(&file).expect("read cache");
    let (header, meta, payload) = split_cache(&good);
    assert!(meta["time_format"].is_string(), "an absolute time axis");

    // The time axis comes first: one 32-byte packed timestamp per row, its
    // first 16 bytes the `i128` tick.
    const PACKED: usize = 32;
    for (what, tick) in [
        ("the smallest tick", i128::MIN),
        ("the largest tick", i128::MAX),
        ("just past half the range", i128::MAX / 2 + 1),
    ] {
        let mut tampered = payload.clone();
        let row = 131;
        tampered[row * PACKED..row * PACKED + 16].copy_from_slice(&tick.to_ne_bytes());
        std::fs::write(&file, join_cache(header.clone(), &meta, &tampered)).expect("tamper cache");
        let reopened = open(&path, IngestOverrides::default(), u64::MAX, cache.path()).expect(what);
        assert_same_open(&first, &reopened);
        assert_eq!(
            std::fs::read(&file).expect("read cache"),
            good,
            "{what}: the fallback parse rewrites a good cache"
        );
    }
}
