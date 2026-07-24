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

//! Time-domain view v1 (docs/ROADMAP.md M2, SPEC §4.1): renders every raw
//! numeric sample of a [`Dataset`] as a line-plus-points plot with
//! pan/zoom/box-zoom (via `egui_plot`, see the workspace `Cargo.toml`
//! dependency comment), a "Fit to data" button, and a cursor readout of the
//! exact raw value and timestamp at the nearest sample to the pointer.
//!
//! This is deliberately the "small files, pre-pyramid" half of SPEC §3.1:
//! every raw sample is plotted directly (never averaged or LTTB-resampled —
//! forbidden by SPEC §3.1 for the time-domain view), with no min/max
//! decimation pyramid behind it yet (docs/ROADMAP.md M3). [`nearest_index`]
//! is therefore a plain linear scan, bounded to the same "small file" sample
//! counts this view already assumes; M3's pyramid is what makes an
//! arbitrary-size file's viewport query fast, not this module.
//!
//! Non-numeric columns (`bool`/`string`, SPEC §1.4) are not drawn here —
//! they route to the state timeline (SPEC §4.3, docs/ROADMAP.md M6), not
//! yet built.

use egui_plot::{Legend, Line, Plot, PlotBounds, PlotPoints, Points};
use glyde_core::ingest::{Dataset, TimeAxis};
use glyde_core::series::{Series, SeriesValues, ViewKind};
use glyde_core::time::format_timestamp;

/// Renders `dataset`'s numeric columns as a time-domain plot (SPEC §4.1)
/// into `ui`: pan (drag), zoom (scroll wheel and box-select), a "Fit to
/// data" button, and — while the pointer hovers the plot — a readout row
/// below it showing the exact raw value of every plotted series and the
/// timestamp at the nearest sample.
pub fn show(ui: &mut egui::Ui, dataset: &Dataset) {
    let x = x_axis_seconds(&dataset.time);
    let fit_clicked = ui.button("Fit to data").clicked();

    let plot = Plot::new("time_domain_view")
        .legend(Legend::default())
        .allow_zoom(true)
        .allow_scroll(true)
        .allow_drag(true)
        .allow_boxed_zoom(true);

    let response = plot.show(ui, |plot_ui| {
        if fit_clicked {
            if let Some(bounds) = data_bounds(&x, &dataset.columns) {
                plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                    [bounds.x_min, bounds.y_min],
                    [bounds.x_max, bounds.y_max],
                ));
            }
        }

        for series in &dataset.columns {
            if series.view_kind() != ViewKind::TimeDomain {
                continue;
            }
            let points = series_points(&x, series.values());
            plot_ui.line(Line::new(PlotPoints::new(points.clone())).name(series.name()));
            // SPEC §3.1: "when the visible range contains fewer samples than
            // pixels, draw the raw samples with visible point markers ... the
            // user must be able to reach the individual sample." There is no
            // decimation pyramid yet (M3), so every raw sample is always in
            // that regime for now — the markers are drawn unconditionally.
            plot_ui.points(
                Points::new(PlotPoints::new(points))
                    .name(series.name())
                    .radius(2.0_f32),
            );
        }

        plot_ui
            .pointer_coordinate()
            .and_then(|pointer| nearest_index(&x, pointer.x))
    });

    if let Some(index) = response.inner {
        ui.horizontal(|ui| {
            ui.label(format_cursor_time(&dataset.time, index));
            for series in &dataset.columns {
                if series.view_kind() != ViewKind::TimeDomain {
                    continue;
                }
                if let Some(text) = format_series_value(series.values(), index) {
                    ui.label(format!("{}: {text}", series.name()));
                }
            }
        });
    }
}

/// `time`'s samples as plain `f64` x-coordinates for `egui_plot`, which only
/// ever plots `f64` points. For [`TimeAxis::Absolute`] this converts each
/// [`glyde_core::time::Timestamp`]'s integer ticks to seconds-since-epoch —
/// a rendering-only coordinate transform for pixel placement, not a change
/// to how time is stored (SPEC §2.1's "never store absolute time as `f64`
/// seconds" governs [`Dataset`]'s own fields, which stay `i128` ticks; nothing
/// here mutates them).
fn x_axis_seconds(time: &TimeAxis) -> Vec<f64> {
    match time {
        TimeAxis::Absolute { timestamps, .. } => timestamps
            .iter()
            .map(|timestamp| timestamp.ticks as f64 / timestamp.unit.ticks_per_second() as f64)
            .collect(),
        TimeAxis::Progressive { values } => values.clone(),
    }
}

