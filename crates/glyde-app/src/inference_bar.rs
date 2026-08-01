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
//! nothing is ever guessed silently (Golden Rule 2).
//!
//! docs/ROADMAP.md M4 "One-click correction of each field → triggers a
//! re-index" (docs/SPEC.md §1.2): the delimiter and decimal separator each
//! get a correction dropdown, and an ambiguous day/month timestamp format
//! gets the exact "one-click swap" SPEC §2.1's ambiguity rule describes. A
//! click returns a [`Correction`] out of [`show`]; `crate::app::GlydeApp`
//! turns it into a re-open of the current file with the matching
//! [`glyde_core::ingest::IngestOverrides`] field set — this module only ever
//! renders and reports the click, it never re-indexes anything itself
//! (docs/ARCHITECTURE.md Hard rule 2: no product logic in the UI layer).
//! Encoding and time-column correction are not covered yet (issue #97).
//!
//! **Every correction control is gated to its own field's confidence**: a
//! high-confidence field renders as a plain label, nothing more. A clean,
//! unambiguous file (the common case) never shows a single correctable
//! control — there is nothing to correct, and offering one anyway invites
//! fiddling with a setting that already works (maintainer review: "I don't
//! want to leave the user the possibility to change something that breaks
//! the app or is useless in most cases"). This mirrors the one rule the
//! date-order swap already followed by construction (it only ever appeared
//! for the ambiguous `DD/MM`/`MM/DD` case); delimiter and decimal separator
//! now follow the same rule explicitly.
//!
//! [`Delimiter::Whitespace`] is deliberately absent from the delimiter
//! correction options. Unlike every other candidate, it does not fail
//! predictably when wrong: a byte delimiter simply absent from the file
//! collapses every row to one field and reports `SingleColumnFile` quickly,
//! but whitespace is present in almost every real file (quoted text,
//! multi-word categorical values), so an incorrect whitespace split can
//! produce a wildly inflated, content-dependent field count instead. That
//! usually — but not provably always, since the affordability check only
//! samples a bounded head sample (SPEC §1.2) — still routes to the budgeted
//! spill path rather than exceeding the RAM budget outright, but it is a
//! probabilistic safety net, not a guarantee, and even the guaranteed case
//! commits to a slow, unindicated full-file scan before reporting back.
//! Not worth offering as a one-click option.

use glyde_core::ingest::{Confidence, DecimalSeparator, Delimiter, InferenceReport, InferredField};
use glyde_core::time::TimestampFormat;
use std::path::Path;

/// A one-click field correction requested from the inference bar. See the
/// module docs for how `crate::app::GlydeApp` acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Correction {
    Delimiter(Delimiter),
    DecimalSeparator(DecimalSeparator),
    TimestampFormat(TimestampFormat),
}

/// Renders `report` as a collapsible header into `ui`, one row of field
/// labels when expanded, returning the correction the user clicked this
/// frame, if any. `open_id_source` (the file path) keys the
/// collapsed/expanded state so each newly opened file gets its own fresh
/// default — a low-confidence file always opens expanded even if the
/// previous file's bar was left collapsed, and vice versa.
pub fn show(
    ui: &mut egui::Ui,
    report: &InferenceReport,
    open_id_source: &Path,
) -> Option<Correction> {
    let low_confidence = report.has_low_confidence_field();
    let response = egui::CollapsingHeader::new(header_text(report, low_confidence))
        .id_salt(open_id_source)
        .default_open(low_confidence)
        .show(ui, |ui| {
            let mut correction = None;
            ui.horizontal_wrapped(|ui| {
                field_label(
                    ui,
                    "encoding",
                    &report.encoding.value,
                    report.encoding.confidence,
                );
                if let Some(picked) = delimiter_control(ui, &report.delimiter) {
                    correction = Some(Correction::Delimiter(picked));
                }
                if let Some(picked) = decimal_separator_control(ui, &report.decimal_separator) {
                    correction = Some(Correction::DecimalSeparator(picked));
                }
                field_label(
                    ui,
                    "time column",
                    display_option(&report.time_column.value),
                    report.time_column.confidence,
                );
                if let Some(picked) = timestamp_format_control(ui, &report.timestamp_format) {
                    correction = Some(Correction::TimestampFormat(picked));
                }
                ui.label(format!("{} samples", report.sample_count));
                ui.label(format!("sampling: {:?}", report.sampling_class));
            });
            correction
        });
    response.body_returned.flatten()
}

/// Every delimiter a user can pick in the correction dropdown, in the same
/// priority order `glyde_core::ingest::infer_delimiter` tries them.
/// [`Delimiter::Whitespace`] is deliberately excluded — see the module docs.
const DELIMITER_OPTIONS: [Delimiter; 4] = [
    Delimiter::Comma,
    Delimiter::Semicolon,
    Delimiter::Tab,
    Delimiter::Pipe,
];

