//! Echobind's controls, drawn to the radius and padding scale in `theme`.

use crate::{
    icons::{self, Icon},
    theme::{self, Tone, Weight},
};
use eframe::egui::{
    self, pos2,
    text::{LayoutJob, TextFormat},
    vec2, Align, Color32, CornerRadius, FontId, Galley, Id, Key, Layout, Margin, Rect, Response,
    RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, Vec2, WidgetInfo, WidgetType,
};
use std::{ops::RangeInclusive, sync::Arc};

fn galley(ui: &Ui, text: impl Into<String>, font: FontId, color: Color32) -> Arc<Galley> {
    ui.painter().layout_no_wrap(text.into(), font, color)
}

fn truncated_galley(
    ui: &Ui,
    text: impl Into<String>,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.into(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width.max(0.0));
    ui.painter().layout_job(job)
}

fn paint_centered_galley(ui: &Ui, galley: Arc<Galley>, left: f32, center_y: f32) {
    let top = center_y - galley.size().y / 2.0;
    ui.painter().galley(pos2(left, top), galley, theme::TEXT);
}

pub fn text(text: impl Into<String>, size: f32, weight: Weight, color: Color32) -> RichText {
    RichText::new(text)
        .font(theme::inter(size, weight))
        .color(color)
}

pub fn mono_text(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text)
        .font(theme::mono(size, Weight::Regular))
        .color(color)
}

pub fn title(ui: &mut Ui, title: &str) {
    ui.label(text(title, 20.0, Weight::SemiBold, theme::TEXT).extra_letter_spacing(-0.2));
}

pub fn heading(ui: &mut Ui, heading: &str) {
    ui.label(text(heading, 16.0, Weight::SemiBold, theme::TEXT));
}

pub fn muted(ui: &mut Ui, body: impl Into<String>, size: f32) {
    ui.add(egui::Label::new(text(body, size, Weight::Regular, theme::MUTED)).wrap());
}

pub fn overline(ui: &mut Ui, label: &str) {
    ui.label(
        text(label.to_uppercase(), 11.0, Weight::SemiBold, theme::MUTED).extra_letter_spacing(0.66),
    );
}

pub fn field_label(ui: &mut Ui, label: &str) {
    ui.label(text(label, 13.0, Weight::Medium, theme::TEXT));
}

/// Runs `add` with a fixed vertical gap between the widgets it adds.
pub fn stack<R>(ui: &mut Ui, gap: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = gap;
        add(ui)
    })
    .inner
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Danger,
}

pub fn button(
    ui: &mut Ui,
    kind: ButtonKind,
    icon: Option<Icon>,
    label: &str,
    width: Option<f32>,
) -> Response {
    let enabled = ui.is_enabled();
    let weight = if kind == ButtonKind::Primary {
        Weight::SemiBold
    } else {
        Weight::Medium
    };
    let label_galley = galley(ui, label, theme::inter(14.0, weight), theme::TEXT);
    let icon_width = if icon.is_some() { 16.0 + 8.0 } else { 0.0 };
    let content_width = icon_width + label_galley.size().x;
    let width = width.unwrap_or(content_width + 32.0);
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, theme::CONTROL_HEIGHT), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let (fill, stroke, foreground) = match kind {
        ButtonKind::Primary => (
            if pressed {
                theme::SIGNAL_PRESSED
            } else if hovered {
                theme::SIGNAL_HOVER
            } else {
                theme::SIGNAL
            },
            Stroke::NONE,
            theme::ON_SIGNAL,
        ),
        ButtonKind::Secondary => (
            if pressed {
                theme::PRESSED
            } else if hovered {
                theme::SELECTED
            } else {
                theme::RAISED
            },
            Stroke::new(
                1.0,
                if hovered {
                    theme::HOVER_BORDER
                } else {
                    theme::CONTROL_BORDER
                },
            ),
            theme::TEXT,
        ),
        ButtonKind::Danger => (
            theme::tint(theme::DANGER, if hovered { 0.2 } else { 0.12 }),
            Stroke::NONE,
            theme::DANGER,
        ),
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::RADIUS_CONTROL),
        fill,
        stroke,
        StrokeKind::Inside,
    );
    let left = rect.center().x - content_width / 2.0;
    if let Some(icon) = icon {
        let icon_rect =
            Rect::from_center_size(pos2(left + 8.0, rect.center().y), Vec2::splat(16.0));
        icons::paint(ui.painter(), icon_rect, icon, foreground);
    }
    let label_galley = galley(ui, label, theme::inter(14.0, weight), foreground);
    paint_centered_galley(ui, label_galley, left + icon_width, rect.center().y);
    response
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconButtonKind {
    Ghost,
    Secondary,
    Pressed,
    Danger,
}

/// A full-height but narrow ghost button, used for the toolbar's output menu.
pub fn narrow_icon_button(ui: &mut Ui, icon: Icon, label: &str) -> Response {
    let enabled = ui.is_enabled();
    let (rect, response) =
        ui.allocate_exact_size(vec2(28.0, theme::CONTROL_HEIGHT), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        if enabled && response.hovered() {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(theme::RADIUS_CONTROL),
                theme::SELECTED,
            );
        }
        let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
        icons::paint(ui.painter(), icon_rect, icon, theme::MUTED);
    }
    response.on_hover_text(label)
}

