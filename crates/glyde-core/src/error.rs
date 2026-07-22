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

//! The one error type every fallible `glyde-core` operation returns.
//!
//! Ingestion, indexing, and DSP must never panic on user data (CLAUDE.md
//! Golden Rules; docs/ARCHITECTURE.md §Error philosophy) — a malformed file
//! or an unreadable path becomes a [`GlydeError`] instead, which
//! `glyde-app` reports at its `anyhow`-wrapped boundary.

use std::path::PathBuf;

/// Every error `glyde-core` can produce.
#[derive(Debug, thiserror::Error)]
pub enum GlydeError {
    /// A file could not be opened or read.
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// No registered [`crate::ingest::Reader`] recognizes this path's format
    /// (docs/SPEC.md §1.1 lists the v1-frozen set: `.csv`, `.tsv`, `.txt`,
    /// `.parquet`).
    #[error("unrecognized format: {path}")]
    UnrecognizedFormat { path: PathBuf },
}

/// The `Result` alias every fallible `glyde-core` function returns.
pub type Result<T> = std::result::Result<T, GlydeError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn read_file(path: &Path) -> Result<String> {
        std::fs::read_to_string(path).map_err(|source| GlydeError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    #[test]
    fn displays_a_human_readable_message() {
        let err = GlydeError::Io {
            path: Path::new("/nonexistent/glyde-error-test.csv").to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        assert_eq!(
            err.to_string(),
            "could not read /nonexistent/glyde-error-test.csv: no such file"
        );
    }

    #[test]
    fn propagates_through_question_mark_and_preserves_source() {
        let path = Path::new("/nonexistent/glyde-error-test.csv");
        let err = read_file(path).expect_err("reading a nonexistent file must fail");

        let GlydeError::Io { path: err_path, .. } = &err else {
            panic!("expected the Io variant, got {err:?}");
        };
        assert_eq!(err_path, path);

        let source = std::error::Error::source(&err).expect("Io variant must carry a source");
        assert_eq!(
            source
                .downcast_ref::<std::io::Error>()
                .expect("source must be the io::Error")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
