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

//! The PSD view (docs/SPEC.md §4.2, docs/ROADMAP.md M5 "PSD view").
//!
//! - **Computed only on request.** The "Compute PSD" button takes the
//!   interval the time view shows at that moment (zoom or box-select to
//!   choose it; "Fit to data" for the whole signal). Panning or zooming
//!   afterwards never starts a computation: on a large or spilled file each
//!   one reads every selected sample, so it is the user's call. A PSD that no
//!   longer matches the view or the settings stays on screen, marked as such.
//! - One spectrum per numeric series, overlaid on one plot or stacked on
//!   several, always sharing one synchronized frequency axis.
//! - Log/linear toggles on both axes.
//! - A "computed on" readout: samples, windows, window, overlap, Δf, sample
//!   rate, segments averaged, anything excluded, and the memory used against
//!   the PSD memory cap.
//!
//! Every decision about *what* is computed — and whether it fits the memory
//! cap — lives in `glyde_core::dsp::psd`; this module only renders its
//! answers and forwards the user's requests (docs/ARCHITECTURE.md Hard
//! rule 2).

use std::ops::Range;
use std::sync::Arc;

use egui_plot::{Legend, Line, Plot, PlotPoints};
use glyde_core::dsp::psd::{
    FrequencyUnit, PsdPlan, PsdSettings, PsdUnavailable, SegmentLength, OVERLAP_CHOICES,
    SEGMENT_LENGTH_CHOICES,
};
use glyde_core::dsp::welch::Psd;
use glyde_core::dsp::window::Window;
use glyde_core::ingest::Dataset;

use crate::plumbing::psd::{PsdJob, PsdOutcome, Spectrum};
use crate::views::time::series_color;

/// Height of one plot when spectra are stacked.
const STACKED_PLOT_HEIGHT: f32 = 140.0;

/// Overlaid on one plot, or one plot per series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Overlay,
    Stacked,
}

/// A finished computation, with what it was asked for.
struct Shown {
    selection: Range<usize>,
    settings: PsdSettings,
    outcome: PsdOutcome,
}

/// What the user asked the panel to do this frame, for the caller to carry
/// out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsdRequest {
    /// Move the time view onto these rows (the uniform stretch an
    /// `Irregular` series offers).
    ShowRows(Range<usize>),
}

/// A one-click way out of a refused PSD.
enum Alternative {
    ShowRows(Range<usize>),
    SegmentLength(usize),
}

/// The PSD view's state for the file currently open. A new file starts a
/// new panel (see `crate::app`).
pub struct PsdPanel {
    settings: PsdSettings,
    layout: Layout,
    log_frequency: bool,
    log_power: bool,
    job: Option<PsdJob>,
    shown: Option<Shown>,
}

impl Default for PsdPanel {
    fn default() -> Self {
        Self {
            settings: PsdSettings::default(),
            layout: Layout::Overlay,
            log_frequency: false,
            // Power spans orders of magnitude; a linear power axis flattens
            // everything but the strongest peak.
            log_power: true,
            job: None,
            shown: None,
        }
    }
}

impl PsdPanel {
    /// Collects a finished computation, if any. Returns `true` while one is
    /// still running, so the caller keeps repainting.
    pub fn poll(&mut self) -> bool {
        if let Some(job) = &self.job {
            if let Some(outcome) = job.try_outcome() {
                let (selection, settings) = job.request();
                self.shown = Some(Shown {
                    selection: selection.clone(),
                    settings: *settings,
                    outcome,
                });
                self.job = None;
            }
        }
        self.job.is_some()
    }

    /// Starts computing the PSD of `selection` under the current settings,
    /// replacing (and so cancelling) any computation still running.
    pub fn compute(&mut self, dataset: &Arc<Dataset>, selection: Range<usize>) {
        self.job = Some(PsdJob::spawn(Arc::clone(dataset), selection, self.settings));
    }

    /// Stops the running computation, if any; what was shown stays shown.
    pub fn cancel(&mut self) {
        if self.job.take().is_some() {
            tracing::info!("user cancelled the PSD computation");
        }
    }

