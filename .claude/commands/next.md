---
description: Triage blocking issues, then ship the next roadmap item as a PR
---

Read `docs/ROADMAP.md` and CLAUDE.md.

## Step 1 — triage open issues first

Run `gh issue list --state open` and read the labels:

- `blocking-decision` → do NOT act on it, do NOT pick work depending on the decision it raises.
  This includes anything requiring an edit to `.github/workflows/**` or `deny.toml`: those files
  are hard-denied to you by design. Never resolve these yourself and never route around the guard
  with Bash cat/tee/sed. Tell me it is waiting on me.

- `blocking-autonomous` with no open PR → make the oldest one this task (one per invocation).
  FALLBACK: if once you start it turns out to need a hard-denied file (CI or deny.toml) or a product
  decision, then: relabel it `blocking-decision`, edit the issue body to include the exact
  ready-to-paste diff / file for me, comment why, and CONTINUE to the roadmap in this same session —
  do not stop having done nothing.

- Otherwise → first unchecked roadmap item with no open PR.

State what you picked and why. If nothing is actionable, say so and stop — do not invent work.

## Step 2 — do it

1. Confirm the golden tests / corpus cases that cover this. If none, write them FIRST and let them
   fail. Never write implementation before its test.
2. Implement on a `feat/<slug>` or `fix/<slug>` branch, respecting docs/ARCHITECTURE.md boundaries.
3. Verify: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
   `cargo deny check`. Indexing/decimation/DSP → `cargo bench` against docs/SPEC.md §5.
4. CHANGELOG.md [Unreleased]: plain-language entry for a human testing the app. Record assumptions.
5. `gh pr create`, describing what changes for the user and what you verified. Tick the roadmap item
   or reference the issue in the same PR.

## If you discover new work mid-task

Open a labelled issue. Rule: touches `.github/workflows/**` or `deny.toml` → `blocking-decision`,
and put the ready-to-paste diff in the body (I apply CI/deny changes by hand). Would change SPEC.md
or PRODUCT.md, or pick between product behaviours → `blocking-decision`. Pure technical prerequisite,
no denied file → `blocking-autonomous`. Neither → `backlog`. Unsure → `blocking-decision`.
Then keep going; do not widen this PR.

One item per PR. Do not merge — CI and I decide.