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

//! Issue #209: invalid byte sequences are replaced with U+FFFD (SPEC §1.3,
//! "replaced, never fatal"), but that is a lossy change to the source text
//! (Golden Rule 1), and often the first sign that the encoding guess is
//! wrong (Golden Rule 2). The only trace used to be a `warn` in the log,
//! with the encoding shown at full confidence. The report now counts the
//! replacements and, for an inferred encoding, marks it low confidence, on
//! every path a file can open through.

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, Confidence, EncodingChoice, InferenceReport, IngestOverrides};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Corpus case 12: a clean UTF-8 file with one stray `0xFF` byte.
fn case_12() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corpus/case-12-invalid-byte-sequences.csv")
}

fn open_in(path: &Path, overrides: IngestOverrides, ram: u64, cache: &Path) -> InferenceReport {
    ingest::open_dataset_progressive_in(
        path,
        overrides,
        RamBudget::from_total_ram_bytes(ram),
        cache,
        &AtomicBool::new(false),
        |_| {},
    )
    .expect("case 12 opens")
    .1
}

#[test]
fn replaced_bytes_are_counted_and_lower_the_encoding_confidence() {
    let (_, report, _) = ingest::open_dataset(&case_12()).expect("case 12 opens");

    assert_eq!(report.encoding.value, "utf-8");
    assert_eq!(report.replaced_sequences, 1, "one invalid byte, one U+FFFD");
    assert_eq!(report.encoding.confidence, Confidence::Low);
    assert!(
        report.has_low_confidence_field(),
        "the inference bar opens expanded"
    );
}

#[test]
fn a_clean_file_reports_no_replacement_at_full_confidence() {
    let clean = case_12().with_file_name("case-01-comma-clean.csv");
    let (_, report, _) = ingest::open_dataset(&clean).expect("case 1 opens");

    assert_eq!(report.replaced_sequences, 0);
    assert_eq!(report.encoding.confidence, Confidence::High);
}

#[test]
fn the_count_is_the_same_spilled_in_memory_and_from_the_dataset_cache() {
    let cache = tempfile::tempdir().expect("temp cache dir");
    let in_memory = open_in(
        &case_12(),
        IngestOverrides::default(),
        u64::MAX,
        cache.path(),
    );
    let reopened = open_in(
        &case_12(),
        IngestOverrides::default(),
        u64::MAX,
        cache.path(),
    );
    let spilled = open_in(&case_12(), IngestOverrides::default(), 0, cache.path());

    assert_eq!(in_memory.replaced_sequences, 1);
    assert_eq!(reopened, in_memory, "a cached reopen keeps the count");
    assert_eq!(
        spilled.replaced_sequences, 1,
        "the streaming decoder counts"
    );
    assert_eq!(spilled.encoding.confidence, Confidence::Low);
}

// An encoding the user picked is not a guess: still `High`, but the
// replacements it leads to are still reported, never hidden.
#[test]
fn a_user_picked_encoding_still_reports_its_replacements() {
    let overrides = IngestOverrides {
        encoding: Some(EncodingChoice::Utf8),
        ..IngestOverrides::default()
    };
    let cache = tempfile::tempdir().expect("temp cache dir");
    for ram in [u64::MAX, 0] {
        let report = open_in(&case_12(), overrides, ram, cache.path());
        assert_eq!(report.replaced_sequences, 1, "ram {ram}");
        assert_eq!(report.encoding.confidence, Confidence::High, "ram {ram}");
    }
}
