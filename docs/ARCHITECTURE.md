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

### Two classes of inference (decision, issue #38)

Sniffing runs on a bounded head sample (SPEC §1.2, default 1 MB). Not every
inference it produces has the same authority once streaming begins:

- **File-format inferences** — encoding, delimiter, header, decimal separator —
  are *stable configuration*. They describe how bytes are laid out, so a correct
  sniff settles them for the whole file. If a later row seems to contradict one
  (e.g. a sudden column-count change), that is malformed input to be salvaged
  under SPEC §1.3 — a skipped/flagged row — **not** a signal to re-decide the
  format. They are never "promoted" mid-stream.
- **Semantic inferences** — column dtype — are *provisional hypotheses*. A
  bounded sample can only ever guess a candidate dtype; a value in an unsampled
  row that doesn't fit is **new information, not an error**. The natural model is
  therefore monotonic promotion along the widening lattice
  `Bool → Integer → Float → String` (String being the always-safe terminus that
  degrades no raw data — Golden Rule 1): the streaming reader may widen a
  column's dtype as it observes more rows, never narrow it.

This is why dtype inference splits into two responsibilities across two pipeline
stages:

- `infer` (sniff, bounded sample): produce a **candidate** dtype + confidence.
  Materializes nothing — the raw column stays memory-mapped.
- The **streaming reader / index build** (budget-aware): validate the candidate
  against every row, promote it when a row demands it, and materialize typed
  values in bounded chunks under the peak-RSS cap (SPEC §5.1). The full column is
  never held at once.

The `infer_delimiter` / `infer_header` / `infer_decimal_separator` sample-only
signatures are correct as they stand (stable configuration); `infer_column`'s
whole-column `&[String] → Series` shape is the odd one out and belongs to the
streaming stage, not to `infer`. Reconciling it is the M2 "CSV-reader wiring"
item's job, built to this principle.

## The index

The multi-resolution min/max pyramid is the heart of the performance contract.

- Level 0 = raw samples (in file, memory-mapped, never fully loaded).
- Level *k* stores `(min, max, first_ts, last_ts, nan_count)` per bucket; bucket size grows geometrically (factor 8 recommended, documented in code).
- Level *k+1* is derived from level *k* — **exactly**, min-of-mins and max-of-maxes. Never from resampled or averaged data.
- Built progressively during background indexing; the plot improves as levels complete.
- Bounded by the RAM budget; spills to a cache file in the OS app-data dir, keyed by file path + size + mtime, so reopening a known file is instant.
- The pyramid serves **rendering only**. DSP never reads it.

### Where Level 0 actually lives (decision, issue #59)

"In file, memory-mapped" above is literally true for Parquet — the file
already is typed binary data. It is not true for CSV/TSV: raw samples exist
only as text, and reaching sample *n* means re-parsing from the start, while
`decimate_viewport`'s exactness requirement (every pixel column's min/max
must match a brute-force scan of the raw samples, not an approximation)
needs random access to individual raw samples at the edges of every column,
not just a streaming pass.

Resolved as follows:

