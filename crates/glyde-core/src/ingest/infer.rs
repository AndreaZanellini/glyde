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

//! Ingestion inference (docs/ARCHITECTURE.md `ingest/infer.rs`).
//!
//! Encoding inference (SPEC §1.2.1) lands first; delimiter, header, and
//! decimal-separator inference (SPEC §1.2.2-1.2.4) join it here. Dtype
//! inference is a later docs/ROADMAP.md M2 item.

use chardetng::EncodingDetector;
use encoding_rs::Encoding;
use std::collections::HashMap;
use tracing::{info, warn};

/// SPEC §1.2: every inference is made from a bounded head sample, never the
/// whole file.
pub const HEAD_SAMPLE_BYTES: usize = 1024 * 1024;

/// Above this fraction of invalid UTF-8 bytes in the head sample, the file is
/// treated as a genuinely different single-byte encoding rather than UTF-8
/// with isolated corruption (SPEC §1.3: "invalid byte sequences are
/// replaced, never fatal" describes stray corrupted bytes, not a
/// wholesale different encoding). 1% comfortably separates corpus case 12
/// (one stray byte in a clean ASCII/UTF-8 file, ~0.4%) from cases 8 and 9
/// (a real Windows-1252 file, >1%) — see the corpus-derived tests below.
const INVALID_BYTE_TOLERANCE_FRACTION: f64 = 0.01;

/// v1's frozen non-UTF-16 fallback (SPEC §1.2.1's encoding list has no CJK
/// or other multi-byte encodings). `chardetng`'s general-purpose guess can
/// land outside this set on short, mostly-ASCII samples like the corpus
/// fixtures (it has too little evidence to rule out e.g. Big5's two-byte
/// sequences); when that happens we clamp to this so the reported encoding
/// stays inside what Glyde v1 actually decodes correctly.
const FALLBACK_SINGLE_BYTE_ENCODING: &Encoding = encoding_rs::WINDOWS_1252;

/// How [`detect_encoding`] settled on its answer, for the inference-bar
/// confidence signal (SPEC §1.2 "Confidence is tracked per inference").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingSource {
    /// A byte-order mark determined the encoding outright.
    Bom,
    /// No BOM; a `chardetng` statistical heuristic guessed among UTF-8,
    /// Windows-1252 (also covers Latin-1, which WHATWG treats as an alias of
    /// Windows-1252), and the other encodings it supports.
    Heuristic,
}

/// Result of SPEC §1.2.1 encoding inference: which [`Encoding`] to decode
/// with, and how confident that choice is.
#[derive(Debug, Clone, Copy)]
pub struct EncodingInference {
    pub encoding: &'static Encoding,
    pub source: EncodingSource,
}

impl EncodingInference {
    /// The lowercase WHATWG label shown in the inference bar and recorded in
    /// logs (e.g. `"utf-8"`, `"windows-1252"`, `"utf-16le"`).
    pub fn label(&self) -> String {
        self.encoding.name().to_ascii_lowercase()
    }
}

/// Infers the encoding of `bytes` (SPEC §1.2.1): BOM sniffing first; then,
/// over a bounded head sample, UTF-8 with tolerance for isolated corruption
/// (SPEC §1.3); then a `chardetng` statistical heuristic, clamped to v1's
/// supported set. Never fails — some encoding is always returned, so callers
/// can always proceed to [`decode`].
pub fn detect_encoding(bytes: &[u8]) -> EncodingInference {
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes) {
        info!(
            encoding = encoding.name(),
            bom_len, "encoding inferred from byte-order mark"
        );
        return EncodingInference {
            encoding,
            source: EncodingSource::Bom,
        };
    }

    let head = &bytes[..bytes.len().min(HEAD_SAMPLE_BYTES)];
    let invalid_fraction = utf8_invalid_byte_fraction(head);
    if invalid_fraction <= INVALID_BYTE_TOLERANCE_FRACTION {
        info!(
            invalid_fraction,
            "encoding inferred as UTF-8 (no byte-order mark; isolated invalid bytes tolerated per SPEC §1.3)"
        );
        return EncodingInference {
            encoding: encoding_rs::UTF_8,
            source: EncodingSource::Heuristic,
        };
    }

    let mut detector = EncodingDetector::new();
    detector.feed(head, true);
    // `allow_utf8: false` — the tolerance check above already ruled out
    // treating this sample as UTF-8, so don't let the detector reconsider.
    let guess = detector.guess(None, false);
    let encoding = match guess.name() {
        "UTF-8" | "windows-1252" | "UTF-16LE" | "UTF-16BE" => guess,
        _ => {
            warn!(
                chardetng_guess = guess.name(),
                invalid_fraction,
                "heuristic guessed an encoding outside Glyde's v1 supported set (SPEC §1.2.1); \
                 falling back to windows-1252"
            );
            FALLBACK_SINGLE_BYTE_ENCODING
        }
    };
    info!(
        encoding = encoding.name(),
        invalid_fraction, "encoding inferred by heuristic (no byte-order mark present)"
    );
    EncodingInference {
        encoding,
        source: EncodingSource::Heuristic,
    }
}

