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

//! RAM budget and affordability checks (docs/SPEC.md §5.1, docs/ARCHITECTURE.md
//! §budget, docs/ROADMAP.md M3).
//!
//! Glyde computes its memory cap from system RAM once at startup and checks
//! every prospective allocation against it **before** acting, never after —
//! a freeze or crash on a large file is "the single most serious class of
//! bug in this product" (SPEC §5.1). This module only answers "how much may
//! I use, and does this fit" — callers (the index build, PSD) are
//! responsible for actually refusing/offering an alternative when a check
//! fails (SPEC §5.1's "clear explanation and affordable alternative").

use tracing::{info, warn};

use crate::{GlydeError, Result};

/// Hard ceiling on the budget regardless of installed RAM (SPEC §5 "Peak
/// RSS": `min(25% of system RAM, 4 GB)`).
const MAX_CAP_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The fraction of system RAM Glyde is allowed to use, before the
/// [`MAX_CAP_BYTES`] ceiling is applied.
const RAM_FRACTION: u64 = 4; // 1/4 = 25%

/// The memory cap Glyde must stay under, computed once from system RAM
/// (SPEC §5.1 "Glyde knows system RAM at startup and computes its budget").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamBudget {
    cap_bytes: u64,
}

impl RamBudget {
    /// Computes the budget from the machine's actual total RAM, as read by
    /// `sysinfo` (docs/ARCHITECTURE.md §System info).
    pub fn from_system() -> Self {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        Self::from_total_ram_bytes(system.total_memory())
    }

    /// The pure math behind the budget: `min(25% of total_ram_bytes, 4 GB)`
    /// (SPEC §5 "Peak RSS"). Exposed separately from [`Self::from_system`]
    /// so the formula is testable without depending on the host machine's
    /// actual RAM.
    pub fn from_total_ram_bytes(total_ram_bytes: u64) -> Self {
        let cap_bytes = (total_ram_bytes / RAM_FRACTION).min(MAX_CAP_BYTES);
        info!(
            total_ram_bytes,
            cap_bytes, "computed RAM budget: min(25% of system RAM, 4 GB)"
        );
        Self { cap_bytes }
    }

    /// The computed cap, in bytes.
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    /// Whether `requested_bytes` fits under the budget, as a plain
    /// predicate. [`Self::check_affordable`] is the form for a caller that
    /// must *refuse* an action it cannot afford; this one is for a caller
    /// that has an affordable alternative to fall back to instead (SPEC
    /// §5.1: "refuses it with a clear explanation **and offers the
    /// affordable alternative**") — `ingest::dataset` uses it to pick
    /// between materializing a file in memory and spilling it to disk.
    pub fn affords(&self, requested_bytes: u64) -> bool {
        requested_bytes <= self.cap_bytes
    }

    /// Checks whether `requested_bytes` fits under the budget. Callers must
    /// call this **before** performing the allocation/read it describes
    /// (SPEC §5.1 "checks affordability before acting, never after") and,
    /// on [`GlydeError::BudgetExceeded`], refuse the action with a clear
    /// explanation rather than attempting it anyway.
    pub fn check_affordable(&self, requested_bytes: u64) -> Result<()> {
        if requested_bytes <= self.cap_bytes {
            return Ok(());
        }
        warn!(
            requested_bytes,
            cap_bytes = self.cap_bytes,
            "refusing action: exceeds RAM budget"
        );
        Err(GlydeError::BudgetExceeded {
            requested_bytes,
            cap_bytes: self.cap_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn budget_is_25_percent_of_ram_when_that_is_below_the_4gb_ceiling() {
        let budget = RamBudget::from_total_ram_bytes(8 * GB);
        assert_eq!(budget.cap_bytes(), 2 * GB);
    }

    #[test]
    fn budget_is_capped_at_4gb_when_25_percent_of_ram_would_exceed_it() {
        let budget = RamBudget::from_total_ram_bytes(64 * GB);
        assert_eq!(budget.cap_bytes(), 4 * GB);
    }

    #[test]
    fn budget_equals_the_4gb_ceiling_exactly_at_16gb_ram() {
        let budget = RamBudget::from_total_ram_bytes(16 * GB);
        assert_eq!(budget.cap_bytes(), 4 * GB);
    }

    #[test]
    fn budget_is_zero_on_a_machine_reporting_zero_ram() {
        let budget = RamBudget::from_total_ram_bytes(0);
        assert_eq!(budget.cap_bytes(), 0);
    }

    #[test]
    fn a_request_at_or_under_the_cap_is_affordable() {
        let budget = RamBudget::from_total_ram_bytes(8 * GB);
        assert!(budget.check_affordable(2 * GB).is_ok());
        assert!(budget.check_affordable(2 * GB - 1).is_ok());
    }

    #[test]
    fn a_request_over_the_cap_is_refused_with_both_numbers_reported() {
        let budget = RamBudget::from_total_ram_bytes(8 * GB);
        let err = budget
            .check_affordable(2 * GB + 1)
            .expect_err("a request over the cap must be refused");

        let GlydeError::BudgetExceeded {
            requested_bytes,
            cap_bytes,
        } = err
        else {
            panic!("expected GlydeError::BudgetExceeded, got {err:?}");
        };
        assert_eq!(requested_bytes, 2 * GB + 1);
        assert_eq!(cap_bytes, 2 * GB);
    }

    #[test]
    fn from_system_reflects_the_actual_host_ram() {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let expected = RamBudget::from_total_ram_bytes(system.total_memory());

        assert_eq!(RamBudget::from_system(), expected);
    }
}
