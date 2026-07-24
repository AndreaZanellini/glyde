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
//! the four epoch formats, [`TimestampFormat::DayFirst`] /
//! [`TimestampFormat::MonthFirst`] (with the SPEC §2.1 day-vs-month ambiguity
//! rule), [`TimestampFormat::LabViewEpoch`], and [`TimestampFormat::ExcelSerial`]
//! are implemented (docs/ROADMAP.md M2's "Time index" item, "DD/MM vs MM/DD
//! disambiguation", and the Excel/LabVIEW/picosecond-index item). The two
//! ISO 8601 formats also preserve sub-nanosecond (picosecond) fractional
//! seconds exactly, rather than silently rounding them to `chrono`'s
//! nanosecond precision. `DateTimeSpace` is a separate, not-yet-started M2
//! roadmap item and remains `todo!()`.

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
/// `DateTimeSpace` is a separate, not-yet-started roadmap item and remains
/// `todo!()`.
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
        TimestampFormat::LabViewEpoch => parse_labview_epoch(input),
        TimestampFormat::ExcelSerial => parse_excel_serial(input),
        TimestampFormat::DateTimeSpace => {
            todo!("docs/ROADMAP.md M2: {format:?} timestamp parsing is a separate, not-yet-started roadmap item")
        }
    }
}

/// Formats `timestamp` back into `format`'s textual representation. For
/// every format in [`TimestampFormat`],
/// `format_timestamp(&parse_timestamp(s, format)?, format) == s`
/// (docs/QUALITY.md §2 Time: format round-trip).
///
/// `DateTimeSpace` is a separate, not-yet-started roadmap item and remains
/// `todo!()`.
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
        TimestampFormat::LabViewEpoch => format_labview_epoch(timestamp),
        TimestampFormat::ExcelSerial => format_excel_serial(timestamp),
        TimestampFormat::DateTimeSpace => {
            todo!("docs/ROADMAP.md M2: {format:?} timestamp formatting is a separate, not-yet-started roadmap item")
        }
    }
}

/// The finest [`TimeUnit`] `chrono` itself can represent a fractional second
/// in — beyond this many fractional digits, `chrono`'s parser would silently
/// round the extra digits away (a Golden Rule 1 violation), so
/// [`parse_iso8601_with_offset`] and [`parse_iso8601_naive`] route anything
/// with more digits through [`parse_subnanosecond_iso8601`] instead.
const CHRONO_MAX_FRACTIONAL_DIGITS: usize = 9;

fn parse_iso8601_with_offset(input: &str) -> crate::Result<Timestamp> {
    if let Some((before, frac, suffix)) = split_iso8601_fraction(input) {
        if frac.len() > CHRONO_MAX_FRACTIONAL_DIGITS {
            return parse_subnanosecond_iso8601(
                input,
                TimestampFormat::Iso8601WithOffset,
                before,
                frac,
                suffix,
            );
        }
    }
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
    if timestamp.unit == TimeUnit::Picoseconds {
        return format_subnanosecond_iso8601_with_offset(timestamp);
    }
    let offset_seconds = timestamp.offset_seconds.unwrap_or(0);
    let offset = chrono::FixedOffset::east_opt(offset_seconds)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("zero offset is always valid"));
    utc_datetime_from_ticks(timestamp)
        .with_timezone(&offset)
        .format("%Y-%m-%dT%H:%M:%S%.f%:z")
        .to_string()
}

fn parse_iso8601_naive(input: &str) -> crate::Result<Timestamp> {
    if let Some((before, frac, suffix)) = split_iso8601_fraction(input) {
        if frac.len() > CHRONO_MAX_FRACTIONAL_DIGITS {
            return parse_subnanosecond_iso8601(
                input,
                TimestampFormat::Iso8601Naive,
                before,
                frac,
                suffix,
            );
        }
    }
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
    if timestamp.unit == TimeUnit::Picoseconds {
        return format_subnanosecond_iso8601_naive(timestamp);
    }
    utc_datetime_from_ticks(timestamp)
        .format("%Y-%m-%dT%H:%M:%S%.f")
        .to_string()
}

