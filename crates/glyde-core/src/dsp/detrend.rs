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

//! Per-segment detrending for Welch's method (docs/SPEC.md §3.2: "Mean
//! removal per segment (detrend = constant) by default, documented").
//!
//! Locked by the golden tests in `crates/glyde-core/tests/golden/welch.rs`
//! (docs/QUALITY.md §2 Welch PSD). Never widen a golden test's tolerance or
//! change its expectations to make an implementation pass — if one looks
//! wrong, that is a `blocking-decision` issue, not an edit.

/// Detrend method applied to each segment before windowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detrend {
    /// No detrending; the segment is used as-is.
    None,
    /// Subtract the segment's own mean (SPEC §3.2 default).
    Constant,
}

/// Applies `method` to `segment` in place.
pub fn apply(segment: &mut [f64], method: Detrend) {
    match method {
        Detrend::None => {}
        Detrend::Constant => {
            if segment.is_empty() {
                return;
            }
            let mean = segment.iter().sum::<f64>() / segment.len() as f64;
            for x in segment.iter_mut() {
                *x -= mean;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_leaves_the_segment_unchanged() {
        let original = vec![1.0, 5.0, -3.0, 100.0];
        let mut segment = original.clone();
        apply(&mut segment, Detrend::None);
        assert_eq!(segment, original);
    }

    #[test]
    fn constant_subtracts_the_mean_to_within_floating_point_precision() {
        let mut segment = vec![10.0, 20.0, 30.0, 40.0];
        apply(&mut segment, Detrend::Constant);
        assert_eq!(segment, vec![-15.0, -5.0, 5.0, 15.0]);
        let residual_mean = segment.iter().sum::<f64>() / segment.len() as f64;
        assert!(residual_mean.abs() < 1e-12);
    }

    #[test]
    fn constant_on_an_empty_segment_never_panics() {
        let mut segment: Vec<f64> = Vec::new();
        apply(&mut segment, Detrend::Constant);
        assert!(segment.is_empty());
    }
}
