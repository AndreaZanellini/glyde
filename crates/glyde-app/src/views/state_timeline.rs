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
//! states, or two or more events, is drawn as a gray "multiple" block or
//! tick whose tooltip says so, never as whichever state happened to be drawn
//! last. Zooming in spreads the same states over more columns until each
//! shows on its own. A lane
//! too dense to keep every run is a bounded overview, so when one of its
//! cells is wider than a screen column it is rebuilt for the visible window
//! off the UI thread ([`LaneRefiner`]) and redrawn when ready.
//!
//! Time alignment (SPEC §4.1, §4.3: one synchronized time axis): the lanes
//! are stacked under [`super::time`]'s plot ([`plot_above_lanes`]) and share
//! its x axis through [`super::time::TIME_AXIS_LINK_GROUP`] — panning,
//! zooming, a fit or a jump in the plot or in any lane moves all of them —
//! and each lane is laid out over exactly the plot's frame
//! ([`super::time::SharedAxis`]), so a band sits under the samples of its
//! time.
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

use super::time::{
    axis_bounds, seconds_to_tick, tick_to_seconds, SharedAxis, TIME_AXIS_LINK_GROUP,
};

/// Height, in points, of a text-state lane (issue #230, the approved
/// mockup's `LANE_H.state`): tall enough for its band labels.
const STATE_LANE_HEIGHT: f32 = 22.0;
/// Height, in points, of an on/off lane: a thin strip.
const BOOL_LANE_HEIGHT: f32 = 12.0;
/// Height, in points, of an event-marker lane.
const MARKER_LANE_HEIGHT: f32 = 22.0;
/// Height, in points, of the state group's header row — all that is left
/// of the group when it is collapsed.
const GROUP_HEADER_HEIGHT: f32 = 22.0;

/// The most of the time view the state group (its header and its lanes)
/// takes from the plot above it (issue #230, the approved mockup's
/// `STATE_CAP`); more lanes than fit scroll inside the group.
pub const STATE_GROUP_SHARE: f32 = 0.32;

/// The state group header's tooltip: what the "multiple" glyph means and how
/// to expand it. It used to be a permanent line above the lanes (issue
/// #200).
const COLLAPSE_LEGEND: &str = "A gray mark: several states or events share one screen column — \
     zoom in to see each one (Ctrl/⌘ + scroll or pinch to zoom, drag to pan). \
     The lanes move with the plot above. Double-click to show the whole file.\n\
     Click to collapse or expand the state channels.";

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
    /// How tall the state group was last frame, in points; `None` before
    /// it was first drawn.
    height: Option<f32>,
    /// How tall the whole time view was this frame ([`plot_above_lanes`]),
    /// which the group's share is of.
    view_height: Option<f32>,
    /// Whether the user collapsed the group to its header row.
    collapsed: bool,
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

    /// How tall the time plot above the lanes is, out of `available`
    /// points: what the state group does not take — as tall as last frame,
    /// at most [`STATE_GROUP_SHARE`] of it — less `reserved` for the rows
    /// that are always there (the time axis at the bottom).
    pub fn time_plot_height(&self, available: f32, reserved: f32) -> f32 {
        let lanes = self
            .height
            .unwrap_or(0.0)
            .min(available * STATE_GROUP_SHARE);
        (available - lanes - reserved).max(0.0)
    }

    /// Collapses the state group to its header row (`true`) or opens it.
    pub fn set_collapsed(&mut self, collapsed: bool) {
        if self.collapsed != collapsed {
            tracing::info!(collapsed, "state channel group collapsed or expanded");
        }
        self.collapsed = collapsed;
    }

    /// Whether the state group is collapsed to its header row.
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
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

/// Lays out `plot` — the time view — in `ui` above the state group drawn
/// next with [`show`] and the time axis drawn last with
/// [`super::time::show_time_axis`], leaving the group the room it took last
/// frame ([`StateTimelineState::time_plot_height`]) and the axis its row:
/// stacked, so all three are on screen, the axis at the bottom.
pub fn plot_above_lanes<R>(
    ui: &mut egui::Ui,
    lanes: &mut StateTimelineState,
    plot: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let available = ui.available_height();
    lanes.view_height = Some(available);
    let spacing = ui.spacing().item_spacing.y;
    let reserved = super::time::TIME_AXIS_HEIGHT + 3.0 * spacing;
    let height = lanes.time_plot_height(available, reserved);
    let width = ui.available_width();
    ui.allocate_ui(egui::vec2(width, height), |ui| {
        ui.set_min_size(egui::vec2(width, height));
        plot(ui)
    })
    .inner
}

/// How tall a lane of `kind` is.
fn lane_height(kind: LaneKind) -> f32 {
    match kind {
        LaneKind::Bool => BOOL_LANE_HEIGHT,
        LaneKind::State => STATE_LANE_HEIGHT,
        LaneKind::Markers => MARKER_LANE_HEIGHT,
    }
}

