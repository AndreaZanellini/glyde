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

//! Corpus manifest gate (docs/QUALITY.md §1, docs/ROADMAP.md M1).
//!
//! `every_committed_case_has_a_well_formed_expected_json` runs on every PR
//! and covers whatever cases are committed so far. `corpus_has_all_56_cases`
//! is the milestone-complete gate: it stays `#[ignore]`d until the last
//! corpus batch (QUALITY.md §1, cases 53-56) lands, per the M1 convention of
//! writing golden/manifest tests before the work they gate.

mod support;

use support::{corpus_dir, discover_cases, load_expected};

const EXPECTED_CORPUS_SIZE: usize = 56;

#[test]
fn every_committed_case_has_a_well_formed_expected_json() {
    let dir = corpus_dir();
    let cases = discover_cases(&dir).expect("read testdata/corpus");

    for case in &cases {
        assert!(
            case.data_path.exists(),
            "corpus case '{}' has an expected.json ({}) but no sibling data file/directory",
            case.name,
            case.expected_path.display(),
        );

        load_expected(&case.expected_path)
            .unwrap_or_else(|e| panic!("invalid expected.json for case '{}': {e}", case.name));
    }
}

#[test]
#[ignore = "un-ignore once the last corpus batch (QUALITY.md §1, cases 53-56) is \
            committed — see docs/ROADMAP.md M1"]
fn corpus_has_all_56_cases() {
    let dir = corpus_dir();
    let cases = discover_cases(&dir).expect("read testdata/corpus");

    assert_eq!(
        cases.len(),
        EXPECTED_CORPUS_SIZE,
        "expected all {EXPECTED_CORPUS_SIZE} torture-corpus cases (docs/QUALITY.md §1) \
         to be committed under testdata/corpus/"
    );
}
