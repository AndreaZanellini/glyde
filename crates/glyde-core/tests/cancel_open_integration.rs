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

//! Issue #119: an open the user has superseded (another file opened, or a
//! correction applied mid-load) must stop reading the file and give back the
//! spill files it wrote, instead of running to the end and doubling I/O and
//! disk usage. The caller raises a flag; the open notices it within a
//! bounded number of rows and returns [`GlydeError::Cancelled`].

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use glyde_core::budget::RamBudget;
use glyde_core::ingest;
use glyde_core::GlydeError;

/// Comfortably past the first progress checkpoint (20 000 rows), so a
/// cancel raised there leaves most of the file unread.
const ROWS: usize = 80_000;

fn write_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("signals.csv");
    let mut text = String::from("timestamp,a,b\n");
    for i in 0..ROWS {
        text.push_str(&format!(
            "2026-01-01T{:02}:{:02}:{:02}Z,{},{}\n",
            i / 3600,
            (i / 60) % 60,
            i % 60,
            i as f64 * 0.5,
            i as i64 - 7,
        ));
    }
    let mut file = std::fs::File::create(&path).expect("create fixture");
    file.write_all(text.as_bytes()).expect("write fixture");
    path
}

fn spill_sets(cache_dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(cache_dir.join("spill")) {
        Ok(entries) => entries
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Spill deletion runs on a background thread, so wait for it.
fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Opens `path` under `budget`, raising the cancel flag from inside the
/// first progress checkpoint. Returns the result and how many checkpoints
/// arrived.
fn open_cancelling_at_first_checkpoint(
    path: &Path,
    budget: RamBudget,
    cache_dir: &Path,
) -> (glyde_core::Result<ingest::Dataset>, usize) {
    let cancelled = AtomicBool::new(false);
    let mut checkpoints = 0;
    let result = ingest::load_progressive_with_budget_cancellable(
        path,
        budget,
        cache_dir,
        &cancelled,
        |_| {
            checkpoints += 1;
            cancelled.store(true, Ordering::Relaxed);
        },
    );
    (result, checkpoints)
}

#[test]
fn a_cancelled_in_memory_open_stops_reading_and_reports_cancelled() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());

    let (result, checkpoints) = open_cancelling_at_first_checkpoint(
        &path,
        RamBudget::from_total_ram_bytes(u64::MAX),
        cache.path(),
    );

    assert!(
        matches!(result, Err(GlydeError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert_eq!(checkpoints, 1, "no checkpoint may follow a cancel");
}

#[test]
fn a_cancelled_spilled_open_stops_and_deletes_its_spill_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());

    let (result, checkpoints) = open_cancelling_at_first_checkpoint(
        &path,
        RamBudget::from_total_ram_bytes(0),
        cache.path(),
    );

    assert!(
        matches!(result, Err(GlydeError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
    assert_eq!(checkpoints, 1, "no checkpoint may follow a cancel");
    assert!(
        eventually(|| spill_sets(cache.path()).is_empty()),
        "a cancelled open must not leave its spill files behind: {:?}",
        spill_sets(cache.path())
    );
}

#[test]
fn an_open_cancelled_before_it_starts_reads_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());
    let cancelled = AtomicBool::new(true);

    for budget in [
        RamBudget::from_total_ram_bytes(u64::MAX),
        RamBudget::from_total_ram_bytes(0),
    ] {
        let mut checkpoints = 0;
        let result = ingest::load_progressive_with_budget_cancellable(
            &path,
            budget,
            cache.path(),
            &cancelled,
            |_| checkpoints += 1,
        );
        assert!(
            matches!(result, Err(GlydeError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(checkpoints, 0);
    }
    assert!(eventually(|| spill_sets(cache.path()).is_empty()));
}

#[test]
fn an_uncancelled_open_is_unchanged() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = write_fixture(dir.path());
    let cancelled = AtomicBool::new(false);

    let dataset = ingest::load_progressive_with_budget_cancellable(
        &path,
        RamBudget::from_total_ram_bytes(u64::MAX),
        cache.path(),
        &cancelled,
        |_| {},
    )
    .expect("open");
    let reference = ingest::load(&path).expect("open");
    assert_eq!(dataset.time, reference.time);
    assert_eq!(dataset.columns.len(), reference.columns.len());
}