/// Mute toggle plus volume slider, with the level shown beside the label.
/// Returns true when the level or mute state changed.
pub fn volume_control(ui: &mut Ui, label: &str, percent: &mut u32, muted: &mut bool) -> bool {
    let mut changed = false;
    stack(ui, theme::LABEL_GAP, |ui| {
        ui.horizontal(|ui| {
            field_label(ui, label);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (level, color) = if *muted {
                    ("Muted".to_owned(), theme::MUTED)
                } else {
                    (format!("{percent}%"), theme::TEXT)
                };
                ui.label(mono_text(level, 13.0, color));
            });
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            changed |= mute_button(ui, muted, IconButtonKind::Secondary);
            changed |= volume_slider(ui, percent, muted, label, ui.available_width());
        });
    });
    changed
}

/// The stream toolbar's compact volume: a ghost mute toggle and a short
/// slider. Returns true when the level or mute state changed.
pub fn volume_inline(ui: &mut Ui, percent: &mut u32, muted: &mut bool, slider_width: f32) -> bool {
    let mut changed = mute_button(ui, muted, IconButtonKind::Ghost);
    if slider_width > 0.0 {
        changed |= volume_slider(ui, percent, muted, "Volume", slider_width);
    }
    changed
}

fn mute_button(ui: &mut Ui, muted: &mut bool, kind: IconButtonKind) -> bool {
    let (icon, label) = if *muted {
        (Icon::VolumeOff, "Unmute")
    } else {
        (Icon::Volume, "Mute")
    };
    let clicked = icon_button(ui, icon, label, kind).clicked();
    if clicked {
        *muted = !*muted;
    }
    clicked
}

fn volume_slider(
    ui: &mut Ui,
    percent: &mut u32,
    muted: &mut bool,
    label: &str,
    width: f32,
) -> bool {
    ui.scope(|ui| {
        if *muted {
            ui.multiply_opacity(0.45);
        }
        let changed = slider(ui, percent, 0..=100, label, width).changed();
        // Moving the slider is a clear request to hear the new level.
        if changed {
            *muted = false;
        }
        changed
    })
    .inner
}

pub fn icon_button(ui: &mut Ui, icon: Icon, label: &str, kind: IconButtonKind) -> Response {
    let enabled = ui.is_enabled();
    let (rect, response) =
        ui.allocate_exact_size(Vec2::splat(theme::CONTROL_HEIGHT), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        let (fill, stroke, foreground) = match kind {
            IconButtonKind::Ghost => (
                if hovered {
                    theme::SELECTED
                } else {
                    Color32::TRANSPARENT
                },
                Stroke::NONE,
                theme::TEXT,
            ),
            IconButtonKind::Secondary => (
                if hovered {
                    theme::SELECTED
                } else {
                    theme::RAISED
                },
                Stroke::new(1.0, theme::CONTROL_BORDER),
                theme::MUTED,
            ),
            IconButtonKind::Pressed => (theme::SELECTED, Stroke::NONE, theme::SIGNAL),
            IconButtonKind::Danger => (
                theme::tint(theme::DANGER, if hovered { 0.2 } else { 0.12 }),
                Stroke::NONE,
                theme::DANGER,
            ),
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(theme::RADIUS_CONTROL),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(18.0));
        icons::paint(ui.painter(), icon_rect, icon, foreground);
    }
    response.on_hover_text(label)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentTrack {
    /// Borderless track on the window background, sized to its labels.
    Bar,
    /// Bordered track inside a card, stretched to the available width.
    Field,
}

pub struct Segment<'a, T> {
    pub value: T,
    pub label: &'a str,
    pub icon: Option<Icon>,
    pub enabled: bool,
}

