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
//! [`TimestampFormat::Iso8601WithOffset`], [`TimestampFormat::Iso8601Naive`],
//! the four epoch formats, and [`TimestampFormat::DayFirst`] /
//! [`TimestampFormat::MonthFirst`] (with the SPEC §2.1 day-vs-month ambiguity
//! rule) are implemented (docs/ROADMAP.md M2's "Time index" item and its
//! "DD/MM vs MM/DD disambiguation" item). `DateTimeSpace`, `LabViewEpoch`,
//! and `ExcelSerial` are separate, not-yet-started M2 roadmap items and
//! remain `todo!()`.

use tracing::{info, warn};

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
/// (1970-01-01T00:00:00Z), counted in `unit`, always the true UTC-based
/// instant (never adjusted for [`Self::offset_seconds`]). Never converted to
/// `f64` seconds internally (SPEC §2.1, Golden Rule 1: raw timestamps are
/// never degraded) — that is what makes nanosecond and picosecond precision
/// survive a multi-year span, which an `f64` (53 bits of mantissa) cannot
/// (docs/QUALITY.md §2 Time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub ticks: i128,
    pub unit: TimeUnit,
    /// The source's UTC offset in seconds, kept only so
    /// [`TimestampFormat::Iso8601WithOffset`] can honor and redisplay it
    /// (SPEC §2.1: "if the source carries one, honor it and display it").
    /// `ticks` is always the correctly offset-adjusted absolute instant, so
    /// comparisons and DSP never need this field — it exists purely for
    /// display fidelity. `None` for every format that carries no offset
    /// (naive timestamps, epoch formats, a progressive numeric index).
    pub offset_seconds: Option<i32>,
}

impl Timestamp {
    pub fn new(ticks: i128, unit: TimeUnit) -> Self {
        Self {
            ticks,
            unit,
            offset_seconds: None,
        }
    }

