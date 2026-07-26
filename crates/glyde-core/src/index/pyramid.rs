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

//! Pyramid-level spill cache (docs/ROADMAP.md M3 "Pyramid level spill",
//! docs/ARCHITECTURE.md §The index "Deferred, tracked separately").
//!
//! Only Level 0 was spilled to disk before this module
//! ([`crate::index::level0`]): reopening a large file rebuilt
//! [`crate::dsp::decimation::build_pyramid`]'s `Vec<Vec<Bucket>>` from the
//! cached Level 0 on every open — a full aggregation pass over the whole
//! dataset, not an instant reopen. This module closes that gap: a pyramid is
//! written once to a cache file, keyed exactly like Level 0
//! ([`super::CacheKey`] — source path + size + mtime), and a reopen reads the
//! bucket data straight back instead of recomputing it.
//!
//! Unlike [`crate::index::level0::Level0Cache`], this cache is not
//! memory-mapped: [`dsp::decimation`](crate::dsp::decimation)'s
//! golden-tested query API takes an owned `&[Vec<Bucket>]`, never a borrowed
//! cache view, so a hit here materializes an owned `Vec<Vec<Bucket>>` —
//! exactly what [`crate::dsp::decimation::build_pyramid`] would have produced
//! — rather than a zero-copy mmap slice. The win is skipping the O(n)
//! aggregation work (and the need to have Level 0 open at all to recompute
//! it), not zero-copy; the locked decimation API is untouched.
//!
//! **Disk cost.** Each bucket record is a fixed 56 bytes; the total bucket
//! count across all levels sums to roughly `n / (PYRAMID_FACTOR - 1)`, i.e.
//! about 8 bytes per raw sample on top of Level 0's 24 bytes/sample.
//! **Cache eviction is not implemented**, same as Level 0 (the cache
//! directory only ever grows).
//!
//! **Byte order.** Every multi-byte field is written native-endian
//! (`to_ne_bytes`/`from_ne_bytes`), the same choice [`crate::index::level0`]
//! already makes: this cache never leaves the machine that wrote it, so
//! there is no cross-platform-portability requirement to justify a fixed
//! endianness.
//!
//! **Trust boundary.** Unlike [`Level0Cache`](crate::index::level0::Level0Cache),
//! which is mmap-backed and so gets every offset bounds-checked against the
//! real file length for free, this reader parses `level_count` and each
//! level's `bucket_count` as plain integers from the file. Both are
//! validated against the file's actual size *before* being trusted to size
//! a `Vec::with_capacity` — a corrupted or hand-edited cache claiming an
//! enormous count must be rejected as a cache miss, never trusted into an
//! unbounded allocation.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use super::CacheKey;
use crate::dsp::decimation::{build_pyramid, Bucket};
use crate::{GlydeError, Result};

const MAGIC: &[u8; 8] = b"GLYDEPYR";
const FORMAT_VERSION: u32 = 1;
/// Fixed header size: an 8-byte magic, a 4-byte format version, a 4-byte
/// level count.
const HEADER_LEN: usize = 16;
/// One [`Bucket`] on disk: `min` (f64, 8) + `max` (f64, 8) + `first_ts`
/// (i128, 16) + `last_ts` (i128, 16) + `nan_count` (u64, 8), written as raw
/// field bytes with no padding — this is a hand-packed record, not a cast
/// over [`Bucket`]'s in-memory (padded) layout.
const BUCKET_RECORD_LEN: usize = 56;

fn cache_paths(cache_dir: &Path, key: &CacheKey) -> (PathBuf, PathBuf) {
    let stem = key.cache_stem();
    (
        cache_dir.join(format!("{stem}.pyramid.glypy")),
        cache_dir.join(format!("{stem}.pyramid.glypy.tmp")),
    )
}

/// Opens the existing cache for `key` if one is present and valid; builds
/// (and caches) a new pyramid from `samples`/`timestamps` otherwise. This is
/// the pyramid-level analogue of [`crate::index::level0::build_or_open`]: a
/// cache hit never touches `samples`/`timestamps`, and never re-runs
/// [`build_pyramid`]'s aggregation.
pub fn build_or_open(
    cache_dir: &Path,
    key: &CacheKey,
    samples: &[f64],
    timestamps: &[i128],
) -> Result<Vec<Vec<Bucket>>> {
    if let Some(pyramid) = try_open(cache_dir, key)? {
        return Ok(pyramid);
    }
    build(cache_dir, key, samples, timestamps)
}

