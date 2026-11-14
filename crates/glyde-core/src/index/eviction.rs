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

//! A size cap for the *reused* caches (issue #213): the Level-0 caches
//! (`*.ts.glyc0`, `*.val.glyc0`, `index::level0`), the pyramid caches
//! (`*.pyramid.glypy`, `index::pyramid`) and the dataset caches
//! (`datasets/*.glyds`, `ingest::dataset_cache`). Each was written once and
//! kept forever, so the cache directory only grew — 7.5 GB after three
//! weeks on the maintainer's machine.
//!
//! **The cap** ([`cache_cap_bytes`]) is a tenth of the disk space the cache
//! could use — what is free plus what it already holds — and never more
//! than [`MAX_CACHE_BYTES`]. Counting what it holds keeps a full cache on a
//! nearly full disk from shrinking its own cap to nothing.
//!
//! **What goes first.** Least recently *used*, not least recently written:
//! every cache hit marks the files it read ([`mark_used`] sets their
//! modification time to now — access times are unreliable, most systems
//! mount with `relatime` or `noatime`). An entry is every file that shares
//! a cache stem (a column's Level-0 values and its pyramid; a file's shared
//! timestamps and its dataset cache), evicted together and dated by its
//! newest file, so a half-evicted entry never lingers. The caches of a file
//! that was edited, moved or deleted are never hit again (their key no
//! longer matches anything), so they age into the front of the line without
//! the cache having to record the source path.
//!
//! **What is never touched.** An entry used in the last
//! [`RECENT_USE_GUARD`] (an open in progress, here or in another running
//! Glyde), a `.tmp` file still being written (younger than
//! [`STALE_TMP_AGE`]; older ones are interrupted writes and always
//! deleted), the spill sets under `spill/` (issue #118's sweep owns those),
//! and any file whose name is not one of ours. Deleting a file another
//! process has memory-mapped is safe on Unix (the mapping keeps it alive)
//! and refused on Windows, where it is logged and left for the next start.
//!
//! Runs once per start, in the background, after the spill sweep
//! (`glyde-app`'s `plumbing::spawn_startup_cache_sweep`). Never fails:
//! whatever cannot be read or deleted is logged and kept.

use std::collections::HashMap;
use std::fs::{DirEntry, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tracing::{debug, info, warn};

/// The most the reused caches may hold, whatever the free space: room for
/// the caches of several multi-gigabyte files (a 10 GB CSV's numeric data is
/// a few GB of Level 0).
pub const MAX_CACHE_BYTES: u64 = 10_000_000_000;

/// The cache may hold at most this share (1/10) of the disk space it could
/// use: what is free plus what it already holds.
pub const DISK_SHARE_DIVISOR: u64 = 10;

/// An entry used more recently than this is never evicted: an open in
/// progress has just written or hit it.
pub const RECENT_USE_GUARD: Duration = Duration::from_secs(60);

/// A `.tmp` file untouched for this long is an interrupted write (a crash or
/// a forced quit mid-write), never read by anything. A live writer touches
/// its file continuously, and no single cache file takes this long.
pub const STALE_TMP_AGE: Duration = Duration::from_secs(60 * 60);

/// The subdirectory `ingest::dataset_cache` writes into.
const DATASETS_DIR: &str = "datasets";

/// The extensions of the reused caches.
const CACHE_EXTENSIONS: [&str; 3] = ["glyc0", "glypy", "glyds"];

/// What one [`enforce_cache_cap`] / [`evict_to_cap`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictionReport {
    /// The cap applied, in bytes.
    pub cap_bytes: u64,
    /// The reused caches' size before anything was deleted, in bytes
    /// (`.tmp` files included).
    pub bytes_before: u64,
    /// Cache entries (all the files of one key) deleted to meet the cap.
    pub evicted_entries: usize,
    /// Interrupted writes (`*.tmp`) deleted.
    pub stale_tmp_files: usize,
    /// Disk space given back, in bytes.
    pub bytes_freed: u64,
}

/// The cap for a cache holding `cache_bytes` on a disk with
/// `available_bytes` free: a tenth of both together, at most
/// [`MAX_CACHE_BYTES`]. Free space unknown: [`MAX_CACHE_BYTES`].
pub fn cache_cap_bytes(cache_bytes: u64, available_bytes: Option<u64>) -> u64 {
    match available_bytes {
        Some(available) => {
            (available.saturating_add(cache_bytes) / DISK_SHARE_DIVISOR).min(MAX_CACHE_BYTES)
        }
        None => MAX_CACHE_BYTES,
    }
}

