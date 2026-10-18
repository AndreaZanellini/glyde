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

//! The state timeline's collapse rule (docs/SPEC.md §4.3, docs/ROADMAP.md
//! M6): "when multiple states collapse into one pixel column, render a
//! 'multiple states' glyph that expands on zoom. Losing an event is a bug."
//!
//! A lane is drawn one screen column at a time: [`ColumnGrid`] splits the
//! visible tick window into equal columns, and each lane collapses into one
//! [`StateCell`] per column — [`StateCell::Mixed`] wherever two or more
//! different states touch the same column — or, for markers, one event
//! count per column (above `1` is the "multiple events" glyph). Every state
//! and every event that touches the window lands in at least one column, so
//! nothing can be painted over by a neighbour drawn later.
//!
//! Zooming in narrows the window, so the same states spread over more
//! columns and a `Mixed` column resolves into its individual states. An
//! overview lane (too many runs to retain) can only resolve down to its own
//! cell width; [`ColumnGrid::is_resolved_by`] says when it is too coarse for
//! the screen, and the caller then rebuilds it for the visible window (the
//! builders' `for_window` constructors) to recover every run.

use super::categorical::{state_label_id, StateCell, StateLane};
use super::markers::MarkerLane;
use super::state::BoolLane;

/// The id a `false` sample collapses to in a bool lane's cells.
pub const BOOL_OFF_ID: u32 = 0;
/// The id a `true` sample collapses to in a bool lane's cells.
pub const BOOL_ON_ID: u32 = 1;

/// The visible tick window `[start, end]` split into `columns` equal screen
/// columns: column `k` holds `[k, k + 1) / columns` of the window, and the
/// last one also holds `end` itself — the same split the lane overviews use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnGrid {
    start: i128,
    end: i128,
    columns: usize,
}

