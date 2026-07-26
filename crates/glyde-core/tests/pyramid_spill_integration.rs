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

//! Proves the docs/ROADMAP.md M3 "Pyramid level spill" item end to end: a
//! pyramid reopened from `index::pyramid`'s cache is bucket-for-bucket
//! identical to a freshly built one, and `dsp::decimation::decimate_viewport`
//! — the golden-tested query engine — produces exactly the same viewport
//! output from either, the same guarantee
//! `level0_decimation_integration.rs` already locks for Level 0.

use glyde_core::dsp::decimation::{build_pyramid, decimate_viewport};
use glyde_core::index::{level0, pyramid};

fn sine_wave(sample_count: usize) -> (Vec<f64>, Vec<i128>) {
    let timestamps: Vec<i128> = (0..sample_count as i128).collect();
    let samples: Vec<f64> = (0..sample_count)
        .map(|i| ((i as f64) * 0.011).cos() * 500.0)
        .collect();
    (samples, timestamps)
}

#[test]
fn a_reopened_pyramid_decimates_identically_to_a_freshly_built_one() {
    const SAMPLE_COUNT: usize = 5_000;
    const PIXEL_COLUMNS: usize = 211; // deliberately not a divisor of SAMPLE_COUNT

    let (samples, timestamps) = sine_wave(SAMPLE_COUNT);

    let source_dir = tempfile::tempdir().expect("temp source dir");
    let source_path = source_dir.path().join("source.csv");
    std::fs::write(
        &source_path,
        b"large-file stand-in; index::pyramid never reads this file's bytes",
    )
    .expect("write stand-in source file");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");
    let key = level0::CacheKey::for_path(&source_path).expect("source metadata must be readable");

    let fresh_pyramid = build_pyramid(&samples, &timestamps);
    let cached_pyramid = pyramid::build_or_open(cache_dir.path(), &key, &samples, &timestamps)
        .expect("pyramid cache build must succeed");
    assert_eq!(
        cached_pyramid, fresh_pyramid,
        "a freshly cached pyramid must equal an in-memory build_pyramid over the same data"
    );

    // The whole point of this roadmap item: reopening must serve the cache,
    // not recompute — prove it by reopening and comparing decimate_viewport
    // output computed from the reopened pyramid against the freshly built one.
    let reopened_pyramid = pyramid::try_open(cache_dir.path(), &key)
        .expect("try_open must not error")
        .expect("cache must be present after build_or_open");

    let range = (timestamps[0], timestamps[timestamps.len() - 1]);
    let columns_from_fresh =
        decimate_viewport(&fresh_pyramid, &samples, &timestamps, range, PIXEL_COLUMNS);
    let columns_from_reopened = decimate_viewport(
        &reopened_pyramid,
        &samples,
        &timestamps,
        range,
        PIXEL_COLUMNS,
    );

    assert_eq!(
        columns_from_reopened, columns_from_fresh,
        "decimating a reopened cached pyramid must produce exactly the same buckets \
         as decimating a freshly built one — same engine, same result"
    );
}

#[test]
fn reopening_a_cached_pyramid_serves_the_cache_without_recomputing_from_different_samples() {
    let source_dir = tempfile::tempdir().expect("temp source dir");
    let source_path = source_dir.path().join("source.csv");
    std::fs::write(&source_path, b"stand-in source").expect("write stand-in source file");
    let cache_dir = tempfile::tempdir().expect("temp cache dir");
    let key = level0::CacheKey::for_path(&source_path).expect("source metadata must be readable");

    let (samples, timestamps) = sine_wave(300);
    let built = pyramid::build_or_open(cache_dir.path(), &key, &samples, &timestamps)
        .expect("first build must succeed");

    // Reopen with different slices: if this recomputed instead of hitting the
    // cache, the assertion below would see a pyramid built from these instead.
    let different_timestamps: Vec<i128> = vec![100, 200, 300, 400];
    let different_samples: Vec<f64> = vec![-1.0, -2.0, -3.0, -4.0];
    let reopened = pyramid::build_or_open(
        cache_dir.path(),
        &key,
        &different_samples,
        &different_timestamps,
    )
    .expect("reopen must succeed");

    assert_eq!(reopened, built);
}
