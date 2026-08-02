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

//! Time-domain view (docs/ROADMAP.md M2/M3, SPEC §4.1/§3.1): renders a
//! [`Dataset`]'s numeric columns as a min/max-decimated plot with
//! pan/zoom/box-zoom (via `egui_plot`, see the workspace `Cargo.toml`
//! dependency comment), a "Fit to data" button, and a cursor readout of the
//! exact raw value and timestamp at the nearest sample to the pointer.
//!
//! Every frame queries [`decimate_viewport`] per numeric column against the
//! plot's *current* pan/zoom bounds and pixel width (SPEC §3.1: "for each
//! pixel column, compute min and max of the raw samples whose timestamps
//! fall in that column's time range; draw the vertical extent between
//! them"), using `pyramids` — the same min/max pyramid `glyde_core::ingest`
//! builds at index time — wherever it is available, and falling back to a
//! direct, viewport-bounded raw scan when it is not (see
//! [`crate::plumbing::IndexingMessage::Completed`]'s doc comment for when
//! that happens). A pixel column with no finite reading collapses to a gap
//! bucket ([`bucket_is_gap`]) and is skipped, never interpolated across
//! (SPEC §1.3); once the viewport has zoomed in far enough that every raw
//! sample gets its own pixel column, `decimate_viewport` itself switches to
//! returning one bucket per sample, which is what makes SPEC §3.1's
//! "draw individual point markers" convergence guarantee hold here without
//! this module needing its own `samples < pixels` check.
//!
//! Non-numeric columns (`bool`/`string`, SPEC §1.4) are not drawn here —
//! they route to the state timeline (SPEC §4.3, docs/ROADMAP.md M6), not
//! yet built.

use std::sync::Arc;

use egui_plot::{GridMark, Legend, Line, Plot, PlotBounds, PlotPoints, Points};
use glyde_core::dsp::decimation::{decimate_viewport, Bucket};
use glyde_core::ingest::{
    progressive_tick_to_value, progressive_value_to_tick, Dataset, Level0Cache, TimeAxis,
};
use glyde_core::series::{Series, SeriesValues, ViewKind};
use glyde_core::time::{format_timestamp, Timestamp};

