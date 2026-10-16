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

//! Issue #97 / SPEC §1.2: encoding is one of the inference bar's mandatory
//! fields, and "each is correctable in one click". An
//! [`IngestOverrides::encoding`] bypasses SPEC §1.2.1's detection, is
//! reported at full confidence (a deliberate choice is never a guess), and
//! must change the decoded text itself — not only the label — on both
//! storage paths. A byte-order mark never silently overrules it.

use std::path::{Path, PathBuf};

use glyde_core::budget::RamBudget;
use glyde_core::ingest::{self, Confidence, Dataset, EncodingChoice, IngestOverrides};
use glyde_core::series::{SeriesValues, SpilledValues};

fn corpus(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corpus")
        .join(name)
}

fn with_encoding(encoding: EncodingChoice) -> IngestOverrides {
    IngestOverrides {
        encoding: Some(encoding),
        ..IngestOverrides::default()
    }
}

fn strings(dataset: &Dataset, column: &str) -> Vec<String> {
    let series = dataset
        .columns
        .iter()
        .find(|series| series.name() == column)
        .unwrap_or_else(|| panic!("no column {column:?}"));
    match series.values() {
        SeriesValues::String(values) => values.clone(),
        SeriesValues::Spilled(SpilledValues::String(values)) => {
            values.iter().map(str::to_owned).collect()
        }
        other => panic!("expected a string column, got {other:?}"),
    }
}

/// In memory, then spilled, under the same overrides.
fn load_both(path: &Path, overrides: IngestOverrides) -> [Dataset; 2] {
    let cache = tempfile::tempdir().expect("temp cache dir");
    let datasets = [u64::MAX, 0].map(|ram| {
        ingest::load_with_overrides_and_budget(
            path,
            overrides,
            RamBudget::from_total_ram_bytes(ram),
            cache.path(),
        )
        .expect("open")
    });
    assert!(!datasets[0].is_spilled() && datasets[1].is_spilled());
    datasets
}

// Corpus 9 is Windows-1252 (byte 0x92 is a right single quote). Forcing
// UTF-8 must decode that byte as U+FFFD instead (SPEC §1.2.1: replaced,
// never fatal), and the report must name the encoding the user picked.
#[test]
fn an_encoding_override_changes_the_decoded_text_and_the_report() {
    let path = corpus("case-09-windows1252-smart-quotes.csv");

    let (_, auto_report, auto) =
        ingest::open_dataset_with_overrides(&path, IngestOverrides::default()).expect("auto open");
    assert_eq!(auto_report.encoding.value, "windows-1252");
    assert!(strings(&auto, "note")[0].contains('\u{2019}'));

    let (_, report, forced) =
        ingest::open_dataset_with_overrides(&path, with_encoding(EncodingChoice::Utf8))
            .expect("forced open");
    assert_eq!(report.encoding.value, "utf-8");
    assert_eq!(report.encoding.confidence, Confidence::High);
    let notes = strings(&forced, "note");
    assert!(notes[0].contains('\u{FFFD}'), "{notes:?}");
    assert!(!notes[0].contains('\u{2019}'), "{notes:?}");

    let [in_memory, spilled] = load_both(&path, with_encoding(EncodingChoice::Utf8));
    assert_eq!(strings(&in_memory, "note"), notes);
    assert_eq!(strings(&spilled, "note"), notes);
}

// A UTF-16 file without a BOM is not recognized on its own; picking UTF-16LE
// is what makes it readable at all.
#[test]
fn picking_utf16_reads_a_file_detection_could_not() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("utf16le.csv");
    let text = "timestamp,value,label\n\
                2026-01-01T00:00:00Z,1.5,été\n\
                2026-01-01T00:00:01Z,2.5,hiver\n";
    let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    std::fs::write(&path, bytes).expect("write fixture");

    for dataset in load_both(&path, with_encoding(EncodingChoice::Utf16Le)) {
        assert_eq!(dataset.time_column_name, "timestamp");
        assert_eq!(strings(&dataset, "label"), ["été", "hiver"]);
    }
}

// The user's pick wins over a byte-order mark: a UTF-8 BOM decoded as
// Windows-1252 shows up as its three characters, rather than the BOM
// quietly switching the decoder back to UTF-8.
#[test]
fn a_byte_order_mark_never_overrules_the_user_s_pick() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("bom.csv");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(
        "timestamp,value,label\n2026-01-01T00:00:00Z,1.5,é\n2026-01-01T00:00:01Z,2.5,a\n"
            .as_bytes(),
    );
    std::fs::write(&path, bytes).expect("write fixture");

    let (_, report, dataset) =
        ingest::open_dataset_with_overrides(&path, with_encoding(EncodingChoice::Windows1252))
            .expect("open");
    assert_eq!(report.encoding.value, "windows-1252");
    assert_eq!(strings(&dataset, "label")[0], "Ã©");
}

// The same pick on a file that already decodes that way changes nothing.
#[test]
fn confirming_the_detected_encoding_changes_nothing() {
    let path = corpus("case-09-windows1252-smart-quotes.csv");
    let auto = ingest::load(&path).expect("auto open");
    let [confirmed, _] = load_both(&path, with_encoding(EncodingChoice::Windows1252));
    assert_eq!(strings(&confirmed, "note"), strings(&auto, "note"));
}
