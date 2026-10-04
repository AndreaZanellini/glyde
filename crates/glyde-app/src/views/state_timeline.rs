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
//! `string` columns get the same treatment as labeled bands
//! ([`glyde_core::series::string_state_bands`]): the label is printed inside
//! a band wide enough to hold it and shown as a hover tooltip otherwise.
//!
//! Deliberately out of scope here — remaining M6 roadmap items, not yet
//! built:
//! - Markers (single-sample events on their own lane).
//! - Zooming into a mixed-state interval to recover its individual runs.
//!   Dense files use a bounded overview that visibly marks mixed intervals;
//!   interactive expansion is still a separate M6 item.
//! - A single pannable/zoomable axis shared with [`super::time`]'s plot.
//!   Each lane here is a fixed, always-fit-to-data, non-interactive view of
//!   the *whole* file instead — panning/zooming the time-domain view above
//!   does not (yet) move these bands with it.
use egui_plot::{Plot, PlotBounds, PlotPoint, PlotPoints, Polygon, Text, VLine};
use glyde_core::index::spill::SpillVec;
use glyde_core::ingest::{Dataset, TimeAxis};
use glyde_core::series::{
    state_label_id, BoolBand, BoolBandBuilder, BoolLane, SeriesValues, SpilledValues, StateBand,
    StateBandBuilder, StateCell, StateLane,
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
const MIXED_COLOR: egui::Color32 = egui::Color32::from_rgb(205, 130, 45);

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
    bool_bands: &[Option<BoolLane>],
    state_lanes: &[Option<StateLane>],
) {
    let time = &dataset.time;
    let bounds = bool_bands
        .iter()
        .flatten()
        .next()
        .map(BoolLane::tick_bounds)
        .or_else(|| {
            state_lanes
                .iter()
                .flatten()
                .next()
                .map(StateLane::tick_bounds)
        });
    let Some((min_tick, max_tick)) = bounds else {
        return;
    };
    let (axis_min, axis_max) = axis_bounds(min_tick, max_tick, time);

    for (index, series) in dataset.columns.iter().enumerate() {
        if let Some(Some(lane)) = state_lanes.get(index) {
            ui.label(series.name());
            show_state_lane(ui, index, lane, ticks, time, (axis_min, axis_max));
            continue;
        }
        let Some(Some(bands)) = bool_bands.get(index) else {
            continue;
        };

        ui.label(series.name());
        if matches!(bands, BoolLane::Overview { .. }) {
            ui.label("Multiple on/off states share some intervals at this scale");
        }
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
                match bands {
                    BoolLane::Exact { bands, .. } => {
                        for (index, band) in bands.iter().enumerate() {
                            draw_band(plot_ui, time, band, index + 1 == bands.len(), axis_max);
                        }
                        // A state that changes again at the same timestamp has
                        // zero duration. Draw it above the filled bands so the
                        // change remains visible without inventing a duration.
                        for band in bands.iter().take(bands.len().saturating_sub(1)) {
                            if band.start_tick == band.end_tick {
                                plot_ui.vline(
                                    VLine::new(tick_to_seconds(time, band.start_tick))
                                        .color(MIXED_COLOR)
                                        .width(2.0_f32),
                                );
                            }
                        }
                    }
                    BoolLane::Overview { cells, .. } => {
                        draw_overview(plot_ui, cells, axis_min, axis_max, min_tick == max_tick);
                    }
                }
            });
    }
}

