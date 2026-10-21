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

//! Pyramid levels from Parquet row-group statistics (docs/ARCHITECTURE.md
//! §why-Parquet, docs/ROADMAP.md M7, issue #162).
//!
//! A Parquet footer carries each row group's min and max per column. A
//! pyramid level can be taken from them, instead of aggregated from the
//! level below, **only where the result is provably the bucket a scan would
//! produce** (maintainer decision on #162, "exact only"):
//!
//! - **Alignment.** A level's buckets are fixed groups of `8^(k+1)` rows
//!   counted from row 0 ([`crate::dsp::decimation`]). A level comes from the
//!   statistics only when each of its buckets is a union of whole row
//!   groups: every row group but the last has the same row count `R`, the
//!   last has at most `R`, and the bucket size is a multiple of `R`. The
//!   lowest such level and every level above it come from the statistics;
//!   the levels below are scanned.
//! - **`nan_count`** comes from the series' own NaN runs, already flagged at
//!   ingestion, never from the footer's null count (which `parquet` reports
//!   as 0 when a writer omitted it).
//! - **`first_ts`/`last_ts`** are the time axis's ticks at the bucket's edge
//!   rows, read directly: exact, and no scan.
//! - A row group with no min/max is accepted only when the NaN runs show
//!   every one of its rows is missing (an all-null group, whose bucket has
//!   the empty envelope a scan gives it). Otherwise the whole column is
//!   scanned.
//!
//! Which columns carry statistics at all (integers only: Parquet's float
//! statistics leave NaN out without counting it) is the reader's decision,
//! see `ingest::parquet`.

use std::ops::Range;

use crate::dsp::decimation::{
    bucket_level_from_buckets, bucket_level_from_samples, Bucket, PYRAMID_FACTOR,
};

/// One row group's exact min and max for one column, as the `f64` the
/// pyramid holds. `None` when the footer has none (an all-null group, or a
/// writer that wrote no statistics).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowGroupBounds {
    pub rows: usize,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// One column's row-group statistics, in row order across the whole source
/// (every file of a partitioned directory, in turn).
#[derive(Debug, Clone, PartialEq)]
pub struct RowGroupStats {
    groups: Vec<RowGroupBounds>,
}

impl RowGroupStats {
    pub fn new(groups: Vec<RowGroupBounds>) -> Self {
        Self { groups }
    }

    pub fn groups(&self) -> &[RowGroupBounds] {
        &self.groups
    }

    fn row_count(&self) -> usize {
        self.groups.iter().map(|group| group.rows).sum()
    }

    /// The common row count of every row group but the last, if they share
    /// one and the last is no longer: the condition for any bucket to be a
    /// union of whole groups.
    fn uniform_group_rows(&self) -> Option<usize> {
        let (last, rest) = self.groups.split_last()?;
        let rows = rest.first().map_or(last.rows, |first| first.rows);
        let uniform = rows > 0 && rest.iter().all(|group| group.rows == rows) && last.rows <= rows;
        uniform.then_some(rows)
    }
}

/// Why a column's pyramid is scanned in full rather than partly built from
/// its row-group statistics, in words for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanReason {
    /// The statistics cover a different number of rows than the series.
    RowCountMismatch,
    /// The row groups are not all the same size, so no bucket boundary is
    /// guaranteed to fall on a row-group boundary.
    UnevenRowGroups,
    /// No level's bucket size is a multiple of the row-group size, or the
    /// series is too short to have that level.
    NoAlignedLevel,
    /// A row group with present values has no min/max in the footer.
    MissingStatistics,
}

impl std::fmt::Display for ScanReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ScanReason::RowCountMismatch => "the statistics cover a different number of rows",
            ScanReason::UnevenRowGroups => "its row groups are not all the same size",
            ScanReason::NoAlignedLevel => {
                "no pyramid level lines up with its row groups (or the file is too short for it)"
            }
            ScanReason::MissingStatistics => "a row group with values has no min/max statistics",
        })
    }
}

