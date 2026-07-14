# Glyde — Quality Gates

The maintainer does not read the code. **This document is the safety net.** Everything here is a build-blocking gate unless stated otherwise.

---

## 1. The torture corpus

The corpus is the most valuable asset in this repository — more than any feature. It lives in `testdata/corpus/`, each file paired with a `<name>.expected.json` describing what a correct open produces (inferred encoding/delimiter/decimal/time column/format, row count, skipped-row count, sampling class, gap count).

**Gate: every file in the corpus opens without panic, and matches its `.expected.json`. 100%, every PR.**

Build it *before* the features it protects. Minimum set for v1:

**Delimiters & locale**
1. `,` delimiter, `.` decimals, UTF-8, clean
2. `;` delimiter, `,` decimals (European) — the trap case
3. Tab-delimited
4. Whitespace-aligned `.txt`
5. `|` delimiter
6. Quoted fields containing the delimiter
7. Quoted fields containing newlines

**Encoding & characters**
8. Latin-1 header with `°C`, `µm/s²`
9. Windows-1252 with smart quotes
10. UTF-8 with BOM
11. UTF-16LE
12. Invalid byte sequences mid-file
13. Header with unit suffixes in brackets `Temp [°C]`

**Structure**
14. Metadata preamble (5 junk lines) before the header
15. No header at all (data from line 1)
16. Duplicate column names
17. Empty columns
18. Single column (index only — must fail with a clear message, not a crash)
19. Single data row
20. Trailing empty lines / trailing delimiter on every row
21. Ragged rows (wrong field count on some lines)
22. Truncated final line (file being written)
23. Empty file (clear message, no crash)

**Time index**
24. ISO 8601 with timezone
25. ISO 8601 naive
26. `DD/MM/YYYY` unambiguous (day > 12 present)
27. `MM/DD/YYYY` unambiguous
28. Fully ambiguous dates (all fields ≤ 12) → low confidence + expanded bar
29. Epoch seconds / 30. epoch ms / 31. epoch µs / 32. epoch ns
33. Excel serial dates
34. LabVIEW epoch
35. Progressive integer index (no absolute time)
36. Non-monotonic timestamps
37. Duplicate timestamps
38. Timestamps with jitter (quasi-uniform)
39. Irregular timestamps (event log) → `Irregular`, PSD disabled
40. Segmented: 3 bursts days apart → `SegmentedUniform`, 2 gaps
41. Picosecond-resolution index
42. Multi-year span, second resolution

**Values**
43. NaN runs
44. Infinities
45. Extreme outliers (1e12 next to unit values)
46. Mixed numeric/string in one column
47. Boolean column (`true/false`, `0/1`, `TRUE/FALSE`)
48. String state column (machine states)
49. Sparse markers (one sample every N thousand)
50. `i64` values beyond 2⁵³ (precision-loss flag case)
51. Constant series
52. All-NaN series

**Parquet**
53. Clean Parquet, timestamp column
54. Parquet with nulls
55. Partitioned Parquet directory
56. Parquet with dictionary-encoded strings

**Growth rule:** every `file-wont-open` issue must add its (anonymized) file to the corpus *with the fix in the same PR*. This is the product's growth engine.

---

## 2. Golden tests (signal processing)

In `crates/glyde-core/tests/golden/`. **Written before the implementation. Never modified to make code pass** — if a golden test fails, the code is wrong, or the test's numerical tolerance was wrong in a way the maintainer must approve explicitly in an issue.

### Decimation
- **Spike preservation**: 10 M samples of zeros with a single 1-sample spike of amplitude 1.0 → rendering into 800 pixel columns *must* show max = 1.0 in the correct column. A lost spike = failed test.
- **Envelope exactness**: for random data, min/max per bucket must equal the brute-force min/max over the same time range, exactly, at every pyramid level.
- **Pyramid consistency**: level *k+1* min/max == aggregation of level *k* min/max, exactly.
- **Convergence**: as zoom increases past 1 sample/pixel, rendered values equal raw samples exactly.
- **No aliasing**: 1 kHz sine sampled at 100 kHz, decimated to 500 columns → the rendered envelope is the true ±amplitude band, with no beat/moiré pattern (assert envelope constancy).

