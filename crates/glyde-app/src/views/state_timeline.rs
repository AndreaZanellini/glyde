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

//! State timeline (docs/ROADMAP.md M6, SPEC §4.3): renders each `bool`
//! [`glyde_core::series::Series`] as its own compact lane of on/off
//! horizontal bands — filled rectangles, never a numeric line/step plot
//! ("Boolean flags → horizontal bands (on/off), not step plots on a numeric
//! axis").
//!
//! [`glyde_core::series::bool_state_bands`] does the actual collapsing of
//! raw samples into bands (product logic, so it lives in `glyde-core` per
//! `docs/ARCHITECTURE.md` Hard rule 2); this module only draws them.
//!
//! `string` columns get the same treatment as labeled bands
//! ([`glyde_core::series::string_state_bands`]): the label is printed inside
//! a band wide enough to hold it and shown as a hover tooltip otherwise.
//!
//! A mostly-empty `string` column is an event column instead, drawn as
//! marker ticks ([`glyde_core::series::MarkerBuilder`]). Which of the two a
//! column is gets inferred ([`glyde_core::series::is_marker_column`]); the
//! lane header always says what was inferred and switches to the other view
//! in one click (SPEC §1.2: silent guessing is a bug).
//!
//! Collapse rule (SPEC §4.3: "when multiple states collapse into one pixel
//! column, render a 'multiple states' glyph that expands on zoom"): every
//! lane is drawn one screen column at a time from
//! [`glyde_core::series::ColumnGrid`] — a column touched by two or more
//! states, or two or more events, is drawn as an orange "multiple" block or
//! tick whose tooltip says so, never as whichever state happened to be drawn
//! last. The lanes zoom and pan horizontally, together; zooming in spreads
//! the same states over more columns until each shows on its own. A lane
//! too dense to keep every run is a bounded overview, so when one of its
//! cells is wider than a screen column it is rebuilt for the visible window
//! off the UI thread ([`LaneRefiner`]) and redrawn when ready.
//!
//! Deliberately out of scope here — the remaining M6 roadmap item: sharing
//! [`super::time`]'s pannable axis. The lanes zoom with each other, not yet
//! with the time-domain plot above them.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use egui_plot::{Plot, PlotPoint, PlotPoints, Polygon, Text, VLine};
use glyde_core::index::spill::SpillVec;
use glyde_core::ingest::{Dataset, TimeAxis};
use glyde_core::series::{
    is_marker_column, state_label_id, BoolBandBuilder, BoolLane, ColumnGrid, MarkerBuilder,
    MarkerLane, SeriesValues, SpilledValues, StateBandBuilder, StateCell, StateLane, BOOL_ON_ID,
};

use super::time::{format_x_axis_tick, seconds_to_tick, tick_to_seconds};

/// Height, in points, of one lane.
const LANE_HEIGHT: f32 = 36.0;

/// Fill color for a `true` band. `egui`'s own default accent blue — matches
/// nothing else drawn today, so an on/off lane never reads as a numeric
/// series (`views::time::series_color`'s golden-ratio hue stepping is a
/// different, per-series palette, deliberately not reused here since bands
/// only ever need the two states, not N distinct series colors).
const ON_COLOR: egui::Color32 = egui::Color32::from_rgb(66, 133, 244);
/// Fill color for a `false` band — a neutral, visibly "off" gray.
const OFF_COLOR: egui::Color32 = egui::Color32::from_gray(60);
/// The "multiple states / events in this column" glyph (SPEC §4.3).
const MIXED_COLOR: egui::Color32 = egui::Color32::from_rgb(205, 130, 45);

/// Every lane zooms and pans horizontally with every other.
const LANE_LINK_GROUP: &str = "state_timeline_lanes";

/// The legend under which the lanes are drawn: what the "multiple" glyph
/// means and how to expand it.
const COLLAPSE_LEGEND: &str = "Orange: several states or events share one screen column — \
     zoom in to see each one (Ctrl/⌘ + scroll or pinch to zoom, drag to pan). \
     Double-click a lane to show the whole file.";

/// The most cells a lane is rebuilt with for a zoomed-in window.
const MAX_REFINE_CELLS: usize = 1 << 15;

/// Which drawing of a column a lane is (a `string` column has two).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneKind {
    Bool,
    State,
    Markers,
}

/// A lane rebuilt for a zoomed-in window by a [`LaneRefiner`].
#[derive(Debug, Clone, PartialEq)]
pub enum LaneData {
    Bool(BoolLane),
    State(StateLane),
    Markers(MarkerLane),
}

impl LaneData {
    fn as_ref(&self) -> LaneRef<'_> {
        match self {
            LaneData::Bool(lane) => LaneRef::Bool(lane),
            LaneData::State(lane) => LaneRef::State(lane),
            LaneData::Markers(lane) => LaneRef::Markers(lane),
        }
    }
}

/// A lane to draw: the whole-file one prepared with the dataset, or a
/// [`LaneData`] rebuilt for the visible window.
#[derive(Debug, Clone, Copy)]
enum LaneRef<'a> {
    Bool(&'a BoolLane),
    State(&'a StateLane),
    Markers(&'a MarkerLane),
}

impl LaneRef<'_> {
    fn resolves(self, grid: &ColumnGrid) -> bool {
        match self {
            LaneRef::Bool(lane) => lane.resolves(grid),
            LaneRef::State(lane) => lane.resolves(grid),
            LaneRef::Markers(lane) => lane.resolves(grid),
        }
    }

    fn tick_bounds(self) -> (i128, i128) {
        match self {
            LaneRef::Bool(lane) => lane.tick_bounds(),
            LaneRef::State(lane) => lane.tick_bounds(),
            LaneRef::Markers(lane) => lane.tick_bounds(),
        }
    }
}

/// See [`LaneRefiner`].
type RefineFn =
    dyn Fn(usize, LaneKind, (i128, i128), usize, &AtomicBool) -> Option<LaneData> + Send + Sync;

/// Rebuilds one lane for a window, from any thread: `(column index, kind,
/// tick window, cells, cancel) -> lane` ([`refine_lane`] over shared handles
/// to the data). `None` when cancelled or the column has no such lane.
#[derive(Clone)]
pub struct LaneRefiner(Arc<RefineFn>);

impl LaneRefiner {
    pub fn new(
        refine: impl Fn(usize, LaneKind, (i128, i128), usize, &AtomicBool) -> Option<LaneData>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self(Arc::new(refine))
    }
}

impl std::fmt::Debug for LaneRefiner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LaneRefiner")
    }
}

