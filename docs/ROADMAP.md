# Glyde — v1 Roadmap

The complete v1 backlog, derived only from `docs/PRODUCT.md`, `docs/SPEC.md`,
`docs/ARCHITECTURE.md`, and `docs/QUALITY.md`. No scope is invented here; every item
traces to a specific section. Each item is one PR.

Milestones are ordered by dependency and, within that, by what unblocks a hand-test
earliest. Every milestone ends in something the maintainer can **test by hand on a real
file** (or, for the foundation milestone, by reading CI/test results — the maintainer's
other validation channel per `CLAUDE.md`).

**Two structural decisions (flagged for veto):**
- **M0 (Foundation)** is not in the suggested shape but precedes M1: no item can be "one
  PR" until the workspace, CI matrix, and architecture guards exist. Its maintainer test
  is reading test/CI output, not using the app.
- **Gap handling is split.** Gap *detection* (`Δt > 10 × median`) lands in M2 because
  sampling classification (SPEC §2.2) depends on it. The gap *view* (compressed axis,
  cluster-fit, anomaly gutter) is the hand-testable payload of **M8**.

Golden tests are **written before their implementation** (Golden Rule 4 / QUALITY §2). In
M1 they are committed and `#[ignore]`d with a pointer to the milestone that un-ignores
them, so CI stays green while honoring "written first."

---

## M0 — Foundation: workspace, CI, guards

- [x] Create Cargo workspace with `glyde-core`, `glyde-app`, `glyde-devtools` (publish=false) + `rust-toolchain.toml` → ARCH §Workspace layout · proven by: `cargo build --workspace` green in CI
- [x] CI matrix (`ubuntu-latest`, `macos-14`, `windows-latest`): `fmt --check`, `clippy --all-targets -- -D warnings`, `test --workspace` → QUALITY §4.1–4.3 · proven by: 3-OS matrix green on the scaffolding PR
- [x] Architecture guard: CI asserts no GUI crate under `cargo tree -p glyde-core` → ARCH Hard rule 1, QUALITY §4.5 · proven by: step goes red if `egui` is added to core
- [x] `deny.toml` + `cargo deny check` job (licenses/advisories) → ARCH §deps, QUALITY §4.6 · proven by: deny job green
- [x] `tracing` + `tracing-subscriber` + `tracing-appender` init in `glyde-app` (rotating file in OS app-data dir + console) → SPEC §6, ARCH §Logging · proven by: log file appears on run
- [x] `GlydeError` (`thiserror`) + core `Result` alias + app-boundary `anyhow` wiring → ARCH §Error philosophy · proven by: unit test constructing/propagating an error

> **MAINTAINER TEST — M0**
> - Pull the branch, run `cargo test --workspace` → passes locally.
> - Open the PR's CI page → green on Linux, macOS, Windows.
> - Correct looks like: fmt/clippy/test/deny all green; if someone adds `egui` to
>   `glyde-core`, the arch step turns red. Nothing to click in an app yet.

---

## M1 — Torture corpus + golden-test scaffolding (the safety net, built first)

- [x] Corpus loader + `<name>.expected.json` schema + open→compare harness (behind a feature so assertions activate as ingestion lands) → QUALITY §1 · proven by: manifest test confirms 56 files each have a valid `expected.json`
- [x] Commit corpus cases 1–7 (delimiters & locale) + expected.json → QUALITY §1 (1–7)
- [x] Commit corpus cases 8–13 (encoding & characters) + expected.json → QUALITY §1 (8–13)
- [x] Commit corpus cases 14–23 (structure) + expected.json → QUALITY §1 (14–23)
- [x] Commit corpus cases 24–42 (time index) + expected.json → QUALITY §1 (24–42)
- [x] Commit corpus cases 43–52 (values) + expected.json → QUALITY §1 (43–52)
- [x] Commit corpus cases 53–56 (Parquet) + expected.json → QUALITY §1 (53–56)
- [x] Write decimation golden tests, `#[ignore]` until M3 → QUALITY §2 Decimation (spike, envelope, pyramid, convergence, no-alias)
- [x] Write Welch golden tests, `#[ignore]` until M5 → QUALITY §2 Welch (sinusoid, Parseval, one-sided, window-norm, detrend, two-tone, segmented, raw-guarantee)
- [x] Write time golden tests, `#[ignore]` until M2/M8 → QUALITY §2 Time (format round-trip, ns/ps precision, gap detection)
- [x] Property-test scaffolding (`proptest`): render-never-panics + CSV fuzz-lite, `#[ignore]` until deps exist → QUALITY §2 Property tests