/// The `[x, y]` points `egui_plot` draws for one series: `x` paired with
/// every raw sample of `values` that has a plottable `f64` reading (SPEC
/// §1.4: NaN/missing values are preserved as gaps, never interpolated — a
/// sample with no numeric reading is simply omitted from the line here
/// rather than connected across, though f64 NaN values still round-trip
/// through since `f64::partial_cmp`-based plotting already breaks the line
/// at them).
fn series_points(x: &[f64], values: &SeriesValues) -> Vec<[f64; 2]> {
    x.iter()
        .enumerate()
        .filter_map(|(index, &xi)| value_as_f64(values, index).map(|y| [xi, y]))
        .collect()
}

/// The axis-aligned bounding box of `x` and every plottable value across
/// `columns`' numeric series — what "Fit to data" resets the plot's view to.
/// `None` when there is nothing plottable at all (e.g. every column is
/// non-numeric).
#[derive(Debug, Clone, Copy, PartialEq)]
struct DataBounds {
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
}

fn data_bounds(x: &[f64], columns: &[Series]) -> Option<DataBounds> {
    let (x_min, x_max) = finite_min_max(x.iter().copied())?;

    let y_values = columns
        .iter()
        .filter(|series| series.view_kind() == ViewKind::TimeDomain)
        .flat_map(|series| {
            (0..series.len()).filter_map(|index| value_as_f64(series.values(), index))
        });
    let (y_min, y_max) = finite_min_max(y_values)?;

    let (x_min, x_max) = pad_if_degenerate(x_min, x_max);
    let (y_min, y_max) = pad_if_degenerate(y_min, y_max);

    Some(DataBounds {
        x_min,
        x_max,
        y_min,
        y_max,
    })
}

/// The min and max of every finite value in `values`, ignoring NaN/infinite
/// readings (SPEC §1.3: they are flagged anomalies, not fit targets) —
/// `None` if nothing finite was seen at all.
fn finite_min_max(values: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for value in values.filter(|v| v.is_finite()) {
        min = min.min(value);
        max = max.max(value);
    }
    (min.is_finite() && max.is_finite()).then_some((min, max))
}

/// A constant or single-sample series (SPEC §1.4: both valid inputs) has
/// zero extent on one or both axes. `egui_plot` needs a nonzero span to fit
/// to, so a degenerate `[min, max]` is padded symmetrically instead of left
/// as a zero-width bound that would zoom in on nothing.
fn pad_if_degenerate(min: f64, max: f64) -> (f64, f64) {
    if (max - min).abs() > f64::EPSILON {
        return (min, max);
    }
    let pad = if min.abs() > f64::EPSILON {
        min.abs() * 0.1
    } else {
        1.0
    };
    (min - pad, max + pad)
}

/// The index of `x`'s entry closest to `target`, or `None` for an empty
/// axis. A plain linear scan (see the module docs: bounded to this view's
/// "small file" scope), correct regardless of whether `x` happens to be
/// sorted — SPEC §2.1 non-monotonic timestamps are preserved, not reordered,
/// so a binary search would silently give the wrong answer on such a file.
fn nearest_index(x: &[f64], target: f64) -> Option<usize> {
    x.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (*a - target)
                .abs()
                .partial_cmp(&(*b - target).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index)
}

