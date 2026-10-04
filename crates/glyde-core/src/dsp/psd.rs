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

//! The PSD product logic (docs/SPEC.md §3.2–3.3, §5.1, docs/ROADMAP.md M5):
//! given a selected interval of a time axis, decide *whether* it may have a
//! PSD and *what exactly* it is computed on — before a single sample is read.
//!
//! [`plan_psd`] classifies the selection itself (SPEC §2.2's rules, applied to
//! just the selected rows) and answers with either a [`PsdPlan`] or a
//! [`PsdUnavailable`] that explains why not and, where one exists, offers the
//! affordable alternative:
//!
//! - `Uniform` → one segment, the whole selection.
//! - `SegmentedUniform` → one segment per gap-delimited run; runs shorter
//!   than one analysis window are excluded and *counted*, so the plot can
//!   say so (SPEC §3.3).
//! - `Irregular` → no PSD. The largest uniform gap-delimited run of the whole
//!   series is offered instead, when one long enough exists. Nothing is ever
//!   resampled.
//! - Out-of-order timestamps → no PSD: a Δt is only a sampling interval when
//!   time moves forward.
//!
//! [`compute_psd`] then runs the plan through `dsp::welch`'s streaming entry
//! points, once per numeric column. It reads raw samples only — a
//! [`SampleSource`], never the pyramid (SPEC §3.2) — in bounded chunks, so
//! the memory it needs is a fixed multiple of the window length whatever the
//! selection's size. That working set is the one thing that is checked
//! against the RAM budget up front (SPEC §5.1: "checks affordability before
//! acting, never after").

use std::fmt;
use std::ops::Range;

use tracing::info;

use super::detrend::Detrend;
use super::welch::{
    default_segment_length, welch_segmented_source, welch_source, Psd, WelchConfig,
    DEFAULT_OVERLAP, MAX_SEGMENT_LEN, MIN_SEGMENT_LEN,
};
use super::window::Window;
use crate::budget::RamBudget;
use crate::ingest::{TimeAxis, PROGRESSIVE_TICK_SCALE};
use crate::series::SampleSource;
use crate::time::{is_uniform_range, scan_range, SamplingClass, TickSource};
use crate::Result;

/// Every segment length the PSD settings offer: the powers of two the
/// software's own default can pick (SPEC §3.2's `[256, 65536]` clamp).
pub const SEGMENT_LENGTH_CHOICES: [usize; 9] =
    [256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];

/// Every overlap fraction the PSD settings offer.
pub const OVERLAP_CHOICES: [f64; 4] = [0.0, 0.25, 0.5, 0.75];

/// Fewer selected samples than this has no spectrum to speak of (a single
/// sample has no Δt, so no sampling rate either).
pub const MIN_PSD_SAMPLES: usize = 2;

/// How the analysis segment length is chosen (SPEC §3.2's second control).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentLength {
    /// The software's default: [`default_segment_length`] of the selection
    /// (of its longest run, for a segmented selection).
    Auto,
    /// A user-chosen length, one of [`SEGMENT_LENGTH_CHOICES`].
    Fixed(usize),
}

/// SPEC §3.2's "at most three controls", hidden behind one "PSD settings"
/// affordance and never required for a correct first result. Detrend is the
/// documented constant (mean removal) default, not a fourth control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PsdSettings {
    pub window: Window,
    pub segment_length: SegmentLength,
    pub overlap: f64,
}

impl Default for PsdSettings {
    fn default() -> Self {
        Self {
            window: Window::Hann,
            segment_length: SegmentLength::Auto,
            overlap: DEFAULT_OVERLAP,
        }
    }
}

/// The unit a PSD's frequency axis is in, which follows from the time axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrequencyUnit {
    /// An absolute timestamp axis: cycles per second.
    Hertz,
    /// A progressive numeric index (SPEC §2.1), which has no time unit: cycles
    /// per unit of the index itself. Never relabeled as Hz.
    PerIndexUnit,
}

