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

//! Closes issue #92 (split from #81, sibling of `pyramid_reopen_integration.rs`):
//! `ingest::level0_for_dataset_cached` wires `index::level0` into the
//! *completed-load* open path, so a second open of an unchanged file serves
//! each numeric column's raw `(timestamp, value)` pairs from the memory-mapped
//! cache instead of re-deriving them from `Dataset`'s in-memory columns —
//! docs/ROADMAP.md M3 "Level-0 typed spill cache … reopen is instant". Skipping
//! the CSV text parse itself stays out of scope (issue #92's own body: a
//! bigger, product-shaped question), so this only proves the numeric-sample
//! half, mirroring exactly what `pyramid_reopen_integration.rs` already proves
//! for the pyramid half.

use glyde_core::ingest::{level0_for_dataset_cached_with_cache_dir, IngestOverrides, Level0Cache};
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

/// A fresh, uncached build's raw samples for `dataset`'s numeric columns, for
/// comparing against a cached build — since there is no "uncached level0"
/// entry point (a `Level0Cache` is always backed by mapped files, unlike the
/// pyramid's plain `Vec<Vec<Bucket>>`), this reads what the in-memory dataset
/// itself would report via the same `as_f64_slice`/`to_f64_vec` fallback
/// `views::time::column_f64_samples` uses.
fn uncached_samples(dataset: &glyde_core::ingest::Dataset) -> Vec<Option<Vec<f64>>> {
    dataset
        .columns
        .iter()
        .map(|series| match series.values().as_f64_slice() {
            Some(samples) => Some(samples.to_vec()),
            None => series.values().to_f64_vec(),
        })
        .collect()
}

fn cached_samples(caches: &[Option<Level0Cache>]) -> Vec<Option<Vec<f64>>> {
    caches
        .iter()
        .map(|cache| cache.as_ref().map(|c| c.samples().to_vec()))
        .collect()
}

#[test]
fn a_second_open_serves_every_columns_raw_samples_from_cache() {
    let file = multi_column_csv(5_000);
    let dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let uncached = uncached_samples(&dataset);
    let first_open = level0_for_dataset_cached_with_cache_dir(
        file.path(),
        &dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );

    assert_eq!(
        cached_samples(&first_open),
        uncached,
        "a freshly cached level0 build must equal the dataset's own in-memory samples"
    );
    for cache in &first_open {
        assert!(
            cache.as_ref().is_some_and(|c| c.sample_count() == 5_000),
            "every numeric column must produce a cache covering every row"
        );
    }

    // Reopen against `file`'s path but with a *different* dataset (same
    // shape, different numeric content, loaded from a different source
    // file): if the second call rebuilt instead of hitting the cache, it
    // would return samples over *this* dataset's values instead of the ones
    // written to cache above — the same "wrong data on a hit" proof
    // `pyramid_reopen_integration.rs` uses for the pyramid half.
    let other_file = multi_column_csv_with(5_000, |t| (t * 2.0 + 1.0, t * -3.0 - 7.0));
    let other_dataset = glyde_core::ingest::load(other_file.path()).expect("load must succeed");
    assert_ne!(
        uncached_samples(&other_dataset),
        uncached,
        "the other dataset must actually hold different samples, or this test proves nothing"
    );

    let second_open = level0_for_dataset_cached_with_cache_dir(
        file.path(),
        &other_dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );
    assert_eq!(
        cached_samples(&second_open),
        cached_samples(&first_open),
        "the second open must serve the cache written under `file`'s path, not rebuild from \
         whatever dataset happens to be passed in"
    );
}

#[test]
fn two_columns_of_the_same_file_do_not_share_a_cached_level0_entry() {
    let file = multi_column_csv(2_000);
    let dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let cached = level0_for_dataset_cached_with_cache_dir(
        file.path(),
        &dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );

    assert_eq!(cached.len(), 2, "the fixture has two numeric data columns");
    assert_ne!(
        cached[0].as_ref().map(Level0Cache::samples),
        cached[1].as_ref().map(Level0Cache::samples),
        "sine and cosine columns must cache to distinct level0 entries, not one shared entry"
    );
    assert_eq!(cached_samples(&cached), uncached_samples(&dataset));
}

/// A file whose date is genuinely ambiguous between `DD/MM` and `MM/DD` (day
/// and month both ≤ 12) on every row — the same shape
/// `pyramid_reopen_integration.rs` uses for its own overrides-collision test.
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
// re-index": correcting a field changes the dataset a path parses to without
// the file on disk changing at all — same path, same size, same mtime, the
// exact "unchanged file" signal `level0_for_dataset_cached` otherwise trusts
// to serve a cache hit. A corrected re-open must not collide with the level0
// entry cached under the pre-correction reading, since a stale entry's
// timestamps would not match the corrected dataset's own ticks.
#[test]
fn different_overrides_for_the_same_path_do_not_share_a_cached_level0_entry() {
    let file = ambiguous_date_csv(2_000);
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let default_dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let default_cached = level0_for_dataset_cached_with_cache_dir(
        file.path(),
        &default_dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );
    assert!(
        default_cached[0]
            .as_ref()
            .is_some_and(|c| c.sample_count() == 2_000),
        "the fixture must produce a full-coverage cache"
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

    let overridden_cached = level0_for_dataset_cached_with_cache_dir(
        file.path(),
        &overridden_dataset,
        cache_dir.path(),
        overrides,
    );
    assert_eq!(
        cached_samples(&overridden_cached),
        uncached_samples(&overridden_dataset),
        "a corrected re-open of the same path must build (and cache) its own level0 entry, \
         never silently serve back the entry cached under a different set of overrides"
    );
}

/// The cached entry's timestamps must be the same ticks
/// `TimeAxis::to_pyramid_ticks` produces for the dataset — not just the
/// samples — since `decimate_viewport` pairs them by index.
#[test]
fn a_cached_entrys_timestamps_match_the_datasets_own_pyramid_ticks() {
    let file = multi_column_csv(500);
    let dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let cached = level0_for_dataset_cached_with_cache_dir(
        file.path(),
        &dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );
    let ticks = dataset.time.to_pyramid_ticks();

    for cache in cached.into_iter().flatten() {
        assert_eq!(cache.timestamps(), ticks.as_ref());
    }
}
