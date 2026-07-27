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

//! The one on-disk spill primitive every bounded column in `glyde-core` is
//! built from (issue #75, docs/SPEC.md §5.1 "the index ... if it would exceed
//! budget, is spilled to a cache file").
//!
//! [`SpillVecWriter`] appends one fixed-width element at a time to a file in
//! the cache directory — so a streaming reader never has to hold a whole
//! column in memory to build one — and [`SpillVecWriter::finish`] hands back
//! a [`SpillVec`], a memory-mapped view whose [`SpillVec::as_slice`] is a
//! real `&[T]` usable anywhere a heap `Vec<T>`'s slice would be (including
//! `dsp::decimation`'s golden-test-locked `&[f64]`/`&[i128]` signatures,
//! docs/ARCHITECTURE.md §The index).
//!
//! [`SpillStrings`] is the one variable-width case, built from two
//! [`SpillVec`]s — a byte arena plus a table of element end offsets — so
//! string/categorical columns (SPEC §1.4) spill with their source text
//! preserved byte for byte, not re-encoded (Golden Rule 1).
//!
//! **How this relates to `index::level0`.** Both write fixed-width typed
//! bytes behind a small header and map them back, but they answer different
//! questions and are deliberately kept separate rather than one being layered
//! on the other:
//!
//! - [`Level0CacheWriter`](super::level0::Level0CacheWriter) writes the
//!   `(timestamp, value)` *pair* `dsp::decimation` consumes, under a cache key
//!   (path + size + mtime) so a later open can recognize and reuse it
//!   (`try_open`, docs/ARCHITECTURE.md §The index). It is a **cache**.
//! - This module writes one file per column, in that column's own dtype
//!   width, as the ingestion path's **backing store** for a file too large to
//!   materialize in budget. It is written every open and read back only by the
//!   `Dataset` it belongs to; reusing it across opens is a separate item.
//!
//! Collapsing them would mean either giving the Level-0 cache a dtype it does
//! not need or giving every spilled column a duplicate timestamp file it does
//! not want, so they share the scheme rather than the code.
//!
//! **Disk cost and eviction.** Spill files are named from the caller's stem
//! and simply overwritten by a later open of the same source file; nothing
//! ever deletes them. Cache eviction is deferred and tracked separately, the
//! same deferral `index::level0` already records.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::marker::PhantomData;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytemuck::Pod;
use memmap2::Mmap;

use crate::{GlydeError, Result};

const MAGIC: &[u8; 8] = b"GLYDESP\0";
const FORMAT_VERSION: u32 = 1;

/// Fixed header size: an 8-byte magic, a 4-byte format version, and the
/// 4-byte element size the file was written with. 16 is itself a multiple of
/// `align_of::<i128>()` (16, the widest element any caller spills), so data
/// immediately following the header in a page-aligned `mmap` is correctly
/// aligned for [`bytemuck::cast_slice`] with no extra padding logic.
const HEADER_LEN: usize = 16;

/// File extension every spill file carries, so a stray file in the cache
/// directory is recognizably ours.
const EXTENSION: &str = "glysp";

/// A memory-mapped, fixed-width typed column spilled to disk.
///
/// Cloning is cheap (the mapping is shared through an [`Arc`]), so a
/// spilled column can be handed to another thread or snapshotted into a
/// message without copying the samples themselves.
pub struct SpillVec<T> {
    mmap: Arc<Mmap>,
    len: usize,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for SpillVec<T> {
    fn clone(&self) -> Self {
        Self {
            mmap: Arc::clone(&self.mmap),
            len: self.len,
            marker: PhantomData,
        }
    }
}

impl<T: Pod> SpillVec<T> {
    /// The spilled elements, in the order they were pushed.
    ///
    /// Reading this makes the corresponding pages resident; a caller that
    /// must stay under the peak-RSS cap should scan it once, sequentially,
    /// rather than holding derived copies of it.
    pub fn as_slice(&self) -> &[T] {
        let end = HEADER_LEN + self.len * size_of::<T>();
        // The mapping starts page-aligned and `HEADER_LEN` is a multiple of
        // 16 (>= `align_of::<T>()` for every `T` spilled here), and `end` is
        // within the file because `len` was computed from the file's own
        // length; both are `cast_slice`'s only preconditions.
        bytemuck::cast_slice(&self.mmap[HEADER_LEN..end])
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The element at `index`, or `None` when out of range.
    pub fn get(&self, index: usize) -> Option<T> {
        self.as_slice().get(index).copied()
    }
}

/// Prints the shape only, never the samples: a spilled column can hold
/// billions of elements and `Debug` is reached from log lines and test
/// output alike.
impl<T> std::fmt::Debug for SpillVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpillVec")
            .field("len", &self.len)
            .field("element_bytes", &size_of::<T>())
            .finish_non_exhaustive()
    }
}

