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

//! Time shown as dates and times (issue #196): where the time axis puts its
//! labels, what each label says, and the readable form of a timestamp in
//! the cursor readout.
//!
//! Everything here is integer arithmetic on a [`Timestamp`]'s `i128` ticks
//! in their native [`TimeUnit`] — never `f64` seconds (SPEC §2.1) — so a
//! label at deep zoom on a nanosecond or picosecond axis is exact.
//!
//! * [`calendar_ticks`] picks the smallest "round" step (1/2/5 × 10ⁿ below a
//!   second; 1, 2, 5, 10, 15, 30 s and min; 1, 2, 3, 6, 12 h; 1 and 2 days;
//!   1 and 2 weeks from a Monday; 1, 2, 3, 6 months; 1, 2, 5 × 10ⁿ years) that puts at most `max_ticks` labels on the visible
//!   range, aligned to the wall clock of the axis's own zone. Each label is
//!   the time of day at the step's precision (`09:20`, `09:16:40`,
//!   `10:04:10.587`), or the date once the step is a day or more. The date
//!   (and, on the first label, the zone) goes on a second line only where
//!   it is new: on the first label and where the day changes.
//! * [`format_cursor_timestamp`] is the readout's timestamp: text formats
//!   round-trip as they always did; numeric ones (epoch, LabVIEW, Excel)
//!   read as a date and time, with the exact raw value still beside it
//!   (SPEC §4.1 "exact raw … timestamp").
//!
//! The zone follows SPEC §2.1: an offset the source carried is honored and
//! shown, a UTC epoch says `UTC`, and a naive source is labelled
//! `naive local time` — never an invented zone.

use chrono::{Datelike, NaiveDate};

use super::format::{format_timestamp, TimeUnit, Timestamp, TimestampFormat};

const SECONDS_PER_DAY: i128 = 86_400;
const PICOS_PER_SECOND: i128 = 1_000_000_000_000;
/// `chrono`'s day number (days since 0001-01-01, plus one) of 1970-01-01.
const UNIX_EPOCH_DAY_FROM_CE: i128 = 719_163;
/// 1970-01-05, the first Monday after the epoch, in days: where weekly
/// labels are aligned from.
const FIRST_MONDAY_DAY: i128 = 4;

/// The zone an axis's wall clock is read in (SPEC §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisZone {
    /// A numeric epoch defined in UTC (Unix epoch, LabVIEW).
    Utc,
    /// The UTC offset, in seconds, the source itself carried.
    Offset(i32),
    /// No zone in the source: naive local time, labelled as such.
    NaiveLocal,
}

impl AxisZone {
    /// The zone of a timestamp in `format` whose row carried
    /// `offset_seconds`. The same rule the inference bar's timezone field
    /// states (`ingest::report`'s `TimezoneLabel`).
    pub fn for_format(format: TimestampFormat, offset_seconds: Option<i32>) -> Self {
        use TimestampFormat::*;
        match format {
            EpochSeconds | EpochMillis | EpochMicros | EpochNanos | LabViewEpoch => AxisZone::Utc,
            Iso8601WithOffset | ParquetTimestamp => match offset_seconds {
                Some(offset) => AxisZone::Offset(offset),
                None => AxisZone::NaiveLocal,
            },
            Iso8601Naive | DateTimeSpace | DayFirst | MonthFirst | ExcelSerial => {
                AxisZone::NaiveLocal
            }
        }
    }

    /// How the axis names the zone: `UTC`, `UTC+02:00`, `naive local time`.
    pub fn label(&self) -> String {
        match *self {
            AxisZone::Utc | AxisZone::Offset(0) => "UTC".to_string(),
            AxisZone::Offset(offset) => {
                let sign = if offset < 0 { '-' } else { '+' };
                let magnitude = offset.unsigned_abs();
                format!(
                    "UTC{sign}{:02}:{:02}",
                    magnitude / 3_600,
                    (magnitude % 3_600) / 60
                )
            }
            AxisZone::NaiveLocal => "naive local time".to_string(),
        }
    }

    /// Seconds to add to a UTC instant to read its wall clock.
    fn offset_seconds(&self) -> i128 {
        match *self {
            AxisZone::Offset(offset) => i128::from(offset),
            AxisZone::Utc | AxisZone::NaiveLocal => 0,
        }
    }
}

/// How a date is written: ISO by default, or in the day/month order of a
/// `DD/MM/YYYY` or `MM/DD/YYYY` source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateOrder {
    Iso,
    DayFirst,
    MonthFirst,
}

