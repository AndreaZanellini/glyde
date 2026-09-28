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
//! `dsp::welch::{welch, welch_segmented}` are implemented (docs/ROADMAP.md M5
//! "Welch core"), but both require a resident input slice — there
//! is no streaming source-based path yet. The 10M-sample budget this bench
//! must assert against belongs to that streaming path (docs/ROADMAP.md M5
//! "Streaming Welch"), so this stays a stub until that item lands; benching
//! the current in-memory `welch` against a 10M-sample budget would measure
//! the wrong code path.

fn main() {
    // TODO(M5 "Streaming Welch"): criterion harness against the streaming
    // path; assert against the SPEC §5 PSD budget.
    println!(
        "bench welch: scaffolding stub — blocked on docs/ROADMAP.md M5 \"Streaming Welch\" \
         (dsp::welch::welch/welch_segmented exist but have no streaming variant yet)"
    );
}
