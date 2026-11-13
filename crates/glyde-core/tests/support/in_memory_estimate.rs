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

//! Shared by the issue #193 peak-RSS tests (one test binary per format,
//! since `VmHWM` is process-wide): reads the RAM estimate a reader takes its
//! SPEC §5.1 decision on, then measures what an in-memory open of the same
//! file really holds at its peak, the way `glyde-app`'s indexer runs it.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, OpenRequest, Registry};
use glyde_core::GlydeError;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// A `/proc/self/status` field, in bytes.
fn status_bytes(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let kb: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .unwrap_or_else(|| panic!("{field} not found in /proc/self/status"))
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .expect("parse /proc/self/status value");
    kb * 1024
}

/// The bytes the reader that claims `path` estimates an in-memory open costs:
/// asked with a zero budget and nowhere to spill, every reader refuses with
/// the estimate it compared against the cap.
fn estimated_bytes(path: &Path) -> u64 {
    let registry = Registry::default_registry();
    let reader = registry.find(path).unwrap_or(&ingest::CsvReader);
    let never = AtomicBool::new(false);
    let request = OpenRequest {
        overrides: Default::default(),
        budget: RamBudget::from_total_ram_bytes(0),
        cache_dir: None,
        cancelled: &never,
        on_checkpoint: None,
    };
    match reader.open_dataset(path, request) {
        Err(GlydeError::BudgetExceeded {
            requested_bytes, ..
        }) => requested_bytes,
        other => panic!(
            "a zero budget with no cache directory must be refused with the estimate, got {:?}",
            other.map(|(summary, _, _)| summary)
        ),
    }
}

/// Opens `path` in memory as the app does — the progressive open, then both
/// derived caches against an empty cache directory, all kept alive together —
/// and asserts the peak RSS it added stays within the reader's own estimate.
pub fn assert_estimate_covers_peak(path: &Path, rows: usize) {
    let estimate = estimated_bytes(path);
    let file_bytes = std::fs::metadata(path).expect("stat fixture").len();

    // Writing the fixture and the refused open above allocated too: restart
    // the high-water mark from what is resident now (Linux `clear_refs` 5).
    std::fs::write("/proc/self/clear_refs", "5").expect("reset VmHWM");
    let baseline = status_bytes("VmRSS:");

    let cache = tempfile::tempdir().expect("temp cache dir");
    // Total RAM large enough that the cap is SPEC §5's 4 GB ceiling: the
    // fixture must open in memory, which is the path being measured.
    let budget = RamBudget::from_total_ram_bytes(64 * 1024 * 1024 * 1024);
    let never = AtomicBool::new(false);
    let (_summary, _report, dataset) = ingest::open_dataset_progressive_in(
        path,
        Default::default(),
        budget,
        cache.path(),
        &never,
        drop,
    )
    .expect("fixture must open");
    assert!(!dataset.is_spilled(), "the fixture must open in memory");
    assert_eq!(dataset.time.len(), rows);
    let caches = ingest::derived_caches_for_dataset_cached_with_cache_dir(
        path,
        &dataset,
        cache.path(),
        Default::default(),
    );
    assert!(
        caches.0.iter().filter(|pyramid| pyramid.is_some()).count() >= 2,
        "the fixture's numeric columns must get pyramids, or this measures nothing"
    );

    let peak = status_bytes("VmHWM:").saturating_sub(baseline);
    drop((caches, dataset));
    eprintln!(
        "file {file_bytes} B, estimate {estimate} B, measured peak {peak} B ({:.2}x the estimate)",
        peak as f64 / estimate as f64
    );
    assert!(
        peak <= estimate,
        "an in-memory open of a {file_bytes}-byte file peaked {peak} bytes above its baseline, \
         over the {estimate} bytes its reader estimated: SPEC §5.1 would let a file in memory \
         that does not fit (issue #193)"
    );
}
