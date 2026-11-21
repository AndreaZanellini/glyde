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

//! Issue #233: the cache holds decoded copies of the user's data and the logs
//! hold file paths, so both directories are readable by their owner only.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use glyde_core::index::private_dir::ensure_private_dir;

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

#[test]
fn a_new_directory_is_created_for_its_owner_only() {
    let root = tempfile::tempdir().expect("temp dir");
    let dir = root.path().join("a").join("glyde");

    ensure_private_dir(&dir).expect("create");

    assert!(dir.is_dir());
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn an_existing_directory_open_to_others_is_tightened() {
    let root = tempfile::tempdir().expect("temp dir");
    let dir = root.path().join("glyde");
    std::fs::create_dir(&dir).expect("mkdir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    ensure_private_dir(&dir).expect("tighten");

    assert_eq!(mode(&dir), 0o700);
}