    /// Renders the view. `selection` is the rows the time view shows right
    /// now — what "Compute PSD" would analyze.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        dataset: &Arc<Dataset>,
        selection: Option<Range<usize>>,
    ) -> Option<PsdRequest> {
        let mut request = None;

        ui.horizontal(|ui| {
            ui.strong("Power spectral density");
            ui.separator();
            if self.job.is_some() {
                if ui.button("Cancel").clicked() {
                    self.cancel();
                }
            } else {
                let compute = ui
                    .add_enabled(selection.is_some(), egui::Button::new("Compute PSD"))
                    .on_hover_text("Compute the spectrum of the interval the time view shows now");
                if let (true, Some(selection)) = (compute.clicked(), selection.clone()) {
                    self.compute(dataset, selection);
                }
            }
            ui.separator();
            ui.selectable_value(&mut self.layout, Layout::Overlay, "Overlay");
            ui.selectable_value(&mut self.layout, Layout::Stacked, "Stacked");
            ui.separator();
            ui.checkbox(&mut self.log_frequency, "Log frequency");
            ui.checkbox(&mut self.log_power, "Log power");
        });
        self.show_settings(ui);

        if let Some(job) = &self.job {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!(
                    "Computing PSD of the selected interval… {:.0}%",
                    job.progress() * 100.0
                ));
            });
            if job.is_streaming() {
                ui.label(
                    "The selection is larger than the PSD memory limit, so its samples are \
                     read progressively (streaming) rather than loaded at once.",
                );
            }
        }

        let Some(shown) = &self.shown else {
            if self.job.is_none() {
                ui.label(
                    "Zoom the time view to the interval you want (or use \"Fit to data\" for \
                     the whole signal), then press \"Compute PSD\".",
                );
            }
            return request;
        };

        if self.job.is_none()
            && (selection.as_ref() != Some(&shown.selection) || shown.settings != self.settings)
        {
            ui.colored_label(
                ui.visuals().weak_text_color(),
                "The time view or the settings changed since this PSD was computed — press \
                 \"Compute PSD\" to update it.",
            );
        }

        match &shown.outcome {
            PsdOutcome::Ready { plan, spectra } => {
                ui.label(computed_on(plan, spectra));
                ui.label(memory_note(plan));
                for line in missing_sample_notes(spectra) {
                    ui.label(line);
                }
                self.show_spectra(ui, plan, spectra);
            }
            PsdOutcome::Unavailable(unavailable) => {
                ui.colored_label(ui.visuals().warn_fg_color, unavailable.to_string());
                // The affordable alternative, one click away (SPEC §3.3, §5.1).
                let alternative = match unavailable {
                    PsdUnavailable::Irregular {
                        largest_uniform: Some(range),
                    } => Some((
                        "Show the largest uniform stretch".to_string(),
                        Alternative::ShowRows(range.clone()),
                    )),
                    PsdUnavailable::OverMemoryCap {
                        affordable_segment_len: Some(len),
                        ..
                    } => Some((
                        format!("Use {}-sample segments", group_thousands(*len)),
                        Alternative::SegmentLength(*len),
                    )),
                    _ => None,
                };
                if let Some((label, alternative)) = alternative {
                    if ui.button(label).clicked() {
                        match alternative {
                            Alternative::ShowRows(range) => {
                                tracing::info!(
                                    ?range,
                                    "user chose to view the offered uniform stretch"
                                );
                                request = Some(PsdRequest::ShowRows(range));
                            }
                            Alternative::SegmentLength(len) => {
                                tracing::info!(len, "user chose the affordable segment length");
                                self.settings.segment_length = SegmentLength::Fixed(len);
                            }
                        }
                    }
                }
            }
            PsdOutcome::NoNumericSeries => {
                ui.label("This file has no numeric series to take a spectrum of.");
            }
            PsdOutcome::Failed(message) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("PSD failed: {message}"),
                );
            }
        }
        request
    }

    /// The spectra currently shown, if the last computation produced any.
    pub fn spectra(&self) -> Option<&[Spectrum]> {
        match self.shown.as_ref().map(|shown| &shown.outcome) {
            Some(PsdOutcome::Ready { spectra, .. }) => Some(spectra),
            _ => None,
        }
    }

    /// SPEC §3.2: the three controls, behind one affordance, never required.
    fn show_settings(&mut self, ui: &mut egui::Ui) {
        let before = self.settings;
        egui::CollapsingHeader::new("PSD settings")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Window");
                    egui::ComboBox::from_id_salt("psd_window")
                        .selected_text(window_name(self.settings.window))
                        .show_ui(ui, |ui| {
                            for window in [Window::Hann, Window::Hamming, Window::Rectangular] {
                                ui.selectable_value(
                                    &mut self.settings.window,
                                    window,
                                    window_name(window),
                                );
                            }
                        });
                    ui.label("Segment length");
                    egui::ComboBox::from_id_salt("psd_segment_length")
                        .selected_text(segment_length_name(self.settings.segment_length))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.settings.segment_length,
                                SegmentLength::Auto,
                                segment_length_name(SegmentLength::Auto),
                            );
                            for len in SEGMENT_LENGTH_CHOICES {
                                let choice = SegmentLength::Fixed(len);
                                ui.selectable_value(
                                    &mut self.settings.segment_length,
                                    choice,
                                    segment_length_name(choice),
                                );
                            }
                        });
                    ui.label("Overlap");
                    egui::ComboBox::from_id_salt("psd_overlap")
                        .selected_text(percent(self.settings.overlap))
                        .show_ui(ui, |ui| {
                            for overlap in OVERLAP_CHOICES {
                                ui.selectable_value(
                                    &mut self.settings.overlap,
                                    overlap,
                                    percent(overlap),
                                );
                            }
                        });
                    if ui.button("Defaults").clicked() {
                        self.settings = PsdSettings::default();
                    }
                });
            });
        if self.settings != before {
            tracing::info!(settings = ?self.settings, "user changed the PSD settings");
        }
    }

    fn show_spectra(&self, ui: &mut egui::Ui, plan: &PsdPlan, spectra: &[Spectrum]) {
        let x_label = frequency_axis_label(plan.frequency_unit, self.log_frequency);
        let y_label = power_axis_label(plan.frequency_unit, self.log_power);
        let log_frequency = self.log_frequency;
        let log_power = self.log_power;
        let build = |id: egui::Id| {
            Plot::new(id)
                .x_axis_label(x_label.clone())
                .y_axis_label(y_label.clone())
                .x_axis_formatter(move |mark, _| axis_value(mark.value, log_frequency))
                .y_axis_formatter(move |mark, _| axis_value(mark.value, log_power))
                .label_formatter(move |name, point| {
                    format!(
                        "{name}\n{} · {}",
                        axis_value(point.x, log_frequency),
                        axis_value(point.y, log_power)
                    )
                })
                // SPEC §4.2: one synchronized frequency axis for every plot.
                .link_axis("psd_frequency_axis", [true, false])
                .link_cursor("psd_frequency_axis", [true, false])
        };

        match self.layout {
            Layout::Overlay => {
                build(egui::Id::new("psd_overlay"))
                    .legend(Legend::default())
                    .show(ui, |plot_ui| {
                        for (index, spectrum) in spectra.iter().enumerate() {
                            plot_ui.line(spectrum_line(spectrum, index, log_frequency, log_power));
                        }
                    });
            }
            Layout::Stacked => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (index, spectrum) in spectra.iter().enumerate() {
                        build(egui::Id::new(("psd_stacked", index)))
                            .height(STACKED_PLOT_HEIGHT)
                            .legend(Legend::default())
                            .show(ui, |plot_ui| {
                                plot_ui.line(spectrum_line(
                                    spectrum,
                                    index,
                                    log_frequency,
                                    log_power,
                                ));
                            });
                    }
                });
            }
        }
    }
}

