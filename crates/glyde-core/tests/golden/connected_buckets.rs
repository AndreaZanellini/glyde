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

//! Connected-buckets golden tests (docs/QUALITY.md §2 Decimation, "Boundary
//! exactness" and "Continuity"; SPEC §3.1; issue #147). Written before
//! `Bucket::first_value`/`last_value` and `dsp::decimation::connectors`
//! existed.
//!
//! SPEC §3.1: every column also records the value of its first and last raw
//! sample, and two consecutive columns are joined by a connector from the
//! first one's last sample to the next one's first sample, exactly the line
//! the convergence regime draws between those two samples. No connector
//! touches a NaN sample. Never loosen an assertion here to make an
//! implementation pass: if one looks wrong, that is a `blocking-decision`
//! issue, not an edit.

use glyde_core::dsp::decimation::{
    build_pyramid, build_pyramid_streaming, connectors, decimate_viewport, extend_pyramid, Bucket,
    Connector,
};
use glyde_core::series::SampleSource;
use glyde_core::time::TickSource;
use glyde_core::Result;
use std::ops::Range;

/// A tiny deterministic PRNG (xorshift64*), the same one
/// `golden/decimation.rs` uses, so "random data" is reproducible.
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    /// Uniform in `[-1.0, 1.0)`.
    fn next_f64(&mut self) -> f64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        ((x >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

/// Samples in the boundary fixture: not a multiple of any bucket span, so
/// every pyramid level ends in a short bucket.
const BOUNDARY_SAMPLES: usize = 5_003;

/// Random samples with NaN at the first and last sample of buckets at every
/// pyramid level (spans 8, 64, 512 and 4096), at the very first and very
/// last sample, across two whole level-0 buckets, and scattered inside
/// buckets. The timestamps are strictly increasing and unevenly spaced, so
/// each timestamp names exactly one raw sample.
fn boundary_fixture() -> (Vec<f64>, Vec<i128>) {
    let mut rng = Xorshift64::new(0x147);
    let mut samples: Vec<f64> = (0..BOUNDARY_SAMPLES)
        .map(|_| rng.next_f64() * 1000.0)
        .collect();
    let edges = [0, 7, 8, 63, 64, 511, 512, 4095, 4096, 4999, 5000];
    let whole_buckets = 80..96;
    let scattered = (0..BOUNDARY_SAMPLES).filter(|i| i % 37 == 5);
    for i in edges
        .into_iter()
        .chain(whole_buckets)
        .chain(scattered)
        .chain([BOUNDARY_SAMPLES - 1])
    {
        samples[i] = f64::NAN;
    }
    let timestamps = (0..BOUNDARY_SAMPLES as i128)
        .map(|i| 1_000 + i * 3 + i / 7)
        .collect();
    (samples, timestamps)
}

/// The index of the one raw sample at `timestamp`.
fn index_at(timestamps: &[i128], timestamp: i128) -> usize {
    timestamps
        .binary_search(&timestamp)
        .expect("a bucket's edge is a real sample's timestamp")
}

/// Fails unless `bucket`'s first/last value is, bit for bit, the raw sample
/// at its first/last timestamp (a NaN included).
fn assert_edges_are_raw_samples(bucket: &Bucket, samples: &[f64], timestamps: &[i128], what: &str) {
    let first = samples[index_at(timestamps, bucket.first_ts)];
    let last = samples[index_at(timestamps, bucket.last_ts)];
    assert_eq!(
        bucket.first_value.to_bits(),
        first.to_bits(),
        "{what}: first_value {} is not the raw sample {first} at its first timestamp {}",
        bucket.first_value,
        bucket.first_ts
    );
    assert_eq!(
        bucket.last_value.to_bits(),
        last.to_bits(),
        "{what}: last_value {} is not the raw sample {last} at its last timestamp {}",
        bucket.last_value,
        bucket.last_ts
    );
}

/// Every field of a bucket, bit for bit, so NaN compares equal to itself.
fn bits(bucket: &Bucket) -> (u64, u64, i128, i128, u64, u64, u64) {
    (
        bucket.min.to_bits(),
        bucket.max.to_bits(),
        bucket.first_ts,
        bucket.last_ts,
        bucket.first_value.to_bits(),
        bucket.last_value.to_bits(),
        bucket.nan_count,
    )
}

fn assert_same_pyramid(actual: &[Vec<Bucket>], expected: &[Vec<Bucket>], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: level count");
    for (level, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(actual.len(), expected.len(), "{what}: level {level} length");
        for (index, (a, e)) in actual.iter().zip(expected).enumerate() {
            assert_eq!(
                bits(a),
                bits(e),
                "{what}: level {level} bucket {index}: {a:?} != {e:?}"
            );
        }
    }
}

#[test]
fn boundary_exactness_first_and_last_values_are_the_edge_samples_at_every_level() {
    let (samples, timestamps) = boundary_fixture();
    let pyramid = build_pyramid(&samples, &timestamps);
    assert_eq!(pyramid.len(), 4, "spans 8, 64, 512 and 4096");

    for (level, buckets) in pyramid.iter().enumerate() {
        for (index, bucket) in buckets.iter().enumerate() {
            assert_edges_are_raw_samples(
                bucket,
                &samples,
                &timestamps,
                &format!("level {level} bucket {index}"),
            );
        }
        // The fixture must exercise NaN edges on every level, both ends.
        assert!(
            buckets.iter().any(|b| b.first_value.is_nan()),
            "level {level}: some bucket must start on a NaN"
        );
        assert!(
            buckets.iter().any(|b| b.last_value.is_nan()),
            "level {level}: some bucket must end on a NaN"
        );
    }
}

/// A [`SampleSource`]/[`TickSource`] that hands its data over in awkward
/// chunk sizes, so a bucket's edges can fall on any side of a chunk boundary.
struct Chunked<T> {
    values: Vec<T>,
    chunk_len: usize,
}

impl<T: Copy> Chunked<T> {
    fn visit(&self, range: Range<usize>, visit: &mut dyn FnMut(&[T]) -> Result<()>) -> Result<()> {
        let end = range.end.min(self.values.len());
        let start = range.start.min(end);
        for chunk in self.values[start..end].chunks(self.chunk_len.max(1)) {
            visit(chunk)?;
        }
        Ok(())
    }
}

impl SampleSource for Chunked<f64> {
    fn sample_count(&self) -> usize {
        self.values.len()
    }

    fn visit_sample_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[f64]) -> Result<()>,
    ) -> Result<()> {
        self.visit(range, visit)
    }
}

