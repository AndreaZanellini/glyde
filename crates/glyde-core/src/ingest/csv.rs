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

//! Delimited-text reader (docs/SPEC.md §1.1: `.csv`, `.tsv`, `.txt`).
//!
//! `.csv`, `.tsv`, and `.txt` share one reader: SPEC §1.1 treats them as one
//! input class ("any delimiter, any decimal separator, any common encoding"),
//! with the actual delimiter inferred from content rather than the
//! extension. Only extension-based recognition is implemented here; the
//! streaming parse and inference pipeline are later docs/ROADMAP.md M2 items.

use super::Reader;
use std::path::Path;

/// Recognizes the delimited-text inputs SPEC §1.1 groups together.
pub struct CsvReader;

impl Reader for CsvReader {
    fn format_name(&self) -> &'static str {
        "csv"
    }

    fn recognizes(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                ext.eq_ignore_ascii_case("csv")
                    || ext.eq_ignore_ascii_case("tsv")
                    || ext.eq_ignore_ascii_case("txt")
            })
    }
}

/// Parses `bytes` as delimited text (SPEC §1.1). Provisional entry point:
/// the real streaming parse over a memory-mapped file — encoding/delimiter/
/// decimal inference, ragged-row salvage, skipped-row counting — lands with
/// docs/ROADMAP.md M2. This stub exists only so the CSV fuzz-lite property
/// test (docs/QUALITY.md §2 Property tests, docs/ROADMAP.md M1) has an entry
/// point to target today: "any random byte sequence fed to the CSV reader:
/// never panics."
///
/// **Not implemented yet** (docs/ROADMAP.md M2).
pub fn parse(bytes: &[u8]) -> crate::Result<()> {
    let _ = bytes;
    todo!("docs/ROADMAP.md M2: CSV streaming parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_csv_tsv_and_txt_case_insensitively() {
        let reader = CsvReader;

        assert!(reader.recognizes(Path::new("readings.csv")));
        assert!(reader.recognizes(Path::new("readings.TSV")));
        assert!(reader.recognizes(Path::new("readings.txt")));
    }

    #[test]
    fn does_not_recognize_other_extensions() {
        let reader = CsvReader;

        assert!(!reader.recognizes(Path::new("readings.parquet")));
        assert!(!reader.recognizes(Path::new("readings")));
    }

    #[test]
    fn format_name_is_csv() {
        assert_eq!(CsvReader.format_name(), "csv");
    }
}
