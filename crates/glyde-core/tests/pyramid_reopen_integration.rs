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

//! Proves issue #81's pyramid half end to end: `ingest::pyramids_for_dataset_cached`
//! wires `index::pyramid` into the *completed-load* open path, so a second
//! open of an unchanged file serves each numeric column's pyramid from cache
//! instead of rebuilding it — docs/ROADMAP.md M3 "Pyramid level spill …
//! reopening rebuilds the pyramid from cached Level 0 rather than loading it
//! too". Level 0 (raw-sample) wiring and skipping the CSV text parse itself
//! are out of scope here — split into a follow-up issue, same as #80/#88/#90
//! split adjacent M3 concerns.

use glyde_core::ingest::{
    pyramids_for_dataset, pyramids_for_dataset_cached_with_cache_dir, IngestOverrides,
};
use glyde_core::time::TimestampFormat;

fn multi_column_csv(row_count: usize) -> tempfile::NamedTempFile {
    multi_column_csv_with(row_count, |t| {
        ((t * 0.017).sin() * 1000.0, (t * 0.011).cos() * 500.0)
    })
}

fn multi_column_csv_with(
    row_count: usize,
    formula: impl Fn(f64) -> (f64, f64),
) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("create temp file");
    let mut text = String::from("time,sine,cosine\n");
    for i in 0..row_count {
        let (a, b) = formula(i as f64);
        text.push_str(&format!("{i},{a},{b}\n"));
    }
    std::io::Write::write_all(&mut file, text.as_bytes()).expect("write temp file");
    file
}

#[test]
fn a_second_open_serves_every_columns_pyramid_from_cache() {
    // Enough rows that each column's pyramid has multiple levels, not just one.
    let file = multi_column_csv(5_000);
    let dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let uncached = pyramids_for_dataset(&dataset);
    let first_open = pyramids_for_dataset_cached_with_cache_dir(
        file.path(),
        &dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );

    assert_eq!(
        first_open, uncached,
        "a freshly cached pyramid must equal an uncached build over the same dataset"
    );
    for pyramid in &first_open {
        assert!(
            pyramid.as_ref().is_some_and(|levels| levels.len() > 1),
            "the fixture must be large enough to produce a multi-level pyramid"
        );
    }

    // Reopen against `file`'s path but with a *different* dataset (same
    // shape, different numeric content, loaded from a different source
    // file): if the second call rebuilt instead of hitting the cache, it
    // would return a pyramid over *this* dataset's values instead of the
    // ones written to cache above — the same "wrong data on a hit" proof
    // pyramid_spill_integration.rs already uses for the bare cache API.
    let other_file = multi_column_csv_with(5_000, |t| (t * 2.0 + 1.0, t * -3.0 - 7.0));
    let other_dataset = glyde_core::ingest::load(other_file.path()).expect("load must succeed");
    assert_ne!(
        pyramids_for_dataset(&other_dataset),
        first_open,
        "the other dataset must actually build a different pyramid, or this test proves nothing"
    );

    let second_open = pyramids_for_dataset_cached_with_cache_dir(
        file.path(),
        &other_dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );
    assert_eq!(
        second_open, first_open,
        "the second open must serve the cache written under `file`'s path, not rebuild from \
         whatever dataset happens to be passed in"
    );
}

#[test]
fn two_columns_of_the_same_file_do_not_share_a_cached_pyramid() {
    let file = multi_column_csv(2_000);
    let dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let cached = pyramids_for_dataset_cached_with_cache_dir(
        file.path(),
        &dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );

    assert_eq!(cached.len(), 2, "the fixture has two numeric data columns");
    assert_ne!(
        cached[0], cached[1],
        "sine and cosine columns must cache to distinct pyramids, not one shared entry"
    );
    // Each per-column pyramid must still equal what a fresh, uncached build
    // over that same column would produce.
    let uncached = pyramids_for_dataset(&dataset);
    assert_eq!(cached, uncached);
}

/// A file whose date is genuinely ambiguous between `DD/MM` and `MM/DD`
/// (day and month both ≤ 12) on every row — the same shape as torture-corpus
/// case 28, built large enough here for a multi-level pyramid. Only the
/// time-of-day varies row to row, so every row's date component is
/// identical and the whole column stays ambiguous throughout.
fn ambiguous_date_csv(row_count: usize) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("create temp file");
    let mut text = String::from("time,value\n");
    for i in 0..row_count {
        let h = i / 3600;
        let m = (i / 60) % 60;
        let s = i % 60;
        text.push_str(&format!(
            "05/07/2026 {h:02}:{m:02}:{s:02},{}\n",
            i as f64 * 0.5
        ));
    }
    std::io::Write::write_all(&mut file, text.as_bytes()).expect("write temp file");
    file
}

// docs/ROADMAP.md M4 "One-click correction of each field → triggers a
// re-index": correcting a field (e.g. the ambiguous day/month swap) changes
// the dataset a path parses to *without the file on disk changing at all* —
// same path, same size, same mtime, the exact "unchanged file" signal
// `pyramids_for_dataset_cached` otherwise trusts to serve a cache hit
// (`a_second_open_serves_every_columns_pyramid_from_cache`, above, locks that
// trust deliberately for the *un-corrected* case). A corrected re-open must
// not collide with the pyramid cached under the pre-correction reading: the
// stale buckets would carry the wrong timestamps for the corrected dataset's
// own ticks — silently wrong at best, an out-of-range query on a total
// mismatch at worst.
#[test]
fn different_overrides_for_the_same_path_do_not_share_a_cached_pyramid() {
    let file = ambiguous_date_csv(2_000);
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let default_dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let default_cached = pyramids_for_dataset_cached_with_cache_dir(
        file.path(),
        &default_dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );
    assert!(
        default_cached[0]
            .as_ref()
            .is_some_and(|levels| levels.len() > 1),
        "the fixture must be large enough to produce a multi-level pyramid"
    );

    let overrides = IngestOverrides {
        timestamp_format: Some(TimestampFormat::MonthFirst),
        ..Default::default()
    };
    let overridden_dataset = glyde_core::ingest::load_with_overrides(file.path(), overrides)
        .expect("a month-first override must still open the same file");
    assert_ne!(
        overridden_dataset.time, default_dataset.time,
        "swapping day/month must actually change the parsed ticks, or this test proves nothing"
    );

    let overridden_cached = pyramids_for_dataset_cached_with_cache_dir(
        file.path(),
        &overridden_dataset,
        cache_dir.path(),
        overrides,
    );
    let uncached = pyramids_for_dataset(&overridden_dataset);
    assert_eq!(
        overridden_cached, uncached,
        "a corrected re-open of the same path must build (and cache) its own pyramid, never \
         silently serve back the pyramid cached under a different set of overrides"
    );
}
