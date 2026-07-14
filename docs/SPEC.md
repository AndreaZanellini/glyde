# Glyde — Technical Specification (v1)

This document is the source of truth for *what* Glyde must do. Where it is silent, follow `CLAUDE.md` §"When requirements are ambiguous".

Priority order when requirements conflict: **fidelity > transparency > speed > convenience**.

---

## 1. Ingestion

### 1.1 Supported inputs (v1, frozen)

| Format | Notes |
|---|---|
| `.csv`, `.tsv`, `.txt` | Any delimiter, any decimal separator, any common encoding |
| `.parquet` | Columnar, may be multi-file/partitioned directory |

Readers sit behind a stable `Reader` trait (see `ARCHITECTURE.md`). Adding a format must never require changes outside its own module.

### 1.2 Inference (automatic, visible, correctable)

At open time, Glyde infers, in this order, from a bounded head sample (default 1 MB, never the whole file):

1. **Encoding** — UTF-8 (with/without BOM), UTF-16, Latin-1, Windows-1252. Use `encoding_rs` with BOM sniffing then heuristic. Invalid byte sequences are replaced, never fatal.
2. **Delimiter** — `,`, `;`, `\t`, `|`, whitespace. Chosen by column-count consistency across sampled lines, not by frequency alone.
3. **Header** — detect 0..N leading non-data lines (metadata preambles are common). The header row is the last non-data line whose field count matches the data rows.
4. **Decimal separator** — `.` or `,`. Must be resolved *jointly* with the delimiter: `1,5;2,3` is two columns with comma decimals, not four columns.
5. **Time index column** — see §2.
6. **Column dtypes** — see §1.4.

**Mandatory UX rule:** whatever is inferred is displayed in a persistent, discreet inference bar showing: encoding, delimiter, decimal separator, time column, detected timestamp format, sample count, and sampling classification. Each is correctable in one click; correcting triggers a re-index. **Silent guessing is a bug**, even when the guess is right.

Confidence is tracked per inference. Anything below the confidence threshold opens the inference bar expanded on first render, instead of collapsed.

### 1.3 Robustness (must never block the user)

- Non-ASCII / unit-of-measure characters in headers (`°C`, `µm/s²`, `Ω`) are preserved and displayed as-is. They must never cause a parse failure.
- Rows with wrong field count, unparseable values, or truncated tails: **skipped, counted, logged at `warn`, surfaced in the inference bar** ("142 rows skipped — view details"). Never abort the load.
- A file that is still being written (truncated last line) opens normally, minus the incomplete line.
- NaN / missing values: preserved as gaps in the series, **never interpolated**, rendered as a visible discontinuity plus a marker in the anomaly gutter.
- Outliers are **never removed or clipped**. They are detected (robust MAD-based rule, threshold documented in code) and only *flagged* in the anomaly gutter so the user can jump to them. The plotted data always contains them.
- Malformed user data must never `panic!`. Ingestion returns errors as values.

### 1.4 Data types

Preserve the source dtype end-to-end. Supported: `bool`, `i8..i64`, `u8..u64`, `f32`, `f64`, `string`/categorical.

- Never upcast silently in a way that loses precision. `i64` values beyond ±2⁵³ must not be converted to `f64` for rendering without being flagged in the log and in the inference bar.
- `f32` data stays `f32` in storage; DSP promotes to `f64` internally *for computation only*, which is lossless and must be documented in the code.
- `bool` and `string`/categorical series are never rendered as numeric line plots → they route to the **state timeline** (§4.3).
- Constant or single-sample series are valid inputs and must render (single-sample series → marker, §4.3).

---

## 2. Time axis

The time index is the foundation of the product: without an index, there is no time series.

### 2.1 Index detection

Accepted index kinds:
- **Progressive numeric** — monotonic integer/float sequence with no absolute time meaning.
- **Absolute timestamp** — must support at minimum: ISO 8601 / RFC 3339 (with and without timezone), `YYYY-MM-DD HH:MM:SS[.fff…]`, `DD/MM/YYYY HH:MM:SS`, `MM/DD/YYYY HH:MM:SS`, epoch seconds / milliseconds / microseconds / nanoseconds (integer or float), LabVIEW/NI epoch, Excel serial date.

**Ambiguity rule (`01/02/2026`):** never coin-flip. Resolve by scanning enough rows to find a discriminating value (a field > 12 settles day-vs-month). If the whole column is ambiguous, pick ISO-leaning default (`DD/MM` if locale-agnostic evidence is absent), mark the inference **low confidence**, and open the inference bar expanded with a one-click swap. Log the decision.