/// The state group's header row: how many state channels there are, and a
/// click to collapse the group to this row or open it again. Its tooltip
/// explains the "multiple" glyph.
fn group_header(ui: &mut egui::Ui, count: usize, state: &mut StateTimelineState) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), GROUP_HEADER_HEIGHT),
        egui::Sense::click(),
    );
    let response = response.on_hover_text(COLLAPSE_LEGEND);
    if response.clicked() {
        state.set_collapsed(!state.collapsed);
    }
    // The arrow, drawn in its own 8-point box: pointing right when
    // collapsed, down when open. (egui's `paint_default_icon` sizes it from
    // the response's rect, which here is the whole header row.)
    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        egui::vec2(8.0, 8.0),
    );
    let points = if state.collapsed {
        vec![icon.left_top(), icon.right_center(), icon.left_bottom()]
    } else {
        vec![icon.left_top(), icon.right_top(), icon.center_bottom()]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        ui.style().interact(&response).fg_stroke.color,
        egui::Stroke::NONE,
    ));
    let plural = if count == 1 { "" } else { "s" };
    ui.painter().text(
        egui::pos2(rect.left() + 20.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{count} state channel{plural}"),
        egui::TextStyle::Small.resolve(ui.style()),
        ui.visuals().text_color(),
    );
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
///
/// `shared` is the time plot's axis this frame
/// ([`super::time::TimeViewState::shared_axis`]): the lanes fit to its span
/// and are laid out over its frame. `None` (no plot) fits them to their own
/// span, full width. Lanes that do not fit the room left under the plot
/// scroll.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    dataset: &Dataset,
    bool_bands: &[Option<BoolLane>],
    state_lanes: &[Option<StateLane>],
    marker_lanes: &[Option<MarkerColumn>],
    shared: Option<SharedAxis>,
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
        state.height = Some(0.0);
        return;
    };
    let axis = shared.map_or_else(|| axis_bounds(min_tick, max_tick, time), |s| s.fit);
    let count = (0..dataset.columns.len())
        .filter(|&index| {
            [
                bool_bands.get(index).is_some_and(Option::is_some),
                state_lanes.get(index).is_some_and(Option::is_some),
                marker_lanes.get(index).is_some_and(Option::is_some),
            ]
            .contains(&true)
        })
        .count();
    let spacing = ui.spacing().item_spacing.y;
    // At most its share of the time view; this frame, also never over the
    // time axis under it (the plot above makes the room next frame).
    let view = state.view_height.unwrap_or_else(|| ui.available_height());
    let share = (view * STATE_GROUP_SHARE - GROUP_HEADER_HEIGHT - 2.0 * spacing).max(0.0);
    let room =
        ui.available_height() - super::time::TIME_AXIS_HEIGHT - GROUP_HEADER_HEIGHT - 3.0 * spacing;
    group_header(ui, count, state);
    let mut height = GROUP_HEADER_HEIGHT;
    if !state.collapsed {
        let output = egui::ScrollArea::vertical()
            .id_salt("state_timeline_lanes")
            .auto_shrink([false, true])
            .max_height(share.min(room).max(0.0))
            .show(ui, |ui| {
                show_lanes(
                    ui,
                    dataset,
                    bool_bands,
                    state_lanes,
                    marker_lanes,
                    axis,
                    shared,
                    state,
                );
            });
        // What the group wants, not what fitted this frame: measuring the
        // clipped height would keep the plot from ever giving room back.
        height += spacing + output.content_size.y.min(share);
    }
    if state.height.is_none_or(|last| (last - height).abs() > 0.5) {
        // The plot above takes its room from this: lay it out again.
        state.height = Some(height);
        ui.ctx().request_repaint();
    }
}

/// Every lane, inside [`show`]'s scroll area: one row each, its name in
/// the gutter left of the plot's frame and the lane over exactly that frame.
#[allow(clippy::too_many_arguments)]
fn show_lanes(
    ui: &mut egui::Ui,
    dataset: &Dataset,
    bool_bands: &[Option<BoolLane>],
    state_lanes: &[Option<StateLane>],
    marker_lanes: &[Option<MarkerColumn>],
    axis: (f64, f64),
    shared: Option<SharedAxis>,
    state: &mut StateTimelineState,
) {
    let time = &dataset.time;
    for (index, series) in dataset.columns.iter().enumerate() {
        let states = state_lanes.get(index).and_then(Option::as_ref);
        let markers = marker_lanes.get(index).and_then(Option::as_ref);
        let bools = bool_bands.get(index).and_then(Option::as_ref);
        // A text column that can be drawn either way gets the switch in its
        // gutter; every other lane is what it is.
        let choice = match (states, markers, bools) {
            (Some(states), Some(markers), _) => LaneChoice::Switchable(states, markers),
            (None, Some(markers), _) => LaneChoice::Fixed(LaneRef::Markers(&markers.lane)),
            (Some(states), None, _) => LaneChoice::Fixed(LaneRef::State(states)),
            (None, None, Some(bools)) => LaneChoice::Fixed(LaneRef::Bool(bools)),
            (None, None, None) => continue,
        };
        let row_left = ui.cursor().left();
        // Over exactly the plot's frame: the same x, the same pixel. With no
        // plot, the gutter is as wide as the plot's would be.
        let (left, width) = match shared {
            Some(shared) => (shared.left, shared.width),
            None => (
                row_left + super::time::NAME_GUTTER,
                (ui.available_width() - super::time::NAME_GUTTER).max(1.0),
            ),
        };
        ui.horizontal(|ui| {
            let gutter = (left - row_left - ui.spacing().item_spacing.x).max(0.0);
            let lane = ui
                .allocate_ui_with_layout(
                    egui::vec2(gutter, GROUP_HEADER_HEIGHT),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.set_width(gutter);
                        let lane = match choice {
                            LaneChoice::Switchable(states, markers) => {
                                if string_lane_toggle(ui, index, series.name(), markers) {
                                    LaneRef::Markers(&markers.lane)
                                } else {
                                    LaneRef::State(states)
                                }
                            }
                            LaneChoice::Fixed(lane) => lane,
                        };
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(series.name()).small())
                                    .truncate(),
                            );
                        });
                        lane
                    },
                )
                .inner;
            ui.add_space((left - ui.cursor().left()).max(0.0));
            show_lane(ui, index, lane, time, axis, width, state);
        });
    }
}

