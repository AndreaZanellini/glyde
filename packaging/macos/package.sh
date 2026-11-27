#!/usr/bin/env bash
# Builds SPEC §7's macOS artifact: `Glyde.app` inside a compressed `.dmg`.
# Usage: packaging/macos/package.sh <glyde binary> <version> <output dir>
# Unsigned until the Apple Developer account exists (SPEC §7); the release
# notes carry the first-run workaround.
set -euo pipefail

binary=$1
version=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
app="$stage/Glyde.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/glyde"
chmod +x "$app/Contents/MacOS/glyde"
sed "s/__VERSION__/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
# Issue #232: the Dock and Finder icon, named by CFBundleIconFile.
cp "$here/Glyde.icns" "$app/Contents/Resources/Glyde.icns"
# Glyde's license, and those of the open-source code compiled into it.
cp "$root/LICENSE" "$root/NOTICE" "$root/THIRD-PARTY-LICENSES.txt" "$app/Contents/Resources/"
# Drag-to-install: the usual Applications shortcut next to the app.
ln -s /Applications "$stage/Applications"

# Ad-hoc signature over the whole bundle: no certificate, no identity, so the
# build is still unsigned in SPEC §7's sense. Without it only the executable
# carries the linker's signature, Info.plist and the icon are not sealed, and
# macOS calls the downloaded app "damaged" instead of "could not verify",
# which leaves no Open Anyway button.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

mkdir -p "$out"
dmg="$out/glyde-$version-macos-arm64.dmg"
rm -f "$dmg"
hdiutil create -volname "Glyde $version" -srcfolder "$stage" -ov -format UDZO "$dmg" >/dev/null
echo "$dmg"
