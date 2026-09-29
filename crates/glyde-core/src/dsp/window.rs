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

//! Analysis windows for Welch's method (docs/SPEC.md §3.2).
//!
//! Symmetric (not periodic) coefficients — the standard choice for spectral
//! analysis of a fixed-length segment, as opposed to the periodic form used
//! for FIR filter design. Locked by the golden tests in
//! `crates/glyde-core/tests/golden/welch.rs` (docs/QUALITY.md §2 Welch PSD).
//! Never widen a golden test's tolerance or change its expectations to make
//! an implementation pass — if one looks wrong, that is a `blocking-decision`
//! issue, not an edit.

use std::f64::consts::PI;

/// The window functions SPEC §3.2 exposes as one of the "at most three
/// controls" behind the PSD settings affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// No tapering (box car). Used to isolate scaling/normalization behavior
    /// in golden tests, since it introduces no spectral leakage of its own.
    Rectangular,
    Hann,
    Hamming,
}

/// Per-sample coefficients for `window` over a segment of `len` samples.
pub fn coefficients(window: Window, len: usize) -> Vec<f64> {
    match window {
        Window::Rectangular => vec![1.0; len],
        Window::Hann => raised_cosine(len, 0.5, 0.5),
        Window::Hamming => raised_cosine(len, 0.54, 0.46),
    }
}

/// `a - b * cos(2*pi*n / (len - 1))`, the shared form of the Hann
/// (`a = b = 0.5`) and Hamming (`a = 0.54`, `b = 0.46`) windows. A window of
/// `len <= 1` has no `n / (len - 1)` interval to taper across, so every
/// coefficient is `1.0` (matches `Rectangular` for the degenerate case).
fn raised_cosine(len: usize, a: f64, b: f64) -> Vec<f64> {
    if len <= 1 {
        return vec![1.0; len];
    }
    let denom = (len - 1) as f64;
    (0..len)
        .map(|n| a - b * (2.0 * PI * n as f64 / denom).cos())
        .collect()
}

/// The mean-square of a window's coefficients, `(1/len) * sum(w[n]^2)`. This
/// is the normalization constant ("U") that Welch's method divides the
/// periodogram by so that differently-shaped windows report the same total
/// power for the same signal (docs/QUALITY.md §2 Welch "Window
/// normalization").
pub fn mean_square(window: Window, len: usize) -> f64 {
    if len == 0 {
        return 0.0;
    }
    coefficients(window, len).iter().map(|w| w * w).sum::<f64>() / len as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangular_is_all_ones_with_unit_mean_square() {
        let coeffs = coefficients(Window::Rectangular, 8);
        assert!(coeffs.iter().all(|&w| w == 1.0));
        assert_eq!(mean_square(Window::Rectangular, 8), 1.0);
    }

    #[test]
    fn hann_and_hamming_are_symmetric_and_bounded() {
        for window in [Window::Hann, Window::Hamming] {
            let coeffs = coefficients(window, 9);
            for (front, back) in coeffs.iter().zip(coeffs.iter().rev()) {
                assert!(
                    (front - back).abs() < 1e-12,
                    "{window:?} must be symmetric, got {coeffs:?}"
                );
            }
            for &w in &coeffs {
                assert!(
                    (0.0..=1.0).contains(&w),
                    "{window:?} coefficient {w} out of [0, 1]"
                );
            }
        }
    }

    #[test]
    fn hann_endpoints_are_zero_and_hamming_endpoints_are_not() {
        let hann = coefficients(Window::Hann, 8);
        assert!(hann.first().unwrap().abs() < 1e-12);
        assert!(hann.last().unwrap().abs() < 1e-12);

        let hamming = coefficients(Window::Hamming, 8);
        // Hamming's endpoints are 0.54 - 0.46 = 0.08, never fully tapered to
        // zero — that is the whole point of the extra 0.08 relative to Hann.
        assert!((hamming.first().unwrap() - 0.08).abs() < 1e-9);
        assert!((hamming.last().unwrap() - 0.08).abs() < 1e-9);
    }

    #[test]
    fn mean_square_matches_direct_computation_from_coefficients() {
        for window in [Window::Rectangular, Window::Hann, Window::Hamming] {
            for len in [1, 2, 16, 1024] {
                let expected =
                    coefficients(window, len).iter().map(|w| w * w).sum::<f64>() / len as f64;
                assert_eq!(mean_square(window, len), expected);
            }
        }
    }

    #[test]
    fn degenerate_lengths_never_panic() {
        for window in [Window::Rectangular, Window::Hann, Window::Hamming] {
            assert_eq!(coefficients(window, 0).len(), 0);
            assert_eq!(coefficients(window, 1), vec![1.0]);
            assert_eq!(mean_square(window, 0), 0.0);
        }
    }
}