impl DateOrder {
    fn for_format(format: TimestampFormat) -> Self {
        match format {
            TimestampFormat::DayFirst => DateOrder::DayFirst,
            TimestampFormat::MonthFirst => DateOrder::MonthFirst,
            _ => DateOrder::Iso,
        }
    }

    fn date(self, date: NaiveDate) -> String {
        let (y, m, d) = (date.year(), date.month(), date.day());
        match self {
            DateOrder::Iso => format!("{y:04}-{m:02}-{d:02}"),
            DateOrder::DayFirst => format!("{d:02}/{m:02}/{y:04}"),
            DateOrder::MonthFirst => format!("{m:02}/{d:02}/{y:04}"),
        }
    }

    fn month(self, date: NaiveDate) -> String {
        let (y, m) = (date.year(), date.month());
        match self {
            DateOrder::Iso => format!("{y:04}-{m:02}"),
            DateOrder::DayFirst | DateOrder::MonthFirst => format!("{m:02}/{y:04}"),
        }
    }
}

/// Everything about a label that comes from the source, not the zoom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisLabelStyle {
    pub zone: AxisZone,
    pub date_order: DateOrder,
}

impl AxisLabelStyle {
    /// The style of an axis in `format` whose rows carry `offset_seconds`
    /// (for a per-row offset, the one of the sample nearest the view).
    pub fn new(format: TimestampFormat, offset_seconds: Option<i32>) -> Self {
        Self {
            zone: AxisZone::for_format(format, offset_seconds),
            date_order: DateOrder::for_format(format),
        }
    }
}

/// One labelled position on the time axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarTick {
    /// Where, in the axis's own ticks (the true UTC-based instant).
    pub tick: i128,
    /// The distance to the next label of this step, in ticks — exact for a
    /// fixed step, the length of this month or year for a calendar one.
    pub step_ticks: i128,
    /// The label: time of day at the step's precision, or the date for a
    /// step of a day or more.
    pub label: String,
    /// A second line, only where it is new: the date (on the first label and
    /// where the day changes) and, on the first label, the zone.
    pub context: Option<String>,
}

/// A candidate label step.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// A fixed duration in picoseconds, aligned to whole multiples of it
    /// counted from `origin_days` (local midnight of that day).
    Fixed { picos: i128, origin_days: i128 },
    /// A whole number of calendar months, aligned to month (or, for a
    /// multiple of 12, year) boundaries.
    Months(i128),
}

/// Every candidate step, finest first. Consecutive steps are at most ~2.5×
/// apart, so the one chosen always leaves at least two labels on a range
/// wide enough for five.
fn steps() -> impl Iterator<Item = Step> {
    let fixed = |picos: i128| Step::Fixed {
        picos,
        origin_days: 0,
    };
    let sub_second = (0..12).flat_map(move |exponent| {
        [1, 2, 5]
            .into_iter()
            .map(move |mantissa| fixed(mantissa * 10_i128.pow(exponent)))
    });
    let clock = [
        1, 2, 5, 10, 15, 30, // seconds
        60, 120, 300, 600, 900, 1_800, // minutes
        3_600, 7_200, 10_800, 21_600, 43_200, // hours
        86_400, 172_800, // days
    ]
    .into_iter()
    .map(move |seconds| fixed(seconds * PICOS_PER_SECOND));
    let weeks = [7, 14].into_iter().map(|days| Step::Fixed {
        picos: days * SECONDS_PER_DAY * PICOS_PER_SECOND,
        origin_days: FIRST_MONDAY_DAY,
    });
    let months = [1, 2, 3, 6].into_iter().map(Step::Months);
    let years = (0..6).flat_map(|exponent| {
        [1, 2, 5]
            .into_iter()
            .map(move |mantissa| Step::Months(12 * mantissa * 10_i128.pow(exponent)))
    });
    sub_second
        .chain(clock)
        .chain(weeks)
        .chain(months)
        .chain(years)
}

/// How much of a wall-clock instant a label shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Precision {
    Year,
    Month,
    Date,
    Minutes,
    Seconds,
    /// Seconds and this many fractional digits (1..=12).
    Fraction(usize),
}