impl<'a, T> Segment<'a, T> {
    pub fn new(value: T, label: &'a str) -> Self {
        Self {
            value,
            label,
            icon: None,
            enabled: true,
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

fn segment_width<T>(ui: &Ui, segment: &Segment<'_, T>) -> f32 {
    let label = galley(
        ui,
        segment.label,
        theme::inter(14.0, Weight::Medium),
        theme::TEXT,
    );
    let icon = if segment.icon.is_some() { 24.0 } else { 0.0 };
    icon + label.size().x + 28.0
}

pub fn segmented<T: Copy + PartialEq>(
    ui: &mut Ui,
    id_salt: &str,
    selected: &mut T,
    segments: &[Segment<'_, T>],
    track: SegmentTrack,
) -> Response {
    let inset = 4.0;
    let gap = 4.0;
    let natural_widths: Vec<f32> = segments
        .iter()
        .map(|segment| segment_width(ui, segment))
        .collect();
    let width = match track {
        SegmentTrack::Bar => {
            natural_widths.iter().sum::<f32>()
                + gap * segments.len().saturating_sub(1) as f32
                + inset * 2.0
        }
        SegmentTrack::Field => ui.available_width(),
    };
    let (rect, mut response) =
        ui.allocate_exact_size(vec2(width, theme::CONTROL_HEIGHT), Sense::hover());
    let (fill, stroke) = match track {
        SegmentTrack::Bar => (theme::SURFACE, Stroke::NONE),
        SegmentTrack::Field => (theme::RAISED, Stroke::new(1.0, theme::CONTROL_BORDER)),
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::RADIUS_GROUP),
        fill,
        stroke,
        StrokeKind::Inside,
    );

    let equal_width = (rect.width() - inset * 2.0 - gap * segments.len().saturating_sub(1) as f32)
        / segments.len().max(1) as f32;
    let mut left = rect.left() + inset;
    let base_id = ui.id().with(id_salt);
    for (index, segment) in segments.iter().enumerate() {
        let item_width = match track {
            SegmentTrack::Bar => natural_widths[index],
            SegmentTrack::Field => equal_width,
        };
        let item_rect = Rect::from_min_size(
            pos2(left, rect.center().y - theme::SEGMENT_HEIGHT / 2.0),
            vec2(item_width, theme::SEGMENT_HEIGHT),
        );
        left += item_width + gap;

        let enabled = ui.is_enabled() && segment.enabled;
        let is_selected = *selected == segment.value;
        let sense = if enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let item = ui.interact(item_rect, base_id.with(index), sense);
        item.widget_info(|| {
            WidgetInfo::selected(
                WidgetType::SelectableLabel,
                enabled,
                is_selected,
                segment.label,
            )
        });
        if item.clicked() && !is_selected {
            *selected = segment.value;
            response.mark_changed();
        }
        let is_selected = *selected == segment.value;
        let hovered = enabled && item.hovered();
        if is_selected {
            let shadow = egui::Shadow {
                offset: [0, 1],
                blur: 2,
                spread: 0,
                color: Color32::from_black_alpha(100),
            };
            ui.painter()
                .add(shadow.as_shape(item_rect, CornerRadius::same(theme::RADIUS_CONTROL)));
            ui.painter().rect_filled(
                item_rect,
                CornerRadius::same(theme::RADIUS_CONTROL),
                theme::SELECTED,
            );
        }
        let foreground = if is_selected || hovered {
            theme::TEXT
        } else if enabled {
            theme::MUTED
        } else {
            theme::MUTED.gamma_multiply(0.5)
        };
        let label = galley(
            ui,
            segment.label,
            theme::inter(14.0, Weight::Medium),
            foreground,
        );
        let icon_width = if segment.icon.is_some() { 24.0 } else { 0.0 };
        let content_left = item_rect.center().x - (icon_width + label.size().x) / 2.0;
        if let Some(icon) = segment.icon {
            let icon_rect = Rect::from_center_size(
                pos2(content_left + 8.0, item_rect.center().y),
                Vec2::splat(16.0),
            );
            icons::paint(ui.painter(), icon_rect, icon, foreground);
        }
        paint_centered_galley(ui, label, content_left + icon_width, item_rect.center().y);
    }
    response
}

pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let enabled = ui.is_enabled();
    let (rect, mut response) = ui.allocate_exact_size(vec2(40.0, 24.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, enabled, *on, label));
    if ui.is_rect_visible(rect) {
        let progress = ui.ctx().animate_bool_with_time(response.id, *on, 0.15);
        let track = theme::CONTROL_BORDER.lerp_to_gamma(theme::SIGNAL, progress);
        ui.painter()
            .rect_filled(rect, CornerRadius::same(12), track);
        let knob = pos2(rect.left() + 12.0 + progress * 16.0, rect.center().y);
        ui.painter()
            .circle_filled(knob + vec2(0.0, 1.0), 10.0, Color32::from_black_alpha(90));
        ui.painter().circle_filled(knob, 10.0, Color32::WHITE);
        if response.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(14),
                Stroke::new(2.0, theme::SIGNAL),
                StrokeKind::Outside,
            );
        }
    }
    response
}

pub fn slider(
    ui: &mut Ui,
    value: &mut u32,
    range: RangeInclusive<u32>,
    label: &str,
    width: f32,
) -> Response {
    let enabled = ui.is_enabled();
    let (rect, mut response) = ui.allocate_exact_size(vec2(width, 24.0), Sense::click_and_drag());
    let (min, max) = (*range.start(), *range.end());
    let knob_radius = 9.0;
    let track = rect.shrink2(vec2(knob_radius, 0.0));
    if let Some(pointer) = response.interact_pointer_pos() {
        if response.dragged() || response.clicked() {
            let fraction = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
            let next = (min as f32 + fraction * (max - min) as f32).round() as u32;
            if next != *value {
                *value = next;
                response.mark_changed();
            }
        }
    }
    if response.has_focus() {
        let step = ui.input(|input| {
            let big = if input.modifiers.shift { 10 } else { 1 };
            if input.key_pressed(Key::ArrowRight) || input.key_pressed(Key::ArrowUp) {
                big
            } else if input.key_pressed(Key::ArrowLeft) || input.key_pressed(Key::ArrowDown) {
                -big
            } else {
                0
            }
        });
        if step != 0 {
            let next = (*value as i64 + step).clamp(min as i64, max as i64) as u32;
            if next != *value {
                *value = next;
                response.mark_changed();
            }
        }
    }
    response.widget_info(|| WidgetInfo::slider(enabled, f64::from(*value), label));

    if ui.is_rect_visible(rect) {
        let fraction = (*value - min) as f32 / (max - min).max(1) as f32;
        let knob_x = track.left() + fraction * track.width();
        let rail = Rect::from_min_max(
            pos2(track.left(), rect.center().y - 3.0),
            pos2(track.right(), rect.center().y + 3.0),
        );
        ui.painter()
            .rect_filled(rail, CornerRadius::same(3), theme::CONTROL_BORDER);
        ui.painter().rect_filled(
            Rect::from_min_max(rail.min, pos2(knob_x, rail.max.y)),
            CornerRadius::same(3),
            theme::SIGNAL,
        );
        let knob = pos2(knob_x, rect.center().y);
        ui.painter().circle_filled(
            knob + vec2(0.0, 1.0),
            knob_radius,
            Color32::from_black_alpha(90),
        );
        ui.painter()
            .circle_filled(knob, knob_radius, Color32::WHITE);
        if response.has_focus() {
            ui.painter()
                .circle_stroke(knob, knob_radius + 3.0, Stroke::new(2.0, theme::SIGNAL));
        }
    }
    response
}

