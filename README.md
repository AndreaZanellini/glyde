<h1 align="center">Glyde</h1>
<p align="center"><em>Glide through your time series.</em></p>

<!-- TODO(release): replace with an animated GIF showing a 10 GB file opening and scrolling fluidly.
     This GIF is the single most important marketing asset of the project. -->

**Glyde** opens a file containing time series and lets you explore it immediately — scroll, zoom, compare — no matter how big the file is or how messy its contents are.

No configuration panels. No plotting scripts. Drop the file in and look at your data.

## Why

Inspecting measurement data usually means writing throwaway code: parse the file, guess the delimiter, fix the encoding, configure axes and colors, then watch your laptop freeze because the dataset does not fit in RAM. Glyde exists to make that entire ritual unnecessary.

## What it does

- **Opens anything reasonable** — CSV/TSV/TXT (any delimiter, any decimal separator, broken encodings, `°C` in headers, ragged rows) and Parquet. Weird files are absorbed, not rejected.
- **Handles size** — memory-mapped, progressively indexed, budget-aware. A 20 GB file opens as fast as a small one and never eats your RAM.
- **Respects the signal** — min/max decimation that never loses a spike, textbook Welch PSD computed on raw samples, nanosecond-and-below time precision. Fidelity is the point.
- **Tells you what it inferred** — delimiter, timestamp format, sampling rate. Wrong guess? One click to fix it. Never silent.
- **Three views, done properly** — time domain, PSD, and a state timeline for booleans, machine states, and markers, all time-aligned.

## Status

Early development. Not yet released.

## Install

Download a binary from [Releases](../../releases) — macOS (Apple Silicon), Windows (x64), Linux (x64). Nothing else to install.

## Documentation

- [`docs/PRODUCT.md`](docs/PRODUCT.md) — what Glyde is, and deliberately is not
- [`docs/SPEC.md`](docs/SPEC.md) — technical requirements
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — how it is built
- [`docs/QUALITY.md`](docs/QUALITY.md) — the quality gates
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to help

## A file that will not open?

That is the bug we care about most. [Open an issue](../../issues/new/choose) using the **"My file won't open"** template and attach an anonymized sample. Every such file becomes a permanent test case.

## License

Apache License 2.0 — see [`LICENSE`](LICENSE).

Apache-2.0 includes an explicit patent grant, which is what makes Glyde adoptable
inside the industrial R&D organizations its users work for.