impl ColumnGrid {
    /// A grid of at least one column; a reversed window is put in order.
    pub fn new(start: i128, end: i128, columns: usize) -> Self {
        Self {
            start: start.min(end),
            end: start.max(end),
            columns: columns.max(1),
        }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn window(&self) -> (i128, i128) {
        (self.start, self.end)
    }

    /// The column an instant at `tick` is drawn in, if it is in the window.
    pub fn column_of(&self, tick: i128) -> Option<usize> {
        self.instant_tick(tick).map(|(column, _)| column)
    }

    /// The part of this grid inside `[min, max]` (the data's own extent),
    /// with as many columns as that part spans on screen — the grid a lane
    /// must resolve, since nothing can be rebuilt where there is no data.
    /// `None` when the window shows no data at all.
    pub fn clipped(&self, (min, max): (i128, i128)) -> Option<ColumnGrid> {
        let (start, end) = (self.start.max(min), self.end.min(max));
        if start > end {
            return None;
        }
        let span = self.end.saturating_sub(self.start);
        if span == 0 {
            return Some(*self);
        }
        let share = end.saturating_sub(start) as f64 / span as f64;
        let columns = (self.columns as f64 * share).ceil() as usize;
        Some(ColumnGrid::new(start, end, columns))
    }

    /// Whether an overview built over `window` with `cells` cells is fine
    /// enough to draw this grid without hiding anything a rebuild would
    /// show: it covers the whole visible window and none of its cells is
    /// wider than one screen column. An exact lane always resolves.
    pub fn is_resolved_by(&self, (min, max): (i128, i128), cells: usize) -> bool {
        if min > self.start || max < self.end {
            return false;
        }
        let cell_span = max.saturating_sub(min) as f64 * self.columns as f64;
        let column_span = self.end.saturating_sub(self.start) as f64 * cells.max(1) as f64;
        // Equal widths compare equal up to rounding.
        cell_span <= column_span * (1.0 + 1e-12)
    }

    /// The columns touched by an instant at `tick`, in exact integer
    /// arithmetic (a tick on a column boundary belongs to the column it
    /// starts); falls back to [`Self::instant`] only on overflow.
    fn instant_tick(&self, tick: i128) -> Option<(usize, usize)> {
        let span = self.end.saturating_sub(self.start);
        let offset = tick.checked_sub(self.start);
        let n = self.columns as i128;
        match offset.and_then(|offset| Some((offset, offset.checked_mul(n)?))) {
            Some((offset, _)) if span == 0 => (offset == 0).then_some((0, self.columns - 1)),
            Some((offset, scaled)) => {
                if offset < 0 || offset > span {
                    return None;
                }
                let column = ((scaled / span) as usize).min(self.columns - 1);
                Some((column, column))
            }
            None => self.instant(self.offset(tick)),
        }
    }

    /// The columns touched by a state held over `[lo, hi)` (`lo < hi`), in
    /// exact integer arithmetic; falls back to [`Self::interval`] only on
    /// overflow.
    fn interval_ticks(&self, lo: i128, hi: i128) -> Option<(usize, usize)> {
        let span = self.end.saturating_sub(self.start);
        let n = self.columns as i128;
        let scaled = |tick: i128| tick.checked_sub(self.start)?.checked_mul(n);
        let (Some(from), Some(to)) = (scaled(lo), scaled(hi)) else {
            return self.interval(self.offset(lo), self.offset(hi));
        };
        if span == 0 {
            return (from <= 0 && 0 < to).then_some((0, self.columns - 1));
        }
        // Positions in units of `1 / span` columns: `[from, to)` touches
        // columns `floor(from / span) ..= ceil(to / span) - 1`.
        if to <= 0 || from > n * span {
            return None;
        }
        let first = (from.max(0) / span).min(n - 1);
        let last = ((to.min(n * span) + span - 1) / span - 1).clamp(first, n - 1);
        Some((first as usize, last as usize))
    }

    /// `tick`'s distance from the window start, in ticks. Subtracting in
    /// `i128` first keeps nanosecond epochs exact before the `f64` step.
    fn offset(&self, tick: i128) -> f64 {
        tick.saturating_sub(self.start) as f64
    }

    /// The columns `first..=last` touched by an instant at `offset` (ticks
    /// from the window start), if it is inside the window at all.
    fn instant(&self, offset: f64) -> Option<(usize, usize)> {
        let span = self.end.saturating_sub(self.start) as f64;
        if span <= 0.0 {
            // A one-tick window: every column shows that tick.
            return (offset == 0.0).then_some((0, self.columns - 1));
        }
        let position = offset / span * self.columns as f64;
        if !(0.0..=self.columns as f64).contains(&position) {
            return None;
        }
        let column = (position.floor() as usize).min(self.columns - 1);
        Some((column, column))
    }

    /// The columns touched by a state held over `[from, to)` (offsets from
    /// the window start, `from < to`); the last column also holds the
    /// window's end tick.
    fn interval(&self, from: f64, to: f64) -> Option<(usize, usize)> {
        let span = self.end.saturating_sub(self.start) as f64;
        if span <= 0.0 {
            return (from <= 0.0 && 0.0 < to).then_some((0, self.columns - 1));
        }
        let scale = self.columns as f64 / span;
        let (p0, p1) = (from * scale, to * scale);
        let n = self.columns as f64;
        if p1 <= 0.0 || p0 > n {
            return None;
        }
        let first = (p0.max(0.0).floor() as usize).min(self.columns - 1);
        let last = (p1.min(n).ceil() as usize)
            .saturating_sub(1)
            .clamp(first, self.columns - 1);
        Some((first, last))
    }
}

/// Per-column difference arrays: what [`collapse_runs`] and the overview
/// resampling add ranges to in constant time each, materialized once. A
/// column's ids are all equal iff `count * sum(id²) == sum(id)²` (equality
/// in Cauchy–Schwarz), exact for integer ids.
struct Columns {
    count: Vec<i128>,
    sum: Vec<i128>,
    sumsq: Vec<i128>,
    mixed: Vec<i64>,
}

impl Columns {
    fn new(columns: usize) -> Self {
        Self {
            count: vec![0; columns + 1],
            sum: vec![0; columns + 1],
            sumsq: vec![0; columns + 1],
            mixed: vec![0; columns + 1],
        }
    }

    fn add(&mut self, (first, last): (usize, usize), cell: StateCell) {
        match cell {
            StateCell::Empty => {}
            StateCell::Single(id) => {
                let id = i128::from(id);
                self.count[first] += 1;
                self.count[last + 1] -= 1;
                self.sum[first] += id;
                self.sum[last + 1] -= id;
                self.sumsq[first] += id * id;
                self.sumsq[last + 1] -= id * id;
            }
            StateCell::Mixed => {
                self.mixed[first] += 1;
                self.mixed[last + 1] -= 1;
            }
        }
    }