/// What one rebuild was asked for: a tick window and its cell count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RefineRequest {
    window: (i128, i128),
    cells: usize,
}

/// A rebuild running on the `rayon` pool; dropping it cancels it.
#[derive(Debug)]
struct PendingRefinement {
    request: RefineRequest,
    result: mpsc::Receiver<Option<LaneData>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for PendingRefinement {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// One lane's rebuilt view and the rebuild in flight, if any.
#[derive(Debug, Default)]
struct LaneSlot {
    refined: Option<LaneData>,
    pending: Option<PendingRefinement>,
    /// The last rebuild started, so an identical one is never repeated.
    requested: Option<RefineRequest>,
}

impl LaneSlot {
    /// Takes the pending rebuild's result if it has arrived.
    fn collect(&mut self) {
        let Some(pending) = &self.pending else {
            return;
        };
        match pending.result.try_recv() {
            Ok(lane) => {
                if lane.is_some() {
                    self.refined = lane;
                }
                self.pending = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
        }
    }
}

/// What the state timeline keeps from one frame to the next: each lane's
/// view rebuilt for a zoomed-in window, and where rebuilds run.
#[derive(Debug, Default)]
pub struct StateTimelineState {
    slots: HashMap<(usize, LaneKind), LaneSlot>,
    refiner: Option<LaneRefiner>,
}

impl StateTimelineState {
    /// Drops every rebuilt lane and cancels the rebuilds in flight; the data
    /// on screen changed.
    pub fn data_changed(&mut self) {
        self.slots.clear();
    }

    /// Rebuilds dense lanes for a zoomed-in window with `refiner`, off the
    /// UI thread; `None` draws the prepared lanes only (a file still
    /// loading). Must be set for the data on screen.
    pub fn set_refiner(&mut self, refiner: Option<LaneRefiner>) {
        self.slots.clear();
        self.refiner = refiner;
    }

    /// How many lanes currently draw a rebuilt view.
    #[cfg(test)]
    fn refined_lanes(&self) -> usize {
        self.slots.values().filter(|s| s.refined.is_some()).count()
    }

    /// Picks what to draw for `grid`: the prepared lane `base`, or one
    /// rebuilt for the visible window — starting that rebuild when neither
    /// resolves the screen (an overview cell wider than a screen column).
    fn lane_for<'a>(
        &'a mut self,
        key: (usize, LaneKind),
        base: LaneRef<'a>,
        grid: &ColumnGrid,
        ctx: &egui::Context,
    ) -> LaneRef<'a> {
        let refiner = self.refiner.clone();
        let slot = self.slots.entry(key).or_default();
        slot.collect();
        let data = base.tick_bounds();
        let Some(visible) = grid.clipped(data) else {
            return base;
        };
        let resolves =
            |lane: Option<&LaneData>| lane.is_some_and(|l| l.as_ref().resolves(&visible));
        if !base.resolves(&visible) && !resolves(slot.refined.as_ref()) {
            let in_flight = slot
                .pending
                .as_ref()
                .is_some_and(|p| visible.is_resolved_by(p.request.window, p.request.cells));
            let request = refine_request(&visible, data);
            if let (Some(refiner), false) = (refiner, in_flight || slot.requested == Some(request))
            {
                tracing::debug!(
                    column = key.0,
                    kind = ?key.1,
                    window = ?request.window,
                    cells = request.cells,
                    "rebuilding a dense state lane for the zoomed-in window"
                );
                slot.pending = Some(spawn_refinement(&refiner, key, request, ctx.clone()));
                slot.requested = Some(request);
            }
        }
        match &slot.refined {
            Some(refined) if refined.as_ref().resolves(&visible) => refined.as_ref(),
            _ if base.resolves(&visible) => base,
            // Not yet fine enough, but finer than the whole-file overview
            // wherever it covers the screen.
            Some(refined) if covers(refined.as_ref().tick_bounds(), visible.window()) => {
                refined.as_ref()
            }
            _ => base,
        }
    }
}

fn covers((min, max): (i128, i128), (start, end): (i128, i128)) -> bool {
    min <= start && max >= end
}

/// The rebuild to ask for when `visible` is not resolved: one screen width
/// of margin on each side (so a short pan needs no new rebuild), within the
/// data's own extent, at one cell per screen column.
fn refine_request(visible: &ColumnGrid, (data_min, data_max): (i128, i128)) -> RefineRequest {
    let (start, end) = visible.window();
    let span = end.saturating_sub(start);
    let window = (
        start.saturating_sub(span).max(data_min),
        end.saturating_add(span).min(data_max),
    );
    let cells = if span == 0 {
        visible.columns()
    } else {
        let share = window.1.saturating_sub(window.0) as f64 / span as f64;
        (visible.columns() as f64 * share).ceil() as usize
    };
    RefineRequest {
        window,
        cells: cells.clamp(1, MAX_REFINE_CELLS),
    }
}

fn spawn_refinement(
    refiner: &LaneRefiner,
    (column, kind): (usize, LaneKind),
    request: RefineRequest,
    ctx: egui::Context,
) -> PendingRefinement {
    let (tx, result) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let job_cancel = Arc::clone(&cancel);
    let refiner = refiner.clone();
    rayon::spawn(move || {
        let lane = (refiner.0)(column, kind, request.window, request.cells, &job_cancel);
        if tx.send(lane).is_ok() {
            ctx.request_repaint();
        }
    });
    PendingRefinement {
        request,
        result,
        cancel,
    }
}

/// Renders `dataset`'s state lanes (SPEC §4.3) into `ui`, one
/// [`egui_plot::Plot`] per lane. `bool_bands`, `state_lanes` and
/// `marker_lanes` are `dataset.columns`-parallel (see [`cache_bool_bands`],
/// [`cache_state_lanes`], [`cache_marker_lanes`]); a `None` entry draws
/// nothing. `ticks` is `dataset.time`'s own pyramid ticks
/// (`glyde_core::ingest::TimeAxis::to_pyramid_ticks`), taken as a parameter
/// and computed by the caller once per status change — the same
/// once-not-per-frame discipline [`super::time::show`] follows for the
/// identical reason (see its own doc comment).
pub fn show(
    ui: &mut egui::Ui,
    dataset: &Dataset,
    ticks: &[i128],
    bool_bands: &[Option<BoolLane>],
    state_lanes: &[Option<StateLane>],
    marker_lanes: &[Option<MarkerColumn>],
    state: &mut StateTimelineState,
) {
    let time = &dataset.time;
    let bounds = bool_bands
        .iter()
        .flatten()
        .map(BoolLane::tick_bounds)
        .chain(state_lanes.iter().flatten().map(StateLane::tick_bounds))
        .chain(marker_lanes.iter().flatten().map(|m| m.lane.tick_bounds()))
        .next();
    let Some((min_tick, max_tick)) = bounds else {
        return;
    };
    let axis = axis_bounds(min_tick, max_tick, time);
    ui.weak(COLLAPSE_LEGEND);

    for (index, series) in dataset.columns.iter().enumerate() {
        let states = state_lanes.get(index).and_then(Option::as_ref);
        let markers = marker_lanes.get(index).and_then(Option::as_ref);
        let bools = bool_bands.get(index).and_then(Option::as_ref);
        let lane = match (states, markers, bools) {
            (Some(states), Some(markers), _) => {
                ui.label(series.name());
                if string_lane_toggle(ui, index, series.name(), markers) {
                    LaneRef::Markers(&markers.lane)
                } else {
                    LaneRef::State(states)
                }
            }
            (None, Some(markers), _) => {
                ui.label(series.name());
                LaneRef::Markers(&markers.lane)
            }
            (Some(states), None, _) => {
                ui.label(series.name());
                LaneRef::State(states)
            }
            (None, None, Some(bools)) => {
                ui.label(series.name());
                LaneRef::Bool(bools)
            }
            (None, None, None) => continue,
        };
        show_lane(ui, index, lane, ticks, time, axis, state);
    }
}

/// The visible window as screen columns, and where those columns are in
/// plot seconds ([`tick_to_seconds`]'s coordinate space).
#[derive(Debug, Clone, Copy)]
struct ViewColumns {
    grid: ColumnGrid,
    x0: f64,
    x1: f64,
}

impl ViewColumns {
    fn new(time: &TimeAxis, (x_min, x_max): (f64, f64), columns: usize) -> Self {
        let grid = ColumnGrid::new(
            seconds_to_tick(time, x_min),
            seconds_to_tick(time, x_max),
            columns,
        );
        let (start, end) = grid.window();
        Self {
            grid,
            x0: tick_to_seconds(time, start),
            x1: tick_to_seconds(time, end),
        }
    }

