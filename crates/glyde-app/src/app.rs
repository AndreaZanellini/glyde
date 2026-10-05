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
//! (docs/ROADMAP.md): a file opens off-thread, its inference report renders
//! in the inference bar, and its samples render as a plot via
//! [`crate::views::time`].

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use glyde_core::dsp::decimation::Bucket;
use glyde_core::ingest::{Dataset, InferenceReport, IngestOverrides, Level0Cache};

use crate::inference_bar::Correction;
use crate::plumbing::{
    spawn_index_job, spawn_index_job_with_overrides, spawn_open_dialog, IndexingMessage,
    PreparedView, PROGRESS_BOOL_BAND_ROWS,
};
use crate::views::psd::{selection_rows, PsdPanel, PsdRequest};
use crate::{inference_bar, views};

/// One numeric column's min/max pyramid, or `None` for a non-numeric column
/// — parallel to `Dataset::columns` (see `glyde_core::ingest::Checkpoint::pyramids`).
type Pyramids = Vec<Option<Vec<Vec<Bucket>>>>;

/// See `crate::plumbing::IndexingMessage::Completed`'s doc comment (issue
/// #92) — the raw-sample counterpart of [`Pyramids`], only ever populated
/// for a completed, non-spilled load.
type Level0Caches = Vec<Option<Arc<Level0Cache>>>;

/// The most recent background progress checkpoint for a file still being
/// indexed (docs/ROADMAP.md M3 "Background progressive build emitting
/// partial levels"): a real, renderable [`Dataset`] with fewer rows than the
/// final one, plus how many rows it reflects and that checkpoint's own
/// min/max pyramid (docs/ROADMAP.md M3, issue #80). `ticks` is
/// `dataset.time`'s own pyramid ticks, and `sample_cache` is every non-`f64`
/// numeric column's converted samples (`views::time::cache_column_samples`),
/// both computed once here rather than by [`views::time::show`] on every
/// frame: `TimeAxis::to_pyramid_ticks` and `SeriesValues::to_f64_vec` each
/// materialize a fresh `Vec` over every sample for an in-memory dataset, so
/// computing either per frame instead of per status change is exactly the
/// unconditional-per-frame-O(n) mistake issue #80's own frame-time bench
/// (`crates/glyde-app/benches/time_view_render.rs`) was written to catch —
/// `sample_cache` specifically was missed in that PR's first version (an
/// `f64`-only bench fixture didn't exercise it) and added after review.
struct PartialLoad {
    dataset: Box<Dataset>,
    pyramids: Pyramids,
    /// `ticks`, `sample_cache`, the boolean bands and the cursor lookup,
    /// derived once per checkpoint on the indexer thread (issue #114) —
    /// never here on the UI thread, and never per frame.
    prepared: Box<PreparedView>,
    rows_read: u64,
    /// Issue #87: whether ingestion chose the on-disk spill path for this
    /// file (SPEC §5.1). Drives [`loading_label`]'s explanation — the two
    /// things the user notices about such an open (it is slower, and the plot
    /// stops filling in past the preview cap) are consequences of a decision
    /// Glyde took for them, so they are stated rather than left to look like
    /// Glyde struggling.
    spilled: bool,
}

