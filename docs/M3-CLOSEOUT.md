# M3 Closeout — gap analysis and closure

**Status: M3 is closed.** Verified 2026-08-02 against `main` at `92c586a`, with
`cargo test --workspace` green (414 passed, 0 failed, 8 ignored — the Welch
goldens waiting on M5).

This document started on 2026-07-27 as a gap analysis: every M3 checkbox was
ticked, but three of M3's five maintainer tests exercised the *app*, and the app
did not call any of the M3 machinery. It is kept as the record of what was
missing and what closed it, because that history is the argument for why M3 can
be called done now. The plan it laid out (§3) was followed; each item's outcome
is recorded in place rather than deleted.

**What closing M3 does *not* mean** is in §6. Four things are deliberately left
open, each with an issue. None of them block M4.

---

## 1. Verdict

**M3's core was done. M3's product now is too.**

| M3 maintainer test | Status | Gated by |
|---|---|---|
| First plot within ~2 s, indexing continuing in background | **Passes** | `first_plot` bench; progressive checkpoints reach the UI |
| Scroll/zoom for 30 s → no stutter; memory under the cap | **Passes** | `memory_gate` at 4 GB in CI; `time_view_render` p99 gate — but see issue #107, the frame-time gate covers in-memory files only |
| One-sample spike stays visible at every zoom level | **Passes** | decimation golden — spike preservation at 800 columns |
| Zoom all the way in → converges to individual sample points | **Passes** | decimation golden — convergence; markers now conditional per SPEC §3.1 |
| Close and reopen the big file → opens instantly (from cache) | **Passes, with a documented residual** | `pyramid_reopen_integration.rs`, `level0_reopen_integration.rs`. The *plot* is instant; the CSV text is still re-parsed on every open, so the inference bar is re-derived — issue #106 |

The last row is the one judgement call in this document, and it is the
maintainer's: M3 closes with the re-parse residual documented rather than staying
open for it. The reasoning is in §6.1.

---

## 2. The gaps, and what closed them

### G1 — The decimation pyramid was never wired into the UI · issue #80 · **closed**

`decimate_viewport` had zero callers outside `glyde-core`'s own benches and
tests; `views/time.rs` drew a `Line` per NaN-delimited segment over every raw
sample, with point markers drawn unconditionally, and the pyramids built at each
checkpoint were thrown away because `IndexingMessage` had nowhere to put them.

Closed by PRs #89/#91: `Checkpoint::pyramids` and `pyramids_for_dataset` flow
through `IndexingMessage` into `Status::Loading`/`Status::Loaded`, and
`views/time.rs` queries `decimate_viewport` every frame against the plot's live
pan/zoom bounds and pixel width. The raw-samples-plus-markers regime is now
conditional on `decimate_viewport`'s own convergence, per SPEC §3.1.

A new headless bench, `crates/glyde-app/benches/time_view_render.rs` — which
required splitting `glyde-app` into a `[lib]` + `[[bin]]` so a bench could reach
`views::time::show` — gates the real render path against SPEC §5's p99 ≤ 100 ms
ceiling. It earned its keep immediately: the first version of `show` recomputed
`TimeAxis::to_pyramid_ticks()` on every frame, which the bench caught at 4.75 s
for one frame at 8 M samples, ~47× the ceiling and entirely invisible to
`decimate_viewport`'s own isolated bench. Fixed by moving `ticks` out of the
render loop into `crate::app`'s status transition, and by switching
`format_x_axis_tick`'s per-gridline offset lookup from an O(n) exact scan to an
O(log n) search. 4.75 s → 121 ms → **~60 µs** steady state.

