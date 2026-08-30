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

//! State timeline, `bool` columns only for now (docs/ROADMAP.md M6, SPEC
//! §4.3): renders each `bool` [`glyde_core::series::Series`] as its own
//! compact lane of on/off horizontal bands — filled rectangles, never a
//! numeric line/step plot ("Boolean flags → horizontal bands (on/off), not
//! step plots on a numeric axis").
//!
//! [`glyde_core::series::bool_state_bands`] does the actual collapsing of
//! raw samples into bands (product logic, so it lives in `glyde-core` per
//! `docs/ARCHITECTURE.md` Hard rule 2); this module only draws them.
//!
//! Deliberately out of scope here — remaining M6 roadmap items, not yet
//! built:
//! - `string`/categorical state bands (SPEC §4.3's other state-timeline
//!   case; only `bool` columns are read by [`show`] today).
//! - Markers (single-sample events on their own lane).
//! - The "multiple states" collapse glyph for zoomed-out bands — every
//!   band is always drawn in full here, which is correct (never loses an
//!   event) but not yet decimation-aware.
//! - A single pannable/zoomable axis shared with [`super::time`]'s plot.
//!   Each lane here is a fixed, always-fit-to-data, non-interactive view of
//!   the *whole* file instead — panning/zooming the time-domain view above
//!   does not (yet) move these bands with it.
use egui_plot::{Plot, PlotBounds, PlotPoints, Polygon};
use glyde_core::index::spill::SpillVec;
use glyde_core::ingest::{Dataset, TimeAxis};
use glyde_core::series::{
    bool_state_bands, BoolBand, BoolBandBuilder, SeriesValues, SpilledValues,
};

use super::time::{format_x_axis_tick, tick_to_seconds};

/// Height, in points, of one `bool` column's lane.
const LANE_HEIGHT: f32 = 36.0;

/// Fill color for a `true` band. `egui`'s own default accent blue — matches
/// nothing else drawn today, so an on/off lane never reads as a numeric
/// series (`views::time::series_color`'s golden-ratio hue stepping is a
/// different, per-series palette, deliberately not reused here since bands
/// only ever need the two states, not N distinct series colors).
const ON_COLOR: egui::Color32 = egui::Color32::from_rgb(66, 133, 244);
/// Fill color for a `false` band — a neutral, visibly "off" gray.
const OFF_COLOR: egui::Color32 = egui::Color32::from_gray(60);

/// Renders `dataset`'s `bool` columns as on/off band lanes (SPEC §4.3) into
/// `ui`, one [`egui_plot::Plot`] per column. `bool_bands` is
/// `dataset.columns`-parallel (see [`cache_bool_bands`]); a `None` entry
/// (every non-`bool` column) draws nothing. `ticks` is `dataset.time`'s own
/// pyramid ticks (`glyde_core::ingest::TimeAxis::to_pyramid_ticks`), taken
/// as a parameter and computed by the caller once per status change — the
/// same once-not-per-frame discipline [`super::time::show`] follows for the
/// identical reason (see its own doc comment).
pub fn show(
    ui: &mut egui::Ui,
    dataset: &Dataset,
    ticks: &[i128],
    bool_bands: &[Option<Vec<BoolBand>>],
) {
    let time = &dataset.time;
    let Some((axis_min, axis_max)) = axis_bounds(ticks, time) else {
        return;
    };

    for (index, series) in dataset.columns.iter().enumerate() {
        let Some(Some(bands)) = bool_bands.get(index) else {
            continue;
        };

        ui.label(series.name());
        Plot::new(format!("state_timeline_{index}"))
            .height(LANE_HEIGHT)
            .show_axes([true, false])
            .show_grid([false, false])
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_drag(false)
            .allow_boxed_zoom(false)
            .x_axis_formatter(move |mark, _range| format_x_axis_tick(ticks, mark, time))
            .show(ui, |plot_ui| {
                plot_ui.set_plot_bounds(PlotBounds::from_min_max([axis_min, 0.0], [axis_max, 1.0]));
                for band in bands {
                    draw_band(plot_ui, time, band, axis_max);
                }
            });
    }
}

/// Draws one [`BoolBand`] as a filled rectangle spanning its own vertical
/// lane (`y` in `[0, 1]`, an arbitrary unit range — this view has no
/// numeric axis, SPEC §4.3). A still-open final run (`end_tick ==
/// start_tick`, see [`BoolBand`]'s own doc comment for why `bool_state_bands`
/// reports it that way rather than inventing an end) is extended visually to
/// `axis_max` — the same "known to hold at least this long" reading the
/// lane's own always-fit bounds already commit to, not a new claim about the
/// data.
fn draw_band(plot_ui: &mut egui_plot::PlotUi, time: &TimeAxis, band: &BoolBand, axis_max: f64) {
    let x0 = tick_to_seconds(time, band.start_tick);
    let x1 = if band.end_tick > band.start_tick {
        tick_to_seconds(time, band.end_tick)
    } else {
        axis_max
    };
    let color = if band.value { ON_COLOR } else { OFF_COLOR };
    plot_ui.polygon(
        Polygon::new(PlotPoints::new(vec![
            [x0, 0.0],
            [x1, 0.0],
            [x1, 1.0],
            [x0, 1.0],
        ]))
        .fill_color(color)
        .stroke(egui::Stroke::NONE),
    );
}

