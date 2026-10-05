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

//! SPEC §3.3's PSD behavior on non-uniform data, proven on the torture
//! corpus through the real loader (docs/QUALITY.md §1 cases 39 and 40,
//! docs/ROADMAP.md M5 "`SegmentedUniform` ... `Irregular`").

use glyde_core::dsp::psd::{
    plan_psd, PsdMemoryCap, PsdSettings, PsdUnavailable, PSD_MEMORY_CAP_BYTES,
};
use glyde_core::dsp::welch::MIN_SEGMENT_LEN;
use glyde_core::ingest::load;
use std::path::{Path, PathBuf};

fn corpus_path(file_name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/corpus")
        .join(file_name)
}

fn plan_whole_file(file_name: &str) -> Result<glyde_core::dsp::psd::PsdPlan, PsdUnavailable> {
    let dataset = load(&corpus_path(file_name)).expect("corpus file opens");
    plan_psd(
        &dataset.time,
        0..dataset.time.len(),
        &PsdSettings::default(),
        dataset.columns.len(),
        PsdMemoryCap::from_bytes(PSD_MEMORY_CAP_BYTES),
    )
    .expect("an in-memory axis cannot fail to scan")
}

#[test]
fn corpus_39_irregular_event_log_has_its_psd_disabled_with_an_explanation() {
    let refusal = plan_whole_file("case-39-irregular-event-log.csv").unwrap_err();

    assert_eq!(
        refusal,
        PsdUnavailable::Irregular {
            largest_uniform: None
        },
        "six irregular events hold no uniform stretch long enough for one window"
    );
    let explanation = refusal.to_string();
    assert!(explanation.contains("PSD requires uniform sampling"));
    assert!(explanation.contains("irregular timestamps"));
}

#[test]
fn corpus_40_three_bursts_are_planned_per_segment_and_too_short_ones_are_reported() {
    let refusal = plan_whole_file("case-40-segmented-three-bursts.csv").unwrap_err();

    assert_eq!(
        refusal,
        PsdUnavailable::NoSegmentFitsAWindow {
            segment_count: 3,
            longest: 3,
            window: MIN_SEGMENT_LEN,
        },
        "each burst is three samples: all three are excluded, and the user is told why"
    );
    assert!(refusal.to_string().contains("3 segments"));
}
