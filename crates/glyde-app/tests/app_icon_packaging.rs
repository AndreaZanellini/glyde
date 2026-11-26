// Copyright 2026 The Glyde Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Issue #232: the app icon reaches every place an OS looks for it.
//!
//! The icon files are generated from `packaging/icon/glyde.svg` by
//! `packaging/icon/build_icons.mjs` and committed. These tests check that each
//! generated file is a well-formed container holding the sizes its OS needs,
//! and that the packaging puts it where that OS reads it. The Linux and macOS
//! packaging scripts are run for real, with their one external tool
//! (`appimagetool`, `hdiutil`) replaced by a stub that keeps the staged tree.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> Vec<u8> {
    let path = repo_root().join(relative);
    std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn read_text(relative: &str) -> String {
    String::from_utf8(read(relative)).unwrap_or_else(|err| panic!("{relative}: {err}"))
}

/// Width, height and color type of a PNG, from its IHDR chunk.
fn png_header(bytes: &[u8]) -> (u32, u32, u8) {
    assert!(bytes.len() > 33, "too short to be a PNG");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG signature");
    assert_eq!(&bytes[12..16], b"IHDR", "PNG does not start with IHDR");
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    (width, height, bytes[25])
}

const PNG_SIZES: [u32; 8] = [16, 24, 32, 48, 64, 128, 256, 512];

