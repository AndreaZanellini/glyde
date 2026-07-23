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

//! Property-test scaffolding (docs/QUALITY.md §2 Property tests,
//! docs/ROADMAP.md M1): invariants that must hold across the whole input
//! space, not just the fixed golden-test cases. The decimation property
//! below is still `#[ignore]`d because `dsp::decimation`'s pyramid lands in
//! docs/ROADMAP.md M3; `cargo test -- --ignored --list` is the M1
//! maintainer proof this scaffolding exists. Un-ignore each one only once
//! its implementation makes it pass — never loosen an assertion to make
//! that happen.

use glyde_core::dsp::decimation::{build_pyramid, decimate_viewport};
use glyde_core::ingest;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// SPEC §3.1: decimation renders `min`/`max` aggregates of the raw
    /// samples, so no rendered value can ever fall outside the series'
    /// own `[min, max]` range, for any random valid series and any pixel
    /// width.
    #[test]
    #[ignore = "docs/ROADMAP.md M3: pyramid/decimation not implemented yet"]
    fn rendering_never_panics_and_never_invents_values_outside_the_series_range(
        samples in prop::collection::vec(
            any::<f64>().prop_filter("finite", |v| v.is_finite()),
            1..2000,
        ),
        pixel_columns in 1usize..500,
    ) {
        let timestamps: Vec<i128> = (0..samples.len() as i128).collect();
        let pyramid = build_pyramid(&samples, &timestamps);
        let range = (timestamps[0], timestamps[timestamps.len() - 1]);
        let columns = decimate_viewport(&pyramid, &samples, &timestamps, range, pixel_columns);

        let series_min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let series_max = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        for bucket in &columns {
            prop_assert!(
                bucket.min >= series_min && bucket.min <= series_max,
                "bucket.min {} must lie within the series range [{series_min}, {series_max}]",
                bucket.min
            );
            prop_assert!(
                bucket.max >= series_min && bucket.max <= series_max,
                "bucket.max {} must lie within the series range [{series_min}, {series_max}]",
                bucket.max
            );
        }
    }

    /// SPEC §1.3: malformed user data must never `panic!` — ingestion
    /// returns errors as values. This is the fuzz-lite counterpart: any
    /// random byte sequence, not just the torture corpus's curated cases,
    /// handed to the CSV parser.
    #[test]
    fn csv_reader_never_panics_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..4096),
    ) {
        let _ = ingest::parse(&bytes);
    }
}
