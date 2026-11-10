---
description: Adversarial review of a pull request against the project's promises
argument-hint: "[PR number]"
model: opus
effort: medium
---

You are reviewing a pull request as an adversary, not its author. Assume the author was
optimizing to make tests pass; find where that diverged from what the project promises. Cite lines.

Which PR: `$ARGUMENTS` if given; else the PR named in the trigger context; else the only open PR
(`gh pr list --state open`). If that is ambiguous, say so and stop.

## Step 1 — read, in full, before judging

CLAUDE.md, docs/SPEC.md, docs/QUALITY.md, then the whole diff (`gh pr diff <n>`) and the PR
description. Check out the PR head so you can run it.

## Step 2 — evidence you must produce

A review that skips these is not a review. Put the results in the comment.

- List every changed file under `crates/glyde-core/tests/golden/**`, `testdata/corpus/**`, and
  every changed benchmark ceiling or test tolerance, by name. "None" is a valid answer, stated.
- Run the tests of every crate the PR touches (`cargo test -p <crate>`) and report pass/fail
  counts. If the PR claims a new test covers the change, check that the test fails without the
  change (revert the implementation hunk locally, run that test, restore).
- If the PR touches indexing, decimation, DSP or rendering, find the benchmark numbers in the PR
  description. Missing numbers are a finding.
- Name the behaviour the PR changes that no test exercises, if any.

## Step 3 — check, in order

1. Did this PR weaken, delete, loosen a tolerance on, or skip any golden test or corpus
   expectation? The single most serious finding. Flag it loudly.
2. Does any DSP change match its textbook definition, or was it fitted to the test?
3. Is any raw sample mutated, interpolated, upcast with precision loss, or reconstructed from
   the pyramid? PSD reads raw samples only.
4. Does anything inferred get applied silently, without surfacing in the inference bar?
5. Does anything load an unbounded amount of the file, or exceed SPEC §5 budgets?
6. Scope beyond docs/PRODUCT.md §Scope, or beyond the one item/issue this PR claims?
7. Did this PR resolve a `blocking-decision` issue, or edit `.github/workflows/**`, `deny.toml`,
   `docs/SPEC.md`, `docs/PRODUCT.md`, `docs/QUALITY.md` or `CLAUDE.md`? Those are
   maintainer-only. If an agent PR touched them, that is a red flag — request changes.
   Also flag, in the first line, any change under `.claude/**` or to `docs/ROUTINES.md`: these
   are the rules the agents and this review run by. Such a change is legitimate only when the
   maintainer asked for it; say what it loosens.
   (Implementing a `ready` issue is fine: check the code does what the decision comment says, and
   that any spec change is a patch in the PR description, not an edit.)
8. Is the CHANGELOG entry honest and readable by a non-programmer? Assumptions listed?

## Step 4 — post one review comment

First line: the verdict. If you find a golden-rule violation, a resolved `blocking-decision`
issue, or an agent edit to a maintainer-only file, say so there and request changes. Then the
Step 2 evidence, then findings with file and line. If clean, say so in one or two sentences after
the evidence. Do not pad.

Do not fix, approve, or merge. Review only. Leave the working tree as you found it.
