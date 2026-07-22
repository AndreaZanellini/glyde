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

//! Gap detection over a timestamp series (docs/SPEC.md §2.2–2.3).
//!
//! **Not implemented yet.** See the module doc in `time/mod.rs`.

/// A detected gap between two consecutive samples in a timestamp series
/// (SPEC §2.2–2.3: `gap = Δt > 10 × median Δt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// Index of the last sample before the gap.
    pub before_index: usize,
    /// Index of the first sample after the gap (always `before_index + 1`).
    pub after_index: usize,
    /// The gap's Δt, in the same tick unit as the input timestamps.
    pub delta: i128,
}

/// Scans consecutive Δt in `timestamps` (non-decreasing, all in the same
/// tick unit) and reports every gap where `Δt > 10 × median Δt` (SPEC
/// §2.2–2.3). Feeds both sampling classification (`SegmentedUniform` vs.
/// `Irregular`, SPEC §2.2, docs/ROADMAP.md M2) and the gap view
/// (docs/ROADMAP.md M8).
///
/// **Not implemented yet** (docs/ROADMAP.md M2/M8).
pub fn detect_gaps(timestamps: &[i128]) -> Vec<Gap> {
    let _ = timestamps;
    todo!("docs/ROADMAP.md M2: gap detection (Δt > 10 × median Δt)")
}
