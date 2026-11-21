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

//! What every target does with a file once it has one: open it the way the
//! app does, then derive everything the app derives from it. An `Err` is a
//! correct answer to a malformed file; only a panic, hang or out-of-bounds
//! access is a finding.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, IngestOverrides};

/// A budget that holds the file in memory, or one so small it spills.
pub fn budget(in_memory: bool) -> RamBudget {
    RamBudget::from_total_ram_bytes(if in_memory { u64::MAX } else { 0 })
}

pub fn open_and_derive(path: &Path, budget: RamBudget, cache_dir: &Path) {
    let Ok((_summary, _report, dataset)) = ingest::open_dataset_progressive_in(
        path,
        IngestOverrides::default(),
        budget,
        cache_dir,
        &AtomicBool::new(false),
        |_| {},
    ) else {
        return;
    };
    if dataset.is_spilled() {
        let _ = ingest::disk_pyramids_for_dataset(&dataset, cache_dir);
        return;
    }
    let (pyramids, level0) = ingest::derived_caches_for_dataset_cached_with_cache_dir(
        path,
        &dataset,
        cache_dir,
        IngestOverrides::default(),
    );
    // Touch what came back, so a cache that maps garbage is read through.
    let buckets: usize = pyramids.iter().flatten().flatten().map(Vec::len).sum();
    let samples: f64 = level0
        .iter()
        .flatten()
        .map(|cache| cache.samples().iter().sum::<f64>() + cache.timestamps().len() as f64)
        .sum();
    std::hint::black_box((buckets, samples));
}
