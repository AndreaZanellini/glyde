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

//! Owner-only directories for what Glyde keeps on disk (issue #233).
//!
//! The caches hold decoded copies of the user's data (every sample, every
//! string) and the logs hold file paths, column names and some cell values.
//! On a shared machine neither may be readable by other accounts, so the
//! root of each is created, or tightened if it already exists, with mode
//! `0700`. Everything Glyde creates below that root inherits the protection:
//! nobody else can traverse into it.
//!
//! On Windows the per-user `%LOCALAPPDATA%` these roots live under is
//! already private to its owner by its ACL, so there is nothing to change and
//! this only creates the directory.
//!
//! A symlink in place of the directory is followed, not refused: moving a
//! multi-gigabyte cache to another disk with a link is a reasonable thing for
//! a user to do, and planting one requires write access to the user's own
//! home directory, which is already everything.

use std::path::Path;

use tracing::info;

/// Creates `dir` (and its parents) if missing, then makes it accessible to
/// its owner only. Logged at `info` when an existing directory was tightened.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict(dir)
}

#[cfg(unix)]
fn restrict(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    const OWNER_ONLY: u32 = 0o700;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode != OWNER_ONLY {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(OWNER_ONLY))?;
        info!(
            path = %dir.display(),
            was = format!("{mode:o}"),
            "made the directory readable by its owner only (issue #233)"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}