/// Splits an ISO 8601 string's fractional-seconds digits from the rest of
/// the string, e.g. `"2026-01-01T00:00:00.000000000001Z"` ->
/// `("2026-01-01T00:00:00", "000000000001", "Z")`. `before` joined with
/// `suffix` (with no `.` between them) is a valid fraction-free ISO 8601
/// string, parseable by the ordinary `chrono`-based path. Returns `None`
/// when `input` has no `.` at all (an integer-second timestamp, which never
/// needs the picosecond-preserving path).
fn split_iso8601_fraction(input: &str) -> Option<(&str, &str, &str)> {
    let (before, after_dot) = input.split_once('.')?;
    let digit_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
    let (frac, suffix) = after_dot.split_at(digit_len);
    Some((before, frac, suffix))
}

/// Exact picoseconds-of-second represented by an ISO 8601 fractional-seconds
/// digit string (e.g. `"5"` = .5s -> `500_000_000_000`; `"000000000001"` =
/// .000000000001s -> `1`), computed by exact `i128` arithmetic so it is
/// never rounded through `f64` (Golden Rule 1). Digits past the 12th (finer
/// than picosecond resolution, which SPEC §2.1 defines no unit for) are
/// dropped.
fn frac_digits_to_picos(frac_digits: &str) -> i128 {
    let truncated = &frac_digits[..frac_digits.len().min(12)];
    if truncated.is_empty() {
        return 0;
    }
    let numerator: i128 = truncated
        .parse()
        .expect("an all-ASCII-digit substring always parses as an i128");
    numerator * 10_i128.pow(12 - truncated.len() as u32)
}

/// Parses an ISO 8601 timestamp whose fractional seconds exceed `chrono`'s
/// nanosecond precision (SPEC §2.1: "sub-nanosecond sources (picoseconds)
/// are stored in their native unit"): the whole-second part is parsed by the
/// ordinary `chrono` path (via `before` joined with `suffix`, the same
/// string with the fraction removed), and the fractional part is scaled to
/// picoseconds directly from its digit text, never through `chrono` or
/// `f64`.
fn parse_subnanosecond_iso8601(
    input: &str,
    format: TimestampFormat,
    before: &str,
    frac_digits: &str,
    suffix: &str,
) -> crate::Result<Timestamp> {
    let whole_seconds_text = format!("{before}{suffix}");
    let picos_of_second = frac_digits_to_picos(frac_digits);
    match format {
        TimestampFormat::Iso8601WithOffset => {
            let parsed =
                chrono::DateTime::parse_from_rfc3339(&whole_seconds_text).map_err(|source| {
                    crate::GlydeError::InvalidTimestamp {
                        input: input.to_string(),
                        format,
                        reason: source.to_string(),
                    }
                })?;
            let ticks = i128::from(parsed.timestamp()) * TimeUnit::Picoseconds.ticks_per_second()
                + picos_of_second;
            Ok(Timestamp::with_offset(
                ticks,
                TimeUnit::Picoseconds,
                parsed.offset().local_minus_utc(),
            ))
        }
        TimestampFormat::Iso8601Naive => {
            let parsed =
                chrono::NaiveDateTime::parse_from_str(&whole_seconds_text, "%Y-%m-%dT%H:%M:%S")
                    .map_err(|source| crate::GlydeError::InvalidTimestamp {
                        input: input.to_string(),
                        format,
                        reason: source.to_string(),
                    })?;
            let ticks = i128::from(parsed.and_utc().timestamp())
                * TimeUnit::Picoseconds.ticks_per_second()
                + picos_of_second;
            Ok(Timestamp::new(ticks, TimeUnit::Picoseconds))
        }
        _ => unreachable!("only called for the two ISO 8601 formats"),
    }
}

/// The inverse of [`parse_subnanosecond_iso8601`] for
/// [`TimestampFormat::Iso8601WithOffset`]: rebuilds the whole-second instant
/// with `chrono` (for date/time/offset formatting) and appends the exact
/// 12-digit picosecond fraction as plain text, since `chrono` cannot format
/// sub-nanosecond precision itself.
fn format_subnanosecond_iso8601_with_offset(timestamp: &Timestamp) -> String {
    let (whole_seconds, picos_of_second) = split_picosecond_ticks(timestamp);
    let offset_seconds = timestamp.offset_seconds.unwrap_or(0);
    let offset = chrono::FixedOffset::east_opt(offset_seconds)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("zero offset is always valid"));
    let local = utc_datetime_from_whole_seconds(whole_seconds).with_timezone(&offset);
    format!(
        "{}.{picos_of_second:012}{}",
        local.format("%Y-%m-%dT%H:%M:%S"),
        local.format("%:z")
    )
}