/// The number of buckets at level `k` over `n` samples, as
/// [`crate::dsp::decimation::build_pyramid`] builds them.
fn bucket_count(n: usize, level: usize) -> usize {
    n.div_ceil(bucket_rows(level))
}

/// Rows per bucket at pyramid level `k` (`pyramid[0]` groups
/// [`PYRAMID_FACTOR`] samples).
fn bucket_rows(level: usize) -> usize {
    PYRAMID_FACTOR.pow(level as u32 + 1)
}

/// Whether `build_pyramid` over `n` samples has a level `k`: level 0 exists
/// for any non-empty series, and each next one while the level below has at
/// least [`PYRAMID_FACTOR`] buckets.
fn level_exists(n: usize, level: usize) -> bool {
    n > 0 && (level == 0 || bucket_count(n, level - 1) >= PYRAMID_FACTOR)
}

/// Samples in `range` that fall in a NaN run (`runs` sorted and disjoint).
fn nan_count_in(runs: &[Range<usize>], range: Range<usize>) -> u64 {
    let first = runs.partition_point(|run| run.end <= range.start);
    runs[first..]
        .iter()
        .take_while(|run| run.start < range.end)
        .map(|run| (run.end.min(range.end) - run.start.max(range.start)) as u64)
        .sum()
}

/// The lowest pyramid level `stats` can build exactly over a series of
/// `sample_count` samples with these NaN runs, or why none can.
pub fn first_stats_level(
    stats: &RowGroupStats,
    sample_count: usize,
    nan_runs: &[Range<usize>],
) -> Result<usize, ScanReason> {
    if stats.row_count() != sample_count {
        return Err(ScanReason::RowCountMismatch);
    }
    let group_rows = stats
        .uniform_group_rows()
        .ok_or(ScanReason::UnevenRowGroups)?;
    let level = (0..)
        .take_while(|&level| level_exists(sample_count, level))
        .find(|&level| bucket_rows(level).is_multiple_of(group_rows))
        .ok_or(ScanReason::NoAlignedLevel)?;
    let mut start = 0;
    for group in stats.groups() {
        let missing = group.min.is_none() || group.max.is_none();
        if missing && nan_count_in(nan_runs, start..start + group.rows) != group.rows as u64 {
            return Err(ScanReason::MissingStatistics);
        }
        start += group.rows;
    }
    Ok(level)
}

/// Level `level` built from `stats` alone: each bucket the union of its
/// whole row groups (see the module docs). The caller has checked the
/// alignment with [`first_stats_level`].
fn stats_level(
    stats: &RowGroupStats,
    level: usize,
    ticks: &[i128],
    nan_runs: &[Range<usize>],
) -> Vec<Bucket> {
    let rows = bucket_rows(level);
    let group_rows = stats
        .uniform_group_rows()
        .expect("checked by first_stats_level");
    let groups_per_bucket = rows / group_rows;
    stats
        .groups()
        .chunks(groups_per_bucket)
        .enumerate()
        .map(|(index, groups)| {
            let start = index * rows;
            let end = start + groups.iter().map(|group| group.rows).sum::<usize>();
            let bounds = || groups.iter().filter_map(|group| group.min.zip(group.max));
            Bucket {
                min: bounds().map(|(min, _)| min).fold(f64::INFINITY, f64::min),
                max: bounds()
                    .map(|(_, max)| max)
                    .fold(f64::NEG_INFINITY, f64::max),
                first_ts: ticks[start],
                last_ts: ticks[end - 1],
                nan_count: nan_count_in(nan_runs, start..end),
            }
        })
        .collect()
}