    fn finish(self) -> Vec<StateCell> {
        let columns = self.count.len() - 1;
        let (mut count, mut sum, mut sumsq, mut mixed) = (0i128, 0i128, 0i128, 0i64);
        (0..columns)
            .map(|index| {
                count += self.count[index];
                sum += self.sum[index];
                sumsq += self.sumsq[index];
                mixed += self.mixed[index];
                if mixed > 0 || count * sumsq != sum * sum {
                    StateCell::Mixed
                } else if count == 0 {
                    StateCell::Empty
                } else {
                    StateCell::Single((sum / count) as u32)
                }
            })
            .collect()
    }
}

/// Collapses runs `(start_tick, end_tick, id)` onto `grid`. A run holds
/// `[start_tick, end_tick)` — until the tick where the next run starts — and
/// a run with `start_tick == end_tick` is an instant at that tick (a state
/// that changed again at the same timestamp: zero duration, still an event
/// that must stay visible). A run with `end_tick < start_tick` (a source
/// whose timestamps run backwards, SPEC §2.1) covers both ticks.
pub fn collapse_runs(
    runs: impl IntoIterator<Item = (i128, i128, u32)>,
    grid: &ColumnGrid,
) -> Vec<StateCell> {
    let mut columns = Columns::new(grid.columns);
    for (start, end, id) in runs {
        let (lo, hi) = (start.min(end), start.max(end));
        let touched = if lo == hi {
            grid.instant_tick(lo)
        } else {
            grid.interval_ticks(lo, hi)
        };
        if let Some(range) = touched {
            columns.add(range, StateCell::Single(id));
        }
    }
    columns.finish()
}

/// Exact runs `(start, end, id)` with the final, still-open run held until
/// `max_tick`, as the lanes draw it.
fn exact_runs<'a, B: 'a>(
    bands: &'a [B],
    max_tick: i128,
    run: impl Fn(&B) -> (i128, i128, u32) + 'a,
) -> impl Iterator<Item = (i128, i128, u32)> + 'a {
    let last = bands.len().saturating_sub(1);
    bands.iter().enumerate().map(move |(index, band)| {
        let (start, end, id) = run(band);
        if index == last {
            (start, end.max(max_tick), id)
        } else {
            (start, end, id)
        }
    })
}

/// Resamples overview `cells` over the lane axis `[min_tick, max_tick]`
/// onto `grid`: cell `k` holds `[k, k + 1) / N` of the lane axis (the last
/// one also its end), and lands in every column it overlaps.
fn collapse_overview(
    cells: impl ExactSizeIterator<Item = StateCell>,
    (min_tick, max_tick): (i128, i128),
    grid: &ColumnGrid,
) -> Vec<StateCell> {
    let mut columns = Columns::new(grid.columns);
    let total = cells.len();
    let base = grid.offset(min_tick);
    let span = max_tick.saturating_sub(min_tick) as f64;
    for (index, cell) in cells.enumerate() {
        if cell == StateCell::Empty {
            continue;
        }
        if span <= 0.0 {
            // A one-tick lane keeps everything in its first cell.
            if let Some(range) = grid.instant(base) {
                columns.add(range, cell);
            }
            continue;
        }
        let from = base + span * index as f64 / total as f64;
        let to = base + span * (index + 1) as f64 / total as f64;
        if let Some(range) = grid.interval(from, to) {
            columns.add(range, cell);
        }
        if index + 1 == total {
            if let Some(range) = grid.instant(to) {
                columns.add(range, cell);
            }
        }
    }
    columns.finish()
}

impl BoolLane {
    /// This lane's state in every column of `grid` ([`BOOL_OFF_ID`] /
    /// [`BOOL_ON_ID`]). The final, still-open run holds until the lane's own
    /// `max_tick`, as it is drawn.
    pub fn collapse(&self, grid: &ColumnGrid) -> Vec<StateCell> {
        match self {
            BoolLane::Exact {
                bands, max_tick, ..
            } => collapse_runs(
                exact_runs(bands, *max_tick, |band| {
                    let id = if band.value { BOOL_ON_ID } else { BOOL_OFF_ID };
                    (band.start_tick, band.end_tick, id)
                }),
                grid,
            ),
            BoolLane::Overview {
                cells,
                min_tick,
                max_tick,
            } => collapse_overview(
                cells.iter().map(|&mask| match mask {
                    0 => StateCell::Empty,
                    1 => StateCell::Single(BOOL_OFF_ID),
                    2 => StateCell::Single(BOOL_ON_ID),
                    _ => StateCell::Mixed,
                }),
                (*min_tick, *max_tick),
                grid,
            ),
        }
    }

