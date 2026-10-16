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
//! every caller — [`crate::ingest::level0_for_dataset_cached_with_cache_dir`],
//! wired into the completed-load open path per issue #92 — but not a reason
//! to widen this module's own contract).

use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use memmap2::Mmap;

use crate::{GlydeError, Result};

const MAGIC: &[u8; 8] = b"GLYDEC0\0";
/// 2: the timestamps file is shared by every column of one file (issue
/// #115) — a version-1 values file, written next to its own per-column
/// timestamps, is rebuilt rather than paired with a shared axis.
const FORMAT_VERSION: u32 = 2;
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
    /// `None` identifies the whole file (the original, single-series
    /// scheme); `Some(index)` scopes the key to one column of a
    /// multi-column dataset, so a reopen's per-column Level 0/pyramid caches
    /// (issue #81) never collide with each other or with the whole-file
    /// form — `Option<u32>` makes the two kinds hash and compare unequal by
    /// construction, not by convention.
    column: Option<u32>,
    /// A fingerprint of whatever `crate::ingest::IngestOverrides` produced
    /// the dataset this key caches (docs/ROADMAP.md M4 "One-click
    /// correction of each field → triggers a re-index"), `0` for the
    /// no-overrides case. Path + size + mtime alone assume a given file
    /// always parses to the same dataset — true for every reader until a
    /// caller can request a *different* parse of the same, byte-for-byte
    /// unchanged file (a corrected delimiter/decimal-separator/timestamp
    /// format). Without this field, a corrected re-open would collide with
    /// the cache entry written under the pre-correction reading (or vice
    /// versa) and silently serve stale buckets. A plain `u64` rather than
    /// `IngestOverrides` itself so this module — the lower-level indexing
    /// primitive — never has to depend on `crate::ingest`'s types.
    overrides_signature: u64,
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
            column: None,
            overrides_signature: 0,
        })
    }

    /// Scopes this key to one column of the dataset, so a file with several
    /// numeric columns gets one independent cache entry per column instead
    /// of every column overwriting the same one (issue #81). Cheap: reuses
    /// the already-read path/size/mtime rather than re-reading metadata.
    pub fn with_column(&self, column_index: usize) -> Self {
        Self {
            column: Some(column_index as u32),
            ..self.clone()
        }
    }

    /// Scopes this key to the [`crate::ingest::IngestOverrides`] that
    /// produced the dataset being cached (docs/ROADMAP.md M4), so a
    /// corrected re-open of the same, byte-for-byte unchanged file never
    /// collides with the cache entry a different set of overrides (or no
    /// overrides at all) wrote for it. `signature` is a plain hash of the
    /// overrides — see the field's own doc comment for why this takes a
    /// `u64` rather than the `IngestOverrides` type itself.
    pub fn with_overrides_signature(&self, signature: u64) -> Self {
        Self {
            overrides_signature: signature,
            ..self.clone()
        }
    }

    /// A filesystem-safe, content-addressed stem shared by every cache file
    /// for this key — Level 0's pair and `index::pyramid`'s cache alike. Any
    /// change to path, size, or mtime hashes to a different stem, so a stale
    /// cache is simply never found under the new key's name (no explicit
    /// invalidation step needed).
    pub(crate) fn cache_stem(&self) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// The stem of the timestamps file every column of this key's file
    /// shares (issue #115): this key without its column, so the source
    /// file and the overrides still pick it, but the column no longer does.
    fn timestamps_stem(&self) -> String {
        Self {
            column: None,
            ..self.clone()
        }
        .cache_stem()
    }

    fn timestamps_path(&self, cache_dir: &Path) -> PathBuf {
        cache_dir.join(format!("{}.ts.glyc0", self.timestamps_stem()))
    }

    fn values_path(&self, cache_dir: &Path) -> PathBuf {
        cache_dir.join(format!("{}.val.glyc0", self.cache_stem()))
    }
}

/// A memory-mapped Level-0 cache: the raw `timestamps`/`samples` of one
/// dataset, backed by two on-disk files rather than a heap `Vec`. Both
/// accessors return real borrowed slices, usable anywhere
/// [`crate::dsp::decimation::build_pyramid`]/[`crate::dsp::decimation::decimate_viewport`]
/// take `&[f64]`/`&[i128]`.
pub struct Level0Cache {
    /// Shared by every column of the same file (issue #115).
    timestamps_mmap: Arc<Mmap>,
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

/// Cheap: prints only the sample count, never the mapped contents — a cache
/// can back a file with millions of samples, so an assertion failure message
/// must not try to render every one of them.
impl std::fmt::Debug for Level0Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Level0Cache")
            .field("sample_count", &self.sample_count)
            .finish()
    }
}