/// The pyramid [`crate::dsp::decimation::build_pyramid`] builds over
/// `samples`/`ticks`, bucket for bucket and level for level, with every level
/// from [`first_stats_level`] up taken from `stats` instead of aggregated.
/// Returns that level too (`None`: the whole pyramid was scanned, with the
/// reason).
pub fn build_pyramid_with_row_groups(
    samples: &[f64],
    ticks: &[i128],
    nan_runs: &[Range<usize>],
    stats: &RowGroupStats,
) -> (Vec<Vec<Bucket>>, Result<usize, ScanReason>) {
    let first = first_stats_level(stats, samples.len(), nan_runs);
    if samples.is_empty() {
        return (Vec::new(), first);
    }
    // The first aligned level comes from the statistics; the ones above it
    // aggregate it, so they never touch a sample either.
    let stats_from = first.ok();
    let level = |level: usize, below: Option<&Vec<Bucket>>| match below {
        _ if stats_from == Some(level) => stats_level(stats, level, ticks, nan_runs),
        Some(below) => bucket_level_from_buckets(below),
        None => bucket_level_from_samples(samples, ticks),
    };
    let mut levels = vec![level(0, None)];
    while levels.last().expect("never empty").len() >= PYRAMID_FACTOR {
        let next = level(levels.len(), levels.last());
        levels.push(next);
    }
    (levels, first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::decimation::build_pyramid;

    /// `rows` samples of a deterministic, non-monotonic signal, with NaN at
    /// every index in `nan_at`.
    fn signal(rows: usize, nan_at: &[Range<usize>]) -> (Vec<f64>, Vec<i128>) {
        let samples = (0..rows)
            .map(|i| {
                if nan_at.iter().any(|run| run.contains(&i)) {
                    f64::NAN
                } else {
                    ((i * 7919) % 1013) as f64 - 500.0
                }
            })
            .collect();
        let ticks = (0..rows as i128).map(|i| 1_000 + i * 10).collect();
        (samples, ticks)
    }

    /// What a footer would say for `samples` split into `group_rows`-row
    /// groups: each group's min/max over its non-NaN values.
    fn true_stats(samples: &[f64], group_rows: usize) -> RowGroupStats {
        RowGroupStats::new(
            samples
                .chunks(group_rows)
                .map(|group| {
                    let present: Vec<f64> = group.iter().copied().filter(|v| !v.is_nan()).collect();
                    RowGroupBounds {
                        rows: group.len(),
                        min: present.iter().copied().reduce(f64::min),
                        max: present.iter().copied().reduce(f64::max),
                    }
                })
                .collect(),
        )
    }

    fn assert_same_pyramid(left: &[Vec<Bucket>], right: &[Vec<Bucket>]) {
        assert_eq!(left.len(), right.len(), "level count");
        for (level, (l, r)) in left.iter().zip(right).enumerate() {
            assert_eq!(l.len(), r.len(), "bucket count at level {level}");
            for (index, (a, b)) in l.iter().zip(r).enumerate() {
                assert!(
                    a.min == b.min
                        && a.max == b.max
                        && a.first_ts == b.first_ts
                        && a.last_ts == b.last_ts
                        && a.nan_count == b.nan_count,
                    "level {level} bucket {index}: {a:?} != {b:?}"
                );
            }
        }
    }

    #[test]
    fn aligned_row_groups_build_the_same_pyramid_as_a_scan() {
        // 64-row groups line up with level 1 (64-row buckets); a short last
        // group, and NaN runs inside and across groups.
        let nan_runs = [3..5, 60..70, 200..201];
        for rows in [64 * 40 + 17, 64 * 512, 64 * 9] {
            let (samples, ticks) = signal(rows, &nan_runs);
            let runs = crate::series::detect_nan_runs(&samples);
            let stats = true_stats(&samples, 64);

            let (pyramid, first) = build_pyramid_with_row_groups(&samples, &ticks, &runs, &stats);

            assert_eq!(first, Ok(1), "{rows} rows");
            assert_same_pyramid(&pyramid, &build_pyramid(&samples, &ticks));
        }
    }

    #[test]
    fn the_first_aligned_level_is_the_lowest_whose_bucket_is_a_multiple_of_the_group() {
        let level = |group_rows: usize| {
            let (samples, _) = signal(1 << 16, &[]);
            first_stats_level(&true_stats(&samples, group_rows), samples.len(), &[])
        };
        assert_eq!(level(8), Ok(0));
        assert_eq!(level(4), Ok(0));
        assert_eq!(level(64), Ok(1));
        assert_eq!(level(128), Ok(2), "two groups per 512-row bucket");
        assert_eq!(level(4096), Ok(3));
        // pyarrow's default, 2^20 rows, aligns at level 6 (8^7 = 2^21): far
        // beyond a 2^16-row file.
        assert_eq!(level(1 << 20), Err(ScanReason::NoAlignedLevel));
        // DuckDB's 122,880 rows (15 · 2^13) never divides a power of 8.
        assert_eq!(level(100), Err(ScanReason::NoAlignedLevel));
    }

    /// The "no scan" half of the contract: with planted statistics that
    /// disagree with the samples, every level from the first aligned one up
    /// shows the statistics, and every level below shows the samples. (Real
    /// statistics agree, see the test above; this proves which one was read.)
    #[test]
    fn levels_from_the_first_aligned_one_up_are_read_from_the_statistics_not_the_samples() {
        let (samples, ticks) = signal(64 * 64, &[]);
        let mut groups = true_stats(&samples, 64).groups().to_vec();
        groups[0].max = Some(1e9);
        let stats = RowGroupStats::new(groups);

        let (pyramid, first) = build_pyramid_with_row_groups(&samples, &ticks, &[], &stats);

        assert_eq!(first, Ok(1));
        assert!(
            pyramid[0][..8].iter().all(|bucket| bucket.max < 1e9),
            "scanned"
        );
        for level in &pyramid[1..] {
            assert_eq!(level[0].max, 1e9, "from the statistics");
        }
    }

    #[test]
    fn uneven_or_mismatched_row_groups_fall_back_to_the_full_scan() {
        let (samples, ticks) = signal(64 * 20, &[]);
        let mut groups = true_stats(&samples, 64).groups().to_vec();
        groups[3].rows -= 1;
        groups[4].rows += 1;
        let uneven = RowGroupStats::new(groups);
        let (pyramid, first) = build_pyramid_with_row_groups(&samples, &ticks, &[], &uneven);
        assert_eq!(first, Err(ScanReason::UnevenRowGroups));
        assert_same_pyramid(&pyramid, &build_pyramid(&samples, &ticks));

        let fewer = true_stats(&samples[..64 * 19], 64);
        let (_, first) = build_pyramid_with_row_groups(&samples, &ticks, &[], &fewer);
        assert_eq!(first, Err(ScanReason::RowCountMismatch));
    }

    #[test]
    fn a_group_without_statistics_is_accepted_only_when_all_its_rows_are_missing() {
        let all_null = 64..128;
        let (samples, ticks) = signal(64 * 16, std::slice::from_ref(&all_null));
        let runs = crate::series::detect_nan_runs(&samples);
        let stats = true_stats(&samples, 64);
        assert_eq!(stats.groups()[1].min, None, "the all-null group");

        let (pyramid, first) = build_pyramid_with_row_groups(&samples, &ticks, &runs, &stats);
        assert_eq!(first, Ok(1));
        assert_same_pyramid(&pyramid, &build_pyramid(&samples, &ticks));

        // A group with values but no statistics (a writer that skipped them).
        let mut groups = true_stats(&samples, 64).groups().to_vec();
        groups[5].min = None;
        let (pyramid, first) =
            build_pyramid_with_row_groups(&samples, &ticks, &runs, &RowGroupStats::new(groups));
        assert_eq!(first, Err(ScanReason::MissingStatistics));
        assert_same_pyramid(&pyramid, &build_pyramid(&samples, &ticks));
    }

    #[test]
    fn nan_counts_are_counted_from_the_runs_over_any_range() {
        let runs = [2..5, 10..20];
        assert_eq!(nan_count_in(&runs, 0..2), 0);
        assert_eq!(nan_count_in(&runs, 0..100), 13);
        assert_eq!(nan_count_in(&runs, 4..12), 3);
        assert_eq!(nan_count_in(&runs, 20..30), 0);
    }
}
