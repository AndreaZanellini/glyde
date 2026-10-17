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
//! `pyramids`, `ticks`, and `sample_cache` are all built once, outside the
//! timed render loop — exactly like `crate::app::PartialLoad` caches them
//! once per status change rather than once per frame. Both `ticks` and
//! `sample_cache` earned that treatment the hard way: the first version of
//! this benchmark caught `views::time::show` recomputing `ticks` per frame
//! (4.75s for one frame at 8M `f64` samples, ~47x the ceiling); a PR review
//! then caught that the fixture's `f64` column never exercised
//! `sample_cache`'s conversion path at all, so **two** dtypes are benched
//! here — `f64` (zero-copy) and `i64` (the converted path, and the one SPEC
//! §1.4 dtype whose conversion also runs a per-element precision check) —
//! so a regression in either path fails the build.
//!
//! Every frame shows an explicit horizontal span (issue #107): the **full
//! view**, every sample on screen (the worst case for decimation), and a
//! **pan**, a window a tenth of the file wide that moves on every frame.
//! Earlier versions left the plot at `egui_plot`'s default bounds, about one
//! second wide, so each frame decimated only a handful of samples and every
//! fixture measured ~35 µs whatever its size. Each measured frame now also
//! asserts the tick range it really showed.
//!
//! A third fixture is a **spilled** file (issue #107): a file too large for
//! the RAM budget renders through a different path — its pyramid lives on
//! disk, one memory-mapped file per level (issue #102), prepared here
//! exactly as `plumbing::run_index_job` prepares a completed spilled load.
//! SPEC §5's frame budget applies to it just the same; this fixture is the
//! regression gate for that path.
//!
//! A fourth fixture is a spilled file with **many channels**
//! ([`MULTI_CHANNELS`]), rendered twice: every channel shown, and only one
//! shown through the channel picker (`views::channels`). A hidden channel
//! must cost nothing — the picker's reason to exist on a spilled file — so
//! the one-shown variant is the gate for that. A last case times a **click
//! in the picker** with every channel shown and the view unchanged: the
//! other channels' decimations are reused (issue #135), so it decimates one
//! channel, not all of them (drawing them all remains, issue #137).
//!
//! The view keeps each shown channel's decimation while the view does not
//! change (`views::time::TimeViewState`), so every frame meant to time a
//! decimation first drops that cache — otherwise a repeated full-view frame
//! would time a cache hit, not the worst case this bench gates.
//!
//! Per issue #61 decision 2 (mirrored from `glyde-core`'s benches), only the
//! absolute SPEC §5 ceiling is build-blocking here; the >15% regression
//! comparison is a manual check on the SPEC §5 reference machine using
//! criterion's own local baseline comparison.

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use egui::{Pos2, Rect, Vec2};
use glyde_app::views;
use glyde_app::views::channels::ChannelVisibility;
use glyde_app::views::time::TimeViewState;
use glyde_core::index::disk_pyramid::ColumnPyramid;
use glyde_core::ingest::{pyramids_for_dataset, Dataset, TimeAxis};
use glyde_core::series::{Series, SeriesValues};
use glyde_core::time::{TimeUnit, Timestamp, TimestampFormat};

const SAMPLE_COUNT: usize = 8_000_000;

/// The channel count of the many-channel spilled fixture.
const MULTI_CHANNELS: usize = 32;

/// Samples per channel in the many-channel spilled fixture: the same total
/// sample count as the single-channel fixtures, spread across its channels.
const MULTI_CHANNEL_SAMPLES: usize = SAMPLE_COUNT / MULTI_CHANNELS;

/// A typical desktop viewport, not the headless test harness's oversized
/// 10,000 x 10,000 fallback (`egui`'s `InputState` default screen size) —
/// pixel width directly bounds `decimate_viewport`'s per-frame cost (SPEC
/// §3.1), so an unrealistically wide viewport would understate it.
const VIEWPORT_SIZE: Vec2 = Vec2::new(1920.0, 1080.0);

/// SPEC §5 "Pan/zoom frame time": p99 ≤100ms.
const FRAME_TIME_CEILING: Duration = Duration::from_millis(100);

