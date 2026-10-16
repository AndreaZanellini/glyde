#!/usr/bin/env bash
# Builds SPEC §7's Linux artifacts: an AppImage and a tarball.
# Usage: packaging/linux/package.sh <glyde binary> <version> <output dir>
# Needs `appimagetool` on PATH (the release workflow downloads a pinned one).
set -euo pipefail

binary=$1
version=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
mkdir -p "$out"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

# Tarball: the binary plus the license files.
tar_dir="$stage/glyde-$version-linux-x86_64"
mkdir -p "$tar_dir"
cp "$binary" "$tar_dir/glyde"
chmod +x "$tar_dir/glyde"
cp "$root/LICENSE" "$root/NOTICE" "$root/README.md" "$tar_dir/"
tar -C "$stage" -czf "$out/glyde-$version-linux-x86_64.tar.gz" "glyde-$version-linux-x86_64"

# AppImage.
appdir="$stage/Glyde.AppDir"
mkdir -p "$appdir/usr/bin"
cp "$binary" "$appdir/usr/bin/glyde"
chmod +x "$appdir/usr/bin/glyde"
cp "$here/glyde.desktop" "$appdir/glyde.desktop"
cp "$here/glyde.png" "$appdir/glyde.png"
cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/glyde" "$@"
APPRUN
chmod +x "$appdir/AppRun"
ARCH=x86_64 appimagetool "$appdir" "$out/glyde-$version-linux-x86_64.AppImage" >/dev/null
ls "$out"