/// What the central panel currently shows, driven by [`IndexingMessage`]s
/// polled from the background indexer thread.
enum Status {
    Idle,
    Loading {
        path: PathBuf,
        /// `Some` once at least one [`IndexingMessage::Progress`] has
        /// arrived for the current open — SPEC §5 "first meaningful plot ...
        /// render what is indexed, keep indexing in background".
        partial: Option<PartialLoad>,
    },
    Loaded {
        path: PathBuf,
        report: Box<InferenceReport>,
        /// Shared with the background PSD job (`crate::plumbing::psd`),
        /// which reads its raw samples off the UI thread.
        dataset: Arc<Dataset>,
        pyramids: Pyramids,
        /// See [`PartialLoad::prepared`]: the same once-per-status-change
        /// derivations, for the completed dataset.
        prepared: Box<PreparedView>,
        /// See `crate::plumbing::IndexingMessage::Completed`'s doc comment
        /// (issue #92) — `views::time::show` prefers this over `dataset`'s
        /// own column, and over `sample_cache`, whenever an entry is `Some`.
        level0_caches: Level0Caches,
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
    /// SPEC §1.2 "[each field is] correctable in one click; correcting
    /// triggers a re-index" (docs/ROADMAP.md M4): every correction the user
    /// has made to the file currently open, applied on top of automatic
    /// inference the next time it is (re-)indexed. Reset to
    /// [`IngestOverrides::default`] whenever a *different* file is opened —
    /// a correction never follows from one file to the next.
    overrides: IngestOverrides,
    /// SPEC §4.2's PSD view for the file currently loaded; replaced with a
    /// fresh one whenever a load completes, so nothing from a previous file
    /// (or a previous reading of this one) is ever shown.
    psd: PsdPanel,
    /// The rows the time view showed last frame — the selection that drives
    /// the PSD view (SPEC §4.2).
    time_selection: Option<std::ops::Range<usize>>,
    /// A horizontal span, in plot seconds, the time view should move to on
    /// its next frame (the PSD view's "analyze the largest uniform stretch").
    time_focus: Option<(f64, f64)>,
}

impl Default for GlydeApp {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            status: Status::Idle,
            tx,
            rx,
            generation: 0,
            overrides: IngestOverrides::default(),
            psd: PsdPanel::default(),
            time_selection: None,
            time_focus: None,
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
        self.overrides = IngestOverrides::default();
        self.status = Status::Loading {
            path: path.clone(),
            partial: None,
        };
        spawn_index_job(self.generation, path, self.tx.clone());
    }