- **Typed Level-0 spill cache** (`glyde_core::index::level0`). At index time,
  decoded `(timestamp, value)` pairs are written once, streaming (one sample
  at a time — no `Vec` ever holds the whole column), to a fixed-width typed
  cache file pair in the OS cache directory, keyed by source path + size +
  mtime — the same scheme already used for the pyramid above. Reopening a
  known file memory-maps the existing cache directly instead of re-parsing.
  Cost: roughly the size of the raw numeric data again on disk (16
  bytes/sample for timestamps, 8 bytes/sample for values) per opened file.
  Rejected alternatives: re-parsing raw text on every edge-bucket read (too
  slow against the pan/zoom frame budget, and doesn't extend to Parquet);
  caching only the current viewport window (loses the "reopening a known
  file is instant" property and still doesn't give exact edge buckets when
  the user jumps around).
- **The locked golden-test API is the engine for every file size**, not a
  small-file-only façade. `dsp::decimation::{build_pyramid,
  decimate_viewport}` keep exactly the `&[f64]`/`&[i128]` signatures the
  golden tests were written against; the large-file path memory-maps the
  Level-0 cache and hands these functions a real slice over the mapped
  bytes, rather than introducing a second implementation or a generic
  `RawSamples` trait for one call site.
- **Amendment (issue #88): one *reading* protocol beside the locked
  signatures, still one aggregation.** The bullet above holds for querying,
  and `decimate_viewport`'s signature is unchanged. It did not hold for
  *building*: a slice over a spilled column is only cheap if nobody walks it
  end to end, and `build_pyramid` walks it end to end by definition, making
  every page of a memory-mapped column resident — the same residency issue
  #85 removed from the time axis, worth 8 bytes/sample per column plus 16
  bytes/row of ticks. `dsp::decimation::build_pyramid_streaming` is therefore
  an additional entry point taking a `series::SampleSource` and a
  `time::TickSource` (the sample-side counterpart of the trait #85 already
  introduced for ticks) and reading both in bounded chunks. This is not a
  second implementation: both entry points feed the same private level-0
  accumulator and the same level-growing step, and a golden test locks them to
  bucket-for-bucket equality at every chunk size, so Hard rule 4 still holds.
  `ingest::pyramids_for_dataset` picks between them by
  `Dataset::is_spilled()` — the zero-copy slice path stays the in-memory
  default, so no small-file cost was added.
- **Pyramid levels are also spilled to disk** (`glyde_core::index::pyramid`,
  `docs/ROADMAP.md` M3 "Pyramid level spill"): reopening a large file reads
  the already-computed `Vec<Vec<Bucket>>` back from a cache file, keyed the
  same way as Level 0, instead of rebuilding it from the cached Level 0.
  Unlike Level 0, this cache is not memory-mapped — `dsp::decimation`'s
  locked query API takes an owned `&[Vec<Bucket>]`, not a borrowed cache
  view, so a hit materializes an owned pyramid from the cache file rather
  than mapping it in place. The win is skipping the aggregation pass (and
  not needing Level 0 open at all to redo it), not a zero-copy reopen.
- **Deferred, tracked separately:** cache eviction (the cache directory only
  ever grows, for Level 0, the pyramid, and the ingestion spill files alike);
  and the size of an owned pyramid itself — ~9 bytes per sample per column
  across all levels, which is proportional to file size and is what a
  pyramid-accelerated view of a *spilled* file would hit first (issue #102).

### Where the raw `Dataset` lives (decision, issue #75)

`Level0Cache` and the pyramid cache spill *derived* data. The raw `Dataset`'s
own columns were, until issue #75, unconditionally `Vec`-backed for any file
size, which put peak RSS at ~3.3× the source file — proportional, where SPEC §5
caps it at a flat `min(25% RAM, 4 GB)`. Resolved as follows (the maintainer's
"Option B" on that issue):

- **One spill primitive, `index::spill`.** `SpillVec<T>`/`SpillVecWriter<T>`
  append fixed-width typed elements to a file in the cache directory one at a
  time and memory-map the finished file back as a real `&[T]`. `SpillStrings`
  is the variable-width case (byte arena + end-offset table). `index::level0`
  is unchanged and stays a separate thing: it is a *cache*, keyed for reuse
  across opens, holding the `(i128, f64)` pair `dsp::decimation` consumes;
  `index::spill` is the ingestion path's *backing store*, one file per column
  in that column's own dtype width. They share the scheme, not the code —
  merging them would give the Level-0 cache a dtype it does not need or give
  every spilled column a duplicate timestamp file it does not want.
- **The backing store is chosen before the read, from the RAM budget.**
  `ingest::dataset` sniffs the bounded head sample, estimates what
  materializing the file would cost, and asks `budget::RamBudget`. Affordable →
  the existing in-memory path, untouched. Not affordable → the spill path.
  SPEC §5.1's "checks affordability before acting, never after", with the
  spill as the "affordable alternative" the same sentence requires.
- **The spill path never memory-maps the whole file.** Walking a mapping end to
  end makes every page resident, which is itself proportional to file size, so
  the source is read through a fixed-size buffer and decoded incrementally
  (SPEC §5.1's "read in bounded chunks" taken literally). The in-memory path
  still maps, as before.
- **Inference is incremental, never sampled.** `time::TimestampFormatScan` and
  `ingest::infer::ColumnDtypeScan` are the whole-slice `infer_timestamp_format`
  / `infer_column` decisions restated as one-pass scans, and those whole-slice
  functions are implemented *on top of them* — so a file's inferred timestamp
  format and column dtypes are decided over every row on both paths, and can
  never depend on how large the file happens to be. This is the
  §"Two classes of inference" split above, finally realized: dtype is a
  provisional hypothesis the streaming stage validates and may only widen.
- **Progress checkpoints survive the spill.** SPEC §5's "first meaningful plot,
  any file size ≤ 2 s" applies to spilled files most of all, but a checkpoint
  cannot hand out a `Dataset` over spill files still being written — they are
  published by an atomic rename, and Windows will not rename a mapped file. The
  spilled path therefore keeps a *bounded* in-memory preview of the first rows
  (`ingest::dataset::PREVIEW_MAX_ROWS`) and checkpoints from that, on the same
  doubling schedule the in-memory path uses; past the cap it retires the preview
  and streams straight through. A preview is a real heap-backed `Dataset`, so
  `glyde-app` needs no special case for it.
- **Two passes over the source, not one.** A column's dtype is only settled once
  the last row has been seen (SPEC §1.4: one non-numeric cell keeps the whole
  column as text), so the spill path scans first and writes typed values second.
  Re-reading the source is cheaper, and far more faithful, than spilling every
  field's text first only to re-type it.
- **Storage is invisible above `ingest`.** `SeriesValues::Spilled`,
  `TimeAxis`'s `Timestamps` and `ProgressiveValues` compare equal to the
  heap-backed values they would have been, element for element, so no consumer
  has to know which it holds (Golden Rule 1: a storage change, not a data
  change).
- **The derived statistics are bounded too.** SPEC §2.2's rules are defined over
  the *median* of the whole Δt distribution, and an exact median cannot be
  computed in one bounded pass — so `time::gap` reads its ticks through
  `time::TickSource`, which is replayable and hands them over in bounded chunks
  (a spilled axis reads them back through a fixed-size buffer rather than
  through the mapping, so scanning does not make the column resident), and
  selects each order statistic by iterative histogram refinement: one pass for
  the key range, then at most eight passes each narrowing the surviving range by
  2¹⁶, with 512 KiB of counters. The result is the value a full sort would have
  picked — exact, not approximate, so no golden test or corpus expectation
  changes — and `ingest::report` derives the gap count, the sampling class and
  the monotonicity counts from *one* median and *one* Δt pass instead of three
  independent scans (issue #85). Peak RSS is now flat: `memory_gate` measures
  10.7 MB on a 2 GB fixture and 10.3 MB on an 8 GB one, against the 0.42× it
  cost before.

### Progressive-axis tick mapping and `TimeUnit` placement (decision, issue #60)

`Bucket`'s `first_ts`/`last_ts` and `build_pyramid`/`decimate_viewport`'s
`timestamps`/`range` are bare `i128` ticks with no unit attached, and SPEC
§2.1's two time-axis kinds (`Absolute` timestamps, `Progressive` numeric
index) don't obviously share one tick space — `Absolute` ticks come from
`Timestamp`, `Progressive` values are unitless `f64`.

Resolved as follows:

- **`Absolute` ticks pass through unchanged.** Every `Timestamp` in one axis
  was parsed against the same detected `TimestampFormat`, hence already
  shares one `TimeUnit` — `Timestamp::ticks` is handed to the pyramid
  verbatim (`TimeAxis::to_pyramid_ticks`, `glyde_core::ingest::dataset`).
- **`Progressive` values are fixed-point scaled**, by a documented ×10⁹
  factor (`ingest::PROGRESSIVE_TICK_SCALE`), into the same `i128` tick
  space. This is chosen over using the sample *ordinal* as the tick: scaling
  preserves true x-distance between samples, so an unevenly-spaced
  progressive axis decimates the same way an absolute-time axis with
  identical physical spacing would, matching the pyramid's exactness
  guarantee instead of degrading to "aggregate by sample count" for one of
  the two axis kinds. The ×10⁹ factor is an assumption (SPEC §2.1 does not
  name one): it caps exact representation at magnitudes below
  `i128::MAX / 1e9` (~1.7×10²⁹) and nine fractional digits, which no
  realistic progressive numeric index is expected to exceed.
- **`Bucket` carries no `TimeUnit` field.** `dsp::decimation` stays exactly
  what its own module doc already describes: a pure engine over opaque
  `i128` ticks, unaware of calendar time or which axis kind produced them.
  The caller — which already knows the `TimeAxis` variant it built the
  pyramid from — owns converting ticks back to a display value
  (`Timestamp::unit` for `Absolute`, `ingest::progressive_tick_to_value` for
  `Progressive`). This was chosen over adding `unit: Option<TimeUnit>` to
  every `Bucket` (redundant per-bucket state within one homogeneous
  pyramid) and over making the pyramid generic over the tick type (the
  Level-0 on-disk spill cache is tightly typed to fixed `i128`/`f64` byte
  layouts today; genericizing it is a much larger change for the less
  common of the two axis kinds).
- **Not yet wired into the app.** `TimeAxis::to_pyramid_ticks` exists and is
  tested, but nothing yet calls `build_pyramid`/`index::level0::build` from
  `Dataset` — that lands with the ROADMAP M3 "background progressive build"
  item, which will use this conversion at the call site.

## Threading model

- **UI thread**: render loop, input, state. Never blocks.
- **Indexer thread**: sniff, infer, build pyramid; emits progress + partial levels.
- **Compute pool** (`rayon`): decimation queries, Welch.
- Communication: message channels, immutable snapshots. No shared mutable state across threads without a documented reason.

## Error philosophy

- `glyde-core` returns `Result<_, GlydeError>` (`thiserror`). It **never panics on user data**.
- Every absorbed anomaly (skipped row, bad encoding byte, non-monotonic timestamp) becomes a counted, logged, user-visible event — never a silent drop.
- `unwrap()`/`expect()` in core are allowed only where a comment proves the invariant.
