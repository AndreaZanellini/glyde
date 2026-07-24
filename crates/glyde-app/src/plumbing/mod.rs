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
#[derive(Debug, Clone, PartialEq)]
pub enum IndexingMessage {
    /// The indexer thread started work on `path`.
    Started { path: PathBuf },
    /// `path` opened successfully; `summary` is what was inferred.
    Completed {
        path: PathBuf,
        summary: Box<OpenSummary>,
    },
    /// `path` failed to open; `message` is the human-readable reason.
    Failed { path: PathBuf, message: String },
}

/// Spawns a background thread that opens `path` and reports the outcome on
/// `tx`. Returns immediately — the caller (the UI thread) never blocks on
/// the file read.
pub fn spawn_index_job(path: PathBuf, tx: Sender<IndexingMessage>) {
    thread::Builder::new()
        .name("glyde-indexer".to_string())
        .spawn(move || run_index_job(path, &tx))
        .expect("failed to spawn the background indexer thread");
}

/// Spawns a thread that shows the native "Open file" dialog and, if the user
/// picks a path, opens it exactly like [`spawn_index_job`] — the dialog
/// itself is a blocking call, so it runs off the UI thread too
/// (docs/ARCHITECTURE.md §Hard rule 3). Sends nothing if the user cancels.
///
/// Not unit-tested: `rfd`'s native dialog has no headless backend, so this
/// can only be exercised manually (docs/ROADMAP.md M2's proof for this item).
pub fn spawn_open_dialog(tx: Sender<IndexingMessage>) {
    thread::Builder::new()
        .name("glyde-file-dialog".to_string())
        .spawn(move || {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                run_index_job(path, &tx);
            }
        })
        .expect("failed to spawn the file dialog thread");
}

/// The indexer thread's body, split out from [`spawn_index_job`] so tests can
/// run it synchronously against a real corpus fixture without waiting on
/// thread scheduling.
fn run_index_job(path: PathBuf, tx: &Sender<IndexingMessage>) {
    let _ = tx.send(IndexingMessage::Started { path: path.clone() });
    tracing::info!(path = %path.display(), "opening file");

    // Check format recognition first so an unsupported extension (e.g. a
    // `.parquet` file — not landed until M7) reports a clear "unrecognized
    // format" error instead of failing deep inside the CSV-specific parser
    // with a confusing message (Golden Rule 2: never guess silently).
    if let Err(err) = glyde_core::ingest::open(&path) {
        tracing::error!(path = %path.display(), error = %err, "failed to open file");
        let _ = tx.send(IndexingMessage::Failed {
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
                path,
                summary: Box::new(summary),
            });
        }
        Err(err) => {
            tracing::error!(path = %path.display(), error = %err, "failed to open file");
            let _ = tx.send(IndexingMessage::Failed {
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
    /// channel reports Started then Completed with a sane summary.
    #[test]
    fn spawn_index_job_reports_started_then_completed_for_a_clean_csv() {
        let path = corpus_path("case-01-comma-clean.csv");
        let (tx, rx) = mpsc::channel();

        spawn_index_job(path.clone(), tx);

        let started = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Started message");
        assert_eq!(started, IndexingMessage::Started { path: path.clone() });

        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Completed message")
        {
            IndexingMessage::Completed {
                path: completed_path,
                summary,
            } => {
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

        spawn_index_job(path.clone(), tx);

        let _started = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Started message");

        match rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Failed message")
        {
            IndexingMessage::Failed {
                path: failed_path,
                message,
            } => {
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

        spawn_index_job(path.clone(), tx);

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

    fn corpus_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/corpus")
            .join(name)
    }
}
