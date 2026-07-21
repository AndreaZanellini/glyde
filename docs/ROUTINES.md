# Glyde — routine prompts

Paste these into claude.ai/code/routines → New routine. They run on Anthropic's cloud
infrastructure, so they work with the laptop closed. Routines run as full autonomous
sessions with no approval prompts, so the prompt must be self-contained and explicit
about what success looks like.

Repository: your `glyde` repo. Environment: Default (Trusted network is enough — crates.io
and github.com are in the default allowlist). Leave "Allow unrestricted branch pushes" OFF.

## The issue triage model these routines depend on

Every issue Claude opens MUST carry exactly one of three labels:

- `blocking-autonomous` — blocks progress AND is pure technical work with NO product decision
  AND does not require editing a hard-denied file. The nightly routine does these FIRST.
- `blocking-decision` — blocks progress but resolving it needs a maintainer choice. This
  ALSO covers anything that requires editing `.github/workflows/**` or `deny.toml`, even if
  the change itself is trivial and purely technical: those files are hard-denied to agent
  sessions by design (CI and the license gate are the safety net; an agent must never edit
  its own gates). The routine must NOT act on these.
- `backlog` — noted for later, not blocking now.

**The CI rule (important, this is Option A):** if resolving an issue requires editing
`.github/workflows/**` or `deny.toml`, it is `blocking-decision` — NEVER `blocking-autonomous`
— no matter how safe the edit looks. When Claude files such an issue it MUST include, in the
issue body, the exact diff or full file content ready for the maintainer to paste in. The
maintainer applies CI changes by hand; the agent's job is to make that a copy-paste, not to
make the edit.

Rule of thumb for the other two: if resolving the issue would change SPEC.md or PRODUCT.md,
or pick between two product behaviours → `blocking-decision`. When unsure → `blocking-decision`.
Asking the maintainer is always safe; deciding for them is not.

---

## Routine 1 — "Build the next thing" (schedule: nightly)

    Read CLAUDE.md and docs/ROADMAP.md.

    STEP 1 — TRIAGE OPEN ISSUES BEFORE TOUCHING THE ROADMAP.
    Run `gh issue list --state open` and read the labels.

    - `blocking-decision` issues: do NOT act on them and do NOT pick work that depends on the
      decision they raise. They are the maintainer's queue. (This includes every CI / deny.toml
      change — you cannot edit those files, and you must not try to route around the guard with
      Bash cat/tee/sed. If you find such work, ensure the issue exists and carries a ready-to-paste
      diff in its body, then move on.)

    - `blocking-autonomous` issues with no open PR: pick the oldest and make it this session's
      task. Cap: one per session.
      IMPORTANT FALLBACK: if, once you start, the task turns out NOT to be completable by an
      autonomous session — most commonly because it actually requires editing a hard-denied file
      (.github/workflows/** or deny.toml) — do the following and then CONTINUE to the roadmap in
      the same session; do not end the session having done nothing:
        (a) relabel the issue from `blocking-autonomous` to `blocking-decision`,
        (b) edit the issue body to include the exact diff / full file the maintainer should paste,
        (c) leave a short comment explaining it bottoms out at a hard-denied file,
        (d) then proceed as if no blocking-autonomous issue existed → go to the roadmap.

    - Otherwise: go straight to the roadmap.

    STEP 2 — DO THE WORK: either the one completable blocking-autonomous issue, or the first
    unchecked roadmap item with no open PR.

    Follow CLAUDE.md's golden rules and docs/ARCHITECTURE.md's boundaries. Test-first is mandatory
    for glyde-core::dsp and glyde-core::ingest: the golden test or corpus case must exist and fail
    before implementation. Never weaken a golden test to make code pass — if one seems wrong, open a
    `blocking-decision` issue and pick different work.

    If you discover out-of-scope work, open an issue and LABEL it per the triage model above
    (remember: anything touching CI or deny.toml is `blocking-decision` with a ready-to-paste diff).
    Then keep going; do not widen this PR.

    STEP 3 — VERIFY: cargo fmt; cargo clippy --all-targets -- -D warnings; cargo test --workspace;
    cargo deny check. If you touched indexing, decimation or DSP, run cargo bench against docs/SPEC.md
    §5 budgets.

    STEP 4 — CHANGELOG.md [Unreleased]: a plain-language entry describing what changes for someone
    USING the app. List assumptions under "Assumptions made".

    STEP 5 — Open a PR. Tick the roadmap item, or reference the issue so it auto-closes, in the same
    PR. Do not merge.

    If there is genuinely nothing completable — no actionable issue and every roadmap item checked or
    blocked — post a one-line summary of why and stop. Do not invent work.

---

## Routine 2 — "Adversarial review" (GitHub trigger: pull_request.opened)

    You are reviewing this pull request as an adversary, not its author. Assume the author was
    optimizing to make tests pass; find where that diverged from what the project promises. Cite lines.

    Read CLAUDE.md, docs/SPEC.md and docs/QUALITY.md first. Then check, in order:

    1. Did this PR weaken, delete, loosen a tolerance on, or skip any golden test or corpus
       expectation? The single most serious finding. Flag it loudly.
    2. Does any DSP change match its textbook definition, or was it fitted to the test?
    3. Is any raw sample mutated, interpolated, upcast with precision loss, or reconstructed from
       the pyramid? PSD reads raw samples only.
    4. Does anything inferred get applied silently, without surfacing in the inference bar?
    5. Does anything load an unbounded amount of the file, or exceed SPEC §5 budgets?
    6. Scope beyond docs/PRODUCT.md §Scope, or beyond the one item/issue this PR claims?
    7. Did this PR resolve a `blocking-decision` issue, or edit `.github/workflows/**` or
       `deny.toml`? Those are maintainer-only. If an agent PR touched them, that is a red flag —
       request changes.
    8. Is the CHANGELOG entry honest and readable by a non-programmer? Assumptions listed?

    Post one review comment. If you find a golden-rule violation, a resolved `blocking-decision`
    issue, or an agent edit to CI/deny.toml, say so in the first line and request changes. If clean,
    say so in one or two sentences. Do not pad.

    Do not fix, approve, or merge. Review only.