/// Structural equality over the mapped contents, not the backing files —
/// two caches (e.g. one freshly built, one reopened from disk) compare equal
/// whenever their samples and timestamps do, which is what
/// `tests/level0_reopen_integration.rs`'s cache-reuse assertions need.
impl PartialEq for Level0Cache {
    fn eq(&self, other: &Self) -> bool {
        self.sample_count == other.sample_count
            && self.timestamps() == other.timestamps()
            && self.samples() == other.samples()
    }
}

/// Appends samples to a new Level-0 cache one at a time, so a chunked reader
/// never has to hold the whole column in memory to build the cache
/// (docs/SPEC.md §5.1 "the full file is never loaded"). Construct with
/// [`Level0CacheWriter::create`], call [`Self::push`] once per sample in
/// order, then [`Self::finish`].
///
/// The timestamps are only written when no valid shared timestamps file
/// exists for the key's file yet (issue #115); otherwise the existing one is
/// reused and the pushed timestamps are only counted.
pub struct Level0CacheWriter {
    timestamps: TimestampsSink,
    values_writer: BufWriter<File>,
    values_tmp_path: PathBuf,
    values_final_path: PathBuf,
    sample_count: u64,
}

/// Where a [`Level0CacheWriter`]'s timestamps go.
enum TimestampsSink {
    /// This file's shared timestamps already exist.
    Shared { mmap: Arc<Mmap>, path: PathBuf },
    /// First writer for this file: write them, to a temp file of its own.
    Writing {
        writer: BufWriter<File>,
        tmp_path: PathBuf,
        final_path: PathBuf,
    },
}

/// Makes concurrent writers' temp files distinct (two columns of one file
/// may both find no shared timestamps yet and both write them).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmp(path: &Path) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    PathBuf::from(name)
}

impl Level0CacheWriter {
    /// Creates the backing files (as `.tmp` siblings, swapped into place
    /// atomically on [`Self::finish`] so a reader can never observe a
    /// half-written cache) and writes their headers.
    pub fn create(cache_dir: &Path, key: &CacheKey) -> Result<Self> {
        std::fs::create_dir_all(cache_dir).map_err(|source| GlydeError::Io {
            path: cache_dir.to_path_buf(),
            source,
        })?;

        let timestamps_final_path = key.timestamps_path(cache_dir);
        let values_final_path = key.values_path(cache_dir);
        let values_tmp_path = unique_tmp(&values_final_path);

        let timestamps = match open_shared_timestamps(&timestamps_final_path)? {
            Some(mmap) => TimestampsSink::Shared {
                mmap,
                path: timestamps_final_path,
            },
            None => {
                let tmp_path = unique_tmp(&timestamps_final_path);
                let mut writer = BufWriter::new(create_file(&tmp_path)?);
                write_header(&mut writer, &tmp_path)?;
                TimestampsSink::Writing {
                    writer,
                    tmp_path,
                    final_path: timestamps_final_path,
                }
            }
        };
        let mut values_writer = BufWriter::new(create_file(&values_tmp_path)?);
        write_header(&mut values_writer, &values_tmp_path)?;

        Ok(Self {
            timestamps,
            values_writer,
            values_tmp_path,
            values_final_path,
            sample_count: 0,
        })
    }

    /// Appends one `(timestamp, value)` pair. Samples must be pushed in the
    /// same order the dataset's [`Level0Cache::timestamps`]/
    /// [`Level0Cache::samples`] should later report them.
    pub fn push(&mut self, timestamp: i128, value: f64) -> Result<()> {
        self.push_slices(&[timestamp], &[value])
    }

    /// Appends many `(timestamp, value)` pairs at once — byte for byte what
    /// one [`Self::push`] per pair writes (both are the elements' native
    /// in-memory bytes), but as two bulk writes instead of two per sample
    /// (issue #114: per-sample writes dominated building the cache for an
    /// already-materialized column).
    pub fn push_slices(&mut self, timestamps: &[i128], samples: &[f64]) -> Result<()> {
        debug_assert_eq!(
            samples.len(),
            timestamps.len(),
            "samples and timestamps must be the same length"
        );
        let count = timestamps.len().min(samples.len());
        if let TimestampsSink::Writing {
            writer, tmp_path, ..
        } = &mut self.timestamps
        {
            writer
                .write_all(bytemuck::cast_slice(&timestamps[..count]))
                .map_err(|source| GlydeError::Io {
                    path: tmp_path.clone(),
                    source,
                })?;
        }
        self.values_writer
            .write_all(bytemuck::cast_slice(&samples[..count]))
            .map_err(|source| GlydeError::Io {
                path: self.values_tmp_path.clone(),
                source,
            })?;
        self.sample_count += count as u64;
        Ok(())
    }

