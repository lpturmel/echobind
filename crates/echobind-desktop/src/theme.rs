use eframe::egui::{
    self,
    epaint::text::VariationCoords,
    style::{Selection, WidgetVisuals},
    Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, FontTweak,
    Frame, Margin, Shadow, Stroke, TextStyle, Vec2,
};
use std::sync::Arc;

pub const WINDOW: Color32 = Color32::from_rgb(0x0D, 0x0E, 0x11);
pub const SURFACE: Color32 = Color32::from_rgb(0x15, 0x17, 0x1C);
pub const RAISED: Color32 = Color32::from_rgb(0x1C, 0x1F, 0x26);
pub const SELECTED: Color32 = Color32::from_rgb(0x2A, 0x2E, 0x37);
pub const PRESSED: Color32 = Color32::from_rgb(0x31, 0x35, 0x3F);
pub const DIVIDER: Color32 = Color32::from_rgb(0x23, 0x26, 0x2E);
pub const CONTROL_BORDER: Color32 = Color32::from_rgb(0x34, 0x39, 0x45);
pub const HOVER_BORDER: Color32 = Color32::from_rgb(0x4A, 0x50, 0x5C);
pub const STAGE: Color32 = Color32::from_rgb(0x08, 0x09, 0x0B);
pub const TEXT: Color32 = Color32::from_rgb(0xEC, 0xEE, 0xF2);
pub const MUTED: Color32 = Color32::from_rgb(0x9A, 0xA0, 0xAC);
pub const BANNER_TEXT: Color32 = Color32::from_rgb(0xD5, 0xD8, 0xDE);
pub const SIGNAL: Color32 = Color32::from_rgb(0x3D, 0xD9, 0xB3);
pub const SIGNAL_HOVER: Color32 = Color32::from_rgb(0x6B, 0xE6, 0xC7);
pub const SIGNAL_PRESSED: Color32 = Color32::from_rgb(0x2F, 0xC7, 0xA2);
pub const ON_SIGNAL: Color32 = Color32::from_rgb(0x04, 0x22, 0x1B);
pub const CAUTION: Color32 = Color32::from_rgb(0xF2, 0xB8, 0x4B);
pub const DANGER: Color32 = Color32::from_rgb(0xFF, 0x7A, 0x7A);

pub const RADIUS_COMPACT: u8 = 6;
pub const RADIUS_CONTROL: u8 = 8;
pub const RADIUS_GROUP: u8 = 12;
pub const RADIUS_SURFACE: u8 = 16;

pub const CONTROL_HEIGHT: f32 = 40.0;
pub const SEGMENT_HEIGHT: f32 = 32.0;
pub const PILL_HEIGHT: f32 = 28.0;
pub const TAG_HEIGHT: f32 = 24.0;
pub const TILE_HEIGHT: f32 = 84.0;

pub const GUTTER: f32 = 16.0;
pub const CARD_PADDING: f32 = 20.0;
pub const DIALOG_PADDING: f32 = 24.0;
pub const SECTION_GAP: f32 = 24.0;
pub const FIELD_GAP: f32 = 16.0;
pub const LABEL_GAP: f32 = 8.0;

pub const PANEL_WIDTH: f32 = 340.0;
pub const HEALTH_WIDTH: f32 = 320.0;
pub const DRAWER_WIDTH: f32 = 300.0;
pub const COMPACT_COLUMN_WIDTH: f32 = 480.0;
/// Below this window width the two-pane layout collapses to one column.
pub const COMPACT_BREAKPOINT: f32 = 960.0;

const INTER: &[u8] = include_bytes!("../assets/fonts/Inter-Variable.ttf");
const JETBRAINS_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Variable.ttf");

const INTER_REGULAR: &str = "inter-400";
const INTER_MEDIUM: &str = "inter-500";
const INTER_SEMIBOLD: &str = "inter-600";
const MONO_REGULAR: &str = "mono-400";
const MONO_MEDIUM: &str = "mono-500";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Medium,
    SemiBold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Signal,
    Caution,
    Danger,
}

impl Tone {
    pub fn color(self) -> Color32 {
        match self {
            Self::Neutral => MUTED,
            Self::Signal => SIGNAL,
            Self::Caution => CAUTION,
            Self::Danger => DANGER,
        }
    }
}

pub fn inter(size: f32, weight: Weight) -> FontId {
    let family = match weight {
        Weight::Regular => FontFamily::Proportional,
        Weight::Medium => FontFamily::Name(INTER_MEDIUM.into()),
        Weight::SemiBold => FontFamily::Name(INTER_SEMIBOLD.into()),
    };
    FontId::new(size, family)
}

pub fn mono(size: f32, weight: Weight) -> FontId {
    let family = match weight {
        Weight::Regular => FontFamily::Monospace,
        Weight::Medium | Weight::SemiBold => FontFamily::Name(MONO_MEDIUM.into()),
    };
    FontId::new(size, family)
}

/// Signal colors at 12% for soft fills and 24–28% for their borders.
pub fn tint(color: Color32, alpha: f32) -> Color32 {
    color.gamma_multiply(alpha)
}

pub fn card_frame() -> Frame {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, DIVIDER))
        .corner_radius(RADIUS_SURFACE)
        .inner_margin(CARD_PADDING as i8)
}

