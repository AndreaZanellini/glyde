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
//!
//! [`build_pyramid_streaming`] (issue #88) is the one addition to that shape,
//! and it does not change it: the slice-taking signatures above are untouched
//! and stay the golden-tested engine, while the streaming entry point feeds
//! the *same* bucket accumulator from a [`SampleSource`]/[`TickSource`] pair
//! read in bounded chunks. It exists because handing `build_pyramid` one
//! contiguous slice over a spilled, memory-mapped column makes every page of
//! that column resident — the residency issue #85 removed from the time axis
//! — and a golden test locks the two paths to bucket-for-bucket equality at
//! every chunk size, so there is still exactly one aggregation definition
//! (docs/ARCHITECTURE.md Hard rule 4).

use crate::series::SampleSource;
use crate::time::TickSource;
use crate::Result;

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

    grow_levels(bucket_level_from_samples(samples, timestamps))
}

/// Builds the same pyramid [`build_pyramid`] builds — bucket for bucket, level
/// for level — but reading `samples` and `timestamps` in bounded chunks
/// instead of taking one contiguous slice over each (issue #88).
///
/// This is the entry point for a *spilled* column (issue #75): its samples and
/// its ticks are memory-mapped files, and handing `build_pyramid` a slice over
/// them makes every page resident as the aggregation walks it end to end — 8
/// bytes per sample plus 16 per tick, proportional to file size, which cannot
/// fit SPEC §5's flat `min(25% RAM, 4 GB)` cap. Reading through
/// [`SampleSource`]/[`TickSource`] leaves those pages in the OS page cache
/// instead, so the only memory this call adds beyond the pyramid it returns is
/// two fixed-size chunk buffers.
///
/// The pyramid itself is still `O(rows)` — about 9 bytes per sample across all
/// levels — so this bounds the *input*, not the output (issue #102).
///
/// Both sources must hold the same number of elements and `timestamps` must be
/// non-decreasing, exactly as [`build_pyramid`] requires. Fails only if a
/// source's own read fails (a spilled column's file being unreadable); an
/// in-memory source cannot fail.
pub fn build_pyramid_streaming<S, T>(samples: &S, timestamps: &T) -> Result<Vec<Vec<Bucket>>>
where
    S: SampleSource + ?Sized,
    T: TickSource + ?Sized,
{
    let sample_count = samples.sample_count();
    debug_assert_eq!(
        sample_count,
        timestamps.tick_count(),
        "samples and timestamps must be the same length"
    );

    if sample_count == 0 {
        return Ok(Vec::new());
    }

    // Ticks drive the outer loop and samples the inner one, so each tick chunk
    // is paired with exactly the sample range it covers — the two sources are
    // free to chunk differently (and do: a spilled column and an in-memory one
    // have different natural chunk sizes), and neither ever has to be
    // materialized whole to line them up.
    let mut level0 = Level0Builder::for_sample_count(sample_count);
    let mut position = 0usize;
    timestamps.visit_tick_chunks(0..sample_count, &mut |tick_chunk| {
        let start = position;
        position += tick_chunk.len();
        let mut offset = 0usize;
        samples.visit_sample_chunks(start..position, &mut |sample_chunk| {
            let end = offset + sample_chunk.len();
            level0.push_chunk(sample_chunk, &tick_chunk[offset..end]);
            offset = end;
            Ok(())
        })?;
        debug_assert_eq!(
            offset,
            tick_chunk.len(),
            "the sample source must yield exactly as many samples as there are ticks"
        );
        Ok(())
    })?;

    Ok(grow_levels(level0.finish()))
}

/// Aggregates a finished level 0 into the levels above it, [`PYRAMID_FACTOR`]
/// children at a time, until the top level is too short to group further.
/// Shared by [`build_pyramid`] and [`build_pyramid_streaming`] so the two can
/// only ever differ in how level 0 was *read*, never in how it is aggregated.
fn grow_levels(level0: Vec<Bucket>) -> Vec<Vec<Bucket>> {
    if level0.is_empty() {
        return Vec::new();
    }

    let mut levels = vec![level0];
    while levels.last().expect("levels is never empty").len() >= PYRAMID_FACTOR {
        let next = bucket_level_from_buckets(levels.last().expect("levels is never empty"));
        levels.push(next);
    }
    levels
}

/// Groups raw samples into level-0 buckets of [`PYRAMID_FACTOR`] samples each,
/// fed either all at once ([`build_pyramid`]) or a chunk at a time
/// ([`build_pyramid_streaming`]).
///
/// A bucket's contents depend only on its position — buckets are the fixed
/// [`PYRAMID_FACTOR`]-sized groups counted from the start of the column, the
/// same grouping [`slice::chunks`] produces — so carrying a partially filled
/// bucket across a chunk boundary is what makes the streamed result identical
/// to the whole-slice one. Only the very last bucket of the column can be
/// short.
struct Level0Builder {
    buckets: Vec<Bucket>,
    /// The bucket currently being filled, and how many samples are in it
    /// (always `1..PYRAMID_FACTOR` while `Some`: a bucket is pushed the moment
    /// it fills).
    pending: Option<(Bucket, usize)>,
}

