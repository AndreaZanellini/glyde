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

//! Per-segment detrending for Welch's method (docs/SPEC.md §3.2: "Mean
//! removal per segment (detrend = constant) by default, documented").
//!
//! **Not implemented yet.** This is the API surface the golden tests in
//! `crates/glyde-core/tests/golden/welch.rs` (docs/QUALITY.md §2 Welch PSD,
//! docs/ROADMAP.md M1) are written against, `#[ignore]`d until
//! docs/ROADMAP.md M5 lands the real body. Never widen a golden test's
//! tolerance or change its expectations to make an implementation pass — if
//! one looks wrong, that is a `blocking-decision` issue, not an edit.

/// Detrend method applied to each segment before windowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detrend {
    /// No detrending; the segment is used as-is.
    None,
    /// Subtract the segment's own mean (SPEC §3.2 default).
    Constant,
}

/// Applies `method` to `segment` in place.
pub fn apply(segment: &mut [f64], method: Detrend) {
    let _ = (segment, method);
    todo!("docs/ROADMAP.md M5: detrend")
}