impl TickSource for Chunked<i128> {
    fn tick_count(&self) -> usize {
        self.values.len()
    }

    fn visit_tick_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[i128]) -> Result<()>,
    ) -> Result<()> {
        self.visit(range, visit)
    }
}

/// The streaming builder and the progressive extension are locked to
/// `build_pyramid` elsewhere by `==`; this repeats it bit for bit on a
/// fixture whose edges are NaN, so a first/last value can never depend on
/// how the column was read or how its load was split into checkpoints.
#[test]
fn boundary_values_are_identical_through_every_pyramid_builder() {
    let (samples, timestamps) = boundary_fixture();
    let expected = build_pyramid(&samples, &timestamps);

    for chunk_len in [1usize, 3, 7, 8, 9, 13, 1_000] {
        let streamed = build_pyramid_streaming(
            &Chunked {
                values: samples.clone(),
                chunk_len,
            },
            &Chunked {
                values: timestamps.clone(),
                chunk_len: chunk_len + 2,
            },
        )
        .expect("an in-memory chunked source never fails");
        assert_same_pyramid(
            &streamed,
            &expected,
            &format!("streamed {chunk_len} at a time"),
        );
    }

    let mut extended: Vec<Vec<Bucket>> = Vec::new();
    let mut previous_len = 0;
    for checkpoint in [1, 7, 8, 9, 63, 65, 512, 513, 4_095, 4_097, BOUNDARY_SAMPLES] {
        extended = extend_pyramid(
            extended,
            previous_len,
            &samples[..checkpoint],
            &timestamps[..checkpoint],
        );
        assert_same_pyramid(
            &extended,
            &build_pyramid(&samples[..checkpoint], &timestamps[..checkpoint]),
            &format!("extended to {checkpoint} samples from {previous_len}"),
        );
        previous_len = checkpoint;
    }
    assert_same_pyramid(&extended, &expected, "extended to the whole column");
}

