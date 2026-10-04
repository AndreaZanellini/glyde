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

//! String/categorical state bands (docs/SPEC.md §4.3, docs/ROADMAP.md M6): a
//! `string` series' raw samples collapsed into maximal runs of the same
//! label, one [`StateBand`] per run, mirroring [`super::BoolBand`].
//!
//! Past [`MAX_EXACT_STATE_BANDS`] runs a view builder stops retaining runs
//! and keeps a fixed [`STATE_OVERVIEW_CELLS`]-cell overview instead, where a
//! cell holding more than one label is flagged [`StateCell::Mixed`] rather
//! than dropping any of them (SPEC §4.3: no event may silently disappear).
//! "Uniform cell" is decided in constant time per run with running
//! `count`/`sum`/`sum of squares` of a label's 31-bit FNV-1a id: all labels in
//! a cell are equal iff `count * sumsq == sum²`. Two *different* labels
//! sharing a 31-bit id is the only way this misjudges a cell (~2⁻³¹ per
//! pair); the exact path never relies on ids.

use std::collections::BTreeMap;

/// One maximal run of the same label; `end_tick` semantics are those of
/// [`super::BoolBand`] (the next run's start, or the run's own last sample
/// for the final, still-open run — never an invented extension).
#[derive(Debug, Clone, PartialEq)]
pub struct StateBand {
    pub start_tick: i128,
    pub end_tick: i128,
    pub label: String,
}

