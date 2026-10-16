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

//! Issue #110: sorting a spilled file by time ([Sort], SPEC §2.1) must stay
//! as flat in memory as spilling it does (`tests/spilled_peak_rss.rs`): the
//! external sort holds one run of rows and a fixed handful of buffers, never
//! anything proportional to the file. A zero RAM budget makes the runs as
//! small as they get, so both fixtures below are cut into many runs and the
//! larger one into eight times as many.
//!
//! Its own test binary (one test, one process): `VmHWM` is a process-wide
//! high-water mark, which is also why the smaller sort runs first.

#![cfg(target_os = "linux")]

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, IngestOverrides};
use std::io::Write as _;

fn peak_rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .expect("parse VmHWM value");
            return kb * 1024;
        }
    }
    panic!("VmHWM not found in /proc/self/status");
}

/// Timestamps scrambled across the whole file, so every run holds rows from
/// everywhere and the merge interleaves all of them.
fn write_fixture(path: &std::path::Path, rows: usize) -> u64 {
    let mut file = std::fs::File::create(path).expect("create fixture");
    let mut buffer = String::with_capacity(1 << 16);
    buffer.push_str("timestamp,a,b,label\n");
    for i in 0..rows {
        let second = (i * 7_919) % rows;
        buffer.push_str(&format!(
            "2026-01-01T{:02}:{:02}:{:02}Z,{}.{:03},{},s{}\n",
            (second / 3600) % 24,
            (second / 60) % 60,
            second % 60,
            i % 977,
            i % 1000,
            i % 811,
            i % 13,
        ));
        if buffer.len() > 1 << 16 {
            file.write_all(buffer.as_bytes()).expect("write fixture");
            buffer.clear();
        }
    }
    file.write_all(buffer.as_bytes()).expect("write fixture");
    file.flush().expect("flush fixture");
    file.metadata().expect("stat fixture").len()
}

fn sort_spilled(path: &std::path::Path, cache: &std::path::Path, rows: usize) {
    let dataset = ingest::load_with_overrides_and_budget(
        path,
        IngestOverrides {
            sort_by_time: true,
            ..IngestOverrides::default()
        },
        RamBudget::from_total_ram_bytes(0),
        cache,
    )
    .expect("fixture must open sorted");
    assert!(
        dataset.is_spilled(),
        "a zero budget must force the spill path"
    );
    assert_eq!(dataset.time.len(), rows);
}

#[test]
fn sorting_a_file_eight_times_larger_costs_essentially_no_extra_memory() {
    /// Same fixed allowance as `spilled_peak_rss.rs`, for the same reason:
    /// room for allocator noise and the merge's per-run read buffers (at most
    /// `SORT_FAN_IN` of them), far too small to hide a footprint that grew
    /// with the file.
    const EXTRA_ALLOWANCE_BYTES: u64 = 24 * 1024 * 1024;
    const SMALL_ROWS: usize = 60_000;
    const LARGE_ROWS: usize = SMALL_ROWS * 8;

    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");

    let small_path = dir.path().join("small.csv");
    let small_bytes = write_fixture(&small_path, SMALL_ROWS);
    sort_spilled(&small_path, cache.path(), SMALL_ROWS);
    let after_small = peak_rss_bytes();

    let large_path = dir.path().join("large.csv");
    let large_bytes = write_fixture(&large_path, LARGE_ROWS);
    sort_spilled(&large_path, cache.path(), LARGE_ROWS);
    let after_large = peak_rss_bytes();

    let extra = after_large.saturating_sub(after_small);
    assert!(
        extra <= EXTRA_ALLOWANCE_BYTES,
        "sorting a {large_bytes}-byte spilled file after a {small_bytes}-byte one raised peak \
         RSS by {extra} bytes, over the {EXTRA_ALLOWANCE_BYTES}-byte allowance — the on-disk \
         sort's memory use must not grow with file size (issue #110, SPEC §5 \"Peak RSS\")"
    );
}