/// Merge adjacent equal cells so rendering stays bounded even when the
/// original column alternates on every sample. A mixed cell is conspicuous
/// rather than dropping a short state that falls below one screen pixel.
fn draw_overview(
    plot_ui: &mut egui_plot::PlotUi,
    cells: &[u8],
    min: f64,
    max: f64,
    one_tick: bool,
) {
    let mut start = 0;
    while start < cells.len() {
        let mask = cells[start];
        let mut end = start + 1;
        while end < cells.len() && cells[end] == mask {
            end += 1;
        }
        if mask != 0 {
            let x0 = if one_tick {
                min
            } else {
                min + (max - min) * start as f64 / cells.len() as f64
            };
            let x1 = if one_tick {
                max
            } else {
                min + (max - min) * end as f64 / cells.len() as f64
            };
            let color = match mask {
                1 => OFF_COLOR,
                2 => ON_COLOR,
                _ => MIXED_COLOR,
            };
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
        start = end;
    }
}

/// Draws one [`BoolBand`] as a filled rectangle spanning its own vertical
/// lane (`y` in `[0, 1]`, an arbitrary unit range — this view has no
/// numeric axis, SPEC §4.3). Only the final still-open run is extended
/// visually to `axis_max`; an earlier run may also have equal start/end ticks
/// when two state changes share a timestamp.
fn draw_band(
    plot_ui: &mut egui_plot::PlotUi,
    time: &TimeAxis,
    band: &BoolBand,
    is_last: bool,
    axis_max: f64,
) {
    let x0 = tick_to_seconds(time, band.start_tick);
    let x1 = band_end_seconds(time, band, is_last, axis_max);
    if x0 == x1 {
        return;
    }
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

fn band_end_seconds(time: &TimeAxis, band: &BoolBand, is_last: bool, axis_max: f64) -> f64 {
    if is_last {
        axis_max
    } else {
        tick_to_seconds(time, band.end_tick)
    }
}

/// The full-file x-axis bounds (in plot seconds, [`tick_to_seconds`]'s
/// coordinate space) every lane always fits itself to. A single-sample file (SPEC §1.4: a valid
/// input) has a zero-width range; it is padded symmetrically so the plot
/// still has a nonzero span to fit, the same degenerate case
/// `views::time::pad_if_degenerate` handles for the time-domain view.
fn axis_bounds(min_tick: i128, max_tick: i128, time: &TimeAxis) -> (f64, f64) {
    let min = tick_to_seconds(time, min_tick);
    let max = tick_to_seconds(time, max_tick);
    if (max - min).abs() > f64::EPSILON {
        (min, max)
    } else {
        (min - 1.0, max + 1.0)
    }
}

/// Builds `bool_bands` for [`show`]: `dataset.columns`-parallel, `Some` with
/// a bounded [`BoolLane`] for a `bool` column, `None` for every other dtype.
/// The completed file is prepared on the indexer thread; progressive updates
/// inspect only a capped preview. Rendering never scans the source column.
pub fn cache_bool_bands(dataset: &Dataset, ticks: &[i128]) -> Vec<Option<BoolLane>> {
    if !dataset.columns.iter().any(|series| {
        matches!(
            series.values(),
            SeriesValues::Bool(_) | SeriesValues::Spilled(SpilledValues::Bool(_))
        )
    }) {
        return vec![None; dataset.columns.len()];
    }
    let Some((&min_tick, &max_tick)) = ticks.iter().min().zip(ticks.iter().max()) else {
        return vec![None; dataset.columns.len()];
    };
    dataset
        .columns
        .iter()
        .map(|series| match series.values() {
            SeriesValues::Bool(values) => {
                let mut builder = BoolBandBuilder::for_view(min_tick, max_tick);
                for (&value, &tick) in values.iter().zip(ticks) {
                    builder.push(value, tick);
                }
                Some(builder.finish_lane())
            }
            SeriesValues::Spilled(SpilledValues::Bool(values)) => {
                Some(bool_bands_from_spilled(values, ticks, min_tick, max_tick))
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
fn bool_bands_from_spilled(
    values: &SpillVec<u8>,
    ticks: &[i128],
    min_tick: i128,
    max_tick: i128,
) -> BoolLane {
    let len = values.len().min(ticks.len());
    let mut builder = BoolBandBuilder::for_view(min_tick, max_tick);
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
    builder.finish_lane()
}

/// Estimated width, in points, of one label glyph — a band narrower than
/// `label.chars().count() * LABEL_CHAR_WIDTH` shows its label as a tooltip
/// instead (SPEC §4.3: "label when wide enough, tooltip otherwise").
const LABEL_CHAR_WIDTH: f64 = 7.0;
const LABEL_PADDING: f64 = 6.0;

/// A stable, readable fill for `label`, so the same state is the same color
/// everywhere and across files.
fn label_color(label_id: u32) -> egui::Color32 {
    let hue = (f64::from(label_id) * 0.618_033_988_75).fract() as f32;
    egui::ecolor::Hsva::new(hue, 0.55, 0.55, 1.0).into()
}

fn show_state_lane(
    ui: &mut egui::Ui,
    index: usize,
    lane: &StateLane,
    ticks: &[i128],
    time: &TimeAxis,
    (axis_min, axis_max): (f64, f64),
) {
    if let StateLane::Overview { .. } = lane {
        ui.label("Many state changes: intervals holding several states are drawn as one block");
    }
    let mut hover_x = None;
    let mut width_per_second = 1.0;
    let response = Plot::new(format!("state_timeline_{index}"))
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
            hover_x = plot_ui.pointer_coordinate().map(|point| point.x);
            width_per_second = plot_ui.transform().dpos_dvalue_x().abs().recip();
            match lane {
                StateLane::Exact { bands, .. } => {
                    for (position, band) in bands.iter().enumerate() {
                        let (x0, x1) =
                            state_band_span(time, band, position + 1 == bands.len(), axis_max);
                        if x0 == x1 {
                            continue;
                        }
                        fill_rect(plot_ui, x0, x1, label_color(state_label_id(&band.label)));
                        let points_wide = (x1 - x0) / width_per_second.max(f64::EPSILON);
                        let needed =
                            band.label.chars().count() as f64 * LABEL_CHAR_WIDTH + LABEL_PADDING;
                        if points_wide >= needed {
                            plot_ui.text(
                                Text::new(
                                    PlotPoint::new((x0 + x1) / 2.0, 0.5),
                                    egui::RichText::new(&band.label).color(egui::Color32::WHITE),
                                )
                                .anchor(egui::Align2::CENTER_CENTER),
                            );
                        }
                    }
                }
                StateLane::Overview { cells, labels, .. } => {
                    draw_state_overview(plot_ui, cells, labels, axis_min, axis_max);
                }
            }
        });
    // Tooltip for whatever band the pointer is over, narrow or not.
    if let (Some(x), true) = (hover_x, response.response.hovered()) {
        if let Some(text) = state_hover_text(lane, time, x, axis_max) {
            response.response.on_hover_text(text);
        }
    }
}

fn state_band_span(time: &TimeAxis, band: &StateBand, is_last: bool, axis_max: f64) -> (f64, f64) {
    let x0 = tick_to_seconds(time, band.start_tick);
    let x1 = if is_last {
        axis_max
    } else {
        tick_to_seconds(time, band.end_tick)
    };
    (x0, x1)
}

fn fill_rect(plot_ui: &mut egui_plot::PlotUi, x0: f64, x1: f64, color: egui::Color32) {
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

fn draw_state_overview(
    plot_ui: &mut egui_plot::PlotUi,
    cells: &[StateCell],
    labels: &std::collections::BTreeMap<u32, String>,
    min: f64,
    max: f64,
) {
    let _ = labels;
    let cell_x = |position: usize| min + (max - min) * position as f64 / cells.len() as f64;
    let mut start = 0;
    while start < cells.len() {
        let cell = cells[start];
        let mut end = start + 1;
        while end < cells.len() && cells[end] == cell {
            end += 1;
        }
        let color = match cell {
            StateCell::Empty => None,
            StateCell::Single(id) => Some(label_color(id)),
            StateCell::Mixed => Some(MIXED_COLOR),
        };
        if let Some(color) = color {
            fill_rect(plot_ui, cell_x(start), cell_x(end), color);
        }
        start = end;
    }
}

/// What the tooltip says at plot-x `x` (seconds): the band's label and the
/// interval it holds, or, in an overview lane, the cell's label / a mixed
/// notice.
fn state_hover_text(lane: &StateLane, time: &TimeAxis, x: f64, axis_max: f64) -> Option<String> {
    match lane {
        StateLane::Exact { bands, .. } => bands.iter().enumerate().find_map(|(position, band)| {
            let (x0, x1) = state_band_span(time, band, position + 1 == bands.len(), axis_max);
            (x >= x0 && x <= x1).then(|| format!("{:?}", band.label))
        }),
        StateLane::Overview {
            cells,
            labels,
            min_tick,
            max_tick,
        } => {
            let (min, max) = (
                tick_to_seconds(time, *min_tick),
                tick_to_seconds(time, *max_tick),
            );
            if max <= min || x < min || x > max {
                return None;
            }
            let position = (((x - min) / (max - min)) * cells.len() as f64) as usize;
            match cells.get(position.min(cells.len() - 1))? {
                StateCell::Empty => None,
                StateCell::Mixed => Some("several different states in this interval".to_owned()),
                StateCell::Single(id) => Some(
                    labels
                        .get(id)
                        .map_or_else(|| "(label not retained)".to_owned(), |l| format!("{l:?}")),
                ),
            }
        }
    }
}

/// Builds `state_lanes` for [`show`]: `dataset.columns`-parallel, `Some` for
/// a `string` column, `None` otherwise. Same once-per-status-change,
/// off-the-UI-thread contract as [`cache_bool_bands`].
pub fn cache_state_lanes(dataset: &Dataset, ticks: &[i128]) -> Vec<Option<StateLane>> {
    let is_string = |series: &glyde_core::series::Series| {
        matches!(
            series.values(),
            SeriesValues::String(_) | SeriesValues::Spilled(SpilledValues::String(_))
        )
    };
    let Some((&min_tick, &max_tick)) = ticks.iter().min().zip(ticks.iter().max()) else {
        return vec![None; dataset.columns.len()];
    };
    dataset
        .columns
        .iter()
        .map(|series| {
            if !is_string(series) {
                return None;
            }
            let mut builder = StateBandBuilder::for_view(min_tick, max_tick);
            match series.values() {
                SeriesValues::String(values) => {
                    for (label, &tick) in values.iter().zip(ticks) {
                        builder.push(label, tick);
                    }
                }
                SeriesValues::Spilled(SpilledValues::String(values)) => {
                    // `iter` borrows straight from the mapped arena — one
                    // field at a time, never the whole column resident.
                    for (label, &tick) in values.iter().zip(ticks) {
                        builder.push(label, tick);
                    }
                }
                _ => return None,
            }
            Some(builder.finish_lane())
        })
        .collect()
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
    fn frequent_state_changes_render_a_bounded_number_of_shapes() {
        let len = glyde_core::series::MAX_EXACT_BOOL_BANDS * 3;
        let dataset = Dataset {
            time: TimeAxis::Progressive {
                values: (0..len)
                    .map(|index| index as f64)
                    .collect::<Vec<_>>()
                    .into(),
            },
            time_column_name: "index".to_string(),
            columns: vec![Series::new(
                "flag",
                SeriesValues::Bool((0..len).map(|index| index % 2 == 0).collect()),
            )],
        };
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let lanes = cache_bool_bands(&dataset, &ticks);
        assert!(matches!(lanes[0], Some(BoolLane::Overview { .. })));

        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &ticks, &lanes, &[]);
            });
        });
        assert!(
            output.shapes.len() < 3000,
            "the lane must not draw every run: {} shapes",
            output.shapes.len()
        );
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
                show(ui, &dataset, &ticks, &bool_bands, &[]);
            });
        });

        assert!(
            !output.shapes.is_empty(),
            "must draw something for a real bool column"
        );
    }

    #[test]
    fn show_renders_a_string_column_as_labeled_bands() {
        let dataset = Dataset {
            columns: vec![Series::new(
                "state",
                SeriesValues::String(
                    ["idle", "idle", "running", "fault"]
                        .map(str::to_owned)
                        .to_vec(),
                ),
            )],
            ..sample_dataset()
        };
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let lanes = cache_state_lanes(&dataset, &ticks);
        let Some(StateLane::Exact { bands, .. }) = &lanes[0] else {
            panic!("a string column must get an exact lane: {lanes:?}");
        };
        assert_eq!(bands.len(), 3);

        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &ticks, &[], &lanes);
            });
        });
        assert!(!output.shapes.is_empty());
    }

    #[test]
    fn non_string_columns_get_no_state_lane() {
        let dataset = sample_dataset();
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        assert!(cache_state_lanes(&dataset, &ticks)
            .iter()
            .all(Option::is_none));
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
                show(ui, &dataset, &ticks, &bool_bands, &[]);
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
                show(ui, &dataset, &ticks, &bool_bands, &[]);
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
    use glyde_core::series::bool_state_bands;
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

        assert_eq!(
            bands[0],
            Some(BoolLane::Exact {
                bands: bool_state_bands(&[true, false], &ticks),
                min_tick: 0,
                max_tick: 1,
            })
        );
        assert_eq!(bands[1], None);
    }

    #[test]
    fn axis_bounds_pads_a_single_sample_axis_symmetrically() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(5, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };

        assert_eq!(axis_bounds(5, 5, &time), (4.0, 6.0));
    }

    #[test]
    fn axis_bounds_spans_the_first_to_last_tick() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Nanoseconds)].into(),
            format: TimestampFormat::EpochNanos,
        };

        assert_eq!(axis_bounds(0, 3_000_000_000, &time), (0.0, 3.0));
    }

    #[test]
    fn duplicate_timestamp_does_not_extend_a_closed_run() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };
        let bands = bool_state_bands(&[false, true, false], &[0, 0, 1]);
        assert_eq!(bands.len(), 3);
        assert_eq!(band_end_seconds(&time, &bands[0], false, 10.0), 0.0);
        assert_eq!(band_end_seconds(&time, &bands[1], false, 10.0), 1.0);
        assert_eq!(band_end_seconds(&time, &bands[2], true, 10.0), 10.0);
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
        use glyde_core::index::spill::{SpillSet, SpillVecWriter};
        use glyde_core::series::SpilledValues;

        let dir = tempfile::tempdir().expect("temp dir");
        let values = [true, true, false, false, false, true, false];
        let mut writer =
            SpillVecWriter::<u8>::create(&SpillSet::create(dir.path()).expect("set"), "flag")
                .expect("create");
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

        assert_eq!(
            bands[0],
            Some(BoolLane::Exact {
                bands: bool_state_bands(&values, &ticks),
                min_tick: 0,
                max_tick: values.len() as i128 - 1,
            })
        );
    }
}
