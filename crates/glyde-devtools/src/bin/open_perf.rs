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

//! Headless "open a file, then navigate it" timing harness (issue #114).
//!
//! Replays, phase by phase, exactly what `glyde-app`'s indexer thread does
//! when the user opens a file (`plumbing::run_index_job`):
//!
//! 1. `ingest::open_dataset_progressive` — time to the first progress
//!    checkpoint (the first plot the user sees) and to completion.
//! 2. `ingest::derived_caches_for_dataset_cached_with_cache_dir` against an
//!    empty cache directory (first open: pyramid + Level-0 cache built and
//!    written), then again against the now-warm directory (reopen).
//!
//! It then replays a scripted pan/zoom session against the completed
//! pyramids — the per-frame `decimate_viewport` call the time-domain view
//! makes for every numeric column — and reports per-frame p50/p99.
//!
//! Prints one machine-readable `RESULT key=value` line per metric so two runs
//! (e.g. `main` versus a branch) can be diffed directly. Not a CI gate: the
//! criterion benches are. This exists so a performance claim in a PR comes
//! with numbers anyone can reproduce on their own machine.

use anyhow::{Context, Result};
use clap::Parser;
use glyde_core::budget::RamBudget;
use glyde_core::dsp::decimation::decimate_viewport;
use glyde_core::ingest;
use glyde_devtools::{write_csv_fixture, PeakRssSampler};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
struct Args {
    /// Fixture size in GB. Generated once at `--fixture` and reused.
    #[arg(long, default_value_t = 1.0)]
    size_gb: f64,

    /// Where the fixture lives (generated on demand if missing).
    #[arg(long)]
    fixture: Option<PathBuf>,

    /// Deterministic seed for fixture generation.
    #[arg(long, default_value_t = 0xF1CDA7A)]
    seed: u64,

    /// Viewport width in pixel columns for the navigation replay.
    #[arg(long, default_value_t = 1600)]
    pixel_columns: usize,

    /// Number of frames in the scripted pan/zoom session.
    #[arg(long, default_value_t = 600)]
    frames: usize,

