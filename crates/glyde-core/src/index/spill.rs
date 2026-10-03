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
//! **Disk cost and cleanup (issue #118).** Spill files are a backing store,
//! never read back by a later open, so they live exactly as long as the
//! columns they back. Every open spills into its own [`SpillSet`]: a fresh
//! directory under `<cache_dir>/spill/` that holds an OS file lock for as
//! long as the set is alive. Every [`SpillVec`] and writer keeps its set
//! alive, and when the last one is dropped (the dataset was closed,
//! replaced by a reopen, or the open failed or was abandoned midway) the
//! whole directory is deleted. A process that dies before that leaves its
//! directory behind with the lock released by the OS, which is how
//! [`sweep_orphaned_spill_files`] tells an orphan from another running
//! Glyde's live set. The sweep also removes the loose `*.glysp` and
//! `*.glysp.tmp` files earlier versions wrote straight into the cache
//! directory and never deleted. Level-0 and pyramid caches are *reused*
//! caches, not backing stores, and their eviction is still deferred.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::mem::size_of;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use bytemuck::Pod;
use memmap2::Mmap;
use tracing::{debug, info, warn};

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

/// The subdirectory of the cache directory every [`SpillSet`] lives under.
const SETS_DIR: &str = "spill";

/// The file inside a [`SpillSet`]'s directory its owner holds locked.
const LOCK_FILE: &str = "lock";

/// How old a set directory must be before [`sweep_orphaned_spill_files`]
/// considers it. [`SpillSet::create`] makes the directory a moment before it
/// takes the lock; a sweep running in that moment must not mistake a set
/// being born for one whose owner died.
const SWEEP_MIN_AGE: Duration = Duration::from_secs(60);

/// Makes every set directory name unique within this process, on top of the
/// process id and clock that make it unique across processes.
static NEXT_SET: AtomicU64 = AtomicU64::new(0);

/// The spill files of one open (issue #118): a directory of its own under
/// `<cache_dir>/spill/`, deleted with everything in it once the last
/// [`SpillVec`] or writer created in it is dropped.
///
/// Cloning is cheap; every clone keeps the set alive.
#[derive(Clone, Debug)]
pub struct SpillSet {
    inner: Arc<SpillSetDir>,
}

#[derive(Debug)]
struct SpillSetDir {
    dir: PathBuf,
    /// Held locked for the set's lifetime, so a sweep from another process
    /// (or this one) can tell this set is still in use. `None` once dropped,
    /// and on a filesystem that does not support locking.
    lock: Option<File>,
}

