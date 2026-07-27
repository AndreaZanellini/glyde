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

//! Non-monotonic and duplicate timestamp detection over a timestamp series
//! (docs/SPEC.md §2.1, docs/ROADMAP.md M2).

use std::cmp::Ordering;
use tracing::warn;

use super::gap::SLICE_SCAN_IS_INFALLIBLE;
use super::ticks::{for_each_delta, TickSource};
use crate::Result;

/// SPEC §2.1: how many consecutive samples ran backwards, and how many were
/// an exact repeat of the sample before them. Counting only — the input
/// order itself is never touched by this or any caller: "rows out of order
/// are not reordered silently" and duplicate timestamps are "preserved,
/// flagged", never collapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MonotonicityReport {
    /// Number of consecutive pairs where `timestamps[i] < timestamps[i - 1]`.
    pub non_monotonic_count: usize,
    /// Number of consecutive pairs where `timestamps[i] == timestamps[i - 1]`.
    pub duplicate_count: usize,
}

impl MonotonicityReport {
    /// Accounts one consecutive Δt: a negative one is a backward step, a zero
    /// one an exact repeat (SPEC §2.1). The single definition of both counts —
    /// [`super::summarize_ticks`] feeds this from the same Δt pass it uses for
    /// gaps, so the two can never disagree about a series.
    pub(super) fn observe_delta(&mut self, delta: i128) {
        match delta.cmp(&0) {
            Ordering::Less => self.non_monotonic_count += 1,
            Ordering::Equal => self.duplicate_count += 1,
            Ordering::Greater => {}
        }
    }

    /// Logs a `warn!` for each anomaly actually counted, matching the
    /// ingestion-decision logging convention used elsewhere in this crate
    /// (e.g. `ingest::infer`'s NaN-run flagging). SPEC §2.1's "detected,
    /// counted, **logged**" — split from the counting so a caller that
    /// accumulated the counts itself still reports them exactly once.
    pub(super) fn log_anomalies(&self) {
        if self.non_monotonic_count > 0 {
            warn!(
                non_monotonic_count = self.non_monotonic_count,
                "timestamps not monotonic — rows out of order are preserved, not reordered (SPEC §2.1)"
            );
        }
        if self.duplicate_count > 0 {
            warn!(
                duplicate_count = self.duplicate_count,
                "duplicate timestamps detected and preserved (SPEC §2.1)"
            );
        }
    }
}

/// Scans `timestamps` in their original, unmodified order (non-decreasing or
/// not — this makes no assumption either way, unlike [`super::detect_gaps`])
/// and counts every backward step and every exact repeat (SPEC §2.1). Never
/// reorders or deduplicates `timestamps`.
///
/// A caller that also needs SPEC §2.2's gap and sampling verdicts should use
/// [`super::summarize_ticks`], which derives all three from one pass.
pub fn detect_monotonicity(timestamps: &[i128]) -> MonotonicityReport {
    detect_monotonicity_from(timestamps).expect(SLICE_SCAN_IS_INFALLIBLE)
}

/// [`detect_monotonicity`] over any [`TickSource`] — the bounded-memory form,
/// for a time axis too large to hold as one slice (issue #85).
fn detect_monotonicity_from<S: TickSource + ?Sized>(source: &S) -> Result<MonotonicityReport> {
    let mut report = MonotonicityReport::default();
    for_each_delta(source, 0..source.tick_count(), &mut |delta| {
        report.observe_delta(delta);
        Ok(())
    })?;
    report.log_anomalies();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::corpus_fixture::corpus_ticks;

    #[test]
    fn fewer_than_two_samples_has_nothing_to_compare() {
        assert_eq!(detect_monotonicity(&[]), MonotonicityReport::default());
        assert_eq!(detect_monotonicity(&[42]), MonotonicityReport::default());
    }

    #[test]
    fn strictly_increasing_timestamps_report_no_anomalies() {
        assert_eq!(
            detect_monotonicity(&[0, 1, 2, 3]),
            MonotonicityReport::default()
        );
    }

    // Corpus case 36 (docs/QUALITY.md §1.36): six otherwise-uniform 1s ticks
    // with one inversion (index 3 drops from :02 back to :01, then resumes
    // forward from there) — exactly one backward step, no duplicates.
    #[test]
    fn corpus_case_36_non_monotonic_timestamps_reports_one_backward_step() {
        let ticks = corpus_ticks("case-36-non-monotonic-timestamps.csv", "timestamp");

        assert_eq!(
            detect_monotonicity(&ticks),
            MonotonicityReport {
                non_monotonic_count: 1,
                duplicate_count: 0,
            }
        );
    }

    // Corpus case 37 (docs/QUALITY.md §1.37): six otherwise-uniform 1s ticks
    // with one exact repeat (index 2 and 3 are both :02) — one duplicate, no
    // backward step (a repeat is not itself a decrease).
    #[test]
    fn corpus_case_37_duplicate_timestamps_reports_one_duplicate() {
        let ticks = corpus_ticks("case-37-duplicate-timestamps.csv", "timestamp");

        assert_eq!(
            detect_monotonicity(&ticks),
            MonotonicityReport {
                non_monotonic_count: 0,
                duplicate_count: 1,
            }
        );
    }
}
