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
//! This is the M2 "single egui window" slice (docs/ROADMAP.md): it proves a
//! file opens off-thread and shows the resulting [`OpenSummary`] as text.
//! Rendering it as a time plot is the next roadmap item.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use glyde_core::ingest::OpenSummary;

use crate::plumbing::{spawn_index_job, spawn_open_dialog, IndexingMessage};

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
}

impl Default for GlydeApp {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            status: Status::Idle,
            tx,
            rx,
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
        self.status = Status::Loading { path: path.clone() };
        spawn_index_job(path, self.tx.clone());
    }

    /// Drains every [`IndexingMessage`] currently queued, keeping only the
    /// most recent as the displayed status (SPEC §6: single file at a time —
    /// an in-flight open superseded by a newer one need not be shown).
    fn drain_indexing_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.status = match message {
                IndexingMessage::Started { path } => Status::Loading { path },
                IndexingMessage::Completed { path, summary } => Status::Loaded { path, summary },
                IndexingMessage::Failed { path, message } => Status::Failed { path, message },
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
                        spawn_open_dialog(self.tx.clone());
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
            Status::Loaded { path, summary } => {
                ui.heading(path.display().to_string());
                ui.label(format!("{} rows", summary.row_count));
                if summary.skipped_row_count > 0 {
                    ui.label(format!("{} rows skipped", summary.skipped_row_count));
                }
                ui.label(format!("sampling: {:?}", summary.sampling_class));
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
