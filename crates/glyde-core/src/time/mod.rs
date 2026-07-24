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

//! Time index: absolute-timestamp parsing/formatting and gap detection
//! (docs/SPEC.md §2, docs/ARCHITECTURE.md workspace layout `time/`).
//!
//! Timestamp parsing/formatting for [`TimestampFormat::Iso8601WithOffset`],
//! [`TimestampFormat::Iso8601Naive`], the four epoch formats,
//! [`TimestampFormat::DayFirst`]/[`TimestampFormat::MonthFirst`],
//! [`TimestampFormat::LabViewEpoch`], and [`TimestampFormat::ExcelSerial`] —
//! plus [`infer_timestamp_format`], which also recognizes when a column is a
//! progressive numeric index rather than an absolute timestamp, and resolves
//! the day-vs-month ambiguity per SPEC §2.1 — landed with earlier
//! docs/ROADMAP.md M2 items. [`detect_gaps`] and [`classify_sampling`] (SPEC
//! §2.2–2.3) land with M2's "Sampling classification" item.
//! [`TimestampFormat::DateTimeSpace`] is a separate, not-yet-started M2
//! roadmap item and stays `todo!()`. Never widen a golden test's tolerance or
//! change its expectations to make an implementation pass — if one looks
//! wrong, that is a `blocking-decision` issue, not an edit.
//!
//! `Timestamp` stores ticks as `i128`, never `f64` seconds (SPEC §2.1) — the
//! type system enforces Golden Rule 1 (never degrade the raw data) at the
//! signature level, the same way `dsp::welch` enforces "raw samples only" by
//! never taking a pyramid type.

mod format;
mod gap;

pub use format::{
    format_timestamp, infer_timestamp_format, parse_timestamp, TimeUnit, Timestamp,
    TimestampFormat, TimestampFormatInference,
};
pub use gap::{classify_sampling, detect_gaps, Gap, SamplingClass};

/// Reads a `testdata/corpus/` fixture's raw column text, shared by this
/// module's own test suites (`format.rs`'s timestamp-format tests and
/// `gap.rs`'s sampling-classification tests both exercise real corpus text
/// rather than hand-picked fixtures).
#[cfg(test)]
pub(crate) mod corpus_fixture {
    use std::path::{Path, PathBuf};

    fn corpus_path(file_name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("testdata")
            .join("corpus")
            .join(file_name)
    }

    /// Every raw-text field of `column_name` in `file_name`'s data rows, in
    /// row order. Every corpus case used here is a plain comma-delimited
    /// file with no quoted fields, so a straightforward split on the
    /// inferred delimiter is sufficient (the quote-aware tokenizer lives in
    /// `ingest::infer` and is exercised by that module's own tests).
    pub(crate) fn corpus_column(file_name: &str, column_name: &str) -> Vec<String> {
        let bytes = std::fs::read(corpus_path(file_name))
            .unwrap_or_else(|e| panic!("read {file_name}: {e}"));
        let encoding = crate::ingest::detect_encoding(&bytes);
        let text = crate::ingest::decode(&bytes, &encoding);
        let delimiter = crate::ingest::infer_delimiter(&text).delimiter;
        let header = crate::ingest::infer_header(&text, delimiter);
        let column_index = header
            .column_names
            .iter()
            .position(|name| name == column_name)
            .unwrap_or_else(|| panic!("column '{column_name}' not found in {file_name}"));
        let data_start = header
            .header_row_index
            .map_or(header.skipped_preamble_rows, |index| index + 1);

        text.lines()
            .skip(data_start)
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.split(delimiter.as_str())
                    .nth(column_index)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
            .collect()
    }
}
