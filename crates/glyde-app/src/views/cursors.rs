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

//! SPEC §4.1's measurement cursors (issue #231): two vertical cursors, A
//! and B, across every track of the time view, and the panel that reads
//! `Δt`, `1/Δt` and, per shown numeric channel, the value at A and at B,
//! `ΔY` and `ΔY/Δt` (the approved mockup, docs/design).
//!
//! A cursor is a **sample**, not a position: it is stored as the row it
//! snapped to, so every value the panel shows is that row's raw sample and
//! `Δt` is the exact time between the two rows' own timestamps
//! (`glyde_core::time::TimeSpan`), also on a compressed axis. The
//! arithmetic lives in `glyde_core` (`series::measure_between`,
//! `ingest::TimeAxis::span`); this module only draws it and forwards the
//! drags.

use super::time::{format_cursor_time, ShownColumn};
use glyde_core::ingest::Dataset;
use glyde_core::series::Reading;
use glyde_core::time::{format_frequency, format_significant, AxisSpan};

/// Width of the cursor panel, in points (docs/design: 316 px).
pub const READOUT_WIDTH: f32 = 316.0;

/// How close to a cursor line, in points, a press grabs it.
const GRAB_DISTANCE: f32 = 6.0;

/// One of the two cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    A,
    B,
}

impl Handle {
    fn label(self) -> &'static str {
        match self {
            Handle::A => "A",
            Handle::B => "B",
        }
    }
}

/// The cursors' view state: whether they are on, the rows they snapped
/// to, and where the last frame drew them.
#[derive(Debug, Default)]
pub struct MeasurementCursors {
    /// The rows under A and B; `None` while the cursors are off.
    rows: Option<(usize, usize)>,
    /// The cursor being dragged, if any.
    drag: Option<Handle>,
    /// Where the last frame drew them, for hit-testing and painting.
    screen: Option<CursorScreen>,
}

/// The cursors on screen this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CursorScreen {
    /// Screen x of A and B; `None` when out of the view.
    pub a: Option<f32>,
    pub b: Option<f32>,
    /// The numeric tracks' area: lines start at its top, flags sit on it.
    pub area: egui::Rect,
}

impl MeasurementCursors {
    /// Whether the cursors are on.
    pub fn enabled(&self) -> bool {
        self.rows.is_some()
    }

    /// The rows under A and B, while on.
    pub fn rows(&self) -> Option<(usize, usize)> {
        self.rows
    }

    /// Turns them on at `rows` (already snapped), or off.
    pub fn set(&mut self, rows: Option<(usize, usize)>) {
        self.rows = rows;
        self.drag = None;
        if rows.is_none() {
            self.screen = None;
        }
    }

    /// Forgets rows that no longer exist (a shorter reading of the file).
    pub(crate) fn retain_within(&mut self, len: usize) {
        if self.rows.is_some_and(|(a, b)| a >= len || b >= len) {
            tracing::info!(len, "measurement cursors removed: their rows are gone");
            self.set(None);
        }
    }