/// Brings the reused caches under `cache_dir` under [`cache_cap_bytes`] for
/// the disk they are on. See the module docs for what goes first and what is
/// never touched.
pub fn enforce_cache_cap(cache_dir: &Path) -> EvictionReport {
    enforce_with_available(cache_dir, crate::budget::available_disk_bytes)
}

/// [`enforce_cache_cap`] with the free-space probe given, so tests do not
/// depend on the machine's disk.
fn enforce_with_available(
    cache_dir: &Path,
    available_disk_bytes: impl FnOnce(&Path) -> Option<u64>,
) -> EvictionReport {
    let scan = scan(cache_dir);
    let available = available_disk_bytes(cache_dir);
    let cap_bytes = cache_cap_bytes(scan.bytes, available);
    info!(
        cache_dir = %cache_dir.display(),
        cache_bytes = scan.bytes,
        available_bytes = ?available,
        cap_bytes,
        "cache size cap computed: a tenth of free disk plus the cache, at most 10 GB (issue #213)"
    );
    evict(cache_dir, scan, cap_bytes)
}

/// Brings the reused caches under `cache_dir` under `cap_bytes`, least
/// recently used first. [`enforce_cache_cap`] with the cap given.
pub fn evict_to_cap(cache_dir: &Path, cap_bytes: u64) -> EvictionReport {
    evict(cache_dir, scan(cache_dir), cap_bytes)
}

/// Records that the cache file at `path` was just used, so eviction keeps it
/// over entries not used since. Called on every cache hit. A failure (the
/// file vanished, a read-only cache) only makes the entry look older than it
/// is: logged at `debug`, never an error.
pub(crate) fn mark_used(path: &Path) {
    let result = OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()));
    if let Err(error) = result {
        debug!(path = %path.display(), error = %error, "could not mark a cache file as used");
    }
}

/// One cache file found by [`scan`].
struct CacheFile {
    path: PathBuf,
    bytes: u64,
    /// `None` when unreadable: treated as "just used", so it is kept.
    modified: Option<SystemTime>,
}

/// Every file of one cache stem.
struct Entry {
    stem: String,
    files: Vec<CacheFile>,
}

impl Entry {
    fn bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }

    /// When the entry was last used: its newest file. `None` if any file's
    /// time is unreadable.
    fn last_used(&self) -> Option<SystemTime> {
        self.files
            .iter()
            .map(|file| file.modified)
            .try_fold(SystemTime::UNIX_EPOCH, |newest, modified| {
                modified.map(|modified| newest.max(modified))
            })
    }
}

#[derive(Default)]
struct Scan {
    entries: Vec<Entry>,
    tmp_files: Vec<CacheFile>,
    /// Everything above together.
    bytes: u64,
}

fn scan(cache_dir: &Path) -> Scan {
    let mut by_stem: HashMap<String, Vec<CacheFile>> = HashMap::new();
    let mut scan = Scan::default();
    for dir in [cache_dir.to_path_buf(), cache_dir.join(DATASETS_DIR)] {
        for entry in read_dir_or_empty(&dir) {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let kind = classify(&name);
            if kind == Kind::Other || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let metadata = entry.metadata().ok();
            let file = CacheFile {
                path: entry.path(),
                bytes: metadata.as_ref().map_or(0, |metadata| metadata.len()),
                modified: metadata.and_then(|metadata| metadata.modified().ok()),
            };
            scan.bytes += file.bytes;
            match kind {
                Kind::Tmp => scan.tmp_files.push(file),
                Kind::Cache => by_stem
                    .entry(stem(&name).to_owned())
                    .or_default()
                    .push(file),
                Kind::Other => {}
            }
        }
    }
    scan.entries = by_stem
        .into_iter()
        .map(|(stem, files)| Entry { stem, files })
        .collect();
    scan
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Cache,
    Tmp,
    Other,
}

