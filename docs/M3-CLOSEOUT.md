# M3 Closeout — gap analysis and roadmap

Status as of 2026-07-27, against `docs/ROADMAP.md` §M3 and its maintainer test.
Working tree: merge-base `4214a64`. `cargo test --workspace` is **green** (296 passed,
0 failed, 9 ignored — the 8 Welch goldens for M5 plus 1 time golden for issue #82).

---

## 1. Verdict

**M3's core is done. M3's product is not.**

Every M3 checkbox in `docs/ROADMAP.md` is ticked, and each one is defensible *as a
`glyde-core` deliverable*: the pyramid, the decimation query, the Level-0 and pyramid
spill caches, the RAM-budget module, and the criterion benches all exist and are
covered by golden tests, integration tests, and build-blocking budget assertions.

What is missing is the wiring. Three of M3's five maintainer tests exercise the *app*,
and the app does not call any of the M3 machinery:

| M3 maintainer test | Status |
|---|---|
| First plot within ~2 s, indexing continuing in background | **Passes** — progressive checkpoints reach the UI |
| Scroll/zoom for 30 s → no stutter; memory under the cap | **Fails** — every frame draws every raw sample; peak RSS is unbounded |
| One-sample spike stays visible at every zoom level | **Not demonstrated** — holds only incidentally, by drawing everything |
| Zoom all the way in → converges to individual sample points | **Cannot pass as specified** — there is no min/max regime to converge *from* |
| Close and reopen the big file → opens instantly (from cache) | **Fails** — the spill cache is never read or written by the product |

This is a documentation-vs-reality gap, not a regression. Nothing was broken; the
integration step was simply never scheduled as its own roadmap item. The purpose of
this document is to schedule it, before M4 and M5 stack more UI on an unfinished base.

---

## 2. The gaps, with evidence

### G1 — The decimation pyramid is never wired into the UI · issue #80

`decimate_viewport` has **zero callers** outside `glyde-core`'s own benches and tests:

```
$ grep -rn "decimate_viewport" crates/ --include=*.rs | sed 's/:.*//' | sort | uniq -c
      7 crates/glyde-core/benches/viewport_query.rs
      1 crates/glyde-core/src/dsp/decimation.rs        # its own definition
      1 crates/glyde-core/src/index/level0.rs          # doc comment
      2 crates/glyde-core/src/ingest/dataset.rs        # doc comments only
      5 crates/glyde-core/tests/golden/decimation.rs
      4 crates/glyde-core/tests/level0_decimation_integration.rs
      2 crates/glyde-core/tests/property.rs
      5 crates/glyde-core/tests/pyramid_spill_integration.rs
```

`crates/glyde-app/src/views/time.rs` still carries its M2 module doc ("the 'small
files, pre-pyramid' half of SPEC §3.1 … no min/max decimation pyramid behind it yet")
and the code matches: a `Line` per NaN-delimited segment over every raw sample, point
markers drawn **unconditionally** (SPEC §3.1 makes them conditional on
`samples < pixels`), and a linear scan for the cursor readout.

Worse, the pyramids that *are* built get thrown away. `ingest::dataset::Checkpoint`
carries `pyramids: Vec<Option<Vec<Vec<Bucket>>>>`, populated at every progressive
checkpoint by `pyramids_for()` — which rebuilds every column's pyramid from scratch
and allocates a full-length `to_f64_vec()` copy per column each time. `spawn_index_job`
forwards only `dataset` and `rows_read`; neither `Status::Loading` nor `Status::Loaded`
has anywhere to put a pyramid. On the row-count-doubling checkpoint schedule that is
O(n log n) of pure waste, and it is an active contributor to #75.

Note that `viewport_query.rs` benches `decimate_viewport` against its ceiling and
passes — but it benches a function the app never calls. A green performance gate there
says nothing about actual pan/zoom frame time, which nothing currently measures.

### G2 — The Level-0 / pyramid spill cache is dead code · issue #81

```
$ grep -rn "level0::\|pyramid::" crates/ --include=*.rs | sed 's/:.*//' | sort | uniq -c
      1 crates/glyde-core/src/index/mod.rs                        # re-export
      5 crates/glyde-core/src/index/pyramid.rs                     # intra-module
      6 crates/glyde-core/tests/level0_decimation_integration.rs
      6 crates/glyde-core/tests/pyramid_spill_integration.rs
```

Zero hits in `ingest/`, zero in `glyde-app/`. `index::level0::{build, build_or_open,
try_open}` and `index::pyramid::{build, build_or_open, try_open}` have no callers
outside `index/` and the two integration tests written for them. `CacheKey` — the
path+size+mtime reuse mechanism — never enters the open path.

This is not only a speed feature. SPEC §5.1 makes the spill part of the *memory*
contract ("the index is bounded and, if it would exceed budget, is spilled to a cache
file"). With the cache unwired there is no spill path at all in the running product.

### G3 — Raw `Dataset` memory is unbounded · issue #75 (already open, `blocking-decision`)

Independently confirmed. `Dataset` is `Vec`-backed for any file size; `ColumnText`
captures every field of every column while the mmap is live; and
`RamBudget::check_affordable` has exactly one caller in the entire workspace —
`glyde-devtools/src/bin/memory_gate.rs`, a *measurement* tool, not the read path.
SPEC §5.1's "checks affordability before acting, never after" is unimplemented in the
product. Measured at ~2.5× file size on a 1 GB fixture.

**This is the only gap that needs a maintainer decision rather than an implementation.**

### G4 — `YYYY-MM-DD HH:MM:SS` files do not open at all · issue #82

An M2 leak, not an M3 one, but it blocks M4 and is the most user-visible defect found.
SPEC §2.1 lists the formats absolute-timestamp detection *"must support at minimum"*,
and `YYYY-MM-DD HH:MM:SS[.fff…]` is one of them. Verified against the merge-base:

```csv
timestamp,temp
2026-01-01 00:00:00,1.5
2026-01-01 00:00:01,1.6
```
```
glyde_core::ingest::load(path)
→ Err(NonNumericTimeIndex { input: "2026-01-01 00:00:00" })
```

`TimestampFormat::DateTimeSpace` exists but `parse_timestamp` / `format_timestamp` are
`todo!()` and `IN_SCOPE_FORMATS` deliberately excludes it, so inference falls through
to the progressive-numeric branch, `parse::<f64>()` fails, and the load aborts. Two
distinct problems: the format is missing, *and* an unrecognized time column is a hard
load failure where SPEC §1.3 says "never abort the load".

It is also a landmine for M4's next unchecked item ("one-click correction of each
field"): a correction UI offering `DateTimeSpace` reaches a `todo!()` — a `panic!`
through normal interaction, against SPEC §1.3 and SPEC §6's crash-free target.

### G5 — The memory gate runs at 1/200th of spec scale · issue #83

`docs/QUALITY.md` §3 calls the 20 GB memory gate "the most important benchmark in the
suite". `.github/workflows/ci.yml:113` sets `GLYDE_BENCH_FIXTURE_GB: "0.1"`. At 100 MB
the gate cannot fail for any reason that matters — even the current ~2.5× multiplier
lands ~250 MB against a ~1.88 GB cap.

Issue #61 correctly decided CI should size the fixture by `--size-gb` rather than
literally 20 GB (runner disk is ~14 GB). But **0.1 is not the largest fixture that
fits** — the `index_build` bench in the same job already generates a 1 GB fixture of
its own. The value is almost certainly pinned by #75: a larger memory-gate fixture
would breach the cap and turn the job red. The one benchmark designed to catch "the
single most serious class of bug in this product" is disarmed by the bug it exists to
catch. Separately, QUALITY §3.2's ">15% vs `main`" regression gate is not implemented
in CI at all (a documented deferral, but it means the gate exists only on paper).

---

## 3. Roadmap

Ordered by dependency. Every item is one PR unless noted.

```
        ┌─────────────────────────────────────────────┐
  now   │ R1  #82a  DateTimeSpace parse/format        │  independent
        │ R2  #82b  never-abort on unknown time col   │  independent
        └─────────────────────────────────────────────┘
                          │
        ┌─────────────────▼───────────────────────────┐
 gate   │ R0  #75  MAINTAINER DECISION (A / B / A→B)  │  blocks R4, R5, R6
        └─────────────────┬───────────────────────────┘
                          │
        ┌─────────────────▼───────────────────────────┐
        │ R3  #80a  plumb pyramids into app state     │  can start before R0
        │ R4  #80b  decimated render + SPEC §3.1 rule │  shape depends on R0
        │ R5  #81   wire the spill cache into open()  │  shape depends on R0
        │ R6  #83   re-arm the memory gate            │  needs R0's fix landed
        └─────────────────┬───────────────────────────┘
                          │
                    M4 → M5 → …
```

### R0 · #75 — Decide the raw-`Dataset` memory strategy · **decided and landed**

The maintainer picked **Option B** (stream rows into bounded/spilled storage), not A and
not A-then-B, for the reason this section recommended: A is work that gets thrown away,
and R5 (#81) has to pick the same storage shape anyway.

**Landed.** `index::spill` is the one on-disk primitive; `ingest::dataset` runs the
SPEC §5.1 affordability check on a bounded head sample *before* reading and picks
between the untouched in-memory path and a bounded-chunk streaming path that writes each
typed column straight to disk. `docs/ARCHITECTURE.md` §"Where the raw `Dataset` lives"
records the full decision. Measured with `memory_gate` on the 8-column generated
fixture: a 2 GB file went from 7.16 GB peak RSS (3.34×, over the 4.21 GB cap) to
0.90 GB (0.42×); a 0.5 GB file still fits the budget and is unchanged.

**Residual, since closed (#85).** Peak RSS was ~0.42× file size, not flat: the sample
data was fully on disk, but `time::gap`'s `O(rows)` Δt temporaries plus the resident
tick pages SPEC §2.2's median-based statistics scanned still cost ~48 bytes/row.
`time::TickSource` (bounded, replayable chunk reads) plus an exact iterative-histogram
order statistic removed both. Measured with `memory_gate` on the same 8-column fixture:
**10.7 MB on a 2 GB file and 10.3 MB on an 8 GB one** — flat, and 0.4% of the cap
rather than 21% of it. R6 (#83) can now re-arm the memory gate at a large fixture size.

### R1 · #82a — Implement `DateTimeSpace`

`parse_naive_with_pattern` / `format_naive_with_pattern` already exist for
`DayFirst`/`MonthFirst`; the `[.fff…]` variant needs the same
`split_iso8601_fraction` / `parse_subnanosecond_iso8601` treatment the ISO paths have.
Add to `IN_SCOPE_FORMATS` ordered so it cannot shadow the ISO variants. Un-`#[ignore]`
`tests/golden/time.rs:109`. Add a torture-corpus case — QUALITY §1's time block
(cases 24–42) has no space-separated case today.

### R2 · #82b — A time column that matches no format must not abort the load

SPEC §1.3 says never abort; today one unparseable field in column 0 takes the whole
file down with no plot and no inference bar to correct from. Needs a product call
framed under fidelity > transparency > speed > convenience — likely a row-ordinal
fallback flagged loudly in the inference bar rather than `GlydeError::NonNumericTimeIndex`.
Flag the assumption in the PR description and `CHANGELOG.md` per CLAUDE.md.

Doing this *before* M4 also removes the `todo!()` panic landmine from M4's correction UI.

### R3 · #80a — Plumb pyramids from ingest to app state

Mechanical and decision-independent: carry `Checkpoint::pyramids` (and an equivalent
for the completed load) through `IndexingMessage` into `Status::Loading`/`Status::Loaded`.
Also stop rebuilding every pyramid from scratch at every checkpoint — build
incrementally, or at minimum drop the per-column `to_f64_vec()` copy per checkpoint.
Worth splitting from R4 so the wasteful rebuild is fixed early; it feeds #75.

### R4 · #80b — Decimated rendering

`views/time.rs` queries `decimate_viewport(range, pixels)` per numeric column per
frame instead of iterating raw samples, and the raw-samples-plus-markers path becomes
conditional on `samples < pixels` per SPEC §3.1. Add a frame-time gate (or a headless
viewport-render bench over the app's actual draw path) so SPEC §5's p50 ≤ 16 ms /
p99 ≤ 100 ms budget is enforced rather than assumed.

### R5 · #81 — Wire the spill cache into the open path

`try_open` on path+size+mtime at the top of `open()`, short-circuit the parse on a hit,
write the cache after a completed build. Whether the write goes through
`Level0CacheWriter::push` during ingestion (streaming — the Option B direction) or the
whole-slice `build()` after materialization is **R0's decision**, not an independent
one. Add an integration test that opens a file twice and asserts the second open takes
the cache-hit path, so "reopen is instant" is covered by something other than a stopwatch.

### R6 · #83 — Re-arm the memory gate

Raise `GLYDE_BENCH_FIXTURE_GB` to the largest size the runner's disk supports (order
4–8 GB, leaving headroom for the 1 GB `index_build` fixture in the same job), so the
gate exercises a file *larger* than the RAM cap. Decide QUALITY §3.2's fate: implement
the >15%-vs-`main` comparison somewhere it can be trusted, or amend QUALITY §3 to say
regression detection is a manual reference-machine ritual.

> ⚠️ **Both files are hard-denied to agent sessions** (`.claude/settings.json`:
> `Edit(.github/workflows/**)`, `Edit(docs/QUALITY.md)`). R6 must be delivered as a
> ready-to-paste diff in the issue for the maintainer to apply, exactly as #61 was.

### Also open, not on the M3 path

**#30** — the release workflow can publish unvalidated, mis-packaged binaries on tag.
Mostly M9 work, but the `v*` trigger is armed *now*. If a tag is at all likely before
M9, narrow the trigger to `workflow_dispatch` first. Also a denied file → maintainer-applied diff.

---

## 4. Model and effort per item

"Effort" is `output_config.effort` (`low` → `medium` → `high` → `xhigh` → `max`).
Picks are the **minimum** that should complete the task reliably, not the safest —
step up one level if a first attempt stalls.

| # | Item | Model | Effort | Why this is the floor |
|---|---|---|---|---|
| R0 | #75 decision | — | — | Maintainer call; no model involved — **decided: Option B** |
| R0-B | #75 Option B implementation | `claude-opus-5` | `xhigh` | Cross-module architecture change (~1,500 LOC across 5 files) that must keep every decimation golden and the corpus gate green. The one item where under-powering it costs more than it saves |
| R1 | #82a `DateTimeSpace` | `claude-sonnet-5` | `medium` | Well-scoped TDD: golden test already written, sibling helpers already exist. Not Haiku — the sub-nanosecond fractional path is exactly where a cheap model silently rounds (Golden Rule 1) |
| R2 | #82b never-abort fallback | `claude-opus-5` | `high` | Small diff, but it is a product decision under SPEC ambiguity plus a blast radius across every corpus case. Judgment, not volume |
| R3 | #80a plumb pyramids | `claude-sonnet-5` | `medium` | Mechanical struct/channel threading with an obvious shape; the incremental-rebuild fix is local |
| R4 | #80b decimated render | `claude-opus-5` | `high` | Per-frame render path against a hard latency budget, plus a new headless bench harness. Correctness here is what M3's maintainer test actually checks |
| R5 | #81 spill wiring | `claude-sonnet-5` | `high` | Mechanical *once R0 is decided*, but cache-key correctness (path+size+mtime) is a silent-wrong-data risk, so not `medium` |
| R6 | #83 fixture + QUALITY | `claude-haiku-4-5` | `low` | One env var, one comment, one doc paragraph — and the output is a pasteable diff, not a merge |
| — | #30 release workflow | `claude-sonnet-5` | `medium` | Routine CI/packaging work; also a pasteable diff |

Two cross-cutting notes:

- **Do not lower effort to shorten output.** On Opus 5 it does not reliably work, and on
  this repo the deliverable is tests + a `CHANGELOG.md` entry, which are cheap either way.
- **`claude-fable-5` is not warranted anywhere on this list.** Its edge is long-horizon
  autonomous work above what Opus 5 handles; every item here is a bounded single-PR
  change with a written test as the acceptance criterion.

---

## 5. Suggested ROADMAP.md amendment

M3's checkboxes describe `glyde-core` deliverables accurately but overstate what the
app can do. Rather than un-tick them, annotate — the work genuinely landed:

- "Decimation query `viewport(range, pixels)` …" → add *"(core only; UI wiring is #80)"*
- "Level-0 typed spill cache … reopen is instant" → add *"(core only; open-path wiring is #81)"*
- "Pyramid *level* spill" → same annotation
- "Background progressive build emitting partial levels" → add *"(pyramids are built per
  checkpoint but discarded by the app — #80; raw `Dataset` memory remains unbounded — #75)"*

#75 already asks for this reconciliation; #80 and #81 extend it to the decimation and
cache items.
