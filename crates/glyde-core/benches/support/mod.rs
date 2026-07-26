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

//! Fixture helpers shared by `index_build` and `viewport_query`
//! (docs/QUALITY.md §3, docs/ROADMAP.md M3). Lives under `benches/support/`
//! rather than directly in `benches/` so Cargo's bench auto-discovery does
//! not also try to treat this as its own bench target (mirrors
//! `tests/support/mod.rs`).
//!
//! Not a dependency on `glyde-devtools`: that crate depends on
//! `glyde-core`, and a bench-only dependency back onto it would be an
//! unnecessary and confusing reverse edge in the crate graph for what is a
//! ~30-line fixture writer. The two are similar in spirit but serve
//! different needs (this one targets a fixed, small, in-repo-testable size
//! for fast `cargo bench` runs; `glyde-devtools` streams arbitrarily large
//! CI fixtures without holding them in memory).

#![allow(dead_code)] // Not every bench uses every helper.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// A tiny deterministic PRNG (xorshift64*), the same one
/// `tests/golden/decimation.rs` uses, so "random" bench fixtures are
/// reproducible without adding a `rand` dependency to `glyde-core`.
pub struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Uniform in `[-1.0, 1.0)`.
    pub fn next_f64(&mut self) -> f64 {
        let bits = self.next_u64() >> 11; // 53 significant bits
        (bits as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

/// Directory generated bench fixtures live in: the OS temp directory, not a
/// repo-relative path — `cargo bench` (like `cargo test`) runs the bench
/// binary with its working directory set to the *package* root
/// (`crates/glyde-core/`), not the workspace root, so a repo-relative path
/// here would silently land in the wrong place (and need its own
/// `.gitignore` entry) depending on how the binary is invoked. Also reused
/// across `cargo bench` runs instead of regenerated every time.
pub fn fixture_dir() -> PathBuf {
    std::env::temp_dir().join("glyde-bench-fixtures")
}

/// Builds an in-memory synthetic series of `n` samples: deterministic
/// pseudo-random `f64` values, one per integer tick (matches the timestamp
/// convention `tests/golden/decimation.rs` uses — pyramid math only cares
/// about ordering, not units).
pub fn synthetic_series(n: usize, seed: u64) -> (Vec<f64>, Vec<i128>) {
    let mut rng = Xorshift64::new(seed);
    let samples: Vec<f64> = (0..n).map(|_| rng.next_f64() * 1000.0).collect();
    let timestamps: Vec<i128> = (0..n as i128).collect();
    (samples, timestamps)
}

/// Writes a deterministic CSV fixture at `path` (an ISO 8601 timestamp
/// column plus `columns` `f64` columns), streamed row-by-row until it
/// reaches `target_bytes`, if it does not already exist. Reused across
/// `cargo bench` invocations instead of regenerated every run.
pub fn ensure_csv_fixture(
    path: &Path,
    target_bytes: u64,
    columns: usize,
    seed: u64,
) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);

    write!(writer, "timestamp")?;
    for col in 0..columns {
        write!(writer, ",ch{col}")?;
    }
    writeln!(writer)?;

    let mut rng = Xorshift64::new(seed);
    let epoch_start_ms: i64 = 1_700_000_000_000;
    let mut bytes_written: u64 = 0;
    let mut row: u64 = 0;
    let mut line = String::with_capacity(160);
    while bytes_written < target_bytes {
        line.clear();
        line.push_str(&format_iso8601_millis(epoch_start_ms + row as i64));
        for _ in 0..columns {
            line.push(',');
            line.push_str(&format!("{:.6}", rng.next_f64() * 1000.0));
        }
        line.push('\n');
        writer.write_all(line.as_bytes())?;
        bytes_written += line.len() as u64;
        row += 1;
    }

    writer.flush()
}

/// Formats milliseconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SS.sssZ`.
/// Small, self-contained civil-calendar conversion (Howard Hinnant's
/// `civil_from_days`) so this bench-only helper does not need a timezone
/// database.
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
