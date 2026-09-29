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

//! Boolean state bands (docs/SPEC.md §4.3, docs/ROADMAP.md M6): a `bool`
//! series' raw samples collapsed into maximal runs of the same value, one
//! per [`BoolBand`], so `glyde-app`'s state timeline view (SPEC §4.3: "not
//! step plots on a numeric axis") can draw one band per run instead of one
//! point per sample.

/// One maximal run of a [`crate::series::SeriesValues::Bool`] series holding
/// the same `value`, as the closed interval on the time axis it is known to
/// hold over: `start_tick` is the run's first sample's own tick.
/// `end_tick` is the tick of the sample where the *next* run starts (SPEC
/// §4.3: "the interval each value holds", i.e. until it changes) for every
/// run but the last, whose true end is unknown — a still-open run only ever
/// reports its own last sample's tick, never an invented extension (Golden
/// Rule 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoolBand {
    pub start_tick: i128,
    pub end_tick: i128,
    pub value: bool,
}

/// The fixed-size overview used when a series changes state too often to
/// retain and draw every run. Each nonzero cell records every state seen in
/// that part of the time axis; `3` means both states occurred and must be
/// drawn as a visible multiple-states glyph, not silently dropped.
pub const BOOL_OVERVIEW_CELLS: usize = 2048;
pub const MAX_EXACT_BOOL_BANDS: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub enum BoolLane {
    Exact {
        bands: Vec<BoolBand>,
        min_tick: i128,
        max_tick: i128,
    },
    Overview {
        cells: Vec<u8>,
        min_tick: i128,
        max_tick: i128,
    },
}

impl BoolLane {
    pub fn tick_bounds(&self) -> (i128, i128) {
        match self {
            BoolLane::Exact {
                min_tick, max_tick, ..
            }
            | BoolLane::Overview {
                min_tick, max_tick, ..
            } => (*min_tick, *max_tick),
        }
    }
}

struct Overview {
    min_tick: i128,
    max_tick: i128,
    off_diff: Vec<i64>,
    on_diff: Vec<i64>,
}

impl Overview {
    fn cell(&self, tick: i128) -> usize {
        if self.max_tick <= self.min_tick {
            return 0;
        }
        let span = self.max_tick.saturating_sub(self.min_tick) as f64;
        let offset = tick.saturating_sub(self.min_tick) as f64;
        ((offset / span) * (BOOL_OVERVIEW_CELLS - 1) as f64)
            .floor()
            .clamp(0.0, (BOOL_OVERVIEW_CELLS - 1) as f64) as usize
    }

    fn record(&mut self, band: BoolBand) {
        let first = self.cell(band.start_tick.min(band.end_tick));
        let last = self.cell(band.start_tick.max(band.end_tick));
        // Range updates are constant time even when a non-monotonic source
        // jumps across the entire axis on every row. Materialize the masks
        // once in finish(), after all runs have been seen.
        let diff = if band.value {
            &mut self.on_diff
        } else {
            &mut self.off_diff
        };
        diff[first] += 1;
        diff[last + 1] -= 1;
    }

    fn finish(self) -> Vec<u8> {
        let mut off = 0;
        let mut on = 0;
        let mut cells = Vec::with_capacity(BOOL_OVERVIEW_CELLS);
        for index in 0..BOOL_OVERVIEW_CELLS {
            off += self.off_diff[index];
            on += self.on_diff[index];
            cells.push(u8::from(off > 0) | (u8::from(on > 0) << 1));
        }
        cells
    }
}

/// The run [`BoolBandBuilder`] is still accumulating — not yet a [`BoolBand`]
/// because its true end (what the *next* sample turns out to be, or that
/// there is no next sample) isn't known yet.
struct OpenRun {
    start_tick: i128,
    last_tick: i128,
    value: bool,
}

/// Builds [`BoolBand`]s one sample at a time via [`Self::push`], so a caller
/// can feed it a column too large to hold in memory at once — read back in
/// bounded chunks (SPEC §5.1: "read in bounded chunks", the same discipline
/// [`crate::index::spill::SpillVec::read_chunks`] already applies to a
/// spilled column) — instead of collecting the whole column into a slice
/// first just to call [`bool_state_bands`]. Samples must be pushed in
/// original file order, covering every sample exactly once; call
/// [`Self::finish`] after the last one to flush the still-open run.
#[derive(Default)]
pub struct BoolBandBuilder {
    bands: Vec<BoolBand>,
    open: Option<OpenRun>,
    overview: Option<Overview>,
    summarized: bool,
}

