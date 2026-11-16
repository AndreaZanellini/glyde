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
    /// `offset_seconds`.
    pub fn for_format(format: TimestampFormat, offset_seconds: Option<i32>) -> Self {
        let _ = (format, offset_seconds);
        AxisZone::NaiveLocal
    }

    /// How the axis names the zone: `UTC`, `UTC+02:00`, `naive local time`.
    pub fn label(&self) -> String {
        String::new()
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
        let _ = (format, offset_seconds);
        Self {
            zone: AxisZone::NaiveLocal,
            date_order: DateOrder::Iso,
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
    let _ = (start, end, unit, style, max_ticks);
    Vec::new()
}

/// The cursor readout's timestamp (SPEC §4.1): a text format as the source
/// wrote it ([`format_timestamp`]); a numeric one as a date and time in its
/// zone, followed by the exact raw value, e.g.
/// `2026-06-21 10:04:10.587450 UTC (1782036250587450)`.
pub fn format_cursor_timestamp(timestamp: &Timestamp, format: TimestampFormat) -> String {
    format_timestamp(timestamp, format)
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