/// How a time axis's ticks convert to its own unit (seconds, or index units).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisScale {
    pub ticks_per_unit: i128,
    pub frequency_unit: FrequencyUnit,
}

impl AxisScale {
    /// The scale of `time`'s pyramid ticks (`TimeAxis::to_pyramid_ticks`,
    /// also what `TimeAxis`'s [`TickSource`] yields).
    pub fn of(time: &TimeAxis) -> Self {
        match time {
            TimeAxis::Absolute { timestamps, .. } => Self {
                ticks_per_unit: timestamps
                    .get(0)
                    .map(|t| t.unit.ticks_per_second())
                    .unwrap_or(1),
                frequency_unit: FrequencyUnit::Hertz,
            },
            TimeAxis::Progressive { .. } => Self {
                ticks_per_unit: PROGRESSIVE_TICK_SCALE as i128,
                frequency_unit: FrequencyUnit::PerIndexUnit,
            },
        }
    }
}

/// Gap-delimited runs left out of a segmented PSD because each is shorter
/// than one analysis window (SPEC §3.3: "excluded and reported").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExcludedSegments {
    pub count: usize,
    pub samples: usize,
}

/// Exactly what a PSD will be computed on, decided before any sample is read.
#[derive(Debug, Clone, PartialEq)]
pub struct PsdPlan {
    /// The selected rows, as asked for.
    pub selection: Range<usize>,
    /// The selection's sampling class (SPEC §2.2) — never `Irregular` here.
    pub sampling_class: SamplingClass,
    /// Samples per [`FrequencyUnit`] unit: `1 / median Δt` of the selection.
    pub sample_rate: f64,
    pub frequency_unit: FrequencyUnit,
    /// The contiguous, gap-free runs Welch runs on, in row order.
    pub segments: Vec<Range<usize>>,
    pub excluded: ExcludedSegments,
    pub config: WelchConfig,
    /// The selection's raw samples alone would not fit the RAM budget, so it
    /// is only ever computable streaming — which is how it is computed — and
    /// the user is told so (SPEC §5.1's example: "PSD over the full 8-hour
    /// range needs streaming — computing progressively").
    pub exceeds_memory_budget: bool,
}

impl PsdPlan {
    /// Whether this is SPEC §3.3's per-segment average rather than one
    /// uniform run.
    pub fn is_segmented(&self) -> bool {
        self.sampling_class == SamplingClass::SegmentedUniform
    }

    /// Samples that actually enter the estimate (excluded runs not counted).
    pub fn samples_used(&self) -> usize {
        self.segments.iter().map(|r| r.len()).sum()
    }

    /// The analysis window length actually used: the configured segment
    /// length, or the whole selection when a uniform selection is shorter.
    pub fn window_len(&self) -> usize {
        if self.is_segmented() {
            self.config.segment_len
        } else {
            self.config.segment_len.min(self.samples_used())
        }
    }
}

/// Why a selection has no PSD, with the affordable alternative where one
/// exists (SPEC §3.3, §5.1). [`fmt::Display`] is the user-facing explanation.
#[derive(Debug, Clone, PartialEq)]
pub enum PsdUnavailable {
    TooFewSamples {
        available: usize,
    },
    /// Some timestamps in the selection step backwards.
    NonMonotonic {
        backward_steps: usize,
    },
    /// SPEC §3.3 `Irregular`: disabled, never resampled. `largest_uniform` is
    /// the longest uniform gap-delimited run of the whole series that holds
    /// at least one minimum-length window, if there is one.
    Irregular {
        largest_uniform: Option<Range<usize>>,
    },
    /// Segmented, but every run is shorter than one analysis window.
    NoSegmentFitsAWindow {
        segment_count: usize,
        longest: usize,
        window: usize,
    },
    /// Even one analysis window's working set would not fit the RAM budget.
    OverBudget {
        requested_bytes: u64,
        cap_bytes: u64,
        affordable_segment_len: Option<usize>,
    },
}