impl BoolBandBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bounded-memory form for a whole-file, fixed-axis view. It retains
    /// exact runs while they fit, then switches to a 2048-cell overview.
    /// Every on/off occurrence remains visible through a mixed-state cell.
    pub fn for_view(min_tick: i128, max_tick: i128) -> Self {
        Self {
            overview: Some(Overview {
                min_tick,
                max_tick,
                off_diff: vec![0; BOOL_OVERVIEW_CELLS + 1],
                on_diff: vec![0; BOOL_OVERVIEW_CELLS + 1],
            }),
            ..Self::default()
        }
    }

    fn emit(&mut self, band: BoolBand) {
        if let Some(overview) = &mut self.overview {
            overview.record(band);
            if !self.summarized {
                self.bands.push(band);
                if self.bands.len() > MAX_EXACT_BOOL_BANDS {
                    self.bands.clear();
                    self.summarized = true;
                }
            }
        } else {
            self.bands.push(band);
        }
    }

    /// Feeds one more `(value, tick)` sample, in file order, into the run
    /// currently being accumulated — closing it into a [`BoolBand`] first if
    /// `value` differs from the run's own value (SPEC §4.3: a run holds
    /// "until it changes", i.e. until the tick of the sample that changed
    /// it).
    pub fn push(&mut self, value: bool, tick: i128) {
        match &mut self.open {
            None => {
                self.open = Some(OpenRun {
                    start_tick: tick,
                    last_tick: tick,
                    value,
                })
            }
            Some(run) if run.value == value => run.last_tick = tick,
            Some(run) => {
                let band = BoolBand {
                    start_tick: run.start_tick,
                    end_tick: tick,
                    value: run.value,
                };
                *run = OpenRun {
                    start_tick: tick,
                    last_tick: tick,
                    value,
                };
                self.emit(band);
            }
        }
    }

    /// Flushes the still-open run, if any, and returns every [`BoolBand`]
    /// accumulated so far — its true end is unknown (see [`BoolBand`]'s own
    /// doc comment), so it is closed at its own last pushed tick rather than
    /// an invented one.
    pub fn finish(mut self) -> Vec<BoolBand> {
        debug_assert!(
            self.overview.is_none(),
            "use finish_lane for a view builder"
        );
        if let Some(run) = self.open.take() {
            self.emit(BoolBand {
                start_tick: run.start_tick,
                end_tick: run.last_tick,
                value: run.value,
            });
        }
        self.bands
    }

    /// Finishes a builder created with [`Self::for_view`].
    pub fn finish_lane(mut self) -> BoolLane {
        if let Some(run) = self.open.take() {
            self.emit(BoolBand {
                start_tick: run.start_tick,
                end_tick: run.last_tick,
                value: run.value,
            });
        }
        let overview = self.overview.expect("finish_lane needs a view builder");
        if self.summarized {
            let min_tick = overview.min_tick;
            let max_tick = overview.max_tick;
            BoolLane::Overview {
                cells: overview.finish(),
                min_tick,
                max_tick,
            }
        } else {
            BoolLane::Exact {
                bands: self.bands,
                min_tick: overview.min_tick,
                max_tick: overview.max_tick,
            }
        }
    }
}

