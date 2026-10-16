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

//! Issue #30 / docs/ROADMAP.md M9: what a tagged release publishes is only
//! exercised when a tag is pushed, so the parts that can be checked without
//! one are locked here — the release notes' mandatory content (docs/SPEC.md
//! §7, CLAUDE.md §Release checklist) and the release workflow's gate and
//! artifact set.

use std::path::PathBuf;

fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

#[test]
fn release_notes_carry_the_unsigned_build_workarounds_and_the_qa_reminder() {
    let notes = repo_file("packaging/release-notes.md");
    for required in [
        // CLAUDE.md §Release checklist, verbatim.
        "Maintainer: run the 10-file smoke ritual (docs/QUALITY.md §Manual QA) before announcing.",
        // docs/SPEC.md §7: the exact first-run workaround on both platforms.
        "right-click (or Control-click) Glyde → Open",
        "xattr -dr com.apple.quarantine /Applications/Glyde.app",
        "More info → Run anyway",
    ] {
        assert!(
            notes.contains(required),
            "release notes must contain {required:?}"
        );
    }
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