    /// Plot-x of the left edge of column `column` (`columns()` for the
    /// right edge of the last one).
    fn x(&self, column: usize) -> f64 {
        self.x0 + (self.x1 - self.x0) * column as f64 / self.grid.columns() as f64
    }

    /// The column under plot-x `x`, if it is inside the window.
    fn column_at(&self, x: f64) -> Option<usize> {
        if !(self.x0..=self.x1).contains(&x) || self.x1 <= self.x0 {
            return (self.x1 <= self.x0 && x == self.x0).then_some(0);
        }
        let column = ((x - self.x0) / (self.x1 - self.x0) * self.grid.columns() as f64) as usize;
        Some(column.min(self.grid.columns() - 1))
    }
}

/// What one lane drew this frame, for its hover tooltip.
enum Drawn<'a> {
    Cells {
        cells: Vec<StateCell>,
        labels: Labels<'a>,
    },
    Markers {
        counts: Vec<u32>,
        lane: &'a MarkerLane,
    },
}

/// Label id → label text, for a state lane's labels and tooltips; empty for
/// a bool lane.
type Labels<'a> = HashMap<u32, &'a str>;

fn lane_labels(lane: LaneRef<'_>) -> Labels<'_> {
    match lane {
        LaneRef::State(StateLane::Exact { bands, .. }) => bands
            .iter()
            .map(|band| (state_label_id(&band.label), band.label.as_str()))
            .collect(),
        LaneRef::State(StateLane::Overview { labels, .. }) => labels
            .iter()
            .map(|(&id, label)| (id, label.as_str()))
            .collect(),
        LaneRef::Bool(_) | LaneRef::Markers(_) => Labels::new(),
    }
}

/// Draws one lane as a horizontally zoomable plot linked to every other
/// lane, collapsing it onto this frame's screen columns.
fn show_lane(
    ui: &mut egui::Ui,
    index: usize,
    base: LaneRef<'_>,
    ticks: &[i128],
    time: &TimeAxis,
    axis: (f64, f64),
    state: &mut StateTimelineState,
) {
    let kind = match base {
        LaneRef::Bool(_) => LaneKind::Bool,
        LaneRef::State(_) => LaneKind::State,
        LaneRef::Markers(_) => LaneKind::Markers,
    };
    let id = match kind {
        LaneKind::Markers => format!("marker_lane_{index}"),
        LaneKind::Bool | LaneKind::State => format!("state_timeline_{index}"),
    };
    let mut hover = None;
    let response = Plot::new(id)
        .height(LANE_HEIGHT)
        .show_axes([true, false])
        .show_grid([false, false])
        .allow_zoom([true, false])
        .allow_scroll([true, false])
        .allow_drag([true, false])
        .allow_boxed_zoom(false)
        .link_axis(LANE_LINK_GROUP, [true, false])
        // What the first frame and a double-click fit: the whole file, the
        // lane's fixed unit height. Never set per frame, which would undo
        // the zoom another linked lane applied this frame.
        .include_x(axis.0)
        .include_x(axis.1)
        .include_y(0.0)
        .include_y(1.0)
        .set_margin_fraction(egui::Vec2::ZERO)
        .x_axis_formatter(move |mark, _range| format_x_axis_tick(ticks, mark, time))
        .show(ui, |plot_ui| {
            let x_range = if plot_ui.auto_bounds().x {
                axis
            } else {
                let bounds = plot_ui.plot_bounds();
                (bounds.min()[0], bounds.max()[0])
            };
            // A lane drawn before the one being zoomed learns the new range
            // from the link group a frame later: draw that frame too.
            let memory = egui::Id::new((LANE_LINK_GROUP, "last_x_range", index));
            let changed = plot_ui.ctx().data_mut(|data| {
                let previous = data.get_temp::<(f64, f64)>(memory);
                data.insert_temp(memory, x_range);
                previous != Some(x_range)
            });
            if changed {
                plot_ui.ctx().request_repaint();
            }
            let columns = plot_ui.transform().frame().width().round().max(1.0) as usize;
            let view = ViewColumns::new(time, x_range, columns);
            let ctx = plot_ui.ctx().clone();
            let lane = state.lane_for((index, kind), base, &view.grid, &ctx);
            let seconds_per_point = (view.x1 - view.x0).abs() / columns as f64;
            let drawn = match lane {
                LaneRef::Bool(lane) => {
                    let cells = lane.collapse(&view.grid);
                    draw_cells(plot_ui, &view, &cells, |id| {
                        if id == BOOL_ON_ID {
                            ON_COLOR
                        } else {
                            OFF_COLOR
                        }
                    });
                    Drawn::Cells {
                        cells,
                        labels: Labels::new(),
                    }
                }
                LaneRef::State(states) => {
                    let cells = states.collapse(&view.grid);
                    let labels = lane_labels(lane);
                    draw_cells(plot_ui, &view, &cells, label_color);
                    draw_labels(plot_ui, &view, &cells, &labels, seconds_per_point);
                    Drawn::Cells { cells, labels }
                }
                LaneRef::Markers(markers) => {
                    let counts = markers.collapse(&view.grid);
                    draw_markers(plot_ui, &view, &counts);
                    Drawn::Markers {
                        counts,
                        lane: markers,
                    }
                }
            };
            let column = plot_ui
                .pointer_coordinate()
                .and_then(|point| view.column_at(point.x));
            hover = column.and_then(|column| hover_text(kind, &drawn, &view.grid, column));
        });
    if let (Some(text), true) = (hover, response.response.hovered()) {
        response.response.on_hover_text(text);
    }
}

/// Fills each run of equal columns, `Mixed` ones in the "multiple" color.
fn draw_cells(
    plot_ui: &mut egui_plot::PlotUi,
    view: &ViewColumns,
    cells: &[StateCell],
    color: impl Fn(u32) -> egui::Color32,
) {
    for (first, end, cell) in cell_runs(cells) {
        let fill = match cell {
            StateCell::Empty => continue,
            StateCell::Single(id) => color(id),
            StateCell::Mixed => MIXED_COLOR,
        };
        fill_rect(plot_ui, view.x(first), view.x(end), fill);
    }
}

/// Maximal runs `(first, end, cell)` of equal consecutive cells, `end`
/// exclusive — what is filled as one rectangle.
fn cell_runs(cells: &[StateCell]) -> impl Iterator<Item = (usize, usize, StateCell)> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        let cell = *cells.get(start)?;
        let first = start;
        while start < cells.len() && cells[start] == cell {
            start += 1;
        }
        Some((first, start, cell))
    })
}

