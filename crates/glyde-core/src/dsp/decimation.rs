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

//! The multi-resolution min/max pyramid and the viewport decimation query
//! that renders it (docs/SPEC.md §3.1, docs/ARCHITECTURE.md §The index).
//!
//! This is the in-memory engine: both functions take plain `&[f64]`/`&[i128]`
//! slices, exactly as locked by the golden tests in
//! `crates/glyde-core/tests/golden/decimation.rs` (docs/QUALITY.md §2
//! Decimation). That is a deliberate API choice (issue #59): rather than
//! adding an abstraction layer for large files, the large-file path
//! (`crate::index::level0`) memory-maps a typed on-disk cache and hands this
//! module a real `&[f64]`/`&[i128]` slice over the mapped bytes, so the exact
//! same engine — and the exact same golden tests — cover both the small-file
//! and large-file cases. Never widen a golden test's tolerance or change its
//! expectations to make an implementation pass — if one looks wrong, that is
//! a `blocking-decision` issue, not an edit.

/// One pyramid bucket: `(min, max, first_ts, last_ts, nan_count)` over the
/// raw samples/time-range it aggregates (ARCH §The index).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub min: f64,
    pub max: f64,
    pub first_ts: i128,
    pub last_ts: i128,
    pub nan_count: u64,
}

/// Bucket-size growth factor between consecutive pyramid levels (ARCH §The
/// index: "factor 8 recommended, documented in code").
pub const PYRAMID_FACTOR: usize = 8;

/// Builds the full min/max pyramid over `samples`/`timestamps` (level 0 is
/// the raw data itself and is not materialized here). `pyramid[0]` groups
/// every [`PYRAMID_FACTOR`] raw samples into one bucket; `pyramid[k + 1]`'s
/// buckets are the exact min-of-mins/max-of-maxes aggregation of
/// [`PYRAMID_FACTOR`] consecutive buckets from `pyramid[k]` — never a
/// re-aggregation of resampled or averaged data (SPEC §3.1).
///
/// `samples[i]` is the value at `timestamps[i]`; both slices must be the
/// same length and `timestamps` must be non-decreasing.
pub fn build_pyramid(samples: &[f64], timestamps: &[i128]) -> Vec<Vec<Bucket>> {
    debug_assert_eq!(
        samples.len(),
        timestamps.len(),
        "samples and timestamps must be the same length"
    );

    if samples.is_empty() {
        return Vec::new();
    }

    let mut levels = vec![bucket_level_from_samples(samples, timestamps)];

    while levels.last().expect("levels is never empty").len() >= PYRAMID_FACTOR {
        let next = bucket_level_from_buckets(levels.last().expect("levels is never empty"));
        levels.push(next);
    }

    levels
}

/// Groups raw samples into level-0 buckets of [`PYRAMID_FACTOR`] samples each
/// (the final group may be smaller — only ever the *last* group, since
/// [`slice::chunks`] processes elements strictly in order).
fn bucket_level_from_samples(samples: &[f64], timestamps: &[i128]) -> Vec<Bucket> {
    samples
        .chunks(PYRAMID_FACTOR)
        .zip(timestamps.chunks(PYRAMID_FACTOR))
        .map(|(sample_chunk, ts_chunk)| raw_range_bucket(sample_chunk, ts_chunk))
        .collect()
}

/// Aggregates one pyramid level into the next, exactly — min-of-mins,
/// max-of-maxes over groups of [`PYRAMID_FACTOR`] children (SPEC §3.1: "never
/// from resampled or averaged data").
fn bucket_level_from_buckets(children: &[Bucket]) -> Vec<Bucket> {
    children
        .chunks(PYRAMID_FACTOR)
        .map(|group| {
            let mut merged = group[0];
            for bucket in &group[1..] {
                merged = merge_buckets(merged, *bucket);
            }
            merged
        })
        .collect()
}

/// The exact min/max/nan_count of `samples`/`timestamps` (same length,
/// non-empty), treating NaN samples as excluded from the min/max envelope but
/// counted separately (docs/SPEC.md §4.1 anomaly gutter reads `nan_count`;
/// the envelope itself must stay a real number whenever any non-NaN sample is
/// present).
fn raw_range_bucket(samples: &[f64], timestamps: &[i128]) -> Bucket {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut nan_count = 0u64;
    for &value in samples {
        if value.is_nan() {
            nan_count += 1;
        } else {
            min = min.min(value);
            max = max.max(value);
        }
    }
    Bucket {
        min,
        max,
        first_ts: timestamps[0],
        last_ts: *timestamps.last().expect("non-empty chunk"),
        nan_count,
    }
}

/// Merges two buckets that cover adjacent ranges, `left` before `right` in
/// time/index order.
fn merge_buckets(left: Bucket, right: Bucket) -> Bucket {
    Bucket {
        min: left.min.min(right.min),
        max: left.max.max(right.max),
        first_ts: left.first_ts,
        last_ts: right.last_ts,
        nan_count: left.nan_count + right.nan_count,
    }
}

