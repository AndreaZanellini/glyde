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

//! Background indexing (docs/ARCHITECTURE.md §Threading model, §Hard rule 3):
//! the UI thread never performs I/O and never blocks, so opening a file —
//! whether via File→Open or drag-drop — hands the path to a dedicated
//! indexer thread and communicates progress back through an
//! [`std::sync::mpsc`] channel. [`GlydeApp`](crate::app::GlydeApp) only ever
//! polls that channel with `try_recv` from its `update` loop.
//!
//! Every message carries the `generation` of the open request that produced
//! it (assigned by [`GlydeApp`](crate::app::GlydeApp) when the request is
//! made, one higher each time). SPEC §6 is "single file at a time": if the
//! user opens file A, then opens file B before A's indexer thread reports
//! back, A's message must not be allowed to overwrite B's already-displayed
//! state when it eventually arrives late — the generation is how the caller
//! tells a current message from a stale, superseded one.
//!
//! This started as the M2 "single egui window" / "Time-domain view v1"
//! slice: [`spawn_index_job`] wires up the channel plumbing and reuses
//! [`glyde_core::ingest::open_dataset`] (built on the same inference
//! pipeline the torture-corpus gate exercises through `inspect`) to get both
//! the summary and the actual samples the time-domain view plots from a
//! single parse of the file (issue #58 — this used to be two independent
//! calls, `inspect` then `load`, each re-reading and re-decoding the whole
//! file). [`run_index_job`] now calls
//! [`glyde_core::ingest::open_dataset_progressive`] instead, so a large
//! file's plot fills in while indexing continues rather than staying a bare
//! spinner until the whole file has been read (docs/ROADMAP.md M3
//! "Background progressive build emitting partial levels", SPEC §5 "first
//! meaningful plot ... ≤ 2s").

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;

use glyde_core::dsp::decimation::Bucket;
use glyde_core::ingest::{Dataset, InferenceReport, IngestOverrides, Level0Cache, OpenSummary};

/// One numeric column's min/max pyramid, or `None` for a non-numeric column
/// — parallel to `Dataset::columns` (see `glyde_core::ingest::Checkpoint::pyramids`).
type Pyramids = Vec<Option<Vec<Vec<Bucket>>>>;

/// One numeric column's cached raw `(timestamp, value)` pairs, or `None` for
/// a non-numeric column or a cache miss — parallel to `Dataset::columns`,
/// the raw-sample counterpart of [`Pyramids`] (issue #92). `Arc`-wrapped
/// solely so [`IndexingMessage`] stays `Clone` (a memory-mapped
/// [`Level0Cache`] itself is not, since [`memmap2::Mmap`] isn't); every
/// column's cache still has exactly one owner in practice, `Arc::new` here
/// is not for sharing.
type Level0Caches = Vec<Option<Arc<Level0Cache>>>;

/// Progress emitted by a background indexing job, polled by the UI thread.
/// `generation` identifies which open request this message belongs to (see
/// the module docs) — the receiver must ignore any message whose generation
/// isn't the current one.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexingMessage {
    /// The indexer thread started work on `path`.
    Started { generation: u64, path: PathBuf },
    /// A background progress checkpoint (docs/ROADMAP.md M3 "Background
    /// progressive build emitting partial levels"): `dataset` is every
    /// sample read so far, renderable exactly like a [`Self::Completed`]
    /// dataset, just with fewer rows — `views::time::show` needs no special
    /// case for a partial dataset. `rows_read` is how many rows are reflected
    /// in it, for a "N rows so far" progress readout. `pyramids` is that same
    /// checkpoint's own min/max pyramid (issue #80), exact over the rows read
    /// so far — never an approximation — so `views::time::show` can decimate
    /// a still-growing plot exactly like a finished one. `spilled` carries the
    /// storage decision ingestion took before reading a byte (issue #75), so
    /// the loading readout can explain a slow open while it is slow rather
    /// than only in the log (issue #87, SPEC §5.1's "clear explanation").
    Progress {
        generation: u64,
        path: PathBuf,
        dataset: Box<Dataset>,
        pyramids: Pyramids,
        rows_read: u64,
        spilled: bool,
    },
    /// `path` opened successfully; `summary` is what was inferred, `report`
    /// is the same inference surfaced as SPEC §1.2's mandatory UX fields
    /// (docs/ROADMAP.md M4), and `dataset` is every sample, ready for
    /// [`crate::views::time::show`]. `pyramids` is the completed dataset's
    /// own min/max pyramid (issue #80) — `None` per column when `dataset`
    /// came from the spilled storage path (`Dataset::is_spilled`);
    /// `views::time::show` falls back to querying raw samples directly for
    /// the affected columns, which stays bounded to whatever range is
    /// actually on screen. `level0_caches` is the same dataset's raw samples
    /// served from (and written to) the on-disk Level-0 cache (issue #92) —
    /// also `None` per column on the spilled path, for the same reason
    /// `pyramids` is; `views::time::show` prefers it over `dataset`'s own
    /// in-memory column whenever it is `Some`, so a reopened file's deep-zoom
    /// raw-sample view is cache-backed too, not just its pyramid.
    ///
    /// *Reading* a spilled column is bounded as of issue #88
    /// (`ingest::pyramids_for_dataset` streams it), so that is no longer what
    /// keeps both of these off the spilled path. What keeps them off it is
    /// what they would produce: a pyramid costs ~9 bytes per sample per
    /// column held owned in RAM, and a Level-0 cache is a whole second copy
    /// of the column's `(timestamp, value)` pairs — both proportional to file
    /// size, so neither fits SPEC §5's flat cap on the very files that spill
    /// (issue #102). Turning either on is a call for #102 to settle first.
    Completed {
        generation: u64,
        path: PathBuf,
        summary: Box<OpenSummary>,
        report: Box<InferenceReport>,
        dataset: Box<Dataset>,
        pyramids: Pyramids,
        level0_caches: Level0Caches,
    },
    /// `path` failed to open; `message` is the human-readable reason.
    Failed {
        generation: u64,
        path: PathBuf,
        message: String,
    },
}

