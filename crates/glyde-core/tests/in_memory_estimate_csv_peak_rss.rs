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

//! Issue #193, delimited text: the RAM estimate the in-memory/spill decision
//! is taken on (SPEC §5.1) must cover what an in-memory open really holds at
//! its peak — the open itself **and** the derived caches the app builds right
//! after it (min/max pyramids, Level-0 caches), on a mixed-dtype file.
//!
//! Before the fix, the estimate sized only the parse and the typed columns,
//! and a 155 MB CSV estimated at 850 MB peaked at 1.98 GB in the app.
//!
//! Its own test binary for the same reason as `tests/peak_rss.rs`: `VmHWM` is
//! process-wide. Linux-only for the same reason too.

#![cfg(target_os = "linux")]

#[path = "support/in_memory_estimate.rs"]
mod in_memory_estimate;

use in_memory_estimate::assert_estimate_covers_peak;
use std::fmt::Write as _;
use std::io::Write as _;

const ROWS: usize = 400_000;

/// Timestamp, six float columns, an integer counter, a `true`/`false` column
/// and a six-label state column: the shape of a real test-bench log (issue
/// #193's measurements were on one), not the numeric-only shape
/// `memory_gate` measures.
fn write_fixture(path: &std::path::Path) {
    const STATES: [&str; 6] = ["OFF", "CRANKING", "IDLE", "RUN", "FUEL_CUT", "FAULT"];
    let mut file = std::fs::File::create(path).expect("create fixture");
    let mut buffer = String::with_capacity(1 << 16);
    buffer.push_str("timestamp,speed,throttle,torque,coolant,oil,lambda,cycles,fan,state\n");
    for i in 0..ROWS {
        writeln!(
            &mut buffer,
            "2026-03-12 09:{:02}:{:02}.{:03},{}.{},{}.{},{}.{},{}.{},{}.{:03},{}.{:03},{},{},{}",
            (i / 60_000) % 60,
            (i / 1000) % 60,
            i % 1000,
            800 + i % 3000,
            i % 10,
            i % 100,
            i % 7,
            i % 400,
            i % 3,
            80 + i % 20,
            i % 9,
            i % 5,
            i % 997,
            i % 3,
            i % 991,
            i / 50,
            if i % 1000 < 500 { "true" } else { "false" },
            STATES[(i / 20_000) % STATES.len()],
        )
        .expect("format row");
        if buffer.len() > 1 << 16 {
            file.write_all(buffer.as_bytes()).expect("write fixture");
            buffer.clear();
        }
    }
    file.write_all(buffer.as_bytes()).expect("write fixture");
    file.flush().expect("flush fixture");
}

#[test]
fn a_mixed_csv_estimate_covers_the_peak_of_its_open_and_derived_caches() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("mixed.csv");
    write_fixture(&path);
    assert_estimate_covers_peak(&path, ROWS);
}
