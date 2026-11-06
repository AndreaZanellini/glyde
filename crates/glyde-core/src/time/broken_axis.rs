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

//! The compressed ("broken") time axis (docs/SPEC.md §2.3, docs/ROADMAP.md M8).
//!
//! A **view-state-only** coordinate map over plot seconds: every gap is
//! collapsed to a fixed narrow width, everything between gaps keeps its true
//! scale. It never touches a timestamp, a sample or any DSP input (PSD always
//! uses real timestamps); the app applies it to *where things are drawn* and
//! inverts it for *what is under the pointer / in view*.
//!
//! The map is strictly increasing and continuous, and [`BrokenAxis::expand`]
//! is its exact inverse.

/// SPEC §2.3: the compress affordance appears when detected gaps account for
/// more than this share of the total time span.
pub const OFFER_GAP_SHARE: f64 = 0.5;

/// A collapsed gap's width as a share of the time that is *not* gap, so it is
/// narrow next to the data on any file.
const GAP_WIDTH_SHARE: f64 = 0.02;

/// The share of `span` (start, end; plot seconds) covered by `gaps`
/// (start, end), which must be ordered and disjoint. `0.0` for an empty span.
pub fn gap_share(gaps: &[(f64, f64)], span: (f64, f64)) -> f64 {
    let total = span.1 - span.0;
    if total.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return 0.0;
    }
    let covered: f64 = gaps.iter().map(|&(start, end)| end - start).sum();
    covered / total
}

/// Whether the one-click compress affordance is offered (SPEC §2.3: gaps
/// account for strictly more than 50% of the total span).
pub fn should_offer_compression(gaps: &[(f64, f64)], span: (f64, f64)) -> bool {
    gap_share(gaps, span) > OFFER_GAP_SHARE
}

/// One collapsed gap.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Collapsed {
    /// The gap in true plot seconds.
    start: f64,
    end: f64,
    /// Time removed by every gap *before* this one.
    removed_before: f64,
    /// Its width once collapsed.
    width: f64,
}

impl Collapsed {
    /// Where the collapsed gap starts, in compressed coordinates.
    fn x_start(&self) -> f64 {
        self.start - self.removed_before
    }
}

/// The compressed coordinate map. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct BrokenAxis {
    gaps: Vec<Collapsed>,
}

impl BrokenAxis {
    /// Builds the map for `gaps` (start, end in plot seconds; ordered,
    /// disjoint, each `start < end`) over a data `span`. `None` when there is
    /// no gap to collapse or the input is not ordered and disjoint — the
    /// caller then simply has no compressed mode, never a wrong one.
    pub fn new(gaps: &[(f64, f64)], span: (f64, f64)) -> Option<Self> {
        if gaps.is_empty() {
            return None;
        }
        let mut previous_end = f64::NEG_INFINITY;
        for &(start, end) in gaps {
            if !(start.is_finite() && end.is_finite() && start < end && start >= previous_end) {
                return None;
            }
            previous_end = end;
        }
        let covered: f64 = gaps.iter().map(|&(start, end)| end - start).sum();
        let clusters = (span.1 - span.0 - covered).max(0.0);
        let nominal = if clusters > 0.0 {
            clusters * GAP_WIDTH_SHARE
        } else {
            1.0
        };
        let mut removed_before = 0.0;
        let collapsed = gaps
            .iter()
            .map(|&(start, end)| {
                let width = nominal.min(end - start);
                let gap = Collapsed {
                    start,
                    end,
                    removed_before,
                    width,
                };
                removed_before += (end - start) - width;
                gap
            })
            .collect();
        Some(Self { gaps: collapsed })
    }

    /// How many gaps are collapsed.
    pub fn gap_count(&self) -> usize {
        self.gaps.len()
    }

    /// Plot seconds → compressed coordinate. A time inside a gap lands
    /// proportionally inside its collapsed width.
    pub fn compress(&self, seconds: f64) -> f64 {
        let before = self.gaps.partition_point(|gap| gap.end <= seconds);
        match self.gaps.get(before) {
            Some(gap) if seconds > gap.start => {
                gap.x_start() + (seconds - gap.start) / (gap.end - gap.start) * gap.width
            }
            Some(gap) => seconds - gap.removed_before,
            None => {
                let last = self.gaps.last().expect("a BrokenAxis has a gap");
                seconds - last.removed_before - ((last.end - last.start) - last.width)
            }
        }
    }

    /// Exact inverse of [`Self::compress`].
    pub fn expand(&self, x: f64) -> f64 {
        let before = self
            .gaps
            .partition_point(|gap| gap.x_start() + gap.width <= x);
        match self.gaps.get(before) {
            Some(gap) if x > gap.x_start() => {
                gap.start + (x - gap.x_start()) / gap.width * (gap.end - gap.start)
            }
            Some(gap) => x + gap.removed_before,
            None => {
                let last = self.gaps.last().expect("a BrokenAxis has a gap");
                x + last.removed_before + ((last.end - last.start) - last.width)
            }
        }
    }