    /// Builds a [`Timestamp`] that also carries the source's UTC offset, for
    /// [`TimestampFormat::Iso8601WithOffset`].
    pub fn with_offset(ticks: i128, unit: TimeUnit, offset_seconds: i32) -> Self {
        Self {
            ticks,
            unit,
            offset_seconds: Some(offset_seconds),
        }
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
    /// already-disambiguated column. Also the ISO-leaning default
    /// [`infer_timestamp_format`] picks when a column is fully ambiguous.
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
/// `DateTimeSpace`, `LabViewEpoch`, and `ExcelSerial` are separate,
/// not-yet-started roadmap items and remain `todo!()`.
pub fn parse_timestamp(input: &str, format: TimestampFormat) -> crate::Result<Timestamp> {
    match format {
        TimestampFormat::Iso8601WithOffset => parse_iso8601_with_offset(input),
        TimestampFormat::Iso8601Naive => parse_iso8601_naive(input),
        TimestampFormat::DayFirst => parse_naive_with_pattern(input, "%d/%m/%Y %H:%M:%S", format),
        TimestampFormat::MonthFirst => parse_naive_with_pattern(input, "%m/%d/%Y %H:%M:%S", format),
        TimestampFormat::EpochSeconds => parse_epoch_integer(input, TimeUnit::Seconds, format),
        TimestampFormat::EpochMillis => parse_epoch_integer(input, TimeUnit::Milliseconds, format),
        TimestampFormat::EpochMicros => parse_epoch_integer(input, TimeUnit::Microseconds, format),
        TimestampFormat::EpochNanos => parse_epoch_integer(input, TimeUnit::Nanoseconds, format),
        TimestampFormat::DateTimeSpace
        | TimestampFormat::LabViewEpoch
        | TimestampFormat::ExcelSerial => {
            todo!("docs/ROADMAP.md M2: {format:?} timestamp parsing is a separate, not-yet-started roadmap item")
        }
    }
}

/// Formats `timestamp` back into `format`'s textual representation. For
/// every format in [`TimestampFormat`],
/// `format_timestamp(&parse_timestamp(s, format)?, format) == s`
/// (docs/QUALITY.md §2 Time: format round-trip).
///
/// `DateTimeSpace`, `LabViewEpoch`, and `ExcelSerial` are separate,
/// not-yet-started roadmap items and remain `todo!()`.
pub fn format_timestamp(timestamp: &Timestamp, format: TimestampFormat) -> String {
    match format {
        TimestampFormat::Iso8601WithOffset => format_iso8601_with_offset(timestamp),
        TimestampFormat::Iso8601Naive => format_iso8601_naive(timestamp),
        TimestampFormat::DayFirst => format_naive_with_pattern(timestamp, "%d/%m/%Y %H:%M:%S"),
        TimestampFormat::MonthFirst => format_naive_with_pattern(timestamp, "%m/%d/%Y %H:%M:%S"),
        TimestampFormat::EpochSeconds
        | TimestampFormat::EpochMillis
        | TimestampFormat::EpochMicros
        | TimestampFormat::EpochNanos => timestamp.ticks.to_string(),
        TimestampFormat::DateTimeSpace
        | TimestampFormat::LabViewEpoch
        | TimestampFormat::ExcelSerial => {
            todo!("docs/ROADMAP.md M2: {format:?} timestamp formatting is a separate, not-yet-started roadmap item")
        }
    }
}

fn parse_iso8601_with_offset(input: &str) -> crate::Result<Timestamp> {
    let parsed = chrono::DateTime::parse_from_rfc3339(input).map_err(|source| {
        crate::GlydeError::InvalidTimestamp {
            input: input.to_string(),
            format: TimestampFormat::Iso8601WithOffset,
            reason: source.to_string(),
        }
    })?;
    let ticks = nanos_since_epoch(input, TimestampFormat::Iso8601WithOffset, &parsed)?;
    Ok(Timestamp::with_offset(
        ticks,
        TimeUnit::Nanoseconds,
        parsed.offset().local_minus_utc(),
    ))
}

fn format_iso8601_with_offset(timestamp: &Timestamp) -> String {
    let offset_seconds = timestamp.offset_seconds.unwrap_or(0);
    let offset = chrono::FixedOffset::east_opt(offset_seconds)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("zero offset is always valid"));
    utc_datetime_from_ticks(timestamp)
        .with_timezone(&offset)
        .format("%Y-%m-%dT%H:%M:%S%.f%:z")
        .to_string()
}

fn parse_iso8601_naive(input: &str) -> crate::Result<Timestamp> {
    let parsed =
        chrono::NaiveDateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S%.f").map_err(|source| {
            crate::GlydeError::InvalidTimestamp {
                input: input.to_string(),
                format: TimestampFormat::Iso8601Naive,
                reason: source.to_string(),
            }
        })?;
    let ticks = nanos_since_epoch(input, TimestampFormat::Iso8601Naive, &parsed.and_utc())?;
    Ok(Timestamp::new(ticks, TimeUnit::Nanoseconds))
}

fn format_iso8601_naive(timestamp: &Timestamp) -> String {
    utc_datetime_from_ticks(timestamp)
        .format("%Y-%m-%dT%H:%M:%S%.f")
        .to_string()
}

/// Shared parser for [`TimestampFormat::DayFirst`] and
/// [`TimestampFormat::MonthFirst`]: both are naive (no timezone in the SPEC
/// §2.1 grammar, unlike [`TimestampFormat::Iso8601WithOffset`]) `chrono`
/// strptime patterns differing only in whether `%d` or `%m` comes first.
/// `chrono` itself rejects an out-of-range day/month (e.g. a "month" field >
/// 12), which is what lets [`infer_day_month_format`] use this as the final
/// confirmation step after its own field-scan.
fn parse_naive_with_pattern(
    input: &str,
    pattern: &str,
    format: TimestampFormat,
) -> crate::Result<Timestamp> {
    let parsed = chrono::NaiveDateTime::parse_from_str(input, pattern).map_err(|source| {
        crate::GlydeError::InvalidTimestamp {
            input: input.to_string(),
            format,
            reason: source.to_string(),
        }
    })?;
    let ticks = nanos_since_epoch(input, format, &parsed.and_utc())?;
    Ok(Timestamp::new(ticks, TimeUnit::Nanoseconds))
}

