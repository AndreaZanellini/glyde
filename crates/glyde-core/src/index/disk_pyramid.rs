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

//! The min/max pyramid of a column too large for memory, on disk (issue
//! #102).
//!
//! [`crate::dsp::decimation::build_pyramid`] returns an owned
//! `Vec<Vec<Bucket>>`: about 11 bytes of RAM per sample per column, which is
//! what SPEC §5's flat `min(25% RAM, 4 GB)` cap forbids on exactly the files
//! that spill. Here every level is a [`SpillVec`] — one memory-mapped spill
//! file per level, written once, deleted with its [`SpillSet`] — and is read
//! through [`BucketLevel`], so [`decimate_viewport_levels_skipping_level_0`]
//! decimates over it with the very code the golden tests lock through
//! `decimate_viewport`, minus level 0: a level file of its own, one disk page
//! more per column edge than the raw samples it summarises (issue #145).
//!
//! Building it never holds a level in memory: level 0 is streamed from the
//! column (bounded chunks, the same grouping `build_pyramid` uses), and each
//! level above is aggregated from the one below read back sequentially
//! through a buffered cursor, [`PYRAMID_FACTOR`] buckets at a time. A frame
//! then touches only the handful of buckets per pixel column the query
//! needs.
//!
//! [`decimate_viewport_levels_skipping_level_0`]: crate::dsp::decimation::decimate_viewport_levels_skipping_level_0

use bytemuck::{Pod, Zeroable};

use crate::dsp::decimation::{
    decimate_viewport_levels_skipping_level_0, finite_range_skipping_level_0, merge_buckets,
    Bucket, BucketLevel, Level0Stream, Pyramid, PYRAMID_FACTOR,
};
use crate::index::spill::{SpillSet, SpillVec, SpillVecWriter};
use crate::series::SampleSource;
use crate::time::TickSource;
use crate::Result;

/// A [`Bucket`] as stored on disk: no implicit padding, so it can be
/// memory-mapped and read back by value.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct PackedBucket {
    first_ts: i128,
    last_ts: i128,
    min: f64,
    max: f64,
    first_value: f64,
    last_value: f64,
    nan_count: u64,
    _reserved: u64,
}

// SAFETY: `repr(C)`, plain numeric fields only, and no padding: 16 + 16 + 8
// + 8 + 8 + 8 + 8 + 8 bytes at offsets 0, 16, 32, 40, 48, 56, 64, 72 — 80 in
// all, a multiple of the 16-byte alignment. Every bit pattern is a valid
// value (an f64 of any bits is a valid f64), all-zero included.
unsafe impl Zeroable for PackedBucket {}
unsafe impl Pod for PackedBucket {}
const _: () = assert!(std::mem::size_of::<PackedBucket>() == 80);

/// Bytes one bucket takes on disk, for sizing a pyramid before writing it.
pub const PACKED_BUCKET_BYTES: u64 = std::mem::size_of::<PackedBucket>() as u64;

impl From<Bucket> for PackedBucket {
    fn from(bucket: Bucket) -> Self {
        Self {
            first_ts: bucket.first_ts,
            last_ts: bucket.last_ts,
            min: bucket.min,
            max: bucket.max,
            first_value: bucket.first_value,
            last_value: bucket.last_value,
            nan_count: bucket.nan_count,
            _reserved: 0,
        }
    }
}

impl From<PackedBucket> for Bucket {
    fn from(packed: PackedBucket) -> Self {
        Bucket {
            min: packed.min,
            max: packed.max,
            first_ts: packed.first_ts,
            last_ts: packed.last_ts,
            first_value: packed.first_value,
            last_value: packed.last_value,
            nan_count: packed.nan_count,
        }
    }
}

/// One pyramid level on disk — or level 0, of which only the bucket count
/// is kept (issue #174): decimation over an on-disk pyramid reads a column
/// edge's raw samples instead (#145), and its finite range scans them under
/// a level-1 bucket that holds ±∞, so its buckets — one per 8 samples, most
/// of the pyramid's size — are never written.
#[derive(Clone, Debug)]
pub struct DiskLevel {
    buckets: Option<SpillVec<PackedBucket>>,
    count: usize,
}

impl DiskLevel {
    fn stored(buckets: SpillVec<PackedBucket>) -> Self {
        let count = buckets.len();
        Self {
            buckets: Some(buckets),
            count,
        }
    }

    fn unwritten(count: usize) -> Self {
        Self {
            buckets: None,
            count,
        }
    }

    /// Whether this level's buckets are on disk: every level but 0.
    pub fn is_stored(&self) -> bool {
        self.buckets.is_some()
    }
}

impl BucketLevel for DiskLevel {
    fn bucket_count(&self) -> usize {
        self.count
    }

    fn bucket(&self, index: usize) -> Bucket {
        self.buckets
            .as_ref()
            .expect(
                "level 0 of an on-disk pyramid is never read: its decimation and its finite \
                 range skip it (issue #174)",
            )
            .get(index)
            .expect("callers index below bucket_count()")
            .into()
    }
}