struct Fixture {
    label: &'static str,
    /// Keeps a spilled fixture's source CSV alive (its spill files live as
    /// long as `dataset`).
    _dir: Option<tempfile::TempDir>,
    dataset: Dataset,
    pyramids: Vec<Option<ColumnPyramid>>,
    ticks: Vec<i128>,
    sample_cache: Vec<Option<Vec<f64>>>,
    cursor: views::time::CursorLookup,
}

fn dataset_with_column(sample_count: usize, values: SeriesValues) -> Dataset {
    let timestamps: Vec<Timestamp> = (0..sample_count)
        .map(|i| Timestamp::new(i as i128, TimeUnit::Seconds))
        .collect();
    Dataset {
        time: TimeAxis::Absolute {
            timestamps: timestamps.into(),
            format: TimestampFormat::EpochSeconds,
        },
        time_column_name: "timestamp".to_string(),
        columns: vec![Series::new("value", values)],
    }
}

fn synthetic_fixture(label: &'static str, sample_count: usize, values: SeriesValues) -> Fixture {
    let dataset = dataset_with_column(sample_count, values);
    let pyramids = pyramids_for_dataset(&dataset)
        .into_iter()
        .map(|pyramid| pyramid.map(ColumnPyramid::Memory))
        .collect();
    let ticks = dataset.time.to_pyramid_ticks().into_owned();
    let sample_cache = views::time::cache_column_samples(&dataset);
    let cursor = views::time::CursorLookup::for_time(&dataset.time);
    Fixture {
        label,
        _dir: None,
        dataset,
        pyramids,
        ticks,
        sample_cache,
        cursor,
    }
}

fn f64_fixture(sample_count: usize) -> Fixture {
    let values = (0..sample_count)
        .map(|i| (i as f64 * 0.001).sin())
        .collect();
    synthetic_fixture("f64 (zero-copy)", sample_count, SeriesValues::F64(values))
}

fn i64_fixture(sample_count: usize) -> Fixture {
    let values = (0..sample_count as i64).collect();
    synthetic_fixture(
        "i64 (cached conversion)",
        sample_count,
        SeriesValues::I64(values),
    )
}

/// The same shape as [`f64_fixture`] with `channels` value columns (each a
/// differently-phased sine), written to a CSV and opened with a zero RAM
/// budget so it takes the spill path, then prepared exactly like the app
/// prepares a completed spilled load (issue #102).
fn spilled_fixture(label: &'static str, sample_count: usize, channels: usize) -> Fixture {
    use std::io::Write as _;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("spilled.csv");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create fixture"));
    write!(file, "timestamp").expect("write fixture");
    for channel in 0..channels {
        write!(file, ",value{channel}").expect("write fixture");
    }
    writeln!(file).expect("write fixture");
    for i in 0..sample_count {
        write!(file, "{i}").expect("write fixture");
        for channel in 0..channels {
            write!(file, ",{}", (i as f64 * 0.001 + channel as f64).sin()).expect("write fixture");
        }
        writeln!(file).expect("write fixture");
    }
    file.flush().expect("flush fixture");
    drop(file);

    let dataset = glyde_core::ingest::load_with_budget(
        &path,
        glyde_core::budget::RamBudget::from_total_ram_bytes(0),
        dir.path(),
    )
    .expect("spilled fixture must open");
    assert!(
        dataset.is_spilled(),
        "a zero budget must force the spill path"
    );
    assert_eq!(dataset.time.len(), sample_count);
    // Exactly what the app builds for a completed spilled load (issue #102).
    let pyramids = glyde_core::ingest::disk_pyramids_for_dataset(&dataset, dir.path())
        .into_iter()
        .map(|pyramid| pyramid.map(ColumnPyramid::Disk))
        .collect();
    let ticks = dataset.time.to_pyramid_ticks().into_owned();
    let sample_cache = views::time::cache_column_samples(&dataset);
    let cursor = views::time::CursorLookup::for_time(&dataset.time);
    Fixture {
        label,
        _dir: Some(dir),
        dataset,
        pyramids,
        ticks,
        sample_cache,
        cursor,
    }
}

