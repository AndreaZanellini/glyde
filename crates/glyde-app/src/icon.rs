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

//! The window icon (issue #232).
//!
//! The title bar, taskbar, app switcher and, on Linux, the dock read the icon
//! the window sets. It is the same 256 px raster the Linux package installs,
//! generated from `packaging/icon/glyde.svg` (see
//! `packaging/icon/build_icons.mjs`). The macOS Dock and the Windows exe file
//! read the bundle's `.icns` and the exe's embedded `.ico` instead.

/// The 256 px icon, PNG-encoded, embedded in the binary.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../../packaging/icon/png/glyde-256.png");

/// The decoded window icon, ready for `ViewportBuilder::with_icon`.
///
/// # Errors
/// Only if the embedded PNG cannot be decoded, which the tests rule out; the
/// caller then opens the window without an icon instead of failing.
pub fn window_icon() -> Result<egui::IconData, String> {
    eframe::icon_data::from_png_bytes(WINDOW_ICON_PNG).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(icon: &egui::IconData, x: u32, y: u32) -> u8 {
        icon.rgba[((y * icon.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_window_icon_decodes_to_a_256_px_square() {
        let icon = window_icon().expect("embedded icon decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

    #[test]
    fn the_window_icon_is_a_tile_with_transparent_corners() {
        let icon = window_icon().expect("embedded icon decodes");
        // Rounded tile: the corners show whatever is behind the icon...
        for (x, y) in [(0, 0), (255, 0), (0, 255), (255, 255)] {
            assert_eq!(alpha(&icon, x, y), 0, "corner ({x}, {y})");
        }
        // ...and the tile itself is opaque.
        assert_eq!(alpha(&icon, 128, 128), 255);
        assert_eq!(alpha(&icon, 128, 20), 255);
    }
}