/// The inverse of [`parse_naive_with_pattern`].
fn format_naive_with_pattern(timestamp: &Timestamp, pattern: &str) -> String {
    utc_datetime_from_ticks(timestamp)
        .format(pattern)
        .to_string()
}

/// Nanoseconds since the Unix epoch for an already-parsed `chrono` instant,
/// reported as [`crate::GlydeError::InvalidTimestamp`] rather than a panic
/// on the rare out-of-range date `chrono`'s `i64` nanosecond timestamp can't
/// represent (SPEC §2.1: malformed/extreme user data must never crash the
/// app).
fn nanos_since_epoch<Tz: chrono::TimeZone>(
    input: &str,
    format: TimestampFormat,
    instant: &chrono::DateTime<Tz>,
) -> crate::Result<i128> {
    instant
        .timestamp_nanos_opt()
        .map(i128::from)
        .ok_or_else(|| crate::GlydeError::InvalidTimestamp {
            input: input.to_string(),
            format,
            reason: "date is out of range for nanosecond-precision representation".to_string(),
        })
}

/// The inverse of [`nanos_since_epoch`]: rebuilds a UTC `chrono` instant from
/// `timestamp`'s ticks, for formatting. `timestamp.ticks` is always
/// nanoseconds since the epoch for the two ISO formats that call this (both
/// producers above only ever construct [`TimeUnit::Nanoseconds`] timestamps).
fn utc_datetime_from_ticks(timestamp: &Timestamp) -> chrono::DateTime<chrono::Utc> {
    let secs = timestamp.ticks.div_euclid(1_000_000_000);
    let nanos = timestamp.ticks.rem_euclid(1_000_000_000);
    chrono::DateTime::from_timestamp(
        i64::try_from(secs).expect("nanosecond-range instant fits in an i64 second count"),
        u32::try_from(nanos).expect("nanos-of-second remainder is always in 0..1_000_000_000"),
    )
    .expect("a valid (secs, nanos) pair from an already-parsed timestamp always builds")
}

fn parse_epoch_integer(
    input: &str,
    unit: TimeUnit,
    format: TimestampFormat,
) -> crate::Result<Timestamp> {
    let ticks: i128 = input
        .trim()
        .parse()
        .map_err(
            |source: std::num::ParseIntError| crate::GlydeError::InvalidTimestamp {
                input: input.to_string(),
                format,
                reason: source.to_string(),
            },
        )?;
    Ok(Timestamp::new(ticks, unit))
}

/// The result of [`infer_timestamp_format`]: which [`TimestampFormat`] every
/// field matched, and whether that choice was a confident one or SPEC §2.1's
/// ambiguity-rule fallback — mirroring `ingest::infer::DtypeInference`'s
/// `ambiguous` flag (SPEC §1.2 "confidence is tracked per inference").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampFormatInference {
    pub format: TimestampFormat,
    /// `true` only for the [`TimestampFormat::DayFirst`]/
    /// [`TimestampFormat::MonthFirst`] ambiguity fallback (SPEC §2.1: "If the
    /// whole column is ambiguous ... mark the inference low confidence").
    /// Every other format is either an unambiguous syntactic match or not a
    /// match at all — there is no partial confidence to report for them.
    pub ambiguous: bool,
}

/// The in-scope, syntactically self-disambiguating absolute-timestamp
/// formats [`infer_timestamp_format`] tries first. [`TimestampFormat::DayFirst`]
/// / [`TimestampFormat::MonthFirst`] are deliberately excluded from this list
/// — unlike every format here, a `DD/MM/YYYY ...` and an `MM/DD/YYYY ...`
/// string can be syntactically identical, so picking between them needs
/// [`infer_day_month_format`]'s dedicated field-scan (SPEC §2.1 ambiguity
/// rule) rather than the "first format that parses every field" rule this
/// list is tried under. `DateTimeSpace`, `LabViewEpoch`, and `ExcelSerial`
/// are separate, not-yet-started roadmap items and are never inferred.
const IN_SCOPE_FORMATS: [TimestampFormat; 6] = [
    TimestampFormat::Iso8601WithOffset,
    TimestampFormat::Iso8601Naive,
    TimestampFormat::EpochNanos,
    TimestampFormat::EpochMicros,
    TimestampFormat::EpochMillis,
    TimestampFormat::EpochSeconds,
];

