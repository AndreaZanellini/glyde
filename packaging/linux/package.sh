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

# The licenses of the open-source code compiled into Glyde travel with every
# package (THIRD-PARTY-LICENSES.txt, packaging/third-party/generate.sh). The
# portable Windows .exe is a single file, so the release page also carries
# them on their own; this job, which runs once per release, writes that copy.
cp "$root/THIRD-PARTY-LICENSES.txt" "$out/glyde-$version-third-party-licenses.txt"

# Tarball: the binary plus the license files.
tar_dir="$stage/glyde-$version-linux-x86_64"
mkdir -p "$tar_dir"
cp "$binary" "$tar_dir/glyde"
chmod +x "$tar_dir/glyde"
cp "$root/LICENSE" "$root/NOTICE" "$root/THIRD-PARTY-LICENSES.txt" "$root/README.md" "$tar_dir/"
tar -C "$stage" -czf "$out/glyde-$version-linux-x86_64.tar.gz" "glyde-$version-linux-x86_64"

# AppImage.
appdir="$stage/Glyde.AppDir"
mkdir -p "$appdir/usr/bin"
cp "$binary" "$appdir/usr/bin/glyde"
chmod +x "$appdir/usr/bin/glyde"
mkdir -p "$appdir/usr/share/doc/glyde"
cp "$root/LICENSE" "$root/NOTICE" "$root/THIRD-PARTY-LICENSES.txt" "$appdir/usr/share/doc/glyde/"
cp "$here/glyde.desktop" "$appdir/glyde.desktop"
# Issue #232: the icon at the AppDir root (named after the desktop entry's
# Icon=) and as .DirIcon, per the AppImage spec; every size in the hicolor
# theme for desktop integration tools.
icons="$root/packaging/icon/png"
cp "$icons/glyde-256.png" "$appdir/glyde.png"
cp "$icons/glyde-256.png" "$appdir/.DirIcon"
for size in 16 24 32 48 64 128 256 512; do
    mkdir -p "$appdir/usr/share/icons/hicolor/${size}x${size}/apps"
    cp "$icons/glyde-$size.png" "$appdir/usr/share/icons/hicolor/${size}x${size}/apps/glyde.png"
done
cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/glyde" "$@"
APPRUN
chmod +x "$appdir/AppRun"

# The AppImage runtime is the small program at the front of the AppImage: the
# first code that runs when a user opens it. Left to itself, appimagetool
# downloads it from type2-runtime's moving `continuous` release at every
# build, unchecked. Use a tagged release instead, check its SHA-256 (the
# digest GitHub shows for that asset), and hand exactly that file over.
runtime_url=https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64
runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
runtime="$stage/runtime-x86_64"
curl -fsSL "$runtime_url" -o "$runtime"
echo "$runtime_sha256  $runtime" | sha256sum -c -
ARCH=x86_64 appimagetool --runtime-file "$runtime" "$appdir" "$out/glyde-$version-linux-x86_64.AppImage" >/dev/null
ls "$out"
