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

//! How a column's samples are *read* when the column is too large to make
//! resident (issue #88) — the sample-side counterpart of
//! [`crate::time::TickSource`], and the exact same argument.
//!
//! [`SeriesValues::as_f64_slice`] hands out one contiguous `&[f64]` over a
//! whole column, which is the right thing for a column that already fits the
//! RAM budget and stays the input shape
//! [`crate::dsp::decimation::build_pyramid`]'s golden tests are written
//! against. For a *spilled* column (issue #75) that slice is a view over a
//! memory mapping, and walking it end to end makes every page of it resident —
//! 8 bytes per sample, proportional to file size, which cannot fit SPEC §5's
//! flat `min(25% RAM, 4 GB)` cap on a large enough file. That is the residency
//! issue #85 removed from the time axis and issue #88 flagged as still
//! reachable through the pyramid builder.
//!
//! [`SampleSource`] is the bounded alternative: a source that hands its
//! samples to a visitor in fixed-size chunks, promoted to `f64` on the way
//! past. `&[f64]` implements it (one borrow, no copy), and so does any
//! numeric [`SeriesValues`] via [`SeriesValues::sample_source`], which reads a
//! spilled column back through a fixed-size buffer
//! ([`crate::index::spill::SpillVec::read_chunks`]) instead of through its
//! mapping.
//!
//! Unlike [`crate::time::TickSource`], replayability is not required: the one
//! consumer, [`crate::dsp::decimation::build_pyramid_streaming`], is a single
//! forward pass. Every implementation here happens to be replayable anyway.

use std::ops::Range;

use super::dtype::warn_if_precision_loss;
use super::{SeriesValues, SpilledValues};
use crate::Result;

/// Samples per chunk a [`SampleSource`] hands its visitor: 1 MiB worth of
/// `f64`. Chosen to match [`crate::time::TICK_CHUNK_LEN`]'s reasoning — large
/// enough that per-chunk overhead is negligible, small enough that a scan's
/// footprint is a flat number regardless of how many rows there are.
pub const SAMPLE_CHUNK_LEN: usize = (1 << 20) / std::mem::size_of::<f64>();

/// A column's samples as `f64`, readable in bounded chunks (see the module
/// docs).
pub trait SampleSource {
    /// How many samples this source holds.
    fn sample_count(&self) -> usize;

    /// Hands `range`'s samples to `visit` in contiguous, non-empty chunks of
    /// at most [`SAMPLE_CHUNK_LEN`] elements, in row order. A `range` reaching
    /// past [`Self::sample_count`] is clamped to it.
    fn visit_sample_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[f64]) -> Result<()>,
    ) -> Result<()>;
}

/// The in-memory `f64` case: one borrow of the samples that are already
/// there, handed over in [`SAMPLE_CHUNK_LEN`] pieces so a chunk-boundary bug
/// cannot hide on the slice path and show up only on the spilled one.
impl SampleSource for [f64] {
    fn sample_count(&self) -> usize {
        self.len()
    }

    fn visit_sample_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[f64]) -> Result<()>,
    ) -> Result<()> {
        let Some(window) = clamp(range, self.len()) else {
            return Ok(());
        };
        for chunk in self[window].chunks(SAMPLE_CHUNK_LEN) {
            visit(chunk)?;
        }
        Ok(())
    }
}

/// A numeric [`SeriesValues`] read as a bounded-chunk [`SampleSource`],
/// whatever its dtype and whichever backing store it uses. Obtained from
/// [`SeriesValues::sample_source`].
///
/// Reading is the only thing this does: the samples themselves are never
/// modified, and the `f64` promotion is the same one
/// [`SeriesValues::to_f64_vec`] performs, warn-logged identically when an
/// `i64`/`u64` magnitude exceeds `f64`'s exact range (SPEC §1.4).
#[derive(Debug, Clone, Copy)]
pub struct SeriesSamples<'a> {
    values: &'a SeriesValues,
}

impl SampleSource for SeriesSamples<'_> {
    fn sample_count(&self) -> usize {
        self.values.len()
    }

    fn visit_sample_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[f64]) -> Result<()>,
    ) -> Result<()> {
        let Some(window) = clamp(range, self.values.len()) else {
            return Ok(());
        };

        match self.values {
            // Already `f64` and already on the heap: borrow, never copy.
            SeriesValues::F64(v) => {
                let window: &[f64] = &v[window];
                window.visit_sample_chunks(0..window.len(), visit)
            }
            // Already `f64` but memory-mapped: read it back through a
            // fixed-size buffer rather than through the mapping, so the pages
            // stay in the OS page cache (which the SPEC §5 cap does not count)
            // instead of in this process's RSS.
            SeriesValues::Spilled(SpilledValues::F64(v)) => v.read_chunks(window, visit),
            SeriesValues::Spilled(SpilledValues::I64(v)) => {
                let mut buffer = Vec::with_capacity(SAMPLE_CHUNK_LEN);
                v.read_chunks(window, &mut |chunk| {
                    buffer.clear();
                    buffer.extend(chunk.iter().map(|&n| {
                        warn_if_precision_loss(n.unsigned_abs() as u128, n);
                        n as f64
                    }));
                    visit(&buffer)
                })
            }
            // Every other numeric dtype needs a real conversion, so it is
            // promoted one bounded chunk at a time — never into a whole-column
            // `to_f64_vec` copy.
            other => {
                let mut buffer = Vec::with_capacity(SAMPLE_CHUNK_LEN);
                let mut start = window.start;
                while start < window.end {
                    let end = (start + SAMPLE_CHUNK_LEN).min(window.end);
                    buffer.clear();
                    buffer.extend((start..end).map(|index| {
                        other
                            .f64_at_checked(index)
                            .expect("sample_source rejects bool/string, and index is in range")
                    }));
                    visit(&buffer)?;
                    start = end;
                }
                Ok(())
            }
        }
    }
}