/// A permissive digit-count window used only to disambiguate *which* epoch
/// resolution a column of bare integers represents (SPEC §2.1 lists epoch
/// seconds/millis/micros/nanos as distinct formats with no delimiter to tell
/// them apart syntactically). The window brackets `2001-09-09` to
/// `2286-11-20` in `unit`'s resolution — comfortably covering any real sensor
/// log — so a small index-like integer column (corpus case 35: `0`..`5`)
/// matches none of them and is correctly left as "no absolute timestamp
/// format", rather than misidentified as an implausible 1970-01-01 epoch.
fn plausible_epoch_magnitude(unit: TimeUnit) -> (i128, i128) {
    let per_second = unit.ticks_per_second();
    (1_000_000_000 * per_second, 10_000_000_000 * per_second)
}

fn epoch_unit(format: TimestampFormat) -> Option<TimeUnit> {
    match format {
        TimestampFormat::EpochSeconds => Some(TimeUnit::Seconds),
        TimestampFormat::EpochMillis => Some(TimeUnit::Milliseconds),
        TimestampFormat::EpochMicros => Some(TimeUnit::Microseconds),
        TimestampFormat::EpochNanos => Some(TimeUnit::Nanoseconds),
        _ => None,
    }
}

/// Whether every field in `fields` parses under `format`, applying
/// [`plausible_epoch_magnitude`] for the epoch formats so a small
/// progressive-index-like integer column is never claimed by an epoch
/// format just because it happens to parse as one syntactically.
fn field_matches_format(field: &str, format: TimestampFormat) -> bool {
    let field = field.trim();
    match epoch_unit(format) {
        Some(unit) => {
            let (min, max) = plausible_epoch_magnitude(unit);
            field
                .parse::<i128>()
                .is_ok_and(|value| (min..max).contains(&value.abs()))
        }
        None => parse_timestamp(field, format).is_ok(),
    }
}

/// Extracts the first two `/`-separated numeric groups from a
/// `DD/MM/YYYY ...` or `MM/DD/YYYY ...` field, e.g. `"25/01/2026 08:00:00"`
/// -> `Some((25, 1))`. Returns `None` for anything that does not have the
/// three-part `.../.../...`  shape (a bare `"3/4"`, a non-numeric field, or a
/// field with no `/` at all), so [`infer_day_month_format`] only treats a
/// column as a slash-delimited date candidate once every field actually
/// looks like one.
fn leading_day_month_groups(field: &str) -> Option<(u32, u32)> {
    let mut parts = field.trim().splitn(3, '/');
    let first = parts.next()?.trim().parse::<u32>().ok()?;
    let second = parts.next()?.trim().parse::<u32>().ok()?;
    parts.next()?; // the year-and-time remainder; its mere presence confirms the 3-part shape
    Some((first, second))
}

