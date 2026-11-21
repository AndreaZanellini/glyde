# Security review before v0.1 (issue #233)

What was checked before Glyde went to external users, what was found, what
was fixed, and what was knowingly left as it is. Re-read this before changing
how a file or cache is read.

## Threat model

Glyde is a local desktop app. Its code makes no network connections. What can
hurt a user:

- **Untrusted files.** CSV, TSV, TXT and Parquet files, and Parquet dataset
  directories, come from colleagues, instruments and the internet. Opening
  one must never crash, hang or exhaust the machine. A release build aborts
  on any panic outside a guarded Parquet open (see below), so for a user a
  panic is a crash.
- **Glyde's own caches.** They sit in the OS cache directory, which anything
  running as the user can write. A reopen treats them as untrusted input too.
- **What Glyde leaves on disk.** The caches hold decoded copies of the user's
  data, and the logs hold paths, column names and some values.
- **The supply chain.** The dependencies, and the CI that builds the binaries
  users download.

Out of scope: an attacker who already runs code as the user (they can read
the source files directly), and the OS, GPU driver and windowing stack.

## Untrusted input

**Fuzzing.** Three `cargo-fuzz` targets in `crates/glyde-core/fuzz/` drive
the public open API end to end: `csv_open`, `parquet_open` and
`cache_reopen`. How to run them, and what to do with a finding:
[`fuzzing.md`](fuzzing.md).

- **Found and fixed:** a Parquet footer whose column chunk starts at a
  negative offset reached an `assert!` inside the `parquet` crate
  (`ColumnChunkMetaData::byte_range`) and crashed the app. Every column
  chunk's byte range is now checked against the file's length when the
  footer is read, and a bad one is a reported error that names the file.
  Locked by torture-corpus case 64.
- **Found and fixed:** the whole-dataset cache sized an allocation from its
  stored row count before checking that the file held that many rows. A
  damaged or edited cache could crash the app with a capacity overflow.
  `ingest/dataset_cache.rs` now checks every count against the bytes the file
  really holds, and every stored index (NaN runs, outliers, skipped rows, the
  time column) against the row and column counts, before trusting any of
  them. Locked by
  `a_cache_whose_metadata_disagrees_with_its_arrays_falls_back_to_parsing`.