/// A single-line field for addresses and numbers, in the monospace font.
pub fn text_field(
    ui: &mut Ui,
    value: &mut String,
    hint: &str,
    width: f32,
    invalid: bool,
) -> Response {
    field(
        ui,
        value,
        mono_text(hint, 14.0, theme::MUTED),
        theme::mono(14.0, Weight::Regular),
        width,
        invalid,
        usize::MAX,
    )
}

/// A single-line field for a name, in the UI font, at most `limit` characters.
pub fn name_field(
    ui: &mut Ui,
    value: &mut String,
    hint: &str,
    width: f32,
    invalid: bool,
    limit: usize,
) -> Response {
    field(
        ui,
        value,
        text(hint, 14.0, Weight::Regular, theme::MUTED),
        theme::inter(14.0, Weight::Regular),
        width,
        invalid,
        limit,
    )
}

fn field(
    ui: &mut Ui,
    value: &mut String,
    hint: RichText,
    font: FontId,
    width: f32,
    invalid: bool,
    limit: usize,
) -> Response {
    ui.scope(|ui| {
        if invalid {
            let visuals = &mut ui.visuals_mut().widgets;
            for state in [
                &mut visuals.inactive,
                &mut visuals.hovered,
                &mut visuals.active,
            ] {
                state.bg_stroke = Stroke::new(1.0, theme::DANGER);
            }
        }
        ui.add_sized(
            vec2(width, theme::CONTROL_HEIGHT),
            TextEdit::singleline(value)
                .font(font)
                .text_color(theme::TEXT)
                .hint_text(hint)
                .char_limit(limit)
                .margin(Margin::symmetric(12, 0))
                .vertical_align(Align::Center)
                .min_size(vec2(width, theme::CONTROL_HEIGHT))
                .desired_width(f32::INFINITY),
        )
    })
    .inner
}

/// A full-width row with a leading icon, a title over a detail line, and one
/// trailing icon button. A `clickable` row also acts as one button. Returns
/// the row's response and the trailing button's.
/// The badge takes `badge`'s tone, and `status` leads the detail line.
#[allow(clippy::too_many_arguments)]
pub fn list_row(
    ui: &mut Ui,
    badge: (Icon, Tone),
    title: &str,
    status: Option<(&str, Tone)>,
    detail: &str,
    note: Option<&str>,
    action: (Icon, &str),
    label: &str,
    clickable: bool,
) -> (Response, Response) {
    let enabled = ui.is_enabled();
    let sense = if clickable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, row) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), sense);
    let kind = if clickable {
        WidgetType::Button
    } else {
        WidgetType::Label
    };
    row.widget_info(|| WidgetInfo::labeled(kind, enabled, label));

    // The row's hover is the pointer anywhere over it, so it stays lit while
    // the pointer is on the trailing button that sits on top of it.
    let hovered = clickable && enabled && ui.rect_contains_pointer(rect);
    let pressed = clickable && enabled && row.is_pointer_button_down_on();
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(theme::RADIUS_GROUP),
        if pressed {
            theme::PRESSED
        } else if hovered {
            theme::SELECTED
        } else {
            theme::RAISED
        },
    );
    let (icon, tone) = badge;
    let (badge_fill, badge_color) = match tone {
        Tone::Neutral => (theme::SELECTED, theme::MUTED),
        tone => (theme::tint(tone.color(), 0.12), tone.color()),
    };
    let badge = Rect::from_center_size(
        pos2(rect.left() + 12.0 + 16.0, rect.center().y),
        Vec2::splat(32.0),
    );
    ui.painter()
        .rect_filled(badge, CornerRadius::same(theme::RADIUS_CONTROL), badge_fill);
    icons::paint(
        ui.painter(),
        Rect::from_center_size(badge.center(), Vec2::splat(16.0)),
        icon,
        badge_color,
    );

    let button_rect = Rect::from_center_size(
        pos2(
            rect.right() - 8.0 - theme::CONTROL_HEIGHT / 2.0,
            rect.center().y,
        ),
        Vec2::splat(theme::CONTROL_HEIGHT),
    );
    let left = badge.right() + 12.0;
    let text_width = button_rect.left() - 8.0 - left;
    let title_galley = truncated_galley(
        ui,
        title,
        theme::inter(14.0, Weight::SemiBold),
        theme::TEXT,
        text_width,
    );
    let mut job = LayoutJob::default();
    if let Some((status, tone)) = status {
        job.append(
            status,
            0.0,
            TextFormat::simple(theme::inter(12.0, Weight::Medium), tone.color()),
        );
        job.append(
            " · ",
            0.0,
            TextFormat::simple(theme::inter(12.0, Weight::Regular), theme::MUTED),
        );
    }
    job.append(
        detail,
        0.0,
        TextFormat::simple(theme::mono(12.0, Weight::Regular), theme::MUTED),
    );
    if let Some(note) = note {
        job.append(
            &format!(" · {note}"),
            0.0,
            TextFormat::simple(theme::inter(12.0, Weight::Regular), theme::MUTED),
        );
    }
    job.wrap = egui::text::TextWrapping::truncate_at_width(text_width.max(0.0));
    let detail_galley = ui.painter().layout_job(job);
    let gap = 2.0;
    let top = rect.center().y - (title_galley.size().y + gap + detail_galley.size().y) / 2.0;
    let detail_top = top + title_galley.size().y + gap;
    ui.painter()
        .galley(pos2(left, top), title_galley, theme::TEXT);
    ui.painter()
        .galley(pos2(left, detail_top), detail_galley, theme::MUTED);

    // A detached child, so placing the button never moves the row's layout.
    let mut button_ui = ui.new_child(UiBuilder::new().max_rect(button_rect));
    let button = icon_button(&mut button_ui, action.0, action.1, IconButtonKind::Ghost);
    (row, button)
}