/// The inverse of [`parse_subnanosecond_iso8601`] for
/// [`TimestampFormat::Iso8601Naive`].
fn format_subnanosecond_iso8601_naive(timestamp: &Timestamp) -> String {
    let (whole_seconds, picos_of_second) = split_picosecond_ticks(timestamp);
    format!(
        "{}.{picos_of_second:012}",
        utc_datetime_from_whole_seconds(whole_seconds).format("%Y-%m-%dT%H:%M:%S")
    )
}

/// Splits a [`TimeUnit::Picoseconds`]-resolution [`Timestamp`] into whole
/// seconds since the Unix epoch and the picosecond-of-second remainder.
fn split_picosecond_ticks(timestamp: &Timestamp) -> (i128, i128) {
    let ticks_per_second = TimeUnit::Picoseconds.ticks_per_second();
    (
        timestamp.ticks.div_euclid(ticks_per_second),
        timestamp.ticks.rem_euclid(ticks_per_second),
    )
}

/// Builds a UTC `chrono` instant from a whole-second Unix timestamp (no
/// sub-second component) — the shared building block for formatting the
/// date/time/offset portion of a sub-nanosecond [`Timestamp`], whose exact
/// fractional digits are appended as plain text instead.
fn utc_datetime_from_whole_seconds(whole_seconds: i128) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(
        i64::try_from(whole_seconds).expect("picosecond-range instant fits in an i64 second count"),
        0,
    )
    .expect("a valid whole-second instant always builds")
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

/// Seconds between the LabVIEW/NI epoch (1904-01-01T00:00:00Z) and the Unix
/// epoch (1970-01-01T00:00:00Z), i.e. `labview_seconds - LABVIEW_EPOCH_OFFSET_SECONDS
/// == unix_seconds` (SPEC §2.1: "LabVIEW/NI epoch: seconds since
/// 1904-01-01T00:00:00Z").
const LABVIEW_EPOCH_OFFSET_SECONDS: i128 = 2_082_844_800;

/// Days between the Excel serial-date epoch (1899-12-30) and the Unix epoch
/// (1970-01-01), i.e. `excel_serial - EXCEL_EPOCH_OFFSET_DAYS == unix_days`
/// (SPEC §2.1: "Excel serial date: days since 1899-12-30"). 1899-12-30 (not
/// 1900-01-01) is Excel's actual epoch — it "counts" 1900 as a leap year in
/// error, and 1899-12-30 as day 0 absorbs that historical bug so serial
/// arithmetic still lands on the correct calendar date.
const EXCEL_EPOCH_OFFSET_DAYS: i128 = 25_569;

const SECONDS_PER_DAY: i128 = 86_400;

/// Splits a bare decimal numeral like `"3850027200.0"` or `"46023"` into its
/// integer whole part and, if present, its fractional digit text (kept as
/// text so callers scale it by exact `i128` arithmetic rather than `f64` —
/// SPEC §2.1/Golden Rule 1). Returns `None` for anything that is not
/// `[-]digits[.digits]` (scientific notation, thousands separators,
/// non-numeric text, a bare trailing `.` with no digits after it, ...).
fn split_decimal(field: &str) -> Option<(i128, &str)> {
    let field = field.trim();
    match field.split_once('.') {
        Some((whole, frac)) if !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit()) => {
            Some((whole.parse().ok()?, frac))
        }
        Some(_) => None,
        None => Some((field.parse().ok()?, "")),
    }
}

/// Exact ticks (in `ticks_per_whole`'s unit) represented by a decimal
/// fractional-part digit string, e.g. `frac_str_to_ticks("5", 1_000_000_000)`
/// (.5 of a whole) -> `500_000_000`. Truncates rather than rounds when
/// `ticks_per_whole` does not divide evenly — no corpus fixture exercises
/// that case, and it never arises for the exact multiples this module's
/// formats produce.
fn frac_str_to_ticks(frac_digits: &str, ticks_per_whole: i128) -> i128 {
    if frac_digits.is_empty() {
        return 0;
    }
    let numerator: i128 = frac_digits
        .parse()
        .expect("split_decimal only ever returns all-ASCII-digit fractional text");
    let denominator = 10_i128.pow(frac_digits.len() as u32);
    numerator * ticks_per_whole / denominator
}

