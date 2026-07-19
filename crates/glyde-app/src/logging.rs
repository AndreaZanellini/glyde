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

//! Tracing initialization: console output plus a daily-rotating log file in
//! the OS-standard app-data directory (SPEC §6, ARCH §Logging).
//!
//! A user reporting a bug must be able to attach one log file that explains
//! what happened, so the file sink and the console sink are always wired up
//! together — there is no "quiet" mode.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter, Registry};

/// Keeps the background log-file writer alive. Drop it only on application
/// exit — dropping it earlier stops flushing buffered log lines to disk.
pub struct LoggingGuard {
    _file_guard: WorkerGuard,
}

/// Initializes the global tracing subscriber: console output plus a
/// daily-rotating file under the OS app-data directory.
pub fn init() -> anyhow::Result<LoggingGuard> {
    let dir = log_dir()?;
    let (subscriber, guard) = build_subscriber(&dir)?;
    tracing::subscriber::set_global_default(subscriber)
        .context("a tracing subscriber was already installed")?;
    Ok(LoggingGuard { _file_guard: guard })
}

/// Resolves (and creates) the directory rotating log files are written to:
/// `<OS app-data dir>/logs`.
fn log_dir() -> anyhow::Result<PathBuf> {
    let project_dirs = directories::ProjectDirs::from("com", "glyde", "Glyde")
        .context("could not determine the OS app-data directory")?;
    let dir = project_dirs.data_local_dir().join("logs");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create log directory {}", dir.display()))?;
    Ok(dir)
}

/// Builds a subscriber writing to both stdout and a daily-rotating file
/// under `log_dir`, without installing it globally. Split out from [`init`]
/// so tests can exercise it against a temp directory via
/// `tracing::subscriber::with_default` instead of the real app-data dir.
fn build_subscriber(
    log_dir: &Path,
) -> anyhow::Result<(impl tracing::Subscriber + Send + Sync, WorkerGuard)> {
    let file_appender = tracing_appender::rolling::daily(log_dir, "glyde.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let console_layer = fmt::layer();
    let file_layer = fmt::layer().with_ansi(false).with_writer(non_blocking);

    let subscriber = Registry::default()
        .with(filter)
        .with(console_layer)
        .with(file_layer);

    Ok((subscriber, guard))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the M0 requirement "log file appears on run": emitting an
    /// event through the subscriber built by [`build_subscriber`] must
    /// produce a readable file containing that event.
    #[test]
    fn writes_log_file_and_console() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (subscriber, guard) = build_subscriber(dir.path()).expect("build subscriber");

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("hello from the test suite");
        });
        drop(guard); // flushes the non-blocking writer's background thread

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read log dir")
            .filter_map(Result::ok)
            .collect();
        assert!(
            !entries.is_empty(),
            "expected a rotating log file in {}",
            dir.path().display()
        );

        let content = entries
            .iter()
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            content.contains("hello from the test suite"),
            "log file did not contain the emitted event: {content:?}"
        );
    }
}
