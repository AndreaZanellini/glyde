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
//! PSD, *what exactly* it is computed on, and *how much memory* computing it
//! will take — before a single sample is read.
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
//! # The memory cap
//!
//! A PSD never holds more than [`PsdMemoryCap`] bytes, whatever the
//! selection's length, the number of gaps in it, or the number of series
//! (SPEC §5.1: "checks affordability before acting, never after"). Nothing
//! here grows with the selection: planning only counts segments (they are
//! enumerated again on the fly while computing, never stored), and each
//! column's samples are streamed through one analysis window. What remains
//! is bounded and accounted for in [`PsdMemory`]:
//!
//! - a fixed planning overhead (the bounded-memory median's counters and the
//!   tick read buffers);
//! - each column's result (its spectrum and running average), held until
//!   the job ends;
//! - each column *being computed*: one window's buffers, FFT plan and
//!   scratch, and its read buffers.
//!
//! The plan fixes how many columns are computed at once so that the total
//! stays under the cap, and refuses — naming the largest segment length that
//! would fit — when not even one column at a time does. A counting-allocator
//! test (`tests/psd_memory_cap.rs`) proves the real peak never exceeds the
//! estimate.

use std::fmt;
use std::ops::Range;

use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use tracing::{info, warn};

use super::detrend::Detrend;
use super::welch::{
    default_segment_length, is_standard_length, plan_bytes_upper_bound, welch_source,
    LengthWeightedAverage, Psd, WelchConfig, DEFAULT_OVERLAP, MAX_SEGMENT_LEN, MIN_SEGMENT_LEN,
};
use super::window::Window;
use crate::budget::RamBudget;
use crate::ingest::{TimeAxis, PROGRESSIVE_TICK_SCALE};
use crate::series::SampleSource;
use crate::time::{for_each_segment, is_uniform_range, scan_range, SamplingClass, TickSource};
use crate::{GlydeError, Result};

/// Every segment length the PSD settings offer: the powers of two the
/// software's own default can pick (SPEC §3.2's `[256, 65536]` clamp).
pub const SEGMENT_LENGTH_CHOICES: [usize; 9] =
    [256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];

/// Every overlap fraction the PSD settings offer.
pub const OVERLAP_CHOICES: [f64; 4] = [0.0, 0.25, 0.5, 0.75];

/// Fewer selected samples than this has no spectrum to speak of (a single
/// sample has no Δt, so no sampling rate either).
pub const MIN_PSD_SAMPLES: usize = 2;

/// The most memory a PSD computation may ever hold: 256 MiB, or the
/// application's whole RAM budget on a machine where that is smaller.
pub const PSD_MEMORY_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Planning's fixed overhead: the bounded-memory median's 2¹⁶ counters
/// (512 KiB), the tick read buffer of a spilled axis and the conversion
/// buffer of a progressive one (1 MiB each), rounded up for the bookkeeping
/// around them.
const PLANNING_BYTES: u64 = 4 * 1024 * 1024;

/// Read buffers one column in flight may hold at once: the spill reader's
/// buffer and the dtype-conversion buffer (1 MiB each, see
/// `series::SAMPLE_CHUNK_LEN`), plus a tick read buffer for enumerating a
/// segmented selection's runs.
const READ_BUFFERS_PER_COLUMN: u64 = 3 * 1024 * 1024;

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

/// The ceiling on what a PSD computation may hold (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsdMemoryCap {
    bytes: u64,
}

impl PsdMemoryCap {
    /// [`PSD_MEMORY_CAP_BYTES`], lowered to `budget`'s cap if that is smaller.
    pub fn for_budget(budget: &RamBudget) -> Self {
        Self::from_bytes(PSD_MEMORY_CAP_BYTES.min(budget.cap_bytes()))
    }

    /// An explicit cap, e.g. for a test.
    pub fn from_bytes(bytes: u64) -> Self {
        Self { bytes }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// What a planned PSD will hold at its peak, by part (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsdMemory {
    /// How many columns are computed at the same time.
    pub concurrent_columns: usize,
    /// The estimated peak: planning + every result + the columns in flight.
    pub peak_bytes: u64,
    pub cap_bytes: u64,
}

impl PsdMemory {
    /// The peak for `columns` series at window length `len`, computing
    /// `concurrent` of them at a time.
    pub fn estimate(len: usize, columns: usize, concurrent: usize, cap: PsdMemoryCap) -> Self {
        Self::estimate_for(len, len, columns, concurrent, cap)
    }

