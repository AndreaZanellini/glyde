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

//! How far the time view may zoom and pan along x (issue #197). Pure
//! arithmetic on plot coordinates, shared by every plot and lane of the time
//! view; it never touches a sample.
//!
//! - **Zoom-out** stops at the data's extent plus [`VIEW_MARGIN_FRACTION`]
//!   of it on each side: past that the file shrinks to a dot.
//! - **Zoom-in** stops at [`MIN_STEPS_PER_VIEW`] typical sample interval
//!   (the median Δt) across the view — two samples, one at each edge: past
//!   that there is nothing more to resolve, only empty space between two
//!   samples.
//! - **Pan** keeps the view's centre on the data, so at least half of the
//!   view always shows the file, and never past the zoom-out margin.

/// The empty room a fully zoomed-out view may show on each side of the
/// data, as a fraction of the data's extent.
pub const VIEW_MARGIN_FRACTION: f64 = 0.05;

/// The fewest typical sample intervals a view may span: the deepest zoom-in
/// still shows two consecutive samples, so a span of two rows (what the
/// spectrum panel's "show these rows" may ask for) is always reachable.
pub const MIN_STEPS_PER_VIEW: f64 = 1.0;

/// The x spans the time view may show, for one file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct XLimits {
    /// The data's extent in plot coordinates: what a fit shows.
    extent: (f64, f64),
    /// The empty room allowed on each side of `extent`.
    margin: f64,
    /// The narrowest span allowed.
    min_span: f64,
}

impl XLimits {
    /// The limits for data spanning `extent` (plot coordinates, as a fit
    /// shows it) with a typical sample interval of `typical_step` in the same
    /// units (the median Δt; `None` when the axis has none). `None` when
    /// `extent` is not a finite, non-empty span.
    ///
    /// Without a usable `typical_step`, the narrowest span is the extent
    /// itself: there is no sampling to zoom into. A `typical_step` so large
    /// that [`MIN_STEPS_PER_VIEW`] of it exceed the extent is capped at the
    /// extent too.
    pub fn new(extent: (f64, f64), typical_step: Option<f64>) -> Option<Self> {
        let (lo, hi) = extent;
        let span = hi - lo;
        if !(lo.is_finite() && hi.is_finite() && span.is_finite() && span > 0.0) {
            return None;
        }
        let min_span = typical_step
            .filter(|step| step.is_finite() && *step > 0.0)
            .map_or(span, |step| (step * MIN_STEPS_PER_VIEW).min(span));
        Some(Self {
            extent,
            margin: span * VIEW_MARGIN_FRACTION,
            min_span,
        })
    }

    /// The narrowest span allowed.
    pub fn min_span(&self) -> f64 {
        self.min_span
    }

    /// The widest span allowed: the extent and its margin on both sides.
    pub fn max_span(&self) -> f64 {
        (self.extent.1 - self.extent.0) + 2.0 * self.margin
    }

