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

//! Issue #233: a partitioned Parquet directory is untrusted input like any
//! file. Symlink loops, links that point nowhere, very deep trees and many
//! empty folders must neither hang, overflow the stack nor stop the real
//! data from opening.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use glyde_core::budget::RamBudget;
use glyde_core::ingest;

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corpus/case-55-partitioned-parquet-directory")
}

/// A copy of corpus case 55 under a fresh temp dir.
fn copy_of_case_55(root: &Path) -> PathBuf {
    let dataset = root.join("dataset");
    let mut pending = vec![(corpus_dir(), dataset.clone())];
    while let Some((from, to)) = pending.pop() {
        std::fs::create_dir_all(&to).expect("mkdir");
        for entry in std::fs::read_dir(&from).expect("read corpus dir") {
            let entry = entry.expect("entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("file type").is_dir() {
                pending.push((entry.path(), target));
            } else {
                std::fs::copy(entry.path(), target).expect("copy");
            }
        }
    }
    dataset
}

fn rows(path: &Path, cache: &Path) -> glyde_core::Result<usize> {
    ingest::open_dataset_with_budget(path, RamBudget::from_total_ram_bytes(u64::MAX), cache)
        .map(|(_, _, dataset)| dataset.time.len())
}

#[test]
fn hostile_entries_in_a_dataset_directory_do_not_stop_the_data_from_opening() {
    let root = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let clean = copy_of_case_55(root.path());
    let expected = rows(&clean, cache.path()).expect("the clean copy opens");
    assert!(expected > 0);

    let hostile = copy_of_case_55(&root.path().join("hostile"));
    // A directory link back to its own parent: following it would loop.
    std::os::unix::fs::symlink("..", hostile.join("date=2026-01-01").join("loop")).expect("link");
    // A link to itself (ELOOP on every stat that follows it).
    std::os::unix::fs::symlink("self", hostile.join("self")).expect("link");
    // A stray link to nothing, named like a note, not like data.
    std::os::unix::fs::symlink("/nonexistent/glyde/README", hostile.join("README")).expect("link");
    // A very deep chain of empty folders: a recursive walk would overflow.
    let mut deep = hostile.join("deep");
    for _ in 0..200 {
        deep = deep.join("d");
    }
    std::fs::create_dir_all(&deep).expect("deep tree");
    // Many empty folders.
    for i in 0..2_000 {
        std::fs::create_dir(hostile.join(format!("empty-{i}"))).expect("empty dir");
    }

    assert_eq!(
        rows(&hostile, cache.path()).expect("the data opens despite the hostile entries"),
        expected
    );
}

#[test]
fn a_parquet_link_that_points_nowhere_is_a_reported_error_not_a_crash() {
    let root = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let dataset = copy_of_case_55(root.path());
    std::os::unix::fs::symlink(
        "/nonexistent/glyde/part-9.parquet",
        dataset.join("part-9.parquet"),
    )
    .expect("link");

    let error = rows(&dataset, cache.path()).expect_err("a data file that cannot be read");
    assert!(
        error.to_string().contains("part-9.parquet"),
        "the error names the file: {error}"
    );
}