    /// [`Self::estimate`] when the window actually used is `window_len` —
    /// shorter than `len` for a uniform selection shorter than one segment.
    fn estimate_for(
        len: usize,
        window_len: usize,
        columns: usize,
        concurrent: usize,
        cap: PsdMemoryCap,
    ) -> Self {
        Self {
            concurrent_columns: concurrent,
            peak_bytes: fixed_bytes(len, window_len, columns)
                + concurrent as u64 * in_flight_bytes(len),
            cap_bytes: cap.bytes(),
        }
    }

    /// The most columns at a time that keep `columns` series at window
    /// length `len` under `cap` — at most `parallelism` — or `None` when not
    /// even one does.
    fn fit(len: usize, columns: usize, parallelism: usize, cap: PsdMemoryCap) -> Option<Self> {
        Self::fit_for(len, len, columns, parallelism, cap)
    }

    /// [`Self::fit`] for a window of `window_len` samples (see
    /// [`Self::estimate_for`]).
    fn fit_for(
        len: usize,
        window_len: usize,
        columns: usize,
        parallelism: usize,
        cap: PsdMemoryCap,
    ) -> Option<Self> {
        let room = cap
            .bytes()
            .checked_sub(fixed_bytes(len, window_len, columns))?;
        let concurrent = (room / in_flight_bytes(len)).min(parallelism.min(columns).max(1) as u64);
        (concurrent >= 1)
            .then(|| Self::estimate_for(len, window_len, columns, concurrent as usize, cap))
    }
}

/// What a job holds from start to end: planning's overhead, every column's
/// result, and — when the window length is not one of the standard lengths
/// planned at startup — the one FFT plan all its columns share.
fn fixed_bytes(len: usize, window_len: usize, columns: usize) -> u64 {
    let on_demand_plan = if is_standard_length(window_len) {
        0
    } else {
        plan_bytes_upper_bound(window_len)
    };
    PLANNING_BYTES + columns as u64 * result_bytes(len) + on_demand_plan
}

/// Bytes one column holds for the whole job at window length `len`: its
/// running length-weighted average and its finished spectrum (frequencies and
/// power, `len / 2 + 1` bins of 8 bytes each), with room for the average's
/// power array and the spectrum's to coexist while one becomes the other.
fn result_bytes(len: usize) -> u64 {
    (len / 2 + 1) as u64 * 8 * 3
}

/// Bytes one column holds only while it is being computed, at window length
/// `len`: the window being filled, its detrend buffer and the window
/// coefficients (8 bytes per sample each); the complex spectrum, the FFT's
/// scratch and its twiddle factors (16 bytes per sample each); the running sum
/// of periodograms and one finished per-segment estimate (8 + 16 bytes per
/// bin); and its read buffers.
fn in_flight_bytes(len: usize) -> u64 {
    len as u64 * (3 * 8 + 3 * 16) + (len / 2 + 1) as u64 * (8 + 16) + READ_BUFFERS_PER_COLUMN
}

/// Whether a PSD of `columns` series with `len`-sample segments fits under
/// `cap` at all (one series at a time) — what the PSD settings use to
/// disable the segment lengths that would not, before anything is computed.
pub fn segment_length_fits(len: usize, columns: usize, cap: PsdMemoryCap) -> bool {
    PsdMemory::fit(len, columns, 1, cap).is_some()
}

/// The longest of [`SEGMENT_LENGTH_CHOICES`] that fits `columns` series
/// under `cap`, or `None` when not even the shortest does.
pub fn largest_affordable_segment_len(columns: usize, cap: PsdMemoryCap) -> Option<usize> {
    SEGMENT_LENGTH_CHOICES
        .iter()
        .rev()
        .copied()
        .find(|&len| segment_length_fits(len, columns, cap))
}

/// The least memory a PSD of `columns` series with `len`-sample segments
/// can take (one series at a time), for explaining why a length is not
/// offered.
pub fn minimum_peak_bytes(len: usize, columns: usize) -> u64 {
    PsdMemory::estimate(len, columns, 1, PsdMemoryCap::from_bytes(u64::MAX)).peak_bytes
}

/// Running count of the memory a PSD computation has claimed, checked
/// against the cap *before* each claim: the guard behind the plan's
/// estimate. A claim over the cap is refused with
/// [`GlydeError::PsdMemoryLimit`], which stops the computation — it never
/// takes the application down.
struct MemoryLedger {
    cap_bytes: u64,
    used: AtomicU64,
}

/// A claim on a [`MemoryLedger`], given back when dropped.
struct Claim<'a> {
    ledger: &'a MemoryLedger,
    bytes: u64,
}

