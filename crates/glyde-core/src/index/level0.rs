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

//! Typed Level-0 spill cache (issue #59 decision, recorded in
//! docs/ARCHITECTURE.md §The index).
//!
//! `docs/ARCHITECTURE.md` describes Level 0 as "raw samples (in file,
//! memory-mapped, never fully loaded)" — true for Parquet, where the file
//! already *is* typed binary data. It is not true for CSV/TSV: raw samples
//! there exist only as text, and reaching sample *n* means re-parsing from
//! the start. This module is what makes the sentence true for every format:
//! at index time, decoded `(timestamp, value)` pairs are written once,
//! streaming, to a fixed-width typed cache file pair in the OS cache
//! directory, keyed by source path + size + mtime — the same scheme
//! `docs/ARCHITECTURE.md` §The index already commits to for the pyramid.
//! Reopening a known file memory-maps the existing cache directly, no
//! re-parsing, and the mapped bytes are handed to
//! [`crate::dsp::decimation`] as real `&[f64]`/`&[i128]` slices: the
//! large-file path is the *same* engine the decimation golden tests lock,
//! just fed a memory-mapped slice instead of a heap `Vec`.
//!
//! **Disk cost.** Roughly the size of the raw numeric data again per opened
//! file (16 bytes/sample for timestamps, 8 bytes/sample for values), on top
//! of the source file. **Cache eviction is not implemented** — this module
//! only ever adds cache files, never removes stale ones; tracked as a
//! follow-up (see the PR that introduced this module for the issue link).
//!
//! **Streaming write.** [`Level0CacheWriter::push`] appends one sample at a
//! time, so a future chunked CSV reader can feed it row-by-row without ever
//! materializing the whole column in memory. [`build`] is a convenience
//! wrapper over the writer for callers that already have both slices (today,
//! every caller — the small-file loader in [`crate::ingest::dataset`] — but
//! not a reason to widen this module's own contract).

use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::{GlydeError, Result};

const MAGIC: &[u8; 8] = b"GLYDEC0\0";
const FORMAT_VERSION: u32 = 1;
/// Fixed header size for both the timestamps and values cache files: an
/// 8-byte magic, a 4-byte format version, 4 bytes reserved. 16 is itself a
/// multiple of `size_of::<i128>()` (16), so data immediately following the
/// header in a page-aligned `mmap` is correctly aligned for
/// [`bytemuck::cast_slice`] without any extra padding logic.
const HEADER_LEN: usize = 16;

/// Identifies one cached dataset by the same "reopening a known file is
/// instant" scheme `docs/ARCHITECTURE.md` §The index uses for the pyramid:
/// source path + file size + modification time. Any of the three changing
/// misses the cache and rebuilds it — this is a correctness mechanism, not
/// just a performance one: a file edited between opens must never serve
/// stale cached samples.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    source_path: PathBuf,
    source_size: u64,
    source_mtime_unix_nanos: i128,
}

impl CacheKey {
    /// Reads `source_path`'s current size and modification time from the
    /// filesystem.
    pub fn for_path(source_path: &Path) -> Result<Self> {
        let to_io_error = |source: std::io::Error| GlydeError::Io {
            path: source_path.to_path_buf(),
            source,
        };
        let metadata = std::fs::metadata(source_path).map_err(to_io_error)?;
        let modified = metadata.modified().map_err(to_io_error)?;
        let source_mtime_unix_nanos = match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(since_epoch) => since_epoch.as_nanos() as i128,
            // A modification time before the Unix epoch is unusual but not
            // invalid (SPEC §2.1's "never guess silently" spirit applies to
            // our own cache keys too: represent it exactly, don't clamp it).
            Err(before_epoch) => -(before_epoch.duration().as_nanos() as i128),
        };

        Ok(Self {
            source_path: source_path.to_path_buf(),
            source_size: metadata.len(),
            source_mtime_unix_nanos,
        })
    }

    /// A filesystem-safe, content-addressed stem shared by both cache files
    /// for this key. Any change to path, size, or mtime hashes to a
    /// different stem, so a stale cache is simply never found under the new
    /// key's name (no explicit invalidation step needed).
    fn cache_stem(&self) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

