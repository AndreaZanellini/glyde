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
//!
//! This library target exists only so `src/main.rs` and this crate's own
//! `benches/` can share one crate root (docs/ROADMAP.md M3, issue #80: a
//! frame-time gate over the real render path needs `views::time::show`
//! reachable from a `[[bench]]`, which a bin-only crate cannot do) — it is
//! not a public API for other crates to depend on.

pub mod app;
pub mod error_boundary;
pub mod inference_bar;
pub mod logging;
pub mod plumbing;
pub mod views;

pub use app::GlydeApp;