**Resolution:** internal time representation must be at least **nanosecond-precision integer** offsets plus an epoch anchor. Sampling periods range from picoseconds to years, so: store the native tick resolution declared by the source; use `i128` nanoseconds or an explicit `(i64 value, TimeUnit)` pair. Never store absolute time as `f64` seconds — it silently loses sub-microsecond resolution over long spans. Sub-nanosecond sources (picoseconds) are stored in their native unit and only converted for display.

- Non-monotonic timestamps: detected, counted, logged. Rows out of order are **not** reordered silently; the inference bar reports "timestamps not monotonic — [Sort] / [Keep as-is]".
- Duplicate timestamps: preserved, flagged.
- Timezone: if the source carries one, honor it and display it. If not, treat as naive local time and label it as such in the axis. Never invent a timezone.

### 2.2 Sampling classification

At index time, per series, compute the distribution of Δt and classify:

| Class | Rule | Consequence |
|---|---|---|
| `Uniform` | jitter (robust CV of Δt) ≤ 1% of median Δt | Full DSP available |
| `SegmentedUniform` | uniform within contiguous segments separated by gaps (gap = Δt > 10× median Δt) | DSP available per segment (§3.3) |
| `Irregular` | neither of the above | PSD disabled with explanation (§3.3) |

The classification and the estimated sampling rate are always shown in the inference bar.

### 2.3 Gap handling (decision)

Gaps are detected at index time (`Δt > 10 × median Δt`).

- **Default: truthful linear time axis.** A gap looks like a gap. This is the fidelity-first default and it is never overridden automatically.
- **One-click escape hatch:** when detected gaps account for > 50% of the total time span, a discreet affordance appears — `N gaps detected · [Compress]`. Toggling it switches to a **compressed (broken) axis** where gap regions are collapsed to a fixed narrow width marked with an unmistakable break glyph, and the axis labels make the discontinuity explicit.
- Compressed mode is a **view state only**. It never affects data, selections in real time, or DSP. PSD always uses real timestamps.
- Zooming into a single cluster must be trivial: clicking a gap-adjacent cluster fits it to the viewport.

---

## 3. Signal processing (the differentiator)

Every algorithm here is locked by a golden test written before its implementation (see `QUALITY.md`). **Never modify a golden test to make code pass.**

### 3.1 Decimation for rendering (decision)

Rendering N samples into P pixel columns where N ≫ P uses **min/max aggregation per pixel bucket**, and nothing else:

- For each pixel column, compute `min` and `max` of the raw samples whose timestamps fall in that column's time range; draw the vertical extent between them.
- This preserves every peak and every outlier: a spike one sample wide is always visible. Averaging (destroys peaks), naive subsampling (aliasing), and LTTB (can drop true extrema) are **forbidden** for the time-domain view.
- Precomputed min/max pyramids are built at index time (see `ARCHITECTURE.md` §Index) so that any zoom level resolves in O(pixels).
- When the visible range contains fewer samples than pixels, draw the **raw samples** with visible point markers — the user must be able to reach the individual sample. Zooming in far enough always converges to the true samples.
- The pyramid must be exact: level *k+1* min/max derives from level *k* min/max, never from resampled values.

### 3.2 PSD (Welch)

- Method: **Welch's averaged modified periodogram**.
- Defaults chosen by the software, exposed as *at most* three controls (window, segment length, overlap) that are hidden behind a single "PSD settings" affordance and are never required to get a correct first result. Defaults: Hann window, segment length = largest power of two ≤ N/8 clamped to [256, 65536], 50% overlap.
- **Correctness requirements (all golden-tested):**
  - Proper window power normalization (one-sided PSD in units²/Hz; the sum over bins × Δf must equal signal variance for a broadband signal, within tolerance).
  - Mean removal per segment (detrend = constant) by default, documented.
  - Correct one-sided scaling (×2 for all bins except DC and Nyquist).
  - Amplitude of a known sinusoid recovered within tolerance; frequency exact to bin resolution.
- **PSD is always computed on the raw samples of the selected interval — never on decimated/pyramid data.** If the selected interval contains more samples than the memory budget allows, compute Welch **streaming** (accumulate segment periodograms while reading), never by loading everything.

### 3.3 PSD on non-uniform data (decision)