impl Level0Builder {
    fn for_sample_count(sample_count: usize) -> Self {
        Self {
            // Exact, not a growth guess: level 0 always has this many buckets,
            // so the accumulator never reallocates (and never spikes to 1.5x
            // its own size doing so) as it fills.
            buckets: Vec::with_capacity(sample_count.div_ceil(PYRAMID_FACTOR)),
            pending: None,
        }
    }

    /// Adds one contiguous run of samples, in row order, continuing from
    /// whatever the previous run left half-finished.
    fn push_chunk(&mut self, samples: &[f64], timestamps: &[i128]) {
        debug_assert_eq!(
            samples.len(),
            timestamps.len(),
            "samples and timestamps must be the same length"
        );

        let mut offset = 0;
        while offset < samples.len() {
            let filled = self.pending.map_or(0, |(_, filled)| filled);
            let take = (PYRAMID_FACTOR - filled).min(samples.len() - offset);
            let end = offset + take;
            let piece = raw_range_bucket(&samples[offset..end], &timestamps[offset..end]);

            self.pending = Some(match self.pending.take() {
                Some((partial, filled)) => (merge_buckets(partial, piece), filled + take),
                None => (piece, take),
            });
            offset = end;

            if self
                .pending
                .is_some_and(|(_, filled)| filled == PYRAMID_FACTOR)
            {
                let (bucket, _) = self.pending.take().expect("checked as Some above");
                self.buckets.push(bucket);
            }
        }
    }

    /// The finished level 0, including a final short bucket if the column's
    /// length is not a multiple of [`PYRAMID_FACTOR`].
    fn finish(mut self) -> Vec<Bucket> {
        if let Some((bucket, _)) = self.pending.take() {
            self.buckets.push(bucket);
        }
        self.buckets
    }
}

/// [`Level0Builder`] over one whole slice — the in-memory case.
fn bucket_level_from_samples(samples: &[f64], timestamps: &[i128]) -> Vec<Bucket> {
    let mut builder = Level0Builder::for_sample_count(samples.len());
    builder.push_chunk(samples, timestamps);
    builder.finish()
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

/// Extends a pyramid previously built over `samples[..previous_len]` (by
/// [`build_pyramid`] or by this function itself) to cover all of `samples`,
/// bucket-for-bucket identical to calling `build_pyramid(samples,
/// timestamps)` from scratch — but touching only the buckets that new
/// samples can actually change, never re-aggregating the ones that cannot
/// (docs/ROADMAP.md M3's progressive checkpoint schedule calls this once per
/// checkpoint as a file loads; re-running `build_pyramid` on every larger
/// prefix is `O(n log n)` of pure waste across a full load — issue #90).
///
/// `previous` must be exactly what `build_pyramid(&samples[..previous_len],
/// &timestamps[..previous_len])` would return (or the empty pyramid, when
/// `previous_len` is `0`, in which case this is equivalent to calling
/// [`build_pyramid`] directly). Golden rule: level *k*'s buckets are
/// grouped from the start of the array in fixed [`PYRAMID_FACTOR`]-sized
/// chunks (`slice::chunks`), so a bucket's contents depend only on its
/// position, never on how the call was split up — every bucket fully
/// contained in `samples[..previous_len_rounded_down_to_a_bucket_boundary]`
/// is therefore already final and is kept as-is; only the last, possibly
/// partial bucket at each level (and everything built on top of it) is
/// recomputed, from the raw samples up.
pub fn extend_pyramid(
    mut previous: Vec<Vec<Bucket>>,
    previous_len: usize,
    samples: &[f64],
    timestamps: &[i128],
) -> Vec<Vec<Bucket>> {
    debug_assert_eq!(
        samples.len(),
        timestamps.len(),
        "samples and timestamps must be the same length"
    );
    debug_assert!(
        previous_len <= samples.len(),
        "previous_len must be a prefix length of the current samples"
    );

    if samples.is_empty() {
        return Vec::new();
    }
    if previous_len == 0 || previous.is_empty() {
        return build_pyramid(samples, timestamps);
    }

    // Level 0: every bucket fully inside the previous checkpoint's complete
    // groups is final; only the raw-sample tail from the last complete
    // group onward needs (re)bucketing.
    let mut complete = previous_len / PYRAMID_FACTOR;
    previous[0].truncate(complete);
    previous[0].extend(bucket_level_from_samples(
        &samples[complete * PYRAMID_FACTOR..],
        &timestamps[complete * PYRAMID_FACTOR..],
    ));

    // Existing higher levels: the same "keep the final prefix, recompute
    // the tail from the level below" step, one level at a time — the tail
    // of children is small (only what changed) so this never re-copies a
    // level's already-final prefix.
    let mut level = 1;
    while level < previous.len() {
        let level_complete = complete / PYRAMID_FACTOR;
        let tail_children = previous[level - 1][level_complete * PYRAMID_FACTOR..].to_vec();
        previous[level].truncate(level_complete);
        previous[level].extend(bucket_level_from_buckets(&tail_children));
        complete = level_complete;
        level += 1;
    }

    // The pyramid may need to grow taller now that there is more data,
    // exactly mirroring `build_pyramid`'s own growth condition.
    while previous.last().expect("previous is non-empty").len() >= PYRAMID_FACTOR {
        let next = bucket_level_from_buckets(previous.last().expect("checked above"));
        previous.push(next);
    }

    previous
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
