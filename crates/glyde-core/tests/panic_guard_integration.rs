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

//! Issue #233: the safety claim behind `panic = "unwind"`. With the app's
//! hook installed, a panic outside a guarded Parquet open still aborts the
//! process, and a panic inside one (corpus case 65) is an error the process
//! survives. Each test re-runs itself in a child process, because a hook
//! that aborts must not run inside the test harness.

use std::path::Path;
use std::process::{Command, ExitStatus};

use glyde_core::panic_guard::install_abort_outside_guards;

const CHILD: &str = "GLYDE_PANIC_GUARD_CHILD";

fn in_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// Runs this test binary's `test` alone in a child process with the hook.
fn run_in_child(test: &str) -> ExitStatus {
    Command::new(std::env::current_exe().expect("test binary path"))
        .args([test, "--exact", "--test-threads=1", "--nocapture"])
        .env(CHILD, "1")
        .status()
        .expect("run the child test process")
}

/// The exit code the test harness gives a test that panicked normally.
const TEST_FAILED: i32 = 101;

#[test]
fn a_panic_outside_a_guarded_open_aborts_the_process() {
    if in_child() {
        install_abort_outside_guards();
        panic!("a bug outside any Parquet open");
    }
    let status = run_in_child("a_panic_outside_a_guarded_open_aborts_the_process");
    assert!(!status.success(), "the child must not survive: {status:?}");
    assert_ne!(
        status.code(),
        Some(TEST_FAILED),
        "the child unwound like a normal test failure instead of aborting: {status:?}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        const SIGABRT: i32 = 6;
        assert_eq!(status.signal(), Some(SIGABRT), "{status:?}");
    }
}

#[test]
fn a_panic_inside_a_parquet_open_is_an_error_the_process_survives() {
    if in_child() {
        install_abort_outside_guards();
        let case = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/corpus/case-65-parquet-dictionary-page-missing.parquet");
        let error = glyde_core::ingest::inspect(&case).expect_err("a damaged file");
        assert!(error.to_string().contains("its data is damaged"), "{error}");
        return;
    }
    let status = run_in_child("a_panic_inside_a_parquet_open_is_an_error_the_process_survives");
    assert!(
        status.success(),
        "the child must survive the decoder panic: {status:?}"
    );
}
