//! Custom color themes for the application.
//!
//! Provides Nord, Tokyo Night, and Claude inspired themes, plus the stock
//! egui themes with the same text field styling.
//!
//! Text fields: egui fills a `TextEdit` with `extreme_bg_color` unless
//! `text_edit_bg_color` is set. In dark themes that is near-black, which makes
//! every field look read-only. Each theme sets `text_edit_bg_color` to a fill
//! that stands apart from the panel, and an inactive widget stroke the field
//! outline is drawn with. `extreme_bg_color` is left alone: plots and meters
//! use it as their backdrop.

use egui::{Color32, Stroke, Visuals};

/// Standard egui dark theme, with text fields that look editable.
pub fn egui_dark() -> Visuals {
    let mut visuals = Visuals::dark();
    // Panel is gray 27 and buttons gray 60: sit the field between them.
    visuals.text_edit_bg_color = Some(Color32::from_gray(48));
    // egui draws no inactive stroke by default, so fields have no outline.
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_gray(90));
    visuals
}

/// Standard egui light theme, with text fields that look editable.
pub fn egui_light() -> Visuals {
    let mut visuals = Visuals::light();
    visuals.text_edit_bg_color = Some(Color32::WHITE);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_gray(170));
    visuals
}

/// Claude/Anthropic color palette
mod claude {
    use egui::Color32;

    // Dark backgrounds - warm brown tones
    pub const BG_DARK: Color32 = Color32::from_rgb(0x1f, 0x1e, 0x1d);
    pub const BG_DARK_ELEVATED: Color32 = Color32::from_rgb(0x2a, 0x28, 0x26);
    pub const BG_DARK_SURFACE: Color32 = Color32::from_rgb(0x35, 0x32, 0x2f);
    /// Outline of inactive widgets (text fields, buttons)
    pub const BORDER_DARK: Color32 = Color32::from_rgb(0x4a, 0x45, 0x41);

    // Light backgrounds - warm cream
    pub const BG_LIGHT: Color32 = Color32::from_rgb(0xfa, 0xf9, 0xf5);
    pub const BG_LIGHT_ELEVATED: Color32 = Color32::from_rgb(0xf4, 0xf3, 0xee);
    pub const BG_LIGHT_SURFACE: Color32 = Color32::from_rgb(0xeb, 0xe9, 0xe4);
    /// Outline of inactive widgets (text fields, buttons)
    pub const BORDER_LIGHT: Color32 = Color32::from_rgb(0xd6, 0xd1, 0xc7);

    // Text colors
    pub const TEXT_CREAM: Color32 = Color32::from_rgb(0xf5, 0xe6, 0xd3);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(0xc4, 0xa5, 0x84);
    pub const TEXT_DARK: Color32 = Color32::from_rgb(0x1f, 0x1e, 0x1d);
    pub const TEXT_DARK_MUTED: Color32 = Color32::from_rgb(0x6a, 0x65, 0x5e);

    // Accent colors - Claude orange/terracotta
    pub const ORANGE: Color32 = Color32::from_rgb(0xe6, 0x7d, 0x22);
    pub const ORANGE_LIGHT: Color32 = Color32::from_rgb(0xff, 0xb3, 0x8a);
    pub const TERRACOTTA: Color32 = Color32::from_rgb(0xd9, 0x77, 0x57);
    pub const CORAL: Color32 = Color32::from_rgb(0xc9, 0x64, 0x42);

    // Semantic colors
    pub const RED: Color32 = Color32::from_rgb(0xe0, 0x6c, 0x75);
}

/// Nord color palette
mod nord {
    use egui::Color32;

    // Polar Night - dark backgrounds
    pub const NORD0: Color32 = Color32::from_rgb(0x2e, 0x34, 0x40);
    pub const NORD1: Color32 = Color32::from_rgb(0x3b, 0x42, 0x52);
    pub const NORD2: Color32 = Color32::from_rgb(0x43, 0x4c, 0x5e);
    pub const NORD3: Color32 = Color32::from_rgb(0x4c, 0x56, 0x6a);

