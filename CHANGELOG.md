# Changelog

All notable changes to Glyde are documented here.

This file is written for the **maintainer and users, not for developers**: describe
what changes for someone using the app, in plain language. It is the primary way the
maintainer reviews work without reading code. Every PR adds a line here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