impl<T: Pod + PartialEq> PartialEq for SpillVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

/// Appends fixed-width elements to a new spill file one at a time (SPEC
/// §5.1 "the full file is never loaded"): construct with
/// [`SpillVecWriter::create`], [`SpillVecWriter::push`] once per element in
/// order, then [`SpillVecWriter::finish`].
pub struct SpillVecWriter<T> {
    writer: BufWriter<File>,
    tmp_path: PathBuf,
    final_path: PathBuf,
    len: usize,
    marker: PhantomData<fn() -> T>,
}

/// Buffer every spill writer wraps its file in. Large enough that a
/// row-at-a-time `push` costs one `memcpy` rather than a syscall, small
/// enough that N columns' worth of them is still a flat, file-size-
/// independent amount of memory.
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

impl<T: Pod> SpillVecWriter<T> {
    /// Creates the backing file for `stem` under `cache_dir` (as a `.tmp`
    /// sibling, renamed into place atomically by [`Self::finish`] so a
    /// reader can never observe a half-written column) and writes its
    /// header.
    pub fn create(cache_dir: &Path, stem: &str) -> Result<Self> {
        std::fs::create_dir_all(cache_dir).map_err(|source| GlydeError::Io {
            path: cache_dir.to_path_buf(),
            source,
        })?;

        let final_path = cache_dir.join(format!("{stem}.{EXTENSION}"));
        let tmp_path = cache_dir.join(format!("{stem}.{EXTENSION}.tmp"));

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|source| GlydeError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);

        let mut header = [0u8; HEADER_LEN];
        header[0..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&FORMAT_VERSION.to_ne_bytes());
        header[12..16].copy_from_slice(&(size_of::<T>() as u32).to_ne_bytes());
        writer.write_all(&header).map_err(|source| GlydeError::Io {
            path: tmp_path.clone(),
            source,
        })?;

        Ok(Self {
            writer,
            tmp_path,
            final_path,
            len: 0,
            marker: PhantomData,
        })
    }

    /// Appends one element.
    pub fn push(&mut self, value: T) -> Result<()> {
        self.write_bytes(bytemuck::bytes_of(&value))?;
        self.len += 1;
        Ok(())
    }

    /// Appends every element of `values` — the bulk form of [`Self::push`],
    /// for callers such as [`SpillStringsWriter`] that already hold a run of
    /// elements contiguously.
    pub fn extend_from_slice(&mut self, values: &[T]) -> Result<()> {
        self.write_bytes(bytemuck::cast_slice(values))?;
        self.len += values.len();
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .map_err(|source| GlydeError::Io {
                path: self.tmp_path.clone(),
                source,
            })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Flushes the file, renames it into place, and memory-maps the result.
    pub fn finish(mut self) -> Result<SpillVec<T>> {
        self.writer
            .flush()
            .and_then(|()| self.writer.get_ref().sync_all())
            .map_err(|source| GlydeError::Io {
                path: self.tmp_path.clone(),
                source,
            })?;
        drop(self.writer);

        std::fs::rename(&self.tmp_path, &self.final_path).map_err(|source| GlydeError::Io {
            path: self.final_path.clone(),
            source,
        })?;

        let len = self.len;
        let mmap = map_spill_file(&self.final_path, len, size_of::<T>())?;
        Ok(SpillVec {
            mmap: Arc::new(mmap),
            len,
            marker: PhantomData,
        })
    }
}

