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

//! Proves the issue #59 decision end to end: a memory-mapped
//! `index::level0::Level0Cache` — not a heap `Vec` — feeds `dsp::decimation`
//! and produces byte-identical results to the in-memory path, using the same
//! `build_pyramid`/`decimate_viewport` functions the golden tests lock.

use glyde_core::dsp::decimation::{build_pyramid, decimate_viewport};
use glyde_core::index::level0;

#[test]
fn a_memory_mapped_level0_cache_decimates_identically_to_an_in_memory_vec() {
    const SAMPLE_COUNT: usize = 5_000;
    const PIXEL_COLUMNS: usize = 137; // deliberately not a divisor of SAMPLE_COUNT

    let timestamps: Vec<i128> = (0..SAMPLE_COUNT as i128).collect();
    let samples: Vec<f64> = (0..SAMPLE_COUNT)
        .map(|i| ((i as f64) * 0.017).sin() * 1000.0)
        .collect();

    let source_dir = tempfile::tempdir().expect("temp source dir");
    let source_path = source_dir.path().join("source.csv");
    std::fs::write(
        &source_path,
        b"large-file stand-in; level0 never reads this file's bytes",
    )
    .expect("write stand-in source file");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");
    let key = level0::CacheKey::for_path(&source_path).expect("source metadata must be readable");

    let cache = level0::build_or_open(cache_dir.path(), &key, &samples, &timestamps)
        .expect("Level0Cache build must succeed");

    // The whole point of issue #59's decision: these are real borrowed
    // slices over memory-mapped bytes, not a re-materialized `Vec`, and nothing
    // downstream needs to know the difference.
    let mmap_samples: &[f64] = cache.samples();
    let mmap_timestamps: &[i128] = cache.timestamps();

    let range = (
        mmap_timestamps[0],
        mmap_timestamps[mmap_timestamps.len() - 1],
    );

    let pyramid_from_heap = build_pyramid(&samples, &timestamps);
    let columns_from_heap = decimate_viewport(
        &pyramid_from_heap,
        &samples,
        &timestamps,
        range,
        PIXEL_COLUMNS,
    );

    let pyramid_from_mmap = build_pyramid(mmap_samples, mmap_timestamps);
    let columns_from_mmap = decimate_viewport(
        &pyramid_from_mmap,
        mmap_samples,
        mmap_timestamps,
        range,
        PIXEL_COLUMNS,
    );

    assert_eq!(
        columns_from_mmap, columns_from_heap,
        "decimating a memory-mapped Level0Cache must produce exactly the same buckets \
         as decimating the equivalent in-memory Vec — same engine, same result"
    );
}

#[test]
fn reopening_a_cached_file_serves_the_cache_without_touching_the_provided_slices() {
    let source_dir = tempfile::tempdir().expect("temp source dir");
    let source_path = source_dir.path().join("source.csv");
    std::fs::write(&source_path, b"stand-in source").expect("write stand-in source file");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");
    let key = level0::CacheKey::for_path(&source_path).expect("source metadata must be readable");

    let timestamps: Vec<i128> = vec![0, 1, 2, 3];
    let samples: Vec<f64> = vec![1.0, 2.0, 3.0, 4.0];
    level0::build_or_open(cache_dir.path(), &key, &samples, &timestamps)
        .expect("first build must succeed");

    // Reopen with different slices: if this rebuilt instead of hitting the
    // cache, the assertions below would see the wrong data.
    let different_timestamps: Vec<i128> = vec![100, 200];
    let different_samples: Vec<f64> = vec![-1.0, -2.0];
    let reopened = level0::build_or_open(
        cache_dir.path(),
        &key,
        &different_samples,
        &different_timestamps,
    )
    .expect("reopen must succeed");

    assert_eq!(reopened.samples(), samples.as_slice());
    assert_eq!(reopened.timestamps(), timestamps.as_slice());
}
