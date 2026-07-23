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

//! Time golden tests (docs/QUALITY.md §2 Time, docs/ROADMAP.md M1/M2).
//!
//! The single combined round-trip test M1 committed
//! (`every_timestamp_format_round_trips_through_parse_and_format`, looping
//! over all 11 `TimestampFormat` variants in one `#[ignore]`d `#[test]`) has
//! been split into one test per format here: docs/ROADMAP.md M2's "Time
//! index" item implements `Iso8601WithOffset`, `Iso8601Naive`, the four
//! epoch formats, and `DayFirst`/`MonthFirst` (`DateTimeSpace`,
//! `LabViewEpoch`, and `ExcelSerial` are separate, not-yet-started M2 roadmap
//! items) — a single shared test could not be un-ignored without either
//! implementing every format at once or editing the loop, and this file's own
//! rule (below) is to never do the latter. Splitting the test changes no
//! input, no expected output, and no assertion for any format; only the
//! in-scope formats' tests are un-ignored here. Un-ignore each remaining test
//! only once its implementation makes it pass — never loosen an assertion to
//! make that happen.

use glyde_core::time::{detect_gaps, format_timestamp, parse_timestamp, Gap, TimeUnit, Timestamp};

/// Asserts `parse_timestamp` then `format_timestamp` reproduces `input`
/// byte-for-byte (docs/QUALITY.md §2 Time: format round-trip) — the shared
/// body every per-format test below calls with its own fixture.
fn assert_round_trips(format: glyde_core::time::TimestampFormat, input: &str) {
    let parsed = parse_timestamp(input, format)
        .unwrap_or_else(|err| panic!("{input:?} under {format:?} must parse: {err}"));
    let formatted = format_timestamp(&parsed, format);
    assert_eq!(
        formatted, input,
        "{format:?}: formatting the timestamp parsed from {input:?} must reproduce the \
         identical string, not just an equivalent one"
    );
}

#[test]
fn iso8601_with_offset_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::Iso8601WithOffset,
        "2026-07-22T14:30:00+02:00",
    );
}

#[test]
fn iso8601_naive_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::Iso8601Naive,
        "2026-07-22T14:30:00",
    );
}

// Integer-valued examples only: float epoch strings have more than one valid
// textual spelling of the same value (`"10.5"` vs `"10.500000000"`), which
// would make the "identical string" oracle ambiguous rather than a property
// of the implementation.

#[test]
fn epoch_seconds_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::EpochSeconds,
        "1753193400",
    );
}

#[test]
fn epoch_millis_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::EpochMillis,
        "1753193400000",
    );
}

#[test]
fn epoch_micros_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::EpochMicros,
        "1753193400000000",
    );
}

#[test]
fn epoch_nanos_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::EpochNanos,
        "1753193400000000000",
    );
}

// The `DayFirst`/`MonthFirst` examples both carry a day field > 12, so a
// correct joint delimiter/date resolution (SPEC §2.1 ambiguity rule) never
// confuses them for one another, once that item is implemented.

#[test]
#[ignore = "docs/ROADMAP.md M2: DateTimeSpace timestamp parsing not implemented yet"]
fn date_time_space_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::DateTimeSpace,
        "2026-07-22 14:30:00.123",
    );
}

#[test]
fn day_first_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::DayFirst,
        "22/07/2026 14:30:00",
    );
}

#[test]
fn month_first_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::MonthFirst,
        "07/22/2026 14:30:00",
    );
}

#[test]
fn lab_view_epoch_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::LabViewEpoch,
        "3849213000",
    );
}

#[test]
fn excel_serial_round_trips() {
    assert_round_trips(glyde_core::time::TimestampFormat::ExcelSerial, "46590");
}

#[test]
fn iso8601_with_offset_picosecond_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::Iso8601WithOffset,
        "2026-07-22T14:30:00.123456789012+02:00",
    );
}

#[test]
fn iso8601_naive_picosecond_round_trips() {
    assert_round_trips(
        glyde_core::time::TimestampFormat::Iso8601Naive,
        "2026-07-22T14:30:00.123456789012",
    );
}

