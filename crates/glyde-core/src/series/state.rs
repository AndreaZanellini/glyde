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

/// Collapses `values`/`ticks` (`ticks[i]` is `values[i]`'s own timestamp,
/// the same parallel-slice convention [`crate::dsp::decimation`] uses) into
/// one [`BoolBand`] per maximal run of consecutive equal values (SPEC §4.3).
///
/// `values` and `ticks` must be the same length; a length mismatch or an
/// empty input yields no bands — there is no axis to place a band on, not an
/// error to propagate (this mirrors [`crate::dsp::decimation::build_pyramid`]'s
/// own "nothing to render" handling for an empty column).
pub fn bool_state_bands(values: &[bool], ticks: &[i128]) -> Vec<BoolBand> {
    if values.is_empty() || values.len() != ticks.len() {
        return Vec::new();
    }

    let mut bands = Vec::new();
    let mut run_start = 0usize;
    for index in 1..=values.len() {
        let run_ended = index == values.len() || values[index] != values[run_start];
        if !run_ended {
            continue;
        }
        let end_tick = if index == values.len() {
            // The last run: its true end is unknown, so this reports only
            // its own last sample's tick (see the doc comment above).
            ticks[index - 1]
        } else {
            // Holds until the next run's first sample.
            ticks[index]
        };
        bands.push(BoolBand {
            start_tick: ticks[run_start],
            end_tick,
            value: values[run_start],
        });
        run_start = index;
    }
    bands
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
}
