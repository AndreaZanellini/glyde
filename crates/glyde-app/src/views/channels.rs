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

//! The time view's channel picker: which numeric channels are plotted.
//!
//! It replaces `egui_plot`'s built-in legend, which had two problems on a
//! file with many channels. Its box was not scrollable, so channels past the
//! plot's edge could not be reached at all. And it could only hide a series
//! *after* the series had been decimated and handed to the plot, so a hidden
//! channel still cost its full per-frame decimation — on a spilled file, a
//! read of its on-disk pyramid every frame. Here visibility is known before
//! decimation, so [`crate::views::time`] skips hidden channels entirely.
//!
//! The picker keeps the legend's look — one row per channel, a dot in the
//! channel's plot color that is filled while the channel is shown — in a
//! fixed-height box of [`VISIBLE_ROWS`] rows that scrolls through the rest.
//! Only the rows on screen are laid out, so its cost does not grow with the
//! channel count.

use egui::epaint::CircleShape;
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, Rect, Sense, Shape, TextStyle, Ui};

/// How many channel rows the picker shows before it scrolls.
pub const VISIBLE_ROWS: usize = 4;

/// The picker's fixed width, in points; longer names are truncated with an
/// ellipsis and shown in full on hover.
const WIDTH: f32 = 200.0;

/// Which of the time view's channels are shown. A channel is identified by
/// its position among the plotted columns (the same index that picks its
/// color, `views::time::series_color`), so a channel keeps its color whatever
/// else is hidden.
///
/// Every channel starts visible. The state survives re-renders of the same
/// channel list — including the switch from a still-loading preview to the
/// completed file — and resets to all-visible when the list changes (another
/// file, or a correction that re-splits the columns): an index-based choice
/// made for one list means nothing for another.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelVisibility {
    names: Vec<String>,
    hidden: Vec<bool>,
}

impl ChannelVisibility {
    /// Adopts `names` as the current channel list, keeping the current
    /// choice if it is the same list and showing every channel otherwise.
    pub fn sync<'a>(&mut self, names: impl ExactSizeIterator<Item = &'a str> + Clone) {
        let same = names.len() == self.names.len()
            && names.clone().zip(&self.names).all(|(new, old)| new == old);
        if same {
            return;
        }
        if self.hidden.iter().any(|&hidden| hidden) {
            tracing::info!(
                channels = names.len(),
                "the plotted channel list changed; showing every channel again"
            );
        }
        self.names = names.map(str::to_owned).collect();
        self.hidden = vec![false; self.names.len()];
    }

    /// The number of channels in the current list.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the current list has no channels.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Whether channel `index` is shown. A channel outside the current list
    /// is shown: hiding is only ever something the user asked for.
    pub fn is_visible(&self, index: usize) -> bool {
        !self.hidden.get(index).copied().unwrap_or(false)
    }

    /// How many channels are shown.
    pub fn visible_count(&self) -> usize {
        self.hidden.iter().filter(|&&hidden| !hidden).count()
    }

    /// Shows channel `index` if hidden, hides it if shown.
    pub fn toggle(&mut self, index: usize) {
        if let Some(hidden) = self.hidden.get_mut(index) {
            *hidden = !*hidden;
        }
    }

    /// Shows only channel `index` — or, if it already is the only one shown,
    /// every channel again (the same Alt+click behavior `egui_plot`'s legend
    /// had).
    pub fn solo(&mut self, index: usize) {
        if index >= self.hidden.len() {
            return;
        }
        let already_solo = self
            .hidden
            .iter()
            .enumerate()
            .all(|(other, &hidden)| hidden == (other != index));
        for (other, hidden) in self.hidden.iter_mut().enumerate() {
            *hidden = !already_solo && other != index;
        }
    }

    /// Shows every channel.
    pub fn show_all(&mut self) {
        self.hidden.fill(false);
    }

    /// Hides every channel.
    pub fn hide_all(&mut self) {
        self.hidden.fill(true);
    }
}

/// Draws the picker for `visibility`'s channels inside the top-right corner
/// of `plot_rect`, colored by `color`, and applies any click to
/// `visibility`. It floats above the plot (its own layer), so the plot never
/// pans or zooms from a click, drag or scroll meant for the picker.
pub fn show(
    ui: &Ui,
    plot_rect: Rect,
    visibility: &mut ChannelVisibility,
    color: impl Fn(usize) -> Color32,
) {
    if visibility.is_empty() {
        return;
    }
    let before = visibility.clone();
    let margin = 8.0;
    egui::Area::new(ui.id().with("time_view_channel_picker"))
        .order(egui::Order::Middle)
        .pivot(Align2::RIGHT_TOP)
        .fixed_pos(plot_rect.right_top() + egui::vec2(-margin, margin))
        .constrain_to(plot_rect)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(WIDTH);
                ui.set_max_width(WIDTH);
                header(ui, visibility);
                rows(ui, visibility, color);
            });
        });
    if *visibility != before {
        // The plot was already decimated for this frame with the old
        // choice; draw the new one right away rather than on the next input.
        ui.ctx().request_repaint();
    }
}

