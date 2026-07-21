---
description: Triage blocking issues, then ship the next roadmap item as a PR
---

Read `docs/ROADMAP.md` and CLAUDE.md.

## Step 1 — triage open issues first

Run `gh issue list --state open` and look at the labels:

- Any `blocking-decision` issue → do NOT act on it, and do NOT pick roadmap work that depends
  on the decision it raises. Tell me it is waiting on me and, if I have not decided, stop here.
  Never resolve a `blocking-decision` issue yourself, even if the fix looks obvious — the whole
  point is that I choose the product behaviour, not you.
- Any `blocking-autonomous` issue with no open PR → this is a prerequisite. Make the oldest one
  this task instead of a roadmap item (at most one per invocation), then continue from step 2
  treating it as the work.
- Otherwise → take the first unchecked roadmap item with no open PR.

State which you picked and why. If ROADMAP.md is missing or everything is checked/blocked, say
so and stop — do not invent work.

## Step 2 — do it

1. Confirm the golden tests / corpus cases that cover this work. If none exist, write them FIRST
   and let them fail. Never write implementation before its test.
2. Implement on a `feat/<slug>` or `fix/<slug>` branch, respecting docs/ARCHITECTURE.md boundaries.
3. Verify: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
   `cargo deny check`. If you touched indexing, decimation or DSP, run `cargo bench` against the
   docs/SPEC.md §5 budgets.
4. Add a plain-language entry to `CHANGELOG.md` under `[Unreleased]` — write it for a human
   testing the app, not a reviewer. Record any assumption under "Assumptions made".
5. Open a PR with `gh pr create`, describing what changes for the user and what you verified.
   Tick the roadmap item, or reference the issue so it auto-closes, in the same PR.

## If you discover new work mid-task

Open a GitHub issue and LABEL it. Use this rule: if resolving it would require editing SPEC.md or
PRODUCT.md, or choosing between two product behaviours, label it `blocking-decision`. If it is pure
technical work that blocks progress, `blocking-autonomous`. If it is neither urgent nor a decision,
`backlog`. When unsure, choose `blocking-decision` — asking me is always safe; deciding for me is not.
Then keep going; do not widen this PR.

Scope discipline: one item per PR. Do not merge — CI and I decide.