pub fn inset_frame() -> Frame {
    Frame::new()
        .fill(RAISED)
        .corner_radius(RADIUS_GROUP)
        .inner_margin(Margin::symmetric(16, 12))
}

pub fn overlay_shadow() -> Shadow {
    Shadow {
        offset: [0, 16],
        blur: 40,
        spread: 0,
        color: Color32::from_black_alpha(115),
    }
}

pub fn install(context: &egui::Context) {
    context.set_fonts(font_definitions());
    context.set_theme(egui::Theme::Dark);
    context.style_mut_of(egui::Theme::Dark, apply_style);
}

fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    for (name, data, weight) in [
        (INTER_REGULAR, INTER, 400.0),
        (INTER_MEDIUM, INTER, 500.0),
        (INTER_SEMIBOLD, INTER, 600.0),
        (MONO_REGULAR, JETBRAINS_MONO, 400.0),
        (MONO_MEDIUM, JETBRAINS_MONO, 500.0),
    ] {
        let tweak = FontTweak {
            coords: VariationCoords::new([("wght", weight)]),
            ..Default::default()
        };
        fonts.font_data.insert(
            name.to_owned(),
            Arc::new(FontData::from_static(data).tweak(tweak)),
        );
    }

    // Keep egui's bundled fonts as fallbacks for symbols Inter does not cover.
    let proportional_fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let monospace_fallbacks = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let with_fallbacks = |primary: &str, fallbacks: &[String]| {
        std::iter::once(primary.to_owned())
            .chain(fallbacks.iter().cloned())
            .collect::<Vec<_>>()
    };
    fonts.families.insert(
        FontFamily::Proportional,
        with_fallbacks(INTER_REGULAR, &proportional_fallbacks),
    );
    fonts.families.insert(
        FontFamily::Name(INTER_MEDIUM.into()),
        with_fallbacks(INTER_MEDIUM, &proportional_fallbacks),
    );
    fonts.families.insert(
        FontFamily::Name(INTER_SEMIBOLD.into()),
        with_fallbacks(INTER_SEMIBOLD, &proportional_fallbacks),
    );
    fonts.families.insert(
        FontFamily::Monospace,
        with_fallbacks(MONO_REGULAR, &monospace_fallbacks),
    );
    fonts.families.insert(
        FontFamily::Name(MONO_MEDIUM.into()),
        with_fallbacks(MONO_MEDIUM, &monospace_fallbacks),
    );
    fonts
}

fn apply_style(style: &mut egui::Style) {
    style.text_styles = [
        (TextStyle::Small, inter(12.0, Weight::Regular)),
        (TextStyle::Body, inter(14.0, Weight::Regular)),
        (TextStyle::Button, inter(14.0, Weight::Medium)),
        (TextStyle::Heading, inter(20.0, Weight::SemiBold)),
        (TextStyle::Monospace, mono(13.0, Weight::Regular)),
    ]
    .into();

    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::splat(8.0);
    spacing.button_padding = Vec2::new(12.0, 6.0);
    spacing.menu_margin = Margin::same(4);
    spacing.window_margin = Margin::same(16);

    let visuals = &mut style.visuals;
    visuals.dark_mode = true;
    visuals.panel_fill = WINDOW;
    visuals.window_fill = SURFACE;
    visuals.window_stroke = Stroke::new(1.0, CONTROL_BORDER);
    visuals.window_corner_radius = CornerRadius::same(RADIUS_SURFACE);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_GROUP);
    visuals.window_shadow = overlay_shadow();
    visuals.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(100),
    };
    visuals.extreme_bg_color = STAGE;
    visuals.faint_bg_color = RAISED;
    visuals.code_bg_color = RAISED;
    visuals.text_edit_bg_color = Some(RAISED);
    visuals.hyperlink_color = SIGNAL;
    visuals.warn_fg_color = CAUTION;
    visuals.error_fg_color = DANGER;
    visuals.weak_text_color = Some(MUTED);
    visuals.selection = Selection {
        bg_fill: tint(SIGNAL, 0.32),
        stroke: Stroke::new(2.0, SIGNAL),
    };
    visuals.text_cursor.stroke = Stroke::new(2.0, SIGNAL);
    visuals.interact_cursor = Some(CursorIcon::PointingHand);
    set_widget(&mut visuals.widgets.noninteractive, SURFACE, DIVIDER);
    set_widget(&mut visuals.widgets.inactive, RAISED, CONTROL_BORDER);
    set_widget(&mut visuals.widgets.hovered, SELECTED, HOVER_BORDER);
    set_widget(&mut visuals.widgets.active, PRESSED, SIGNAL);
    set_widget(&mut visuals.widgets.open, SELECTED, CONTROL_BORDER);
}

fn set_widget(widget: &mut WidgetVisuals, fill: Color32, border: Color32) {
    widget.bg_fill = fill;
    widget.weak_bg_fill = fill;
    widget.bg_stroke = Stroke::new(1.0, border);
    widget.fg_stroke = Stroke::new(1.0, TEXT);
    widget.corner_radius = CornerRadius::same(RADIUS_CONTROL);
    widget.expansion = 0.0;
}