    /// Plan against this RAM cap (GB) instead of the machine's own, to force
    /// the spilled storage path (issue #75). The spilled path builds no
    /// derived caches (issue #102), so only the open phase is measured.
    #[arg(long)]
    budget_gb: Option<f64>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.size_gb > 0.0, "--size-gb must be positive");

    let fixture = args.fixture.clone().unwrap_or_else(|| {
        std::env::temp_dir()
            .join("glyde-open-perf")
            .join(format!("open_perf_{}gb.csv", args.size_gb))
    });
    if !fixture.exists() {
        let target_bytes = (args.size_gb * 1024.0 * 1024.0 * 1024.0) as u64;
        eprintln!("generating {} ...", fixture.display());
        write_csv_fixture(&fixture, target_bytes, args.seed)
            .with_context(|| format!("writing fixture {}", fixture.display()))?;
    }
    let file_bytes = std::fs::metadata(&fixture)?.len();
    println!("RESULT fixture_bytes={file_bytes}");

    let rss = PeakRssSampler::start(Duration::from_millis(10));

    if let Some(budget_gb) = args.budget_gb {
        // RamBudget's cap is min(25% of total RAM, 4 GB), so a "total RAM"
        // of 4x the requested cap yields exactly that cap.
        let budget =
            RamBudget::from_total_ram_bytes((budget_gb * 4.0 * 1024.0 * 1024.0 * 1024.0) as u64);
        let cache_dir = tempfile_dir()?;
        let start = Instant::now();
        let mut first_checkpoint: Option<Duration> = None;
        let dataset = ingest::load_progressive_with_budget(&fixture, budget, &cache_dir, |_| {
            first_checkpoint.get_or_insert_with(|| start.elapsed());
        })
        .context("opening the fixture on the spilled path")?;
        let open_elapsed = start.elapsed();
        println!("RESULT spilled={}", dataset.is_spilled());
        println!("RESULT rows={}", dataset.time.len());
        println!(
            "RESULT first_plot_ms={:.1}",
            first_checkpoint.unwrap_or(open_elapsed).as_secs_f64() * 1e3
        );
        println!("RESULT open_ms={:.1}", open_elapsed.as_secs_f64() * 1e3);
        println!(
            "RESULT open_mb_per_s={:.1}",
            file_bytes as f64 / 1e6 / open_elapsed.as_secs_f64()
        );
        drop(dataset);
        println!("RESULT peak_rss_mb={:.1}", rss.stop() as f64 / 1e6);
        let _ = std::fs::remove_dir_all(&cache_dir);
        return Ok(());
    }

    // Phase 1: the progressive open (first plot + completion).
    let start = Instant::now();
    let mut first_checkpoint: Option<Duration> = None;
    let mut checkpoints = 0usize;
    let (summary, _report, dataset) = ingest::open_dataset_progressive(&fixture, |checkpoint| {
        checkpoints += 1;
        first_checkpoint.get_or_insert_with(|| start.elapsed());
        drop(checkpoint);
    })
    .context("opening the fixture")?;
    let open_elapsed = start.elapsed();
    println!("RESULT rows={}", summary.row_count);
    println!("RESULT checkpoints={checkpoints}");
    println!(
        "RESULT first_plot_ms={:.1}",
        first_checkpoint.unwrap_or(open_elapsed).as_secs_f64() * 1e3
    );
    println!("RESULT open_ms={:.1}", open_elapsed.as_secs_f64() * 1e3);
    println!(
        "RESULT open_mb_per_s={:.1}",
        file_bytes as f64 / 1e6 / open_elapsed.as_secs_f64()
    );

    // Phase 2: derived caches, cold then warm.
    let cache_dir = tempfile_dir()?;
    let start = Instant::now();
    let (pyramids, level0) = ingest::derived_caches_for_dataset_cached_with_cache_dir(
        &fixture,
        &dataset,
        &cache_dir,
        Default::default(),
    );
    let cold = start.elapsed();
    drop((pyramids, level0));
    let start = Instant::now();
    let (pyramids, level0) = ingest::derived_caches_for_dataset_cached_with_cache_dir(
        &fixture,
        &dataset,
        &cache_dir,
        Default::default(),
    );
    let warm = start.elapsed();
    println!("RESULT derived_cold_ms={:.1}", cold.as_secs_f64() * 1e3);
    println!("RESULT derived_warm_ms={:.1}", warm.as_secs_f64() * 1e3);
    println!(
        "RESULT time_to_interactive_ms={:.1}",
        (open_elapsed + cold).as_secs_f64() * 1e3
    );

    // Phase 3: scripted navigation. Zoom from the full range down to a few
    // hundred samples, panning as it goes, then back out.
    let ticks = dataset.time.to_pyramid_ticks();
    let (Some(&first), Some(&last)) = (ticks.first(), ticks.last()) else {
        anyhow::bail!("fixture has no rows");
    };
    // Each frame decimates every column, once one after another (the
    // per-query cost) and once in parallel on the rayon pool, the way
    // `glyde-app`'s time-domain view does since issue #114.
    let full = (last - first).max(1);
    let range_for = |frame: usize| {
        let phase = frame as f64 / args.frames as f64;
        // Triangle wave: zoom in for the first half, out for the second.
        let depth = if phase < 0.5 {
            phase * 2.0
        } else {
            (1.0 - phase) * 2.0
        };
        let span = ((full as f64) * (1e-6f64).powf(depth)).max(64.0) as i128;
        let center = first + ((full - span) as f64 * (0.5 + 0.45 * (phase * 37.0).sin())) as i128;
        (center, center + span)
    };
    let columns: Vec<(&[Vec<_>], &[f64])> = pyramids
        .iter()
        .enumerate()
        .filter_map(|(index, pyramid)| {
            let pyramid = pyramid.as_ref()?;
            let samples = match level0[index].as_ref() {
                Some(cache) => cache.samples(),
                None => dataset.columns[index].values().as_f64_slice()?,
            };
            Some((pyramid.as_slice(), samples))
        })
        .collect();
    for parallel in [false, true] {
        let mut frame_times = Vec::with_capacity(args.frames);
        for frame in 0..args.frames {
            let range = range_for(frame);
            let query = |&(pyramid, samples): &(&[Vec<_>], &[f64])| {
                decimate_viewport(pyramid, samples, &ticks, range, args.pixel_columns)
            };
            let start = Instant::now();
            if parallel {
                use rayon::prelude::*;
                std::hint::black_box(columns.par_iter().map(query).collect::<Vec<_>>());
            } else {
                std::hint::black_box(columns.iter().map(query).collect::<Vec<_>>());
            }
            frame_times.push(start.elapsed());
        }
        frame_times.sort();
        let pct = |p: f64| frame_times[((frame_times.len() - 1) as f64 * p) as usize];
        let label = if parallel {
            "nav_parallel"
        } else {
            "nav_frame"
        };
        println!("RESULT {label}_p50_us={:.1}", pct(0.50).as_secs_f64() * 1e6);
        println!("RESULT {label}_p99_us={:.1}", pct(0.99).as_secs_f64() * 1e6);
    }

    println!("RESULT peak_rss_mb={:.1}", rss.stop() as f64 / 1e6);
    let _ = std::fs::remove_dir_all(&cache_dir);
    Ok(())
}

/// A fresh, empty cache directory under the OS temp dir, so the "cold"
/// measurement never hits a cache left over from an earlier run.
fn tempfile_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "glyde-open-perf-cache-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