/// Implements SPEC §2.1's day-vs-month ambiguity rule for a column of
/// `DD/MM/YYYY HH:MM:SS` / `MM/DD/YYYY HH:MM:SS` candidates: "never
/// coin-flip. Resolve by scanning enough rows to find a discriminating value
/// (a field > 12 settles day-vs-month). If the whole column is ambiguous,
/// pick ISO-leaning default (`DD/MM` ...), mark the inference low
/// confidence ... Log the decision."
///
/// A value in the first slash-group greater than 12 can only be a day (no
/// month exceeds 12), which settles [`TimestampFormat::DayFirst`]; the
/// symmetric case in the second group settles
/// [`TimestampFormat::MonthFirst`]. A column where evidence for *both*
/// readings appears across different rows is genuinely contradictory — no
/// single fixed format explains every row — so that is reported as "no
/// match" (`None`) rather than guessed either way, the same fidelity-first
/// default `field_matches_format`'s epoch-magnitude window applies to
/// implausible epoch values.
fn infer_day_month_format(fields: &[String]) -> Option<TimestampFormatInference> {
    let groups: Vec<(u32, u32)> = fields
        .iter()
        .map(|field| leading_day_month_groups(field))
        .collect::<Option<Vec<_>>>()?;

    let day_first_evidence = groups.iter().any(|&(first, _)| first > 12);
    let month_first_evidence = groups.iter().any(|&(_, second)| second > 12);

    let (format, ambiguous) = match (day_first_evidence, month_first_evidence) {
        (true, true) => return None,
        (true, false) => (TimestampFormat::DayFirst, false),
        (false, true) => (TimestampFormat::MonthFirst, false),
        (false, false) => (TimestampFormat::DayFirst, true),
    };

    if !fields
        .iter()
        .all(|field| parse_timestamp(field, format).is_ok())
    {
        return None;
    }

    if ambiguous {
        warn!(
            format = ?format,
            field_count = fields.len(),
            "day-vs-month ambiguous in every row (no field > 12 in either position) — defaulting \
             to the ISO-leaning DD/MM reading per SPEC §2.1; low-confidence inference"
        );
    } else {
        info!(
            format = ?format,
            field_count = fields.len(),
            "day-vs-month disambiguated by a field > 12 (SPEC §2.1 ambiguity rule)"
        );
    }

    Some(TimestampFormatInference { format, ambiguous })
}