impl Precision {
    fn of(step: Step) -> Self {
        match step {
            Step::Months(months) if months % 12 == 0 => Precision::Year,
            Step::Months(_) => Precision::Month,
            Step::Fixed { picos, .. } => {
                if picos % (SECONDS_PER_DAY * PICOS_PER_SECOND) == 0 {
                    Precision::Date
                } else if picos % (60 * PICOS_PER_SECOND) == 0 {
                    Precision::Minutes
                } else if picos % PICOS_PER_SECOND == 0 {
                    Precision::Seconds
                } else {
                    let mut zeros = 0;
                    let mut rest = picos;
                    while rest % 10 == 0 {
                        rest /= 10;
                        zeros += 1;
                    }
                    Precision::Fraction(12 - zeros)
                }
            }
        }
    }

    fn is_day_or_more(self) -> bool {
        matches!(self, Precision::Year | Precision::Month | Precision::Date)
    }
}

/// A local (wall-clock) instant, split into calendar parts.
#[derive(Debug, Clone, Copy)]
struct Wall {
    /// Days since 1970-01-01 on the wall clock.
    days: i128,
    second_of_day: i128,
    /// Picoseconds into the second, `0..10¹²`.
    picos: i128,
    date: NaiveDate,
}

impl Wall {
    fn of(local_ticks: i128, unit: TimeUnit) -> Option<Self> {
        let per_second = unit.ticks_per_second();
        let seconds = local_ticks.div_euclid(per_second);
        let picos = local_ticks.rem_euclid(per_second) * (PICOS_PER_SECOND / per_second);
        let days = seconds.div_euclid(SECONDS_PER_DAY);
        let date = date_of_day(days)?;
        Some(Self {
            days,
            second_of_day: seconds.rem_euclid(SECONDS_PER_DAY),
            picos,
            date,
        })
    }

    /// Months since year 0, January: what monthly steps align to.
    fn month_index(&self) -> i128 {
        i128::from(self.date.year()) * 12 + i128::from(self.date.month0())
    }

    fn is_month_start(&self) -> bool {
        self.date.day() == 1 && self.second_of_day == 0 && self.picos == 0
    }

    fn clock(&self, fraction_digits: usize, with_seconds: bool) -> String {
        let (h, m, s) = (
            self.second_of_day / 3_600,
            (self.second_of_day % 3_600) / 60,
            self.second_of_day % 60,
        );
        let mut text = if with_seconds {
            format!("{h:02}:{m:02}:{s:02}")
        } else {
            format!("{h:02}:{m:02}")
        };
        if fraction_digits > 0 {
            let digits = format!("{:012}", self.picos);
            text.push('.');
            text.push_str(&digits[..fraction_digits.min(12)]);
        }
        text
    }

    fn label(&self, precision: Precision, order: DateOrder) -> String {
        match precision {
            Precision::Year => format!("{:04}", self.date.year()),
            Precision::Month => order.month(self.date),
            Precision::Date => order.date(self.date),
            Precision::Minutes => self.clock(0, false),
            Precision::Seconds => self.clock(0, true),
            Precision::Fraction(digits) => self.clock(digits, true),
        }
    }
}

fn date_of_day(days_since_epoch: i128) -> Option<NaiveDate> {
    let from_ce = i32::try_from(days_since_epoch.checked_add(UNIX_EPOCH_DAY_FROM_CE)?).ok()?;
    NaiveDate::from_num_days_from_ce_opt(from_ce)
}

/// Local ticks of the first instant of month `index` (months since year 0).
fn month_start(index: i128, unit: TimeUnit) -> Option<i128> {
    let year = i32::try_from(index.div_euclid(12)).ok()?;
    let month = u32::try_from(index.rem_euclid(12) + 1).ok()?;
    let date = NaiveDate::from_ymd_opt(year, month, 1)?;
    let days = i128::from(date.num_days_from_ce()) - UNIX_EPOCH_DAY_FROM_CE;
    days.checked_mul(SECONDS_PER_DAY * unit.ticks_per_second())
}

fn floor_div(a: i128, b: i128) -> i128 {
    a.div_euclid(b)
}

fn ceil_div(a: i128, b: i128) -> i128 {
    -((-a).div_euclid(b))
}

