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

//! Issue #88's Golden Rule 1 gate: a spilled dataset's pyramid is built by
//! streaming its memory-mapped columns in bounded chunks rather than by
//! handing `dsp::decimation::build_pyramid` one slice over each — and the
//! result must be **indistinguishable** from what the in-memory path produces
//! for the same file. Storage change, not a data change.
//!
//! `tests/golden/decimation.rs` locks the two *builders* to each other at
//! every chunk size; this locks the `ingest` dispatch that chooses between
//! them, including the dtype promotion (`i64` → `f64`) and the tick mapping
//! (absolute ticks, and a progressive axis's fixed-point scaling) each path
//! reaches through a different code route. `tests/spilled_pyramid_peak_rss.rs`
//! is the other half: that the streamed path is actually bounded.

use glyde_core::budget::RamBudget;
use glyde_core::dsp::decimation::Bucket;
use glyde_core::ingest::{self, IngestOverrides};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// A budget so large that no realistic file could exceed it — forces the
/// in-memory path.
fn unlimited_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(u64::MAX)
}

/// A zero budget — nothing is affordable in memory, so every file spills.
fn zero_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(0)
}

/// An ISO 8601 time index plus one `f64` column (with a NaN run, SPEC §1.3)
/// and one `i64` column, so both the zero-copy `f64` sample path and the
/// promoted-dtype one are covered. Enough rows for a multi-level pyramid.
fn write_absolute_fixture(dir: &Path, row_count: usize) -> PathBuf {
    let path = dir.join("absolute.csv");
    let mut file = std::fs::File::create(&path).expect("create fixture");
    let mut text = String::from("timestamp,reading,count\n");
    for i in 0..row_count {
        let reading = if i % 97 == 0 {
            "NaN".to_string()
        } else {
            format!("{:.4}", (i as f64 * 0.037).sin() * 1000.0)
        };
        text.push_str(&format!(
            "2026-01-01T00:00:{:02}.{:03}Z,{reading},{}\n",
            i % 60,
            i % 1000,
            i as i64 * 7 - 3,
        ));
        if text.len() > 64 * 1024 {
            file.write_all(text.as_bytes()).expect("write fixture");
            text.clear();
        }
    }
    file.write_all(text.as_bytes()).expect("write fixture");
    file.flush().expect("flush fixture");
    path
}

/// The same shape as [`write_absolute_fixture`] but with visibly different
/// sample values, for proving a cache *hit* served the first file's pyramid
/// rather than rebuilding from whatever dataset it was handed.
fn write_shifted_fixture(dir: &Path, row_count: usize) -> PathBuf {
    let path = dir.join("shifted.csv");
    let mut file = std::fs::File::create(&path).expect("create fixture");
    let mut text = String::from("timestamp,reading,count\n");
    for i in 0..row_count {
        text.push_str(&format!(
            "2026-01-01T00:00:{:02}.{:03}Z,{:.4},{}\n",
            i % 60,
            i % 1000,
            i as f64 * 2.0 + 1.0,
            i as i64 * -3 - 7,
        ));
        if text.len() > 64 * 1024 {
            file.write_all(text.as_bytes()).expect("write fixture");
            text.clear();
        }
    }
    file.write_all(text.as_bytes()).expect("write fixture");
    file.flush().expect("flush fixture");
    path
}

/// A progressive numeric index (SPEC §2.1) on a deliberately uneven, non-
/// integral spacing, so the fixed-point tick scaling the spilled path applies
/// chunk-by-chunk has to match the whole-column mapping exactly.
fn write_progressive_fixture(dir: &Path, row_count: usize) -> PathBuf {
    let path = dir.join("progressive.csv");
    let mut file = std::fs::File::create(&path).expect("create fixture");
    let mut text = String::from("x,reading\n");
    for i in 0..row_count {
        let x = i as f64 * 0.125 + (i % 7) as f64 * 0.001;
        text.push_str(&format!("{x:.6},{:.4}\n", (i as f64 * 0.019).cos() * 500.0));
        if text.len() > 64 * 1024 {
            file.write_all(text.as_bytes()).expect("write fixture");
            text.clear();
        }
    }
    file.write_all(text.as_bytes()).expect("write fixture");
    file.flush().expect("flush fixture");
    path
}

/// Compares two pyramids bucket for bucket, floats by *bit pattern*: SPEC §1.3
/// keeps NaN samples and `NaN == NaN` is false, so an all-NaN bucket's `+∞`
/// /`−∞` envelope and every real min/max must be identical bytes, not merely
/// "equal enough".
fn assert_same_pyramid(streamed: &[Vec<Bucket>], expected: &[Vec<Bucket>], what: &str) {
    assert_eq!(
        streamed.len(),
        expected.len(),
        "{what}: level count must match"
    );
    for (level, (got_level, want_level)) in streamed.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            got_level.len(),
            want_level.len(),
            "{what}: level {level} bucket count must match"
        );
        for (index, (got, want)) in got_level.iter().zip(want_level.iter()).enumerate() {
            assert_eq!(
                got.min.to_bits(),
                want.min.to_bits(),
                "{what}: level {level} bucket {index} min"
            );
            assert_eq!(
                got.max.to_bits(),
                want.max.to_bits(),
                "{what}: level {level} bucket {index} max"
            );
            assert_eq!(
                got.first_ts, want.first_ts,
                "{what}: level {level} bucket {index} first_ts"
            );
            assert_eq!(
                got.last_ts, want.last_ts,
                "{what}: level {level} bucket {index} last_ts"
            );
            assert_eq!(
                got.nan_count, want.nan_count,
                "{what}: level {level} bucket {index} nan_count"
            );
        }
    }
}

