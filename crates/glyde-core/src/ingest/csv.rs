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

use super::infer::{self, Confidence, DecimalSeparator, Delimiter};
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

/// One column's captured raw text across every kept row: every field is
/// appended to a single arena buffer, with a `(start, len)` entry per field
/// recording where it lives, instead of one heap-allocated `String` per
/// field (issue #62 — that per-field allocation, times millions of rows,
/// was measured driving peak RSS to ~12.75x the source file's size).
/// [`ColumnText::iter`] hands back borrowed `&str` slices into the arena, so
/// a caller that only needs to inspect or type the text (SPEC §1.4's dtype
/// inference, `time::infer_timestamp_format`) never pays for an owned copy.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ColumnText {
    arena: String,
    ranges: Vec<(usize, usize)>,
}

impl ColumnText {
    fn push(&mut self, field: &str) {
        let start = self.arena.len();
        self.arena.push_str(field);
        self.ranges.push((start, field.len()));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Every captured field, in row order, borrowed from the arena.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        self.ranges
            .iter()
            .map(move |&(start, len)| &self.arena[start..start + len])
    }

    #[cfg(test)]
    pub(crate) fn get(&self, index: usize) -> Option<&str> {
        let &(start, len) = self.ranges.get(index)?;
        Some(&self.arena[start..start + len])
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
    /// The lowercase WHATWG encoding label (SPEC §1.2.1), e.g. `"utf-8"`.
    pub encoding_label: String,
    pub delimiter: Delimiter,
    pub decimal_separator: DecimalSeparator,
    /// SPEC §1.2 "confidence is tracked per inference" — [`InferenceReport`]
    /// (docs/ROADMAP.md M4) surfaces these to the UI.
    ///
    /// [`InferenceReport`]: super::report::InferenceReport
    pub encoding_confidence: Confidence,
    pub delimiter_confidence: Confidence,
    pub decimal_separator_confidence: Confidence,
    /// [`infer::HeaderInference::ambiguous`]: a leading preamble existed but
    /// no header candidate could be confidently identified, so the column
    /// names (including the time column's) are a guess, not a resolved
    /// reading.
    pub header_ambiguous: bool,
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
    parse_impl(bytes, Capture::None, None).map(|(outcome, _)| outcome)
}

/// Parses `bytes` exactly like [`parse`], additionally capturing every kept
/// row's raw text for `column_index` (docs/ROADMAP.md M2 "Activate corpus
/// open→compare gate": `ingest::report::inspect` needs the time-index
/// column's raw values to run `time::infer_timestamp_format` and friends
/// against). This is bounded by `row_count` strings from *one* column, not
/// the whole table — genuinely bounded/chunked reading for arbitrary-size
/// files is still docs/ROADMAP.md M3's job, the same deferral
/// [`CsvParseOutcome`]'s original doc comment noted for row data in general.
pub(crate) fn parse_capturing_column(
    bytes: &[u8],
    column_index: usize,
) -> Result<(CsvParseOutcome, ColumnText)> {
    let (outcome, mut columns) = parse_impl(bytes, Capture::Column(column_index), None)?;
    Ok((outcome, columns.pop().unwrap_or_default()))
}

/// Parses `bytes` exactly like [`parse`], additionally capturing every kept
/// row's raw text for *every* column, one `Vec<String>` per column in header
/// order (docs/ROADMAP.md M2 "Time-domain view v1": `ingest::dataset::load`
/// needs every data series' raw values, not just the time index's). Same
/// "small files only" deferral as [`parse_capturing_column`]: this holds the
/// whole table in memory at once, which is exactly what
/// [`CsvParseOutcome`]'s doc comment flags as docs/ROADMAP.md M3's job to
/// bound for arbitrary-size files.
pub(crate) fn parse_capturing_all_columns(
    bytes: &[u8],
) -> Result<(CsvParseOutcome, Vec<ColumnText>)> {
    parse_impl(bytes, Capture::All, None)
}

/// [`parse_capturing_all_columns`], additionally invoking `on_chunk` with a
/// snapshot of the parse-so-far (a [`CsvParseOutcome`] whose `row_count`/
/// `skipped_row_count` reflect only the rows kept up to that point, and the
/// matching prefix of each column's captured text) at a geometrically
/// growing row-count schedule (docs/ROADMAP.md M3 "Background progressive
/// build emitting partial levels"): first at [`FIRST_PROGRESS_CHECKPOINT_ROWS`]
/// kept rows, then doubling. Doubling — rather than a fixed interval — bounds
/// the *total* extra work every checkpoint costs a caller that re-derives a
/// typed dataset from the snapshot (as [`super::dataset::load_with_outcome_progressive`]
/// does): re-deriving from scratch at row counts `C, 2C, 4C, ...` costs
/// `O(n)` in total (a geometric series), the same asymptotic cost as the
/// final parse alone, rather than the `O(n²)` a fixed-interval schedule would
/// cost on a huge file. A file with fewer than
/// [`FIRST_PROGRESS_CHECKPOINT_ROWS`] kept rows never checkpoints — it is
/// fast enough end to end that a progress update would have nothing useful
/// to add.
pub(crate) fn parse_capturing_all_columns_with_progress(
    bytes: &[u8],
    mut on_chunk: impl FnMut(&CsvParseOutcome, &[ColumnText]),
) -> Result<(CsvParseOutcome, Vec<ColumnText>)> {
    parse_impl(bytes, Capture::All, Some(&mut on_chunk))
}

/// The first checkpoint [`parse_capturing_all_columns_with_progress`] fires
/// at, in kept rows (not skipped/ragged ones) — see that function's doc
/// comment for the doubling schedule this seeds.
const FIRST_PROGRESS_CHECKPOINT_ROWS: u64 = 20_000;

/// What [`parse_impl`] should hold onto per kept row, alongside the tallies
/// every capture mode needs regardless.
enum Capture {
    /// Tallies only ([`parse`]) — no row text is retained.
    None,
    /// One column's raw text ([`parse_capturing_column`]).
    Column(usize),
    /// Every column's raw text, one `Vec<String>` per column
    /// ([`parse_capturing_all_columns`]).
    All,
}

/// A progress checkpoint callback (see
/// [`parse_capturing_all_columns_with_progress`]), named so
/// [`parse_impl`]/[`maybe_checkpoint`] don't repeat this trait-object type
/// inline (clippy `type_complexity`).
type ChunkCallback<'a> = &'a mut dyn FnMut(&CsvParseOutcome, &[ColumnText]);

fn parse_impl(
    bytes: &[u8],
    capture: Capture,
    mut on_chunk: Option<ChunkCallback>,
) -> Result<(CsvParseOutcome, Vec<ColumnText>)> {
    if bytes.is_empty() {
        return Err(GlydeError::EmptyFile);
    }

    let encoding = infer::detect_encoding(bytes);
    let encoding_confidence = encoding.confidence();
    let text = infer::decode(bytes, &encoding);

    let sample = bounded_head_sample(&text);
    let delimiter_inference = infer::infer_delimiter(sample);
    let delimiter = delimiter_inference.delimiter;
    let delimiter_confidence = delimiter_inference.confidence();
    let decimal_inference = infer::infer_decimal_separator(sample, delimiter);
    let decimal_separator = decimal_inference.separator;
    let decimal_separator_confidence = decimal_inference.confidence();
    let header = infer::infer_header(sample, delimiter);

    let expected_field_count = header.column_names.len();
    let data_start_row = header
        .header_row_index
        .map_or(header.skipped_preamble_rows, |header_row_index| {
            header_row_index + 1
        });

    let mut acc = ParseAccumulator {
        captured: match capture {
            Capture::None => Vec::new(),
            Capture::Column(_) => vec![ColumnText::default()],
            Capture::All => (0..expected_field_count)
                .map(|_| ColumnText::default())
                .collect(),
        },
        row_count: 0,
        skipped_row_count: 0,
    };

    // Built once, only when a checkpoint callback is actually installed: the
    // fields of `CsvParseOutcome` known before the row loop starts, cloned
    // fresh into each checkpoint snapshot with that snapshot's own
    // `row_count`/`skipped_row_count` (see `maybe_checkpoint`).
    let outcome_template = on_chunk.is_some().then(|| CsvParseOutcome {
        column_names: header.column_names.clone(),
        row_count: 0,
        skipped_row_count: 0,
        encoding_label: encoding.label(),
        delimiter,
        decimal_separator,
        encoding_confidence,
        delimiter_confidence,
        decimal_separator_confidence,
        header_ambiguous: header.ambiguous,
    });
    let mut next_checkpoint_rows = FIRST_PROGRESS_CHECKPOINT_ROWS;

    match delimiter.as_csv_byte() {
        Some(byte) => {
            let mut reader = csv::ReaderBuilder::new()
                .delimiter(byte)
                .has_headers(false)
                .flexible(true)
                .from_reader(text.as_bytes());
            let mut record = csv::StringRecord::new();
            let mut row_index = 0usize;
            loop {
                match reader.read_record(&mut record) {
                    Ok(true) => {
                        if row_index >= data_start_row {
                            record_kept_or_ragged(
                                row_index,
                                record.iter(),
                                record.len(),
                                expected_field_count,
                                &capture,
                                &mut acc,
                            );
                            maybe_checkpoint(
                                &mut on_chunk,
                                &outcome_template,
                                &mut next_checkpoint_rows,
                                &acc,
                            );
                        }
                        row_index += 1;
                    }
                    Ok(false) => break,
                    Err(reason) => {
                        if row_index >= data_start_row {
                            warn!(
                                row_index,
                                %reason,
                                "row skipped: could not be parsed (SPEC §1.3 truncated-tail tolerance)"
                            );
                            acc.skipped_row_count += 1;
                        }
                        row_index += 1;
                    }
                }
            }
        }
        None => {
            let mut row_index = 0usize;
            for line in text.lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.is_empty() {
                    continue;
                }
                if row_index >= data_start_row {
                    record_kept_or_ragged(
                        row_index,
                        fields.iter().copied(),
                        fields.len(),
                        expected_field_count,
                        &capture,
                        &mut acc,
                    );
                    maybe_checkpoint(
                        &mut on_chunk,
                        &outcome_template,
                        &mut next_checkpoint_rows,
                        &acc,
                    );
                }
                row_index += 1;
            }
        }
    }