/// Renders `dataset`'s numeric columns as a time-domain plot (SPEC §4.1,
/// §3.1) into `ui`: pan (drag), zoom (scroll wheel and box-select), a "Fit
/// to data" button, and — while the pointer hovers the plot — a readout row
/// below it showing the exact raw value of every plotted series and the
/// timestamp at the nearest sample. `pyramids` is `dataset.columns`-parallel
/// (see the module docs); a `None` entry (non-numeric column, or a
/// completed load whose storage was spilled) falls back to an un-pyramided
/// [`decimate_viewport`] query. `ticks` is `dataset.time`'s own pyramid
/// ticks (`glyde_core::ingest::TimeAxis::to_pyramid_ticks`) — taken as a
/// parameter, computed by the caller once per status change, rather than
/// computed here: for an in-memory dataset that call materializes a fresh
/// `Vec` over every raw sample, so calling it once per egui frame (as this
/// function used to) reintroduced the same unconditional-per-frame-O(n) cost
/// issue #80's own frame-time bench was written to catch — see
/// `crate::app::PartialLoad::ticks`'s doc comment. `sample_cache` is the
/// same once-per-status-change treatment applied to
/// [`decimate_viewport`]'s other O(n) input: `dataset.columns`-parallel,
/// `Some(converted)` for a numeric column whose native dtype is not already
/// `f64` (built via [`cache_column_samples`]), `None` when the column is
/// already `f64` (zero-copy from `dataset` directly) or non-numeric. Review
/// finding on the PR that introduced `ticks` caching (issue #80): the same
/// per-frame cost existed here too, just for `to_f64_vec()` instead of
/// `to_pyramid_ticks()`, and was missed the first time because the PR's own
/// bench fixture happened to use an `f64` column, the one dtype this cost
/// doesn't apply to. `level0_caches` is `dataset.columns`-parallel too
/// (issue #92): `Some` for a completed, non-spilled load's column whose raw
/// samples are served from the on-disk Level-0 cache — [`column_f64_samples`]
/// prefers it over both the zero-copy `f64` path and `sample_cache`, since
/// the cache already holds exactly the converted values a reopen would
/// otherwise redo the conversion for. `&[]` for a still-loading partial
/// dataset, which has no cache of its own yet.
pub fn show(
    ui: &mut egui::Ui,
    dataset: &Dataset,
    pyramids: &[Option<Vec<Vec<Bucket>>>],
    ticks: &[i128],
    sample_cache: &[Option<Vec<f64>>],
    level0_caches: &[Option<Arc<Level0Cache>>],
) {
    let fit_clicked = ui.button("Fit to data").clicked();

    let plot = Plot::new("time_domain_view")
        .legend(Legend::default())
        .allow_zoom(true)
        .allow_scroll(true)
        .allow_drag(true)
        .allow_boxed_zoom(true)
        .x_axis_formatter(|mark, _range| format_x_axis_tick(ticks, mark, &dataset.time));

    let response = plot.show(ui, |plot_ui| {
        if fit_clicked {
            // Computed lazily, only on the frame the button was actually
            // clicked (SPEC §5 frame-time budget, issue #80's bench found
            // this cost dominating a per-frame unconditional computation —
            // see `x_axis_seconds`'s doc comment).
            let x = x_axis_seconds(&dataset.time);
            if let Some(bounds) = data_bounds(&x, &dataset.columns) {
                plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                    [bounds.x_min, bounds.y_min],
                    [bounds.x_max, bounds.y_max],
                ));
            }
        }

        // SPEC §3.1: resolve this frame's visible range and pixel width from
        // the plot's own current transform, so a decimation query always
        // matches exactly what is about to be drawn — never a fixed,
        // once-per-open resampling (docs/ROADMAP.md M3, issue #80).
        let plot_bounds = plot_ui.plot_bounds();
        let pixel_columns = plot_ui.transform().frame().width().round().max(1.0) as usize;
        let range = query_range(&plot_bounds, ticks, &dataset.time);
        // `egui_plot` computes axis gridlines from the *previous* frame's
        // memory (same staleness `query_range` accounts for above), so the
        // frame a "Fit to data" click changes the view still renders with
        // the old gridlines for one frame. Request an immediate follow-up
        // frame so that lag is never something the user has to nudge (e.g.
        // a mouse move) their way out of.
        if fit_clicked {
            plot_ui.ctx().request_repaint();
        }

        // Whether this frame's samples-in-range fit one raw sample per
        // pixel column or fewer (SPEC §3.1 convergence) — the same
        // condition `decimate_viewport` itself branches on internally, but
        // needed here too since the branches render differently: connected
        // (SPEC §3.1's raw-samples-plus-markers regime, docs/ROADMAP.md M2)
        // vs. separate per-column vertical bars ("and nothing else").
        let converged = is_converged(ticks, range, pixel_columns);

        let mut next_color_index = 0usize;
        for (column_index, series) in dataset.columns.iter().enumerate() {
            if series.view_kind() != ViewKind::TimeDomain {
                continue;
            }
            // Issue #55: one color per series, assigned here rather than
            // left to `egui_plot`'s own per-draw-call auto-assignment —
            // `egui_plot` would otherwise hand out a new color to every
            // `line()`/`points()` call, so a series with more than one
            // vertical-extent bar (drawn as separate `Line`s below) rendered
            // as several differently-colored segments instead of one
            // consistent color.
            let color = series_color(next_color_index);
            next_color_index += 1;

            let pyramid = pyramids
                .get(column_index)
                .and_then(Option::as_ref)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let cached = sample_cache.get(column_index).and_then(Option::as_ref);
            let level0 = level0_caches
                .get(column_index)
                .and_then(Option::as_ref)
                .map(Arc::as_ref);
            let samples = column_f64_samples(series.values(), cached, level0);
            let buckets = decimate_viewport(pyramid, samples, ticks, range, pixel_columns);

            if converged {
                // SPEC §3.1's raw-samples regime (docs/ROADMAP.md M2): a
                // connected line through consecutive raw samples, broken at
                // each gap bucket (SPEC §1.3), plus point markers on top so
                // "the user must be able to reach the individual sample".
                // A bug found from a real repro: this used to push every
                // converged bucket into the point-markers list only, with
                // no connecting line at all, rendering a small file as
                // scattered disconnected dots instead of a line plot.
                let segments = bucket_segments(&dataset.time, &buckets);
                for segment in &segments {
                    plot_ui.line(
                        Line::new(PlotPoints::new(segment.clone()))
                            .name(series.name())
                            .color(color),
                    );
                }
                let points: Vec<[f64; 2]> = segments.into_iter().flatten().collect();
                if !points.is_empty() {
                    plot_ui.points(
                        Points::new(PlotPoints::new(points))
                            .name(series.name())
                            .radius(2.0_f32)
                            .color(color),
                    );
                }
            } else {
                // SPEC §3.1's aggregated regime: one vertical extent per
                // pixel column — never a line connecting one pixel column
                // to the next, and never interpolated across a gap bucket
                // (SPEC §1.3).
                let mut points: Vec<[f64; 2]> = Vec::new();
                for bucket in &buckets {
                    if bucket_is_gap(bucket) {
                        continue;
                    }
                    let bucket_x =
                        tick_to_seconds(&dataset.time, (bucket.first_ts + bucket.last_ts) / 2);
                    if bucket.min == bucket.max {
                        points.push([bucket_x, bucket.min]);
                    } else {
                        plot_ui.line(
                            Line::new(PlotPoints::new(vec![
                                [bucket_x, bucket.min],
                                [bucket_x, bucket.max],
                            ]))
                            .name(series.name())
                            .color(color),
                        );
                    }
                }
                if !points.is_empty() {
                    plot_ui.points(
                        Points::new(PlotPoints::new(points))
                            .name(series.name())
                            .radius(2.0_f32)
                            .color(color),
                    );
                }
            }
        }

        // Computed lazily, only while the pointer actually hovers the plot
        // (same rationale as the `fit_clicked` branch above): SPEC §4.1's
        // "exact raw value" cursor readout needs the exact (not
        // decimation-approximated) nearest sample, which `nearest_index`
        // finds via a real linear scan (correct even on a non-monotonic
        // axis — see its own doc comment) rather than the O(log n) but
        // offset-only-accurate lookup `format_x_axis_tick` uses.
        plot_ui.pointer_coordinate().and_then(|pointer| {
            let x = x_axis_seconds(&dataset.time);
            nearest_index(&x, pointer.x)
        })
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
///
/// O(n) — every raw sample, not the decimated view — so [`show`] only ever
/// calls this lazily, on the (rare, at most one per frame) occasions it is
/// actually needed: a "Fit to data" click, or the pointer hovering the plot
/// for the cursor readout. Never called unconditionally once per frame; see
/// [`format_x_axis_tick`]'s doc comment for the per-frame call site this
/// used to be on and why that had to change.
fn x_axis_seconds(time: &TimeAxis) -> Vec<f64> {
    match time {
        TimeAxis::Absolute { timestamps, .. } => timestamps
            .iter()
            .map(|timestamp| timestamp.ticks as f64 / timestamp.unit.ticks_per_second() as f64)
            .collect(),
        TimeAxis::Progressive { values } => values.as_slice().to_vec(),
    }
}

/// The x-axis tick label for `mark` (SPEC §4.1's timestamp fidelity applied
/// to the axis, not just the cursor readout — issue #56). For
/// [`TimeAxis::Absolute`] this reuses [`format_timestamp`] with the same
/// format/unit as the dataset's own samples, so the axis reads identically to
/// the cursor readout ([`format_cursor_time`]) instead of the raw
/// seconds-since-epoch number [`x_axis_seconds`] uses for plotting.
///
/// `mark.value` is a grid position, not necessarily an existing sample's
/// tick, so its *tick count* is derived via [`seconds_to_tick`] — a pure
/// display transform, not a new inference. Its UTC *offset*, however, is
/// taken from `ticks`'s nearest real sample ([`nearest_tick_index`]) rather
/// than always the first one: SPEC §2.1 honors whatever offset each source
/// row carried, and [`crate::time::format::parse_iso8601_with_offset`]-parsed
/// columns can carry a different offset per row (e.g. a DST transition
/// partway through the file), so anchoring every tick to the first sample's
/// offset would mislabel ticks elsewhere in such a file even though their
/// underlying instant is unaffected. `glyde_core`'s ISO 8601 parser reads
/// each row's offset independently, so this is a real, not merely
/// hypothetical, case.
///
/// `egui_plot` calls this once per rendered gridline, and typically several
/// more times per frame while it settles on a tick spacing — cheap on a
/// small file, but `nearest_index`'s O(n) exact scan turned into the
/// dominant per-frame cost once large files became a real, supported case
/// via decimated rendering (issue #80's `crates/glyde-app/benches/time_view_render.rs`
/// caught an 8M-sample frame taking ~4.75s, entirely from this call site).
/// [`nearest_tick_index`]'s O(log n) binary search is what this function
/// uses instead — see its own doc comment for why that is safe for an
/// *offset label*, unlike the cursor readout's exact-value guarantee.
///
/// A [`TimeAxis::Progressive`] index has no calendar meaning, so its tick is
/// shown as a plain number, matching `egui_plot`'s own default axis
/// formatting.
fn format_x_axis_tick(ticks: &[i128], mark: GridMark, time: &TimeAxis) -> String {
    match time {
        TimeAxis::Absolute { timestamps, format } => {
            let tick = seconds_to_tick(time, mark.value);
            let Some(index) = nearest_tick_index(ticks, tick) else {
                return String::new();
            };
            let Some(nearest) = timestamps.get(index) else {
                return String::new();
            };
            let timestamp = Timestamp {
                ticks: tick,
                unit: nearest.unit,
                offset_seconds: nearest.offset_seconds,
            };
            format_timestamp(&timestamp, *format)
        }
        TimeAxis::Progressive { .. } => {
            let num_decimals = (-mark.step_size.log10().round()) as usize;
            egui::emath::format_with_decimals_in_range(mark.value, num_decimals..=num_decimals)
        }
    }
}

/// The index into `ticks` nearest `target`, assuming `ticks` is sorted
/// (non-decreasing) — via binary search ([`slice::partition_point`]),
/// O(log n) rather than [`nearest_index`]'s O(n) exact scan.
///
/// This assumption is safe *here* specifically because the only thing
/// [`format_x_axis_tick`] uses the result for is which real sample's UTC
/// *offset* an axis label borrows — a cosmetic refinement (issue #56), not
/// the label's own instant (computed directly from `target`, independent of
/// any sample) and not SPEC §4.1's "exact raw value" guarantee, which the
/// cursor readout gets from [`nearest_index`]'s real scan instead. SPEC
/// §2.1 non-monotonic timestamps are a flagged anomaly, not the default
/// shape of a file (`docs/SPEC.md` §2.1, §2.3), so on the rare file where
/// `ticks` is not actually sorted this can pick a nearby-but-not-globally-
/// nearest sample's offset — still a real sample's real offset, never a
/// fabricated one, and never affecting the plotted data itself.
fn nearest_tick_index(ticks: &[i128], target: i128) -> Option<usize> {
    if ticks.is_empty() {
        return None;
    }
    let pos = ticks.partition_point(|&tick| tick < target);
    if pos == 0 {
        return Some(0);
    }
    if pos == ticks.len() {
        return Some(ticks.len() - 1);
    }
    let before_distance = target - ticks[pos - 1];
    let after_distance = ticks[pos] - target;
    Some(if before_distance <= after_distance {
        pos - 1
    } else {
        pos
    })
}

/// The color for the `index`-th plotted [`ViewKind::TimeDomain`] series
/// (issue #55), shared by every `Line` segment and the `Points` overlay
/// drawn for that series so a NaN-delimited gap never reads as a second
/// series. Reuses the same hue-stepping formula as `egui_plot::PlotUi`'s own
/// (private) auto-color assignment — equal-saturation/value HSV hues spaced
/// by the golden ratio — for a similarly distinct, non-repeating palette,
/// but stepped once per *series* here rather than once per *draw call*:
/// `egui_plot`'s own auto-assignment bumps its counter on every `line()`/
/// `points()` call, so even a gap-free series previously got a different
/// color for its line than for its point markers. This is not merely a
/// gap fix, then — every series' color shifts relative to before, and a
/// series' line and points now finally match, which they never did under
/// the library's own auto-assignment.
fn series_color(index: usize) -> egui::Color32 {
    let golden_ratio = (5.0_f32.sqrt() - 1.0) / 2.0; // 0.61803398875
    let hue = index as f32 * golden_ratio;
    egui::epaint::Hsva::new(hue, 0.85, 0.5, 1.0).into()
}

/// `values` as an `f64` slice for [`decimate_viewport`] (which needs direct
/// index access over the *whole* column, not just what is on screen):
/// `level0` — a memory-mapped [`Level0Cache`] entry served from (and written
/// to) the on-disk cache (issue #92) — first when present, since it already
/// holds exactly the converted values this column would otherwise need to
/// re-derive on every reopen; then zero-copy via
/// [`SeriesValues::as_f64_slice`] when the column is already `f64`;
/// otherwise `cached` — a converted copy [`cache_column_samples`] already
/// built for this column once, at the last status change, not this frame.
/// Never calls [`SeriesValues::to_f64_vec`] itself: doing the conversion
/// here, inline, was the exact per-frame O(n) cost (worse still for
/// `i64`/`u64`, which additionally run `warn_if_precision_loss` per element)
/// a PR #91 review caught this function reintroducing for every non-`f64`
/// numeric dtype, the same class of mistake `ticks`' own doc comment on
/// [`show`] already covers for `TimeAxis::to_pyramid_ticks`. `bool`/`string`
/// columns never reach here (callers only invoke this for
/// [`ViewKind::TimeDomain`] series, which excludes them by construction) —
/// `cached` being `None` for a numeric column with no `level0` entry either
/// would be a caller bug (the cache is dataset-columns-parallel), so this
/// falls back to empty rather than panicking, matching Golden Rule 2's
/// "never guess silently" only in spirit: an empty column draws nothing
/// rather than crashing, which is the crash-free target SPEC §6 asks for
/// even on a caller error.
fn column_f64_samples<'a>(
    values: &'a SeriesValues,
    cached: Option<&'a Vec<f64>>,
    level0: Option<&'a Level0Cache>,
) -> &'a [f64] {
    if let Some(cache) = level0 {
        return cache.samples();
    }
    match values.as_f64_slice() {
        Some(slice) => slice,
        None => cached.map(Vec::as_slice).unwrap_or(&[]),
    }
}

