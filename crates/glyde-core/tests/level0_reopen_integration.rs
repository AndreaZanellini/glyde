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
//! `ingest::derived_caches_for_dataset_cached` wires `index::level0` into the
//! *completed-load* open path, so a second open of an unchanged file serves
//! each numeric column's raw `(timestamp, value)` pairs from the memory-mapped
//! cache instead of re-deriving them from `Dataset`'s in-memory columns —
//! docs/ROADMAP.md M3 "Level-0 typed spill cache … reopen is instant". Skipping
//! the CSV text parse itself stays out of scope (issue #92's own body: a
//! bigger, product-shaped question), so this only proves the numeric-sample
//! half, mirroring exactly what `pyramid_reopen_integration.rs` already proves
//! for the pyramid half.

use glyde_core::ingest::{
    derived_caches_for_dataset_cached_with_cache_dir, IngestOverrides, Level0Cache,
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
    let (_first_pyramids, first_open) = derived_caches_for_dataset_cached_with_cache_dir(
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

    let (_second_pyramids, second_open) = derived_caches_for_dataset_cached_with_cache_dir(
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

    let (_pyramids, cached) = derived_caches_for_dataset_cached_with_cache_dir(
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
// exact "unchanged file" signal `derived_caches_for_dataset_cached` otherwise trusts
// to serve a cache hit. A corrected re-open must not collide with the level0
// entry cached under the pre-correction reading, since a stale entry's
// timestamps would not match the corrected dataset's own ticks.
#[test]
fn different_overrides_for_the_same_path_do_not_share_a_cached_level0_entry() {
    let file = ambiguous_date_csv(2_000);
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let default_dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let (_default_pyramids, default_cached) = derived_caches_for_dataset_cached_with_cache_dir(
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

    let (_overridden_pyramids, overridden_cached) =
        derived_caches_for_dataset_cached_with_cache_dir(
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

    let (_pyramids, cached) = derived_caches_for_dataset_cached_with_cache_dir(
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

// ---------------------------------------------------------------------------
// Issue #92 review: the conversion count itself.
//
// Both derived caches need a column as `&[f64]`, which for a non-`f64` dtype
// means a whole-column `SeriesValues::to_f64_vec` — per element, with SPEC
// §1.4's precision-loss check on every `i64`/`u64` value. The first version of
// this PR built the two caches in independent passes, so a *reopen* re-ran
// that conversion twice even though both caches hit. Nothing caught it: every
// fixture above generates decimal columns, which infer as `f64` and take the
// zero-copy `as_f64_slice` branch, so `to_f64_vec` never ran in any of them.
//
// These tests use an `i64` column whose every value exceeds `f64`'s exact
// range (±2⁵³), which makes each converted element emit exactly one
// `warn_if_precision_loss` event. Counting those events is a precise,
// deterministic measure of how many times the column was converted.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Counts `WARN` events emitted from `glyde_core::series` — i.e. SPEC §1.4's
/// per-value precision-loss warning, one per `i64` element actually converted
/// to `f64`. Hand-rolled rather than pulling in `tracing-subscriber` as a
/// dev-dependency for one assertion.
struct PrecisionLossCounter {
    count: Arc<AtomicUsize>,
}

impl tracing::Subscriber for PrecisionLossCounter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN && metadata.target().starts_with("glyde_core")
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target().starts_with("glyde_core::series") {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Runs `body` with the counting subscriber installed on this thread, and
/// returns how many precision-loss warnings it emitted.
fn count_precision_loss_warnings<T>(body: impl FnOnce() -> T) -> (T, usize) {
    let count = Arc::new(AtomicUsize::new(0));
    let subscriber = PrecisionLossCounter {
        count: Arc::clone(&count),
    };
    let guard = tracing::subscriber::set_default(subscriber);
    let value = body();
    drop(guard);
    (value, count.load(Ordering::Relaxed))
}

/// One `i64` data column whose every value is beyond `f64`'s exact integer
/// range, so converting it emits exactly one precision-loss warning per row.
fn large_i64_csv(row_count: usize) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("create temp file");
    let mut text = String::from("time,counts\n");
    for i in 0..row_count {
        // 2^53 + 1 upward: every value needs more than f64's 53-bit mantissa.
        text.push_str(&format!("{i},{}\n", 9_007_199_254_740_993_i64 + i as i64));
    }
    std::io::Write::write_all(&mut file, text.as_bytes()).expect("write temp file");
    file
}

#[test]
fn a_reopen_of_an_i64_column_converts_it_to_f64_no_times_and_a_first_open_exactly_once() {
    const ROWS: usize = 2_000;

    let file = large_i64_csv(ROWS);
    let dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    assert_eq!(
        dataset.columns.len(),
        1,
        "the fixture must have exactly one data column, or the counts below are ambiguous"
    );
    assert_eq!(
        dataset.columns[0].dtype(),
        glyde_core::series::Dtype::I64,
        "the fixture's column must infer as i64, or it takes the zero-copy f64 path and this \
         test proves nothing"
    );

    // First open: both caches miss, so the column is converted — once.
    let ((first_pyramids, first_level0), first_open_conversions) =
        count_precision_loss_warnings(|| {
            derived_caches_for_dataset_cached_with_cache_dir(
                file.path(),
                &dataset,
                cache_dir.path(),
                IngestOverrides::default(),
            )
        });
    assert_eq!(
        first_open_conversions,
        ROWS,
        "a first open must convert the column exactly once ({ROWS} values, one precision-loss \
         warning each) — {first_open_conversions} warnings means it was converted \
         {} times",
        first_open_conversions as f64 / ROWS as f64
    );
    assert!(first_level0[0].is_some(), "level 0 must have been cached");
    assert!(
        first_pyramids[0].is_some(),
        "the pyramid must have been built"
    );

    // Reopen, same path, same cache directory: both caches hit, so nothing
    // should be converted at all — the pyramid is built from (or served
    // alongside) the Level-0 cache's already-converted, memory-mapped samples.
    let ((second_pyramids, second_level0), reopen_conversions) =
        count_precision_loss_warnings(|| {
            derived_caches_for_dataset_cached_with_cache_dir(
                file.path(),
                &dataset,
                cache_dir.path(),
                IngestOverrides::default(),
            )
        });
    assert_eq!(
        reopen_conversions, 0,
        "a reopen must serve both caches without converting the column again — \
         {reopen_conversions} precision-loss warnings means to_f64_vec ran on a cache hit"
    );

    // And it must be the same data, not merely cheap.
    assert_eq!(
        cached_samples(&second_level0),
        cached_samples(&first_level0),
        "the reopened level 0 cache must hold the first open's samples"
    );
    assert_eq!(
        second_pyramids, first_pyramids,
        "the reopened pyramid must equal the first open's"
    );
}

/// The values themselves must survive the cache round trip exactly as
/// `to_f64_vec` would have produced them — skipping the conversion must not
/// mean skipping fidelity (Golden Rule 1).
#[test]
fn an_i64_columns_cached_samples_match_an_in_memory_conversion_exactly() {
    let file = large_i64_csv(500);
    let dataset = glyde_core::ingest::load(file.path()).expect("load must succeed");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");

    let (_pyramids, cached) = derived_caches_for_dataset_cached_with_cache_dir(
        file.path(),
        &dataset,
        cache_dir.path(),
        IngestOverrides::default(),
    );

    assert_eq!(cached_samples(&cached), uncached_samples(&dataset));
}
