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

//! Issue #103: `detect_gaps` collects one `Gap` per gap over a whole slice,
//! which is unbounded in principle (corpus case 60: a pathological median
//! makes almost every other step a gap) and cannot run on a spilled axis at
//! all. Whatever shape M8's gap view takes — a window around the viewport,
//! per-pixel aggregates, or pages — it is built on [`visit_gaps`]: a
//! streaming scan of any row range of any [`TickSource`], spilled included,
//! against the **whole axis'** threshold ([`gap_threshold`]), holding no
//! list of gaps.

use std::path::Path;

use glyde_core::budget::RamBudget;
use glyde_core::time::{detect_gaps, gap_threshold, visit_gaps, Gap};

fn case_60() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corpus/case-60-pathological-gap-median.csv")
}

fn visited(ticks: &[i128], range: std::ops::Range<usize>) -> Vec<Gap> {
    let threshold = gap_threshold(ticks)
        .expect("in memory")
        .expect("two or more samples");
    let mut gaps = Vec::new();
    visit_gaps(ticks, range, threshold, &mut |gap| {
        gaps.push(gap);
        Ok(())
    })
    .expect("in memory");
    gaps
}

#[test]
fn a_window_yields_exactly_the_whole_axis_gaps_that_start_in_it() {
    let dataset = glyde_core::ingest::load(&case_60()).expect("case 60 loads");
    let ticks = dataset.time.to_pyramid_ticks();
    let all = detect_gaps(&ticks);
    assert_eq!(all.len(), 98, "corpus case 60's documented gap count");
    assert_eq!(visited(&ticks, 0..ticks.len()), all);

    for range in [37..120, 150..200, 0..1, 60..60, 120..140] {
        let expected: Vec<Gap> = all
            .iter()
            .copied()
            .filter(|gap| range.contains(&gap.before_index) && gap.after_index < range.end)
            .collect();
        assert_eq!(visited(&ticks, range.clone()), expected, "{range:?}");
    }
}

// The threshold belongs to the whole axis: a window made only of the long
// steps still reports them as gaps, though on its own its median step is
// long too.
#[test]
fn the_threshold_is_the_whole_axis_not_the_window() {
    let dataset = glyde_core::ingest::load(&case_60()).expect("case 60 loads");
    let ticks = dataset.time.to_pyramid_ticks();
    // Rows 0..=49 are joined by 49 long steps (case 60's layout).
    assert_eq!(visited(&ticks, 0..50).len(), 49);
}

#[test]
fn gaps_stream_from_a_spilled_axis_too() {
    let cache = tempfile::tempdir().expect("temp cache dir");
    let spilled = glyde_core::ingest::load_with_budget(
        &case_60(),
        RamBudget::from_total_ram_bytes(0),
        cache.path(),
    )
    .expect("case 60 opens spilled");
    assert!(spilled.is_spilled());

    let threshold = gap_threshold(&spilled.time)
        .expect("reading the spilled axis")
        .expect("two or more samples");
    let mut count = 0;
    visit_gaps(&spilled.time, 0..spilled.time.len(), threshold, &mut |_| {
        count += 1;
        Ok(())
    })
    .expect("reading the spilled axis");
    assert_eq!(count, 98);
}

#[test]
fn fewer_than_two_samples_have_no_threshold() {
    assert_eq!(gap_threshold(&[][..] as &[i128]).expect("in memory"), None);
    assert_eq!(gap_threshold(&[5i128][..]).expect("in memory"), None);
}
