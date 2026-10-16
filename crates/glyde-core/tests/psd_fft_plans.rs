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

//! Once the application has planned the standard FFT lengths at startup, a
//! PSD never creates an FFT plan (so it never asks `rustfft` for memory,
//! which `rustfft` allocates infallibly). The plan counter is process-wide,
//! so this file holds a single test.

use glyde_core::dsp::psd::{
    compute_psds, plan_psd_on, AxisScale, FrequencyUnit, PsdMemoryCap, PsdSettings, SegmentLength,
    PSD_MEMORY_CAP_BYTES, SEGMENT_LENGTH_CHOICES,
};
use glyde_core::dsp::welch::{fft_plans_created, prepare_fft_plans};

#[test]
fn after_startup_planning_no_psd_creates_an_fft_plan() {
    prepare_fft_plans();
    let prepared = fft_plans_created();
    assert_eq!(
        prepared,
        SEGMENT_LENGTH_CHOICES.len(),
        "one plan per standard length"
    );
    prepare_fft_plans();
    assert_eq!(
        fft_plans_created(),
        prepared,
        "preparing twice plans nothing new"
    );

    let scale = AxisScale {
        ticks_per_unit: 1_000_000_000,
        frequency_unit: FrequencyUnit::Hertz,
    };
    let cap = PsdMemoryCap::from_bytes(PSD_MEMORY_CAP_BYTES);
    let day = 86_400_000_000_000i128;
    // A uniform series, then the same with gaps (three 70 000-sample bursts).
    let uniform: Vec<i128> = (0..210_000i128).map(|n| n * 1_000_000).collect();
    let segmented: Vec<i128> = (0..210_000i128)
        .map(|n| (n / 70_000) * day + (n % 70_000) * 1_000_000)
        .collect();
    let columns: Vec<Vec<f64>> = (0..3)
        .map(|k| {
            (0..210_000)
                .map(|n| ((n * (k + 1)) as f64 * 0.01).sin())
                .collect()
        })
        .collect();
    let slices: Vec<&[f64]> = columns.iter().map(Vec::as_slice).collect();

    for ticks in [&uniform, &segmented] {
        for segment_length in std::iter::once(SegmentLength::Auto).chain(
            SEGMENT_LENGTH_CHOICES
                .iter()
                .map(|&len| SegmentLength::Fixed(len)),
        ) {
            let settings = PsdSettings {
                segment_length,
                ..PsdSettings::default()
            };
            let plan = plan_psd_on(&ticks[..], scale, 0..ticks.len(), &settings, 3, cap)
                .unwrap()
                .unwrap();
            compute_psds(&ticks[..], &slices, &plan, &|_| true)
                .unwrap()
                .unwrap();
        }
    }

    assert_eq!(
        fft_plans_created(),
        prepared,
        "a PSD with a standard window length must reuse the startup plans"
    );
}