/// The labels for the visible range `start..=end` (ticks in `unit`): the
/// smallest round step that yields at most `max_ticks` of them, every one
/// inside the range, in increasing order. Empty for `max_ticks == 0`, an
/// empty or reversed range, or a range outside the calendar's reach.
pub fn calendar_ticks(
    start: i128,
    end: i128,
    unit: TimeUnit,
    style: &AxisLabelStyle,
    max_ticks: usize,
) -> Vec<CalendarTick> {
    if max_ticks == 0 || start > end {
        return Vec::new();
    }
    let per_second = unit.ticks_per_second();
    let picos_per_tick = PICOS_PER_SECOND / per_second;
    let Some(offset) = style.zone.offset_seconds().checked_mul(per_second) else {
        return Vec::new();
    };
    let (Some(local_start), Some(local_end)) = (start.checked_add(offset), end.checked_add(offset))
    else {
        return Vec::new();
    };
    let budget = max_ticks as i128;

    for step in steps() {
        // Each candidate's labels as local-tick positions, if it fits.
        let positions: Vec<(i128, i128)> = match step {
            Step::Fixed { picos, origin_days } => {
                // Only a whole number of the axis's own ticks.
                if picos % picos_per_tick != 0 {
                    continue;
                }
                let size = picos / picos_per_tick;
                let Some(origin) = origin_days.checked_mul(SECONDS_PER_DAY * per_second) else {
                    return Vec::new();
                };
                let first = ceil_div(local_start - origin, size);
                let last = floor_div(local_end - origin, size);
                if last - first + 1 > budget {
                    continue;
                }
                (first..=last).map(|k| (origin + k * size, size)).collect()
            }
            Step::Months(months) => {
                let (Some(start_wall), Some(end_wall)) =
                    (Wall::of(local_start, unit), Wall::of(local_end, unit))
                else {
                    return Vec::new();
                };
                let first_boundary =
                    start_wall.month_index() + i128::from(!start_wall.is_month_start());
                let first = ceil_div(first_boundary, months);
                let last = floor_div(end_wall.month_index(), months);
                if last - first + 1 > budget {
                    continue;
                }
                let mut positions = Vec::new();
                for k in first..=last {
                    let (Some(at), Some(next)) = (
                        month_start(k * months, unit),
                        month_start((k + 1) * months, unit),
                    ) else {
                        return Vec::new();
                    };
                    positions.push((at, next - at));
                }
                positions
            }
        };
        return label_positions(&positions, offset, unit, style, Precision::of(step))
            .unwrap_or_default();
    }
    Vec::new()
}

/// Labels local-tick `positions` (each with its step) at `precision`; `None`
/// if any falls outside the calendar.
fn label_positions(
    positions: &[(i128, i128)],
    offset: i128,
    unit: TimeUnit,
    style: &AxisLabelStyle,
    precision: Precision,
) -> Option<Vec<CalendarTick>> {
    let zone = style.zone.label();
    let mut previous_day = None;
    positions
        .iter()
        .map(|&(local, step_ticks)| {
            let wall = Wall::of(local, unit)?;
            let context = match previous_day {
                None if precision.is_day_or_more() => Some(zone.clone()),
                None => Some(format!("{} · {zone}", style.date_order.date(wall.date))),
                Some(day) if !precision.is_day_or_more() && day != wall.days => {
                    Some(style.date_order.date(wall.date))
                }
                Some(_) => None,
            };
            previous_day = Some(wall.days);
            Some(CalendarTick {
                tick: local - offset,
                step_ticks,
                label: wall.label(precision, style.date_order),
                context,
            })
        })
        .collect()
}

/// The cursor readout's timestamp (SPEC §4.1): a text format as the source
/// wrote it ([`format_timestamp`]); a numeric one as a date and time in its
/// zone, followed by the exact raw value, e.g.
/// `2026-06-21 10:04:10.587450 UTC (1782036250587450)`.
pub fn format_cursor_timestamp(timestamp: &Timestamp, format: TimestampFormat) -> String {
    use TimestampFormat::*;
    let raw = format_timestamp(timestamp, format);
    match format {
        EpochSeconds | EpochMillis | EpochMicros | EpochNanos | LabViewEpoch | ExcelSerial => {
            match readable_timestamp(timestamp, format) {
                Some(readable) => format!("{readable} ({raw})"),
                None => raw,
            }
        }
        Iso8601WithOffset | Iso8601Naive | DateTimeSpace | DayFirst | MonthFirst
        | ParquetTimestamp => raw,
    }
}

