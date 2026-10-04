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

//! The PSD memory cap is never exceeded (docs/SPEC.md §5.1; `dsp::psd`
//! module docs). A counting global allocator measures every byte the planner
//! and the computation actually allocate, and the peak is compared with the
//! plan's own estimate — which the plan has already checked against the cap.
//!
//! Each test measures the allocations of this process as a whole, so the
//! tests here hold one lock and never run concurrently.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use glyde_core::budget::RamBudget;
use glyde_core::dsp::psd::{
    compute_psds, plan_psd, PsdMemory, PsdMemoryCap, PsdPlan, PsdSettings, SegmentLength,
    PSD_MEMORY_CAP_BYTES,
};
use glyde_core::ingest::{load_with_budget, Dataset};
use glyde_core::series::ViewKind;

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            if new_size >= layout.size() {
                let now = CURRENT.fetch_add(new_size - layout.size(), Ordering::Relaxed)
                    + (new_size - layout.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static SERIAL: Mutex<()> = Mutex::new(());

/// Plans and computes the PSD of every numeric column of `dataset` under
/// `cap`, returning the plan and the most memory held at once while doing
/// so, beyond what was already allocated before.
fn measured_psd(dataset: &Dataset, settings: PsdSettings, cap: PsdMemoryCap) -> (PsdPlan, usize) {
    let columns: Vec<_> = dataset
        .columns
        .iter()
        .filter(|series| series.view_kind() == ViewKind::TimeDomain)
        .filter_map(|series| series.values().sample_source())
        .collect();
    // Start the rayon pool outside the measured window: its threads and
    // queues are the application's, not this computation's.
    rayon::join(|| (), || ());

    let baseline = CURRENT.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);

    let plan = plan_psd(
        &dataset.time,
        0..dataset.time.len(),
        &settings,
        columns.len(),
        cap,
    )
    .expect("scan")
    .expect("the fixture has a PSD");
    let spectra = compute_psds(&dataset.time, &columns, &plan, &|_| true)
        .expect("read")
        .expect("never cancelled");
    assert_eq!(spectra.len(), columns.len());
    let peak = PEAK.load(Ordering::SeqCst) - baseline;
    drop(spectra);
    (plan, peak)
}

/// A CSV of `rows` rows at 1 kHz with `columns` numeric columns (integers,
/// so reading them takes the dtype-conversion path), and — when
/// `gap_every` is `Some(n)` — a one-hour gap every `n` rows.
fn csv(rows: usize, columns: usize, gap_every: Option<usize>) -> tempfile::NamedTempFile {
    use std::io::Write;
    let file = tempfile::NamedTempFile::new().expect("temp file");
    let mut out = std::io::BufWriter::new(file.reopen().expect("reopen"));
    write!(out, "t").unwrap();
    for c in 0..columns {
        write!(out, ",c{c}").unwrap();
    }
    writeln!(out).unwrap();
    let mut t_ms: u64 = 1_700_000_000_000;
    for row in 0..rows {
        if gap_every.is_some_and(|n| row > 0 && row % n == 0) {
            t_ms += 3_600_000;
        }
        write!(out, "{t_ms}").unwrap();
        for c in 0..columns {
            write!(out, ",{}", (row * (c + 3)) % 1000).unwrap();
        }
        writeln!(out).unwrap();
        t_ms += 1;
    }
    out.flush().unwrap();
    drop(out);
    file
}

fn roomy() -> PsdMemoryCap {
    PsdMemoryCap::from_bytes(PSD_MEMORY_CAP_BYTES)
}

fn assert_within(plan: &PsdPlan, peak: usize, cap: PsdMemoryCap) {
    assert!(
        plan.memory.peak_bytes <= cap.bytes(),
        "the plan itself is over the cap: {} > {}",
        plan.memory.peak_bytes,
        cap.bytes()
    );
    assert!(
        peak as u64 <= plan.memory.peak_bytes,
        "the PSD held {peak} bytes at its peak, more than the {} its plan estimated \
         ({} columns, {} at a time, {}-sample window)",
        plan.memory.peak_bytes,
        plan.columns,
        plan.memory.concurrent_columns,
        plan.config.segment_len
    );
}

#[test]
fn an_in_memory_psd_never_holds_more_than_its_plan_estimated() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let file = csv(1_000_000, 6, None);
    let dataset = glyde_core::ingest::load(file.path()).expect("load");
    assert!(!dataset.is_spilled());

    let (plan, peak) = measured_psd(&dataset, PsdSettings::default(), roomy());

    assert_eq!(plan.config.segment_len, 65536);
    assert_within(&plan, peak, roomy());
}

#[test]
fn a_cap_that_allows_one_column_at_a_time_is_never_exceeded() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let file = csv(500_000, 6, None);
    let dataset = glyde_core::ingest::load(file.path()).expect("load");
    let settings = PsdSettings {
        segment_length: SegmentLength::Fixed(65536),
        ..PsdSettings::default()
    };
    // Room for every result but only one column in flight.
    let cap = PsdMemoryCap::from_bytes(PsdMemory::estimate(65536, 6, 1, roomy()).peak_bytes);

    let (plan, peak) = measured_psd(&dataset, settings, cap);

    assert_eq!(plan.memory.concurrent_columns, 1);
    assert_within(&plan, peak, cap);
}

#[test]
fn a_spilled_file_with_gaps_never_holds_more_than_its_plan_estimated() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let file = csv(600_000, 4, Some(150_000));
    let cache = tempfile::tempdir().expect("cache dir");
    // A budget far too small to hold the file forces the spill path.
    let dataset = load_with_budget(
        file.path(),
        RamBudget::from_total_ram_bytes(8 * 1024 * 1024),
        cache.path(),
    )
    .expect("load");
    assert!(dataset.is_spilled());

    let (plan, peak) = measured_psd(&dataset, PsdSettings::default(), roomy());

    assert!(plan.is_segmented());
    assert_eq!(plan.segment_count, 4);
    assert_within(&plan, peak, roomy());
}
