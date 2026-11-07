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

//! Issue #202, SPEC §2.2: "The classification and the estimated sampling
//! rate are always shown in the inference bar." The report carries the
//! median sample interval the classification was measured against, in
//! seconds, for an absolute time axis.

use glyde_core::ingest::{self, SamplingClass};
use std::path::{Path, PathBuf};

fn corpus(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corpus")
        .join(name)
}

#[test]
fn the_report_carries_the_median_sample_interval() {
    for (name, class) in [
        ("case-01-comma-clean.csv", SamplingClass::Uniform),
        (
            "case-40-segmented-three-bursts.csv",
            SamplingClass::SegmentedUniform,
        ),
    ] {
        let (_, report, _) = ingest::open_dataset(&corpus(name)).expect("opens");
        assert_eq!(report.sampling_class, class, "{name}");
        assert_eq!(report.median_interval_seconds, Some(1.0), "{name}");
    }
}

#[test]
fn a_millisecond_epoch_axis_is_measured_in_seconds() {
    let (_, report, _) =
        ingest::open_dataset(&corpus("case-30-epoch-milliseconds.csv")).expect("opens");
    // 100 ms steps.
    assert_eq!(report.median_interval_seconds, Some(0.1));
}

#[test]
fn a_progressive_index_has_no_interval_in_seconds() {
    let (_, report, _) =
        ingest::open_dataset(&corpus("case-35-progressive-integer-index.csv")).expect("opens");
    assert_eq!(report.median_interval_seconds, None);
}
