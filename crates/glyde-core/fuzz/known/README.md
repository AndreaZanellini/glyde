# Known fuzz findings that are not fixed yet

Each file here is a raw `parquet_open` fuzz input: its first byte selects the
in-memory (even) or spilled (odd) open, and the rest is the Parquet file
(drop the first byte to get the `.parquet`). They are kept so a fix can be
checked against every input the fuzzer found, not only the one in its issue.
Replay one with:

```sh
cargo +nightly fuzz run parquet_open known/274-dictionary-oom/<file> -- -rss_limit_mb=3072
```

| Folder | Issue | What it does | Where (parquet 60.1) |
|---|---|---|---|
| `274-dictionary-oom/` | #274 | allocates up to ~17 GB from a dictionary page header's value count (Bool, Int32, Int64 and Double columns; with Bool it is slow rather than huge) | `PlainDecoder::set_dict`, `encodings/decoding.rs:379` |

**In a release build, not only a fuzz build.** Measured on 2026-11-21 with
`ingest::open_dataset_progressive_in`, an optimized build outside the fuzz
harness, a 16 GB RAM budget, a 6 GB address-space cap and a 120 s limit per
input:

| Folder | parquet 60.1.0 | parquet 60.2.0 (what Glyde ships) |
|---|---|---|
| `274-dictionary-oom/` (33) | 25 abort the app; 8 refused, slowest 2.6 s | 17 abort the app; 16 refused, slowest 2.4 s |

So parquet 60.2.0 removes only some of the over-allocations. It removes every
hang of #275, whose 42 inputs are now torture-corpus cases 69–110. The same
counts (25 and 17 aborts) came out again on 2026-11-25, on macOS,
with a 6 GB cap on live heap.

Found by the #277 runs on 2026-11-21. When an issue is fixed, its inputs
become torture-corpus cases (see `fuzzing.md`) and its folder is deleted.
