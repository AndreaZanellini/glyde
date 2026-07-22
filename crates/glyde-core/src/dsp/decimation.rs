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

//! The multi-resolution min/max pyramid and the viewport decimation query
//! that renders it (docs/SPEC.md §3.1, docs/ARCHITECTURE.md §The index).
//!
//! **Not implemented yet.** This is the API surface the golden tests in
//! `crates/glyde-core/tests/golden/decimation.rs` (docs/QUALITY.md §2
//! Decimation, docs/ROADMAP.md M1) are written against, `#[ignore]`d until
//! docs/ROADMAP.md M3 lands the real bodies. Never widen a golden test's
//! tolerance or change its expectations to make an implementation pass — if
//! one looks wrong, that is a `blocking-decision` issue, not an edit.

/// One pyramid bucket: `(min, max, first_ts, last_ts, nan_count)` over the
/// raw samples/time-range it aggregates (ARCH §The index).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub min: f64,
    pub max: f64,
    pub first_ts: i128,
    pub last_ts: i128,
    pub nan_count: u64,
}

/// Bucket-size growth factor between consecutive pyramid levels (ARCH §The
/// index: "factor 8 recommended, documented in code").
pub const PYRAMID_FACTOR: usize = 8;

/// Builds the full min/max pyramid over `samples`/`timestamps` (level 0 is
/// the raw data itself and is not materialized here). `pyramid[0]` groups
/// every [`PYRAMID_FACTOR`] raw samples into one bucket; `pyramid[k + 1]`'s
/// buckets are the exact min-of-mins/max-of-maxes aggregation of
/// [`PYRAMID_FACTOR`] consecutive buckets from `pyramid[k]` — never a
/// re-aggregation of resampled or averaged data (SPEC §3.1).
///
/// `samples[i]` is the value at `timestamps[i]`; both slices must be the
/// same length and `timestamps` must be non-decreasing.
pub fn build_pyramid(samples: &[f64], timestamps: &[i128]) -> Vec<Vec<Bucket>> {
    let _ = (samples, timestamps);
    todo!("docs/ROADMAP.md M3: index pyramid build")
}

/// Resolves the viewport `range` (inclusive, in the same tick units as
/// `timestamps`) into one [`Bucket`] per pixel column, using `pyramid` where
/// available.
///
/// When `range` contains fewer raw samples than `pixel_columns`, returns one
/// bucket per raw sample instead of aggregating (`min == max == samples[i]`)
/// so the caller can draw individual point markers — the convergence
/// guarantee in SPEC §3.1: zooming in far enough always reaches the true
/// samples.
pub fn decimate_viewport(
    pyramid: &[Vec<Bucket>],
    samples: &[f64],
    timestamps: &[i128],
    range: (i128, i128),
    pixel_columns: usize,
) -> Vec<Bucket> {
    let _ = (pyramid, samples, timestamps, range, pixel_columns);
    todo!("docs/ROADMAP.md M3: viewport decimation query")
}