/// Estimated width, in points, of one label glyph — a band narrower than
/// `label.chars().count() * LABEL_CHAR_WIDTH` shows its label as a tooltip
/// instead (SPEC §4.3: "label when wide enough, tooltip otherwise").
const LABEL_CHAR_WIDTH: f64 = 7.0;
const LABEL_PADDING: f64 = 6.0;

/// Prints each single-state run's label inside it when it fits.
fn draw_labels(
    plot_ui: &mut egui_plot::PlotUi,
    view: &ViewColumns,
    cells: &[StateCell],
    labels: &Labels<'_>,
    seconds_per_point: f64,
) {
    for (first, end, cell) in cell_runs(cells) {
        let StateCell::Single(id) = cell else {
            continue;
        };
        let Some(label) = labels.get(&id) else {
            continue;
        };
        let (x0, x1) = (view.x(first), view.x(end));
        let points_wide = (x1 - x0) / seconds_per_point.max(f64::EPSILON);
        let needed = label.chars().count() as f64 * LABEL_CHAR_WIDTH + LABEL_PADDING;
        if points_wide >= needed {
            plot_ui.text(
                Text::new(
                    PlotPoint::new((x0 + x1) / 2.0, 0.5),
                    egui::RichText::new(*label).color(egui::Color32::WHITE),
                )
                .anchor(egui::Align2::CENTER_CENTER),
            );
        }
    }
}

/// Color of a single marker tick.
const MARKER_COLOR: egui::Color32 = egui::Color32::from_rgb(235, 190, 60);

/// One full-height tick per column holding any event (SPEC §4.3: "always
/// visible regardless of zoom"), in the "multiple" color and wider where a
/// column holds several.
fn draw_markers(plot_ui: &mut egui_plot::PlotUi, view: &ViewColumns, counts: &[u32]) {
    for (column, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let x = (view.x(column) + view.x(column + 1)) / 2.0;
        let (color, width) = if count > 1 {
            (MIXED_COLOR, 3.0_f32)
        } else {
            (MARKER_COLOR, 2.0_f32)
        };
        plot_ui.vline(VLine::new(x).color(color).width(width));
    }
}

