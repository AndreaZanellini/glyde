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

//! Issue #85's headline claim, as a build-blocking assertion: the **whole**
//! open pipeline — `ingest::open_dataset_with_budget`, so the SPEC §2.1–2.2
//! statistics `ingest::report` derives about the time column, not only the
//! materialization `tests/spilled_peak_rss.rs` covers — costs an amount of
//! memory that does *not* grow with the file.
//!
//! `tests/spilled_peak_rss.rs` is the same gate for `ingest::load_with_budget`
//! (issue #75: the sample data itself). It passed while this one could not:
//! with the samples on disk, the remaining growth was the derived Δt
//! statistics, which allocated a `Vec` of deltas per open (16 B/row), an `f64`
//! copy of them (8 B/row), a third `Vec` of absolute deviations for the MAD
//! (8 B/row), and made the whole mapped tick column resident by scanning it
//! end to end (16 B/row) — 48 B/row, or ~0.42x file size, which is
//! proportional and so cannot satisfy SPEC §5's flat `min(25% RAM, 4 GB)` cap
//! on a large enough file.
//!
//! Its own test binary (one test, one process) for the same reason as its
//! sibling: `VmHWM` is a process-wide high-water mark, so a second test
//! sharing the process would measure the first one's peak, and the two opens
//! below must run smallest first.
//!
//! Linux-only: `/proc/self/status`'s `VmHWM` has no portable equivalent. The
//! cross-platform measurement is `glyde-devtools`' `memory_gate` harness
//! (docs/QUALITY.md §3), which measures this same `open_dataset` pipeline
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

/// A clean ISO 8601 + four-`f64`-column CSV of `rows` data rows, on a
/// deliberately *jittered* millisecond cadence: consecutive Δt take many
/// distinct values spread over a wide range, so the bounded-memory median this
/// test guards is exercised on its multi-pass path rather than on the
/// single-distinct-Δt shortcut a perfectly uniform fixture would take.
fn write_fixture(path: &std::path::Path, rows: usize) -> u64 {
    let mut file = std::fs::File::create(path).expect("create fixture");
    let mut buffer = String::with_capacity(1 << 16);
    buffer.push_str("timestamp,a,b,c,d\n");
    for i in 0..rows {
        // Monotonic to the second, jittered within it: a 1 ms nominal cadence
        // with up to 999 ms of jitter on each sample.
        buffer.push_str(&format!(
            "2026-01-01T00:{:02}:{:02}.{:03}Z,{}.{:03},{}.{:03},{}.{:03},{}.{:03}\n",
            (i / 1000 / 60) % 60,
            (i / 1000) % 60,
            (i * 7919) % 1000,
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
fn opening_a_file_sixteen_times_larger_costs_essentially_no_extra_memory() {
    /// How much extra peak RSS the second, much larger open is allowed to add.
    /// A fixed allowance, not a fraction of the file: the point is that the
    /// open path's footprint is a handful of fixed-size buffers — the bounded
    /// read buffer, the decode buffer, one write buffer per column, one tick
    /// chunk, one bucket-count table per order statistic — none of which
    /// depend on how many rows went past. Generous enough to absorb allocator
    /// behaviour, far too small to hide the 48 B/row the Δt statistics used to
    /// cost: the extra 1.5 M rows below would have added ~72 MB of that.
    const EXTRA_ALLOWANCE_BYTES: u64 = 24 * 1024 * 1024;
    const SMALL_ROWS: usize = 100_000;
    const LARGE_ROWS: usize = SMALL_ROWS * 16;

    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");

    let small_path = dir.path().join("small.csv");
    let small_bytes = write_fixture(&small_path, SMALL_ROWS);
    let (small_summary, _report, small) =
        glyde_core::ingest::open_dataset_with_budget(&small_path, zero_budget(), cache.path())
            .expect("small fixture must open");
    assert!(
        small.is_spilled(),
        "a zero budget must force the spill path"
    );
    assert_eq!(small.time.len(), SMALL_ROWS);
    assert_eq!(small_summary.row_count, SMALL_ROWS as u64);
    drop(small);
    let after_small = peak_rss_bytes();

    let large_path = dir.path().join("large.csv");
    let large_bytes = write_fixture(&large_path, LARGE_ROWS);
    let (large_summary, _report, large) =
        glyde_core::ingest::open_dataset_with_budget(&large_path, zero_budget(), cache.path())
            .expect("large fixture must open");
    assert!(large.is_spilled());
    assert_eq!(large.time.len(), LARGE_ROWS);
    assert_eq!(large_summary.row_count, LARGE_ROWS as u64);
    // The summary is the whole point of this pipeline: assert it was actually
    // derived, so a future change that made it cheap by making it empty could
    // not pass this test.
    assert_eq!(large_summary.sampling_class, small_summary.sampling_class);
    assert_eq!(
        large_summary.timestamp_format,
        Some("iso8601".to_string()),
        "the fixture's ISO 8601 timestamps must be recognized as such"
    );
    drop(large);
    let after_large = peak_rss_bytes();

    let extra = after_large.saturating_sub(after_small);
    assert!(
        extra <= EXTRA_ALLOWANCE_BYTES,
        "opening a {large_bytes}-byte file after a {small_bytes}-byte one raised peak RSS by \
         {extra} bytes, over the {EXTRA_ALLOWANCE_BYTES}-byte allowance — the full open \
         pipeline's memory use, statistics included, must not grow with file size (issue #85, \
         SPEC §5 \"Peak RSS\")"
    );
}