/// Renders `numerator / denominator` (a fraction in `[0, 1)`) as the
/// shortest decimal digit string that reconstructs it exactly, trying up to
/// `max_digits` digits of precision — the inverse of [`frac_str_to_ticks`]
/// for formats whose native whole unit (a calendar day, for
/// [`TimestampFormat::ExcelSerial`]) is not itself a power of ten. Falls
/// back to `max_digits` of truncated precision when no exact digit count is
/// found in range (a fraction with no finite decimal expansion at that
/// precision) — not exercised by any corpus fixture, whose values are all
/// whole.
fn fraction_to_decimal_digits(numerator: i128, denominator: i128, max_digits: u32) -> String {
    for digits in 1..=max_digits {
        let scaled = numerator * 10_i128.pow(digits);
        if scaled % denominator == 0 {
            return format!("{:0width$}", scaled / denominator, width = digits as usize);
        }
    }
    let scaled = numerator * 10_i128.pow(max_digits);
    format!(
        "{:0width$}",
        scaled / denominator,
        width = max_digits as usize
    )
}

fn parse_labview_epoch(input: &str) -> crate::Result<Timestamp> {
    let (whole, frac) =
        split_decimal(input).ok_or_else(|| crate::GlydeError::InvalidTimestamp {
            input: input.to_string(),
            format: TimestampFormat::LabViewEpoch,
            reason: "not a decimal number of seconds".to_string(),
        })?;
    let ticks_per_second = TimeUnit::Nanoseconds.ticks_per_second();
    let unix_seconds = whole - LABVIEW_EPOCH_OFFSET_SECONDS;
    let ticks = unix_seconds * ticks_per_second + frac_str_to_ticks(frac, ticks_per_second);
    Ok(Timestamp::new(ticks, TimeUnit::Nanoseconds))
}

fn format_labview_epoch(timestamp: &Timestamp) -> String {
    let ticks_per_second = TimeUnit::Nanoseconds.ticks_per_second();
    let unix_seconds = timestamp.ticks.div_euclid(ticks_per_second);
    let frac_ns = timestamp.ticks.rem_euclid(ticks_per_second);
    let labview_seconds = unix_seconds + LABVIEW_EPOCH_OFFSET_SECONDS;
    if frac_ns == 0 {
        labview_seconds.to_string()
    } else {
        let frac_digits = fraction_to_decimal_digits(frac_ns, ticks_per_second, 9);
        format!("{labview_seconds}.{frac_digits}")
    }
}

fn parse_excel_serial(input: &str) -> crate::Result<Timestamp> {
    let (whole, frac) =
        split_decimal(input).ok_or_else(|| crate::GlydeError::InvalidTimestamp {
            input: input.to_string(),
            format: TimestampFormat::ExcelSerial,
            reason: "not a decimal number of days".to_string(),
        })?;
    let ticks_per_day = SECONDS_PER_DAY * TimeUnit::Nanoseconds.ticks_per_second();
    let unix_days = whole - EXCEL_EPOCH_OFFSET_DAYS;
    let ticks = unix_days * ticks_per_day + frac_str_to_ticks(frac, ticks_per_day);
    Ok(Timestamp::new(ticks, TimeUnit::Nanoseconds))
}

fn format_excel_serial(timestamp: &Timestamp) -> String {
    let ticks_per_day = SECONDS_PER_DAY * TimeUnit::Nanoseconds.ticks_per_second();
    let unix_days = timestamp.ticks.div_euclid(ticks_per_day);
    let frac_ns_of_day = timestamp.ticks.rem_euclid(ticks_per_day);
    let serial = unix_days + EXCEL_EPOCH_OFFSET_DAYS;
    if frac_ns_of_day == 0 {
        serial.to_string()
    } else {
        let frac_digits = fraction_to_decimal_digits(frac_ns_of_day, ticks_per_day, 12);
        format!("{serial}.{frac_digits}")
    }
}