impl MemoryLedger {
    fn new(cap_bytes: u64) -> Self {
        Self {
            cap_bytes,
            used: AtomicU64::new(0),
        }
    }

    fn claim(&self, bytes: u64) -> Result<Claim<'_>> {
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let needed = used.saturating_add(bytes);
            if needed > self.cap_bytes {
                warn!(
                    needed_bytes = needed,
                    cap_bytes = self.cap_bytes,
                    "PSD stopped: it would have gone over the PSD memory cap"
                );
                return Err(GlydeError::PsdMemoryLimit {
                    needed_bytes: needed,
                    cap_bytes: self.cap_bytes,
                });
            }
            match self.used.compare_exchange_weak(
                used,
                needed,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(Claim {
                        ledger: self,
                        bytes,
                    })
                }
                Err(actual) => used = actual,
            }
        }
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.ledger.used.fetch_sub(self.bytes, Ordering::Relaxed);
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

/// Exactly what a PSD will be computed on, and how much memory it will take,
/// decided before any sample is read. Holds no per-segment list: its size is
/// the same for any selection.
#[derive(Debug, Clone, PartialEq)]
pub struct PsdPlan {
    /// The selected rows, as asked for.
    pub selection: Range<usize>,
    /// The selection's sampling class (SPEC §2.2) — never `Irregular` here.
    pub sampling_class: SamplingClass,
    /// Samples per [`FrequencyUnit`] unit: `1 / median Δt` of the selection.
    pub sample_rate: f64,
    pub frequency_unit: FrequencyUnit,
    /// The selection's gap threshold in ticks (SPEC §2.2–2.3), with which
    /// [`compute_psds`] enumerates the same runs planning counted.
    pub gap_threshold: f64,
    /// Gap-free runs that enter the estimate (1 for a uniform selection).
    pub segment_count: usize,
    /// Samples that enter the estimate (excluded runs not counted).
    pub samples_used: usize,
    pub excluded: ExcludedSegments,
    pub config: WelchConfig,
    /// How many series the plan was made for.
    pub columns: usize,
    pub memory: PsdMemory,
    /// The selection's raw samples, all series together, are larger than the
    /// memory cap: they are only ever read progressively — which is how they
    /// are read anyway — and the user is told so (SPEC §5.1's example: "PSD
    /// over the full 8-hour range needs streaming — computing progressively").
    pub larger_than_memory_cap: bool,
    /// `Some(default)` when [`SegmentLength::Auto`]'s default length did not
    /// fit the memory cap and a shorter one was used instead.
    pub segment_len_reduced_from: Option<usize>,
}

impl PsdPlan {
    /// Whether this is SPEC §3.3's per-segment average rather than one
    /// uniform run.
    pub fn is_segmented(&self) -> bool {
        self.sampling_class == SamplingClass::SegmentedUniform
    }

