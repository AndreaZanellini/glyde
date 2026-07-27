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

//! How the SPEC §2.1–2.2 statistics in this module *read* a timestamp axis
//! (issue #85).
//!
//! Every statistic here — gap detection, sampling classification,
//! monotonicity — is a scan of consecutive Δt. A `&[i128]` is the obvious way
//! to hand ticks to a scan, and stays the right one for a small series, but it
//! forces the whole tick column to be addressable at once: for a spilled axis
//! (issue #75) that means making every page of the mapped column resident,
//! which is memory proportional to file size and so cannot fit SPEC §5's flat
//! `min(25% RAM, 4 GB)` cap on a large enough file.
//!
//! [`TickSource`] is the alternative: a *replayable* source that hands its
//! ticks to a visitor in bounded chunks. `&[i128]` implements it (one
//! borrow, no copy), and so does a spilled `Timestamps` axis
//! (`ingest::dataset`), which reads its ticks back through a fixed-size buffer.
//! Every scan in this module is written once, against this trait, and the
//! slice-taking public functions are thin wrappers over that one
//! implementation (docs/ARCHITECTURE.md Hard rule 4).
//!
//! "Replayable" is a real requirement, not a convenience: an *exact* median —
//! which is what SPEC §2.2 defines its gap threshold and its uniformity
//! tolerance against — cannot be computed in a single bounded pass, so
//! `gap.rs` scans the same source several times (see `select_kth_key`).

use std::ops::Range;

use crate::Result;

/// Ticks per chunk a [`TickSource`] hands its visitor: 1 MiB worth of `i128`.
/// Large enough that per-chunk overhead is negligible, small enough that a
/// scan's footprint is a flat number regardless of how many rows there are.
pub const TICK_CHUNK_LEN: usize = (1 << 20) / std::mem::size_of::<i128>();

/// A timestamp axis's `i128` ticks, readable in bounded chunks and readable
/// again as many times as a statistic needs (see the module docs).
///
/// Implementors must yield exactly the same ticks, in the same order, on every
/// scan — the bounded-memory order statistics in [`super::gap`] are only exact
/// because successive passes see the same values.
pub trait TickSource {
    /// How many ticks this source holds.
    fn tick_count(&self) -> usize;

    /// Hands `range`'s ticks to `visit` in contiguous, non-empty chunks of at
    /// most [`TICK_CHUNK_LEN`] elements, in row order. A `range` reaching past
    /// [`Self::tick_count`] is clamped to it.
    fn visit_tick_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[i128]) -> Result<()>,
    ) -> Result<()>;
}

/// The in-memory case: one borrow of the ticks that are already there, handed
/// over in [`TICK_CHUNK_LEN`] pieces so a chunk-boundary bug cannot hide on
/// the slice path and show up only on the spilled one.
impl TickSource for [i128] {
    fn tick_count(&self) -> usize {
        self.len()
    }

    fn visit_tick_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[i128]) -> Result<()>,
    ) -> Result<()> {
        let start = range.start.min(self.len());
        let end = range.end.min(self.len());
        if start >= end {
            return Ok(());
        }
        for chunk in self[start..end].chunks(TICK_CHUNK_LEN) {
            visit(chunk)?;
        }
        Ok(())
    }
}

/// Hands every consecutive Δt of `source`'s `range` to `visit`, in row order,
/// stitching the difference across each chunk boundary. The one place a Δt is
/// ever computed: `Δt` is the exact `i128` difference of two neighbouring
/// ticks, never a rounded or rescaled one (Golden Rule 1).
///
/// A `range` shorter than two ticks has no Δt at all and visits nothing.
pub(crate) fn for_each_delta<S: TickSource + ?Sized>(
    source: &S,
    range: Range<usize>,
    visit: &mut dyn FnMut(i128) -> Result<()>,
) -> Result<()> {
    let mut previous: Option<i128> = None;
    source.visit_tick_chunks(range, &mut |chunk| {
        for &tick in chunk {
            if let Some(previous) = previous.replace(tick) {
                visit(tick - previous)?;
            }
        }
        Ok(())
    })
}

