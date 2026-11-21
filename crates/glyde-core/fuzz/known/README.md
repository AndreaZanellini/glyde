# Known fuzz findings that are not fixed yet

Each file here is a raw `parquet_open` fuzz input: its first byte selects the
in-memory (even) or spilled (odd) open, and the rest is the Parquet file. They
are kept so a fix can be checked against every input the fuzzer found, not
only the one in its issue. Replay one with:

```sh
cargo +nightly fuzz run parquet_open known/274-dictionary-oom/<file> -- -rss_limit_mb=3072
```

| Folder | Issue | What it does | Where (parquet 60.1) |
|---|---|---|---|
| `274-dictionary-oom/` | #274 | allocates 4.6–16 GB from a dictionary page header's value count (Int32, Int64 and Double columns) | `PlainDecoder::set_dict`, `encodings/decoding.rs:379` |
| `275-page-header-hang/` | #275 | spins for minutes in the Thrift page-header reader | `read_new_page` → `read_page_header_len` → `skip_till_depth` |

Found by the #277 runs on 2026-11-21. When an issue is fixed, its inputs
become torture-corpus cases (see `fuzzing.md`) and its folder is deleted.