    /// Whether [`Self::collapse`] onto `grid` shows everything a rebuild
    /// for the visible window would.
    pub fn resolves(&self, grid: &ColumnGrid) -> bool {
        match self {
            BoolLane::Exact { .. } => true,
            BoolLane::Overview {
                cells,
                min_tick,
                max_tick,
            } => grid.is_resolved_by((*min_tick, *max_tick), cells.len()),
        }
    }
}

impl StateLane {
    /// This lane's label id ([`state_label_id`]) in every column of `grid`.
    pub fn collapse(&self, grid: &ColumnGrid) -> Vec<StateCell> {
        match self {
            StateLane::Exact {
                bands, max_tick, ..
            } => collapse_runs(
                exact_runs(bands, *max_tick, |band| {
                    (band.start_tick, band.end_tick, state_label_id(&band.label))
                }),
                grid,
            ),
            StateLane::Overview {
                cells,
                min_tick,
                max_tick,
                ..
            } => collapse_overview(cells.iter().copied(), (*min_tick, *max_tick), grid),
        }
    }

    /// See [`BoolLane::resolves`].
    pub fn resolves(&self, grid: &ColumnGrid) -> bool {
        match self {
            StateLane::Exact { .. } => true,
            StateLane::Overview {
                cells,
                min_tick,
                max_tick,
                ..
            } => grid.is_resolved_by((*min_tick, *max_tick), cells.len()),
        }
    }
}

impl MarkerLane {
    /// How many events fall in every column of `grid`. An overview cell's
    /// events are counted in the column holding the cell's center, where
    /// they are drawn.
    pub fn collapse(&self, grid: &ColumnGrid) -> Vec<u32> {
        let mut counts = vec![0u32; grid.columns];
        let mut add = |touched: Option<(usize, usize)>, count: u32| {
            if let Some((column, _)) = touched {
                counts[column] = counts[column].saturating_add(count);
            }
        };
        match self {
            MarkerLane::Exact { markers, .. } => {
                for marker in markers {
                    add(grid.instant_tick(marker.tick), 1);
                }
            }
            MarkerLane::Overview {
                counts: cells,
                min_tick,
                max_tick,
            } => {
                let base = grid.offset(*min_tick);
                let span = max_tick.saturating_sub(*min_tick) as f64;
                for (index, &count) in cells.iter().enumerate() {
                    if count > 0 {
                        let center = if span <= 0.0 {
                            0.0
                        } else {
                            span * (index as f64 + 0.5) / cells.len() as f64
                        };
                        add(grid.instant(base + center), count);
                    }
                }
            }
        }
        counts
    }