/// The full-file x-axis bounds (in plot seconds, [`tick_to_seconds`]'s
/// coordinate space) every lane always fits itself to — `None` for an empty
/// axis, which draws nothing. A single-sample file (SPEC §1.4: a valid
/// input) has a zero-width range; it is padded symmetrically so the plot
/// still has a nonzero span to fit, the same degenerate case
/// `views::time::pad_if_degenerate` handles for the time-domain view.
fn axis_bounds(ticks: &[i128], time: &TimeAxis) -> Option<(f64, f64)> {
    let (&first, &last) = ticks.first().zip(ticks.last())?;
    let min = tick_to_seconds(time, first);
    let max = tick_to_seconds(time, last);
    if (max - min).abs() > f64::EPSILON {
        Some((min, max))
    } else {
        Some((min - 1.0, max + 1.0))
    }
}

/// Builds `bool_bands` for [`show`]: `dataset.columns`-parallel, `Some` with
/// the column's [`BoolBand`]s for a `bool` column, `None` for every other
/// dtype. Callers compute this once per status change, mirroring
/// `views::time::cache_column_samples` — see its own doc comment for why a
/// per-frame call here would reintroduce issue #80's per-frame-O(n) mistake.
pub fn cache_bool_bands(dataset: &Dataset, ticks: &[i128]) -> Vec<Option<Vec<BoolBand>>> {
    dataset
        .columns
        .iter()
        .map(|series| match series.values() {
            SeriesValues::Bool(values) => Some(bool_state_bands(values, ticks)),
            SeriesValues::Spilled(SpilledValues::Bool(values)) => {
                Some(bool_bands_from_spilled(values, ticks))
            }
            _ => None,
        })
        .collect()
}

