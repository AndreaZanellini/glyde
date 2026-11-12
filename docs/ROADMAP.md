# Glyde — Roadmap to v0.1.0

**Goal:** the shortest path to a release an external user (the maintainer's colleagues) can
download and use. Re-planned on 2026-11-11 with the maintainer; it replaces the M0–M9
roadmap, which is archived unchanged in [`history/ROADMAP-M0-M9.md`](history/ROADMAP-M0-M9.md)
(M0–M7 done, M8 partly done, M9 folded into Step 3 below). Code comments that cite
"docs/ROADMAP.md M*n*" refer to that archived file.

Each item is one PR and links its issue. Work them **in order**, one PR at a time. GitHub
milestones: `v0.1` and `v0.2`.

## Maintainer decisions behind this plan (2026-11-11)

1. **Features and performance are sufficient for v0.1.** The measured performance-budget
   breaches (#194, #195, #181 and the other v0.2 performance issues) do not block v0.1;
   they are listed in the release notes as known limitations.
2. **Outlier detection and the anomaly gutter move to v0.2** (#236).
3. **The state timeline must be rethought, not tweaked**: with several non-numeric
   channels everything is small and squashed. It is redesigned together with the whole
   time-view layout (#230). Fallback if the mockup does not convince: non-numeric channels
   hidden by default and enabled one by one.
4. **The stacked numeric view (#198) is in v0.1.**
5. **Measurement cursors enter v1 scope** (#231): Δt, ΔY and slope.
6. **v0.1 ships unsigned**, with the first-run workaround in the release notes. Signing is
   a v1.0 requirement.
7. **UI and icon get a full redesign** before v0.1, mockup first (#229, #232).
8. **A security review** happens before the release (#233).

`docs/PRODUCT.md` and `docs/SPEC.md` need matching edits for decisions 1, 2, 3, 5 and 6.
They are hard-denied for agents: the patch is in the description of the PR that
introduced this roadmap, for the maintainer to apply.

---

## Step 1 — Correctness and stability fixes

Small, independent PRs. Nothing here changes the look of the app.

- [x] CI: make the `first_plot` gate stop failing by chance (it also gates `release.yml`) → #184
- [ ] Blank cells in a numeric CSV column become gaps, not a text column → #189
- [ ] Gap count is meaningful when the median Δt is 0 (repeated timestamps) → #188
- [ ] Flag `i64` values beyond ±2⁵³ as precision loss (corpus 50) → #228
- [ ] In-memory open: calibrate the RAM estimate to the measured peak (2.3× low today) → #193
- [ ] Bound the cache directory (size cap, oldest-first eviction, logged) → #213

> **MAINTAINER TEST — Step 1**
> - Open a CSV with empty cells in a numeric column → the column is plotted with holes,
>   not moved to a text lane.
> - Open `testdata/corpus/case-50-*` → a precision-loss flag names the column.
> - Open the 269 MB gearbox Parquet showcase file → Activity Monitor stays well under 4 GB.
> - After many opens, the cache folder stays under its cap (path in the user docs).

---

## Step 2 — Redesign: look, layout, cursors, icon

Design first. Nothing is implemented before the maintainer approves the mockup.

- [ ] Interactive mockup, light and dark, built on the engine test-bench showcase file:
      visual identity, track layout with state channels, PSD panel, cursors → #229, #230, #231
- [ ] Visual identity in the app: palette, typography, light/dark following the OS → #229
- [ ] Track layout: one shared time axis at the bottom, compact state tracks, guaranteed
      plot height, collapsible state group, PSD panel → #230, #200
- [ ] Stacked numeric view sharing the time axis → #198
- [ ] Time shown as dates and times, not raw numbers (epoch axes, readout, hover) → #196
- [ ] Time navigation: zoom acts on x, y follows the visible data, bounded zoom-out → #197
- [ ] Plot x coordinate relative to the file start, so sub-µs data is exact → #206
- [ ] Measurement cursors A/B: Δt, 1/Δt, values, ΔY and slope from raw samples → #231
- [ ] App icon on the three OSes and the window → #232

> **MAINTAINER TEST — Step 2**
> - Open `01_engine_testbench.csv` in a 1440×900 window → the plot dominates; state
>   channels are readable; nothing looks squashed; switch the OS theme → the app follows.
> - Toggle stacked view → every channel is readable on its own scale.
> - Place two cursors on a ramp → Δt, ΔY and slope match a hand calculation.
> - Open `case-41-picosecond-resolution-index.csv` → six separate samples, not one line.
> - The Dock / taskbar shows the Glyde icon.

---

## Step 3 — Security, cleanup, release

- [ ] Security review: fuzzing, `unsafe` and cache validation, supply chain, log privacy → #233
- [ ] Repository cleanup and end-user documentation (README, user guide, `docs/dev/`) → #234
- [ ] Release `v0.1.0`: size check, dry run, release notes with known limitations, tag → #235

> **MAINTAINER TEST — Step 3**
> - Read the new README as if you were a colleague → you can install and reach a plot.
> - Download the dry-run artifacts on a clean machine → they run after the documented
>   unsigned-build steps.
> - Run the 10-file smoke ritual (docs/QUALITY.md §5), then tag `v0.1.0`.

---

## Done since the re-plan

- PR #225 — click a cluster next to a gap to fit it to the viewport (last open M8 view item).

## v0.2 (not before v0.1 ships)

- Anomaly gutter + MAD outlier flagging → #236 · per-series unit label → #237
- Performance: #194, #195, #181, #226, #210, #211, #204, #175
- State timeline follow-ups: Compress with state lanes (#201), value-like text columns (#212)
- Signing (macOS notarization, Windows code signing; SignPath Foundation is a free option
  for Windows)