    /// A one-click field correction from the inference bar (docs/ROADMAP.md
    /// M4, SPEC §1.2): folds `correction` into the running set of overrides
    /// for the current file and re-indexes it from scratch under them —
    /// exactly [`Self::open`]'s job, minus resetting `overrides` itself and
    /// starting from a fresh [`Status::Loading`] with nothing carried over,
    /// same as any other new open (partial progress from the pre-correction
    /// read would be stale under the new reading).
    fn apply_correction(&mut self, path: PathBuf, correction: Correction) {
        match correction {
            Correction::Delimiter(delimiter) => {
                self.overrides.delimiter = Some(delimiter);
                // A new delimiter re-splits every row: a picked column
                // position would now name a different column, if any.
                self.overrides.time_column = None;
            }
            Correction::DecimalSeparator(separator) => {
                self.overrides.decimal_separator = Some(separator)
            }
            Correction::TimestampFormat(format) => self.overrides.timestamp_format = Some(format),
            Correction::SortByTime => self.overrides.sort_by_time = true,
            Correction::TimeColumn(choice) => {
                self.overrides.time_column = Some(choice);
                // Both belonged to the previous time column: its timestamp
                // format would misparse (or fail on) the new one, and sorting
                // by it would scramble the new axis.
                self.overrides.timestamp_format = None;
                self.overrides.sort_by_time = false;
            }
        }
        tracing::info!(
            path = %path.display(),
            overrides = ?self.overrides,
            "user corrected an inferred field; re-indexing"
        );
        self.generation += 1;
        self.status = Status::Loading {
            path: path.clone(),
            partial: None,
        };
        spawn_index_job_with_overrides(self.generation, path, self.overrides, self.tx.clone());
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
            if matches!(message, IndexingMessage::Completed { .. }) {
                self.psd = PsdPanel::default();
                self.time_selection = None;
                self.time_focus = None;
            }
            self.status = match message {
                IndexingMessage::Started { path, .. } => Status::Loading {
                    path,
                    partial: None,
                },
                IndexingMessage::Progress {
                    path,
                    dataset,
                    pyramids,
                    rows_read,
                    spilled,
                    prepared,
                    ..
                } => Status::Loading {
                    path,
                    partial: Some(PartialLoad {
                        dataset,
                        pyramids,
                        prepared,
                        rows_read,
                        spilled,
                    }),
                },
                IndexingMessage::Completed {
                    path,
                    report,
                    dataset,
                    pyramids,
                    level0_caches,
                    prepared,
                    ..
                } => Status::Loaded {
                    path,
                    report,
                    dataset: Arc::from(dataset),
                    pyramids,
                    prepared,
                    level0_caches,
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

        let mut pending_correction: Option<(PathBuf, Correction)> = None;

        // SPEC §4.2: the PSD view, in a resizable panel under the time view.
        // It computes only when the user presses "Compute PSD", on the
        // interval the time view showed last frame.
        if let Status::Loaded {
            dataset, prepared, ..
        } = &self.status
        {
            if self.psd.poll() {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            let request = egui::TopBottomPanel::bottom("psd_view")
                .resizable(true)
                .default_height(320.0)
                .show(ctx, |ui| {
                    self.psd.show(ui, dataset, self.time_selection.clone())
                })
                .inner;
            if let Some(PsdRequest::ShowRows(range)) = request {
                let ticks = &prepared.ticks;
                if let (Some(&first), Some(&last)) = (
                    ticks.get(range.start),
                    range.end.checked_sub(1).and_then(|last| ticks.get(last)),
                ) {
                    self.time_focus = Some((
                        views::time::tick_to_seconds(&dataset.time, first),
                        views::time::tick_to_seconds(&dataset.time, last),
                    ));
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| match &self.status {
            Status::Idle => {
                ui.centered_and_justified(|ui| {
                    ui.label("Drop a file here, or use File → Open");
                });
            }
            Status::Loading { path, partial } => {
                match partial {
                    // SPEC §5 "first meaningful plot ... render what is
                    // indexed, keep indexing in background": a background
                    // progress checkpoint has already arrived, so render its
                    // dataset like a normal (if still-growing) plot instead
                    // of a bare spinner.
                    Some(partial) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(loading_label(path, partial.rows_read, partial.spilled));
                        });
                        views::time::show(
                            ui,
                            &partial.dataset,
                            &partial.pyramids,
                            &partial.prepared.ticks,
                            &partial.prepared.sample_cache,
                            // A still-growing checkpoint has no level0 cache
                            // of its own (issue #92's doc comment on
                            // `IndexingMessage::Completed`) — it renders
                            // straight from the in-memory dataset like it
                            // always has, same as a `None` entry would.
                            &[],
                            partial.prepared.cursor,
                        );
                        let ticks = &partial.prepared.ticks;
                        views::state_timeline::show(
                            ui,
                            &partial.dataset,
                            &ticks[..ticks.len().min(PROGRESS_BOOL_BAND_ROWS)],
                            &partial.prepared.bool_bands,
                        );
                    }
                    None => {
                        ui.centered_and_justified(|ui| {
                            ui.spinner();
                            ui.label(format!("Opening {}…", path.display()));
                        });
                    }
                }
                // Nothing else drives repaint while waiting on the indexer
                // thread's channel message, so poll for it explicitly.
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Status::Loaded {
                path,
                report,
                dataset,
                pyramids,
                prepared,
                level0_caches,
            } => {
                ui.heading(path.display().to_string());
                // SPEC §1.2 mandatory UX / docs/ROADMAP.md M4 "Inference bar
                // widget: persistent and discreet; opens expanded when any
                // inference is low-confidence; each field correctable in one
                // click, correcting triggers a re-index".
                if let Some(correction) =
                    inference_bar::show(ui, report, path, !dataset.is_spilled())
                {
                    pending_correction = Some((path.clone(), correction));
                }
                // SPEC §4.1 / docs/ROADMAP.md M2 "Time-domain view v1"; SPEC
                // §3.1 decimation via `pyramids` (docs/ROADMAP.md M3, issue #80).
                // Its visible interval is the PSD view's selection (SPEC §4.2).
                let visible = views::time::show_with_selection(
                    ui,
                    dataset,
                    pyramids,
                    &prepared.ticks,
                    &prepared.sample_cache,
                    level0_caches,
                    prepared.cursor,
                    self.time_focus.take(),
                );
                self.time_selection = Some(selection_rows(
                    &prepared.ticks,
                    prepared.cursor.is_sorted(),
                    visible,
                ));
                // SPEC §4.3 / docs/ROADMAP.md M6 "Boolean series → on/off
                // horizontal bands" (`string`/categorical bands, markers,
                // and sharing this view's axis with the plot above are
                // still-open M6 items, not yet built).
                views::state_timeline::show(ui, dataset, &prepared.ticks, &prepared.bool_bands);
            }
            Status::Failed { path, message } => {
                ui.colored_label(
                    egui::Color32::RED,
                    format!("Could not open {}: {message}", path.display()),
                );
            }
        });

        if let Some((path, correction)) = pending_correction {
            self.apply_correction(path, correction);
        }
    }
}

/// The line shown next to the spinner while a file is still being indexed
/// (split out from the render so it is unit-testable without an `egui::Ui`,
/// the same way `inference_bar::field_text` is).
///
/// Issue #87 / SPEC §5.1: a file whose typed columns do not fit
/// `min(25% RAM, 4 GB)` is streamed to the on-disk cache instead, a decision
/// `ingest::choose_storage` takes on the user's behalf before reading a byte.
/// It has two consequences the user can see — the open is slower (two passes
/// over the source instead of one) and the plot stops filling in once the
/// bounded preview is full — and until now both were explained only in the
/// tracing log, which is nowhere a user looks. So a spilled open says what it
/// is doing and why, while it is doing it.
///
/// Deliberately transient: it is not a new inference-bar field (SPEC §1.2
/// freezes that list, and a storage choice is not an inference), and it does
/// not survive the load, because once the file is open there is nothing left
/// for the user to wonder about.
fn loading_label(path: &Path, rows_read: u64, spilled: bool) -> String {
    if spilled {
        format!(
            "Indexing {}… {rows_read} rows so far. This file is too large to hold in memory, \
             so Glyde is streaming it to disk — expect a slower open, and a preview that stops \
             growing before the complete plot appears.",
            path.display(),
        )
    } else {
        format!("Indexing {}… {rows_read} rows so far", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyde_core::ingest::{
        Confidence, DecimalSeparator, Delimiter, InferredField, OpenSummary, SamplingClass,
        TimeAxis, TimeColumnChoice,
    };
    use glyde_core::series::{Series, SeriesValues};
    use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};
    use std::time::Instant;

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

    fn sample_report() -> Box<InferenceReport> {
        Box::new(InferenceReport {
            encoding: InferredField {
                value: "utf-8".to_string(),
                confidence: Confidence::High,
            },
            delimiter: InferredField {
                value: Some(",".to_string()),
                confidence: Confidence::High,
            },
            decimal_separator: InferredField {
                value: Some(".".to_string()),
                confidence: Confidence::High,
            },
            time_column: InferredField {
                value: Some("timestamp".to_string()),
                confidence: Confidence::High,
            },
            timestamp_format: InferredField {
                value: Some("iso8601".to_string()),
                confidence: Confidence::High,
            },
            sample_count: 1,
            sampling_class: SamplingClass::Uniform,
            skipped_row_count: 0,
            skipped_row_details: Vec::new(),
            non_monotonic_count: 0,
            duplicate_timestamp_count: 0,
            timezone: Some(glyde_core::ingest::TimezoneLabel::NaiveLocal),
            time_index: glyde_core::ingest::TimeIndexSource::Column {
                index: 0,
                name: "timestamp".to_string(),
            },
            column_names: vec!["timestamp".to_string(), "value".to_string()],
        })
    }

    fn sample_dataset() -> Box<Dataset> {
        Box::new(Dataset {
            time: TimeAxis::Absolute {
                timestamps: vec![Timestamp::new(0, TimeUnit::Seconds)].into(),
                format: TimestampFormat::EpochSeconds,
            },
            time_column_name: "timestamp".to_string(),
            columns: vec![Series::new("value", SeriesValues::F64(vec![1.0]))],
        })
    }

    /// One `None` pyramid per [`sample_dataset`] column — a plausible
    /// `Progress`/`Completed` payload without needing a real
    /// `pyramids_for_dataset` build for these single-sample fixtures.
    fn sample_pyramids() -> Pyramids {
        vec![None]
    }

    /// [`sample_pyramids`]'s counterpart for `level0_caches` — a `Completed`
    /// message's field, never a real on-disk cache for these fixtures.
    fn sample_level0_caches() -> Level0Caches {
        vec![None]
    }

    /// The indexer-prepared view inputs for [`sample_dataset`].
    fn sample_prepared() -> Box<PreparedView> {
        Box::new(PreparedView::new(
            &sample_dataset(),
            &sample_level0_caches(),
            usize::MAX,
        ))
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
            partial: None,
        };

        app.tx
            .send(IndexingMessage::Completed {
                generation: 1, // file A's generation, superseded by B's (2)
                path: PathBuf::from("a.csv"),
                summary: sample_summary(),
                report: sample_report(),
                dataset: sample_dataset(),
                pyramids: sample_pyramids(),
                level0_caches: sample_level0_caches(),
                prepared: sample_prepared(),
            })
            .expect("channel send");

        app.drain_indexing_messages();

        match &app.status {
            Status::Loading { path, .. } => assert_eq!(path, &path_b),
            _ => {
                panic!("a message from a superseded generation must not change the current status")
            }
        }
    }

    /// docs/ROADMAP.md M3 "Background progressive build emitting partial
    /// levels": a `Progress` message tagged with the current generation must
    /// switch the status to `Loading` with a partial dataset attached, so the
    /// UI can render a growing plot instead of only a spinner.
    #[test]
    fn a_progress_message_attaches_a_partial_dataset_to_the_loading_status() {
        let mut app = GlydeApp::new();
        app.generation = 1;
        let path = PathBuf::from("a.csv");

        app.tx
            .send(IndexingMessage::Progress {
                generation: 1,
                path: path.clone(),
                dataset: sample_dataset(),
                pyramids: sample_pyramids(),
                rows_read: 1,
                spilled: false,
                prepared: sample_prepared(),
            })
            .expect("channel send");

        app.drain_indexing_messages();

        match &app.status {
            Status::Loading {
                path: loading_path,
                partial: Some(partial),
            } => {
                assert_eq!(loading_path, &path);
                assert_eq!(partial.rows_read, 1);
                assert!(!partial.spilled);
            }
            _ => panic!("expected a Loading status with a partial dataset attached"),
        }
    }

    /// Issue #87: the storage decision has to survive the trip from the
    /// checkpoint to the thing that renders it, or the explanation never
    /// reaches the user it is for.
    #[test]
    fn a_spilled_progress_message_carries_the_storage_decision_to_the_status() {
        let mut app = GlydeApp::new();
        app.generation = 1;

        app.tx
            .send(IndexingMessage::Progress {
                generation: 1,
                path: PathBuf::from("huge.csv"),
                dataset: sample_dataset(),
                pyramids: sample_pyramids(),
                rows_read: 200_000,
                spilled: true,
                prepared: sample_prepared(),
            })
            .expect("channel send");

        app.drain_indexing_messages();

        match &app.status {
            Status::Loading {
                partial: Some(partial),
                ..
            } => assert!(partial.spilled),
            _ => panic!("expected a Loading status with a partial dataset attached"),
        }
    }

    // Issue #87 / SPEC §5.1: the readout for an ordinary open says only what
    // it is doing; a spilled one also says why it is slow and why the preview
    // stops growing, since both are consequences of a decision Glyde took for
    // the user (Golden Rule 2's "never silently").
    #[test]
    fn the_loading_label_explains_a_spilled_open_and_stays_quiet_otherwise() {
        let path = PathBuf::from("recording.csv");

        let ordinary = loading_label(&path, 40_000, false);
        assert_eq!(ordinary, "Indexing recording.csv… 40000 rows so far");

        let spilled = loading_label(&path, 40_000, true);
        assert!(spilled.starts_with("Indexing recording.csv… 40000 rows so far"));
        assert!(
            spilled.contains("too large to hold in memory"),
            "the explanation must name the reason, not just the symptom: {spilled}"
        );
        assert!(
            spilled.contains("streaming it to disk"),
            "SPEC §5.1's \"affordable alternative\" is what Glyde did instead: {spilled}"
        );
        assert!(
            spilled.contains("stops growing"),
            "the frozen preview is the other thing the user sees and must be \
             explained too: {spilled}"
        );
    }

    /// The same generation guard that protects `Completed` must protect
    /// `Progress`: a checkpoint from a superseded open must not resurrect a
    /// stale partial dataset over whatever the current open has already
    /// shown.
    #[test]
    fn a_stale_progress_message_does_not_overwrite_the_current_status() {
        let mut app = GlydeApp::new();
        let path_b = PathBuf::from("b.csv");
        app.generation = 2;
        app.status = Status::Loading {
            path: path_b.clone(),
            partial: None,
        };

        app.tx
            .send(IndexingMessage::Progress {
                generation: 1,
                path: PathBuf::from("a.csv"),
                dataset: sample_dataset(),
                pyramids: sample_pyramids(),
                rows_read: 1,
                spilled: false,
                prepared: sample_prepared(),
            })
            .expect("channel send");

        app.drain_indexing_messages();

        match &app.status {
            Status::Loading { path, partial } => {
                assert_eq!(path, &path_b);
                assert!(partial.is_none());
            }
            _ => panic!("a stale Progress message must not change the current status"),
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
                report: sample_report(),
                dataset: sample_dataset(),
                pyramids: sample_pyramids(),
                level0_caches: sample_level0_caches(),
                prepared: sample_prepared(),
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

    // docs/ROADMAP.md M4 "One-click correction of each field → triggers a
    // re-index": `open` must start every newly opened file from a clean
    // slate — a correction made on the previous file must never leak into
    // the next one.
    #[test]
    fn open_resets_overrides_left_over_from_a_previous_file() {
        let mut app = GlydeApp::new();
        app.overrides.delimiter = Some(Delimiter::Pipe);

        app.open(PathBuf::from("does-not-exist-glyde-app-test.csv"));

        assert_eq!(app.overrides, IngestOverrides::default());
    }

    // A correction must accumulate on top of previous ones for the same
    // file (swapping the delimiter must not forget an earlier decimal
    // separator correction), and must bump the generation so a stale
    // in-flight reply from before the correction is dropped like any other
    // superseded message.
    #[test]
    fn apply_correction_accumulates_overrides_and_bumps_the_generation() {
        let mut app = GlydeApp::new();
        app.generation = 1;
        let path = PathBuf::from("does-not-exist-glyde-app-test.csv");

        app.apply_correction(path.clone(), Correction::Delimiter(Delimiter::Semicolon));
        assert_eq!(app.overrides.delimiter, Some(Delimiter::Semicolon));
        assert!(app.overrides.decimal_separator.is_none());
        assert_eq!(app.generation, 2);

        app.apply_correction(path, Correction::DecimalSeparator(DecimalSeparator::Comma));
        assert_eq!(
            app.overrides.delimiter,
            Some(Delimiter::Semicolon),
            "a later correction must not clear an earlier one"
        );
        assert_eq!(
            app.overrides.decimal_separator,
            Some(DecimalSeparator::Comma)
        );
        assert_eq!(app.generation, 3);
    }

    /// End-to-end: a real correction on a real file must reach the
    /// background indexer and change what comes back — the roadmap's own
    /// maintainer test ("swap delimiter / decimal / date order → plot
    /// updates"), exercised through `GlydeApp` rather than `glyde-core`
    /// directly.
    #[test]
    fn apply_correction_re_indexes_the_file_with_the_override_applied() {
        let mut app = GlydeApp::new();
        app.generation = 1;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("comma-decimal.csv");
        std::fs::write(
            &path,
            "timestamp;value\n2026-01-01T00:00:00Z;1,5\n2026-01-01T00:00:01Z;2,5\n",
        )
        .expect("write fixture");

        app.apply_correction(
            path.clone(),
            Correction::DecimalSeparator(DecimalSeparator::Comma),
        );

        assert_eq!(app.generation, 2);
        match &app.status {
            Status::Loading {
                path: loading_path, ..
            } => assert_eq!(loading_path, &path),
            _ => panic!("a correction must switch immediately to Loading"),
        }

        let _started = app
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Started message");
        match app
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Completed message")
        {
            IndexingMessage::Completed {
                generation,
                report,
                dataset,
                ..
            } => {
                assert_eq!(generation, 2);
                assert_eq!(report.decimal_separator.value.as_deref(), Some(","));
                assert_eq!(report.decimal_separator.confidence, Confidence::High);
                assert_eq!(
                    dataset.columns[0].values(),
                    &SeriesValues::F64(vec![1.5, 2.5]),
                    "the comma-decimal override must actually change the parsed values, \
                     not just the reported confidence"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    // Picking a time column invalidates the two overrides that belonged to
    // the previous one; a new delimiter invalidates a picked column position.
    #[test]
    fn a_time_column_choice_resets_what_belonged_to_the_previous_time_column() {
        let mut app = GlydeApp::new();
        let path = PathBuf::from("does-not-exist-glyde-app-test.csv");
        app.overrides.timestamp_format = Some(TimestampFormat::MonthFirst);
        app.overrides.sort_by_time = true;

        app.apply_correction(
            path.clone(),
            Correction::TimeColumn(TimeColumnChoice::Column(2)),
        );
        assert_eq!(app.overrides.time_column, Some(TimeColumnChoice::Column(2)));
        assert_eq!(app.overrides.timestamp_format, None);
        assert!(!app.overrides.sort_by_time);

        app.apply_correction(path, Correction::Delimiter(Delimiter::Semicolon));
        assert_eq!(app.overrides.time_column, None);
    }

    /// End-to-end: asking for the row index on a file with a real time column
    /// re-indexes it with the former time column kept as a series.
    #[test]
    fn picking_the_row_index_re_indexes_the_file_against_it() {
        let mut app = GlydeApp::new();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("counter.csv");
        std::fs::write(
            &path,
            "counter,value
0,1.5
1,2.5
2,0.5
",
        )
        .expect("write fixture");

        app.apply_correction(
            path.clone(),
            Correction::TimeColumn(TimeColumnChoice::RowIndex),
        );

        let _started = app
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Started message");
        match app
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("expected a Completed message")
        {
            IndexingMessage::Completed {
                report, dataset, ..
            } => {
                let names: Vec<&str> = dataset.columns.iter().map(Series::name).collect();
                assert_eq!(names, ["counter", "value"]);
                assert_eq!(
                    report.time_index,
                    glyde_core::ingest::TimeIndexSource::Generated(
                        glyde_core::ingest::GeneratedIndexReason::Requested
                    )
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// End to end, headless (docs/ROADMAP.md M5 maintainer test "the PSD
    /// peak lands where physics says"): a real CSV opened through the real
    /// background indexer, its whole time-view interval handed to the PSD
    /// view, the PSD computed off-thread and rendered.
    #[test]
    fn opening_a_50_hz_sine_shows_its_psd_peak_at_50_hz() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("sine_50hz.csv");
        let mut csv = String::from("t,sine,noise\n");
        for n in 0..20_000 {
            let t = n as f64 / 1000.0;
            let sine = (2.0 * std::f64::consts::PI * 50.0 * t).sin();
            let noise = ((n * 7919) % 1000) as f64 / 1000.0 - 0.5;
            csv.push_str(&format!("{t:.3},{sine:.6},{noise:.3}\n"));
        }
        std::fs::write(&path, csv).expect("write fixture");

        let mut app = GlydeApp::new();
        app.open(path);
        let deadline = Instant::now() + Duration::from_secs(30);
        let (dataset, ticks) = loop {
            app.drain_indexing_messages();
            if let Status::Loaded {
                dataset, prepared, ..
            } = &app.status
            {
                break (Arc::clone(dataset), prepared.ticks.clone());
            }
            assert!(Instant::now() < deadline, "file never finished loading");
            std::thread::sleep(Duration::from_millis(5));
        };

        // The time view's "Fit to data" interval: every row.
        let selection = selection_rows(&ticks, true, (ticks[0], *ticks.last().unwrap()));
        assert_eq!(selection, 0..20_000);
        // The user presses "Compute PSD".
        app.psd.compute(&dataset, selection.clone());
        while app.psd.poll() {
            assert!(Instant::now() < deadline, "PSD never finished");
            std::thread::sleep(Duration::from_millis(5));
        }

        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                app.psd.show(ui, &dataset, Some(selection.clone()));
            });
        });
        assert!(!output.shapes.is_empty());

        let spectra = app.psd.spectra().expect("a uniform sine has a PSD");
        let sine = &spectra[0].psd;
        assert_eq!(spectra[0].name, "sine");
        assert_eq!(spectra.len(), 2);
        let peak = (0..sine.power.len())
            .max_by(|&a, &b| sine.power[a].total_cmp(&sine.power[b]))
            .unwrap();
        assert!(
            (sine.freqs[peak] - 50.0).abs() <= sine.delta_f / 2.0,
            "peak at {} Hz, expected 50 Hz (Δf = {})",
            sine.freqs[peak],
            sine.delta_f
        );
    }
}
