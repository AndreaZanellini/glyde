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

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
app="$stage/Glyde.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/glyde"
chmod +x "$app/Contents/MacOS/glyde"
sed "s/__VERSION__/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
# Drag-to-install: the usual Applications shortcut next to the app.
ln -s /Applications "$stage/Applications"

mkdir -p "$out"
dmg="$out/glyde-$version-macos-arm64.dmg"
rm -f "$dmg"
hdiutil create -volname "Glyde $version" -srcfolder "$stage" -ov -format UDZO "$dmg" >/dev/null
echo "$dmg"
