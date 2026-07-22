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

//! Absolute-timestamp parsing and formatting (docs/SPEC.md §2.1).
//!
//! **Not implemented yet.** See the module doc in `time/mod.rs`.

/// The native tick resolution a timestamp source declares (SPEC §2.1: "store
/// the native tick resolution declared by the source ... never store
/// absolute time as `f64` seconds"). Sub-nanosecond sources (picoseconds)
/// are stored in their native unit and only converted for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeUnit {
    Seconds,
    Milliseconds,
    Microseconds,
    Nanoseconds,
    Picoseconds,
}

impl TimeUnit {
    /// How many of this unit's ticks make up one second, as an `i128` so a
    /// picosecond-resolution multi-year span is never routed through an
    /// `f64` on the way to computing it (SPEC §2.1).
    pub fn ticks_per_second(self) -> i128 {
        match self {
            TimeUnit::Seconds => 1,
            TimeUnit::Milliseconds => 1_000,
            TimeUnit::Microseconds => 1_000_000,
            TimeUnit::Nanoseconds => 1_000_000_000,
            TimeUnit::Picoseconds => 1_000_000_000_000,
        }
    }
}

/// An absolute timestamp: `ticks` since the Unix epoch
/// (1970-01-01T00:00:00Z), counted in `unit`. Never converted to `f64`
/// seconds internally (SPEC §2.1, Golden Rule 1: raw timestamps are never
/// degraded) — that is what makes nanosecond and picosecond precision
/// survive a multi-year span, which an `f64` (53 bits of mantissa) cannot
/// (docs/QUALITY.md §2 Time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub ticks: i128,
    pub unit: TimeUnit,
}

impl Timestamp {
    pub fn new(ticks: i128, unit: TimeUnit) -> Self {
        Self { ticks, unit }
    }
}

/// Every absolute-timestamp format `open()` must recognize (SPEC §2.1).
/// Progressive numeric indices (no absolute time meaning) are not a
/// `TimestampFormat` — they carry no calendar value to round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampFormat {
    /// RFC 3339 / ISO 8601 with a UTC or explicit offset, e.g.
    /// `2026-07-22T14:30:00+02:00`.
    Iso8601WithOffset,
    /// ISO 8601 with no timezone, honored as naive local time (SPEC §2.1:
    /// "if not, treat as naive local time and label it as such").
    Iso8601Naive,
    /// `YYYY-MM-DD HH:MM:SS[.fff…]`.
    DateTimeSpace,
    /// `DD/MM/YYYY HH:MM:SS`, used once the day-vs-month ambiguity is
    /// resolved (SPEC §2.1 ambiguity rule) — this format itself assumes an
    /// already-disambiguated column.
    DayFirst,
    /// `MM/DD/YYYY HH:MM:SS`, the disambiguated counterpart of
    /// [`TimestampFormat::DayFirst`].
    MonthFirst,
    /// Integer or float seconds since the Unix epoch.
    EpochSeconds,
    /// Integer or float milliseconds since the Unix epoch.
    EpochMillis,
    /// Integer or float microseconds since the Unix epoch.
    EpochMicros,
    /// Integer or float nanoseconds since the Unix epoch.
    EpochNanos,
    /// LabVIEW/NI epoch: seconds since 1904-01-01T00:00:00Z.
    LabViewEpoch,
    /// Excel serial date: days since 1899-12-30, fractional part is the time
    /// of day.
    ExcelSerial,
}

/// Parses `input` as `format`, producing a [`Timestamp`] in that format's
/// native resolution (SPEC §2.1).
///
/// **Not implemented yet** (docs/ROADMAP.md M2).
pub fn parse_timestamp(input: &str, format: TimestampFormat) -> crate::Result<Timestamp> {
    let _ = (input, format);
    todo!("docs/ROADMAP.md M2: timestamp parsing")
}

/// Formats `timestamp` back into `format`'s textual representation. For
/// every format in [`TimestampFormat`],
/// `format_timestamp(&parse_timestamp(s, format)?, format) == s`
/// (docs/QUALITY.md §2 Time: format round-trip).
///
/// **Not implemented yet** (docs/ROADMAP.md M2).
pub fn format_timestamp(timestamp: &Timestamp, format: TimestampFormat) -> String {
    let _ = (timestamp, format);
    todo!("docs/ROADMAP.md M2: timestamp formatting")
}