    // Snow Storm - light text/backgrounds
    pub const NORD4: Color32 = Color32::from_rgb(0xd8, 0xde, 0xe9);
    pub const NORD5: Color32 = Color32::from_rgb(0xe5, 0xe9, 0xf0);
    pub const NORD6: Color32 = Color32::from_rgb(0xec, 0xef, 0xf4);

    // Frost - accent blues/cyans
    pub const NORD8: Color32 = Color32::from_rgb(0x88, 0xc0, 0xd0);
    pub const NORD9: Color32 = Color32::from_rgb(0x81, 0xa1, 0xc1);
    pub const NORD10: Color32 = Color32::from_rgb(0x5e, 0x81, 0xac);

    // Aurora - semantic colors
    pub const NORD11: Color32 = Color32::from_rgb(0xbf, 0x61, 0x6a); // red/error
    pub const NORD12: Color32 = Color32::from_rgb(0xd0, 0x87, 0x70); // orange/warning
}

/// Tokyo Night color palette
mod tokyo {
    use egui::Color32;

    // Backgrounds
    pub const BG_NIGHT: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x26);
    pub const BG_STORM: Color32 = Color32::from_rgb(0x24, 0x28, 0x3b);
    pub const BG_LIGHT: Color32 = Color32::from_rgb(0xd5, 0xd6, 0xdb);

    // Foreground/text
    pub const FG_DARK: Color32 = Color32::from_rgb(0xa9, 0xb1, 0xd6);
    pub const FG_BRIGHT: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
    pub const FG_LIGHT: Color32 = Color32::from_rgb(0x34, 0x3b, 0x58);

    // UI elements
    pub const COMMENT: Color32 = Color32::from_rgb(0x56, 0x5f, 0x89);
    pub const SELECTION: Color32 = Color32::from_rgb(0x28, 0x2d, 0x42);

    // Accent colors
    pub const RED: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);
    pub const ORANGE: Color32 = Color32::from_rgb(0xff, 0x9e, 0x64);
    pub const CYAN: Color32 = Color32::from_rgb(0x2a, 0xc3, 0xde);
    pub const BLUE: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);
}

/// Create Nord Dark theme visuals
pub fn nord_dark() -> Visuals {
    let mut visuals = Visuals::dark();

    // Window and panel backgrounds
    visuals.window_fill = nord::NORD0;
    visuals.panel_fill = nord::NORD0;
    visuals.faint_bg_color = nord::NORD1;
    visuals.extreme_bg_color = nord::NORD1;
    visuals.text_edit_bg_color = Some(nord::NORD1);

    // Text colors
    visuals.override_text_color = Some(nord::NORD4);

    // Selection
    visuals.selection.bg_fill = nord::NORD3;
    visuals.selection.stroke = Stroke::new(1.0_f32, nord::NORD8);

    // Hyperlinks
    visuals.hyperlink_color = nord::NORD8;

    // Semantic colors
    visuals.warn_fg_color = nord::NORD12;
    visuals.error_fg_color = nord::NORD11;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = nord::NORD1;
    visuals.widgets.noninteractive.weak_bg_fill = nord::NORD1;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, nord::NORD3);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, nord::NORD4);

    visuals.widgets.inactive.bg_fill = nord::NORD2;
    visuals.widgets.inactive.weak_bg_fill = nord::NORD2;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, nord::NORD3);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, nord::NORD4);

    visuals.widgets.hovered.bg_fill = nord::NORD3;
    visuals.widgets.hovered.weak_bg_fill = nord::NORD3;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, nord::NORD8);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, nord::NORD6);

    visuals.widgets.active.bg_fill = nord::NORD9;
    visuals.widgets.active.weak_bg_fill = nord::NORD9;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, nord::NORD8);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0_f32, nord::NORD6);

    visuals.widgets.open.bg_fill = nord::NORD2;
    visuals.widgets.open.weak_bg_fill = nord::NORD2;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, nord::NORD8);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, nord::NORD4);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0_f32, nord::NORD3);

    visuals
}