/// Maps a finished spill file and checks it is exactly the file this writer
/// just wrote: right magic, right format version, right element width, and a
/// length matching the element count pushed. Any mismatch is
/// [`GlydeError::CorruptCache`] rather than a silent misread — unlike a
/// *reopen* (`index::level0::try_open`), where a mismatch is an ordinary
/// cache miss, a file we wrote moments ago failing validation means
/// something is genuinely wrong.
fn map_spill_file(path: &Path, expected_len: usize, element_size: usize) -> Result<Mmap> {
    let corrupt = |reason: &str| GlydeError::CorruptCache {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    };

    let file = File::open(path).map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // SAFETY: the spill file lives in glyde's own cache directory and is only
    // ever replaced through the atomic rename in `SpillVecWriter::finish`, so
    // no other process is expected to truncate or mutate it while mapped —
    // the same trust boundary `index::level0` and the source-file mapping in
    // `ingest::csv` already accept (docs/ARCHITECTURE.md §The index).
    let mmap = unsafe { Mmap::map(&file) }.map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    if mmap.len() < HEADER_LEN {
        return Err(corrupt("spill file shorter than its own header"));
    }
    if &mmap[0..8] != MAGIC {
        return Err(corrupt("spill file magic does not match"));
    }
    let version = u32::from_ne_bytes(mmap[8..12].try_into().expect("4-byte slice"));
    if version != FORMAT_VERSION {
        return Err(corrupt("spill file written by a different format version"));
    }
    let written_element_size =
        u32::from_ne_bytes(mmap[12..16].try_into().expect("4-byte slice")) as usize;
    if written_element_size != element_size {
        return Err(corrupt(
            "spill file element width does not match the reader's",
        ));
    }
    if mmap.len() - HEADER_LEN != expected_len * element_size {
        return Err(corrupt(
            "spill file length does not match the element count written",
        ));
    }

    Ok(mmap)
}

/// A spilled string/categorical column (SPEC §1.4): every field's source
/// bytes concatenated into one arena, plus the arena offset each field ends
/// at. [`SpillStrings::get`] hands back a borrowed `&str` into the mapped
/// arena, so reading a field costs no allocation.
#[derive(Debug, Clone)]
pub struct SpillStrings {
    arena: SpillVec<u8>,
    ends: SpillVec<u64>,
}

impl SpillStrings {
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The `index`-th field's text, or `None` when out of range (or, which
    /// should be unreachable for a file this crate wrote, when the arena
    /// bytes are not valid UTF-8).
    pub fn get(&self, index: usize) -> Option<&str> {
        let ends = self.ends.as_slice();
        let end = *ends.get(index)? as usize;
        let start = if index == 0 {
            0
        } else {
            ends[index - 1] as usize
        };
        let arena = self.arena.as_slice();
        std::str::from_utf8(arena.get(start..end)?).ok()
    }

    /// Every field, in row order.
    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        (0..self.len()).map(move |index| self.get(index).unwrap_or_default())
    }
}

impl PartialEq for SpillStrings {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().zip(other.iter()).all(|(a, b)| a == b)
    }
}

/// Appends string fields to a new [`SpillStrings`], one at a time.
pub struct SpillStringsWriter {
    arena: SpillVecWriter<u8>,
    ends: SpillVecWriter<u64>,
    arena_len: u64,
}

impl SpillStringsWriter {
    /// Creates the arena/offset file pair for `stem` under `cache_dir`.
    pub fn create(cache_dir: &Path, stem: &str) -> Result<Self> {
        Ok(Self {
            arena: SpillVecWriter::create(cache_dir, &format!("{stem}.arena"))?,
            ends: SpillVecWriter::create(cache_dir, &format!("{stem}.ends"))?,
            arena_len: 0,
        })
    }

