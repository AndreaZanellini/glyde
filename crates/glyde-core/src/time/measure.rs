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

//! The time side of SPEC §4.1's measurement cursors (issue #231): the exact
//! time between two samples, its reciprocal, and the rate of change of a
//! value over it.
//!
//! `Δt` is computed in integer ticks, never in `f64` seconds, so two
//! samples one picosecond apart in 2026 are exactly one picosecond apart
//! (an absolute `f64` second count resolves ~0.24 µs at that date, issue
//! #206). It is always the real time between the two samples, whatever axis
//! the plot draws them on (a compressed one included, SPEC §2.3): it comes
//! from the samples' own timestamps, not from where they are drawn.

use super::{TimeUnit, Timestamp};
use std::fmt;

/// The exact, signed time from one timestamp to another (`to − from`),
/// counted in the finer of their two units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSpan {
    ticks: i128,
    unit: TimeUnit,
}

/// Every unit, coarsest first: what [`TimeSpan`]'s text picks from.
const UNITS: [(TimeUnit, &str); 5] = [
    (TimeUnit::Seconds, "s"),
    (TimeUnit::Milliseconds, "ms"),
    (TimeUnit::Microseconds, "µs"),
    (TimeUnit::Nanoseconds, "ns"),
    (TimeUnit::Picoseconds, "ps"),
];

impl TimeSpan {
    /// `to − from`. A row promoted to a finer unit than its neighbour (SPEC
    /// §2.1, a sub-nanosecond fraction) is compared in the finer unit: units
    /// are powers of 1000 apart, so the coarser one scales exactly.
    pub fn between(from: &Timestamp, to: &Timestamp) -> Self {
        let (from_per_second, to_per_second) =
            (from.unit.ticks_per_second(), to.unit.ticks_per_second());
        let unit = if to_per_second >= from_per_second {
            to.unit
        } else {
            from.unit
        };
        let per_second = unit.ticks_per_second();
        let scaled =
            |timestamp: &Timestamp, own: i128| timestamp.ticks.saturating_mul(per_second / own);
        Self {
            ticks: scaled(to, to_per_second).saturating_sub(scaled(from, from_per_second)),
            unit,
        }
    }

    /// The span in [`Self::unit`]'s ticks, signed.
    pub fn ticks(self) -> i128 {
        self.ticks
    }

    /// The unit [`Self::ticks`] counts in.
    pub fn unit(self) -> TimeUnit {
        self.unit
    }

    /// The span in seconds, signed. Whole seconds and the remainder are
    /// converted separately, so a short span is correctly rounded however
    /// fine its unit.
    pub fn seconds(self) -> f64 {
        let per_second = self.unit.ticks_per_second();
        let whole = self.ticks / per_second;
        let rest = self.ticks % per_second;
        whole as f64 + rest as f64 / per_second as f64
    }

    /// `1/|Δt|` in hertz; `None` for a zero span, which has none.
    pub fn frequency_hz(self) -> Option<f64> {
        (self.ticks != 0)
            .then(|| self.unit.ticks_per_second() as f64 / self.ticks.unsigned_abs() as f64)
    }
}

/// Exactly, in the largest unit the span fills at least once (never finer
/// than its own), with no rounding and no trailing zeros: `5 ps`,
/// `1.5 µs`, `-300 ms`, `3725.5 s`. A zero span is `0 s`.
impl fmt::Display for TimeSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ticks == 0 {
            return f.write_str("0 s");
        }
        let own = self.unit.ticks_per_second();
        let magnitude = self.ticks.unsigned_abs();
        let (ratio, symbol) = UNITS
            .iter()
            .map(|&(unit, symbol)| ((own / unit.ticks_per_second()) as u128, symbol))
            .find(|&(ratio, _)| ratio >= 1 && magnitude >= ratio)
            .unwrap_or((1, "?"));
        let sign = if self.ticks < 0 { "-" } else { "" };
        let whole = magnitude / ratio;
        let rest = magnitude % ratio;
        if rest == 0 {
            return write!(f, "{sign}{whole} {symbol}");
        }
        let digits = ratio.ilog10() as usize;
        let fraction = format!("{rest:0digits$}");
        write!(
            f,
            "{sign}{whole}.{} {symbol}",
            fraction.trim_end_matches('0')
        )
    }
}

/// The span between two cursors on either kind of time axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AxisSpan {
    /// An absolute axis: real time.
    Time(TimeSpan),
    /// A progressive numeric index (SPEC §2.1): the difference of its own
    /// numbers, which carry no unit.
    Progressive(f64),
}

