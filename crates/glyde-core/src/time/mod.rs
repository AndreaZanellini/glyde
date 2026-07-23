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

//! Time index: absolute-timestamp parsing/formatting and gap detection
//! (docs/SPEC.md §2, docs/ARCHITECTURE.md workspace layout `time/`).
//!
//! Timestamp parsing/formatting for [`TimestampFormat::Iso8601WithOffset`],
//! [`TimestampFormat::Iso8601Naive`], and the four epoch formats — plus
//! [`infer_timestamp_format`], which also recognizes when a column is a
//! progressive numeric index rather than an absolute timestamp — landed with
//! docs/ROADMAP.md M2 "Time index: progressive numeric + core timestamp
//! formats". `detect_gaps` and the remaining [`TimestampFormat`] variants
//! (`DateTimeSpace`, `DayFirst`/`MonthFirst`, `LabViewEpoch`, `ExcelSerial`)
//! are separate, not-yet-started M2 roadmap items and stay `#[ignore]`d /
//! `todo!()` in `crates/glyde-core/tests/golden/time.rs`. Never widen a
//! golden test's tolerance or change its expectations to make an
//! implementation pass — if one looks wrong, that is a `blocking-decision`
//! issue, not an edit.
//!
//! `Timestamp` stores ticks as `i128`, never `f64` seconds (SPEC §2.1) — the
//! type system enforces Golden Rule 1 (never degrade the raw data) at the
//! signature level, the same way `dsp::welch` enforces "raw samples only" by
//! never taking a pyramid type.

mod format;
mod gap;

pub use format::{
    format_timestamp, infer_timestamp_format, parse_timestamp, TimeUnit, Timestamp, TimestampFormat,
};
pub use gap::{detect_gaps, Gap};