/// What the tooltip says over screen column `column`.
fn hover_text(
    kind: LaneKind,
    drawn: &Drawn<'_>,
    grid: &ColumnGrid,
    column: usize,
) -> Option<String> {
    match drawn {
        Drawn::Cells { cells, labels } => match (*cells.get(column)?, kind) {
            (StateCell::Empty, _) => None,
            (StateCell::Mixed, LaneKind::Bool) => {
                Some("on and off both occur here — zoom in to see each change".to_owned())
            }
            (StateCell::Mixed, _) => Some("several states here — zoom in to see them".to_owned()),
            (StateCell::Single(id), LaneKind::Bool) => {
                Some(if id == BOOL_ON_ID { "on" } else { "off" }.to_owned())
            }
            (StateCell::Single(id), _) => Some(
                labels
                    .get(&id)
                    .map_or_else(|| "(label not retained)".to_owned(), |l| format!("{l:?}")),
            ),
        },
        Drawn::Markers { counts, lane } => match *counts.get(column)? {
            0 => None,
            1 => Some(match lane {
                MarkerLane::Exact { markers, .. } => markers
                    .iter()
                    .find(|marker| grid.column_of(marker.tick) == Some(column))
                    .map_or_else(|| "1 event".to_owned(), |m| format!("{:?}", m.label)),
                MarkerLane::Overview { .. } => "1 event".to_owned(),
            }),
            count => Some(format!("{count} events here — zoom in to separate them")),
        },
    }
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

/// The full-file x-axis bounds (in plot seconds, [`tick_to_seconds`]'s
/// coordinate space) a lane fits on its first frame and on a double-click.
/// A single-sample file (SPEC §1.4: a valid input) has a zero-width range;
/// it is padded symmetrically so the plot still has a nonzero span to fit,
/// the same degenerate case `views::time::pad_if_degenerate` handles for
/// the time-domain view.
fn axis_bounds(min_tick: i128, max_tick: i128, time: &TimeAxis) -> (f64, f64) {
    let min = tick_to_seconds(time, min_tick);
    let max = tick_to_seconds(time, max_tick);
    if (max - min).abs() > f64::EPSILON {
        (min, max)
    } else {
        (min - 1.0, max + 1.0)
    }
}

/// Rebuilds column `column`'s `kind` lane over the tick window `window`
/// with a `cells`-cell overview — the work behind [`LaneRefiner`]. On a
/// time-ordered axis (`sorted`) only the rows from the last one before the
/// window to the first one after it are read; otherwise every row is, in
/// file order. `None` if `cancel` is raised first or the column has no such
/// lane. A spilled column is read row by row from its mapping, never
/// collected (SPEC §5.1).
#[allow(clippy::too_many_arguments)]
pub fn refine_lane<'d>(
    dataset: &'d Dataset,
    ticks: &[i128],
    sorted: bool,
    column: usize,
    kind: LaneKind,
    window: (i128, i128),
    cells: usize,
    cancel: &AtomicBool,
) -> Option<LaneData> {
    const CANCEL_STRIDE: usize = 1 << 16;
    let series = dataset.columns.get(column)?;
    let rows = if sorted {
        let first = ticks.partition_point(|&t| t < window.0).saturating_sub(1);
        let end = (ticks.partition_point(|&t| t <= window.1) + 1).min(ticks.len());
        first..end
    } else {
        0..ticks.len()
    };
    let cancelled =
        |row: usize| row.is_multiple_of(CANCEL_STRIDE) && cancel.load(Ordering::Relaxed);
    let (min, max) = window;
    match (kind, series.values()) {
        (LaneKind::Bool, SeriesValues::Bool(values)) => {
            let mut builder = BoolBandBuilder::for_window(min, max, cells);
            for row in rows {
                if cancelled(row) {
                    return None;
                }
                builder.push(*values.get(row)?, ticks[row]);
            }
            Some(LaneData::Bool(builder.finish_lane()))
        }
        (LaneKind::Bool, SeriesValues::Spilled(SpilledValues::Bool(values))) => {
            let mut builder = BoolBandBuilder::for_window(min, max, cells);
            let mut row = rows.start;
            let mut stopped = false;
            let result = values.read_chunks(rows, &mut |chunk: &[u8]| {
                if cancel.load(Ordering::Relaxed) {
                    stopped = true;
                    return Ok(());
                }
                if !stopped {
                    for &byte in chunk {
                        builder.push(byte != 0, ticks[row]);
                        row += 1;
                    }
                }
                Ok(())
            });
            if let Err(error) = result {
                tracing::warn!(%error, "failed to read a spilled bool column back for a zoomed-in lane");
            }
            (!stopped).then(|| LaneData::Bool(builder.finish_lane()))
        }
        (LaneKind::State | LaneKind::Markers, values) => {
            let text: Box<dyn Fn(usize) -> Option<&'d str> + 'd> = match values {
                SeriesValues::String(values) => Box::new(|row| values.get(row).map(String::as_str)),
                SeriesValues::Spilled(SpilledValues::String(values)) => {
                    Box::new(|row| values.get(row))
                }
                _ => return None,
            };
            if kind == LaneKind::State {
                let mut builder = StateBandBuilder::for_window(min, max, cells);
                for row in rows {
                    if cancelled(row) {
                        return None;
                    }
                    builder.push(text(row)?, ticks[row]);
                }
                Some(LaneData::State(builder.finish_lane()))
            } else {
                let mut builder = MarkerBuilder::for_window(min, max, cells);
                for row in rows {
                    if cancelled(row) {
                        return None;
                    }
                    builder.push(text(row)?, ticks[row]);
                }
                Some(LaneData::Markers(builder.finish_lane()))
            }
        }
        (LaneKind::Bool, _) => None,
    }
}

/// Header line for a `string` column that can be drawn either way, and the
/// one-click switch between the two; returns whether to draw it as markers.
/// The choice starts at the inferred view and is remembered per column for
/// the session (`egui` temp memory).
fn string_lane_toggle(ui: &mut egui::Ui, index: usize, name: &str, markers: &MarkerColumn) -> bool {
    let inferred = markers.inferred_as_markers();
    let id = egui::Id::new(("string_lane_as_markers", index, name, markers.rows));
    let mut as_markers = ui.data_mut(|data| *data.get_temp_mut_or(id, inferred));
    ui.horizontal(|ui| {
        ui.weak(string_lane_notice(inferred, as_markers, markers));
        let button = if as_markers {
            "Show as state bands"
        } else {
            "Show as events"
        };
        if ui.small_button(button).clicked() {
            as_markers = !as_markers;
            ui.data_mut(|data| data.insert_temp(id, as_markers));
            tracing::info!(
                column = name,
                as_markers,
                inferred_as_markers = inferred,
                "user switched how a text column is drawn"
            );
        }
    });
    as_markers
}

/// What the header of a switchable `string` lane says: what is drawn, and
/// what was inferred from how many rows hold text.
fn string_lane_notice(inferred: bool, as_markers: bool, markers: &MarkerColumn) -> String {
    let kind = |markers: bool| if markers { "events" } else { "state bands" };
    let evidence = format!("{} of {} rows have text", markers.non_empty, markers.rows);
    if inferred == as_markers {
        format!("Drawn as {} (inferred: {evidence})", kind(as_markers))
    } else {
        format!(
            "Drawn as {} (your choice; inferred {}: {evidence})",
            kind(as_markers),
            kind(inferred)
        )
    }
}

/// A `string` column's event-marker view plus the counts that decide whether
/// it is drawn as markers by default (see [`is_marker_column`]).
#[derive(Debug, Clone, PartialEq)]
pub struct MarkerColumn {
    pub lane: MarkerLane,
    pub non_empty: usize,
    pub rows: usize,
}

impl MarkerColumn {
    pub fn inferred_as_markers(&self) -> bool {
        is_marker_column(self.non_empty, self.rows)
    }
}

/// Builds `marker_lanes` for [`show`]: `dataset.columns`-parallel, `Some`
/// for every `string` column with at least one non-empty cell (whether it is
/// *drawn* as markers by default is [`MarkerColumn::inferred_as_markers`]),
/// `None` for everything else. One pass per column; a spilled string column
/// is streamed through its iterator, never collected (SPEC §5.1).
pub fn cache_marker_lanes(dataset: &Dataset, ticks: &[i128]) -> Vec<Option<MarkerColumn>> {
    let Some((&min_tick, &max_tick)) = ticks.iter().min().zip(ticks.iter().max()) else {
        return vec![None; dataset.columns.len()];
    };
    dataset
        .columns
        .iter()
        .map(|series| {
            let builder = MarkerBuilder::for_view(min_tick, max_tick);
            let column = match series.values() {
                SeriesValues::String(values) => {
                    marker_column(values.iter().map(String::as_str), ticks, builder)
                }
                SeriesValues::Spilled(SpilledValues::String(values)) => {
                    marker_column(values.iter(), ticks, builder)
                }
                _ => return None,
            }?;
            tracing::info!(
                column = series.name(),
                non_empty = column.non_empty,
                rows = column.rows,
                drawn_as = if column.inferred_as_markers() {
                    "events"
                } else {
                    "state bands"
                },
                "inferred how a text column is drawn (at most half the rows holding text means events)"
            );
            Some(column)
        })
        .collect()
}

