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

//! Glyde: glide through your time series.
//!
//! The actual application code lives in `src/lib.rs` and its modules; this
//! binary is just the native entry point (docs/ARCHITECTURE.md: `glyde-app`
//! is a thin shell).

use glyde_app::GlydeApp;

fn main() -> anyhow::Result<()> {
    // Keep the guard alive for the whole process: dropping it stops the
    // background thread that flushes log lines to disk.
    let _logging_guard = glyde_app::logging::init()?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "glyde starting");

    // Issue #118: give back the disk space a previous run's spill files
    // still hold, in the background.
    match glyde_core::index::level0::os_cache_dir() {
        Ok(cache_dir) => {
            glyde_app::plumbing::spawn_spill_sweep(cache_dir);
        }
        Err(err) => tracing::warn!(error = %err, "no cache directory; skipping the spill sweep"),
    }

    // Plan the PSD's FFTs now, off the UI thread, while memory is
    // plentiful: rustfft allocates a plan infallibly, so a PSD must never be
    // the one asking for it (`dsp::welch::prepare_fft_plans`).
    let fft_planning = std::thread::Builder::new()
        .name("glyde-fft-plans".to_string())
        .spawn(|| {
            let started = std::time::Instant::now();
            glyde_core::dsp::welch::prepare_fft_plans();
            tracing::info!(elapsed = ?started.elapsed(), "PSD FFT plans prepared");
        });
    if let Err(err) = fft_planning {
        tracing::warn!(error = %err, "could not plan the PSD FFTs at startup; they will be planned on first use");
    }

    // SPEC §6: single window, single file at a time.
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("Glyde"),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Glyde",
        native_options,
        Box::new(|_creation_context| Ok(Box::new(GlydeApp::new()))),
    )
    .map_err(|err| anyhow::anyhow!("glyde window failed: {err}"));

    // Issue #118: closing the window closed the open file, whose spill files
    // are being deleted in the background; finish that before the process
    // ends instead of leaving them for the next start's sweep.
    glyde_core::index::spill::wait_for_pending_deletions();
    result
}
