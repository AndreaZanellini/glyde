# CLAUDE.md — Glyde Development Contract

This file governs how Claude Code works on this repository. Read it at the start of every session. The documents in `docs/` are the source of truth for *what* to build; this file defines *how* to build it.

## Context you must never forget

- **The maintainer does not read the code.** He validates behavior by using the app and by reading test results, benchmark results, and release notes. Therefore: tests are the only safety net, compiler warnings are errors, and anything not covered by a test does not exist.
- The product thesis is in `docs/PRODUCT.md`. Requirements are in `docs/SPEC.md`. Architecture constraints are in `docs/ARCHITECTURE.md`. Quality gates are in `docs/QUALITY.md`.
- Everything — code, comments, commit messages, docs, UI text — is in **English**.

## The 5 Golden Rules (non-negotiable)

1. **Never degrade the raw data.** Raw samples, their dtype, and their timestamps are immutable once ingested. Every visualization operates on derived views; the original is always recoverable. No lossy conversion is ever applied silently.
2. **Never guess silently.** When ingestion inference is uncertain (delimiter, decimal separator, timestamp format, encoding), pick the best hypothesis, but surface what was inferred in the UI and make it correctable in one click. Wrong-but-silent is the worst failure mode this product can have.
3. **Performance is a contract, not a feature.** The budgets in `docs/SPEC.md` §Performance are enforced by benchmarks in CI. If a change breaks a budget, the change is wrong, not the budget.
4. **Signal processing is done by the book.** Every DSP operation (decimation, windowing, PSD) must match its textbook definition and is locked by a golden test written *before* the implementation. Never modify a golden test to make an implementation pass.
5. **Scope is sacred.** v1 scope is defined in `docs/PRODUCT.md` §Scope. Do not add formats, visualizations, or settings beyond it, no matter how easy they seem. If something appears necessary, open a GitHub issue proposing it and wait for the maintainer's decision.

## Architecture constraints

- Cargo workspace with two crates: `glyde-core` (no GUI dependencies, 100% testable headless) and `glyde-app` (egui/eframe shell, as thin as possible). Module boundaries are defined in `docs/ARCHITECTURE.md` and must not be blurred.
- `glyde-core` must never depend on `glyde-app` or on any GUI/windowing crate.
- All heavy work (file indexing, reading, DSP) runs off the UI thread. The UI thread never blocks on I/O.
- New third-party dependencies require a one-line justification in the PR description. Prefer the ones already chosen in `docs/ARCHITECTURE.md`.

## Development workflow

- **Branching:** GitHub Flow. `main` is always releasable. Every change goes through a feature branch (`feat/...`, `fix/...`, `chore/...`) and a PR. Never commit directly to `main`.
- **Commits:** Conventional Commits (`feat:`, `fix:`, `test:`, `perf:`, `docs:`, `chore:`, `refactor:`).
- **Every PR must include:** the tests that cover the change, a passing `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, and unchanged-or-better benchmark results when touching core paths.
- **TDD for DSP and ingestion:** for anything in `glyde-core::dsp` or `glyde-core::ingest`, write the failing test first (golden test or torture-corpus case), then implement.
- **Issues:** track known limitations and deferred ideas as GitHub issues with labels (`bug`, `enhancement`, `format-request`, `file-wont-open`). Do not leave TODO comments in code as the only record of anything.
- **Logging:** use `tracing`. Every ingestion decision (inferred delimiter, encoding, timestamp format, detected gaps, dropped/flagged rows) is logged at `info`; recoverable anomalies at `warn`; never `panic!` on malformed user data.

## Definition of Done for any feature

1. Behavior matches `docs/SPEC.md`.
2. Covered by tests (unit + integration; golden test if DSP; torture-corpus case if ingestion).
3. `fmt`, `clippy -D warnings`, all tests, all benchmarks pass on the CI matrix (Linux, macOS, Windows).
4. Logging added for every decision the code takes on the user's behalf.
5. A human-readable note added to `CHANGELOG.md` under `[Unreleased]` — this is what the maintainer reads instead of the code.

## When requirements are ambiguous

Do not invent product decisions. If `docs/SPEC.md` does not answer a question, choose the interpretation that best serves the golden rules (fidelity > transparency > speed > convenience), implement it behind the smallest possible surface, and flag the assumption explicitly in the PR description and in `CHANGELOG.md` so the maintainer can veto it by testing the app.

## Release checklist (per tagged release)

1. All CI green on the 3-OS matrix, benchmarks within budget.
2. Binaries produced by CI for macOS (arm64), Windows (x64), Linux (x64) and attached to the GitHub Release.
3. `CHANGELOG.md` section finalized in plain language (the maintainer reads this).
4. Manual QA reminder printed in the release notes: "Maintainer: run the 10-file smoke ritual (docs/QUALITY.md §Manual QA) before announcing."
