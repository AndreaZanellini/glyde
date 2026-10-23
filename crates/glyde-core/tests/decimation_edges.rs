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

//! Issue #145: the first full view of a spilled file took ~3 s, because
//! every pixel column's two exact edges read their raw samples *and* a
//! level-0 bucket, which lie in two different files, so in two different
//! disk pages, for every column of every channel. On a page cache that no
//! longer holds them, each page is one random read from disk.
//!
//! A level-0 bucket summarises [`PYRAMID_FACTOR`] raw samples, and the raw
//! samples next to it are on the page the edge reads anyway. So an edge
//! aggregates up to `PYRAMID_FACTOR²` raw samples itself and never reads
//! level 0: same buckets, one page fewer per edge. Locked here on an
//! instrumented pyramid, against a raw-scan reference.

use std::cell::Cell;

use glyde_core::dsp::decimation::{
    build_pyramid, decimate_viewport_levels, Bucket, BucketLevel, PYRAMID_FACTOR,
};

/// One pyramid level that counts how many buckets were read from it.
struct Counted<'a> {
    level: &'a [Bucket],
    reads: Cell<usize>,
}

impl BucketLevel for Counted<'_> {
    fn bucket_count(&self) -> usize {
        self.level.len()
    }

    fn bucket(&self, index: usize) -> Bucket {
        self.reads.set(self.reads.get() + 1);
        self.level[index]
    }
}

/// The textbook answer: each pixel column's bucket scanned from its raw
/// samples, the columns split as SPEC §3.1 splits them, one bucket per
/// sample when there are no more samples than columns.
fn reference(samples: &[f64], ticks: &[i128], range: (i128, i128), columns: usize) -> Vec<Bucket> {
    let lo = ticks.partition_point(|&tick| tick < range.0);
    let end = ticks.partition_point(|&tick| tick <= range.1);
    if lo >= end || columns == 0 {
        return Vec::new();
    }
    let total = end - lo;
    let scan = |first: usize, last: usize| {
        let finite = samples[first..=last]
            .iter()
            .copied()
            .filter(|v| !v.is_nan());
        Bucket {
            min: finite.clone().fold(f64::INFINITY, f64::min),
            max: finite.fold(f64::NEG_INFINITY, f64::max),
            first_ts: ticks[first],
            last_ts: ticks[last],
            first_value: samples[first],
            last_value: samples[last],
            nan_count: samples[first..=last].iter().filter(|v| v.is_nan()).count() as u64,
        }
    };
    if total <= columns {
        return (lo..end).map(|index| scan(index, index)).collect();
    }
    (0..columns)
        .map(|column| {
            let first = lo + column * total / columns;
            let last = lo + (column + 1) * total / columns - 1;
            scan(first, last)
        })
        .collect()
}

#[test]
fn a_decimation_never_reads_level_0_and_still_gives_the_exact_buckets() {
    let n = 1_000_003usize;
    let samples: Vec<f64> = (0..n)
        .map(|i| match i % 1_009 {
            0..=2 => f64::NAN,
            _ => ((i * 7_919) % 4_001) as f64 - 2_000.0 + (i as f64 * 0.01).sin(),
        })
        .collect();
    let ticks: Vec<i128> = (0..n as i128).map(|i| i * 3 + 1).collect();
    let pyramid = build_pyramid(&samples, &ticks);
    assert!(pyramid.len() >= 6, "every level is exercised");
    let levels: Vec<Counted> = pyramid
        .iter()
        .map(|level| Counted {
            level,
            reads: Cell::new(0),
        })
        .collect();
    let last = ticks[n - 1];
    // Wide and narrow columns, ranges on and off bucket edges, down to a
    // few samples per column.
    let views = [
        ((ticks[0], last), 2_880),
        ((ticks[0], last), 1),
        ((ticks[0], last), 7),
        ((5, last - 11), 1_920),
        ((ticks[12_345], ticks[812_345]), 641),
        ((ticks[100_001], ticks[100_001 + 9_000]), 1_440),
        ((ticks[3], ticks[3 + 2_000]), 999),
        ((ticks[777], ticks[777 + 500]), 400),
    ];
    for (range, columns) in views {
        for level in &levels {
            level.reads.set(0);
        }
        let decimated = decimate_viewport_levels(&levels, &samples, &ticks, range, columns);
        assert_eq!(
            decimated,
            reference(&samples, &ticks, range, columns),
            "view {range:?} at {columns} columns"
        );
        assert_eq!(
            levels[0].reads.get(),
            0,
            "view {range:?} at {columns} columns read level 0: its {PYRAMID_FACTOR} raw \
             samples per bucket are cheaper to read where the edge already is"
        );
        let samples_per_column = (range.1 - range.0) as usize / 3 / columns;
        if samples_per_column >= 2 * PYRAMID_FACTOR * PYRAMID_FACTOR {
            assert!(
                levels[1..].iter().any(|level| level.reads.get() > 0),
                "view {range:?} at {columns} columns must still aggregate from the pyramid"
            );
        }
    }
}