/// Collapses `values`/`ticks` (`ticks[i]` is `values[i]`'s own timestamp,
/// the same parallel-slice convention [`crate::dsp::decimation`] uses) into
/// one [`BoolBand`] per maximal run of consecutive equal values (SPEC §4.3),
/// via [`BoolBandBuilder`] — the right choice for a column already resident
/// in memory (a heap-backed [`crate::series::SeriesValues::Bool`]); a caller
/// reading a column back in bounded chunks instead should drive
/// [`BoolBandBuilder`] directly, one [`BoolBandBuilder::push`] per sample.
///
/// `values` and `ticks` must be the same length; a length mismatch or an
/// empty input yields no bands — there is no axis to place a band on, not an
/// error to propagate (this mirrors [`crate::dsp::decimation::build_pyramid`]'s
/// own "nothing to render" handling for an empty column).
pub fn bool_state_bands(values: &[bool], ticks: &[i128]) -> Vec<BoolBand> {
    if values.is_empty() || values.len() != ticks.len() {
        return Vec::new();
    }

    let mut builder = BoolBandBuilder::new();
    for (&value, &tick) in values.iter().zip(ticks) {
        builder.push(value, tick);
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_no_bands() {
        assert_eq!(bool_state_bands(&[], &[]), Vec::new());
    }

    #[test]
    fn mismatched_lengths_yield_no_bands() {
        assert_eq!(bool_state_bands(&[true, false], &[0]), Vec::new());
    }

    // SPEC §1.4: "single-sample series are valid inputs and must render" —
    // a lone sample is a run of one, reporting only what is actually known
    // (its own tick as both ends), never an invented extension.
    #[test]
    fn a_single_sample_is_one_zero_width_band() {
        assert_eq!(
            bool_state_bands(&[true], &[5]),
            vec![BoolBand {
                start_tick: 5,
                end_tick: 5,
                value: true,
            }]
        );
    }

    #[test]
    fn a_constant_series_is_one_band_spanning_the_whole_axis() {
        let values = [true, true, true, true];
        let ticks = [0, 1, 2, 3];

        assert_eq!(
            bool_state_bands(&values, &ticks),
            vec![BoolBand {
                start_tick: 0,
                end_tick: 3,
                value: true,
            }]
        );
    }

    // Corpus case 47's `flag_lower` column: true, false, true, false at
    // ticks 0..=3 — every sample its own run, each band holding until the
    // next sample except the last, which only reports its own tick.
    #[test]
    fn alternating_values_produce_one_band_per_sample() {
        let values = [true, false, true, false];
        let ticks = [0, 1, 2, 3];

        assert_eq!(
            bool_state_bands(&values, &ticks),
            vec![
                BoolBand {
                    start_tick: 0,
                    end_tick: 1,
                    value: true,
                },
                BoolBand {
                    start_tick: 1,
                    end_tick: 2,
                    value: false,
                },
                BoolBand {
                    start_tick: 2,
                    end_tick: 3,
                    value: true,
                },
                BoolBand {
                    start_tick: 3,
                    end_tick: 3,
                    value: false,
                },
            ]
        );
    }

    // A run of several equal samples collapses into one band spanning from
    // its first sample to the first sample of the next (different) run —
    // not one band per raw sample.
    #[test]
    fn a_multi_sample_run_collapses_into_one_band() {
        let values = [true, true, false, false, false, true];
        let ticks = [0, 1, 2, 3, 4, 5];

        assert_eq!(
            bool_state_bands(&values, &ticks),
            vec![
                BoolBand {
                    start_tick: 0,
                    end_tick: 2,
                    value: true,
                },
                BoolBand {
                    start_tick: 2,
                    end_tick: 5,
                    value: false,
                },
                BoolBand {
                    start_tick: 5,
                    end_tick: 5,
                    value: true,
                },
            ]
        );
    }

    // Ticks need not be evenly spaced — the band boundaries are the real
    // sample ticks, never a synthesized midpoint or uniform step.
    #[test]
    fn bands_use_the_real_sample_ticks_not_a_uniform_step() {
        let values = [true, false, false];
        let ticks = [0, 100, 250];

        assert_eq!(
            bool_state_bands(&values, &ticks),
            vec![
                BoolBand {
                    start_tick: 0,
                    end_tick: 100,
                    value: true,
                },
                BoolBand {
                    start_tick: 100,
                    end_tick: 250,
                    value: false,
                },
            ]
        );
    }

    // The whole point of `BoolBandBuilder` (split out for issue found on PR
    // #113's own review: `cache_bool_bands` was materializing an entire
    // spilled column into a `Vec<bool>` before calling `bool_state_bands`,
    // defeating SPEC §5.1's "read in bounded chunks" for the one dtype path
    // that only runs when a column didn't fit the memory budget) is that
    // feeding it samples one at a time, or in arbitrarily-sized pieces, must
    // give the exact same bands as handing `bool_state_bands` the whole
    // slice at once — a caller reading a spilled column back through
    // `SpillVec::read_chunks` has no choice but the former.
    #[test]
    fn builder_pushed_one_sample_at_a_time_matches_the_whole_slice_function() {
        let values = [true, true, false, false, false, true, true, false];
        let ticks = [0, 1, 2, 3, 4, 5, 6, 7];

        let mut builder = BoolBandBuilder::new();
        for (&value, &tick) in values.iter().zip(&ticks) {
            builder.push(value, tick);
        }

        assert_eq!(builder.finish(), bool_state_bands(&values, &ticks));
    }

    // The same equivalence, but pushed in irregular chunks that deliberately
    // split runs across a "chunk boundary" (mimicking a fixed-size
    // `read_chunks` buffer that has no idea where a run starts or ends) —
    // proving the builder's state correctly carries a still-open run across
    // separate `push` calls rather than only working when called in a tight
    // loop over one contiguous slice.
    #[test]
    fn builder_pushed_in_chunks_that_split_runs_matches_the_whole_slice_function() {
        let values = [true, true, true, false, false, true, false, false, false];
        let ticks = [0, 1, 2, 3, 4, 5, 6, 7, 8];
        let chunks: [&[bool]; 4] = [&values[0..2], &values[2..5], &values[5..6], &values[6..9]];
        let tick_chunks: [&[i128]; 4] = [&ticks[0..2], &ticks[2..5], &ticks[5..6], &ticks[6..9]];

        let mut builder = BoolBandBuilder::new();
        for (value_chunk, tick_chunk) in chunks.iter().zip(&tick_chunks) {
            for (&value, &tick) in value_chunk.iter().zip(*tick_chunk) {
                builder.push(value, tick);
            }
        }

        assert_eq!(builder.finish(), bool_state_bands(&values, &ticks));
    }

    #[test]
    fn an_empty_builder_finishes_with_no_bands() {
        assert_eq!(BoolBandBuilder::new().finish(), Vec::new());
    }

    #[test]
    fn many_transitions_use_bounded_overview_without_losing_either_state() {
        let len = MAX_EXACT_BOOL_BANDS * 3;
        let mut builder = BoolBandBuilder::for_view(0, len as i128 - 1);
        for index in 0..len {
            builder.push(index % 2 == 0, index as i128);
        }
        let BoolLane::Overview { cells, .. } = builder.finish_lane() else {
            panic!("frequently changing data must use the bounded overview");
        };
        assert_eq!(cells.len(), BOOL_OVERVIEW_CELLS);
        assert!(cells.contains(&3));
        assert!(cells.iter().all(|&cell| cell != 0));
    }

    #[test]
    fn a_few_transitions_keep_their_exact_ticks() {
        let mut builder = BoolBandBuilder::for_view(0, 10);
        builder.push(true, 0);
        builder.push(false, 7);
        builder.push(false, 10);
        assert_eq!(
            builder.finish_lane(),
            BoolLane::Exact {
                bands: bool_state_bands(&[true, false, false], &[0, 7, 10]),
                min_tick: 0,
                max_tick: 10,
            }
        );
    }

    #[test]
    fn duplicate_timestamps_with_many_transitions_still_show_both_states() {
        let mut builder = BoolBandBuilder::for_view(42, 42);
        for index in 0..=MAX_EXACT_BOOL_BANDS {
            builder.push(index % 2 == 0, 42);
        }
        let BoolLane::Overview { cells, .. } = builder.finish_lane() else {
            panic!("the view must be bounded");
        };
        assert_eq!(cells[0], 3);
    }

    #[test]
    fn non_monotonic_transitions_cover_the_overview_without_repeated_cell_scans() {
        let mut builder = BoolBandBuilder::for_view(0, 100);
        for index in 0..=MAX_EXACT_BOOL_BANDS {
            builder.push(index % 2 == 0, if index % 2 == 0 { 0 } else { 100 });
        }
        let BoolLane::Overview { cells, .. } = builder.finish_lane() else {
            panic!("the view must be bounded");
        };
        assert!(cells.iter().all(|&cell| cell == 3));
    }
}
