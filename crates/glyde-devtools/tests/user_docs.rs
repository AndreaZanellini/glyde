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

/// Every `.rs` file under `dir`, concatenated: the text a UI label must
/// appear in.
fn rust_sources(dir: &Path) -> String {
    let mut text = String::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|err| panic!("list {}: {err}", dir.display()));
        for entry in entries {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                text.push_str(&std::fs::read_to_string(&path).expect("read source"));
                text.push('\n');
            }
        }
    }
    text
}

/// The `**bold**` spans of `markdown`, outside fenced code.
fn bold_spans(markdown: &str) -> Vec<String> {
    let mut in_code = false;
    let mut spans = Vec::new();
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let mut parts = line.split("**");
        parts.next(); // text before the first `**`
        while let (Some(bold), Some(_)) = (parts.next(), parts.next()) {
            spans.push(bold.to_string());
        }
    }
    spans
}

/// The literal pieces of a bold label the app's source must contain: a menu
/// path `File → Open…` or a pair `Sort / Keep as-is` is checked piece by
/// piece, and a standalone `N` stands for a number the app formats in
/// (`Use N-sample segments` is `format!("Use {}-sample segments", …)`).
fn label_pieces(label: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    for part in label.split(" → ").flat_map(|part| part.split(" / ")) {
        let words: Vec<&str> = part.split(' ').collect();
        let mut current = Vec::new();
        for word in words {
            if let Some(rest) = word.strip_prefix('N').filter(|rest| {
                rest.is_empty() || rest.starts_with('-') || rest.starts_with(['.', ','])
            }) {
                if !current.is_empty() {
                    pieces.push(format!("{} ", current.join(" ")));
                    current.clear();
                }
                if !rest.is_empty() {
                    current.push(rest);
                }
            } else {
                current.push(word);
            }
        }
        if !current.is_empty() {
            let joined = current.join(" ");
            // A piece that followed an `N` keeps its leading space.
            pieces.push(if label.contains(&format!("N {joined}")) {
                format!(" {joined}")
            } else {
                joined
            });
        }
    }
    pieces.retain(|piece| !piece.trim().is_empty());
    pieces
}

#[test]
fn every_bold_control_in_the_user_guide_is_a_label_the_app_shows() {
    // The guide's convention (stated in its first paragraph): bold is
    // exactly what the user sees on screen. A label that drifts from the
    // app's text leaves a colleague looking for a button that is not there.
    let root = repo_root();
    let app_text = rust_sources(&root.join("crates/glyde-app/src"))
        + &rust_sources(&root.join("crates/glyde-core/src"));
    let guide = repo_file("docs/USER-GUIDE.md");
    assert!(guide.contains("**bold** words are exactly what you\nsee on screen"));
    let mut missing = Vec::new();
    for label in bold_spans(&guide) {
        if label == "bold" {
            continue; // the convention's own statement
        }
        for piece in label_pieces(&label) {
            if !app_text.contains(&piece) {
                missing.push(format!(
                    "**{label}** ({piece:?} is not in the app's source)"
                ));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "docs/USER-GUIDE.md names controls the app does not show:\n{}",
        missing.join("\n")
    );
}

#[test]
fn label_pieces_split_menu_paths_pairs_and_numbers() {
    assert_eq!(label_pieces("File → Open…"), ["File", "Open…"]);
    assert_eq!(label_pieces("Sort / Keep as-is"), ["Sort", "Keep as-is"]);
    assert_eq!(
        label_pieces("Use N-sample segments"),
        ["Use ", "-sample segments"]
    );
    assert_eq!(
        label_pieces("N rows skipped — view details"),
        [" rows skipped — view details"]
    );
    assert_eq!(label_pieces("None"), ["None"]);
}

#[test]
fn the_documented_cache_folders_are_the_ones_the_app_uses() {
    // `ProjectDirs::from("com", "glyde", "Glyde").cache_dir()` resolves to
    // these three folders; the app keeps its caches below them.
    let level0 = repo_file("crates/glyde-core/src/index/level0.rs");
    for required in [
        "ProjectDirs::from(\"com\", \"glyde\", \"Glyde\")",
        "project_dirs.cache_dir().join(\"index\")",
    ] {
        assert!(
            level0.contains(required),
            "os_cache_dir() changed ({required:?} is gone): update the cache folders in \
             docs/USER-GUIDE.md, then this test"
        );
    }
    let guide = repo_file("docs/USER-GUIDE.md");
    for folder in [
        "~/Library/Caches/com.glyde.Glyde/",
        "%LOCALAPPDATA%\\glyde\\Glyde\\cache\\",
        "~/.cache/glyde/",
    ] {
        assert!(
            guide.contains(folder),
            "the user guide lacks the cache folder {folder}"
        );
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
