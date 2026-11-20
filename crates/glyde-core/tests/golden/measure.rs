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

//! Measurement-cursor golden tests (SPEC §4.1, issue #231): `Δt`, `1/Δt`,
//! the value at each cursor, `ΔY` and the slope `ΔY/Δt`, between two raw
//! samples of real corpus files. Every expected value below is computed by
//! hand from the file's text, written next to it. Never change an expected
//! value or a tolerance to make the implementation pass.

use glyde_core::ingest::{load, Dataset};
use glyde_core::series::{measure_between, Reading};
use glyde_core::time::{format_frequency, AxisSpan, TimeSpan, TimeUnit, Timestamp};
use std::path::{Path, PathBuf};

fn corpus(file_name: &str) -> Dataset {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testdata")
        .join("corpus")
        .join(file_name);
    load(&path).unwrap_or_else(|err| panic!("{file_name} must open: {err}"))
}

fn value_column(dataset: &Dataset) -> &glyde_core::series::SeriesValues {
    dataset
        .columns
        .iter()
        .find(|series| series.name() == "value")
        .expect("every case used here has a `value` column")
        .values()
}

fn time_span(dataset: &Dataset, from: usize, to: usize) -> TimeSpan {
    match dataset.time.span(from, to) {
        Some(AxisSpan::Time(span)) => span,
        other => panic!("an absolute axis measures time, got {other:?}"),
    }
}

/// Case 41: `…00.000000000000Z` (10.0) to `…00.000000000005Z` (10.5).
#[test]
fn picoseconds_apart_measure_exactly() {
    let dataset = corpus("case-41-picosecond-resolution-index.csv");
    let span = time_span(&dataset, 0, 5);
    // 5 − 0 = 5 ps, exact in integer ticks.
    assert_eq!(span.ticks(), 5);
    assert_eq!(span.unit(), TimeUnit::Picoseconds);
    assert_eq!(span.to_string(), "5 ps");
    assert_eq!(span.seconds(), 5e-12);
    // 1 / 5 ps = 2 × 10¹¹ Hz.
    assert_eq!(span.frequency_hz(), Some(2e11));
    assert_eq!(format_frequency(2e11), "200 GHz");

    let measured = measure_between(value_column(&dataset), 0, 5).expect("numeric column");
    assert_eq!(measured.at_from, Reading::Value("10".to_owned()));
    assert_eq!(measured.at_to, Reading::Value("10.5".to_owned()));
    let delta = measured.delta.expect("both samples are numbers");
    // 10.5 − 10.0 = 0.5.
    assert_eq!(delta.value, 0.5);
    assert_eq!(delta.text, "+0.5");
    // 0.5 / 5 ps = 10¹¹ per second.
    let slope = AxisSpan::Time(span).slope(delta.value).expect("Δt ≠ 0");
    assert!((slope - 1e11).abs() <= 1e11 * 1e-12, "slope {slope}");
}

/// Two neighbours 1 ps apart, the smallest step the axis has.
#[test]
fn neighbouring_picosecond_samples_are_one_picosecond_apart() {
    let dataset = corpus("case-41-picosecond-resolution-index.csv");
    for row in 0..5 {
        let span = time_span(&dataset, row, row + 1);
        assert_eq!((span.ticks(), span.unit()), (1, TimeUnit::Picoseconds));
        assert_eq!(span.to_string(), "1 ps");
    }
}

/// Case 32: epoch nanoseconds, `1770000000100000000` (10.1) to
/// `1770000000400000000` (10.4).
#[test]
fn epoch_nanoseconds_measure_exactly() {
    let dataset = corpus("case-32-epoch-nanoseconds.csv");
    let span = time_span(&dataset, 1, 4);
    // 1770000000400000000 − 1770000000100000000 = 300000000 ns = 300 ms.
    assert_eq!(span.ticks(), 300_000_000);
    assert_eq!(span.unit(), TimeUnit::Nanoseconds);
    assert_eq!(span.to_string(), "300 ms");
    assert_eq!(span.seconds(), 0.3);
    // 1 / 0.3 s = 3.333… Hz, to six significant digits.
    assert_eq!(
        span.frequency_hz().map(format_frequency).as_deref(),
        Some("3.33333 Hz")
    );

    let measured = measure_between(value_column(&dataset), 1, 4).expect("numeric column");
    assert_eq!(measured.at_from, Reading::Value("10.1".to_owned()));
    assert_eq!(measured.at_to, Reading::Value("10.4".to_owned()));
    let delta = measured.delta.expect("both samples are numbers");
    // 10.4 − 10.1 = 0.3 exactly in decimal; the readout never shows the
    // binary residue of `10.4_f64 − 10.1_f64` (0.3000000000000007).
    assert_eq!(delta.text, "+0.3");
    assert_eq!(delta.value, 0.3);
    // 0.3 / 0.3 s = 1 per second.
    let slope = AxisSpan::Time(span).slope(delta.value).expect("Δt ≠ 0");
    assert!((slope - 1.0).abs() <= 1e-12, "slope {slope}");
}