pub const STATE_OVERVIEW_CELLS: usize = 2048;
pub const MAX_EXACT_STATE_BANDS: usize = 4096;
/// Distinct labels remembered by name in an overview lane.
pub const MAX_OVERVIEW_LABELS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateCell {
    Empty,
    /// Exactly one label (its 31-bit id, see [`state_label_id`]) in this cell.
    Single(u32),
    /// Two or more different labels share this cell.
    Mixed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StateLane {
    Exact {
        bands: Vec<StateBand>,
        min_tick: i128,
        max_tick: i128,
    },
    Overview {
        cells: Vec<StateCell>,
        /// Id → label for up to [`MAX_OVERVIEW_LABELS`] labels seen.
        labels: BTreeMap<u32, String>,
        min_tick: i128,
        max_tick: i128,
    },
}

impl StateLane {
    pub fn tick_bounds(&self) -> (i128, i128) {
        match self {
            StateLane::Exact {
                min_tick, max_tick, ..
            }
            | StateLane::Overview {
                min_tick, max_tick, ..
            } => (*min_tick, *max_tick),
        }
    }
}

/// Stable 31-bit FNV-1a id of a label.
pub fn state_label_id(label: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in label.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash & 0x7fff_ffff
}

struct Overview {
    min_tick: i128,
    max_tick: i128,
    count: Vec<i64>,
    sum: Vec<i128>,
    sumsq: Vec<i128>,
    labels: BTreeMap<u32, String>,
}

impl Overview {
    fn cell(&self, tick: i128) -> usize {
        if self.max_tick <= self.min_tick {
            return 0;
        }
        let span = self.max_tick.saturating_sub(self.min_tick) as f64;
        let offset = tick.saturating_sub(self.min_tick) as f64;
        ((offset / span) * (STATE_OVERVIEW_CELLS - 1) as f64)
            .floor()
            .clamp(0.0, (STATE_OVERVIEW_CELLS - 1) as f64) as usize
    }

    fn record(&mut self, start_tick: i128, end_tick: i128, label: &str) {
        let id = state_label_id(label);
        if self.labels.len() < MAX_OVERVIEW_LABELS {
            self.labels.entry(id).or_insert_with(|| label.to_owned());
        }
        let first = self.cell(start_tick.min(end_tick));
        let last = self.cell(start_tick.max(end_tick));
        let id = i128::from(id);
        self.count[first] += 1;
        self.count[last + 1] -= 1;
        self.sum[first] += id;
        self.sum[last + 1] -= id;
        self.sumsq[first] += id * id;
        self.sumsq[last + 1] -= id * id;
    }

    fn finish(self) -> (Vec<StateCell>, BTreeMap<u32, String>) {
        let (mut count, mut sum, mut sumsq) = (0i64, 0i128, 0i128);
        let mut cells = Vec::with_capacity(STATE_OVERVIEW_CELLS);
        for index in 0..STATE_OVERVIEW_CELLS {
            count += self.count[index];
            sum += self.sum[index];
            sumsq += self.sumsq[index];
            cells.push(if count == 0 {
                StateCell::Empty
            } else if i128::from(count) * sumsq == sum * sum {
                StateCell::Single((sum / i128::from(count)) as u32)
            } else {
                StateCell::Mixed
            });
        }
        (cells, self.labels)
    }
}

struct OpenRun {
    start_tick: i128,
    last_tick: i128,
    label: String,
}

/// Builds [`StateBand`]s one sample at a time, in file order (see
/// [`super::BoolBandBuilder`], which this mirrors).
#[derive(Default)]
pub struct StateBandBuilder {
    bands: Vec<StateBand>,
    open: Option<OpenRun>,
    overview: Option<Overview>,
    summarized: bool,
}

impl StateBandBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bounded-memory form for a whole-file, fixed-axis view: retains exact
    /// runs while they fit, then switches to the fixed-size overview.
    pub fn for_view(min_tick: i128, max_tick: i128) -> Self {
        Self {
            overview: Some(Overview {
                min_tick,
                max_tick,
                count: vec![0; STATE_OVERVIEW_CELLS + 1],
                sum: vec![0; STATE_OVERVIEW_CELLS + 1],
                sumsq: vec![0; STATE_OVERVIEW_CELLS + 1],
                labels: BTreeMap::new(),
            }),
            ..Self::default()
        }
    }

    fn emit(&mut self, band: StateBand) {
        if let Some(overview) = &mut self.overview {
            overview.record(band.start_tick, band.end_tick, &band.label);
            if !self.summarized {
                self.bands.push(band);
                if self.bands.len() > MAX_EXACT_STATE_BANDS {
                    self.bands = Vec::new();
                    self.summarized = true;
                }
            }
        } else {
            self.bands.push(band);
        }
    }

    /// Feeds one more `(label, tick)` sample in file order, closing the open
    /// run first when the label changes.
    pub fn push(&mut self, label: &str, tick: i128) {
        match &mut self.open {
            None => {
                self.open = Some(OpenRun {
                    start_tick: tick,
                    last_tick: tick,
                    label: label.to_owned(),
                })
            }
            Some(run) if run.label == label => run.last_tick = tick,
            Some(run) => {
                let closed = StateBand {
                    start_tick: run.start_tick,
                    end_tick: tick,
                    label: std::mem::replace(&mut run.label, label.to_owned()),
                };
                run.start_tick = tick;
                run.last_tick = tick;
                self.emit(closed);
            }
        }
    }

    fn flush(&mut self) {
        if let Some(run) = self.open.take() {
            self.emit(StateBand {
                start_tick: run.start_tick,
                end_tick: run.last_tick,
                label: run.label,
            });
        }
    }

    /// Flushes the still-open run (closed at its own last tick, never an
    /// invented one) and returns every band.
    pub fn finish(mut self) -> Vec<StateBand> {
        debug_assert!(
            self.overview.is_none(),
            "use finish_lane for a view builder"
        );
        self.flush();
        self.bands
    }

    /// Finishes a builder created with [`Self::for_view`].
    pub fn finish_lane(mut self) -> StateLane {
        self.flush();
        let overview = self.overview.expect("finish_lane needs a view builder");
        let (min_tick, max_tick) = (overview.min_tick, overview.max_tick);
        if self.summarized {
            let (cells, labels) = overview.finish();
            StateLane::Overview {
                cells,
                labels,
                min_tick,
                max_tick,
            }
        } else {
            StateLane::Exact {
                bands: self.bands,
                min_tick,
                max_tick,
            }
        }
    }
}

