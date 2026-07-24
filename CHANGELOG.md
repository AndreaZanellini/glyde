# Changelog

All notable changes to Glyde are documented here.

This file is written for the **maintainer and users, not for developers**: describe
what changes for someone using the app, in plain language. It is the primary way the
maintainer reviews work without reading code. Every PR adds a line here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
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
