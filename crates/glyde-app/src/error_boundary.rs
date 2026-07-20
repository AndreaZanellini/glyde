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

//! Proves the glyde-core -> glyde-app error boundary (docs/ARCHITECTURE.md
//! §Error philosophy): `glyde-core` returns `GlydeError` (`thiserror`), and
//! `glyde-app` reports failures as `anyhow::Result` without needing to know
//! `GlydeError`'s concrete shape. This module has no runtime code of its
//! own — it exists only to hold that proof.

#[cfg(test)]
mod tests {
    use glyde_core::{GlydeError, Result as CoreResult};
    use std::path::Path;

    fn read_via_core(path: &Path) -> CoreResult<String> {
        std::fs::read_to_string(path).map_err(|source| GlydeError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    fn app_boundary(path: &Path) -> anyhow::Result<String> {
        Ok(read_via_core(path)?)
    }

    #[test]
    fn core_error_propagates_through_the_anyhow_boundary() {
        let err = app_boundary(Path::new("/nonexistent/glyde-error-test.csv"))
            .expect_err("reading a nonexistent file must fail");

        assert!(err.to_string().contains("could not read"));
        assert!(
            err.downcast_ref::<GlydeError>().is_some(),
            "anyhow::Error must retain the concrete GlydeError for callers that want it"
        );
    }
}