/// Builds `sample_cache` for [`show`]: `dataset.columns`-parallel, `Some`
/// with a real converted `f64` copy for a [`ViewKind::TimeDomain`] column
/// whose native dtype is not already `f64`, `None` for an already-`f64`
/// column (zero-copy from `dataset` itself covers it — see
/// [`column_f64_samples`]) or a non-numeric one (never drawn by [`show`]).
/// Callers compute this once per status change, mirroring
/// `glyde_core::ingest::pyramids_for_dataset` and `TimeAxis::to_pyramid_ticks`
/// — see `crate::app::PartialLoad::ticks`'s doc comment for why per-frame
/// would be wrong.
pub fn cache_column_samples(dataset: &Dataset) -> Vec<Option<Vec<f64>>> {
    dataset
        .columns
        .iter()
        .map(|series| {
            if series.view_kind() != ViewKind::TimeDomain {
                return None;
            }
            match series.values().as_f64_slice() {
                Some(_) => None,
                None => series.values().to_f64_vec(),
            }
        })
        .collect()
}

/// The decimation query range for this frame: `plot_bounds` converted to
/// ticks, or the dataset's own full time range (`ticks`'s first and last
/// entry) as a fallback when `plot_bounds` is either invalid or shares no
/// overlap at all with the dataset's real tick range.
///
/// The invalid case: a brand-new `egui_plot::Plot`'s bounds start at
/// `PlotBounds::NOTHING`, and `plot_ui.plot_bounds()` always reflects the
/// *previous* frame's memory, read before this frame has drawn anything.
///
/// The no-overlap case is the one that actually matters in practice, found
/// from a real repro (opening corpus case-01 rendered a permanently empty
/// plot): `egui_plot::PlotTransform::new` *sanitizes* an invalid/`NOTHING`
/// bounds into a small, arbitrary `[-1, 1]` window before this function ever
/// sees it — which passes `PlotBounds::is_valid_x` (it is finite and has
/// positive width), so checking validity alone does not detect the
/// bootstrap case at all. Querying `[-1, 1]` against a file's real (e.g.
/// epoch-second, ~1.7 billion) tick range returns nothing, and since
/// `egui_plot`'s own auto-bounds only ever grows to fit whatever *was*
/// drawn, an empty first frame stays empty forever: nothing to fit around.
/// The same no-overlap check also recovers if `egui_plot`'s bounds memory
/// (keyed by a fixed id, not the open file) is left over from a previously
/// opened, unrelated file.
///
/// This does mean a deliberate pan/zoom to *just past* the data's edge — a
/// real, valid, non-empty `plot_bounds` that happens to share no overlap
/// with the data either — gets treated the same way and snapped back to the
/// full range, rather than shown as empty space at the edge. Accepted
/// tradeoff, flagged in `CHANGELOG.md`: recovering from the far more common
/// "no real bounds established yet" case matters more than preserving that
/// one interaction.
fn query_range(plot_bounds: &PlotBounds, ticks: &[i128], time: &TimeAxis) -> (i128, i128) {
    let Some((&first, &last)) = ticks.first().zip(ticks.last()) else {
        return (0, 0);
    };
    if !plot_bounds.is_valid_x() {
        return (first, last);
    }
    let requested = (
        seconds_to_tick(time, plot_bounds.min()[0]),
        seconds_to_tick(time, plot_bounds.max()[0]),
    );
    if requested.1 < first || requested.0 > last {
        (first, last)
    } else {
        requested
    }
}

