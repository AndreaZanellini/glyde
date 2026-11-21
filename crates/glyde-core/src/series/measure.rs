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

//! The value side of SPEC §4.1's measurement cursors (issue #231): each
//! numeric series' raw sample at cursor A and at cursor B, and the
//! difference `ΔY = B − A`.
//!
//! Every value is read from the raw sample itself (never from the min/max
//! pyramid) in its own dtype, the way the hover readout reads it. A `NaN`
//! at a cursor is missing: no difference is made up for it, and nothing is
//! interpolated from its neighbours.
//!
//! - **Integers** differ exactly, in `i128`: `9223372036854775807 −
//!   9223372036854775806` is `1`, where `f64` would say `0`.
//! - **Floats** differ in `f64`, rounded to the most decimals either
//!   sample is written with (its shortest round-trip text): `10.4 − 10.1`
//!   reads `0.3`, the exact difference of the two numbers shown, not
//!   `0.3000000000000007`, the binary residue of the subtraction. That
//!   residue is far below the last shown decimal for any magnitude an
//!   `f64` holds to that many decimals.
//! - An infinite value is shown as such, with no difference.

use super::{SeriesValues, SpilledValues};

/// One series' raw sample at a cursor.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    /// The sample, written exactly in its own dtype.
    Value(String),
    /// `NaN`: no value at this sample.
    Missing,
}

/// `B − A` for one series.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueDelta {
    /// The difference, for a rate (`ΔY/Δt`).
    pub value: f64,
    /// The difference as shown, signed: `+0.3`, `-1`, `0`.
    pub text: String,
}

/// One series between the two cursors.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorMeasurement {
    /// The sample at the cursor measured from (A).
    pub at_from: Reading,
    /// The sample at the cursor measured to (B).
    pub at_to: Reading,
    /// `B − A`; `None` when either sample is missing or infinite.
    pub delta: Option<ValueDelta>,
}

/// `values` at rows `from` (cursor A) and `to` (cursor B). `None` for a
/// series that is not numeric (`bool`, text) or a row past its end.
pub fn measure_between(values: &SeriesValues, from: usize, to: usize) -> Option<CursorMeasurement> {
    if let (Some(a), Some(b)) = (integer_at(values, from), integer_at(values, to)) {
        let difference = b - a;
        return Some(CursorMeasurement {
            at_from: Reading::Value(a.to_string()),
            at_to: Reading::Value(b.to_string()),
            delta: Some(ValueDelta {
                value: difference as f64,
                text: signed_integer(difference),
            }),
        });
    }
    let a = values.f64_at(from)?;
    let b = values.f64_at(to)?;
    let reading = |value: f64, index: usize| {
        if value.is_nan() {
            Reading::Missing
        } else {
            Reading::Value(
                values
                    .display_at(index)
                    .unwrap_or_else(|| value.to_string()),
            )
        }
    };
    let (at_from, at_to) = (reading(a, from), reading(b, to));
    let delta = match (&at_from, &at_to) {
        (Reading::Value(a_text), Reading::Value(b_text)) if a.is_finite() && b.is_finite() => {
            Some(float_delta(b - a, decimals(a_text).max(decimals(b_text))))
        }
        _ => None,
    };
    Some(CursorMeasurement {
        at_from,
        at_to,
        delta,
    })
}

/// An integer column's sample, exactly; `None` for any other dtype.
fn integer_at(values: &SeriesValues, index: usize) -> Option<i128> {
    match values {
        SeriesValues::I8(v) => v.get(index).map(|&n| n.into()),
        SeriesValues::I16(v) => v.get(index).map(|&n| n.into()),
        SeriesValues::I32(v) => v.get(index).map(|&n| n.into()),
        SeriesValues::I64(v) => v.get(index).map(|&n| n.into()),
        SeriesValues::U8(v) => v.get(index).map(|&n| n.into()),
        SeriesValues::U16(v) => v.get(index).map(|&n| n.into()),
        SeriesValues::U32(v) => v.get(index).map(|&n| n.into()),
        SeriesValues::U64(v) => v.get(index).map(|&n| n.into()),
        SeriesValues::Spilled(SpilledValues::I8(v)) => v.get(index).map(i128::from),
        SeriesValues::Spilled(SpilledValues::I16(v)) => v.get(index).map(i128::from),
        SeriesValues::Spilled(SpilledValues::I32(v)) => v.get(index).map(i128::from),
        SeriesValues::Spilled(SpilledValues::I64(v)) => v.get(index).map(i128::from),
        SeriesValues::Spilled(SpilledValues::U8(v)) => v.get(index).map(i128::from),
        SeriesValues::Spilled(SpilledValues::U16(v)) => v.get(index).map(i128::from),
        SeriesValues::Spilled(SpilledValues::U32(v)) => v.get(index).map(i128::from),
        SeriesValues::Spilled(SpilledValues::U64(v)) => v.get(index).map(i128::from),
        _ => None,
    }
}