/// The result of [`infer_timestamp_format`]: which [`TimestampFormat`] every
/// field matched, and whether that choice was a confident one or SPEC §2.1's
/// ambiguity-rule fallback — mirroring `ingest::infer::DtypeInference`'s
/// `ambiguous` flag (SPEC §1.2 "confidence is tracked per inference").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampFormatInference {
    pub format: TimestampFormat,
    /// `true` for the [`TimestampFormat::DayFirst`]/[`TimestampFormat::MonthFirst`]
    /// ambiguity fallback and for [`TimestampFormat::EpochSeconds`] when a
    /// bare-integer column's magnitude also falls in
    /// [`TimestampFormat::LabViewEpoch`]'s plausibility window (SPEC §2.1:
    /// "If the whole column is ambiguous ... mark the inference low
    /// confidence") — both are the same shape: two absolute-time readings
    /// are syntactically indistinguishable, so a single pick is reported as
    /// the best guess but flagged, never asserted with full confidence.
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
/// list is tried under. `DateTimeSpace` is a separate, not-yet-started
/// roadmap item and is never inferred.
const IN_SCOPE_FORMATS: [TimestampFormat; 8] = [
    TimestampFormat::Iso8601WithOffset,
    TimestampFormat::Iso8601Naive,
    TimestampFormat::EpochNanos,
    TimestampFormat::EpochMicros,
    TimestampFormat::EpochMillis,
    TimestampFormat::EpochSeconds,
    TimestampFormat::LabViewEpoch,
    TimestampFormat::ExcelSerial,
];

/// The plausibility window every epoch-like format's magnitude check is
/// built from: `2001-09-09` to `2286-11-20` in whole seconds since the Unix
/// epoch — comfortably covering any real sensor log, and far above a
/// small index-like integer column (corpus case 35: `0`..`5`).
const PLAUSIBLE_EPOCH_SECONDS: (i128, i128) = (1_000_000_000, 10_000_000_000);

/// A permissive digit-count window used only to disambiguate *which* epoch
/// resolution a column of bare integers represents (SPEC §2.1 lists epoch
/// seconds/millis/micros/nanos as distinct formats with no delimiter to tell
/// them apart syntactically). Brackets [`PLAUSIBLE_EPOCH_SECONDS`] in
/// `unit`'s resolution, so a small index-like integer column matches none of
/// them and is correctly left as "no absolute timestamp format", rather than
/// misidentified as an implausible 1970-01-01 epoch.
fn plausible_epoch_magnitude(unit: TimeUnit) -> (i128, i128) {
    let per_second = unit.ticks_per_second();
    let (min, max) = PLAUSIBLE_EPOCH_SECONDS;
    (min * per_second, max * per_second)
}

/// The same [`PLAUSIBLE_EPOCH_SECONDS`] window, shifted from Unix seconds
/// into LabVIEW epoch seconds (SPEC §2.1), so a column of small integers
/// (or one already claimed by [`TimestampFormat::EpochSeconds`]) is never
/// misidentified as a LabVIEW timestamp, and vice versa.
fn plausible_labview_epoch_magnitude() -> (i128, i128) {
    let (min, max) = PLAUSIBLE_EPOCH_SECONDS;
    (
        min + LABVIEW_EPOCH_OFFSET_SECONDS,
        max + LABVIEW_EPOCH_OFFSET_SECONDS,
    )
}

