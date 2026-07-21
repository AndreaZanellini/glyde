# Glyde — routine prompts

Paste these into claude.ai/code/routines → New routine. They run on Anthropic's cloud
infrastructure, so they work with the laptop closed. Routines run as full autonomous
sessions with no approval prompts, so the prompt must be self-contained and explicit
about what success looks like.

Repository: your `glyde` repo. Environment: Default (Trusted network is enough — crates.io
and github.com are in the default allowlist). Leave "Allow unrestricted branch pushes" OFF.

## The issue triage model these routines depend on

Every issue Claude opens MUST carry exactly one of three labels. This is what lets the
nightly routine know whether an issue is its job or yours:

- `blocking-autonomous` — something is broken or missing that blocks progress AND fixing it
  is pure technical work with NO product decision. Example: "the CSV reader panics on empty
  rows". The nightly routine does these FIRST, before the roadmap.
- `blocking-decision` — something blocks progress but resolving it requires a choice that
  belongs to the maintainer (anything that would change docs/SPEC.md or docs/PRODUCT.md, or
  pick between product behaviours). Example: "spec doesn't say whether non-monotonic
  timestamps should be sorted or preserved". The routine must NOT act on these — it flags
  them and works around them.
- `backlog` — noted for later, not blocking anything now.

The rule of thumb the routines use: **if resolving the issue would require editing SPEC.md or
PRODUCT.md, or choosing between two product behaviours, it is `blocking-decision`, never
`blocking-autonomous`.** When unsure, default to `blocking-decision` — it is always safe to
ask the maintainer, never safe to silently decide product behaviour.

---

## Routine 1 — "Build the next thing" (schedule: nightly)

    Read CLAUDE.md and docs/ROADMAP.md.

    STEP 1 — TRIAGE OPEN ISSUES BEFORE TOUCHING THE ROADMAP.
    List open issues with `gh issue list --state open`. Then:

    - If any issue is labelled `blocking-decision`: do NOT attempt it and do NOT work on any
      roadmap item or issue that depends on the decision it raises. If independent roadmap
      work exists, proceed to that in step 2. If everything remaining depends on a pending
      decision, stop and post a single comment on the blocking issue restating that it needs
      the maintainer, then end the session. Never resolve a `blocking-decision` issue yourself,
      even if the fix seems obvious — the point is that the maintainer chooses, not you.

    - If any issue is labelled `blocking-autonomous` and has no open PR: pick the oldest one and
      make IT this session's task instead of a roadmap item. It is a prerequisite; clearing it
      unblocks roadmap progress. Cap: at most ONE such issue per session, so the roadmap remains
      the backbone and you don't spend the session generating and clearing your own work.

    - Otherwise: proceed to the roadmap.

    STEP 2 — DO THE WORK (either the one blocking-autonomous issue from step 1, or the first
    unchecked roadmap item that has no open PR).

    Follow CLAUDE.md's golden rules and docs/ARCHITECTURE.md's module boundaries.

    Test-first is mandatory: for anything in glyde-core::dsp or glyde-core::ingest, the golden
    test or corpus case must exist and fail before you write the implementation. Never weaken an
    existing golden test to make code pass — if one seems wrong, stop, open a `blocking-decision`
    issue explaining why, and pick different work.

    If, while working, you discover something out of scope, open an issue for it and LABEL IT
    (blocking-autonomous / blocking-decision / backlog) using the rule of thumb above. Then keep
    going — do not widen this PR.

    STEP 3 — VERIFY before opening the PR: cargo fmt; cargo clippy --all-targets -- -D warnings;
    cargo test --workspace; cargo deny check. If you touched indexing, decimation or DSP, run
    cargo bench and confirm the budgets in docs/SPEC.md §5 still hold.

    STEP 4 — Add a plain-language entry to CHANGELOG.md under [Unreleased] describing what changes
    for someone USING the app. List any assumption you made under "Assumptions made".

    STEP 5 — Open a PR. If this session did a roadmap item, tick it in docs/ROADMAP.md in the same
    PR. If it did a `blocking-autonomous` issue, reference the issue so it auto-closes on merge.
    Do not merge.

    If there is genuinely nothing to do — no actionable issue and every roadmap item checked or
    blocked — do not invent work. Post a one-line summary of why and stop.

---

## Routine 2 — "Adversarial review" (GitHub trigger: pull_request.opened)

    You are reviewing this pull request as an adversary, not as its author. Assume the author was
    optimizing to make tests pass, and your job is to find where that diverged from what the
    project actually promises. Be specific and cite lines.

    Read CLAUDE.md, docs/SPEC.md and docs/QUALITY.md first. Then check, in this order:

    1. Did this PR weaken, delete, loosen a tolerance on, or skip any golden test or corpus
       expectation? This is the single most serious finding possible. Flag it loudly.
    2. Does any DSP change match its textbook definition, or was it fitted to the test? Check the
       maths, not the assertions.
    3. Is any raw sample mutated, interpolated, upcast with precision loss, or reconstructed from
       the pyramid? PSD must read raw samples only.
    4. Does anything the software inferred get applied silently, without surfacing in the
       inference bar?
    5. Does anything load an unbounded amount of the file, or exceed the SPEC §5 budgets?
    6. Is there scope beyond docs/PRODUCT.md §Scope, or beyond the single roadmap item / issue
       this PR claims to address?
    7. If this PR resolved an issue: was it a `blocking-decision` issue? If so, that is a red flag
       — product decisions belong to the maintainer, not an autonomous session. Call it out.
    8. Is the CHANGELOG entry honest and readable by a non-programmer? Are assumptions listed?

    Post one review comment summarizing your findings. If you find a violation of any of CLAUDE.md's
    five golden rules, or a resolved `blocking-decision` issue, say so in the first line and request
    changes. If it is clean, say so plainly in one or two sentences — do not pad the review.

    Do not fix anything and do not approve or merge. Review only.
