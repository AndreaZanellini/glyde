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
//! extension.
//!
//! [`parse`] is the streaming single-pass reader (docs/ARCH.md §deps: `csv`
//! over a memory-mapped file); [`open_path`] is the memmap entry point a
//! caller reaches for from disk. Dtype inference and the time index are
//! later docs/ROADMAP.md M2 items — for now every field is carried as its
//! raw source text (Golden Rule 1: never degrade the raw data).

use super::infer::{self, Delimiter};
use crate::{GlydeError, Result};
use std::fs::File;
use std::path::Path;
use tracing::{info, warn};

/// Recognizes the delimited-text inputs SPEC §1.1 groups together.
pub struct CsvReader;

impl super::Reader for CsvReader {
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

/// The result of a full single-pass parse: the header's column names, how
/// many rows were salvaged after ragged-row / truncated-tail tolerance
/// (SPEC §1.3), and how many were skipped along the way. This intentionally
/// carries counts only, not the row data itself: SPEC §5.1 ("data is
/// memory-mapped and read in bounded chunks; the full file is never
/// loaded") is a hard budget constraint this milestone's item does not yet
/// enforce (`docs/ROADMAP.md` M3 owns the RAM-budget module and the
/// bounded/chunked reading built on it) — accumulating every row into an
/// owned `Vec<Vec<String>>` here would bake an unbounded shape into the
/// first public consumer of this reader, which is exactly the risk M3
/// exists to close off. Row values themselves belong to whatever
/// milestone item actually reads them under that future budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvParseOutcome {
    pub column_names: Vec<String>,
    pub row_count: u64,
    pub skipped_row_count: u64,
}

/// Parses `bytes` as delimited text (SPEC §1.1) in one streaming pass:
/// encoding and delimiter/header are inferred from a bounded head sample
/// (SPEC §1.2), then every remaining row is read once and tallied — kept or
/// skipped, never accumulated into an intermediate table (see
/// [`CsvParseOutcome`]'s doc comment on why row *data* is out of scope
/// here). A row whose field count disagrees with the header (ragged rows,
/// a truncated final line) is skipped, counted, and logged at `warn`
/// rather than aborting the read (SPEC §1.3). Malformed data never causes
/// a `panic!`; an empty input is the only rejected input, reported as
/// [`GlydeError::EmptyFile`].
pub fn parse(bytes: &[u8]) -> Result<CsvParseOutcome> {
    if bytes.is_empty() {
        return Err(GlydeError::EmptyFile);
    }

    let encoding = infer::detect_encoding(bytes);
    let text = infer::decode(bytes, &encoding);

    let sample = bounded_head_sample(&text);
    let delimiter = infer::infer_delimiter(sample).delimiter;
    let header = infer::infer_header(sample, delimiter);

    let expected_field_count = header.column_names.len();
    let data_start_row = header
        .header_row_index
        .map_or(header.skipped_preamble_rows, |header_row_index| {
            header_row_index + 1
        });

    let mut row_count = 0u64;
    let mut skipped_row_count = 0u64;

    for (row_index, record) in stream_records(&text, delimiter).enumerate() {
        if row_index < data_start_row {
            continue;
        }
        match record {
            Ok(fields) if fields.len() == expected_field_count => row_count += 1,
            Ok(fields) => {
                warn!(
                    row_index,
                    field_count = fields.len(),
                    expected_field_count,
                    "row skipped: field count does not match the header (SPEC §1.3 ragged-row salvage)"
                );
                skipped_row_count += 1;
            }
            Err(reason) => {
                warn!(
                    row_index,
                    reason, "row skipped: could not be parsed (SPEC §1.3 truncated-tail tolerance)"
                );
                skipped_row_count += 1;
            }
        }
    }

    info!(
        row_count,
        skipped_row_count,
        column_count = expected_field_count,
        "CSV parsed in one streaming pass"
    );

    Ok(CsvParseOutcome {
        column_names: header.column_names,
        row_count,
        skipped_row_count,
    })
}

/// Memory-maps `path` and parses it in one streaming pass (ARCH §deps: "CSV
/// ingestion | `csv` / `csv-core` — streaming, single-pass over a
/// memory-mapped file"). The mapping only backs the parse; it is dropped
/// once this returns.
pub fn open_path(path: &Path) -> Result<CsvParseOutcome> {
    let file = File::open(path).map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Safety: the mapping is read-only and used synchronously within this
    // call; concurrent external truncation of `path` is the same class of
    // risk every memory-mapped reader accepts (ARCH §deps).
    let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&mmap)
}

