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

//! Markers (docs/SPEC.md §4.3, docs/ROADMAP.md M6): single samples at an
//! instant, drawn as tick glyphs on their own lane and never decimated away.
//!
//! A *marker column* is a `string` column in which most cells are empty and
//! the few non-empty cells name an event (corpus case 49: `event_a`,
//! `event_b` on otherwise blank rows). SPEC §4.3 does not say how a marker
//! column is told apart from a categorical state column, so this is an
//! assumption (listed in CHANGELOG.md): a string column with at least one
//! non-empty cell, in which at most half of the cells are non-empty, is a
//! marker column. Anything denser is a state column, not handled here.

/// At most this many markers are kept individually (with their labels).
/// A denser marker column falls back to a fixed-size per-cell count, so
/// memory stays bounded however many events a file holds.
pub const MAX_EXACT_MARKERS: usize = 4096;
/// Number of cells in the bounded whole-file overview of a very dense marker
/// column.
pub const MARKER_OVERVIEW_CELLS: usize = 2048;

/// One event: the tick of its sample and the verbatim cell text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub tick: i128,
    pub label: String,
}

/// A marker column prepared for drawing over a fixed `[min_tick, max_tick]`
/// axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerLane {
    /// Every marker, in file order.
    Exact {
        markers: Vec<Marker>,
        min_tick: i128,
        max_tick: i128,
    },
    /// Too many markers to keep individually: `counts[i]` is the number of
    /// markers in the `i`-th cell of the axis. A nonzero cell must always be
    /// drawn, and a cell above `1` as a "multiple events" glyph.
    Overview {
        counts: Vec<u32>,
        min_tick: i128,
        max_tick: i128,
    },
}

impl MarkerLane {
    pub fn tick_bounds(&self) -> (i128, i128) {
        match self {
            MarkerLane::Exact {
                min_tick, max_tick, ..
            }
            | MarkerLane::Overview {
                min_tick, max_tick, ..
            } => (*min_tick, *max_tick),
        }
    }
}

/// Whether a string column of `total` cells, `non_empty` of which hold text,
/// is a marker column (see the module docs for the rule).
pub fn is_marker_column(non_empty: usize, total: usize) -> bool {
    non_empty > 0 && non_empty.saturating_mul(2) <= total
}

/// Builds a [`MarkerLane`] one cell at a time, so a spilled column can be
/// streamed through it without being collected first (SPEC §5.1).
pub struct MarkerBuilder {
    min_tick: i128,
    max_tick: i128,
    markers: Vec<Marker>,
    counts: Vec<u32>,
    summarized: bool,
}

impl MarkerBuilder {
    pub fn for_view(min_tick: i128, max_tick: i128) -> Self {
        Self::for_window(min_tick, max_tick, MARKER_OVERVIEW_CELLS)
    }

    /// [`Self::for_view`] over just the window `[min_tick, max_tick]`, with a
    /// `cells`-cell overview: events outside the window are skipped, so a
    /// zoomed-in view can recover the individual markers of a dense column.
    pub fn for_window(min_tick: i128, max_tick: i128, cells: usize) -> Self {
        Self {
            min_tick,
            max_tick,
            markers: Vec::new(),
            counts: vec![0; cells.max(1)],
            summarized: false,
        }
    }

    fn cell(&self, tick: i128) -> usize {
        if self.max_tick <= self.min_tick {
            return 0;
        }
        let span = self.max_tick.saturating_sub(self.min_tick) as f64;
        let offset = tick.saturating_sub(self.min_tick) as f64;
        // Cell `k` holds `[k, k + 1) / N` of the axis (the last one also
        // holds `max_tick`); `glyde-app` draws each tick at its cell's center.
        let cells = self.counts.len();
        ((offset / span) * cells as f64)
            .floor()
            .clamp(0.0, (cells - 1) as f64) as usize
    }

    /// Feeds one cell. An empty cell is not an event and is ignored.
    pub fn push(&mut self, text: &str, tick: i128) {
        if text.is_empty() || tick < self.min_tick || tick > self.max_tick {
            return;
        }
        let cell = self.cell(tick);
        self.counts[cell] = self.counts[cell].saturating_add(1);
        if !self.summarized {
            self.markers.push(Marker {
                tick,
                label: text.to_owned(),
            });
            if self.markers.len() > MAX_EXACT_MARKERS {
                self.markers = Vec::new();
                self.summarized = true;
            }
        }
    }