/// `YYYY-MM-DD HH:MM:SS[.fff…] [zone]` at the timestamp's full native
/// precision, trailing zero groups of three dropped (`.500`, not
/// `.500000000`). A naive source gets no zone suffix.
fn readable_timestamp(timestamp: &Timestamp, format: TimestampFormat) -> Option<String> {
    let zone = AxisZone::for_format(format, timestamp.offset_seconds);
    let per_second = timestamp.unit.ticks_per_second();
    let local = timestamp
        .ticks
        .checked_add(zone.offset_seconds().checked_mul(per_second)?)?;
    let wall = Wall::of(local, timestamp.unit)?;
    let mut digits = per_second.ilog10() as usize;
    let all_digits = format!("{:012}", wall.picos);
    while digits >= 3 && all_digits[digits - 3..digits] == *"000" {
        digits -= 3;
    }
    let date = DateOrder::for_format(format).date(wall.date);
    let clock = wall.clock(digits, true);
    Some(match zone {
        AxisZone::NaiveLocal => format!("{date} {clock}"),
        AxisZone::Utc | AxisZone::Offset(_) => format!("{date} {clock} {}", zone.label()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::parse_timestamp;

    const US: i128 = 1_000_000;
    const NS: i128 = 1_000_000_000;

    fn utc() -> AxisLabelStyle {
        AxisLabelStyle::new(TimestampFormat::EpochMicros, None)
    }

    fn labels(ticks: &[CalendarTick]) -> Vec<&str> {
        ticks.iter().map(|t| t.label.as_str()).collect()
    }

    /// 2026-06-21 10:04:10.587450 UTC, the PV inverter file of issue #196.
    const PV_START_US: i128 = 1_782_036_250_587_450;

    // ---- zone and style -------------------------------------------------

    #[test]
    fn every_format_gets_the_zone_spec_2_1_gives_it() {
        use TimestampFormat::*;
        for format in [
            EpochSeconds,
            EpochMillis,
            EpochMicros,
            EpochNanos,
            LabViewEpoch,
        ] {
            assert_eq!(
                AxisZone::for_format(format, None),
                AxisZone::Utc,
                "{format:?}"
            );
        }
        for format in [
            Iso8601Naive,
            DateTimeSpace,
            DayFirst,
            MonthFirst,
            ExcelSerial,
            ParquetTimestamp,
        ] {
            assert_eq!(
                AxisZone::for_format(format, None),
                AxisZone::NaiveLocal,
                "{format:?}"
            );
        }
        assert_eq!(
            AxisZone::for_format(Iso8601WithOffset, Some(7_200)),
            AxisZone::Offset(7_200)
        );
        assert_eq!(
            AxisZone::for_format(ParquetTimestamp, Some(-18_000)),
            AxisZone::Offset(-18_000)
        );
        // An offset-carrying format whose row somehow has none is never
        // given an invented zone.
        assert_eq!(
            AxisZone::for_format(Iso8601WithOffset, None),
            AxisZone::NaiveLocal
        );
    }

    #[test]
    fn zone_labels_name_the_zone_and_never_invent_one() {
        assert_eq!(AxisZone::Utc.label(), "UTC");
        assert_eq!(AxisZone::Offset(0).label(), "UTC");
        assert_eq!(AxisZone::Offset(7_200).label(), "UTC+02:00");
        assert_eq!(AxisZone::Offset(-16_200).label(), "UTC-04:30");
        assert_eq!(AxisZone::NaiveLocal.label(), "naive local time");
    }

    #[test]
    fn a_day_first_or_month_first_source_keeps_its_date_order() {
        assert_eq!(
            AxisLabelStyle::new(TimestampFormat::DayFirst, None).date_order,
            DateOrder::DayFirst
        );
        assert_eq!(
            AxisLabelStyle::new(TimestampFormat::MonthFirst, None).date_order,
            DateOrder::MonthFirst
        );
        assert_eq!(
            AxisLabelStyle::new(TimestampFormat::EpochSeconds, None).date_order,
            DateOrder::Iso
        );
    }

    // ---- ticks: issue #196's own cases ----------------------------------

    // One hour of the PV inverter file at most 8 labels: 10-minute steps on
    // the wall clock, the date and zone once, on the first label.
    #[test]
    fn an_hour_of_epoch_microseconds_reads_as_times_of_day() {
        let ticks = calendar_ticks(
            PV_START_US,
            PV_START_US + 3_600 * US,
            TimeUnit::Microseconds,
            &utc(),
            8,
        );
        assert_eq!(
            labels(&ticks),
            ["10:10", "10:20", "10:30", "10:40", "10:50", "11:00"]
        );
        assert_eq!(ticks[0].context.as_deref(), Some("2026-06-21 · UTC"));
        assert!(ticks[1..].iter().all(|t| t.context.is_none()));
        assert_eq!(ticks[1].tick - ticks[0].tick, 600 * US);
        assert!(ticks.iter().all(|t| t.step_ticks == 600 * US));
    }

    #[test]
    fn the_date_is_repeated_only_where_the_day_changes() {
        // 2026-06-21 20:00 → 2026-06-22 04:00 UTC, at most 5 labels: 2 h.
        let start = 1_782_072_000 * US;
        let ticks = calendar_ticks(
            start,
            start + 8 * 3_600 * US,
            TimeUnit::Microseconds,
            &utc(),
            5,
        );
        assert_eq!(
            labels(&ticks),
            ["20:00", "22:00", "00:00", "02:00", "04:00"]
        );
        let contexts: Vec<Option<&str>> = ticks.iter().map(|t| t.context.as_deref()).collect();
        assert_eq!(
            contexts,
            [
                Some("2026-06-21 · UTC"),
                None,
                Some("2026-06-22"),
                None,
                None
            ]
        );
    }

    #[test]
    fn deep_zoom_shows_exactly_the_sub_second_digits_the_step_needs() {
        // 10 ms after 10:04:10.587450, at most 10 labels: a 1 ms step.
        let ticks = calendar_ticks(
            PV_START_US,
            PV_START_US + 10_000,
            TimeUnit::Microseconds,
            &utc(),
            10,
        );
        assert_eq!(ticks[0].label, "10:04:10.588");
        assert_eq!(ticks.last().unwrap().label, "10:04:10.597");
        // 2 µs over 10 labels: 0.2 µs is finer than a tick, so 1 µs it is.
        let fine = calendar_ticks(
            PV_START_US,
            PV_START_US + 2,
            TimeUnit::Microseconds,
            &utc(),
            10,
        );
        assert_eq!(
            labels(&fine),
            ["10:04:10.587450", "10:04:10.587451", "10:04:10.587452"]
        );
        // A 200 ms step needs one digit.
        let tenths = calendar_ticks(
            PV_START_US,
            PV_START_US + 1_000_000,
            TimeUnit::Microseconds,
            &utc(),
            5,
        );
        assert_eq!(tenths[0].label, "10:04:10.6");
    }

    #[test]
    fn picosecond_ticks_label_exactly_at_picosecond_precision() {
        // case-41's resolution: a few picoseconds after 2026-01-01T00:00:00.
        let start = 1_767_225_600 * 1_000_000_000_000_i128;
        let ticks = calendar_ticks(start + 1, start + 5, TimeUnit::Picoseconds, &utc(), 10);
        assert_eq!(
            labels(&ticks),
            [
                "00:00:00.000000000001",
                "00:00:00.000000000002",
                "00:00:00.000000000003",
                "00:00:00.000000000004",
                "00:00:00.000000000005",
            ]
        );
    }

    #[test]
    fn whole_seconds_and_minutes_drop_the_digits_they_do_not_need() {
        let start = 1_782_036_000 * NS; // 10:00:00
        let seconds = calendar_ticks(start, start + 50 * NS, TimeUnit::Nanoseconds, &utc(), 6);
        assert_eq!(
            labels(&seconds),
            ["10:00:00", "10:00:10", "10:00:20", "10:00:30", "10:00:40", "10:00:50"]
        );
        let quarter = calendar_ticks(start, start + 3_600 * NS, TimeUnit::Nanoseconds, &utc(), 5);
        assert_eq!(
            labels(&quarter),
            ["10:00", "10:15", "10:30", "10:45", "11:00"]
        );
    }

    #[test]
    fn day_steps_label_dates_and_weeks_start_on_monday() {
        // 2026-03-02 (a Monday) to 2026-03-08: one label a day.
        let monday = 1_772_409_600 * US;
        let days = calendar_ticks(
            monday,
            monday + 6 * 86_400 * US,
            TimeUnit::Microseconds,
            &utc(),
            7,
        );
        assert_eq!(
            labels(&days),
            [
                "2026-03-02",
                "2026-03-03",
                "2026-03-04",
                "2026-03-05",
                "2026-03-06",
                "2026-03-07",
                "2026-03-08"
            ]
        );
        // A date label carries only the zone, on the first one.
        assert_eq!(days[0].context.as_deref(), Some("UTC"));
        assert!(days[1..].iter().all(|t| t.context.is_none()));

        // Five weeks at most 6 labels: weekly, on Mondays.
        let weeks = calendar_ticks(
            monday - 86_400 * US,
            monday + 35 * 86_400 * US,
            TimeUnit::Microseconds,
            &utc(),
            6,
        );
        assert_eq!(
            labels(&weeks),
            [
                "2026-03-02",
                "2026-03-09",
                "2026-03-16",
                "2026-03-23",
                "2026-03-30",
                "2026-04-06"
            ]
        );
    }

    #[test]
    fn months_and_years_follow_the_calendar() {
        // 2024-11-15 → 2027-11-15 at most 7 labels: every 6 months.
        let start = 1_731_628_800 * NS;
        let end = 1_826_323_200 * NS;
        let half_years = calendar_ticks(start, end, TimeUnit::Nanoseconds, &utc(), 7);
        assert_eq!(
            labels(&half_years),
            ["2025-01", "2025-07", "2026-01", "2026-07", "2027-01", "2027-07"]
        );
        assert_eq!(half_years[0].context.as_deref(), Some("UTC"));
        // January to July 2025 is 181 days.
        assert_eq!(half_years[0].step_ticks, 181 * 86_400 * NS);

        let years = calendar_ticks(start, end, TimeUnit::Nanoseconds, &utc(), 3);
        assert_eq!(labels(&years), ["2025", "2026", "2027"]);

        let day_first = AxisLabelStyle::new(TimestampFormat::DayFirst, None);
        let months = calendar_ticks(start, end, TimeUnit::Nanoseconds, &day_first, 7);
        assert_eq!(months[0].label, "01/2025");
    }

    #[test]
    fn day_first_and_month_first_dates_keep_the_sources_order() {
        let start = 1_772_409_600 * NS + 9 * 3_600 * NS; // 2026-03-02 09:00
        let day_first = AxisLabelStyle::new(TimestampFormat::DayFirst, None);
        let ticks = calendar_ticks(
            start,
            start + 3_600 * NS,
            TimeUnit::Nanoseconds,
            &day_first,
            3,
        );
        assert_eq!(
            ticks[0].context.as_deref(),
            Some("02/03/2026 · naive local time")
        );
        let month_first = AxisLabelStyle::new(TimestampFormat::MonthFirst, None);
        let ticks = calendar_ticks(
            start,
            start + 3_600 * NS,
            TimeUnit::Nanoseconds,
            &month_first,
            3,
        );
        assert_eq!(
            ticks[0].context.as_deref(),
            Some("03/02/2026 · naive local time")
        );
    }

    #[test]
    fn an_offset_source_is_labelled_on_its_own_wall_clock() {
        // 2026-07-22T12:30:00Z = 14:30 at +02:00.
        let start = 1_784_723_400 * NS;
        let style = AxisLabelStyle::new(TimestampFormat::Iso8601WithOffset, Some(7_200));
        let ticks = calendar_ticks(
            start,
            start + 3 * 3_600 * NS,
            TimeUnit::Nanoseconds,
            &style,
            4,
        );
        assert_eq!(labels(&ticks), ["15:00", "16:00", "17:00"]);
        // 15:00 local is 13:00 UTC.
        assert_eq!(ticks[0].tick, start + 1_800 * NS);
        assert_eq!(ticks[0].context.as_deref(), Some("2026-07-22 · UTC+02:00"));
    }

    #[test]
    fn an_offset_moves_the_day_boundary_to_local_midnight() {
        // 2026-07-22T21:00Z → 23:00 at +02:00; midnight local is 22:00Z.
        let start = 1_784_754_000 * NS;
        let style = AxisLabelStyle::new(TimestampFormat::Iso8601WithOffset, Some(7_200));
        let ticks = calendar_ticks(
            start,
            start + 4 * 3_600 * NS,
            TimeUnit::Nanoseconds,
            &style,
            5,
        );
        assert_eq!(
            labels(&ticks),
            ["23:00", "00:00", "01:00", "02:00", "03:00"]
        );
        assert_eq!(ticks[1].context.as_deref(), Some("2026-07-23"));
    }

    #[test]
    fn dates_before_1970_work() {
        // 1969-12-31 22:00 → 1970-01-01 02:00 UTC.
        let start = -2 * 3_600 * NS;
        let ticks = calendar_ticks(
            start,
            start + 4 * 3_600 * NS,
            TimeUnit::Nanoseconds,
            &utc(),
            5,
        );
        assert_eq!(
            labels(&ticks),
            ["22:00", "23:00", "00:00", "01:00", "02:00"]
        );
        assert_eq!(ticks[0].context.as_deref(), Some("1969-12-31 · UTC"));
        assert_eq!(ticks[2].context.as_deref(), Some("1970-01-01"));
    }

    // ---- invariants, over every unit and a sweep of spans ---------------

    #[test]
    fn ticks_stay_inside_the_range_ordered_and_within_the_budget() {
        let units = [
            TimeUnit::Seconds,
            TimeUnit::Milliseconds,
            TimeUnit::Microseconds,
            TimeUnit::Nanoseconds,
            TimeUnit::Picoseconds,
        ];
        // From 1 tick up to ~300 years, starting off any round boundary.
        for unit in units {
            let per_second = unit.ticks_per_second();
            let start = 1_782_036_250 * per_second + 7;
            let mut span: i128 = 1;
            while span < 300 * 366 * 86_400 * per_second {
                for max_ticks in [1, 2, 5, 8, 10] {
                    let ticks = calendar_ticks(start, start + span, unit, &utc(), max_ticks);
                    assert!(
                        ticks.len() <= max_ticks,
                        "{unit:?} span {span}: {} > {max_ticks}",
                        ticks.len()
                    );
                    assert!(ticks
                        .iter()
                        .all(|t| (start..=start + span).contains(&t.tick)));
                    assert!(ticks.windows(2).all(|w| w[0].tick < w[1].tick));
                    assert!(ticks
                        .iter()
                        .all(|t| t.step_ticks > 0 && !t.label.is_empty()));
                    if let Some(first) = ticks.first() {
                        assert!(first.context.as_deref().is_some_and(|c| c.ends_with("UTC")));
                    }
                    // A wide enough budget always finds something to label.
                    if max_ticks >= 5 && span >= 10 {
                        assert!(ticks.len() >= 2, "{unit:?} span {span}: {ticks:?}");
                    }
                }
                span = span * 3 + 1;
            }
        }
    }

    #[test]
    fn nothing_to_label_is_empty_not_a_panic() {
        assert!(calendar_ticks(10, 20, TimeUnit::Seconds, &utc(), 0).is_empty());
        assert!(calendar_ticks(20, 10, TimeUnit::Seconds, &utc(), 5).is_empty());
        // Beyond chrono's calendar: no labels, no panic.
        let far = i128::MAX / 4;
        assert!(calendar_ticks(far, far + 1_000, TimeUnit::Seconds, &utc(), 5).is_empty());
        let _ = calendar_ticks(
            i128::MIN / 4,
            i128::MAX / 4,
            TimeUnit::Picoseconds,
            &utc(),
            5,
        );
    }

    // ---- cursor readout -------------------------------------------------

    fn readout(text: &str, format: TimestampFormat) -> String {
        let timestamp = parse_timestamp(text, format).expect("parses");
        format_cursor_timestamp(&timestamp, format)
    }

    #[test]
    fn a_numeric_timestamp_reads_as_a_date_with_its_raw_value_beside_it() {
        use TimestampFormat::*;
        assert_eq!(
            readout("1782036250587450", EpochMicros),
            "2026-06-21 10:04:10.587450 UTC (1782036250587450)"
        );
        assert_eq!(
            readout("1782036250", EpochSeconds),
            "2026-06-21 10:04:10 UTC (1782036250)"
        );
        assert_eq!(
            readout("1782036250.5", EpochSeconds),
            "2026-06-21 10:04:10.500 UTC (1782036250.5)"
        );
        assert_eq!(
            readout("1782036250587", EpochMillis),
            "2026-06-21 10:04:10.587 UTC (1782036250587)"
        );
        assert_eq!(
            readout("1782036250587450123", EpochNanos),
            "2026-06-21 10:04:10.587450123 UTC (1782036250587450123)"
        );
        // case-34's LabVIEW value and an Excel serial at noon (Excel is
        // naive: no zone is invented for it).
        assert_eq!(
            readout("3850027200", LabViewEpoch),
            "2025-12-31 12:00:00 UTC (3850027200)"
        );
        assert_eq!(
            readout("46023.5", ExcelSerial),
            "2026-01-01 12:00:00 (46023.5)"
        );
    }

    #[test]
    fn a_text_timestamp_reads_exactly_as_the_source_wrote_it() {
        use TimestampFormat::*;
        for (text, format) in [
            ("2026-07-22T14:30:00+02:00", Iso8601WithOffset),
            ("2026-07-22T14:30:00.250", Iso8601Naive),
            ("2026-07-22 14:30:00", DateTimeSpace),
            ("22/07/2026 14:30:00", DayFirst),
        ] {
            assert_eq!(readout(text, format), text);
        }
    }
}