/// A select-style trigger; pair it with [`egui::Popup::menu`].
pub fn select_button(ui: &mut Ui, selected: &str, width: f32) -> Response {
    let enabled = ui.is_enabled();
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, theme::CONTROL_HEIGHT), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::ComboBox, enabled, selected));
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        ui.painter().rect(
            rect,
            CornerRadius::same(theme::RADIUS_CONTROL),
            if hovered {
                theme::SELECTED
            } else {
                theme::RAISED
            },
            Stroke::new(
                1.0,
                if hovered {
                    theme::HOVER_BORDER
                } else {
                    theme::CONTROL_BORDER
                },
            ),
            StrokeKind::Inside,
        );
        let label = truncated_galley(
            ui,
            selected,
            theme::inter(14.0, Weight::Regular),
            theme::TEXT,
            rect.width() - 48.0,
        );
        paint_centered_galley(ui, label, rect.left() + 12.0, rect.center().y);
        let chevron = Rect::from_center_size(
            pos2(rect.right() - 20.0, rect.center().y),
            Vec2::splat(16.0),
        );
        icons::paint(ui.painter(), chevron, Icon::ChevronDown, theme::MUTED);
    }
    response
}

pub fn menu_row(ui: &mut Ui, label: &str, selected: bool) -> Response {
    let width = ui.available_width().max(200.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, selected, label));
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS_CONTROL),
            theme::SELECTED,
        );
    }
    let label_galley = truncated_galley(
        ui,
        label,
        theme::inter(14.0, Weight::Regular),
        theme::TEXT,
        rect.width() - 44.0,
    );
    paint_centered_galley(ui, label_galley, rect.left() + 10.0, rect.center().y);
    if selected {
        let check = Rect::from_center_size(
            pos2(rect.right() - 18.0, rect.center().y),
            Vec2::splat(16.0),
        );
        icons::paint(ui.painter(), check, Icon::Check, theme::SIGNAL);
    }
    response
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dot {
    Solid,
    Hollow,
    Pulse,
}

pub fn status_pill(ui: &mut Ui, tone: Tone, label: &str, detail: Option<&str>, dot: Dot) {
    let (fill, stroke, foreground) = match tone {
        Tone::Neutral => (theme::SURFACE, theme::DIVIDER, theme::MUTED),
        Tone::Signal => (
            theme::tint(theme::SIGNAL, 0.12),
            theme::tint(theme::SIGNAL, 0.24),
            theme::TEXT,
        ),
        Tone::Caution => (
            theme::tint(theme::CAUTION, 0.12),
            theme::tint(theme::CAUTION, 0.24),
            theme::CAUTION,
        ),
        Tone::Danger => (
            theme::tint(theme::DANGER, 0.12),
            theme::tint(theme::DANGER, 0.24),
            theme::DANGER,
        ),
    };
    let label_galley = (!label.is_empty())
        .then(|| galley(ui, label, theme::inter(13.0, Weight::Medium), foreground));
    let detail_galley = detail.map(|detail| {
        let text = if label.is_empty() {
            detail.to_owned()
        } else {
            format!(" · {detail}")
        };
        galley(ui, text, theme::mono(12.0, Weight::Regular), foreground)
    });
    let text_width = label_galley.as_ref().map_or(0.0, |g| g.size().x)
        + detail_galley.as_ref().map_or(0.0, |g| g.size().x);
    let (rect, _) = ui.allocate_exact_size(
        vec2(12.0 + 8.0 + 8.0 + text_width + 12.0, theme::PILL_HEIGHT),
        Sense::hover(),
    );
    ui.painter().rect(
        rect,
        CornerRadius::same(14),
        fill,
        Stroke::new(1.0, stroke),
        StrokeKind::Inside,
    );
    let dot_center = pos2(rect.left() + 16.0, rect.center().y);
    let dot_color = tone.color();
    match dot {
        Dot::Solid => {
            ui.painter().circle_filled(dot_center, 4.0, dot_color);
        }
        Dot::Hollow => {
            ui.painter()
                .circle_stroke(dot_center, 3.25, Stroke::new(1.5, dot_color));
        }
        Dot::Pulse => {
            let time = ui.input(|input| input.time) as f32;
            let alpha = 0.35 + 0.65 * (0.5 + 0.5 * (time * std::f32::consts::TAU / 1.4).cos());
            ui.painter()
                .circle_filled(dot_center, 4.0, dot_color.gamma_multiply(alpha));
            ui.ctx().request_repaint();
        }
    }
    let mut left = rect.left() + 28.0;
    if let Some(label_galley) = label_galley {
        let width = label_galley.size().x;
        paint_centered_galley(ui, label_galley, left, rect.center().y);
        left += width;
    }
    if let Some(detail_galley) = detail_galley {
        paint_centered_galley(ui, detail_galley, left, rect.center().y);
    }
}