> **MAINTAINER TEST — M1**
> - Run `cargo test` → the corpus manifest test passes; `cargo test -- --ignored --list`
>   shows the full golden-test set (counts match QUALITY §2).
> - Open the 56 files in `testdata/corpus/` in your own editor/tools → they are the messy
>   real-world shapes QUALITY §1 describes (European decimals, `°C` headers, ragged rows,
>   epoch timestamps, etc.).
> - Correct looks like: the asset exists and is well-formed; no feature claims coverage yet.

---

## M2 — Open a clean CSV → see a time plot (first end-to-end slice)

- [x] `Series` + dtype model (`bool`, `i8..i64`, `u8..u64`, `f32`, `f64`, string/categorical) + anomaly container → SPEC §1.4 · proven by: corpus 51 (constant), 47 (bool dtype)
- [x] `Reader` trait + `open()` entry point + format registry → ARCH Hard rule 5, SPEC §1.1 · proven by: unit test registering the CSV reader
- [x] Encoding inference (`encoding_rs` BOM sniff → `chardetng` heuristic, invalid bytes replaced) → SPEC §1.2.1 · proven by: corpus 8, 9, 10, 11, 12
- [x] Joint delimiter / header / decimal-separator inference (column-count consistency; `1,5;2,3` = 2 cols) → SPEC §1.2.2–1.2.4 · proven by: corpus 1, 2, 3, 4, 5, 6, 7, 14, 15
- [x] CSV reader: streaming single-pass over memmap; ragged-row salvage, skipped-row count, truncated-tail tolerance → SPEC §1.3, ARCH §CSV · proven by: corpus 20, 21, 22, 23
- [ ] Column dtype inference incl. NaN / infinity / mixed-column handling → SPEC §1.4 · proven by: corpus 43, 44, 46, 47, 48
- [ ] Time index: progressive numeric + core timestamp formats (ISO 8601 ±tz, epoch s/ms/µs/ns); internal `i128` ns or `(i64, TimeUnit)`, never `f64` seconds → SPEC §2.1 · proven by: corpus 24, 25, 29–32, 35 + time golden round-trip (un-ignore subset)
- [ ] `DD/MM` vs `MM/DD` disambiguation by scanning for a field > 12; fully ambiguous → low confidence → SPEC §2.1 ambiguity rule · proven by: corpus 26, 27, 28
- [ ] Excel serial + LabVIEW epoch + multi-year/second-res + picosecond-res index → SPEC §2.1 · proven by: corpus 33, 34, 42, 41 + ns/ps precision golden (un-ignore)
- [ ] Sampling classification `Uniform` / `SegmentedUniform` / `Irregular` at index time (needs gap detection `Δt > 10× median`) → SPEC §2.2–2.3 · proven by: corpus 38, 39, 40 + gap-detection golden (un-ignore)
- [ ] Non-monotonic + duplicate timestamp detection (counted, flagged, never reordered) → SPEC §2.1 · proven by: corpus 36, 37
- [ ] Activate corpus open→compare gate for all cases handled so far → QUALITY §1
- [ ] `glyde-app`: single egui window, File→Open + drag-drop, background indexer thread + channels (UI never blocks/does I/O) → SPEC §6, ARCH §Threading · proven by: manual (window opens a file off-thread)
- [ ] Time-domain view v1: render raw samples (small files, pre-pyramid), pan / zoom / fit-to-data, cursor readout of exact raw value + timestamp → SPEC §4.1 · proven by: manual + headless render snapshot (`insta`)

