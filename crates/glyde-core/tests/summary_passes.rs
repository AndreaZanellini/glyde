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

//! Issue #163: the time-axis summary (SPEC §2.1–2.2: gaps, sampling class,
//! monotonicity) is a sequential stretch of every open, a quarter of a 1 GB
//! Parquet file's. Each of its passes reads every tick again, so it must
//! not make one it does not need: the median Δt of a gap-free axis is the
//! one its uniformity test needs, so it is computed once, not twice; and the
//! two middle keys of an even count, equal on a real time axis, come from
//! one selection.

use std::cell::Cell;
use std::ops::Range;

use glyde_core::time::{summarize_ticks, SamplingClass, TickSource};
use glyde_core::Result;

/// A time axis that counts how many ticks were read from it.
struct Counted<'a> {
    ticks: &'a [i128],
    read: Cell<usize>,
}

impl TickSource for Counted<'_> {
    fn tick_count(&self) -> usize {
        self.ticks.len()
    }

    fn visit_tick_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[i128]) -> Result<()>,
    ) -> Result<()> {
        let end = range.end.min(self.ticks.len());
        let start = range.start.min(end);
        self.read.set(self.read.get() + (end - start));
        self.ticks.visit_tick_chunks(range, visit)
    }
}

// A constant Δt makes every exact selection a single pass (its keys are all
// equal), so the passes can be counted: one for the median Δt (an even
// number of Δt, whose two equal middle keys come from that one selection),
// one for the scan that counts gaps and checks monotonicity, one for the
// median absolute deviation. Three passes over the axis, not seven.
#[test]
fn a_gap_free_axis_is_summarized_in_three_passes_with_its_median_computed_once() {
    let n = 100_001usize;
    let ticks: Vec<i128> = (0..n as i128)
        .map(|i| 1_700_000_000_000 + i * 1_000)
        .collect();
    let axis = Counted {
        ticks: &ticks,
        read: Cell::new(0),
    };
    let stats = summarize_ticks(&axis).expect("an in-memory axis cannot fail");
    assert_eq!(stats.sampling_class, SamplingClass::Uniform);
    assert_eq!(stats.gap_count, 0);
    assert_eq!(stats.monotonicity.non_monotonic_count, 0);
    assert_eq!(
        axis.read.get(),
        3 * n,
        "the summary read the axis {} times",
        axis.read.get() as f64 / n as f64
    );
}