**Also closed since:** the from-scratch pyramid rebuild at every progressive
checkpoint (issue #90). `dsp::decimation::extend_pyramid` now extends the
previous checkpoint's pyramid with only the samples added since, bucket-for-bucket
identical to a full rebuild — locked by a golden test at irregular checkpoint
sizes.

### G2 — The Level-0 / pyramid spill cache was dead code · issue #81 · **closed**

`index::level0::{build, build_or_open, try_open}` and `index::pyramid`'s
equivalents had no callers outside `index/` and their own tests. `CacheKey` —
the path+size+mtime reuse mechanism — never entered the open path. SPEC §5.1
makes the spill part of the *memory* contract, so with the cache unwired there
was no spill path at all in the running product.

Closed in two halves:

- **Pyramid half** (PR #93): `ingest::pyramids_for_dataset_cached` calls
  `pyramid::build_or_open` per numeric column, keyed by
  `CacheKey::for_path(path).with_column(index)` — `with_column` being new, since
  both caches were originally keyed one-per-file, not one-per-column — and
  `with_overrides_signature`, so a one-click correction (M4) of a byte-for-byte
  unchanged file cannot collide with the pre-correction entry.
- **Level-0 half** (issue #92, PR #105): `ingest::derived_caches_for_dataset_cached`
  builds both caches together and `views::time::show` reads a column's raw
  samples from the Level-0 cache whenever present, so a reopen's deep-zoom
  raw-sample view is cache-backed too.

The order in that second PR matters and is not incidental: Level 0 is resolved
first and converts a non-`f64` column only on a miss; the pyramid is then built
from the Level-0 cache's own memory-mapped `samples()`/`timestamps()`. That is
what `docs/ARCHITECTURE.md` §"Where Level 0 actually lives" prescribes, and it
means a reopen converts nothing at all while a first open converts each column
exactly once. An earlier revision of that PR built the two independently and so
converted twice per open — caught in review, and now locked by a test that counts
the conversions directly (an `i64` column beyond ±2⁵³ emits one precision-loss
warning per element converted; the assertion is exactly `ROWS` then exactly `0`).

### G3 — Raw `Dataset` memory was unbounded · issue #75 · **closed**

`Dataset` was `Vec`-backed for any file size and `RamBudget::check_affordable`
had exactly one caller in the workspace — the `memory_gate` *measurement* tool,
not the read path. SPEC §5.1's "checks affordability before acting, never after"
was unimplemented. Measured at ~2.5× file size on a 1 GB fixture.

The maintainer chose **Option B** (stream rows into bounded/spilled storage).
Landed in PR #86: `index::spill` is the one on-disk primitive; `ingest::dataset`
runs the affordability check on a bounded head sample *before* reading and picks
between the untouched in-memory path and a bounded-chunk streaming path that
writes each typed column straight to disk. A 2 GB file went from 7.16 GB peak RSS
(3.34×, over the 4.21 GB cap) to 0.90 GB (0.42×).

**Residual, closed by issue #85** (PR #89): that 0.42× was not flat — the sample
data was on disk, but SPEC §2.2's median-based Δt statistics still cost ~48
bytes/row in temporaries and resident tick pages. `time::TickSource` (bounded,
replayable chunk reads) plus an exact iterative-histogram order statistic removed
both: **10.7 MB on a 2 GB file and 10.3 MB on an 8 GB one** — flat, and 0.4% of
the cap rather than 21% of it.

**Residual, closed by issue #88** (PR #104): the same whole-column-slice pattern
survived in the pyramid builder, which handed `build_pyramid` one contiguous
slice over each memory-mapped column. Unreachable from the app, but one wiring
change away from reintroducing proportional peak RSS with no test to catch it.
`dsp::decimation::build_pyramid_streaming` reads a `series::SampleSource` and a
`time::TickSource` in bounded chunks, feeding the same level-0 accumulator as
`build_pyramid` — golden-locked to bucket-for-bucket equality at every chunk
size, so there is still one aggregation definition. `memory_gate` now builds a
real pyramid while sampling, so the flat-RSS property is finally proven on a path
that builds one.

### G4 — `YYYY-MM-DD HH:MM:SS` files did not open at all · issue #82 · **closed**

An M2 leak, not an M3 one, but it blocked M4 and was the most user-visible defect
found: `TimestampFormat::DateTimeSpace` existed but `parse_timestamp` /
`format_timestamp` were `todo!()`, so inference fell through to the
progressive-numeric branch, `parse::<f64>()` failed, and the load aborted with
`NonNumericTimeIndex`. It was also a landmine for M4: a correction UI offering
`DateTimeSpace` would have reached a `todo!()` — a panic through normal
interaction.

Closed in two parts: PR #95 implemented the format (including the sub-nanosecond
fractional path) and added it to `IN_SCOPE_FORMATS` ordered so it cannot shadow
the ISO variants; PR #100 (issue #94) made a time column that matches no known
format stop aborting the load, falling back to a row-ordinal index flagged in the
inference bar instead — SPEC §1.3's "never abort the load".

### G5 — The memory gate ran at 1/200th of spec scale · issue #83 · **closed**

`docs/QUALITY.md` §3 calls the 20 GB memory gate "the most important benchmark in
the suite"; CI ran it against a 100 MB fixture, where the then-current ~2.5×
multiplier landed ~250 MB against a ~1.88 GB cap. The one benchmark designed to
catch the single most serious class of bug in this product was disarmed by the
bug it existed to catch.

Closed by PR #99, applied by the maintainer since `.github/workflows/**` is
hard-denied to agent sessions: `GLYDE_BENCH_FIXTURE_GB` is now `4`, against
`macos-14`'s ~1.75 GB cap — a file that cannot be materialized in memory even
once. The gate also runs before the criterion benches and reclaims its fixture
immediately after, so the runner's SSD never holds every large fixture at once.
QUALITY §3.2's ">15% vs `main`" comparison was resolved the other way: it is now
documented as a manual reference-machine ritual rather than a CI check, because
shared runners are too noisy for a 15% threshold to mean anything.

---

## 3. The plan, and what happened

Every item is one PR unless noted. All are done.

| # | Item | Outcome |
|---|---|---|
| R0 | #75 — decide the raw-`Dataset` memory strategy | **Decided** (Option B) and **landed** — PR #86, §2 G3 |
| R1 | #82a — implement `DateTimeSpace` | **Landed** — PR #95 |
| R2 | #82b — never abort on an unknown time column | **Landed** — PR #100 (issue #94) |
| R3 | #80a — plumb pyramids into app state | **Landed** — PR #89 |
| R4 | #80b — decimated render + SPEC §3.1 rule | **Landed** — PR #91, plus the frame-time bench |
| R5 | #81 — wire the spill cache into `open()` | **Landed** in two halves — PRs #93 (pyramid) and #105 (Level 0, issue #92) |
| R6 | #83 — re-arm the memory gate | **Landed** — PR #99, maintainer-applied |
| — | #90 — incremental checkpoint pyramids | **Landed** — PR #96, split out of R3 rather than dropped |
| — | #88 — bounded pyramid build over a spilled dataset | **Landed** — PR #104, split out of R4/R5 rather than dropped |

The dependency shape the original plan drew held: R0 gated R4/R5/R6, and doing it
first meant none of the later work had to be redone. The one thing the plan did
not anticipate is how much would be found *while* closing it — five of the nine
rows above are follow-ups split out of another item rather than items the
analysis named. That is the process working as intended (§6 lists what is still
open on the same basis), not scope creep.

---

## 4. Model and effort, in retrospect

The original table picked the **minimum** model/effort that should complete each
task reliably. Recorded here with how the picks actually went, since this is the
only place the project keeps that calibration.

| # | Item | Planned | How it went |
|---|---|---|---|
| R0-B | #75 Option B implementation | `claude-opus-5` / `xhigh` | As planned; the one item where under-powering would have cost more than it saved |
| R1 | #82a `DateTimeSpace` | `claude-sonnet-5` / `medium` | As planned — golden test already written, sibling helpers already existed |
| R2 | #82b never-abort fallback | `claude-opus-5` / `high` | As planned; small diff, but a product decision under SPEC ambiguity |
| R3 | #80a plumb pyramids | `claude-sonnet-5` / `medium` | As planned |
| R4 | #80b decimated render | `claude-opus-5` / `high` | **Landed on `claude-sonnet-5`** after the technical design (egui_plot's `PlotTransform`/`PlotBounds`, tick↔seconds conversion, gap-bucket classification) was worked out ahead of implementation |
| R5 | #81 spill wiring | `claude-sonnet-5` / `high` | Split in two. The second half needed a review round: the first revision converted each non-`f64` column twice per open and its CHANGELOG claimed the opposite. Cache-key correctness was the flagged risk; the actual defect was a performance claim that no test covered |
| R6 | #83 fixture + QUALITY | `claude-haiku-4-5` / `low` | As planned — a pasteable diff, not a merge |

Two notes that still hold:

- **Do not lower effort to shorten output.** On Opus 5 it does not reliably work,
  and on this repo the deliverable is tests plus a `CHANGELOG.md` entry, which are
  cheap either way.
- **`claude-fable-5` was not warranted anywhere on this list.** Every item was a
  bounded single-PR change with a written test as the acceptance criterion.

One lesson worth carrying into M4, from R5: the adversarial review caught a
defect that every automated gate passed, because the fixtures all used the one
dtype the defect did not affect. When a change is dtype-sensitive, the test
fixtures need to name that explicitly.

---

## 5. ROADMAP amendments — applied

The M3 checkboxes described `glyde-core` deliverables accurately but overstated
what the app could do. Rather than un-tick them, they were annotated in place;
all four amendments this document proposed are now in `docs/ROADMAP.md`, updated
as each item landed rather than left describing the mid-flight state.

---

## 6. What M3 does not deliver

Four things, each tracked. None blocks M4.

### 6.1 A reopen still re-parses the file's text · issue #106

This is the residual the last maintainer-test row closes with, and the one
deliberate judgement in this document.

A reopen serves both derived caches from disk — the pyramid and the raw samples —
so the *plot* appears without recomputing the index. Everything else is still
derived on every open: encoding sniff, delimiter and decimal inference, the CSV
parse and per-row typing, anomaly detection, and the `InferenceReport` the
inference bar renders.

Closing it fully would mean a persistent whole-`Dataset` cache — every dtype
including strings, plus `CsvParseOutcome`, anomalies and the inference report —
overlapping heavily with what `index::spill`'s per-open backing store already
does, differing mainly in being persistent-and-reused rather than
scratch-and-discarded. That is materially larger than anything else remaining in
M3, and issue #92 called it product-shaped rather than mechanical: it changes
what "the raw `Dataset`" means across a reopen, which issue #75's Option B
decision did not cover.

**Decision: M3 closes with this documented.** The maintainer test's "opens
instantly" is satisfied in the sense that matters for the performance contract —
the index is not rebuilt — and the remaining cost is a bounded, streaming parse
whose memory is already flat. Skipping it is an optimization for a later
milestone, not an unfinished part of this one.

### 6.2 A spilled file gets no pyramid in the app · issue #102

Reading a spilled column to build one is bounded as of issue #88, so that is no
longer the obstacle. What is, is the pyramid's *own* size: `Bucket` is 64 bytes
in memory and the levels sum to about `rows / (PYRAMID_FACTOR - 1)` buckets —
**≈ 9.1 bytes per sample per column**, held owned in RAM. Measured on a 1 GB,
8-column fixture: 676 MB of buckets, 0.61× file size. Proportional to file size,
which is what SPEC §5's flat cap forbids, and it lands hardest on exactly the
files that spill.

So `views::time::show` still falls back to an un-pyramided `decimate_viewport`
scan for a spilled load — correct and viewport-bounded, just not
pyramid-accelerated. Bounding the pyramid itself means it stops being an owned
`Vec<Vec<Bucket>>` and becomes a view over the cache file `index::pyramid`
already writes, which reopens the ARCH §"Where Level 0 actually lives" / issue
#59 decision about `decimate_viewport`'s locked signature — a maintainer call.

### 6.3 The frame-time gate covers in-memory files only · issue #107

`time_view_render` benches two in-memory fixtures. A spilled file takes the
different render path described above, and nothing measures its frame time —
the opposite of the file M3's maintainer test names ("> 5 GB … scroll and zoom
for 30 s"). The memory half of that test *is* gated; the stutter half is gated
only for files small enough not to spill. A spilled fixture can be forced with a
small `RamBudget` without needing a multi-GB file.

### 6.4 The gap list is unbounded in principle · issue #103

`time::detect_gaps` returns `Vec<Gap>`. The open pipeline no longer calls it —
`summarize_ticks` counts gaps without collecting them — but it is what M8's gap
view will call. A file where slightly under half the Δt are large has a small
median and turns nearly every large Δt into a gap, so the list is `O(rows)` in
the worst case. No such file has been observed and the corpus has none; M8 should
decide whether the gap view consumes a bounded window before the return type
becomes load-bearing in the UI.

### Also open, not on the M3 path

**Issue #30** — the release workflow can publish unvalidated, mis-packaged
binaries on tag. Mostly M9 work, but the `v*` trigger is armed *now*. If a tag is
at all likely before M9, narrow the trigger to `workflow_dispatch` first. A
denied file, so it needs a maintainer-applied diff.

**Issue #97** — one-click correction covers delimiter, decimal separator and the
day/month swap, but not encoding, time-column choice, or an arbitrary timestamp
format. M4's problem, not M3's.