/// One spectrum as a plot line, in the same color its series has in the time
/// view (both enumerate the numeric series in column order).
fn spectrum_line(
    spectrum: &Spectrum,
    index: usize,
    log_frequency: bool,
    log_power: bool,
) -> Line<'_> {
    Line::new(PlotPoints::new(plot_points(
        &spectrum.psd,
        log_frequency,
        log_power,
    )))
    .name(&spectrum.name)
    .color(series_color(index))
}

/// The bins of `psd` as plot coordinates. A log axis plots `log10` of the
/// value, so a bin that has no logarithm (zero frequency, zero or non-finite
/// power) is left out of that view — never clamped to a made-up value.
pub fn plot_points(psd: &Psd, log_frequency: bool, log_power: bool) -> Vec<[f64; 2]> {
    psd.freqs
        .iter()
        .zip(&psd.power)
        .filter_map(|(&f, &p)| {
            let x = if log_frequency {
                (f > 0.0).then(|| f.log10())?
            } else {
                f
            };
            let y = if log_power {
                (p > 0.0 && p.is_finite()).then(|| p.log10())?
            } else {
                p
            };
            Some([x, y])
        })
        .collect()
}

/// An axis value for display: the plotted coordinate itself on a linear
/// axis, `10^v` on a log one.
fn axis_value(v: f64, log: bool) -> String {
    format_number(if log { 10f64.powf(v) } else { v })
}