/// B before A: every difference is B − A, so it turns negative; the slope
/// does not change sign.
#[test]
fn a_cursor_b_left_of_a_gives_negative_differences() {
    let dataset = corpus("case-32-epoch-nanoseconds.csv");
    let span = time_span(&dataset, 4, 1);
    assert_eq!(span.ticks(), -300_000_000);
    assert_eq!(span.to_string(), "-300 ms");
    // A frequency is never negative: 1/|Δt|.
    assert_eq!(
        span.frequency_hz().map(format_frequency).as_deref(),
        Some("3.33333 Hz")
    );
    let delta = measure_between(value_column(&dataset), 4, 1)
        .and_then(|measured| measured.delta)
        .expect("both samples are numbers");
    assert_eq!(delta.text, "-0.3");
    let slope = AxisSpan::Time(span).slope(delta.value).expect("Δt ≠ 0");
    assert!((slope - 1.0).abs() <= 1e-12, "slope {slope}");
}

/// Case 29: epoch seconds, `1770000000` to `1770000005`.
#[test]
fn whole_seconds_read_in_seconds() {
    let dataset = corpus("case-29-epoch-seconds.csv");
    let span = time_span(&dataset, 0, 5);
    assert_eq!((span.ticks(), span.unit()), (5, TimeUnit::Seconds));
    assert_eq!(span.to_string(), "5 s");
    // 1 / 5 s = 0.2 Hz = 200 mHz.
    assert_eq!(
        span.frequency_hz().map(format_frequency).as_deref(),
        Some("200 mHz")
    );
}

/// Case 43: the value at `00:00:02` is `NaN`. It reads as missing, and no
/// difference or slope is made up from its neighbours.
#[test]
fn a_nan_at_a_cursor_is_missing_never_interpolated() {
    let dataset = corpus("case-43-nan-runs.csv");
    let measured = measure_between(value_column(&dataset), 1, 2).expect("numeric column");
    assert_eq!(measured.at_from, Reading::Value("10.1".to_owned()));
    assert_eq!(measured.at_to, Reading::Missing);
    assert_eq!(measured.delta, None);

    let measured = measure_between(value_column(&dataset), 3, 6).expect("numeric column");
    assert_eq!(measured.at_from, Reading::Missing);
    assert_eq!(measured.at_to, Reading::Value("10.6".to_owned()));
    assert_eq!(measured.delta, None);
}

/// Case 50: `9223372036854775806` to `9223372036854775807`. As `f64` both
/// are 2⁶³ and the difference would read 0; from the raw `i64` it is 1.
#[test]
fn integers_beyond_two_to_the_53_differ_exactly() {
    let dataset = corpus("case-50-i64-beyond-2-53.csv");
    let measured = measure_between(value_column(&dataset), 1, 2).expect("numeric column");
    assert_eq!(
        measured.at_from,
        Reading::Value("9223372036854775806".to_owned())
    );
    assert_eq!(
        measured.at_to,
        Reading::Value("9223372036854775807".to_owned())
    );
    let delta = measured.delta.expect("both samples are numbers");
    assert_eq!(delta.text, "+1");
    assert_eq!(delta.value, 1.0);

    // 9223372036854775807 − 9007199254740993 = 9214364837600034814.
    let delta = measure_between(value_column(&dataset), 2, 0)
        .and_then(|measured| measured.delta)
        .expect("both samples are numbers");
    assert_eq!(delta.text, "-9214364837600034814");
}

