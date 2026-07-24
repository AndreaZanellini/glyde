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

//! Coarse peak-RSS regression gate for CSV ingestion (issue #58), ahead of
//! docs/ROADMAP.md M3's real `glyde-devtools::memory_gate` harness (which
//! needs the RAM-budget module and a 20 GB fixture, neither of which exist
//! yet).
//!
//! Issue #58 measured `ingest::inspect()` + `ingest::load()` peaking at
//! **~11x** a 124 MB source file's size. This PR fixes two of its three
//! named root causes (`decode()` no longer copies a clean UTF-8 file, and
//! the app's indexer no longer parses the file twice), but peak RSS for a
//! single `load()` is, measured here, still **~12x** the file size: most of
//! it is `ingest::csv::Capture::All` capturing every field as its own owned
//! `String` and `dataset::load`'s typed-`Series` build on top of that — the
//! issue's still-open third root cause, filed as its own follow-up (#62).
//! Never disguise that as fixed: this test's budget is set with headroom
//! *above* the current ~12x baseline, so it fails only on a regression that
//! makes things measurably worse (e.g. `decode()` losing its
//! `Cow::Borrowed` fast path again), not as proof of a met SPEC §5 budget.
//!
//! This is its own file (a separate test binary, one test) so the RSS
//! measurement isn't polluted by unrelated tests sharing the process.
//! Linux-only: `/proc/self/status`'s `VmHWM` (peak resident set size) has no
//! portable equivalent, and this crate has no `sysinfo`-based peak-RSS
//! reader yet (that lands with docs/ROADMAP.md M3's RAM-budget module).

#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::io::Write as _;

const ROWS: usize = 400_000;

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

/// A clean, comma-delimited, dot-decimal CSV with `ROWS` data rows and three
/// numeric columns — the common-case shape (SPEC §1.1's default), so the
/// zero-copy UTF-8 decode path (issue #58) is what's actually exercised.
fn write_synthetic_csv(path: &std::path::Path) -> u64 {
    let mut file = std::fs::File::create(path).expect("create synthetic fixture");
    let mut buffer = String::with_capacity(64 * 1024);
    buffer.push_str("timestamp,value,pressure\n");
    for i in 0..ROWS {
        writeln!(
            &mut buffer,
            "{},{}.{},{}.{}",
            i,
            i % 1000,
            i % 10,
            1000 + i % 500,
            i % 10
        )
        .expect("format row");
        if buffer.len() > 32 * 1024 {
            file.write_all(buffer.as_bytes())
                .expect("write fixture chunk");
            buffer.clear();
        }
    }
    file.write_all(buffer.as_bytes())
        .expect("write final fixture chunk");
    file.flush().expect("flush fixture");
    file.metadata().expect("stat fixture").len()
}

// Measured on this fixture shape at ROWS=400_000 (~7.8 MB) and again at
// 2_000_000 rows (~40 MB): both land around 12x, so this is a real
// proportional cost, not a small file's fixed-overhead artifact. MAX_RATIO
// is set with real headroom above that measured baseline: it does not
// claim SPEC §5 is met (it isn't — see the module docs), only that a
// regression which pushes the ratio measurably higher is caught.
#[test]
fn loading_a_synthetic_csv_keeps_peak_rss_within_a_generous_multiple_of_file_size() {
    const MAX_RATIO: u64 = 16;
    const FIXED_OVERHEAD_BYTES: u64 = 8 * 1024 * 1024; // test harness baseline

    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("synthetic.csv");
    let file_size = write_synthetic_csv(&path);

    let dataset = glyde_core::ingest::load(&path).expect("synthetic fixture must load");
    assert_eq!(dataset.time.len(), ROWS);

    let peak = peak_rss_bytes();
    let budget = file_size * MAX_RATIO + FIXED_OVERHEAD_BYTES;
    assert!(
        peak <= budget,
        "peak RSS {peak} bytes exceeded {budget} bytes ({MAX_RATIO}x a {file_size}-byte file \
         plus {FIXED_OVERHEAD_BYTES} bytes overhead) — see issue #58"
    );
}