    pub fn finish_lane(self) -> MarkerLane {
        if self.summarized {
            MarkerLane::Overview {
                counts: self.counts,
                min_tick: self.min_tick,
                max_tick: self.max_tick,
            }
        } else {
            MarkerLane::Exact {
                markers: self.markers,
                min_tick: self.min_tick,
                max_tick: self.max_tick,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_text_is_a_marker_column() {
        assert!(is_marker_column(2, 9));
        assert!(!is_marker_column(1, 1));
        assert!(is_marker_column(1, 2));
    }

    #[test]
    fn dense_or_empty_columns_are_not_marker_columns() {
        assert!(!is_marker_column(0, 9));
        assert!(!is_marker_column(5, 9));
        assert!(!is_marker_column(9, 9));
    }

    // Corpus case 49's shape: blank cells are not events, text cells are,
    // and each keeps its own tick and verbatim label.
    #[test]
    fn only_non_empty_cells_become_markers() {
        let mut builder = MarkerBuilder::for_view(0, 8);
        for (tick, text) in ["", "", "event_a", "", "", "", "", "event_b", ""]
            .iter()
            .enumerate()
        {
            builder.push(text, tick as i128);
        }
        assert_eq!(
            builder.finish_lane(),
            MarkerLane::Exact {
                markers: vec![
                    Marker {
                        tick: 2,
                        label: "event_a".into()
                    },
                    Marker {
                        tick: 7,
                        label: "event_b".into()
                    },
                ],
                min_tick: 0,
                max_tick: 8,
            }
        );
    }

    #[test]
    fn a_single_marker_is_kept() {
        let mut builder = MarkerBuilder::for_view(5, 5);
        builder.push("only", 5);
        let MarkerLane::Exact { markers, .. } = builder.finish_lane() else {
            panic!("one marker must stay exact");
        };
        assert_eq!(markers.len(), 1);
    }

    // SPEC §4.3: "Losing an event is a bug." Past the exact cap, every
    // marker must still be accounted for in some nonzero overview cell.
    #[test]
    fn a_flood_of_markers_keeps_every_event_in_the_overview() {
        let total = MAX_EXACT_MARKERS * 3;
        let mut builder = MarkerBuilder::for_view(0, total as i128);
        for tick in 0..total {
            builder.push("e", tick as i128);
        }
        let MarkerLane::Overview { counts, .. } = builder.finish_lane() else {
            panic!("expected an overview past the exact cap");
        };
        assert_eq!(counts.len(), MARKER_OVERVIEW_CELLS);
        assert_eq!(counts.iter().map(|&c| c as usize).sum::<usize>(), total);
    }

    #[test]
    fn a_lone_marker_in_a_huge_axis_survives_in_its_own_cell() {
        let mut builder = MarkerBuilder::for_view(0, 1_000_000);
        for tick in 0..=(MAX_EXACT_MARKERS as i128) {
            builder.push("e", tick);
        }
        builder.push("late", 1_000_000);
        let MarkerLane::Overview { counts, .. } = builder.finish_lane() else {
            panic!("expected an overview");
        };
        assert_eq!(counts[MARKER_OVERVIEW_CELLS - 1], 1);
    }

    #[test]
    fn overview_cells_split_the_axis_into_equal_fractions() {
        let max = (MARKER_OVERVIEW_CELLS * 1000) as i128;
        let mut builder = MarkerBuilder::for_view(0, max);
        for tick in 0..=(MAX_EXACT_MARKERS as i128) {
            builder.push("e", tick);
        }
        builder.push("late", 3 * max / 4 + 10);
        let MarkerLane::Overview { counts, .. } = builder.finish_lane() else {
            panic!("expected an overview");
        };
        let three_quarters = MARKER_OVERVIEW_CELLS * 3 / 4;
        assert_eq!(counts[three_quarters], 1);
        assert_eq!(counts[three_quarters - 1], 0);
    }
}
