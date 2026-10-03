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

//! Issue #118: a spilled open must not leave its spill files behind. Every
//! open used to leave about the file's own size in the cache directory,
//! forever — two opens of a 3.2 GB CSV filled a maintainer's disk. Now the
//! files of an open live exactly as long as its dataset, a reopen never
//! accumulates a second copy once the first is dropped, and the loose files
//! earlier versions left are cleared by the next spilled open.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use glyde_core::budget::RamBudget;
use glyde_core::ingest;

fn write_fixture(dir: &Path, row_count: usize) -> PathBuf {
    let path = dir.join("signals.csv");
    let mut text = String::from("timestamp,a,b,state\n");
    for i in 0..row_count {
        text.push_str(&format!(
            "2026-01-01T00:{:02}:{:02}Z,{},{},{}\n",
            (i / 60) % 60,
            i % 60,
            i as f64 * 0.5,
            i as i64 - 7,
            if i % 2 == 0 { "on" } else { "off" }
        ));
    }
    let mut file = std::fs::File::create(&path).expect("create fixture");
    file.write_all(text.as_bytes()).expect("write fixture");
    path
}

/// Nothing fits in memory: every open spills.
fn zero_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(0)
}

/// The spill-set directories currently in `cache_dir`.
fn spill_sets(cache_dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(cache_dir.join("spill")) {
        Ok(entries) => entries
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Every spill file (finished or interrupted) anywhere under `cache_dir`.
fn spill_files(cache_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![cache_dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".glysp") || name.ends_with(".glysp.tmp"))
            {
                found.push(path);
            }
        }
    }
    found
}

/// Deletion runs on a background thread (it can be gigabytes, and the last
/// reference may be dropped on the UI thread), so wait for it.
fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn closing_a_spilled_dataset_deletes_its_spill_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path(), 2_000);

    let dataset = ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("open");
    assert!(dataset.is_spilled());
    assert_eq!(spill_sets(cache.path()).len(), 1);
    assert!(
        !spill_files(cache.path()).is_empty(),
        "an open dataset is backed by its spill files"
    );

    drop(dataset);
    assert!(
        eventually(|| spill_files(cache.path()).is_empty() && spill_sets(cache.path()).is_empty()),
        "closing the dataset must delete its spill files: {:?}",
        spill_files(cache.path())
    );
}

// The scenario from the issue: reopening the same file must not add a second
// copy of it to the cache for good. The app replaces the dataset on reopen,
// so only one copy is ever on disk once the previous one is dropped.
#[test]
fn reopening_the_same_file_keeps_one_copy_on_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path(), 2_000);

    let mut current = ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("open");
    let first_files = spill_files(cache.path()).len();
    for _ in 0..3 {
        let reopened =
            ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("reopen");
        // The previous dataset is still mapped while the new one is read, and
        // both stay readable — a reopen never overwrites a live file.
        assert_eq!(reopened.time, current.time);
        current = reopened;
        assert!(
            eventually(|| spill_sets(cache.path()).len() == 1),
            "the replaced dataset's set must be deleted: {:?}",
            spill_sets(cache.path())
        );
        assert_eq!(spill_files(cache.path()).len(), first_files);
    }
}

// Every earlier version wrote spill files straight into the cache directory
// and never deleted them; the next spilled open gives that space back.
#[test]
fn a_spilled_open_clears_the_files_earlier_versions_left_behind() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path(), 500);
    let legacy = [
        cache.path().join("00112233aabbccdd.c0.glysp"),
        cache.path().join("00112233aabbccdd.ts.glysp.tmp"),
    ];
    let level0_cache = cache.path().join("00112233aabbccdd.ts.glyc0");
    for file in legacy.iter().chain([&level0_cache]) {
        std::fs::write(file, [0u8; 64]).expect("write");
    }

    let _dataset = ingest::load_with_budget(&path, zero_budget(), cache.path()).expect("open");

    assert!(legacy.iter().all(|file| !file.exists()));
    assert!(
        level0_cache.is_file(),
        "a reusable Level-0 cache file is not a spill file and must be kept"
    );
}

// A file small enough for memory never touches the disk cache at all.
#[test]
fn an_in_memory_open_writes_no_spill_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path(), 500);

    let dataset = ingest::load_with_budget(
        &path,
        RamBudget::from_total_ram_bytes(u64::MAX),
        cache.path(),
    )
    .expect("open");

    assert!(!dataset.is_spilled());
    assert!(spill_files(cache.path()).is_empty());
}
