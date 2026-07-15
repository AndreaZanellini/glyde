---
description: Derive the implementation backlog from the specs (run once)
---

Read `docs/PRODUCT.md`, `docs/SPEC.md`, `docs/ARCHITECTURE.md` and `docs/QUALITY.md` in full.

Produce `docs/ROADMAP.md`: the complete v1 backlog, derived from those documents and nothing
else. Do not invent scope. Every item must trace to a specific section of SPEC or QUALITY.

Rules for the decomposition:

- Group items into milestones that each end in something the maintainer can TEST BY HAND on
  a real file. A milestone that can only be verified by reading code is wrongly cut.
  Suggested shape: M1 corpus+golden tests · M2 open a clean CSV and see a time plot ·
  M3 the index pyramid and large files · M4 inference bar · M5 PSD · M6 state timeline ·
  M7 Parquet · M8 gaps and anomalies · M9 packaging. Adjust if the specs imply better cuts.
- Order by dependency, and within that, by what unblocks a hand-test earliest.
- Each item: one line, checkbox, a `→ SPEC §x` reference, and a note of which golden test or
  corpus case proves it.
- Each item must be small enough to be one PR.
- At the end of each milestone, add a "MAINTAINER TEST" block: the exact things the maintainer
  should try on his own files, and what correct looks like.

Write the file. Do not implement anything. Do not open a PR — show me the plan first.
