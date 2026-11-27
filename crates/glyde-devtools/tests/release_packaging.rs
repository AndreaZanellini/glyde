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

//! Issue #30 / docs/history/ROADMAP-M0-M9.md M9: what a tagged release publishes is only
//! exercised when a tag is pushed, so the parts that can be checked without
//! one are locked here — the release notes' mandatory content (docs/SPEC.md
//! §7, CLAUDE.md §Release checklist) and the release workflow's gate and
//! artifact set.

use std::path::PathBuf;

fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    // A Windows checkout turns `\n` into `\r\n`; the checks below are about
    // the words and their order, not the line endings.
    text.replace("\r\n", "\n")
}

#[test]
fn release_notes_carry_the_unsigned_build_workarounds_and_the_qa_reminder() {
    let notes = repo_file("packaging/release-notes.md");
    for required in [
        // CLAUDE.md §Release checklist, verbatim.
        "Maintainer: run the 10-file smoke ritual (docs/QUALITY.md §Manual QA) before announcing.",
        // docs/SPEC.md §7: the exact first-run workaround on both platforms.
        "System Settings → Privacy & Security",
        "Open Anyway",
        "xattr -dr com.apple.quarantine /Applications/Glyde.app",
        "More info → Run anyway",
    ] {
        assert!(
            notes.contains(required),
            "release notes must contain {required:?}"
        );
    }
}

/// Without an ad-hoc signature over the whole bundle, macOS calls the
/// downloaded app "damaged" and offers only the Bin (found on the v0.1.0 dry
/// run). The script must sign the bundle and fail the build if the signature
/// does not verify.
#[test]
fn the_macos_package_seals_the_whole_bundle_and_checks_it() {
    let script = repo_file("packaging/macos/package.sh");
    let sign = script
        .find("codesign --force --deep --sign - \"$app\"")
        .expect("package.sh must ad-hoc sign Glyde.app");
    let verify = script
        .find("codesign --verify --deep --strict \"$app\"")
        .expect("package.sh must verify the bundle signature");
    let dmg = script
        .find("hdiutil create")
        .expect("package.sh builds the dmg");
    assert!(
        sign < verify && verify < dmg,
        "sign, then verify, then build the dmg"
    );
}

/// appimagetool downloads the AppImage runtime from the moving `continuous`
/// release at every build unless it is given one, and the runtime is the
/// first code that runs when a user opens the AppImage. The script must use a
/// runtime from a tagged release, check its SHA-256, and hand exactly that
/// file to appimagetool.
#[test]
fn the_appimage_runtime_is_pinned_and_checked_before_it_is_used() {
    let script = repo_file("packaging/linux/package.sh");
    assert!(
        !script.contains("/continuous/"),
        "the runtime must come from a tagged release, not `continuous`"
    );
    let url = script
        .find("runtime_url=https://github.com/AppImage/type2-runtime/releases/download/")
        .expect("package.sh pins the runtime's release URL");
    let hash_line = script
        .lines()
        .find_map(|line| line.strip_prefix("runtime_sha256="))
        .expect("package.sh pins the runtime's SHA-256");
    assert!(
        hash_line.len() == 64 && hash_line.chars().all(|c| c.is_ascii_hexdigit()),
        "a full SHA-256, got {hash_line:?}"
    );
    let check = script
        .find("sha256sum -c -")
        .expect("package.sh checks the runtime");
    let build = script
        .find("appimagetool --runtime-file \"$runtime\"")
        .expect("appimagetool is given the checked runtime");
    assert!(
        url < check && check < build,
        "download, then check, then build"
    );
}

/// The builds are unsigned until v1.0 (docs/SPEC.md §7), so the release must
/// let a user check what they downloaded: a `SHA256SUMS` file next to the
/// packages, and a signed build provenance attestation for each of them,
/// produced on every run (a dry run too, so a tag never exercises it first).
#[test]
fn the_release_publishes_checksums_and_signed_provenance() {
    let workflow = repo_file(".github/workflows/release.yml");
    let provenance = workflow
        .find("\n  provenance:\n")
        .expect("release.yml has a provenance job");
    let release = workflow
        .find("\n  release:\n")
        .expect("release.yml has a release job");
    assert!(
        provenance < release,
        "provenance runs before the release job"
    );
    let job = &workflow[provenance..release];
    assert!(
        !job.contains("if: startsWith(github.ref"),
        "provenance also runs on a dry run"
    );
    for required in [
        "id-token: write",
        "attestations: write",
        "sha256sum -- glyde-* > SHA256SUMS",
        "uses: actions/attest@",
        "subject-path: dist/glyde-*",
    ] {
        assert!(job.contains(required), "provenance job lacks {required:?}");
    }
    let publish = &workflow[release..];
    assert!(publish.contains("needs: provenance"));
    assert!(
        publish.contains("files: dist/*"),
        "SHA256SUMS is published with the packages"
    );
}

/// What users download is built from the tagged source alone: no Rust build
/// cache restored from an earlier run can end up in a release package.
#[test]
fn the_release_build_restores_no_cache() {
    let workflow = repo_file(".github/workflows/release.yml");
    assert!(
        !workflow.contains("rust-cache"),
        "release.yml must not restore a build cache"
    );
}

/// The Windows installer runs only on the release runner, so its file list
/// is checked here: Glyde's license and the third-party notices are
/// installed next to glyde.exe. (The Linux and macOS packages are built for
/// real in glyde-app's packaging tests.)
#[test]
fn the_windows_installer_installs_the_license_files() {
    let iss = repo_file("packaging/windows/glyde.iss");
    let files = iss
        .split("[Files]")
        .nth(1)
        .and_then(|rest| rest.split("\n[").next())
        .expect("glyde.iss has a [Files] section");
    for file in ["LICENSE", "NOTICE", "THIRD-PARTY-LICENSES.txt"] {
        assert!(
            files.contains(&format!("Source: \"..\\..\\{file}\"; DestDir: \"{{app}}\"")),
            "the installer must install {file}"
        );
    }
}