fn marker_column<'a>(
    cells: impl Iterator<Item = &'a str>,
    ticks: &[i128],
    mut builder: MarkerBuilder,
) -> Option<MarkerColumn> {
    let (mut non_empty, mut rows) = (0, 0);
    for (cell, &tick) in cells.zip(ticks) {
        rows += 1;
        if !cell.is_empty() {
            non_empty += 1;
            builder.push(cell, tick);
        }
    }
    (non_empty > 0).then(|| MarkerColumn {
        lane: builder.finish_lane(),
        non_empty,
        rows,
    })
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

/// A stable, readable fill for `label`, so the same state is the same color
/// everywhere and across files.
fn label_color(label_id: u32) -> egui::Color32 {
    let hue = (f64::from(label_id) * 0.618_033_988_75).fract() as f32;
    egui::ecolor::Hsva::new(hue, 0.55, 0.55, 1.0).into()
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
                show(
                    ui,
                    &dataset,
                    &ticks,
                    &lanes,
                    &[],
                    &[],
                    &mut StateTimelineState::default(),
                );
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
                show(
                    ui,
                    &dataset,
                    &ticks,
                    &bool_bands,
                    &[],
                    &[],
                    &mut StateTimelineState::default(),
                );
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
                show(
                    ui,
                    &dataset,
                    &ticks,
                    &[],
                    &lanes,
                    &[],
                    &mut StateTimelineState::default(),
                );
            });
        });
        assert!(!output.shapes.is_empty());
    }

    // Corpus 48's shape: the last `idle` is one sample at the axis end. It
    // must still own the last screen column (with the `fault` it ends),
    // never be painted over.
    #[test]
    fn a_final_single_sample_state_shows_in_the_last_column() {
        let dataset = Dataset {
            columns: vec![Series::new(
                "state",
                SeriesValues::String(
                    ["idle", "running", "fault", "idle"]
                        .map(str::to_owned)
                        .to_vec(),
                ),
            )],
            ..sample_dataset()
        };
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let lanes = cache_state_lanes(&dataset, &ticks);
        let lane = lanes[0].as_ref().expect("string column gets a lane");
        let (min, max) = lane.tick_bounds();
        let grid = ColumnGrid::new(min, max, 300);
        let drawn = Drawn::Cells {
            cells: lane.collapse(&grid),
            labels: lane_labels(LaneRef::State(lane)),
        };
        assert_eq!(
            hover_text(LaneKind::State, &drawn, &grid, 299).as_deref(),
            Some("several states here — zoom in to see them")
        );
        assert_eq!(
            hover_text(LaneKind::State, &drawn, &grid, 298).as_deref(),
            Some("\"fault\"")
        );
        assert_eq!(
            hover_text(LaneKind::State, &drawn, &grid, 0).as_deref(),
            Some("\"idle\"")
        );
    }

    #[test]
    fn non_string_columns_get_no_state_lane() {
        let dataset = sample_dataset();
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        assert!(cache_state_lanes(&dataset, &ticks)
            .iter()
            .all(Option::is_none));
    }

    fn sparse_and_dense_string_dataset() -> Dataset {
        let mut dataset = sample_dataset();
        dataset.columns.push(Series::new(
            "marker",
            SeriesValues::String(vec!["".into(), "a".into(), "".into(), "".into()]),
        ));
        dataset.columns.push(Series::new(
            "state",
            SeriesValues::String(vec!["x".into(), "y".into(), "x".into(), "y".into()]),
        ));
        dataset
    }

    #[test]
    fn only_a_sparse_string_column_is_inferred_as_markers() {
        let dataset = sparse_and_dense_string_dataset();
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let lanes = cache_marker_lanes(&dataset, &ticks);
        assert!(lanes[0].is_none() && lanes[1].is_none());

        let sparse = lanes[2].as_ref().expect("a string column with text");
        assert!(matches!(&sparse.lane, MarkerLane::Exact { markers, .. } if markers.len() == 1));
        assert_eq!((sparse.non_empty, sparse.rows), (1, 4));
        assert!(sparse.inferred_as_markers());

        // The dense column still gets a marker view, so the user can switch
        // to it in one click — it is just not the inferred default.
        let dense = lanes[3].as_ref().expect("a string column with text");
        assert_eq!((dense.non_empty, dense.rows), (4, 4));
        assert!(!dense.inferred_as_markers());
    }

    #[test]
    fn an_all_empty_string_column_has_no_marker_view() {
        let mut dataset = sample_dataset();
        dataset.columns = vec![Series::new(
            "blank",
            SeriesValues::String(vec![String::new(); 4]),
        )];
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        assert_eq!(cache_marker_lanes(&dataset, &ticks), vec![None]);
    }

    #[test]
    fn the_lane_header_names_what_was_inferred_and_what_is_drawn() {
        let column = |non_empty| MarkerColumn {
            lane: MarkerLane::Exact {
                markers: Vec::new(),
                min_tick: 0,
                max_tick: 1,
            },
            non_empty,
            rows: 9,
        };
        assert_eq!(
            string_lane_notice(true, true, &column(2)),
            "Drawn as events (inferred: 2 of 9 rows have text)"
        );
        assert_eq!(
            string_lane_notice(true, false, &column(2)),
            "Drawn as state bands (your choice; inferred events: 2 of 9 rows have text)"
        );
        assert_eq!(
            string_lane_notice(false, false, &column(7)),
            "Drawn as state bands (inferred: 7 of 9 rows have text)"
        );
        assert_eq!(
            string_lane_notice(false, true, &column(7)),
            "Drawn as events (your choice; inferred state bands: 7 of 9 rows have text)"
        );
    }

    /// Every text `show` painted in one headless frame, one per line.
    fn rendered_lane_texts(dataset: &Dataset, ticks: &[i128]) -> String {
        let state = cache_state_lanes(dataset, ticks);
        let markers = cache_marker_lanes(dataset, ticks);
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(
                    ui,
                    dataset,
                    ticks,
                    &[],
                    &state,
                    &markers,
                    &mut StateTimelineState::default(),
                );
            });
        });
        let mut texts = String::new();
        for clipped in &output.shapes {
            if let egui::epaint::Shape::Text(text) = &clipped.shape {
                texts.push_str(text.galley.text());
                texts.push('\n');
            }
        }
        texts
    }

    #[test]
    fn each_string_column_is_drawn_once_with_its_inference_shown() {
        let dataset = sparse_and_dense_string_dataset();
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let texts = rendered_lane_texts(&dataset, &ticks);
        assert!(
            texts.contains("Drawn as events (inferred: 1 of 4 rows have text)"),
            "{texts}"
        );
        assert!(
            texts.contains("Drawn as state bands (inferred: 4 of 4 rows have text)"),
            "{texts}"
        );
        assert_eq!(texts.matches("Show as state bands").count(), 1, "{texts}");
        assert_eq!(texts.matches("Show as events").count(), 1, "{texts}");
    }

    #[test]
    fn corpus_case_49_yields_its_two_markers() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/corpus/case-49-sparse-markers.csv");
        let dataset = glyde_core::ingest::load(&path).expect("case 49 loads");
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let lanes = cache_marker_lanes(&dataset, &ticks);
        let columns: Vec<&MarkerColumn> = lanes.iter().flatten().collect();
        assert_eq!(columns.len(), 1, "{lanes:?}");
        assert!(columns[0].inferred_as_markers());
        let MarkerLane::Exact { markers, .. } = &columns[0].lane else {
            panic!("two markers stay exact");
        };
        let labels: Vec<&str> = markers.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["event_a", "event_b"]);
    }

    #[test]
    fn view_columns_map_plot_x_to_screen_columns_and_back() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };
        let view = ViewColumns::new(&time, (0.0, 100.0), 10);
        assert_eq!(view.grid.window(), (0, 100));
        assert_eq!(view.x(0), 0.0);
        assert_eq!(view.x(10), 100.0);
        assert_eq!(view.column_at(0.0), Some(0));
        assert_eq!(view.column_at(15.0), Some(1));
        assert_eq!(view.column_at(100.0), Some(9));
        assert_eq!(view.column_at(100.5), None);
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
                show(
                    ui,
                    &dataset,
                    &ticks,
                    &bool_bands,
                    &[],
                    &[],
                    &mut StateTimelineState::default(),
                );
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
                show(
                    ui,
                    &dataset,
                    &ticks,
                    &bool_bands,
                    &[],
                    &[],
                    &mut StateTimelineState::default(),
                );
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

    // Two changes at one timestamp: the zero-duration `true` between them
    // is an event, shown as the "multiple" glyph in its column, while the
    // columns around it keep their own single state.
    #[test]
    fn a_zero_duration_run_is_a_multiple_glyph_not_lost() {
        let lane = BoolLane::Exact {
            bands: bool_state_bands(&[false, true, false, false], &[0, 5, 5, 10]),
            min_tick: 0,
            max_tick: 10,
        };
        let grid = ColumnGrid::new(0, 10, 10);
        let drawn = Drawn::Cells {
            cells: lane.collapse(&grid),
            labels: Labels::new(),
        };
        assert_eq!(
            hover_text(LaneKind::Bool, &drawn, &grid, 5).as_deref(),
            Some("on and off both occur here — zoom in to see each change")
        );
        assert_eq!(
            hover_text(LaneKind::Bool, &drawn, &grid, 6).as_deref(),
            Some("off")
        );
        assert_eq!(
            hover_text(LaneKind::Bool, &drawn, &grid, 4).as_deref(),
            Some("off")
        );
    }

    // Issue #124: a final single-sample run starts at the axis end and used
    // to be skipped as zero-width; it must reach the last screen column.
    #[test]
    fn a_final_single_sample_bool_run_reaches_the_last_column() {
        let lane = BoolLane::Exact {
            bands: bool_state_bands(&[true, true, false], &[0, 1, 2]),
            min_tick: 0,
            max_tick: 2,
        };
        let cells = lane.collapse(&ColumnGrid::new(0, 2, 20));
        assert_eq!(cells[19], StateCell::Mixed, "on until 2, off at 2");
        assert_eq!(cells[0], StateCell::Single(BOOL_ON_ID));
    }

    #[test]
    fn several_events_in_one_column_say_how_many_and_one_names_itself() {
        let mut builder = MarkerBuilder::for_view(0, 100);
        for (tick, label) in [(1, "a"), (2, "b"), (50, "solo")] {
            builder.push(label, tick);
        }
        let lane = builder.finish_lane();
        let grid = ColumnGrid::new(0, 100, 10);
        let drawn = Drawn::Markers {
            counts: lane.collapse(&grid),
            lane: &lane,
        };
        assert_eq!(
            hover_text(LaneKind::Markers, &drawn, &grid, 0).as_deref(),
            Some("2 events here — zoom in to separate them")
        );
        assert_eq!(
            hover_text(LaneKind::Markers, &drawn, &grid, 5).as_deref(),
            Some("\"solo\"")
        );
        assert_eq!(hover_text(LaneKind::Markers, &drawn, &grid, 6), None);
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

    fn dense_dataset(len: usize) -> Dataset {
        let labels = ["a", "b", "c"];
        Dataset {
            time: TimeAxis::Progressive {
                values: (0..len).map(|i| i as f64).collect::<Vec<_>>().into(),
            },
            time_column_name: "index".to_string(),
            columns: vec![
                glyde_core::series::Series::new(
                    "flag",
                    SeriesValues::Bool((0..len).map(|i| i % 2 == 0).collect()),
                ),
                glyde_core::series::Series::new(
                    "state",
                    SeriesValues::String((0..len).map(|i| labels[i % 3].to_owned()).collect()),
                ),
            ],
        }
    }

    // SPEC §4.3 "expands on zoom": a lane too dense for exact runs, rebuilt
    // for a narrow window, gets back every run in it — read from only the
    // rows around the window on a time-ordered axis, or from all of them on
    // one that is not.
    #[test]
    fn refining_a_dense_lane_for_a_window_recovers_its_runs() {
        let len = glyde_core::series::MAX_EXACT_BOOL_BANDS * 3;
        let dataset = dense_dataset(len);
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let bools = cache_bool_bands(&dataset, &ticks);
        assert!(matches!(bools[0], Some(BoolLane::Overview { .. })));
        let window = (ticks[100], ticks[110]);
        let cancel = AtomicBool::new(false);
        for sorted in [true, false] {
            let Some(LaneData::Bool(BoolLane::Exact { bands, .. })) = refine_lane(
                &dataset,
                &ticks,
                sorted,
                0,
                LaneKind::Bool,
                window,
                64,
                &cancel,
            ) else {
                panic!("a window of ten rows holds exact runs (sorted: {sorted})");
            };
            assert_eq!(bands.len(), 12, "rows 99..=110 plus the one closing it");
            let Some(LaneData::State(StateLane::Exact { bands, .. })) = refine_lane(
                &dataset,
                &ticks,
                sorted,
                1,
                LaneKind::State,
                window,
                64,
                &cancel,
            ) else {
                panic!("string lane");
            };
            assert_eq!(bands[1].label, ["a", "b", "c"][100 % 3]);
        }
        assert!(
            refine_lane(
                &dataset,
                &ticks,
                true,
                0,
                LaneKind::State,
                window,
                64,
                &cancel
            )
            .is_none(),
            "a bool column has no state lane"
        );
    }

    #[test]
    fn a_cancelled_refinement_returns_nothing() {
        let dataset = dense_dataset(1000);
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let cancel = AtomicBool::new(true);
        let window = (ticks[0], ticks[999]);
        assert!(refine_lane(
            &dataset,
            &ticks,
            false,
            0,
            LaneKind::Bool,
            window,
            64,
            &cancel
        )
        .is_none());
    }

    #[test]
    fn refining_a_spilled_bool_column_reads_only_its_window() {
        use glyde_core::index::spill::{SpillSet, SpillVecWriter};

        let dir = tempfile::tempdir().expect("temp dir");
        let len = glyde_core::series::MAX_EXACT_BOOL_BANDS * 3;
        let mut writer =
            SpillVecWriter::<u8>::create(&SpillSet::create(dir.path()).expect("set"), "flag")
                .expect("create");
        for index in 0..len {
            writer.push((index % 2) as u8).expect("push");
        }
        let ticks: Vec<i128> = (0..len as i128).collect();
        let dataset = Dataset {
            time: TimeAxis::Progressive {
                values: ticks.iter().map(|&t| t as f64).collect::<Vec<_>>().into(),
            },
            time_column_name: "index".to_string(),
            columns: vec![glyde_core::series::Series::new(
                "flag",
                SeriesValues::Spilled(SpilledValues::Bool(writer.finish().expect("finish"))),
            )],
        };
        let lane = refine_lane(
            &dataset,
            &ticks,
            true,
            0,
            LaneKind::Bool,
            (500, 504),
            16,
            &AtomicBool::new(false),
        );
        let Some(LaneData::Bool(BoolLane::Exact { bands, .. })) = lane else {
            panic!("exact runs expected: {lane:?}");
        };
        assert_eq!(bands.first().map(|b| b.start_tick), Some(499));
        assert!(bands
            .iter()
            .all(|b| b.end_tick >= 500 && b.start_tick <= 505));
    }

    #[test]
    fn a_refinement_spans_a_screen_width_either_side_within_the_data() {
        let visible = ColumnGrid::new(1000, 2000, 500);
        assert_eq!(
            refine_request(&visible, (0, 10_000)),
            RefineRequest {
                window: (0, 3000),
                cells: 1500,
            }
        );
        assert_eq!(
            refine_request(&visible, (900, 2100)),
            RefineRequest {
                window: (900, 2100),
                cells: 600,
            }
        );
        let request = refine_request(&visible, (0, 10_000));
        assert!(visible.is_resolved_by(request.window, request.cells));
    }

    // The whole loop the UI runs: zoomed far into a dense lane, the first
    // frame draws the overview and starts a rebuild; once it lands, the
    // lane drawn is the rebuilt one, resolving every screen column.
    #[test]
    fn zooming_into_a_dense_lane_draws_the_rebuilt_view_once_ready() {
        let len = glyde_core::series::MAX_EXACT_BOOL_BANDS * 3;
        let dataset = Arc::new(dense_dataset(len));
        let ticks: Arc<Vec<i128>> = Arc::new(dataset.time.to_pyramid_ticks().into_owned());
        let base = cache_bool_bands(&dataset, &ticks)[0]
            .clone()
            .expect("bool lane");
        let mut state = StateTimelineState::default();
        state.set_refiner(Some(LaneRefiner::new({
            let (dataset, ticks) = (Arc::clone(&dataset), Arc::clone(&ticks));
            move |column, kind, window, cells, cancel| {
                refine_lane(&dataset, &ticks, true, column, kind, window, cells, cancel)
            }
        })));
        let ctx = egui::Context::default();
        let grid = ColumnGrid::new(ticks[2000], ticks[2040], 400);
        let key = (0, LaneKind::Bool);

        let first = state.lane_for(key, LaneRef::Bool(&base), &grid, &ctx);
        assert!(matches!(first, LaneRef::Bool(BoolLane::Overview { .. })));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let lane = state.lane_for(key, LaneRef::Bool(&base), &grid, &ctx);
            if let LaneRef::Bool(lane @ BoolLane::Exact { .. }) = lane {
                assert!(lane.resolves(&grid));
                let cells = lane.collapse(&grid);
                assert!(cells.contains(&StateCell::Single(BOOL_ON_ID)));
                assert!(cells.contains(&StateCell::Single(glyde_core::series::BOOL_OFF_ID)));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "rebuild never arrived"
            );
            std::thread::yield_now();
        }
        assert_eq!(state.refined_lanes(), 1);
        // New data drops every rebuilt lane.
        state.data_changed();
        assert_eq!(state.refined_lanes(), 0);
    }

    // Whole-file view of a dense lane: drawn from the prepared overview,
    // nothing to rebuild.
    #[test]
    fn a_resolved_view_starts_no_rebuild() {
        let len = glyde_core::series::MAX_EXACT_BOOL_BANDS * 3;
        let dataset = dense_dataset(len);
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let base = cache_bool_bands(&dataset, &ticks)[0]
            .clone()
            .expect("bool lane");
        let mut state = StateTimelineState::default();
        state.set_refiner(Some(LaneRefiner::new(|_, _, _, _, _| {
            panic!("a resolved view must not rebuild")
        })));
        let grid = ColumnGrid::new(ticks[0], ticks[len - 1], 800);
        let ctx = egui::Context::default();
        let lane = state.lane_for((0, LaneKind::Bool), LaneRef::Bool(&base), &grid, &ctx);
        assert!(matches!(lane, LaneRef::Bool(BoolLane::Overview { .. })));
        assert!(state.slots.values().all(|slot| slot.pending.is_none()));
    }
}
