# Glyde — Product Definition

## Vision

**Glyde** is a standalone time-series visualizer built for one experience: open a file containing time series and start exploring it within seconds — scrolling, zooming, comparing — with total fluidity and total respect for the data, regardless of file size or format quirks.

The name comes from *to glide*: moving frictionlessly through a time series, like panning a map, whether the file is 2 MB or 20 GB.

Glyde is deliberately **not** a general-purpose plotting tool. It has no forest of buttons and settings. Configuration problems are solved inside the software, not delegated to the user. It does a small set of things at an obsessive level of quality.

## Users

Engineers and scientists who routinely inspect measurement data: vibration, acoustics, current, temperature/pressure/flow, machine states, process logs. They value speed, correctness, and zero friction over configurability.

## Product pillars

1. **Instant access** — any supported file opens and shows a first meaningful plot in seconds, independent of file size, via background indexing and progressive loading.
2. **Frictionless navigation** — pan/zoom is always fluid; the app pre-fetches and decimates intelligently so interaction never stalls.
3. **Maniacal signal fidelity** — every processing step for visualization (decimation, windowing, PSD) is textbook-correct and artifact-free. The raw sample is the reference and is always preserved and reachable.
4. **Resilient ingestion** — messy real-world files (weird encodings, `°C` in headers, NaNs, outliers, corrupted rows, gaps) never block the user. Problems are absorbed, flagged visibly, and never hidden.
5. **Honest transparency** — when the software infers something (delimiter, timestamp format, sampling rate), it shows what it inferred and lets the user correct it in one click. No silent guessing.

## Scope — v1 (frozen)

**Formats (Tier 1):**
- Delimited text: `.csv`, `.tsv`, `.txt` (any delimiter, any decimal separator, common encodings including Latin-1/Windows-1252, UTF-8 with/without BOM)
- `.parquet`

**Visualizations (exactly three):**
1. **Time domain** — one or more numeric series over time; multiple series on one plot or on stacked plots sharing the time axis.
2. **PSD (frequency domain)** — Welch PSD of a whole signal or of a selected time interval; multiple spectra on one plot or on stacked plots sharing the frequency axis.
3. **State timeline** — boolean flags, categorical/string series, and sparse markers rendered as horizontal state bands / event ticks, time-aligned with the numeric plots.

**Core capabilities:**
- Automatic detection of the time index (numeric progressive or timestamp in common formats), with visible, correctable inference.
- Gap-aware time axis (see SPEC §Gaps).
- NaN/outlier flagging with markers; they never block rendering.
- Interval selection for both zooming and PSD computation.
- Resource-aware operation: the app monitors its memory use and never exceeds its budget (see SPEC §Performance).

**Platforms:** macOS (Apple Silicon), Windows 11 (x64), Linux (x64). Identical behavior on all three.

**UI language:** English only.

## Explicit non-goals for v1 (do not build these)

- Formats beyond Tier 1 (`.xlsx`, `.tdms`, proprietary logs → Tier 2, tracked as issues; the reader layer is plugin-shaped to welcome them later)
- Spectrograms / time-frequency views (natural v2 candidate, not v1)
- Any data *editing*, exporting, or transformation features
- Dashboards, saved workspaces, multi-file sessions
- Theming/customization settings beyond light/dark following the OS
- Scripting or plugin APIs

## Success criteria for v1

- The maintainer's own daily files open and are explorable with zero manual configuration in ≥ 90% of cases; the remaining cases are fixable with one correction click.
- The full torture corpus (docs/QUALITY.md) opens without crash: 100%.
- Performance budgets (docs/SPEC.md §Performance) hold on the reference machine (MacBook Air M1, 16 GB).
- A newcomer can download a release binary and reach a first plot without reading any documentation.

## Future direction (context, not tasks)

The repository will eventually become public and accept external contributions. This is why: license and CONTRIBUTING.md exist from day one, readers sit behind a stable trait, and the issue tracker has a `file-wont-open` template that feeds the torture corpus.