/// The portable `.exe` is one file with no folder around it, so its notices
/// are a file of their own on the release page: the Linux job writes it into
/// the release files, and the release notes say it is there.
#[test]
fn the_release_page_carries_the_notices_for_the_portable_exe() {
    let script = repo_file("packaging/linux/package.sh");
    assert!(script.contains(
        "cp \"$root/THIRD-PARTY-LICENSES.txt\" \"$out/glyde-$version-third-party-licenses.txt\""
    ));
    let notes = repo_file("packaging/release-notes.md");
    assert!(notes.contains("`glyde-<version>-third-party-licenses.txt`"));
}

#[test]
fn the_release_workflow_is_gated_on_ci_and_publishes_every_spec_artifact() {
    let workflow = repo_file(".github/workflows/release.yml");
    assert!(workflow.contains("uses: ./.github/workflows/ci.yml"));
    assert!(workflow.contains("needs: ci"));
    assert!(workflow.contains("if: startsWith(github.ref, 'refs/tags/v')"));
    assert!(workflow.contains("body_path: packaging/release-notes.md"));
    for artifact in [
        "-macos-arm64.dmg",
        "-windows-x64-setup.exe",
        "-windows-x64.exe",
        "-linux-x86_64.AppImage",
        "-linux-x86_64.tar.gz",
    ] {
        assert!(
            workflow.contains(artifact),
            "release must check for {artifact}"
        );
    }
    assert!(repo_file(".github/workflows/ci.yml").contains("workflow_call:"));
}

/// The `## `-level section of `markdown` whose heading starts with `heading`,
/// without the heading line, up to the next `## ` heading.
fn section<'a>(markdown: &'a str, heading: &str) -> Option<&'a str> {
    let start = markdown
        .match_indices("\n## ")
        .map(|(at, _)| at + 1)
        .chain(markdown.starts_with("## ").then_some(0))
        .find(|&at| markdown[at + 3..].starts_with(heading))?;
    let body_start = start + markdown[start..].find('\n')? + 1;
    let end = markdown[body_start..]
        .find("\n## ")
        .map_or(markdown.len(), |at| body_start + at + 1);
    Some(&markdown[body_start..end])
}

/// The `#123` issue references in `text`, in order, deduplicated. A link
/// anchor such as `SPEC.md#5-performance` is not an issue reference.
fn issue_refs(text: &str) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    for (at, _) in text.match_indices('#') {
        let rest = &text[at + 1..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let next = rest[digits.len()..].chars().next();
        let ends_a_number = next.is_none_or(|c| !c.is_alphanumeric() && c != '-');
        if !digits.is_empty() && ends_a_number && !refs.contains(&digits) {
            refs.push(digits);
        }
    }
    refs
}

#[test]
fn the_section_helper_finds_a_heading_and_stops_at_the_next() {
    let doc = "# T\n\n## A\none #1\n## B\ntwo #2 #2 #30\n";
    assert_eq!(section(doc, "A"), Some("one #1\n"));
    assert_eq!(section(doc, "B"), Some("two #2 #2 #30\n"));
    assert_eq!(section(doc, "C"), None);
    assert_eq!(issue_refs("two #2 #2 #30"), vec!["2", "30"]);
    assert_eq!(
        issue_refs("[SPEC §5](docs/SPEC.md#5-performance) (#194)"),
        vec!["194"]
    );
}

/// Issue #235: whoever downloads a release reads its GitHub Release page,
/// not the README, so the known limitations the README lists must be on the
/// release notes too — every one of them, by issue number.
#[test]
fn release_notes_list_every_known_limitation_the_readme_lists() {
    let readme = repo_file("README.md");
    let notes = repo_file("packaging/release-notes.md");
    let readme_limits =
        section(&readme, "Known limitations").expect("README has a Known limitations section");
    let notes_limits = section(&notes, "Known limitations")
        .expect("release notes have a Known limitations section");
    let expected = issue_refs(readme_limits);
    assert!(!expected.is_empty(), "README limitations cite their issues");
    let missing: Vec<_> = expected
        .iter()
        .filter(|n| !issue_refs(notes_limits).contains(n))
        .collect();
    assert!(
        missing.is_empty(),
        "release notes lack known limitations {missing:?}"
    );
}

/// Issue #235 / CLAUDE.md §Release checklist: the CHANGELOG section of the
/// version being released is finalized before the tag, so the version in
/// Cargo.toml has its own dated `## [x.y.z] - YYYY-MM-DD` section, below an
/// `[Unreleased]` section for whatever comes next.
#[test]
fn the_changelog_has_a_dated_section_for_the_workspace_version() {
    let cargo = repo_file("Cargo.toml");
    let version = cargo
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("workspace version in Cargo.toml");
    let changelog = repo_file("CHANGELOG.md");
    let heading = format!("\n## [{version}] - ");
    let at = changelog
        .find(&heading)
        .unwrap_or_else(|| panic!("CHANGELOG.md has no `## [{version}] - <date>` section"));
    let date: String = changelog[at + heading.len()..].chars().take(10).collect();
    let bytes = date.as_bytes();
    assert!(
        date.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && date
                .chars()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()),
        "the [{version}] section is dated YYYY-MM-DD, got {date:?}"
    );
    let unreleased = changelog
        .find("\n## [Unreleased]")
        .expect("CHANGELOG.md keeps an [Unreleased] section");
    assert!(unreleased < at, "[Unreleased] comes before [{version}]");
}
