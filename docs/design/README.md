# Redesign mockup (#229, #230, #231)

`glyde-mockup.html` is an interactive mockup of Glyde's new look and time-view layout. It is a
proposal: nothing in the app changes until the maintainer approves it. Merging the PR that adds
it means "approved"; the Step 2 roadmap items then implement it in the egui app one at a time.

**Open it:** double-click `glyde-mockup.html` (any recent browser, no install). It needs the
internet only for the two fonts; without it, the page falls back to the system fonts.

It draws the real `01_engine_testbench.csv` showcase file (`testdata/showcase/`). Use the bar at
the top to switch light/dark, jump to the interesting moments, run the 20-state-channel stress
test, and pin the window to 1440 × 900. **What to review** lists every decision it asks for.

## What it proposes

| Area | Proposal | Issue |
|---|---|---|
| Time axis | One axis at the bottom, shared by every track. Date and "local time, no zone" at its left | #230, #200 |
| Numeric channels | Stacked by default (one plot each, y follows the visible data); Overlaid puts them on one y axis | #198 |
| How many | At most 8 numeric channels on screen, one per palette color. First 8 in file order on open; the inference bar counts the hidden ones | #230 |
| State channels | At most 32 % of the time view; the group scrolls beyond that and collapses to one row. Text states 22 px, booleans 12 px, markers 22 px | #230 |
| Gaps | Hatched across every track, length written on the axis | #230 |
| Anomaly gutter | Its 12 px row is reserved now, under the numeric tracks. It marks the missing samples of the shown channels (at least 2 px wide); outliers and skipped rows join it in v0.2 | #236 |
| Hover | One sample under the pointer: its exact value and timestamp. Many samples per pixel: their min–max range and how many | #196 |
| Navigation | Zoom and pan act on time only; you cannot zoom out past the file | #197 |
| Cursors | A and B across every track; right panel with Δt, 1/Δt and, per channel, A, B, ΔY, ΔY/Δt; each cursor snaps to the nearest sample | #231 |
| Spectrum | Bottom split, 40 % by default and resizable; fixed controls; legend outside the plot | #200 |
| Look | Cool graphite neutrals, petrol accent for UI state only, 8-color data palette; Inter for the interface, JetBrains Mono for numbers | #229 |

Not in the mockup: horizontal ΔY cursors (optional in SPEC §4.1), the compressed axis (this file
must not offer it), and the app icon (#232; the toolbar mark is a placeholder).

## Design tokens

These are the values the egui implementation copies. The page's `:root` block is the source.

| Token | Light | Dark | Use |
|---|---|---|---|
| window | `#f4f5f7` | `#14171b` | panels, toolbars |
| surface | `#ffffff` | `#1a1e24` | plot areas, cards |
| line | `#d9dde3` | `#2a3038` | hairlines, frames |
| grid | `#eef0f3` | `#222830` | plot grid |
| ink / ink-2 / ink-3 | `#14171c` `#4a515c` `#7b828d` | `#e8eaee` `#b2b8c2` `#7d8591` | text, cursors, axis |
| accent | `#0d6e7b` | `#4cb5c3` | pressed buttons, focus, selection |
| series 1–8 | `#2a78d6 #eb6834 #1baf7a #eda100 #e87ba4 #008300 #6250d6 #e34948` | `#3987e5 #d95926 #199e70 #c98500 #d55181 #0ca30c #9085e9 #e66767` | numeric channels |
| state tints 1–6 | `#dbe5f0 #f3e3c6 #d5ebdd #e7dcf0 #f4d8d4 #d6e9ec` | `#263345 #45371f #213b2c #372c45 #472a27 #1f3d44` | text-state bands (ink label on top) |

The series palette passes the color-blind and normal-vision separation checks in both themes
against these surfaces (worst adjacent CVD ΔE 9.1 light, 8.4 dark). In light mode three colors are
under 3:1 against white, so a series is never identified by color alone: every stacked track
carries its name, and the tooltip and cursor panel name each value.

Sizes: toolbar 44 px, inference bar 34 px, status bar 26 px, name gutter 184 px, time axis 40 px,
cursor panel 316 px. Type: 13 px UI, 11–12 px labels, 10.5–11 px monospaced figures.

## What not to port

Only the tokens, sizes and behaviour above are the reference. The page's drawing code is not:
for example, it joins min/max columns by stretching each one over its neighbour's whole range,
whereas the app keeps SPEC §3.1's connector from the previous column's last sample to this
column's first (#147).

## Fonts

Inter and JetBrains Mono are both under the SIL Open Font License 1.1, which allows embedding
them in an Apache-2.0 application. Embedding the needed weights (Latin subset) should add under 1 MB to the binary, far inside the 50 MB budget.

## Mockup limits

To stay one page, it carries a 250 ms min/max overview of the whole file and every raw sample in
three windows: cranking (09:00:01–09:00:09), the throttle release (09:02:57–09:03:05) and the
misfire (09:26:19–09:26:29). Elsewhere the cursors say the sample is not in the mockup; the app
reads it from the file. The two spectra were computed ahead of time with the same defaults as
`glyde-core` (Hann, 50 % overlap, constant detrend, segment = largest power of two ≤ N/8).

## Rebuild

```sh
pip install numpy pyarrow
python3 testdata/showcase/generate.py OUT_DIR engine
python3 docs/design/build_mockup.py OUT_DIR/01_engine_testbench.csv
```

Edit `mockup.src.html`, never `glyde-mockup.html`: the build overwrites it.
