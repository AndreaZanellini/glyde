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

//! Issue #213: the reused caches (Level 0, pyramids, datasets) are capped,
//! and what goes first is what was *used* least recently — not what was
//! written first. A file reopened yesterday keeps its caches over one
//! opened once last week, even though both were first cached long ago.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use glyde_core::budget::RamBudget;
use glyde_core::index::eviction;
use glyde_core::ingest::{self, IngestOverrides};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn write_csv(dir: &Path, name: &str, scale: f64) -> PathBuf {
    let path = dir.join(name);
    let mut text = String::from("timestamp,a,b\n");
    for i in 0..2_000 {
        text.push_str(&format!(
            "2026-01-01T00:{:02}:{:02}Z,{},{}\n",
            (i / 60) % 60,
            i % 60,
            i as f64 * scale,
            (i as f64 * 0.01).sin() * scale
        ));
    }
    std::fs::write(&path, text).expect("write fixture");
    path
}

/// Opens `path` the way the app does: the dataset (cached in `datasets/`),
/// then its Level-0 and pyramid caches.
fn open(path: &Path, cache: &Path) {
    let (_summary, _report, dataset) = ingest::open_dataset_progressive_in(
        path,
        IngestOverrides::default(),
        RamBudget::from_total_ram_bytes(64_000_000_000),
        cache,
        &AtomicBool::new(false),
        |_| {},
    )
    .expect("open");
    let _derived = ingest::derived_caches_for_dataset_cached_with_cache_dir(
        path,
        &dataset,
        cache,
        IngestOverrides::default(),
    );
}

/// Every file under `cache`, recursively.
fn cache_files(cache: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    let mut dirs = vec![cache.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.insert(path);
            }
        }
    }
    files
}

fn set_age(paths: &BTreeSet<PathBuf>, age: Duration) {
    for path in paths {
        OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(SystemTime::now() - age)
            .expect("set mtime");
    }
}

fn total_bytes(paths: &BTreeSet<PathBuf>) -> u64 {
    paths
        .iter()
        .map(|path| std::fs::metadata(path).expect("metadata").len())
        .sum()
}

#[test]
fn a_reopened_file_keeps_its_caches_over_one_not_used_since() {
    let sources = tempfile::tempdir().expect("sources");
    let cache = tempfile::tempdir().expect("cache");
    let reopened = write_csv(sources.path(), "reopened.csv", 1.0);
    let forgotten = write_csv(sources.path(), "forgotten.csv", 2.0);

    open(&reopened, cache.path());
    let reopened_files = cache_files(cache.path());
    open(&forgotten, cache.path());
    let forgotten_files: BTreeSet<_> = cache_files(cache.path())
        .difference(&reopened_files)
        .cloned()
        .collect();
    assert!(
        reopened_files
            .iter()
            .any(|p| p.to_string_lossy().ends_with(".glyds"))
            && reopened_files
                .iter()
                .any(|p| p.to_string_lossy().ends_with(".glypy"))
            && reopened_files
                .iter()
                .any(|p| p.to_string_lossy().ends_with(".ts.glyc0"))
            && reopened_files
                .iter()
                .any(|p| p.to_string_lossy().ends_with(".val.glyc0")),
        "the open must write every kind of reused cache, or this test proves nothing: \
         {reopened_files:?}"
    );
    assert!(!forgotten_files.is_empty());

    // Both were first cached long ago; `reopened` was cached first.
    set_age(&reopened_files, 3 * DAY);
    set_age(&forgotten_files, 2 * DAY);

    // Reopening is a cache hit, and a hit counts as a use.
    open(&reopened, cache.path());
    assert_eq!(
        cache_files(cache.path()),
        &reopened_files | &forgotten_files,
        "the reopen must be served from the caches, not write new ones"
    );

    // Every hit path marks its own file: the Level-0 values and shared
    // timestamps, the pyramid, the dataset cache.
    for path in &reopened_files {
        let age = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .expect("mtime")
            .elapsed()
            .unwrap_or_default();
        assert!(
            age < eviction::RECENT_USE_GUARD,
            "{} was not marked as used by the reopen",
            path.display()
        );
    }

    // Room for one file's caches only.
    let (reopened_bytes, forgotten_bytes) =
        (total_bytes(&reopened_files), total_bytes(&forgotten_files));
    let report = eviction::evict_to_cap(cache.path(), reopened_bytes);

    assert_eq!(report.bytes_before, reopened_bytes + forgotten_bytes);
    assert_eq!(report.bytes_freed, forgotten_bytes);
    assert!(
        reopened_files.iter().all(|path| path.exists()),
        "the file used today keeps every one of its caches"
    );
    assert!(
        forgotten_files.iter().all(|path| !path.exists()),
        "the file not used for two days loses every one of its caches"
    );
}

/// Review of PR #251: a file with several columns has one Level-0 values
/// file and one pyramid per column, plus one shared timestamps file and one
/// dataset cache. Evicting only some of them would keep files that save
/// nothing on the next open (without its timestamps and dataset cache, the
/// file is parsed again anyway) while still counting against the cap. One
/// file's caches go together.
#[test]
fn a_files_caches_are_evicted_all_together_never_in_part() {
    let sources = tempfile::tempdir().expect("sources");
    let cache = tempfile::tempdir().expect("cache");
    let file = write_csv(sources.path(), "two_columns.csv", 1.0);

    open(&file, cache.path());
    let files = cache_files(cache.path());
    let value_files = files
        .iter()
        .filter(|path| path.to_string_lossy().ends_with(".val.glyc0"))
        .count();
    assert_eq!(
        value_files, 2,
        "the fixture must have two cached columns: {files:?}"
    );
    set_age(&files, DAY);

    // One byte over the cap: something must go, and it must be everything.
    let report = eviction::evict_to_cap(cache.path(), total_bytes(&files) - 1);

    assert_eq!(report.evicted_entries, 1, "one file, one entry");
    let left = cache_files(cache.path());
    assert!(left.is_empty(), "left behind: {left:?}");
}