/// Builds a new pyramid from `samples`/`timestamps` via [`build_pyramid`],
/// writes it to the cache, and returns it.
pub fn build(
    cache_dir: &Path,
    key: &CacheKey,
    samples: &[f64],
    timestamps: &[i128],
) -> Result<Vec<Vec<Bucket>>> {
    let pyramid = build_pyramid(samples, timestamps);
    write_cache(cache_dir, key, &pyramid)?;
    Ok(pyramid)
}

/// Opens the existing pyramid cache for `key`, if the file is present and
/// passes validation. Returns `Ok(None)` — never an error — for an ordinary
/// cache miss (file absent) or a corrupt/foreign cache (header mismatch or a
/// truncated read): both are `warn`-logged and treated as "must rebuild",
/// matching [`crate::index::level0::try_open`]'s error philosophy
/// (docs/ARCHITECTURE.md §Error philosophy: absorbed anomalies are logged,
/// never silently ignored, but never escalated to a hard failure over what is
/// only a cache).
pub fn try_open(cache_dir: &Path, key: &CacheKey) -> Result<Option<Vec<Vec<Bucket>>>> {
    let (final_path, _tmp_path) = cache_paths(cache_dir, key);

    let file = match File::open(&final_path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(GlydeError::Io {
                path: final_path,
                source,
            })
        }
    };
    let file_len = file
        .metadata()
        .map_err(|source| GlydeError::Io {
            path: final_path.clone(),
            source,
        })?
        .len();
    let mut reader = BufReader::new(file);

    let mut header = [0u8; HEADER_LEN];
    if reader.read_exact(&mut header).is_err() {
        tracing::warn!(path = %final_path.display(), "pyramid cache header truncated, rebuilding");
        return Ok(None);
    }
    if header[0..8] != *MAGIC {
        tracing::warn!(path = %final_path.display(), "pyramid cache magic mismatch, rebuilding");
        return Ok(None);
    }
    let version = u32::from_ne_bytes(header[8..12].try_into().expect("4-byte slice"));
    if version != FORMAT_VERSION {
        tracing::warn!(
            path = %final_path.display(),
            version,
            "pyramid cache format version mismatch, rebuilding"
        );
        return Ok(None);
    }
    let level_count = u32::from_ne_bytes(header[12..16].try_into().expect("4-byte slice"));

    // Bound `level_count` against the file's real, on-disk size before
    // trusting it to size an allocation: a corrupted/hand-edited file can
    // claim any u32 here, and Level 0's mmap-backed cache gets this check for
    // free from the OS while this sequential reader has to do it explicitly.
    // Each level-table entry is 8 bytes, so the table alone can never
    // legitimately exceed the bytes actually on disk.
    let level_table_len = u64::from(level_count) * 8;
    let Some(after_level_table) = (HEADER_LEN as u64).checked_add(level_table_len) else {
        tracing::warn!(path = %final_path.display(), level_count, "pyramid cache level count overflows, rebuilding");
        return Ok(None);
    };
    if after_level_table > file_len {
        tracing::warn!(
            path = %final_path.display(),
            level_count,
            file_len,
            "pyramid cache level count exceeds the file's actual size, rebuilding"
        );
        return Ok(None);
    }

    let mut level_lengths = Vec::with_capacity(level_count as usize);
    for _ in 0..level_count {
        let mut buf = [0u8; 8];
        if reader.read_exact(&mut buf).is_err() {
            tracing::warn!(path = %final_path.display(), "pyramid cache level table truncated, rebuilding");
            return Ok(None);
        }
        level_lengths.push(u64::from_ne_bytes(buf));
    }

    // Likewise bound the bucket data every level's declared `bucket_count`
    // implies against the bytes actually remaining, before trusting any
    // individual count to size a `Vec::with_capacity` — this is the same
    // "never let an untrusted length field drive an allocation" rule applied
    // to the bucket data as was just applied to the level table above.
    let mut total_bucket_bytes: u64 = 0;
    for &bucket_count in &level_lengths {
        let record_bytes = bucket_count.checked_mul(BUCKET_RECORD_LEN as u64);
        let running_total = record_bytes.and_then(|b| total_bucket_bytes.checked_add(b));
        let Some(running_total) = running_total else {
            tracing::warn!(path = %final_path.display(), bucket_count, "pyramid cache bucket count overflows, rebuilding");
            return Ok(None);
        };
        total_bucket_bytes = running_total;
    }
    let Some(expected_file_len) = after_level_table.checked_add(total_bucket_bytes) else {
        tracing::warn!(path = %final_path.display(), "pyramid cache bucket count overflows, rebuilding");
        return Ok(None);
    };
    if expected_file_len != file_len {
        tracing::warn!(
            path = %final_path.display(),
            expected_file_len,
            file_len,
            "pyramid cache declared bucket counts do not match the file's actual size, rebuilding"
        );
        return Ok(None);
    }

    let mut levels = Vec::with_capacity(level_count as usize);
    for bucket_count in level_lengths {
        // Safe to preallocate now: `bucket_count` was validated above against
        // the file's real, on-disk length, exactly like `level_count` was.
        let mut level = Vec::with_capacity(bucket_count as usize);
        for _ in 0..bucket_count {
            let mut record = [0u8; BUCKET_RECORD_LEN];
            if reader.read_exact(&mut record).is_err() {
                tracing::warn!(path = %final_path.display(), "pyramid cache bucket data truncated, rebuilding");
                return Ok(None);
            }
            level.push(decode_bucket(&record));
        }
        levels.push(level);
    }

    tracing::info!(
        path = %final_path.display(),
        level_count = levels.len(),
        "pyramid cache hit: reopening without rebuilding"
    );
    Ok(Some(levels))
}