/// A whole pyramid on disk: `levels()[0]` groups every [`PYRAMID_FACTOR`]
/// samples — counted, not written (issue #174) — and each level above groups
/// [`PYRAMID_FACTOR`] buckets of the one below: exactly `build_pyramid`'s
/// levels. Cheap to clone; the files go
/// when the last clone does.
#[derive(Clone, Debug, Default)]
pub struct DiskPyramid {
    levels: Vec<DiskLevel>,
    /// The column's samples as `f64`, written in the same pass that built
    /// level 0, for a column whose own dtype is not `f64`
    /// ([`build_disk_pyramid_keeping_samples`], issue #178): what the view
    /// reads its exact edge samples from instead of an `f64` copy in RAM.
    /// `None` for a column already `f64`, whose own spill file serves.
    samples: Option<SpillVec<f64>>,
}

impl DiskPyramid {
    pub fn levels(&self) -> &[DiskLevel] {
        &self.levels
    }

    /// The column's samples as `f64`, when this pyramid keeps them
    /// ([`build_disk_pyramid_keeping_samples`]).
    pub fn samples(&self) -> Option<&[f64]> {
        self.samples.as_ref().map(SpillVec::as_slice)
    }
}

/// By content, bucket for bucket — never by which files hold it. The kept
/// samples are not compared: they are the column's own, not the pyramid's;
/// nor is level 0, of which only the count exists.
impl PartialEq for DiskPyramid {
    fn eq(&self, other: &Self) -> bool {
        self.levels.len() == other.levels.len()
            && self.levels.iter().zip(&other.levels).all(|(a, b)| {
                a.bucket_count() == b.bucket_count()
                    && a.is_stored() == b.is_stored()
                    && (!a.is_stored() || (0..a.bucket_count()).all(|i| a.bucket(i) == b.bucket(i)))
            })
    }
}

impl Pyramid for DiskPyramid {
    fn decimate(
        &self,
        samples: &[f64],
        timestamps: &[i128],
        range: (i128, i128),
        pixel_columns: usize,
    ) -> Vec<Bucket> {
        decimate_viewport_levels_skipping_level_0(
            &self.levels,
            samples,
            timestamps,
            range,
            pixel_columns,
        )
    }

    fn finite_range(&self, samples: &[f64]) -> Option<(f64, f64)> {
        finite_range_skipping_level_0(&self.levels, samples)
    }

    fn samples(&self) -> Option<&[f64]> {
        DiskPyramid::samples(self)
    }
}

/// A column's pyramid, wherever it lives: in memory for a file that fits the
/// RAM budget, on disk for one that spills (issue #102).
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnPyramid {
    Memory(Vec<Vec<Bucket>>),
    Disk(DiskPyramid),
}

impl Pyramid for ColumnPyramid {
    fn decimate(
        &self,
        samples: &[f64],
        timestamps: &[i128],
        range: (i128, i128),
        pixel_columns: usize,
    ) -> Vec<Bucket> {
        match self {
            ColumnPyramid::Memory(levels) => {
                levels.decimate(samples, timestamps, range, pixel_columns)
            }
            ColumnPyramid::Disk(pyramid) => {
                pyramid.decimate(samples, timestamps, range, pixel_columns)
            }
        }
    }

    fn finite_range(&self, samples: &[f64]) -> Option<(f64, f64)> {
        match self {
            ColumnPyramid::Memory(levels) => levels.finite_range(samples),
            ColumnPyramid::Disk(pyramid) => pyramid.finite_range(samples),
        }
    }

    fn samples(&self) -> Option<&[f64]> {
        match self {
            ColumnPyramid::Memory(_) => None,
            ColumnPyramid::Disk(pyramid) => pyramid.samples(),
        }
    }
}

/// Builds the pyramid of `samples` over `timestamps` (same length,
/// non-decreasing ticks) into `set`, level by level, in bounded memory.
pub fn build_disk_pyramid<S, T>(set: &SpillSet, samples: &S, timestamps: &T) -> Result<DiskPyramid>
where
    S: SampleSource + ?Sized,
    T: TickSource + ?Sized,
{
    build(set, samples, timestamps, false)
}

/// [`build_disk_pyramid`], also keeping the samples it reads as `f64`, in a
/// spill file of the same set ([`DiskPyramid::samples`]): for a spilled
/// column whose own dtype is not `f64`, whose view would otherwise need an
/// `f64` copy of the whole column in RAM to read its edges from (issue
/// #178). They are written in the pass that builds level 0, so the column is
/// read once either way.
pub fn build_disk_pyramid_keeping_samples<S, T>(
    set: &SpillSet,
    samples: &S,
    timestamps: &T,
) -> Result<DiskPyramid>
where
    S: SampleSource + ?Sized,
    T: TickSource + ?Sized,
{
    build(set, samples, timestamps, true)
}

