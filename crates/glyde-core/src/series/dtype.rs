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

//! The dtype model (docs/SPEC.md §1.4): every source dtype Glyde must
//! preserve end-to-end, and the storage that holds values in that native
//! representation rather than upcasting them.

/// Every source dtype `docs/SPEC.md` §1.4 requires Glyde to preserve, byte
/// for byte, from ingestion through rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dtype {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    /// Free text or a fixed set of repeating labels ("categorical"); SPEC
    /// §1.4 does not distinguish the two at the dtype level.
    String,
}

/// Which of the three `docs/SPEC.md` §4 visualizations a series' dtype
/// routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    /// A numeric line plot (SPEC §4.1).
    TimeDomain,
    /// SPEC §4.3: "`bool` and `string`/categorical series are never
    /// rendered as numeric line plots" — bands and tick glyphs instead.
    StateTimeline,
}

impl Dtype {
    /// SPEC §1.4's routing rule, in one place so no call site has to repeat
    /// the `Bool | String` match itself.
    pub fn view_kind(self) -> ViewKind {
        match self {
            Dtype::Bool | Dtype::String => ViewKind::StateTimeline,
            _ => ViewKind::TimeDomain,
        }
    }
}

/// A series' values, stored in their native dtype (Golden Rule 1: raw data
/// is never degraded, so there is one variant per [`Dtype`] and no shared
/// numeric buffer that would force an upcast).
#[derive(Debug, Clone, PartialEq)]
pub enum SeriesValues {
    Bool(Vec<bool>),
    I8(Vec<i8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    String(Vec<String>),
}

/// True when every element of `values` compares equal to its neighbor, or
/// there are fewer than two elements. Shared by every [`SeriesValues`]
/// variant's `is_constant` arm.
fn all_equal<T: PartialEq>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] == pair[1])
}

/// Same as [`all_equal`], but compares floats by bit pattern so two `NaN`
/// samples of identical representation count as equal instead of every
/// `NaN` making the series look non-constant.
fn all_equal_bits<T: Copy, B: PartialEq>(values: &[T], bits: impl Fn(T) -> B) -> bool {
    values.windows(2).all(|pair| bits(pair[0]) == bits(pair[1]))
}

/// The largest integer magnitude an `f64` represents exactly: 2^53, the
/// width of its mantissa plus the implicit leading bit (SPEC §1.4,
/// docs/ROADMAP.md M8).
const MAX_EXACT_F64_MAGNITUDE: u128 = 1u128 << 53;

/// Logs (at `warn`) when `magnitude` — an `i64`/`u64` value's absolute
/// value, widened to `u128` so both signed and unsigned callers share one
/// comparison with no risk of overflow — exceeds what `f64` can represent
/// exactly (see [`SeriesValues::to_f64_vec`]'s doc comment). `original` is
/// the source value, reported so the log line names the actual reading that
/// lost precision, not just the fact that some value did.
fn warn_if_precision_loss(magnitude: u128, original: impl std::fmt::Display) {
    if magnitude > MAX_EXACT_F64_MAGNITUDE {
        tracing::warn!(
            value = %original,
            "integer value exceeds f64's exact range (±2^53); converting to f64 \
             for plotting/pyramid building loses precision (SPEC §1.4, \
             docs/ROADMAP.md M8 owns surfacing this in the inference bar too)"
        );
    }
}