    /// See [`BoolLane::resolves`].
    pub fn resolves(&self, grid: &ColumnGrid) -> bool {
        match self {
            MarkerLane::Exact { .. } => true,
            MarkerLane::Overview {
                counts,
                min_tick,
                max_tick,
            } => grid.is_resolved_by((*min_tick, *max_tick), counts.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::series::{
        bool_state_bands, BoolBandBuilder, MarkerBuilder, StateBandBuilder, MAX_EXACT_BOOL_BANDS,
        MAX_EXACT_MARKERS, MAX_EXACT_STATE_BANDS,
    };
    use StateCell::{Empty, Mixed, Single};

    #[test]
    fn a_clipped_grid_keeps_the_screen_column_width() {
        let grid = ColumnGrid::new(0, 1000, 100);
        let clipped = grid.clipped((500, 2000)).expect("overlaps");
        assert_eq!(clipped.window(), (500, 1000));
        assert_eq!(clipped.columns(), 50);
        assert_eq!(grid.clipped((0, 1000)), Some(grid));
        assert_eq!(grid.clipped((1001, 2000)), None);
        // Panned past the end of the data, a lane built over the data still
        // resolves the part of the screen that shows any.
        assert!(clipped.is_resolved_by((0, 1000), 100));
    }

    #[test]
    fn a_run_wider_than_a_column_fills_exactly_its_columns() {
        // 10 columns of 10 ticks each over [0, 100].
        let grid = ColumnGrid::new(0, 100, 10);
        let cells = collapse_runs([(20, 50, 7)], &grid);
        assert_eq!(
            cells,
            [
                Empty,
                Empty,
                Single(7),
                Single(7),
                Single(7),
                Empty,
                Empty,
                Empty,
                Empty,
                Empty
            ]
        );
    }

    #[test]
    fn two_states_meeting_inside_a_column_make_it_mixed() {
        let grid = ColumnGrid::new(0, 100, 10);
        let cells = collapse_runs([(0, 25, 1), (25, 100, 2)], &grid);
        assert_eq!(cells[1], Single(1));
        assert_eq!(cells[2], Mixed, "the change at 25 is inside column 2");
        assert_eq!(cells[3], Single(2));
        assert_eq!(cells[9], Single(2));
    }

    // A run holds until the next one starts: a change exactly on a column
    // boundary belongs to the column it starts, so neither side is mixed.
    #[test]
    fn a_change_on_a_column_boundary_mixes_nothing() {
        let grid = ColumnGrid::new(0, 100, 10);
        let cells = collapse_runs([(0, 30, 1), (30, 100, 2)], &grid);
        assert_eq!(cells[2], Single(1));
        assert_eq!(cells[3], Single(2));
    }

    // The heart of SPEC §4.3: a state far shorter than a column must not be
    // painted over by its neighbours.
    #[test]
    fn a_sub_column_state_is_never_lost() {
        let grid = ColumnGrid::new(0, 1_000_000, 100);
        let cells = collapse_runs(
            [
                (0, 500_000, 1),
                (500_000, 500_001, 2),
                (500_001, 1_000_000, 1),
            ],
            &grid,
        );
        assert_eq!(cells[50], Mixed);
        assert_eq!(cells.iter().filter(|&&c| c == Mixed).count(), 1);
        assert!(cells
            .iter()
            .enumerate()
            .all(|(i, &c)| i == 50 || c == Single(1)));
    }

    // Two changes at the same timestamp: the middle state has no duration
    // but is still an event.
    #[test]
    fn a_zero_duration_state_still_marks_its_column() {
        let grid = ColumnGrid::new(0, 100, 10);
        let cells = collapse_runs([(0, 40, 1), (40, 40, 2), (40, 100, 1)], &grid);
        assert_eq!(cells[4], Mixed);
        assert_eq!(cells[3], Single(1));
        assert_eq!(cells[5], Single(1));
    }

    #[test]
    fn runs_outside_the_window_touch_nothing() {
        let grid = ColumnGrid::new(100, 200, 10);
        let cells = collapse_runs([(0, 100, 1), (250, 300, 2)], &grid);
        assert!(cells.iter().all(|&c| c == Empty), "{cells:?}");
    }

    #[test]
    fn a_run_covering_the_whole_window_fills_every_column() {
        let grid = ColumnGrid::new(100, 200, 10);
        let cells = collapse_runs([(0, 1_000, 3)], &grid);
        assert!(cells.iter().all(|&c| c == Single(3)));
    }

    // The window's last column also holds its end tick: a single sample
    // there (a final run of one sample) must show.
    #[test]
    fn an_instant_at_the_window_end_lands_in_the_last_column() {
        let grid = ColumnGrid::new(0, 100, 10);
        let cells = collapse_runs([(0, 100, 1), (100, 100, 2)], &grid);
        assert_eq!(cells[9], Mixed);
    }

    #[test]
    fn a_backwards_run_covers_both_of_its_ticks() {
        let grid = ColumnGrid::new(0, 100, 10);
        let cells = collapse_runs([(80, 20, 5)], &grid);
        assert_eq!(cells[2..=7], [Single(5); 6]);
        assert_eq!(cells[1], Empty);
        assert_eq!(cells[8], Empty);
    }

    #[test]
    fn a_one_tick_window_collects_every_state_at_that_tick() {
        let grid = ColumnGrid::new(5, 5, 3);
        assert_eq!(collapse_runs([(5, 5, 1)], &grid), [Single(1); 3]);
        assert_eq!(collapse_runs([(5, 5, 1), (5, 5, 2)], &grid), [Mixed; 3]);
        assert_eq!(collapse_runs([(6, 6, 1)], &grid), [Empty; 3]);
    }

    // Exhaustive guarantee: whatever the zoom (column count), every state of
    // an alternating sequence is present in some column, and each column
    // reports exactly the set of states touching it.
    #[test]
    fn no_state_is_lost_at_any_column_count() {
        let runs: Vec<(i128, i128, u32)> = (0..97)
            .map(|i| (i * 13, (i + 1) * 13, (i % 3) as u32))
            .collect();
        for columns in 1..300 {
            let grid = ColumnGrid::new(0, 97 * 13, columns);
            let cells = collapse_runs(runs.iter().copied(), &grid);
            for (index, &(start, _, id)) in runs.iter().enumerate() {
                let column = (start as usize * columns / (97 * 13)).min(columns - 1);
                assert!(
                    matches!(cells[column], Mixed) || cells[column] == Single(id),
                    "run {index} lost at {columns} columns: {:?}",
                    cells[column]
                );
            }
            assert!(cells.iter().all(|&c| c != Empty));
            if columns == 97 {
                // One run per column, every change on a column boundary:
                // nothing is mixed.
                assert!(cells.iter().all(|&c| c != Mixed), "{columns}: {cells:?}");
            }
        }
    }

    #[test]
    fn an_exact_bool_lane_extends_its_last_run_to_the_lane_end() {
        let lane = BoolLane::Exact {
            bands: bool_state_bands(&[true, false], &[0, 50]),
            min_tick: 0,
            max_tick: 100,
        };
        let cells = lane.collapse(&ColumnGrid::new(0, 100, 4));
        assert_eq!(
            cells,
            [
                Single(BOOL_ON_ID),
                Single(BOOL_ON_ID),
                Single(BOOL_OFF_ID),
                Single(BOOL_OFF_ID)
            ]
        );
        assert!(lane.resolves(&ColumnGrid::new(10, 20, 10_000)));
    }

    fn dense_bool_lane(len: usize) -> BoolLane {
        let mut builder = BoolBandBuilder::for_view(0, len as i128 - 1);
        for index in 0..len {
            builder.push(index % 2 == 0, index as i128);
        }
        builder.finish_lane()
    }

    #[test]
    fn a_dense_bool_overview_collapses_to_mixed_columns_and_asks_for_a_rebuild_when_zoomed() {
        let len = MAX_EXACT_BOOL_BANDS * 4;
        let lane = dense_bool_lane(len);
        assert!(matches!(lane, BoolLane::Overview { .. }));
        let whole = ColumnGrid::new(0, len as i128 - 1, 1000);
        assert!(lane.collapse(&whole).iter().all(|&c| c == Mixed));
        assert!(lane.resolves(&whole), "2048 cells cover 1000 columns");
        let zoomed = ColumnGrid::new(0, 100, 1000);
        assert!(
            !lane.resolves(&zoomed),
            "one cell is far wider than a column"
        );
    }

    // The expand-on-zoom half of the rule: rebuilding the lane for a
    // narrow window recovers its individual runs.
    #[test]
    fn a_window_rebuild_of_a_dense_lane_recovers_every_run() {
        let len = MAX_EXACT_BOOL_BANDS * 4;
        let mut builder = BoolBandBuilder::for_window(1000, 1010, 4096);
        for index in 0..len {
            builder.push(index % 2 == 0, index as i128);
        }
        let lane = builder.finish_lane();
        let BoolLane::Exact { bands, .. } = &lane else {
            panic!("a narrow window holds few runs: {lane:?}");
        };
        assert!(bands
            .iter()
            .all(|band| band.end_tick >= 1000 && band.start_tick <= 1010));
        let grid = ColumnGrid::new(1000, 1010, 10);
        assert!(lane.resolves(&grid));
        let cells = lane.collapse(&grid);
        for (column, cell) in cells.iter().enumerate().take(9) {
            let on = (1000 + column) % 2 == 0;
            assert_eq!(*cell, Single(u32::from(on)), "column {column}");
        }
        // The last column also holds the window's end tick, 1010, where the
        // state changes again: both states really are in it.
        assert_eq!(cells[9], Mixed);
    }

    #[test]
    fn a_window_overview_is_built_only_from_runs_in_the_window() {
        let len = MAX_EXACT_BOOL_BANDS * 4;
        let mut builder = BoolBandBuilder::for_window(0, 9_999, 100);
        for index in 0..len {
            // Alternating in the window, constant `true` after it.
            builder.push(index >= 10_000 || index % 2 == 0, index as i128);
        }
        let lane = builder.finish_lane();
        let BoolLane::Overview { cells, .. } = &lane else {
            panic!("10 000 alternations do not fit: {lane:?}");
        };
        assert_eq!(cells.len(), 100);
        assert!(cells.iter().all(|&mask| mask == 3));
        assert!(lane.resolves(&ColumnGrid::new(0, 9_999, 100)));
        assert!(!lane.resolves(&ColumnGrid::new(0, 9_999, 101)));
        assert!(
            !lane.resolves(&ColumnGrid::new(0, 20_000, 10)),
            "not covered"
        );
    }

    #[test]
    fn a_state_lane_collapses_to_label_ids() {
        let mut builder = StateBandBuilder::for_view(0, 30);
        for (tick, label) in [(0, "idle"), (10, "run"), (20, "idle"), (30, "idle")] {
            builder.push(label, tick);
        }
        let lane = builder.finish_lane();
        let (idle, run) = (state_label_id("idle"), state_label_id("run"));
        assert_eq!(
            lane.collapse(&ColumnGrid::new(0, 30, 3)),
            [Single(idle), Single(run), Single(idle)]
        );
        assert_eq!(lane.collapse(&ColumnGrid::new(0, 30, 1)), [Mixed]);
    }

    #[test]
    fn a_dense_state_lane_expands_on_a_window_rebuild() {
        let len = MAX_EXACT_STATE_BANDS * 3;
        let labels = ["a", "b", "c"];
        let mut whole = StateBandBuilder::for_view(0, len as i128 - 1);
        let mut window = StateBandBuilder::for_window(300, 330, 64);
        for index in 0..len {
            whole.push(labels[index % 3], index as i128);
            window.push(labels[index % 3], index as i128);
        }
        let whole = whole.finish_lane();
        assert!(matches!(whole, StateLane::Overview { .. }));
        let grid = ColumnGrid::new(300, 330, 30);
        assert!(!whole.resolves(&grid));
        let window = window.finish_lane();
        assert!(window.resolves(&grid));
        let cells = window.collapse(&grid);
        assert_eq!(cells[29], Mixed, "the change at the end tick 330");
        for (column, cell) in cells.iter().enumerate().take(29) {
            assert_eq!(
                *cell,
                Single(state_label_id(labels[(300 + column) % 3])),
                "column {column}"
            );
        }
    }

    #[test]
    fn markers_sharing_a_column_are_counted_not_dropped() {
        let mut builder = MarkerBuilder::for_view(0, 100);
        for tick in [5, 7, 8, 55, 100] {
            builder.push("e", tick);
        }
        let lane = builder.finish_lane();
        let counts = lane.collapse(&ColumnGrid::new(0, 100, 10));
        assert_eq!(counts, [3, 0, 0, 0, 0, 1, 0, 0, 0, 1]);
        assert!(lane.resolves(&ColumnGrid::new(0, 1, 1000)));
        // Zoomed in, they separate.
        assert_eq!(
            lane.collapse(&ColumnGrid::new(0, 10, 10)),
            [0, 0, 0, 0, 0, 1, 0, 1, 1, 0]
        );
    }

    #[test]
    fn a_marker_overview_keeps_every_event_and_expands_on_a_window_rebuild() {
        let total = MAX_EXACT_MARKERS * 3;
        let mut whole = MarkerBuilder::for_view(0, total as i128 - 1);
        let mut window = MarkerBuilder::for_window(100, 199, 100);
        for tick in 0..total as i128 {
            whole.push("e", tick);
            window.push("e", tick);
        }
        let whole = whole.finish_lane();
        let grid = ColumnGrid::new(0, total as i128 - 1, 500);
        let counts = whole.collapse(&grid);
        assert_eq!(counts.iter().map(|&c| c as usize).sum::<usize>(), total);
        assert!(whole.resolves(&grid));
        let zoomed = ColumnGrid::new(100, 199, 100);
        assert!(!whole.resolves(&zoomed));
        let window = window.finish_lane();
        assert!(matches!(window, MarkerLane::Exact { ref markers, .. } if markers.len() == 100));
        assert_eq!(window.collapse(&zoomed), vec![1; 100]);
    }
}