> **MAINTAINER TEST — M2**
> - Open corpus case 1 **and** one of your own small, clean CSVs → a time plot appears in
>   seconds; pan, wheel-zoom, box-zoom, and fit-to-data all work; the cursor shows the
>   exact value and timestamp.
> - Open the European `;` + comma-decimal file (case 2) → columns split correctly, not into
>   twice as many.
> - Open the 5-line-preamble file (case 14) → the header is found below the junk.
> - Correct looks like: no configuration was required to reach the plot.

---

## M3 — Index pyramid + large files (fluid navigation, the performance contract)

- [ ] Min/max pyramid: level-k `(min, max, first_ts, last_ts, nan_count)`, factor-8 buckets, level *k+1* exactly from level *k* → ARCH §Index, SPEC §3.1 · proven by: decimation golden — pyramid consistency + envelope exactness (un-ignore)
- [ ] Background progressive build emitting partial levels → first meaningful plot ≤ 2 s while indexing continues → SPEC §5 (first-plot), ARCH §pipeline · proven by: first-plot bench + manual
- [ ] Decimation query `viewport(range, pixels) → min/max per column`; raw samples + point markers when samples < pixels; convergence to true samples → SPEC §3.1 · proven by: decimation golden — spike, convergence, no-alias (un-ignore)
- [ ] Pyramid spill to OS app-data cache keyed by path + size + mtime; reopen is instant → ARCH §Index, SPEC §5.1 · proven by: spill round-trip unit test + manual reopen
- [ ] RAM budget module (`sysinfo`): compute `min(25% RAM, 4 GB)`, affordability check *before* acting → SPEC §5.1, ARCH §budget · proven by: budget-math unit tests
- [ ] `glyde-devtools`: synthetic fixture generator + `memory_gate` headless peak-RSS harness → QUALITY §3, ARCH §devtools · proven by: memory gate under cap on the 20 GB fixture in CI
- [ ] `criterion` benches: index build (CSV 1 GB), viewport query per pyramid level, cold start, peak RSS during scripted 20 GB open; absolute ceilings + >15% regression gate → QUALITY §3, SPEC §5 · proven by: benches within ceilings in CI

> **MAINTAINER TEST — M3**
> - Open one of your own large CSVs (> 5 GB) → first plot within ~2 s, indexing visibly
>   continuing in the background.
> - Scroll and zoom continuously for 30 s → no stutter, no freeze; memory stays under the
>   cap in Activity Monitor; the fan stays sane.
> - Find a one-sample spike in a huge file → it stays visible at every zoom level.
> - Zoom all the way in → the view converges to individual sample points.
> - Close and reopen the big file → it opens instantly (from cache).

---

## M4 — Inference bar (honest transparency)

- [ ] `InferenceReport` surfaced to the UI: encoding, delimiter, decimal, time column, timestamp format, sample count, sampling class, per-field confidence → SPEC §1.2 (mandatory UX) · proven by: report-struct snapshot + manual
- [ ] Inference bar widget: persistent and discreet; opens expanded when any inference is low-confidence → SPEC §1.2, §2.1 · proven by: manual on corpus 28 (ambiguous dates → expanded)
- [ ] One-click correction of each field → triggers a re-index → SPEC §1.2 · proven by: manual (swap delimiter / decimal / date order → plot updates)
- [ ] Skipped-rows detail surface ("N rows skipped — view details") → SPEC §1.3 · proven by: manual on corpus 21 (ragged)
- [ ] Timestamp affordances: "not monotonic — [Sort]/[Keep as-is]", timezone label (honored or "naive local") → SPEC §2.1 · proven by: manual on corpus 36; corpus 24 (tz displayed)