/// A memory-mapped Level-0 cache: the raw `timestamps`/`samples` of one
/// dataset, backed by two on-disk files rather than a heap `Vec`. Both
/// accessors return real borrowed slices, usable anywhere
/// [`crate::dsp::decimation::build_pyramid`]/[`crate::dsp::decimation::decimate_viewport`]
/// take `&[f64]`/`&[i128]`.
pub struct Level0Cache {
    timestamps_mmap: Mmap,
    values_mmap: Mmap,
    sample_count: usize,
}

impl Level0Cache {
    /// The dataset's timestamps, in source row order.
    pub fn timestamps(&self) -> &[i128] {
        bytemuck::cast_slice(&self.timestamps_mmap[HEADER_LEN..])
    }

    /// The dataset's sample values, in source row order, one-to-one with
    /// [`Self::timestamps`].
    pub fn samples(&self) -> &[f64] {
        bytemuck::cast_slice(&self.values_mmap[HEADER_LEN..])
    }

    pub fn sample_count(&self) -> usize {
        self.sample_count
    }
}

/// Appends samples to a new Level-0 cache one at a time, so a chunked reader
/// never has to hold the whole column in memory to build the cache
/// (docs/SPEC.md §5.1 "the full file is never loaded"). Construct with
/// [`Level0CacheWriter::create`], call [`Self::push`] once per sample in
/// order, then [`Self::finish`].
pub struct Level0CacheWriter {
    timestamps_writer: BufWriter<File>,
    values_writer: BufWriter<File>,
    timestamps_tmp_path: PathBuf,
    values_tmp_path: PathBuf,
    timestamps_final_path: PathBuf,
    values_final_path: PathBuf,
    sample_count: u64,
}

impl Level0CacheWriter {
    /// Creates the two backing files (as `.tmp` siblings, swapped into place
    /// atomically on [`Self::finish`] so a reader can never observe a
    /// half-written cache) and writes their headers.
    pub fn create(cache_dir: &Path, key: &CacheKey) -> Result<Self> {
        std::fs::create_dir_all(cache_dir).map_err(|source| GlydeError::Io {
            path: cache_dir.to_path_buf(),
            source,
        })?;

        let stem = key.cache_stem();
        let timestamps_final_path = cache_dir.join(format!("{stem}.ts.glyc0"));
        let values_final_path = cache_dir.join(format!("{stem}.val.glyc0"));
        let timestamps_tmp_path = cache_dir.join(format!("{stem}.ts.glyc0.tmp"));
        let values_tmp_path = cache_dir.join(format!("{stem}.val.glyc0.tmp"));

        let mut timestamps_writer = BufWriter::new(create_file(&timestamps_tmp_path)?);
        let mut values_writer = BufWriter::new(create_file(&values_tmp_path)?);
        write_header(&mut timestamps_writer, &timestamps_tmp_path)?;
        write_header(&mut values_writer, &values_tmp_path)?;

        Ok(Self {
            timestamps_writer,
            values_writer,
            timestamps_tmp_path,
            values_tmp_path,
            timestamps_final_path,
            values_final_path,
            sample_count: 0,
        })
    }

    /// Appends one `(timestamp, value)` pair. Samples must be pushed in the
    /// same order the dataset's [`Level0Cache::timestamps`]/
    /// [`Level0Cache::samples`] should later report them.
    pub fn push(&mut self, timestamp: i128, value: f64) -> Result<()> {
        self.timestamps_writer
            .write_all(bytemuck::bytes_of(&timestamp))
            .map_err(|source| GlydeError::Io {
                path: self.timestamps_tmp_path.clone(),
                source,
            })?;
        self.values_writer
            .write_all(bytemuck::bytes_of(&value))
            .map_err(|source| GlydeError::Io {
                path: self.values_tmp_path.clone(),
                source,
            })?;
        self.sample_count += 1;
        Ok(())
    }

