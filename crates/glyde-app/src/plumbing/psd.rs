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

//! The background PSD job (docs/ARCHITECTURE.md Hard rule 3: DSP never runs
//! on the UI thread). A [`PsdJob`] is started only when the user asks for a
//! PSD; it plans the selection with `glyde_core::dsp::psd::plan_psd` under
//! the PSD memory cap, then streams every numeric column through
//! `compute_psds`, which runs as many columns at once as the cap allows. The
//! UI polls it once per frame; dropping it cancels the work.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;

use glyde_core::budget::RamBudget;
use glyde_core::dsp::psd::{
    compute_psds, plan_psd, PsdMemoryCap, PsdPlan, PsdSettings, PsdUnavailable,
};
use glyde_core::dsp::welch::Psd;
use glyde_core::ingest::Dataset;
use glyde_core::series::ViewKind;

/// One column's estimate.
#[derive(Debug, Clone)]
pub struct Spectrum {
    pub name: String,
    pub psd: Psd,
}

/// What a finished [`PsdJob`] produced.
#[derive(Debug, Clone)]
pub enum PsdOutcome {
    /// One spectrum per numeric column, in column order, all computed on
    /// exactly what `plan` says.
    Ready {
        plan: PsdPlan,
        spectra: Vec<Spectrum>,
    },
    /// The selection may not have a PSD (or would not fit the memory cap);
    /// the explanation is the value's `Display`.
    Unavailable(PsdUnavailable),
    /// The file has no numeric series to take a spectrum of.
    NoNumericSeries,
    /// Reading the samples failed (e.g. a spill file vanished).
    Failed(String),
}

/// State the job thread and the UI share.
#[derive(Default)]
struct Shared {
    cancel: AtomicBool,
    /// Samples read so far, across all columns.
    progress: AtomicUsize,
    /// Samples the plan will read in total; 0 until planned.
    total: AtomicUsize,
    streaming: AtomicBool,
}

/// A PSD being computed off the UI thread. Cancelled when dropped.
pub struct PsdJob {
    selection: Range<usize>,
    settings: PsdSettings,
    shared: Arc<Shared>,
    rx: Receiver<PsdOutcome>,
}

impl PsdJob {
    /// Starts computing the PSD of `selection` (row indices) of every numeric
    /// column of `dataset` under `settings`.
    pub fn spawn(dataset: Arc<Dataset>, selection: Range<usize>, settings: PsdSettings) -> Self {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();

        tracing::info!(
            ?selection,
            ?settings,
            "user requested a PSD of the visible interval"
        );
        {
            let selection = selection.clone();
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("glyde-psd".to_string())
                .spawn(move || {
                    let outcome = run(&dataset, selection, &settings, &shared);
                    match outcome {
                        // The receiver is gone if the job was cancelled.
                        Some(outcome) => {
                            let _ = tx.send(outcome);
                        }
                        None => tracing::info!("PSD cancelled"),
                    }
                })
                .expect("spawning the PSD thread");
        }

        Self {
            selection,
            settings,
            shared,
            rx,
        }
    }

    /// The selection and settings this job computes.
    pub fn request(&self) -> (&Range<usize>, &PsdSettings) {
        (&self.selection, &self.settings)
    }

    /// Fraction of the planned samples read so far, across all columns.
    pub fn progress(&self) -> f32 {
        let total = self.shared.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        (self.shared.progress.load(Ordering::Relaxed) as f64 / total as f64).min(1.0) as f32
    }

    /// Whether the selection's raw samples are larger than the PSD memory
    /// cap, so they are being read progressively (SPEC §5.1).
    pub fn is_streaming(&self) -> bool {
        self.shared.streaming.load(Ordering::Relaxed)
    }

    /// The outcome, once there is one. Never blocks.
    pub fn try_outcome(&self) -> Option<PsdOutcome> {
        match self.rx.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(PsdOutcome::Failed(
                "the PSD computation stopped unexpectedly".to_string(),
            )),
        }
    }
}