/// Create Nord Light theme visuals
pub fn nord_light() -> Visuals {
    let mut visuals = Visuals::light();

    // Window and panel backgrounds
    visuals.window_fill = nord::NORD6;
    visuals.panel_fill = nord::NORD6;
    visuals.faint_bg_color = nord::NORD5;
    visuals.extreme_bg_color = nord::NORD4;
    visuals.text_edit_bg_color = Some(Color32::WHITE);

    // Text colors
    visuals.override_text_color = Some(nord::NORD0);

    // Selection
    visuals.selection.bg_fill = nord::NORD4;
    visuals.selection.stroke = Stroke::new(1.0_f32, nord::NORD10);

    // Hyperlinks
    visuals.hyperlink_color = nord::NORD10;

    // Semantic colors
    visuals.warn_fg_color = nord::NORD12;
    visuals.error_fg_color = nord::NORD11;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = nord::NORD5;
    visuals.widgets.noninteractive.weak_bg_fill = nord::NORD5;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, nord::NORD4);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, nord::NORD1);

    visuals.widgets.inactive.bg_fill = nord::NORD5;
    visuals.widgets.inactive.weak_bg_fill = nord::NORD5;
    // Between NORD4 and NORD3: NORD4 alone barely shows on the NORD6 panel.
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0xc2, 0xc9, 0xd6));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, nord::NORD2);

    visuals.widgets.hovered.bg_fill = nord::NORD4;
    visuals.widgets.hovered.weak_bg_fill = nord::NORD4;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, nord::NORD10);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, nord::NORD0);

    visuals.widgets.active.bg_fill = nord::NORD9;
    visuals.widgets.active.weak_bg_fill = nord::NORD9;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, nord::NORD10);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0_f32, nord::NORD6);

    visuals.widgets.open.bg_fill = nord::NORD5;
    visuals.widgets.open.weak_bg_fill = nord::NORD5;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, nord::NORD10);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, nord::NORD1);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0_f32, nord::NORD4);

    visuals
}

/// Create Tokyo Night theme visuals
pub fn tokyo_night() -> Visuals {
    let mut visuals = Visuals::dark();

    // Window and panel backgrounds
    visuals.window_fill = tokyo::BG_NIGHT;
    visuals.panel_fill = tokyo::BG_NIGHT;
    visuals.faint_bg_color = tokyo::SELECTION;
    visuals.extreme_bg_color = Color32::from_rgb(0x16, 0x16, 0x1e);
    visuals.text_edit_bg_color = Some(tokyo::BG_STORM);

    // Text colors
    visuals.override_text_color = Some(tokyo::FG_DARK);

    // Selection
    visuals.selection.bg_fill = tokyo::SELECTION;
    visuals.selection.stroke = Stroke::new(1.0_f32, tokyo::BLUE);

    // Hyperlinks
    visuals.hyperlink_color = tokyo::CYAN;

    // Semantic colors
    visuals.warn_fg_color = tokyo::ORANGE;
    visuals.error_fg_color = tokyo::RED;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(0x1f, 0x20, 0x2a);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(0x1f, 0x20, 0x2a);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, tokyo::COMMENT);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_DARK);

    visuals.widgets.inactive.bg_fill = tokyo::SELECTION;
    visuals.widgets.inactive.weak_bg_fill = tokyo::SELECTION;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, tokyo::COMMENT);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_DARK);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x32, 0x38, 0x50);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x32, 0x38, 0x50);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, tokyo::BLUE);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_BRIGHT);

    visuals.widgets.active.bg_fill = tokyo::BLUE;
    visuals.widgets.active.weak_bg_fill = tokyo::BLUE;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, tokyo::CYAN);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0_f32, tokyo::BG_NIGHT);

    visuals.widgets.open.bg_fill = tokyo::SELECTION;
    visuals.widgets.open.weak_bg_fill = tokyo::SELECTION;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, tokyo::BLUE);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_DARK);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0_f32, tokyo::COMMENT);

    visuals
}