/// One frame, on a persistent `Context` reused across calls exactly like
/// `eframe` drives the real app across repaints. `hover` rests the pointer
/// over the middle of the plot, which is what turns on the cursor readout
/// (SPEC §4.1) — the state the view is in for as long as the user's mouse is
/// over the plot while panning or zooming (issue #114). `view` is the
/// horizontal span shown, in plot seconds. `state` is reused as is: a
/// channel whose cached decimation matches `view` is not decimated again.
/// Returns the tick range the frame actually decimated.
fn render_frame(
    ctx: &egui::Context,
    fixture: &Fixture,
    state: &mut TimeViewState,
    hover: bool,
    view: (f64, f64),
) -> (i128, i128) {
    let events = if hover {
        vec![egui::Event::PointerMoved(Pos2::new(
            VIEWPORT_SIZE.x / 2.0,
            VIEWPORT_SIZE.y / 2.0,
        ))]
    } else {
        Vec::new()
    };
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, VIEWPORT_SIZE)),
        events,
        ..Default::default()
    };
    let mut visible = (0, 0);
    let _ = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            visible = views::time::show_with_selection(
                ui,
                &fixture.dataset,
                &fixture.pyramids,
                &fixture.ticks,
                &fixture.sample_cache,
                &[],
                fixture.cursor,
                state,
                Some(view),
            );
        });
    });
    visible
}

/// The fixture's whole time span, in plot seconds.
fn full_view(fixture: &Fixture) -> (f64, f64) {
    let time = &fixture.dataset.time;
    let first = *fixture.ticks.first().expect("non-empty fixture");
    let last = *fixture.ticks.last().expect("non-empty fixture");
    (
        views::time::tick_to_seconds(time, first),
        views::time::tick_to_seconds(time, last),
    )
}

/// The `step`-th frame of a pan: a window a tenth of the file wide, moved by
/// a hundredth of the file per frame (wrapping), so every frame decimates a
/// different range — what dragging the plot does.
fn pan_view(fixture: &Fixture, step: usize) -> (f64, f64) {
    let (min, max) = full_view(fixture);
    let span = max - min;
    let start = min + span * 0.9 * ((step % 90) as f64 / 90.0);
    (start, start + span * 0.1)
}

/// Asserts SPEC §5's ceiling on one steady-state frame of `view` that
/// decimates every shown channel (no cached decimation), after a warm-up
/// frame, and that the frame really showed the samples it claims.
fn assert_frame_budget(
    ctx: &egui::Context,
    fixture: &Fixture,
    state: &mut TimeViewState,
    hover: bool,
    view: (f64, f64),
    what: &str,
    min_visible_ticks: i128,
) {
    render_frame(ctx, fixture, state, hover, view);
    state.data_changed();
    let start = Instant::now();
    let (first, last) = render_frame(ctx, fixture, state, hover, view);
    let elapsed = start.elapsed();
    assert!(
        last - first >= min_visible_ticks,
        "{what}: the frame showed ticks {first}..{last}, fewer than the \
         {min_visible_ticks} this measurement is meant to cover"
    );
    assert!(
        elapsed <= FRAME_TIME_CEILING,
        "time-domain view render ({what}) took {elapsed:?} for {} {} samples \
         at a {VIEWPORT_SIZE:?} viewport, exceeding the {FRAME_TIME_CEILING:?} \
         build-blocking ceiling (SPEC §5: pan/zoom p99 ≤100ms)",
        fixture.ticks.len(),
        fixture.label
    );
}