    info!(
        row_count = acc.row_count,
        skipped_row_count = acc.skipped_row_count,
        column_count = expected_field_count,
        "CSV parsed in one streaming pass"
    );

    Ok((
        CsvParseOutcome {
            column_names: header.column_names,
            row_count: acc.row_count,
            skipped_row_count: acc.skipped_row_count,
            encoding_label: encoding.label(),
            delimiter,
            decimal_separator,
            encoding_confidence,
            delimiter_confidence,
            decimal_separator_confidence,
            header_ambiguous: header.ambiguous,
        },
        acc.captured,
    ))
}

/// Mutable state threaded through the row loop: what's been captured per
/// column so far, plus the running kept/skipped row tallies. Bundled into
/// one struct so [`record_kept_or_ragged`] takes one accumulator parameter
/// instead of three, keeping it under clippy's too-many-arguments threshold
/// without silencing the lint (PR #64 review).
struct ParseAccumulator {
    captured: Vec<ColumnText>,
    row_count: u64,
    skipped_row_count: u64,
}

/// Applies SPEC §1.3's ragged-row salvage to one already-tokenized row and,
/// if kept, appends its fields into `acc.captured` per `capture` — borrowing
/// straight from `fields` into each column's [`ColumnText`] arena rather
/// than allocating an owned `String` per field (issue #62).
fn record_kept_or_ragged<'a>(
    row_index: usize,
    fields: impl Iterator<Item = &'a str>,
    field_count: usize,
    expected_field_count: usize,
    capture: &Capture,
    acc: &mut ParseAccumulator,
) {
    if field_count == expected_field_count {
        acc.row_count += 1;
        match *capture {
            Capture::None => {}
            Capture::Column(index) => {
                if let Some(field) = fields.into_iter().nth(index) {
                    acc.captured[0].push(field);
                }
            }
            Capture::All => {
                for (column, field) in acc.captured.iter_mut().zip(fields) {
                    column.push(field);
                }
            }
        }
    } else {
        warn!(
            row_index,
            field_count,
            expected_field_count,
            "row skipped: field count does not match the header (SPEC §1.3 ragged-row salvage)"
        );
        acc.skipped_row_count += 1;
    }
}