/// Whether `range` contains at most one raw sample per pixel column — SPEC
/// §3.1's convergence condition, the same one [`decimate_viewport`] branches
/// on internally to decide between aggregating and returning individual
/// samples. `show` needs to know it too, ahead of the query, since the two
/// regimes render differently (connected line vs. separate bars) — computed
/// via the identical binary-search-over-sorted-`ticks` approach
/// `decimate_viewport` itself uses, so the two never disagree.
fn is_converged(ticks: &[i128], range: (i128, i128), pixel_columns: usize) -> bool {
    let lo = ticks.partition_point(|&tick| tick < range.0);
    let hi_exclusive = ticks.partition_point(|&tick| tick <= range.1);
    hi_exclusive.saturating_sub(lo) <= pixel_columns
}

/// A [`Bucket`] with no finite reading at all — SPEC §1.3's "never
/// interpolated" applied to a decimated pixel column rather than a single
/// raw sample. Two distinct shapes both count: a single raw NaN sample once
/// `decimate_viewport` has converged past the point-per-sample threshold
/// (`min == max == NaN`), and a pixel column whose every raw sample was NaN,
/// which [`glyde_core::dsp::decimation::build_pyramid`]'s bucket aggregation
/// leaves at its `min = +INFINITY, max = -INFINITY` starting sentinel
/// (`min > max`) since nothing ever updated it.
fn bucket_is_gap(bucket: &Bucket) -> bool {
    bucket.min.is_nan() || bucket.max.is_nan() || bucket.min > bucket.max
}