> **MAINTAINER TEST — M4**
> - Open the European `;`/comma file → the bar shows `;` and comma-decimal correctly.
> - Open a fully-ambiguous-date file → the bar is expanded and low-confidence; one click
>   swaps `DD/MM` ↔ `MM/DD` and the plot re-indexes.
> - Deliberately let it mis-infer on one of your files → one click in the bar fixes it.
> - Open a ragged file → the skipped-row count is shown with a details view.
> - Correct looks like: nothing was guessed silently — every inference is visible.

---

## M5 — PSD (Welch, the differentiator)

- [ ] Window functions (Hann/Hamming/rectangular) with correct power normalization → SPEC §3.2 · proven by: Welch golden — window-normalization (un-ignore)
- [ ] Detrend (per-segment mean removal, constant) by default, documented → SPEC §3.2 · proven by: Welch golden — detrend (un-ignore)
- [ ] Welch core on `rustfft`: default segment length (largest pow2 ≤ N/8, clamped [256, 65536]), 50% overlap, one-sided ×2 scaling (not DC/Nyquist), variance conservation → SPEC §3.2 · proven by: Welch golden — sinusoid, Parseval, one-sided, two-tone (un-ignore)
- [ ] Streaming Welch (accumulate segment periodograms, never load all); budget refusal + affordable alternative when the selection won't fit → SPEC §3.2, §5.1 · proven by: PSD-10M bench ≤ 1 s
- [ ] `SegmentedUniform`: per-segment Welch, length-weighted average, no window crosses a gap, short segments excluded+reported; `Irregular`: PSD disabled with explanation + offer largest uniform sub-range → SPEC §3.3 · proven by: Welch golden — segmented (un-ignore) + corpus 39, 40
- [ ] PSD always computed on raw samples, never the pyramid → SPEC §3.2 · proven by: Welch golden — raw-guarantee (bit-identical zoomed-in vs out, un-ignore)
- [ ] PSD view: time-view selection drives it; overlay or stacked with shared synchronized frequency axis; log/linear toggles both axes; "computed on" readout (samples, segments, window, Δf) → SPEC §4.2 · proven by: manual + snapshot

> **MAINTAINER TEST — M5**
> - Pick a signal with known frequency content → the PSD peak lands where physics says.
> - Select a sub-interval in the time view → the PSD recomputes for that interval only.
> - Open the 3-burst segmented file → the PSD is labeled "averaged over N segments".
> - Open the irregular event-log file → PSD is disabled with a clear explanation (and offers
>   the largest uniform sub-range if one exists).
> - Toggle log/linear on both axes → behaves sanely.

---

## M6 — State timeline (non-numeric & event data)

- [ ] Boolean series → on/off horizontal bands (not numeric step plots) → SPEC §4.3 · proven by: corpus 47 + manual
- [ ] String/categorical → labeled state bands; label when the band is wide enough, tooltip otherwise → SPEC §4.3 · proven by: corpus 48 + manual
- [ ] Markers (single-sample events) → tick glyphs on their own lane, never decimated away → SPEC §4.3 · proven by: corpus 49 + manual
- [ ] Collapse rule: when states collapse into one pixel column, render a "multiple states" glyph that expands on zoom — no event ever disappears → SPEC §4.3 · proven by: manual zoom test
- [ ] Time-alignment: state lanes share the single synchronized time axis with the numeric plots, stacked → SPEC §4.1, §4.3 · proven by: manual

> **MAINTAINER TEST — M6**
> - Open a file with boolean + string-state + marker series → bands and ticks render and are
>   time-aligned with the numeric plots above them.
> - Zoom out until many states fall in one column → a "multiple" glyph appears; zoom back in →
>   every event reappears. Nothing vanishes at any zoom.

---

## M7 — Parquet