impl SpillSet {
    /// Creates a new, empty set under `cache_dir`.
    pub fn create(cache_dir: &Path) -> Result<Self> {
        let io_error = |path: &Path| {
            let path = path.to_path_buf();
            move |source| GlydeError::Io { path, source }
        };
        let root = cache_dir.join(SETS_DIR);
        std::fs::create_dir_all(&root).map_err(io_error(&root))?;

        let since_epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let dir = root.join(format!(
            "{}-{:x}-{}",
            std::process::id(),
            since_epoch.as_nanos(),
            NEXT_SET.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).map_err(io_error(&dir))?;

        let lock_path = dir.join(LOCK_FILE);
        let lock = File::create(&lock_path).map_err(io_error(&lock_path))?;
        let lock = match lock.try_lock() {
            Ok(()) => Some(lock),
            // Deletion on drop does not depend on the lock; only the sweep
            // does, and it never deletes a set it could not lock itself.
            Err(error) => {
                warn!(
                    dir = %dir.display(),
                    error = %error,
                    "could not lock the spill directory; it is still deleted when the file \
                     is closed, but if Glyde exits abnormally it will not be cleaned up \
                     automatically"
                );
                None
            }
        };
        debug!(dir = %dir.display(), "created spill set");
        Ok(Self {
            inner: Arc::new(SpillSetDir { dir, lock }),
        })
    }

    /// The directory this set's files are written to.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }
}

impl Drop for SpillSetDir {
    fn drop(&mut self) {
        // Every mapping and file handle into the directory is already closed
        // (each one holds the set alive), which Windows needs before it lets
        // a file be deleted. Once unlocked a concurrent sweep may delete the
        // directory first, which `remove_set_dir` takes in its stride.
        drop(self.lock.take());
        let dir = std::mem::take(&mut self.dir);
        // The last reference can be dropped on the UI thread (a reopen
        // replaces the dataset there), and deleting gigabytes is I/O the UI
        // thread must never wait on (docs/ARCHITECTURE.md §Hard rules).
        *pending_deletions() += 1;
        let delete = move || {
            remove_set_dir(&dir, "the data it held was closed");
            *pending_deletions() -= 1;
            DELETIONS_FINISHED.notify_all();
        };
        if let Err(error) = std::thread::Builder::new()
            .name("glyde-spill-cleanup".to_string())
            .spawn(delete.clone())
        {
            warn!(error = %error, "could not start the spill cleanup thread; deleting inline");
            delete();
        }
    }
}

/// How many dropped sets a cleanup thread is still deleting, so that
/// [`wait_for_pending_deletions`] can wait for the space they give back.
static PENDING_DELETIONS: Mutex<usize> = Mutex::new(0);
static DELETIONS_FINISHED: Condvar = Condvar::new();

/// The longest [`wait_for_pending_deletions`] waits. Deleting even a large
/// set takes well under a second on any local disk; past this, the caller
/// goes ahead and measures whatever is free by then.
const DELETION_WAIT: Duration = Duration::from_secs(30);

fn pending_deletions() -> MutexGuard<'static, usize> {
    // The count stays meaningful even if a cleanup thread panicked holding
    // the lock: it only ever adds or subtracts one.
    PENDING_DELETIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Blocks until every dropped set's deletion has finished (or
/// [`DELETION_WAIT`] passes), so a free-space check right after a dataset
/// was replaced counts the space it is giving back — the reopen of a file
/// too large for memory on a nearly full disk, above all (issue #118).
/// Returns whether nothing is still pending. Never call it on the UI thread.
pub fn wait_for_pending_deletions() -> bool {
    let (pending, timeout) = DELETIONS_FINISHED
        .wait_timeout_while(pending_deletions(), DELETION_WAIT, |pending| *pending > 0)
        .unwrap_or_else(PoisonError::into_inner);
    if timeout.timed_out() {
        warn!(
            pending = *pending,
            "spill files of a closed file are still being deleted; checking free space without \
             waiting for them"
        );
    }
    *pending == 0
}

/// Deletes one set directory, logging what it freed and why.
fn remove_set_dir(dir: &Path, reason: &str) -> Option<u64> {
    let bytes = directory_bytes(dir);
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {
            info!(
                dir = %dir.display(),
                bytes,
                reason,
                "deleted spill files (issue #118)"
            );
            Some(bytes)
        }
        // A concurrent sweep got there first.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            warn!(
                dir = %dir.display(),
                error = %error,
                "could not delete spill files; the next start of Glyde retries"
            );
            None
        }
    }
}

/// The total size of the files directly inside `dir` (a set directory is
/// flat).
fn directory_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

/// What one [`sweep_orphaned_spill_files`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Spill-set directories left behind by a Glyde that exited abnormally.
    pub orphaned_sets: usize,
    /// Loose `*.glysp`/`*.glysp.tmp` files written by Glyde versions before
    /// issue #118.
    pub legacy_files: usize,
    /// Disk space given back, in bytes.
    pub bytes_freed: u64,
}

/// Deletes every spill file under `cache_dir` that no running Glyde is using
/// (issue #118): a [`SpillSet`] whose owner exited without dropping it (a
/// crash, a forced quit, a power cut — its lock was released by the OS),
/// and the loose `*.glysp`/`*.glysp.tmp` files earlier versions left in
/// `cache_dir` itself, which nothing ever reads back.
///
/// A set still locked by any process, this one included, is never touched,
/// and neither is anything else in `cache_dir` (the Level-0 and pyramid
/// caches are reused by later opens). Never fails: whatever cannot be
/// deleted is logged at `warn` and left for the next sweep.
pub fn sweep_orphaned_spill_files(cache_dir: &Path) -> SweepReport {
    sweep_older_than(cache_dir, SWEEP_MIN_AGE)
}