/// `range` clamped to `len`, or `None` when it selects nothing.
fn clamp(range: Range<usize>, len: usize) -> Option<Range<usize>> {
    let start = range.start.min(len);
    let end = range.end.min(len);
    (start < end).then_some(start..end)
}

impl SeriesValues {
    /// This column read as a bounded-chunk [`SampleSource`], or `None` for
    /// `bool`/`string` — the two dtypes that route to the state timeline
    /// rather than to a numeric plot or pyramid, exactly as
    /// [`Self::to_f64_vec`] reports them.
    pub fn sample_source(&self) -> Option<SeriesSamples<'_>> {
        match self {
            SeriesValues::Bool(_) | SeriesValues::String(_) => None,
            SeriesValues::Spilled(SpilledValues::Bool(_) | SpilledValues::String(_)) => None,
            values => Some(SeriesSamples { values }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_chunks<S: SampleSource + ?Sized>(source: &S, range: Range<usize>) -> Vec<Vec<f64>> {
        let mut chunks = Vec::new();
        source
            .visit_sample_chunks(range, &mut |chunk| {
                chunks.push(chunk.to_vec());
                Ok(())
            })
            .expect("an in-memory source never fails");
        chunks
    }

    fn flatten<S: SampleSource + ?Sized>(source: &S, range: Range<usize>) -> Vec<f64> {
        collect_chunks(source, range).concat()
    }

    #[test]
    fn every_numeric_dtype_reads_back_exactly_what_to_f64_vec_would_produce() {
        let columns = [
            SeriesValues::I8(vec![-1, 0, 42]),
            SeriesValues::I16(vec![-300, 7]),
            SeriesValues::I32(vec![-70_000, 12]),
            SeriesValues::I64(vec![-5, 9_007_199_254_740_993]),
            SeriesValues::U8(vec![0, 255]),
            SeriesValues::U16(vec![0, 65_535]),
            SeriesValues::U32(vec![7, 4_000_000_000]),
            SeriesValues::U64(vec![1, 18_000_000_000_000_000_000]),
            SeriesValues::F32(vec![1.5, -2.25]),
            SeriesValues::F64(vec![1.5, -2.25, 3.0]),
        ];

        for values in columns {
            let source = values
                .sample_source()
                .expect("every dtype here is numeric and has a sample source");
            assert_eq!(
                flatten(&source, 0..values.len()),
                values.to_f64_vec().expect("numeric dtype converts"),
                "{:?} must stream exactly the values to_f64_vec produces",
                values.dtype()
            );
        }
    }

    #[test]
    fn bool_and_string_columns_have_no_sample_source() {
        assert!(SeriesValues::Bool(vec![true, false])
            .sample_source()
            .is_none());
        assert!(SeriesValues::String(vec!["on".to_string()])
            .sample_source()
            .is_none());
    }

    #[test]
    fn chunks_are_contiguous_in_row_order_and_bounded_in_length() {
        // Two full chunks plus a short tail, so the boundary logic is
        // exercised without depending on the tail being the only short chunk.
        let count = SAMPLE_CHUNK_LEN * 2 + 17;
        let values = SeriesValues::I32((0..count as i32).collect());
        let source = values.sample_source().expect("i32 is numeric");

        let chunks = collect_chunks(&source, 0..count);
        assert_eq!(chunks.len(), 3);
        assert!(chunks
            .iter()
            .all(|chunk| !chunk.is_empty() && chunk.len() <= SAMPLE_CHUNK_LEN));
        assert_eq!(
            chunks.concat(),
            (0..count).map(|i| i as f64).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_sub_range_yields_only_that_ranges_own_samples() {
        let values = SeriesValues::F64(vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        let source = values.sample_source().expect("f64 is numeric");

        assert_eq!(flatten(&source, 1..4), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_range_past_the_end_is_clamped_rather_than_panicking() {
        let values = SeriesValues::U8(vec![1, 2, 3]);
        let source = values.sample_source().expect("u8 is numeric");

        assert_eq!(flatten(&source, 0..99), vec![1.0, 2.0, 3.0]);
        assert!(flatten(&source, 99..200).is_empty());
        assert!(flatten(&source, 2..2).is_empty());
    }

    #[test]
    fn an_empty_column_visits_nothing() {
        let values = SeriesValues::F64(Vec::new());
        let source = values.sample_source().expect("f64 is numeric");

        assert_eq!(source.sample_count(), 0);
        assert!(collect_chunks(&source, 0..0).is_empty());
    }

    #[test]
    fn a_plain_f64_slice_is_itself_a_sample_source() {
        let samples = [1.0, 2.0, 3.0, 4.0];

        assert_eq!(samples.as_slice().sample_count(), 4);
        assert_eq!(flatten(samples.as_slice(), 1..3), vec![2.0, 3.0]);
    }
}