#[test]
fn boundary_exactness_holds_in_every_decimate_viewport_result() {
    let (samples, timestamps) = boundary_fixture();
    let pyramid = build_pyramid(&samples, &timestamps);
    let ts = |i: usize| timestamps[i];
    let ranges = [
        (ts(0), ts(BOUNDARY_SAMPLES - 1)),
        (ts(512), ts(4_096)),
        (ts(100) + 1, ts(2_000) - 1),
        (ts(7), ts(64)),
        (ts(4_999), ts(5_000)),
        (ts(42), ts(42)),
    ];

    // With the pyramid, and without one (the view's fallback for a column
    // that has none): the same exactness either way.
    let no_pyramid: &[Vec<Bucket>] = &[];
    for levels in [&pyramid[..], no_pyramid] {
        for range in ranges {
            for pixel_columns in [1, 2, 7, 64, 333, 1_000, 4_999, BOUNDARY_SAMPLES, 20_000] {
                let columns =
                    decimate_viewport(levels, &samples, &timestamps, range, pixel_columns);
                assert!(!columns.is_empty(), "{range:?} holds samples");
                for (index, column) in columns.iter().enumerate() {
                    assert_edges_are_raw_samples(
                        column,
                        &samples,
                        &timestamps,
                        &format!(
                            "{} levels, {range:?} at {pixel_columns} columns, column {index}",
                            levels.len()
                        ),
                    );
                }
            }
        }
    }
}

/// Samples and columns of the continuity fixture: about 2.4 samples per
/// column, the zoom at which `measures_v2.csv`'s `torque` looked dashed
/// (issue #147).
const STEEP_SAMPLES: usize = 2_800;
const STEEP_COLUMNS: usize = 1_150;

/// A signal that moves faster than its range within one column: consecutive
/// samples are up to ~0.9 rad of a 100-amplitude sine apart.
fn steep_fixture() -> (Vec<f64>, Vec<i128>) {
    let samples = (0..STEEP_SAMPLES)
        .map(|i| (i as f64 * 0.9).sin() * 100.0)
        .collect();
    let timestamps = (0..STEEP_SAMPLES as i128).map(|i| i * 10).collect();
    (samples, timestamps)
}

fn decimate_steep(samples: &[f64], timestamps: &[i128]) -> Vec<Bucket> {
    let pyramid = build_pyramid(samples, timestamps);
    let range = (timestamps[0], timestamps[timestamps.len() - 1]);
    let columns = decimate_viewport(&pyramid, samples, timestamps, range, STEEP_COLUMNS);
    assert_eq!(
        columns.len(),
        STEEP_COLUMNS,
        "more samples than columns: the aggregated regime"
    );
    columns
}

/// The connector the convergence regime draws from raw sample `from` to raw
/// sample `to`.
fn line(samples: &[f64], timestamps: &[i128], from: usize, to: usize) -> Connector {
    Connector {
        from: (timestamps[from], samples[from]),
        to: (timestamps[to], samples[to]),
    }
}

#[test]
fn continuity_joins_every_pair_of_consecutive_columns_with_their_boundary_samples() {
    let (samples, timestamps) = steep_fixture();
    let columns = decimate_steep(&samples, &timestamps);
    let joined: Vec<Connector> = connectors(&columns).collect();

    assert_eq!(
        joined.len(),
        STEEP_COLUMNS - 1,
        "with no NaN, every pair of consecutive columns is joined"
    );
    for (k, connector) in joined.iter().enumerate() {
        let last = index_at(&timestamps, columns[k].last_ts);
        let first = index_at(&timestamps, columns[k + 1].first_ts);
        assert_eq!(
            first,
            last + 1,
            "columns {k} and {}: their boundary samples are consecutive raw samples",
            k + 1
        );
        assert_eq!(
            *connector,
            line(&samples, &timestamps, last, first),
            "connector {k} runs from column {k}'s last raw sample to column {}'s first",
            k + 1
        );
    }

    // Pixel coverage on a 600-row plot spanning the signal: a column inks
    // the rows from its min to its max, a connector the rows between its two
    // ends. Between two consecutive columns, every row from the lower of
    // their extents to the higher must be inked.
    const ROWS: f64 = 600.0;
    let (low, high) = samples
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    let row = |value: f64| ((value - low) / (high - low) * (ROWS - 1.0)).round() as i64;
    let rows_between = |a: f64, b: f64| row(a.min(b))..=row(a.max(b));
    let mut holes_without_connectors = 0;
    for (k, connector) in joined.iter().enumerate() {
        let bars = [
            rows_between(columns[k].min, columns[k].max),
            rows_between(columns[k + 1].min, columns[k + 1].max),
        ];
        let link = rows_between(connector.from.1, connector.to.1);
        let lowest = *bars[0].start().min(bars[1].start());
        let highest = *bars[0].end().max(bars[1].end());
        let on_a_bar = |r: i64| bars.iter().any(|bar| bar.contains(&r));
        let uninked: Vec<i64> = (lowest..=highest)
            .filter(|r| !on_a_bar(*r) && !link.contains(r))
            .collect();
        assert!(
            uninked.is_empty(),
            "columns {k} and {}: rows {uninked:?} between their extents are left uninked",
            k + 1
        );
        if (lowest..=highest).any(|r| !on_a_bar(r)) {
            holes_without_connectors += 1;
        }
    }
    assert!(
        holes_without_connectors * 2 > STEEP_COLUMNS - 1,
        "the fixture must be steep: bars alone must leave a hole between most pairs of \
         consecutive columns, only {holes_without_connectors} do"
    );
}

