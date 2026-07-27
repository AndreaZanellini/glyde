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

//! Benchmark: the time-domain view's real per-frame render path (SPEC §5
//! "Pan/zoom frame time: p50 ≤16ms, p99 ≤100ms", docs/ROADMAP.md M3, issue
//! #80). `views::time::show` is what the UI thread calls every frame while
//! panning/zooming; `dsp::decimation::decimate_viewport` is already
//! ceiling-gated on its own in `glyde-core/benches/viewport_query.rs`, but a
//! green gate there says nothing about the cost of the surrounding
//! `egui_plot` layout and draw calls, which this benchmark measures
//! headlessly via the same `egui::Context::run` technique
//! `views::time::render_tests` uses (no GPU or display server needed).
//!
//! `pyramids` and `ticks` are built once, outside the timed render loop —
//! exactly like `crate::app::PartialLoad` caches them once per status
//! change rather than once per frame — since the first version of this
//! benchmark caught `views::time::show` itself doing that per-frame
//! unconditionally: 4.75s for one frame at 8M samples, ~40x the ceiling.
//!
//! Per issue #61 decision 2 (mirrored from `glyde-core`'s benches), only the
//! absolute SPEC §5 ceiling is build-blocking here; the >15% regression
//! comparison is a manual check on the SPEC §5 reference machine using
//! criterion's own local baseline comparison.

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use egui::{Pos2, Rect, Vec2};
use glyde_app::views;
use glyde_core::dsp::decimation::Bucket;
use glyde_core::ingest::{pyramids_for_dataset, Dataset, TimeAxis};
use glyde_core::series::{Series, SeriesValues};
use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};

const SAMPLE_COUNT: usize = 8_000_000;

/// A typical desktop viewport, not the headless test harness's oversized
/// 10,000 x 10,000 fallback (`egui`'s `InputState` default screen size) —
/// pixel width directly bounds `decimate_viewport`'s per-frame cost (SPEC
/// §3.1), so an unrealistically wide viewport would understate it.
const VIEWPORT_SIZE: Vec2 = Vec2::new(1920.0, 1080.0);

/// SPEC §5 "Pan/zoom frame time": p99 ≤100ms.
const FRAME_TIME_CEILING: Duration = Duration::from_millis(100);

struct Fixture {
    dataset: Dataset,
    pyramids: Vec<Option<Vec<Vec<Bucket>>>>,
    ticks: Vec<i128>,
}

fn synthetic_fixture(sample_count: usize) -> Fixture {
    let timestamps: Vec<Timestamp> = (0..sample_count)
        .map(|i| Timestamp::new(i as i128, TimeUnit::Seconds))
        .collect();
    let values: Vec<f64> = (0..sample_count)
        .map(|i| (i as f64 * 0.001).sin())
        .collect();
    let dataset = Dataset {
        time: TimeAxis::Absolute {
            timestamps: timestamps.into(),
            format: TimestampFormat::EpochSeconds,
        },
        time_column_name: "timestamp".to_string(),
        columns: vec![Series::new("value", SeriesValues::F64(values))],
    };
    let pyramids = pyramids_for_dataset(&dataset);
    let ticks = dataset.time.to_pyramid_ticks().into_owned();
    Fixture {
        dataset,
        pyramids,
        ticks,
    }
}

/// One frame, on a persistent `Context` reused across calls exactly like
/// `eframe` drives the real app across repaints.
fn render_frame(ctx: &egui::Context, fixture: &Fixture) {
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, VIEWPORT_SIZE)),
        ..Default::default()
    };
    let _ = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            views::time::show(ui, &fixture.dataset, &fixture.pyramids, &fixture.ticks);
        });
    });
}

fn bench_time_view_render(c: &mut Criterion) {
    let fixture = synthetic_fixture(SAMPLE_COUNT);
    let ctx = egui::Context::default();

    // Warm up egui's own font/layout caches so the ceiling check below
    // measures steady-state per-frame cost, not one-time first-frame setup.
    render_frame(&ctx, &fixture);

    let start = Instant::now();
    render_frame(&ctx, &fixture);
    let elapsed = start.elapsed();
    assert!(
        elapsed <= FRAME_TIME_CEILING,
        "time-domain view render took {elapsed:?} for {SAMPLE_COUNT} samples at a \
         {VIEWPORT_SIZE:?} viewport, exceeding the {FRAME_TIME_CEILING:?} build-blocking \
         ceiling (SPEC §5: pan/zoom p99 ≤100ms)"
    );

    c.bench_function("time_view_render", |b| {
        b.iter(|| render_frame(std::hint::black_box(&ctx), std::hint::black_box(&fixture)))
    });
}

criterion_group!(benches, bench_time_view_render);
criterion_main!(benches);