/// `values[index]` as `f64` for plotting (SPEC §1.4: integer dtypes promote
/// losslessly for any value within `f64`'s 53-bit mantissa; `docs/ROADMAP.md`
/// M8 owns flagging the rare `i64`/`u64` magnitude that doesn't fit). `bool`
/// and `string` series never reach here — callers only invoke this for
/// [`ViewKind::TimeDomain`] series.
fn value_as_f64(values: &SeriesValues, index: usize) -> Option<f64> {
    match values {
        SeriesValues::I8(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::I16(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::I32(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::I64(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::U8(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::U16(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::U32(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::U64(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::F32(v) => v.get(index).map(|&n| n as f64),
        SeriesValues::F64(v) => v.get(index).copied(),
        SeriesValues::Bool(_) | SeriesValues::String(_) => None,
    }
}

/// The cursor-readout text for `values[index]`, in the value's own native
/// dtype formatting (SPEC §4.1 "exact raw value") rather than the `f64` cast
/// [`value_as_f64`] uses for plotting — an `i64`/`u64` sample keeps its exact
/// integer text here even where the plotted point itself is an `f64`
/// approximation.
fn format_series_value(values: &SeriesValues, index: usize) -> Option<String> {
    match values {
        SeriesValues::I8(v) => v.get(index).map(i8::to_string),
        SeriesValues::I16(v) => v.get(index).map(i16::to_string),
        SeriesValues::I32(v) => v.get(index).map(i32::to_string),
        SeriesValues::I64(v) => v.get(index).map(i64::to_string),
        SeriesValues::U8(v) => v.get(index).map(u8::to_string),
        SeriesValues::U16(v) => v.get(index).map(u16::to_string),
        SeriesValues::U32(v) => v.get(index).map(u32::to_string),
        SeriesValues::U64(v) => v.get(index).map(u64::to_string),
        SeriesValues::F32(v) => v.get(index).map(f32::to_string),
        SeriesValues::F64(v) => v.get(index).map(f64::to_string),
        SeriesValues::Bool(_) | SeriesValues::String(_) => None,
    }
}

/// The cursor-readout text for the timestamp at `index` (SPEC §4.1 "exact
/// ... timestamp"): [`format_timestamp`] round-trips an absolute timestamp
/// in its own detected format (honoring an offset if one was present, SPEC
/// §2.1); a progressive index has no calendar meaning, so its raw numeric
/// value is shown instead.
fn format_cursor_time(time: &TimeAxis, index: usize) -> String {
    match time {
        TimeAxis::Absolute { timestamps, format } => timestamps
            .get(index)
            .map(|timestamp| format_timestamp(timestamp, *format))
            .unwrap_or_default(),
        TimeAxis::Progressive { values } => {
            values.get(index).map(f64::to_string).unwrap_or_default()
        }
    }
}

/// Builds a minimal-but-real dataset and runs [`show`] through a headless
/// `egui::Context` (docs/ROADMAP.md M2 "Time-domain view v1", proven by
/// "manual + headless render snapshot"). `egui::Context::run` needs no GPU
/// or display server — only `eframe`'s windowing shell does — so this
/// exercises the exact widget code path (`Plot::show`, the line/points
/// items, the cursor-readout row) in CI on every OS, including headless
/// Linux runners with no display, which is what a real click-through cannot
/// do here (see the M2 "single egui window" PR's own CHANGELOG note on the
/// same constraint).
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
                ],
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![Series::new("value", SeriesValues::F64(vec![1.0, 2.0, 1.5]))],
        }
    }

    // Crash-free rendering is SPEC §6's "crash-free target"/CLAUDE.md's
    // "single most serious class of bug" — this proves `show` builds a
    // complete, non-empty set of draw commands for a real dataset without
    // panicking, exercising `egui_plot::Plot::show` itself rather than only
    // the pure helper functions below.
    #[test]
    fn show_renders_a_small_dataset_without_panicking() {
        let dataset = sample_dataset();
        let ctx = egui::Context::default();

        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset);
            });
        });

        assert!(
            !output.shapes.is_empty(),
            "must draw something for a non-empty dataset"
        );
        insta::assert_debug_snapshot!("time_domain_view_shape_count", output.shapes.len());
    }

    // An empty dataset (e.g. every row skipped) must render the surrounding
    // UI (the "Fit to data" button, the empty plot frame) without panicking
    // — no samples is not the same failure class as malformed samples, but
    // both must be crash-free.
    #[test]
    fn show_renders_an_empty_dataset_without_panicking() {
        let dataset = Dataset {
            time: TimeAxis::Absolute {
                timestamps: vec![],
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![Series::new("value", SeriesValues::F64(vec![]))],
        };
        let ctx = egui::Context::default();

        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset);
            });
        });

        assert!(
            !output.shapes.is_empty(),
            "the surrounding UI must still draw"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyde_core::series::Series;
    use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};

    #[test]
    fn x_axis_seconds_converts_nanosecond_ticks_to_seconds() {
        let time = TimeAxis::Absolute {
            timestamps: vec![
                Timestamp::new(0, TimeUnit::Nanoseconds),
                Timestamp::new(1_500_000_000, TimeUnit::Nanoseconds),
            ],
            format: TimestampFormat::EpochNanos,
        };

        assert_eq!(x_axis_seconds(&time), vec![0.0, 1.5]);
    }

    #[test]
    fn x_axis_seconds_passes_progressive_values_through_unchanged() {
        let time = TimeAxis::Progressive {
            values: vec![0.0, 1.0, 2.0],
        };

        assert_eq!(x_axis_seconds(&time), vec![0.0, 1.0, 2.0]);
    }

    #[test]
    fn nearest_index_picks_the_closest_sample() {
        let x = vec![0.0, 1.0, 2.0, 3.0];

        assert_eq!(nearest_index(&x, 1.4), Some(1));
        assert_eq!(nearest_index(&x, 1.6), Some(2));
        assert_eq!(nearest_index(&x, -5.0), Some(0));
        assert_eq!(nearest_index(&x, 50.0), Some(3));
    }

    #[test]
    fn nearest_index_of_an_empty_axis_is_none() {
        assert_eq!(nearest_index(&[], 0.0), None);
    }

    // A non-monotonic time axis (SPEC §2.1: preserved, never reordered) must
    // still resolve to whichever sample is numerically closest, proving this
    // is a real scan and not a sortedness-assuming binary search.
    #[test]
    fn nearest_index_is_correct_on_a_non_monotonic_axis() {
        let x = vec![5.0, 1.0, 3.0];

        assert_eq!(nearest_index(&x, 0.9), Some(1));
        assert_eq!(nearest_index(&x, 4.9), Some(0));
    }

    #[test]
    fn value_as_f64_reads_every_numeric_dtype() {
        assert_eq!(value_as_f64(&SeriesValues::I64(vec![42]), 0), Some(42.0));
        assert_eq!(value_as_f64(&SeriesValues::F32(vec![1.5]), 0), Some(1.5));
        assert_eq!(value_as_f64(&SeriesValues::F64(vec![2.5]), 0), Some(2.5));
        assert_eq!(value_as_f64(&SeriesValues::U8(vec![7]), 0), Some(7.0));
    }

    #[test]
    fn value_as_f64_of_bool_or_string_is_none() {
        assert_eq!(value_as_f64(&SeriesValues::Bool(vec![true]), 0), None);
        assert_eq!(
            value_as_f64(&SeriesValues::String(vec!["x".to_string()]), 0),
            None
        );
    }

    #[test]
    fn format_series_value_keeps_exact_integer_text() {
        assert_eq!(
            format_series_value(&SeriesValues::I64(vec![9_007_199_254_740_993]), 0),
            Some("9007199254740993".to_string())
        );
    }

    #[test]
    fn format_cursor_time_round_trips_an_absolute_timestamp_in_its_own_format() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::with_offset(0, TimeUnit::Nanoseconds, 2 * 3600)],
            format: TimestampFormat::Iso8601WithOffset,
        };

        let text = format_cursor_time(&time, 0);

        assert!(text.contains("02:00"), "must honor the offset: {text}");
    }

    #[test]
    fn format_cursor_time_of_a_progressive_index_shows_the_plain_number() {
        let time = TimeAxis::Progressive {
            values: vec![0.0, 1.0, 2.0],
        };

        assert_eq!(format_cursor_time(&time, 1), "1");
    }

    #[test]
    fn data_bounds_covers_every_plotted_series_and_pads_a_constant_series() {
        let x = vec![0.0, 1.0, 2.0];
        let columns = vec![Series::new("value", SeriesValues::F64(vec![5.0, 5.0, 5.0]))];

        let bounds = data_bounds(&x, &columns).expect("must compute bounds");

        assert_eq!(bounds.x_min, 0.0);
        assert_eq!(bounds.x_max, 2.0);
        assert!(
            bounds.y_min < 5.0 && bounds.y_max > 5.0,
            "must pad a zero-extent y range"
        );
    }

    #[test]
    fn data_bounds_ignores_non_numeric_columns() {
        let x = vec![0.0, 1.0];
        let columns = vec![Series::new(
            "state",
            SeriesValues::String(vec!["on".to_string(), "off".to_string()]),
        )];

        assert_eq!(data_bounds(&x, &columns), None);
    }

    #[test]
    fn series_points_skips_samples_with_no_numeric_reading() {
        let x = vec![0.0, 1.0];
        let points = series_points(&x, &SeriesValues::Bool(vec![true, false]));

        assert!(points.is_empty());
    }
}
