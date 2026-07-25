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

//! Generates the multi-GB synthetic fixture used by the `memory_gate` CI job
//! (docs/QUALITY.md §3). Output is never committed — see `.gitignore` and
//! `testdata/generated/`.

use anyhow::{Context, Result};
use clap::Parser;
use glyde_devtools::{fixture_path, write_csv_fixture};
use tracing::info;

/// Generates a synthetic CSV fixture at a given size for the CI performance
/// gates (docs/QUALITY.md §3).
#[derive(Parser)]
struct Args {
    /// Target fixture size in GB (decimal, e.g. `8` or `0.5`).
    #[arg(long)]
    size_gb: f64,

    /// Deterministic seed for the pseudo-random column values, so repeated
    /// runs of the same size produce byte-identical fixtures.
    #[arg(long, default_value_t = 0xF1CDA7A)]
    seed: u64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    anyhow::ensure!(args.size_gb > 0.0, "--size-gb must be positive");

    let target_bytes = (args.size_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let path = fixture_path(args.size_gb);

    info!(
        size_gb = args.size_gb,
        target_bytes,
        path = %path.display(),
        "generating synthetic CSV fixture"
    );

    let row_count = write_csv_fixture(&path, target_bytes, args.seed)
        .with_context(|| format!("writing fixture to {}", path.display()))?;

    let actual_bytes = std::fs::metadata(&path)
        .with_context(|| format!("reading back fixture metadata for {}", path.display()))?
        .len();

    info!(
        row_count,
        actual_bytes,
        path = %path.display(),
        "generate_fixtures: done"
    );
    println!(
        "generate_fixtures: wrote {row_count} rows ({actual_bytes} bytes) to {}",
        path.display()
    );

    Ok(())
}