/// SPEC §1.2's bounded head sample ([`infer::HEAD_SAMPLE_BYTES`]), cut at a
/// line boundary rather than an arbitrary byte offset: [`infer::infer_header`]
/// reports row indices within this sample, and [`parse`] must line those
/// indices up exactly with the full-text record stream, which a mid-record
/// cut would throw off. Falls back to the raw byte cutoff, walked back to
/// the nearest `char` boundary (`HEAD_SAMPLE_BYTES` is a byte count with no
/// UTF-8 alignment guarantee — a multibyte character such as `°` or `µ`
/// landing across it is ordinary, not malformed, input), only for a single
/// line longer than the whole budget.
fn bounded_head_sample(text: &str) -> &str {
    if text.len() <= infer::HEAD_SAMPLE_BYTES {
        return text;
    }
    let mut cut = infer::HEAD_SAMPLE_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let budget = &text[..cut];
    match budget.rfind('\n') {
        Some(last_newline) => &text[..=last_newline],
        None => budget,
    }
}

/// Tokenizes every row of `text` under `delimiter`, one record at a time —
/// the streaming counterpart to `infer`'s bounded-sample tokenizer. A field
/// count is not checked here; [`parse`] decides per row what to keep.
/// Blank lines produce no record at all (verified against the `csv` crate:
/// consecutive newlines are silently skipped, never surfaced as an
/// empty-field row) so they can never be miscounted as ragged or skipped.
fn stream_records(
    text: &str,
    delimiter: Delimiter,
) -> Box<dyn Iterator<Item = std::result::Result<Vec<String>, String>> + '_> {
    match delimiter.as_csv_byte() {
        Some(byte) => {
            let reader = csv::ReaderBuilder::new()
                .delimiter(byte)
                .has_headers(false)
                .flexible(true)
                .from_reader(text.as_bytes());
            Box::new(reader.into_records().map(|result| {
                result
                    .map(|record| record.iter().map(str::to_string).collect())
                    .map_err(|e| e.to_string())
            }))
        }
        None => Box::new(
            text.lines()
                .map(|line| {
                    line.split_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<String>>()
                })
                .filter(|fields| !fields.is_empty())
                .map(Ok),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::Reader as _;
    use std::path::{Path, PathBuf};

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

    fn corpus_path(file_name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("testdata")
            .join("corpus")
            .join(file_name)
    }

    fn corpus_bytes(file_name: &str) -> Vec<u8> {
        let path = corpus_path(file_name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    // Corpus case 20 (QUALITY.md §1.20): every row, including the header,
    // ends in a trailing delimiter, plus blank trailing lines. Neither the
    // trailing empty field nor the blank lines are ragged rows: every row
    // salvages cleanly, and the blank lines must not even be counted.
    #[test]
    fn corpus_case_20_trailing_empty_lines_are_not_skipped_rows() {
        let bytes = corpus_bytes("case-20-trailing-empty-lines.csv");

        let outcome = parse(&bytes).expect("case 20 must parse");

        assert_eq!(
            outcome.column_names,
            vec!["timestamp", "value", "pressure", ""]
        );
        assert_eq!(outcome.row_count, 4);
        assert_eq!(outcome.skipped_row_count, 0);
    }

    // Corpus case 21 (QUALITY.md §1.21): two of five data rows have the
    // wrong field count (one short, one long) and must be skipped and
    // counted, while the other three salvage normally.
    #[test]
    fn corpus_case_21_ragged_rows_are_skipped_and_counted() {
        let bytes = corpus_bytes("case-21-ragged-rows.csv");

        let outcome = parse(&bytes).expect("case 21 must parse");

        assert_eq!(outcome.column_names, vec!["timestamp", "value", "pressure"]);
        assert_eq!(outcome.row_count, 3);
        assert_eq!(outcome.skipped_row_count, 2);
    }

    // Corpus case 22 (QUALITY.md §1.22): the file ends mid-row (no trailing
    // newline, no value field) as if still being written. The truncated
    // last line must be salvaged-out like any other ragged row, not treated
    // as a hard read error.
    #[test]
    fn corpus_case_22_truncated_final_line_is_skipped_not_fatal() {
        let bytes = corpus_bytes("case-22-truncated-final-line.csv");

        let outcome = parse(&bytes).expect("case 22 must parse");

        assert_eq!(outcome.column_names, vec!["timestamp", "value"]);
        assert_eq!(outcome.row_count, 4);
        assert_eq!(outcome.skipped_row_count, 1);
    }

    // Corpus case 23 (QUALITY.md §1.23): an empty file must fail cleanly
    // with a clear error, never panic and never a bogus zero-row summary.
    #[test]
    fn corpus_case_23_empty_file_is_a_clean_error() {
        let bytes = corpus_bytes("case-23-empty-file.csv");
        assert!(bytes.is_empty(), "fixture must actually be empty");

        let err = parse(&bytes).expect_err("an empty file must be rejected");

        assert!(matches!(err, GlydeError::EmptyFile));
        assert_eq!(err.to_string(), "empty file: no data to read");
    }

    // Corpus case 4's whitespace-aligned columns, parsed end to end: proves
    // the streaming pass (not just the bounded-sample inference) also
    // tokenizes the whitespace-collapsing path correctly.
    #[test]
    fn corpus_case_04_whitespace_aligned_parses_every_row() {
        let bytes = corpus_bytes("case-04-whitespace-aligned.txt");

        let outcome = parse(&bytes).expect("case 4 must parse");

        assert_eq!(outcome.column_names, vec!["timestamp", "value", "pressure"]);
        assert_eq!(outcome.row_count, 6);
        assert_eq!(outcome.skipped_row_count, 0);
    }

    #[test]
    fn open_path_memory_maps_and_parses_a_real_file() {
        let outcome = open_path(&corpus_path("case-01-comma-clean.csv")).expect("case 1 must open");

        assert_eq!(outcome.column_names, vec!["timestamp", "value", "pressure"]);
        assert!(outcome.row_count > 0);
        assert_eq!(outcome.skipped_row_count, 0);
    }

    #[test]
    fn open_path_reports_a_missing_file_instead_of_panicking() {
        let err = open_path(Path::new("/nonexistent/glyde-csv-test.csv"))
            .expect_err("a missing file must be a reported error");

        assert!(matches!(err, GlydeError::Io { .. }));
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // Fuzz-lite smoke test (the full property test lives in
        // tests/property.rs); this is a fast, deterministic companion run
        // on every `cargo test`.
        for seed in 0u8..=255 {
            let bytes: Vec<u8> = (0..64).map(|i| seed.wrapping_add(i)).collect();
            let _ = parse(&bytes);
        }
    }

    #[test]
    fn bounded_head_sample_cuts_on_a_line_boundary() {
        let line = "a,b,c\n";
        let text = line.repeat(infer::HEAD_SAMPLE_BYTES / line.len() + 10);

        let sample = bounded_head_sample(&text);

        assert!(sample.len() <= infer::HEAD_SAMPLE_BYTES);
        assert!(
            sample.ends_with('\n'),
            "cut must land after a full line, not mid-record"
        );
    }

    #[test]
    fn bounded_head_sample_is_unchanged_for_small_input() {
        assert_eq!(bounded_head_sample("a,b\n1,2\n"), "a,b\n1,2\n");
    }

    // Regression (maintainer review on PR #35): a raw `HEAD_SAMPLE_BYTES`
    // byte-offset slice is not guaranteed to land on a UTF-8 char boundary.
    // `°` encodes as two bytes (0xC2 0xB0); placing it so its second byte
    // sits exactly at the cutoff — the realistic case being any
    // `°C`/`µm/s²`-style unit header a little past 1 MiB into an otherwise
    // ordinary file — must not panic.
    #[test]
    fn bounded_head_sample_never_panics_when_a_multibyte_char_straddles_the_cutoff() {
        let prefix = "a".repeat(infer::HEAD_SAMPLE_BYTES - 1);
        let text = format!("{prefix}\u{b0}C and more text after the cutoff\n");
        assert!(
            !text.is_char_boundary(infer::HEAD_SAMPLE_BYTES),
            "fixture must actually straddle the cutoff for this test to prove anything"
        );

        let sample = bounded_head_sample(&text);

        assert!(text.is_char_boundary(sample.len()));
        assert!(sample.len() <= infer::HEAD_SAMPLE_BYTES);
    }

    #[test]
    fn parse_never_panics_on_a_multibyte_char_straddling_the_head_sample_cutoff() {
        // Many short single-byte rows (unlike the single-giant-row shape
        // that would leave no data row visible in the bounded sample) so
        // the header is confidently detected well before the cutoff, which
        // this test then places a straddling multibyte character across.
        let mut text = String::from("value\n");
        while text.len() < infer::HEAD_SAMPLE_BYTES - 1 {
            text.push_str("1\n");
        }
        text.truncate(infer::HEAD_SAMPLE_BYTES - 1); // every byte so far is ASCII: any length is a char boundary
        text.push('\u{b0}'); // its second byte lands exactly at HEAD_SAMPLE_BYTES
        text.push('\n');
        assert!(
            !text.is_char_boundary(infer::HEAD_SAMPLE_BYTES),
            "fixture must actually straddle the cutoff for this test to prove anything"
        );

        let outcome = parse(text.as_bytes()).expect("valid UTF-8 must never be rejected");

        assert_eq!(outcome.column_names, vec!["value"]);
    }
}
