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
//! confidence) -> UI inference bar", docs/ROADMAP.md M4 "InferenceReport
//! surfaced to the UI"): every SPEC §1.2 mandatory field — encoding,
//! delimiter, decimal separator, time column, timestamp format, sample
//! count, sampling classification — rendered from a real
//! [`glyde_core::ingest::InferenceReport`], with a "(low confidence)"
//! caveat wherever the underlying inference could not be settled
//! unambiguously (Golden Rule 2: silent guessing is a bug).
//!
//! This is deliberately just the first M4 checkbox: a persistent, always-
//! expanded row. The second checkbox — a discreet bar that stays collapsed
//! until a low-confidence field opens it, plus one-click correction — is a
//! separate, later roadmap item.

use glyde_core::ingest::{Confidence, InferenceReport};

/// Renders `report`'s fields as one row of labels into `ui`.
pub fn show(ui: &mut egui::Ui, report: &InferenceReport) {
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
/// `egui::Ui`). A future PR (docs/ROADMAP.md M4's second checkbox) styles
/// low confidence distinctly and auto-expands the bar; this PR only
/// guarantees the caveat is never missing from the text.
fn field_text(name: &str, value: &str, confidence: Confidence) -> String {
    match confidence {
        Confidence::High => format!("{name}: {value}"),
        Confidence::Low => format!("{name}: {value} (low confidence)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> InferenceReport {
        use glyde_core::ingest::{InferredField, SamplingClass};

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
                confidence: Confidence::Low,
            },
            sample_count: 42,
            sampling_class: SamplingClass::Uniform,
        }
    }

    // Crash-free rendering (SPEC §6) exercised through a real
    // `egui::Context`, the same pattern `views::time`'s render tests use.
    #[test]
    fn show_renders_a_report_without_panicking() {
        let report = sample_report();
        let ctx = egui::Context::default();

        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, &report);
            });
        });

        assert!(!output.shapes.is_empty(), "must draw something");
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
}
