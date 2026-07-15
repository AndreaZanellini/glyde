---
description: Pick the next unchecked ROADMAP item and ship it as a PR
---

Read `docs/ROADMAP.md` and find the first unchecked item that has no open PR.

Then, for that item only:

1. State which item you picked and why it is the next one. If ROADMAP.md is missing or
   every item is checked, say so and stop — do not invent work.
2. Confirm the golden tests and corpus cases that already cover this item. If none exist,
   write them FIRST and let them fail. Never write implementation before its test.
3. Implement it on a `feat/<slug>` branch. Follow docs/ARCHITECTURE.md module boundaries.
4. Verify: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`.
   If you touched an indexing, decimation or DSP path, run `cargo bench` and compare against
   the budgets in docs/SPEC.md §5.
5. Add a plain-language entry to `CHANGELOG.md` under `[Unreleased]`. This is what the
   maintainer reads instead of the code — write it for a human testing the app, not a reviewer.
6. Record any assumption you had to make under "Assumptions made" in CHANGELOG.md, per
   CLAUDE.md §"When requirements are ambiguous".
7. Open a PR with `gh pr create`. In the body, describe what changes for the user and list
   what you verified. Then tick the item in docs/ROADMAP.md as part of the same PR.

Scope discipline: one ROADMAP item per PR. If you discover adjacent work, open a GitHub
issue for it and keep going — do not widen this PR.

Do not merge the PR. CI and the reviewer decide; the maintainer merges.
