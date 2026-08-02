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

//! Issue #88's headline claim, as a build-blocking assertion: building the
//! min/max pyramid over a **spilled** dataset costs no memory beyond the
//! pyramid itself — the source columns are read in bounded chunks, never made
//! resident.
//!
//! `tests/spilled_open_peak_rss.rs` proves the same thing for the *open*
//! pipeline (issue #85), and passed while this one could not: with the samples
//! on disk and the Δt statistics bounded, `ingest::pyramids_for_dataset`
//! remained a whole-column reader. It handed `dsp::decimation::build_pyramid`
//! one contiguous slice over the memory-mapped tick column (16 B/row) and one
//! over each memory-mapped sample column (8 B/row per column), and
//! `build_pyramid` walks its input end to end — so every page of both became
//! resident, which is memory proportional to file size and cannot fit SPEC
//! §5's flat `min(25% RAM, 4 GB)` cap on a large enough file.
//!
//! **What this test does and does not bound.** The pyramid *itself* is
//! `O(rows)` — roughly `size_of::<Bucket>() / (PYRAMID_FACTOR - 1)` ≈ 9 bytes
//! per sample per column, summed over every level — and that is not what issue
//! #88 was about. So the assertion below subtracts the pyramid's own measured
//! bucket bytes and bounds what is left: the *reading* of the source columns,
//! which must be a handful of fixed-size buffers regardless of row count.
//! Bounding the pyramid's own size is a separate, still-open question
//! (issue #102) about whether a rendered pyramid must be owned in RAM at all.
//!
//! Its own test binary (one test, one process) for the same reason as its
//! siblings: `VmHWM` is a process-wide high-water mark, so a second test
//! sharing the process would measure the first one's peak, and the two builds
//! below must run smallest first.
//!
//! Linux-only: `/proc/self/status`'s `VmHWM` has no portable equivalent. The
//! cross-platform measurement is `glyde-devtools`' `memory_gate` harness
//! (docs/QUALITY.md §3), which builds a real pyramid over its multi-GB fixture
//! on all three CI runners.

#![cfg(target_os = "linux")]

use glyde_core::budget::RamBudget;
use glyde_core::dsp::decimation::Bucket;
use glyde_core::ingest::{self, Dataset};
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

/// A clean ISO 8601 + four-`f64`-column CSV of `rows` data rows, with a NaN
/// every 97th sample (SPEC §1.3: kept, never interpolated — and counted in
/// every bucket it lands in, so the aggregation cannot be skipped).
fn write_fixture(path: &std::path::Path, rows: usize) -> u64 {
    let mut file = std::fs::File::create(path).expect("create fixture");
    let mut buffer = String::with_capacity(1 << 16);
    buffer.push_str("timestamp,a,b,c,d\n");
    for i in 0..rows {
        let a = if i % 97 == 0 {
            "NaN".to_string()
        } else {
            format!("{}.{:03}", i % 977, i % 1000)
        };
        buffer.push_str(&format!(
            "2026-01-01T00:{:02}:{:02}.{:03}Z,{a},{}.{:03},{}.{:03},{}.{:03}\n",
            (i / 1000 / 60) % 60,
            (i / 1000) % 60,
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

/// Every bucket, every level, every column — the pyramid's own unavoidable
/// footprint, which the assertion subtracts before bounding what is left.
fn pyramid_bytes(pyramids: &[Option<Vec<Vec<Bucket>>>]) -> u64 {
    pyramids
        .iter()
        .flatten()
        .flat_map(|levels| levels.iter())
        .map(|level| (level.len() * std::mem::size_of::<Bucket>()) as u64)
        .sum()
}

/// Fails the test unless every column really produced a multi-level pyramid,
/// so "cheap because it built nothing" can never pass as "cheap because it
/// streamed".
fn assert_every_column_pyramided(pyramids: &[Option<Vec<Vec<Bucket>>>], dataset: &Dataset) {
    assert_eq!(pyramids.len(), dataset.columns.len());
    assert_eq!(pyramids.len(), 4, "the fixture has four numeric columns");
    for (index, pyramid) in pyramids.iter().enumerate() {
        let levels = pyramid
            .as_ref()
            .unwrap_or_else(|| panic!("column {index} is numeric and must have a pyramid"));
        assert!(
            levels.len() > 1,
            "column {index} must produce a multi-level pyramid at this row count"
        );
        assert!(!levels[0].is_empty());
    }
}

#[test]
fn pyramiding_a_file_sixteen_times_larger_costs_essentially_no_extra_memory() {
    /// How much extra peak RSS the second, much larger pyramid build is
    /// allowed to add *beyond the buckets it produces*. A fixed allowance, not
    /// a fraction of the file: what this bounds is the reading, whose
    /// footprint is one tick chunk plus one sample chunk plus the spill
    /// reader's own buffer — none of which depend on how many rows went past.
    /// Generous enough to absorb allocator behaviour, far too small to hide
    /// the 48 B/row (16 of ticks + 8 × 4 columns of samples) that reading the
    /// mappings whole used to cost: the extra 1.5 M rows below would have
    /// added ~72 MB of that.
    const EXTRA_ALLOWANCE_BYTES: u64 = 24 * 1024 * 1024;
    const SMALL_ROWS: usize = 100_000;
    const LARGE_ROWS: usize = SMALL_ROWS * 16;

    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");

    let small_path = dir.path().join("small.csv");
    let small_bytes = write_fixture(&small_path, SMALL_ROWS);
    let small = ingest::load_with_budget(&small_path, zero_budget(), cache.path())
        .expect("small fixture must open");
    assert!(
        small.is_spilled(),
        "a zero budget must force the spill path"
    );
    let small_pyramids = ingest::pyramids_for_dataset(&small);
    assert_every_column_pyramided(&small_pyramids, &small);
    let small_pyramid_bytes = pyramid_bytes(&small_pyramids);
    let after_small = peak_rss_bytes();
    drop(small_pyramids);
    drop(small);

    let large_path = dir.path().join("large.csv");
    let large_bytes = write_fixture(&large_path, LARGE_ROWS);
    let large = ingest::load_with_budget(&large_path, zero_budget(), cache.path())
        .expect("large fixture must open");
    assert!(large.is_spilled());
    assert_eq!(large.time.len(), LARGE_ROWS);
    let large_pyramids = ingest::pyramids_for_dataset(&large);
    assert_every_column_pyramided(&large_pyramids, &large);
    let large_pyramid_bytes = pyramid_bytes(&large_pyramids);
    let after_large = peak_rss_bytes();
    drop(large_pyramids);
    drop(large);

    let extra = after_large.saturating_sub(after_small);
    let buckets_grew_by = large_pyramid_bytes.saturating_sub(small_pyramid_bytes);
    let reading_overhead = extra.saturating_sub(buckets_grew_by);

    assert!(
        reading_overhead <= EXTRA_ALLOWANCE_BYTES,
        "pyramiding a {large_bytes}-byte spilled file after a {small_bytes}-byte one raised peak \
         RSS by {extra} bytes, of which only {buckets_grew_by} are the extra buckets themselves — \
         leaving {reading_overhead} bytes of reading overhead, over the \
         {EXTRA_ALLOWANCE_BYTES}-byte allowance. Building a pyramid over a spilled dataset must \
         read its columns in bounded chunks, never make them resident (issue #88, SPEC §5 \
         \"Peak RSS\")"
    );
}
