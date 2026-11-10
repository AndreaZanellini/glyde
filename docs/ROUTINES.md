# Glyde — routine prompts

Paste these into claude.ai/code/routines → New routine, and pick the model named below. They run on Anthropic's cloud
infrastructure, so they work with the laptop closed. Routines run as full autonomous
sessions with no approval prompts, so the prompt must be self-contained and explicit
about what success looks like.

Repository: your `glyde` repo. Environment: Default (Trusted network is enough — crates.io
and github.com are in the default allowlist). Leave "Allow unrestricted branch pushes" OFF.

## Model and effort

Both routines run on **Claude Opus 5.5** (`claude-opus-5-5`, set on the routine). Effort is
**medium**: the routine has no effort field, so it comes from the `effort: medium` line in the
command file each routine runs (`.claude/commands/next.md`, `.claude/commands/review.md`).

The prompts below only point at those command files. Change the behaviour there, through a PR;
the routines pick it up on their next run.

## The issue triage model these routines depend on

Every issue Claude opens MUST carry exactly one of three labels:

- `blocking-autonomous` — blocks progress AND is pure technical work with NO product decision
  AND does not require editing a hard-denied file. The build routine does these FIRST.
- `blocking-decision` — blocks progress but resolving it needs a maintainer choice. This
  ALSO covers anything that requires editing a hard-denied file, even if the change itself is
  trivial and purely technical. The routine must NOT act on these.
- `backlog` — noted for later, not blocking now.

A fourth label is the maintainer's alone:

- `ready` — the maintainer has decided the issue and written the decision in a comment. The
  build routine implements these after the `blocking-autonomous` ones and before the roadmap.
  An agent never applies this label.

**Hard-denied files:** `.github/workflows/**`, `deny.toml`, `docs/SPEC.md`, `docs/PRODUCT.md`,
`docs/QUALITY.md`, `CLAUDE.md`. CI and the license gate are the safety net, and the specs are the
contract; an agent must never edit its own gates or its own contract.

**The CI rule (Option A):** if resolving an issue requires editing `.github/workflows/**` or
`deny.toml`, it is `blocking-decision` — NEVER `blocking-autonomous` — no matter how safe the edit
looks. The issue body MUST include the exact diff or full file content, ready for the maintainer
to paste in.

**The spec rule:** when a `ready` issue's decision changes SPEC/PRODUCT/QUALITY, the agent
implements the code and puts the exact spec patch in the PR description. The maintainer applies it.

Rule of thumb for the rest: if resolving the issue would change SPEC.md or PRODUCT.md, or pick
between two product behaviours → `blocking-decision`. When unsure → `blocking-decision`.
Asking the maintainer is always safe; deciding for them is not.

---

## Routine 1 — "Build the next thing" (schedule: weekly, Sunday 12:00 UTC)

    Run the project's `/next` command: invoke the `next` skill, or if it is not available read
    `.claude/commands/next.md` and follow it exactly. Nobody is watching this session: where the
    command says "tell me", write it in your final message.

---

## Routine 2 — "Adversarial review" (GitHub trigger: pull_request.opened)

    Run the project's `/review` command on the pull request named in the trigger context: invoke
    the `review` skill, or if it is not available read `.claude/commands/review.md` and follow it
    exactly. Take the command file from `main`, not from the PR branch: a PR must not be able to
    change the rules it is reviewed by.