    /// Flushes and syncs the files written, atomically renames them into
    /// place and memory-maps the result.
    pub fn finish(mut self) -> Result<Level0Cache> {
        self.values_writer
            .flush()
            .and_then(|()| self.values_writer.get_ref().sync_all())
            .map_err(|source| GlydeError::Io {
                path: self.values_tmp_path.clone(),
                source,
            })?;
        drop(self.values_writer);

        let timestamps_mmap = match self.timestamps {
            TimestampsSink::Shared { mmap, path } => {
                let shared = (mmap.len() - HEADER_LEN) / size_of::<i128>();
                if shared as u64 != self.sample_count {
                    let _ = std::fs::remove_file(&self.values_tmp_path);
                    return Err(GlydeError::CorruptCache {
                        path,
                        reason: format!(
                            "the shared timestamps hold {shared} samples, this column \
                             {}",
                            self.sample_count
                        ),
                    });
                }
                mmap
            }
            TimestampsSink::Writing {
                mut writer,
                tmp_path,
                final_path,
            } => {
                writer
                    .flush()
                    .and_then(|()| writer.get_ref().sync_all())
                    .map_err(|source| GlydeError::Io {
                        path: tmp_path.clone(),
                        source,
                    })?;
                drop(writer);
                // A concurrent writer for another column may have published
                // the same timestamps meanwhile; its file is identical, so
                // either one is the one kept. Windows refuses to replace a
                // file another column already has mapped: keep that one.
                if let Err(source) = std::fs::rename(&tmp_path, &final_path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    let published = open_shared_timestamps(&final_path)?.filter(|mmap| {
                        (mmap.len() - HEADER_LEN) / size_of::<i128>() == self.sample_count as usize
                    });
                    if published.is_none() {
                        let _ = std::fs::remove_file(&self.values_tmp_path);
                        return Err(GlydeError::Io {
                            path: final_path,
                            source,
                        });
                    }
                }
                open_shared_timestamps(&final_path)?.ok_or_else(|| GlydeError::CorruptCache {
                    path: final_path.clone(),
                    reason: "cache file disappeared immediately after being written".to_string(),
                })?
            }
        };

        std::fs::rename(&self.values_tmp_path, &self.values_final_path).map_err(|source| {
            GlydeError::Io {
                path: self.values_final_path.clone(),
                source,
            }
        })?;
        let values_mmap =
            map_file(&self.values_final_path)?.ok_or_else(|| GlydeError::CorruptCache {
                path: self.values_final_path.clone(),
                reason: "cache file disappeared immediately after being written".to_string(),
            })?;
        Ok(Level0Cache {
            timestamps_mmap,
            values_mmap,
            sample_count: self.sample_count as usize,
        })
    }
}

/// The shared timestamps at `path`, mapped, if present with a valid header.
fn open_shared_timestamps(path: &Path) -> Result<Option<Arc<Mmap>>> {
    let Some(mmap) = map_file(path)? else {
        return Ok(None);
    };
    if validated_sample_count(&mmap, size_of::<i128>()).is_none() {
        tracing::warn!(path = %path.display(), "Level-0 timestamps header invalid, rebuilding");
        return Ok(None);
    }
    Ok(Some(Arc::new(mmap)))
}