impl IndexingMessage {
    /// The generation of the open request this message reports on, common
    /// to every variant.
    pub fn generation(&self) -> u64 {
        match self {
            IndexingMessage::Started { generation, .. }
            | IndexingMessage::Progress { generation, .. }
            | IndexingMessage::Completed { generation, .. }
            | IndexingMessage::Failed { generation, .. } => *generation,
        }
    }
}

/// Spawns a background thread that opens `path` and reports the outcome on
/// `tx`, tagged with `generation`. Returns immediately — the caller (the UI
/// thread) never blocks on the file read.
pub fn spawn_index_job(generation: u64, path: PathBuf, tx: Sender<IndexingMessage>) {
    spawn_index_job_with_overrides(generation, path, IngestOverrides::default(), tx)
}

/// [`spawn_index_job`] with [`IngestOverrides`] applied (docs/ROADMAP.md M4
/// "One-click correction of each field → triggers a re-index"):
/// `crate::app::GlydeApp` calls this to re-open the current file after the
/// user corrects a field in the inference bar.
pub fn spawn_index_job_with_overrides(
    generation: u64,
    path: PathBuf,
    overrides: IngestOverrides,
    tx: Sender<IndexingMessage>,
) {
    thread::Builder::new()
        .name("glyde-indexer".to_string())
        .spawn(move || run_index_job(generation, path, overrides, &tx))
        .expect("failed to spawn the background indexer thread");
}

/// Spawns a thread that shows the native "Open file" dialog and, if the user
/// picks a path, opens it exactly like [`spawn_index_job`] — the dialog
/// itself is a blocking call, so it runs off the UI thread too
/// (docs/ARCHITECTURE.md §Hard rule 3). Sends nothing if the user cancels.
///
/// Not unit-tested: `rfd`'s native dialog has no headless backend, so this
/// can only be exercised manually (docs/ROADMAP.md M2's proof for this item).
pub fn spawn_open_dialog(generation: u64, tx: Sender<IndexingMessage>) {
    thread::Builder::new()
        .name("glyde-file-dialog".to_string())
        .spawn(move || {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                run_index_job(generation, path, IngestOverrides::default(), &tx);
            }
        })
        .expect("failed to spawn the file dialog thread");
}

