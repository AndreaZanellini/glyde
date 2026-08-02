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
//! [`open`] is routing only: which registered [`Reader`] claims a path.
//! [`report::inspect`] wires encoding, delimiter, header, decimal separator,
//! and time-index inference together into one [`OpenSummary`]
//! (docs/ROADMAP.md M2 "Activate corpus open→compare gate") — counts and
//! classifications only, no materialized values. [`dataset::load`] wires the
//! same pieces together with [`infer_column`] (decimal-normalized per SPEC
//! §1.2.4 first) into a full in-memory [`Dataset`], for `glyde-app`'s
//! time-domain view (docs/ROADMAP.md M2 "Time-domain view v1").

mod csv;
mod dataset;
mod infer;
mod report;

pub use crate::index::level0::Level0Cache;
pub use csv::{open_path, parse, CsvParseOutcome, CsvReader};
pub use dataset::{
    level0_for_dataset_cached, level0_for_dataset_cached_with_cache_dir, load, load_progressive,
    load_progressive_with_budget, load_with_budget, load_with_overrides,
    load_with_overrides_and_budget, progressive_tick_to_value, progressive_value_to_tick,
    pyramids_for_dataset, pyramids_for_dataset_cached, pyramids_for_dataset_cached_with_cache_dir,
    Checkpoint, Dataset, ProgressiveValues, TimeAxis, Timestamps, PROGRESSIVE_TICK_SCALE,
};
pub use infer::{
    decode, detect_encoding, infer_column, infer_decimal_separator, infer_delimiter, infer_header,
    Confidence, DecimalSeparator, DecimalSeparatorInference, Delimiter, DelimiterInference,
    DtypeInference, EncodingInference, EncodingSource, HeaderInference, HEAD_SAMPLE_BYTES,
};
pub use report::{
    inspect, open_dataset, open_dataset_progressive, open_dataset_progressive_with_overrides,
    open_dataset_with_budget, open_dataset_with_overrides, InferenceReport, InferredField,
    OpenSummary, SamplingClass,
};

use crate::time::TimestampFormat;
use crate::{GlydeError, Result};
use std::path::Path;

/// User-chosen corrections that bypass the corresponding SPEC §1.2 inference
/// step (docs/ROADMAP.md M4 "One-click correction of each field → triggers a
/// re-index"). Each `None` field keeps the automatic inference; each `Some`
/// field is a settled, deliberate choice, so it is always reported at
/// [`Confidence::High`] — a user's own correction is never "low confidence"
/// the way a guess can be (Golden Rule 2).
///
/// Derives `Hash` so a cache key (`index::level0::CacheKey::with_overrides_signature`)
/// can be scoped to it: a corrected re-open of the same, byte-for-byte
/// unchanged file must never collide with a pyramid cached under a
/// different (or absent) set of overrides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct IngestOverrides {
    pub delimiter: Option<Delimiter>,
    pub decimal_separator: Option<DecimalSeparator>,
    pub timestamp_format: Option<TimestampFormat>,
}

/// A plain hash of `overrides`, for scoping a cache key to it
/// (`index::level0::CacheKey::with_overrides_signature`) without that
/// lower-level module depending on this one's types.
fn overrides_signature(overrides: IngestOverrides) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    overrides.hash(&mut hasher);
    hasher.finish()
}

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