impl Drop for PsdJob {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

/// The job body. `None` when cancelled.
fn run(
    dataset: &Dataset,
    selection: Range<usize>,
    settings: &PsdSettings,
    shared: &Shared,
) -> Option<PsdOutcome> {
    let (names, columns): (Vec<&str>, Vec<_>) = dataset
        .columns
        .iter()
        .filter(|series| series.view_kind() == ViewKind::TimeDomain)
        .filter_map(|series| Some((series.name(), series.values().sample_source()?)))
        .unzip();
    if columns.is_empty() {
        return Some(PsdOutcome::NoNumericSeries);
    }

    let cap = PsdMemoryCap::for_budget(&RamBudget::from_system());
    let plan = match plan_psd(&dataset.time, selection, settings, columns.len(), cap) {
        Ok(Ok(plan)) => plan,
        Ok(Err(unavailable)) => return Some(PsdOutcome::Unavailable(unavailable)),
        Err(error) => return Some(PsdOutcome::Failed(error.to_string())),
    };
    shared
        .total
        .store(plan.samples_used * columns.len(), Ordering::Relaxed);
    shared
        .streaming
        .store(plan.larger_than_memory_cap, Ordering::Relaxed);
    if shared.cancel.load(Ordering::Relaxed) {
        return None;
    }

    let spectra = compute_psds(&dataset.time, &columns, &plan, &|read| {
        shared.progress.fetch_add(read, Ordering::Relaxed);
        !shared.cancel.load(Ordering::Relaxed)
    });
    let spectra = match spectra {
        Ok(Some(spectra)) => spectra,
        Ok(None) => return None,
        Err(error) => return Some(PsdOutcome::Failed(error.to_string())),
    };
    tracing::info!(
        columns = spectra.len(),
        windows = spectra.first().map(|psd| psd.segment_count),
        "PSD computed"
    );
    Some(PsdOutcome::Ready {
        spectra: names
            .into_iter()
            .zip(spectra)
            .map(|(name, psd)| Spectrum {
                name: name.to_string(),
                psd,
            })
            .collect(),
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyde_core::ingest::TimeAxis;
    use glyde_core::series::{Series, SeriesValues};
    use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};
    use std::time::{Duration, Instant};

    /// A 1 kHz axis carrying a 125 Hz tone and a text column.
    fn tone_dataset(len: usize) -> Arc<Dataset> {
        let timestamps: Vec<Timestamp> = (0..len as i128)
            .map(|n| Timestamp::new(n, TimeUnit::Milliseconds))
            .collect();
        let tone: Vec<f64> = (0..len)
            .map(|n| (2.0 * std::f64::consts::PI * 125.0 * n as f64 / 1000.0).sin())
            .collect();
        Arc::new(Dataset {
            time: TimeAxis::Absolute {
                timestamps: timestamps.into(),
                format: TimestampFormat::EpochMillis,
            },
            time_column_name: "t".to_string(),
            columns: vec![
                Series::new("tone", SeriesValues::F64(tone)),
                Series::new("label", SeriesValues::String(vec!["x".to_string(); len])),
            ],
        })
    }

    fn wait(job: &PsdJob) -> PsdOutcome {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(outcome) = job.try_outcome() {
                return outcome;
            }
            assert!(Instant::now() < deadline, "PSD job never finished");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_job_computes_one_spectrum_per_numeric_column_with_the_peak_where_physics_says() {
        let job = PsdJob::spawn(tone_dataset(20_000), 0..20_000, PsdSettings::default());

        let PsdOutcome::Ready { plan, spectra } = wait(&job) else {
            panic!("a uniform tone must have a PSD");
        };
        assert_eq!(plan.sample_rate, 1000.0);
        assert_eq!(spectra.len(), 1, "the text column has no spectrum");
        assert_eq!(spectra[0].name, "tone");
        let psd = &spectra[0].psd;
        let peak = (0..psd.power.len())
            .max_by(|&a, &b| psd.power[a].total_cmp(&psd.power[b]))
            .unwrap();
        assert!((psd.freqs[peak] - 125.0).abs() <= psd.delta_f / 2.0);
        assert_eq!(job.progress(), 1.0);
    }

    #[test]
    fn a_selection_that_may_not_have_a_psd_comes_back_with_its_explanation() {
        let job = PsdJob::spawn(tone_dataset(100), 5..6, PsdSettings::default());
        assert!(matches!(
            wait(&job),
            PsdOutcome::Unavailable(PsdUnavailable::TooFewSamples { available: 1 })
        ));
    }

    #[test]
    fn dropping_a_job_cancels_it() {
        let job = PsdJob::spawn(
            tone_dataset(2_000_000),
            0..2_000_000,
            PsdSettings::default(),
        );
        let shared = Arc::clone(&job.shared);
        drop(job);
        assert!(shared.cancel.load(Ordering::Relaxed));
    }
}