/// Builds the timestamps every column of `key`'s file shares, unless a valid
/// file of the same length already exists (issue #115). Called once per load,
/// before the columns' caches are built in parallel, so they all find it.
pub fn build_or_open_shared_timestamps(
    cache_dir: &Path,
    key: &CacheKey,
    timestamps: &[i128],
) -> Result<()> {
    let path = key.timestamps_path(cache_dir);
    if let Some(mmap) = open_shared_timestamps(&path)? {
        if (mmap.len() - HEADER_LEN) / size_of::<i128>() == timestamps.len() {
            return Ok(());
        }
    }
    std::fs::create_dir_all(cache_dir).map_err(|source| GlydeError::Io {
        path: cache_dir.to_path_buf(),
        source,
    })?;
    let tmp_path = unique_tmp(&path);
    let mut writer = BufWriter::new(create_file(&tmp_path)?);
    write_header(&mut writer, &tmp_path)?;
    let io = |source| GlydeError::Io {
        path: tmp_path.clone(),
        source,
    };
    writer
        .write_all(bytemuck::cast_slice(timestamps))
        .map_err(io)?;
    writer
        .flush()
        .and_then(|()| writer.get_ref().sync_all())
        .map_err(io)?;
    drop(writer);
    std::fs::rename(&tmp_path, &path).map_err(|source| GlydeError::Io {
        path: path.clone(),
        source,
    })?;
    tracing::info!(
        path = %path.display(),
        samples = timestamps.len(),
        "Level-0 timestamps written once, shared by every column (issue #115)"
    );
    Ok(())
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
    writer.push_slices(timestamps, samples)?;
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
    let timestamps_path = key.timestamps_path(cache_dir);
    let values_path = key.values_path(cache_dir);

    let Some(timestamps_mmap) = open_shared_timestamps(&timestamps_path)? else {
        return Ok(None);
    };
    let Some(values_mmap) = map_file(&values_path)? else {
        return Ok(None);
    };

    let ts_sample_count = (timestamps_mmap.len() - HEADER_LEN) / size_of::<i128>();
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
    if !data_len.is_multiple_of(element_size) {
        return None;
    }
    Some(data_len / element_size)
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
    fn with_column_scopes_the_cache_key_so_columns_never_collide() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 42);

        let column_0 = key.with_column(0);
        let column_1 = key.with_column(1);

        assert_ne!(
            column_0, key,
            "a column-scoped key must differ from the whole-file key"
        );
        assert_ne!(
            column_0, column_1,
            "two different columns of the same file must produce different keys"
        );
        assert_ne!(
            column_0.cache_stem(),
            column_1.cache_stem(),
            "different columns must never share a cache file stem"
        );

        // Columns of one file share its time axis (issue #115); only their
        // values differ.
        let timestamps: Vec<i128> = vec![0, 1, 2];
        let samples_0: Vec<f64> = vec![10.0, 20.0, 30.0];
        let samples_1: Vec<f64> = vec![-1.0, -2.0, -3.0];

        build(dir.path(), &column_0, &samples_0, &timestamps).expect("build column 0");
        build(dir.path(), &column_1, &samples_1, &timestamps).expect("build column 1");

        let reopened_0 = try_open(dir.path(), &column_0)
            .expect("try_open must not error")
            .expect("column 0's cache must be present");
        let reopened_1 = try_open(dir.path(), &column_1)
            .expect("try_open must not error")
            .expect("column 1's cache must be present");

        assert_eq!(reopened_0.samples(), samples_0.as_slice());
        assert_eq!(reopened_1.samples(), samples_1.as_slice());
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

    fn files_ending(dir: &Path, suffix: &str) -> usize {
        std::fs::read_dir(dir)
            .expect("read cache dir")
            .filter(|entry| {
                entry
                    .as_ref()
                    .expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .ends_with(suffix)
            })
            .count()
    }

    // Issue #115: the time axis is the same for every column of a file, so
    // it is written once and shared, not once per column.
    #[test]
    fn columns_of_one_file_share_a_single_timestamps_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 4);
        let timestamps: Vec<i128> = vec![0, 10, 20, 30];

        let caches: Vec<Level0Cache> = (0..3)
            .map(|column| {
                let samples: Vec<f64> = (0..4).map(|i| (column * 10 + i) as f64).collect();
                build(dir.path(), &key.with_column(column), &samples, &timestamps)
                    .expect("build column")
            })
            .collect();

        assert_eq!(files_ending(dir.path(), ".ts.glyc0"), 1);
        assert_eq!(files_ending(dir.path(), ".val.glyc0"), 3);
        for (column, cache) in caches.iter().enumerate() {
            assert_eq!(cache.timestamps(), timestamps.as_slice());
            let reopened = try_open(dir.path(), &key.with_column(column))
                .expect("no error")
                .expect("cache hit");
            assert_eq!(&reopened, cache);
        }
    }

    // A different reading of the same file (an override) is a different time
    // axis, so it never shares the other reading's timestamps.
    #[test]
    fn different_overrides_never_share_timestamps() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 2);
        let plain = key.with_column(0);
        let corrected = key.with_overrides_signature(7).with_column(0);

        build(dir.path(), &plain, &[1.0, 2.0], &[0, 1]).expect("build plain");
        build(dir.path(), &corrected, &[1.0, 2.0], &[5, 9]).expect("build corrected");

        assert_eq!(files_ending(dir.path(), ".ts.glyc0"), 2);
        let reopened = try_open(dir.path(), &plain)
            .expect("no error")
            .expect("hit");
        assert_eq!(reopened.timestamps(), &[0, 1]);
    }

    // Values that do not line up with the shared timestamps are never served.
    #[test]
    fn values_that_disagree_with_the_shared_timestamps_are_a_miss() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = temp_key(dir.path(), 3);
        build(
            dir.path(),
            &key.with_column(0),
            &[1.0, 2.0, 3.0],
            &[0, 1, 2],
        )
        .expect("build");
        let values = dir
            .path()
            .join(format!("{}.val.glyc0", key.with_column(0).cache_stem()));
        let mut bytes = std::fs::read(&values).expect("read values");
        bytes.truncate(bytes.len() - 8);
        std::fs::write(&values, bytes).expect("truncate values");

        assert!(try_open(dir.path(), &key.with_column(0))
            .expect("no error")
            .is_none());
    }
}