impl fmt::Display for PsdUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PsdUnavailable::TooFewSamples { available } => write!(
                f,
                "The selection holds {available} sample(s); a spectrum needs at least \
                 {MIN_PSD_SAMPLES}. Select a longer interval."
            ),
            PsdUnavailable::NonMonotonic { backward_steps } => write!(
                f,
                "PSD requires timestamps in time order, but this selection steps backwards \
                 {backward_steps} time(s). Sort the file by time from the inference bar, or \
                 select an interval where time only moves forward."
            ),
            PsdUnavailable::Irregular { largest_uniform } => {
                write!(
                    f,
                    "PSD requires uniform sampling; this series has irregular timestamps. \
                     Glyde does not resample it to fake a uniform rate."
                )?;
                match largest_uniform {
                    Some(range) => write!(
                        f,
                        " Its largest uniformly sampled stretch ({} samples) can be analyzed \
                         instead.",
                        range.len()
                    ),
                    None => write!(
                        f,
                        " No uniformly sampled stretch of at least {MIN_SEGMENT_LEN} samples \
                         exists in it."
                    ),
                }
            }
            PsdUnavailable::NoSegmentFitsAWindow {
                segment_count,
                longest,
                window,
            } => write!(
                f,
                "This selection is split by gaps into {segment_count} segments, and no \
                 analysis window may cross a gap — but even the longest segment ({longest} \
                 samples) is shorter than one {window}-sample window."
            ),
            PsdUnavailable::OverBudget {
                requested_bytes,
                cap_bytes,
                affordable_segment_len,
            } => {
                write!(
                    f,
                    "This PSD's working memory ({requested_bytes} bytes) is over Glyde's \
                     {cap_bytes}-byte memory budget."
                )?;
                match affordable_segment_len {
                    Some(len) => write!(
                        f,
                        " A {len}-sample segment length fits — choose it in PSD settings."
                    ),
                    None => write!(f, " No segment length fits on this machine."),
                }
            }
        }
    }
}

/// Bytes one Welch estimate holds while it runs, for a window of `len`
/// samples: the window being filled, its detrend buffer, the window
/// coefficients (8 bytes each), the complex spectrum and FFT scratch (16
/// each), plus the per-bin running sums the estimate and a segmented average
/// keep (three `len / 2 + 1` arrays of 8 bytes).
pub fn working_set_bytes(len: usize, segment_count: usize) -> u64 {
    let window = 3 * 8 + 2 * 16;
    let bins = (len / 2 + 1) as u64 * 8 * 3;
    len as u64 * window + bins + segment_count as u64 * 16
}

/// [`plan_psd_on`] for a [`TimeAxis`], with its own [`AxisScale`].
pub fn plan_psd(
    time: &TimeAxis,
    selection: Range<usize>,
    settings: &PsdSettings,
    budget: &RamBudget,
) -> Result<std::result::Result<PsdPlan, PsdUnavailable>> {
    plan_psd_on(time, AxisScale::of(time), selection, settings, budget)
}

