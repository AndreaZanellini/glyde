# Glyde — Architecture

Binding constraints. Claude Code must not restructure this without the maintainer's approval via an issue.

## Language and stack (decided)

**Rust**, stable toolchain, pinned via `rust-toolchain.toml`.

Rationale (short version, for future contributors): Glyde needs deterministic memory control and a single dependency-free binary on three OSes, while being developed by an AI agent under a maintainer who does not read the code. Rust is the only mainstream option that gives both native performance/packaging *and* a compiler that catches whole classes of bugs before runtime — a second safety net next to the test suite.

| Layer | Choice |
|---|---|
| GUI + rendering | `eframe`/`egui` on `wgpu` |
| CSV ingestion | `csv` / `csv-core` — streaming, single-pass over a memory-mapped file |
| Parquet ingestion | `parquet` + `arrow` (arrow-rs) — row-group access |
| File access | `memmap2` |
| Encoding | `encoding_rs`, `chardetng` |
| Time | `chrono`, `chrono-tz` |
| FFT | `rustfft` (Welch, windowing and normalization are **ours**, on top) |
| Parallelism | `rayon` (compute), `std::thread` + channels (background indexing) |
| Logging | `tracing`, `tracing-subscriber`, `tracing-appender` |
| Errors | `thiserror` (core), `anyhow` (app boundary) |
| System info | `sysinfo` (RAM budget) |
| Tests | `cargo test`, `proptest`, `insta` (snapshots), `criterion` (benchmarks) |

New dependencies require a one-line justification in the PR. Prefer the above. `deny.toml` bans a few crates by name; read the comments there before proposing an alternative.

### Why not a DataFrame library (polars, etc.)

Considered and rejected. A DataFrame engine is built to load and transform tables in memory; Glyde does the opposite — it never materializes the dataset, it streams once to build an index and thereafter reads only what a viewport needs. Its inference (joint delimiter/decimal resolution, encoding recovery, metadata preambles, ragged-row salvage) is the product's differentiator and must be ours, so a DataFrame library's own inference would go unused. What would remain is raw parse speed, which `csv` over memory-mapped chunks with `rayon` already delivers — without the dependency tree, the compile times, or the pressure on the 50 MB binary budget.

The Parquet path benefits doubly: row groups carry **column min/max statistics** in their metadata, which populate the upper levels of the index pyramid with no scan at all.

## Workspace layout

```
glyde/
├── Cargo.toml                 # workspace
├── rust-toolchain.toml
├── crates/
│   ├── glyde-core/            # NO GUI dependencies. 100% headless-testable.
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── ingest/        # readers + inference
│   │   │   │   ├── mod.rs     # Reader trait, open() entry point
│   │   │   │   ├── infer.rs   # encoding, delimiter, header, decimal, dtype
│   │   │   │   ├── csv.rs
│   │   │   │   └── parquet.rs
│   │   │   ├── time/          # time index, formats, gaps, sampling class
│   │   │   ├── index/         # multi-resolution min/max pyramid, spill
│   │   │   ├── dsp/           # decimate.rs, welch.rs, window.rs, detrend.rs
│   │   │   ├── series/        # Series, dtype, anomalies
│   │   │   └── budget/        # RAM budget, affordability checks
│   │   ├── benches/           # index_build.rs, viewport_query.rs, welch.rs
│   │   └── tests/             # golden tests, torture corpus, property tests
│   ├── glyde-app/             # thin egui shell
│   │   └── src/
│   │       ├── main.rs
│   │       ├── views/         # time.rs, psd.rs, state_timeline.rs
│   │       ├── inference_bar.rs
│   │       └── plumbing/      # background jobs, channels
│   └── glyde-devtools/        # CI tooling only, never shipped (publish = false)
│       └── src/bin/
│           ├── generate_fixtures.rs   # multi-GB synthetic files for benches
│           └── memory_gate.rs         # headless peak-RSS gate
├── testdata/
│   └── corpus/                # torture corpus (small files, committed)
├── docs/
└── .github/workflows/
```

## Hard rules

1. `glyde-core` **must not** depend on `glyde-app`, `egui`, `eframe`, `wgpu`, or any windowing crate. Enforced in CI by checking `cargo tree` for GUI crates under core.
2. All product logic — inference, time handling, indexing, DSP, budget decisions — lives in `glyde-core`. `glyde-app` only renders state and forwards user intent. If a behavior can be tested without a window, it belongs in core.
3. **The UI thread never performs I/O and never blocks.** Indexing, reading, and DSP run on background threads and communicate via channels; the UI renders whatever is currently available.
4. One canonical implementation per concept. There is exactly one time representation, one decimation path, one PSD path. Duplicating them is the failure mode this document exists to prevent.
5. Readers implement a single `Reader` trait; adding a format touches only `ingest/<format>.rs` and a registry entry.

## Core pipeline

```
open(path)
  → sniff (bounded head read, memmap)
  → infer: encoding → delimiter → header → decimal → time column → dtypes
  → InferenceReport (what + confidence)  ─────────────► UI inference bar
  → build TimeIndex (ticks, gaps, sampling class)
  → build min/max pyramid (background, progressive, budget-aware, spillable)
       │
       ├─ first level ready ──────────────────────────► first plot (≤ 2 s)
       └─ continues in background
  → viewport request (time range, pixel width)
       → pyramid level lookup → min/max per pixel column → render
       → if samples < pixels: read raw samples for the range → render points
  → PSD request (time range)
       → ALWAYS raw samples, streaming, per uniform segment → Welch → render
```

## The index

The multi-resolution min/max pyramid is the heart of the performance contract.

- Level 0 = raw samples (in file, memory-mapped, never fully loaded).
- Level *k* stores `(min, max, first_ts, last_ts, nan_count)` per bucket; bucket size grows geometrically (factor 8 recommended, documented in code).
- Level *k+1* is derived from level *k* — **exactly**, min-of-mins and max-of-maxes. Never from resampled or averaged data.
- Built progressively during background indexing; the plot improves as levels complete.
- Bounded by the RAM budget; spills to a cache file in the OS app-data dir, keyed by file path + size + mtime, so reopening a known file is instant.
- The pyramid serves **rendering only**. DSP never reads it.

## Threading model

- **UI thread**: render loop, input, state. Never blocks.
- **Indexer thread**: sniff, infer, build pyramid; emits progress + partial levels.
- **Compute pool** (`rayon`): decimation queries, Welch.
- Communication: message channels, immutable snapshots. No shared mutable state across threads without a documented reason.

## Error philosophy

- `glyde-core` returns `Result<_, GlydeError>` (`thiserror`). It **never panics on user data**.
- Every absorbed anomaly (skipped row, bad encoding byte, non-monotonic timestamp) becomes a counted, logged, user-visible event — never a silent drop.
- `unwrap()`/`expect()` in core are allowed only where a comment proves the invariant.