fn build<S, T>(
    set: &SpillSet,
    samples: &S,
    timestamps: &T,
    keep_samples: bool,
) -> Result<DiskPyramid>
where
    S: SampleSource + ?Sized,
    T: TickSource + ?Sized,
{
    let sample_count = samples.sample_count();
    debug_assert_eq!(
        sample_count,
        timestamps.tick_count(),
        "samples and timestamps must be the same length"
    );
    if sample_count == 0 {
        return Ok(DiskPyramid::default());
    }
    let stem = unique_stem();

    // Level 0, streamed exactly like `build_pyramid_streaming` reads it, and
    // aggregated into level 1 as its buckets go past: level 0 itself is not
    // written (issue #174).
    let mut level_1 = LevelOne {
        set,
        name: format!("{stem}.l1"),
        level_0: 0,
        group: None,
        writer: None,
    };
    let mut kept = keep_samples
        .then(|| SpillVecWriter::<f64>::create(set, &format!("{stem}.samples")))
        .transpose()?;
    let mut stream = Level0Stream::default();
    let mut position = 0usize;
    timestamps.visit_tick_chunks(0..sample_count, &mut |tick_chunk| {
        let start = position;
        position += tick_chunk.len();
        let mut offset = 0usize;
        samples.visit_sample_chunks(start..position, &mut |sample_chunk| {
            if let Some(kept) = &mut kept {
                kept.extend_from_slice(sample_chunk)?;
            }
            let end = offset + sample_chunk.len();
            stream.push_chunk(sample_chunk, &tick_chunk[offset..end], &mut |bucket| {
                level_1.push(bucket)
            })?;
            offset = end;
            Ok(())
        })?;
        Ok(())
    })?;
    stream.finish(&mut |bucket| level_1.push(bucket))?;
    let (level_0, level_1) = level_1.finish()?;
    let mut levels = vec![DiskLevel::unwritten(level_0)];
    levels.extend(level_1.map(DiskLevel::stored));

    // Every level above, from the one below, while it can still be grouped
    // (`grow_levels`' own stopping rule).
    while let Some(below) = levels
        .last()
        .and_then(|level| level.buckets.as_ref())
        .filter(|below| below.len() >= PYRAMID_FACTOR)
    {
        let mut writer =
            SpillVecWriter::<PackedBucket>::create(set, &format!("{stem}.l{}", levels.len()))?;
        let mut cursor = below.cursor()?;
        let mut group: Option<(Bucket, usize)> = None;
        while let Some(packed) = cursor.next_value()? {
            let bucket = Bucket::from(packed);
            group = Some(match group {
                Some((merged, count)) => (merge_buckets(merged, bucket), count + 1),
                None => (bucket, 1),
            });
            if group.is_some_and(|(_, count)| count == PYRAMID_FACTOR) {
                writer.push(group.take().expect("checked").0.into())?;
            }
        }
        if let Some((merged, _)) = group {
            writer.push(merged.into())?;
        }
        levels.push(DiskLevel::stored(writer.finish()?));
    }
    Ok(DiskPyramid {
        levels,
        samples: kept.map(SpillVecWriter::finish).transpose()?,
    })
}

/// Level 1 of an on-disk pyramid, aggregated from level 0's buckets as they
/// stream past — exactly what aggregating a level-0 file would give, in the
/// same order — so level 0 is never written (issue #174). Level 1 exists when
/// level 0 has at least [`PYRAMID_FACTOR`] buckets (`grow_levels`' rule), so
/// its file is created with its first full group.
struct LevelOne<'a> {
    set: &'a SpillSet,
    name: String,
    /// Level-0 buckets seen.
    level_0: usize,
    group: Option<(Bucket, usize)>,
    writer: Option<SpillVecWriter<PackedBucket>>,
}

impl LevelOne<'_> {
    fn push(&mut self, bucket: Bucket) -> Result<()> {
        self.level_0 += 1;
        let (merged, count) = match self.group.take() {
            Some((merged, count)) => (merge_buckets(merged, bucket), count + 1),
            None => (bucket, 1),
        };
        if count < PYRAMID_FACTOR {
            self.group = Some((merged, count));
            return Ok(());
        }
        if self.writer.is_none() {
            self.writer = Some(SpillVecWriter::create(self.set, &self.name)?);
        }
        self.writer
            .as_mut()
            .expect("created above")
            .push(merged.into())
    }

    /// Level 0's bucket count, and level 1 if it exists.
    fn finish(self) -> Result<(usize, Option<SpillVec<PackedBucket>>)> {
        let Some(mut writer) = self.writer else {
            return Ok((self.level_0, None));
        };
        if let Some((merged, _)) = self.group {
            writer.push(merged.into())?;
        }
        Ok((self.level_0, Some(writer.finish()?)))
    }
}

/// Distinguishes the files of several pyramids written into one set.
fn unique_stem() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("pyr{}", NEXT.fetch_add(1, Ordering::Relaxed))
}