fn frequency_axis_label(unit: FrequencyUnit, log: bool) -> String {
    let unit = match unit {
        FrequencyUnit::Hertz => "Hz",
        FrequencyUnit::PerIndexUnit => "cycles per index unit",
    };
    format!("Frequency ({unit}){}", if log { ", log scale" } else { "" })
}

fn power_axis_label(unit: FrequencyUnit, log: bool) -> String {
    let unit = match unit {
        FrequencyUnit::Hertz => "unit²/Hz",
        FrequencyUnit::PerIndexUnit => "unit² per cycle/index unit",
    };
    format!("Power ({unit}){}", if log { ", log scale" } else { "" })
}

/// SPEC §4.2: "The plot always states what was computed on: number of
/// samples, segments, window, Δf" — plus SPEC §3.3's "averaged over N
/// segments" label and the segments it excluded.
pub fn computed_on(plan: &PsdPlan, spectra: &[Spectrum]) -> String {
    let unit = match plan.frequency_unit {
        FrequencyUnit::Hertz => " Hz",
        FrequencyUnit::PerIndexUnit => " per index unit",
    };
    let windows = spectra.first().map_or(0, |s| s.psd.segment_count);
    let delta_f = spectra.first().map_or(0.0, |s| s.psd.delta_f);
    let mut text = format!(
        "Computed on {} samples · {} window{} · {}, {} samples, {} overlap · Δf = {}{unit} · \
         sampling rate {}{unit}",
        group_thousands(plan.samples_used),
        group_thousands(windows),
        if windows == 1 { "" } else { "s" },
        window_name(plan.config.window),
        group_thousands(plan.window_len()),
        percent(plan.config.overlap),
        format_number(delta_f),
        format_number(plan.sample_rate),
    );
    if plan.is_segmented() {
        text.push_str(&format!(
            " · averaged over {} segment{} (no window crosses a gap)",
            plan.segment_count,
            if plan.segment_count == 1 { "" } else { "s" }
        ));
    }
    if plan.excluded.count > 0 {
        text.push_str(&format!(
            " · {} segment{} shorter than one window excluded ({} samples)",
            plan.excluded.count,
            if plan.excluded.count == 1 { "" } else { "s" },
            group_thousands(plan.excluded.samples)
        ));
    }
    text
}

/// The memory the computation was allowed and how it used it (SPEC §5.1):
/// the plan's estimated peak against the PSD memory cap.
pub fn memory_note(plan: &PsdPlan) -> String {
    format!(
        "Memory: at most {} of the {} PSD limit ({} series at a time)",
        megabytes(plan.memory.peak_bytes),
        megabytes(plan.memory.cap_bytes),
        plan.memory.concurrent_columns
    )
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// One note per series that had missing (non-finite) samples in the
/// selection: the windows touching them were skipped, never filled in.
pub fn missing_sample_notes(spectra: &[Spectrum]) -> Vec<String> {
    spectra
        .iter()
        .filter(|s| s.psd.non_finite_count > 0)
        .map(|s| {
            format!(
                "{}: {} missing sample{} in the selection — the windows containing them were \
                 skipped, not filled in.",
                s.name,
                group_thousands(s.psd.non_finite_count),
                if s.psd.non_finite_count == 1 { "" } else { "s" }
            )
        })
        .collect()
}

/// The rows of a time-ordered `ticks` axis whose tick lies within
/// `visible` (inclusive) — the time view's selection as sample indices. An
/// axis that is not time-ordered cannot be cut by time, so its selection is
/// every row (and the PSD then explains why it cannot be computed).
pub fn selection_rows(ticks: &[i128], sorted: bool, visible: (i128, i128)) -> Range<usize> {
    if !sorted {
        return 0..ticks.len();
    }
    let (lo, hi) = visible;
    let start = ticks.partition_point(|&t| t < lo);
    let end = ticks.partition_point(|&t| t <= hi).max(start);
    start..end
}

fn window_name(window: Window) -> &'static str {
    match window {
        Window::Hann => "Hann",
        Window::Hamming => "Hamming",
        Window::Rectangular => "Rectangular",
    }
}

fn segment_length_name(length: SegmentLength) -> String {
    match length {
        SegmentLength::Auto => "Auto".to_string(),
        SegmentLength::Fixed(len) => group_thousands(len),
    }
}