impl AxisSpan {
    /// The span as the divisor of a rate: seconds, or the index's numbers.
    pub fn as_f64(self) -> f64 {
        match self {
            AxisSpan::Time(span) => span.seconds(),
            AxisSpan::Progressive(difference) => difference,
        }
    }

    /// `delta / Δt`, per second on a time axis and per index step on a
    /// progressive one. `None` when the span is zero (or not finite): no
    /// infinity is ever reported as a slope.
    pub fn slope(self, delta: f64) -> Option<f64> {
        let span = self.as_f64();
        (span != 0.0 && span.is_finite() && delta.is_finite()).then(|| delta / span)
    }
}

/// Significant digits [`format_frequency`] and [`format_significant`] keep.
const SIGNIFICANT_DIGITS: usize = 6;

/// SI prefixes, largest first, for [`format_frequency`].
const PREFIXES: [(f64, &str); 9] = [
    (1e12, "T"),
    (1e9, "G"),
    (1e6, "M"),
    (1e3, "k"),
    (1.0, ""),
    (1e-3, "m"),
    (1e-6, "µ"),
    (1e-9, "n"),
    (1e-12, "p"),
];

/// A frequency in hertz to six significant digits, with an SI prefix:
/// `200 GHz`, `3.33333 Hz`, `277.778 µHz`.
pub fn format_frequency(hz: f64) -> String {
    if !hz.is_finite() || hz <= 0.0 {
        return format!("{} Hz", format_significant(hz));
    }
    // Rounded first, so 999999.7 Hz reads 1 MHz rather than 1000 kHz.
    let rounded: f64 = format!("{:.*e}", SIGNIFICANT_DIGITS - 1, hz)
        .parse()
        .unwrap_or(hz);
    match PREFIXES.iter().find(|&&(scale, _)| rounded >= scale) {
        Some(&(scale, prefix)) if rounded < 1e15 => {
            format!("{} {prefix}Hz", plain_significant(rounded / scale))
        }
        _ => format!("{} Hz", format_significant(hz)),
    }
}

/// `value` to six significant digits, without trailing zeros: plain
/// decimals from 10⁻⁴ up to 10⁶, scientific notation (`1e11`,
/// `-2.5e-7`) outside. What a computed rate (a slope) is shown as.
pub fn format_significant(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    let magnitude = value.abs();
    if (1e-4..1e6).contains(&magnitude) {
        let rounded: f64 = format!("{:.*e}", SIGNIFICANT_DIGITS - 1, value)
            .parse()
            .unwrap_or(value);
        if rounded.abs() < 1e6 {
            return plain_significant(rounded);
        }
    }
    let text = format!("{:.*e}", SIGNIFICANT_DIGITS - 1, value);
    match text.split_once('e') {
        Some((mantissa, exponent)) => {
            let mantissa = if mantissa.contains('.') {
                mantissa.trim_end_matches('0').trim_end_matches('.')
            } else {
                mantissa
            };
            format!("{mantissa}e{exponent}")
        }
        None => text,
    }
}

/// `value` (below 10⁶ in magnitude) in plain decimals, six significant
/// digits, trailing zeros trimmed.
fn plain_significant(value: f64) -> String {
    if value == 0.0 || !value.is_finite() {
        return format_significant(value);
    }
    // The power of ten of the first significant digit: 2 for 277.778,
    // -4 for 0.000123.
    let exponent = value.abs().log10().floor() as i32;
    let decimals = (SIGNIFICANT_DIGITS as i32 - 1 - exponent).max(0) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn significant_digits_switch_to_scientific_outside_the_plain_range() {
        assert_eq!(format_significant(1e11), "1e11");
        assert_eq!(format_significant(-2.5e-7), "-2.5e-7");
        assert_eq!(format_significant(1.0000000000000024), "1");
        assert_eq!(format_significant(-0.1), "-0.1");
        assert_eq!(format_significant(0.000123456789), "0.000123457");
        assert_eq!(format_significant(123456.7), "123457");
        assert_eq!(format_significant(999999.7), "1e6");
        assert_eq!(format_significant(0.0), "0");
    }

    #[test]
    fn a_rounded_frequency_moves_up_a_prefix() {
        assert_eq!(format_frequency(999_999.7), "1 MHz");
    }

    #[test]
    fn a_span_saturates_rather_than_overflowing() {
        let span = TimeSpan::between(
            &Timestamp::new(i128::MIN, TimeUnit::Seconds),
            &Timestamp::new(i128::MAX, TimeUnit::Picoseconds),
        );
        assert_eq!(span.ticks(), i128::MAX);
    }
}
