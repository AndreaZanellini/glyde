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

//! Issue #275: crafted ~1 KB Parquet files (corpus cases 69-110, found by
//! fuzzing) kept an open busy for up to minutes inside the `parquet` crate's
//! Thrift reader, where Cancel cannot reach. `corpus_open_compare.rs` checks
//! what each one is refused with; these tests check that it is refused
//! *quickly*, opened the way the app does, in memory and spilled to disk.
//!
//! The limit is on a whole family of files, in both modes. parquet 60.2.0
//! refuses each in milliseconds. On 60.1.0 the fastest one took about 0.3
//! seconds and the slowest minutes, so each family is far over the limit.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, IngestOverrides};

const FAMILY_LIMIT: Duration = Duration::from_secs(10);

fn family(kind: &str) -> Vec<PathBuf> {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus");
    let infix = format!("-parquet-{kind}-hang-");
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus)
        .expect("read testdata/corpus")
        .map(|entry| entry.expect("corpus entry").path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.contains(&infix) && name.ends_with(".parquet")
        })
        .collect();
    files.sort();
    files
}

/// Opens every file in both modes on a worker thread, so that a hang fails
/// the test with the name of the file instead of hanging the test run.
fn assert_refused_quickly(files: Vec<PathBuf>) {
    let (sender, results) = mpsc::channel();
    let opens: Vec<(PathBuf, bool)> = files
        .into_iter()
        .flat_map(|file| [(file.clone(), false), (file, true)])
        .collect();
    let queue = opens.clone();
    std::thread::spawn(move || {
        for (file, spilled) in queue {
            let total_ram = if spilled { 0 } else { u64::MAX };
            let cache = tempfile::tempdir().expect("temp cache dir");
            let refused = ingest::open_dataset_progressive_in(
                &file,
                IngestOverrides::default(),
                RamBudget::from_total_ram_bytes(total_ram),
                cache.path(),
                &AtomicBool::new(false),
                |_| {},
            )
            .is_err();
            if sender.send(refused).is_err() {
                return;
            }
        }
    });

    let started = Instant::now();
    for (done, (file, spilled)) in opens.iter().enumerate() {
        let left = FAMILY_LIMIT.saturating_sub(started.elapsed());
        let refused = results.recv_timeout(left).unwrap_or_else(|_| {
            panic!(
                "{} (spilled: {spilled}) was still opening after {:?} for the family; {done} of \
                 {} opens were done",
                file.display(),
                started.elapsed(),
                opens.len()
            )
        });
        assert!(
            refused,
            "{} (spilled: {spilled}) must be refused, but it opened",
            file.display()
        );
    }
}

#[test]
fn files_that_hung_the_page_header_reader_are_refused_quickly() {
    let files = family("page-header");
    assert_eq!(files.len(), 30, "corpus cases 69-98");
    assert_refused_quickly(files);
}

#[test]
fn files_that_hung_the_footer_reader_are_refused_quickly() {
    let files = family("footer");
    assert_eq!(files.len(), 12, "corpus cases 99-110");
    assert_refused_quickly(files);
}
