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

//! The inference bar (docs/ARCHITECTURE.md's "InferenceReport (what +
//! confidence) -> UI inference bar", docs/ROADMAP.md M4 "Inference bar
//! widget: persistent and discreet; opens expanded when any inference is
//! low-confidence"): every SPEC §1.2 mandatory field — encoding, delimiter,
//! decimal separator, time column, timestamp format, sample count, sampling
//! classification — rendered from a real
//! [`glyde_core::ingest::InferenceReport`], with a "(low confidence)"
//! caveat wherever the underlying inference could not be settled
//! unambiguously (Golden Rule 2: silent guessing is a bug).
//!
//! Collapsed is the default (discreet: it never crowds out the plot for a
//! clean file), but [`InferenceReport::has_low_confidence_field`] — decided
//! in `glyde-core`, not here, per docs/ARCHITECTURE.md's Hard rule 2 — opens
//! it expanded the moment any field could not be settled unambiguously, so
//! nothing is ever guessed silently (Golden Rule 2). One-click correction of
//! each field is a separate, later roadmap item.

use glyde_core::ingest::{Confidence, InferenceReport};
use std::path::Path;

/// Renders `report` as a collapsible header into `ui`, one row of field
/// labels when expanded. `open_id_source` (the file path) keys the
/// collapsed/expanded state so each newly opened file gets its own fresh
/// default — a low-confidence file always opens expanded even if the
/// previous file's bar was left collapsed, and vice versa.
pub fn show(ui: &mut egui::Ui, report: &InferenceReport, open_id_source: &Path) {
    let low_confidence = report.has_low_confidence_field();
    egui::CollapsingHeader::new(header_text(report, low_confidence))
        .id_salt(open_id_source)
        .default_open(low_confidence)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                field_label(
                    ui,
                    "encoding",
                    &report.encoding.value,
                    report.encoding.confidence,
                );
                field_label(
                    ui,
                    "delimiter",
                    display_option(&report.delimiter.value),
                    report.delimiter.confidence,
                );
                field_label(
                    ui,
                    "decimal separator",
                    display_option(&report.decimal_separator.value),
                    report.decimal_separator.confidence,
                );
                field_label(
                    ui,
                    "time column",
                    display_option(&report.time_column.value),
                    report.time_column.confidence,
                );
                field_label(
                    ui,
                    "timestamp format",
                    display_option(&report.timestamp_format.value),
                    report.timestamp_format.confidence,
                );
                ui.label(format!("{} samples", report.sample_count));
                ui.label(format!("sampling: {:?}", report.sampling_class));
            });
        });
}

/// The header shown whether the bar is collapsed or expanded — discreet
/// (just "Inference") unless something needs the user's attention, in which
/// case the header itself flags it so a collapsed bar is never mistaken for
/// "everything was unambiguous" (Golden Rule 2).
fn header_text(report: &InferenceReport, low_confidence: bool) -> String {
    if low_confidence {
        format!(
            "Inference — {} sample{}, {:?} (low confidence — review below)",
            report.sample_count,
            if report.sample_count == 1 { "" } else { "s" },
            report.sampling_class
        )
    } else {
        format!(
            "Inference — {} sample{}, {:?}",
            report.sample_count,
            if report.sample_count == 1 { "" } else { "s" },
            report.sampling_class
        )
    }
}

/// `value`, or a placeholder for a field with no value at all (e.g. a
/// progressive index has no timestamp format) — never an empty label a user
/// could mistake for a rendering bug.
fn display_option(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("—")
}

fn field_label(ui: &mut egui::Ui, name: &str, value: &str, confidence: Confidence) {
    ui.label(field_text(name, value, confidence));
}

/// The label text for one field (split out from [`field_label`] so the
/// SPEC §1.2 "never silently guess" caveat is unit-testable without a real
/// `egui::Ui`).
fn field_text(name: &str, value: &str, confidence: Confidence) -> String {
    match confidence {
        Confidence::High => format!("{name}: {value}"),
        Confidence::Low => format!("{name}: {value} (low confidence)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyde_core::ingest::{InferredField, SamplingClass};

    fn sample_report(timestamp_format_confidence: Confidence) -> InferenceReport {
        InferenceReport {
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
                confidence: timestamp_format_confidence,
            },
            sample_count: 42,
            sampling_class: SamplingClass::Uniform,
        }
    }

    fn render_shape_count(report: &InferenceReport) -> usize {
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, report, Path::new("/some/file.csv"));
            });
        });
        output.shapes.len()
    }

    // Crash-free rendering (SPEC §6) exercised through a real
    // `egui::Context`, the same pattern `views::time`'s render tests use.
    #[test]
    fn show_renders_a_report_without_panicking() {
        assert!(render_shape_count(&sample_report(Confidence::Low)) > 0);
    }

    // SPEC §1.2 "opens expanded when any inference is low-confidence": on a
    // fresh context (no prior collapsed/expanded state persisted for this
    // file), a low-confidence report must render its field row, while an
    // all-high-confidence report must stay collapsed to the header alone.
    #[test]
    fn low_confidence_report_opens_expanded_by_default() {
        let collapsed = render_shape_count(&sample_report(Confidence::High));
        let expanded = render_shape_count(&sample_report(Confidence::Low));

        assert!(
            expanded > collapsed,
            "a low-confidence report must draw more than just the collapsed header \
             (collapsed: {collapsed} shapes, expanded: {expanded} shapes)"
        );
    }

    #[test]
    fn display_option_shows_a_placeholder_for_none() {
        assert_eq!(display_option(&None), "—");
        assert_eq!(display_option(&Some("x".to_string())), "x");
    }

    // The exact SPEC §1.2 requirement: a low-confidence inference must never
    // be presented indistinguishably from a confident one.
    #[test]
    fn field_text_flags_low_confidence_and_high_confidence_does_not() {
        assert_eq!(
            field_text("delimiter", ",", Confidence::High),
            "delimiter: ,"
        );
        assert_eq!(
            field_text("delimiter", ",", Confidence::Low),
            "delimiter: , (low confidence)"
        );
    }

    // A collapsed bar's header text is the only thing a user sees without
    // clicking, so it must itself flag low confidence — never silently
    // collapse away the fact that something needs review (Golden Rule 2).
    #[test]
    fn header_text_flags_low_confidence_and_high_confidence_does_not() {
        let high = sample_report(Confidence::High);
        let low = sample_report(Confidence::Low);

        assert!(!header_text(&high, high.has_low_confidence_field()).contains("low confidence"));
        assert!(header_text(&low, low.has_low_confidence_field()).contains("low confidence"));
    }
}
