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

//! Issue #119 part 2: on a multi-GB file a wrong time column is visible in
//! the progressive preview for minutes, so the time-column picker must be
//! offered while the file is still loading. Every progress checkpoint
//! therefore carries the file's header and where its time index comes from,
//! and says whether that decision can still change before the open ends:
//! final on the spilled path (pass 1 scanned the whole file first),
//! provisional in memory (a later row running backwards can still demote
//! the candidate).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, Checkpoint, GeneratedIndexReason, TimeIndexSource};

const ROWS: usize = 50_000;

fn write_fixture(dir: &Path, header: &str, row: impl Fn(usize) -> String) -> PathBuf {
    let path = dir.join("signals.csv");
    let mut text = format!("{header}\n");
    for i in 0..ROWS {
        text.push_str(&row(i));
        text.push('\n');
    }
    std::fs::File::create(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .expect("write fixture");
    path
}

fn timestamped(dir: &Path) -> PathBuf {
    write_fixture(dir, "timestamp,a,b", |i| {
        format!(
            "2026-01-01T{:02}:{:02}:{:02}Z,{},{}",
            i / 3600,
            (i / 60) % 60,
            i % 60,
            i as f64 * 0.5,
            i as i64 - 7
        )
    })
}

/// No time column: the first column jumps back and forth, so Glyde generates
/// a row index (SPEC §2.1 "Files without a time column").
fn without_time_column(dir: &Path) -> PathBuf {
    write_fixture(dir, "ax,ay", |i| {
        format!("{},{}", if i % 2 == 0 { 1.5 } else { -1.5 }, i)
    })
}

fn checkpoints(path: &Path, budget: RamBudget) -> Vec<Checkpoint> {
    let cache = tempfile::tempdir().expect("temp cache dir");
    let mut seen = Vec::new();
    ingest::load_progressive_with_budget(path, budget, cache.path(), |checkpoint| {
        seen.push(checkpoint)
    })
    .expect("open");
    assert!(!seen.is_empty(), "a {ROWS}-row file must checkpoint");
    seen
}

fn in_memory() -> RamBudget {
    RamBudget::from_total_ram_bytes(u64::MAX)
}

fn spilled() -> RamBudget {
    RamBudget::from_total_ram_bytes(0)
}

#[test]
fn checkpoints_carry_the_header_and_the_time_column_they_read() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = timestamped(dir.path());

    for (budget, final_at_once) in [(in_memory(), false), (spilled(), true)] {
        for checkpoint in checkpoints(&path, budget) {
            assert_eq!(checkpoint.column_names, ["timestamp", "a", "b"]);
            assert_eq!(
                checkpoint.time_index,
                TimeIndexSource::Column {
                    index: 0,
                    name: "timestamp".to_string()
                }
            );
            assert_eq!(checkpoint.time_index_final, final_at_once);
        }
    }
}

#[test]
fn checkpoints_say_when_the_row_index_was_generated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = without_time_column(dir.path());

    for budget in [in_memory(), spilled()] {
        let last = checkpoints(&path, budget).pop().expect("checkpoints");
        assert_eq!(last.column_names, ["ax", "ay"]);
        assert!(
            matches!(
                last.time_index,
                TimeIndexSource::Generated(GeneratedIndexReason::NotMonotonic { .. })
            ),
            "{:?}",
            last.time_index
        );
    }
}
