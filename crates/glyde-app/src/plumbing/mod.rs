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
//! This is deliberately the M2 "single egui window" slice, not the M3 index
//! pyramid: [`spawn_index_job`] wires up the channel plumbing and reuses
//! [`glyde_core::ingest::inspect`], the same pipeline the torture-corpus gate
//! already exercises. Streaming a full pyramid build in the background is
//! M3's job.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;

use glyde_core::ingest::OpenSummary;

/// Progress emitted by a background indexing job, polled by the UI thread.
/// `generation` identifies which open request this message belongs to (see
/// the module docs) — the receiver must ignore any message whose generation
/// isn't the current one.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexingMessage {
    /// The indexer thread started work on `path`.
    Started { generation: u64, path: PathBuf },
    /// `path` opened successfully; `summary` is what was inferred.
    Completed {
        generation: u64,
        path: PathBuf,
        summary: Box<OpenSummary>,
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
            | IndexingMessage::Completed { generation, .. }
            | IndexingMessage::Failed { generation, .. } => *generation,
        }
    }
}

/// Spawns a background thread that opens `path` and reports the outcome on
/// `tx`, tagged with `generation`. Returns immediately — the caller (the UI
/// thread) never blocks on the file read.
pub fn spawn_index_job(generation: u64, path: PathBuf, tx: Sender<IndexingMessage>) {
    thread::Builder::new()
        .name("glyde-indexer".to_string())
        .spawn(move || run_index_job(generation, path, &tx))
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
                run_index_job(generation, path, &tx);
            }
        })
        .expect("failed to spawn the file dialog thread");
}

/// The indexer thread's body, split out from [`spawn_index_job`] so tests can
/// run it synchronously against a real corpus fixture without waiting on
/// thread scheduling.
fn run_index_job(generation: u64, path: PathBuf, tx: &Sender<IndexingMessage>) {
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

    match glyde_core::ingest::inspect(&path) {
        Ok(summary) => {
            tracing::info!(
                path = %path.display(),
                row_count = summary.row_count,
                sampling_class = ?summary.sampling_class,
                "file opened"
            );
            let _ = tx.send(IndexingMessage::Completed {
                generation,
                path,
                summary: Box::new(summary),
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
            } => {
                assert_eq!(generation, 7);
                assert_eq!(completed_path, path);
                assert_eq!(summary.row_count, 6);
                assert_eq!(summary.skipped_row_count, 0);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
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