/// "<shown> of <total> shown", then the All and None buttons.
fn header(ui: &mut Ui, visibility: &mut ChannelVisibility) {
    ui.horizontal(|ui| {
        ui.label(format!(
            "{} of {} shown",
            visibility.visible_count(),
            visibility.len()
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("None")
                .on_hover_text("Hide every channel")
                .clicked()
            {
                visibility.hide_all();
            }
            if ui
                .small_button("All")
                .on_hover_text("Show every channel")
                .clicked()
            {
                visibility.show_all();
            }
        });
    });
}

/// The channel rows, [`VISIBLE_ROWS`] at a time; only the rows on screen are
/// laid out.
fn rows(ui: &mut Ui, visibility: &mut ChannelVisibility, color: impl Fn(usize) -> Color32) {
    let font_id = TextStyle::Body.resolve(ui.style());
    let row_height = ui.fonts(|fonts| fonts.row_height(&font_id)) + 2.0;
    let spacing = ui.spacing().item_spacing.y;
    let shown_rows = visibility.len().min(VISIBLE_ROWS) as f32;
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .max_height(shown_rows * (row_height + spacing))
        .auto_shrink([false, true])
        .show_rows(ui, row_height, visibility.len(), |ui, range| {
            for index in range {
                let response = row(
                    ui,
                    index,
                    &visibility.names[index],
                    color(index),
                    visibility.is_visible(index),
                    row_height,
                );
                if response.clicked() {
                    clicked = Some((index, ui.input(|input| input.modifiers.alt)));
                }
            }
        });
    match clicked {
        Some((index, true)) => visibility.solo(index),
        Some((index, false)) => visibility.toggle(index),
        None => {}
    }
}

/// The id of channel `index`'s row (there is only ever one picker).
pub(crate) fn row_id(index: usize) -> egui::Id {
    egui::Id::new(("glyde_channel_picker_row", index))
}

