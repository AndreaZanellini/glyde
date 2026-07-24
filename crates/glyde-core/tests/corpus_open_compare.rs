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

//! Activates docs/QUALITY.md §1's corpus gate for every case docs/ROADMAP.md
//! M2 has implemented inference for so far (docs/ROADMAP.md M2 "Activate
//! corpus open→compare gate for all cases handled so far"):
//! `glyde_core::ingest::inspect` is run against each committed delimited-text
//! corpus file and compared field-by-field against its `.expected.json`,
//! rather than only validating the JSON's own shape
//! (`tests/corpus_manifest.rs`'s unconditional job).
//!
//! Gated behind the `corpus-open-compare` feature (on by default) so a
//! future PR can flip it off if a not-yet-implemented format needs to land
//! ahead of satisfying every one of its cases.
//!
//! Parquet (corpus cases 53-56, docs/ROADMAP.md M7) have no reader
//! registered yet — `inspect` cannot open them at all today — so they are
//! not part of "so far" and are explicitly skipped here, not asserted, until
//! M7 lands a Parquet `Reader`.
//!
//! Case 21 is also skipped, for a different reason: it is a genuine
//! disagreement this gate uncovered between two already-existing,
//! independently-authored pieces (`time::classify_sampling`'s robust-CV
//! statistic and case 21's `.expected.json`), not something this PR should
//! resolve by quietly editing either one — see issue #48 for the root cause
//! and the maintainer decision it's waiting on.

#![cfg(feature = "corpus-open-compare")]

mod support;

use glyde_core::ingest::inspect;
use support::{compare, corpus_dir, discover_cases, load_expected, ExpectedOutcome};

/// docs/ROADMAP.md M7, not yet started: no `Reader` claims `.parquet`, and
/// case 55 is a directory `inspect` cannot even memory-map as a file.
const NOT_YET_IMPLEMENTED: &[&str] = &[
    "case-53-clean-parquet-timestamp",
    "case-54-parquet-with-nulls",
    "case-55-partitioned-parquet-directory",
    "case-56-parquet-dictionary-strings",
];

/// Issue #48: `classify_sampling`'s robust-CV is degenerate for the 2-delta
/// segment case 21's ragged-row salvage leaves behind, so it disagrees with
/// case 21's already-committed `.expected.json`. Awaiting a maintainer
/// decision, not something to silently paper over here.
const AWAITING_MAINTAINER_DECISION: &[&str] = &["case-21-ragged-rows"];

#[test]
fn every_implemented_corpus_case_opens_and_matches_its_expected_json() {
    let dir = corpus_dir();
    let cases = discover_cases(&dir).expect("read testdata/corpus");
    assert!(!cases.is_empty(), "the corpus must be discoverable");

    let skipped = NOT_YET_IMPLEMENTED.len() + AWAITING_MAINTAINER_DECISION.len();
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for case in &cases {
        if NOT_YET_IMPLEMENTED.contains(&case.name.as_str())
            || AWAITING_MAINTAINER_DECISION.contains(&case.name.as_str())
        {
            continue;
        }
        checked += 1;

        let expected = load_expected(&case.expected_path)
            .unwrap_or_else(|e| panic!("{}: invalid expected.json: {e}", case.name));

        match (inspect(&case.data_path), expected) {
            (Ok(actual), ExpectedOutcome::Open(expected)) => {
                let mismatches = compare(&actual, &expected);
                if !mismatches.is_empty() {
                    failures.push(format!(
                        "{}: open() disagreed with .expected.json: {mismatches:#?}",
                        case.name
                    ));
                }
            }
            (Err(err), ExpectedOutcome::Error { error }) => {
                if err.to_string() != error {
                    failures.push(format!(
                        "{}: error message disagreed with .expected.json (expected {error:?}, got {err:?})",
                        case.name
                    ));
                }
            }
            (Ok(_), ExpectedOutcome::Error { error }) => {
                failures.push(format!(
                    "{}: expected a failed open ({error:?}) but it opened successfully",
                    case.name
                ));
            }
            (Err(err), ExpectedOutcome::Open(_)) => {
                failures.push(format!(
                    "{}: expected a successful open but got an error: {err}",
                    case.name
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} corpus case(s) disagreed with open():\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(
        checked,
        cases.len() - skipped,
        "every case not in a documented skip list must have been checked against a real open()"
    );
}