/// Fires `on_chunk` with a snapshot of `acc` once `acc.row_count` reaches
/// `next_checkpoint_rows`, then doubles `next_checkpoint_rows` for the next
/// call (see [`parse_capturing_all_columns_with_progress`]'s doc comment for
/// why doubling). A no-op whenever `on_chunk`/`outcome_template` are `None`
/// (the ordinary, non-progressive parse paths) or the threshold hasn't been
/// reached yet.
fn maybe_checkpoint(
    on_chunk: &mut Option<ChunkCallback>,
    outcome_template: &Option<CsvParseOutcome>,
    next_checkpoint_rows: &mut u64,
    acc: &ParseAccumulator,
) {
    let (Some(on_chunk), Some(template)) = (on_chunk.as_deref_mut(), outcome_template.as_ref())
    else {
        return;
    };
    if acc.row_count < *next_checkpoint_rows {
        return;
    }

    let snapshot = CsvParseOutcome {
        row_count: acc.row_count,
        skipped_row_count: acc.skipped_row_count,
        ..template.clone()
    };
    on_chunk(&snapshot, &acc.captured);
    *next_checkpoint_rows = next_checkpoint_rows.saturating_mul(2);
}

/// Memory-maps `path` and parses it in one streaming pass (ARCH §deps: "CSV
/// ingestion | `csv` / `csv-core` — streaming, single-pass over a
/// memory-mapped file"). The mapping only backs the parse; it is dropped
/// once this returns.
pub fn open_path(path: &Path) -> Result<CsvParseOutcome> {
    let mmap = map_file(path)?;
    parse(&mmap)
}