fn percent(fraction: f64) -> String {
    format!("{:.0}%", fraction * 100.0)
}

fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Four significant digits, switching to scientific notation outside
/// `[1e-3, 1e6)`.
fn format_number(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return format!("{v}");
    }
    let magnitude = v.abs();
    if !(1e-3..1e6).contains(&magnitude) {
        return format!("{v:.3e}");
    }
    let decimals = (3 - magnitude.log10().floor() as i32).max(0) as usize;
    let text = format!("{v:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyde_core::dsp::detrend::Detrend;
    use glyde_core::dsp::psd::{ExcludedSegments, PsdMemory};
    use glyde_core::dsp::welch::WelchConfig;
    use glyde_core::time::SamplingClass;

    fn plan(segment_count: usize, samples_used: usize, class: SamplingClass) -> PsdPlan {
        PsdPlan {
            selection: 0..20_000,
            sampling_class: class,
            sample_rate: 1000.0,
            frequency_unit: FrequencyUnit::Hertz,
            gap_threshold: 1e7,
            segment_count,
            samples_used,
            excluded: ExcludedSegments::default(),
            config: WelchConfig {
                window: Window::Hann,
                segment_len: 1024,
                overlap: 0.5,
                detrend: Detrend::Constant,
            },
            columns: 1,
            memory: PsdMemory {
                concurrent_columns: 1,
                peak_bytes: 12 * 1024 * 1024 + 512 * 1024,
                cap_bytes: 256 * 1024 * 1024,
            },
            larger_than_memory_cap: false,
        }
    }

    fn spectrum(windows: usize, non_finite: usize) -> Spectrum {
        Spectrum {
            name: "accel".to_string(),
            psd: Psd {
                freqs: vec![0.0, 0.9765625, 1.953125],
                power: vec![0.0, 2.0, 1e-30],
                delta_f: 0.9765625,
                segment_count: windows,
                non_finite_count: non_finite,
            },
        }
    }

    #[test]
    fn the_readout_states_samples_windows_window_and_delta_f() {
        let text = computed_on(&plan(1, 20_000, SamplingClass::Uniform), &[spectrum(38, 0)]);
        assert_eq!(
            text,
            "Computed on 20,000 samples · 38 windows · Hann, 1,024 samples, 50% overlap · \
             Δf = 0.9766 Hz · sampling rate 1000 Hz"
        );
    }

    #[test]
    fn a_segmented_readout_says_how_many_segments_it_averaged_and_what_it_excluded() {
        let mut plan = plan(3, 12_288, SamplingClass::SegmentedUniform);
        plan.excluded = ExcludedSegments {
            count: 2,
            samples: 300,
        };
        let text = computed_on(&plan, &[spectrum(21, 0)]);
        assert!(text.contains("averaged over 3 segments"), "{text}");
        assert!(
            text.contains("2 segments shorter than one window excluded (300 samples)"),
            "{text}"
        );
        assert!(text.starts_with("Computed on 12,288 samples"), "{text}");
    }

    #[test]
    fn the_memory_note_states_the_estimated_peak_against_the_cap() {
        assert_eq!(
            memory_note(&plan(1, 20_000, SamplingClass::Uniform)),
            "Memory: at most 12.5 MB of the 256.0 MB PSD limit (1 series at a time)"
        );
    }

    #[test]
    fn missing_samples_are_reported_per_series() {
        assert!(missing_sample_notes(&[spectrum(3, 0)]).is_empty());
        assert_eq!(
            missing_sample_notes(&[spectrum(3, 1)]),
            vec![
                "accel: 1 missing sample in the selection — the windows containing them were \
                  skipped, not filled in."
                    .to_string()
            ]
        );
    }

    #[test]
    fn log_axes_leave_out_bins_with_no_logarithm_rather_than_inventing_one() {
        let psd = spectrum(1, 0).psd;
        assert_eq!(plot_points(&psd, false, false).len(), 3);
        // Zero frequency has no log.
        assert_eq!(plot_points(&psd, true, false).len(), 2);
        // Zero power has no log; 1e-30 does.
        let log_power = plot_points(&psd, false, true);
        assert_eq!(log_power.len(), 2);
        assert_eq!(log_power[1], [1.953125, -30.0]);
    }

    #[test]
    fn selection_rows_are_the_samples_inside_the_visible_interval() {
        let ticks = [0, 10, 20, 30, 40];
        assert_eq!(selection_rows(&ticks, true, (10, 30)), 1..4);
        assert_eq!(selection_rows(&ticks, true, (11, 29)), 2..3);
        assert_eq!(selection_rows(&ticks, true, (-5, 100)), 0..5);
        assert_eq!(selection_rows(&ticks, true, (50, 60)), 5..5);
        assert_eq!(selection_rows(&ticks, false, (10, 30)), 0..5);
    }

    #[test]
    fn numbers_are_shown_to_four_significant_digits() {
        assert_eq!(format_number(1000.0), "1000");
        assert_eq!(format_number(0.9765625), "0.9766");
        assert_eq!(format_number(125.0), "125");
        assert_eq!(format_number(2.5e-7), "2.500e-7");
        assert_eq!(axis_value(2.0, true), "100");
    }

    #[test]
    fn group_thousands_inserts_separators() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(10_000_000), "10,000,000");
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use glyde_core::dsp::psd::{
        compute_psds, plan_psd_on, AxisScale, PsdMemoryCap, PSD_MEMORY_CAP_BYTES,
    };

    /// A real plan and spectrum: a 1 kHz axis with a 125 Hz tone.
    fn ready() -> PsdOutcome {
        let ticks: Vec<i128> = (0..8192i128).map(|n| n * 1_000_000).collect();
        let samples: Vec<f64> = (0..8192)
            .map(|n| (2.0 * std::f64::consts::PI * 125.0 * n as f64 / 1000.0).sin())
            .collect();
        let scale = AxisScale {
            ticks_per_unit: 1_000_000_000,
            frequency_unit: FrequencyUnit::Hertz,
        };
        let cap = PsdMemoryCap::from_bytes(PSD_MEMORY_CAP_BYTES);
        let plan = plan_psd_on(&ticks[..], scale, 0..8192, &PsdSettings::default(), 2, cap)
            .unwrap()
            .unwrap();
        let psd = compute_psds(&ticks[..], &[&samples[..]], &plan, &|_| true)
            .unwrap()
            .unwrap()
            .remove(0);
        let spectra = vec![
            Spectrum {
                name: "a".to_string(),
                psd: psd.clone(),
            },
            Spectrum {
                name: "b".to_string(),
                psd,
            },
        ];
        PsdOutcome::Ready { plan, spectra }
    }

    fn panel_showing(outcome: PsdOutcome, layout: Layout) -> PsdPanel {
        PsdPanel {
            layout,
            shown: Some(Shown {
                selection: 0..8192,
                settings: PsdSettings::default(),
                outcome,
            }),
            ..PsdPanel::default()
        }
    }

    fn tiny_dataset() -> Arc<Dataset> {
        use glyde_core::ingest::TimeAxis;
        use glyde_core::series::{Series, SeriesValues};
        Arc::new(Dataset {
            time: TimeAxis::Progressive {
                values: vec![0.0, 1.0].into(),
            },
            time_column_name: "row".to_string(),
            columns: vec![Series::new("v", SeriesValues::F64(vec![0.0, 1.0]))],
        })
    }

    /// Renders `panel` as if the time view showed rows `0..8192` — the same
    /// interval the shown PSD was computed on, unless `selection` says
    /// otherwise.
    fn render_with(
        panel: &mut PsdPanel,
        selection: Option<Range<usize>>,
    ) -> (egui::FullOutput, Option<PsdRequest>) {
        let ctx = egui::Context::default();
        let dataset = tiny_dataset();
        let mut request = None;
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                request = panel.show(ui, &dataset, selection.clone());
            });
        });
        (output, request)
    }

    fn render(panel: &mut PsdPanel) -> (egui::FullOutput, Option<PsdRequest>) {
        render_with(panel, Some(0..8192))
    }

    #[test]
    fn overlaid_spectra_render_without_panicking() {
        let (output, offered) = render(&mut panel_showing(ready(), Layout::Overlay));
        assert!(offered.is_none());
        insta::assert_debug_snapshot!("psd_view_overlay_shape_count", output.shapes.len());
    }

    #[test]
    fn stacked_spectra_render_one_plot_each_without_panicking() {
        let (output, _) = render(&mut panel_showing(ready(), Layout::Stacked));
        insta::assert_debug_snapshot!("psd_view_stacked_shape_count", output.shapes.len());
    }

    #[test]
    fn linear_axes_render_without_panicking() {
        let mut panel = panel_showing(ready(), Layout::Overlay);
        panel.log_power = false;
        panel.log_frequency = false;
        let (output, _) = render(&mut panel);
        assert!(!output.shapes.is_empty());
        let mut panel = panel_showing(ready(), Layout::Overlay);
        panel.log_frequency = true;
        let (output, _) = render(&mut panel);
        assert!(!output.shapes.is_empty());
    }

    #[test]
    fn an_irregular_series_renders_its_explanation_instead_of_a_plot() {
        let outcome = PsdOutcome::Unavailable(PsdUnavailable::Irregular {
            largest_uniform: Some(3000..5000),
        });
        let (output, offered) = render(&mut panel_showing(outcome, Layout::Overlay));
        assert!(
            offered.is_none(),
            "nothing is offered until the button is clicked"
        );
        insta::assert_debug_snapshot!("psd_view_irregular_shape_count", output.shapes.len());
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use glyde_core::ingest::TimeAxis;
    use glyde_core::series::{Series, SeriesValues};
    use std::time::{Duration, Instant};

    fn dataset() -> Arc<Dataset> {
        Arc::new(Dataset {
            time: TimeAxis::Progressive {
                values: (0..4096).map(f64::from).collect::<Vec<_>>().into(),
            },
            time_column_name: "row".to_string(),
            columns: vec![Series::new(
                "v",
                SeriesValues::F64((0..4096).map(|n| (n as f64).sin()).collect()),
            )],
        })
    }

    fn finish(panel: &mut PsdPanel) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while panel.poll() {
            assert!(Instant::now() < deadline, "PSD never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn render(panel: &mut PsdPanel, dataset: &Arc<Dataset>, selection: Range<usize>) -> String {
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                panel.show(ui, dataset, Some(selection.clone()));
            });
        });
        format!("{:?}", output.shapes)
    }

    #[test]
    fn nothing_is_computed_until_the_user_asks() {
        let dataset = dataset();
        let mut panel = PsdPanel::default();
        // Many frames with a moving time view: no computation starts.
        for end in (100..4096).step_by(100) {
            assert!(!panel.poll());
            render(&mut panel, &dataset, 0..end);
            assert!(panel.job.is_none(), "rendering must never start a PSD");
        }
        assert!(panel.shown.is_none());
    }

    #[test]
    fn computing_takes_the_interval_given_and_shows_its_result() {
        let dataset = dataset();
        let mut panel = PsdPanel::default();
        panel.compute(&dataset, 1000..3000);
        assert_eq!(
            panel.job.as_ref().map(|job| job.request().0.clone()),
            Some(1000..3000)
        );
        finish(&mut panel);
        let shown = panel.shown.as_ref().expect("a result");
        assert_eq!(shown.selection, 1000..3000);
        assert!(panel.spectra().is_some());
    }

    #[test]
    fn moving_the_view_afterwards_keeps_the_result_and_never_recomputes() {
        let dataset = dataset();
        let mut panel = PsdPanel::default();
        panel.compute(&dataset, 0..4096);
        finish(&mut panel);

        let text = render(&mut panel, &dataset, 0..2000);
        assert!(panel.job.is_none());
        assert_eq!(panel.shown.as_ref().unwrap().selection, 0..4096);
        assert!(
            text.contains("changed since this PSD was computed"),
            "a PSD that no longer matches the view must say so"
        );
        let text = render(&mut panel, &dataset, 0..4096);
        assert!(!text.contains("changed since this PSD was computed"));
    }

    #[test]
    fn a_settings_change_marks_the_result_out_of_date_without_recomputing() {
        let dataset = dataset();
        let mut panel = PsdPanel::default();
        panel.compute(&dataset, 0..4096);
        finish(&mut panel);

        panel.settings.window = Window::Hamming;
        let text = render(&mut panel, &dataset, 0..4096);
        assert!(panel.job.is_none());
        assert!(text.contains("changed since this PSD was computed"));
    }

    #[test]
    fn cancelling_stops_the_job_and_keeps_what_was_shown() {
        let dataset = dataset();
        let mut panel = PsdPanel::default();
        panel.compute(&dataset, 0..4096);
        finish(&mut panel);
        panel.compute(&dataset, 0..2048);
        panel.cancel();
        assert!(panel.job.is_none());
        assert_eq!(panel.shown.as_ref().unwrap().selection, 0..4096);
    }
}