/// Decides what a PSD of `selection` (row indices into `ticks`) is computed
/// on, in bounded memory over the ticks alone — see the module docs.
pub fn plan_psd_on<T: TickSource + ?Sized>(
    ticks: &T,
    scale: AxisScale,
    selection: Range<usize>,
    settings: &PsdSettings,
    budget: &RamBudget,
) -> Result<std::result::Result<PsdPlan, PsdUnavailable>> {
    let end = selection.end.min(ticks.tick_count());
    let selection = selection.start.min(end)..end;
    if selection.len() < MIN_PSD_SAMPLES {
        return Ok(Err(PsdUnavailable::TooFewSamples {
            available: selection.len(),
        }));
    }

    // Every run long enough to hold a minimum-length window is kept (at most
    // `len / MIN_SEGMENT_LEN` of them, a bounded list); shorter ones are only
    // counted.
    let mut runs: Vec<Range<usize>> = Vec::new();
    let mut short = ExcludedSegments::default();
    let mut run_count = 0usize;
    let mut longest = 0usize;
    let scan = scan_range(ticks, selection.clone(), &mut |run| {
        run_count += 1;
        longest = longest.max(run.len());
        if run.len() >= MIN_SEGMENT_LEN {
            runs.push(run);
        } else {
            short.count += 1;
            short.samples += run.len();
        }
        Ok(())
    })?;

    let backward_steps = scan.stats.monotonicity.non_monotonic_count;
    if backward_steps > 0 {
        info!(
            backward_steps,
            "PSD unavailable: selection is not in time order"
        );
        return Ok(Err(PsdUnavailable::NonMonotonic { backward_steps }));
    }

    let sampling_class = scan.stats.sampling_class;
    if sampling_class == SamplingClass::Irregular {
        let largest_uniform = largest_uniform_run(ticks)?;
        info!(
            ?largest_uniform,
            "PSD unavailable: irregular sampling (SPEC §3.3); offering the largest uniform run"
        );
        return Ok(Err(PsdUnavailable::Irregular { largest_uniform }));
    }

    let median_delta = scan
        .median_delta
        .expect("two or more ticks always have a median Δt");
    if median_delta <= 0.0 {
        // Every timestamp identical: there is no sampling interval at all.
        return Ok(Err(PsdUnavailable::Irregular {
            largest_uniform: None,
        }));
    }
    let sample_rate = scale.ticks_per_unit as f64 / median_delta;

    let (segments, excluded, basis) = if sampling_class == SamplingClass::Uniform {
        (
            vec![selection.clone()],
            ExcludedSegments::default(),
            selection.len(),
        )
    } else {
        (runs, short, longest)
    };
    let segment_len = match settings.segment_length {
        SegmentLength::Auto => default_segment_length(basis),
        SegmentLength::Fixed(len) => len.clamp(MIN_SEGMENT_LEN, MAX_SEGMENT_LEN),
    };

    let (segments, excluded) = if sampling_class == SamplingClass::SegmentedUniform {
        let mut excluded = excluded;
        let mut kept = Vec::with_capacity(segments.len());
        for run in segments {
            if run.len() >= segment_len {
                kept.push(run);
            } else {
                excluded.count += 1;
                excluded.samples += run.len();
            }
        }
        if kept.is_empty() {
            info!(
                run_count,
                longest, segment_len, "PSD unavailable: no gap-free segment holds one window"
            );
            return Ok(Err(PsdUnavailable::NoSegmentFitsAWindow {
                segment_count: run_count,
                longest,
                window: segment_len,
            }));
        }
        (kept, excluded)
    } else {
        (segments, excluded)
    };

    let requested_bytes = working_set_bytes(segment_len, segments.len());
    if !budget.affords(requested_bytes) {
        let affordable_segment_len = SEGMENT_LENGTH_CHOICES
            .iter()
            .rev()
            .copied()
            .find(|&len| budget.affords(working_set_bytes(len, segments.len())));
        info!(
            requested_bytes,
            cap_bytes = budget.cap_bytes(),
            ?affordable_segment_len,
            "PSD refused before computing: working set over the RAM budget (SPEC §5.1)"
        );
        return Ok(Err(PsdUnavailable::OverBudget {
            requested_bytes,
            cap_bytes: budget.cap_bytes(),
            affordable_segment_len,
        }));
    }

    let selection_bytes = selection.len() as u64 * std::mem::size_of::<f64>() as u64;
    let plan = PsdPlan {
        selection,
        sampling_class,
        sample_rate,
        frequency_unit: scale.frequency_unit,
        segments,
        excluded,
        config: WelchConfig {
            window: settings.window,
            segment_len,
            overlap: settings.overlap,
            detrend: Detrend::Constant,
        },
        exceeds_memory_budget: !budget.affords(selection_bytes),
    };
    info!(
        selection = ?plan.selection,
        sampling_class = ?plan.sampling_class,
        sample_rate = plan.sample_rate,
        segments = plan.segments.len(),
        excluded_segments = plan.excluded.count,
        excluded_samples = plan.excluded.samples,
        window = ?plan.config.window,
        segment_len = plan.config.segment_len,
        overlap = plan.config.overlap,
        streaming_required = plan.exceeds_memory_budget,
        "PSD planned (SPEC §3.2–3.3)"
    );
    Ok(Ok(plan))
}