#[test]
fn nanosecond_precision_survives_a_multi_year_span_where_f64_seconds_would_not() {
    // 5 years plus a sub-second remainder, expressed in nanoseconds since the
    // Unix epoch. Chosen only to comfortably exceed 2^53 (9_007_199_254_740_992,
    // the largest integer an f64 mantissa holds exactly) with a low-order bit
    // pattern an f64 round-trip would corrupt — this is the test that forbids
    // storing absolute time as `f64` seconds (SPEC §2.1).
    const FIVE_YEARS_PLUS_REMAINDER_NANOS: i128 =
        5 * 365 * 24 * 60 * 60 * 1_000_000_000 + 123_456_789;

    // Compile-time proof the fixture is large enough to exercise f64's
    // precision limit — both operands are `const`, so this is checked once
    // at build time, not re-evaluated on every test run.
    const { assert!(FIVE_YEARS_PLUS_REMAINDER_NANOS > (1_i128 << 53)) };
    assert_ne!(
        (FIVE_YEARS_PLUS_REMAINDER_NANOS as f64) as i128,
        FIVE_YEARS_PLUS_REMAINDER_NANOS,
        "the fixture value must not be exactly representable as f64 — otherwise this test would \
         not distinguish an i128-ticks implementation from a forbidden f64-seconds one"
    );

    let input = FIVE_YEARS_PLUS_REMAINDER_NANOS.to_string();
    let parsed = parse_timestamp(&input, glyde_core::time::TimestampFormat::EpochNanos)
        .expect("an integer epoch-nanoseconds string must parse");

    assert_eq!(
        parsed,
        Timestamp::new(FIVE_YEARS_PLUS_REMAINDER_NANOS, TimeUnit::Nanoseconds),
        "parsing must preserve the exact nanosecond tick count over a multi-year span — any \
         detour through f64 seconds would corrupt the low-order digits asserted above"
    );
}

#[test]
fn picosecond_ticks_are_never_representable_exactly_as_f64_over_a_multi_year_span() {
    // A multi-year span expressed in picoseconds. This does not exercise the
    // parser (SPEC §2.1 names no textual "epoch picoseconds" format — a
    // picosecond-resolution index, corpus case 41, is a raw column value
    // paired with a declared `TimeUnit`, not a format string to parse). It
    // exercises the representation guarantee directly: `Timestamp` stores
    // `ticks` as `i128`, so this value is held exactly, whereas the `f64`
    // seconds representation SPEC §2.1 forbids would round it.
    const FIVE_YEARS_PLUS_REMAINDER_PICOS: i128 =
        5 * 365 * 24 * 60 * 60 * 1_000_000_000_000 + 123_456_789_012;

    // Compile-time proof the fixture is large enough to exercise f64's
    // precision limit — both operands are `const`, so this is checked once
    // at build time, not re-evaluated on every test run.
    const { assert!(FIVE_YEARS_PLUS_REMAINDER_PICOS > (1_i128 << 53)) };
    assert_ne!(
        (FIVE_YEARS_PLUS_REMAINDER_PICOS as f64) as i128,
        FIVE_YEARS_PLUS_REMAINDER_PICOS,
        "the fixture value must not be exactly representable as f64 — otherwise this test would \
         not distinguish an i128-ticks implementation from a forbidden f64-seconds one"
    );

    let timestamp = Timestamp::new(FIVE_YEARS_PLUS_REMAINDER_PICOS, TimeUnit::Picoseconds);

    assert_eq!(
        timestamp.ticks, FIVE_YEARS_PLUS_REMAINDER_PICOS,
        "Timestamp must hold the exact picosecond tick count — this is a property of the type \
         (i128, not f64), so it is provable without waiting for the parser/formatter to land"
    );
    assert_eq!(timestamp.unit, TimeUnit::Picoseconds);
}

#[test]
#[ignore = "docs/ROADMAP.md M2: gap detection not implemented yet"]
fn gap_detection_matches_hand_computed_gaps_on_a_synthetic_series() {
    const STEP: i128 = 1_000;
    const RUN_LEN: usize = 10;
    const GAP_1: i128 = 50_000; // > 10 * median (1_000) -> a gap
    const GAP_2: i128 = 20_000; // > 10 * median (1_000) -> a gap

    // Three uniform runs of 10 samples each (step 1_000), separated by two
    // deliberate gaps. 27 of the 29 consecutive deltas equal 1_000, so the
    // median delta is 1_000 and the threshold (SPEC §2.2: `Δt > 10 × median
    // Δt`) is 10_000 — comfortably below both inserted gaps and comfortably
    // above the uniform step, so there is no ambiguity about which deltas
    // must be flagged.
    let mut timestamps = Vec::new();
    let mut t = 0_i128;
    for _ in 0..RUN_LEN {
        timestamps.push(t);
        t += STEP;
    }
    t = *timestamps.last().unwrap() + GAP_1;
    for _ in 0..RUN_LEN {
        timestamps.push(t);
        t += STEP;
    }
    t = *timestamps.last().unwrap() + GAP_2;
    for _ in 0..RUN_LEN {
        timestamps.push(t);
        t += STEP;
    }
    assert_eq!(timestamps.len(), 3 * RUN_LEN);

    let gaps = detect_gaps(&timestamps);

    assert_eq!(
        gaps,
        vec![
            Gap {
                before_index: RUN_LEN - 1,
                after_index: RUN_LEN,
                delta: GAP_1,
            },
            Gap {
                before_index: 2 * RUN_LEN - 1,
                after_index: 2 * RUN_LEN,
                delta: GAP_2,
            },
        ],
        "detect_gaps must report exactly the two hand-inserted gaps, at their exact indices and \
         deltas, and nothing else from the otherwise-uniform runs"
    );
}