    /// The analysis window length actually used: the configured segment
    /// length, or the whole selection when a uniform selection is shorter.
    pub fn window_len(&self) -> usize {
        if self.is_segmented() {
            self.config.segment_len
        } else {
            self.config.segment_len.min(self.samples_used)
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
    /// Even one series at a time would go over the PSD memory cap.
    OverMemoryCap {
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
            PsdUnavailable::OverMemoryCap {
                requested_bytes,
                cap_bytes,
                affordable_segment_len,
            } => {
                write!(
                    f,
                    "This PSD would need {} of memory, over its {} limit.",
                    mebibytes(*requested_bytes),
                    mebibytes(*cap_bytes)
                )?;
                match affordable_segment_len {
                    Some(len) => write!(
                        f,
                        " A {len}-sample segment length fits — choose it in PSD settings."
                    ),
                    None => write!(f, " No segment length fits for this many series."),
                }
            }
        }
    }
}

fn mebibytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// [`plan_psd_on`] for a [`TimeAxis`], with its own [`AxisScale`].
pub fn plan_psd(
    time: &TimeAxis,
    selection: Range<usize>,
    settings: &PsdSettings,
    columns: usize,
    cap: PsdMemoryCap,
) -> Result<std::result::Result<PsdPlan, PsdUnavailable>> {
    plan_psd_on(time, AxisScale::of(time), selection, settings, columns, cap)
}

/// Decides what a PSD of `selection` (row indices into `ticks`) of `columns`
/// series is computed on, in bounded memory over the ticks alone — see the
/// module docs.
pub fn plan_psd_on<T: TickSource + ?Sized>(
    ticks: &T,
    scale: AxisScale,
    selection: Range<usize>,
    settings: &PsdSettings,
    columns: usize,
    cap: PsdMemoryCap,
) -> Result<std::result::Result<PsdPlan, PsdUnavailable>> {
    let end = selection.end.min(ticks.tick_count());
    let selection = selection.start.min(end)..end;
    if selection.len() < MIN_PSD_SAMPLES {
        return Ok(Err(PsdUnavailable::TooFewSamples {
            available: selection.len(),
        }));
    }

    // Runs are only counted here, never collected.
    let mut run_count = 0usize;
    let mut longest = 0usize;
    let scan = scan_range(ticks, selection.clone(), &mut |run| {
        run_count += 1;
        longest = longest.max(run.len());
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
    let gap_threshold = scan.gap_threshold().expect("a median Δt exists");

    let segmented = sampling_class == SamplingClass::SegmentedUniform;
    let parallelism = rayon::current_num_threads();
    let mut segment_len_reduced_from = None;
    let segment_len = match settings.segment_length {
        SegmentLength::Auto => {
            let default = default_segment_length(if segmented { longest } else { selection.len() });
            // The software's own default never asks for more memory than the
            // cap allows: it steps down to the longest length that fits, and
            // says so (the readout shows the length actually used).
            match largest_affordable_segment_len(columns, cap) {
                Some(affordable) if affordable < default => {
                    info!(
                        default,
                        affordable,
                        columns,
                        cap_bytes = cap.bytes(),
                        "PSD default segment length reduced to fit the PSD memory cap"
                    );
                    segment_len_reduced_from = Some(default);
                    affordable
                }
                _ => default,
            }
        }
        SegmentLength::Fixed(len) => len.clamp(MIN_SEGMENT_LEN, MAX_SEGMENT_LEN),
    };

    let (segment_count, samples_used, excluded) = if segmented {
        // A second, median-free pass now that the window length is known.
        let mut kept = 0usize;
        let mut samples = 0usize;
        let mut excluded = ExcludedSegments::default();
        for_each_segment(ticks, selection.clone(), gap_threshold, &mut |run| {
            if run.len() >= segment_len {
                kept += 1;
                samples += run.len();
            } else {
                excluded.count += 1;
                excluded.samples += run.len();
            }
            Ok(())
        })?;
        if kept == 0 {
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
        (kept, samples, excluded)
    } else {
        (1, selection.len(), ExcludedSegments::default())
    };

    let window_len = if segmented {
        segment_len
    } else {
        segment_len.min(selection.len())
    };
    let Some(memory) = PsdMemory::fit_for(segment_len, window_len, columns, parallelism, cap)
    else {
        let affordable_segment_len = SEGMENT_LENGTH_CHOICES
            .iter()
            .rev()
            .copied()
            .filter(|&len| len < segment_len)
            .find(|&len| PsdMemory::fit(len, columns, parallelism, cap).is_some());
        let requested_bytes =
            PsdMemory::estimate_for(segment_len, window_len, columns, 1, cap).peak_bytes;
        info!(
            requested_bytes,
            cap_bytes = cap.bytes(),
            columns,
            segment_len,
            ?affordable_segment_len,
            "PSD refused before computing: over the PSD memory cap (SPEC §5.1)"
        );
        return Ok(Err(PsdUnavailable::OverMemoryCap {
            requested_bytes,
            cap_bytes: cap.bytes(),
            affordable_segment_len,
        }));
    };

    let raw_bytes = (selection.len() as u64)
        .saturating_mul(columns as u64)
        .saturating_mul(std::mem::size_of::<f64>() as u64);
    let plan = PsdPlan {
        selection,
        sampling_class,
        sample_rate,
        frequency_unit: scale.frequency_unit,
        gap_threshold,
        segment_count,
        samples_used,
        excluded,
        config: WelchConfig {
            window: settings.window,
            segment_len,
            overlap: settings.overlap,
            detrend: Detrend::Constant,
        },
        columns,
        memory,
        larger_than_memory_cap: raw_bytes > cap.bytes(),
        segment_len_reduced_from,
    };
    info!(
        selection = ?plan.selection,
        sampling_class = ?plan.sampling_class,
        sample_rate = plan.sample_rate,
        segments = plan.segment_count,
        excluded_segments = plan.excluded.count,
        excluded_samples = plan.excluded.samples,
        window = ?plan.config.window,
        segment_len = plan.config.segment_len,
        overlap = plan.config.overlap,
        columns,
        concurrent_columns = plan.memory.concurrent_columns,
        peak_bytes = plan.memory.peak_bytes,
        cap_bytes = plan.memory.cap_bytes,
        larger_than_memory_cap = plan.larger_than_memory_cap,
        "PSD planned (SPEC §3.2–3.3, §5.1)"
    );
    Ok(Ok(plan))
}

/// The longest gap-delimited run of the whole series that is itself uniform
/// (SPEC §2.2) and holds at least one minimum-length window — what an
/// `Irregular` series offers instead of a PSD (SPEC §3.3). Constant memory:
/// runs are visited, and each is tested for uniformity only if it would beat
/// the best found so far, so each run is tested at most once. The earliest of
/// equally long runs wins.
fn largest_uniform_run<T: TickSource + ?Sized>(ticks: &T) -> Result<Option<Range<usize>>> {
    let whole = 0..ticks.tick_count();
    let Some(threshold) = scan_range(ticks, whole.clone(), &mut |_| Ok(()))?.gap_threshold() else {
        return Ok(None);
    };
    let mut best: Option<Range<usize>> = None;
    for_each_segment(ticks, whole, threshold, &mut |run| {
        let longer = run.len() > best.as_ref().map_or(MIN_SEGMENT_LEN - 1, |b| b.len());
        if longer && is_uniform_range(ticks, run.clone())? {
            best = Some(run);
        }
        Ok(())
    })?;
    Ok(best)
}

/// Runs `plan` over `columns` — the raw samples of each series, in order,
/// sharing the time axis `ticks` — streaming, at most
/// [`PsdMemory::concurrent_columns`] at a time so the plan's memory estimate
/// holds (see the module docs).
///
/// `on_progress` is called with each newly read batch of samples (counted
/// across all columns) and cancels the whole computation by returning
/// `false`, in which case this returns `Ok(None)` — never a partial result.
pub fn compute_psds<T, S>(
    ticks: &T,
    columns: &[S],
    plan: &PsdPlan,
    on_progress: &(dyn Fn(usize) -> bool + Sync),
) -> Result<Option<Vec<Psd>>>
where
    T: TickSource + Sync + ?Sized,
    S: SampleSource + Sync,
{
    let len = plan.config.segment_len;
    let ledger = MemoryLedger::new(plan.memory.cap_bytes);
    // Planning's overhead and every column's result are held for the whole
    // job; each column in flight claims its own working set on top.
    let _held = ledger.claim(fixed_bytes(len, plan.window_len(), columns.len()))?;
    let mut spectra = Vec::new();
    spectra
        .try_reserve_exact(columns.len())
        .map_err(|_| GlydeError::OutOfMemory {
            requested_bytes: (columns.len() * std::mem::size_of::<Psd>()) as u64,
        })?;
    for batch in columns.chunks(plan.memory.concurrent_columns.max(1)) {
        let results: Vec<Result<Option<Psd>>> = batch
            .par_iter()
            .map(|samples| {
                let _working_set = ledger.claim(in_flight_bytes(len))?;
                compute_column(ticks, samples, plan, on_progress)
            })
            .collect();
        for result in results {
            match result? {
                Some(psd) => spectra.push(psd),
                None => return Ok(None),
            }
        }
    }
    Ok(Some(spectra))
}

/// One column of [`compute_psds`].
fn compute_column<T, S>(
    ticks: &T,
    samples: &S,
    plan: &PsdPlan,
    on_progress: &(dyn Fn(usize) -> bool + Sync),
) -> Result<Option<Psd>>
where
    T: TickSource + ?Sized,
    S: SampleSource + ?Sized,
{
    // `welch_source` reports a running total per call; `on_progress` wants
    // what is new since the last report.
    let reporter = || {
        let mut reported = 0usize;
        move |read: usize| {
            let fresh = read - reported;
            reported = read;
            on_progress(fresh)
        }
    };

    if !plan.is_segmented() {
        return welch_source(
            samples,
            plan.selection.clone(),
            plan.sample_rate,
            &plan.config,
            &mut reporter(),
        );
    }

    // SPEC §3.3: each gap-free run on its own (no window ever crosses a
    // gap), folded into the length-weighted average as soon as it is done.
    let mut average = LengthWeightedAverage::default();
    let mut cancelled = false;
    for_each_segment(
        ticks,
        plan.selection.clone(),
        plan.gap_threshold,
        &mut |run| {
            if cancelled || run.len() < plan.config.segment_len {
                return Ok(());
            }
            let len = run.len();
            match welch_source(
                samples,
                run,
                plan.sample_rate,
                &plan.config,
                &mut reporter(),
            )? {
                Some(psd) => average.add(len, psd),
                None => cancelled = true,
            }
            Ok(())
        },
    )?;
    if cancelled {
        return Ok(None);
    }
    Ok(Some(average.finish(plan.sample_rate, &plan.config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::welch::{welch, welch_segmented};

    const SECONDS: AxisScale = AxisScale {
        ticks_per_unit: 1_000_000_000,
        frequency_unit: FrequencyUnit::Hertz,
    };

    fn roomy() -> PsdMemoryCap {
        PsdMemoryCap::from_bytes(PSD_MEMORY_CAP_BYTES)
    }

    /// `len` ticks at `period_ns`, starting at `start_ns`.
    fn run(start_ns: i128, period_ns: i128, len: usize) -> Vec<i128> {
        (0..len as i128).map(|n| start_ns + n * period_ns).collect()
    }

    fn plan(
        ticks: &[i128],
        selection: Range<usize>,
    ) -> std::result::Result<PsdPlan, PsdUnavailable> {
        plan_psd_on(
            ticks,
            SECONDS,
            selection,
            &PsdSettings::default(),
            1,
            roomy(),
        )
        .unwrap()
    }

    fn compute_one(ticks: &[i128], samples: &[f64], plan: &PsdPlan) -> Psd {
        compute_psds(ticks, &[samples], plan, &|_| true)
            .unwrap()
            .unwrap()
            .remove(0)
    }

    #[test]
    fn a_uniform_selection_is_one_segment_at_the_rate_its_timestamps_imply() {
        let ticks = run(0, 1_000_000, 10_000); // 1 kHz
        let plan = plan(&ticks, 0..10_000).unwrap();

        assert_eq!(plan.sampling_class, SamplingClass::Uniform);
        assert_eq!(plan.segment_count, 1);
        assert_eq!(plan.samples_used, 10_000);
        assert_eq!(plan.sample_rate, 1000.0);
        assert_eq!(plan.config.segment_len, 1024); // largest 2^k ≤ 10000/8
        assert_eq!(plan.config.window, Window::Hann);
        assert_eq!(plan.config.overlap, 0.5);
        assert_eq!(plan.config.detrend, Detrend::Constant);
        assert_eq!(plan.excluded, ExcludedSegments::default());
        assert!(!plan.larger_than_memory_cap);
    }

    #[test]
    fn only_the_selected_rows_are_planned() {
        let ticks = run(0, 1_000_000, 10_000);
        let plan = plan(&ticks, 2_000..6_000).unwrap();
        assert_eq!(plan.selection, 2_000..6_000);
        assert_eq!(plan.samples_used, 4_000);
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
        assert_eq!(plan.segment_count, 3);
        assert_eq!(plan.samples_used, 3 * 4096);
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
    fn the_plans_memory_estimate_is_under_the_cap_by_construction() {
        let ticks = run(0, 1_000_000, 1_000_000);
        let cap = PsdMemoryCap::from_bytes(64 * 1024 * 1024);
        let plan = plan_psd_on(
            &ticks[..],
            SECONDS,
            0..ticks.len(),
            &PsdSettings::default(),
            50,
            cap,
        )
        .unwrap()
        .unwrap();
        assert!(plan.memory.peak_bytes <= cap.bytes());
        assert!(plan.memory.concurrent_columns >= 1);
        assert_eq!(plan.memory.cap_bytes, cap.bytes());
    }

    #[test]
    fn a_tight_cap_lowers_how_many_columns_are_computed_at_once() {
        let ticks = run(0, 1_000_000, 1_000_000);
        let settings = PsdSettings {
            segment_length: SegmentLength::Fixed(65536),
            ..PsdSettings::default()
        };
        let one_at_a_time = PsdMemory::estimate(65536, 8, 1, roomy()).peak_bytes;
        let cap = PsdMemoryCap::from_bytes(one_at_a_time);
        let plan = plan_psd_on(&ticks[..], SECONDS, 0..ticks.len(), &settings, 8, cap)
            .unwrap()
            .unwrap();
        assert_eq!(plan.memory.concurrent_columns, 1);
        assert_eq!(plan.memory.peak_bytes, one_at_a_time);
    }

    #[test]
    fn a_psd_over_the_cap_is_refused_before_computing_with_an_affordable_length() {
        let ticks = run(0, 1_000_000, 1_000_000);
        let settings = PsdSettings {
            segment_length: SegmentLength::Fixed(65536),
            ..PsdSettings::default()
        };
        // Exactly enough for one column at a 1024-sample window.
        let cap = PsdMemoryCap::from_bytes(PsdMemory::estimate(1024, 1, 1, roomy()).peak_bytes);

        let refused = plan_psd_on(&ticks[..], SECONDS, 0..ticks.len(), &settings, 1, cap)
            .unwrap()
            .unwrap_err();

        assert_eq!(
            refused,
            PsdUnavailable::OverMemoryCap {
                requested_bytes: PsdMemory::estimate(65536, 1, 1, cap).peak_bytes,
                cap_bytes: cap.bytes(),
                affordable_segment_len: Some(1024),
            }
        );
        assert!(refused
            .to_string()
            .contains("1024-sample segment length fits"));
    }

    #[test]
    fn the_cap_never_exceeds_the_applications_ram_budget() {
        let small = RamBudget::from_total_ram_bytes(400 * 1024 * 1024); // cap 100 MiB
        assert_eq!(PsdMemoryCap::for_budget(&small).bytes(), small.cap_bytes());
        let large = RamBudget::from_total_ram_bytes(64 * 1024 * 1024 * 1024);
        assert_eq!(
            PsdMemoryCap::for_budget(&large).bytes(),
            PSD_MEMORY_CAP_BYTES
        );
    }

    #[test]
    fn a_selection_larger_than_the_cap_is_planned_as_streaming_not_refused() {
        let ticks = run(0, 1_000_000, 4_000_000);
        let cap = PsdMemoryCap::from_bytes(PsdMemory::estimate(65536, 1, 1, roomy()).peak_bytes);
        let plan = plan_psd_on(
            &ticks[..],
            SECONDS,
            0..ticks.len(),
            &PsdSettings::default(),
            1,
            cap,
        )
        .unwrap()
        .unwrap();
        assert!(plan.larger_than_memory_cap);
    }

    #[test]
    fn compute_runs_a_uniform_plan_exactly_like_welch_on_the_selected_slice() {
        let ticks = run(0, 1_000_000, 20_000);
        let samples: Vec<f64> = (0..20_000).map(|n| (n as f64 * 0.3).sin()).collect();
        let plan = plan(&ticks, 1_000..19_000).unwrap();

        let psd = compute_one(&ticks, &samples, &plan);

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

        let psd = compute_one(&ticks, &samples, &plan);

        let expected = welch_segmented(
            &[&samples[0..4096], &samples[4196..10196]],
            1000.0,
            &plan.config,
        );
        assert_eq!(psd.power, expected.power);
        assert_eq!(plan.excluded.count, 1);
    }

    #[test]
    fn every_column_gets_its_own_spectrum_in_order_whatever_the_concurrency() {
        let ticks = run(0, 1_000_000, 8192);
        let columns: Vec<Vec<f64>> = (1..=5)
            .map(|k| {
                (0..8192)
                    .map(|n| (n as f64 * 0.01 * k as f64).sin())
                    .collect()
            })
            .collect();
        let slices: Vec<&[f64]> = columns.iter().map(Vec::as_slice).collect();
        let mut plan = plan_psd_on(
            &ticks[..],
            SECONDS,
            0..8192,
            &PsdSettings::default(),
            5,
            roomy(),
        )
        .unwrap()
        .unwrap();

        let parallel = compute_psds(&ticks[..], &slices, &plan, &|_| true)
            .unwrap()
            .unwrap();
        plan.memory.concurrent_columns = 1;
        let serial = compute_psds(&ticks[..], &slices, &plan, &|_| true)
            .unwrap()
            .unwrap();

        for (k, (p, s)) in parallel.iter().zip(&serial).enumerate() {
            assert_eq!(p.power, s.power);
            assert_eq!(p.power, welch(slices[k], 1000.0, &plan.config).power);
        }
    }

    #[test]
    fn progress_counts_every_sample_read_and_can_cancel() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let ticks = run(0, 1_000_000, 300_000);
        let samples: Vec<f64> = (0..300_000).map(|n| (n as f64).sin()).collect();
        let plan = plan(&ticks, 0..300_000).unwrap();

        let read = AtomicUsize::new(0);
        compute_psds(&ticks[..], &[&samples[..]], &plan, &|n| {
            read.fetch_add(n, Ordering::Relaxed);
            true
        })
        .unwrap()
        .unwrap();
        assert_eq!(read.load(Ordering::Relaxed), 300_000);

        let cancelled = compute_psds(&ticks[..], &[&samples[..]], &plan, &|_| false).unwrap();
        assert!(cancelled.is_none());
    }

    #[test]
    fn a_computation_that_would_exceed_the_cap_stops_with_an_error_instead_of_crashing() {
        let ticks = run(0, 1_000_000, 100_000);
        let columns: Vec<Vec<f64>> = (0..4).map(|_| vec![1.0; 100_000]).collect();
        let slices: Vec<&[f64]> = columns.iter().map(Vec::as_slice).collect();
        let mut plan = plan_psd_on(
            &ticks[..],
            SECONDS,
            0..100_000,
            &PsdSettings::default(),
            4,
            roomy(),
        )
        .unwrap()
        .unwrap();
        // An estimate that turned out wrong: the guard must still hold.
        plan.memory.cap_bytes = plan.memory.peak_bytes / 2;

        let stopped = compute_psds(&ticks[..], &slices, &plan, &|_| true);

        match stopped {
            Err(GlydeError::PsdMemoryLimit {
                needed_bytes,
                cap_bytes,
            }) => assert!(needed_bytes > cap_bytes),
            other => panic!("expected the memory-limit error, got {other:?}"),
        }
    }

    #[test]
    fn the_guard_refuses_any_claim_that_would_go_over_the_cap_and_gives_memory_back() {
        let ledger = MemoryLedger::new(1000);
        let first = ledger.claim(600).expect("within the cap");
        let refused = ledger.claim(500).err().expect("600 + 500 is over 1000");
        assert!(matches!(
            refused,
            GlydeError::PsdMemoryLimit {
                needed_bytes: 1100,
                cap_bytes: 1000
            }
        ));
        drop(first);
        let _again = ledger.claim(1000).expect("released claims are given back");
    }

    #[test]
    fn the_settings_know_in_advance_which_segment_lengths_fit() {
        let cap = PsdMemoryCap::from_bytes(minimum_peak_bytes(4096, 10));
        assert!(segment_length_fits(4096, 10, cap));
        assert!(segment_length_fits(256, 10, cap));
        assert!(!segment_length_fits(8192, 10, cap));
        assert_eq!(largest_affordable_segment_len(10, cap), Some(4096));
        let tiny = PsdMemoryCap::from_bytes(1024);
        assert_eq!(largest_affordable_segment_len(10, tiny), None);
    }

    #[test]
    fn auto_steps_its_default_down_to_the_longest_length_that_fits_and_says_so() {
        let ticks = run(0, 1_000_000, 4_000_000); // default would be 65536
        let cap = PsdMemoryCap::from_bytes(minimum_peak_bytes(8192, 20));

        let plan = plan_psd_on(
            &ticks[..],
            SECONDS,
            0..ticks.len(),
            &PsdSettings::default(),
            20,
            cap,
        )
        .unwrap()
        .unwrap();

        assert_eq!(plan.config.segment_len, 8192);
        assert_eq!(plan.segment_len_reduced_from, Some(65536));
        assert!(plan.memory.peak_bytes <= cap.bytes());
    }

    #[test]
    fn a_progressive_index_reports_frequency_per_index_unit_never_hertz() {
        let time = TimeAxis::Progressive {
            values: (0..1000).map(|n| n as f64 * 0.5).collect::<Vec<_>>().into(),
        };
        let plan = plan_psd(&time, 0..1000, &PsdSettings::default(), 1, roomy())
            .unwrap()
            .unwrap();
        assert_eq!(plan.frequency_unit, FrequencyUnit::PerIndexUnit);
        assert_eq!(plan.sample_rate, 2.0);
    }
}