/// `buckets` (already one per raw sample — the SPEC §3.1 convergence
/// regime, `show`'s `converged` branch) as `(x, value)` runs, split wherever
/// a bucket is a gap ([`bucket_is_gap`]) — the same NaN-discontinuity shape
/// `series_segments` had before decimation (SPEC §1.3: "never
/// interpolated"), just over buckets instead of raw `(x, value)` pairs
/// directly. Each run is drawn as its own connected `Line` by [`show`], so a
/// gap can never be bridged by a single continuous line.
fn bucket_segments(time: &TimeAxis, buckets: &[Bucket]) -> Vec<Vec<[f64; 2]>> {
    let mut segments: Vec<Vec<[f64; 2]>> = Vec::new();
    let mut current: Vec<[f64; 2]> = Vec::new();
    for bucket in buckets {
        if bucket_is_gap(bucket) {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            continue;
        }
        let x = tick_to_seconds(time, (bucket.first_ts + bucket.last_ts) / 2);
        // In the convergence regime every bucket is exactly one raw sample,
        // so `min == max` is that sample's own value.
        current.push([x, bucket.min]);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// `time`'s own ticks-per-second, for converting a single synthesized pyramid
/// tick (a bucket boundary, not necessarily any real sample's own tick) to
/// and from plot seconds — the same scale [`x_axis_seconds`] applies to every
/// real sample, generalized to an arbitrary tick. `None` for
/// [`TimeAxis::Progressive`], whose ticks instead use the fixed
/// [`glyde_core::ingest::PROGRESSIVE_TICK_SCALE`] via
/// [`progressive_tick_to_value`]/[`progressive_value_to_tick`].
fn absolute_ticks_per_second(time: &TimeAxis) -> Option<i128> {
    match time {
        TimeAxis::Absolute { timestamps, .. } => Some(timestamps.get(0)?.unit.ticks_per_second()),
        TimeAxis::Progressive { .. } => None,
    }
}

/// Inverse of [`seconds_to_tick`]: a pyramid tick (e.g. a [`Bucket`]'s
/// midpoint) as a plot-seconds x-coordinate, the same coordinate space
/// [`x_axis_seconds`] produces for real samples.
fn tick_to_seconds(time: &TimeAxis, tick: i128) -> f64 {
    match absolute_ticks_per_second(time) {
        Some(ticks_per_second) => tick as f64 / ticks_per_second as f64,
        None => progressive_tick_to_value(tick),
    }
}

/// Inverse of [`tick_to_seconds`]: a plot-seconds x-coordinate (e.g. the
/// current viewport bounds from `egui_plot`) back to a pyramid tick, for
/// querying [`decimate_viewport`] with a range in the same tick units
/// `glyde_core::ingest::TimeAxis::to_pyramid_ticks` produces.
fn seconds_to_tick(time: &TimeAxis, seconds: f64) -> i128 {
    match absolute_ticks_per_second(time) {
        Some(ticks_per_second) => (seconds * ticks_per_second as f64).round() as i128,
        None => progressive_value_to_tick(seconds),
    }
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
///
/// The per-dtype match itself lives in `glyde_core::series` (Hard rule 2:
/// all product logic in core, and one canonical implementation of it) —
/// this is the rendering-side name for it.
fn value_as_f64(values: &SeriesValues, index: usize) -> Option<f64> {
    values.f64_at(index)
}

/// The cursor-readout text for `values[index]`, in the value's own native
/// dtype formatting (SPEC §4.1 "exact raw value") rather than the `f64` cast
/// [`value_as_f64`] uses for plotting — an `i64`/`u64` sample keeps its exact
/// integer text here even where the plotted point itself is an `f64`
/// approximation.
fn format_series_value(values: &SeriesValues, index: usize) -> Option<String> {
    values.display_at(index)
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
            .map(|timestamp| format_timestamp(&timestamp, *format))
            .unwrap_or_default(),
        TimeAxis::Progressive { values } => values
            .as_slice()
            .get(index)
            .map(f64::to_string)
            .unwrap_or_default(),
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
                ]
                .into(),
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
        let pyramids = glyde_core::ingest::pyramids_for_dataset(&dataset);
        let ticks = dataset.time.to_pyramid_ticks();
        let sample_cache = cache_column_samples(&dataset);
        let ctx = egui::Context::default();

        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &pyramids, &ticks, &sample_cache, &[]);
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
                timestamps: vec![].into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![Series::new("value", SeriesValues::F64(vec![]))],
        };
        let ctx = egui::Context::default();

        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &[], &[], &[], &[]);
            });
        });

        assert!(
            !output.shapes.is_empty(),
            "the surrounding UI must still draw"
        );
    }

    // Regression test for a real repro: opening corpus case-01 (a small,
    // clean file — exactly what a user would open first) rendered a
    // permanently empty plot until "Fit to data" was clicked. `query_range`
    // has its own precise unit tests for the mechanism (this is the same
    // check, end to end); the point of this one is to prove it against the
    // real corpus fixture and the real `decimate_viewport` call `show`
    // makes, rather than trust the two agree. `egui`'s own draw-call shape
    // count turned out to be a bad signal here — it swung *either* direction
    // between an empty dataset and this real one, dominated by how wide the
    // (real vs. blank) axis-label text renders, not by whether real data
    // was found — so this asserts on `decimate_viewport`'s own bucket count
    // instead, which is what actually determines whether anything is drawn.
    #[test]
    fn show_finds_real_data_despite_egui_plots_sanitized_bootstrap_bounds() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("testdata")
            .join("corpus")
            .join("case-01-comma-clean.csv");
        let dataset = glyde_core::ingest::load(&path).expect("case 1 must load");
        let pyramids = glyde_core::ingest::pyramids_for_dataset(&dataset);
        let ticks = dataset.time.to_pyramid_ticks();

        // The exact bounds a brand-new `egui_plot::Plot` presents on its
        // first frame — verified via a temporary diagnostic print inside
        // `show`'s real render path — not `PlotBounds::NOTHING` itself,
        // which `egui_plot::PlotTransform::new` sanitizes away first.
        let egui_plot_bootstrap_bounds = PlotBounds::from_min_max([-1.0, -1.0], [1.0, 1.0]);
        let range = query_range(&egui_plot_bootstrap_bounds, &ticks, &dataset.time);

        let samples = match dataset.columns[0].values() {
            SeriesValues::F64(values) => values.clone(),
            other => panic!("expected an f64 column, got {other:?}"),
        };
        let pyramid = pyramids[0].as_deref().unwrap_or(&[]);
        let buckets = decimate_viewport(pyramid, &samples, &ticks, range, 800);

        assert!(
            !buckets.is_empty(),
            "the very first frame must find real data even though egui_plot's \
             own bounds start at a sanitized [-1, 1] window that shares no \
             overlap with this file's real (epoch-second) tick range"
        );

        // Also drive the full `show` path once, on a brand-new `egui::Context`
        // with no simulated interaction, purely as the crash-free check SPEC
        // §6 asks for — the bucket assertion above is what actually proves
        // the fix.
        let sample_cache = cache_column_samples(&dataset);
        let output = egui::Context::default().run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &pyramids, &ticks, &sample_cache, &[]);
            });
        });
        assert!(!output.shapes.is_empty());
    }

    // Review finding on the original M2 PR: the NaN-discontinuity claim was
    // previously untested end to end — no test actually drove a NaN-bearing
    // series through `show`. This loads the real torture-corpus case 43
    // fixture (a 3-sample NaN run in the middle of an otherwise clean
    // series) through the same `glyde_core::ingest::load` the app uses, and
    // proves both that `decimate_viewport`'s buckets correctly mark every
    // NaN sample as a gap ([`bucket_is_gap`], SPEC §1.3) and that the full
    // pipeline — real ingestion into a `Dataset`, then `show` — renders it
    // without panicking.
    #[test]
    fn show_renders_a_real_nan_run_corpus_file_without_panicking() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("testdata")
            .join("corpus")
            .join("case-43-nan-runs.csv");
        let dataset = glyde_core::ingest::load(&path).expect("case 43 must load");

        // Sanity check on the fixture itself before trusting the assertions
        // below: 7 samples, 3 of them NaN in the middle.
        let samples = match dataset.columns[0].values() {
            SeriesValues::F64(values) => values.clone(),
            other => panic!("expected an f64 column, got {other:?}"),
        };
        assert_eq!(samples.iter().filter(|v| v.is_nan()).count(), 3);
        assert_eq!(samples.len(), 7);

        // At full convergence (pixel_columns >= sample count) every raw
        // sample gets its own bucket, so this proves the NaN samples come
        // back as gap buckets one for one — SPEC §1.3's "never
        // interpolated" applied to the exact mechanism `show` uses.
        let ticks = dataset.time.to_pyramid_ticks();
        let range = (*ticks.first().unwrap(), *ticks.last().unwrap());
        let buckets = decimate_viewport(&[], &samples, &ticks, range, samples.len());
        assert_eq!(buckets.len(), 7);
        assert_eq!(
            buckets.iter().filter(|b| bucket_is_gap(b)).count(),
            3,
            "each NaN sample must come back as its own gap bucket"
        );
        assert_eq!(
            buckets.iter().filter(|b| !bucket_is_gap(b)).count(),
            4,
            "the 4 non-NaN samples must not be flagged as gaps"
        );

        let pyramids = glyde_core::ingest::pyramids_for_dataset(&dataset);
        let sample_cache = cache_column_samples(&dataset);
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &pyramids, &ticks, &sample_cache, &[]);
            });
        });

        assert!(
            !output.shapes.is_empty(),
            "must draw something around a NaN run without panicking"
        );
    }

    // docs/ROADMAP.md M3 "Zoom all the way in → converges to individual
    // sample points" / "one-sample spike stays visible at every zoom
    // level" (issue #80): a dataset far larger than any plausible pixel
    // width must still render without panicking, exercising the
    // *aggregated* (min/max-bar) branch of `show` rather than only the
    // small-dataset convergence branch every other test in this module
    // hits — the headless test harness's default viewport is large
    // (10,000 x 10,000 points, see `egui::input_state::InputState`'s
    // fallback), but nowhere near this fixture's 200,000 samples. Uses an
    // `i64` column specifically (not `f64`) so this test actually exercises
    // `cache_column_samples`'s conversion path — a PR #91 review found the
    // original version of this test used `f64`, the one dtype that path
    // doesn't apply to, so it never caught the per-frame `to_f64_vec()` bug
    // the review flagged.
    #[test]
    fn show_renders_a_large_non_f64_dataset_without_panicking() {
        let sample_count = 200_000;
        let timestamps: Vec<Timestamp> = (0..sample_count)
            .map(|i| Timestamp::new(i as i128, TimeUnit::Seconds))
            .collect();
        let values: Vec<i64> = (0..sample_count as i64).collect();
        let dataset = Dataset {
            time: TimeAxis::Absolute {
                timestamps: timestamps.into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![Series::new("value", SeriesValues::I64(values))],
        };
        let pyramids = glyde_core::ingest::pyramids_for_dataset(&dataset);
        let ticks = dataset.time.to_pyramid_ticks();
        let sample_cache = cache_column_samples(&dataset);
        assert!(
            sample_cache[0].is_some(),
            "an i64 column must populate the sample cache, not rely on a zero-copy f64 slice"
        );

        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &dataset, &pyramids, &ticks, &sample_cache, &[]);
            });
        });

        assert!(
            !output.shapes.is_empty(),
            "must draw something for a large, decimated dataset without panicking"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyde_core::series::Series;
    use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};

    // Issue #55: every segment of a NaN-split series (and its points
    // overlay) must reuse the same color rather than one per draw call —
    // this is what `series_color` being a pure function of the series'
    // index, called once per series in `show`, guarantees.
    #[test]
    fn series_color_is_stable_for_the_same_index() {
        assert_eq!(series_color(0), series_color(0));
        assert_eq!(series_color(3), series_color(3));
    }

    #[test]
    fn series_color_differs_across_indices() {
        let colors: Vec<_> = (0..5).map(series_color).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "series {i} and {j} must not share a color"
                );
            }
        }
    }

    #[test]
    fn x_axis_seconds_converts_nanosecond_ticks_to_seconds() {
        let time = TimeAxis::Absolute {
            timestamps: vec![
                Timestamp::new(0, TimeUnit::Nanoseconds),
                Timestamp::new(1_500_000_000, TimeUnit::Nanoseconds),
            ]
            .into(),
            format: TimestampFormat::EpochNanos,
        };

        assert_eq!(x_axis_seconds(&time), vec![0.0, 1.5]);
    }

    #[test]
    fn x_axis_seconds_passes_progressive_values_through_unchanged() {
        let time = TimeAxis::Progressive {
            values: vec![0.0, 1.0, 2.0].into(),
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
    fn nearest_tick_index_of_an_empty_slice_is_none() {
        assert_eq!(nearest_tick_index(&[], 0), None);
    }

    #[test]
    fn nearest_tick_index_finds_an_exact_match() {
        let ticks = [0, 10, 20, 30];

        assert_eq!(nearest_tick_index(&ticks, 20), Some(2));
    }

    #[test]
    fn nearest_tick_index_clamps_to_the_first_and_last_entry() {
        let ticks = [10, 20, 30];

        assert_eq!(nearest_tick_index(&ticks, -100), Some(0));
        assert_eq!(nearest_tick_index(&ticks, 100), Some(2));
    }

    #[test]
    fn nearest_tick_index_picks_whichever_neighbor_is_closer() {
        let ticks = [0, 10];

        assert_eq!(nearest_tick_index(&ticks, 3), Some(0));
        assert_eq!(nearest_tick_index(&ticks, 7), Some(1));
    }

    // A tie is broken towards the earlier index — an arbitrary but
    // deterministic choice; ties only matter for which of two real samples'
    // offsets an axis label borrows (see the function's own doc comment).
    #[test]
    fn nearest_tick_index_breaks_a_tie_towards_the_earlier_index() {
        let ticks = [0, 10];

        assert_eq!(nearest_tick_index(&ticks, 5), Some(0));
    }

    // The exact bug found from a real repro: opening a small, clean file
    // (corpus case-01) showed a permanently empty plot. Root cause: a
    // brand-new `egui_plot::Plot`'s bounds start at `PlotBounds::NOTHING`
    // (`is_valid_x()` false), and querying that range returned nothing to
    // draw — with nothing drawn, `egui_plot`'s own auto-bounds had nothing
    // to expand around, so it stayed `NOTHING` forever. `query_range` must
    // fall back to the dataset's own full tick range in exactly this case.
    #[test]
    fn query_range_falls_back_to_the_full_tick_range_when_plot_bounds_is_invalid() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };
        let ticks = [10, 20, 30];

        assert_eq!(query_range(&PlotBounds::NOTHING, &ticks, &time), (10, 30));
    }

    #[test]
    fn query_range_of_an_empty_tick_slice_falls_back_to_a_degenerate_zero_range() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };

        assert_eq!(query_range(&PlotBounds::NOTHING, &[], &time), (0, 0));
    }

    // The actual, empirically-confirmed shape of the real repro: a fresh
    // `egui_plot::Plot`'s `plot_bounds()` is not `PlotBounds::NOTHING`
    // itself — `PlotTransform::new` sanitizes that into a small, arbitrary
    // `[-1, 1]` window first, which passes `is_valid_x()`. Verified via a
    // temporary diagnostic print inside `show`'s real render path against
    // corpus case-01 (ticks around 1.767e18 for nanosecond-precision epoch
    // timestamps): `plot_bounds` was exactly `[-1, 1]`, so a fallback keyed
    // only on `is_valid_x()` (an earlier version of this function) never
    // triggered, and `decimate_viewport` returned zero buckets for every
    // column every frame — a plot that never draws anything, matching
    // exactly what was reported: open a small clean file, see nothing.
    #[test]
    fn query_range_recovers_from_egui_plots_own_sanitized_bootstrap_bounds() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Nanoseconds)].into(),
            format: TimestampFormat::EpochNanos,
        };
        let ticks = [1_767_225_600_000_000_000_i128, 1_767_225_605_000_000_000];
        let egui_plot_bootstrap_bounds = PlotBounds::from_min_max([-1.0, -1.0], [1.0, 1.0]);

        let range = query_range(&egui_plot_bootstrap_bounds, &ticks, &time);

        assert_eq!(range, (ticks[0], ticks[1]));
    }

    // A deliberate pan/zoom to a region with no data snaps back to the full
    // range too — the same no-overlap check that recovers the bootstrap
    // case above can't tell "not yet established" apart from "genuinely
    // panned away", and recovering the former matters more (see
    // `query_range`'s own doc comment for the accepted tradeoff).
    #[test]
    fn query_range_snaps_back_to_the_full_range_when_bounds_are_valid_but_disjoint_from_the_data() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };
        let ticks = [0, 1, 2];
        let bounds = PlotBounds::from_min_max([100.0, 0.0], [200.0, 1.0]);

        assert_eq!(query_range(&bounds, &ticks, &time), (0, 2));
    }

    #[test]
    fn query_range_uses_real_bounds_when_they_overlap_the_data() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };
        let ticks = [0, 1, 2, 3, 4, 5];
        // Overlaps ticks 1..=3 but isn't identical to the full range —
        // proves a real, in-range pan/zoom is used as-is, not overridden.
        let bounds = PlotBounds::from_min_max([1.0, 0.0], [3.0, 1.0]);

        assert_eq!(query_range(&bounds, &ticks, &time), (1, 3));
    }

    #[test]
    fn is_converged_is_true_when_every_sample_fits_its_own_pixel_column() {
        let ticks = [0, 1, 2, 3];

        assert!(is_converged(&ticks, (0, 3), 10));
        assert!(is_converged(&ticks, (0, 3), 4));
    }

    #[test]
    fn is_converged_is_false_when_more_samples_than_pixel_columns_are_in_range() {
        let ticks: Vec<i128> = (0..100).collect();

        assert!(!is_converged(&ticks, (0, 99), 10));
    }

    #[test]
    fn is_converged_of_an_empty_range_is_vacuously_true() {
        let ticks = [0, 1, 2];

        assert!(is_converged(&ticks, (10, 20), 1));
    }

    // The exact other half of the same real repro: after "Fit to data",
    // every sample rendered as a disconnected point with no connecting
    // line — `show`'s converged branch used to push every bucket straight
    // into the point-markers list and never drew a `Line` between them.
    #[test]
    fn bucket_segments_connects_consecutive_non_gap_buckets_into_one_run() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };
        let buckets = vec![
            Bucket {
                min: 1.0,
                max: 1.0,
                first_ts: 0,
                last_ts: 0,
                nan_count: 0,
            },
            Bucket {
                min: 2.0,
                max: 2.0,
                first_ts: 1,
                last_ts: 1,
                nan_count: 0,
            },
        ];

        assert_eq!(
            bucket_segments(&time, &buckets),
            vec![vec![[0.0, 1.0], [1.0, 2.0]]]
        );
    }

    #[test]
    fn bucket_segments_breaks_into_separate_runs_at_a_gap_bucket() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };
        let buckets = vec![
            Bucket {
                min: 1.0,
                max: 1.0,
                first_ts: 0,
                last_ts: 0,
                nan_count: 0,
            },
            Bucket {
                min: f64::NAN,
                max: f64::NAN,
                first_ts: 1,
                last_ts: 1,
                nan_count: 1,
            },
            Bucket {
                min: 3.0,
                max: 3.0,
                first_ts: 2,
                last_ts: 2,
                nan_count: 0,
            },
        ];

        assert_eq!(
            bucket_segments(&time, &buckets),
            vec![vec![[0.0, 1.0]], vec![[2.0, 3.0]]]
        );
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
            timestamps: vec![Timestamp::with_offset(0, TimeUnit::Nanoseconds, 2 * 3600)].into(),
            format: TimestampFormat::Iso8601WithOffset,
        };

        let text = format_cursor_time(&time, 0);

        assert!(text.contains("02:00"), "must honor the offset: {text}");
    }

    #[test]
    fn format_cursor_time_of_a_progressive_index_shows_the_plain_number() {
        let time = TimeAxis::Progressive {
            values: vec![0.0, 1.0, 2.0].into(),
        };

        assert_eq!(format_cursor_time(&time, 1), "1");
    }

    // Issue #56: the axis must show the same human-readable timestamp the
    // cursor readout does, not the raw seconds-since-epoch `x_axis_seconds`
    // coordinate.
    #[test]
    fn format_x_axis_tick_formats_an_absolute_axis_like_the_cursor_readout() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::with_offset(0, TimeUnit::Nanoseconds, 2 * 3600)].into(),
            format: TimestampFormat::Iso8601WithOffset,
        };
        let ticks = time.to_pyramid_ticks();
        let mark = GridMark {
            value: 0.0,
            step_size: 1.0,
        };

        let text = format_x_axis_tick(&ticks, mark, &time);

        assert!(text.contains("02:00"), "must honor the offset: {text}");
    }

    // A tick between two sample instants must still convert to a real
    // timestamp at that grid position, not the nearest sample's tick.
    #[test]
    fn format_x_axis_tick_converts_a_grid_value_that_is_not_an_existing_sample() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
            format: TimestampFormat::EpochSeconds,
        };
        let ticks = time.to_pyramid_ticks();
        let mark = GridMark {
            value: 1.4,
            step_size: 1.0,
        };

        assert_eq!(format_x_axis_tick(&ticks, mark, &time), "1");
    }

    // A source column can carry a different UTC offset per row (e.g. a DST
    // transition partway through the file, SPEC §2.1's "honor it and display
    // it" applied per row). A tick far from the first sample must use the
    // offset of the sample nearest to it, not always the first sample's —
    // otherwise ticks after the transition would show a stale offset even
    // though their underlying instant is unaffected.
    #[test]
    fn format_x_axis_tick_uses_the_nearest_samples_own_offset_not_the_first_samples() {
        let time = TimeAxis::Absolute {
            timestamps: vec![
                Timestamp::with_offset(0, TimeUnit::Seconds, 3600),
                Timestamp::with_offset(3600, TimeUnit::Seconds, 2 * 3600),
            ]
            .into(),
            format: TimestampFormat::Iso8601WithOffset,
        };
        let ticks = time.to_pyramid_ticks();
        let mark = GridMark {
            value: 3600.0,
            step_size: 1.0,
        };

        let text = format_x_axis_tick(&ticks, mark, &time);

        assert!(
            text.contains("02:00"),
            "must use the second sample's own offset, not the first's: {text}"
        );
    }

    #[test]
    fn format_x_axis_tick_of_a_progressive_index_shows_a_plain_number() {
        let time = TimeAxis::Progressive {
            values: vec![0.0, 1.0, 2.0].into(),
        };
        let ticks = time.to_pyramid_ticks();
        let mark = GridMark {
            value: 1.5,
            step_size: 0.1,
        };

        assert_eq!(format_x_axis_tick(&ticks, mark, &time), "1.5");
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
    fn column_f64_samples_is_zero_copy_for_an_f64_series_even_with_no_cache() {
        let values = SeriesValues::F64(vec![1.0, 2.0, 3.0]);

        let samples = column_f64_samples(&values, None, None);

        assert_eq!(samples, &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn column_f64_samples_of_a_non_f64_dtype_reads_the_cache_instead_of_converting() {
        let values = SeriesValues::I64(vec![1, 2, 3]);
        let cached = vec![1.0, 2.0, 3.0];

        let samples = column_f64_samples(&values, Some(&cached), None);

        // Not merely equal in value — this asserts it is `cached`'s own
        // allocation, i.e. `column_f64_samples` never itself calls
        // `to_f64_vec()` (PR #91 review: that per-frame call was the bug).
        assert_eq!(samples.as_ptr(), cached.as_ptr());
    }

    #[test]
    fn column_f64_samples_of_a_non_f64_dtype_with_no_cache_falls_back_to_empty() {
        let values = SeriesValues::I64(vec![1, 2, 3]);

        let samples = column_f64_samples(&values, None, None);

        assert!(samples.is_empty());
    }

    #[test]
    fn column_f64_samples_of_a_non_numeric_series_is_empty() {
        let values = SeriesValues::Bool(vec![true, false]);
        let samples = column_f64_samples(&values, None, None);

        assert!(samples.is_empty());
    }

    /// Issue #92: when a `Level0Cache` entry is present, it wins over both
    /// the zero-copy `f64` slice and `sample_cache` — the whole point of
    /// wiring it in is that a reopen should read *from* the cache rather
    /// than falling back to re-deriving from the in-memory dataset.
    #[test]
    fn column_f64_samples_prefers_the_level0_cache_over_both_other_sources() {
        use glyde_core::index::level0::{build, CacheKey};

        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let source_file = tempfile::NamedTempFile::new().expect("temp file");
        let key = CacheKey::for_path(source_file.path()).expect("stat the temp file");
        let cache_samples = [9.0, 8.0, 7.0];
        let cache_ticks = [0i128, 1, 2];
        let level0 = build(cache_dir.path(), &key, &cache_samples, &cache_ticks)
            .expect("building a level0 cache must succeed");

        let values = SeriesValues::F64(vec![1.0, 2.0, 3.0]);
        let cached = vec![1.0, 2.0, 3.0];

        let samples = column_f64_samples(&values, Some(&cached), Some(&level0));

        assert_eq!(samples, &[9.0, 8.0, 7.0]);
    }

    #[test]
    fn cache_column_samples_is_none_for_an_already_f64_column() {
        let dataset = Dataset {
            time: TimeAxis::Progressive {
                values: vec![0.0, 1.0].into(),
            },
            time_column_name: "index".to_string(),
            columns: vec![Series::new("value", SeriesValues::F64(vec![1.0, 2.0]))],
        };

        assert_eq!(cache_column_samples(&dataset), vec![None]);
    }

    #[test]
    fn cache_column_samples_converts_a_non_f64_numeric_column() {
        let dataset = Dataset {
            time: TimeAxis::Progressive {
                values: vec![0.0, 1.0, 2.0].into(),
            },
            time_column_name: "index".to_string(),
            columns: vec![Series::new("value", SeriesValues::I64(vec![1, 2, 3]))],
        };

        assert_eq!(
            cache_column_samples(&dataset),
            vec![Some(vec![1.0, 2.0, 3.0])]
        );
    }

    #[test]
    fn cache_column_samples_is_none_for_a_non_numeric_column() {
        let dataset = Dataset {
            time: TimeAxis::Progressive {
                values: vec![0.0, 1.0].into(),
            },
            time_column_name: "index".to_string(),
            columns: vec![Series::new(
                "state",
                SeriesValues::String(vec!["on".to_string(), "off".to_string()]),
            )],
        };

        assert_eq!(cache_column_samples(&dataset), vec![None]);
    }

    // SPEC §1.3's "never interpolated" applied to a decimated pixel column:
    // both shapes of "no finite reading" must count as a gap.
    #[test]
    fn bucket_is_gap_detects_a_nan_sample_and_an_all_nan_aggregate() {
        let single_nan_sample = Bucket {
            min: f64::NAN,
            max: f64::NAN,
            first_ts: 0,
            last_ts: 0,
            nan_count: 1,
        };
        let all_nan_aggregate = Bucket {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            first_ts: 0,
            last_ts: 7,
            nan_count: 8,
        };

        assert!(bucket_is_gap(&single_nan_sample));
        assert!(bucket_is_gap(&all_nan_aggregate));
    }

    #[test]
    fn bucket_is_gap_is_false_for_a_bucket_with_any_finite_reading() {
        let single_sample = Bucket {
            min: 1.5,
            max: 1.5,
            first_ts: 0,
            last_ts: 0,
            nan_count: 0,
        };
        let aggregate_with_one_finite_reading = Bucket {
            min: 2.0,
            max: 2.0,
            first_ts: 0,
            last_ts: 7,
            nan_count: 7,
        };

        assert!(!bucket_is_gap(&single_sample));
        assert!(!bucket_is_gap(&aggregate_with_one_finite_reading));
    }

    // SPEC §1.4 / corpus case 44: an explicit `Infinity` is valid data, not
    // a gap.
    #[test]
    fn bucket_is_gap_is_false_for_an_infinite_reading() {
        let bucket = Bucket {
            min: 1.0,
            max: f64::INFINITY,
            first_ts: 0,
            last_ts: 1,
            nan_count: 0,
        };

        assert!(!bucket_is_gap(&bucket));
    }

    #[test]
    fn tick_to_seconds_and_seconds_to_tick_round_trip_on_an_absolute_axis() {
        let time = TimeAxis::Absolute {
            timestamps: vec![Timestamp::new(0, TimeUnit::Nanoseconds)].into(),
            format: TimestampFormat::EpochNanos,
        };

        assert_eq!(tick_to_seconds(&time, 1_500_000_000), 1.5);
        assert_eq!(seconds_to_tick(&time, 1.5), 1_500_000_000);
    }

    #[test]
    fn tick_to_seconds_and_seconds_to_tick_round_trip_on_a_progressive_axis() {
        let time = TimeAxis::Progressive {
            values: vec![0.0].into(),
        };

        assert_eq!(tick_to_seconds(&time, 2_500_000_000), 2.5);
        assert_eq!(seconds_to_tick(&time, 2.5), 2_500_000_000);
    }
}