/// Fraction of `bytes` that are not part of a valid UTF-8 sequence, scanning
/// past each invalid span (Unicode's "maximal subpart" replacement rule, the
/// same one `str::from_utf8`'s error reports and [`Encoding::decode`] use) to
/// find every one, not just the first.
fn utf8_invalid_byte_fraction(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }

    let mut remaining = bytes;
    let mut invalid = 0usize;
    loop {
        match std::str::from_utf8(remaining) {
            Ok(_) => break,
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                let bad_len = e.error_len().unwrap_or(remaining.len() - valid_up_to);
                invalid += bad_len;
                let next = valid_up_to + bad_len;
                if next >= remaining.len() {
                    break;
                }
                remaining = &remaining[next..];
            }
        }
    }
    invalid as f64 / bytes.len() as f64
}

/// Decodes `bytes` using an already-[`detect_encoding`]-inferred encoding.
/// Any byte-order mark for that encoding is stripped; invalid byte sequences
/// are replaced with U+FFFD rather than failing (SPEC §1.3: malformed data
/// must never block the user). Replacement is logged at `warn`, never
/// silent (CLAUDE.md Golden Rule 2).
pub fn decode(bytes: &[u8], inference: &EncodingInference) -> String {
    let (text, _, had_errors) = inference.encoding.decode(bytes);
    if had_errors {
        warn!(
            encoding = inference.encoding.name(),
            "invalid byte sequences encountered; replaced with U+FFFD"
        );
    }
    text.into_owned()
}

/// A field delimiter candidate (SPEC §1.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delimiter {
    Comma,
    Semicolon,
    Tab,
    Pipe,
    /// One or more consecutive whitespace characters, collapsed to a single
    /// field boundary (e.g. column-aligned fixed-width text).
    Whitespace,
}

/// Every candidate SPEC §1.2.2 names, in priority order: this is also the
/// tie-break order [`infer_delimiter`] uses when two candidates tokenize the
/// sample with identical consistency (e.g. a tab-delimited file is equally
/// consistent read as generic whitespace; the more specific `Tab` wins).
const DELIMITER_CANDIDATES: [Delimiter; 5] = [
    Delimiter::Comma,
    Delimiter::Semicolon,
    Delimiter::Tab,
    Delimiter::Pipe,
    Delimiter::Whitespace,
];

impl Delimiter {
    /// The literal string shown in the inference bar and recorded in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Delimiter::Comma => ",",
            Delimiter::Semicolon => ";",
            Delimiter::Tab => "\t",
            Delimiter::Pipe => "|",
            Delimiter::Whitespace => " ",
        }
    }

    /// The single byte a quote-aware CSV tokenizer should split on, or
    /// `None` for [`Delimiter::Whitespace`], which is handled separately
    /// (it collapses runs of whitespace rather than splitting on one byte).
    ///
    /// `pub(crate)`: also used by `ingest::csv`'s full-file streaming parse,
    /// which needs the same byte-vs-whitespace split this module's own
    /// sample tokenizer uses.
    pub(crate) fn as_csv_byte(self) -> Option<u8> {
        match self {
            Delimiter::Comma => Some(b','),
            Delimiter::Semicolon => Some(b';'),
            Delimiter::Tab => Some(b'\t'),
            Delimiter::Pipe => Some(b'|'),
            Delimiter::Whitespace => None,
        }
    }
}

/// Result of SPEC §1.2.2 delimiter inference.
#[derive(Debug, Clone, Copy)]
pub struct DelimiterInference {
    pub delimiter: Delimiter,
    /// Fraction of sampled lines that tokenized to `delimiter`'s dominant
    /// field count (SPEC §1.2 "confidence is tracked per inference"): `1.0`
    /// means every line agreed, lower values mean the choice was closer.
    pub consistency: f64,
}

