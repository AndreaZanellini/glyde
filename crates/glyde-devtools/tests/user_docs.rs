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

//! Issue #234: the end-user documentation (README, user guide) is what a
//! colleague reads instead of asking. Nothing renders it in CI, so what can
//! rot silently is locked here: every relative link and image resolves, every
//! `#anchor` names a real heading, the first-run workaround matches the
//! release notes, and the log folders it tells users to attach match the ones
//! the app writes to.

use std::path::{Component, Path, PathBuf};

/// The documents a user (or a contributor arriving from the README) follows.
const USER_DOCS: [&str; 4] = [
    "README.md",
    "docs/USER-GUIDE.md",
    "docs/history/README.md",
    "CONTRIBUTING.md",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn repo_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// Every link or image target in `markdown`: `[text](target)`,
/// `![alt](target)` and HTML `src="target"`.
fn link_targets(markdown: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for (start, _) in markdown.match_indices("](") {
        let rest = &markdown[start + 2..];
        if let Some(end) = rest.find(')') {
            targets.push(rest[..end].trim().to_string());
        }
    }
    for (start, _) in markdown.match_indices("src=\"") {
        let rest = &markdown[start + 5..];
        if let Some(end) = rest.find('"') {
            targets.push(rest[..end].to_string());
        }
    }
    targets
}

/// GitHub's heading anchor: lowercase, punctuation dropped (hyphens and
/// underscores kept), spaces to hyphens.
fn github_slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

fn heading_slugs(markdown: &str) -> Vec<String> {
    let mut in_code = false;
    let mut slugs = Vec::new();
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let hashes = line.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            slugs.push(github_slug(&line[hashes..]));
        }
    }
    slugs
}

/// `base.join(relative)` with `.` and `..` folded, or `None` when it climbs
/// above the repository root: a GitHub web route such as `../../releases`,
/// which only resolves on github.com.
fn resolve_in_repo(doc_dir: &Path, relative: &str) -> Option<PathBuf> {
    let mut parts: Vec<String> = Vec::new();
    for component in doc_dir.join(relative).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            _ => return None,
        }
    }
    Some(parts.iter().collect())
}

#[test]
fn every_relative_link_and_image_in_the_user_docs_resolves() {
    let root = repo_root();
    let mut broken = Vec::new();
    for doc in USER_DOCS {
        let text = repo_file(doc);
        let doc_dir = Path::new(doc).parent().unwrap_or(Path::new(""));
        for target in link_targets(&text) {
            if target.contains("://") || target.starts_with("mailto:") {
                continue;
            }
            let (path, anchor) = match target.split_once('#') {
                Some((path, anchor)) => (path, Some(anchor)),
                None => (target.as_str(), None),
            };
            let resolved = if path.is_empty() {
                Some(PathBuf::from(doc))
            } else {
                resolve_in_repo(doc_dir, path)
            };
            let Some(resolved) = resolved else {
                continue; // a github.com route above the repository root
            };
            if !root.join(&resolved).exists() {
                broken.push(format!("{doc}: {target} (no {})", resolved.display()));
                continue;
            }
            if let Some(anchor) = anchor {
                let linked = repo_file(&resolved.to_string_lossy());
                if !heading_slugs(&linked).iter().any(|slug| slug == anchor) {
                    broken.push(format!("{doc}: {target} (no such heading)"));
                }
            }
        }
    }
    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

#[test]
fn the_slug_rule_matches_githubs_on_the_headings_the_docs_link_to() {
    assert_eq!(
        github_slug(" 5. Performance (contract, enforced by CI)"),
        "5-performance-contract-enforced-by-ci"
    );
    assert_eq!(github_slug(" Spectrum (PSD)"), "spectrum-psd");
    assert_eq!(
        github_slug(" A file that won't open?"),
        "a-file-that-wont-open"
    );
}

#[test]
fn the_readme_has_a_screenshot_and_no_leftover_todo() {
    let readme = repo_file("README.md");
    assert!(
        readme.contains("<img src=\"docs/images/"),
        "the README shows the app"
    );
    assert!(!readme.contains("TODO"), "the README has a leftover TODO");
}

#[test]
fn the_readme_gives_the_same_first_run_workaround_as_the_release_notes() {
    let readme = repo_file("README.md");
    let notes = repo_file("packaging/release-notes.md");
    for required in [
        "right-click (or Control-click) Glyde → Open",
        "xattr -dr com.apple.quarantine /Applications/Glyde.app",
        "More info → Run anyway",
        "chmod +x glyde-*.AppImage",
    ] {
        assert!(notes.contains(required), "release notes lost {required:?}");
        assert!(readme.contains(required), "README lacks {required:?}");
    }
}

#[test]
fn the_documented_log_folders_are_the_ones_the_app_writes_to() {
    // `directories::ProjectDirs::from("com", "glyde", "Glyde")` resolves
    // `data_local_dir()` to these three folders; the app adds `logs/`.
    let logging = repo_file("crates/glyde-app/src/logging.rs");
    for required in [
        "ProjectDirs::from(\"com\", \"glyde\", \"Glyde\")",
        "data_local_dir()",
        "root.join(\"logs\")",
        "rolling::daily(log_dir, \"glyde.log\")",
    ] {
        assert!(
            logging.contains(required),
            "logging.rs changed ({required:?} is gone): update the log folders in \
             README.md and docs/USER-GUIDE.md, then this test"
        );
    }
    for doc in ["README.md", "docs/USER-GUIDE.md"] {
        let text = repo_file(doc);
        for folder in [
            "~/Library/Application Support/com.glyde.Glyde/logs/",
            "%LOCALAPPDATA%\\glyde\\Glyde\\data\\logs\\",
            "~/.local/share/glyde/logs/",
        ] {
            assert!(text.contains(folder), "{doc} lacks the log folder {folder}");
        }
    }
}
