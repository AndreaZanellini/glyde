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

//! Glyde engine: ingestion, time indexing, multi-resolution index, and DSP.
//!
//! This crate contains ALL product logic and has NO GUI dependencies, so every
//! behavior is testable headlessly. See docs/ARCHITECTURE.md.
//!
//! Scaffolding stub: modules are created by the first implementation task.

mod error;
pub mod ingest;
pub use error::{GlydeError, Result};

// TODO(scaffold): pub mod budget; ingest; time; index; dsp; series;