/// Resolves the viewport `range` (inclusive, in the same tick units as
/// `timestamps`) into one [`Bucket`] per pixel column, using `pyramid` where
/// available.
///
/// When `range` contains fewer raw samples than `pixel_columns`, returns one
/// bucket per raw sample instead of aggregating (`min == max == samples[i]`)
/// so the caller can draw individual point markers — the convergence
/// guarantee in SPEC §3.1: zooming in far enough always reaches the true
/// samples.
pub fn decimate_viewport(
    pyramid: &[Vec<Bucket>],
    samples: &[f64],
    timestamps: &[i128],
    range: (i128, i128),
    pixel_columns: usize,
) -> Vec<Bucket> {
    debug_assert_eq!(
        samples.len(),
        timestamps.len(),
        "samples and timestamps must be the same length"
    );

    if pixel_columns == 0 || samples.is_empty() {
        return Vec::new();
    }

    // timestamps is non-decreasing (function precondition), so the samples
    // whose timestamp falls in `range` (inclusive) form one contiguous index
    // run, found in O(log n) instead of a linear scan.
    let (range_lo, range_hi) = range;
    let lo = timestamps.partition_point(|&ts| ts < range_lo);
    let hi_exclusive = timestamps.partition_point(|&ts| ts <= range_hi);
    if lo >= hi_exclusive {
        return Vec::new();
    }
    let hi = hi_exclusive - 1;
    let total_in_range = hi - lo + 1;

    if total_in_range <= pixel_columns {
        // SPEC §3.1 convergence guarantee: fewer raw samples than pixel
        // columns means draw every sample as its own point, never aggregate
        // them away.
        return (lo..=hi)
            .map(|i| Bucket {
                min: samples[i],
                max: samples[i],
                first_ts: timestamps[i],
                last_ts: timestamps[i],
                nan_count: u64::from(samples[i].is_nan()),
            })
            .collect();
    }

    (0..pixel_columns)
        .map(|column| {
            // Standard "split N items into K contiguous, near-equal groups"
            // boundary: consecutive differences are always floor(N/K) or
            // floor(N/K)+1, and since total_in_range > pixel_columns here,
            // every column gets at least one raw sample.
            let column_lo = lo + (column * total_in_range) / pixel_columns;
            let column_hi = lo + ((column + 1) * total_in_range) / pixel_columns - 1;
            aggregate_range(pyramid, samples, timestamps, column_lo, column_hi)
        })
        .collect()
}

/// Exact min/max/first_ts/last_ts/nan_count over raw sample indices
/// `lo..=hi`, using precomputed pyramid buckets wherever a bucket's raw range
/// falls entirely inside `[lo, hi]` and recursing to finer levels — down to
/// individual raw samples — only for the (at most `PYRAMID_FACTOR` per level)
/// boundary buckets that straddle the edge of the query. This is the
/// "exact edges, aggregated interior" requirement the spike-preservation
/// golden test locks in: O(PYRAMID_FACTOR · log(n)) per column, not O(n).
fn aggregate_range(
    pyramid: &[Vec<Bucket>],
    samples: &[f64],
    timestamps: &[i128],
    lo: usize,
    hi: usize,
) -> Bucket {
    aggregate_at_level(
        pyramid,
        samples,
        timestamps,
        pyramid.len() as isize - 1,
        lo,
        hi,
    )
}

/// `level == -1` means "raw samples" (the recursion's base case); `level ==
/// k` (0-indexed into `pyramid`) means bucket span `PYRAMID_FACTOR.pow(k+1)`.
///
/// Only the *last* bucket at any level can cover fewer than `span` raw
/// samples ([`slice::chunks`] only ever leaves a short group at the end), so
/// every bucket's start index is exactly `index * span`; only its end needs
/// clamping to the real sample count.
fn aggregate_at_level(
    pyramid: &[Vec<Bucket>],
    samples: &[f64],
    timestamps: &[i128],
    level: isize,
    lo: usize,
    hi: usize,
) -> Bucket {
    let Some(level) = usize::try_from(level).ok() else {
        return raw_range_bucket(&samples[lo..=hi], &timestamps[lo..=hi]);
    };

    let span = PYRAMID_FACTOR.pow((level + 1) as u32);
    let buckets = &pyramid[level];
    let bucket_lo = lo / span;
    let bucket_hi = (hi / span).min(buckets.len() - 1);

    let mut merged: Option<Bucket> = None;
    for (offset, &bucket) in buckets[bucket_lo..=bucket_hi].iter().enumerate() {
        let bucket_index = bucket_lo + offset;
        let bucket_start = bucket_index * span;
        let bucket_end = (bucket_start + span - 1).min(samples.len() - 1);

        let piece = if bucket_start >= lo && bucket_end <= hi {
            bucket
        } else {
            let covered_lo = bucket_start.max(lo);
            let covered_hi = bucket_end.min(hi);
            aggregate_at_level(
                pyramid,
                samples,
                timestamps,
                level as isize - 1,
                covered_lo,
                covered_hi,
            )
        };

        merged = Some(match merged {
            Some(existing) => merge_buckets(existing, piece),
            None => piece,
        });
    }

    merged.expect("bucket_lo..=bucket_hi is always non-empty")
}