pub fn tag(ui: &mut Ui, label: &str, tone: Tone) -> Response {
    let (fill, foreground, weight) = match tone {
        Tone::Neutral => (theme::RAISED, theme::TEXT, Weight::Medium),
        tone => (
            theme::tint(tone.color(), 0.12),
            tone.color(),
            Weight::SemiBold,
        ),
    };
    let has_dot = tone != Tone::Neutral;
    let label_galley = galley(ui, label, theme::inter(12.0, weight), foreground);
    let dot_width = if has_dot { 6.0 + 6.0 } else { 0.0 };
    let (rect, response) = ui.allocate_exact_size(
        vec2(
            10.0 + dot_width + label_galley.size().x + 10.0,
            theme::TAG_HEIGHT,
        ),
        Sense::hover(),
    );
    ui.painter().rect_filled(rect, CornerRadius::same(12), fill);
    if has_dot {
        ui.painter()
            .circle_filled(pos2(rect.left() + 13.0, rect.center().y), 3.0, foreground);
    }
    paint_centered_galley(
        ui,
        label_galley,
        rect.left() + 10.0 + dot_width,
        rect.center().y,
    );
    response
}

pub fn kbd(ui: &mut Ui, key: &str) {
    let key_galley = galley(ui, key, theme::mono(12.0, Weight::Regular), theme::TEXT);
    let (rect, _) = ui.allocate_exact_size(vec2(key_galley.size().x + 12.0, 22.0), Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::RADIUS_COMPACT),
        theme::RAISED,
        Stroke::new(1.0, theme::CONTROL_BORDER),
        StrokeKind::Inside,
    );
    paint_centered_galley(ui, key_galley, rect.left() + 6.0, rect.center().y);
}

pub fn icon_tile(ui: &mut Ui, icon: Icon, tone: Tone) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
    let (fill, stroke, foreground) = match tone {
        Tone::Neutral => (
            theme::SURFACE,
            Stroke::new(1.0, theme::DIVIDER),
            theme::MUTED,
        ),
        tone => (theme::tint(tone.color(), 0.12), Stroke::NONE, tone.color()),
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::RADIUS_GROUP),
        fill,
        stroke,
        StrokeKind::Inside,
    );
    icons::paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(22.0)),
        icon,
        foreground,
    );
}

/// A pulsing ring around a status icon, for states that wait on the other side.
pub fn pulse(ui: &mut Ui, icon: Icon, tone: Tone) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(96.0), Sense::hover());
    let color = tone.color();
    let time = ui.input(|input| input.time) as f32;
    for offset in [0.0, 0.5] {
        let phase = ((time / 2.0) + offset).fract();
        let radius = 24.0 * (1.0 + 1.2 * phase);
        ui.painter().circle_stroke(
            rect.center(),
            radius,
            Stroke::new(2.0, color.gamma_multiply(0.5 * (1.0 - phase))),
        );
    }
    ui.painter()
        .circle_filled(rect.center(), 24.0, theme::tint(color, 0.12));
    icons::paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(22.0)),
        icon,
        color,
    );
    ui.ctx().request_repaint();
}

pub fn banner(ui: &mut Ui, tone: Tone, title: &str, body: &str) {
    let color = tone.color();
    egui::Frame::new()
        .fill(theme::tint(color, 0.12))
        .stroke(Stroke::new(1.0, theme::tint(color, 0.28)))
        .corner_radius(theme::RADIUS_GROUP)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (icon_rect, _) = ui.allocate_exact_size(vec2(16.0, 18.0), Sense::hover());
                let icon = if tone == Tone::Danger {
                    Icon::AlertCircle
                } else {
                    Icon::Alert
                };
                icons::paint(
                    ui.painter(),
                    Rect::from_center_size(icon_rect.center(), Vec2::splat(16.0)),
                    icon,
                    color,
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(text(title, 13.0, Weight::SemiBold, color));
                    if !body.is_empty() {
                        ui.add(
                            egui::Label::new(text(body, 12.0, Weight::Regular, theme::BANNER_TEXT))
                                .wrap(),
                        );
                    }
                });
            });
        });
}

pub fn metric_tile(ui: &mut Ui, width: f32, label: &str, value: &str, unit: &str, context: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, theme::TILE_HEIGHT), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    ui.painter()
        .rect_filled(rect, CornerRadius::same(theme::RADIUS_GROUP), theme::RAISED);
    let inner = rect.shrink(12.0);
    let label_galley = truncated_galley(
        ui,
        label,
        theme::inter(12.0, Weight::Regular),
        theme::MUTED,
        inner.width(),
    );
    paint_centered_galley(ui, label_galley, inner.left(), inner.top() + 8.0);
    let value_galley = galley(ui, value, theme::mono(20.0, Weight::Medium), theme::TEXT);
    let value_width = value_galley.size().x;
    paint_centered_galley(ui, value_galley, inner.left(), inner.top() + 30.0);
    if !unit.is_empty() {
        let unit_galley = galley(ui, unit, theme::inter(12.0, Weight::Regular), theme::MUTED);
        paint_centered_galley(
            ui,
            unit_galley,
            inner.left() + value_width + 4.0,
            inner.top() + 32.0,
        );
    }
    let context_galley = truncated_galley(
        ui,
        context,
        theme::inter(12.0, Weight::Regular),
        theme::MUTED,
        inner.width(),
    );
    paint_centered_galley(ui, context_galley, inner.left(), inner.bottom() - 8.0);
}

/// Lays out tiles in rows of `columns`, each `TILE_HEIGHT` tall.
pub fn tile_grid(ui: &mut Ui, columns: usize, tiles: &[(&str, String, &str, String)]) {
    let gap = 8.0;
    let columns = columns.max(1);
    let width = (ui.available_width() - gap * (columns - 1) as f32) / columns as f32;
    stack(ui, gap, |ui| {
        for row in tiles.chunks(columns) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (label, value, unit, context) in row {
                    metric_tile(ui, width, label, value, unit, context);
                }
            });
        }
    });
}

