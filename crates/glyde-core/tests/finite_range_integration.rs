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

//! Issue #144: "Fit to data" frames each shown channel's finite values, and
//! used to find them by scanning every sample on the UI thread — seconds on
//! a spilled file. [`finite_range`] reads them from the column's pyramid
//! instead. It must be exactly the scan's answer, NaN and ±∞ left out, for
//! a pyramid in memory, on disk, or none at all.

use glyde_core::dsp::decimation::{build_pyramid, finite_range, Bucket, Pyramid};
use glyde_core::index::disk_pyramid::{build_disk_pyramid, ColumnPyramid};
use glyde_core::index::spill::SpillSet;

/// The reference: every finite sample, scanned.
fn scanned(samples: &[f64]) -> Option<(f64, f64)> {
    let finite = samples.iter().copied().filter(|v| v.is_finite());
    let min = finite.clone().reduce(f64::min)?;
    let max = finite.reduce(f64::max)?;
    Some((min, max))
}

/// A deterministic signal of `n` samples, with NaN runs and the infinities
/// `infinite` names (`(index, sign)`), plus the extremes planted away from
/// the infinities so a wrong descent would miss them.
fn fixture(n: usize, infinite: &[(usize, f64)]) -> Vec<f64> {
    let mut samples: Vec<f64> = (0..n)
        .map(|i| match i % 101 {
            0..=4 => f64::NAN,
            _ => ((i * 7919) % 2003) as f64 - 1000.0 + (i as f64 * 0.37).sin(),
        })
        .collect();
    if n > 50 {
        samples[n / 3] = 5_000.5;
        samples[2 * n / 3] = -5_000.25;
    }
    for &(index, sign) in infinite {
        if index < n {
            samples[index] = sign * f64::INFINITY;
        }
    }
    samples
}

fn assert_every_storage_agrees(samples: &[f64], what: &str) {
    let ticks: Vec<i128> = (0..samples.len() as i128).collect();
    let expected = scanned(samples);
    let memory = build_pyramid(samples, &ticks);
    let none: &[Vec<Bucket>] = &[];
    assert_eq!(finite_range(none, samples), expected, "{what}: no pyramid");
    assert_eq!(
        finite_range(&memory, samples),
        expected,
        "{what}: in-memory pyramid"
    );
    let dir = tempfile::tempdir().expect("temp dir");
    let set = SpillSet::create(dir.path()).expect("spill set");
    let disk = build_disk_pyramid(&set, samples, &ticks[..]).expect("disk pyramid");
    assert_eq!(
        Pyramid::finite_range(&disk, samples),
        expected,
        "{what}: on-disk pyramid"
    );
    for column in [ColumnPyramid::Memory(memory), ColumnPyramid::Disk(disk)] {
        assert_eq!(
            column.finite_range(samples),
            expected,
            "{what}: through the Pyramid trait"
        );
    }
}

#[test]
fn the_finite_range_is_the_scans_answer_at_every_size() {
    for n in [0, 1, 2, 7, 8, 9, 63, 64, 65, 511, 513, 4_097, 100_003] {
        assert_every_storage_agrees(&fixture(n, &[]), &format!("n={n}"));
    }
}

#[test]
fn infinities_are_left_out_wherever_they_fall() {
    let n = 40_001;
    let cases: Vec<Vec<(usize, f64)>> = vec![
        vec![(0, 1.0)],
        vec![(n - 1, -1.0)],
        vec![(n / 3, 1.0), (2 * n / 3, -1.0)],
        vec![(8, 1.0), (9, -1.0), (511, 1.0), (512, -1.0), (4_095, 1.0)],
        (0..n)
            .step_by(97)
            .map(|i| (i, if i % 2 == 0 { 1.0 } else { -1.0 }))
            .collect(),
    ];
    for infinite in cases {
        let what = format!("{} infinities, first at {:?}", infinite.len(), infinite[0]);
        assert_every_storage_agrees(&fixture(n, &infinite), &what);
    }
}

#[test]
fn a_column_with_no_finite_value_has_no_range() {
    for samples in [
        vec![],
        vec![f64::NAN; 1_000],
        vec![f64::INFINITY; 1_000],
        (0..1_000)
            .map(|i| match i % 3 {
                0 => f64::NAN,
                1 => f64::INFINITY,
                _ => f64::NEG_INFINITY,
            })
            .collect(),
    ] {
        assert_every_storage_agrees(&samples, &format!("{} non-finite samples", samples.len()));
    }
}

/// The point of the pyramid: with no infinity in it, the range comes from
/// its top level alone, never from the samples. Shown by handing it samples
/// that disagree with the pyramid they supposedly built.
#[test]
fn without_an_infinity_only_the_pyramid_is_read() {
    let samples = fixture(100_003, &[]);
    let ticks: Vec<i128> = (0..samples.len() as i128).collect();
    let pyramid = build_pyramid(&samples, &ticks);
    let unrelated = vec![0.0; samples.len()];
    assert_eq!(finite_range(&pyramid, &unrelated), scanned(&samples));
}