/// How many Δt `for_each_delta` visits over `range` of a source holding
/// `tick_count` ticks.
pub(crate) fn delta_count(range: &Range<usize>, tick_count: usize) -> usize {
    let start = range.start.min(tick_count);
    let end = range.end.min(tick_count);
    end.saturating_sub(start).saturating_sub(1)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A [`TickSource`] that deliberately chunks its ticks as finely as
    /// possible, so every scan in this module is exercised across chunk
    /// boundaries by the same tests that cover the slice path. Stands in for a
    /// spilled axis (`ingest::dataset`'s `Timestamps`) without needing a file.
    pub(crate) struct ChunkedTicks {
        pub(crate) ticks: Vec<i128>,
        pub(crate) chunk_len: usize,
    }

    impl TickSource for ChunkedTicks {
        fn tick_count(&self) -> usize {
            self.ticks.len()
        }

        fn visit_tick_chunks(
            &self,
            range: Range<usize>,
            visit: &mut dyn FnMut(&[i128]) -> Result<()>,
        ) -> Result<()> {
            let start = range.start.min(self.ticks.len());
            let end = range.end.min(self.ticks.len());
            if start >= end {
                return Ok(());
            }
            for chunk in self.ticks[start..end].chunks(self.chunk_len.max(1)) {
                visit(chunk)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::ChunkedTicks;
    use super::*;

    fn deltas_of<S: TickSource + ?Sized>(source: &S, range: Range<usize>) -> Vec<i128> {
        let mut deltas = Vec::new();
        for_each_delta(source, range, &mut |delta| {
            deltas.push(delta);
            Ok(())
        })
        .expect("an in-memory tick source never fails");
        deltas
    }

    #[test]
    fn deltas_are_the_same_however_finely_the_ticks_are_chunked() {
        let ticks: Vec<i128> = vec![0, 10, 30, 25, 25, 1_000, 1_001];
        let expected = vec![10, 20, -5, 0, 975, 1];

        assert_eq!(deltas_of(ticks.as_slice(), 0..ticks.len()), expected);
        for chunk_len in 1..=ticks.len() + 1 {
            let chunked = ChunkedTicks {
                ticks: ticks.clone(),
                chunk_len,
            };
            assert_eq!(
                deltas_of(&chunked, 0..ticks.len()),
                expected,
                "chunking {chunk_len} ticks at a time must not change the Δt sequence"
            );
        }
    }

    #[test]
    fn a_sub_range_yields_only_that_ranges_own_deltas() {
        let ticks: Vec<i128> = vec![0, 1, 2, 100, 101, 102];

        assert_eq!(deltas_of(ticks.as_slice(), 3..6), vec![1, 1]);
        assert_eq!(delta_count(&(3..6), ticks.len()), 2);
    }

    #[test]
    fn ranges_with_fewer_than_two_ticks_have_no_deltas() {
        let ticks: Vec<i128> = vec![7, 8, 9];

        assert!(deltas_of(ticks.as_slice(), 1..2).is_empty());
        assert!(deltas_of(ticks.as_slice(), 2..2).is_empty());
        assert!(deltas_of(&[] as &[i128], 0..0).is_empty());
        assert_eq!(delta_count(&(1..2), ticks.len()), 0);
        assert_eq!(delta_count(&(0..0), ticks.len()), 0);
    }

    #[test]
    fn a_range_past_the_end_is_clamped_rather_than_panicking() {
        let ticks: Vec<i128> = vec![0, 5, 9];

        assert_eq!(deltas_of(ticks.as_slice(), 0..99), vec![5, 4]);
        assert_eq!(delta_count(&(0..99), ticks.len()), 2);
        assert!(deltas_of(ticks.as_slice(), 99..200).is_empty());
    }
}