fn signed_integer(value: i128) -> String {
    if value == 0 {
        "0".to_owned()
    } else {
        format!("{value:+}")
    }
}

/// How many decimals a sample's text is written with.
fn decimals(text: &str) -> usize {
    text.split_once('.')
        .map_or(0, |(_, fraction)| fraction.len())
}

/// A float difference rounded to `decimals`, signed.
fn float_delta(difference: f64, decimals: usize) -> ValueDelta {
    let text = format!("{difference:+.decimals$}");
    let value: f64 = text.parse().unwrap_or(difference);
    if value == 0.0 {
        return ValueDelta {
            value: 0.0,
            text: "0".to_owned(),
        };
    }
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    };
    ValueDelta { value, text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_bool_columns_are_not_measured() {
        assert!(measure_between(&SeriesValues::Bool(vec![true, false]), 0, 1).is_none());
        let text = SeriesValues::String(vec!["on".to_owned(), "off".to_owned()]);
        assert!(measure_between(&text, 0, 1).is_none());
    }

    #[test]
    fn an_infinite_sample_is_shown_with_no_difference() {
        let values = SeriesValues::F64(vec![1.0, f64::INFINITY]);
        let measured = measure_between(&values, 0, 1).expect("numeric");
        assert_eq!(measured.at_to, Reading::Value("inf".to_owned()));
        assert_eq!(measured.delta, None);
    }

    #[test]
    fn equal_samples_differ_by_an_unsigned_zero() {
        let values = SeriesValues::F64(vec![2.5, 2.5]);
        let delta = measure_between(&values, 0, 1)
            .and_then(|m| m.delta)
            .expect("numbers");
        assert_eq!(delta.text, "0");
        let values = SeriesValues::I32(vec![7, 7]);
        let delta = measure_between(&values, 1, 0)
            .and_then(|m| m.delta)
            .expect("numbers");
        assert_eq!(delta.text, "0");
    }

    // A column too large for memory is read from the spill cache in its own
    // dtype too: beyond 2⁵³ it still differs exactly.
    #[test]
    fn a_spilled_integer_column_differs_exactly() {
        use crate::index::spill::{SpillSet, SpillVecWriter};
        let dir = tempfile::tempdir().expect("temp dir");
        let set = SpillSet::create(dir.path()).expect("spill set");
        let mut writer = SpillVecWriter::<i64>::create(&set, "value").expect("writer");
        writer
            .extend_from_slice(&[9_223_372_036_854_775_806, 9_223_372_036_854_775_807])
            .expect("write");
        let values = SeriesValues::Spilled(SpilledValues::I64(writer.finish().expect("finish")));
        let measured = measure_between(&values, 0, 1).expect("numeric");
        assert_eq!(
            measured.at_to,
            Reading::Value("9223372036854775807".to_owned())
        );
        assert_eq!(measured.delta.expect("numbers").text, "+1");
    }

    #[test]
    fn an_f32_difference_is_that_of_the_shown_numbers() {
        // 0.3_f32 − 0.1_f32 in f64 is 0.20000000298023224.
        let values = SeriesValues::F32(vec![0.1, 0.3]);
        let delta = measure_between(&values, 0, 1)
            .and_then(|m| m.delta)
            .expect("numbers");
        assert_eq!(delta.text, "+0.2");
    }
}
