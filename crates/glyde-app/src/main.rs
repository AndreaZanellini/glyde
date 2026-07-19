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
//! Thin shell: renders state and forwards user intent. If a behavior can be
//! tested without a window, it belongs in glyde-core. See docs/ARCHITECTURE.md.

mod logging;

fn main() -> anyhow::Result<()> {
    // Keep the guard alive for the whole process: dropping it stops the
    // background thread that flushes log lines to disk.
    let _logging_guard = logging::init()?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "glyde starting");

    // The eframe shell (window, File->Open, drag-drop) arrives with M2 —
    // see docs/ROADMAP.md. Until then this stub proves logging works.
    println!("glyde: scaffolding stub");
    Ok(())
}