/// The longest gap-delimited run of the whole series that is itself uniform
/// (SPEC §2.2) and holds at least one minimum-length window — what an
/// `Irregular` series offers instead of a PSD (SPEC §3.3).
fn largest_uniform_run<T: TickSource + ?Sized>(ticks: &T) -> Result<Option<Range<usize>>> {
    let mut runs: Vec<Range<usize>> = Vec::new();
    scan_range(ticks, 0..ticks.tick_count(), &mut |run| {
        if run.len() >= MIN_SEGMENT_LEN {
            runs.push(run);
        }
        Ok(())
    })?;
    // Longest first; the earliest of equally long runs wins, deterministically.
    runs.sort_by(|a, b| b.len().cmp(&a.len()).then(a.start.cmp(&b.start)));
    for run in runs {
        if is_uniform_range(ticks, run.clone())? {
            return Ok(Some(run));
        }
    }
    Ok(None)
}

/// Runs `plan` over one column's raw samples, streaming (see the module
/// docs). `keep_going` is called with the number of samples read so far and
/// cancels the estimate by returning `false`, in which case this returns
/// `Ok(None)`.
pub fn compute_psd<S: SampleSource + ?Sized>(
    samples: &S,
    plan: &PsdPlan,
    keep_going: &mut dyn FnMut(usize) -> bool,
) -> Result<Option<Psd>> {
    if plan.is_segmented() {
        welch_segmented_source(
            samples,
            &plan.segments,
            plan.sample_rate,
            &plan.config,
            keep_going,
        )
    } else {
        welch_source(
            samples,
            plan.selection.clone(),
            plan.sample_rate,
            &plan.config,
            keep_going,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::welch::{welch, welch_segmented};

    const SECONDS: AxisScale = AxisScale {
        ticks_per_unit: 1_000_000_000,
        frequency_unit: FrequencyUnit::Hertz,
    };

    fn roomy() -> RamBudget {
        RamBudget::from_total_ram_bytes(16 * 1024 * 1024 * 1024)
    }

    /// `len` ticks at `period_ns`, starting at `start_ns`.
    fn run(start_ns: i128, period_ns: i128, len: usize) -> Vec<i128> {
        (0..len as i128).map(|n| start_ns + n * period_ns).collect()
    }

    fn plan(
        ticks: &[i128],
        selection: Range<usize>,
    ) -> std::result::Result<PsdPlan, PsdUnavailable> {
        plan_psd_on(ticks, SECONDS, selection, &PsdSettings::default(), &roomy()).unwrap()
    }

    #[test]
    fn a_uniform_selection_is_one_segment_at_the_rate_its_timestamps_imply() {
        let ticks = run(0, 1_000_000, 10_000); // 1 kHz
        let plan = plan(&ticks, 0..10_000).unwrap();

        assert_eq!(plan.sampling_class, SamplingClass::Uniform);
        assert_eq!(plan.segments, vec![0..10_000]);
        assert_eq!(plan.sample_rate, 1000.0);
        assert_eq!(plan.config.segment_len, 1024); // largest 2^k ≤ 10000/8
        assert_eq!(plan.config.window, Window::Hann);
        assert_eq!(plan.config.overlap, 0.5);
        assert_eq!(plan.config.detrend, Detrend::Constant);
        assert_eq!(plan.excluded, ExcludedSegments::default());
        assert!(!plan.exceeds_memory_budget);
    }

    #[test]
    fn only_the_selected_rows_are_planned() {
        let ticks = run(0, 1_000_000, 10_000);
        let plan = plan(&ticks, 2_000..6_000).unwrap();
        assert_eq!(plan.segments, vec![2_000..6_000]);
        assert_eq!(plan.samples_used(), 4_000);
    }

    #[test]
    fn a_segmented_selection_keeps_every_run_that_holds_a_window_and_reports_the_rest() {
        // Three 4096-sample bursts at 1 kHz, then a 100-sample one, each a
        // day apart.
        let day = 86_400_000_000_000i128;
        let mut ticks = run(0, 1_000_000, 4096);
        ticks.extend(run(day, 1_000_000, 4096));
        ticks.extend(run(2 * day, 1_000_000, 100));
        ticks.extend(run(3 * day, 1_000_000, 4096));

        let plan = plan(&ticks, 0..ticks.len()).unwrap();

        assert!(plan.is_segmented());
        assert_eq!(plan.segments, vec![0..4096, 4096..8192, 8292..12388]);
        assert_eq!(
            plan.excluded,
            ExcludedSegments {
                count: 1,
                samples: 100
            }
        );
        assert_eq!(plan.sample_rate, 1000.0);
        // Auto length follows the longest run: largest 2^k ≤ 4096/8.
        assert_eq!(plan.config.segment_len, 512);
    }

    #[test]
    fn a_segmented_selection_whose_runs_are_all_too_short_explains_why() {
        let mut ticks = run(0, 1_000_000_000, 3);
        ticks.extend(run(172_800_000_000_000, 1_000_000_000, 3));
        ticks.extend(run(345_600_000_000_000, 1_000_000_000, 3));

        assert_eq!(
            plan(&ticks, 0..9),
            Err(PsdUnavailable::NoSegmentFitsAWindow {
                segment_count: 3,
                longest: 3,
                window: MIN_SEGMENT_LEN
            })
        );
    }

    #[test]
    fn an_irregular_series_is_refused_and_offers_its_largest_uniform_run() {
        // A jittery stretch, a gap, then a clean 1 kHz run of 2000 samples,
        // another gap, and a clean but shorter run.
        let mut ticks: Vec<i128> = Vec::new();
        // Pseudo-random ±30% jitter (an alternating pattern would have a MAD
        // of zero and so count as uniform under SPEC §2.2's robust CV).
        let mut t = 0i128;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..3000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            t += 700_000 + (state % 600_000) as i128;
            ticks.push(t);
        }
        t += 60_000_000_000;
        let clean_start = ticks.len();
        ticks.extend(run(t, 1_000_000, 2000));
        t += 2000 * 1_000_000 + 60_000_000_000;
        ticks.extend(run(t, 1_000_000, 700));

        let result = plan(&ticks, 0..ticks.len());

        assert_eq!(
            result,
            Err(PsdUnavailable::Irregular {
                largest_uniform: Some(clean_start..clean_start + 2000)
            })
        );
        // And the offered run itself, once selected, is analyzable.
        let offered = plan(&ticks, clean_start..clean_start + 2000).unwrap();
        assert_eq!(offered.sampling_class, SamplingClass::Uniform);
    }

    #[test]
    fn out_of_order_timestamps_have_no_psd() {
        let mut ticks = run(0, 1_000_000, 1000);
        ticks.swap(10, 11);
        assert_eq!(
            plan(&ticks, 0..1000),
            Err(PsdUnavailable::NonMonotonic { backward_steps: 1 })
        );
    }

    #[test]
    fn a_single_sample_is_too_few() {
        let ticks = run(0, 1_000_000, 10);
        assert_eq!(
            plan(&ticks, 3..4),
            Err(PsdUnavailable::TooFewSamples { available: 1 })
        );
    }

    #[test]
    fn a_working_set_over_budget_is_refused_before_computing_with_an_affordable_length() {
        let ticks = run(0, 1_000_000, 1_000_000);
        let settings = PsdSettings {
            segment_length: SegmentLength::Fixed(65536),
            ..PsdSettings::default()
        };
        // A budget that holds a 1024-sample window but not a 65536 one.
        let cap = working_set_bytes(1024, 1);
        let budget = RamBudget::from_total_ram_bytes(cap * 4);
        assert_eq!(budget.cap_bytes(), cap);

        let refused = plan_psd_on(&ticks[..], SECONDS, 0..ticks.len(), &settings, &budget)
            .unwrap()
            .unwrap_err();

        assert_eq!(
            refused,
            PsdUnavailable::OverBudget {
                requested_bytes: working_set_bytes(65536, 1),
                cap_bytes: cap,
                affordable_segment_len: Some(1024),
            }
        );
        assert!(refused
            .to_string()
            .contains("1024-sample segment length fits"));
    }

    #[test]
    fn a_selection_too_large_to_hold_is_planned_as_streaming_not_refused() {
        let ticks = run(0, 1_000_000, 1_000_000);
        // Enough for the working set, far too little for a million samples.
        let budget = RamBudget::from_total_ram_bytes(working_set_bytes(65536, 1) * 4);
        let plan = plan_psd_on(
            &ticks[..],
            SECONDS,
            0..ticks.len(),
            &PsdSettings::default(),
            &budget,
        )
        .unwrap()
        .unwrap();
        assert!(plan.exceeds_memory_budget);
    }

    #[test]
    fn compute_runs_a_uniform_plan_exactly_like_welch_on_the_selected_slice() {
        let ticks = run(0, 1_000_000, 20_000);
        let samples: Vec<f64> = (0..20_000).map(|n| (n as f64 * 0.3).sin()).collect();
        let plan = plan(&ticks, 1_000..19_000).unwrap();

        let psd = compute_psd(&samples[..], &plan, &mut |_| true)
            .unwrap()
            .unwrap();

        let expected = welch(&samples[1_000..19_000], 1000.0, &plan.config);
        assert_eq!(psd.power, expected.power);
    }

    #[test]
    fn compute_runs_a_segmented_plan_exactly_like_welch_segmented_on_the_kept_runs() {
        let day = 86_400_000_000_000i128;
        let mut ticks = run(0, 1_000_000, 4096);
        ticks.extend(run(day, 1_000_000, 100));
        ticks.extend(run(2 * day, 1_000_000, 6000));
        let samples: Vec<f64> = (0..ticks.len()).map(|n| (n as f64 * 0.7).cos()).collect();
        let plan = plan(&ticks, 0..ticks.len()).unwrap();

        let psd = compute_psd(&samples[..], &plan, &mut |_| true)
            .unwrap()
            .unwrap();

        let expected = welch_segmented(
            &[&samples[0..4096], &samples[4196..10196]],
            1000.0,
            &plan.config,
        );
        assert_eq!(psd.power, expected.power);
        assert_eq!(plan.excluded.count, 1);
    }

    #[test]
    fn a_progressive_index_reports_frequency_per_index_unit_never_hertz() {
        let time = TimeAxis::Progressive {
            values: (0..1000).map(|n| n as f64 * 0.5).collect::<Vec<_>>().into(),
        };
        let plan = plan_psd(&time, 0..1000, &PsdSettings::default(), &roomy())
            .unwrap()
            .unwrap();
        assert_eq!(plan.frequency_unit, FrequencyUnit::PerIndexUnit);
        assert_eq!(plan.sample_rate, 2.0);
    }
}
