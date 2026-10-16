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

//! Issue #102: the min/max pyramid of a file too large for memory lives on
//! disk, one memory-mapped spill file per level, instead of as an owned
//! `Vec<Vec<Bucket>>` costing ~9 bytes of RAM per sample per column. It must
//! be the *same* pyramid, bucket for bucket, as the golden-tested
//! [`build_pyramid`] (SPEC §3.1, Golden Rule 4), and decimating through it
//! must give exactly what [`decimate_viewport`] gives over the in-memory
//! one — the golden tests lock that function, and the disk-backed path must
//! not be a second, unlocked algorithm.

use glyde_core::dsp::decimation::{
    build_pyramid, decimate_viewport, decimate_viewport_levels, Bucket, BucketLevel,
};
use glyde_core::index::disk_pyramid::build_disk_pyramid;
use glyde_core::index::spill::SpillSet;

/// A deterministic signal with NaN runs, flat stretches and spikes, over
/// non-decreasing ticks that repeat now and then.
fn fixture(n: usize) -> (Vec<f64>, Vec<i128>) {
    let samples = (0..n)
        .map(|i| match i % 97 {
            0..=2 => f64::NAN,
            50 => 1e6,
            60..=70 => 3.0,
            _ => ((i as f64) * 0.37).sin() * (1.0 + (i % 13) as f64),
        })
        .collect();
    let ticks = (0..n).map(|i| (i - i / 50) as i128 * 10).collect();
    (samples, ticks)
}

/// Field-by-field equality, NaN-aware (a converged bucket's min/max is the
/// raw sample, which can be NaN).
fn same(a: &Bucket, b: &Bucket) -> bool {
    a.min.to_bits() == b.min.to_bits()
        && a.max.to_bits() == b.max.to_bits()
        && a.first_ts == b.first_ts
        && a.last_ts == b.last_ts
        && a.nan_count == b.nan_count
}

#[test]
fn the_disk_pyramid_is_the_in_memory_pyramid_bucket_for_bucket() {
    let dir = tempfile::tempdir().expect("temp dir");
    let set = SpillSet::create(dir.path()).expect("spill set");
    for n in [1, 7, 8, 9, 63, 64, 65, 513, 4_097, 100_003] {
        let (samples, ticks) = fixture(n);
        let memory = build_pyramid(&samples, &ticks);
        let disk = build_disk_pyramid(&set, &samples[..], &ticks[..]).expect("build on disk");
        let levels = disk.levels();
        assert_eq!(levels.len(), memory.len(), "level count for n={n}");
        for (level, (on_disk, in_memory)) in levels.iter().zip(&memory).enumerate() {
            assert_eq!(
                on_disk.bucket_count(),
                in_memory.len(),
                "n={n} level {level}"
            );
            for (index, bucket) in in_memory.iter().enumerate() {
                assert!(
                    same(&on_disk.bucket(index), bucket),
                    "n={n} level {level} bucket {index}: {:?} != {bucket:?}",
                    on_disk.bucket(index)
                );
            }
        }
    }
}

#[test]
fn decimating_through_the_disk_pyramid_matches_decimate_viewport() {
    let dir = tempfile::tempdir().expect("temp dir");
    let set = SpillSet::create(dir.path()).expect("spill set");
    let (samples, ticks) = fixture(250_007);
    let memory = build_pyramid(&samples, &ticks);
    let disk = build_disk_pyramid(&set, &samples[..], &ticks[..]).expect("build on disk");
    let last = *ticks.last().expect("non-empty");

    for range in [
        (0, last),
        (last / 3, last / 3 + 5_000),
        (17, 23),
        (last / 2, last / 2),
        (-100, 40),
        (last - 1_000, last + 1_000),
    ] {
        for pixel_columns in [1, 7, 640, 1_920, 100_000] {
            let expected = decimate_viewport(&memory, &samples, &ticks, range, pixel_columns);
            let actual =
                decimate_viewport_levels(disk.levels(), &samples, &ticks, range, pixel_columns);
            assert_eq!(actual.len(), expected.len(), "{range:?} @ {pixel_columns}");
            for (a, b) in actual.iter().zip(&expected) {
                assert!(same(a, b), "{range:?} @ {pixel_columns}: {a:?} != {b:?}");
            }
        }
    }
}

#[test]
fn an_empty_column_has_an_empty_disk_pyramid() {
    let dir = tempfile::tempdir().expect("temp dir");
    let set = SpillSet::create(dir.path()).expect("spill set");
    let disk = build_disk_pyramid(&set, &[][..], &[][..]).expect("build on disk");
    assert!(disk.levels().is_empty());
}

// The app's path for a file too large for memory: a spilled dataset gets an
// on-disk pyramid per numeric column, equal to the in-memory pyramid the same
// file gets when it fits, and none for a non-numeric column.
#[test]
fn a_spilled_dataset_gets_the_same_pyramids_on_disk() {
    use glyde_core::budget::RamBudget;
    use glyde_core::ingest;

    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("signals.csv");
    let mut text = String::from("timestamp,a,b,label\n");
    for i in 0..20_000 {
        text.push_str(&format!(
            "{i},{},{},{}\n",
            ((i as f64) * 0.01).sin(),
            if i % 333 == 0 {
                "NaN".to_string()
            } else {
                (i % 17).to_string()
            },
            ["x", "y"][i % 2]
        ));
    }
    std::fs::write(&path, text).expect("write fixture");

    let in_memory = ingest::load_with_budget(
        &path,
        RamBudget::from_total_ram_bytes(u64::MAX),
        cache.path(),
    )
    .expect("open in memory");
    let spilled = ingest::load_with_budget(&path, RamBudget::from_total_ram_bytes(0), cache.path())
        .expect("open spilled");
    assert!(spilled.is_spilled());

    let expected = ingest::pyramids_for_dataset(&in_memory);
    let on_disk = ingest::disk_pyramids_for_dataset(&spilled, cache.path());
    assert_eq!(on_disk.len(), expected.len());
    for (column, (disk, memory)) in on_disk.iter().zip(&expected).enumerate() {
        match (disk, memory) {
            (None, None) => {}
            (Some(disk), Some(memory)) => {
                assert_eq!(disk.levels().len(), memory.len(), "column {column}");
                for (on_disk, in_memory) in disk.levels().iter().zip(memory) {
                    assert_eq!(on_disk.bucket_count(), in_memory.len());
                    for (index, bucket) in in_memory.iter().enumerate() {
                        assert!(same(&on_disk.bucket(index), bucket), "column {column}");
                    }
                }
            }
            other => panic!("column {column}: {other:?}"),
        }
    }
}