fn delimiter_display(delimiter: Delimiter) -> &'static str {
    match delimiter {
        Delimiter::Comma => "Comma (,)",
        Delimiter::Semicolon => "Semicolon (;)",
        Delimiter::Tab => "Tab",
        Delimiter::Pipe => "Pipe (|)",
        Delimiter::Whitespace => "Whitespace",
    }
}

/// The delimiter label text renders as (`Delimiter::as_str`, via
/// `CsvParseOutcome`), read back into the typed value the dropdown needs to
/// mark the current selection — `None` only for a value this UI has never
/// produced itself (defensive; every inferred label is one of
/// [`DELIMITER_OPTIONS`]).
fn delimiter_from_label(label: &str) -> Option<Delimiter> {
    DELIMITER_OPTIONS
        .into_iter()
        .find(|delimiter| delimiter.as_str() == label)
}

/// Renders the delimiter field's label, plus a correction dropdown when (and
/// only when) [`Confidence::Low`] — a confidently-inferred delimiter is
/// almost always right, and offering a control next to it anyway would
/// invite fiddling with a setting that already works (see the module docs).
/// Returns the newly picked delimiter if the user chose one different from
/// the current value this frame (docs/ROADMAP.md M4, SPEC §1.2).
fn delimiter_control(
    ui: &mut egui::Ui,
    field: &InferredField<Option<String>>,
) -> Option<Delimiter> {
    let current = field.value.as_deref().and_then(delimiter_from_label);
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.label(field_text(
            "delimiter",
            display_option(&field.value),
            field.confidence,
        ));
        if field.confidence == Confidence::Low {
            egui::ComboBox::from_id_salt("inference-bar-delimiter-correction")
                .selected_text("Correct…")
                .show_ui(ui, |ui| {
                    for candidate in DELIMITER_OPTIONS {
                        let is_current = current == Some(candidate);
                        if ui
                            .selectable_label(is_current, delimiter_display(candidate))
                            .clicked()
                            && !is_current
                        {
                            picked = Some(candidate);
                        }
                    }
                });
        }
    });
    picked
}

fn decimal_separator_display(separator: DecimalSeparator) -> &'static str {
    match separator {
        DecimalSeparator::Dot => "Dot (.)",
        DecimalSeparator::Comma => "Comma (,)",
    }
}

fn decimal_separator_from_label(label: &str) -> Option<DecimalSeparator> {
    [DecimalSeparator::Dot, DecimalSeparator::Comma]
        .into_iter()
        .find(|separator| separator.as_str() == label)
}

/// [`delimiter_control`], for the decimal separator field — same
/// low-confidence-only gating.
fn decimal_separator_control(
    ui: &mut egui::Ui,
    field: &InferredField<Option<String>>,
) -> Option<DecimalSeparator> {
    let current = field
        .value
        .as_deref()
        .and_then(decimal_separator_from_label);
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.label(field_text(
            "decimal separator",
            display_option(&field.value),
            field.confidence,
        ));
        if field.confidence == Confidence::Low {
            egui::ComboBox::from_id_salt("inference-bar-decimal-separator-correction")
                .selected_text("Correct…")
                .show_ui(ui, |ui| {
                    for candidate in [DecimalSeparator::Dot, DecimalSeparator::Comma] {
                        let is_current = current == Some(candidate);
                        if ui
                            .selectable_label(is_current, decimal_separator_display(candidate))
                            .clicked()
                            && !is_current
                        {
                            picked = Some(candidate);
                        }
                    }
                });
        }
    });
    picked
}