- [ ] Parquet reader behind the `Reader` trait: `arrow`/`parquet` row-group access, dtype mapping → SPEC §1.1, §1.4, ARCH §Parquet · proven by: corpus 53
- [ ] Parquet nulls → gaps; dictionary-encoded strings → categorical → SPEC §1.3, §1.4 · proven by: corpus 54, 56
- [ ] Partitioned / multi-file directory support → SPEC §1.1 · proven by: corpus 55
- [ ] Row-group min/max column statistics populate upper pyramid levels with no scan → ARCH §Index, ARCH §why-Parquet · proven by: stats→pyramid unit test + index-build (Parquet 1 GB) bench ≤ budget
- [ ] Parquet timestamp columns flow through the same time-index path → SPEC §2 · proven by: corpus 53 (time column)

> **MAINTAINER TEST — M7**
> - Open a clean Parquet with a timestamp column → time plot appears.
> - Open your own Parquet, including a partitioned directory → opens fast.
> - Confirm PSD and the state timeline work on Parquet identically to CSV.

---

## M8 — Gaps & anomalies (fidelity, made visible)

- [ ] Truthful linear time axis is the default; a gap looks like a gap and is never auto-overridden → SPEC §2.3 · proven by: corpus 40 + gap-detection golden
- [ ] Compressed/broken-axis escape hatch when gaps > 50% of total span (`N gaps · [Compress]`), break glyph, explicit labels; view-state only (no effect on data/DSP) → SPEC §2.3 · proven by: manual on a gap-heavy file
- [ ] Click a gap-adjacent cluster → fits it to the viewport → SPEC §2.3 · proven by: manual
- [ ] Anomaly gutter under the plot: NaN runs, skipped rows, flagged outliers; clickable to jump → SPEC §4.1 · proven by: corpus 43, 21, 45
- [ ] Outlier detection: robust MAD-based rule (documented threshold), **flag only — never remove or clip** → SPEC §1.3 · proven by: corpus 45
- [ ] NaN/missing → visible discontinuity + gutter marker, never interpolated → SPEC §1.3 · proven by: corpus 43, 52
- [ ] `i64` beyond ±2⁵³ → precision-loss flag in the log and inference bar → SPEC §1.4 · proven by: corpus 50
- [ ] Per-series unit label taken verbatim from the header (`°C`, `m/s²`) → SPEC §4.1 · proven by: corpus 8, 13

> **MAINTAINER TEST — M8**
> - Open a file with gaps → gaps look truthful; when gaps dominate the span, `[Compress]`
>   appears and is obviously a view change (data unchanged). Click a cluster → it fits.
> - Open a file with NaN runs + extreme outliers → discontinuities and gutter marks appear;
>   click to jump to them; the outliers are still plotted (never clipped).
> - Open the `i64 > 2⁵³` file → a precision-loss flag is shown.

---

## M9 — Packaging & release

- [ ] Release binary size gate ≤ 50 MB + cold-start ≤ 500 ms bench → SPEC §5, §7 · proven by: artifact-size CI check + cold-start bench
- [ ] Light/dark follows the OS; no theme settings → SPEC §6 · proven by: manual OS theme switch
- [ ] CI release job on tags: macOS arm64 `.dmg`, Windows x64 installer + portable `.exe`, Linux x64 `AppImage` + tarball, attached to the GitHub Release → SPEC §7, QUALITY §4.7 · proven by: tag build produces + attaches all artifacts
- [ ] Unsigned-build workaround steps for macOS & Windows in release notes → SPEC §7 · proven by: release-notes template contains them
- [ ] `CHANGELOG.md` finalized in plain language + manual-QA reminder ("run the 10-file smoke ritual") printed in release notes → CLAUDE.md §Release checklist, QUALITY §5 · proven by: release notes contain the ritual reminder

> **MAINTAINER TEST — M9**
> - Download the release binary on a clean machine (no Rust, no runtime) → it runs and
>   reaches a first plot with no documentation and nothing to install.
> - Run the full QUALITY §5 10-file smoke ritual on your personal file set.
> - Confirm the binary is ≤ 50 MB and that toggling the OS to dark mode themes the app.