/// Case 37: rows 2 and 3 share `00:00:02`. Δt is zero, so there is no
/// `1/Δt` and no slope — never an infinity.
#[test]
fn a_zero_time_difference_has_no_frequency_and_no_slope() {
    let dataset = corpus("case-37-duplicate-timestamps.csv");
    let span = time_span(&dataset, 2, 3);
    assert_eq!(span.ticks(), 0);
    assert_eq!(span.to_string(), "0 s");
    assert_eq!(span.frequency_hz(), None);
    let delta = measure_between(value_column(&dataset), 2, 3)
        .and_then(|measured| measured.delta)
        .expect("both samples are numbers");
    // 10.3 − 10.2 = 0.1.
    assert_eq!(delta.text, "+0.1");
    assert_eq!(AxisSpan::Time(span).slope(delta.value), None);
}

/// Case 35: a progressive index (`0`…`5`) has no unit; the difference is
/// in the index's own numbers.
#[test]
fn a_progressive_index_measures_in_its_own_numbers() {
    let dataset = corpus("case-35-progressive-integer-index.csv");
    match dataset.time.span(1, 4) {
        Some(AxisSpan::Progressive(difference)) => assert_eq!(difference, 3.0),
        other => panic!("a progressive axis measures its own numbers, got {other:?}"),
    }
    let span = dataset.time.span(1, 4).expect("in range");
    // ΔY = 10.4 − 10.1 = 0.3 over Δx = 3: 0.1 per index step.
    let slope = span.slope(0.3).expect("Δx ≠ 0");
    assert!((slope - 0.1).abs() <= 1e-15, "slope {slope}");
}

/// Out of range on either side: nothing to measure.
#[test]
fn an_index_past_the_end_measures_nothing() {
    let dataset = corpus("case-32-epoch-nanoseconds.csv");
    assert_eq!(dataset.time.span(0, 6), None);
    assert!(measure_between(value_column(&dataset), 6, 0).is_none());
}

/// SPEC §2.1: a row promoted to a finer unit than its neighbour is compared
/// in the finer unit, exactly.
#[test]
fn timestamps_in_different_units_are_compared_in_the_finer_one() {
    // 1 s + 1 ps after the epoch, against 1 s (in nanoseconds).
    let from = Timestamp::new(1_000_000_000, TimeUnit::Nanoseconds);
    let to = Timestamp::new(1_000_000_000_001, TimeUnit::Picoseconds);
    let span = TimeSpan::between(&from, &to);
    assert_eq!((span.ticks(), span.unit()), (1, TimeUnit::Picoseconds));
    let span = TimeSpan::between(&to, &from);
    assert_eq!((span.ticks(), span.unit()), (-1, TimeUnit::Picoseconds));
}

/// The span is written exactly, in the largest unit it fills, with no
/// rounding and no trailing zeros.
#[test]
fn spans_are_written_exactly() {
    let ns = |ticks| {
        TimeSpan::between(
            &Timestamp::new(0, TimeUnit::Nanoseconds),
            &Timestamp::new(ticks, TimeUnit::Nanoseconds),
        )
        .to_string()
    };
    assert_eq!(ns(1), "1 ns");
    assert_eq!(ns(1_500), "1.5 µs");
    assert_eq!(ns(1_250_000_000), "1.25 s");
    assert_eq!(ns(1_000_000_001), "1.000000001 s");
    assert_eq!(ns(3_725_500_000_000), "3725.5 s");
    assert_eq!(ns(-999), "-999 ns");
    let ps = TimeSpan::between(
        &Timestamp::new(0, TimeUnit::Picoseconds),
        &Timestamp::new(123_456_789_012_345, TimeUnit::Picoseconds),
    );
    assert_eq!(ps.to_string(), "123.456789012345 s");
}

#[test]
fn frequencies_are_written_to_six_significant_digits() {
    assert_eq!(format_frequency(1.0), "1 Hz");
    assert_eq!(format_frequency(0.8), "800 mHz");
    assert_eq!(format_frequency(50_000.0), "50 kHz");
    assert_eq!(format_frequency(1.0 / 3600.0), "277.778 µHz");
    assert_eq!(format_frequency(1e12), "1 THz");
}
