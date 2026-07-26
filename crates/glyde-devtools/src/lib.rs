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

//! Shared helpers for `generate_fixtures` and `memory_gate` (docs/QUALITY.md
//! §3, docs/ROADMAP.md M3). Split out of the two binaries so both agree on
//! the fixture path for a given size without a path being passed between the
//! separate `cargo run` invocations `.github/workflows/ci.yml` makes for
//! each (see issue #61).

use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

/// Directory generated fixtures live in. Never committed — reserved by
/// `.gitignore` (docs/QUALITY.md §3: "large fixtures ... never committed").
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from("testdata/generated")
}

/// Deterministic path for the CSV fixture of a given size, so a `cargo run
/// --bin memory_gate -- --size-gb N` in a later CI step finds exactly what
/// `generate_fixtures --size-gb N` wrote, without a path argument.
pub fn fixture_path(size_gb: f64) -> PathBuf {
    fixtures_dir().join(format!("fixture_{size_gb}gb.csv"))
}

/// Number of `f64` data columns a generated fixture carries, alongside its
/// timestamp column.
pub const FIXTURE_COLUMN_COUNT: usize = 8;

/// Writes a synthetic CSV fixture at `path`: an ISO 8601 header row, then a
/// millisecond-cadence timestamp column plus [`FIXTURE_COLUMN_COUNT`]
/// pseudo-random `f64` columns, streamed row-by-row until the file reaches
/// `target_bytes` (never materialized in memory — this itself must not
/// defeat the point of a memory-gate fixture). Returns the number of rows
/// written.
pub fn write_csv_fixture(path: &Path, target_bytes: u64, seed: u64) -> Result<u64> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating fixture directory {}", parent.display()))?;
    }

    let file =
        File::create(path).with_context(|| format!("creating fixture file {}", path.display()))?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);

    write!(writer, "timestamp")?;
    for col in 0..FIXTURE_COLUMN_COUNT {
        write!(writer, ",ch{col}")?;
    }
    writeln!(writer)?;

    let mut rng = StdRng::seed_from_u64(seed);
    let epoch_start_ms: i64 = 1_700_000_000_000; // 2023-11-14T22:13:20Z, arbitrary fixed anchor
    let mut row_count: u64 = 0;
    let mut bytes_written: u64 = 0;
    let mut row = String::with_capacity(160);

    while bytes_written < target_bytes {
        row.clear();
        let ts_ms = epoch_start_ms + row_count as i64;
        row.push_str(&format_iso8601_millis(ts_ms));
        for _ in 0..FIXTURE_COLUMN_COUNT {
            row.push(',');
            let value: f64 = rng.gen_range(-1000.0..1000.0);
            row.push_str(&format!("{value:.6}"));
        }
        row.push('\n');

        writer.write_all(row.as_bytes())?;
        bytes_written += row.len() as u64;
        row_count += 1;
    }

    writer.flush()?;
    Ok(row_count)
}

/// Formats milliseconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SS.sssZ`
/// without pulling `chrono` into this crate's hot fixture-writing loop —
/// this is a fixed, always-valid civil-calendar conversion (no timezone
/// database needed for UTC), reimplemented here deliberately small.
fn format_iso8601_millis(epoch_ms: i64) -> String {
    let millis = epoch_ms.rem_euclid(1000);
    let epoch_secs = epoch_ms.div_euclid(1000);
    let days = epoch_secs.div_euclid(86_400);
    let secs_of_day = epoch_secs.rem_euclid(86_400);

    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a
/// proleptic-Gregorian (year, month, day), valid for the full `i64` range.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

/// Samples this process's resident memory on a background thread at a fixed
/// interval and tracks the maximum observed, as a portable approximation of
/// "peak RSS" (docs/QUALITY.md §3) that needs no platform-specific API:
/// `sysinfo` already reports current process RSS on Linux/macOS/Windows
/// alike. The polling interval bounds how short a spike can be and still be
/// caught — short enough to catch a "materializes the whole file" bug, which
/// holds the extra memory for the entire parse, not a few milliseconds.
pub struct PeakRssSampler {
    peak_bytes: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PeakRssSampler {
    /// Starts sampling this process's RSS every `interval`.
    pub fn start(interval: Duration) -> Self {
        let peak_bytes = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let peak_bytes_thread = Arc::clone(&peak_bytes);
        let stop_thread = Arc::clone(&stop);
        let pid = Pid::from_u32(std::process::id());

        let handle = std::thread::spawn(move || {
            let mut system = System::new_with_specifics(
                RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing().with_memory()),
            );
            while !stop_thread.load(Ordering::Relaxed) {
                system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
                if let Some(process) = system.process(pid) {
                    let current = process.memory();
                    peak_bytes_thread.fetch_max(current, Ordering::Relaxed);
                }
                std::thread::sleep(interval);
            }
        });

        Self {
            peak_bytes,
            stop,
            handle: Some(handle),
        }
    }

    /// Stops sampling and returns the peak RSS observed, in bytes.
    pub fn stop(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.peak_bytes.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_path_is_a_deterministic_function_of_size() {
        assert_eq!(fixture_path(8.0), fixture_path(8.0));
        assert_ne!(fixture_path(1.0), fixture_path(8.0));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_675), (2023, 11, 14));
    }

    #[test]
    fn format_iso8601_millis_matches_a_known_instant() {
        assert_eq!(
            format_iso8601_millis(1_700_000_000_000),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(
            format_iso8601_millis(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
    }

    #[test]
    fn write_csv_fixture_reaches_the_target_size_with_a_header_and_typed_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tiny.csv");

        let row_count = write_csv_fixture(&path, 4096, 42).expect("fixture write must succeed");
        assert!(row_count > 0);

        let contents = std::fs::read_to_string(&path).expect("fixture must be readable");
        assert!(contents.len() as u64 >= 4096);
        let mut lines = contents.lines();
        let header = lines.next().expect("fixture must have a header row");
        assert_eq!(header.split(',').count(), FIXTURE_COLUMN_COUNT + 1);
        let first_data_row = lines.next().expect("fixture must have at least one row");
        assert_eq!(first_data_row.split(',').count(), FIXTURE_COLUMN_COUNT + 1);
    }
}
