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

// Issue #233: catching a panic in the Parquet decoders needs unwinding.
#[cfg(panic = "abort")]
compile_error!(
    "glyde-app must be built with panic = \"unwind\": see glyde_core::panic_guard (issue #233)"
);

fn main() -> anyhow::Result<()> {
    // Issue #233: any panic outside a guarded Parquet open still ends the
    // process, as `panic = "abort"` did.
    glyde_core::panic_guard::install_abort_outside_guards();
    // Keep the guard alive for the whole process: dropping it stops the
    // background thread that flushes log lines to disk.
    let _logging_guard = glyde_app::logging::init()?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "glyde starting");

    // Issues #118 and #213: give back the disk space a previous run's spill
    // files still hold, then trim the reused caches to their size cap, in the
    // background.
    match glyde_core::index::level0::os_cache_dir() {
        Ok(cache_dir) => {
            // Issue #233: the cache holds decoded copies of opened files, so
            // its root is readable by its owner only.
            let root = cache_dir.parent().unwrap_or(&cache_dir);
            if let Err(err) = glyde_core::index::private_dir::ensure_private_dir(root) {
                tracing::warn!(
                    path = %root.display(),
                    error = %err,
                    "could not make the cache directory owner-only"
                );
            }
            glyde_app::plumbing::spawn_startup_cache_sweep(cache_dir);
        }
        Err(err) => tracing::warn!(error = %err, "no cache directory; skipping the cache sweep"),
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
    let mut viewport = egui::ViewportBuilder::default().with_title("Glyde");
    // Issue #232: the icon of the title bar, taskbar and app switcher. A
    // missing icon is cosmetic: log it and open the window anyway.
    match glyde_app::icon::window_icon() {
        Ok(icon) => viewport = viewport.with_icon(std::sync::Arc::new(icon)),
        Err(err) => {
            tracing::warn!(error = %err, "window icon could not be decoded; opening without it")
        }
    }
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let result = eframe::run_native(
        "Glyde",
        native_options,
        Box::new(|creation_context| {
            // Issue #229: the visual identity, light or dark as the OS says.
            glyde_app::theme::install(&creation_context.egui_ctx);
            Ok(Box::new(GlydeApp::new()))
        }),
    )
    .map_err(|err| anyhow::anyhow!("glyde window failed: {err}"));

    // Issue #118: closing the window closed the open file, whose spill files
    // are being deleted in the background; finish that before the process
    // ends instead of leaving them for the next start's sweep.
    glyde_core::index::spill::wait_for_pending_deletions();
    result
}