/// Create Tokyo Night Storm theme visuals (slightly lighter background)
pub fn tokyo_night_storm() -> Visuals {
    let mut visuals = Visuals::dark();

    // Window and panel backgrounds - Storm uses lighter bg
    visuals.window_fill = tokyo::BG_STORM;
    visuals.panel_fill = tokyo::BG_STORM;
    visuals.faint_bg_color = Color32::from_rgb(0x2a, 0x2e, 0x45);
    visuals.extreme_bg_color = tokyo::BG_NIGHT;
    visuals.text_edit_bg_color = Some(Color32::from_rgb(0x2d, 0x32, 0x4a));

    // Text colors
    visuals.override_text_color = Some(tokyo::FG_BRIGHT);

    // Selection
    visuals.selection.bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.selection.stroke = Stroke::new(1.0_f32, tokyo::BLUE);

    // Hyperlinks
    visuals.hyperlink_color = tokyo::CYAN;

    // Semantic colors
    visuals.warn_fg_color = tokyo::ORANGE;
    visuals.error_fg_color = tokyo::RED;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(0x29, 0x2e, 0x42);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(0x29, 0x2e, 0x42);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, tokyo::COMMENT);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_BRIGHT);

    visuals.widgets.inactive.bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, tokyo::COMMENT);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_BRIGHT);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x38, 0x3e, 0x5a);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x38, 0x3e, 0x5a);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, tokyo::BLUE);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_BRIGHT);

    visuals.widgets.active.bg_fill = tokyo::BLUE;
    visuals.widgets.active.weak_bg_fill = tokyo::BLUE;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, tokyo::CYAN);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0_f32, tokyo::BG_STORM);

    visuals.widgets.open.bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.widgets.open.weak_bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, tokyo::BLUE);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_BRIGHT);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0_f32, tokyo::COMMENT);

    visuals
}

/// Create Tokyo Night Light theme visuals
pub fn tokyo_night_light() -> Visuals {
    let mut visuals = Visuals::light();

    // Window and panel backgrounds
    visuals.window_fill = tokyo::BG_LIGHT;
    visuals.panel_fill = tokyo::BG_LIGHT;
    visuals.faint_bg_color = Color32::from_rgb(0xd0, 0xd1, 0xd6);
    visuals.extreme_bg_color = Color32::from_rgb(0xc8, 0xc9, 0xce);
    visuals.text_edit_bg_color = Some(Color32::from_rgb(0xe6, 0xe7, 0xec));

    // Text colors
    visuals.override_text_color = Some(tokyo::FG_LIGHT);

    // Selection
    visuals.selection.bg_fill = Color32::from_rgb(0xc4, 0xc8, 0xda);
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::from_rgb(0x29, 0x59, 0xaa));

    // Hyperlinks
    visuals.hyperlink_color = Color32::from_rgb(0x29, 0x59, 0xaa);

    // Semantic colors
    visuals.warn_fg_color = Color32::from_rgb(0x8f, 0x5e, 0x15);
    visuals.error_fg_color = Color32::from_rgb(0x8c, 0x43, 0x51);

    // Widget colors
    let light_accent = Color32::from_rgb(0x29, 0x59, 0xaa);

    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(0xd0, 0xd1, 0xd6);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(0xd0, 0xd1, 0xd6);
    visuals.widgets.noninteractive.bg_stroke =
        Stroke::new(1.0_f32, Color32::from_rgb(0xb0, 0xb1, 0xb6));
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_LIGHT);

    visuals.widgets.inactive.bg_fill = Color32::from_rgb(0xca, 0xcb, 0xd0);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0xca, 0xcb, 0xd0);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0xb0, 0xb1, 0xb6));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_LIGHT);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0xc0, 0xc4, 0xd0);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0xc0, 0xc4, 0xd0);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, light_accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_LIGHT);

    visuals.widgets.active.bg_fill = light_accent;
    visuals.widgets.active.weak_bg_fill = light_accent;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0x1e, 0x40, 0x80));
    visuals.widgets.active.fg_stroke = Stroke::new(2.0_f32, Color32::WHITE);

    visuals.widgets.open.bg_fill = Color32::from_rgb(0xc8, 0xcc, 0xda);
    visuals.widgets.open.weak_bg_fill = Color32::from_rgb(0xc8, 0xcc, 0xda);
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, light_accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, tokyo::FG_LIGHT);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0xb0, 0xb1, 0xb6));

    visuals
}