/// Whether `name` is one of the reused caches, a `.tmp` one of their writers
/// left (`x.val.glyc0.<pid>-<n>.tmp`, `x.pyramid.glypy.tmp`,
/// `x.glyds.<pid>.tmp`), or something else.
fn classify(name: &str) -> Kind {
    let mut parts = name.split('.');
    // The first part is the stem, never an extension.
    parts.next();
    let parts: Vec<&str> = parts.collect();
    let has_cache_extension =
        |parts: &[&str]| parts.iter().any(|part| CACHE_EXTENSIONS.contains(part));
    match parts.last() {
        Some(last) if CACHE_EXTENSIONS.contains(last) => Kind::Cache,
        Some(&"tmp") if has_cache_extension(&parts) => Kind::Tmp,
        _ => Kind::Other,
    }
}

/// The cache stem: the file name up to its first dot.
fn stem(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

fn evict(cache_dir: &Path, scan: Scan, cap_bytes: u64) -> EvictionReport {
    let mut report = EvictionReport {
        cap_bytes,
        bytes_before: scan.bytes,
        ..EvictionReport::default()
    };
    let mut remaining = scan.bytes;

    for file in &scan.tmp_files {
        if !is_older_than(file.modified, STALE_TMP_AGE) {
            continue;
        }
        if remove(&file.path, "interrupted cache write") {
            report.stale_tmp_files += 1;
            report.bytes_freed += file.bytes;
            remaining = remaining.saturating_sub(file.bytes);
        }
    }

    let mut entries = scan.entries;
    // Least recently used first; an unreadable time sorts last (kept).
    entries.sort_by_key(|entry| entry.last_used().unwrap_or(SystemTime::now()));
    for entry in &entries {
        if remaining <= cap_bytes {
            break;
        }
        if !is_older_than(entry.last_used(), RECENT_USE_GUARD) || used_since_scan(entry) {
            debug!(stem = %entry.stem, "cache entry in use; kept although over the cap");
            continue;
        }
        let mut freed = 0;
        for file in &entry.files {
            if remove(&file.path, "least recently used cache entry over the cap") {
                freed += file.bytes;
            }
        }
        debug!(
            stem = %entry.stem,
            bytes = entry.bytes(),
            last_used = ?entry.last_used(),
            "evicted a least recently used cache entry (issue #213)"
        );
        report.evicted_entries += 1;
        report.bytes_freed += freed;
        remaining = remaining.saturating_sub(freed);
    }

    if remaining > cap_bytes {
        warn!(
            cache_dir = %cache_dir.display(),
            remaining_bytes = remaining,
            cap_bytes,
            "the cache is still over its cap: what is left is in use or could not be deleted"
        );
    }
    if report.evicted_entries > 0 || report.stale_tmp_files > 0 {
        info!(
            cache_dir = %cache_dir.display(),
            cap_bytes,
            bytes_before = report.bytes_before,
            evicted_entries = report.evicted_entries,
            stale_tmp_files = report.stale_tmp_files,
            bytes_freed = report.bytes_freed,
            "cache trimmed to its size cap, least recently used first (issue #213)"
        );
    }
    report
}

/// Whether `modified` is at least `age` ago. An unreadable time, or one in
/// the future, counts as "too young": what cannot be judged is kept.
fn is_older_than(modified: Option<SystemTime>, age: Duration) -> bool {
    modified
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed >= age)
}

/// Whether an open hit one of `entry`'s files between the scan and now —
/// checked again just before deleting, since a file opened at startup can
/// race the sweep.
fn used_since_scan(entry: &Entry) -> bool {
    entry.files.iter().any(|file| {
        match std::fs::metadata(&file.path) {
            Ok(metadata) => !is_older_than(metadata.modified().ok(), RECENT_USE_GUARD),
            // Already gone: nothing to keep.
            Err(_) => false,
        }
    })
}

/// Deletes `path`; `true` if it is gone. A file already gone counts as not
/// deleted by us; any other failure is logged at `warn` and kept.
fn remove(path: &Path, why: &str) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                why,
                "could not delete a cache file; kept for the next start"
            );
            false
        }
    }
}