    /// The cursor a press at `pointer` grabs (or the one being dragged):
    /// the nearest line within [`GRAB_DISTANCE`] on last frame's tracks.
    pub(crate) fn grab_at(&self, pointer: Option<egui::Pos2>) -> Option<Handle> {
        if self.drag.is_some() {
            return self.drag;
        }
        let (screen, pointer) = (self.screen?, pointer?);
        if !screen.area.contains(pointer) {
            return None;
        }
        [(Handle::A, screen.a), (Handle::B, screen.b)]
            .into_iter()
            .filter_map(|(handle, x)| Some((handle, (x? - pointer.x).abs())))
            .filter(|&(_, distance)| distance <= GRAB_DISTANCE)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(handle, _)| handle)
    }

    /// Starts, continues or ends a drag of `grabbed` from this frame's
    /// pointer; `snap` turns a screen x into the row nearest it. Returns
    /// whether a cursor is being dragged.
    pub(crate) fn drag(
        &mut self,
        ctx: &egui::Context,
        grabbed: Option<Handle>,
        snap: impl Fn(f32) -> Option<usize>,
    ) -> bool {
        let (pressed, down, pointer) = ctx.input(|input| {
            (
                input.pointer.primary_pressed(),
                input.pointer.primary_down(),
                input.pointer.interact_pos(),
            )
        });
        if self.drag.is_none() && pressed {
            self.drag = grabbed;
        }
        let Some(handle) = self.drag else {
            return false;
        };
        if let (Some(pointer), Some((a, b))) = (pointer, self.rows.as_mut()) {
            if let Some(row) = snap(pointer.x) {
                match handle {
                    Handle::A => *a = row,
                    Handle::B => *b = row,
                }
            }
        }
        if !down {
            self.drag = None;
            if let Some((a, b)) = self.rows {
                tracing::info!(
                    cursor = handle.label(),
                    row_a = a,
                    row_b = b,
                    "measurement cursor snapped to the nearest sample (SPEC §4.1)"
                );
            }
        }
        ctx.request_repaint();
        true
    }

    pub(crate) fn set_screen(&mut self, screen: Option<CursorScreen>) {
        self.screen = screen;
    }

    #[cfg(test)]
    pub(crate) fn screen(&self) -> Option<CursorScreen> {
        self.screen
    }
}

/// Paints the two cursor lines from the top of the numeric tracks down to
/// `bottom` (the time axis), across every state lane in between, with
/// their A/B flags and, when there is room, `Δt` between them.
pub(crate) fn paint(ui: &egui::Ui, cursors: &MeasurementCursors, dataset: &Dataset, bottom: f32) {
    let Some(screen) = cursors.screen else {
        return;
    };
    let palette = crate::theme::palette(ui.visuals());
    let painter = ui.painter();
    let top = screen.area.top();
    let flag_font = egui::FontId::proportional(11.0);
    for (handle, x) in [(Handle::A, screen.a), (Handle::B, screen.b)] {
        let Some(x) = x else { continue };
        let x = x.round() + 0.5;
        painter.vline(x, top..=bottom, egui::Stroke::new(1.0_f32, palette.ink));
        let flag = egui::Rect::from_center_size(egui::pos2(x, top + 8.0), egui::vec2(18.0, 16.0));
        painter.rect_filled(flag, 3.0, palette.ink);
        painter.text(
            flag.center(),
            egui::Align2::CENTER_CENTER,
            handle.label(),
            flag_font.clone(),
            palette.window,
        );
    }
    if let (Some(a), Some(b), Some((row_a, row_b))) = (screen.a, screen.b, cursors.rows) {
        let (left, right) = (a.min(b), a.max(b));
        if right - left > 120.0 {
            if let Some(span) = dataset.time.span(row_a, row_b) {
                painter.text(
                    egui::pos2((left + right) / 2.0, top + 8.0),
                    egui::Align2::CENTER_CENTER,
                    format!("{} {}", delta_name(&span), span_text(&span)),
                    egui::TextStyle::Monospace.resolve(ui.style()),
                    palette.ink_2,
                );
            }
        }
    }
}

/// `Δt` on a time axis, `Δx` on a progressive index, which has no unit.
fn delta_name(span: &AxisSpan) -> &'static str {
    match span {
        AxisSpan::Time(_) => "Δt",
        AxisSpan::Progressive(_) => "Δx",
    }
}

fn span_text(span: &AxisSpan) -> String {
    match span {
        AxisSpan::Time(span) => span.to_string(),
        // The index's own numbers, exactly (shortest round-trip text).
        AxisSpan::Progressive(difference) => difference.to_string(),
    }
}

fn reciprocal_text(span: &AxisSpan) -> String {
    match span {
        AxisSpan::Time(span) => span
            .frequency_hz()
            .map_or_else(|| "—".to_owned(), format_frequency),
        AxisSpan::Progressive(difference) if *difference != 0.0 => {
            format_significant(1.0 / difference.abs())
        }
        AxisSpan::Progressive(_) => "—".to_owned(),
    }
}

