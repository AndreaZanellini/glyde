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

//! The three SPEC §4 visualizations (docs/ARCHITECTURE.md workspace layout
//! `views/`). Only [`time`] exists so far (docs/ROADMAP.md M2 "Time-domain
//! view v1"); `psd` and `state_timeline` land with M5 and M6.

pub mod time;
