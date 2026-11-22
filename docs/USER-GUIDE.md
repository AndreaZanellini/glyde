# Glyde user guide

This short guide covers everything Glyde v0.1 does. If you have not installed it yet,
see the [README](../README.md#install).

- [Open a file](#open-a-file)
- [The inference bar: what Glyde guessed, and how to correct it](#the-inference-bar)
- [Moving around the time view](#moving-around-the-time-view)
- [Channels: stacked or overlaid, shown or hidden](#channels)
- [State tracks: on/off, states and events](#state-tracks)
- [Gaps and Compress](#gaps-and-compress)
- [Measurement cursors](#measurement-cursors)
- [Spectrum (PSD)](#spectrum-psd)
- [Large files](#large-files)
- [Where Glyde keeps its log and its cache](#log-and-cache)

---

## Open a file

Drag a file onto the window, or use **File → Open…**. Glyde reads:

- delimited text: `.csv`, `.tsv`, `.txt`, with any delimiter and any decimal separator, in
  UTF-8 (with or without a byte-order mark), UTF-16 or Windows-1252 / Latin-1;
- `.parquet` files, and a **folder** of Parquet files that together form one dataset
  (drop the folder, or use **File → Open folder…**).

One file is open at a time: opening another replaces it. Glyde never changes your file.
Everything you do in Glyde changes only how the data is *shown*.

## The inference bar

The **Inference** line under the file name says what Glyde found in your file, for example
`Inference — 1440000 samples, SegmentedUniform`. Click it to expand the details. **Check
them first**, every time: they are how you know the plot shows your data correctly. They
list:

- the **encoding**, the **delimiter** and the **decimal separator**;
- the **time column**, how it was read (for example `YYYY-MM-DD HH:MM:SS.fff`) and its
  **time zone**: the one in the file, `UTC` for epoch times, or `naive local time` when
  the file says nothing (Glyde never invents a zone);
- the **sampling**: `Uniform · 25 kHz`, `median 1 kHz` for bursts, `every 15 min`, or the
  typical spacing for irregular data. A wrong time unit shows up here as a rate 1000×
  too high or too low;
- the number of **gaps**, and anything Glyde had to absorb: skipped rows, unreadable
  characters replaced with `�`, very large whole numbers that the plot rounds. Skipped
  rows stay visible even while the details are collapsed.

Anything Glyde is **not sure about is marked `(low confidence)`**, and that is where a
one-click correction appears:

| You see | What it does |
|---|---|
| **Correct…** next to the encoding, delimiter or decimal separator | Pick the right one; the file is read again with it. |
| **Swap to DD/MM** / **Swap to MM/DD** | For dates where no day is above 12, so both readings are possible. |
| **time column:** picker | Choose another column as the time axis, or **Row index (generated)** to plot against the row number. You can do this while the file is still loading. |
| **Sort** / **Keep as-is** | Shown when timestamps run out of order. Sort shows the rows in time order; your file is not changed. |
| **N rows skipped — view details** | Lists each skipped row (line number, counted from the top of the file including the header) and why it was skipped. |

Corrections add up: correct the delimiter, then the date order, and both stay applied
until you open another file. Changing the encoding or the delimiter forgets a time
column you picked by hand, because the columns may split differently.

A file with no recognisable time column still opens: every column is plotted against
the row number, and the bar says so.

## Moving around the time view

- **Zoom in time:** mouse wheel with Ctrl (⌘ on a Mac), or a trackpad pinch.
- **Pan in time:** drag with the left button, or scroll sideways. Drag or scroll the
  time axis row at the bottom to do the same.
- **Zoom in a box:** drag with the right mouse button. This zooms y too, and that plot
  keeps its y range while you pan.
- **Fit to data**, or a **double-click** on a plot: back to the whole file.
- **Click a cluster** (on a file with gaps): fits the view to that stretch of data.

Every plot and every state track shares one time axis, so they move together. Each
plot's y range follows the values you are looking at. Zoom stops at two samples across
the plot, and zoom-out at the whole file plus a small margin.

The **readout row** under the plots shows the exact time and every channel's value at
the pointer, read from the file, not from the drawing. Times are shown as dates and
times of day, with the zone named under the first axis label; a numeric timestamp
also shows its raw value in brackets.

Zoomed out, a one-sample spike is never lost: each pixel column draws the minimum and
the maximum of the samples under it.

## Channels

- **Stacked** (the default) gives each numeric channel its own plot and y scale, one
  under the other. **Overlaid** puts them all on one plot with one y axis.
- The **channel list** (top right of the time view) shows or hides each channel. **All**
  and **None** do what they say; **Alt+click** (Option+click on a Mac) a channel shows
  only that one, and Alt+click again shows them all. Hiding a channel hides it in the
  spectrum too.
- A missing value (`NaN`, or a blank cell in a numeric column) breaks the line: nothing
  is drawn across it.

## State tracks

Columns that are not plain numbers become compact tracks under the plots, on the same
time axis:

- **on/off** columns (`true`/`false`, `0`/`1`) are a strip that is filled while "on";
- **text states** (`idle`, `running`, `fault`) are labelled bands;
- **events** (a text column that is mostly empty, with a word now and then) are ticks.

A text column that could be drawn either way has an **events / bands** button next to
its name. The tracks are grouped under a "*N* state channels" header: they never take
more than about a third of the view (scroll inside the group for more), and a click on
the header collapses them. Where several states fall in one pixel, a gray mark says so;
zoom in to see them.

## Gaps and Compress

A **gap** is a pause in recording: two samples more than 10 times the typical spacing
apart. The time axis is true to scale, so a pause takes its real width, and no line is
drawn across it.

When gaps cover more than half of the file's time span, an `N gaps detected · Compress`
control appears above the plot. **Compress** shrinks every gap to the same narrow
break, marked `⫽` with its real length (`⫽ 2.8 h gap`), so the stretches of data become
readable. **Expand** goes back. Compress changes only the drawing: the readout, the
cursors and the spectrum still use the real times.

## Measurement cursors

Press **Cursors** in the time view's toolbar. Two vertical lines, **A** and **B**, appear
across every plot and track; drag one to move it, and it snaps to the nearest sample.
The panel on the right shows:

- the time of each cursor, and **Δt = B − A** (exact, never rounded) with **1/Δt** in Hz;
- for each shown channel: its value at A and at B as written in the file,
  **ΔY = B − A** and the **slope ΔY/Δt** per second.

A missing sample reads `missing`, with no ΔY or slope: nothing is interpolated. On a
file timed by a plain index the panel says **Δx** instead of Δt. Press **Cursors** again
to remove them.

## Spectrum (PSD)

The panel under the time view computes the power spectral density (Welch's method) of
each shown channel, on the **raw samples** of the stretch the time view shows.

1. Zoom the time view onto the stretch you want (or **Fit to data** for everything).
2. Press **Compute PSD**. It runs in the background; **Cancel** stops it.
3. The **Computed on** line says exactly what was used: number of samples and windows,
   window type and length, overlap, Δf and the sampling rate.

Moving the time view afterwards never recomputes by itself: the spectrum stays, with a
note when it no longer matches the view. **Overlay / Stacked**, **Log frequency** and
**Log power** change the display. **PSD settings** (window, segment length, overlap)
are never needed for a correct first result; **Defaults** resets them.

Glyde refuses to compute rather than give a misleading answer, and says why:

- **Bursts separated by gaps:** each burst is analysed separately (no window crosses a
  gap) and the results are averaged; bursts too short for one window are listed.
- **Irregular timestamps:** no spectrum, because Glyde does not resample to fake a
  uniform rate. If a uniform stretch exists, **Analyze the largest uniform stretch**
  moves the view onto it.
- **Timestamps out of order:** use **Sort** in the inference bar first.
- **Missing values:** windows containing one are skipped, and the panel says how many.

## Large files

Glyde never loads more than it can afford. It allows itself `min(25 % of your RAM, 4 GB)`.
A file whose data would not fit is **spilled**: its index is written to Glyde's cache
folder (never next to your file), and a notice says so while it loads. The first part of
the plot appears while the rest of the file is still being read.

Reopening a file you opened before is much faster, because Glyde keeps a cached copy of
its index. The cache is capped at a tenth of the disk space it could use, and never more than
10 GB; the least recently used files are removed first.

## Log and cache

Glyde writes a log of every decision it takes on your behalf: what it inferred, what it
skipped, what it refused, and why. Attach the log when you report a problem.

| | Log files (`glyde.log.<date>`, one per day) | Cache |
|---|---|---|
| macOS | `~/Library/Application Support/com.glyde.Glyde/logs/` | `~/Library/Caches/com.glyde.Glyde/` |
| Windows | `%LOCALAPPDATA%\glyde\Glyde\data\logs\` | `%LOCALAPPDATA%\glyde\Glyde\cache\` |
| Linux | `~/.local/share/glyde/logs/` | `~/.cache/glyde/` |

Both folders are readable by your user account only: the log names the files you open,
and the cache holds decoded copies of them. Deleting the cache is always safe; Glyde
rebuilds what it needs on the next open.