fn assert_multi_level(pyramids: &[Option<Vec<Vec<Bucket>>>], expected_columns: usize) {
    assert_eq!(pyramids.len(), expected_columns);
    for pyramid in pyramids {
        let levels = pyramid.as_ref().expect("every column here is numeric");
        assert!(
            levels.len() > 1,
            "the fixture must be large enough to produce a multi-level pyramid, \
             or this test proves nothing about the aggregation above level 0"
        );
    }
}

#[test]
fn a_spilled_absolute_axis_pyramid_matches_the_in_memory_one_exactly() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_absolute_fixture(dir.path(), 20_000);

    let in_memory = ingest::load_with_budget(&path, unlimited_budget(), cache.path())
        .expect("in-memory open must succeed");
    let spilled = ingest::load_with_budget(&path, zero_budget(), cache.path())
        .expect("spilled open must succeed");
    assert!(!in_memory.is_spilled());
    assert!(spilled.is_spilled());

    let expected = ingest::pyramids_for_dataset(&in_memory);
    let streamed = ingest::pyramids_for_dataset(&spilled);

    assert_multi_level(&expected, 2);
    assert_multi_level(&streamed, 2);
    for (index, (got, want)) in streamed.iter().zip(expected.iter()).enumerate() {
        assert_same_pyramid(
            got.as_ref().expect("numeric column"),
            want.as_ref().expect("numeric column"),
            &format!("column {index}"),
        );
    }
}

#[test]
fn a_spilled_progressive_axis_pyramid_matches_the_in_memory_one_exactly() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_progressive_fixture(dir.path(), 20_000);

    let in_memory = ingest::load_with_budget(&path, unlimited_budget(), cache.path())
        .expect("in-memory open must succeed");
    let spilled = ingest::load_with_budget(&path, zero_budget(), cache.path())
        .expect("spilled open must succeed");
    assert!(!in_memory.is_spilled());
    assert!(spilled.is_spilled());

    let expected = ingest::pyramids_for_dataset(&in_memory);
    let streamed = ingest::pyramids_for_dataset(&spilled);

    assert_multi_level(&expected, 1);
    assert_multi_level(&streamed, 1);
    assert_same_pyramid(
        streamed[0].as_ref().expect("numeric column"),
        expected[0].as_ref().expect("numeric column"),
        "progressive column 0",
    );
}

// The cached entry point takes the same streaming build on a miss, so a
// spilled dataset must both (a) produce the same pyramid the uncached build
// produces, and (b) serve the identical thing back from the cache on a second
// open — proven the same way `pyramid_reopen_integration.rs` proves it for the
// in-memory half: the second call is made against a dataset whose *values*
// differ, so a result matching the first call can only have come from the
// cache.
#[test]
fn a_spilled_datasets_cached_pyramid_matches_the_uncached_one_and_is_reused_on_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spill = tempfile::tempdir().expect("temp spill dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_absolute_fixture(dir.path(), 20_000);

    let spilled = ingest::load_with_budget(&path, zero_budget(), spill.path())
        .expect("spilled open must succeed");
    assert!(spilled.is_spilled());

    let uncached = ingest::pyramids_for_dataset(&spilled);
    let first_open = ingest::pyramids_for_dataset_cached_with_cache_dir(
        &path,
        &spilled,
        cache.path(),
        IngestOverrides::default(),
    );
    assert_multi_level(&first_open, 2);
    for (index, (got, want)) in first_open.iter().zip(uncached.iter()).enumerate() {
        assert_same_pyramid(
            got.as_ref().expect("numeric column"),
            want.as_ref().expect("numeric column"),
            &format!("cached vs uncached column {index}"),
        );
    }

    // Reopen against the first fixture's *path* but with a different, spilled
    // dataset loaded from a different source file: a rebuild would return a
    // pyramid over these other values, so a result matching `first_open` can
    // only have come from the cache written under `path`.
    let other_dir = tempfile::tempdir().expect("temp dir");
    let other_path = write_shifted_fixture(other_dir.path(), 20_000);
    let other = ingest::load_with_budget(&other_path, zero_budget(), spill.path())
        .expect("second spilled open must succeed");
    assert!(other.is_spilled());
    assert_ne!(
        ingest::pyramids_for_dataset(&other),
        first_open,
        "the other dataset must actually build a different pyramid, or this test proves nothing"
    );

    let second_open = ingest::pyramids_for_dataset_cached_with_cache_dir(
        &path,
        &other,
        cache.path(),
        IngestOverrides::default(),
    );
    for (index, (got, want)) in second_open.iter().zip(first_open.iter()).enumerate() {
        assert_same_pyramid(
            got.as_ref().expect("numeric column"),
            want.as_ref().expect("numeric column"),
            &format!("reopened column {index}"),
        );
    }
}