/// One channel: a dot in its plot color (filled while it is shown) and its
/// name, the whole row clickable.
fn row(
    ui: &mut Ui,
    index: usize,
    name: &str,
    color: Color32,
    visible: bool,
    height: f32,
) -> egui::Response {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::hover());
    let response = ui.interact(rect, row_id(index), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), visible, name)
    });
    let response = response.on_hover_text(format!(
        "{name}\nClick: show/hide · Alt/Option+click: show only this one"
    ));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.style().interact(&response);
    let painter = ui.painter();

    let dot_size = height - 4.0;
    let dot_center = egui::pos2(rect.left() + dot_size / 2.0, rect.center().y);
    painter.add(CircleShape {
        center: dot_center,
        radius: dot_size / 2.0,
        fill: visuals.bg_fill,
        stroke: visuals.bg_stroke,
    });
    if visible {
        painter.add(Shape::circle_filled(dot_center, dot_size * 0.4, color));
    }

    let text_left = rect.left() + dot_size + dot_size / 5.0;
    let mut job = LayoutJob::simple_singleline(
        name.to_owned(),
        TextStyle::Body.resolve(ui.style()),
        visuals.text_color(),
    );
    job.wrap = TextWrapping::truncate_at_width(rect.right() - text_left);
    let galley = ui.fonts(|fonts| fonts.layout_job(job));
    painter.galley(
        egui::pos2(text_left, rect.center().y - galley.size().y / 2.0),
        galley,
        visuals.text_color(),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visibility(names: &[&str]) -> ChannelVisibility {
        let mut visibility = ChannelVisibility::default();
        visibility.sync(names.iter().copied());
        visibility
    }

    fn shown(visibility: &ChannelVisibility) -> Vec<bool> {
        (0..visibility.len())
            .map(|index| visibility.is_visible(index))
            .collect()
    }

    #[test]
    fn every_channel_starts_visible() {
        let visibility = visibility(&["a", "b", "c"]);
        assert_eq!(shown(&visibility), [true, true, true]);
        assert_eq!(visibility.visible_count(), 3);
    }

    #[test]
    fn toggle_flips_one_channel() {
        let mut visibility = visibility(&["a", "b", "c"]);
        visibility.toggle(1);
        assert_eq!(shown(&visibility), [true, false, true]);
        visibility.toggle(1);
        assert_eq!(shown(&visibility), [true, true, true]);
    }

    #[test]
    fn solo_shows_one_channel_then_all_again() {
        let mut visibility = visibility(&["a", "b", "c"]);
        visibility.solo(2);
        assert_eq!(shown(&visibility), [false, false, true]);
        visibility.solo(2);
        assert_eq!(shown(&visibility), [true, true, true]);
    }

    #[test]
    fn solo_on_another_channel_moves_the_focus() {
        let mut visibility = visibility(&["a", "b", "c"]);
        visibility.solo(0);
        visibility.solo(1);
        assert_eq!(shown(&visibility), [false, true, false]);
    }

    #[test]
    fn solo_on_a_hidden_channel_shows_only_it() {
        let mut visibility = visibility(&["a", "b", "c"]);
        visibility.hide_all();
        visibility.solo(0);
        assert_eq!(shown(&visibility), [true, false, false]);
    }

    #[test]
    fn all_and_none() {
        let mut visibility = visibility(&["a", "b", "c"]);
        visibility.hide_all();
        assert_eq!(visibility.visible_count(), 0);
        visibility.show_all();
        assert_eq!(visibility.visible_count(), 3);
    }

    #[test]
    fn the_choice_survives_the_same_list_and_resets_on_a_new_one() {
        let mut visibility = visibility(&["a", "b", "c"]);
        visibility.toggle(0);
        visibility.sync(["a", "b", "c"].into_iter());
        assert_eq!(shown(&visibility), [false, true, true]);

        visibility.sync(["a", "x", "c"].into_iter());
        assert_eq!(shown(&visibility), [true, true, true]);

        visibility.toggle(0);
        visibility.sync(["a", "x"].into_iter());
        assert_eq!(shown(&visibility), [true, true]);
    }

    /// Draws the picker for `visibility` over a fixed plot rect, one frame,
    /// with `events` as that frame's input.
    fn picker_frame(
        ctx: &egui::Context,
        visibility: &mut ChannelVisibility,
        events: Vec<egui::Event>,
        modifiers: egui::Modifiers,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            modifiers,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let plot_rect = ui.max_rect();
                show(ui, plot_rect, visibility, |_| Color32::RED);
            });
        });
    }

    /// Clicks the center of channel `index`'s row, as laid out by the
    /// previous frame.
    fn click_row(
        ctx: &egui::Context,
        visibility: &mut ChannelVisibility,
        index: usize,
        modifiers: egui::Modifiers,
    ) {
        let rect = ctx
            .read_response(row_id(index))
            .expect("the row must have been laid out")
            .rect;
        let at = rect.center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        };
        // Hover, press and release on separate frames, as a real click is.
        for events in [
            vec![egui::Event::PointerMoved(at)],
            vec![button(true)],
            vec![button(false)],
        ] {
            picker_frame(ctx, visibility, events, modifiers);
        }
    }

    #[test]
    fn clicking_a_row_hides_its_channel_and_alt_click_solos_it() {
        let ctx = egui::Context::default();
        let mut visibility = visibility(&["a", "b", "c"]);
        // A new area sizes itself on its first frame and is placed on the
        // second; the third reports the placed rows.
        for _ in 0..3 {
            picker_frame(&ctx, &mut visibility, Vec::new(), egui::Modifiers::NONE);
        }

        click_row(&ctx, &mut visibility, 1, egui::Modifiers::NONE);
        assert_eq!(shown(&visibility), [true, false, true]);

        click_row(&ctx, &mut visibility, 2, egui::Modifiers::ALT);
        assert_eq!(shown(&visibility), [false, false, true]);
    }

    #[test]
    fn only_the_rows_on_screen_are_laid_out() {
        let ctx = egui::Context::default();
        let names: Vec<String> = (0..1000).map(|i| format!("channel {i}")).collect();
        let mut visibility = ChannelVisibility::default();
        visibility.sync(names.iter().map(String::as_str));
        picker_frame(&ctx, &mut visibility, Vec::new(), egui::Modifiers::NONE);
        picker_frame(&ctx, &mut visibility, Vec::new(), egui::Modifiers::NONE);

        let laid_out = (0..names.len())
            .filter(|&index| ctx.read_response(row_id(index)).is_some())
            .count();
        assert!(
            (VISIBLE_ROWS..=VISIBLE_ROWS + 1).contains(&laid_out),
            "a scrolled box of {VISIBLE_ROWS} rows must lay out about that many, not {laid_out}"
        );
    }

    #[test]
    fn out_of_range_indices_are_visible_and_ignored() {
        let mut visibility = visibility(&["a"]);
        visibility.toggle(5);
        visibility.solo(5);
        assert!(visibility.is_visible(5));
        assert_eq!(shown(&visibility), [true]);
    }
}
