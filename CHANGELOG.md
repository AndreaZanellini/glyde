# Changelog

All notable changes to Glyde are documented here.

This file is written for the **maintainer and users, not for developers**: describe
what changes for someone using the app, in plain language. It is the primary way the
maintainer reviews work without reading code. Every PR adds a line here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed
- **Panning and zooming a large file is now smooth, and a one-sample spike
  never disappears.** The time-domain plot used to draw every single raw
  sample on every frame, however many rows the file had — fine for a small
  file, but a file with millions of rows meant redrawing millions of points
  every time you moved the mouse, which is why panning or zooming a big file
  could stutter or freeze. Glyde now draws one min/max bar per pixel column
  instead, recomputed from the exact data every frame as you pan and zoom
  (never a resampled or averaged approximation), so the amount of work stays
  tied to how wide your window is, not how many rows the file has. A
  one-sample spike still always shows up, at any zoom level, because the
  min/max bar always includes it even when many other samples are collapsed
  into the same pixel column; zoom in far enough and the view switches back
  to drawing individual points, one per sample, exactly as before.

  A gap (a run of missing/NaN readings) still shows up as a visible break in
  the plot rather than being smoothed over, same as always (`docs/SPEC.md`
  §1.3).

  Measured on a synthetic 8-million-sample file, one simulated pan/zoom
  frame: **4.75 seconds before → well under a tenth of a millisecond after**
  (a new automated benchmark now fails the build if this ever regresses).

  **Assumption made:** for a very large file that had to be streamed from
  disk instead of held in memory (`docs/SPEC.md` §5.1's affordability check),
  the fast, precomputed version of this speedup is not built for the parts of
  the file that arrived after the plot first appeared — Glyde still draws
  those correctly, just by scanning the visible range directly rather than
  from a cache. In practice this only matters on files large enough to spill
  to disk in the first place; a follow-up on making that instant too is
  tracked as its own issue (glyde-app issue #81).

- **Opening a big file now costs the same memory whatever its size — about
  10 MB.** The previous entry got a 2 GB file down from 7.16 GB of memory to
  0.90 GB by keeping the sample data on disk, but the figure was still a
  *fraction* of the file (0.42×), so it kept growing: a 10 GB file would have
  needed ~4.2 GB, right at the ceiling Glyde promises to stay under
  (`docs/SPEC.md` §5: `min(25% of your RAM, 4 GB)`), and over it on a smaller
  machine (issue #85).

  What was left growing was not your data — that is fully on disk — it was the
  handful of *statistics* Glyde works out about the time column every time it
  opens a file: the typical spacing between samples, where the gaps are, and
  whether the sampling is regular enough for a PSD. Those were computed by laying
  every gap between consecutive samples out in memory at once and sorting them.
  Glyde now reads the time column back a small piece at a time and works the same
  numbers out without ever holding them all, so the cost no longer depends on how
  many rows there are.

  Measured with the memory harness on the generated test fixtures (an ISO 8601
  timestamp column plus eight numeric columns), before and after, on the same
  files:

  | Fixture | Peak memory before | Peak memory after |
  |---|---|---|
  | 2 GB | 895 MB (0.42× the file) | **10.7 MB (0.005×)** |
  | 8 GB | not measured (0.42× predicts ~3.6 GB) | **10.3 MB (0.001×)** |

  A 2 GB file and an 8 GB file now cost the *same* memory to open — 10.7 MB and
  10.3 MB, a difference smaller than the run-to-run noise. That is what "flat"
  means, and it is what `SPEC.md` §5 asks for. The 4 GB ceiling is no longer
  anywhere near in play: a 20 GB file should open in the same ~10 MB.
  Opening is not slower: the same 2 GB file took 37.6 s before and 38.4 s after
  (within run-to-run noise), because the new method usually settles the answer in
  a couple of quick passes where the old one had to sort millions of values.

  **Nothing about the results changed.** The spacing, gap count, regular-vs-
  irregular verdict, and out-of-order/duplicate timestamp counts are the same
  numbers as before, not approximations of them: the new method finds the *exact*
  same middle value a full sort would have found, and every existing test —
  including the torture corpus's file-by-file expected results and the
  hand-checked timestamp tests — passes unchanged. A property test cross-checks
  the new method against a plain sort on 256 random series per run, and another
  checks that reading a series in tiny pieces gives the identical verdict to
  reading it whole.

  **Assumptions made:**
  - **Nothing in `SPEC.md` needed reinterpreting.** §2.2 defines its rules
    against the *median* spacing, and the median is the one statistic that cannot
    be computed in a single bounded pass. Rather than substitute a cheaper
    approximation (a running average, a sampled subset) and quietly change what
    the app reports, Glyde re-reads the time column a few times to get the
    genuine median. Fidelity over speed, per the golden rules.
  - **Where the extra reading happens.** Re-reading comes from the scratch files
    Glyde already wrote, or from memory for a file small enough to fit — never
    from re-parsing your original file, which is still read exactly once.
  - **A file with an implausible number of gaps.** Gap *counting* is now
    unbounded-file-safe, but the gap *list* (which a future gap view will draw)
    is still built in memory. A normal file has a handful of gaps; a
    deliberately pathological one could have millions. Noted as a backlog issue,
    not a change here.
- **Big files no longer eat memory in proportion to their size.** Before this
  change, opening a CSV cost roughly **3.3× the file's size in RAM** — so a
  2 GB file needed about 7 GB, which is past the memory ceiling Glyde promises
  to stay under (`docs/SPEC.md` §5: `min(25% of your RAM, 4 GB)`) and, on a
  smaller machine, past what the machine actually has. That is the freeze/crash
  risk `SPEC.md` calls the most serious class of bug in this product (issue
  #75).

  Glyde now works out, **before it reads a single row**, how much memory a file
  would need. If it fits the budget, nothing changes — the file is read exactly
  as fast as before. If it does not, Glyde reads the file in small pieces and
  writes each column straight to a scratch file on disk as it goes, instead of
  building the whole thing in memory first. The plot then reads from those
  scratch files.

  Measured on this branch, opening the generated test fixtures (an ISO 8601
  timestamp column plus eight numeric columns):

  | Fixture | Peak memory before | Peak memory after | Which path |
  |---|---|---|---|
  | 0.5 GB | 1.79 GB (3.34× the file) | 1.79 GB (unchanged) | in memory — it fits the budget |
  | 2 GB | 7.16 GB (3.34×) — **over the 4.21 GB ceiling** | **0.90 GB (0.42×)** | spilled to disk |

  So a 2 GB file went from needing 7 GB to needing 0.9 GB — an 8× reduction,
  and comfortably inside the ceiling instead of 70% past it.

  **What this does not yet do:** peak memory is smaller, but it still grows with
  file size (about 0.42× of it) rather than being completely flat. The remaining
  growth is not the sample data any more — that part is fully on disk now — it
  is the *statistics* Glyde computes about the time column when it opens a file
  (spacing, gaps, regular-vs-irregular sampling), which still look at every row
  at once. Extrapolating, a 10 GB file would need about 4.2 GB, which is right
  at the ceiling on a 16 GB machine and over it on a smaller one. That is
  tracked separately as issue #85; this change was scoped to the sample data.

  Nothing about the data itself changed: a file opened through the disk path
  gives byte-for-byte the same values, the same column types, and the same
  timestamps (including per-row time-zone offsets and preserved NaN gaps) as
  one opened in memory. A test opens the same file both ways and compares them
  field by field.

  **Assumptions made:**
  - **How the memory estimate is made.** Glyde predicts a file's cost from its
    first 1 MB — average row length, column count — and deliberately errs on the
    high side. Over-estimating sends a file down the (correct but slower) disk
    path unnecessarily; under-estimating risks the freeze this change exists to
    prevent, so the trade is one-sided. `SPEC.md` does not specify a formula.
  - **Scratch files are never cleaned up, and are not reused yet.** They live in
    the OS cache directory keyed by the file's path, size and modification time,
    so reopening the same file overwrites rather than accumulating — but a
    *different* big file adds a new set. Reading them back to make a reopen
    instant is issue #81; deleting old ones was already a tracked gap.
  - **A big file's plot fills in from a capped preview.** Both paths still draw a
    growing plot while a file loads. For a spilled file the growing plot is built
    from the first 200,000 rows kept in memory; past that it stops growing and
    the complete plot appears when the read finishes. (Drawing directly from the
    scratch files while they are still being written is not safe on all three
    OSes Glyde ships on, so the preview is capped instead — which also keeps its
    memory cost fixed at roughly 20 MB whatever the file's size.)
  - **If your machine has no usable cache directory**, a file too large for the
    budget is refused with a clear message rather than attempted anyway
    (`SPEC.md` §5.1: never start an action that would run the machine out of
    memory).
  - **Non-UTF-8 big files** (Latin-1, Windows-1252, UTF-16) are decoded in small
    pieces too, so they get the same benefit; the character-by-character result
    is identical to before, including how invalid bytes are replaced.

### Documentation
- **New: `docs/M3-CLOSEOUT.md` — an honest status report on Milestone 3, plus a
  plan to finish it.** No app behavior changes in this entry; it is analysis.

  The short version: **M3's engine is built and tested, but it is not connected
  to the app.** The min/max pyramid, the decimation query, the disk cache that
  should make reopening a big file instant, and the memory-budget module all
  exist in `glyde-core` and are covered by tests that pass. None of them are
  called by the actual Glyde window. So if you run the M3 maintainer test today:

  - "First plot within ~2 s while indexing continues" — **works.**
  - "Scroll and zoom for 30 s without stutter" — **will not.** Every frame still
    draws every single sample, and memory is still unbounded.
  - "A one-sample spike stays visible at every zoom level" — happens only by
    accident (because everything is drawn), not because decimation guarantees it.
  - "Zoom in until it converges to individual points" — there is nothing to
    converge *from* yet.
  - "Close and reopen the big file → instant" — **will not.** The cache is never
    written or read by the app; a reopen re-parses the whole file.

  Nothing here is broken or a regression — the integration step was simply never
  scheduled as its own item. Four new issues (#80, #81, #82, #83) now track it,
  and `docs/M3-CLOSEOUT.md` lays out the order to do them in.

  **One thing worth your attention right now, separate from M3:** a CSV whose
  timestamps look like `2026-01-01 00:00:00` (space between date and time — one
  of the most common exports there is) **does not open at all** today. It fails
  with an error instead of a plot. That is issue #82, it is an M2 gap that was
  missed, and it also blocks the "click to correct a field" work in M4.

  **The one decision waiting on you** is issue #75 (how to bound memory when
  reading a large file). Two of the four new issues depend on which way you go,
  so it is the first domino. `docs/M3-CLOSEOUT.md` §R0 recommends going straight
  for the real fix rather than the stopgap, and says why.

### Added
- The inference bar (the row above the plot showing what Glyde detected about
  a file — encoding, delimiter, decimal separator, time column, timestamp
  format, sample count, sampling class) is now collapsed by default instead
  of always taking up space, and opens itself automatically the moment any
  one of those detections is low-confidence — e.g. a file with a genuinely
  ambiguous date column. A clean, unambiguous file now shows a single
  discreet header line; nothing is hidden that needs your attention
  (`docs/SPEC.md` §1.2, `docs/ROADMAP.md` M4 "Inference bar widget"). One-click
  correction of an individual field is still a separate, later item.

  **Assumptions made:**
  - The collapsed/expanded state is remembered per file (keyed by its path),
    so opening a different low-confidence file always opens expanded even if
    you'd previously collapsed a different file's bar, and re-opening a file
    you'd manually expanded stays expanded. Nothing in `docs/SPEC.md` §1.2
    specifies this precisely; worth a veto if "one collapsed/expanded
    preference for the whole app" was intended instead.

- Internal groundwork, no visible app behavior change: reopening a large file
  no longer has to recompute its min/max zoom index ("pyramid") from the
  cached raw samples every time — the pyramid itself is now also written to
  a small cache file the first time a file is opened, keyed the same way as
  the raw-sample cache (source path + size + modification time), so a
  reopen reads the already-computed index straight back instead of
  redoing that work (`docs/ROADMAP.md` M3 "Pyramid level spill",
  `docs/ARCHITECTURE.md` §The index). This closes the last gap that section
  flagged as deferred. Nothing in `glyde-app` calls this yet — like the
  raw-sample cache before it, this lands the storage layer, proven by tests,
  not the wiring into the running app (that follow-up is a chunked/streaming
  large-file reader, tracked separately, `docs/ARCHITECTURE.md` §The index).

  **Assumptions made:**
  - This cache is a plain file read/written sequentially, not
    memory-mapped like the raw-sample cache: the pyramid query engine's
    locked signature takes an owned pyramid, not a borrowed cache view, so a
    hit here copies the already-computed bucket data back into memory rather
    than mapping it in place. The saving is skipping the recomputation (and
    not needing the raw-sample cache open at all to do it), not a zero-copy
    reopen. Worth a veto if a fully zero-copy pyramid reopen is expected
    instead.
  - Same known gap as the raw-sample cache before it: nothing prunes old
    cache files yet (no size cap, no eviction) — tracked as a pre-existing,
    not new, limitation.

- Opening a large file now shows a growing plot within a couple of seconds
  instead of a bare spinner for the whole open: the background indexer reads
  the file in progressively larger chunks and sends each chunk's data to the
  window as soon as it's ready, so the plot fills in — more rows, further out
  on the time axis — while the rest of the file keeps loading in the
  background. Nothing about the final result changes; this only affects how
  quickly *something* appears on screen for a file too big to open instantly.
  Small files (a few tens of thousands of rows or fewer) open exactly as
  before — they finish before the first progress update would even fire.
  (`docs/ROADMAP.md` M3 "Background progressive build emitting partial
  levels".)

  **Assumptions made:** the background build also computes each chunk's own
  min/max index ("pyramid") over the rows read so far, matching what the
  final, complete file's index would show for that same prefix — this is
  tested in `glyde-core`, but the on-screen plot itself still draws every raw
  point directly (as it has since M2) rather than the aggregated index, since
  no roadmap item yet asks the time view to switch to index-based rendering.
  Progress checkpoints are spaced at a doubling row-count schedule (20,000,
  40,000, 80,000, ...) rather than a fixed interval or a wall-clock timer, so
  that re-deriving each checkpoint's data never costs more in total than
  reading the file once — this keeps the ≤30s/10GB full-build budget intact
  even on a file with many checkpoints.

  This is also the first code path that runs real ingested integer columns
  through the min/max pyramid, which surfaced a pre-existing gap (review
  finding on this PR): a whole-number column value larger than about 9
  quadrillion (2⁵³, `f64`'s exact-integer limit) loses precision when
  converted for the pyramid, the same way it already silently does for the
  on-screen plot line (SPEC §1.4's mandatory flagging for this case,
  `docs/ROADMAP.md` M8, was never implemented for either). This PR adds a
  `warn`-level log line when that happens, so the loss is at least visible
  in the log file even though the inference-bar warning M8 also promises
  still doesn't exist — flagging this now rather than letting a second
  silent call site accumulate on top of the first.

- Resolves the `blocking-decision` issue (#61) on the CI "Performance gates"
  job, which previously passed unconditionally: `generate_fixtures` and
  `memory_gate` are real now (a synthetic CSV fixture writer and a headless
  peak-RSS harness), and the `index_build`/`viewport_query` `criterion`
  benches assert against the `docs/SPEC.md` §5 budgets and fail the build on
  breach, instead of being no-op stubs. No visible app behavior changes.

  **Decisions made (per the maintainer's comment on #61):**
  - The 20 GB memory-gate fixture QUALITY §3 describes does not fit a
    GitHub-hosted runner's 14 GB of SSD. CI now generates a smaller
    fixture instead (size set by `ci.yml`'s `$GLYDE_BENCH_FIXTURE_GB`); the
    true 20 GB run moves to the maintainer's manual QA ritual (QUALITY §5).
    The peak-RSS budget itself does not scale with file size, so the
    invariant it protects — a file must never be materialized whole in RAM —
    is still exercised at the smaller size.
  - CI enforces the absolute SPEC §5 ceilings only (build-blocking). The
    QUALITY §3.2 ">15% vs main" regression comparison is not computed in CI
    — shared, virtualized runners are too noisy for that threshold to mean
    anything — and is instead a manual `cargo bench` run on the SPEC §5
    reference machine (MacBook Air M1), where `criterion`'s own local
    baseline comparison already reports a regression with no extra tooling.
  - This PR delivers the code side of both decisions (`glyde-devtools`, the
    two benches). The `ci.yml` fixture-size change and the matching
    `docs/QUALITY.md` §3 wording are hard-denied files for an agent session
    to edit and are included as ready-to-paste diffs in the PR description
    for the maintainer to apply by hand, per `docs/ROUTINES.md`'s CI rule.
  - Two benched paths QUALITY §3 lists are still out of scope: Parquet index
    build (no Parquet `Reader` is registered in `ingest` yet) and cold start
    (an app-level, windowed metric with no headless harness in `glyde-core`).
    The `welch` bench stays a stub — `dsp::welch` itself is `todo!()` until
    `docs/ROADMAP.md` M5.

### Fixed
- Resolves the `blocking-decision` issue (#48) left by the torture-corpus
  gate's discovery that one file's answer key ("ragged rows", corpus case
  21) disagreed with the real sampling-classification statistic. Checked the
  arithmetic myself: after this file's two malformed rows are correctly
  skipped, the 3 surviving samples have a genuine 3x swing between
  consecutive intervals (1s, then 3s) — a real irregularity, not a formula
  artifact of the statistic. The sampling-classification code (already
  reviewed, and still correct on every other corpus case) is unchanged; the
  file's answer key is corrected from "uniform" to "irregular", and the
  torture-corpus gate now checks this file like every other one instead of
  skipping it.

### Added
- No visible app behavior change yet: resolves a `blocking-decision` issue
  (#60) on how a file's time axis maps into the min/max decimation pyramid
  from `docs/ROADMAP.md` M3. A file with a real calendar timestamp column
  and a file with a plain progressive numeric index (e.g. a bare row
  counter) are Glyde's two supported time-axis kinds; the pyramid needed one
  consistent way to represent both. Decided: a timestamp column's own
  internal representation is used as-is, and a progressive numeric column is
  converted using a fixed, documented scale factor chosen to preserve its
  precision — so zooming and panning aggregate by true position along the
  axis, not by row count, the same way for either kind of file. This lands
  the conversion, tested directly; it is not yet wired into the plot itself
  (that follow-up work is `docs/ROADMAP.md` M3's remaining pyramid items).

  **Assumptions made:**
  - The progressive-axis scale factor is invented (`docs/SPEC.md` §2.1 does
    not specify one): it is precise to nine fractional digits and exact for
    values well below ~10²⁹ in magnitude. No realistic progressive index
    (a row counter, a sample number, a simple elapsed-time float) is
    expected to exceed either limit; flagged here in case a real file
    proves otherwise.

### Fixed
- A timestamp column using a fractional (decimal) epoch value — e.g.
  `1770000000.5` seconds, or the millisecond/microsecond/nanosecond
  equivalents — is now recognized and read correctly, matching
  `docs/SPEC.md` §2.1's "integer or float" epoch formats. Previously only a
  whole-number epoch column opened; a file with even one fractional epoch
  value in its time column failed to open at all. Every fractional digit is
  preserved exactly (never rounded through a floating-point number), the
  same fidelity guarantee the LabVIEW-epoch and Excel-serial-date formats
  already have (issue #41).

  **Assumptions made:**
  - A fractional epoch value upconverts to nanosecond precision (or
    picosecond, for a fractional nanosecond value) internally, mirroring
    the precision ceiling every other textual timestamp format in this
    codebase already uses — not a new convention invented for this fix.
  - A field whose fraction is all zeros (e.g. `3850027200.0`) is treated the
    same as a bare integer for the purpose of telling a genuine epoch-seconds
    column apart from a LabVIEW-epoch column written with a trailing `.0` —
    torture-corpus case 34 is exactly this shape, and this is what already
    let it be told apart from epoch-seconds before this fix. A column
    written with a meaningless-but-nonzero fraction on every row (unlikely
    in practice) is now read as float epoch seconds rather than LabVIEW
    epoch; worth a veto if a real file's export tool does something like
    that.
  - No new torture-corpus case was added for this (the existing 56-case
    corpus is a fixed, numbered set); instead, dedicated unit tests exercise
    the fractional epoch parse/format round trip directly, the same way the
    LabVIEW/Excel fractional round trip already is one file above this in
    `crates/glyde-core/src/time/format.rs`.

  **Caught and fixed by maintainer review before merge:** a pre-1970
  (negative Unix epoch) timestamp with a fractional part parsed to the wrong
  instant — `"-100.25"` came out as -99.75s, and `"-0.5"` lost its sign
  entirely and came out *positive*, with nothing flagged (exactly the
  "wrong-but-silent" failure CLAUDE.md Golden Rule 2 forbids). The bug was
  reachable through ordinary automatic inference, not just a manual format
  override: a column of pre-1970 fractional epoch-seconds values would have
  been auto-detected and silently mis-parsed. The formatter had the mirror
  problem for negative values (its `div_euclid`/`rem_euclid` decomposition
  produced non-canonical text, e.g. -100.25s formatting back as "-101.75").
  Both now decompose sign and magnitude separately; new tests cover a
  negative fractional value, the `-0.x` sign-loss case specifically, and
  detection through inference.

### Added
- No visible app behavior change yet (this lands the rendering engine, not
  the plot itself — that follow-up wires it into `glyde-app`): the min/max
  decimation pyramid used to render any file, of any size, as it's zoomed
  and panned is implemented and locked by its golden tests
  (`docs/ROADMAP.md` M3, `docs/SPEC.md` §3.1). A one-sample spike stays
  visible no matter how far you zoom out; zooming in far enough always shows
  the true individual samples.

  This also resolves a `blocking-decision` issue (#59) on *where the raw
  samples a large file's viewport queries read from actually live*, since
  CSV data exists only as text and cannot be memory-mapped as typed numbers
  the way a Parquet file can. Decided: a typed cache of each opened file's
  decoded samples is written once, streaming, to a small cache folder on
  your machine (the OS-standard cache location, next to where Glyde already
  planned to keep its index); reopening a file you've opened before reads
  straight from that cache instead of re-reading the original file. This
  cache uses noticeably more disk space than the original file while it
  exists — the maintainer should weigh in via issue #59 if that trade-off
  needs revisiting before large real files are opened through it.

  **Assumptions made:**
  - Only the raw samples are cached on disk so far; the pyramid itself is
    still rebuilt (quickly, from the cache) each time a file is reopened
    rather than also being loaded from disk. Full "reopen is instant for
    everything" is tracked as a follow-up, not silently treated as done.
  - The cache is never cleaned up automatically yet (no size cap, no
    least-recently-used eviction) — it only grows. Flagged as a known gap,
    not a silent limitation.
  - This PR does not yet change how `glyde-app` reads a file end to end for
    files too large to fit in memory (that still needs a streaming CSV
    reader feeding the new cache row by row); it lands the storage layer and
    the rendering engine issue #59 was blocking, both proven by tests,
    ready for that follow-up to build on.

- Opening a file now shows an inference bar above the plot with everything
  Glyde inferred about it: encoding, delimiter, decimal separator, time
  column, timestamp format, sample count, and sampling classification
  (`docs/SPEC.md` §1.2's mandatory transparency rule, `docs/ROADMAP.md` M4).
  Any field Glyde could not resolve with full confidence — e.g. a genuinely
  ambiguous encoding guess, or a header row that could not be confidently
  located — is now labeled "(low confidence)" right next to it, instead of
  being presented the same as a confident inference.

  **Assumptions made:** `docs/SPEC.md` §1.2 says "confidence is tracked per
  inference" but does not define what makes an inference confident, so this
  PR picks concrete rules per field, each worth a veto if it doesn't match
  intent:
  - Encoding: a byte-order mark, or a sample that decodes cleanly as UTF-8, is
    "High"; only a real statistical guess among competing encodings (e.g. a
    genuine Windows-1252 file) is "Low".
  - Delimiter: "High" only if every sampled line agreed on the column count
    under the winning delimiter.
  - Decimal separator: "High" only if one separator (`.`/`,`) strictly
    outvoted the other in the sample; a tie, including no numeric evidence at
    all, is "Low".
  - Time column name: tied to whether the header row itself was confidently
    located.
  - Timestamp format: "Low" exactly when SPEC §2.1's day/month or
    epoch-seconds/LabVIEW-epoch ambiguity rule had to fall back to a default.
  - Sample count and sampling classification are reported facts, not guesses,
    so they carry no confidence label.

  This is the first of two `docs/ROADMAP.md` M4 items: the bar is currently
  always-expanded and read-only. A later item makes it collapse by default,
  auto-expand only when something is low-confidence, and offer one-click
  correction.

- No visible app behavior change: gap detection and sampling classification
  (used to decide whether a file's time index is `Uniform`,
  `SegmentedUniform`, or `Irregular`, `docs/SPEC.md` §2.2–2.3) now write to
  the log file, matching every other ingestion decision Glyde already logs.
  Previously these two decisions were silent, which meant support couldn't
  see them in a bug report's logs (issue #47).

  **Assumptions made:**
  - Logged at `info`, not `warn`: `CLAUDE.md` names "detected gaps" among the
    decisions logged at `info`, and a classification is a routine decision
    rather than a recoverable anomaly, so both new log lines use `info!`.

- Internal only, no visible app behavior change yet: Glyde now computes a RAM
  budget from the machine's total memory at the point it's asked for (`min(25%
  of system RAM, 4 GB)`, `docs/SPEC.md` §5's "Peak RSS" cap), and offers a
  reusable "does this fit?" check ahead of any single action. Nothing in the
  app calls it yet — the index build and PSD paths that will actually refuse
  an oversized action and offer an affordable alternative (`docs/SPEC.md`
  §5.1) are later `docs/ROADMAP.md` M3/M5 items. This lands the foundation
  those depend on (`docs/ROADMAP.md` M3 "RAM budget module").

  **Assumptions made:**
  - None beyond what M3's roadmap line already specifies — the `min(25% RAM,
    4 GB)` formula and "check before acting" behavior are taken verbatim from
    `docs/SPEC.md` §5/§5.1, not invented.

### Fixed
- Time-domain view: a series with a gap (a NaN run in the middle) now stays
  one consistent color across the gap instead of switching to a different
  color on the other side, which made a single series with a gap look like
  two different series (issue #55). Each series is assigned one color, shared
  by every line segment on either side of a gap and by that series' point
  markers.

  **Assumptions made:**
  - The assigned colors reuse `egui_plot`'s own default per-item palette
    formula (equally spaced hues via the golden ratio), but stepped once per
    series rather than once per draw call as `egui_plot` itself does — so
    this does *not* leave a gap-free series looking exactly as before: today,
    a series' line and its point markers already get two different
    auto-assigned colors, even with no gap. Every series' color shifts
    relative to before, and a series' line now finally matches its own point
    markers, which it never did previously.

- Time-domain view: the x-axis tick labels now show a formatted date/time
  (e.g. `2026-01-01T00:00:00Z`) instead of the raw seconds-since-epoch number
  (e.g. `1767225600`) for files with an absolute timestamp column. The
  cursor readout already formatted timestamps correctly; only the axis tick
  labels themselves were showing the raw number (issue #56). A progressive
  numeric index (no calendar meaning) still shows its plain number, matching
  prior behavior.

  **Assumptions made:**
  - None beyond what the issue already specified: the axis reuses the same
    `format_timestamp`/format the cursor readout already uses, so the two
    stay consistent. Each tick's displayed UTC offset is taken from the
    nearest real sample rather than always the file's first sample, since a
    source column can carry a different offset per row (e.g. a DST
    transition partway through the file) — this was caught in self-review
    before push, not part of the original issue's text.

- Opening a delimited-text file (`.csv`/`.tsv`/`.txt`) uses meaningfully
  less memory than before: measured on a synthetic fixture, peak memory use
  while opening a file dropped from ~12.75x the file's own size to ~7x
  (issue #62, the still-open third root cause of issue #58). The
  ingestion path used to capture every field of every row as its own
  small text allocation before deciding each column's type (numbers,
  booleans, text); it now appends each column's raw text into one shared
  buffer per column instead, with typing reading straight from that
  buffer rather than from a copy of it. No visible behavior change — the
  same values, same dtype decisions, same skipped-row handling as before,
  just materially less memory spent getting there. `docs/SPEC.md` §5's
  full memory budget is still not met (that needs `docs/ROADMAP.md` M3's
  chunked/bounded reader); this is a real step toward it, and the peak-RSS
  regression test's threshold has been tightened to lock in the
  improvement.

  **Assumptions made:**
  - This closes out issue #62 in full and, with it, issue #58 (whose
    remaining scope was exactly #62's). No product behavior changed, so
    nothing here is a decision the maintainer needs to veto — flagged only
    because CLAUDE.md asks every assumption to be named: the exact
    "how much better" number (~7x vs. the ~12.75x baseline) is a
    measurement of one synthetic fixture shape (400,000 rows, three
    numeric columns), not a guarantee for every file shape.

- Opening a file uses noticeably less memory and CPU time than before,
  especially for a clean UTF-8 file (the common case): the ingestion path
  no longer needlessly duplicates the whole file into memory during
  decoding, and opening a file no longer re-reads and re-parses it twice in
  a row (issue #58). Peak memory use while opening a file is still well
  above what `docs/SPEC.md` §5's budget ultimately requires — the biggest
  remaining cost, capturing every field as its own text allocation before
  it is typed, is tracked in a follow-up issue for `docs/ROADMAP.md` M3 to
  finish. A new (Linux-only, for now) test asserts peak memory use while
  opening a file stays within a generous bound of the file's own size, so a
  regression back toward "many full copies of the file" is caught in CI
  ahead of M3's proper memory-budget gate.

  **Assumptions made:**
  - Issue #58 named five sub-tasks; this PR completes two of them
    (bounding the decode copy, and collapsing the app's two full parses
    into one) plus a coarse version of a third (a peak-RSS test, item 5).
    The remaining, larger structural change — replacing the per-field
    `Vec<String>` capture and the separate typed-`Series` build with
    bounded/streaming accumulation — is filed as its own
    `blocking-autonomous` follow-up (#62) rather than folded into this PR,
    per CLAUDE.md's "scope is sacred" and "one item per PR": it is a
    bigger, riskier change that deserves its own review and its own golden
    tests, not a drive-by addition here.

### Changed
- Roadmap bookkeeping only, no app behavior change: ticked the "Time-domain
  view v1" box in `docs/ROADMAP.md`'s M2 milestone. It was left unticked
  when the feature landed because this is a headless container with no
  display server, so the manual click-through the milestone's own
  "proven by: manual" note requires couldn't be run in that session. It has
  since been run by hand (pan, wheel-zoom, box-zoom via right-click-drag,
  fit-to-data, cursor readout, the European-locale file, the
  metadata-preamble file, and a NaN-run file all behave as specified) —
  see the manual test report on issue #54. M2 is now fully ticked. Two
  small, non-blocking follow-ups were filed from that pass rather than
  fixed inline: NaN-split line segments render in inconsistent colors per
  series (#55), and the x-axis ticks show raw epoch numbers instead of a
  formatted date/time, unlike the cursor readout which already formats
  correctly (#56).

### Added
- Opening a file now shows an actual plot, not just a text summary. Every
  numeric column renders as a line with visible sample markers on a shared
  time axis, and you can pan by dragging, zoom with the scroll wheel or a
  box-select, and click "Fit to data" to snap back to the full range.
  Hovering the plot shows a readout of the exact timestamp and every
  series' exact raw value at the nearest sample — never a rounded or
  decimated approximation (`docs/SPEC.md` §4.1, `docs/ROADMAP.md` M2
  "Time-domain view v1"). This is the small-file, pre-pyramid version of the
  view: every sample in the file is loaded and drawn directly, with no
  decimation yet — that (and the performance work needed for large files)
  is `docs/ROADMAP.md` M3.
  European-locale files (semicolon-delimited, comma-decimal, e.g. `1,5`)
  now plot correctly instead of the value column silently falling back to
  text: decimal-separator normalization, previously inferred but not
  actually applied to column values, is now wired into the file-loading
  path. A run of `NaN`/missing samples (`docs/SPEC.md` §1.3) now always
  shows as a visible gap in the line — the line is split at every `NaN`
  sample rather than relying on the plotting library to do that on its own,
  which review found it does not.

  **Assumptions made:**
  - As with the M2 "single egui window" item, this is a headless container
    with no display server, so the plot could not be clicked through by
    hand in this session. Unlike that item, the actual rendering code path
    (not just the data plumbing behind it) could still be exercised
    automatically: a new test drives the real plotting widget through a
    headless `egui::Context` (no GPU/display needed for that, only for
    `eframe`'s windowing shell) and checks it produces real draw output
    without panicking, snapshotted with `insta`. The roadmap item's
    checkbox is left unticked, matching the prior item's precedent, until
    someone runs the manual click-through this milestone's "proven by:
    manual" note also asks for.
  - A file whose time-index column matches no known timestamp format and
    also isn't a plain number (so it can neither be read as an absolute
    timestamp nor as SPEC §2.1's progressive numeric index) now fails to
    open with a clear message instead of silently opening with an empty
    plot. No corpus case exercises this today; it was previously
    unreachable because nothing actually parsed a progressive index's
    values.
- Glyde now has an actual window. Launching the app opens a single window
  (`docs/SPEC.md` §6) with a "File → Open…" menu and support for dragging a
  file straight onto the window. Either way, opening runs entirely on a
  background thread — the window keeps redrawing and never freezes while a
  file is being read, even before there's a plot to show for it
  (`docs/ARCHITECTURE.md` §Threading model). Once a file finishes opening,
  the window shows its path, row count, any skipped-row count, and its
  detected sampling class as plain text. Opening a file the engine doesn't
  recognize (e.g. a `.parquet` file — not supported until `docs/ROADMAP.md`
  M7) or a file that fails to open shows a clear message instead of doing
  nothing or crashing. Opening a second file while an earlier one is still
  being read (e.g. a quick drag-drop right after another) correctly shows
  the second file once it's ready — the first file's result can no longer
  arrive late and silently replace what's on screen, even though both were
  reading in the background at once. There is no time plot yet — rendering
  the data itself is the next `docs/ROADMAP.md` M2 item; this is only the
  window, the File→Open/drag-drop entry points, and the background-thread
  plumbing connecting them to the engine.

  **Assumptions made:**
  - This is a headless container with no display server, so the window
    itself could not be visually verified by launching it in this session —
    only the background-thread plumbing (`crates/glyde-app/src/plumbing`)
    was exercised by automated tests against real torture-corpus files. The
    roadmap item's checkbox is deliberately left unticked until someone runs
    the manual click-through (File→Open, drag-drop, and opening a file the
    engine doesn't recognize) on a real desktop per this milestone's own
    "proven by: manual" note, so a green roadmap line never implies more
    than CI actually proved.
  - The native "Open…" file dialog (`rfd`) has no headless test backend, so
    it is exercised only by code review and the manual check above, not by
    an automated test.

### Added
- Internal groundwork: the engine can now actually open a delimited-text file
  end to end and report what it inferred — encoding, delimiter, decimal
  separator, which column is the time index, its timestamp format, row
  counts, sampling classification, gap count, and non-monotonic/duplicate
  timestamp counts — instead of each of those living only in its own
  disconnected function as before. This is the "activate the torture-corpus
  gate" milestone item (`docs/ROADMAP.md` M2): every real-world-shaped test
  file in the corpus is now opened for real and checked against its answer
  key on every test run, not just checked for having a well-formed answer
  key. 51 of the 52 non-Parquet corpus files (Parquet itself is a later
  milestone, `docs/ROADMAP.md` M7, and isn't opened by anything yet) now open
  and match their expected result exactly. A file with only one column (just
  a time index, no data to plot) is now also rejected with a clear message
  instead of silently "succeeding" with nothing to show, per
  `docs/QUALITY.md` §1.18. There is nothing to see in the app yet — no
  window calls into this yet (that's the next `docs/ROADMAP.md` M2 item) —
  but this is the first time all of this milestone's ingestion work has been
  proven to work together on a real file rather than only in isolation.

  **Assumptions made:**
  - The time index is always assumed to be the first column. Every one of
    the 56 torture-corpus files puts it there, and none exercises a file
    where it sits elsewhere, so searching among several candidate columns
    isn't implemented; worth a veto if a real file ever puts its time column
    somewhere else.
  - One corpus file (case 21, "ragged rows") turned up a genuine
    disagreement between two pieces of already-existing, already-tested code
    that had simply never been run against each other before now: the
    sampling-classification statistic from an earlier PR calls this file's
    surviving 3 rows "irregular", while its answer key (written even earlier)
    says "uniform". Root cause and options are written up in issue #48 for a
    decision — I did not change either the algorithm or the answer key to
    make them agree, and the new gate explicitly skips this one file with a
    comment pointing at the issue until that's decided.

### Added
- Internal groundwork: for a timestamp column, the engine now detects rows
  that are out of order (a timestamp earlier than the one before it) and
  rows that repeat the exact same timestamp as the one before them
  (`docs/SPEC.md` §2.1). Both are only counted and reported, never acted on:
  an out-of-order row is **not** silently sorted, and a repeated timestamp is
  **not** silently dropped or merged — the eventual inference bar
  (`docs/ROADMAP.md` M4) is what will offer a one-click "Sort" / "Keep as-is"
  choice once it exists. Proven against the two relevant torture-corpus cases
  (one hand-inserted out-of-order row, one hand-inserted exact repeat) plus a
  hand-computed golden test. There is nothing to see in the app yet — this
  plugs into the CSV reader once the rest of `docs/ROADMAP.md` M2 lands.

  **Assumptions made:**
  - `docs/SPEC.md` §2.1 doesn't define "duplicate timestamp" precisely. This
    counts an *immediately consecutive* exact repeat (`timestamps[i] ==
    timestamps[i-1]`), matching how the corpus case for it is shaped and how
    the existing sampling-classification code already reasons about
    duplicate Δt. A value that reappears later in the series without being
    adjacent to its earlier occurrence is not counted as a duplicate today;
    worth a veto if the intended meaning is "appears more than once
    anywhere," not just "repeats its immediate predecessor."
  - The corpus `.expected.json` schema gained two new fields
    (`non_monotonic_count`, `duplicate_timestamp_count`), defaulted to 0 so
    the 54 existing corpus cases unrelated to this check did not need to be
    individually edited — only the two cases this item is proven against
    (36, 37) set them explicitly.
- Internal groundwork: for a timestamp column, the engine now classifies the
  sampling pattern per `docs/SPEC.md` §2.2 — `Uniform` (evenly spaced, full
  signal processing available later), `SegmentedUniform` (evenly spaced in
  bursts separated by gaps, e.g. a logger that pauses between measurement
  windows), or `Irregular` (neither, e.g. an event log). This also lands gap
  detection itself (`docs/SPEC.md` §2.2–2.3: a gap is a spacing more than 10×
  the typical spacing), which the eventual gap view and PSD (`docs/ROADMAP.md`
  M5/M8) will build on. Proven against the three relevant torture-corpus
  cases (a uniform series with realistic millisecond jitter, an irregular
  event log, and a three-burst segmented series) plus a hand-computed
  golden test. There is nothing to see in the app yet — this plugs into the
  CSV reader once the rest of `docs/ROADMAP.md` M2 lands.

  **Assumptions made:**
  - `docs/SPEC.md` §2.2 defines "uniform" as "jitter (robust CV of Δt) ≤ 1% of
    median Δt" but does not name the robust-CV formula. This uses median
    absolute deviation divided by the median (a standard robust dispersion
    measure, and the same "robust MAD-based rule" language `docs/SPEC.md`
    §1.3 already uses for outlier detection) — worth a veto if a different
    formula was intended.
  - A single-sample segment (the tail end of a series right after a gap) and
    a series with fewer than two samples have no spacing to judge jitter
    against, so both are treated as vacuously uniform rather than flagged.

- Internal groundwork: the engine can now also read two more absolute-timestamp
  formats named in `docs/SPEC.md` §2.1 — Excel serial dates (days since
  1899-12-30, e.g. `46023`) and the LabVIEW/NI epoch (seconds since
  1904-01-01, e.g. `3850027200.0`) — and correctly tells both apart from a
  plain epoch-seconds column and from each other by their typical magnitude,
  the same way the four Unix epoch resolutions are already told apart.
  Separately, a timestamp column with more than nanosecond precision (a
  picosecond-resolution log, `docs/SPEC.md` §2.1's "sub-nanosecond sources")
  is now read with every picosecond digit intact instead of being silently
  rounded down to the nearest nanosecond, which is what would otherwise
  happen with off-the-shelf date parsing. Proven against the four relevant
  torture-corpus cases: Excel serial dates, LabVIEW epoch, a
  picosecond-resolution index, and a plain multi-year series (a scale check
  that nothing above breaks ordinary multi-year files). There is nothing to
  see in the app yet — this plugs into the CSV reader once the rest of
  `docs/ROADMAP.md` M2 lands.

  **Assumptions made:**
  - Excel serial dates and LabVIEW epoch values can carry a fractional part
    (a time of day for Excel, sub-second precision for LabVIEW) per
    `docs/SPEC.md` §2.1, but no torture-corpus fixture uses one — every
    value in cases 33/34 is a whole number. Fractional support is
    implemented and covered by two hand-written tests (not a corpus case),
    computed with exact whole-number arithmetic rather than `f64`, so a
    fractional value is never silently mis-scaled; worth a veto if a real
    file's fractional values expose a rounding case these two tests don't.
  - Telling Excel serial dates and the LabVIEW epoch apart from the four
    Unix epoch resolutions (and from a plain progressive-index column) uses
    the same "plausible modern-era magnitude" heuristic already in place for
    the four epoch formats, shifted into each format's own epoch and unit.

    **Caught and fixed by maintainer review before merge:** a whole-number
    LabVIEW-epoch column (no trailing `.0` — an entirely ordinary shape for a
    real LabVIEW/NI export) fell inside the same plausible-magnitude window
    as plain epoch seconds, and epoch seconds was checked first — so such a
    column would have been read as a Unix timestamp roughly 66 years off,
    confidently and with nothing flagged. It's now read the same way the
    `DD/MM` vs `MM/DD` date ambiguity already is: still reported as epoch
    seconds (by far the more common real-world format at that magnitude),
    but marked low-confidence rather than asserted outright, so the
    upcoming inference bar (`docs/ROADMAP.md` M4) can surface it as a
    one-click-correctable guess.
    This isn't written down in `docs/SPEC.md`; flag if a real file's values
    could plausibly fall outside that window.

- Internal groundwork: when a timestamp column uses ambiguous slash-separated
  dates (`25/01/2026` or `01/25/2026`), the engine now works out on its own
  whether the file means day-first (`DD/MM`) or month-first (`MM/DD`), the
  same way a person would: it looks for any date in the column where one of
  the two number fields is bigger than 12 — since no month can be 13 or
  higher, that settles which field is the day and which is the month
  (`docs/SPEC.md` §2.1). If every date in the file is ambiguous even after
  checking (e.g. every row is in the first twelve days of the month), it
  never silently guesses: it defaults to day-first and marks the inference
  as low-confidence, so the upcoming "inference bar" (`docs/ROADMAP.md` M4)
  can show it as a one-click-correctable guess rather than a hidden
  assumption. Proven against the three relevant torture-corpus cases: an
  unambiguous day-first file, an unambiguous month-first file, and a fully
  ambiguous one. There is nothing to see in the app yet — this plugs into
  the CSV reader once the rest of `docs/ROADMAP.md` M2 lands.

### Added
- Internal groundwork: the engine can now read and re-write absolute
  timestamps in the most common formats (`docs/SPEC.md` §2.1) — ISO 8601
  with a timezone offset (`2026-07-22T14:30:00+02:00`) or without one
  (honored as naive local time), and Unix epoch counters in seconds,
  milliseconds, microseconds, or nanoseconds. It also correctly tells a real
  timestamp column apart from a plain row counter: a small column like
  `0, 1, 2, 3, 4, 5` is recognized as having no absolute-time meaning
  (`docs/SPEC.md`'s "progressive numeric" index) rather than being
  misread as a nonsensical timestamp a few seconds after 1970-01-01.
  Proven against the six relevant torture-corpus cases: ISO 8601 with and
  without a timezone, all four epoch resolutions, and the progressive
  row-counter file. There is nothing to see in the app yet — this plugs
  into the CSV reader once the remaining `docs/ROADMAP.md` M2 time-index
  items (Excel/LabVIEW formats and sampling classification) land alongside
  it.

  **Assumptions made:**
  - `docs/SPEC.md` §2.1 says epoch values may be "integer or float"; only
    integer epoch text is implemented here. A float epoch string like
    `"1770000000.5"` has more than one valid spelling for the same instant
    (`"1770000000.500"` means the same thing), which doesn't fit cleanly
    into an exact-round-trip guarantee, and no torture-corpus case exercises
    it yet. Tracked as a follow-up (`backlog` issue) rather than guessed at
    here.
  - Telling epoch seconds/milliseconds/microseconds/nanoseconds apart — and
    telling all four apart from a plain index column — uses a magnitude
    (digit-count) heuristic: a value only counts as a plausible epoch
    timestamp if it falls within roughly the year 2001–2286 range at that
    resolution. This isn't written down anywhere in `docs/SPEC.md`; it's the
    smallest rule that both resolves the four epoch formats correctly and
    keeps a small index-like column (torture-corpus case 35) from being
    misread as an absolute timestamp. Worth a veto if a real file's epoch
    values could plausibly fall outside that window.
  - An ISO 8601 timestamp's timezone offset (e.g. `+02:00`) is now kept
    alongside the parsed instant purely so it can be redisplayed exactly as
    written (`docs/SPEC.md` §2.1: "honor it and display it") — comparisons
    and future signal-processing always use the correctly offset-adjusted
    absolute instant, never the raw offset, so this cannot affect anything
    but on-screen formatting.

### Added
- Internal groundwork: when the dtype engine reads a column of only `0`s and
  `1`s as a boolean flag rather than a whole-number column, it now flags that
  specific call as low-confidence, matching how every other automatic guess
  (encoding, delimiter, header, decimal separator) already carries a
  confidence signal (`docs/SPEC.md` §1.2). Nothing else about dtype
  inference changes — explicit `true`/`false` spellings are still read as
  confidently boolean, and whole numbers, decimals, and free text are
  unaffected. There is nothing to see in the app yet: this is a data-shape
  change to an internal function that isn't wired into the CSV reader until
  the rest of `docs/ROADMAP.md` M2 lands, so no existing behavior can
  regress from it. (Issue #37.)

- Internal groundwork: the engine can now work out each column's data type
  from the plain text a delimited file actually contains (`docs/SPEC.md`
  §1.4) — whole numbers, decimals, `true`/`false` (spelled as `true`/`false`,
  `0`/`1`, or `TRUE`/`FALSE`), or free text — one column at a time. A run of
  missing (`NaN`) readings is kept as real values and flagged as a single
  gap rather than three separate ones; `Infinity`/`-Infinity` are accepted as
  ordinary numbers, not treated as broken data; and a column with even one
  stray non-numeric entry (a sensor's `"ERR"`/`"OK"` reading mixed in with
  its normal numbers) is kept as plain text in its entirety — the numbers in
  it are never silently parsed out while the odd entries are quietly
  dropped. Proven against the five relevant torture-corpus cases: a run of
  missing values, infinities, a mixed numeric/text column, a boolean column
  spelled three different ways, and a plain text "machine state" column.
  There is nothing to see in the app yet — this plugs into the CSV reader
  once the rest of `docs/ROADMAP.md` M2 lands.

  **Assumption made:** whole-number columns always come out as the widest
  signed integer type (`i64`) and decimal columns always as the widest
  floating-point type (`f64`), never a narrower width. A delimited text file
  has no dtype of its own to preserve the way a typed Parquet column will
  (`docs/ROADMAP.md` M7) — there's only a dtype to infer — so defaulting to
  the widest lossless type is the safe choice until a narrower one is
  actually needed. Also: a column containing only `0`s and `1`s reads as
  boolean rather than as whole numbers, matching torture-corpus case 47's
  `flag_numeric` column; flag if a genuinely numeric 0/1 column should be
  told apart from a boolean one before more of M2 depends on this rule.

- Internal groundwork: the engine can now actually read a delimited text
  file end to end in one streaming pass over the file's bytes, not just
  sniff its shape (`docs/SPEC.md` §1.3, `docs/ARCHITECTURE.md` §CSV). Rows
  whose field count doesn't match the header — a line missing a trailing
  value, an extra stray field — are salvaged out one by one, counted, and
  logged, rather than aborting the whole file; a file that's still being
  written (a truncated last line) opens with everything up to that point
  and the incomplete line quietly dropped; a genuinely empty file is
  reported with a clear "no data to read" message instead of a crash or a
  bogus empty plot. Proven against the 4 relevant torture-corpus cases:
  trailing blank lines and trailing delimiters, ragged rows, a truncated
  final line, and an empty file. There is nothing to see in the app yet —
  this plugs into the full ingestion pipeline once the rest of
  `docs/ROADMAP.md` M2 lands.

  **Assumption made** (flagged by maintainer review before merge, fixed in
  this PR): a file's inferred header/delimiter sample was being cut at a
  raw byte offset, which could panic on perfectly ordinary UTF-8 (a `°C`/
  `µm/s²`-style unit character straddling the cutoff) — now cut at the
  nearest valid character boundary, with a regression test proving it.
  Separately, this reader only tallies row counts; it deliberately does
  not accumulate parsed row data yet, because `docs/SPEC.md` §5.1 ("the
  full file is never loaded") isn't enforceable until `docs/ROADMAP.md`
  M3's RAM-budget module and bounded/chunked reading land — an early
  `Vec` of every row would have baked an unbounded shape into the first
  public consumer of this reader. The file's decoded text itself is still
  held in memory for this milestone's small/clean-file scope; genuine
  bounded reading of multi-GB files is M3's job, not this item's.

- Internal groundwork: the engine can now work out a text file's column
  delimiter (comma, semicolon, tab, pipe, or aligned whitespace), where its
  header row is, and whether its decimals use a `.` or a `,` (`docs/SPEC.md`
  §1.2.2-1.2.4). The three are resolved together, not one at a time: a file
  like `timestamp;value;pressure` / `...;1,5;101,3` is correctly read as
  three semicolon-separated columns with comma decimals, never mis-split
  into extra columns by treating every comma as a separator. A metadata
  preamble of junk lines above the real header is skipped without being
  mistaken for data, and a file with no header at all still gets usable
  column names. Proven against the 9 relevant torture-corpus cases: clean
  comma, the semicolon/comma-decimal trap, tab, space-aligned, pipe, a
  quoted field containing the delimiter, a quoted field containing an
  embedded newline, a 5-line metadata preamble, and a headerless file.
  There is nothing to see in the app yet — like the encoding inference
  before it, this plugs into the CSV reader once the rest of
  `docs/ROADMAP.md` M2 lands.

  **Assumption made** (SPEC.md doesn't fully specify tie-breaking): when two
  delimiter candidates are equally consistent (e.g. a tab-delimited file
  reads just as consistently as generic whitespace), the more specific
  delimiter wins over the generic whitespace fallback. Flagging this so it
  can be vetoed by testing a real file where this matters.

  Following review: header detection now correctly picks the row closest to
  the data when more than one leading line could pass as a header (e.g. a
  units row directly under a label row), per SPEC §1.2.3's wording; and the
  delimiter, decimal-separator, and header results now carry their
  confidence signal (consistency ratio, vote counts, an "ambiguous" flag)
  instead of discarding it, so the inference bar can use it once it's wired
  up (`docs/SPEC.md` §1.2's confidence requirement).

- Internal groundwork: the engine can now work out what character encoding a
  text file uses (`docs/SPEC.md` §1.2.1) — a byte-order mark if the file has
  one, and otherwise a statistical guess between plain UTF-8 and
  Windows-1252 (which also covers Latin-1, since the standard the encoding
  library follows treats the two as identical). A stray corrupted byte in an
  otherwise-clean UTF-8 file is replaced with the standard "unknown
  character" mark instead of misdetecting the whole file's encoding, and
  every replacement is logged rather than silently dropped, per
  `CLAUDE.md`'s "never guess silently" rule. Proven against the five
  encoding torture-corpus cases: a Latin-1-flavored header with `°C`/`µm/s²`,
  Windows-1252 smart quotes, a UTF-8 file with a leading byte-order mark, a
  full UTF-16LE file, and a file with one corrupted byte spliced into an
  otherwise-valid field. There is nothing to see in the app yet — this
  plugs into the CSV reader once the rest of `docs/ROADMAP.md` M2 lands.

### Fixed
- A hole in the project's own automated performance checks (nothing visible in
  the app). The CI "Performance gates" job was reporting success on every run
  while actually running no benchmark at all: the command that runs them was
  failing on a wrong option, but that failure was hidden by the way the command
  was piped into a log file, so the job stayed green regardless. This meant the
  performance and memory budgets `docs/SPEC.md` §5 promises were not being
  enforced even though the CI dashboard showed them as passing — the exact
  "green but never actually checked" failure mode the maintainer relies on CI
  never to hide. The job now runs the benchmarks honestly, and every CI step now
  fails loudly instead of swallowing an error hidden inside a pipe, so once the
  real benchmarks land (`docs/ROADMAP.md` M3) a measurement that exceeds a
  budget will fail the build as intended. (Fixes #29.)

### Added
- Four more golden tests locking in the future time axis (docs/QUALITY.md §2
  Time): every timestamp format Glyde must recognize (ISO 8601 with and
  without a timezone, `DD/MM`/`MM/DD`, epoch seconds through nanoseconds,
  LabVIEW epoch, Excel serial dates) will have to parse and then format back
  to the identical string; a multi-year timestamp span keeps its exact
  nanosecond and picosecond digits instead of the subtle rounding an `f64`
  seconds representation would introduce; and gap detection between samples
  is checked against a hand-computed synthetic series. One of the four
  already runs and passes today — it only proves the timestamp type itself
  can hold picosecond precision exactly — the other three wait on the actual
  time-parsing engine (`docs/ROADMAP.md` M2/M8). Also added the scaffolding
  for two new randomized ("property") tests that will run thousands of
  random inputs instead of fixed examples once their engines exist: one
  proving the zoom/pan rendering can never show a value outside a series'
  own min/max range, the other proving the CSV reader never crashes no
  matter what bytes are thrown at it. Nothing visible in the app yet — both
  are scaffolding for milestones still to come.

### Added
- Internal groundwork: the engine's data model for a single ingested column
  (`Series`). It records a column's values in their original data type —
  boolean, every integer width, `f32`/`f64`, or text — without ever
  converting one type into another, plus a container for anomalies flagged
  against it (missing-value runs, outliers, skipped rows) that nothing yet
  populates. Two rules from `docs/SPEC.md` §1.4 are enforced here already: a
  constant or single-sample column is a valid, ordinary input (not a special
  case to reject), and boolean/text columns know they must never be drawn as
  a numeric line plot — they report that they belong on the future state
  timeline instead. Proven against two torture-corpus shapes: the constant
  series (case 51) and the boolean column shown three different ways in the
  source text (case 47, `true`/`false`, `0`/`1`, `TRUE`/`FALSE`) each parse
  into the same `Bool` model and route to the state timeline, regardless of
  source spelling. There is nothing to see in the app yet — no reader
  populates a `Series` until the rest of `docs/ROADMAP.md` M2 lands; this is
  only the shape the CSV/Parquet readers will build.

### Added
- The eight golden tests for the future PSD (Welch) view, covering every
  guarantee `docs/QUALITY.md` §2 Welch requires: a known sinusoid's peak
  landing at its exact frequency bin with the right recovered amplitude,
  total power summing back to a white-noise signal's known variance (the
  test that catches window-normalization bugs), DC and Nyquist bins never
  being doubled while every other bin is, three different windows
  (rectangular/Hann/Hamming) reporting the same total power for the same
  signal, a large DC offset's spectral leakage disappearing once detrending
  is applied, two simultaneous tones resolving at their correct frequencies
  and relative levels, a 3-segment signal's PSD matching the length-weighted
  average of its per-segment spectra with no window ever crossing a gap
  (proven by injecting a discontinuity at each gap that a leaking window
  would show up as broadband noise), and a "zoomed in" vs. "zoomed out"
  selection of the same raw samples producing bit-identical results. There
  is nothing to see in the app yet — the Welch engine these tests grade is
  `docs/ROADMAP.md` M5, still to come — but the eight tests are committed
  now (skipped for the moment) so that milestone can't ship without
  satisfying every one of them.
- The five golden tests for the future zoom/pan rendering path (decimation),
  covering the guarantees `docs/QUALITY.md` §2 requires: a single-sample
  spike never disappearing however far you zoom out, the rendered min/max
  envelope matching a brute-force scan exactly, each zoom level being an
  exact aggregation of the level below it, zooming in far enough always
  reaching the true individual samples, and a 1 kHz sine wave rendering as a
  clean constant-amplitude band instead of a beat/aliasing pattern. There is
  nothing to see in the app yet — the pyramid/decimation engine these tests
  grade is `docs/ROADMAP.md` M3, still to come — but the five tests are
  committed now (skipped for the moment) so that milestone can't ship
  without satisfying every one of them.
- The final four real-world-shaped test files for the torture corpus
  (`testdata/corpus/`), covering Parquet: a clean Parquet file with a native
  timestamp column, a Parquet file whose value column has nulls, a
  Hive-style partitioned Parquet directory (two date partitions with a gap
  between them), and a Parquet file with a dictionary-encoded (categorical)
  string column, mirroring the string-state case in the CSV set. Each ships
  with a `.expected.json` answer key, same as every other corpus case. This
  is the last of the 56 planned corpus cases (`docs/QUALITY.md` §1) — the
  `corpus_has_all_56_cases` milestone-complete test, committed already but
  skipped until now, is un-ignored in this PR and passes. There is nothing
  to see in the app yet — the Parquet reader that will be graded against
  these still doesn't exist (that's `docs/ROADMAP.md` M7). See "Assumptions
  made" below for the inferred values the maintainer should veto by testing.

### Added
- Ten more real-world-shaped test files for the torture corpus
  (`testdata/corpus/`), covering value-level edge cases: a run of consecutive
  missing (`NaN`) samples, `Infinity`/`-Infinity` values, a single extreme
  outlier (1e12) next to unit-scale values, a numeric column with a few
  stray non-numeric tokens mixed in, a boolean column shown three common
  ways (`true`/`false`, `0`/`1`, `TRUE`/`FALSE`), a string "machine state"
  column, a mostly-empty marker column with a couple of sparse events, whole
  integer values beyond the point where `f64` can represent them exactly,
  a constant (unchanging) series, and a series that is entirely missing
  values. Each ships with a `.expected.json` answer key, same as the
  corpus cases before it. There is nothing to see in the app yet — the
  dtype/value inference that will be graded against these still doesn't
  exist (that's `docs/ROADMAP.md` M2). See "Assumptions made" below for the
  inferred values the maintainer should veto by testing.

### Added
- Nineteen more real-world-shaped test files for the torture corpus
  (`testdata/corpus/`), covering how a file's time index is recognized: ISO 8601
  timestamps with and without a timezone, day-first (`DD/MM/YYYY`) and
  month-first (`MM/DD/YYYY`) dates, a fully ambiguous date that could be read
  either way, Unix epoch counters in seconds/milliseconds/microseconds/
  nanoseconds, Excel serial dates, a LabVIEW-style epoch, a plain progressive
  row counter with no real time column, non-monotonic and duplicated
  timestamps, timestamps with small millisecond jitter, an irregular event log,
  a run made of three separated bursts, a picosecond-resolution index, and a
  multi-year series sampled once a year. Each ships with a `.expected.json`
  answer key recording the expected time column, timestamp format, and sampling
  classification (uniform / segmented / irregular / progressive index). There is
  nothing to see in the app yet — the time-index inference that will be graded
  against these still doesn't exist (that's `docs/ROADMAP.md` M2). See
  "Assumptions made" below for the inferred values the maintainer should veto by
  testing.

### Added
- Internal groundwork: the engine can now tell which built-in reader would
  handle a given file, by looking at its extension (`.csv`, `.tsv`, and
  `.txt` all route to the same delimited-text reader today; anything else is
  reported as an unrecognized format instead of silently failing). This is
  routing only — no file is actually parsed yet, so there is nothing to see
  in the app. It's the first piece of `docs/ROADMAP.md` M2 ("open a clean CSV
  → see a time plot"): the `Reader` trait and format registry that every
  future format (and the real CSV parser) will plug into.

### Added
- Ten more real-world-shaped test files for the torture corpus
  (`testdata/corpus/`), covering structural edge cases: a metadata preamble
  before the real header, a file with no header row at all, duplicate column
  names, a header with a blank column name, a single-column (time-only) file,
  a file with just one data row, trailing blank lines and a trailing
  delimiter on every row, rows with the wrong number of fields, a file
  truncated mid-write, and a completely empty file. Each ships with a
  `.expected.json` answer key. Two of these (the single-column file and the
  empty file) are cases the app must *refuse to open* with a clear message
  rather than crash or silently show nothing — their answer key now records
  that expected failure instead of a successful-open summary (see
  "Assumptions made" below). There is nothing to see in the app yet — the CSV
  reader that will be graded against these still doesn't exist (that's
  `docs/ROADMAP.md` M2).

### Changed
- Roadmap bookkeeping only, no app behavior change: the "CI matrix" box in
  `docs/ROADMAP.md`'s foundation milestone is now ticked. It was left
  unticked previously because `clippy`/`test` didn't yet cover the whole
  workspace (`glyde-app` was excluded) and `fmt`/`clippy` weren't matrixed
  across all three OSes; that gap has since been closed in
  `.github/workflows/ci.yml` (the `lint`/`test`/`gui` jobs together now run
  fmt, clippy `-D warnings`, and tests for every crate on Linux, macOS, and
  Windows) — the checklist just hadn't caught up.

### Added
- Six more real-world-shaped test files for the torture corpus
  (`testdata/corpus/`), covering encoding and character edge cases: a
  Latin-1-safe file with `°C`/`µm/s²` in the header, a Windows-1252 file with
  smart quotes and an em dash in a text column, a UTF-8 file with a leading
  byte-order mark, a full UTF-16LE file, a file with one invalid byte spliced
  into an otherwise-valid UTF-8 field, and a file whose header uses bracketed
  unit suffixes (`Temp [°C]`). Each ships with a `.expected.json` answer key,
  same as the first seven corpus cases. There is nothing to see in the app
  yet — the CSV reader that will be graded against these still doesn't exist
  (that's `docs/ROADMAP.md` M2).

### Changed
- Roadmap bookkeeping only, no app behavior change: ticked the "architecture guard"
  and "`cargo deny check` job" boxes in `docs/ROADMAP.md`'s foundation milestone —
  both have been passing in CI for a while, the checklist just hadn't caught up. The
  "CI matrix" box on the same milestone was deliberately left unticked: unlike the
  other two, it isn't actually satisfied yet (fmt/clippy don't run across all three
  OSes, and clippy/test don't cover the whole workspace) — see issue #7, which tracks
  finishing that item.

### Added
- Initial repository scaffolding: product/spec/architecture/quality documentation,
  CI matrix for Linux, macOS and Windows, issue templates.

### Added
- Seven real-world-shaped test files for the "torture corpus" (`testdata/corpus/`),
  the fixture set the app will be proven against as ingestion lands: a clean
  comma-delimited file, the European semicolon-delimiter/comma-decimal trap, a
  tab-delimited file, a whitespace-aligned `.txt`, a pipe-delimited file, a file with
  a comma-containing value quoted inside a field, and a file with an embedded
  newline quoted inside a field. Each ships with a `.expected.json` describing
  exactly what a correct open should infer. There is nothing to see in the app yet —
  these are the answer key the CSV reader will be graded against once it lands.

### Fixed
- The app (`glyde-app`) now builds on Linux again. It was configured in a way that
  left its windowing layer with no display-server backend on Linux, so it failed to
  compile there at all — which also meant the planned Linux download could never be
  produced. It now supports **both X11 and Wayland**: a single Linux build runs
  under either, choosing automatically at startup. macOS and Windows are unaffected.
  The Linux "does it still compile and lint?" check has been switched back on in CI
  so this can't regress unnoticed (issue #8).
- The two internal crates that make up the app (`glyde-core`, the engine, and
  `glyde-app`, the window you actually run) are now explicitly marked as
  "not published to crates.io", matching the third internal crate
  (`glyde-devtools`) that already was. This has no effect on using the app —
  it only prevents an internal build tool from ever being accidentally
  published as a public library.
- The app now writes a log file every time it runs, in the standard per-OS app-data
  location (e.g. `~/.local/share/Glyde/logs` on Linux, `~/Library/Application
  Support/Glyde/logs` on macOS, `%LOCALAPPDATA%\Glyde\logs` on Windows), in addition
  to printing to the console. There is nothing to see in the app itself yet — this is
  the foundation so that, once real features land, a bug report can include one log
  file that explains what happened, per `docs/SPEC.md` §6.
- Internal groundwork: the engine now has a single, shared way to report a failure
  (a malformed file, an unreadable path, and — as more of the app lands — every other
  kind of failure) instead of each part inventing its own. There is nothing to see in
  the app yet; this is what makes the next features able to fail loudly and clearly
  instead of silently or with a crash, per `docs/ARCHITECTURE.md` §Error philosophy.

- Internal groundwork: the torture-corpus loader and its `open()`-vs-expectation
  comparison harness (`docs/ROADMAP.md` M1). This is the machinery that will let every
  future PR verify a file opens correctly against `testdata/corpus/<name>.expected.json`
  — nothing to see in the app yet, and the corpus itself is still empty (the actual
  torture files land in the next few PRs, per `docs/QUALITY.md` §1). A test asserting
  all 56 corpus cases are present is committed but intentionally skipped until the last
  batch of files lands.

### Assumptions made (maintainer: veto by testing)
- Encoding inference (corpus cases 8, 9, 12) needed a judgment call
  `docs/SPEC.md` §1.2.1 doesn't spell out: how much invalid-byte corruption
  should still read as "UTF-8 with a few bad bytes" versus "this is actually
  a different single-byte encoding"? I used a 1% invalid-byte-density
  threshold over the bounded head sample (documented alongside the code) —
  it comfortably separates case 12's one stray byte (~0.4% of the file) from
  cases 8/9's genuinely Windows-1252 content (>1%). Above that threshold the
  sample goes to `chardetng` (the heuristic `docs/ARCHITECTURE.md` names for
  this step), but its guess is clamped to Windows-1252 whenever it lands
  outside Glyde's v1-frozen encoding set (UTF-8/UTF-16/Latin-1/Windows-1252
  per SPEC §1.2.1): on the short, mostly-ASCII case-8 fixture, chardetng's
  raw guess is "Big5" — three high bytes happen to look like a valid
  two-byte CJK sequence when there are only six data rows of evidence to
  weigh against it — and Glyde has no Big5 decoder to route that to. Both
  the density threshold and the out-of-scope clamp are new decisions this
  PR introduces rather than something SPEC.md dictates; worth a veto before
  more of M2 builds on this function's exact boundary.
- Corpus case 47's three boolean columns (`flag_lower`, `flag_numeric`,
  `flag_upper`) turned out **not** to be the same boolean sequence spelled
  three ways: `flag_numeric` (`0,1,0,1`) is the logical inverse of
  `flag_lower`/`flag_upper` (`true/false/true/false`,
  `TRUE/FALSE/TRUE/FALSE`) in the committed fixture. The new `Series`-model
  test asserts each column's actual values rather than cross-column
  equality. Worth checking whether the fixture's numeric column was meant
  to mirror the other two and drifted — nothing in `docs/QUALITY.md` §1's
  one-line description of case 47 requires them to match, so this is a
  flag, not a fix.
- The Welch golden tests are written against a new `glyde_core::dsp::welch`
  API this PR also stubs in (`WelchConfig`, `Psd`, `default_segment_length()`,
  `welch()`, `welch_segmented()`), plus two small supporting modules,
  `glyde_core::dsp::window` (`Window` enum, `coefficients()`, `mean_square()`)
  and `glyde_core::dsp::detrend` (`Detrend` enum, `apply()`) — matching
  `docs/ARCHITECTURE.md`'s planned `dsp/welch.rs`, `dsp/window.rs`,
  `dsp/detrend.rs` file layout. Every function body is `todo!()`, no algorithm
  is implemented, and every test is `#[ignore]`d so CI stays green. This is
  test-first scaffolding, not a design decision on the final API:
  `docs/ROADMAP.md` M5 is free to reshape the function signatures as long as
  it keeps satisfying what each golden test asserts. Worth a veto if the shape
  (e.g. `welch_segmented()` taking `&[&[f64]]` rather than a richer segment
  type, or `overlap` as a bare `f64` fraction) looks like the wrong direction
  before M5 commits to it.
- The decimation golden tests are written against a `glyde_core::dsp::decimation`
  API this PR also stubs in (`Bucket`, `PYRAMID_FACTOR = 8`, `build_pyramid()`,
  `decimate_viewport()`) so the tests compile — every function body is
  `todo!()`, no algorithm is implemented, and every test is `#[ignore]`d so CI
  stays green. This is test-first scaffolding, not a design decision on the
  final API: `docs/ROADMAP.md` M3 is free to reshape the function signatures
  as long as it keeps satisfying what each golden test asserts. Worth a veto
  if the shape (e.g. `i128` tick timestamps, one `Vec<Bucket>` per pyramid
  level) looks like the wrong direction before M3 commits to it.
- Corpus cases 53–56 (Parquet) record `"encoding": "n/a"` rather than an
  `encoding_rs` canonical name, since Parquet is a self-describing binary
  format with no text-encoding concept to infer — the corpus README already
  documents `delimiter`/`decimal_separator` as `null` "for formats without a
  delimiter concept (e.g. Parquet)" but was silent on `encoding`, which is
  the one field in `OpenSummary` with no `Option` wrapper. `"n/a"` was the
  smallest change that doesn't force the field optional workspace-wide for
  four cases; worth a veto since it's a new convention, not one already in
  the schema.
- Corpus cases 53–56 record `"timestamp_format": "parquet_timestamp"`, a new
  vocabulary entry (alongside `iso8601`, `epoch_ms`, etc. from cases 24–42)
  standing for "read directly from the column's native Arrow/Parquet
  timestamp type, no text parsing involved" — there's no format-string
  ambiguity to name for a typed column, unlike the CSV formats it sits
  alongside.
- Case 54 ("Parquet with nulls") keeps `row_count: 6` and `skipped_row_count:
  0` with the two null values still counted as present rows — consistent
  with how CSV `NaN` runs (case 43) are treated: a missing *value* is a
  flagged anomaly within a row (`docs/ROADMAP.md` M7: "Parquet nulls →
  gaps" refers to a plotted discontinuity, not a dropped row), not a
  structural reason to skip the row.
- Case 55 ("partitioned Parquet directory") uses two Hive-style
  `date=YYYY-MM-DD/` partitions, three rows each, one calendar-day apart —
  large enough for the combined timestamp gap between partitions to clear
  the ×10-median threshold (`docs/SPEC.md` §2.2), so it's recorded as
  `sampling_class: "segmented_uniform"` with `gap_count: 1`, the same shape
  as CSV case 40's three-bursts case rather than a from-scratch convention.
- Corpus cases 43–52 all record `row_count` equal to every data row present,
  with `skipped_row_count: 0` — per `docs/SPEC.md` §1.3, `NaN`/missing values
  and out-of-range/mixed-type cell values are preserved as flagged anomalies
  within a row, not reasons to drop the whole row (`skipped_row_count` is
  reserved for structural problems like wrong field count or a truncated
  tail, per cases 21/22). This differs from how `skipped_row_count` is used
  elsewhere in the corpus, so it's worth an explicit veto.
- Case 46 ("mixed numeric/string in one column") assumes the eventual dtype
  inference falls back the whole column to `string`/categorical rather than
  parsing the numeric-looking cells and dropping/flagging the non-numeric
  ones — per `docs/SPEC.md` §1.4's "never upcast silently in a way that
  loses precision" and Golden Rule 1 ("never degrade the raw data"), turning
  `"ERR"`/`"OK"` into missing values would silently discard what the source
  file actually says. No dtype field exists in the `.expected.json` schema
  yet to assert this directly (same gap noted for cases 36/37 previously),
  so this is an assumption about future behavior, not something this PR's
  test enforces.
- Case 49 ("sparse markers") is a 9-row file rather than literally "one
  sample every N thousand" (`docs/QUALITY.md` §1) — the corpus favors small,
  hand-readable fixtures (see the existing 6-56 row cases), and large-N
  sparsity is already covered separately by the `criterion` benches and the
  `glyde-devtools` synthetic-fixture generator (`docs/ROADMAP.md` M3). This
  file exists to pin the *shape* (a marker column that's empty almost
  everywhere with rare events), not the scale.
- Case 50 (`i64` beyond 2⁵³) uses `9007199254740993` (2⁵³+1, the smallest
  integer that loses precision when rounded to `f64`) and `i64::MAX`, to
  bound both ends of the range `docs/SPEC.md` §1.4 says must be flagged
  rather than silently narrowed.
- Corpus cases 24–42 invent a `timestamp_format` vocabulary (no format names
  existed yet beyond the three examples in `testdata/corpus/README.md`):
  `iso8601`, `iso8601_naive`, `dd_mm_yyyy`, `mm_dd_yyyy`, `epoch_s`, `epoch_ms`,
  `epoch_us`, `epoch_ns`, `excel_serial`, `labview_epoch`. These are the labels
  a correct reader will report once time-index inference lands (M2); veto any
  name now, before the inference code depends on them.
- Corpus case 28 (fully ambiguous `01/02/2026` dates) records `dd_mm_yyyy` per
  `docs/SPEC.md` §2.1's stated ISO-leaning default. The "low confidence →
  expanded inference bar" half of that case can't be expressed here — the
  answer-key schema has no confidence field — so it's deferred to the UI work
  in M4.
- Corpus case 34 (LabVIEW epoch) uses an approximate 1904-epoch second count for
  2026 (~122 years × 365.25 days); the arithmetic was not verified against a
  reference implementation, since only the *shape* of a LabVIEW-epoch value
  matters for this fixture, not the exact real-world instant it names.
- Corpus cases 36 (non-monotonic) and 37 (duplicate timestamps) are both
  recorded as `sampling_class: "uniform"`, because the answer-key schema has no
  monotonicity or duplicate-count field: a single reversed or repeated interval
  doesn't move a robust (MAD-based) jitter statistic past the 1% threshold. The
  non-monotonic/duplicate *flagging* `docs/SPEC.md` §2.1 requires isn't
  observable through this schema at all yet — arguably a schema gap worth its
  own issue once the real time-inference code needs to assert it.
- Corpus case 39 (irregular event log) is shaped so exactly one interval (24 s
  vs. a 2 s median) exceeds the ×10 gap threshold (`gap_count: 1`) while the
  segment as a whole is non-uniform enough to fail the "uniform within segments"
  requirement, landing on `irregular` per `docs/SPEC.md` §2.2. Case 40 (three
  bursts) matches `docs/QUALITY.md` §1's explicit "→ SegmentedUniform, 2 gaps"
  wording.
- All nineteen cases use plain `utf-8` / `,` / `.` for encoding/delimiter/
  decimal — this batch exercises time-column inference only, not delimiter or
  encoding detection (those are cases 1–13).
- Corpus case 8 ("Latin-1 header") is written using only bytes in the
  0xA0–0xFF range (`°`, `µ`, `²`), which are byte-identical between true
  ISO-8859-1 and Windows-1252, and its `.expected.json` records `"encoding":
  "windows-1252"` rather than an ISO-8859-1-specific label. `encoding_rs`
  (the crate `docs/SPEC.md` §1.2.1 names for encoding inference) has no
  separate ISO-8859-1 decoder — the WHATWG standard it implements aliases
  that label to windows-1252 — so this is what a correct reader will actually
  report once ingestion lands. Case 9 is the one that exercises the
  0x80–0x9F range (smart quotes, em dash) where the two encodings diverge.
- Corpus case 12 ("invalid byte sequences mid-file") assumes the eventual CSV
  reader replaces individual invalid bytes with U+FFFD in place (per
  `docs/SPEC.md` §1.2.1) rather than dropping the whole row, so its
  `row_count` is 6 and `skipped_row_count` is 0 — the malformed byte sits
  inside one field's value, not on a delimiter, so row structure survives.
- The `<name>.expected.json` schema (`encoding`, `delimiter`, `decimal_separator`,
  `time_column`, `timestamp_format`, `row_count`, `skipped_row_count`,
  `sampling_class`, `gap_count`) is my reading of `docs/QUALITY.md` §1's one-sentence
  description ("inferred encoding/delimiter/decimal/time column/format, row count,
  skipped-row count, sampling class, gap count") into concrete field names and types.
  No per-field confidence score is included yet — `docs/SPEC.md` tracks confidence for
  the *inference bar* (M4), and QUALITY.md's own corpus description doesn't ask for it
  in the expectation file, so I left it out rather than inventing a second use for the
  same data ahead of need. Every future corpus-case PR writes against this schema, so
  it is worth the maintainer's veto now rather than after 56 files depend on it.
- The comparison harness that will diff a real `open()` against a corpus case's
  `.expected.json` is stubbed behind an unused `corpus-open-compare` Cargo feature on
  `glyde-core`, rather than written against real code, because the `Reader` trait and
  `open()` entry point don't exist yet (that's M2). Only the schema, the file-pairing
  logic, and the comparison function itself (`compare()`) are implemented and tested
  today; wiring it to a real `open()` call is deferred to whichever M2 PR adds ingestion.
- Corpus case 4 (whitespace-aligned `.txt`) records its `delimiter` as a single
  space (`" "`) in `.expected.json`, standing in for "one or more whitespace
  characters" rather than a literal single-space separator — no format-specific
  delimiter vocabulary is defined yet.
- Corpus case 7's quoted field spans a physical newline inside the file, so its
  `row_count` (6) is *lower* than a naive line count (8) — this is intentional: a
  correct CSV reader must treat the quoted newline as part of one field, not as a
  new row. Recorded here since it's easy to misread as an error at a glance.
- The app-data location is resolved as `directories::ProjectDirs::from("com", "glyde",
  "Glyde")`. Neither `docs/SPEC.md` nor `docs/ARCHITECTURE.md` specifies the exact
  qualifier/organization/application strings, so I picked a conventional
  reverse-DNS-style identifier. This only affects where the log folder lives on disk;
  it is free to change before release without any user-visible consequence.
- Log files rotate daily (`glyde.log.<date>`) with no retention/pruning yet. Retention
  policy isn't specified anywhere in scope; I judged unbounded daily files
  acceptable for now given how small a log file is expected to be, versus the risk of
  guessing a deletion policy that silently discards evidence a bug report might need.
- The shared error type only has one kind of failure so far (a file that can't be
  read). `docs/ARCHITECTURE.md` doesn't enumerate the full list up front — more kinds
  (bad encoding, unsupported format, and so on) are added alongside the feature that
  can actually produce them, so the error type never claims to handle a failure no
  code path yet raises.
- The app-boundary test (proving a failure from the engine surfaces correctly to the
  app) lives on its own next to `main.rs` rather than inside a real feature, since no
  feature calls into the engine yet — File→Open lands in M2. It will move once there
  is a real call site to attach it to.
- Corpus cases 18 (single-column file) and 23 (empty file) are QUALITY.md §1's two
  required-failure cases ("must fail with a clear message, not a crash"), but the
  existing `.expected.json` schema (`OpenSummary`) only had a shape for a *successful*
  open — every field mandatory, no way to say "must error." I extended the corpus
  loader's schema to an `ExpectedOutcome` enum: the existing `OpenSummary` shape for
  the 21 success cases, plus a new `{ "error": "<message>" }` shape for these two. The
  `error` string is a human-readable message for the maintainer to recognize, not
  something a future test pattern-matches against — no error taxonomy exists yet
  (`GlydeError` has one variant, `Io`), and inventing one now, before the CSV reader
  that would raise it exists, would be guessing ahead of M2.
- Corpus case 15 (no header row) assumes headerless files get synthetic column names
  `column_0`, `column_1`, ... in source-column order. Neither `docs/SPEC.md` nor
  `docs/ARCHITECTURE.md` specifies a naming scheme for this case; this is the smallest,
  most predictable convention I could pick, and it only affects a fallback display
  name — please veto if you'd rather have something else (e.g. spreadsheet-style `A`,
  `B`, ...).
- Corpus case 14's five `#`-prefixed metadata-preamble lines are not counted in
  `skipped_row_count` — per `docs/SPEC.md` §1.2's header-detection rule, they are
  consumed while *locating* the header, not read as malformed data rows, so they are
  a different concept from the "wrong field count / unparseable / truncated" rows
  §1.3 says `skipped_row_count` tracks (exercised by cases 21 and 22 instead).
- Corpus case 19 (a file with exactly one data row) records `sampling_class` as
  `"uniform"`: with only one sample there is no interval to measure, so uniformity
  holds vacuously (there is nothing for it to violate) — the same reasoning that lets
  a single-sample series render at all per `docs/SPEC.md` §1.4.
- Corpus case 22's truncated final line has no value field at all (not just a
  truncated number), so it is unambiguously incomplete rather than a valid-but-short
  row; per `docs/SPEC.md` §1.3, which lists "truncated tails" alongside wrong-field-count
  rows as a `skipped_row_count` category, it counts as 1 skipped row, not 0.
