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

//! Opens the synthetic fixture headlessly and asserts peak RSS stays under
//! the cap in docs/SPEC.md §5. The anti-freeze/anti-crash gate; exits
//! non-zero on breach (docs/QUALITY.md §3: "Breaching = build failure").
//!
//! The true 20 GB fixture QUALITY §3 describes does not fit a GitHub-hosted
//! runner's 14 GB of SSD, so CI runs this against a smaller fixture sized by
//! `--size-gb` (issue #61 decision 1); the 20 GB run is part of the
//! maintainer's manual QA ritual (QUALITY §5). The budget itself
//! (`min(25% RAM, 4 GB)`) does not scale with file size — it exists to catch
//! "the whole file was materialized in RAM" bugs, which blow the cap at a
//! few hundred MB already (issue #58), so a smaller fixture still exercises
//! the invariant that matters.

use anyhow::{Context, Result};
use clap::Parser;
use glyde_core::budget::RamBudget;
use glyde_core::ingest;
use glyde_devtools::{fixture_path, write_csv_fixture, PeakRssSampler};
use std::time::Duration;
use tracing::{info, warn};

/// Opens a synthetic fixture and asserts peak RSS stays under the SPEC §5
/// budget (docs/QUALITY.md §3).
#[derive(Parser)]
struct Args {
    /// Fixture size in GB — must match the `--size-gb` `generate_fixtures`
    /// was run with, so both agree on the fixture path.
    #[arg(long)]
    size_gb: f64,

    /// Deterministic seed, only used if the fixture must be generated
    /// on-demand (it isn't found at the expected path).
    #[arg(long, default_value_t = 0xF1CDA7A)]
    seed: u64,

    /// How often to sample this process's RSS while the fixture is open.
    #[arg(long, default_value = "20")]
    sample_interval_ms: u64,

    /// Override the RAM budget ingestion *plans* against, in GB (issue #75),
    /// so a fixture that would comfortably fit in memory can be measured
    /// through the spill path instead. Without it, the machine's real
    /// `min(25% RAM, 4 GB)` cap decides, which is what the CI gate measures.
    ///
    /// This can only ever *lower* the cap: SPEC §5's 4 GB ceiling is part of
    /// the budget formula, so no value here makes a file above it affordable
    /// in memory. Measuring the in-memory path on a large fixture therefore
    /// means running this harness from a checkout that predates the spill
    /// path, not passing a large number here.
    #[arg(long)]
    budget_gb: Option<f64>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    anyhow::ensure!(args.size_gb > 0.0, "--size-gb must be positive");

    let path = fixture_path(args.size_gb);
    if !path.exists() {
        warn!(path = %path.display(), "fixture not found, generating it on demand");
        let target_bytes = (args.size_gb * 1024.0 * 1024.0 * 1024.0) as u64;
        write_csv_fixture(&path, target_bytes, args.seed)
            .with_context(|| format!("generating fixture at {}", path.display()))?;
    }

    let file_bytes = std::fs::metadata(&path)
        .with_context(|| format!("reading fixture metadata for {}", path.display()))?
        .len();

    // The cap the gate is asserted against is always the real SPEC §5 one;
    // `--budget-gb` only changes the budget ingestion *plans* against, so a
    // forced in-memory run is still measured against the same ceiling.
    let budget = RamBudget::from_system();
    let planning_budget = args.budget_gb.map(|gb| {
        RamBudget::from_total_ram_bytes(((gb * 1024.0 * 1024.0 * 1024.0) as u64).saturating_mul(4))
    });
    info!(
        path = %path.display(),
        file_bytes,
        cap_bytes = budget.cap_bytes(),
        planning_cap_bytes = planning_budget.map(|b| b.cap_bytes()),
        "memory_gate: opening fixture"
    );

    let spill_dir = std::env::temp_dir().join("glyde-memory-gate-spill");
    let sampler = PeakRssSampler::start(Duration::from_millis(args.sample_interval_ms));
    let open_result = match planning_budget {
        Some(planning_budget) => {
            ingest::open_dataset_with_budget(&path, planning_budget, &spill_dir)
        }
        None => ingest::open_dataset(&path),
    };
    let peak_bytes = sampler.stop();

    // Propagate an open failure after the sampler is stopped, not before —
    // the peak RSS up to the point of failure is still useful in the error.
    let (_summary, _report, dataset) =
        open_result.with_context(|| format!("opening fixture {}", path.display()))?;
    let row_count = dataset.time.len();
    let storage = if dataset.is_spilled() {
        "spilled"
    } else {
        "in-memory"
    };
    drop(dataset);

    let cap_bytes = budget.cap_bytes();
    let ratio = peak_bytes as f64 / file_bytes.max(1) as f64;

    println!(
        "memory_gate: {row_count} rows, file {file_bytes} bytes, storage {storage}, \
         peak RSS {peak_bytes} bytes ({ratio:.2}x file size), cap {cap_bytes} bytes"
    );

    anyhow::ensure!(
        peak_bytes <= cap_bytes,
        "memory_gate: peak RSS {peak_bytes} bytes exceeded the {cap_bytes} byte budget \
         (SPEC §5 \"Peak RSS\": min(25% RAM, 4 GB)) while opening a {file_bytes} byte fixture \
         — this is the anti-freeze/anti-crash gate, breaching it fails the build"
    );

    info!(peak_bytes, cap_bytes, "memory_gate: within budget");
    Ok(())
}