/// Splits `sample` into fields per line for `delimiter`. Byte delimiters are
/// tokenized with a quote-aware CSV reader (`flexible`, so ragged lines don't
/// error out) — this is what keeps a comma inside a quoted field, or a
/// newline inside a quoted field, from being miscounted as an extra column or
/// row (corpus cases 6 and 7). Whitespace has no quoting convention in SPEC
/// §1.2.2's input class, so it is tokenized by collapsing each line's
/// whitespace runs instead.
fn tokenize_records(sample: &str, delimiter: Delimiter) -> Vec<Vec<String>> {
    match delimiter.as_csv_byte() {
        Some(byte) => {
            let mut reader = csv::ReaderBuilder::new()
                .delimiter(byte)
                .has_headers(false)
                .flexible(true)
                .from_reader(sample.as_bytes());
            reader
                .records()
                .filter_map(Result::ok)
                .map(|record| record.iter().map(str::to_string).collect::<Vec<String>>())
                .filter(|fields| !fields.is_empty())
                .collect()
        }
        None => sample
            .lines()
            .map(|line| {
                line.split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<String>>()
            })
            .filter(|fields| !fields.is_empty())
            .collect(),
    }
}

/// The most common field count across `records`, and how many records have
/// it. Ties (e.g. corpus case 14: 5 one-field preamble lines vs. 5 two-field
/// header+data lines) favor the larger field count — a multi-column split is
/// the more informative signal than a run of degenerate single-field lines.
fn dominant_field_count(records: &[Vec<String>]) -> Option<(usize, usize)> {
    let mut frequency: HashMap<usize, usize> = HashMap::new();
    for record in records {
        *frequency.entry(record.len()).or_insert(0) += 1;
    }
    frequency
        .into_iter()
        .max_by_key(|&(field_count, occurrences)| (occurrences, field_count))
}

/// Infers the field delimiter of `sample` (SPEC §1.2.2): each candidate is
/// scored by what fraction of lines tokenize to its dominant field count —
/// "column-count consistency", not raw character frequency, so a file with
/// far more commas-as-decimals than semicolons-as-delimiters (corpus case 2)
/// still resolves to the semicolon. `sample` must already be a bounded head
/// sample ([`HEAD_SAMPLE_BYTES`]), decoded ([`decode`]) text.
pub fn infer_delimiter(sample: &str) -> DelimiterInference {
    let mut best: Option<(Delimiter, f64)> = None;

    for &candidate in &DELIMITER_CANDIDATES {
        let records = tokenize_records(sample, candidate);
        if records.is_empty() {
            continue;
        }
        let Some((field_count, occurrences)) = dominant_field_count(&records) else {
            continue;
        };
        // A "dominant" single-field split carries no column information;
        // only candidates that actually separate multiple columns compete.
        if field_count < 2 {
            continue;
        }

        let consistency = occurrences as f64 / records.len() as f64;
        let is_better = match best {
            Some((_, best_consistency)) => consistency > best_consistency,
            None => true,
        };
        if is_better {
            best = Some((candidate, consistency));
        }
    }

    let (delimiter, consistency) = best.unwrap_or((Delimiter::Comma, 0.0));
    info!(
        delimiter = delimiter.as_str(),
        consistency, "delimiter inferred by column-count consistency (SPEC §1.2.2)"
    );
    DelimiterInference {
        delimiter,
        consistency,
    }
}

/// `.` or `,` as a decimal separator (SPEC §1.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalSeparator {
    Dot,
    Comma,
}

impl DecimalSeparator {
    pub fn as_str(self) -> &'static str {
        match self {
            DecimalSeparator::Dot => ".",
            DecimalSeparator::Comma => ",",
        }
    }
}

/// Result of SPEC §1.2.4 decimal-separator inference.
#[derive(Debug, Clone, Copy)]
pub struct DecimalSeparatorInference {
    pub separator: DecimalSeparator,
    /// How many sampled fields looked like a dot-decimal number (SPEC §1.2
    /// "confidence is tracked per inference"): compare against
    /// [`Self::comma_votes`] to judge how one-sided the choice was — e.g.
    /// `0` vs. `0` means no numeric evidence was seen at all.
    pub dot_votes: usize,
    /// How many sampled fields looked like a comma-decimal number.
    pub comma_votes: usize,
}

