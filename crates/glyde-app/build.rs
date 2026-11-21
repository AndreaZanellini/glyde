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

//! Issue #232: embeds `packaging/windows/glyde.ico` in `glyde.exe`, so
//! Explorer, the Start menu and the installer's shortcuts show the Glyde icon.
//! Does nothing when building for macOS or Linux.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let ico = manifest_dir.join("../../packaging/windows/glyde.ico");
    println!("cargo:rerun-if-changed=../../packaging/windows/glyde.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // The resource script names the icon by absolute path, so the resource
    // compiler finds it whatever directory it runs from.
    let ico = ico
        .canonicalize()
        .expect("packaging/windows/glyde.ico exists");
    let ico = ico.to_string_lossy();
    // Canonical Windows paths start with `\\?\`, which rc.exe does not read.
    let ico = ico.strip_prefix(r"\\?\").unwrap_or(&ico).replace('\\', "/");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo"));
    let rc = out_dir.join("glyde.rc");
    // ID 1: the lowest icon ID is the one Windows shows for the exe.
    std::fs::write(&rc, format!("1 ICON \"{ico}\"\n")).expect("OUT_DIR is writable");

    // Only the `glyde` binary: tests and benches do not need an icon.
    // Required, not optional: a Windows build without its icon must fail
    // here rather than ship an exe that looks like nothing.
    embed_resource::compile_for(&rc, ["glyde"], embed_resource::NONE)
        .manifest_required()
        .unwrap_or_else(|err| panic!("could not embed the Windows icon: {err}"));
}