### Welch PSD
- **Sinusoid**: 1 V amplitude sine at exactly a bin center → peak at the exact frequency; recovered amplitude within tolerance; documented tolerance.
- **Parseval / power conservation**: white noise of known variance σ² → `Σ PSD × Δf ≈ σ²` within tolerance (this is the test that catches window-normalization bugs).
- **One-sided scaling**: DC and Nyquist bins not doubled; all others doubled.
- **Window normalization**: same signal, different windows (Hann/Hamming/rectangular) → same total power within tolerance.
- **Detrend**: signal with a large DC offset → no leakage skirt at low frequency beyond tolerance.
- **Two tones**: two sinusoids of different amplitudes resolve at their exact frequencies with correct relative levels.
- **Segmented data**: a signal split into 3 segments by gaps → PSD equals the length-weighted average of per-segment PSDs; **no window crosses a gap** (assert by construction: inject a discontinuity at the gap that would produce a broadband artifact if a window crossed it).
- **Raw-data guarantee**: computing a PSD from a fully-zoomed-out view must produce bit-identical results to computing it from a fully-zoomed-in view. This proves PSD never touches the pyramid.

### Time
- Every timestamp format in the corpus round-trips: parse → internal ticks → format → identical string.
- Nanosecond and picosecond precision preserved over a multi-year span (the test that forbids `f64` seconds).
- Gap detection matches hand-computed gaps on synthetic series.

### Property tests (`proptest`)
- Any random valid series: rendering never panics, never invents values outside `[min, max]` of the range.
- Any random byte sequence fed to the CSV reader: never panics (fuzz-lite).

---

## 3. Benchmarks (build-blocking)

`criterion` benches in `benches/`, run in CI. Budgets from `SPEC.md` §5. **Exceeding a budget fails the build.**

Reference machine for the official numbers: MacBook Air M1 16 GB. CI runners differ, so CI enforces:
1. **Absolute ceilings** on GitHub-hosted runners, calibrated per-OS with a documented margin over the SPEC budgets.
2. **Regression detection**: > 15% slowdown versus the `main` baseline fails the PR.

Benched paths: index build (CSV 1 GB / Parquet 1 GB), viewport query at each pyramid level, Welch on 1 M / 10 M samples, cold start, peak RSS during a scripted 20 GB open.

Large fixtures are generated by `testdata/generators/` in CI, never committed.

**Memory gate:** a scripted headless run opening the 20 GB synthetic file must keep peak RSS under the cap. Breaching = build failure. This is the anti-freeze/anti-crash gate and it is the most important benchmark in the suite.

---

## 4. CI matrix

Every PR, on `ubuntu-latest`, `macos-14` (arm64), `windows-latest`:

1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test --workspace` (unit + golden + corpus + property)
4. `cargo bench` gates (see §3)
5. Architecture check: no GUI crate appears in `cargo tree -p glyde-core`
6. `cargo deny check` (licenses/advisories) — required before going public
7. Build release artifacts (on tags: attach to the GitHub Release)

---

## 5. Manual QA ritual (the maintainer's job)

Automated tests cannot see a plot. Before every release, the maintainer performs and confirms:

1. Open **10 real work files** (the personal set, not the corpus). Each reaches a first plot with no manual configuration.
2. For a large file (> 5 GB): scroll and zoom continuously for 30 s — no stutter, no freeze, fan behavior sane, memory stable in Activity Manager/Task Manager.
3. Pick a signal with a known frequency content → PSD peak lands where physics says it should.
4. Zoom from full view down to individual samples on a noisy signal — no visual glitch, no popping, no disappearing spikes.
5. Open a file with gaps → gaps look truthful; `Compress` toggle works and is obviously a view change.
6. Open a file with boolean + string + marker series → state timeline is readable and time-aligned; no event vanishes at any zoom.
7. Open a deliberately broken file → it still opens, anomalies are flagged, nothing crashes.
8. Wrong inference case → the inference bar shows the mistake and one click fixes it.
9. Same file on macOS and on Windows → identical behavior.
10. Download the release binary on a clean machine → runs with no install of anything.

Any failure blocks the release and becomes an issue with a corpus file attached where applicable.