    /// Every collapsed gap as (start, end) in compressed coordinates, in
    /// order — where the break glyphs go.
    pub fn collapsed_extents(&self) -> impl Iterator<Item = (f64, f64)> + '_ {
        self.gaps
            .iter()
            .map(|gap| (gap.x_start(), gap.x_start() + gap.width))
    }

    /// The true duration, in plot seconds, of the gap whose collapsed extent
    /// contains compressed coordinate `x` — what an explicit label says.
    pub fn gap_at(&self, x: f64) -> Option<(f64, f64)> {
        let index = self
            .gaps
            .partition_point(|gap| gap.x_start() + gap.width < x);
        self.gaps
            .get(index)
            .filter(|gap| x >= gap.x_start())
            .map(|gap| (gap.start, gap.end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two clusters, [0,10] and [1010,1020] and [3020,3030], gaps (10,1010)
    // and (1020,3020): 3000 of 3030 s is gap.
    const GAPS: [(f64, f64); 2] = [(10.0, 1010.0), (1020.0, 3020.0)];
    const SPAN: (f64, f64) = (0.0, 3030.0);

    fn axis() -> BrokenAxis {
        BrokenAxis::new(&GAPS, SPAN).expect("gaps")
    }

    #[test]
    fn offer_requires_strictly_more_than_half_the_span_in_gaps() {
        assert!(should_offer_compression(&GAPS, SPAN));
        assert!(!should_offer_compression(&[(0.0, 50.0)], (0.0, 100.0)));
        assert!(should_offer_compression(&[(0.0, 51.0)], (0.0, 100.0)));
        assert!(!should_offer_compression(&[], SPAN));
        assert!(!should_offer_compression(&GAPS, (0.0, 0.0)));
    }

    #[test]
    fn clusters_keep_their_true_scale_and_gaps_become_narrow() {
        let axis = axis();
        // Cluster widths are preserved exactly.
        let width = |a: f64, b: f64| axis.compress(b) - axis.compress(a);
        assert!((width(1010.0, 1020.0) - 10.0).abs() < 1e-9);
        assert!((width(3020.0, 3030.0) - 10.0).abs() < 1e-9);
        assert!((width(0.0, 10.0) - 10.0).abs() < 1e-9);
        // A 1000 s and a 2000 s gap both collapse to the same fixed width,
        // far narrower than the gaps themselves.
        let g1 = width(10.0, 1010.0);
        let g2 = width(1020.0, 3020.0);
        assert!((g1 - g2).abs() < 1e-9, "fixed width: {g1} vs {g2}");
        assert!(g1 > 0.0 && g1 < 10.0, "narrow: {g1}");
    }

    #[test]
    fn compress_is_strictly_increasing_and_expand_inverts_it() {
        let axis = axis();
        let mut previous = f64::NEG_INFINITY;
        let mut t = -50.0;
        while t < 3100.0 {
            let x = axis.compress(t);
            assert!(x > previous, "not increasing at {t}");
            previous = x;
            assert!((axis.expand(x) - t).abs() < 1e-6, "round trip at {t}");
            t += 7.3;
        }
    }

    #[test]
    fn gap_edges_are_continuous() {
        let axis = axis();
        for edge in [10.0, 1010.0, 1020.0, 3020.0] {
            let below = axis.compress(edge - 1e-9);
            let at = axis.compress(edge);
            let above = axis.compress(edge + 1e-9);
            assert!((at - below).abs() < 1e-6 && (above - at).abs() < 1e-6);
        }
    }

    #[test]
    fn collapsed_extents_and_gap_lookup_name_the_true_gap() {
        let axis = axis();
        let extents: Vec<_> = axis.collapsed_extents().collect();
        assert_eq!(extents.len(), 2);
        let middle = (extents[1].0 + extents[1].1) / 2.0;
        assert_eq!(axis.gap_at(middle), Some((1020.0, 3020.0)));
        assert_eq!(axis.gap_at(axis.compress(5.0)), None);
    }

    #[test]
    fn malformed_gap_lists_have_no_compressed_mode() {
        assert!(BrokenAxis::new(&[], SPAN).is_none());
        assert!(BrokenAxis::new(&[(5.0, 5.0)], SPAN).is_none());
        assert!(BrokenAxis::new(&[(20.0, 30.0), (10.0, 15.0)], SPAN).is_none());
        assert!(BrokenAxis::new(&[(0.0, f64::NAN)], SPAN).is_none());
    }

    #[test]
    fn a_gap_shorter_than_the_nominal_width_is_never_widened() {
        let axis = BrokenAxis::new(&[(1.0, 1.001)], (0.0, 2.0)).expect("gap");
        assert!((axis.compress(2.0) - 2.0).abs() < 1e-9);
    }
}
