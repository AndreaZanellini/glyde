# Contributing to Glyde

Thanks for considering a contribution. Glyde has a narrow, deliberate scope — please read [`docs/PRODUCT.md`](docs/PRODUCT.md) before proposing features.

## The most valuable contribution: a file that won't open

If Glyde fails to open a file, opens it incorrectly, or infers something wrong, that is our highest-priority bug. Open an issue with the **"My file won't open"** template and attach an anonymized sample (truncated is fine — the first few thousand rows usually reproduce it). It becomes a permanent test case.

## Ground rules

- **Everything in English**: code, comments, commits, docs, UI.
- **Scope**: features outside `docs/PRODUCT.md` §Scope will be declined, however good. Open an issue to discuss before writing code.
- **Fidelity first**: priority order is *fidelity > transparency > speed > convenience*. A change that makes Glyde faster by approximating the data will not be merged.
- **No silent guessing**: anything the software infers must be visible and correctable.

## Workflow

1. Open an issue first for anything non-trivial.
2. Branch from `main`: `feat/...`, `fix/...`, `docs/...`, `chore/...`.
3. Commit with [Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `test:`, `perf:`, `docs:`, `refactor:`, `chore:`.
4. Open a PR. Describe *what changes for the user*, not just the code.
5. CI must be green on Linux, macOS and Windows.

## Before you push

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

## Tests are not optional

- **Ingestion change** → add a file to `testdata/corpus/` with its `.expected.json`.
- **DSP change** → a golden test, written *before* the implementation. See [`docs/QUALITY.md`](docs/QUALITY.md) §2. Golden tests are never edited to make code pass.
- **Performance-sensitive change** → benchmarks must stay within budget (`docs/SPEC.md` §5).
- Untested code is not merged.

## Adding a file format

Readers live behind the `Reader` trait. A new format touches only `crates/glyde-core/src/ingest/<format>.rs` plus a registry entry, and arrives with corpus files. Note that formats beyond Tier 1 are post-v1 — check the issue tracker first.

## Architecture

`glyde-core` holds all logic and has **no GUI dependencies**; `glyde-app` is a thin egui shell. If your behavior can be tested without a window, it belongs in core. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Licensing of contributions

Glyde is licensed under [Apache-2.0](LICENSE). Per section 5 of that license, any
contribution you intentionally submit for inclusion is licensed under the same terms,
with no separate CLA to sign. If your employer owns your work, make sure you have
clearance before submitting.

## Code of conduct

Be decent. Assume good faith. Technical disagreement is welcome; disrespect is not.