/// What a column's lane is: fixed, or a text column's two views, one of
/// which its gutter switch picks.
#[derive(Clone, Copy)]
enum LaneChoice<'a> {
    Fixed(LaneRef<'a>),
    Switchable(&'a StateLane, &'a MarkerColumn),
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

/// Draws one lane as a horizontally zoomable plot sharing the time plot's
/// x axis, `width` points wide, collapsing it onto this frame's screen
/// columns. It has no axis of its own: the time axis is the one at the
/// bottom of the time view ([`super::time::show_time_axis`]).
fn show_lane(
    ui: &mut egui::Ui,
    index: usize,
    base: LaneRef<'_>,
    time: &TimeAxis,
    axis: (f64, f64),
    width: f32,
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
    let palette = crate::theme::palette(ui.visuals());
    let response = Plot::new(id)
        .height(lane_height(kind))
        .width(width)
        .show_axes([false, false])
        .show_x(false)
        .show_y(false)
        .show_grid([false, false])
        .allow_zoom([true, false])
        .allow_scroll([true, false])
        .allow_drag([true, false])
        .allow_boxed_zoom(false)
        .link_axis(TIME_AXIS_LINK_GROUP, [true, false])
        // What the first frame and a double-click fit: the whole file — the
        // span the time plot fits too — and the lane's fixed unit height.
        // Never set per frame, which would undo the zoom the plot or
        // another lane applied this frame.
        .include_x(axis.0)
        .include_x(axis.1)
        .include_y(0.0)
        .include_y(1.0)
        .set_margin_fraction(egui::Vec2::ZERO)
        .show(ui, |plot_ui| {
            let x_range = if plot_ui.auto_bounds().x {
                axis
            } else {
                let bounds = plot_ui.plot_bounds();
                (bounds.min()[0], bounds.max()[0])
            };
            let columns = plot_ui.transform().frame().width().round().max(1.0) as usize;
            let view = ViewColumns::new(time, x_range, columns);
            let ctx = plot_ui.ctx().clone();
            let lane = state.lane_for((index, kind), base, &view.grid, &ctx);
            let seconds_per_point = (view.x1 - view.x0).abs() / columns as f64;
            let drawn = match lane {
                LaneRef::Bool(lane) => {
                    let cells = lane.collapse(&view.grid);
                    // The mockup's on/off strip: on is a filled strip, off a
                    // hairline through the middle.
                    draw_cells(plot_ui, &view, &cells, palette, |id| {
                        if id == BOOL_ON_ID {
                            Fill::Band(palette.bool_on)
                        } else {
                            Fill::Hairline(palette.line)
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
                    draw_cells(plot_ui, &view, &cells, palette, |id| {
                        Fill::Band(state_tint(palette, id))
                    });
                    draw_labels(plot_ui, &view, &cells, &labels, seconds_per_point, palette);
                    Drawn::Cells { cells, labels }
                }
                LaneRef::Markers(markers) => {
                    let counts = markers.collapse(&view.grid);
                    draw_markers(plot_ui, &view, &counts, palette);
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
            x_range
        });
    // The columns were laid out for the range this lane had before this
    // frame's pan or zoom — its own, or the plot's or another lane's,
    // linked in after drawing: draw the next frame for the range now shown,
    // once it is off by a visible amount (half a screen column), never for
    // float rounding, which would redraw forever.
    let bounds = response.transform.bounds();
    let shown = (bounds.min()[0], bounds.max()[0]);
    if is_visibly_different(response.inner, shown, response.transform.frame().width()) {
        ui.ctx().request_repaint();
    }
    #[cfg(test)]
    {
        let frame = response.transform.frame();
        let view = LaneView {
            x_range: shown,
            left: frame.left(),
            width: frame.width(),
            top: response.response.rect.top(),
            bottom: response.response.rect.bottom(),
        };
        ui.ctx()
            .data_mut(|data| data.insert_temp(lane_view_id(index), view));
    }
    if let (Some(text), true) = (hover, response.response.hovered()) {
        response.response.on_hover_text(text);
    }
}

/// Whether x range `drawn` would put anything half a screen column or more
/// away from where x range `shown` puts it, over a `width`-point frame.
fn is_visibly_different(drawn: (f64, f64), shown: (f64, f64), width: f32) -> bool {
    let half_column = (shown.1 - shown.0).abs() / f64::from(width.max(1.0)) / 2.0;
    (drawn.0 - shown.0).abs() >= half_column || (drawn.1 - shown.1).abs() >= half_column
}

/// How one state's run is drawn.
#[derive(Debug, Clone, Copy)]
enum Fill {
    /// A band filling the lane's height.
    Band(egui::Color32),
    /// A thin line through the lane's middle (an "off" stretch).
    Hairline(egui::Color32),
}

/// Fills each run of equal columns, `Mixed` ones as the "multiple" glyph.
fn draw_cells(
    plot_ui: &mut egui_plot::PlotUi,
    view: &ViewColumns,
    cells: &[StateCell],
    palette: &crate::theme::Palette,
    fill: impl Fn(u32) -> Fill,
) {
    for (first, end, cell) in cell_runs(cells) {
        let (x0, x1) = (view.x(first), view.x(end));
        match cell {
            StateCell::Empty => {}
            StateCell::Single(id) => match fill(id) {
                Fill::Band(color) => fill_rect(plot_ui, x0, x1, (0.0, 1.0), color),
                Fill::Hairline(color) => fill_rect(plot_ui, x0, x1, (0.45, 0.55), color),
            },
            StateCell::Mixed => fill_rect(plot_ui, x0, x1, (0.0, 1.0), palette.ink_3),
        }
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
    palette: &crate::theme::Palette,
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
                    egui::RichText::new(*label).small().color(palette.ink),
                )
                .anchor(egui::Align2::CENTER_CENTER),
            );
        }
    }
}

/// One full-height tick per column holding any event (SPEC §4.3: "always
/// visible regardless of zoom"), in the "multiple" color and wider where a
/// column holds several.
fn draw_markers(
    plot_ui: &mut egui_plot::PlotUi,
    view: &ViewColumns,
    counts: &[u32],
    palette: &crate::theme::Palette,
) {
    for (column, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let x = (view.x(column) + view.x(column + 1)) / 2.0;
        let (color, width) = if count > 1 {
            (palette.ink_3, 3.0_f32)
        } else {
            (palette.ink, 2.0_f32)
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

/// Fills plot-x `x0..x1` between lane heights `y0..y1` (0 is the lane's
/// bottom, 1 its top).
fn fill_rect(
    plot_ui: &mut egui_plot::PlotUi,
    x0: f64,
    x1: f64,
    (y0, y1): (f64, f64),
    color: egui::Color32,
) {
    plot_ui.polygon(
        Polygon::new(PlotPoints::new(vec![
            [x0, y0],
            [x1, y0],
            [x1, y1],
            [x0, y1],
        ]))
        .fill_color(color)
        .stroke(egui::Stroke::NONE),
    );
}

/// Where lane `index` was drawn on the last frame, for the tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct LaneView {
    x_range: (f64, f64),
    left: f32,
    width: f32,
    top: f32,
    bottom: f32,
}

#[cfg(test)]
fn lane_view_id(index: usize) -> egui::Id {
    egui::Id::new(("lane_view", index))
}

#[cfg(test)]
fn lane_view(ctx: &egui::Context, index: usize) -> Option<LaneView> {
    ctx.data(|data| data.get_temp(lane_view_id(index)))
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

/// The switch of a `string` column that can be drawn either way, in its
/// lane's gutter: it names what is drawn and changes it in one click, and
/// its tooltip says what was inferred and why. Returns whether to draw the
/// column as markers. The choice starts at the inferred view and is
/// remembered per column for the session (`egui` temp memory).
fn string_lane_toggle(ui: &mut egui::Ui, index: usize, name: &str, markers: &MarkerColumn) -> bool {
    let inferred = markers.inferred_as_markers();
    let id = egui::Id::new(("string_lane_as_markers", index, name, markers.rows));
    let mut as_markers = ui.data_mut(|data| *data.get_temp_mut_or(id, inferred));
    let (label, tooltip) = string_lane_switch(inferred, as_markers, markers);
    if ui
        .add(egui::Button::new(egui::RichText::new(label).small()).small())
        .on_hover_text(tooltip)
        .clicked()
    {
        as_markers = !as_markers;
        ui.data_mut(|data| data.insert_temp(id, as_markers));
        tracing::info!(
            column = name,
            as_markers,
            inferred_as_markers = inferred,
            "user switched how a text column is drawn"
        );
    }
    as_markers
}

/// A switchable `string` lane's switch: its label (what is drawn) and its
/// tooltip ([`string_lane_notice`], then what a click does).
fn string_lane_switch(
    inferred: bool,
    as_markers: bool,
    markers: &MarkerColumn,
) -> (&'static str, String) {
    let (label, other) = if as_markers {
        ("events", "state bands")
    } else {
        ("bands", "events")
    };
    (
        label,
        format!(
            "{}\nClick to draw as {other}",
            string_lane_notice(inferred, as_markers, markers)
        ),
    )
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

/// The tint under state `label_id`'s bands — one of the theme's six, the
/// same for the same state everywhere and across files, with an ink label
/// on top that stays readable on each (checked in `crate::theme`).
fn state_tint(palette: &crate::theme::Palette, label_id: u32) -> egui::Color32 {
    palette.state_tints[label_id as usize % palette.state_tints.len()]
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

    /// One frame of the time plot stacked above `dataset`'s lanes, laid
    /// out the way the app does it, in an 800 x 600 window.
    fn stacked_frame(
        ctx: &egui::Context,
        dataset: &Dataset,
        time_state: &mut super::super::time::TimeViewState,
        lanes_state: &mut StateTimelineState,
        focus: Option<(f64, f64)>,
    ) {
        time_view_frame(
            ctx,
            dataset,
            time_state,
            lanes_state,
            focus,
            egui::vec2(800.0, 600.0),
            None,
        );
    }

    /// One frame of the whole time view — the plot, the state group under
    /// it and the one time axis at the bottom — laid out the way the app
    /// does it, in a `size` window, with the pointer at `pointer`.
    fn time_view_frame(
        ctx: &egui::Context,
        dataset: &Dataset,
        time_state: &mut super::super::time::TimeViewState,
        lanes_state: &mut StateTimelineState,
        focus: Option<(f64, f64)>,
        size: egui::Vec2,
        pointer: Option<egui::Pos2>,
    ) -> egui::FullOutput {
        let ticks = dataset.time.to_pyramid_ticks().into_owned();
        let pyramids = glyde_core::ingest::pyramids_for_dataset(dataset);
        let samples = super::super::time::cache_column_samples(dataset);
        let bools = cache_bool_bands(dataset, &ticks);
        let states = cache_state_lanes(dataset, &ticks);
        let markers = cache_marker_lanes(dataset, &ticks);
        let cursor = super::super::time::CursorLookup::for_time(&dataset.time);
        let fit = super::super::time::FitBounds::for_dataset(
            dataset,
            &pyramids,
            &ticks,
            &samples,
            &[],
            cursor.is_sorted(),
        );
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events: pointer
                .map(|pos| vec![egui::Event::PointerMoved(pos)])
                .unwrap_or_default(),
            ..Default::default()
        };
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                plot_above_lanes(ui, lanes_state, |ui| {
                    super::super::time::show_with_selection(
                        ui,
                        dataset,
                        &pyramids,
                        &ticks,
                        &samples,
                        &[],
                        &cursor,
                        &fit,
                        time_state,
                        focus,
                    )
                });
                show(
                    ui,
                    dataset,
                    &bools,
                    &states,
                    &markers,
                    time_state.shared_axis(),
                    lanes_state,
                );
                super::super::time::show_time_axis(ui, &dataset.time, &ticks, &fit, time_state);
            });
        })
    }

    /// One numeric column, then `bools` on/off columns and `texts` text
    /// columns with a state on every row, over 200 one-second rows.
    fn layout_dataset(bools: usize, texts: usize) -> Dataset {
        let rows = 200;
        let mut columns = vec![Series::new(
            "speed",
            SeriesValues::F64((0..rows).map(|row| (row % 17) as f64).collect()),
        )];
        for lane in 0..bools {
            columns.push(Series::new(
                format!("flag{lane}"),
                SeriesValues::Bool((0..rows).map(|row| (row / (lane + 3)) % 2 == 0).collect()),
            ));
        }
        for lane in 0..texts {
            columns.push(Series::new(
                format!("mode{lane}"),
                SeriesValues::String(
                    (0..rows)
                        .map(|row| ["IDLE", "RUNNING", "FAULT"][(row / (lane + 20)) % 3].to_owned())
                        .collect(),
                ),
            ));
        }
        Dataset {
            time: TimeAxis::Absolute {
                timestamps: (0..rows as i64)
                    .map(|second| {
                        Timestamp::new(1_700_000_000 + i128::from(second), TimeUnit::Seconds)
                    })
                    .collect::<Vec<_>>()
                    .into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns,
        }
    }

    /// Every text painted in `output`, with where it was painted.
    fn painted_texts(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        fn walk(shape: &egui::Shape, texts: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, texts)),
                egui::Shape::Text(text) => texts.push((
                    text.galley.text().to_owned(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                )),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut texts);
        }
        texts
    }

    const LAPTOP: egui::Vec2 = egui::vec2(1440.0, 900.0);

    /// Issue #230's "done when": at 1440 x 900, with 0, 4 and 20 state
    /// channels, the numeric plot keeps its share of the time view; the
    /// state group never takes more than [`STATE_GROUP_SHARE`] of it, and
    /// the one time axis is the last track, inside the window.
    #[test]
    fn the_plot_keeps_its_share_at_1440x900_with_0_4_and_20_state_channels() {
        for (bools, texts) in [(0, 0), (2, 2), (10, 10)] {
            let channels = bools + texts;
            let dataset = layout_dataset(bools, texts);
            let ctx = egui::Context::default();
            let mut time_state = super::super::time::TimeViewState::default();
            let mut lanes_state = StateTimelineState::default();
            for _ in 0..4 {
                time_view_frame(
                    &ctx,
                    &dataset,
                    &mut time_state,
                    &mut lanes_state,
                    None,
                    LAPTOP,
                    None,
                );
            }

            let plot = time_state.shared_axis().expect("the plot is drawn").frame;
            let axis = super::super::time::time_axis_rect(&ctx).expect("the time axis is drawn");
            let group = lanes_state.height.expect("the group was measured");
            assert!(
                group <= STATE_GROUP_SHARE * LAPTOP.y,
                "{channels} channels: the state group takes {group} points"
            );
            let least = if channels == 0 { 0.8 } else { 0.55 };
            assert!(
                plot.height() >= least * LAPTOP.y,
                "{channels} channels: the plot keeps only {} of {} points",
                plot.height(),
                LAPTOP.y
            );
            assert!(
                axis.top() >= plot.bottom() + group - 1.0 && axis.bottom() <= LAPTOP.y,
                "{channels} channels: the axis {axis:?} is the last track (plot {plot:?})"
            );
            // The group got the room it wants: every lane fits when they
            // can (4), its whole share when they cannot (20). Measuring the
            // group by what fitted on the first frame — the plot had taken
            // everything then — would leave it a sliver for good.
            let view = lanes_state.view_height.expect("the view was measured");
            if channels == 4 {
                for index in 1..=channels {
                    let lane = lane_view(&ctx, index).expect("the lane is drawn");
                    assert!(
                        lane.top >= plot.bottom() && lane.bottom <= axis.top(),
                        "lane {index} is fully on screen: {lane:?}, plot {plot:?}, axis {axis:?}"
                    );
                }
            }
            if channels == 20 {
                assert!(
                    group >= STATE_GROUP_SHARE * view - 6.0,
                    "the group uses its share: {group} of {view}"
                );
            }
        }
    }

    // Issue #200: the time-axis labels were repeated under every lane. Now
    // only the time axis at the bottom has them: no text is painted twice,
    // and the axis track carries the time labels.
    #[test]
    fn the_time_labels_are_painted_once_on_the_bottom_axis() {
        let dataset = layout_dataset(4, 0);
        let ctx = egui::Context::default();
        let mut time_state = super::super::time::TimeViewState::default();
        let mut lanes_state = StateTimelineState::default();
        let mut output = None;
        for _ in 0..4 {
            output = Some(time_view_frame(
                &ctx,
                &dataset,
                &mut time_state,
                &mut lanes_state,
                None,
                LAPTOP,
                None,
            ));
        }
        let texts = painted_texts(&output.unwrap());
        let axis = super::super::time::time_axis_rect(&ctx).expect("the time axis is drawn");
        let mut seen = std::collections::HashSet::new();
        // A numeric channel's name is in the channel picker and left of its
        // stacked plot (issue #198): exactly twice, and not a time label.
        assert_eq!(
            texts.iter().filter(|(text, _)| text == "speed").count(),
            2,
            "\"speed\" in the picker and beside its plot: {texts:?}"
        );
        for (text, _) in texts
            .iter()
            .filter(|(text, _)| !text.trim().is_empty() && text != "speed")
        {
            assert!(seen.insert(text.as_str()), "{text:?} is painted twice");
        }
        for name in ["flag0", "flag1", "flag2", "flag3"] {
            assert!(seen.contains(name), "{name} is on screen: {texts:?}");
        }
        let on_axis = texts
            .iter()
            .filter(|(text, rect)| !text.is_empty() && axis.y_range().contains(rect.center().y))
            .count();
        assert!(on_axis >= 2, "the axis {axis:?} has its labels: {texts:?}");
    }

    // Issue #230: compact lanes — text states 22 points, booleans 12 — laid
    // over exactly the plot's frame, with their names in the gutter to its
    // left instead of a header row each.
    #[test]
    fn lanes_are_compact_with_their_names_in_the_gutter() {
        let dataset = layout_dataset(1, 1);
        let ctx = egui::Context::default();
        let mut time_state = super::super::time::TimeViewState::default();
        let mut lanes_state = StateTimelineState::default();
        let mut output = None;
        for _ in 0..4 {
            output = Some(time_view_frame(
                &ctx,
                &dataset,
                &mut time_state,
                &mut lanes_state,
                None,
                LAPTOP,
                None,
            ));
        }
        let shared = time_state.shared_axis().expect("the plot is drawn");
        assert!(shared.left >= super::super::time::NAME_GUTTER);
        for (index, name, height) in [
            (1, "flag0", BOOL_LANE_HEIGHT),
            (2, "mode0", STATE_LANE_HEIGHT),
        ] {
            let lane = lane_view(&ctx, index).expect("the lane is drawn");
            assert_eq!(lane.bottom - lane.top, height, "{name}");
            assert_eq!(
                (lane.left, lane.width),
                (shared.left, shared.width),
                "{name}"
            );
            let texts = painted_texts(output.as_ref().unwrap());
            let (_, rect) = texts
                .iter()
                .find(|(text, _)| text == name)
                .unwrap_or_else(|| panic!("{name} is named: {texts:?}"));
            assert!(rect.right() <= shared.left, "{name} sits in the gutter");
            assert!(
                rect.center().y >= lane.top - 4.0 && rect.center().y <= lane.bottom + 4.0,
                "{name} is on its lane's row"
            );
        }
    }

    /// Every fill color painted in `output`.
    fn painted_fills(output: &egui::FullOutput) -> Vec<egui::Color32> {
        fn walk(shape: &egui::Shape, fills: &mut Vec<egui::Color32>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, fills)),
                egui::Shape::Path(path) => fills.push(path.fill),
                egui::Shape::Rect(rect) => fills.push(rect.fill),
                _ => {}
            }
        }
        let mut fills = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut fills);
        }
        fills
    }

    // Issue #230 / #229: the lanes are drawn in the approved mockup's
    // tokens, in both themes — a strip in `bool_on` for on, pale tints under
    // text states, which are now tall enough to carry them.
    #[test]
    fn lanes_are_drawn_in_the_themes_state_tokens() {
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            let dataset = layout_dataset(1, 1);
            let ctx = egui::Context::default();
            ctx.set_visuals(crate::theme::visuals(theme));
            let palette = *crate::theme::palette(&ctx.style().visuals);
            let mut time_state = super::super::time::TimeViewState::default();
            let mut lanes_state = StateTimelineState::default();
            let mut output = None;
            for _ in 0..4 {
                output = Some(time_view_frame(
                    &ctx,
                    &dataset,
                    &mut time_state,
                    &mut lanes_state,
                    None,
                    LAPTOP,
                    None,
                ));
            }
            let fills = painted_fills(output.as_ref().unwrap());
            assert!(fills.contains(&palette.bool_on), "{theme:?}: on strip");
            assert!(
                palette.state_tints.iter().any(|tint| fills.contains(tint)),
                "{theme:?}: state tints"
            );
        }
    }

    // The header's arrow is a small triangle left of its label, open or
    // collapsed. It used to be sized from the whole header row, so the
    // collapsed (rotated) arrow became a white needle ~1000 points tall
    // through the plot (seen at 1440 x 900 on the engine showcase file).
    #[test]
    fn the_group_arrow_stays_in_its_icon_box() {
        fn triangles(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| triangles(s, out)),
                egui::Shape::Path(path) if path.points.len() == 3 => {
                    out.push(egui::Rect::from_points(&path.points));
                }
                _ => {}
            }
        }
        let dataset = layout_dataset(2, 2);
        for collapsed in [false, true] {
            let ctx = egui::Context::default();
            let mut time_state = super::super::time::TimeViewState::default();
            let mut lanes_state = StateTimelineState::default();
            lanes_state.set_collapsed(collapsed);
            let mut output = None;
            for _ in 0..4 {
                output = Some(time_view_frame(
                    &ctx,
                    &dataset,
                    &mut time_state,
                    &mut lanes_state,
                    None,
                    LAPTOP,
                    None,
                ));
            }
            let mut found = Vec::new();
            for clipped in &output.unwrap().shapes {
                triangles(&clipped.shape, &mut found);
            }
            assert!(
                !found.is_empty(),
                "collapsed {collapsed}: the arrow is drawn"
            );
            for rect in found {
                assert!(
                    rect.width() <= 12.0 && rect.height() <= 12.0,
                    "collapsed {collapsed}: a {rect:?} triangle"
                );
            }
        }
    }

    // Issue #230: the group collapses to its one header row, which gives the
    // room back to the plot. The "multiple" glyph's explanation is the header's
    // tooltip, no longer a line of its own (issue #200).
    #[test]
    fn the_state_group_collapses_to_one_row() {
        let dataset = layout_dataset(2, 2);
        let ctx = egui::Context::default();
        let mut time_state = super::super::time::TimeViewState::default();
        let mut lanes_state = StateTimelineState::default();
        let run = |time_state: &mut _, lanes_state: &mut _| {
            let mut output = None;
            for _ in 0..4 {
                output = Some(time_view_frame(
                    &ctx,
                    &dataset,
                    time_state,
                    lanes_state,
                    None,
                    LAPTOP,
                    None,
                ));
            }
            painted_texts(&output.unwrap())
        };
        let texts = run(&mut time_state, &mut lanes_state);
        let open_plot = time_state.shared_axis().unwrap().frame.height();
        assert!(
            texts.iter().any(|(t, _)| t.contains("4 state channels")),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|(t, _)| t.contains("gray mark")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|(t, _)| t == "flag0"));

        lanes_state.set_collapsed(true);
        let texts = run(&mut time_state, &mut lanes_state);
        let group = lanes_state.height.unwrap();
        assert!(
            group <= GROUP_HEADER_HEIGHT + 1.0,
            "collapsed to {group} points"
        );
        assert!(time_state.shared_axis().unwrap().frame.height() > open_plot);
        assert!(
            texts.iter().any(|(t, _)| t.contains("4 state channels")),
            "{texts:?}"
        );
        assert!(!texts.iter().any(|(t, _)| t == "flag0"), "{texts:?}");
    }

    // Issue #200: the cursor readout appearing under the pointer used to
    // push every lane down a row. Its row is reserved under the plot: the
    // readout is painted inside it when hovering, nothing moves, and the
    // row is there (empty) when not.
    #[test]
    fn hovering_the_plot_moves_no_track() {
        let dataset = layout_dataset(2, 0);
        let ctx = egui::Context::default();
        let mut time_state = super::super::time::TimeViewState::default();
        let mut lanes_state = StateTimelineState::default();
        let mut frame = |pointer| {
            let mut output = None;
            for _ in 0..4 {
                output = Some(time_view_frame(
                    &ctx,
                    &dataset,
                    &mut time_state,
                    &mut lanes_state,
                    None,
                    LAPTOP,
                    pointer,
                ));
            }
            let plot = time_state.shared_axis().expect("plot drawn").frame;
            (
                plot,
                lane_view(&ctx, 1).expect("lane drawn"),
                super::super::time::time_axis_rect(&ctx).expect("axis drawn"),
                painted_texts(output.as_ref().unwrap()),
            )
        };
        let readout = |texts: &[(String, egui::Rect)]| {
            texts
                .iter()
                .find(|(text, _)| text.contains("speed: "))
                .map(|(_, rect)| *rect)
        };
        let (plot, lane, axis, texts) = frame(None);
        assert_eq!(readout(&texts), None, "no readout without the pointer");
        let row = lane.top - plot.bottom();
        assert!(
            row >= ui_text_height(&ctx),
            "the readout's row is reserved between the plot and the group: {row} points"
        );

        let (hovered_plot, hovered_lane, hovered_axis, hovered_texts) =
            frame(Some(egui::pos2(700.0, 300.0)));
        let shown = readout(&hovered_texts).expect("the readout is painted while hovering");
        assert!(
            shown.top() >= plot.bottom() - 1.0 && shown.bottom() <= lane.top,
            "the readout {shown:?} sits in its row, between the plot {plot:?} and the lanes"
        );
        assert_eq!(
            (plot, lane.top, axis),
            (hovered_plot, hovered_lane.top, hovered_axis)
        );
    }

    fn assert_close(actual: (f64, f64), expected: (f64, f64)) {
        assert!(
            (actual.0 - expected.0).abs() < 1e-9 && (actual.1 - expected.1).abs() < 1e-9,
            "{actual:?} != {expected:?}"
        );
    }

    // SPEC §4.1/§4.3: one synchronized time axis. A lane shows exactly the
    // span the plot above it shows — the whole file at first, the zoomed
    // window after a jump — over exactly the same screen columns.
    /// The height of one line of body text.
    fn ui_text_height(ctx: &egui::Context) -> f32 {
        ctx.style().text_styles[&egui::TextStyle::Body].size
    }

    #[test]
    fn lanes_share_the_time_plots_axis_and_frame() {
        let dataset = sample_dataset();
        let ctx = egui::Context::default();
        let mut time_state = super::super::time::TimeViewState::default();
        let mut lanes_state = StateTimelineState::default();
        for _ in 0..3 {
            stacked_frame(&ctx, &dataset, &mut time_state, &mut lanes_state, None);
        }

        let shared = time_state.shared_axis().expect("the plot shares its axis");
        assert_close(shared.fit, (0.0, 3.0));
        assert_close(shared.x_range, shared.fit);
        let lane = lane_view(&ctx, 0).expect("the bool lane is drawn");
        assert_close(lane.x_range, shared.x_range);
        assert_eq!((lane.left, lane.width), (shared.left, shared.width));

        stacked_frame(
            &ctx,
            &dataset,
            &mut time_state,
            &mut lanes_state,
            Some((1.0, 2.0)),
        );
        for _ in 0..2 {
            stacked_frame(&ctx, &dataset, &mut time_state, &mut lanes_state, None);
        }

        let shared = time_state.shared_axis().expect("the plot shares its axis");
        assert_close(shared.x_range, (1.0, 2.0));
        let lane = lane_view(&ctx, 0).expect("the bool lane is drawn");
        assert_close(lane.x_range, (1.0, 2.0));
        assert_eq!((lane.left, lane.width), (shared.left, shared.width));
    }

    // Stacked means visible: the plot leaves the lanes the room they took
    // on the frame before, so they are on screen under it.
    #[test]
    fn the_lanes_stay_inside_the_window_under_the_plot() {
        let dataset = sample_dataset();
        let ctx = egui::Context::default();
        let mut time_state = super::super::time::TimeViewState::default();
        let mut lanes_state = StateTimelineState::default();
        for _ in 0..3 {
            stacked_frame(&ctx, &dataset, &mut time_state, &mut lanes_state, None);
        }

        let lane = lane_view(&ctx, 0).expect("the bool lane is drawn");
        assert!(lane.bottom <= 600.0, "lane ends at {}", lane.bottom);
        assert!(lane.top > 300.0, "the plot keeps most of the window");
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
                    &lanes,
                    &[],
                    &[],
                    None,
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
                    &bool_bands,
                    &[],
                    &[],
                    None,
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
                    &[],
                    &lanes,
                    &[],
                    None,
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
                    &[],
                    &state,
                    &markers,
                    None,
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
        // The compact lane's switch says what is drawn — the inferred view
        // until the user picks the other one — and is one click.
        assert_eq!(
            texts.lines().filter(|t| *t == "events").count(),
            1,
            "{texts}"
        );
        assert_eq!(
            texts.lines().filter(|t| *t == "bands").count(),
            1,
            "{texts}"
        );
    }

    // The switch's tooltip carries the evidence the header used to print.
    #[test]
    fn the_lane_switch_names_what_is_drawn_and_explains_the_inference() {
        let column = MarkerColumn {
            lane: MarkerLane::Exact {
                markers: Vec::new(),
                min_tick: 0,
                max_tick: 1,
            },
            non_empty: 2,
            rows: 9,
        };
        assert_eq!(
            string_lane_switch(true, true, &column),
            (
                "events",
                "Drawn as events (inferred: 2 of 9 rows have text)\nClick to draw as state bands"
                    .to_string()
            )
        );
        assert_eq!(
            string_lane_switch(true, false, &column),
            (
                "bands",
                "Drawn as state bands (your choice; inferred events: 2 of 9 rows have text)\n\
                 Click to draw as events"
                    .to_string()
            )
        );
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
                    &bool_bands,
                    &[],
                    &[],
                    None,
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
                    &bool_bands,
                    &[],
                    &[],
                    None,
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
    fn a_lane_redraws_only_for_a_visible_range_change() {
        // 1000 seconds over 1000 points: a column is a second.
        let shown = (0.0, 1000.0);

        assert!(!is_visibly_different((0.0, 1000.0), shown, 1000.0));
        assert!(!is_visibly_different((1e-7, 1000.0 - 1e-7), shown, 1000.0));
        assert!(is_visibly_different((0.5, 1000.0), shown, 1000.0));
        assert!(is_visibly_different((0.0, 990.0), shown, 1000.0));
    }

    #[test]
    fn the_plot_takes_the_window_less_the_lanes_and_the_reserved_rows() {
        let state = StateTimelineState {
            height: Some(120.0),
            ..StateTimelineState::default()
        };

        assert_eq!(state.time_plot_height(600.0, 60.0), 420.0);
    }

    #[test]
    fn the_state_group_never_takes_more_than_its_share_of_the_view() {
        let state = StateTimelineState {
            height: Some(2000.0),
            ..StateTimelineState::default()
        };

        assert_eq!(
            state.time_plot_height(1000.0, 60.0),
            1000.0 - 1000.0 * STATE_GROUP_SHARE - 60.0
        );
    }

    #[test]
    fn before_the_lanes_were_measured_the_plot_keeps_the_window() {
        let state = StateTimelineState::default();

        assert_eq!(state.time_plot_height(600.0, 20.0), 580.0);
        assert_eq!(state.time_plot_height(10.0, 20.0), 0.0);
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