/// Collapses parallel `labels`/`ticks` slices into maximal runs. A length
/// mismatch or empty input yields no bands.
pub fn string_state_bands<S: AsRef<str>>(labels: &[S], ticks: &[i128]) -> Vec<StateBand> {
    if labels.is_empty() || labels.len() != ticks.len() {
        return Vec::new();
    }
    let mut builder = StateBandBuilder::new();
    for (label, &tick) in labels.iter().zip(ticks) {
        builder.push(label.as_ref(), tick);
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(start: i128, end: i128, label: &str) -> StateBand {
        StateBand {
            start_tick: start,
            end_tick: end,
            label: label.to_owned(),
        }
    }

    #[test]
    fn empty_or_mismatched_input_yields_no_bands() {
        assert!(string_state_bands::<&str>(&[], &[]).is_empty());
        assert!(string_state_bands(&["a", "b"], &[0]).is_empty());
    }

    #[test]
    fn corpus_48_shape_collapses_into_four_runs() {
        // testdata/corpus/case-48: idle idle running running fault idle
        let labels = ["idle", "idle", "running", "running", "fault", "idle"];
        let ticks = [0, 1, 2, 3, 4, 5];
        assert_eq!(
            string_state_bands(&labels, &ticks),
            vec![
                band(0, 2, "idle"),
                band(2, 4, "running"),
                band(4, 5, "fault"),
                band(5, 5, "idle"),
            ]
        );
    }

    #[test]
    fn a_single_sample_is_one_zero_width_band() {
        assert_eq!(string_state_bands(&["x"], &[7]), vec![band(7, 7, "x")]);
    }

    #[test]
    fn labels_are_compared_exactly_never_trimmed_or_case_folded() {
        let bands = string_state_bands(&["Run", "run", "run "], &[0, 1, 2]);
        assert_eq!(bands.len(), 3, "{bands:?}");
    }

    #[test]
    fn empty_string_label_is_a_real_state() {
        let bands = string_state_bands(&["a", "", "a"], &[0, 1, 2]);
        assert_eq!(
            bands,
            vec![band(0, 1, "a"), band(1, 2, ""), band(2, 2, "a")]
        );
    }

    #[test]
    fn view_builder_keeps_exact_runs_while_they_fit() {
        let mut builder = StateBandBuilder::for_view(0, 3);
        for (label, tick) in [("a", 0), ("b", 1), ("b", 2), ("a", 3)] {
            builder.push(label, tick);
        }
        match builder.finish_lane() {
            StateLane::Exact { bands, .. } => assert_eq!(bands.len(), 3),
            other => panic!("expected exact lane, got {other:?}"),
        }
    }

    #[test]
    fn dense_alternation_falls_back_to_an_overview_that_flags_mixed_cells() {
        let len = (MAX_EXACT_STATE_BANDS * 3) as i128;
        let mut builder = StateBandBuilder::for_view(0, len - 1);
        for tick in 0..len {
            builder.push(if tick % 2 == 0 { "on" } else { "off" }, tick);
        }
        match builder.finish_lane() {
            StateLane::Overview { cells, labels, .. } => {
                assert_eq!(cells.len(), STATE_OVERVIEW_CELLS);
                assert!(cells.iter().all(|cell| *cell == StateCell::Mixed));
                assert_eq!(labels.len(), 2);
            }
            other => panic!("expected overview, got {other:?}"),
        }
    }

    #[test]
    fn overview_marks_a_uniform_cell_single_and_unvisited_cells_empty() {
        // One short run of "x" at each end of a long axis, nothing between
        // except a jump: force the overview with many alternating runs first.
        let n = (MAX_EXACT_STATE_BANDS + 10) as i128;
        let mut builder = StateBandBuilder::for_view(0, 1_000_000);
        for tick in 0..n {
            builder.push(if tick % 2 == 0 { "p" } else { "q" }, tick);
        }
        builder.push("z", 1_000_000);
        let StateLane::Overview { cells, labels, .. } = builder.finish_lane() else {
            panic!("expected overview");
        };
        assert_eq!(cells[0], StateCell::Mixed);
        assert_eq!(cells[1000], StateCell::Single(state_label_id("q")));
        assert!(labels.contains_key(&state_label_id("z")));
    }
}
