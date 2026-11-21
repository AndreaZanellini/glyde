# Fuzzing glyde-core

Glyde opens files it did not write: CSVs and Parquet files from anywhere, and
its own caches, which sit in a directory the user (or anything running as the
user) can edit. The fuzz targets in `crates/glyde-core/fuzz/` feed arbitrary
bytes to the same public entry points the app uses, and look for what a test
corpus cannot promise to find: panics, hangs, runaway allocations and
out-of-bounds reads. A release build aborts on any panic (`panic = "abort"`),
so for a user every one of these is a crash.

An `Err` is a correct answer to a malformed file. Only a crash is a finding.

## Targets

| Target | Input | What it drives |
|---|---|---|
| `csv_open` | first byte: in memory (even) or spilled to disk (odd); the rest: the file | `open_dataset_progressive_in` on a `.csv`, then the pyramid and Level-0 caches (or the disk pyramids when spilled) |
| `parquet_open` | same, as a `.parquet` | same |
| `cache_reopen` | first byte: which cache file; the rest: its new contents | a first open of a small, varied CSV writes the whole-dataset, Level-0 and pyramid caches; each run replaces one of them and reopens |

## Running

Needs a nightly toolchain and `cargo-fuzz` (`cargo install cargo-fuzz`).
From `crates/glyde-core/fuzz/`:

```sh
# Seed corpora from the torture corpus and the real cache files.
mkdir -p corpus/csv_open corpus/parquet_open corpus/cache_reopen
for f in ../../../testdata/corpus/*.csv; do
  { printf '\0'; cat "$f"; } > "corpus/csv_open/$(basename "$f")"
done
for f in ../../../testdata/corpus/*.parquet ../../../testdata/corpus/*/*/*.parquet; do
  { printf '\0'; cat "$f"; } > "corpus/parquet_open/$(basename "$(dirname "$f")")-$(basename "$f")"
done
GLYDE_FUZZ_WRITE_SEEDS=corpus/cache_reopen cargo +nightly fuzz run cache_reopen -- -runs=1

# Then, per target (here one hour, 4 GB RSS cap, 10 s per input):
TMPDIR=/dev/shm cargo +nightly fuzz run csv_open -- \
  -max_total_time=3600 -rss_limit_mb=4096 -timeout=10 -max_len=262144
```

`TMPDIR=/dev/shm` keeps the temp files every run writes off the disk.

## When it finds something

`cargo fuzz` writes the input to `artifacts/<target>/crash-…` and prints how
to replay it. Then:

1. Turn the input into a regression test **before** fixing anything: a
   torture-corpus case (`testdata/corpus/case-NN-…` with its
   `.expected.json`) for a source file, or an integration test next to
   `tests/dataset_cache_integration.rs` for a cache file.
2. Watch the test fail, fix the code, watch it pass.
3. Keep fuzzing until the target runs clean again.
