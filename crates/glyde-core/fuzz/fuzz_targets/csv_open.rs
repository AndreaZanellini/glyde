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

//! Issue #233: arbitrary bytes saved as a `.csv` file and opened the way the
//! app opens it, in memory or spilled (first byte), then indexed.

#![no_main]

#[path = "common.rs"]
mod common;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, bytes)) = data.split_first() else {
        return;
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let cache = tempfile::tempdir().expect("temp cache dir");
    let path = dir.path().join("input.csv");
    std::fs::write(&path, bytes).expect("write input");
    common::open_and_derive(&path, common::budget(mode & 1 == 0), cache.path());
});
