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

//! The single top-level window (docs/SPEC.md §6: "single window, single file
//! at a time"). Renders whatever the background indexer has reported so far
//! and forwards user intent — File→Open and drag-drop — to it. No I/O and no
//! product logic lives here (docs/ARCHITECTURE.md §Hard rule 2): opening a
//! file always goes through [`crate::plumbing`], never a direct read on this
//! thread.
//!
//! This is the M2 "single egui window" + "Time-domain view v1" slice
//! (docs/ROADMAP.md): a file opens off-thread, its [`OpenSummary`] renders as
//! a small text header, and its samples render as a plot via
//! [`crate::views::time`].

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use glyde_core::ingest::{Dataset, OpenSummary};

use crate::plumbing::{spawn_index_job, spawn_open_dialog, IndexingMessage};
use crate::views;

/// What the central panel currently shows, driven by [`IndexingMessage`]s
/// polled from the background indexer thread.
enum Status {
    Idle,
    Loading {
        path: PathBuf,
    },
    Loaded {
        path: PathBuf,
        summary: Box<OpenSummary>,
        dataset: Box<Dataset>,
    },
    Failed {
        path: PathBuf,
        message: String,
    },
}

pub struct GlydeApp {
    status: Status,
    tx: Sender<IndexingMessage>,
    rx: Receiver<IndexingMessage>,
    /// Bumped every time a new open is requested (drag-drop, or a File→Open
    /// click — before the dialog even resolves). Messages tagged with any
    /// other generation are from a request the user has since superseded and
    /// must not overwrite the current status (SPEC §6: single file at a
    /// time; see `crate::plumbing` module docs).
    generation: u64,
}

impl Default for GlydeApp {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            status: Status::Idle,
            tx,
            rx,
            generation: 0,
        }
    }
}

impl GlydeApp {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hands `path` to the background indexer (never reads it on this
    /// thread) and switches the panel to a loading state.
    fn open(&mut self, path: PathBuf) {
        tracing::info!(path = %path.display(), "user requested to open file");
        self.generation += 1;
        self.status = Status::Loading { path: path.clone() };
        spawn_index_job(self.generation, path, self.tx.clone());
    }

    /// Drains every [`IndexingMessage`] currently queued, keeping only the
    /// most recent as the displayed status (SPEC §6: single file at a time —
    /// an in-flight open superseded by a newer one need not be shown).
    /// Messages from a superseded generation are dropped rather than applied
    /// — otherwise a slow file's late result could silently overwrite the
    /// status of a file opened after it.
    fn drain_indexing_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            if message.generation() != self.generation {
                tracing::debug!(
                    generation = message.generation(),
                    current_generation = self.generation,
                    "dropping indexing message from a superseded open request"
                );
                continue;
            }
            self.status = match message {
                IndexingMessage::Started { path, .. } => Status::Loading { path },
                IndexingMessage::Completed {
                    path,
                    summary,
                    dataset,
                    ..
                } => Status::Loaded {
                    path,
                    summary,
                    dataset,
                },
                IndexingMessage::Failed { path, message, .. } => Status::Failed { path, message },
            };
        }
    }
}

impl eframe::App for GlydeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_indexing_messages();

        // SPEC §6 drag-and-drop. Native builds only populate `path` (the web
        // target isn't part of v1 — docs/ARCHITECTURE.md §Workspace layout);
        // only the most recent drop matters (single file at a time).
        let dropped_path = ctx.input(|i| {
            i.raw
                .dropped_files
                .last()
                .and_then(|file| file.path.clone())
        });
        if let Some(path) = dropped_path {
            self.open(path);
        }

        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open…").clicked() {
                        ui.close_menu();
                        self.generation += 1;
                        spawn_open_dialog(self.generation, self.tx.clone());
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match &self.status {
            Status::Idle => {
                ui.centered_and_justified(|ui| {
                    ui.label("Drop a file here, or use File → Open");
                });
            }
            Status::Loading { path } => {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                    ui.label(format!("Opening {}…", path.display()));
                });
                // Nothing else drives repaint while waiting on the indexer
                // thread's channel message, so poll for it explicitly.
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Status::Loaded {
                path,
                summary,
                dataset,
            } => {
                ui.heading(path.display().to_string());
                ui.horizontal(|ui| {
                    ui.label(format!("{} rows", summary.row_count));
                    if summary.skipped_row_count > 0 {
                        ui.label(format!("{} rows skipped", summary.skipped_row_count));
                    }
                    ui.label(format!("sampling: {:?}", summary.sampling_class));
                });
                // SPEC §4.1 / docs/ROADMAP.md M2 "Time-domain view v1".
                views::time::show(ui, dataset);
            }
            Status::Failed { path, message } => {
                ui.colored_label(
                    egui::Color32::RED,
                    format!("Could not open {}: {message}", path.display()),
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyde_core::ingest::{SamplingClass, TimeAxis};
    use glyde_core::series::{Series, SeriesValues};
    use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};

    fn sample_summary() -> Box<OpenSummary> {
        Box::new(OpenSummary {
            encoding: "utf-8".to_string(),
            delimiter: Some(",".to_string()),
            decimal_separator: Some(".".to_string()),
            time_column: Some("timestamp".to_string()),
            timestamp_format: Some("iso8601".to_string()),
            row_count: 1,
            skipped_row_count: 0,
            sampling_class: SamplingClass::Uniform,
            gap_count: 0,
            non_monotonic_count: 0,
            duplicate_timestamp_count: 0,
        })
    }

    fn sample_dataset() -> Box<Dataset> {
        Box::new(Dataset {
            time: TimeAxis::Absolute {
                timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)],
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![Series::new("value", SeriesValues::F64(vec![1.0]))],
        })
    }

    /// The bug the generation guard exists to prevent: file A is slow to
    /// index, the user opens file B before A's background thread reports
    /// back, and A's late `Completed` message must not silently overwrite
    /// B's already-displayed status (SPEC §6: single file at a time).
    #[test]
    fn a_stale_message_from_a_superseded_open_does_not_overwrite_the_current_status() {
        let mut app = GlydeApp::new();
        let path_b = PathBuf::from("b.csv");
        app.generation = 2;
        app.status = Status::Loading {
            path: path_b.clone(),
        };

        app.tx
            .send(IndexingMessage::Completed {
                generation: 1, // file A's generation, superseded by B's (2)
                path: PathBuf::from("a.csv"),
                summary: sample_summary(),
                dataset: sample_dataset(),
            })
            .expect("channel send");

        app.drain_indexing_messages();

        match &app.status {
            Status::Loading { path } => assert_eq!(path, &path_b),
            _ => {
                panic!("a message from a superseded generation must not change the current status")
            }
        }
    }

    /// A message tagged with the current generation must still be applied —
    /// the guard only drops stale ones.
    #[test]
    fn a_current_generation_message_updates_the_status() {
        let mut app = GlydeApp::new();
        app.generation = 1;
        let path = PathBuf::from("a.csv");

        app.tx
            .send(IndexingMessage::Completed {
                generation: 1,
                path: path.clone(),
                summary: sample_summary(),
                dataset: sample_dataset(),
            })
            .expect("channel send");

        app.drain_indexing_messages();

        match &app.status {
            Status::Loaded {
                path: loaded_path, ..
            } => assert_eq!(loaded_path, &path),
            _ => panic!("expected a current-generation message to be applied"),
        }
    }
}
