> **Maintainer: run the 10-file smoke ritual (docs/QUALITY.md §Manual QA) before announcing.**

## Downloads

| Platform | File |
|---|---|
| macOS (Apple silicon) | `glyde-<version>-macos-arm64.dmg` — open it and drag **Glyde** to **Applications** |
| Windows (x64) | `glyde-<version>-windows-x64-setup.exe` (installer) or `glyde-<version>-windows-x64.exe` (portable, no install) |
| Linux (x64) | `glyde-<version>-linux-x86_64.AppImage` (make it executable, then run it) or `glyde-<version>-linux-x86_64.tar.gz` |

Nothing else to install: no runtime, no toolchain.

## This build is not signed yet — how to open it the first time

Glyde is not yet signed with an Apple or Microsoft certificate, so your system warns you the first time you open it. This is expected for this release; the file is the one built by this repository's CI.

**macOS**
1. Open the `.dmg` and drag **Glyde** to **Applications**.
2. In **Applications**, **right-click (or Control-click) Glyde → Open**, then click **Open** in the dialog. You only need to do this once.
3. If macOS still refuses ("Glyde is damaged" or no **Open** button), open **Terminal** and run:
   `xattr -dr com.apple.quarantine /Applications/Glyde.app`
   then open Glyde normally.

**Windows**
1. Run the installer or the portable `.exe`.
2. If **"Windows protected your PC"** (SmartScreen) appears, click **More info → Run anyway**. You only need to do this once per file.

**Linux**
- AppImage: `chmod +x glyde-*.AppImage` then run it. No warning is expected.

## Known limitations

This is a first release. What it does not do well yet, in plain words:

- **Speed on very large files** is below the targets Glyde sets itself: on the reference
  laptop, a CSV indexes at about 100–175 MB/s, the first plot of a file larger than
  memory takes about 3 s, a Parquet file larger than memory about 5 s per GB, and a
  Parquet file shows nothing until it is read (#194, #195, #181).
- **Outliers are not flagged yet**; they stay visible on the plot (v0.2, #236).
- **Units** from the header are part of the channel name, not shown next to values (#237).
- **Many channels**: every stacked channel gets an equal share of the height, so with
  dozens of channels each plot is thin. Use **Overlaid** or hide channels (#257).
- **Compress** is not offered on files that also have on/off or text channels (#201).
- **Blank cells in an on/off column**, and occasional words such as `ERR` in a numeric
  column, are not yet read as missing values (#240, #241).
- **Do not let another program shorten a CSV while Glyde has it open**: Glyde may crash
  (#272).
- A deliberately crafted Parquet file can keep an open busy for minutes, or make Glyde
  crash (#274, #275). These come from damaged or hand-made files found by fuzzing, not
  from files written by normal tools.

A file that won't open is the bug we care about most: open an issue with the
**"My file won't open"** template and attach an anonymized sample and the day's log file
(the README says where it is).

## What changed

See the section for this version in `CHANGELOG.md`, in plain language; the commit list follows.