/// The indexer thread's body, split out from [`spawn_index_job`] so tests can
/// run it synchronously against a real corpus fixture without waiting on
/// thread scheduling.
fn run_index_job(
    generation: u64,
    path: PathBuf,
    overrides: IngestOverrides,
    tx: &Sender<IndexingMessage>,
) {
    let _ = tx.send(IndexingMessage::Started {
        generation,
        path: path.clone(),
    });
    tracing::info!(path = %path.display(), "opening file");

    // Check format recognition first so an unsupported extension (e.g. a
    // `.parquet` file — not landed until M7) reports a clear "unrecognized
    // format" error instead of failing deep inside the CSV-specific parser
    // with a confusing message (Golden Rule 2: never guess silently).
    if let Err(err) = glyde_core::ingest::open(&path) {
        tracing::error!(path = %path.display(), error = %err, "failed to open file");
        let _ = tx.send(IndexingMessage::Failed {
            generation,
            path,
            message: err.to_string(),
        });
        return;
    }

    // SPEC §4.1 / docs/ROADMAP.md M2 "Time-domain view v1": one parse
    // produces both the inference summary and the materialized dataset
    // `views::time::show` renders (issue #58). An open that produces a
    // summary but fails to materialize as a dataset still has nothing to
    // show, so it is reported as a failed open rather than a summary with no
    // plot. Since issue #94 that no longer includes a time column whose
    // fields are neither timestamps nor numbers: SPEC §1.3 "never abort the
    // load" means ingestion substitutes a row-ordinal index and reports both
    // time fields low-confidence, so such a file arrives here as an ordinary
    // (if loudly flagged) success with a real plot behind it.
    //
    // docs/ROADMAP.md M3 "Background progressive build emitting partial
    // levels": every checkpoint along the way is forwarded as its own
    // `Progress` message, so the UI thread can render a growing plot instead
    // of only a spinner while a large file is still being read.
    match glyde_core::ingest::open_dataset_progressive_with_overrides(
        &path,
        overrides,
        |checkpoint| {
            let _ = tx.send(IndexingMessage::Progress {
                generation,
                path: path.clone(),
                dataset: Box::new(checkpoint.dataset),
                pyramids: checkpoint.pyramids,
                rows_read: checkpoint.rows_read,
                spilled: checkpoint.spilled,
            });
        },
    ) {
        Ok((summary, report, dataset)) => {
            tracing::info!(
                path = %path.display(),
                row_count = summary.row_count,
                sampling_class = ?summary.sampling_class,
                "file opened"
            );
            // issue #102: a spilled dataset's pyramid is never built from the
            // completed load — see the `Completed` variant's doc comment.
            // issue #81: a non-spilled dataset's pyramid is served from (and
            // written to) the on-disk pyramid cache, so reopening an
            // unchanged file skips rebuilding it.
            // issue #92: same treatment for the raw-sample Level-0 cache —
            // never built over a spilled dataset, served from (and written
            // to) the on-disk cache otherwise.
            let (pyramids, level0_caches) = if dataset.is_spilled() {
                (
                    vec![None; dataset.columns.len()],
                    (0..dataset.columns.len()).map(|_| None).collect(),
                )
            } else {
                let pyramids =
                    glyde_core::ingest::pyramids_for_dataset_cached(&path, &dataset, overrides);
                let level0_caches =
                    glyde_core::ingest::level0_for_dataset_cached(&path, &dataset, overrides)
                        .into_iter()
                        .map(|cache| cache.map(Arc::new))
                        .collect();
                (pyramids, level0_caches)
            };
            let _ = tx.send(IndexingMessage::Completed {
                generation,
                path,
                summary: Box::new(summary),
                report: Box::new(report),
                dataset: Box::new(dataset),
                pyramids,
                level0_caches,
            });
        }
        Err(err) => {
            tracing::error!(path = %path.display(), error = %err, "failed to open file");
            let _ = tx.send(IndexingMessage::Failed {
                generation,
                path,
                message: err.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;

    /// docs/ARCHITECTURE.md §Hard rule 3: opening a file must not block the
    /// caller. Runs the real corpus fixture through the real
    /// `glyde_core::ingest::inspect` pipeline off-thread and asserts the
    /// channel reports Started then Completed with a sane summary, both
    /// tagged with the generation the caller passed in.
    #[test]
    fn spawn_index_job_reports_started_then_completed_for_a_clean_csv() {
        let path = corpus_path("case-01-comma-clean.csv");
        let (tx, rx) = mpsc::channel();

        spawn_index_job(7, path.clone(), tx);

        let started = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Started message");
        assert_eq!(
            started,
            IndexingMessage::Started {
                generation: 7,
                path: path.clone()
            }
        );

        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Completed message")
        {
            IndexingMessage::Completed {
                generation,
                path: completed_path,
                summary,
                report,
                dataset,
                pyramids,
                level0_caches,
            } => {
                assert_eq!(generation, 7);
                assert_eq!(completed_path, path);
                assert_eq!(summary.row_count, 6);
                assert_eq!(summary.skipped_row_count, 0);
                assert_eq!(report.sample_count, 6);
                assert_eq!(dataset.time.len(), 6);
                assert_eq!(dataset.columns.len(), 2);
                assert_eq!(pyramids.len(), 2);
                assert!(
                    pyramids.iter().all(Option::is_some),
                    "an in-memory dataset's completed pyramid must be built for every numeric column"
                );
                assert_eq!(level0_caches.len(), 2);
                assert!(
                    level0_caches.iter().all(Option::is_some),
                    "an in-memory dataset's completed level0 cache must be built for every \
                     numeric column (issue #92)"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// docs/ROADMAP.md M3 "Background progressive build emitting partial
    /// levels": a file large enough to cross `glyde_core::ingest`'s first
    /// progress checkpoint must report one or more `Progress` messages,
    /// each with a growing, real, renderable dataset — between `Started`
    /// and `Completed`, tagged with the same generation as both.
    #[test]
    fn spawn_index_job_reports_progress_for_a_large_csv() {
        // `Registry::find` recognizes files by extension (docs/ARCHITECTURE.md
        // §Two classes of inference), so the fixture needs a real `.csv` name
        // rather than `NamedTempFile`'s default extensionless one.
        let mut file = tempfile::Builder::new()
            .suffix(".csv")
            .tempfile()
            .expect("create temp file");
        let mut text = String::from("index,value\n");
        for i in 0..70_000u64 {
            text.push_str(&format!("{i},{}\n", i as f64 * 0.5));
        }
        std::io::Write::write_all(&mut file, text.as_bytes()).expect("write temp file");
        let path = file.path().to_path_buf();
        let (tx, rx) = mpsc::channel();

        spawn_index_job(3, path.clone(), tx);

        let _started = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Started message");

        let mut progress_rows_read: Vec<u64> = Vec::new();
        loop {
            match rx
                .recv_timeout(Duration::from_secs(30))
                .expect("expected a Progress or Completed message")
            {
                IndexingMessage::Progress {
                    generation,
                    path: progress_path,
                    dataset,
                    pyramids,
                    rows_read,
                    spilled,
                } => {
                    assert_eq!(generation, 3);
                    assert_eq!(progress_path, path);
                    assert_eq!(dataset.time.len(), rows_read as usize);
                    assert_eq!(pyramids.len(), dataset.columns.len());
                    assert!(
                        !spilled,
                        "issue #87: a small fixture fits the RAM budget, so its \
                         checkpoints must not claim the file is being streamed from disk"
                    );
                    progress_rows_read.push(rows_read);
                }
                IndexingMessage::Completed { generation, .. } => {
                    assert_eq!(generation, 3);
                    break;
                }
                other => panic!("expected Progress or Completed, got {other:?}"),
            }
        }

        assert!(
            !progress_rows_read.is_empty(),
            "a 70k-row file must cross at least one progress checkpoint"
        );
        assert!(
            progress_rows_read.windows(2).all(|pair| pair[0] < pair[1]),
            "rows_read must strictly increase across checkpoints: {progress_rows_read:?}"
        );
    }

    /// A path with no registered reader (docs/ARCHITECTURE.md §Two classes
    /// of inference — format recognition is stable, checked up front) must
    /// report `Failed`, never panic the indexer thread.
    #[test]
    fn spawn_index_job_reports_failed_for_an_unrecognized_extension() {
        let path = PathBuf::from("readings.xyz");
        let (tx, rx) = mpsc::channel();

        spawn_index_job(1, path.clone(), tx);

        let _started = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Started message");

        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Failed message")
        {
            IndexingMessage::Failed {
                generation,
                path: failed_path,
                message,
            } => {
                assert_eq!(generation, 1);
                assert_eq!(failed_path, path);
                assert!(
                    message.contains("unrecognized format"),
                    "unexpected message: {message:?}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A recognized extension whose file does not exist must still report
    /// `Failed` (the underlying `Io` error), never panic.
    #[test]
    fn spawn_index_job_reports_failed_for_a_missing_file() {
        let path = PathBuf::from("does-not-exist-glyde-plumbing-test.csv");
        let (tx, rx) = mpsc::channel();

        spawn_index_job(1, path.clone(), tx);

        let _started = rx.recv_timeout(Duration::from_secs(5));

        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Failed message")
        {
            IndexingMessage::Failed {
                path: failed_path, ..
            } => assert_eq!(failed_path, path),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The `generation()` accessor must agree with each variant's own field,
    /// since [`crate::app::GlydeApp`] relies on it to drop stale messages.
    #[test]
    fn generation_accessor_matches_every_variant() {
        let path = PathBuf::from("a.csv");
        assert_eq!(
            IndexingMessage::Started {
                generation: 3,
                path: path.clone()
            }
            .generation(),
            3
        );
        assert_eq!(
            IndexingMessage::Progress {
                generation: 5,
                path: path.clone(),
                dataset: Box::new(
                    glyde_core::ingest::load(&corpus_path("case-01-comma-clean.csv"))
                        .expect("corpus case 1 must load")
                ),
                pyramids: Vec::new(),
                rows_read: 6,
                spilled: false,
            }
            .generation(),
            5
        );
        assert_eq!(
            IndexingMessage::Failed {
                generation: 4,
                path: path.clone(),
                message: "oops".to_string()
            }
            .generation(),
            4
        );
    }

    fn corpus_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/corpus")
            .join(name)
    }
}