/// Whether `field` is entirely `<digits><separator><digits>` (an optional
/// leading `-` allowed): a decimal number written with `separator` as its
/// fractional mark, not merely a field that happens to contain the
/// character somewhere (which a timestamp or free-text field also might).
fn looks_like_decimal(field: &str, separator: char) -> bool {
    let Some(sep_index) = field.find(separator) else {
        return false;
    };
    let (int_part, rest) = field.split_at(sep_index);
    let frac_part = &rest[separator.len_utf8()..];
    let int_part = int_part.strip_prefix('-').unwrap_or(int_part);

    !int_part.is_empty()
        && !frac_part.is_empty()
        && int_part.bytes().all(|b| b.is_ascii_digit())
        && frac_part.bytes().all(|b| b.is_ascii_digit())
}

/// Infers the decimal separator of `sample` (SPEC §1.2.4), jointly with the
/// already-chosen `delimiter`: fields are tokenized per `delimiter` first
/// (so a comma consumed as the field separator can never also be read as a
/// decimal mark — the `1,5;2,3` trap in SPEC §1.2.4), then every field is
/// checked against both candidate separators and the more frequent one wins.
pub fn infer_decimal_separator(sample: &str, delimiter: Delimiter) -> DecimalSeparatorInference {
    let records = tokenize_records(sample, delimiter);

    let mut dot_votes = 0usize;
    let mut comma_votes = 0usize;
    for field in records.iter().flatten() {
        let field = field.trim();
        if looks_like_decimal(field, '.') {
            dot_votes += 1;
        } else if looks_like_decimal(field, ',') {
            comma_votes += 1;
        }
    }

    let separator = if comma_votes > dot_votes {
        DecimalSeparator::Comma
    } else {
        DecimalSeparator::Dot
    };
    info!(
        separator = separator.as_str(),
        dot_votes,
        comma_votes,
        "decimal separator inferred jointly with the delimiter (SPEC §1.2.4)"
    );
    DecimalSeparatorInference {
        separator,
        dot_votes,
        comma_votes,
    }
}

/// Result of SPEC §1.2.3 header detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderInference {
    /// Line index of the header row, or `None` if the data starts at line 0
    /// with no header.
    pub header_row_index: Option<usize>,
    /// Leading lines discarded before the header (or before the data, if
    /// there is no header) — a metadata preamble (corpus case 14).
    pub skipped_preamble_rows: usize,
    /// Column labels: the header's own fields if one was found, otherwise
    /// synthesized `column_0`, `column_1`, ... in field order.
    pub column_names: Vec<String>,
    /// A leading preamble existed, but no line within it shared the data
    /// rows' field count, so no header candidate could be identified (SPEC
    /// §1.2 "confidence is tracked per inference"). Column names still had
    /// to be synthesized in this case, but — unlike a clean headerless file
    /// (corpus case 15, where `false`) — that is a guess worth surfacing
    /// rather than presenting with full confidence.
    pub ambiguous: bool,
}

/// Whether `row` looks like a header/label line rather than a data row: every
/// field fails to parse as a plausible data value. A real data row commonly
/// mixes a recognizable column (a timestamp) with ones this crude check
/// can't classify — a text column (corpus case 6's site names, case 7's
/// free-text notes) or a comma-decimal number (case 2) — so it takes only
/// *one* data-looking field to rule a row out as a label; requiring *all*
/// fields to look like data would misclassify those data rows as preamble.
fn row_looks_like_header_label(row: &[String]) -> bool {
    !row.is_empty() && row.iter().all(|field| !field_looks_like_data(field.trim()))
}

fn field_looks_like_data(field: &str) -> bool {
    if field.is_empty() {
        return false;
    }
    if field.parse::<f64>().is_ok() {
        return true;
    }
    // A crude but sufficient ISO-8601-leaning check: a 4-digit year followed
    // by `-`, e.g. "2026-01-01T00:00:00Z". Full timestamp-format parsing is
    // docs/ROADMAP.md M2's separate time-index item.
    let bytes = field.as_bytes();
    bytes.len() >= 5 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-'
}

