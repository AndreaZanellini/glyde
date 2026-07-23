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
//! Encoding inference (SPEC §1.2.1) lands first; delimiter, header, decimal
//! separator, and dtype inference are later docs/ROADMAP.md M2 items and
//! will join this module as they land.

use chardetng::EncodingDetector;
use encoding_rs::Encoding;
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
}