/// Create Claude Dark theme visuals (warm brown)
pub fn claude_dark() -> Visuals {
    let mut visuals = Visuals::dark();

    // Ensure dark mode is set for proper text color auto-detection
    visuals.dark_mode = true;

    // Window and panel backgrounds - warm dark brown
    visuals.window_fill = claude::BG_DARK;
    visuals.panel_fill = claude::BG_DARK;
    visuals.faint_bg_color = claude::BG_DARK_ELEVATED;
    visuals.extreme_bg_color = Color32::from_rgb(0x18, 0x17, 0x16);
    visuals.text_edit_bg_color = Some(claude::BG_DARK_SURFACE);

    // Text colors - cream on brown
    visuals.override_text_color = Some(claude::TEXT_CREAM);

    // Selection
    visuals.selection.bg_fill = claude::BG_DARK_SURFACE;
    visuals.selection.stroke = Stroke::new(1.0_f32, claude::ORANGE);

    // Hyperlinks
    visuals.hyperlink_color = claude::ORANGE_LIGHT;

    // Semantic colors
    visuals.warn_fg_color = claude::ORANGE;
    visuals.error_fg_color = claude::RED;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = claude::BG_DARK_ELEVATED;
    visuals.widgets.noninteractive.weak_bg_fill = claude::BG_DARK_ELEVATED;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, claude::BG_DARK_SURFACE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, claude::TEXT_CREAM);

    visuals.widgets.inactive.bg_fill = claude::BG_DARK_ELEVATED;
    visuals.widgets.inactive.weak_bg_fill = claude::BG_DARK_ELEVATED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, claude::BORDER_DARK);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, claude::TEXT_MUTED);

    visuals.widgets.hovered.bg_fill = claude::BG_DARK_SURFACE;
    visuals.widgets.hovered.weak_bg_fill = claude::BG_DARK_SURFACE;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, claude::TERRACOTTA);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, claude::TEXT_CREAM);

    visuals.widgets.active.bg_fill = claude::TERRACOTTA;
    visuals.widgets.active.weak_bg_fill = claude::TERRACOTTA;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, claude::ORANGE);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0_f32, claude::TEXT_CREAM);

    visuals.widgets.open.bg_fill = claude::BG_DARK_SURFACE;
    visuals.widgets.open.weak_bg_fill = claude::BG_DARK_SURFACE;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, claude::TERRACOTTA);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, claude::TEXT_CREAM);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0_f32, claude::BG_DARK_SURFACE);

    visuals
}