impl SeriesValues {
    /// The [`Dtype`] this variant represents.
    pub fn dtype(&self) -> Dtype {
        match self {
            SeriesValues::Bool(_) => Dtype::Bool,
            SeriesValues::I8(_) => Dtype::I8,
            SeriesValues::I16(_) => Dtype::I16,
            SeriesValues::I32(_) => Dtype::I32,
            SeriesValues::I64(_) => Dtype::I64,
            SeriesValues::U8(_) => Dtype::U8,
            SeriesValues::U16(_) => Dtype::U16,
            SeriesValues::U32(_) => Dtype::U32,
            SeriesValues::U64(_) => Dtype::U64,
            SeriesValues::F32(_) => Dtype::F32,
            SeriesValues::F64(_) => Dtype::F64,
            SeriesValues::String(_) => Dtype::String,
        }
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        match self {
            SeriesValues::Bool(v) => v.len(),
            SeriesValues::I8(v) => v.len(),
            SeriesValues::I16(v) => v.len(),
            SeriesValues::I32(v) => v.len(),
            SeriesValues::I64(v) => v.len(),
            SeriesValues::U8(v) => v.len(),
            SeriesValues::U16(v) => v.len(),
            SeriesValues::U32(v) => v.len(),
            SeriesValues::U64(v) => v.len(),
            SeriesValues::F32(v) => v.len(),
            SeriesValues::F64(v) => v.len(),
            SeriesValues::String(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every sample promoted to `f64` (`None` for `Bool`/`String` — the two
    /// dtypes that route to the state timeline, not a numeric plot or
    /// pyramid), for feeding [`crate::dsp::decimation::build_pyramid`]
    /// (docs/ROADMAP.md M3 "Background progressive build emitting partial
    /// levels") without every call site re-deriving the same dtype match.
    ///
    /// `i8`/`i16`/`i32`/`u8`/`u16`/`u32`/`f32` always convert losslessly —
    /// none of their ranges exceed `f64`'s 53-bit exact-integer mantissa. An
    /// `i64`/`u64` magnitude beyond [`MAX_EXACT_F64_MAGNITUDE`] does not
    /// convert losslessly; SPEC §1.4 requires that to be "flagged in the log
    /// and in the inference bar" — this only does the log half
    /// ([`docs/ROADMAP.md`] M8 owns the inference-bar surfacing this
    /// pyramid/plotting path shares with `glyde-app`'s existing
    /// `views::time::value_as_f64`, which has carried the same gap since
    /// M2). Never silently dropping the `warn` here, even though the UI half
    /// is still pending, is the "flag what was inferred, make it visible"
    /// half of Golden Rule 2 this module can do on its own.
    pub fn to_f64_vec(&self) -> Option<Vec<f64>> {
        match self {
            SeriesValues::I8(v) => Some(v.iter().map(|&n| n as f64).collect()),
            SeriesValues::I16(v) => Some(v.iter().map(|&n| n as f64).collect()),
            SeriesValues::I32(v) => Some(v.iter().map(|&n| n as f64).collect()),
            SeriesValues::I64(v) => Some(
                v.iter()
                    .map(|&n| {
                        warn_if_precision_loss(n.unsigned_abs() as u128, n);
                        n as f64
                    })
                    .collect(),
            ),
            SeriesValues::U8(v) => Some(v.iter().map(|&n| n as f64).collect()),
            SeriesValues::U16(v) => Some(v.iter().map(|&n| n as f64).collect()),
            SeriesValues::U32(v) => Some(v.iter().map(|&n| n as f64).collect()),
            SeriesValues::U64(v) => Some(
                v.iter()
                    .map(|&n| {
                        warn_if_precision_loss(n as u128, n);
                        n as f64
                    })
                    .collect(),
            ),
            SeriesValues::F32(v) => Some(v.iter().map(|&n| n as f64).collect()),
            SeriesValues::F64(v) => Some(v.clone()),
            SeriesValues::Bool(_) | SeriesValues::String(_) => None,
        }
    }

    /// SPEC §1.4: "Constant or single-sample series are valid inputs and
    /// must render." A single sample is vacuously constant (there is no
    /// neighbor for it to disagree with).
    pub fn is_constant(&self) -> bool {
        match self {
            SeriesValues::Bool(v) => all_equal(v),
            SeriesValues::I8(v) => all_equal(v),
            SeriesValues::I16(v) => all_equal(v),
            SeriesValues::I32(v) => all_equal(v),
            SeriesValues::I64(v) => all_equal(v),
            SeriesValues::U8(v) => all_equal(v),
            SeriesValues::U16(v) => all_equal(v),
            SeriesValues::U32(v) => all_equal(v),
            SeriesValues::U64(v) => all_equal(v),
            SeriesValues::F32(v) => all_equal_bits(v, f32::to_bits),
            SeriesValues::F64(v) => all_equal_bits(v, f64::to_bits),
            SeriesValues::String(v) => all_equal(v),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtype_view_kind_routes_bool_and_string_to_the_state_timeline() {
        assert_eq!(Dtype::Bool.view_kind(), ViewKind::StateTimeline);
        assert_eq!(Dtype::String.view_kind(), ViewKind::StateTimeline);
    }

    #[test]
    fn dtype_view_kind_routes_every_numeric_dtype_to_the_time_domain() {
        for dtype in [
            Dtype::I8,
            Dtype::I16,
            Dtype::I32,
            Dtype::I64,
            Dtype::U8,
            Dtype::U16,
            Dtype::U32,
            Dtype::U64,
            Dtype::F32,
            Dtype::F64,
        ] {
            assert_eq!(dtype.view_kind(), ViewKind::TimeDomain);
        }
    }

    #[test]
    fn series_values_reports_its_own_dtype_and_length() {
        let values = SeriesValues::F64(vec![1.0, 2.0, 3.0]);

        assert_eq!(values.dtype(), Dtype::F64);
        assert_eq!(values.len(), 3);
        assert!(!values.is_empty());
    }

    #[test]
    fn empty_series_values_is_empty() {
        assert!(SeriesValues::F64(Vec::new()).is_empty());
    }

    #[test]
    fn is_constant_is_true_for_a_repeated_value() {
        assert!(SeriesValues::F64(vec![42.0, 42.0, 42.0, 42.0]).is_constant());
    }

    #[test]
    fn is_constant_is_false_when_a_value_differs() {
        assert!(!SeriesValues::F64(vec![42.0, 42.0, 7.0, 42.0]).is_constant());
    }

    #[test]
    fn is_constant_is_vacuously_true_for_a_single_sample() {
        assert!(SeriesValues::I64(vec![5]).is_constant());
    }

    #[test]
    fn is_constant_is_vacuously_true_for_an_empty_series() {
        assert!(SeriesValues::I64(Vec::new()).is_constant());
    }

    #[test]
    fn is_constant_treats_identically_bit_patterned_nan_as_equal() {
        let nan = f64::NAN;
        assert!(SeriesValues::F64(vec![nan, nan, nan]).is_constant());
    }

    #[test]
    fn to_f64_vec_promotes_every_numeric_dtype_losslessly() {
        assert_eq!(
            SeriesValues::I64(vec![-1, 0, 42]).to_f64_vec(),
            Some(vec![-1.0, 0.0, 42.0])
        );
        assert_eq!(
            SeriesValues::U8(vec![0, 255]).to_f64_vec(),
            Some(vec![0.0, 255.0])
        );
        assert_eq!(
            SeriesValues::F32(vec![1.5]).to_f64_vec(),
            Some(vec![1.5_f32 as f64])
        );
        assert_eq!(
            SeriesValues::F64(vec![1.5, 2.5]).to_f64_vec(),
            Some(vec![1.5, 2.5])
        );
    }

    #[test]
    fn to_f64_vec_is_none_for_bool_and_string() {
        assert_eq!(SeriesValues::Bool(vec![true]).to_f64_vec(), None);
        assert_eq!(
            SeriesValues::String(vec!["on".to_string()]).to_f64_vec(),
            None
        );
    }

    // SPEC §1.4 / docs/ROADMAP.md M8: an i64/u64 magnitude beyond f64's
    // exact-integer range (2^53) must still convert (never panic) — the
    // value it produces is unchanged from before `warn_if_precision_loss`
    // existed, only a `warn` log is now emitted alongside it.
    #[test]
    fn to_f64_vec_of_i64_beyond_the_exact_range_still_converts_without_panicking() {
        let huge = (1i64 << 53) + 1;
        let result = SeriesValues::I64(vec![huge, -huge])
            .to_f64_vec()
            .expect("i64 must convert");
        assert_eq!(result, vec![huge as f64, -huge as f64]);
    }

    #[test]
    fn to_f64_vec_of_u64_beyond_the_exact_range_still_converts_without_panicking() {
        let huge = (1u64 << 53) + 1;
        let result = SeriesValues::U64(vec![huge])
            .to_f64_vec()
            .expect("u64 must convert");
        assert_eq!(result, vec![huge as f64]);
    }

    #[test]
    fn to_f64_vec_of_i64_at_or_within_the_exact_range_is_unaffected() {
        let boundary = 1i64 << 53;
        let result = SeriesValues::I64(vec![boundary, -boundary])
            .to_f64_vec()
            .expect("i64 must convert");
        assert_eq!(result, vec![boundary as f64, -boundary as f64]);
    }
}
