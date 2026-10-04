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
//! on the UI thread). A [`PsdJob`] plans the selection with
//! `glyde_core::dsp::psd::plan_psd`, then streams every numeric column
//! through `compute_psd` on the `rayon` pool, one column per task. The UI
//! polls it once per frame; dropping it cancels the work (a newer selection
//! supersedes an older one, so a stale estimate is never finished, let alone
//! shown).

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;

use glyde_core::budget::RamBudget;
use glyde_core::dsp::psd::{compute_psd, plan_psd, PsdPlan, PsdSettings, PsdUnavailable};
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
    /// The selection may not have a PSD; the explanation is the value's
    /// `Display`.
    Unavailable(PsdUnavailable),
    /// The file has no numeric series to take a spectrum of.
    NoNumericSeries,
    /// Reading the samples failed (e.g. a spill file vanished).
    Failed(String),
}

/// A PSD being computed off the UI thread. Cancelled when dropped.
pub struct PsdJob {
    selection: Range<usize>,
    settings: PsdSettings,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicUsize>,
    streaming: Arc<AtomicBool>,
    total: usize,
    rx: Receiver<PsdOutcome>,
}

impl PsdJob {
    /// Starts computing the PSD of `selection` (row indices) of every numeric
    /// column of `dataset` under `settings`.
    pub fn spawn(dataset: Arc<Dataset>, selection: Range<usize>, settings: PsdSettings) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicUsize::new(0));
        let streaming = Arc::new(AtomicBool::new(false));
        let numeric_columns = dataset
            .columns
            .iter()
            .filter(|series| series.view_kind() == ViewKind::TimeDomain)
            .count();
        let total = selection.len().saturating_mul(numeric_columns).max(1);
        let (tx, rx) = mpsc::channel();

        tracing::info!(
            ?selection,
            ?settings,
            numeric_columns,
            "PSD requested for the time view's selection"
        );
        {
            let selection = selection.clone();
            let cancel = Arc::clone(&cancel);
            let progress = Arc::clone(&progress);
            let streaming = Arc::clone(&streaming);
            thread::Builder::new()
                .name("glyde-psd".to_string())
                .spawn(move || {
                    let outcome = run(
                        &dataset, selection, &settings, &cancel, &progress, &streaming,
                    );
                    if let Some(outcome) = outcome {
                        // The receiver is gone if the job was superseded.
                        let _ = tx.send(outcome);
                    }
                })
                .expect("spawning the PSD thread");
        }

        Self {
            selection,
            settings,
            cancel,
            progress,
            streaming,
            total,
            rx,
        }
    }

    /// The selection and settings this job computes.
    pub fn request(&self) -> (&Range<usize>, &PsdSettings) {
        (&self.selection, &self.settings)
    }

    /// Fraction of the selection's samples read so far, across all columns.
    pub fn progress(&self) -> f32 {
        (self.progress.load(Ordering::Relaxed) as f64 / self.total as f64).min(1.0) as f32
    }

    /// Whether the plan found the selection too large to hold in memory, so
    /// it is being streamed progressively (SPEC §5.1).
    pub fn is_streaming(&self) -> bool {
        self.streaming.load(Ordering::Relaxed)
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
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The job body. `None` when cancelled.
fn run(
    dataset: &Dataset,
    selection: Range<usize>,
    settings: &PsdSettings,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
    streaming: &AtomicBool,
) -> Option<PsdOutcome> {
    let numeric: Vec<_> = dataset
        .columns
        .iter()
        .filter(|series| series.view_kind() == ViewKind::TimeDomain)
        .filter_map(|series| Some((series.name(), series.values().sample_source()?)))
        .collect();
    if numeric.is_empty() {
        return Some(PsdOutcome::NoNumericSeries);
    }

    let plan = match plan_psd(
        &dataset.time,
        selection,
        settings,
        &RamBudget::from_system(),
    ) {
        Ok(Ok(plan)) => plan,
        Ok(Err(unavailable)) => return Some(PsdOutcome::Unavailable(unavailable)),
        Err(error) => return Some(PsdOutcome::Failed(error.to_string())),
    };
    streaming.store(plan.exceeds_memory_budget, Ordering::Relaxed);
    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    use rayon::prelude::*;
    let spectra: Vec<_> = numeric
        .par_iter()
        .map(|(name, samples)| {
            let mut reported = 0usize;
            let psd = compute_psd(samples, &plan, &mut |read| {
                progress.fetch_add(read - reported, Ordering::Relaxed);
                reported = read;
                !cancel.load(Ordering::Relaxed)
            });
            psd.map(|psd| {
                psd.map(|psd| Spectrum {
                    name: name.to_string(),
                    psd,
                })
            })
        })
        .collect();

    let mut ready = Vec::with_capacity(spectra.len());
    for spectrum in spectra {
        match spectrum {
            Ok(Some(spectrum)) => ready.push(spectrum),
            Ok(None) => return None,
            Err(error) => return Some(PsdOutcome::Failed(error.to_string())),
        }
    }
    tracing::info!(
        columns = ready.len(),
        windows = ready.first().map(|s| s.psd.segment_count),
        "PSD computed"
    );
    Some(PsdOutcome::Ready {
        plan,
        spectra: ready,
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
        let cancel = Arc::clone(&job.cancel);
        drop(job);
        assert!(cancel.load(Ordering::Relaxed));
    }
}