/// [`cache_bool_bands`]'s spilled-column path: a spilled `bool` column is
/// memory-mapped `u8` on disk (`!= 0` read back, the same convention
/// `SpilledValues::eq_in_memory` already uses), and it is spilled — over the
/// memory budget — precisely because it did not fit in memory (SPEC §5.1),
/// so this reads it back through [`SpillVec::read_chunks`]'s fixed-size
/// buffer and feeds each sample straight into a [`BoolBandBuilder`], the
/// same "read in bounded chunks" discipline SPEC §5.1 requires of the
/// original source file — never collecting the column into an owned
/// `Vec<bool>` first, which would materialize the very data being spilled
/// to avoid making resident (found on this PR's own review: freezing or
/// crashing on a large file is "the single most serious class of bug in
/// this product", CLAUDE.md).
fn bool_bands_from_spilled(values: &SpillVec<u8>, ticks: &[i128]) -> Vec<BoolBand> {
    let len = values.len().min(ticks.len());
    let mut builder = BoolBandBuilder::new();
    let mut index = 0usize;
    let result = values.read_chunks(0..len, &mut |chunk: &[u8]| {
        for &byte in chunk {
            builder.push(byte != 0, ticks[index]);
            index += 1;
        }
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!(
            %error,
            "failed to read a spilled bool column back for its state-timeline \
             lane; showing whatever bands were read before the error"
        );
    }
    builder.finish()
}

/// Builds a minimal-but-real dataset with a `bool` column and runs [`show`]
/// through a headless `egui::Context` (docs/ROADMAP.md M6, proven by
/// "corpus 47 + manual" — this is the same headless-render crash-free proof
/// `views::time`'s own `render_tests` module uses, see its doc comment for
/// why `egui::Context::run` alone is enough with no GPU/display server).
#[cfg(test)]
mod render_tests {
    use super::*;
    use glyde_core::series::Series;
    use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};

    fn sample_dataset() -> Dataset {
        Dataset {
            time: TimeAxis::Absolute {
                timestamps: vec![
                    Timestamp::new(0, TimeUnit::Seconds),
                    Timestamp::new(1, TimeUnit::Seconds),
                    Timestamp::new(2, TimeUnit::Seconds),
                    Timestamp::new(3, TimeUnit::Seconds),
                ]
                .into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![
                Series::new("flag", SeriesValues::Bool(vec![true, false, true, false])),
                Series::new("value", SeriesValues::F64(vec![1.0, 2.0, 1.5, 3.0])),
            ],
        }
    }

    #[test]
    fn show_renders_a_bool_column_without_panicking() {
        let dataset = sample_dataset();
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let bool_bands = cache_bool_bands(&dataset, &ticks);
        assert!(
            bool_bands[0].is_some(),
            "the bool column must get real bands"
        );
        assert!(
            bool_bands[1].is_none(),
            "the f64 column must not be treated as a state series"
        );

        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &ticks, &bool_bands);
            });
        });

        assert!(
            !output.shapes.is_empty(),
            "must draw something for a real bool column"
        );
    }

    #[test]
    fn show_renders_a_dataset_with_no_bool_columns_without_panicking() {
        let dataset = Dataset {
            columns: vec![Series::new("value", SeriesValues::F64(vec![1.0, 2.0]))],
            ..sample_dataset()
        };
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let bool_bands = cache_bool_bands(&dataset, &ticks);

        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &ticks, &bool_bands);
            });
        });

        // No lane is drawn, but the surrounding UI (the `CentralPanel`
        // itself) still must not panic — same crash-free bar
        // `views::time::show_renders_an_empty_dataset_without_panicking`
        // holds its own module to.
        let _ = output;
    }

    #[test]
    fn show_renders_a_single_sample_bool_column_without_panicking() {
        let dataset = Dataset {
            time: TimeAxis::Absolute {
                timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![Series::new("flag", SeriesValues::Bool(vec![true]))],
        };
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let bool_bands = cache_bool_bands(&dataset, &ticks);

        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &ticks, &bool_bands);
            });
        });

        assert!(
            !output.shapes.is_empty(),
            "SPEC §1.4: a single-sample series is a valid input and must render"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};

    #[test]
    fn cache_bool_bands_is_some_only_for_bool_columns() {
        let dataset = Dataset {
            time: TimeAxis::Absolute {
                timestamps: vec![
                    Timestamp::new(0, TimeUnit::Seconds),
                    Timestamp::new(1, TimeUnit::Seconds),
                ]
                .into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![
                glyde_core::series::Series::new("flag", SeriesValues::Bool(vec![true, false])),
                glyde_core::series::Series::new("value", SeriesValues::F64(vec![1.0, 2.0])),
            ],
        };
        let ticks = dataset.time.to_pyramid_ticks().into_owned();

        let bands = cache_bool_bands(&dataset, &ticks);

        assert_eq!(bands[0], Some(bool_state_bands(&[true, false], &ticks)));
        assert_eq!(bands[1], None);
    }

    #[test]
    fn axis_bounds_of_an_empty_tick_slice_is_none() {
        let time = TimeAxis::Progressive {
            values: vec![].into(),
        };

        assert_eq!(axis_bounds(&[], &time), None);
    }

    #[test]
    fn axis_bounds_pads_a_single_sample_axis_symmetrically() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(5, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };

        assert_eq!(axis_bounds(&[5], &time), Some((4.0, 6.0)));
    }

    #[test]
    fn axis_bounds_spans_the_first_to_last_tick() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Nanoseconds)].into(),
            format: TimestampFormat::EpochNanos,
        };

        assert_eq!(
            axis_bounds(&[0, 1_000_000_000, 3_000_000_000], &time),
            Some((0.0, 3.0))
        );
    }

    // Found on this PR's own review: `cache_bool_bands`'s spilled-column arm
    // used to collect the whole mmap-backed column into an owned `Vec<bool>`
    // before calling `bool_state_bands`, defeating SPEC §5.1's "read in
    // bounded chunks" on the one dtype path that only runs when a column
    // didn't fit the memory budget. This proves the real fix
    // (`bool_bands_from_spilled`, reading back through
    // `SpillVec::read_chunks`) against a genuine on-disk spilled column, not
    // just the in-memory `BoolBandBuilder` unit tests already covering the
    // builder itself — same expected bands `bool_state_bands` gives the
    // in-memory equivalent.
    #[test]
    fn cache_bool_bands_reads_a_spilled_bool_column_through_bounded_chunks() {
        use glyde_core::index::spill::SpillVecWriter;
        use glyde_core::series::SpilledValues;

        let dir = tempfile::tempdir().expect("temp dir");
        let values = [true, true, false, false, false, true, false];
        let mut writer = SpillVecWriter::<u8>::create(dir.path(), "flag").expect("create");
        for &value in &values {
            writer.push(value as u8).expect("push");
        }
        let spilled = writer.finish().expect("finish");

        let ticks: Vec<i128> = (0..values.len() as i128).collect();
        let dataset = Dataset {
            time: TimeAxis::Progressive {
                values: ticks.iter().map(|&t| t as f64).collect::<Vec<_>>().into(),
            },
            time_column_name: "index".to_string(),
            columns: vec![glyde_core::series::Series::new(
                "flag",
                SeriesValues::Spilled(SpilledValues::Bool(spilled)),
            )],
        };

        let bands = cache_bool_bands(&dataset, &ticks);

        assert_eq!(bands[0], Some(bool_state_bands(&values, &ticks)));
    }
}