fn slope_text(span: &AxisSpan, delta: f64) -> String {
    match span.slope(delta) {
        Some(slope) => {
            let sign = if slope > 0.0 { "+" } else { "" };
            match span {
                AxisSpan::Time(_) => format!("{sign}{} /s", format_significant(slope)),
                AxisSpan::Progressive(_) => format!("{sign}{}", format_significant(slope)),
            }
        }
        None => "—".to_owned(),
    }
}

/// One readout line per value, the text the panel shows (the tests read
/// it): the A/B timestamps, the span and, per channel, its four values.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Readout {
    pub at_a: String,
    pub at_b: String,
    pub span_name: &'static str,
    pub span: String,
    pub reciprocal: String,
    pub channels: Vec<ChannelReadout>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChannelReadout {
    pub color_index: usize,
    pub name: String,
    pub at_a: String,
    pub at_b: String,
    pub delta: String,
    pub slope: String,
}

/// The panel's content for rows `(a, b)` over `shown`, every value read
/// from the raw samples by `glyde_core`.
pub(crate) fn readout(
    dataset: &Dataset,
    (a, b): (usize, usize),
    shown: &[ShownColumn],
) -> Option<Readout> {
    let span = dataset.time.span(a, b)?;
    let reading = |reading: &Reading| match reading {
        Reading::Value(text) => text.clone(),
        Reading::Missing => "missing".to_owned(),
    };
    let channels = shown
        .iter()
        .filter_map(|&(color_index, column)| {
            let series = dataset.columns.get(column)?;
            let measured = glyde_core::series::measure_between(series.values(), a, b)?;
            Some(ChannelReadout {
                color_index,
                name: series.name().to_owned(),
                at_a: reading(&measured.at_from),
                at_b: reading(&measured.at_to),
                delta: measured
                    .delta
                    .as_ref()
                    .map_or_else(|| "—".to_owned(), |delta| delta.text.clone()),
                slope: measured
                    .delta
                    .as_ref()
                    .map_or_else(|| "—".to_owned(), |delta| slope_text(&span, delta.value)),
            })
        })
        .collect();
    Some(Readout {
        at_a: format_cursor_time(&dataset.time, a),
        at_b: format_cursor_time(&dataset.time, b),
        span_name: delta_name(&span),
        span: span_text(&span),
        reciprocal: reciprocal_text(&span),
        channels,
    })
}

/// [`readout`] for the rows `cursors` are on: the per-frame reads the
/// panel does, for the frame-time bench (issue #231).
#[doc(hidden)]
pub fn readout_for_bench(
    dataset: &Dataset,
    cursors: &MeasurementCursors,
    shown: &[ShownColumn],
) -> Option<impl std::fmt::Debug> {
    cursors.rows.and_then(|rows| readout(dataset, rows, shown))
}