fn read_dir_or_empty(dir: &Path) -> impl Iterator<Item = DirEntry> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            warn!(dir = %dir.display(), error = %error, "could not list the cache directory");
            None
        }
    };
    entries.into_iter().flatten().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::path::PathBuf;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// Writes `len` bytes at `dir/name`, last modified `age` ago.
    fn write_aged(dir: &Path, name: &str, len: usize, age: Duration) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create dir");
        }
        std::fs::write(&path, vec![0u8; len]).expect("write");
        set_age(&path, age);
        path
    }

    fn set_age(path: &Path, age: Duration) {
        OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(SystemTime::now() - age)
            .expect("set mtime");
    }

    fn age_of(path: &Path) -> Duration {
        File::open(path)
            .and_then(|file| file.metadata())
            .and_then(|metadata| metadata.modified())
            .expect("mtime")
            .elapsed()
            .unwrap_or_default()
    }

    #[test]
    fn the_cap_is_a_tenth_of_the_usable_disk_space_up_to_ten_gigabytes() {
        // 13 GB free with 7.5 GB already cached (the maintainer's machine in
        // issue #213): a tenth of 20.5 GB.
        assert_eq!(
            cache_cap_bytes(7_500_000_000, Some(13_000_000_000)),
            2_050_000_000
        );
        // Plenty of disk: the absolute ceiling.
        assert_eq!(cache_cap_bytes(0, Some(1_000_000_000_000)), MAX_CACHE_BYTES);
        // Free space unknown: the absolute ceiling, never "no cap".
        assert_eq!(cache_cap_bytes(5, None), MAX_CACHE_BYTES);
        // No overflow at the extremes.
        assert_eq!(cache_cap_bytes(u64::MAX, Some(u64::MAX)), MAX_CACHE_BYTES);
    }

    #[test]
    fn a_cache_under_its_cap_is_left_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let old = write_aged(dir.path(), "aaaaaaaaaaaaaaaa.val.glyc0", 100, 30 * DAY);

        let report = evict_to_cap(dir.path(), 1_000);

        assert_eq!(report.evicted_entries, 0);
        assert_eq!(report.bytes_before, 100);
        assert_eq!(report.bytes_freed, 0);
        assert!(old.exists());
    }

    #[test]
    fn the_least_recently_used_entries_go_first_until_the_cache_fits() {
        let dir = tempfile::tempdir().expect("temp dir");
        let oldest = write_aged(dir.path(), "1111111111111111.val.glyc0", 400, 3 * DAY);
        let middle = write_aged(dir.path(), "2222222222222222.val.glyc0", 400, 2 * DAY);
        let newest = write_aged(dir.path(), "3333333333333333.val.glyc0", 400, DAY);

        let report = evict_to_cap(dir.path(), 900);

        assert_eq!(report.bytes_before, 1_200);
        assert_eq!(report.evicted_entries, 1);
        assert_eq!(report.bytes_freed, 400);
        assert!(!oldest.exists(), "the least recently used entry is evicted");
        assert!(
            middle.exists() && newest.exists(),
            "the cache fits after one eviction"
        );
    }

    #[test]
    fn all_files_of_one_key_are_evicted_together_and_dated_by_the_newest() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Key A: its Level-0 values are old, but its pyramid was used
        // yesterday, so the entry as a whole was used yesterday.
        let a_values = write_aged(dir.path(), "aaaaaaaaaaaaaaaa.val.glyc0", 300, 10 * DAY);
        let a_pyramid = write_aged(dir.path(), "aaaaaaaaaaaaaaaa.pyramid.glypy", 300, DAY);
        // Key B: both files used two days ago.
        let b_values = write_aged(dir.path(), "bbbbbbbbbbbbbbbb.val.glyc0", 300, 2 * DAY);
        let b_pyramid = write_aged(dir.path(), "bbbbbbbbbbbbbbbb.pyramid.glypy", 300, 2 * DAY);

        let report = evict_to_cap(dir.path(), 700);

        assert_eq!(report.evicted_entries, 1);
        assert_eq!(report.bytes_freed, 600);
        assert!(
            !b_values.exists() && !b_pyramid.exists(),
            "B goes, both files"
        );
        assert!(
            a_values.exists() && a_pyramid.exists(),
            "A stays, both files"
        );
    }

    #[test]
    fn dataset_caches_and_shared_timestamps_count_toward_the_cap() {
        let dir = tempfile::tempdir().expect("temp dir");
        let timestamps = write_aged(dir.path(), "cccccccccccccccc.ts.glyc0", 500, 5 * DAY);
        let dataset = write_aged(dir.path(), "datasets/cccccccccccccccc.glyds", 500, 5 * DAY);
        let kept = write_aged(dir.path(), "dddddddddddddddd.val.glyc0", 100, DAY);

        let report = evict_to_cap(dir.path(), 200);

        assert_eq!(report.bytes_before, 1_100);
        assert_eq!(report.evicted_entries, 1);
        assert!(!timestamps.exists() && !dataset.exists());
        assert!(kept.exists());
    }

    #[test]
    fn an_entry_in_use_right_now_is_never_evicted_even_over_the_cap() {
        let dir = tempfile::tempdir().expect("temp dir");
        let just_written = write_aged(
            dir.path(),
            "eeeeeeeeeeeeeeee.val.glyc0",
            500,
            Duration::ZERO,
        );
        let old = write_aged(dir.path(), "ffffffffffffffff.val.glyc0", 500, DAY);

        let report = evict_to_cap(dir.path(), 0);

        assert_eq!(report.evicted_entries, 1);
        assert!(!old.exists());
        assert!(
            just_written.exists(),
            "an open in progress keeps its caches"
        );
    }

    #[test]
    fn interrupted_writes_are_deleted_but_writes_in_progress_are_not() {
        let dir = tempfile::tempdir().expect("temp dir");
        // The three `.tmp` spellings the cache writers use.
        let stale = [
            write_aged(dir.path(), "1111111111111111.val.glyc0.42-0.tmp", 10, DAY),
            write_aged(dir.path(), "2222222222222222.pyramid.glypy.tmp", 10, DAY),
            write_aged(
                dir.path(),
                "datasets/3333333333333333.glyds.42.tmp",
                10,
                DAY,
            ),
        ];
        let in_progress = write_aged(
            dir.path(),
            "4444444444444444.ts.glyc0.42-1.tmp",
            10,
            Duration::ZERO,
        );

        let report = evict_to_cap(dir.path(), MAX_CACHE_BYTES);

        assert_eq!(report.stale_tmp_files, 3);
        assert_eq!(report.evicted_entries, 0);
        assert_eq!(report.bytes_freed, 30);
        assert!(stale.iter().all(|path| !path.exists()));
        assert!(in_progress.exists());
    }

    #[test]
    fn files_that_are_not_reused_caches_are_never_touched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let unrelated = [
            write_aged(dir.path(), "notes.txt", 500, 30 * DAY),
            // Spill files are #118's sweep's business, not this one's.
            write_aged(dir.path(), "spill/123-0/c0.glysp", 500, 30 * DAY),
            write_aged(dir.path(), "0123456789abcdef.c0.glysp", 500, 30 * DAY),
        ];

        let report = evict_to_cap(dir.path(), 0);

        assert_eq!(
            report,
            EvictionReport {
                cap_bytes: 0,
                ..Default::default()
            }
        );
        assert!(unrelated.iter().all(|path| path.exists()));
    }

    #[test]
    fn a_missing_cache_directory_is_an_empty_report_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let report = enforce_cache_cap(&dir.path().join("never-created"));
        assert_eq!(report.bytes_before, 0);
        assert_eq!(report.evicted_entries, 0);
    }

    #[test]
    fn enforce_cache_cap_applies_the_disk_based_cap() {
        let dir = tempfile::tempdir().expect("temp dir");
        let old = write_aged(dir.path(), "aaaaaaaaaaaaaaaa.val.glyc0", 300, 2 * DAY);
        let new = write_aged(dir.path(), "bbbbbbbbbbbbbbbb.val.glyc0", 300, DAY);

        // 4.4 kB free + 600 B cached: the cap is 500 B, one entry must go.
        let report = enforce_with_available(dir.path(), |_| Some(4_400));

        assert_eq!(report.cap_bytes, 500);
        assert_eq!(report.bytes_before, 600);
        assert_eq!(report.evicted_entries, 1);
        assert!(!old.exists() && new.exists());

        // The real probe runs and never panics, whatever the machine.
        let real = enforce_cache_cap(dir.path());
        assert!(real.cap_bytes <= MAX_CACHE_BYTES);
    }

    #[test]
    fn marking_a_cache_file_used_makes_it_the_most_recent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_aged(dir.path(), "aaaaaaaaaaaaaaaa.val.glyc0", 100, 30 * DAY);

        mark_used(&path);

        assert!(age_of(&path) < RECENT_USE_GUARD);
    }

    #[test]
    fn marking_a_missing_file_used_does_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("gone.val.glyc0");
        mark_used(&path);
        assert!(!path.exists(), "marking must never create a cache file");
    }
}
