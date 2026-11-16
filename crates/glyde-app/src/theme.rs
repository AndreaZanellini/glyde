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

//! Glyde's visual identity (issue #229): the light and dark palettes, the
//! two embedded typefaces and the widget styling the approved redesign
//! mockup specifies (`docs/design/README.md` §Design tokens, the source of
//! every value here).
//!
//! There is one light and one dark theme and no theme setting (PRODUCT
//! non-goal): [`install`] registers both and lets the OS choose (SPEC §6).

use std::sync::Arc;

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, Style, TextStyle,
    Theme, ThemePreference, Visuals,
};

/// One theme's design tokens, copied from the mockup's `:root` block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Panels and toolbars.
    pub window: Color32,
    /// Plot areas and cards.
    pub surface: Color32,
    /// Recessed fields (text inputs, slider rails).
    pub sunk: Color32,
    /// Hairlines and frames.
    pub line: Color32,
    /// The plot grid.
    pub grid: Color32,
    /// Primary text, cursors, marker ticks.
    pub ink: Color32,
    /// Secondary text, axis labels.
    pub ink_2: Color32,
    /// Tertiary text, the "several states here" glyph.
    pub ink_3: Color32,
    /// UI state only (pressed, focus, selection), never data.
    pub accent: Color32,
    /// Text on an accent fill.
    pub accent_ink: Color32,
    /// Warning text.
    pub warn_ink: Color32,
    /// Warning glyphs and marks.
    pub warn_mark: Color32,
    /// Error text.
    pub error_ink: Color32,
    /// The numeric channels, in order.
    pub series: [Color32; 8],
    /// Text-state bands, with [`Palette::ink`] labels on top: the 22 px
    /// bands of the track layout (#230).
    pub state_tints: [Color32; 6],
    /// A `true` stretch of an on/off lane (#230).
    pub bool_on: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// The light theme.
pub const LIGHT: Palette = Palette {
    window: hex(0xf4f5f7),
    surface: hex(0xffffff),
    sunk: hex(0xeceef1),
    line: hex(0xd9dde3),
    grid: hex(0xeef0f3),
    ink: hex(0x14171c),
    ink_2: hex(0x4a515c),
    ink_3: hex(0x7b828d),
    accent: hex(0x0d6e7b),
    accent_ink: hex(0xffffff),
    warn_ink: hex(0x7a4a00),
    warn_mark: hex(0xe09a00),
    // Not a mockup token: the mockup shows no error text. Series 8's red,
    // darkened to 4.5:1 on `window` so it reads as text.
    error_ink: hex(0xb3261e),
    series: [
        hex(0x2a78d6),
        hex(0xeb6834),
        hex(0x1baf7a),
        hex(0xeda100),
        hex(0xe87ba4),
        hex(0x008300),
        hex(0x6250d6),
        hex(0xe34948),
    ],
    state_tints: [
        hex(0xdbe5f0),
        hex(0xf3e3c6),
        hex(0xd5ebdd),
        hex(0xe7dcf0),
        hex(0xf4d8d4),
        hex(0xd6e9ec),
    ],
    bool_on: hex(0x4a515c),
};

/// The dark theme.
pub const DARK: Palette = Palette {
    window: hex(0x14171b),
    surface: hex(0x1a1e24),
    sunk: hex(0x111418),
    line: hex(0x2a3038),
    grid: hex(0x222830),
    ink: hex(0xe8eaee),
    ink_2: hex(0xb2b8c2),
    ink_3: hex(0x7d8591),
    accent: hex(0x4cb5c3),
    accent_ink: hex(0x08292e),
    warn_ink: hex(0xf6d68a),
    warn_mark: hex(0xfab219),
    // Not a mockup token (see `LIGHT`): series 8's red, lightened.
    error_ink: hex(0xf28b82),
    series: [
        hex(0x3987e5),
        hex(0xd95926),
        hex(0x199e70),
        hex(0xc98500),
        hex(0xd55181),
        hex(0x0ca30c),
        hex(0x9085e9),
        hex(0xe66767),
    ],
    state_tints: [
        hex(0x263345),
        hex(0x45371f),
        hex(0x213b2c),
        hex(0x372c45),
        hex(0x472a27),
        hex(0x1f3d44),
    ],
    bool_on: hex(0xb2b8c2),
};

