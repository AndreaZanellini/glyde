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

## What changed

See `CHANGELOG.md` for this version, in plain language; the commit list follows.