fn sweep_older_than(cache_dir: &Path, min_age: Duration) -> SweepReport {
    let mut report = SweepReport::default();

    for entry in read_dir_or_empty(&cache_dir.join(SETS_DIR)) {
        let dir = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) || !is_old_enough(&dir, min_age) {
            continue;
        }
        if !set_is_orphaned(&dir) {
            debug!(dir = %dir.display(), "spill set in use by a running Glyde; kept");
            continue;
        }
        if let Some(bytes) =
            remove_set_dir(&dir, "left behind by a Glyde that did not exit cleanly")
        {
            report.orphaned_sets += 1;
            report.bytes_freed += bytes;
        }
    }

    for entry in read_dir_or_empty(cache_dir) {
        let path = entry.path();
        let is_legacy_spill = entry.file_type().is_ok_and(|kind| kind.is_file())
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.ends_with(&format!(".{EXTENSION}"))
                        || name.ends_with(&format!(".{EXTENSION}.tmp"))
                });
        if !is_legacy_spill {
            continue;
        }
        let bytes = entry.metadata().map_or(0, |metadata| metadata.len());
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.legacy_files += 1;
                report.bytes_freed += bytes;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => warn!(
                path = %path.display(),
                error = %error,
                "could not delete a spill file left by an earlier version of Glyde"
            ),
        }
    }

    if report != SweepReport::default() {
        info!(
            cache_dir = %cache_dir.display(),
            orphaned_sets = report.orphaned_sets,
            legacy_files = report.legacy_files,
            bytes_freed = report.bytes_freed,
            "deleted spill files no running Glyde was using (issue #118)"
        );
    }
    report
}

fn read_dir_or_empty(dir: &Path) -> impl Iterator<Item = std::fs::DirEntry> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            warn!(dir = %dir.display(), error = %error, "could not list the spill cache");
            None
        }
    };
    entries.into_iter().flatten().flatten()
}

/// Whether `dir` was last changed at least `min_age` ago. An
/// unreadable time counts as "too young": the sweep keeps what it cannot
/// judge.
fn is_old_enough(dir: &Path, min_age: Duration) -> bool {
    std::fs::metadata(dir)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= min_age)
}