/// Benches `fixture` with `channels` as the picker's choice; `shown`, when
/// non-empty, names that choice in the bench ids (e.g. "1 of 32 shown").
fn bench_one_fixture(
    c: &mut Criterion,
    fixture: &Fixture,
    channels: ChannelVisibility,
    shown: &str,
) {
    let label = if shown.is_empty() {
        fixture.label.to_owned()
    } else {
        format!("{}, {shown}", fixture.label)
    };
    let mut state = TimeViewState::default();
    state.channels = channels;
    let full = full_view(fixture);
    let span_ticks = fixture.ticks.last().expect("non-empty") - fixture.ticks[0];
    for hover in [false, true] {
        let ctx = egui::Context::default();
        let mode = if hover { "hovered" } else { "idle pointer" };

        // The worst case: every sample of the file on screen.
        assert_frame_budget(
            &ctx,
            fixture,
            &mut state,
            hover,
            full,
            &format!("full view, {mode}"),
            span_ticks * 9 / 10,
        );
        // Panning: a different window every frame.
        assert_frame_budget(
            &ctx,
            fixture,
            &mut state,
            hover,
            pan_view(fixture, 37),
            &format!("pan, {mode}"),
            span_ticks / 20,
        );

        let suffix = if hover { "/hovered" } else { "" };
        c.bench_function(
            &format!("time_view_render/{label}/full view{suffix}"),
            |b| {
                b.iter(|| {
                    state.data_changed();
                    render_frame(
                        std::hint::black_box(&ctx),
                        std::hint::black_box(fixture),
                        &mut state,
                        hover,
                        full,
                    )
                })
            },
        );
        let mut step = 0;
        c.bench_function(&format!("time_view_render/{label}/pan{suffix}"), |b| {
            b.iter(|| {
                step += 1;
                render_frame(
                    std::hint::black_box(&ctx),
                    std::hint::black_box(fixture),
                    &mut state,
                    hover,
                    pan_view(fixture, step),
                )
            })
        });
    }
}

/// A click in the picker that shows one more channel, with every other
/// channel shown and the view unchanged: only that channel is decimated.
/// Each iteration toggles it, so iterations alternate hiding it (nothing to
/// decimate) and showing it again (one decimation) — the cost of a click.
fn bench_picker_click(c: &mut Criterion, fixture: &Fixture) {
    let ctx = egui::Context::default();
    let full = full_view(fixture);
    let mut state = TimeViewState::default();
    // Settle: every channel decimated for this view once.
    for _ in 0..3 {
        render_frame(&ctx, fixture, &mut state, false, full);
    }
    assert_eq!(state.channels.visible_count(), MULTI_CHANNELS);

    state.channels.toggle(0);
    render_frame(&ctx, fixture, &mut state, false, full);
    state.channels.toggle(0);
    let start = Instant::now();
    render_frame(&ctx, fixture, &mut state, false, full);
    let elapsed = start.elapsed();
    assert!(
        elapsed <= FRAME_TIME_CEILING,
        "a picker click with all {MULTI_CHANNELS} channels shown took {elapsed:?}, \
         exceeding the {FRAME_TIME_CEILING:?} build-blocking ceiling"
    );

    c.bench_function(
        &format!(
            "time_view_render/{}, all {MULTI_CHANNELS} shown/picker click",
            fixture.label
        ),
        |b| {
            b.iter(|| {
                state.channels.toggle(0);
                render_frame(
                    std::hint::black_box(&ctx),
                    std::hint::black_box(fixture),
                    &mut state,
                    false,
                    full,
                )
            })
        },
    );
}

fn bench_time_view_render(c: &mut Criterion) {
    let all = ChannelVisibility::default;
    bench_one_fixture(c, &f64_fixture(SAMPLE_COUNT), all(), "");
    bench_one_fixture(c, &i64_fixture(SAMPLE_COUNT), all(), "");
    bench_one_fixture(
        c,
        &spilled_fixture("f64 spilled (disk pyramid)", SAMPLE_COUNT, 1),
        all(),
        "",
    );

    let many = spilled_fixture(
        "f64 spilled, many channels",
        MULTI_CHANNEL_SAMPLES,
        MULTI_CHANNELS,
    );
    bench_one_fixture(c, &many, all(), &format!("all {MULTI_CHANNELS} shown"));
    let mut one_shown = ChannelVisibility::default();
    one_shown.sync((0..MULTI_CHANNELS).map(|channel| many.dataset.columns[channel].name()));
    one_shown.solo(0);
    assert_eq!(one_shown.visible_count(), 1);
    bench_one_fixture(c, &many, one_shown, &format!("1 of {MULTI_CHANNELS} shown"));
    bench_picker_click(c, &many);
}

criterion_group!(benches, bench_time_view_render);
criterion_main!(benches);