/// The cursor panel (docs/design: right of the time view, 316 px):
/// A and B's timestamps, `Δt = B − A` and `1/Δt`, then one block per
/// shown numeric channel.
pub(crate) fn show_readout(
    ui: &mut egui::Ui,
    dataset: &Dataset,
    cursors: &MeasurementCursors,
    shown: &[ShownColumn],
    compressed: bool,
) {
    let palette = crate::theme::palette(ui.visuals());
    let mono = egui::TextStyle::Monospace;
    ui.horizontal(|ui| {
        ui.strong("Cursors");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak("drag A or B");
        });
    });
    let Some(readout) = cursors.rows.and_then(|rows| readout(dataset, rows, shown)) else {
        ui.weak("No sample under the cursors.");
        return;
    };
    let flag = |ui: &mut egui::Ui, label: &str| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 3.0, palette.ink);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(11.0),
            palette.window,
        );
    };
    for (label, at) in [("A", &readout.at_a), ("B", &readout.at_b)] {
        ui.horizontal(|ui| {
            flag(ui, label);
            ui.add(egui::Label::new(egui::RichText::new(at).text_style(mono.clone())).truncate())
                .on_hover_text(format!("{label} is on the sample at {at}"));
        });
    }
    ui.add_space(4.0);
    egui::Frame::group(ui.style())
        .fill(palette.surface)
        .show(ui, |ui| {
            egui::Grid::new("cursor_span")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.weak(format!("{} = B − A", readout.span_name));
                    ui.weak(format!("1/{}", readout.span_name));
                    ui.end_row();
                    ui.label(
                        egui::RichText::new(&readout.span)
                            .text_style(mono.clone())
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new(&readout.reciprocal)
                            .text_style(mono.clone())
                            .strong(),
                    );
                    ui.end_row();
                });
        });
    ui.add_space(4.0);
    let footer = if compressed {
        "Every value is the raw sample nearest the cursor, read from the file. A missing sample \
         stays missing; nothing is interpolated. Δt is the real time between the two samples, \
         also on the compressed axis."
    } else {
        "Every value is the raw sample nearest the cursor, read from the file. A missing sample \
         stays missing; nothing is interpolated."
    };
    egui::TopBottomPanel::bottom("cursor_readout_footer")
        .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 6)))
        .show_inside(ui, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(footer).small().weak()).wrap());
        });
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if readout.channels.is_empty() {
                ui.weak("No numeric channel is shown.");
            }
            for (index, channel) in readout.channels.iter().enumerate() {
                ui.separator();
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        rect,
                        2.0,
                        crate::theme::series_color(ui.visuals(), channel.color_index),
                    );
                    ui.add(egui::Label::new(&channel.name).truncate());
                });
                egui::Grid::new(("cursor_channel", index))
                    .num_columns(4)
                    .show(ui, |ui| {
                        let value = |ui: &mut egui::Ui, text: &str| {
                            let rich = egui::RichText::new(text).text_style(mono.clone());
                            let rich = if text == "missing" || text == "—" {
                                rich.weak()
                            } else {
                                rich
                            };
                            ui.add(egui::Label::new(rich).truncate());
                        };
                        ui.weak("A");
                        value(ui, &channel.at_a);
                        ui.weak("B");
                        value(ui, &channel.at_b);
                        ui.end_row();
                        ui.weak("ΔY");
                        value(ui, &channel.delta);
                        ui.weak(format!("ΔY/{}", readout.span_name));
                        value(ui, &channel.slope);
                        ui.end_row();
                    });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::time::{
        cache_column_samples, paint_cursors, show, CursorLookup, FitBounds, TimeViewState,
    };

    fn corpus_dataset(name: &str) -> Dataset {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/corpus")
            .join(name);
        glyde_core::ingest::load(&path).expect("corpus case must load")
    }

    /// Everything the time view needs, prepared once like the indexer does.
    struct Fixture {
        dataset: Dataset,
        pyramids: Vec<Option<Vec<Vec<glyde_core::dsp::decimation::Bucket>>>>,
        ticks: Vec<i128>,
        sample_cache: Vec<Option<Vec<f64>>>,
        cursor: CursorLookup,
        fit: FitBounds,
    }

    impl Fixture {
        fn new(dataset: Dataset) -> Self {
            let pyramids = glyde_core::ingest::pyramids_for_dataset(&dataset);
            let ticks = dataset.time.to_pyramid_ticks().into_owned();
            let sample_cache = cache_column_samples(&dataset);
            let cursor = CursorLookup::for_time(&dataset.time);
            let fit = FitBounds::for_dataset(
                &dataset,
                &pyramids,
                &ticks,
                &sample_cache,
                &[],
                cursor.is_sorted(),
            );
            Self {
                dataset,
                pyramids,
                ticks,
                sample_cache,
                cursor,
                fit,
            }
        }

        /// One 800×600 frame of the time view and the cursor lines.
        fn frame(
            &self,
            ctx: &egui::Context,
            state: &mut TimeViewState,
            events: Vec<egui::Event>,
        ) -> Vec<egui::epaint::ClippedShape> {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show(
                        ui,
                        &self.dataset,
                        &self.pyramids,
                        &self.ticks,
                        &self.sample_cache,
                        &[],
                        &self.cursor,
                        &self.fit,
                        state,
                    );
                    paint_cursors(ui, &self.dataset, state);
                });
            })
            .shapes
        }
    }

    fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, egui::Pos2)> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                egui::Shape::Text(text) => out.push((
                    text.galley.text().to_owned(),
                    text.pos + text.galley.size() / 2.0,
                )),
                _ => {}
            }
        }
        let mut out = Vec::new();
        shapes
            .iter()
            .for_each(|clipped| walk(&clipped.shape, &mut out));
        out
    }

    fn centre_of(shapes: &[egui::epaint::ClippedShape], label: &str) -> egui::Pos2 {
        texts(shapes)
            .into_iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("no {label:?} on screen"))
            .1
    }

    fn click(at: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// A 1 Hz ramp, `value = 2 × second`, 0..=60 s.
    fn ramp() -> Dataset {
        use glyde_core::series::{Series, SeriesValues};
        use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};
        Dataset {
            time: glyde_core::ingest::TimeAxis::Absolute {
                timestamps: (0..=60)
                    .map(|s| Timestamp::new(1_770_000_000 + s, TimeUnit::Seconds))
                    .collect::<Vec<_>>()
                    .into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_owned(),
            columns: vec![Series::new(
                "ramp",
                SeriesValues::F64((0..=60).map(|s| 2.0 * s as f64).collect()),
            )],
        }
    }

    // Done-when of #231: Δt, 1/Δt, ΔY and slope against hand-computed
    // values. Case 32 rows 1 and 4: 1770000000100000000 ns (10.1) and
    // 1770000000400000000 ns (10.4).
    #[test]
    fn the_readout_of_epoch_nanoseconds_matches_a_hand_calculation() {
        let dataset = corpus_dataset("case-32-epoch-nanoseconds.csv");
        let readout = readout(&dataset, (1, 4), &[(0, 0)]).expect("rows exist");
        assert_eq!(readout.span_name, "Δt");
        assert_eq!(readout.span, "300 ms");
        assert_eq!(readout.reciprocal, "3.33333 Hz");
        assert_eq!(readout.channels.len(), 1);
        let value = &readout.channels[0];
        assert_eq!((value.at_a.as_str(), value.at_b.as_str()), ("10.1", "10.4"));
        assert_eq!(value.delta, "+0.3");
        assert_eq!(value.slope, "+1 /s");
        insta::assert_debug_snapshot!("cursor_readout_epoch_nanoseconds", readout);
    }

    // Case 41: 1 ps apart in 2026, which absolute f64 seconds could not
    // tell apart (#206).
    #[test]
    fn the_readout_of_picosecond_samples_is_exact() {
        let dataset = corpus_dataset("case-41-picosecond-resolution-index.csv");
        let readout = readout(&dataset, (0, 5), &[(0, 0)]).expect("rows exist");
        assert_eq!(readout.span, "5 ps");
        assert_eq!(readout.reciprocal, "200 GHz");
        assert_eq!(readout.channels[0].delta, "+0.5");
        assert_eq!(readout.channels[0].slope, "+1e11 /s");
        assert_ne!(readout.at_a, readout.at_b, "{readout:?}");
    }

    #[test]
    fn a_nan_under_a_cursor_reads_missing_with_no_difference() {
        let dataset = corpus_dataset("case-43-nan-runs.csv");
        let readout = readout(&dataset, (1, 3), &[(0, 0)]).expect("rows exist");
        let value = &readout.channels[0];
        assert_eq!(
            (value.at_a.as_str(), value.at_b.as_str()),
            ("10.1", "missing")
        );
        assert_eq!((value.delta.as_str(), value.slope.as_str()), ("—", "—"));
    }

    #[test]
    fn a_progressive_index_reads_differences_without_a_unit() {
        let dataset = corpus_dataset("case-35-progressive-integer-index.csv");
        let readout = readout(&dataset, (1, 4), &[(0, 0)]).expect("rows exist");
        assert_eq!(readout.span_name, "Δx");
        assert_eq!(readout.span, "3");
        assert_eq!(readout.channels[0].slope, "+0.1");
    }

    #[test]
    fn turning_cursors_on_places_them_at_a_third_and_two_thirds_of_the_view() {
        let fixture = Fixture::new(ramp());
        let ctx = egui::Context::default();
        let mut state = TimeViewState::default();
        let shapes = fixture.frame(&ctx, &mut state, Vec::new());
        fixture.frame(&ctx, &mut state, Vec::new());
        assert!(!state.cursors.enabled(), "off by default");

        let button = centre_of(&shapes, "Cursors");
        fixture.frame(
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(button), click(button, true)],
        );
        fixture.frame(&ctx, &mut state, vec![click(button, false)]);
        let shapes = fixture.frame(&ctx, &mut state, Vec::new());
        // The view is 0..=60 s: a third is 20 s, two thirds 40 s.
        assert_eq!(state.cursors.rows(), Some((20, 40)));
        let screen = state.cursors.screen().expect("drawn");
        let (a, b) = (screen.a.expect("A in view"), screen.b.expect("B in view"));
        assert!(a < b);
        // The flags are painted on the lines, Δt between them.
        assert!((centre_of(&shapes, "A").x - a).abs() <= 1.0);
        assert!((centre_of(&shapes, "B").x - b).abs() <= 1.0);
        assert!(texts(&shapes).iter().any(|(text, _)| text == "Δt 20 s"));

        // A second click turns them off.
        fixture.frame(&ctx, &mut state, vec![click(button, true)]);
        fixture.frame(&ctx, &mut state, vec![click(button, false)]);
        let shapes = fixture.frame(&ctx, &mut state, Vec::new());
        assert!(!state.cursors.enabled());
        assert!(!texts(&shapes).iter().any(|(text, _)| text == "A"));
    }

    // Dragging A moves A to the sample nearest the pointer, and never pans
    // the view.
    #[test]
    fn dragging_a_cursor_snaps_it_to_a_sample_and_leaves_the_view_alone() {
        let fixture = Fixture::new(ramp());
        let ctx = egui::Context::default();
        let mut state = TimeViewState::default();
        fixture.frame(&ctx, &mut state, Vec::new());
        fixture.frame(&ctx, &mut state, Vec::new());
        state.cursors.set(Some((20, 40)));
        fixture.frame(&ctx, &mut state, Vec::new());
        let screen = state.cursors.screen().expect("drawn");
        let view = state.shared_axis().expect("drawn").x_range;
        let a = screen.a.expect("A in view");
        let y = screen.area.center().y;
        // 10 s to the left, plus a third of a sample, which snaps away.
        let px_per_second = screen.area.width() / 60.0;
        let target = a - px_per_second * 10.33;

        let start = egui::pos2(a + 2.0, y);
        fixture.frame(&ctx, &mut state, vec![egui::Event::PointerMoved(start)]);
        fixture.frame(&ctx, &mut state, vec![click(start, true)]);
        for step in 1..=5 {
            let at = egui::pos2(start.x + (target - start.x) * step as f32 / 5.0, y);
            fixture.frame(&ctx, &mut state, vec![egui::Event::PointerMoved(at)]);
        }
        fixture.frame(&ctx, &mut state, vec![click(egui::pos2(target, y), false)]);
        fixture.frame(&ctx, &mut state, Vec::new());

        assert_eq!(state.cursors.rows(), Some((10, 40)));
        assert_eq!(
            state.shared_axis().expect("drawn").x_range,
            view,
            "a cursor drag does not pan"
        );
        let readout = readout(&fixture.dataset, (10, 40), &[(0, 0)]).expect("rows exist");
        // Hand calculation: 40 − 10 = 30 s; 80 − 20 = 60; 60 / 30 s = 2 /s.
        assert_eq!(readout.span, "30 s");
        assert_eq!(readout.channels[0].delta, "+60");
        assert_eq!(readout.channels[0].slope, "+2 /s");
    }

    #[test]
    fn a_new_file_removes_the_cursors() {
        let mut state = TimeViewState::default();
        state.cursors.set(Some((1, 2)));
        state.fit_new_file();
        assert!(!state.cursors.enabled());
    }

    #[test]
    fn cursors_on_rows_a_new_reading_lacks_are_removed() {
        let fixture = Fixture::new(ramp());
        let ctx = egui::Context::default();
        let mut state = TimeViewState::default();
        state.cursors.set(Some((10, 600)));
        fixture.frame(&ctx, &mut state, Vec::new());
        assert!(!state.cursors.enabled());
    }

    // The panel itself renders its values (a headless frame of it).
    #[test]
    fn the_panel_shows_every_value_of_a_shown_channel() {
        let dataset = ramp();
        let mut cursors = MeasurementCursors::default();
        cursors.set(Some((10, 40)));
        let ctx = egui::Context::default();
        let shapes = ctx
            .run(egui::RawInput::default(), |ctx| {
                egui::SidePanel::right("readout")
                    .exact_width(READOUT_WIDTH)
                    .show(ctx, |ui| {
                        show_readout(ui, &dataset, &cursors, &[(0, 0)], true)
                    });
            })
            .shapes;
        let shown: Vec<String> = texts(&shapes).into_iter().map(|(text, _)| text).collect();
        for expected in [
            "Δt = B − A",
            "30 s",
            "33.3333 mHz",
            "ramp",
            "20",
            "80",
            "+60",
            "+2 /s",
        ] {
            assert!(
                shown.iter().any(|text| text == expected),
                "{expected:?} in {shown:?}"
            );
        }
        // The whole footer, word for word: no stray runs of spaces.
        let footer = "Every value is the raw sample nearest the cursor, read from the file. \
                      A missing sample stays missing; nothing is interpolated. Δt is the real \
                      time between the two samples, also on the compressed axis.";
        assert!(shown.iter().any(|text| text == footer), "{shown:?}");
    }

    // Δx on a plain index is never rounded: rows 0 and 1 234 567 are
    // 1 234 567 apart.
    #[test]
    fn a_progressive_difference_is_written_exactly() {
        use glyde_core::ingest::{ProgressiveValues, TimeAxis};
        use glyde_core::series::{Series, SeriesValues};
        let dataset = Dataset {
            time: TimeAxis::Progressive {
                values: ProgressiveValues::Memory(vec![0.0, 1_234_567.0, 1_234_567.25]),
            },
            time_column_name: "sample".to_owned(),
            columns: vec![Series::new("v", SeriesValues::F64(vec![0.0, 1.0, 2.0]))],
        };
        assert_eq!(
            readout(&dataset, (0, 1), &[]).expect("rows").span,
            "1234567"
        );
        assert_eq!(
            readout(&dataset, (0, 2), &[]).expect("rows").span,
            "1234567.25"
        );
    }

    /// Two 50-sample clusters at 1 Hz, `0..=49 s` and `10_050..=10_099 s`:
    /// the gap is 99 % of the span, so the compressed axis is offered.
    fn two_clusters() -> Dataset {
        use glyde_core::series::{Series, SeriesValues};
        use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};
        let seconds: Vec<i128> = (0..50).chain(10_050..10_100).collect();
        Dataset {
            time: glyde_core::ingest::TimeAxis::Absolute {
                timestamps: seconds
                    .iter()
                    .map(|&s| Timestamp::new(s, TimeUnit::Seconds))
                    .collect::<Vec<_>>()
                    .into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_owned(),
            columns: vec![Series::new(
                "value",
                SeriesValues::F64(seconds.iter().map(|&s| s as f64).collect()),
            )],
        }
    }

    /// Drags `handle`'s line from where it is drawn by `dx` points.
    fn drag_by(
        fixture: &Fixture,
        ctx: &egui::Context,
        state: &mut TimeViewState,
        handle: Handle,
        dx: f32,
    ) {
        let screen = state.cursors.screen().expect("drawn");
        let x = match handle {
            Handle::A => screen.a,
            Handle::B => screen.b,
        }
        .expect("in view");
        let y = screen.area.center().y;
        let start = egui::pos2(x + 1.0, y);
        fixture.frame(ctx, state, vec![egui::Event::PointerMoved(start)]);
        fixture.frame(ctx, state, vec![click(start, true)]);
        for step in 1..=4 {
            let at = egui::pos2(start.x + dx * step as f32 / 4.0, y);
            fixture.frame(ctx, state, vec![egui::Event::PointerMoved(at)]);
        }
        fixture.frame(ctx, state, vec![click(egui::pos2(start.x + dx, y), false)]);
        fixture.frame(ctx, state, Vec::new());
    }

    // On a compressed axis the lines sit on their samples in both clusters,
    // a drag snaps through the compressed coordinates, and Δt stays the
    // real time across the collapsed gap.
    #[test]
    fn on_a_compressed_axis_cursors_snap_and_measure_real_time() {
        let fixture = Fixture::new(two_clusters());
        let ctx = egui::Context::default();
        let mut state = TimeViewState::default();
        state.set_compressed(true);
        for _ in 0..3 {
            fixture.frame(&ctx, &mut state, Vec::new());
        }
        // Row 10 is 10 s, row 60 is 10 060 s.
        state.cursors.set(Some((10, 60)));
        fixture.frame(&ctx, &mut state, Vec::new());
        let screen = state.cursors.screen().expect("drawn");
        let (a, b) = (screen.a.expect("A in view"), screen.b.expect("B in view"));
        // Compressed, the clusters sit side by side: A and B are both well
        // inside the view, B right of A.
        assert!(
            a < b && b - a < screen.area.width() * 0.9,
            "A at {a}, B at {b}"
        );
        let view = state.shared_axis().expect("drawn").x_range;
        let px_per_second = screen.area.width() / (view.1 - view.0) as f32;

        // Five seconds left, inside the second cluster: row 55, 10 055 s.
        drag_by(&fixture, &ctx, &mut state, Handle::B, -5.0 * px_per_second);
        assert_eq!(state.cursors.rows(), Some((10, 55)));
        let readout = readout(&fixture.dataset, (10, 55), &[(0, 0)]).expect("rows");
        // 10 055 − 10 = 10 045 s, real time, the collapsed gap included.
        assert_eq!(readout.span, "10045 s");
        assert_eq!(readout.channels[0].delta, "+10045");
        assert_eq!(readout.channels[0].slope, "+1 /s");
    }

    // A click on a cursor line next to a gap grabs the cursor; it does not
    // fit the cluster under it (SPEC §2.3's cluster click). The same click
    // a little away from the line does: the guard is what keeps the view.
    #[test]
    fn a_click_on_a_cursor_line_does_not_fit_the_cluster() {
        let fixture = Fixture::new(two_clusters());
        // A single click `offset` points right of cursor A, in a fresh
        // window; the view before and after it, and the rows after.
        let click_near_a = |offset: f32| {
            let ctx = egui::Context::default();
            let mut state = TimeViewState::default();
            state.set_compressed(true);
            for _ in 0..3 {
                fixture.frame(&ctx, &mut state, Vec::new());
            }
            state.cursors.set(Some((20, 70)));
            fixture.frame(&ctx, &mut state, Vec::new());
            let before = state.shared_axis().expect("drawn").x_range;
            let screen = state.cursors.screen().expect("drawn");
            let at = egui::pos2(
                screen.a.expect("A in view") + offset,
                screen.area.center().y,
            );
            fixture.frame(&ctx, &mut state, vec![egui::Event::PointerMoved(at)]);
            fixture.frame(&ctx, &mut state, vec![click(at, true)]);
            fixture.frame(&ctx, &mut state, vec![click(at, false)]);
            for _ in 0..3 {
                fixture.frame(&ctx, &mut state, Vec::new());
            }
            let after = state.shared_axis().expect("drawn").x_range;
            (before, after, state.cursors.rows())
        };
        let (before, after, rows) = click_near_a(1.0);
        assert_eq!(after, before, "a click on the line keeps the view");
        assert_eq!(rows, Some((20, 70)));
        let (before, after, rows) = click_near_a(40.0);
        assert_ne!(after, before, "a click off the line fits the cluster");
        assert_eq!(rows, Some((20, 70)));
    }
}