/// Detects the header row of `sample` under `delimiter` (SPEC §1.2.3): the
/// header is the **last** non-data line whose field count matches the data
/// rows, not merely the first — a units row directly under a label row
/// (`timestamp,value` / `s,V` / data...) would otherwise be mistaken for
/// data-adjacent noise instead of recognized as the closer, more authoritative
/// candidate. Leading lines before the first data-looking row are the
/// preamble to search; if none of them share the data rows' field count, no
/// header candidate exists and the result is flagged [`HeaderInference::ambiguous`].
/// If there is no preamble at all — the very first line already looks like
/// data (corpus case 15) — there is cleanly no header, and column names are
/// synthesized.
pub fn infer_header(sample: &str, delimiter: Delimiter) -> HeaderInference {
    let records = tokenize_records(sample, delimiter);

    let data_start = records
        .iter()
        .position(|record| !row_looks_like_header_label(record))
        .unwrap_or(records.len());
    let preamble = &records[..data_start];
    let data_field_count = records.get(data_start).map_or(0, Vec::len);

    let header_index = preamble
        .iter()
        .enumerate()
        .rev()
        .find(|(_, record)| record.len() == data_field_count)
        .map(|(index, _)| index);

    match header_index {
        Some(index) => {
            info!(
                header_row_index = index,
                skipped_preamble_rows = index,
                "header row detected (SPEC §1.2.3)"
            );
            HeaderInference {
                header_row_index: Some(index),
                skipped_preamble_rows: index,
                column_names: records[index]
                    .iter()
                    .map(|field| field.trim().to_string())
                    .collect(),
                ambiguous: false,
            }
        }
        None => {
            let ambiguous = data_start > 0;
            if ambiguous {
                warn!(
                    skipped_preamble_rows = data_start,
                    "a preamble was found but no line in it matched the data rows' field count; \
                     header detection is ambiguous (SPEC §1.2.3)"
                );
            } else {
                info!("no header row detected; data starts immediately (SPEC §1.2.3)");
            }
            HeaderInference {
                header_row_index: None,
                skipped_preamble_rows: data_start,
                column_names: (0..data_field_count)
                    .map(|i| format!("column_{i}"))
                    .collect(),
                ambiguous,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

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

    // Corpus case 8 (QUALITY.md §1.8): Latin-1 header with `°C`, `µm/s²`.
    // WHATWG treats "latin1" as an alias of windows-1252, so that is the
    // label a correct heuristic reports.
    #[test]
    fn corpus_case_08_latin1_degree_micro_infers_windows_1252() {
        let bytes = corpus_bytes("case-08-latin1-degree-micro.csv");

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.label(), "windows-1252");
        assert_eq!(inference.source, EncodingSource::Heuristic);

        let text = decode(&bytes, &inference);
        assert!(text.contains("temperature [°C]"), "text was: {text}");
        assert!(text.contains("acceleration [µm/s²]"), "text was: {text}");
    }

    // Corpus case 9: Windows-1252 with smart quotes.
    #[test]
    fn corpus_case_09_windows1252_smart_quotes_infers_windows_1252() {
        let bytes = corpus_bytes("case-09-windows1252-smart-quotes.csv");

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.label(), "windows-1252");
        assert_eq!(inference.source, EncodingSource::Heuristic);

        let text = decode(&bytes, &inference);
        assert!(
            text.contains('\u{201c}') || text.contains('\u{2018}'),
            "text was: {text}"
        );
    }

    // Corpus case 10: UTF-8 with BOM. The BOM must be detected and stripped
    // from the decoded text, not left as a leading U+FEFF / mojibake.
    #[test]
    fn corpus_case_10_utf8_bom_infers_utf8_and_strips_the_bom() {
        let bytes = corpus_bytes("case-10-utf8-bom.csv");
        assert_eq!(
            &bytes[..3],
            [0xEF, 0xBB, 0xBF],
            "fixture must start with a UTF-8 BOM"
        );

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.label(), "utf-8");
        assert_eq!(inference.source, EncodingSource::Bom);

        let text = decode(&bytes, &inference);
        assert!(text.starts_with("timestamp,"), "text was: {text:?}");
    }

    // Corpus case 11: UTF-16LE.
    #[test]
    fn corpus_case_11_utf16le_infers_utf16le() {
        let bytes = corpus_bytes("case-11-utf16le.csv");

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.label(), "utf-16le");
        assert_eq!(inference.source, EncodingSource::Bom);

        let text = decode(&bytes, &inference);
        assert!(text.starts_with("timestamp,"), "text was: {text:?}");
    }

    // Corpus case 12: an otherwise-clean UTF-8 file with an invalid byte
    // sequence mid-file. SPEC §1.3/§1.2.1: invalid bytes are replaced, never
    // fatal, and never cause a whole-file misdetection away from UTF-8.
    #[test]
    fn corpus_case_12_invalid_byte_sequences_stays_utf8_with_replacement() {
        let bytes = corpus_bytes("case-12-invalid-byte-sequences.csv");

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.label(), "utf-8");
        assert_eq!(inference.source, EncodingSource::Heuristic);

        let text = decode(&bytes, &inference);
        assert!(
            text.contains('\u{FFFD}'),
            "invalid byte must be replaced with U+FFFD; text was: {text:?}"
        );
        assert!(text.contains("sensor"), "text was: {text:?}");
    }

    #[test]
    fn plain_ascii_with_no_bom_infers_utf8() {
        let bytes = b"timestamp,value\n2026-01-01T00:00:00Z,1.5\n".to_vec();

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.label(), "utf-8");
        assert_eq!(inference.source, EncodingSource::Heuristic);

        let text = decode(&bytes, &inference);
        assert_eq!(text, String::from_utf8(bytes).unwrap());
    }

    #[test]
    fn a_single_stray_byte_in_a_large_ascii_sample_stays_utf8() {
        // A single invalid byte in a 10 KB otherwise-ASCII sample is well
        // under the 1% tolerance, regardless of the sample's absolute size.
        let mut bytes = vec![b'a'; 10_000];
        bytes[5_000] = 0xFF;

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.label(), "utf-8");
    }

    #[test]
    fn dense_high_bytes_that_chardetng_misreads_still_clamp_to_windows_1252() {
        // These three bytes (0xB0 'C', 0xB5 'm', per corpus case 8) are
        // dense enough to fail the UTF-8 tolerance check, and syntactically
        // resemble valid Big5 sequences to chardetng on a short sample —
        // exactly the corpus 8 scenario. v1 has no Big5 support (SPEC
        // §1.2.1), so the out-of-scope guess must clamp to windows-1252
        // rather than propagate an encoding Glyde cannot actually decode.
        let bytes = corpus_bytes("case-08-latin1-degree-micro.csv");

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.encoding, encoding_rs::WINDOWS_1252);
    }

    #[test]
    fn utf8_invalid_byte_fraction_is_zero_for_valid_utf8() {
        assert_eq!(utf8_invalid_byte_fraction(b"clean ascii text"), 0.0);
        assert_eq!(utf8_invalid_byte_fraction("caf\u{e9}".as_bytes()), 0.0);
    }

    #[test]
    fn utf8_invalid_byte_fraction_counts_every_invalid_span() {
        // Two lone invalid bytes (0xFF is never valid in UTF-8) among 10
        // total bytes.
        let bytes = [b'a', b'b', 0xFF, b'c', b'd', 0xFF, b'e', b'f', b'g', b'h'];
        assert_eq!(utf8_invalid_byte_fraction(&bytes), 0.2);
    }

    #[test]
    fn utf8_invalid_byte_fraction_of_empty_input_is_zero() {
        assert_eq!(utf8_invalid_byte_fraction(&[]), 0.0);
    }

    #[test]
    fn utf16be_bom_is_detected_from_the_bom() {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in "ok".encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }

        let inference = detect_encoding(&bytes);

        assert_eq!(inference.label(), "utf-16be");
        assert_eq!(inference.source, EncodingSource::Bom);
    }

    #[test]
    fn decode_never_fails_on_arbitrary_bytes() {
        // Property-adjacent smoke test: whatever detect_encoding guesses,
        // decode() must produce a String, never panic, for any byte soup.
        let bytes: Vec<u8> = (0..=255u8).collect();
        let inference = detect_encoding(&bytes);
        let _ = decode(&bytes, &inference);
    }

    // --- SPEC §1.2.2-1.2.4: joint delimiter / header / decimal-separator
    // inference (docs/ROADMAP.md M2). Golden tests written first, against
    // the torture corpus, per CLAUDE.md's TDD rule for glyde-core::ingest.

    fn corpus_sample(file_name: &str) -> String {
        let bytes = corpus_bytes(file_name);
        let inference = detect_encoding(&bytes);
        decode(&bytes, &inference)
    }

    #[test]
    fn corpus_case_01_comma_clean_infers_comma_and_dot() {
        let sample = corpus_sample("case-01-comma-clean.csv");

        let delimiter_inference = infer_delimiter(&sample);
        let delimiter = delimiter_inference.delimiter;
        assert_eq!(delimiter.as_str(), ",");
        assert_eq!(delimiter_inference.consistency, 1.0);

        let decimal_inference = infer_decimal_separator(&sample, delimiter);
        assert_eq!(decimal_inference.separator.as_str(), ".");
        assert!(decimal_inference.dot_votes > 0);
        assert_eq!(decimal_inference.comma_votes, 0);

        let header = infer_header(&sample, delimiter);
        assert_eq!(header.header_row_index, Some(0));
        assert_eq!(header.skipped_preamble_rows, 0);
        assert_eq!(header.column_names, vec!["timestamp", "value", "pressure"]);
        assert!(!header.ambiguous);
    }

    // The SPEC §1.2.4 trap case verbatim: `1,5;2,3` must be read as two
    // columns with comma decimals, never four comma-split columns.
    #[test]
    fn corpus_case_02_semicolon_comma_decimal_is_not_split_on_the_comma() {
        let sample = corpus_sample("case-02-semicolon-comma-decimal.csv");

        let delimiter = infer_delimiter(&sample).delimiter;
        assert_eq!(delimiter.as_str(), ";");

        let decimal = infer_decimal_separator(&sample, delimiter).separator;
        assert_eq!(decimal.as_str(), ",");

        let header = infer_header(&sample, delimiter);
        assert_eq!(header.column_names, vec!["timestamp", "value", "pressure"]);
    }

    #[test]
    fn corpus_case_03_tab_delimited_infers_tab_not_generic_whitespace() {
        let sample = corpus_sample("case-03-tab-delimited.tsv");

        let delimiter = infer_delimiter(&sample).delimiter;
        assert_eq!(delimiter.as_str(), "\t");

        let decimal = infer_decimal_separator(&sample, delimiter).separator;
        assert_eq!(decimal.as_str(), ".");
    }

    #[test]
    fn corpus_case_04_whitespace_aligned_collapses_padding_runs() {
        let sample = corpus_sample("case-04-whitespace-aligned.txt");

        let delimiter = infer_delimiter(&sample).delimiter;
        assert_eq!(delimiter.as_str(), " ");

        let decimal = infer_decimal_separator(&sample, delimiter).separator;
        assert_eq!(decimal.as_str(), ".");

        let header = infer_header(&sample, delimiter);
        assert_eq!(header.column_names, vec!["timestamp", "value", "pressure"]);
    }

    #[test]
    fn corpus_case_05_pipe_delimited_infers_pipe() {
        let sample = corpus_sample("case-05-pipe-delimited.csv");

        let delimiter = infer_delimiter(&sample).delimiter;
        assert_eq!(delimiter.as_str(), "|");

        let decimal = infer_decimal_separator(&sample, delimiter).separator;
        assert_eq!(decimal.as_str(), ".");
    }

    // Column-count consistency must be quote-aware: the comma inside the
    // quoted field must not be counted as a field separator.
    #[test]
    fn corpus_case_06_quoted_field_with_delimiter_is_not_over_split() {
        let sample = corpus_sample("case-06-quoted-field-with-delimiter.csv");

        let delimiter = infer_delimiter(&sample).delimiter;
        assert_eq!(delimiter.as_str(), ",");

        let header = infer_header(&sample, delimiter);
        assert_eq!(header.column_names, vec!["timestamp", "site", "value"]);
    }

    // Quote-aware row counting: the embedded newlines inside quoted fields
    // must not be counted as extra rows/lines.
    #[test]
    fn corpus_case_07_quoted_field_with_newline_is_not_over_split() {
        let sample = corpus_sample("case-07-quoted-field-with-newline.csv");

        let delimiter = infer_delimiter(&sample).delimiter;
        assert_eq!(delimiter.as_str(), ",");

        let decimal = infer_decimal_separator(&sample, delimiter).separator;
        assert_eq!(decimal.as_str(), ".");

        let header = infer_header(&sample, delimiter);
        assert_eq!(header.column_names, vec!["timestamp", "note", "value"]);
    }

    // SPEC §1.2.3: the header row is the last non-data line whose field
    // count matches the data rows; the 5 leading "#"-prefixed metadata
    // lines above it are preamble, not header candidates.
    #[test]
    fn corpus_case_14_metadata_preamble_skips_five_junk_lines() {
        let sample = corpus_sample("case-14-metadata-preamble.csv");

        let delimiter = infer_delimiter(&sample).delimiter;
        assert_eq!(delimiter.as_str(), ",");

        let header = infer_header(&sample, delimiter);
        assert_eq!(header.skipped_preamble_rows, 5);
        assert_eq!(header.header_row_index, Some(5));
        assert_eq!(header.column_names, vec!["timestamp", "value"]);
    }

    // No header at all: the first line already looks like data, so column
    // names must be synthesized as "column_0", "column_1", ... .
    #[test]
    fn corpus_case_15_no_header_synthesizes_column_names() {
        let sample = corpus_sample("case-15-no-header.csv");

        let delimiter = infer_delimiter(&sample).delimiter;
        assert_eq!(delimiter.as_str(), ",");

        let header = infer_header(&sample, delimiter);
        assert_eq!(header.header_row_index, None);
        assert_eq!(header.skipped_preamble_rows, 0);
        assert_eq!(header.column_names, vec!["column_0", "column_1"]);
    }

    #[test]
    fn infer_delimiter_on_empty_sample_falls_back_to_comma_without_panicking() {
        assert_eq!(infer_delimiter("").delimiter.as_str(), ",");
    }

    #[test]
    fn infer_header_on_empty_sample_reports_no_header_and_no_columns() {
        let header = infer_header("", Delimiter::Comma);
        assert_eq!(header.header_row_index, None);
        assert_eq!(header.skipped_preamble_rows, 0);
        assert!(header.column_names.is_empty());
    }

    #[test]
    fn infer_delimiter_on_a_single_column_file_still_picks_a_delimiter() {
        // No candidate ever splits this into >1 column; the comma fallback
        // must not panic, and decimal/header inference must still run on
        // whatever it returns.
        let sample = "value\n1.5\n1.6\n1.7\n";

        let delimiter = infer_delimiter(sample).delimiter;
        assert_eq!(delimiter.as_str(), ",");

        let header = infer_header(sample, delimiter);
        assert_eq!(header.header_row_index, Some(0));
        assert_eq!(header.column_names, vec!["value"]);
    }

    #[test]
    fn looks_like_decimal_rejects_a_bare_timestamp_field() {
        assert!(!looks_like_decimal("2026-01-01T00:00:00Z", '.'));
        assert!(!looks_like_decimal("2026-01-01T00:00:00Z", ','));
    }

    // Regression for a header-detection bug: SPEC §1.2.3 says the header is
    // the *last* non-data line whose field count matches the data, not the
    // first. A units row directly under the label row (a plausible
    // instrument-export shape, not yet in the torture corpus) is the case
    // that tells the two apart — both "timestamp,value" and "s,V" are
    // non-data 2-field lines, and "s,V" is the one closer to the data.
    #[test]
    fn infer_header_prefers_the_last_matching_line_over_the_first() {
        let sample = "timestamp,value\ns,V\n2026-01-01T00:00:00Z,1.5\n2026-01-01T00:00:01Z,1.6\n";

        let header = infer_header(sample, Delimiter::Comma);

        assert_eq!(header.header_row_index, Some(1));
        assert_eq!(header.skipped_preamble_rows, 1);
        assert_eq!(header.column_names, vec!["s", "V"]);
        assert!(!header.ambiguous);
    }

    // A preamble exists, but nothing in it shares the data rows' field
    // count: there is no usable header candidate, and this must be flagged
    // as ambiguous rather than silently synthesizing column names as if the
    // file cleanly had no header at all (corpus case 15's situation, which
    // is not ambiguous because it has no preamble in the first place).
    #[test]
    fn infer_header_flags_ambiguous_when_no_preamble_line_matches_the_data_shape() {
        let sample = "# note about the file\n2026-01-01T00:00:00Z,1.5\n2026-01-01T00:00:01Z,1.6\n";

        let header = infer_header(sample, Delimiter::Comma);

        assert_eq!(header.header_row_index, None);
        assert_eq!(header.skipped_preamble_rows, 1);
        assert_eq!(header.column_names, vec!["column_0", "column_1"]);
        assert!(header.ambiguous);
    }

    #[test]
    fn infer_header_on_a_clean_headerless_file_is_not_ambiguous() {
        // corpus case 15's situation restated directly: no preamble at all
        // (the first line already looks like data), so this is a confident
        // "no header", not an ambiguous one.
        let header = infer_header("2026-01-01T00:00:00Z,1.5\n", Delimiter::Comma);

        assert_eq!(header.header_row_index, None);
        assert_eq!(header.skipped_preamble_rows, 0);
        assert!(!header.ambiguous);
    }
}
