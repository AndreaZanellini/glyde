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

//! Readers and the format registry (docs/ARCHITECTURE.md §Hard rule 5,
//! docs/SPEC.md §1.1, docs/ROADMAP.md M2).
//!
//! This is routing only: which registered [`Reader`] claims a path. Actual
//! streaming parse, encoding/delimiter/decimal inference, and dtype detection
//! land with each reader's own module as their own roadmap items.

mod csv;
mod infer;

pub use csv::{open_path, parse, CsvParseOutcome, CsvReader};
pub use infer::{
    decode, detect_encoding, infer_decimal_separator, infer_delimiter, infer_header,
    DecimalSeparator, DecimalSeparatorInference, Delimiter, DelimiterInference, EncodingInference,
    EncodingSource, HeaderInference, HEAD_SAMPLE_BYTES,
};

use crate::{GlydeError, Result};
use std::path::Path;

/// A single ingested source format (ARCH hard rule 5): adding a format means
/// adding one `ingest/<format>.rs` implementing this trait plus one registry
/// entry in [`Registry::default_registry`] — nothing else changes.
pub trait Reader: Send + Sync {
    /// Human-readable format name, used in logs and the inference bar.
    fn format_name(&self) -> &'static str;

    /// Cheap recognition test: does this reader claim `path`? Extension-based
    /// for now; content sniffing lands with each reader's own parse logic.
    fn recognizes(&self, path: &Path) -> bool;
}

/// Every reader `glyde-core` knows about, tried in registration order.
pub struct Registry {
    readers: Vec<Box<dyn Reader>>,
}

impl Registry {
    /// The registry [`open`] uses: every format `docs/SPEC.md` §1.1 lists
    /// for v1.
    pub fn default_registry() -> Self {
        Self {
            readers: vec![Box::new(CsvReader)],
        }
    }

    /// Registers an additional reader, tried after the built-ins.
    pub fn register(&mut self, reader: Box<dyn Reader>) {
        self.readers.push(reader);
    }

    /// The first registered reader that recognizes `path`, if any.
    pub fn find(&self, path: &Path) -> Option<&dyn Reader> {
        self.readers
            .iter()
            .find(|reader| reader.recognizes(path))
            .map(Box::as_ref)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::default_registry()
    }
}

/// Resolves which registered reader claims `path`, by format name.
///
/// This performs format routing only today — actual parsing, inference, and
/// indexing land with each reader's implementation (docs/ROADMAP.md M2).
pub fn open(path: &Path) -> Result<&'static str> {
    Registry::default_registry()
        .find(path)
        .map(Reader::format_name)
        .ok_or_else(|| GlydeError::UnrecognizedFormat {
            path: path.to_path_buf(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_registry_recognizes_a_csv_path_as_the_csv_reader() {
        let registry = Registry::default_registry();

        let reader = registry
            .find(Path::new("readings.csv"))
            .expect("a .csv path must be claimed by a registered reader");

        assert_eq!(reader.format_name(), "csv");
    }

    #[test]
    fn default_registry_has_no_reader_for_an_unknown_extension() {
        let registry = Registry::default_registry();

        assert!(registry.find(Path::new("readings.xyz")).is_none());
    }

    #[test]
    fn open_returns_the_format_name_of_the_matching_reader() {
        let format = open(Path::new("readings.csv")).expect("a .csv path must open");

        assert_eq!(format, "csv");
    }

    #[test]
    fn open_reports_an_unrecognized_format_instead_of_panicking() {
        let err = open(Path::new("readings.xyz"))
            .expect_err("an unsupported extension must be a reported error, not a panic");

        assert!(matches!(err, GlydeError::UnrecognizedFormat { .. }));
    }
}
