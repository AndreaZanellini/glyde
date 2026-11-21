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

//! Issue #233: every reused cache (the whole-dataset cache, the Level-0
//! sample caches, the pyramid caches) lives in a user-writable directory, so
//! a reopen reads it as untrusted input. The first byte picks one of the
//! cache files a real open of a small, varied CSV wrote; the rest replaces
//! that file's contents. The reopen must then either reject it and rebuild,
//! or read it without a panic or an out-of-bounds access.
//!
//! Seed the corpus with the real cache files (each prefixed by its index)
//! by running once with `GLYDE_FUZZ_WRITE_SEEDS=<corpus dir>` set.

#![no_main]

#[path = "common.rs"]
mod common;

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

struct Fixture {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    source: PathBuf,
    cache: PathBuf,
    /// Every cache file a first open wrote, with its good contents.
    files: Vec<(PathBuf, Vec<u8>)>,
}

/// Semicolons and comma decimals, UTC offsets, a skipped row, a NaN run, and
/// string, bool, i64 and f64 columns: every array kind the caches hold.
fn fixture_csv() -> String {
    let mut text = String::from("timestamp;value;count;flag;label\n");
    for i in 0..300_i64 {
        let value = if (100..103).contains(&i) {
            "NaN".to_string()
        } else {
            format!("{},{}", i, i % 10)
        };
        text.push_str(&format!(
            "2026-01-01T00:{:02}:{:02}+02:00;{value};{};{};{}\n",
            i / 60,
            i % 60,
            i * 3 - 7,
            i % 2 == 0,
            ["idle", "run", "défaut", ""][(i % 4) as usize],
        ));
        if i == 150 {
            text.push_str("garbage;row\n");
        }
    }
    text
}

fn cache_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).expect("read cache dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                // Spill sets are per-open backing stores, not reread.
                if entry.file_name() != "spill" {
                    pending.push(path);
                }
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let source_dir = tempfile::tempdir().expect("temp dir");
        let cache_dir = tempfile::tempdir().expect("temp cache dir");
        let source = source_dir.path().join("rich.csv");
        std::fs::write(&source, fixture_csv()).expect("write fixture");
        let cache = cache_dir.path().to_path_buf();
        common::open_and_derive(&source, common::budget(true), &cache);
        let files: Vec<(PathBuf, Vec<u8>)> = cache_files(&cache)
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path).expect("read cache file");
                (path, bytes)
            })
            .collect();
        assert!(files.len() >= 3, "the first open wrote its caches");
        if let Some(seeds) = std::env::var_os("GLYDE_FUZZ_WRITE_SEEDS") {
            let seeds = PathBuf::from(seeds);
            std::fs::create_dir_all(&seeds).expect("seed dir");
            for (index, (_, bytes)) in files.iter().enumerate() {
                let mut seed = vec![index as u8];
                seed.extend_from_slice(bytes);
                std::fs::write(seeds.join(format!("cache-file-{index}")), seed)
                    .expect("write seed");
            }
        }
        Fixture {
            _dirs: (source_dir, cache_dir),
            source,
            cache,
            files,
        }
    })
}

fuzz_target!(|data: &[u8]| {
    let fixture = fixture();
    let Some((&pick, bytes)) = data.split_first() else {
        return;
    };
    // Every run starts from the good caches, so a finding reproduces alone.
    for (path, good) in &fixture.files {
        std::fs::write(path, good).expect("restore cache file");
    }
    let (target, _) = &fixture.files[pick as usize % fixture.files.len()];
    std::fs::write(target, bytes).expect("tamper cache file");
    common::open_and_derive(&fixture.source, common::budget(true), &fixture.cache);
});