/// SPEC §2.1's ambiguity-rule UX exactly: "mark the inference low
/// confidence, and open the inference bar expanded with a one-click swap."
/// The swap only appears when confidence is [`Confidence::Low`] *and* the
/// detected format is one half of the `DD/MM`/`MM/DD` ambiguity — a
/// day-first or month-first read settled by a genuine field > 12 (corpus
/// cases 26/27) is confidently correct and gets no swap button, same as
/// every other high-confidence field. Every other timestamp format has no
/// day/month reading to swap in the first place (issue #97 tracks
/// correcting to an arbitrary format).
fn timestamp_format_control(
    ui: &mut egui::Ui,
    field: &InferredField<Option<String>>,
) -> Option<TimestampFormat> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.label(field_text(
            "timestamp format",
            display_option(&field.value),
            field.confidence,
        ));
        if field.confidence == Confidence::Low {
            let swap = match field.value.as_deref() {
                Some("dd_mm_yyyy") => Some(("Swap to MM/DD", TimestampFormat::MonthFirst)),
                Some("mm_dd_yyyy") => Some(("Swap to DD/MM", TimestampFormat::DayFirst)),
                _ => None,
            };
            if let Some((label, target)) = swap {
                if ui.button(label).clicked() {
                    picked = Some(target);
                }
            }
        }
    });
    picked
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

    // docs/ROADMAP.md M4: rendering a frame with no user interaction must
    // never report a correction — only an actual click does.
    #[test]
    fn show_reports_no_correction_when_nothing_was_clicked() {
        let report = sample_report(Confidence::Low);
        let ctx = egui::Context::default();
        let mut correction = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                correction = show(ui, &report, Path::new("/some/file.csv"));
            });
        });

        assert_eq!(correction, None);
    }

    // Every delimiter label the inference bar can display must read back to
    // the exact same `Delimiter` value — the round trip the correction
    // dropdown depends on to mark the current selection.
    #[test]
    fn delimiter_from_label_round_trips_every_delimiter_s_own_label() {
        for delimiter in DELIMITER_OPTIONS {
            assert_eq!(delimiter_from_label(delimiter.as_str()), Some(delimiter));
        }
    }

    #[test]
    fn delimiter_from_label_is_none_for_an_unrecognized_label() {
        assert_eq!(delimiter_from_label("?"), None);
    }

    #[test]
    fn decimal_separator_from_label_round_trips_every_separator_s_own_label() {
        for separator in [DecimalSeparator::Dot, DecimalSeparator::Comma] {
            assert_eq!(
                decimal_separator_from_label(separator.as_str()),
                Some(separator)
            );
        }
    }

    #[test]
    fn decimal_separator_from_label_is_none_for_an_unrecognized_label() {
        assert_eq!(decimal_separator_from_label("?"), None);
    }

    // docs/ROADMAP.md M4 follow-up (maintainer review: "I don't want to
    // leave the user the possibility to change something that breaks the
    // app or is useless in most cases"): a confidently-inferred field must
    // render as a plain label only — no correction control at all. `time_column`
    // (which has no control of its own) is forced Low in both variants so the
    // bar stays expanded either way, isolating exactly what the field under
    // test's own confidence adds to the render.
    #[test]
    fn delimiter_correction_control_only_renders_when_the_field_is_low_confidence() {
        let mut high = sample_report(Confidence::High);
        high.time_column.confidence = Confidence::Low;
        let mut low = high.clone();
        low.delimiter.confidence = Confidence::Low;

        let high_shapes = render_shape_count(&high);
        let low_shapes = render_shape_count(&low);

        assert!(
            low_shapes > high_shapes,
            "a low-confidence delimiter must render a correction control a high-confidence \
             one does not (high: {high_shapes} shapes, low: {low_shapes} shapes)"
        );
    }

    #[test]
    fn decimal_separator_correction_control_only_renders_when_the_field_is_low_confidence() {
        let mut high = sample_report(Confidence::High);
        high.time_column.confidence = Confidence::Low;
        let mut low = high.clone();
        low.decimal_separator.confidence = Confidence::Low;

        let high_shapes = render_shape_count(&high);
        let low_shapes = render_shape_count(&low);

        assert!(
            low_shapes > high_shapes,
            "a low-confidence decimal separator must render a correction control a \
             high-confidence one does not (high: {high_shapes} shapes, low: {low_shapes} shapes)"
        );
    }

    // Corpus cases 26/27's shape: the format genuinely is `DayFirst`, but a
    // field > 12 confidently settled it — that must render no swap button,
    // unlike case 28's genuinely ambiguous `DayFirst` fallback.
    #[test]
    fn timestamp_format_swap_button_only_renders_when_the_field_is_low_confidence() {
        let mut high = sample_report(Confidence::High);
        high.time_column.confidence = Confidence::Low;
        high.timestamp_format.value = Some("dd_mm_yyyy".to_string());
        let mut low = high.clone();
        low.timestamp_format.confidence = Confidence::Low;

        let high_shapes = render_shape_count(&high);
        let low_shapes = render_shape_count(&low);

        assert!(
            low_shapes > high_shapes,
            "a genuinely ambiguous day-first read must render the swap button; a confidently \
             disambiguated one (corpus cases 26/27) must not (high: {high_shapes} shapes, \
             low: {low_shapes} shapes)"
        );
    }

    // The OOM investigation on PR #98 found `Whitespace` uniquely unsafe: it
    // doesn't fail predictably the way an absent byte delimiter does, so it
    // must never be offered as a correction target, low-confidence or not.
    #[test]
    fn delimiter_options_excludes_whitespace() {
        assert!(!DELIMITER_OPTIONS.contains(&Delimiter::Whitespace));
    }
}