/// Writes `pyramid` to `key`'s cache file: a `.tmp` sibling written and
/// `fsync`'d, then renamed into place atomically, so a reader never observes
/// a half-written cache (same pattern as
/// [`crate::index::level0::Level0CacheWriter::finish`]).
fn write_cache(cache_dir: &Path, key: &CacheKey, pyramid: &[Vec<Bucket>]) -> Result<()> {
    std::fs::create_dir_all(cache_dir).map_err(|source| GlydeError::Io {
        path: cache_dir.to_path_buf(),
        source,
    })?;

    let (final_path, tmp_path) = cache_paths(cache_dir, key);

    let to_io_error = |source: std::io::Error| GlydeError::Io {
        path: tmp_path.clone(),
        source,
    };

    {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(to_io_error)?;
        let mut writer = BufWriter::new(file);

        let mut header = [0u8; HEADER_LEN];
        header[0..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&FORMAT_VERSION.to_ne_bytes());
        header[12..16].copy_from_slice(&(pyramid.len() as u32).to_ne_bytes());
        writer.write_all(&header).map_err(to_io_error)?;

        for level in pyramid {
            writer
                .write_all(&(level.len() as u64).to_ne_bytes())
                .map_err(to_io_error)?;
        }

        for level in pyramid {
            for bucket in level {
                writer
                    .write_all(&encode_bucket(bucket))
                    .map_err(to_io_error)?;
            }
        }

        writer
            .flush()
            .and_then(|()| writer.get_ref().sync_all())
            .map_err(to_io_error)?;
    }

    std::fs::rename(&tmp_path, &final_path).map_err(|source| GlydeError::Io {
        path: final_path,
        source,
    })?;
    Ok(())
}

fn encode_bucket(bucket: &Bucket) -> [u8; BUCKET_RECORD_LEN] {
    let mut record = [0u8; BUCKET_RECORD_LEN];
    record[0..8].copy_from_slice(&bucket.min.to_ne_bytes());
    record[8..16].copy_from_slice(&bucket.max.to_ne_bytes());
    record[16..32].copy_from_slice(&bucket.first_ts.to_ne_bytes());
    record[32..48].copy_from_slice(&bucket.last_ts.to_ne_bytes());
    record[48..56].copy_from_slice(&bucket.nan_count.to_ne_bytes());
    record
}

