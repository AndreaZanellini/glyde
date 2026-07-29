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
///
/// Every variant but [`SeriesValues::Spilled`] is heap-backed — the fast
/// path for a file whose typed columns fit the RAM budget. `Spilled` is the
/// same data in the same dtype, memory-mapped from the on-disk spill cache
/// instead (issue #75, SPEC §5.1): a storage choice, never a data change.
/// The two compare equal element for element, so no caller has to know
/// which one it holds.
#[derive(Debug, Clone)]
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
    /// Samples streamed to the on-disk spill cache during ingestion and
    /// memory-mapped back, for a file too large to materialize in budget
    /// (issue #75).
    Spilled(SpilledValues),
}

/// A [`SeriesValues`] column backed by the on-disk spill cache
/// (`crate::index::spill`), in the same dtype the in-memory path would have
/// inferred for it. Only the dtypes CSV ingestion actually produces are
/// representable here (`ingest::infer::infer_column`: bool, `i64`, `f64`,
/// string) — a Parquet reader's narrower integer widths (docs/ROADMAP.md
/// M7) will extend this when they land, rather than being upcast into one
/// of these today.
#[derive(Debug, Clone, PartialEq)]
pub enum SpilledValues {
    /// `0`/`1` per sample: `bool` has no guaranteed byte representation to
    /// map directly, so it spills as `u8` and is read back as `!= 0`.
    Bool(crate::index::spill::SpillVec<u8>),
    I64(crate::index::spill::SpillVec<i64>),
    F64(crate::index::spill::SpillVec<f64>),
    String(crate::index::spill::SpillStrings),
}

