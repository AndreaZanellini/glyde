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

//! Issue #178: the time view reads a column's exact edge samples as one
//! `&[f64]`, so a numeric column of any other dtype needed an `f64` copy of
//! itself — in RAM, even for a file spilled because it does not fit there.
//! The on-disk pyramid of such a column now keeps its samples as `f64`, on
//! disk, written in the pass that builds level 0: exactly the column's own
//! `f64` promotion, and nothing for a column that is `f64` already.

use std::path::Path;

use glyde_core::budget::RamBudget;
use glyde_core::dsp::decimation::Pyramid;
use glyde_core::index::disk_pyramid::{build_disk_pyramid, build_disk_pyramid_keeping_samples};
use glyde_core::index::spill::{SpillSet, SpillVecWriter};
use glyde_core::ingest::{self, Dataset};
use glyde_core::series::{SeriesValues, SpilledValues};

fn bits(values: &[f64]) -> Vec<u64> {
    values.iter().map(|value| value.to_bits()).collect()
}

/// A spilled dataset with an `i64` column (`count`) and an `f64` one
/// (`value`).
fn spilled(dir: &Path) -> Dataset {
    let path = dir.join("mixed.csv");
    let mut text = String::from("timestamp,count,value\n");
    for i in 0..5_000i64 {
        text.push_str(&format!(
            "{},{},{}\n",
            1_700_000_000 + i,
            (i * 7_919) % 10_007 - 5_000,
            (i as f64 * 0.01).sin()
        ));
    }
    std::fs::write(&path, text).expect("write fixture");
    let dataset = ingest::load_with_budget(&path, RamBudget::from_total_ram_bytes(0), dir)
        .expect("open spilled");
    assert!(dataset.is_spilled());
    dataset
}

#[test]
fn a_spilled_integer_columns_pyramid_keeps_its_f64_samples_and_a_float_ones_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let dataset = spilled(dir.path());
    let pyramids = ingest::disk_pyramids_for_dataset(&dataset, dir.path());
    for (series, pyramid) in dataset.columns.iter().zip(&pyramids) {
        let pyramid = pyramid.as_ref().expect("every column is numeric");
        match series.values() {
            SeriesValues::Spilled(SpilledValues::I64(_)) => {
                let kept = pyramid
                    .samples()
                    .expect("an i64 column keeps its f64 samples");
                let promoted = series.values().to_f64_vec().expect("numeric");
                assert_eq!(bits(kept), bits(&promoted), "{}", series.name());
            }
            SeriesValues::Spilled(SpilledValues::F64(_)) => {
                assert!(
                    pyramid.samples().is_none(),
                    "an f64 column is its own f64 samples: {}",
                    series.name()
                );
            }
            other => panic!("unexpected column {:?}", other.dtype()),
        }
        // The trait reports what the pyramid keeps, as the view asks it.
        assert_eq!(
            Pyramid::samples(pyramid).map(bits),
            pyramid.samples().map(bits)
        );
    }
}

// The kept samples change nothing about the pyramid itself, and are the
// column's own promotion for a narrower dtype too.
#[test]
fn keeping_the_samples_builds_the_same_pyramid() {
    let dir = tempfile::tempdir().expect("temp dir");
    let dataset = spilled(dir.path());
    let set = SpillSet::create(dir.path()).expect("spill set");

    let values: Vec<i32> = (0..20_000).map(|i| (i * 7_919) % 70_001 - 35_000).collect();
    let mut writer = SpillVecWriter::create(&set, "narrow").expect("create");
    writer.extend_from_slice(&values).expect("write");
    let column = SeriesValues::Spilled(SpilledValues::I32(writer.finish().expect("finish")));
    let ticks: Vec<i128> = (0..values.len() as i128).collect();
    let source = column.sample_source().expect("numeric");
    let plain = build_disk_pyramid(&set, &source, &ticks[..]).expect("plain");
    let keeping = build_disk_pyramid_keeping_samples(&set, &source, &ticks[..]).expect("keeping");
    assert_eq!(keeping, plain, "the same buckets, level for level");
    assert!(plain.samples().is_none());
    let promoted: Vec<f64> = values.iter().map(|&value| f64::from(value)).collect();
    assert_eq!(keeping.samples().map(bits), Some(bits(&promoted)));

    // And for a whole spilled column of the fixture, against its own time
    // axis.
    let count = dataset.columns[0].values();
    let source = count.sample_source().expect("numeric");
    let plain = build_disk_pyramid(&set, &source, &dataset.time).expect("plain");
    let keeping =
        build_disk_pyramid_keeping_samples(&set, &source, &dataset.time).expect("keeping");
    assert_eq!(keeping, plain);
}
