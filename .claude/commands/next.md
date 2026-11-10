---
description: Triage blocking issues, then ship the next roadmap item as a PR
model: opus
effort: medium
---

Read `docs/ROADMAP.md` and CLAUDE.md.

## Step 0 — one PR at a time

Run `gh pr list --state open`. If any PR is open, stop: say which one is waiting on me and do
nothing else. Exception: I explicitly asked for a batch in this session — then stack the PRs in
merge order and state that order.

## Step 1 — triage open issues first

Run `gh issue list --state open` and read the labels:

- `blocking-decision` → do NOT act on it, do NOT pick work depending on the decision it raises.
  This includes anything requiring an edit to a hard-denied file (list below). Never resolve
  these yourself and never route around the guard with Bash cat/tee/sed. Tell me it is waiting
  on me.

- `blocking-autonomous` → make the oldest one this task (one per invocation).
  FALLBACK: if once you start it turns out to need a hard-denied file or a product decision,
  then: relabel it `blocking-decision`, edit the issue body to include the exact ready-to-paste
  diff / file for me, comment why, and CONTINUE down this list in this same session — do not
  stop having done nothing.

- `ready` → an issue I already decided, with the decision in a comment. Make the oldest one this
  task and implement exactly what the decision comment says. Same FALLBACK as above.

- Otherwise → first unchecked roadmap item.

State what you picked and why. If nothing is actionable, say so and stop — do not invent work.

## Hard-denied files

`.github/workflows/**`, `deny.toml`, `docs/SPEC.md`, `docs/PRODUCT.md`, `docs/QUALITY.md`,
`CLAUDE.md`. I apply changes to these by hand.

- CI or `deny.toml` change needed → `blocking-decision` issue with the ready-to-paste diff.
- A `ready` issue whose decision changes SPEC/PRODUCT/QUALITY → implement the code, and put the
  exact spec patch in the PR description under "Spec patch for the maintainer". Do not edit the
  spec file.

## Step 2 — do it

1. Confirm the golden tests / corpus cases that cover this. If none, write them FIRST and let them
   fail. Never write implementation before its test.
2. Implement on a `feat/<slug>`, `fix/<slug>` or `chore/<slug>` branch — rename the branch if the
   session started on an auto-named one. Respect docs/ARCHITECTURE.md boundaries.
3. Verify: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
   `cargo deny check` (if it is not installed, say so in the PR; CI runs it).
   Indexing/decimation/DSP → run the benches that cover the change (`cargo bench --bench <name>`)
   against docs/SPEC.md §5, and report the numbers. The full suite writes multi-GB fixtures: run
   it only when the change is broad.
4. CHANGELOG.md [Unreleased]: plain-language entry for a human testing the app. Record assumptions.
5. `gh pr create`, describing what changes for the user and what you verified. Tick the roadmap item
   or reference the issue in the same PR.
6. Watch CI on the PR (`gh pr checks --watch`). CI's clippy is newer than the local one. If CI
   fails because of this change, fix it with a follow-up commit (never force-push) and watch
   again. If it fails for another reason, say which job and why.

## If you discover new work mid-task

Open a labelled issue. Rule: touches a hard-denied file → `blocking-decision`, with the
ready-to-paste diff in the body. Would change SPEC.md or PRODUCT.md, or pick between product
behaviours → `blocking-decision`. Pure technical prerequisite, no denied file →
`blocking-autonomous`. Neither → `backlog`. Unsure → `blocking-decision`.
Never apply `ready` yourself: only I do, when I record a decision.
Then keep going; do not widen this PR.

One item per PR. Do not merge — CI and I decide.