/// Create Claude Light theme visuals (warm cream)
pub fn claude_light() -> Visuals {
    let mut visuals = Visuals::light();

    // Ensure light mode is set for proper text color auto-detection
    visuals.dark_mode = false;

    // Window and panel backgrounds - warm cream
    visuals.window_fill = claude::BG_LIGHT;
    visuals.panel_fill = claude::BG_LIGHT;
    visuals.faint_bg_color = claude::BG_LIGHT_ELEVATED;
    visuals.extreme_bg_color = claude::BG_LIGHT_SURFACE;
    visuals.text_edit_bg_color = Some(Color32::WHITE);

    // Text colors
    visuals.override_text_color = Some(claude::TEXT_DARK);

    // Selection
    visuals.selection.bg_fill = Color32::from_rgb(0xe8, 0xd5, 0xc4);
    visuals.selection.stroke = Stroke::new(1.0_f32, claude::CORAL);

    // Hyperlinks
    visuals.hyperlink_color = claude::CORAL;

    // Semantic colors
    visuals.warn_fg_color = claude::ORANGE;
    visuals.error_fg_color = claude::RED;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = claude::BG_LIGHT_ELEVATED;
    visuals.widgets.noninteractive.weak_bg_fill = claude::BG_LIGHT_ELEVATED;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, claude::BG_LIGHT_SURFACE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, claude::TEXT_DARK);

    visuals.widgets.inactive.bg_fill = claude::BG_LIGHT_ELEVATED;
    visuals.widgets.inactive.weak_bg_fill = claude::BG_LIGHT_ELEVATED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, claude::BORDER_LIGHT);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, claude::TEXT_DARK_MUTED);

    visuals.widgets.hovered.bg_fill = claude::BG_LIGHT_SURFACE;
    visuals.widgets.hovered.weak_bg_fill = claude::BG_LIGHT_SURFACE;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, claude::CORAL);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, claude::TEXT_DARK);

    visuals.widgets.active.bg_fill = claude::CORAL;
    visuals.widgets.active.weak_bg_fill = claude::CORAL;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, claude::TERRACOTTA);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0_f32, Color32::WHITE);

    visuals.widgets.open.bg_fill = claude::BG_LIGHT_SURFACE;
    visuals.widgets.open.weak_bg_fill = claude::BG_LIGHT_SURFACE;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, claude::CORAL);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, claude::TEXT_DARK);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0_f32, claude::BG_LIGHT_SURFACE);

    visuals
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of an opaque sRGB colour.
    fn luminance(c: Color32) -> f32 {
        let lin = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    /// WCAG contrast ratio between two colours (1.0 = identical, 21.0 = black on white).
    fn contrast(a: Color32, b: Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    fn all_themes() -> Vec<(&'static str, Visuals)> {
        vec![
            ("egui dark", egui_dark()),
            ("egui light", egui_light()),
            ("nord dark", nord_dark()),
            ("nord light", nord_light()),
            ("tokyo night", tokyo_night()),
            ("tokyo night storm", tokyo_night_storm()),
            ("tokyo night light", tokyo_night_light()),
            ("claude dark", claude_dark()),
            ("claude light", claude_light()),
        ]
    }

    /// Issue #1074: text fields looked read-only because the dark themes
    /// filled them near-black, darker than the panel, with no outline.
    #[test]
    fn text_fields_look_editable_in_every_theme() {
        let mut failures = Vec::new();
        for (name, v) in all_themes() {
            let Some(field) = v.text_edit_bg_color else {
                failures.push(format!("{name}: text_edit_bg_color must be set"));
                continue;
            };
            let panel = v.panel_fill;
            let stroke = v.widgets.inactive.bg_stroke;
            let text = v
                .override_text_color
                .unwrap_or(v.widgets.inactive.fg_stroke.color);

            let fill_vs_panel = contrast(field, panel);
            let outline_vs_field = contrast(stroke.color, field);
            let outline_vs_panel = contrast(stroke.color, panel);
            let text_vs_field = contrast(text, field);
            println!(
                "{name:18} fill/panel {fill_vs_panel:.2}  outline/fill {outline_vs_field:.2}  \
                 outline/panel {outline_vs_panel:.2}  text/fill {text_vs_field:.2}"
            );

            // A dark field reads as a recessed, read-only well: it must be
            // raised above the panel, like the buttons next to it.
            if v.dark_mode && (luminance(field) <= luminance(panel) || fill_vs_panel < 1.15) {
                failures.push(format!(
                    "{name}: field {field:?} must be clearly lighter than panel {panel:?} \
                     ({fill_vs_panel:.2})"
                ));
            }
            // The outline must show against both the field and the panel
            // around it (light themes rely on it: white on near-white).
            if stroke.width < 1.0 || outline_vs_field < 1.25 || outline_vs_panel < 1.25 {
                failures.push(format!(
                    "{name}: outline {stroke:?} not visible (on field {outline_vs_field:.2}, \
                     on panel {outline_vs_panel:.2})"
                ));
            }
            if text_vs_field < 4.5 {
                failures.push(format!(
                    "{name}: text {text:?} on field {field:?} below WCAG AA ({text_vs_field:.2})"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