fn decode_bucket(record: &[u8; BUCKET_RECORD_LEN]) -> Bucket {
    Bucket {
        min: f64::from_ne_bytes(record[0..8].try_into().expect("8-byte slice")),
        max: f64::from_ne_bytes(record[8..16].try_into().expect("8-byte slice")),
        first_ts: i128::from_ne_bytes(record[16..32].try_into().expect("16-byte slice")),
        last_ts: i128::from_ne_bytes(record[32..48].try_into().expect("16-byte slice")),
        nan_count: u64::from_ne_bytes(record[48..56].try_into().expect("8-byte slice")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_key(dir: &Path, tag: &str) -> CacheKey {
        // Mirrors `index::level0`'s test helper: `CacheKey::for_path` reads
        // real filesystem metadata, so tests point it at a real (temp) file
        // rather than fabricating a key by hand.
        let source = dir.join(format!("source-{tag}.csv"));
        std::fs::write(
            &source,
            b"irrelevant, real metadata is all `for_path` reads",
        )
        .expect("temp source file must be writable");
        CacheKey::for_path(&source).expect("temp source file must have readable metadata")
    }

    fn sine_wave(sample_count: usize) -> (Vec<f64>, Vec<i128>) {
        let timestamps: Vec<i128> = (0..sample_count as i128).collect();
        let samples: Vec<f64> = (0..sample_count)
            .map(|i| ((i as f64) * 0.037).sin() * 1000.0)
            .collect();
        (samples, timestamps)
    }

    #[test]
    fn round_trips_a_multi_level_pyramid_through_build_and_reopen() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "multi-level");
        // Enough samples to produce several pyramid levels above the
        // PYRAMID_FACTOR (8) threshold, not just a single-level pyramid.
        let (samples, timestamps) = sine_wave(5_000);

        let built = build(dir.path(), &key, &samples, &timestamps).expect("build must succeed");
        assert!(
            built.len() > 1,
            "5,000 samples must produce multiple pyramid levels"
        );

        let reopened = try_open(dir.path(), &key)
            .expect("try_open must not error")
            .expect("cache must be present after build");

        assert_eq!(
            reopened, built,
            "a reopened pyramid must be bucket-for-bucket identical to the one that was cached"
        );
        assert_eq!(
            reopened,
            build_pyramid(&samples, &timestamps),
            "a reopened pyramid must also match an independently recomputed pyramid over the same data"
        );
    }

    #[test]
    fn empty_input_round_trips_to_a_zero_level_pyramid() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "empty");

        let built = build(dir.path(), &key, &[], &[]).expect("build must succeed on empty input");
        assert!(built.is_empty());

        let reopened = try_open(dir.path(), &key)
            .expect("try_open must not error")
            .expect("cache must be present after build");
        assert!(reopened.is_empty());
    }

    #[test]
    fn build_or_open_reopens_an_existing_cache_without_rebuilding() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "reopen");
        let (samples, timestamps) = sine_wave(200);

        let built =
            build(dir.path(), &key, &samples, &timestamps).expect("first build must succeed");

        // Pass data that, if it were used, would produce a visibly different
        // pyramid — proving `build_or_open` served the existing cache
        // instead of recomputing from these.
        let wrong_timestamps: Vec<i128> = vec![99, 100, 101];
        let wrong_samples: Vec<f64> = vec![-1.0, -2.0, -3.0];
        let reopened = build_or_open(dir.path(), &key, &wrong_samples, &wrong_timestamps)
            .expect("reopen must succeed");

        assert_eq!(reopened, built);
    }

    #[test]
    fn a_changed_source_file_misses_the_old_cache_and_rebuilds() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.csv");
        std::fs::write(&source, b"version one").expect("write source");
        let key_v1 = CacheKey::for_path(&source).expect("metadata readable");
        build(dir.path(), &key_v1, &[1.0], &[0]).expect("build v1");

        std::fs::write(&source, b"a completely different, longer version two")
            .expect("overwrite source");
        let key_v2 = CacheKey::for_path(&source).expect("metadata readable");

        assert_ne!(
            key_v1, key_v2,
            "changing the source file must change its cache key"
        );
        let hit = try_open(dir.path(), &key_v2).expect("try_open must not error");
        assert!(
            hit.is_none(),
            "the new key must not find the old cache written under key_v1"
        );
    }

    #[test]
    fn try_open_reports_no_cache_instead_of_erroring_when_the_file_is_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "absent");

        let result = try_open(dir.path(), &key).expect("a missing cache must not be an error");
        assert!(result.is_none());
    }

    #[test]
    fn a_cache_written_under_a_future_format_version_is_rejected_not_misread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "future-version");
        build(dir.path(), &key, &[1.0], &[0]).expect("build v1 cache");

        let (final_path, _) = cache_paths(dir.path(), &key);
        let mut bytes = std::fs::read(&final_path).expect("read cache file");
        bytes[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_ne_bytes());
        std::fs::write(&final_path, bytes).expect("rewrite cache file with a future version");

        let result = try_open(dir.path(), &key).expect("must not error, only miss");
        assert!(
            result.is_none(),
            "an unrecognized format version must be treated as a cache miss, never misread as v1 data"
        );
    }

    #[test]
    fn a_truncated_cache_file_is_rejected_not_misread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "truncated");
        let (samples, timestamps) = sine_wave(200);
        build(dir.path(), &key, &samples, &timestamps).expect("build must succeed");

        let (final_path, _) = cache_paths(dir.path(), &key);
        let mut bytes = std::fs::read(&final_path).expect("read cache file");
        bytes.truncate(bytes.len() / 2);
        std::fs::write(&final_path, bytes).expect("rewrite cache file truncated");

        let result = try_open(dir.path(), &key).expect("must not error, only miss");
        assert!(
            result.is_none(),
            "a truncated cache file must be treated as a cache miss, never partially misread"
        );
    }

    #[test]
    fn a_corrupted_level_count_field_is_rejected_not_misread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "corrupt-level-count");
        build(dir.path(), &key, &[1.0, 2.0], &[0, 1]).expect("build must succeed");

        let (final_path, _) = cache_paths(dir.path(), &key);
        let mut bytes = std::fs::read(&final_path).expect("read cache file");
        // Claim an enormous level count the file's real (tiny) size cannot
        // back — this must be rejected as corrupt, never trusted to size a
        // `Vec::with_capacity` (that used to panic/OOM on exactly this).
        bytes[12..16].copy_from_slice(&u32::MAX.to_ne_bytes());
        std::fs::write(&final_path, bytes)
            .expect("rewrite cache file with a corrupted level count");

        let result = try_open(dir.path(), &key).expect("must not error, only miss");
        assert!(
            result.is_none(),
            "a level count the file's real size cannot back must be rejected, never used to preallocate"
        );
    }

    #[test]
    fn a_corrupted_bucket_count_field_is_rejected_not_misread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "corrupt-bucket-count");
        let (samples, timestamps) = sine_wave(5_000);
        build(dir.path(), &key, &samples, &timestamps).expect("build must succeed");

        let (final_path, _) = cache_paths(dir.path(), &key);
        let mut bytes = std::fs::read(&final_path).expect("read cache file");
        // The first level-table entry starts right after the 16-byte header;
        // claim an enormous bucket count the file's real size cannot back.
        bytes[16..24].copy_from_slice(&u64::MAX.to_ne_bytes());
        std::fs::write(&final_path, bytes)
            .expect("rewrite cache file with a corrupted bucket count");

        let result = try_open(dir.path(), &key).expect("must not error, only miss");
        assert!(
            result.is_none(),
            "a bucket count the file's real size cannot back must be rejected, never used to preallocate"
        );
    }

    #[test]
    fn preserves_nan_bit_patterns_through_the_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), "nan");
        let timestamps: Vec<i128> = (0..20).collect();
        let samples: Vec<f64> = (0..20)
            .map(|i| if i % 5 == 0 { f64::NAN } else { i as f64 })
            .collect();

        let built = build(dir.path(), &key, &samples, &timestamps).expect("build must succeed");
        let reopened = try_open(dir.path(), &key)
            .expect("try_open must not error")
            .expect("cache must be present");

        for (level_built, level_reopened) in built.iter().zip(reopened.iter()) {
            for (bucket_built, bucket_reopened) in level_built.iter().zip(level_reopened.iter()) {
                assert_eq!(bucket_built.min.to_bits(), bucket_reopened.min.to_bits());
                assert_eq!(bucket_built.max.to_bits(), bucket_reopened.max.to_bits());
                assert_eq!(bucket_built.nan_count, bucket_reopened.nan_count);
            }
        }
    }
}
