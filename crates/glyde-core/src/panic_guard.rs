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

//! Turning a panic inside a third-party decoder into an error (issue #233).
//!
//! The `parquet`/`arrow` readers `panic!` on some corrupted files (a data
//! page that claims a dictionary the column chunk does not have, corpus case
//! 65). Glyde cannot pre-validate every page without decoding it, so a
//! Parquet open runs inside [`catch`]: a panic there becomes an error that
//! names the file, and the app stays open.
//!
//! **Everywhere else a panic still ends the process**, exactly as
//! `panic = "abort"` did before. The release profile now unwinds so that
//! [`catch`] can work, and `glyde-app` installs
//! [`install_abort_outside_guards`] at startup: a panic hook that prints the
//! usual message and then aborts, unless a guarded section is running.
//!
//! The count of guarded sections is process-wide rather than per thread,
//! because the decoding runs on `rayon` workers, and the hook runs on the
//! thread that panicked, not on the thread that called [`catch`]. The cost of
//! that choice: an unrelated panic on another thread *during* a guarded open
//! unwinds that thread instead of aborting the process. A panic on the UI
//! thread still ends the process (an unwinding main thread exits). The
//! window is as long as one Parquet open.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};

static GUARDED: AtomicUsize = AtomicUsize::new(0);

/// True while at least one [`catch`] section is running anywhere in the
/// process.
pub fn is_guarded() -> bool {
    GUARDED.load(Ordering::SeqCst) > 0
}

/// Runs `f`, returning the panic message as `Err` if it panicked.
pub(crate) fn catch<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    struct Section;
    impl Drop for Section {
        fn drop(&mut self) {
            GUARDED.fetch_sub(1, Ordering::SeqCst);
        }
    }
    GUARDED.fetch_add(1, Ordering::SeqCst);
    let _section = Section;
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| message(payload.as_ref()))
}

fn message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_string())
}

/// Installs the process-wide panic hook described in the module docs: the
/// default report, then `abort` unless a guarded section is running. Call
/// once, at startup, from the app (never from tests, which rely on unwinding
/// to report a failed assertion).
pub fn install_abort_outside_guards() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        if !is_guarded() {
            std::process::abort();
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_inside_catch_is_an_error_with_its_message() {
        let caught = catch(|| -> u32 { panic!("decoder exploded at page {}", 3) });
        assert_eq!(caught, Err("decoder exploded at page 3".to_string()));
        assert_eq!(catch(|| 7), Ok(7));
    }

    #[test]
    fn a_panic_on_a_worker_thread_inside_catch_is_caught_too() {
        let caught = catch(|| {
            rayon::join(|| 1, || -> i32 { panic!("worker panicked") });
        });
        assert_eq!(caught, Err("worker panicked".to_string()));
    }
}