    /// Flushes and syncs both files, then atomically renames them into
    /// place and memory-maps the result.
    pub fn finish(mut self) -> Result<Level0Cache> {
        self.timestamps_writer
            .flush()
            .and_then(|()| self.timestamps_writer.get_ref().sync_all())
            .map_err(|source| GlydeError::Io {
                path: self.timestamps_tmp_path.clone(),
                source,
            })?;
        self.values_writer
            .flush()
            .and_then(|()| self.values_writer.get_ref().sync_all())
            .map_err(|source| GlydeError::Io {
                path: self.values_tmp_path.clone(),
                source,
            })?;
        drop(self.timestamps_writer);
        drop(self.values_writer);

        std::fs::rename(&self.timestamps_tmp_path, &self.timestamps_final_path).map_err(
            |source| GlydeError::Io {
                path: self.timestamps_final_path.clone(),
                source,
            },
        )?;
        std::fs::rename(&self.values_tmp_path, &self.values_final_path).map_err(|source| {
            GlydeError::Io {
                path: self.values_final_path.clone(),
                source,
            }
        })?;

        open_cache_files(
            &self.timestamps_final_path,
            &self.values_final_path,
            self.sample_count,
        )
    }
}

fn create_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|source| GlydeError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn write_header(writer: &mut BufWriter<File>, path: &Path) -> Result<()> {
    let mut header = [0u8; HEADER_LEN];
    header[0..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&FORMAT_VERSION.to_ne_bytes());
    writer.write_all(&header).map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Builds a new Level-0 cache from slices already in memory. A convenience
/// over [`Level0CacheWriter`] for callers that already materialized both —
/// today, [`crate::ingest::dataset::load`]'s small-file path.
pub fn build(
    cache_dir: &Path,
    key: &CacheKey,
    samples: &[f64],
    timestamps: &[i128],
) -> Result<Level0Cache> {
    debug_assert_eq!(
        samples.len(),
        timestamps.len(),
        "samples and timestamps must be the same length"
    );

    let mut writer = Level0CacheWriter::create(cache_dir, key)?;
    for (&timestamp, &value) in timestamps.iter().zip(samples) {
        writer.push(timestamp, value)?;
    }
    writer.finish()
}

/// Opens the existing cache for `key` if one is present and valid; builds
/// (and opens) a new one from `samples`/`timestamps` otherwise. This is
/// `docs/ARCHITECTURE.md`'s "reopening a known file is instant" for Level 0:
/// a cache hit never touches `samples`/`timestamps` at all.
pub fn build_or_open(
    cache_dir: &Path,
    key: &CacheKey,
    samples: &[f64],
    timestamps: &[i128],
) -> Result<Level0Cache> {
    if let Some(cache) = try_open(cache_dir, key)? {
        return Ok(cache);
    }
    build(cache_dir, key, samples, timestamps)
}

/// Opens the existing cache for `key`, if the files are present and pass
/// validation. Returns `Ok(None)` — never an error — for an ordinary cache
/// miss (files absent) or a corrupt/foreign cache (header mismatch, or the
/// two files disagree on sample count): both are `warn`-logged and treated
/// as "must rebuild", never trusted (docs/ARCHITECTURE.md §Error philosophy:
/// absorbed anomalies are logged, never silently ignored, but also never
/// escalated to a hard failure over what is only a cache).
pub fn try_open(cache_dir: &Path, key: &CacheKey) -> Result<Option<Level0Cache>> {
    let stem = key.cache_stem();
    let timestamps_path = cache_dir.join(format!("{stem}.ts.glyc0"));
    let values_path = cache_dir.join(format!("{stem}.val.glyc0"));

    if !timestamps_path.exists() || !values_path.exists() {
        return Ok(None);
    }

    let timestamps_mmap = match map_file(&timestamps_path)? {
        Some(mmap) => mmap,
        None => return Ok(None),
    };
    let values_mmap = match map_file(&values_path)? {
        Some(mmap) => mmap,
        None => return Ok(None),
    };

    let Some(ts_sample_count) = validated_sample_count(&timestamps_mmap, size_of::<i128>()) else {
        tracing::warn!(path = %timestamps_path.display(), "Level-0 cache header invalid, rebuilding");
        return Ok(None);
    };
    let Some(val_sample_count) = validated_sample_count(&values_mmap, size_of::<f64>()) else {
        tracing::warn!(path = %values_path.display(), "Level-0 cache header invalid, rebuilding");
        return Ok(None);
    };
    if ts_sample_count != val_sample_count {
        tracing::warn!(
            path = %cache_dir.display(),
            ts_sample_count,
            val_sample_count,
            "Level-0 cache timestamps/values sample counts disagree, rebuilding"
        );
        return Ok(None);
    }

    tracing::info!(
        path = %cache_dir.display(),
        sample_count = ts_sample_count,
        "Level-0 cache hit: reopening without re-parsing the source file"
    );
    Ok(Some(Level0Cache {
        timestamps_mmap,
        values_mmap,
        sample_count: ts_sample_count,
    }))
}

/// `None` if `path` cannot be mapped at all (I/O error mapping it is still
/// surfaced; an empty/too-short file is reported as "no cache" via `Ok(None)`
/// rather than an error, since a zero-length `.tmp` left by a crashed writer
/// is exactly the kind of thing a rebuild should silently recover from).
fn map_file(path: &Path) -> Result<Option<Mmap>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(GlydeError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let len = file
        .metadata()
        .map_err(|source| GlydeError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if len == 0 {
        return Ok(None);
    }
    // SAFETY: the cache file is exclusively owned by `glyde`'s own cache
    // directory and is only ever replaced via the atomic rename in
    // `Level0CacheWriter::finish`, so no other process is expected to
    // truncate or mutate it while mapped. A concurrent external edit would
    // be undefined behavior per `memmap2`'s own safety contract; this is the
    // same trust boundary `docs/ARCHITECTURE.md` already accepts for
    // memory-mapping the *source* file during ingestion.
    let mmap = unsafe { Mmap::map(&file) }.map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(mmap))
}

/// Validates the header (magic + format version) and returns the sample
/// count implied by the file length, or `None` if either check fails.
fn validated_sample_count(mmap: &Mmap, element_size: usize) -> Option<usize> {
    if mmap.len() < HEADER_LEN {
        return None;
    }
    if &mmap[0..8] != MAGIC {
        return None;
    }
    let version = u32::from_ne_bytes(mmap[8..12].try_into().expect("4-byte slice"));
    if version != FORMAT_VERSION {
        return None;
    }
    let data_len = mmap.len() - HEADER_LEN;
    if data_len % element_size != 0 {
        return None;
    }
    Some(data_len / element_size)
}

fn open_cache_files(
    timestamps_path: &Path,
    values_path: &Path,
    sample_count: u64,
) -> Result<Level0Cache> {
    let timestamps_mmap = map_file(timestamps_path)?.ok_or_else(|| GlydeError::CorruptCache {
        path: timestamps_path.to_path_buf(),
        reason: "cache file disappeared immediately after being written".to_string(),
    })?;
    let values_mmap = map_file(values_path)?.ok_or_else(|| GlydeError::CorruptCache {
        path: values_path.to_path_buf(),
        reason: "cache file disappeared immediately after being written".to_string(),
    })?;
    Ok(Level0Cache {
        timestamps_mmap,
        values_mmap,
        sample_count: sample_count as usize,
    })
}

/// Resolves the OS-standard cache directory for Glyde's Level-0/pyramid
/// spill files: `<OS cache dir>/index` (docs/ARCHITECTURE.md §The index).
/// Split out from the build/open functions above (which take an explicit
/// `cache_dir`) so tests exercise the cache format against a temp directory
/// instead of the real OS cache dir — the same split `glyde-app`'s
/// `logging::log_dir` uses for the same reason.
pub fn os_cache_dir() -> Result<PathBuf> {
    let project_dirs = directories::ProjectDirs::from("com", "glyde", "Glyde")
        .ok_or(GlydeError::CacheDirUnavailable)?;
    Ok(project_dirs.cache_dir().join("index"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_key(dir: &Path, samples_len: usize) -> CacheKey {
        // `CacheKey::for_path` reads real filesystem metadata, so tests
        // point it at a real (temp) file rather than fabricating a key by
        // hand — this also exercises the same code path production uses.
        let source = dir.join(format!("source-{samples_len}.csv"));
        std::fs::write(
            &source,
            b"irrelevant, real metadata is all `for_path` reads",
        )
        .expect("temp source file must be writable");
        CacheKey::for_path(&source).expect("temp source file must have readable metadata")
    }

    #[test]
    fn round_trips_samples_and_timestamps_through_build_and_mmap() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 5);
        let timestamps: Vec<i128> = vec![0, 1_000, 2_000, 3_000, 4_000];
        let samples: Vec<f64> = vec![1.5, -2.25, 0.0, f64::NAN, 1_000_000.125];

        let cache = build(dir.path(), &key, &samples, &timestamps).expect("build must succeed");

        assert_eq!(cache.sample_count(), 5);
        assert_eq!(cache.timestamps(), timestamps.as_slice());
        // NaN != NaN, so compare bit patterns instead of using `==` — the
        // round trip must preserve the exact bits, not just "is still NaN".
        assert_eq!(
            cache
                .samples()
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            samples.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn empty_input_round_trips_to_a_zero_sample_cache() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 0);

        let cache = build(dir.path(), &key, &[], &[]).expect("build must succeed on empty input");

        assert_eq!(cache.sample_count(), 0);
        assert!(cache.samples().is_empty());
        assert!(cache.timestamps().is_empty());
    }

    #[test]
    fn build_or_open_reopens_an_existing_cache_without_rebuilding() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 3);
        let timestamps: Vec<i128> = vec![0, 1, 2];
        let samples: Vec<f64> = vec![10.0, 20.0, 30.0];

        build(dir.path(), &key, &samples, &timestamps).expect("first build must succeed");

        // Pass data that, if it were used, would produce a visibly different
        // cache — proving `build_or_open` served the existing cache instead
        // of rebuilding from these.
        let wrong_timestamps: Vec<i128> = vec![99, 100];
        let wrong_samples: Vec<f64> = vec![-1.0, -2.0];
        let cache = build_or_open(dir.path(), &key, &wrong_samples, &wrong_timestamps)
            .expect("reopen must succeed");

        assert_eq!(cache.sample_count(), 3);
        assert_eq!(cache.timestamps(), timestamps.as_slice());
        assert_eq!(cache.samples(), samples.as_slice());
    }

    #[test]
    fn a_changed_source_file_misses_the_old_cache_and_rebuilds() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.csv");
        std::fs::write(&source, b"version one").expect("write source");
        let key_v1 = CacheKey::for_path(&source).expect("metadata readable");
        build(dir.path(), &key_v1, &[1.0], &[0]).expect("build v1");

        // A different size (and, on filesystems with coarse mtime
        // resolution, possibly the same mtime) must still produce a
        // different key, because `CacheKey` hashes size too.
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
    fn writer_push_and_whole_slice_build_agree() {
        let dir = tempfile::tempdir().expect("temp dir");
        let timestamps: Vec<i128> = (0..20).map(|i| i * 100).collect();
        let samples: Vec<f64> = (0..20).map(|i| i as f64 * 0.5).collect();

        let key_a = temp_key(dir.path(), 20);
        let via_writer = {
            let mut writer = Level0CacheWriter::create(dir.path(), &key_a).expect("create writer");
            for (&ts, &value) in timestamps.iter().zip(&samples) {
                writer.push(ts, value).expect("push must succeed");
            }
            writer.finish().expect("finish must succeed")
        };

        let key_b = temp_key(dir.path(), 21);
        let via_build =
            build(dir.path(), &key_b, &samples, &timestamps).expect("build must succeed");

        assert_eq!(via_writer.timestamps(), via_build.timestamps());
        assert_eq!(via_writer.samples(), via_build.samples());
    }

    #[test]
    fn try_open_reports_no_cache_instead_of_erroring_when_files_are_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 1);

        let result = try_open(dir.path(), &key).expect("a missing cache must not be an error");
        assert!(result.is_none());
    }

    #[test]
    fn a_cache_written_under_a_future_format_version_is_rejected_not_misread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 1);
        build(dir.path(), &key, &[1.0], &[0]).expect("build v1 cache");

        let stem = key.cache_stem();
        for suffix in ["ts", "val"] {
            let path = dir.path().join(format!("{stem}.{suffix}.glyc0"));
            let mut bytes = std::fs::read(&path).expect("read cache file");
            bytes[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_ne_bytes());
            std::fs::write(&path, bytes).expect("rewrite cache file with a future version");
        }

        let result = try_open(dir.path(), &key).expect("must not error, only miss");
        assert!(
            result.is_none(),
            "an unrecognized format version must be treated as a cache miss, never misread as v1 data"
        );
    }
}