/// Infers which [`TimestampFormat`] every value in `fields` matches (SPEC
/// §2.1), first trying the syntactically self-disambiguating
/// [`IN_SCOPE_FORMATS`], then falling back to [`infer_day_month_format`]'s
/// dedicated ambiguity resolution. Returns `None` when no candidate explains
/// every field — the signal a caller uses to fall back to a
/// progressive-numeric index (corpus case 35) rather than mis-detecting an
/// absolute timestamp.
pub fn infer_timestamp_format(fields: &[String]) -> Option<TimestampFormatInference> {
    if fields.is_empty() {
        return None;
    }

    if let Some(format) = IN_SCOPE_FORMATS.into_iter().find(|&format| {
        fields
            .iter()
            .all(|field| field_matches_format(field, format))
    }) {
        return Some(TimestampFormatInference {
            format,
            ambiguous: false,
        });
    }

    infer_day_month_format(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn corpus_path(file_name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("testdata")
            .join("corpus")
            .join(file_name)
    }

    /// Every raw-text field of `column_name` in `file_name`'s data rows, in
    /// row order — mirrors `ingest::infer`'s own corpus-driven tests (this
    /// module exercises `infer_timestamp_format`/`parse_timestamp` against
    /// real corpus text, not a hand-picked `Vec<String>`). Every corpus case
    /// used here is a plain comma-delimited file with no quoted fields, so a
    /// straightforward split on the inferred delimiter is sufficient (the
    /// quote-aware tokenizer lives in `ingest::infer` and is exercised by
    /// that module's own tests).
    fn corpus_column(file_name: &str, column_name: &str) -> Vec<String> {
        let bytes = std::fs::read(corpus_path(file_name))
            .unwrap_or_else(|e| panic!("read {file_name}: {e}"));
        let encoding = crate::ingest::detect_encoding(&bytes);
        let text = crate::ingest::decode(&bytes, &encoding);
        let delimiter = crate::ingest::infer_delimiter(&text).delimiter;
        let header = crate::ingest::infer_header(&text, delimiter);
        let column_index = header
            .column_names
            .iter()
            .position(|name| name == column_name)
            .unwrap_or_else(|| panic!("column '{column_name}' not found in {file_name}"));
        let data_start = header
            .header_row_index
            .map_or(header.skipped_preamble_rows, |index| index + 1);

        text.lines()
            .skip(data_start)
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.split(delimiter.as_str())
                    .nth(column_index)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
            .collect()
    }

    fn assert_monotonically_increasing(timestamps: &[Timestamp]) {
        assert!(
            timestamps
                .windows(2)
                .all(|pair| pair[1].ticks > pair[0].ticks),
            "corpus fixture is a monotonically increasing series by construction: {timestamps:?}"
        );
    }

    // Corpus case 24 (docs/QUALITY.md §1.24): ISO 8601 with a `+02:00`
    // offset. `infer_timestamp_format` must pick `Iso8601WithOffset` (not
    // `Iso8601Naive`, which the offset suffix does not match), and the
    // offset must survive into every parsed `Timestamp` for display (SPEC
    // §2.1: "if the source carries [a timezone], honor it and display it").
    #[test]
    fn corpus_case_24_iso8601_with_timezone_detects_and_parses() {
        let fields = corpus_column("case-24-iso8601-with-timezone.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::Iso8601WithOffset);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        assert_monotonically_increasing(&timestamps);
        for timestamp in &timestamps {
            assert_eq!(timestamp.unit, TimeUnit::Nanoseconds);
            assert_eq!(timestamp.offset_seconds, Some(2 * 3600));
        }
    }

    // Corpus case 25: the same shape with no offset — `Iso8601Naive`, SPEC
    // §2.1's "treat as naive local time" path. No offset to honor, so
    // `offset_seconds` must be `None`, not a guessed `Some(0)`.
    #[test]
    fn corpus_case_25_iso8601_naive_detects_and_parses() {
        let fields = corpus_column("case-25-iso8601-naive.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::Iso8601Naive);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        assert_monotonically_increasing(&timestamps);
        for timestamp in &timestamps {
            assert_eq!(timestamp.offset_seconds, None);
        }
    }

    // Corpus cases 29-32: the four epoch resolutions, distinguished purely by
    // magnitude (SPEC §2.1) since the raw text carries no unit marker.
    #[test]
    fn corpus_case_29_epoch_seconds_detects_and_parses() {
        let fields = corpus_column("case-29-epoch-seconds.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::EpochSeconds);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(
            timestamps[0],
            Timestamp::new(1_770_000_000, TimeUnit::Seconds)
        );
        assert_monotonically_increasing(&timestamps);
    }

    #[test]
    fn corpus_case_30_epoch_milliseconds_detects_and_parses() {
        let fields = corpus_column("case-30-epoch-milliseconds.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::EpochMillis);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(
            timestamps[0],
            Timestamp::new(1_770_000_000_000, TimeUnit::Milliseconds)
        );
        assert_monotonically_increasing(&timestamps);
    }

    #[test]
    fn corpus_case_31_epoch_microseconds_detects_and_parses() {
        let fields = corpus_column("case-31-epoch-microseconds.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::EpochMicros);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(
            timestamps[0],
            Timestamp::new(1_770_000_000_000_000, TimeUnit::Microseconds)
        );
        assert_monotonically_increasing(&timestamps);
    }

    #[test]
    fn corpus_case_32_epoch_nanoseconds_detects_and_parses() {
        let fields = corpus_column("case-32-epoch-nanoseconds.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::EpochNanos);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(
            timestamps[0],
            Timestamp::new(1_770_000_000_000_000_000, TimeUnit::Nanoseconds)
        );
        assert_monotonically_increasing(&timestamps);
    }

    // Corpus case 26 (docs/QUALITY.md §1.26): every day field is 25, which
    // exceeds the maximum month (12) — SPEC §2.1's discriminating value that
    // settles `DayFirst` with full confidence (not the ambiguous fallback).
    #[test]
    fn corpus_case_26_dd_mm_yyyy_unambiguous_detects_day_first_confidently() {
        let fields = corpus_column("case-26-dd-mm-yyyy-unambiguous.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::DayFirst);
        assert!(
            !inference.ambiguous,
            "a field > 12 in the first position must resolve confidently, not fall back to the \
             ambiguous default"
        );

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        assert_monotonically_increasing(&timestamps);
    }

    // Corpus case 27: the mirror of case 26 — every second field is 25, which
    // settles `MonthFirst` with full confidence.
    #[test]
    fn corpus_case_27_mm_dd_yyyy_unambiguous_detects_month_first_confidently() {
        let fields = corpus_column("case-27-mm-dd-yyyy-unambiguous.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::MonthFirst);
        assert!(
            !inference.ambiguous,
            "a field > 12 in the second position must resolve confidently, not fall back to the \
             ambiguous default"
        );

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        assert_monotonically_increasing(&timestamps);
    }

    // Corpus case 28: every field is `01/02/2026 ...` — both positions stay
    // <= 12 in every row, so no field ever discriminates day from month. SPEC
    // §2.1: "If the whole column is ambiguous, pick ISO-leaning default
    // (`DD/MM` ...), mark the inference low confidence."
    #[test]
    fn corpus_case_28_fully_ambiguous_dates_defaults_to_day_first_low_confidence() {
        let fields = corpus_column("case-28-fully-ambiguous-dates.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::DayFirst);
        assert!(
            inference.ambiguous,
            "a column with no discriminating field must be flagged low-confidence, never a \
             silent coin-flip (CLAUDE.md Golden Rule 2)"
        );

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        assert_monotonically_increasing(&timestamps);
    }

    // Not itself a corpus case: a column where different rows individually
    // discriminate toward opposite readings (one row's first field > 12,
    // another row's second field > 12). No single fixed format explains every
    // row, so this must be reported as "no match" rather than guessed either
    // way (CLAUDE.md Golden Rule 2) — the same fidelity-first treatment
    // `field_matches_format`'s epoch-magnitude window gives an implausible
    // epoch value.
    #[test]
    fn infer_timestamp_format_rejects_a_column_with_contradictory_day_month_evidence() {
        let fields = vec![
            "25/01/2026 08:00:00".to_string(),
            "01/25/2026 09:00:00".to_string(),
        ];
        assert_eq!(infer_timestamp_format(&fields), None);
    }

    // Corpus case 35 (docs/QUALITY.md §1.35): a small monotonic integer
    // column named "sample", not "timestamp" — SPEC §2.1's "progressive
    // numeric" index. Neither column may be mistaken for an absolute
    // timestamp: `value` is fractional (matches no in-scope format at all),
    // and `sample`'s `0..5` values are exactly the trap
    // `plausible_epoch_magnitude` exists to reject — syntactically valid
    // epoch-second integers, but so implausibly close to 1970-01-01 that
    // treating them as an absolute timestamp would be a wrong-but-silent
    // guess (CLAUDE.md Golden Rule 2).
    #[test]
    fn corpus_case_35_progressive_integer_index_matches_no_timestamp_format() {
        let sample = corpus_column("case-35-progressive-integer-index.csv", "sample");
        assert_eq!(infer_timestamp_format(&sample), None);

        let value = corpus_column("case-35-progressive-integer-index.csv", "value");
        assert_eq!(infer_timestamp_format(&value), None);
    }

    #[test]
    fn infer_timestamp_format_of_empty_fields_is_none() {
        assert_eq!(infer_timestamp_format(&[]), None);
    }

    #[test]
    fn infer_timestamp_format_rejects_a_column_that_only_partially_matches() {
        // Five well-formed epoch-second values plus one non-numeric field:
        // the whole column must be rejected, not silently treated as if the
        // odd field were absent.
        let fields = vec![
            "1770000000".to_string(),
            "1770000001".to_string(),
            "not-a-timestamp".to_string(),
        ];
        assert_eq!(infer_timestamp_format(&fields), None);
    }

    #[test]
    fn parse_timestamp_reports_an_invalid_iso8601_string_instead_of_panicking() {
        let err = parse_timestamp("not-a-date", TimestampFormat::Iso8601WithOffset)
            .expect_err("malformed input must be a reported error, not a panic");
        assert!(matches!(
            err,
            crate::GlydeError::InvalidTimestamp {
                format: TimestampFormat::Iso8601WithOffset,
                ..
            }
        ));
    }

    #[test]
    fn parse_timestamp_reports_an_invalid_epoch_string_instead_of_panicking() {
        let err = parse_timestamp("not-a-number", TimestampFormat::EpochSeconds)
            .expect_err("malformed input must be a reported error, not a panic");
        assert!(matches!(
            err,
            crate::GlydeError::InvalidTimestamp {
                format: TimestampFormat::EpochSeconds,
                ..
            }
        ));
    }
}