| Class | Behavior |
|---|---|
| `Uniform` | Welch directly on raw samples. |
| `SegmentedUniform` | Welch computed **per contiguous segment**; no analysis window may ever cross a gap. If the user's selection spans multiple segments, average the segment periodograms (weighted by segment length) and label the plot explicitly ("averaged over N segments"). Segments shorter than one window are excluded and reported. |
| `Irregular` | PSD is **disabled**, with a clear explanation: PSD requires uniform sampling; this series has irregular timestamps. Offer the largest uniform sub-range if one exists. **Never silently resample and present the result as a PSD.** Lomb–Scargle for irregular data is a tracked post-v1 issue. |

Resampling to force uniformity is forbidden in v1.

---

## 4. Visualizations (exactly three)

### 4.1 Time domain

- One or many numeric series; either overlaid on one plot or stacked on multiple plots **sharing a single, synchronized time axis**.
- Pan, zoom (wheel/pinch, box-select), fit-to-data.
- Cursor readout showing exact raw value(s) and timestamp at the cursor.
- Anomaly gutter: a thin strip under the plot marking NaN runs, skipped rows, and flagged outliers, clickable to jump to them.
- Per-series unit label taken from the header if present (`°C`, `m/s²`), never parsed into a unit system — displayed verbatim.

### 4.2 PSD

- Spectrum of the whole signal or of the selected time interval; selection in the time view drives the PSD view.
- Multiple spectra overlaid on one plot or stacked, **sharing a synchronized frequency axis**.
- Log/linear toggles on both axes (this is the one place where a toggle is worth its pixels).
- The plot always states what was computed on: number of samples, segments, window, Δf.

### 4.3 State timeline

For non-numeric and event-like data, time-aligned with §4.1:

- **Boolean flags** → horizontal bands (on/off), not step plots on a numeric axis.
- **String / categorical series** → labeled state bands covering the interval each value holds; label rendered when the band is wide enough, tooltip otherwise.
- **Markers** (single samples at an instant) → tick glyphs on their own lane, always visible regardless of zoom (never decimated away).

Decimation rule for this view: a band or marker **never disappears** because of zoom level. When multiple states collapse into one pixel column, render a "multiple states" glyph that expands on zoom. Losing an event is a bug.

---

## 5. Performance (contract, enforced by CI)

Reference machine: **MacBook Air M1 (2020), 16 GB**. Budgets are enforced by benchmarks in CI (`QUALITY.md` §Benchmarks); breaching one fails the build.

| Metric | Budget |
|---|---|
| Cold start to window visible | ≤ 500 ms |
| **First meaningful plot, any file size** | ≤ 2 s (progressive: render what is indexed, keep indexing in background) |
| Full index build, CSV | ≤ 30 s per 10 GB |
| Full index build, Parquet | ≤ 10 s per 10 GB |
| Pan/zoom frame time | p50 ≤ 16 ms, p99 ≤ 100 ms — the UI thread never blocks |
| PSD of a 10 M-sample selection | ≤ 1 s |
| **Peak RSS** | ≤ `min(25% of system RAM, 4 GB)` — hard cap |
| Release binary size | ≤ 50 MB |

### 5.1 Resource-aware behavior

- Glyde knows system RAM at startup and computes its budget. It **checks affordability before acting**, never after.
- Data is memory-mapped and read in bounded chunks. The full file is never loaded.
- The index (§ARCHITECTURE) is bounded and, if it would exceed budget, is spilled to a cache file next to the app's data dir — never to the user's directory.
- If an action cannot fit in budget, Glyde **refuses it with a clear explanation and offers the affordable alternative** (e.g. "PSD over the full 8-hour range needs streaming — computing progressively"). It never attempts an action that would OOM the machine.
- Freeze or crash on a large file is the single most serious class of bug in this product.

---

## 6. Application behavior

- Single window, single file at a time (v1). Drag-and-drop and File→Open both work.
- Light/dark follows the OS. No theme settings.
- **Logging** via `tracing`: rotating file log in the OS-standard app data dir + console. `info` for every inference decision and user-visible action, `warn` for absorbed anomalies, `error` for failures. A user reporting a bug must be able to attach one log file that explains what happened.
- Crash-free target: any file in the torture corpus, any interaction sequence.

---

## 7. Distribution

- CI produces, for every tagged release: macOS arm64 `.dmg`, Windows x64 installer + portable `.exe`, Linux x64 `AppImage` + tarball.
- Binaries must run with **no runtime, no toolchain, no dependencies** to install.
- **Signing:** required before the repo goes public — macOS signing + notarization (Apple Developer account), Windows code signing (or, at minimum, winget distribution to soften SmartScreen). Until then, release notes must include the exact "unsigned build" workaround steps for macOS and Windows. A first-run security warning is a product bug, tracked as a release blocker for going public.
- No auto-update in v1 (tracked as post-v1 issue).