/// The same [`PLAUSIBLE_EPOCH_SECONDS`] window, converted from Unix seconds
/// to whole days and shifted into Excel serial days (SPEC §2.1).
fn plausible_excel_serial_magnitude() -> (i128, i128) {
    let (min, max) = PLAUSIBLE_EPOCH_SECONDS;
    (
        min / SECONDS_PER_DAY + EXCEL_EPOCH_OFFSET_DAYS,
        max / SECONDS_PER_DAY + EXCEL_EPOCH_OFFSET_DAYS,
    )
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
/// [`plausible_epoch_magnitude`] for the epoch formats and
/// [`plausible_labview_epoch_magnitude`]/[`plausible_excel_serial_magnitude`]
/// for their two counterparts, so a small progressive-index-like integer
/// column is never claimed by any of them just because it happens to parse
/// as one syntactically.
fn field_matches_format(field: &str, format: TimestampFormat) -> bool {
    let field = field.trim();
    match epoch_unit(format) {
        Some(unit) => {
            let (min, max) = plausible_epoch_magnitude(unit);
            field
                .parse::<i128>()
                .is_ok_and(|value| (min..max).contains(&value.abs()))
        }
        None => match format {
            TimestampFormat::LabViewEpoch => {
                let (min, max) = plausible_labview_epoch_magnitude();
                split_decimal(field).is_some_and(|(whole, _)| (min..max).contains(&whole))
            }
            TimestampFormat::ExcelSerial => {
                let (min, max) = plausible_excel_serial_magnitude();
                split_decimal(field).is_some_and(|(whole, _)| (min..max).contains(&whole))
            }
            _ => parse_timestamp(field, format).is_ok(),
        },
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
        let ambiguous = epoch_seconds_labview_epoch_overlap(format, fields);
        if ambiguous {
            warn!(
                field_count = fields.len(),
                "column magnitude matches both EpochSeconds and LabViewEpoch's plausibility \
                 windows (SPEC §2.1) — defaulting to the far more common EpochSeconds reading; \
                 low-confidence inference"
            );
        }
        return Some(TimestampFormatInference { format, ambiguous });
    }

    infer_day_month_format(fields)
}

/// Whether `format`/`fields` is [`TimestampFormat::EpochSeconds`] chosen for
/// a column whose magnitude *also* falls in
/// [`plausible_labview_epoch_magnitude`]'s window — i.e. every field is a
/// bare integer syntactically valid under both readings, ~66 years apart
/// ([`LABVIEW_EPOCH_OFFSET_SECONDS`]) depending on which one is right (PR #44
/// review: [`IN_SCOPE_FORMATS`] tries `EpochSeconds` first, so without this
/// check that reading would win with full, unwarranted confidence — Golden
/// Rule 2). `LabViewEpoch` itself never needs the symmetric check: reaching
/// it here already means every field failed `EpochSeconds`'s pure-integer
/// match (almost always because of a decimal point, e.g. corpus case 34's
/// `.0`), which is itself the disambiguating evidence.
fn epoch_seconds_labview_epoch_overlap(format: TimestampFormat, fields: &[String]) -> bool {
    format == TimestampFormat::EpochSeconds
        && fields
            .iter()
            .all(|field| field_matches_format(field, TimestampFormat::LabViewEpoch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::corpus_fixture::corpus_column;

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

    // Corpus case 33 (docs/QUALITY.md §1.33): Excel serial dates — integer
    // days since 1899-12-30 (SPEC §2.1). `46023` must be recognized as
    // `ExcelSerial`, not mistaken for a progressive numeric index, and
    // convert to the same Unix-epoch instant `EXCEL_EPOCH_OFFSET_DAYS`
    // documents (2026-01-01T00:00:00Z).
    #[test]
    fn corpus_case_33_excel_serial_dates_detects_and_parses() {
        let fields = corpus_column("case-33-excel-serial-dates.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::ExcelSerial);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        assert_eq!(
            timestamps[0],
            Timestamp::new(
                (46_023 - EXCEL_EPOCH_OFFSET_DAYS) * SECONDS_PER_DAY * 1_000_000_000,
                TimeUnit::Nanoseconds
            )
        );
        assert_monotonically_increasing(&timestamps);
    }

    // Corpus case 34 (docs/QUALITY.md §1.34): LabVIEW/NI epoch — seconds
    // since 1904-01-01 (SPEC §2.1), written as `3850027200.0` (a trailing
    // `.0` that also happens to keep this column from being mistaken for
    // `EpochSeconds`, whose plausibility window it would otherwise overlap).
    #[test]
    fn corpus_case_34_labview_epoch_detects_and_parses() {
        let fields = corpus_column("case-34-labview-epoch.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::LabViewEpoch);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        assert_eq!(
            timestamps[0],
            Timestamp::new(
                (3_850_027_200 - LABVIEW_EPOCH_OFFSET_SECONDS) * 1_000_000_000,
                TimeUnit::Nanoseconds
            )
        );
        assert_monotonically_increasing(&timestamps);
    }

    // Corpus case 41 (docs/QUALITY.md §1.41): a 12-fractional-digit ISO 8601
    // column — picosecond resolution. `chrono`'s nanosecond-only fractional
    // support would silently round this away (Golden Rule 1), so values this
    // fine must come back with `unit == TimeUnit::Picoseconds` and every
    // exact picosecond tick preserved, not just "close".
    #[test]
    fn corpus_case_41_picosecond_resolution_index_detects_and_parses() {
        let fields = corpus_column("case-41-picosecond-resolution-index.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::Iso8601WithOffset);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        for timestamp in &timestamps {
            assert_eq!(timestamp.unit, TimeUnit::Picoseconds);
        }
        assert_monotonically_increasing(&timestamps);
        for pair in timestamps.windows(2) {
            assert_eq!(
                pair[1].ticks - pair[0].ticks,
                1,
                "corpus fixture increments by exactly one picosecond per row"
            );
        }
    }

    // Corpus case 42 (docs/QUALITY.md §1.42): plain ISO 8601 `Z`-suffixed
    // timestamps spanning multiple years at one-year (second-scale)
    // resolution — a scale check that a multi-year span parses with no
    // precision loss through the ordinary (already-implemented) ISO 8601
    // path, the same guarantee the hand-built golden test proves for the
    // `Timestamp` type directly.
    #[test]
    fn corpus_case_42_multi_year_second_resolution_detects_and_parses() {
        let fields = corpus_column("case-42-multi-year-second-resolution.csv", "timestamp");

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::Iso8601WithOffset);
        assert!(!inference.ambiguous);

        let timestamps: Vec<Timestamp> = fields
            .iter()
            .map(|field| parse_timestamp(field, inference.format).expect("must parse"))
            .collect();
        assert_eq!(timestamps.len(), 6);
        for timestamp in &timestamps {
            assert_eq!(timestamp.unit, TimeUnit::Nanoseconds);
        }
        assert_monotonically_increasing(&timestamps);
    }

    // Not a corpus case: Excel serial's fractional part (time of day, SPEC
    // §2.1) and LabVIEW epoch's fractional seconds are exercised by no
    // fixture (every corpus value is whole), so their round trip is proven
    // here directly rather than skipped untested.
    #[test]
    fn excel_serial_fractional_day_round_trips() {
        let parsed = parse_timestamp("46023.5", TimestampFormat::ExcelSerial).expect("must parse");
        assert_eq!(
            format_timestamp(&parsed, TimestampFormat::ExcelSerial),
            "46023.5"
        );
    }

    #[test]
    fn labview_epoch_fractional_second_round_trips() {
        let parsed =
            parse_timestamp("3850027200.25", TimestampFormat::LabViewEpoch).expect("must parse");
        assert_eq!(
            format_timestamp(&parsed, TimestampFormat::LabViewEpoch),
            "3850027200.25"
        );
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

    // Flagged in PR #44 review: a bare-integer field (no trailing `.0`, the
    // ordinary shape for a plain LabVIEW/NI logger export) whose magnitude
    // falls inside the overlap between `EpochSeconds`'s and `LabViewEpoch`'s
    // plausibility windows (~3.08-10 billion) is a syntactically valid
    // reading under *both* formats, ~66 years apart
    // (`LABVIEW_EPOCH_OFFSET_SECONDS`). `IN_SCOPE_FORMATS` tries
    // `EpochSeconds` first, so without this check it would win confidently
    // and silently — exactly the "wrong-but-silent" failure CLAUDE.md Golden
    // Rule 2 forbids. `EpochSeconds` is still the reported best guess (by far
    // the more common real-world format at this magnitude), but the column
    // must come back low-confidence, the same treatment SPEC §2.1 requires
    // for the DD/MM vs MM/DD ambiguity.
    #[test]
    fn infer_timestamp_format_flags_the_epoch_seconds_labview_epoch_overlap_as_ambiguous() {
        let fields = vec!["3850027200".to_string(), "3850027201".to_string()];

        let inference = infer_timestamp_format(&fields).expect("must infer a format");
        assert_eq!(inference.format, TimestampFormat::EpochSeconds);
        assert!(
            inference.ambiguous,
            "a bare integer in the EpochSeconds/LabViewEpoch overlap must never be a silent \
             confident pick (CLAUDE.md Golden Rule 2)"
        );
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