    /// Appends one field's text, byte for byte.
    pub fn push(&mut self, field: &str) -> Result<()> {
        self.arena.extend_from_slice(field.as_bytes())?;
        self.arena_len += field.len() as u64;
        self.ends.push(self.arena_len)
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn finish(self) -> Result<SpillStrings> {
        Ok(SpillStrings {
            arena: self.arena.finish()?,
            ends: self.ends.finish()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_element_width_a_caller_spills() {
        let dir = tempfile::tempdir().expect("temp dir");

        let mut ticks = SpillVecWriter::<i128>::create(dir.path(), "ticks").expect("create");
        for value in [i128::MIN, -1, 0, 1, i128::MAX] {
            ticks.push(value).expect("push");
        }
        let ticks = ticks.finish().expect("finish");
        assert_eq!(ticks.len(), 5);
        assert_eq!(ticks.as_slice(), [i128::MIN, -1, 0, 1, i128::MAX]);

        let mut counts = SpillVecWriter::<i64>::create(dir.path(), "counts").expect("create");
        for value in [i64::MIN, 0, i64::MAX] {
            counts.push(value).expect("push");
        }
        assert_eq!(
            counts.finish().expect("finish").as_slice(),
            [i64::MIN, 0, i64::MAX]
        );

        let mut flags = SpillVecWriter::<u8>::create(dir.path(), "flags").expect("create");
        flags.extend_from_slice(&[1, 0, 1]).expect("extend");
        assert_eq!(flags.finish().expect("finish").as_slice(), [1, 0, 1]);
    }

    // Golden Rule 1: the spill round trip is a storage change, so a sample
    // must come back bit-identical — including NaN, whose payload `==` would
    // not compare at all.
    #[test]
    fn f64_samples_round_trip_bit_for_bit_including_nan() {
        let dir = tempfile::tempdir().expect("temp dir");
        let samples = [1.5_f64, -0.0, f64::NAN, f64::INFINITY, f64::MIN_POSITIVE];

        let mut writer = SpillVecWriter::<f64>::create(dir.path(), "samples").expect("create");
        for &sample in &samples {
            writer.push(sample).expect("push");
        }
        let spilled = writer.finish().expect("finish");

        assert_eq!(
            spilled
                .as_slice()
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            samples.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_empty_spill_vec_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        let writer = SpillVecWriter::<f64>::create(dir.path(), "empty").expect("create");
        let spilled = writer.finish().expect("finish");

        assert!(spilled.is_empty());
        assert!(spilled.as_slice().is_empty());
        assert_eq!(spilled.get(0), None);
    }

    #[test]
    fn strings_round_trip_including_empty_and_multibyte_fields() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fields = ["running", "", "idle", "°C µm/s²", "état"];

        let mut writer = SpillStringsWriter::create(dir.path(), "state").expect("create");
        for field in fields {
            writer.push(field).expect("push");
        }
        let spilled = writer.finish().expect("finish");

        assert_eq!(spilled.len(), fields.len());
        assert_eq!(spilled.iter().collect::<Vec<_>>(), fields);
        assert_eq!(spilled.get(fields.len()), None);
    }

    #[test]
    fn a_spill_vec_clone_shares_one_mapping_and_compares_equal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut writer = SpillVecWriter::<f64>::create(dir.path(), "shared").expect("create");
        writer.push(1.0).expect("push");
        writer.push(2.0).expect("push");
        let spilled = writer.finish().expect("finish");

        let clone = spilled.clone();
        assert_eq!(clone, spilled);
        assert_eq!(clone.as_slice().as_ptr(), spilled.as_slice().as_ptr());
    }

    // A finished spill file is validated against what was just written, so a
    // truncated or foreign file is a reported error rather than a slice of
    // whatever bytes happened to be there.
    #[test]
    fn a_truncated_spill_file_is_reported_not_misread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("truncated.glysp");
        std::fs::write(&path, b"not a spill file").expect("write");

        let err = map_spill_file(&path, 2, size_of::<f64>())
            .expect_err("a foreign file must not be misread as spilled samples");
        assert!(matches!(err, GlydeError::CorruptCache { .. }));
    }

    #[test]
    fn debug_reports_the_shape_without_printing_the_samples() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut writer = SpillVecWriter::<f64>::create(dir.path(), "debug").expect("create");
        writer.push(12345.678).expect("push");
        let spilled = writer.finish().expect("finish");

        let rendered = format!("{spilled:?}");
        assert!(rendered.contains("len: 1"), "unexpected: {rendered}");
        assert!(
            !rendered.contains("12345"),
            "Debug must never print a spilled column's samples: {rendered}"
        );
    }
}