/// The palette of the theme `visuals` belong to.
pub fn palette(visuals: &Visuals) -> &'static Palette {
    if visuals.dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// The family of the semibold UI face, used for headings.
pub const SEMIBOLD: &str = "Inter SemiBold";

const INTER_REGULAR: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
const INTER_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.ttf");
const JETBRAINS_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

/// Inter for the interface, JetBrains Mono for numbers and other monospaced
/// text. egui's own fonts stay behind them as fallbacks, so a glyph outside
/// the embedded Latin subset (an emoji, a CJK column name) still draws.
pub fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let mut add = |name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add("Inter", INTER_REGULAR);
    add(SEMIBOLD, INTER_SEMIBOLD);
    add("JetBrains Mono", JETBRAINS_MONO);

    let fallbacks = |family: FontFamily, fonts: &FontDefinitions| {
        fonts.families.get(&family).cloned().unwrap_or_default()
    };
    let proportional = fallbacks(FontFamily::Proportional, &fonts);
    let monospace = fallbacks(FontFamily::Monospace, &fonts);
    let with_first = |first: &str, rest: &[String]| {
        std::iter::once(first.to_owned())
            .chain(rest.iter().cloned())
            .collect::<Vec<_>>()
    };
    fonts
        .families
        .insert(FontFamily::Proportional, with_first("Inter", &proportional));
    fonts.families.insert(
        FontFamily::Name(SEMIBOLD.into()),
        with_first(SEMIBOLD, &proportional),
    );
    fonts.families.insert(
        FontFamily::Monospace,
        with_first("JetBrains Mono", &monospace),
    );
    fonts
}

/// The widget styling of one theme, built on egui's own light or dark
/// visuals so every field the mockup says nothing about keeps a sane value.
pub fn visuals(theme: Theme) -> Visuals {
    let (p, mut v) = match theme {
        Theme::Light => (&LIGHT, Visuals::light()),
        Theme::Dark => (&DARK, Visuals::dark()),
    };
    let radius = CornerRadius::same(6);
    let hairline = Stroke::new(1.0_f32, p.line);

    v.override_text_color = None;
    v.panel_fill = p.window;
    v.window_fill = p.surface;
    v.window_stroke = hairline;
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = radius;
    v.extreme_bg_color = p.surface;
    v.faint_bg_color = p.sunk;
    v.code_bg_color = p.sunk;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn_ink;
    v.error_fg_color = p.error_ink;
    v.selection.bg_fill = p
        .accent
        .gamma_multiply(if v.dark_mode { 0.35 } else { 0.22 });
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);

    let w = &mut v.widgets;
    // Labels, separators, frames.
    w.noninteractive.bg_fill = p.window;
    w.noninteractive.weak_bg_fill = p.window;
    w.noninteractive.bg_stroke = hairline;
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.ink_2);
    w.noninteractive.corner_radius = radius;
    // Buttons at rest: a surface tile with a hairline.
    w.inactive.bg_fill = p.sunk;
    w.inactive.weak_bg_fill = p.surface;
    w.inactive.bg_stroke = hairline;
    w.inactive.fg_stroke = Stroke::new(1.0_f32, p.ink);
    w.inactive.corner_radius = radius;
    // Hovered: the frame darkens toward the ink.
    w.hovered.bg_fill = p.sunk;
    w.hovered.weak_bg_fill = p.sunk;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, p.ink_3);
    w.hovered.fg_stroke = Stroke::new(1.5_f32, p.ink);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;
    // Pressed: an accent frame. The fill stays neutral because egui also
    // takes `strong` text's color from here (`Visuals::strong_text_color`),
    // and that text sits on the panel. Toggles that are on use `selection`.
    w.active.bg_fill = p.line;
    w.active.weak_bg_fill = p.line;
    w.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    w.active.fg_stroke = Stroke::new(1.5_f32, p.ink);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;
    // An open menu or combo box.
    w.open.bg_fill = p.sunk;
    w.open.weak_bg_fill = p.sunk;
    w.open.bg_stroke = Stroke::new(1.0_f32, p.ink_3);
    w.open.fg_stroke = Stroke::new(1.0_f32, p.ink);
    w.open.corner_radius = radius;
    v
}