/// [`open_path`], additionally capturing `column_index`'s raw text per kept
/// row (see [`parse_capturing_column`]).
pub(crate) fn open_path_capturing_column(
    path: &Path,
    column_index: usize,
) -> Result<(CsvParseOutcome, ColumnText)> {
    let mmap = map_file(path)?;
    parse_capturing_column(&mmap, column_index)
}

/// [`open_path`], additionally capturing every column's raw text (see
/// [`parse_capturing_all_columns`]).
pub(crate) fn open_path_capturing_all_columns(
    path: &Path,
) -> Result<(CsvParseOutcome, Vec<ColumnText>)> {
    let mmap = map_file(path)?;
    parse_capturing_all_columns(&mmap)
}

/// [`open_path_capturing_all_columns`], additionally checkpointing progress
/// (see [`parse_capturing_all_columns_with_progress`]).
pub(crate) fn open_path_capturing_all_columns_with_progress(
    path: &Path,
    on_chunk: impl FnMut(&CsvParseOutcome, &[ColumnText]),
) -> Result<(CsvParseOutcome, Vec<ColumnText>)> {
    let mmap = map_file(path)?;
    parse_capturing_all_columns_with_progress(&mmap, on_chunk)
}

/// Memory-maps `path` read-only. The mapping only backs the caller's parse;
/// it is dropped once that returns.
fn map_file(path: &Path) -> Result<memmap2::Mmap> {
    let file = File::open(path).map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Safety: the mapping is read-only and used synchronously within this
    // call; concurrent external truncation of `path` is the same class of
    // risk every memory-mapped reader accepts (ARCH §deps).
    unsafe { memmap2::Mmap::map(&file) }.map_err(|source| GlydeError::Io {
        path: path.to_path_buf(),
        source,
    })
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

    // Same fixture, capturing every column: proves the multi-column capture
    // path (docs/ROADMAP.md M2 "Time-domain view v1") skips exactly the same
    // two ragged rows as the tally-only path, and that every captured
    // column ends up with the same length (one entry per *kept* row, in the
    // same row order) rather than drifting out of alignment with each other.
    #[test]
    fn parse_capturing_all_columns_skips_ragged_rows_and_keeps_columns_aligned() {
        let bytes = corpus_bytes("case-21-ragged-rows.csv");

        let (outcome, columns) = parse_capturing_all_columns(&bytes).expect("case 21 must parse");

        assert_eq!(outcome.column_names, vec!["timestamp", "value", "pressure"]);
        assert_eq!(outcome.row_count, 3);
        assert_eq!(outcome.skipped_row_count, 2);
        assert_eq!(columns.len(), 3, "one captured Vec per column");
        for column in &columns {
            assert_eq!(column.len(), 3, "one entry per kept row");
        }
        assert_eq!(columns[0].get(0), Some("2026-01-01T00:00:00Z"));
        assert_eq!(columns[1].get(0), Some("1.0"));
        assert_eq!(columns[2].get(0), Some("101.3"));
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

    /// A synthetic CSV with more kept rows than [`FIRST_PROGRESS_CHECKPOINT_ROWS`]
    /// (docs/ROADMAP.md M3 "Background progressive build emitting partial
    /// levels"), so the doubling schedule fires more than once.
    fn many_rows_csv(row_count: u64) -> Vec<u8> {
        let mut text = String::from("timestamp,value\n");
        for i in 0..row_count {
            text.push_str(&format!("{i},{}\n", i as f64 * 0.5));
        }
        text.into_bytes()
    }

    #[test]
    fn progress_checkpoints_follow_the_doubling_schedule_and_report_a_growing_row_count() {
        let bytes = many_rows_csv(70_000);
        let mut checkpoints: Vec<u64> = Vec::new();

        let (outcome, _columns) =
            parse_capturing_all_columns_with_progress(&bytes, |snapshot, _columns| {
                checkpoints.push(snapshot.row_count);
            })
            .expect("many-row CSV must parse");

        assert_eq!(outcome.row_count, 70_000);
        assert_eq!(
            checkpoints,
            vec![
                FIRST_PROGRESS_CHECKPOINT_ROWS,
                FIRST_PROGRESS_CHECKPOINT_ROWS * 2
            ],
            "checkpoints must fire at 20k then 40k kept rows, not again before 80k \
             (which the 70k-row fixture never reaches)"
        );
    }

    #[test]
    fn a_file_smaller_than_the_first_checkpoint_never_checkpoints() {
        let bytes = many_rows_csv(10);
        let mut checkpoint_count = 0;

        parse_capturing_all_columns_with_progress(&bytes, |_snapshot, _columns| {
            checkpoint_count += 1;
        })
        .expect("small CSV must parse");

        assert_eq!(
            checkpoint_count, 0,
            "a file with fewer kept rows than the first checkpoint threshold \
             finishes before any progress update would be useful"
        );
    }

    #[test]
    fn a_checkpoint_snapshot_is_a_true_prefix_of_the_final_parse() {
        let bytes = many_rows_csv(70_000);
        let mut first_checkpoint: Option<(CsvParseOutcome, Vec<ColumnText>)> = None;

        let (final_outcome, final_columns) =
            parse_capturing_all_columns_with_progress(&bytes, |snapshot, columns| {
                if first_checkpoint.is_none() {
                    first_checkpoint = Some((snapshot.clone(), columns.to_vec()));
                }
            })
            .expect("many-row CSV must parse");

        let (checkpoint_outcome, checkpoint_columns) =
            first_checkpoint.expect("at least one checkpoint must have fired");

        assert_eq!(checkpoint_outcome.row_count, FIRST_PROGRESS_CHECKPOINT_ROWS);
        assert_eq!(checkpoint_outcome.column_names, final_outcome.column_names);
        assert_eq!(checkpoint_outcome.delimiter, final_outcome.delimiter);

        // Every field captured so far must equal the same-index field in the
        // final, complete columns — a checkpoint is a true prefix, never a
        // resampled or otherwise different reading of the same rows.
        for (checkpoint_column, final_column) in checkpoint_columns.iter().zip(&final_columns) {
            assert_eq!(checkpoint_column.len() as u64, checkpoint_outcome.row_count);
            for i in 0..checkpoint_column.len() {
                assert_eq!(checkpoint_column.get(i), final_column.get(i));
            }
        }
    }

    #[test]
    fn progress_checkpoints_do_not_change_the_final_parse_result() {
        let bytes = many_rows_csv(70_000);

        let (outcome_without_progress, columns_without_progress) =
            parse_capturing_all_columns(&bytes).expect("parse without progress");
        let (outcome_with_progress, columns_with_progress) =
            parse_capturing_all_columns_with_progress(&bytes, |_snapshot, _columns| {})
                .expect("parse with progress");

        assert_eq!(outcome_without_progress, outcome_with_progress);
        assert_eq!(columns_without_progress.len(), columns_with_progress.len());
        for (a, b) in columns_without_progress.iter().zip(&columns_with_progress) {
            assert_eq!(a.iter().collect::<Vec<_>>(), b.iter().collect::<Vec<_>>());
        }
    }
}
