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

//! Issue #75's headline claim, as a build-blocking assertion: materializing a
//! file through the spill path costs an amount of memory that does **not**
//! grow with the file.
//!
//! `tests/peak_rss.rs` is the same kind of gate for the in-memory path, and
//! is deliberately a *ratio* of file size — that path really is proportional,
//! which is exactly what issue #75 reports. This one is not a ratio: it opens
//! a small fixture, then a fixture several times larger, and asserts the
//! second open added almost nothing to the process's peak RSS. A path whose
//! footprint scaled with file size could not pass it no matter how small the
//! constant.
//!
//! Its own test binary (one test, one process) so the measurement isn't
//! polluted by unrelated tests sharing the process — `VmHWM` is a
//! process-wide high-water mark, not a per-test one, which is also why the
//! two opens below must run in this order, smallest first.
//!
//! Linux-only: `/proc/self/status`'s `VmHWM` has no portable equivalent. The
//! cross-platform measurement is `glyde-devtools`' `memory_gate` harness
//! (docs/QUALITY.md §3), which measures the whole `open_dataset` pipeline
//! against the SPEC §5 cap on all three CI runners.

#![cfg(target_os = "linux")]

use glyde_core::budget::RamBudget;
use std::io::Write as _;

/// Peak resident set size (`VmHWM`, in bytes) of this process so far.
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

/// A clean ISO 8601 + four-`f64`-column CSV of `rows` data rows — the same
/// shape `glyde-devtools`' fixture generator writes, so this exercises the
/// realistic case rather than a degenerate one.
fn write_fixture(path: &std::path::Path, rows: usize) -> u64 {
    let mut file = std::fs::File::create(path).expect("create fixture");
    let mut buffer = String::with_capacity(1 << 16);
    buffer.push_str("timestamp,a,b,c,d\n");
    for i in 0..rows {
        buffer.push_str(&format!(
            "2026-01-01T00:00:{:02}.{:03}Z,{}.{:03},{}.{:03},{}.{:03},{}.{:03}\n",
            i % 60,
            i % 1000,
            i % 977,
            i % 1000,
            i % 811,
            i % 1000,
            i % 653,
            i % 1000,
            i % 499,
            i % 1000,
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

/// Nothing is ever affordable in memory, so both opens take the spill path.
fn zero_budget() -> RamBudget {
    RamBudget::from_total_ram_bytes(0)
}

#[test]
fn spilling_a_file_eight_times_larger_costs_essentially_no_extra_memory() {
    /// How much extra peak RSS the second, much larger open is allowed to
    /// add. This is a fixed allowance, not a fraction of the file: the point
    /// is that the spill path's footprint is a handful of fixed-size buffers
    /// (the bounded read buffer, the decode buffer, one write buffer per
    /// column) plus the process's own baseline, none of which depend on how
    /// many rows went past. Generous enough to absorb allocator behaviour and
    /// the page cache's own accounting, far too small to hide a footprint
    /// that grew with the file.
    const EXTRA_ALLOWANCE_BYTES: u64 = 24 * 1024 * 1024;
    const SMALL_ROWS: usize = 60_000;
    const LARGE_ROWS: usize = SMALL_ROWS * 8;

    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");

    let small_path = dir.path().join("small.csv");
    let small_bytes = write_fixture(&small_path, SMALL_ROWS);
    let small = glyde_core::ingest::load_with_budget(&small_path, zero_budget(), cache.path())
        .expect("small fixture must open");
    assert!(
        small.is_spilled(),
        "a zero budget must force the spill path"
    );
    assert_eq!(small.time.len(), SMALL_ROWS);
    drop(small);
    let after_small = peak_rss_bytes();

    let large_path = dir.path().join("large.csv");
    let large_bytes = write_fixture(&large_path, LARGE_ROWS);
    let large = glyde_core::ingest::load_with_budget(&large_path, zero_budget(), cache.path())
        .expect("large fixture must open");
    assert!(large.is_spilled());
    assert_eq!(large.time.len(), LARGE_ROWS);
    drop(large);
    let after_large = peak_rss_bytes();

    let extra = after_large.saturating_sub(after_small);
    assert!(
        extra <= EXTRA_ALLOWANCE_BYTES,
        "opening a {large_bytes}-byte file after a {small_bytes}-byte one raised peak RSS by \
         {extra} bytes, over the {EXTRA_ALLOWANCE_BYTES}-byte allowance — the spill path's \
         memory use must not grow with file size (issue #75, SPEC §5 \"Peak RSS\")"
    );
}