/// The full style of one theme: [`visuals`], the mockup's type scale
/// (13 px UI, 12 px small, 11.5 px figures, 15 px headings) and spacing.
pub fn style(theme: Theme) -> Style {
    let mut style = Style {
        visuals: visuals(theme),
        ..Style::default()
    };
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(12.0)),
        (
            TextStyle::Heading,
            FontId::new(15.0, FontFamily::Name(SEMIBOLD.into())),
        ),
        (TextStyle::Monospace, FontId::monospace(11.5)),
    ]
    .into();
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.interact_size.y = 24.0;
    style
}

/// Installs the visual identity on `ctx`: both themes, the fonts, and the
/// OS choosing between light and dark (SPEC §6).
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_style_of(Theme::Light, style(Theme::Light));
    ctx.set_style_of(Theme::Dark, style(Theme::Dark));
    ctx.set_theme(ThemePreference::System);
    tracing::info!("visual identity installed; light or dark follows the OS");
}

/// The color of the `index`-th numeric channel in the theme `visuals`
/// belong to: the eight palette colors in order, then (only past eight
/// channels) the same eight lighter, then darker, never a near-duplicate
/// at the same lightness.
pub fn series_color(visuals: &Visuals, index: usize) -> Color32 {
    let series = &palette(visuals).series;
    let base = series[index % series.len()];
    match index / series.len() {
        0 => base,
        cycle if cycle % 2 == 1 => base.lerp_to_gamma(Color32::WHITE, 0.45),
        _ => base.lerp_to_gamma(Color32::BLACK, 0.45),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark() -> Visuals {
        Visuals::dark()
    }
    fn light() -> Visuals {
        Visuals::light()
    }

    /// CIE L*a*b* of an sRGB color (D65).
    fn lab(color: Color32) -> [f64; 3] {
        let linear = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let (r, g, b) = (linear(color.r()), linear(color.g()), linear(color.b()));
        let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
        let f = |t: f64| {
            if t > 0.008856 {
                t.cbrt()
            } else {
                7.787 * t + 16.0 / 116.0
            }
        };
        let (fx, fy, fz) = (f(x), f(y), f(z));
        [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
    }

    fn delta_e(a: Color32, b: Color32) -> f64 {
        let (a, b) = (lab(a), lab(b));
        (0..3).map(|k| (a[k] - b[k]).powi(2)).sum::<f64>().sqrt()
    }

    fn min_delta_e(visuals: &Visuals, n: usize) -> f64 {
        let colors: Vec<_> = (0..n).map(|i| series_color(visuals, i)).collect();
        let mut min = f64::INFINITY;
        for i in 0..n {
            for j in (i + 1)..n {
                min = min.min(delta_e(colors[i], colors[j]));
            }
        }
        min
    }

    /// WCAG contrast ratio of two colors.
    fn contrast(a: Color32, b: Color32) -> f64 {
        let luminance = |c: Color32| {
            let l = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * l(c.r()) + 0.7152 * l(c.g()) + 0.0722 * l(c.b())
        };
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    // The tokens are the approved mockup's, verbatim (docs/design/README.md).
    #[test]
    fn the_palettes_match_the_approved_design_tokens() {
        assert_eq!(LIGHT.window, Color32::from_rgb(0xf4, 0xf5, 0xf7));
        assert_eq!(DARK.window, Color32::from_rgb(0x14, 0x17, 0x1b));
        assert_eq!(LIGHT.surface, Color32::WHITE);
        assert_eq!(DARK.surface, Color32::from_rgb(0x1a, 0x1e, 0x24));
        assert_eq!(LIGHT.accent, Color32::from_rgb(0x0d, 0x6e, 0x7b));
        assert_eq!(DARK.accent, Color32::from_rgb(0x4c, 0xb5, 0xc3));
        assert_eq!(LIGHT.series[0], Color32::from_rgb(0x2a, 0x78, 0xd6));
        assert_eq!(LIGHT.series[7], Color32::from_rgb(0xe3, 0x49, 0x48));
        assert_eq!(DARK.series[0], Color32::from_rgb(0x39, 0x87, 0xe5));
        assert_eq!(DARK.series[7], Color32::from_rgb(0xe6, 0x67, 0x67));
        assert_eq!(LIGHT.state_tints[0], Color32::from_rgb(0xdb, 0xe5, 0xf0));
        assert_eq!(DARK.state_tints[5], Color32::from_rgb(0x1f, 0x3d, 0x44));
    }

    #[test]
    fn each_theme_draws_with_its_own_palette() {
        assert_eq!(palette(&light()), &LIGHT);
        assert_eq!(palette(&dark()), &DARK);
        assert_eq!(visuals(Theme::Light).panel_fill, LIGHT.window);
        assert_eq!(visuals(Theme::Dark).panel_fill, DARK.window);
        assert_eq!(visuals(Theme::Light).extreme_bg_color, LIGHT.surface);
        assert!(!visuals(Theme::Light).dark_mode);
        assert!(visuals(Theme::Dark).dark_mode);
        for i in 0..8 {
            assert_eq!(series_color(&light(), i), LIGHT.series[i]);
            assert_eq!(series_color(&dark(), i), DARK.series[i]);
        }
    }

    // Text must stay readable in both themes: WCAG AA (4.5:1) for body and
    // secondary text and the warning and error text on the panel color.
    #[test]
    fn text_tokens_are_readable_on_the_panels() {
        for p in [&LIGHT, &DARK] {
            for (name, ink) in [
                ("ink", p.ink),
                ("ink_2", p.ink_2),
                ("warn_ink", p.warn_ink),
                ("error_ink", p.error_ink),
            ] {
                let ratio = contrast(ink, p.window);
                assert!(ratio >= 4.5, "{name} on window: {ratio:.2}");
            }
            assert!(contrast(p.accent_ink, p.accent) >= 4.5);
            assert!(contrast(p.accent, p.window) >= 4.5, "accent text on window");
            for tint in p.state_tints {
                assert!(contrast(p.ink, tint) >= 4.5, "label on {tint:?}");
            }
        }
    }

    // `ui.strong` (the PSD panel's title) and pressed buttons take their
    // text color from the active widget style: it must read on the panel.
    #[test]
    fn strong_text_is_readable_in_both_themes() {
        for (theme, p) in [(Theme::Light, &LIGHT), (Theme::Dark, &DARK)] {
            let v = visuals(theme);
            let ratio = contrast(v.strong_text_color(), p.window);
            assert!(ratio >= 7.0, "{theme:?}: strong text {ratio:.2}");
            let selected = contrast(v.selection.stroke.color, p.window);
            assert!(selected >= 4.5, "{theme:?}: selected text {selected:.2}");
        }
    }

    // Issue #55: one color per series index, whatever draws it.
    #[test]
    fn series_color_is_stable_and_differs_across_indices() {
        for v in [light(), dark()] {
            assert_eq!(series_color(&v, 3), series_color(&v, 3));
            let colors: Vec<_> = (0..24).map(|i| series_color(&v, i)).collect();
            for i in 0..colors.len() {
                for j in (i + 1)..colors.len() {
                    assert_ne!(colors[i], colors[j], "series {i} and {j}");
                }
            }
        }
    }

    // Issue #199's check, on the approved palette. It was chosen for
    // color-blind separation (docs/design/README.md), which costs some
    // normal-vision distance against category10's ≥ 25: the first 8 stay
    // ≥ 20 apart, and the lighter and darker repeats past 8 ≥ 13.
    #[test]
    fn the_series_colors_are_perceptually_distinct() {
        for (name, v) in [("light", light()), ("dark", dark())] {
            assert!(
                min_delta_e(&v, 8) >= 20.0,
                "{name} first 8: {}",
                min_delta_e(&v, 8)
            );
            assert!(
                min_delta_e(&v, 24) >= 13.0,
                "{name} first 24: {}",
                min_delta_e(&v, 24)
            );
            // Issue #199's pair: channel 3 and its first repeat.
            let pair = delta_e(series_color(&v, 3), series_color(&v, 11));
            assert!(pair >= 20.0, "{name} 3 vs 11: {pair}");
        }
    }

    #[test]
    fn install_follows_the_os_and_registers_both_themes() {
        let ctx = egui::Context::default();
        install(&ctx);
        assert_eq!(ctx.options(|o| o.theme_preference), ThemePreference::System);
        assert_eq!(ctx.style_of(Theme::Light).visuals.panel_fill, LIGHT.window);
        assert_eq!(ctx.style_of(Theme::Dark).visuals.panel_fill, DARK.window);
        assert_eq!(
            ctx.style_of(Theme::Light).text_styles[&TextStyle::Body],
            FontId::proportional(13.0)
        );
    }

    // The embedded faces are first in their families, egui's own fonts stay
    // as fallbacks, and the glyphs readouts need are in the subset.
    #[test]
    fn fonts_put_inter_and_jetbrains_mono_first_with_fallbacks() {
        let defs = fonts();
        let family = |f: FontFamily| defs.families[&f].clone();
        assert_eq!(family(FontFamily::Proportional)[0], "Inter");
        assert_eq!(family(FontFamily::Monospace)[0], "JetBrains Mono");
        assert_eq!(family(FontFamily::Name(SEMIBOLD.into()))[0], SEMIBOLD);
        assert!(family(FontFamily::Proportional).len() > 1, "fallbacks kept");

        // Every non-ASCII character the app's own text uses (units, deltas,
        // arrows, the gap glyph): whatever egui's stock fonts drew before,
        // the new families still draw.
        const USED: &str = "§—·…Δ±²³µ°⁵∞–⫽×é≤→÷−≥≈Ω⁻←↑↓▾▸✓⌘";
        // What readouts and units need from the embedded faces themselves
        // (JetBrains Mono has no superscript minus: the fallback draws it).
        const EMBEDDED: &str = "AZaz09.,:;-+±×÷−≤≥≈∞µΔΩ°²³⁵→←↑↓…–—·§é";
        let only = |name: &str| FontFamily::Name(format!("only {name}").into());
        let mut defs = defs;
        for name in ["Inter", "JetBrains Mono"] {
            defs.families.insert(only(name), vec![name.to_owned()]);
        }
        let stock = egui::Context::default();
        let ours = egui::Context::default();
        ours.set_fonts(defs);
        for ctx in [&stock, &ours] {
            let _ = ctx.run(egui::RawInput::default(), |_| {});
        }
        let ui_ids = [FontId::proportional(13.0), FontId::monospace(11.5)];
        for c in USED.chars() {
            for id in &ui_ids {
                if stock.fonts(|f| f.has_glyph(id, c)) {
                    assert!(
                        ours.fonts(|f| f.has_glyph(id, c)),
                        "{c:?} lost in {:?}",
                        id.family
                    );
                }
            }
        }
        for c in EMBEDDED.chars() {
            for name in ["Inter", "JetBrains Mono"] {
                let id = FontId::new(13.0, only(name));
                assert!(ours.fonts(|f| f.has_glyph(&id, c)), "{c:?} not in {name}");
            }
        }
    }

    // docs/design/README.md: the embedded faces add under 1 MB.
    #[test]
    fn embedded_fonts_stay_under_one_megabyte() {
        let total = INTER_REGULAR.len() + INTER_SEMIBOLD.len() + JETBRAINS_MONO.len();
        assert!(total < 1_000_000, "{total} bytes");
    }
}