    /// `view` brought within the limits: its span clamped to
    /// [`Self::min_span`]..=[`Self::max_span`] around its centre, then moved
    /// (never resized) so that its centre is on the data and it does not
    /// reach past the margin. A `view` already within the limits is returned
    /// exactly as given, so a view that obeys them is never nudged by
    /// rounding. A `view` that is not a finite, non-empty span becomes the
    /// extent (a fit).
    pub fn clamp(&self, view: (f64, f64)) -> (f64, f64) {
        let (lo, hi) = view;
        let span = hi - lo;
        if !(lo.is_finite() && hi.is_finite() && span.is_finite() && span > 0.0) {
            return self.extent;
        }
        let (allowed_lo, allowed_hi) = (self.extent.0 - self.margin, self.extent.1 + self.margin);
        let centre = (lo + hi) / 2.0;
        // A few ulps of slack at the data's magnitude: a view this returned
        // is accepted as it is the next time, whatever its rounding, so the
        // view never keeps asking for one more frame.
        let slack = 16.0 * f64::EPSILON * self.extent.0.abs().max(self.extent.1.abs()).max(span);
        let in_limits = span >= self.min_span - slack
            && span <= self.max_span() + slack
            && lo >= allowed_lo - slack
            && hi <= allowed_hi + slack
            && centre >= self.extent.0 - slack
            && centre <= self.extent.1 + slack;
        if in_limits {
            return view;
        }
        let span = span.max(self.min_span).min(self.max_span());
        let half = span / 2.0;
        // Never empty (span <= extent + 2 margins), but written with
        // `max`/`min` rather than `clamp`, which panics if rounding ever
        // crossed the two by an ulp.
        let centre = centre
            .min(self.extent.1.min(allowed_hi - half))
            .max(self.extent.0.max(allowed_lo + half));
        (centre - half, centre + half)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0..100 s sampled every second.
    fn limits() -> XLimits {
        XLimits::new((0.0, 100.0), Some(1.0)).expect("a valid extent")
    }

    #[test]
    fn a_view_within_the_limits_is_kept_exactly() {
        let view = (10.1, 37.3);
        assert_eq!(limits().clamp(view), view);
        let full = (0.0, 100.0);
        assert_eq!(limits().clamp(full), full);
    }

    #[test]
    fn zoom_out_stops_at_the_extent_plus_its_margin() {
        let limits = limits();
        assert_eq!(limits.max_span(), 110.0);
        let clamped = limits.clamp((-1_000.0, 1_100.0));
        assert_eq!(clamped, (-5.0, 105.0));
        // Off centre: the widest span, put back over the data.
        let clamped = limits.clamp((-50.0, 400.0));
        assert_eq!(clamped, (-5.0, 105.0));
    }

    #[test]
    fn zoom_in_stops_at_a_few_samples_around_the_views_centre() {
        let limits = limits();
        assert_eq!(limits.min_span(), MIN_STEPS_PER_VIEW);
        let clamped = limits.clamp((50.0, 50.2));
        assert!((clamped.1 - clamped.0 - MIN_STEPS_PER_VIEW).abs() < 1e-12);
        assert!(((clamped.0 + clamped.1) / 2.0 - 50.1).abs() < 1e-12);
    }

    #[test]
    fn panning_keeps_the_views_centre_on_the_data() {
        let limits = limits();
        // A 4 s view dragged far right ends with its centre on the last
        // sample: half of it still shows data. Far left, on the first.
        assert_eq!(limits.clamp((500.0, 504.0)), (98.0, 102.0));
        assert_eq!(limits.clamp((-504.0, -500.0)), (-2.0, 2.0));
    }

    #[test]
    fn panning_a_wide_view_stops_at_the_margin() {
        // 100 s wide: the centre could reach the data's end, but the view
        // may not show more than the margin past it.
        assert_eq!(limits().clamp((30.0, 130.0)), (5.0, 105.0));
        assert_eq!(limits().clamp((-30.0, 70.0)), (-5.0, 95.0));
        // 20 s wide: the margin stops it before its centre reaches the end.
        assert_eq!(limits().clamp((500.0, 520.0)), (85.0, 105.0));
    }

    #[test]
    fn without_a_typical_step_the_extent_is_the_narrowest_span() {
        let limits = XLimits::new((0.0, 2.0), None).expect("valid");
        assert_eq!(limits.min_span(), 2.0);
        let clamped = limits.clamp((0.9, 1.1));
        assert!((clamped.1 - clamped.0 - 2.0).abs() < 1e-12);
        // A non-positive or non-finite step is no step.
        for step in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let limits = XLimits::new((0.0, 2.0), Some(step)).expect("valid");
            assert_eq!(limits.min_span(), 2.0, "step {step}");
        }
    }

    #[test]
    fn a_step_coarser_than_the_extent_is_capped_at_the_extent() {
        let limits = XLimits::new((0.0, 10.0), Some(20.0)).expect("valid");
        assert_eq!(limits.min_span(), 10.0);
    }

    #[test]
    fn an_unusable_extent_has_no_limits() {
        for extent in [
            (1.0, 1.0),
            (2.0, 1.0),
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
        ] {
            assert_eq!(XLimits::new(extent, Some(1.0)), None, "{extent:?}");
        }
    }

    #[test]
    fn an_unusable_view_becomes_the_extent() {
        for view in [
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
            (5.0, 5.0),
            (6.0, 5.0),
        ] {
            assert_eq!(limits().clamp(view), (0.0, 100.0), "{view:?}");
        }
    }

    #[test]
    fn a_clamped_view_is_kept_as_it_is() {
        let start = 1.78e9;
        let cases = [
            (
                limits(),
                [(50.0, 50.2), (-1e3, 1e3), (500.0, 504.0), (-30.0, 70.0)],
            ),
            (
                XLimits::new((start, start + 86_400.0), Some(1e-3)).expect("valid"),
                [
                    (start + 0.1, start + 0.1000001),
                    (start - 1e6, start),
                    (start + 1e6, start + 1e6 + 0.3),
                    (start + 7.7, start + 7.7 + 1e-9),
                ],
            ),
        ];
        for (limits, views) in cases {
            for view in views {
                let clamped = limits.clamp(view);
                assert_eq!(limits.clamp(clamped), clamped, "{view:?}");
            }
        }
    }

    #[test]
    fn far_from_the_epoch_the_limits_hold_without_drifting() {
        // A day of 1 ms samples, at 2026 epoch seconds.
        let start = 1.78e9;
        let limits = XLimits::new((start, start + 86_400.0), Some(1e-3)).expect("valid");
        let view = (start + 3_600.0, start + 3_601.0);
        assert_eq!(limits.clamp(view), view);
        let clamped = limits.clamp((start + 10.0, start + 10.0 + 1e-4));
        assert!((clamped.1 - clamped.0 - 1e-3).abs() < 1e-6);
    }
}