impl SpilledValues {
    /// The [`Dtype`] this column was inferred as — the same one the
    /// in-memory path would report for the same file.
    pub fn dtype(&self) -> Dtype {
        match self {
            SpilledValues::Bool(_) => Dtype::Bool,
            SpilledValues::I64(_) => Dtype::I64,
            SpilledValues::F64(_) => Dtype::F64,
            SpilledValues::String(_) => Dtype::String,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            SpilledValues::Bool(v) => v.len(),
            SpilledValues::I64(v) => v.len(),
            SpilledValues::F64(v) => v.len(),
            SpilledValues::String(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether this spilled column holds exactly the same samples as the
    /// heap-backed `other` — the cross-storage half of [`SeriesValues`]'s
    /// `PartialEq`.
    fn eq_in_memory(&self, other: &SeriesValues) -> bool {
        match (self, other) {
            (SpilledValues::Bool(a), SeriesValues::Bool(b)) => {
                a.len() == b.len() && a.as_slice().iter().zip(b).all(|(&a, &b)| (a != 0) == b)
            }
            (SpilledValues::I64(a), SeriesValues::I64(b)) => a.as_slice() == b.as_slice(),
            (SpilledValues::F64(a), SeriesValues::F64(b)) => a.as_slice() == b.as_slice(),
            (SpilledValues::String(a), SeriesValues::String(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a == b)
            }
            _ => false,
        }
    }
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

/// Compared by *value*, never by storage: a spilled column and the
/// heap-backed column it would have been are the same data (Golden Rule 1,
/// issue #75's "storage change, not a data change"), so they must compare
/// equal. Every same-storage pair compares exactly as the derived
/// implementation used to, element for element in the column's own dtype —
/// in particular an `i64` beyond `f64`'s exact range is compared as `i64`,
/// never through a promotion.
impl PartialEq for SeriesValues {
    fn eq(&self, other: &Self) -> bool {
        use SeriesValues as V;
        match (self, other) {
            (V::Bool(a), V::Bool(b)) => a == b,
            (V::I8(a), V::I8(b)) => a == b,
            (V::I16(a), V::I16(b)) => a == b,
            (V::I32(a), V::I32(b)) => a == b,
            (V::I64(a), V::I64(b)) => a == b,
            (V::U8(a), V::U8(b)) => a == b,
            (V::U16(a), V::U16(b)) => a == b,
            (V::U32(a), V::U32(b)) => a == b,
            (V::U64(a), V::U64(b)) => a == b,
            (V::F32(a), V::F32(b)) => a == b,
            (V::F64(a), V::F64(b)) => a == b,
            (V::String(a), V::String(b)) => a == b,
            (V::Spilled(a), V::Spilled(b)) => a == b,
            (V::Spilled(spilled), in_memory) | (in_memory, V::Spilled(spilled)) => {
                spilled.eq_in_memory(in_memory)
            }
            _ => false,
        }
    }
}

impl SeriesValues {
    /// The [`Dtype`] this variant represents.
    pub fn dtype(&self) -> Dtype {
        match self {
            SeriesValues::Spilled(v) => v.dtype(),
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
            SeriesValues::Spilled(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The samples as a borrowed `&[f64]` when they already *are* `f64` —
    /// heap-backed or memory-mapped alike — so a caller that only needs to
    /// scan them (`dsp::decimation::build_pyramid`, SPEC §1.3's NaN-run
    /// flagging) never has to allocate a whole second copy the way
    /// [`Self::to_f64_vec`] does. `None` for every other dtype, including a
    /// numeric one that would need a real conversion.
    pub fn as_f64_slice(&self) -> Option<&[f64]> {
        match self {
            SeriesValues::F64(v) => Some(v),
            SeriesValues::Spilled(SpilledValues::F64(v)) => Some(v.as_slice()),
            _ => None,
        }
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
            // A spilled column is already memory-mapped; materializing it as
            // an owned `Vec` is exactly the unbounded allocation the spill
            // exists to avoid, so a caller that only needs to *read* the
            // samples should reach for `as_f64_slice` instead. This arm stays
            // for the callers that genuinely need ownership.
            SeriesValues::Spilled(SpilledValues::F64(v)) => Some(v.as_slice().to_vec()),
            SeriesValues::Spilled(SpilledValues::I64(v)) => Some(
                v.as_slice()
                    .iter()
                    .map(|&n| {
                        warn_if_precision_loss(n.unsigned_abs() as u128, n);
                        n as f64
                    })
                    .collect(),
            ),
            SeriesValues::Bool(_) | SeriesValues::String(_) => None,
            SeriesValues::Spilled(SpilledValues::Bool(_) | SpilledValues::String(_)) => None,
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
            SeriesValues::Spilled(SpilledValues::Bool(v)) => all_equal(v.as_slice()),
            SeriesValues::Spilled(SpilledValues::I64(v)) => all_equal(v.as_slice()),
            SeriesValues::Spilled(SpilledValues::F64(v)) => {
                all_equal_bits(v.as_slice(), f64::to_bits)
            }
            SeriesValues::Spilled(SpilledValues::String(v)) => v
                .iter()
                .collect::<Vec<_>>()
                .windows(2)
                .all(|p| p[0] == p[1]),
        }
    }

    /// The `index`-th sample as `f64`, for the plotting/cursor-readout path
    /// that walks a series sample by sample rather than scanning it whole
    /// (SPEC §4.1). `None` for `bool`/`string` (they route to the state
    /// timeline, SPEC §4.3) and for an out-of-range index.
    pub fn f64_at(&self, index: usize) -> Option<f64> {
        match self {
            SeriesValues::I8(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::I16(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::I32(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::I64(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::U8(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::U16(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::U32(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::U64(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::F32(v) => v.get(index).map(|&n| n as f64),
            SeriesValues::F64(v) => v.get(index).copied(),
            SeriesValues::Spilled(SpilledValues::I64(v)) => v.get(index).map(|n| n as f64),
            SeriesValues::Spilled(SpilledValues::F64(v)) => v.get(index),
            SeriesValues::Bool(_) | SeriesValues::String(_) => None,
            SeriesValues::Spilled(SpilledValues::Bool(_) | SpilledValues::String(_)) => None,
        }
    }

    /// [`Self::f64_at`], but also emits the same [`warn_if_precision_loss`]
    /// log [`Self::to_f64_vec`] does for an `i64`/`u64` value beyond the
    /// exact range (SPEC §1.4) — for a caller that reads a series
    /// value-by-value but, unlike the per-frame cursor-readout/plotting
    /// path [`Self::f64_at`] exists for, only ever reads each index once
    /// (e.g. `ingest::dataset`'s incremental pyramid cache, issue #90).
    /// [`Self::f64_at`] deliberately does not carry this log itself: it is
    /// called once per rendered sample per frame, and warning there would
    /// mean re-logging the same value every frame it stays on screen.
    pub fn f64_at_checked(&self, index: usize) -> Option<f64> {
        match self {
            SeriesValues::I64(v) => v.get(index).map(|&n| {
                warn_if_precision_loss(n.unsigned_abs() as u128, n);
                n as f64
            }),
            SeriesValues::U64(v) => v.get(index).map(|&n| {
                warn_if_precision_loss(n as u128, n);
                n as f64
            }),
            SeriesValues::Spilled(SpilledValues::I64(v)) => v.get(index).map(|n| {
                warn_if_precision_loss(n.unsigned_abs() as u128, n);
                n as f64
            }),
            other => other.f64_at(index),
        }
    }

    /// The `index`-th sample rendered exactly as its own dtype would print
    /// it (SPEC §4.1's "exact raw value" cursor readout — never routed
    /// through `f64`, so an `i64` beyond ±2⁵³ reads back exactly). `None`
    /// for `bool`/`string` and for an out-of-range index.
    pub fn display_at(&self, index: usize) -> Option<String> {
        match self {
            SeriesValues::I8(v) => v.get(index).map(i8::to_string),
            SeriesValues::I16(v) => v.get(index).map(i16::to_string),
            SeriesValues::I32(v) => v.get(index).map(i32::to_string),
            SeriesValues::I64(v) => v.get(index).map(i64::to_string),
            SeriesValues::U8(v) => v.get(index).map(u8::to_string),
            SeriesValues::U16(v) => v.get(index).map(u16::to_string),
            SeriesValues::U32(v) => v.get(index).map(u32::to_string),
            SeriesValues::U64(v) => v.get(index).map(u64::to_string),
            SeriesValues::F32(v) => v.get(index).map(f32::to_string),
            SeriesValues::F64(v) => v.get(index).map(f64::to_string),
            SeriesValues::Spilled(SpilledValues::I64(v)) => v.get(index).map(|n| n.to_string()),
            SeriesValues::Spilled(SpilledValues::F64(v)) => v.get(index).map(|n| n.to_string()),
            SeriesValues::Bool(_) | SeriesValues::String(_) => None,
            SeriesValues::Spilled(SpilledValues::Bool(_) | SpilledValues::String(_)) => None,
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

    // issue #90's incremental pyramid cache reads a growing column one index
    // at a time via `f64_at_checked` rather than converting it whole via
    // `to_f64_vec` — the two must agree value-for-value, including past the
    // precision boundary, so a checkpoint's incremental cache is never a
    // different number from what a full `to_f64_vec` conversion would give.
    #[test]
    fn f64_at_checked_of_i64_beyond_the_exact_range_matches_to_f64_vec() {
        let huge = (1i64 << 53) + 1;
        let values = SeriesValues::I64(vec![huge, -huge]);
        let expected = values.to_f64_vec().expect("i64 must convert");

        for (i, &expected) in expected.iter().enumerate() {
            assert_eq!(values.f64_at_checked(i), Some(expected));
        }
    }

    #[test]
    fn f64_at_checked_of_u64_beyond_the_exact_range_matches_to_f64_vec() {
        let huge = (1u64 << 53) + 1;
        let values = SeriesValues::U64(vec![huge]);
        let expected = values.to_f64_vec().expect("u64 must convert");

        assert_eq!(values.f64_at_checked(0), Some(expected[0]));
    }

    #[test]
    fn f64_at_checked_agrees_with_f64_at_for_every_numeric_dtype_within_exact_range() {
        assert_eq!(
            SeriesValues::I32(vec![42]).f64_at_checked(0),
            SeriesValues::I32(vec![42]).f64_at(0)
        );
        assert_eq!(
            SeriesValues::F64(vec![1.5]).f64_at_checked(0),
            SeriesValues::F64(vec![1.5]).f64_at(0)
        );
        assert_eq!(SeriesValues::Bool(vec![true]).f64_at_checked(0), None);
    }

    #[test]
    fn f64_at_checked_is_none_past_the_end() {
        assert_eq!(SeriesValues::I64(vec![1]).f64_at_checked(5), None);
    }
}