pub fn meter_row(ui: &mut Ui, label: &str, value_ms: f32, scale_ms: f32, rail: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let label_width = 104.0;
    let value_width = 44.0;
    let label_galley = truncated_galley(
        ui,
        label,
        theme::inter(13.0, Weight::Regular),
        theme::TEXT,
        label_width,
    );
    paint_centered_galley(ui, label_galley, rect.left(), rect.center().y);
    let bar = Rect::from_min_max(
        pos2(rect.left() + label_width + 12.0, rect.center().y - 3.0),
        pos2(rect.right() - value_width - 12.0, rect.center().y + 3.0),
    );
    ui.painter().rect_filled(bar, CornerRadius::same(3), rail);
    let fraction = (value_ms / scale_ms.max(f32::EPSILON)).clamp(0.0, 1.0);
    if value_ms > 0.0 {
        let fill_width = (bar.width() * fraction).max(4.0);
        ui.painter().rect_filled(
            Rect::from_min_size(bar.min, vec2(fill_width, bar.height())),
            CornerRadius::same(3),
            theme::SIGNAL,
        );
    }
    let value_galley = galley(
        ui,
        format!("{value_ms:.2}"),
        theme::mono(12.0, Weight::Regular),
        theme::TEXT,
    );
    let value_left = rect.right() - value_galley.size().x;
    paint_centered_galley(ui, value_galley, value_left, rect.center().y);
}

/// One label/value row; the value is right-aligned and optionally monospaced.
pub fn key_value(ui: &mut Ui, key: &str, value: &str, mono: bool, size: f32) {
    let row_height = size + 6.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_height), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let value_font = if mono {
        theme::mono(size - 1.0, Weight::Regular)
    } else {
        theme::inter(size, Weight::Regular)
    };
    // Long values (device names, backends) give way before the label does.
    let key_width = galley(ui, key, theme::inter(size, Weight::Regular), theme::MUTED)
        .size()
        .x
        .min(rect.width() * 0.45);
    let value_galley = truncated_galley(
        ui,
        value,
        value_font,
        theme::TEXT,
        rect.width() - key_width - 16.0,
    );
    let value_width = value_galley.size().x;
    let key_galley = truncated_galley(
        ui,
        key,
        theme::inter(size, Weight::Regular),
        theme::MUTED,
        rect.width() - value_width - 16.0,
    );
    paint_centered_galley(ui, key_galley, rect.left(), rect.center().y);
    paint_centered_galley(
        ui,
        value_galley,
        rect.right() - value_width,
        rect.center().y,
    );
}

pub fn divider(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, theme::DIVIDER);
}

/// A collapsible inset section whose open state lives in egui memory.
pub fn disclosure(ui: &mut Ui, id_salt: &str, title: &str, add_body: impl FnOnce(&mut Ui)) {
    let id = Id::new(("echobind-disclosure", id_salt));
    let open = ui.data(|data| data.get_temp::<bool>(id)).unwrap_or(false);
    egui::Frame::new()
        .fill(theme::RAISED)
        .corner_radius(theme::RADIUS_GROUP)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (rect, response) = ui.allocate_exact_size(
                vec2(ui.available_width(), theme::CONTROL_HEIGHT),
                Sense::click(),
            );
            response.widget_info(|| {
                WidgetInfo::selected(WidgetType::CollapsingHeader, true, open, title)
            });
            if response.clicked() {
                ui.data_mut(|data| data.insert_temp(id, !open));
            }
            let title_galley = galley(ui, title, theme::inter(13.0, Weight::Medium), theme::TEXT);
            paint_centered_galley(ui, title_galley, rect.left() + 12.0, rect.center().y);
            let chevron = Rect::from_center_size(
                pos2(rect.right() - 20.0, rect.center().y),
                Vec2::splat(16.0),
            );
            let icon = if open {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            };
            icons::paint(ui.painter(), chevron, icon, theme::MUTED);
            if open {
                ui.scope_builder(
                    UiBuilder::new().layout(Layout::top_down(Align::Min)),
                    |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin {
                                left: 12,
                                right: 12,
                                top: 0,
                                bottom: 12,
                            })
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 6.0;
                                add_body(ui);
                            });
                    },
                );
            }
        });
}

pub fn step_number(ui: &mut Ui, number: usize, active: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
    let (fill, foreground) = if active {
        (theme::tint(theme::SIGNAL, 0.12), theme::SIGNAL)
    } else {
        (theme::RAISED, theme::MUTED)
    };
    ui.painter().circle_filled(rect.center(), 12.0, fill);
    let number = galley(
        ui,
        number.to_string(),
        theme::inter(12.0, Weight::SemiBold),
        foreground,
    );
    let left = rect.center().x - number.size().x / 2.0;
    paint_centered_galley(ui, number, left, rect.center().y);
}

/// Three numbered cards explaining a flow, laid out in one row.
pub fn steps(ui: &mut Ui, steps: &[(&str, &str)]) {
    let gap = 12.0;
    let count = steps.len().max(1) as f32;
    let card_width = (ui.available_width() - gap * (count - 1.0)) / count;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (index, (heading, body)) in steps.iter().enumerate() {
            egui::Frame::new()
                .fill(theme::SURFACE)
                .stroke(Stroke::new(1.0, theme::DIVIDER))
                .corner_radius(theme::RADIUS_GROUP)
                .inner_margin(16)
                .show(ui, |ui| {
                    ui.set_width(card_width - 34.0);
                    ui.set_min_height(84.0);
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        stack(ui, 8.0, |ui| {
                            step_number(ui, index + 1, index == 0);
                            ui.label(text(*heading, 14.0, Weight::Medium, theme::TEXT));
                            ui.add(
                                egui::Label::new(text(*body, 13.0, Weight::Regular, theme::MUTED))
                                    .wrap(),
                            );
                        });
                    });
                });
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    Done,
    Current,
    Pending,
}