/// Whether no process holds `dir`'s lock — taking the lock is the test.
/// A directory with no lock file at all is an owner that died between
/// creating it and locking it (it is older than [`SWEEP_MIN_AGE`]). Any
/// other doubt keeps the set.
fn set_is_orphaned(dir: &Path) -> bool {
    let lock_path = dir.join(LOCK_FILE);
    match File::open(&lock_path) {
        Ok(lock) => match lock.try_lock() {
            Ok(()) => true,
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Error(error)) => {
                warn!(
                    dir = %dir.display(),
                    error = %error,
                    "could not check whether a spill set is in use; kept"
                );
                false
            }
        },
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// A finished spill file, mapped. Deleted with its [`SpillSet`].
struct SpillFile {
    /// Declared first so it is unmapped before `set` can delete the file.
    mmap: Mmap,
    /// Kept so [`SpillVec::read_chunks`] can read elements back *without*
    /// going through the mapping.
    path: PathBuf,
    _set: SpillSet,
}

/// A memory-mapped, fixed-width typed column spilled to disk.
///
/// Cloning is cheap (the mapping is shared through an [`Arc`]), so a
/// spilled column can be handed to another thread or snapshotted into a
/// message without copying the samples themselves. The file is deleted once
/// every clone is gone (issue #118).
pub struct SpillVec<T> {
    file: Arc<SpillFile>,
    len: usize,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for SpillVec<T> {
    fn clone(&self) -> Self {
        Self {
            file: Arc::clone(&self.file),
            len: self.len,
            marker: PhantomData,
        }
    }
}

/// How much a [`SpillVec::read_chunks`] scan reads at a time. Sized like
/// `ingest::csv`'s streaming read buffer: big enough that the syscall cost per
/// element is negligible, small enough that a scan's footprint is a flat
/// number independent of the column's length.
const READ_CHUNK_BYTES: usize = 1 << 20;

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
        bytemuck::cast_slice(&self.file.mmap[HEADER_LEN..end])
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

    /// Hands `range`'s elements to `visit` in contiguous chunks, in order,
    /// reading them back through a fixed-size buffer instead of through the
    /// mapping (issue #85).
    ///
    /// [`Self::as_slice`] is the right choice for a column a caller walks once
    /// and can afford to make resident. This is for the opposite case: a
    /// *multi-pass* statistic over a column far larger than the peak-RSS cap
    /// (SPEC §5), where scanning the mapping end to end would make every page
    /// of it resident and so cost memory proportional to file size. Reading
    /// through a buffer leaves the pages in the OS page cache, which the cap
    /// does not count and the OS is free to reclaim — SPEC §5.1's "read in
    /// bounded chunks" applied to our own spill files, the same way
    /// `ingest::csv::stream_path` applies it to the user's source file.
    ///
    /// A `range` reaching past the column's end is clamped to it, so a caller
    /// need not special-case the last chunk.
    pub fn read_chunks(
        &self,
        range: Range<usize>,
        visit: &mut dyn FnMut(&[T]) -> Result<()>,
    ) -> Result<()> {
        let start = range.start.min(self.len);
        let end = range.end.min(self.len);
        if start >= end {
            return Ok(());
        }

        let io_error = |source: std::io::Error| GlydeError::Io {
            path: self.file.path.clone(),
            source,
        };
        let mut file = File::open(&self.file.path).map_err(io_error)?;
        let offset = HEADER_LEN + start * size_of::<T>();
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(io_error)?;

        // At least one element per chunk even for a `T` wider than the buffer,
        // which no current caller has but the arithmetic must not assume.
        let chunk_len = (READ_CHUNK_BYTES / size_of::<T>()).max(1);
        let mut buffer = vec![T::zeroed(); chunk_len];
        let mut remaining = end - start;
        while remaining > 0 {
            let take = remaining.min(chunk_len);
            file.read_exact(bytemuck::cast_slice_mut(&mut buffer[..take]))
                .map_err(io_error)?;
            visit(&buffer[..take])?;
            remaining -= take;
        }

        Ok(())
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
    /// Declared after `writer` so the file is closed before an abandoned
    /// writer can be the last reference deleting it.
    set: SpillSet,
    marker: PhantomData<fn() -> T>,
}

/// Buffer every spill writer wraps its file in. Large enough that a
/// row-at-a-time `push` costs one `memcpy` rather than a syscall, small
/// enough that N columns' worth of them is still a flat, file-size-
/// independent amount of memory.
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

impl<T: Pod> SpillVecWriter<T> {
    /// Creates the backing file for `stem` in `set` (as a `.tmp`
    /// sibling, renamed into place atomically by [`Self::finish`] so a
    /// reader can never observe a half-written column) and writes its
    /// header.
    pub fn create(set: &SpillSet, stem: &str) -> Result<Self> {
        let final_path = set.dir().join(format!("{stem}.{EXTENSION}"));
        let tmp_path = set.dir().join(format!("{stem}.{EXTENSION}.tmp"));

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
            set: set.clone(),
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
            file: Arc::new(SpillFile {
                mmap,
                path: self.final_path,
                _set: self.set,
            }),
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
    /// Creates the arena/offset file pair for `stem` in `set`.
    pub fn create(set: &SpillSet, stem: &str) -> Result<Self> {
        Ok(Self {
            arena: SpillVecWriter::create(set, &format!("{stem}.arena"))?,
            ends: SpillVecWriter::create(set, &format!("{stem}.ends"))?,
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

    fn new_set(dir: &tempfile::TempDir) -> SpillSet {
        SpillSet::create(dir.path()).expect("spill set")
    }

    #[test]
    fn round_trips_every_element_width_a_caller_spills() {
        let dir = tempfile::tempdir().expect("temp dir");

        let mut ticks = SpillVecWriter::<i128>::create(&new_set(&dir), "ticks").expect("create");
        for value in [i128::MIN, -1, 0, 1, i128::MAX] {
            ticks.push(value).expect("push");
        }
        let ticks = ticks.finish().expect("finish");
        assert_eq!(ticks.len(), 5);
        assert_eq!(ticks.as_slice(), [i128::MIN, -1, 0, 1, i128::MAX]);

        let mut counts = SpillVecWriter::<i64>::create(&new_set(&dir), "counts").expect("create");
        for value in [i64::MIN, 0, i64::MAX] {
            counts.push(value).expect("push");
        }
        assert_eq!(
            counts.finish().expect("finish").as_slice(),
            [i64::MIN, 0, i64::MAX]
        );

        let mut flags = SpillVecWriter::<u8>::create(&new_set(&dir), "flags").expect("create");
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

        let mut writer = SpillVecWriter::<f64>::create(&new_set(&dir), "samples").expect("create");
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
        let writer = SpillVecWriter::<f64>::create(&new_set(&dir), "empty").expect("create");
        let spilled = writer.finish().expect("finish");

        assert!(spilled.is_empty());
        assert!(spilled.as_slice().is_empty());
        assert_eq!(spilled.get(0), None);
    }

    #[test]
    fn strings_round_trip_including_empty_and_multibyte_fields() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fields = ["running", "", "idle", "°C µm/s²", "état"];

        let mut writer = SpillStringsWriter::create(&new_set(&dir), "state").expect("create");
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
        let mut writer = SpillVecWriter::<f64>::create(&new_set(&dir), "shared").expect("create");
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

    /// Every element `read_chunks` hands back over `range`, flattened, plus the
    /// chunk lengths it used.
    fn read_back<T: Pod>(spilled: &SpillVec<T>, range: Range<usize>) -> (Vec<T>, Vec<usize>) {
        let mut elements = Vec::new();
        let mut chunk_lens = Vec::new();
        spilled
            .read_chunks(range, &mut |chunk| {
                chunk_lens.push(chunk.len());
                elements.extend_from_slice(chunk);
                Ok(())
            })
            .expect("reading back a spill file this test just wrote");
        (elements, chunk_lens)
    }

    // Issue #85: the bounded read path must be a *storage* detail only — it has
    // to yield exactly what the mapping does, chunk boundaries included.
    #[test]
    fn read_chunks_yields_exactly_what_the_mapping_does() {
        let dir = tempfile::tempdir().expect("temp dir");
        // More elements than fit one chunk, so the loop runs several times and
        // the last chunk is a partial one.
        let chunk_len = READ_CHUNK_BYTES / size_of::<i128>();
        let count = chunk_len * 2 + 7;

        let mut writer = SpillVecWriter::<i128>::create(&new_set(&dir), "ticks").expect("create");
        for value in 0..count {
            writer.push(value as i128 * 3 - 5).expect("push");
        }
        let spilled = writer.finish().expect("finish");

        let (elements, chunk_lens) = read_back(&spilled, 0..count);
        assert_eq!(elements, spilled.as_slice());
        assert_eq!(chunk_lens, vec![chunk_len, chunk_len, 7]);
    }

    #[test]
    fn read_chunks_reads_a_sub_range_and_clamps_one_past_the_end() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut writer = SpillVecWriter::<f64>::create(&new_set(&dir), "samples").expect("create");
        for value in [1.5_f64, 2.5, 3.5, 4.5, 5.5] {
            writer.push(value).expect("push");
        }
        let spilled = writer.finish().expect("finish");

        assert_eq!(read_back(&spilled, 1..4).0, vec![2.5, 3.5, 4.5]);
        assert_eq!(read_back(&spilled, 3..99).0, vec![4.5, 5.5]);
        assert!(read_back(&spilled, 2..2).0.is_empty());
        assert!(read_back(&spilled, 99..200).0.is_empty());
    }

    // Golden Rule 1 again, for the read path this time: `as_slice`'s bit-for-bit
    // round trip must hold when the same file is read through a buffer instead.
    #[test]
    fn read_chunks_round_trips_f64_samples_bit_for_bit_including_nan() {
        let dir = tempfile::tempdir().expect("temp dir");
        let samples = [1.5_f64, -0.0, f64::NAN, f64::INFINITY, f64::MIN_POSITIVE];

        let mut writer = SpillVecWriter::<f64>::create(&new_set(&dir), "nan").expect("create");
        for &sample in &samples {
            writer.push(sample).expect("push");
        }
        let spilled = writer.finish().expect("finish");

        assert_eq!(
            read_back(&spilled, 0..samples.len())
                .0
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            samples
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn debug_reports_the_shape_without_printing_the_samples() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut writer = SpillVecWriter::<f64>::create(&new_set(&dir), "debug").expect("create");
        writer.push(12345.678).expect("push");
        let spilled = writer.finish().expect("finish");

        let rendered = format!("{spilled:?}");
        assert!(rendered.contains("len: 1"), "unexpected: {rendered}");
        assert!(
            !rendered.contains("12345"),
            "Debug must never print a spilled column's samples: {rendered}"
        );
    }

    // --- Issue #118: spill files are deleted, never left to fill the disk ---

    /// Waits for the background cleanup thread [`SpillSetDir`]'s `Drop`
    /// hands the deletion to.
    fn eventually_gone(path: &Path) -> bool {
        for _ in 0..500 {
            if !path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn spill_one(set: &SpillSet, stem: &str) -> SpillVec<f64> {
        let mut writer = SpillVecWriter::<f64>::create(set, stem).expect("create");
        writer.extend_from_slice(&[1.0, 2.0, 3.0]).expect("extend");
        writer.finish().expect("finish")
    }

    #[test]
    fn every_set_gets_its_own_directory_under_the_spill_subdirectory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = new_set(&dir);
        let second = new_set(&dir);

        assert_ne!(first.dir(), second.dir());
        for set in [&first, &second] {
            assert_eq!(
                set.dir().parent(),
                Some(dir.path().join(SETS_DIR).as_path())
            );
            assert!(set.dir().join(LOCK_FILE).is_file());
        }
    }

    #[test]
    fn dropping_the_last_column_deletes_the_whole_set() {
        let dir = tempfile::tempdir().expect("temp dir");
        let set = new_set(&dir);
        let set_dir = set.dir().to_path_buf();
        let samples = spill_one(&set, "c0");
        let flags = {
            let mut writer = SpillStringsWriter::create(&set, "c1").expect("create");
            writer.push("on").expect("push");
            writer.finish().expect("finish")
        };
        drop(set);

        let clone = samples.clone();
        drop(samples);
        drop(flags);
        assert!(
            set_dir.join("c0.glysp").is_file(),
            "a clone still maps the column, so its file must survive"
        );
        assert_eq!(clone.as_slice(), [1.0, 2.0, 3.0]);

        drop(clone);
        assert!(
            eventually_gone(&set_dir),
            "the set must be deleted once nothing maps it"
        );
    }

    // An open that fails or is abandoned midway (a superseded load, a full
    // disk) drops its writers unfinished; their `.tmp` files must not stay.
    #[test]
    fn an_abandoned_writer_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let set = new_set(&dir);
        let set_dir = set.dir().to_path_buf();
        let mut writer = SpillVecWriter::<i128>::create(&set, "ts").expect("create");
        writer.push(7).expect("push");
        assert!(set_dir.join("ts.glysp.tmp").is_file());

        drop(set);
        drop(writer);
        assert!(eventually_gone(&set_dir));
    }

    // A reopen's free-space check must see the space the replaced dataset
    // gives back, not race its cleanup thread.
    #[test]
    fn waiting_for_pending_deletions_returns_once_dropped_sets_are_gone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let set = new_set(&dir);
        let set_dir = set.dir().to_path_buf();
        let samples = spill_one(&set, "c0");
        drop(set);
        drop(samples);

        assert!(wait_for_pending_deletions());
        assert!(!set_dir.exists());
    }

    #[test]
    fn the_sweep_keeps_a_set_that_is_still_in_use() {
        let dir = tempfile::tempdir().expect("temp dir");
        let set = new_set(&dir);
        let samples = spill_one(&set, "c0");

        let report = sweep_older_than(dir.path(), Duration::ZERO);

        assert_eq!(report, SweepReport::default());
        assert_eq!(samples.as_slice(), [1.0, 2.0, 3.0]);
        assert!(set.dir().join("c0.glysp").is_file());
    }

    /// A set directory as a Glyde that was killed leaves it: files and an
    /// unlocked lock file, no live owner.
    fn orphaned_set(cache_dir: &Path, name: &str, with_lock_file: bool) -> PathBuf {
        let set_dir = cache_dir.join(SETS_DIR).join(name);
        std::fs::create_dir_all(&set_dir).expect("set dir");
        std::fs::write(set_dir.join("c0.glysp"), [0u8; 100]).expect("finished file");
        std::fs::write(set_dir.join("c1.glysp.tmp"), [0u8; 50]).expect("interrupted file");
        if with_lock_file {
            std::fs::write(set_dir.join(LOCK_FILE), b"").expect("lock file");
        }
        set_dir
    }

    #[test]
    fn the_sweep_deletes_sets_whose_owner_is_gone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let killed = orphaned_set(dir.path(), "1-dead-0", true);
        let killed_before_locking = orphaned_set(dir.path(), "1-dead-1", false);

        let report = sweep_older_than(dir.path(), Duration::ZERO);

        assert_eq!(report.orphaned_sets, 2);
        assert_eq!(report.legacy_files, 0);
        assert_eq!(report.bytes_freed, 2 * 150);
        assert!(!killed.exists());
        assert!(!killed_before_locking.exists());
    }

    #[test]
    fn the_sweep_leaves_a_set_younger_than_its_minimum_age_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let being_created = orphaned_set(dir.path(), "1-new-0", false);

        let report = sweep_orphaned_spill_files(dir.path());

        assert_eq!(report, SweepReport::default());
        assert!(being_created.join("c0.glysp").is_file());
    }

    #[test]
    fn the_sweep_deletes_loose_files_from_earlier_versions_and_nothing_else() {
        let dir = tempfile::tempdir().expect("temp dir");
        let write = |name: &str, len: usize| {
            let path = dir.path().join(name);
            std::fs::write(&path, vec![0u8; len]).expect("write");
            path
        };
        let legacy = [
            write("0123456789abcdef.c0.glysp", 64),
            write("0123456789abcdef.ts.glysp.tmp", 32),
        ];
        let kept = [
            write("0123456789abcdef.ts.glyc0", 16),
            write("0123456789abcdef.pyramid.glypy", 16),
            write("notes.txt", 16),
        ];

        let report = sweep_older_than(dir.path(), Duration::ZERO);

        assert_eq!(report.legacy_files, 2);
        assert_eq!(report.orphaned_sets, 0);
        assert_eq!(report.bytes_freed, 96);
        assert!(legacy.iter().all(|path| !path.exists()));
        assert!(kept.iter().all(|path| path.is_file()));
    }

    #[test]
    fn sweeping_a_cache_directory_that_does_not_exist_is_a_no_op() {
        let dir = tempfile::tempdir().expect("temp dir");
        let report = sweep_orphaned_spill_files(&dir.path().join("never-created"));
        assert_eq!(report, SweepReport::default());
    }
}
