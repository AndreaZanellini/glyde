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

//! Benchmark: welch. Budget is build-blocking (docs/SPEC.md §5, docs/QUALITY.md §3:
//! PSD of a 10M-sample selection ≤1s).
//!
//! Still a stub: `dsp::welch::{welch, welch_segmented}` are `todo!()` until
//! docs/ROADMAP.md M5 lands the real bodies (see docs/ROADMAP.md M3 for the
//! benches that *were* filled in this pass — index build, viewport query).
//! Benching a `todo!()` would only measure a panic.

fn main() {
    // TODO(M5): criterion harness; assert against the SPEC §5 PSD budget.
    println!(
        "bench welch: scaffolding stub — blocked on docs/ROADMAP.md M5 (dsp::welch is todo!())"
    );
}