pub fn progress_steps(ui: &mut Ui, steps: &[(&str, StepState)]) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 50.0), Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::RADIUS_GROUP),
        theme::SURFACE,
        Stroke::new(1.0, theme::DIVIDER),
        StrokeKind::Inside,
    );
    let inner = rect.shrink(5.0);
    let gap = 4.0;
    let count = steps.len().max(1) as f32;
    let width = (inner.width() - gap * (count - 1.0)) / count;
    for (index, (label, state)) in steps.iter().enumerate() {
        let item = Rect::from_min_size(
            pos2(inner.left() + index as f32 * (width + gap), inner.top()),
            vec2(width, theme::CONTROL_HEIGHT),
        );
        if *state == StepState::Current {
            ui.painter().rect_filled(
                item,
                CornerRadius::same(theme::RADIUS_CONTROL),
                theme::SELECTED,
            );
        }
        let foreground = if *state == StepState::Pending {
            theme::MUTED
        } else {
            theme::TEXT
        };
        let label = galley(ui, *label, theme::inter(13.0, Weight::Medium), foreground);
        let content_width = 16.0 + 8.0 + label.size().x;
        let left = item.center().x - content_width / 2.0;
        let marker = Rect::from_center_size(pos2(left + 8.0, item.center().y), Vec2::splat(16.0));
        match state {
            StepState::Done => icons::paint(ui.painter(), marker, Icon::Check, theme::SIGNAL),
            StepState::Current => {
                let time = ui.input(|input| input.time) as f32;
                let alpha = 0.35 + 0.65 * (0.5 + 0.5 * (time * std::f32::consts::TAU / 1.4).cos());
                ui.painter().circle_filled(
                    marker.center(),
                    4.0,
                    theme::CAUTION.gamma_multiply(alpha),
                );
                ui.ctx().request_repaint();
            }
            StepState::Pending => {
                ui.painter()
                    .circle_stroke(marker.center(), 3.25, Stroke::new(1.5, theme::MUTED));
            }
        }
        paint_centered_galley(ui, label, left + 24.0, item.center().y);
    }
}

/// Centers a row of key caps followed by a short muted explanation.
pub fn key_hint(ui: &mut Ui, keys: &[&str], separator: &str, explanation: &str) {
    let font = theme::inter(13.0, Weight::Regular);
    let key_widths: f32 = keys
        .iter()
        .map(|key| {
            galley(ui, *key, theme::mono(12.0, Weight::Regular), theme::TEXT)
                .size()
                .x
                + 12.0
        })
        .sum();
    let separator_width = galley(ui, separator, font.clone(), theme::MUTED).size().x;
    let explanation_width = galley(ui, explanation, font.clone(), theme::MUTED).size().x;
    let total = key_widths
        + separator_width * keys.len().saturating_sub(1) as f32
        + explanation_width
        + 8.0 * (keys.len() * 2) as f32;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0) - 8.0);
        for (index, key) in keys.iter().enumerate() {
            if index > 0 {
                ui.label(
                    RichText::new(separator)
                        .font(font.clone())
                        .color(theme::MUTED),
                );
            }
            kbd(ui, key);
        }
        ui.label(RichText::new(explanation).font(font).color(theme::MUTED));
    });
}

pub fn summary_pill(ui: &mut Ui, icon: Icon, label: &str) {
    let label_galley = galley(ui, label, theme::inter(13.0, Weight::Medium), theme::TEXT);
    let (rect, _) = ui.allocate_exact_size(
        vec2(
            12.0 + 14.0 + 8.0 + label_galley.size().x + 12.0,
            theme::PILL_HEIGHT,
        ),
        Sense::hover(),
    );
    ui.painter().rect(
        rect,
        CornerRadius::same(14),
        theme::SURFACE,
        Stroke::new(1.0, theme::DIVIDER),
        StrokeKind::Inside,
    );
    icons::paint(
        ui.painter(),
        Rect::from_center_size(pos2(rect.left() + 19.0, rect.center().y), Vec2::splat(14.0)),
        icon,
        theme::SIGNAL,
    );
    paint_centered_galley(ui, label_galley, rect.left() + 34.0, rect.center().y);
}

/// Counters as value-over-label cells that wrap to the available width.
pub fn counter_grid(ui: &mut Ui, counters: &[(&str, String)]) {
    let gap = 16.0;
    let columns = ((ui.available_width() + gap) / (120.0 + gap))
        .floor()
        .max(1.0) as usize;
    let width = (ui.available_width() - gap * (columns - 1) as f32) / columns as f32;
    stack(ui, 12.0, |ui| {
        for row in counters.chunks(columns) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (label, value) in row {
                    let (rect, _) = ui.allocate_exact_size(vec2(width, 38.0), Sense::hover());
                    let value = galley(
                        ui,
                        value.clone(),
                        theme::mono(15.0, Weight::Regular),
                        theme::TEXT,
                    );
                    paint_centered_galley(ui, value, rect.left(), rect.top() + 10.0);
                    let label = truncated_galley(
                        ui,
                        *label,
                        theme::inter(12.0, Weight::Regular),
                        theme::MUTED,
                        width,
                    );
                    paint_centered_galley(ui, label, rect.left(), rect.bottom() - 8.0);
                }
            });
        }
    });
}