#[test]
fn the_vector_source_is_on_apples_1024_grid() {
    let svg = read_text("packaging/icon/glyde.svg");
    assert!(svg.contains("<svg"), "glyde.svg is not an SVG");
    // build_icons.mjs crops this exact viewBox for Windows and Linux.
    assert!(svg.contains(r#"viewBox="0 0 1024 1024""#));
}

#[test]
fn every_linux_png_is_an_rgba_square_of_its_size() {
    for size in PNG_SIZES {
        let png = read(&format!("packaging/icon/png/glyde-{size}.png"));
        let (width, height, color_type) = png_header(&png);
        assert_eq!((width, height), (size, size), "glyde-{size}.png");
        assert_eq!(
            color_type, 6,
            "glyde-{size}.png must be RGBA (transparent corners)"
        );
    }
}

#[test]
fn the_windows_ico_holds_every_size_windows_asks_for() {
    let ico = read("packaging/windows/glyde.ico");
    assert_eq!(&ico[..4], &[0, 0, 1, 0], "not an icon file header");
    let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
    let mut sizes = BTreeSet::new();
    for i in 0..count {
        let entry = &ico[6 + 16 * i..6 + 16 * (i + 1)];
        let size = if entry[0] == 0 {
            256
        } else {
            u32::from(entry[0])
        };
        assert_eq!(entry[0], entry[1], "icon frame {i} is not square");
        assert_eq!(
            u16::from_le_bytes([entry[6], entry[7]]),
            32,
            "frame {size}: not 32-bit"
        );
        let length = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        let data = ico
            .get(offset..offset + length)
            .unwrap_or_else(|| panic!("frame {size} points outside the file"));
        if size == 256 {
            assert_eq!(
                png_header(data),
                (256, 256, 6),
                "256 px frame must be an RGBA PNG"
            );
        } else {
            // BITMAPINFOHEADER: 40 bytes, height doubled for the AND mask.
            assert_eq!(u32::from_le_bytes(data[0..4].try_into().unwrap()), 40);
            assert_eq!(
                i32::from_le_bytes(data[4..8].try_into().unwrap()),
                size as i32
            );
            assert_eq!(
                i32::from_le_bytes(data[8..12].try_into().unwrap()),
                2 * size as i32
            );
            let and_row = (size as usize).div_ceil(32) * 4;
            let expected = 40 + (size * size * 4) as usize + and_row * size as usize;
            assert_eq!(length, expected, "frame {size}: DIB length");
        }
        sizes.insert(size);
    }
    assert_eq!(sizes, BTreeSet::from([16, 24, 32, 48, 64, 128, 256]));
}

#[test]
fn the_macos_icns_holds_a_full_iconset() {
    let icns = read("packaging/macos/Glyde.icns");
    assert_eq!(&icns[..4], b"icns");
    let total = u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize;
    assert_eq!(total, icns.len(), "declared .icns length");
    let mut found = BTreeSet::new();
    let mut at = 8;
    while at < icns.len() {
        let kind = std::str::from_utf8(&icns[at..at + 4]).unwrap().to_string();
        let length = u32::from_be_bytes(icns[at + 4..at + 8].try_into().unwrap()) as usize;
        assert!(
            length > 8 && at + length <= icns.len(),
            "{kind}: bad length"
        );
        let (width, height, _) = png_header(&icns[at + 8..at + length]);
        let expected = match kind.as_str() {
            "icp4" => 16,
            "icp5" | "ic11" => 32,
            "ic12" => 64,
            "ic07" => 128,
            "ic08" | "ic13" => 256,
            "ic09" | "ic14" => 512,
            "ic10" => 1024,
            other => panic!("unexpected .icns element {other}"),
        };
        assert_eq!((width, height), (expected, expected), "{kind}");
        found.insert(kind);
        at += length;
    }
    let all = [
        "icp4", "icp5", "ic11", "ic12", "ic07", "ic13", "ic08", "ic14", "ic09", "ic10",
    ];
    assert_eq!(found, all.iter().map(|s| s.to_string()).collect());
}

#[test]
fn the_window_icon_is_the_256_px_linux_png() {
    // One raster for the window and the Linux package, so they never drift.
    let src = read_text("crates/glyde-app/src/icon.rs");
    assert!(src.contains(r#"include_bytes!("../../../packaging/icon/png/glyde-256.png")"#));
    let main = read_text("crates/glyde-app/src/main.rs");
    assert!(
        main.contains(".with_icon("),
        "main.rs does not set the window icon"
    );
}

#[test]
fn the_windows_exe_embeds_the_ico() {
    let build = read_text("crates/glyde-app/build.rs");
    assert!(build.contains("packaging/windows/glyde.ico"));
    assert!(build.contains("embed_resource::compile_for"));
    // Building for Windows fails rather than shipping an exe with no icon.
    assert!(build.contains(".manifest_required()"));
    let manifest = read_text("crates/glyde-app/Cargo.toml");
    assert!(manifest.contains("[build-dependencies]") && manifest.contains("embed-resource"));
}

#[test]
fn the_windows_installer_uses_the_ico() {
    let iss = read_text("packaging/windows/glyde.iss");
    let lines: Vec<&str> = iss.lines().map(str::trim).collect();
    assert!(
        lines.contains(&"SetupIconFile=glyde.ico"),
        "installer's own icon"
    );
    assert!(
        lines.contains(&r"UninstallDisplayIcon={app}\glyde.exe"),
        "Apps & features icon"
    );
}

#[test]
fn the_desktop_entry_names_the_icon() {
    let desktop = read_text("packaging/linux/glyde.desktop");
    assert!(desktop.lines().any(|l| l.trim() == "Icon=glyde"));
}

#[test]
fn the_macos_bundle_declares_the_icns() {
    let plist = read_text("packaging/macos/Info.plist");
    let compact: String = plist.split_whitespace().collect();
    assert!(compact.contains("<key>CFBundleIconFile</key><string>Glyde</string>"));
}

/// Writes an executable stub script.
#[cfg(unix)]
fn stub(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\nset -e\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn run_packaging(script: &str, stubs: &Path, out: &Path) {
    let binary = stubs.join("glyde-binary");
    std::fs::write(&binary, b"not a real binary").unwrap();
    let path = format!(
        "{}:{}",
        stubs.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let status = std::process::Command::new("bash")
        .arg(repo_root().join(script))
        .arg(&binary)
        .arg("0.0.0-test")
        .arg(out)
        .env("PATH", path)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("bash runs");
    assert!(status.success(), "{script} failed");
}

/// `hdiutil create ... -srcfolder <dir> ... <dmg>`: keeps the staged folder
/// instead of building a disk image.
#[cfg(unix)]
const HDIUTIL_STUB: &str = r#"src=""; prev=""; last=""
for arg in "$@"; do
  if [ "$prev" = "-srcfolder" ]; then src=$arg; fi
  prev=$arg; last=$arg
done
cp -R "$src" "$last""#;

/// `curl ... -o <path>`: writes stand-in bytes where the runtime should land,
/// without touching the network.
#[cfg(unix)]
const CURL_STUB: &str = r#"out=""; prev=""
for arg in "$@"; do
  if [ "$prev" = "-o" ]; then out=$arg; fi
  prev=$arg
done
printf 'not the AppImage runtime' > "$out""#;

#[cfg(unix)]
#[test]
fn the_appimage_carries_the_icon_where_linux_looks() {
    let tmp = tempfile::tempdir().unwrap();
    let stubs = tmp.path().join("bin");
    std::fs::create_dir(&stubs).unwrap();
    // The runtime download and its check are not what this test is about
    // (see the next test): stand-in bytes, and a check that accepts them.
    stub(&stubs, "curl", CURL_STUB);
    stub(&stubs, "sha256sum", "cat > /dev/null");
    // appimagetool --runtime-file <runtime> <AppDir> <output>: keep the
    // AppDir instead of squashing it.
    stub(
        &stubs,
        "appimagetool",
        r#"[ "$1" = "--runtime-file" ] && [ -f "$2" ] || { echo "no --runtime-file" >&2; exit 1; }
cp -R "$3" "$4""#,
    );
    let out = tmp.path().join("out");
    run_packaging("packaging/linux/package.sh", &stubs, &out);

    let appdir = out.join("glyde-0.0.0-test-linux-x86_64.AppImage");
    let icon_256 = read("packaging/icon/png/glyde-256.png");
    // AppImage spec: the top-level icon named after the desktop entry's Icon=.
    assert_eq!(std::fs::read(appdir.join("glyde.png")).unwrap(), icon_256);
    assert_eq!(std::fs::read(appdir.join(".DirIcon")).unwrap(), icon_256);
    // Desktop integration (and the window manager) reads the hicolor theme.
    for size in PNG_SIZES {
        let installed = appdir.join(format!(
            "usr/share/icons/hicolor/{size}x{size}/apps/glyde.png"
        ));
        let expected = read(&format!("packaging/icon/png/glyde-{size}.png"));
        assert_eq!(
            std::fs::read(&installed).unwrap_or_else(|e| panic!("{}: {e}", installed.display())),
            expected
        );
    }
}

/// The license files every package must carry: Glyde's own, and the notices
/// of the open-source code compiled into it (MIT, BSD, OFL, … ask that their
/// text travel with every copy of the binary).
#[cfg(unix)]
const LICENSE_FILES: [&str; 3] = ["LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.txt"];

#[cfg(unix)]
#[test]
fn the_linux_packages_carry_the_license_files() {
    let tmp = tempfile::tempdir().unwrap();
    let stubs = tmp.path().join("bin");
    std::fs::create_dir(&stubs).unwrap();
    stub(&stubs, "curl", CURL_STUB);
    stub(&stubs, "sha256sum", "cat > /dev/null");
    stub(&stubs, "appimagetool", r#"cp -R "$3" "$4""#);
    let out = tmp.path().join("out");
    run_packaging("packaging/linux/package.sh", &stubs, &out);

    let appdir = out.join("glyde-0.0.0-test-linux-x86_64.AppImage");
    let unpacked = tmp.path().join("unpacked");
    std::fs::create_dir(&unpacked).unwrap();
    let untar = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(out.join("glyde-0.0.0-test-linux-x86_64.tar.gz"))
        .arg("-C")
        .arg(&unpacked)
        .status()
        .expect("tar runs");
    assert!(untar.success());
    let tarball = unpacked.join("glyde-0.0.0-test-linux-x86_64");
    for file in LICENSE_FILES {
        let expected = read(file);
        let in_appimage = appdir.join("usr/share/doc/glyde").join(file);
        assert_eq!(
            std::fs::read(&in_appimage)
                .unwrap_or_else(|e| panic!("{}: {e}", in_appimage.display())),
            expected
        );
        assert_eq!(std::fs::read(tarball.join(file)).unwrap(), expected);
    }
    // The portable Windows .exe is a single file: the release page carries
    // the notices next to it, written by this (once-per-release) job.
    assert_eq!(
        std::fs::read(out.join("glyde-0.0.0-test-third-party-licenses.txt")).unwrap(),
        read("THIRD-PARTY-LICENSES.txt")
    );
}

#[cfg(unix)]
#[test]
fn the_macos_app_bundle_carries_the_license_files() {
    let tmp = tempfile::tempdir().unwrap();
    let stubs = tmp.path().join("bin");
    std::fs::create_dir(&stubs).unwrap();
    stub(&stubs, "hdiutil", HDIUTIL_STUB);
    stub(&stubs, "codesign", "exit 0");
    let out = tmp.path().join("out");
    run_packaging("packaging/macos/package.sh", &stubs, &out);

    let resources = out.join("glyde-0.0.0-test-macos-arm64.dmg/Glyde.app/Contents/Resources");
    for file in LICENSE_FILES {
        assert_eq!(
            std::fs::read(resources.join(file)).unwrap_or_else(|e| panic!("{file}: {e}")),
            read(file)
        );
    }
}

/// The AppImage runtime is the first code that runs when a user opens the
/// AppImage. A runtime that does not match its pinned SHA-256 must stop the
/// build before appimagetool puts it in the file users download.
#[cfg(unix)]
#[test]
fn the_appimage_is_never_built_on_a_runtime_that_fails_its_checksum() {
    let tmp = tempfile::tempdir().unwrap();
    // The real sha256sum does the checking: first prove it is here and
    // works, so the failure below cannot come from a missing tool.
    let known = tmp.path().join("known");
    std::fs::write(&known, b"abc").unwrap();
    let mut check = std::process::Command::new("sha256sum")
        .args(["-c", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("sha256sum is on PATH");
    use std::io::Write;
    writeln!(
        check.stdin.take().unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  {}",
        known.display()
    )
    .unwrap();
    assert!(check.wait().unwrap().success(), "sha256sum -c works here");

    let stubs = tmp.path().join("bin");
    std::fs::create_dir(&stubs).unwrap();
    stub(&stubs, "curl", CURL_STUB);
    let ran = tmp.path().join("appimagetool-ran");
    stub(
        &stubs,
        "appimagetool",
        &format!("touch \"{}\"", ran.display()),
    );
    let out = tmp.path().join("out");
    let binary = stubs.join("glyde-binary");
    std::fs::write(&binary, b"not a real binary").unwrap();
    let status = std::process::Command::new("bash")
        .arg(repo_root().join("packaging/linux/package.sh"))
        .arg(&binary)
        .arg("0.0.0-test")
        .arg(&out)
        .env(
            "PATH",
            format!(
                "{}:{}",
                stubs.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("bash runs");
    assert!(
        !status.success(),
        "package.sh must fail on a runtime that does not match"
    );
    assert!(
        !ran.exists(),
        "appimagetool must not run on an unverified runtime"
    );
}

#[cfg(unix)]
#[test]
fn the_macos_app_bundle_carries_the_icns() {
    let tmp = tempfile::tempdir().unwrap();
    let stubs = tmp.path().join("bin");
    std::fs::create_dir(&stubs).unwrap();
    stub(&stubs, "hdiutil", HDIUTIL_STUB);
    // codesign: absent on Linux, and on macOS it would refuse the stand-in
    // binary. The signing steps are checked in glyde-devtools'
    // release_packaging tests and on the real build in the release workflow.
    stub(&stubs, "codesign", "exit 0");
    let out = tmp.path().join("out");
    run_packaging("packaging/macos/package.sh", &stubs, &out);

    let app = out.join("glyde-0.0.0-test-macos-arm64.dmg/Glyde.app");
    assert_eq!(
        std::fs::read(app.join("Contents/Resources/Glyde.icns")).unwrap(),
        read("packaging/macos/Glyde.icns")
    );
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
    let compact: String = plist.split_whitespace().collect();
    assert!(compact.contains("<key>CFBundleIconFile</key><string>Glyde</string>"));
}