#[test]
fn continuity_never_crosses_a_nan() {
    let (mut samples, timestamps) = steep_fixture();
    // Where the columns fall depends on the timestamps only.
    let clean = decimate_steep(&samples, &timestamps);
    let first_of = |k: usize| index_at(&timestamps, clean[k].first_ts);
    let last_of = |k: usize| index_at(&timestamps, clean[k].last_ts);
    let interior = (300..)
        .find(|&k| last_of(k) - first_of(k) >= 2)
        .expect("some column holds three samples");

    samples[last_of(100)] = f64::NAN; // column 100 ends on a NaN
    samples[first_of(200)] = f64::NAN; // column 200 starts on one
    samples[first_of(interior) + 1] = f64::NAN; // inside a column only
    for value in &mut samples[first_of(400)..=last_of(400)] {
        *value = f64::NAN; // a whole column of NaN: a gap bucket
    }
    let columns = decimate_steep(&samples, &timestamps);
    for (k, (column, clean)) in columns.iter().zip(&clean).enumerate() {
        assert_eq!(
            (column.first_ts, column.last_ts),
            (clean.first_ts, clean.last_ts),
            "column {k}: NaN values never move a column"
        );
    }

    // The convergence regime's line from each column's last sample to the
    // next column's first, wherever neither sample is NaN.
    let expected: Vec<Connector> = (0..STEEP_COLUMNS - 1)
        .map(|k| (last_of(k), first_of(k + 1)))
        .filter(|&(from, to)| !samples[from].is_nan() && !samples[to].is_nan())
        .map(|(from, to)| line(&samples, &timestamps, from, to))
        .collect();
    let joined: Vec<Connector> = connectors(&columns).collect();
    assert_eq!(joined, expected);

    let joins = |k: usize| {
        let (from, to) = (clean[k].last_ts, clean[k + 1].first_ts);
        joined.iter().any(|c| c.from.0 == from && c.to.0 == to)
    };
    for k in [100, 199, 399, 400] {
        assert!(
            !joins(k),
            "columns {k} and {} must not be joined across a NaN",
            k + 1
        );
    }
    for k in [interior - 1, interior, 99, 101, 198, 200] {
        assert!(
            joins(k),
            "columns {k} and {} have finite boundary samples and must be joined",
            k + 1
        );
    }
    assert_eq!(
        joined.len(),
        STEEP_COLUMNS - 1 - 4,
        "exactly the four joins that touch a NaN are left out"
    );
}

/// The connector rule is the convergence line's rule: once every sample has
/// its own column, the connectors are exactly the segments between
/// consecutive raw samples, broken wherever either one is NaN (SPEC §1.3).
#[test]
fn converged_columns_are_joined_exactly_like_the_convergence_line() {
    let mut rng = Xorshift64::new(0xC0DE);
    let mut samples: Vec<f64> = (0..50).map(|_| rng.next_f64() * 10.0).collect();
    for i in [0, 10, 11, 30, 49] {
        samples[i] = f64::NAN;
    }
    let timestamps: Vec<i128> = (0..50).map(|i| i * 1_000).collect();
    let pyramid = build_pyramid(&samples, &timestamps);
    let range = (timestamps[0], timestamps[49]);
    let columns = decimate_viewport(&pyramid, &samples, &timestamps, range, 200);
    assert_eq!(columns.len(), 50, "one column per raw sample");

    let expected: Vec<Connector> = (0..49)
        .filter(|&i| !samples[i].is_nan() && !samples[i + 1].is_nan())
        .map(|i| line(&samples, &timestamps, i, i + 1))
        .collect();
    assert_eq!(connectors(&columns).collect::<Vec<_>>(), expected);
    assert_eq!(expected.len(), 49 - 7, "seven segments touch a NaN");
}