- **Found and contained:** a data page that claims a dictionary its column
  chunk does not have makes the `parquet` decoder `panic!` ("Decoder for dict
  should have been set"). No footer check can see that without decoding every
  page. So every Parquet open now runs inside `glyde_core::panic_guard::catch`:
  a panic there becomes "could not read … as Parquet: its data is damaged
  (the Parquet reader failed on it; details are in the log)". The panic text
  itself goes to the `warn` log only, so a reworded panic in a future
  `parquet` release cannot change what users see or break the corpus. The
  app stays open. Locked
  by torture-corpus case 65.
  - For this, the release profile changes from `panic = "abort"` to
    `panic = "unwind"`. **Every other panic still ends the process.** At
    startup `glyde-app` installs a hook that prints the usual report and
    then aborts, unless a guarded open is in progress. `glyde-app` refuses
    to compile under `panic = "abort"`, where nothing could be caught.
  - The guard counts open sections process-wide, because decoding runs on
    `rayon` workers and the hook fires on the thread that panicked. So an
    unrelated panic on another thread *during* a Parquet open unwinds that
    thread instead of aborting the process. A panic on the UI thread still
    ends the process.
  - Pre-validation stays where it is possible (the footer check above): it
    gives a message that says what is wrong with the file, not only that the
    decoder failed.
- **Found, not fixed:** a crafted 920-byte Parquet file whose dictionary page
  header claims an enormous value count makes the `parquet` decoder allocate
  more than 10 GB. An allocation failure aborts rather than panics, so the
  guard cannot catch it, and the count is in a page header, not the footer.
  The fix belongs upstream, or in a page-header pre-pass: #274, with the
  reproducer.
- **Found, not fixed:** four crafted ~900-byte Parquet files make the open
  spin for minutes inside the `parquet` crate's page-header reader. The UI
  stays responsive, but that open never finishes and Cancel cannot interrupt
  it: #275, with a reproducer.
- **What ran, on 2026-11-21** (a 4-core sandbox, in parallel): `csv_open` 45
  min, 274 450 inputs, clean. `cache_reopen` 45 min, 90 411 inputs, clean.
  `parquet_open` ran in four rounds: 1 min and about 10 min until the
  two crashes, each fixed before the next round; then 16 min until the
  over-allocation (#274); then 30 min that skipped out-of-memory inputs
  (3.5 million inputs), which found the hangs (#275) and no further crash. The
  issue asked for several hours per target; those runs are #277, due before
  v0.1.0 is tagged.

**Partitioned directories** (`ingest/parquet.rs`, `index/level0.rs`). Both
walks are iterative, so a very deep tree cannot overflow the stack. Symlinked
directories are not followed, so a link cycle cannot loop. A link that points
nowhere, a link to itself, a 200-level-deep tree and 2 000 empty folders
next to the data do not stop it from opening
(`tests/untrusted_directory_integration.rs`). A `.parquet` link that points
nowhere is an error naming that file. There is no cap on the number of files:
each Parquet footer is held in memory for the whole open, so a directory of
very many tiny files costs memory in proportion to the count. This is
accepted for v0.1.

## `unsafe` and caches

Every `unsafe` in `glyde-core/src` was reviewed:

| Where | What | Verdict |
|---|---|---|
| `ingest/csv.rs` `map_file` | maps the source file read-only for one parse | sound; see SIGBUS below |
| `index/level0.rs` `map_file` | maps a Level-0 cache file | sound: magic, version and a length that is a whole number of elements are checked before the bytes are cast; the 16-byte header keeps `i128` data aligned in a page-aligned map |
| `index/spill.rs` `map_spill_file` | maps a spill file this open just wrote | sound: magic, version, element width and exact length are checked |
| `ingest/dataset_cache.rs` `Pod for PackedTimestamp` | raw bytes ↔ timestamps | sound: `repr(C)`, integers only, no padding (asserted at compile time: 32 bytes) |
| `index/disk_pyramid.rs` `Pod for PackedBucket` | raw bytes ↔ pyramid buckets | sound: `repr(C)`, numbers only, no padding (asserted: 80 bytes) |

The remaining `unsafe` blocks are in tests and benches (a counting allocator,
`getrusage`, `msync`, `posix_fadvise`).

**Cache validation.** Every cache that a later open reuses is validated
before use, and a failed check is a cache miss: logged at `warn`, rebuilt
from the source, never trusted.

- Whole-dataset cache: magic, version, exact length, every count against the
  bytes remaining, every index against the data, UTF-8 for strings.
- Level-0 caches: magic, version, whole number of elements, and timestamps
  and values agreeing on the sample count.
- Pyramid caches: magic, version, level and bucket counts against the file
  size before any allocation.

The `cache_reopen` fuzz target replaces each of these files with arbitrary
bytes and reopens. What is **not** checked is whether well-formed values are
the *right* values: there is no checksum. Writes are atomic (temp file,
`fsync`, rename), so a crash mid-write cannot leave a torn cache. Silent disk
corruption or a hand edit could still show wrong numbers. Whether to add a
checksum is a speed-against-certainty choice for the maintainer: #273.

**A file truncated while mapped (SIGBUS).** On macOS and Linux, if another
program truncates a CSV *while* Glyde is parsing it in memory, reading a
mapped page that no longer exists kills the process with `SIGBUS`.
Appending to the file is safe. Windows refuses to truncate a mapped file.
Spilled opens read through buffers and get an ordinary error. Documented
here, and left for the maintainer to decide on: #272.

**Owner-only directories.** The cache root and the app-data root (where the
logs are) are created with mode `0700`, or tightened to it at startup if
they already exist, which is logged (`index/private_dir.rs`, called from
`glyde-app`'s `main` and `logging`). On Windows they live under the per-user
`%LOCALAPPDATA%`, which is already private to its owner, so nothing changes
there.

## Supply chain and release

- `cargo deny check`: advisories, bans, licenses and sources all ok. Its two
  yanked crates, `clipboard-win` 5.4.1 and `async-executor` 1.14.0, are
  updated to 5.4.3 and 1.14.1.
- `cargo audit`: the same two `quick-xml` advisories (RUSTSEC-2026-0194,
  -0195) that `deny.toml` already ignores with a written reason. `quick-xml`
  runs only at build time, inside the `wayland-scanner` proc-macro, never on
  user data. The other findings are "unmaintained" warnings (`chrono`,
  `chrono-tz`, `paste`, …) with no known vulnerability.
- CI: pin every action by commit SHA, verify `appimagetool`'s checksum, and
  give each job the least token permission it needs. These are workflow
  edits, so they are a ready-to-paste patch for the maintainer: #271.
- Secret scan of the full git history (all 184 commits, every branch): a
  pattern scan of `git log -p` for cloud, GitHub, Slack, npm and AI-provider
  tokens, private-key blocks and `password=`/`token=`-style assignments, plus
  a check of every file name ever committed (`.env`, `*.pem`, `*.key`,
  `id_rsa`, `settings.local`). Nothing found. A dedicated scanner (gitleaks,
  trufflehog) was not run.

## Privacy

The log names every file opened, its columns, and some of its values, for
example a timestamp that could not be read. Both issue templates that ask for
the log now say so, and ask the reporter to redact anything confidential.
They also give the log's real location on each OS, which was wrong for
macOS and Windows.
