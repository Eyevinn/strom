//! Custom color themes for the application.
//!
//! Provides Nord and Tokyo Night inspired themes.

use egui::{Color32, Stroke, Visuals};

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

    // Text colors
    visuals.override_text_color = Some(nord::NORD4);

    // Selection
    visuals.selection.bg_fill = nord::NORD3;
    visuals.selection.stroke = Stroke::new(1.0, nord::NORD8);

    // Hyperlinks
    visuals.hyperlink_color = nord::NORD8;

    // Semantic colors
    visuals.warn_fg_color = nord::NORD12;
    visuals.error_fg_color = nord::NORD11;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = nord::NORD1;
    visuals.widgets.noninteractive.weak_bg_fill = nord::NORD1;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, nord::NORD3);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, nord::NORD4);

    visuals.widgets.inactive.bg_fill = nord::NORD2;
    visuals.widgets.inactive.weak_bg_fill = nord::NORD2;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, nord::NORD3);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, nord::NORD4);

    visuals.widgets.hovered.bg_fill = nord::NORD3;
    visuals.widgets.hovered.weak_bg_fill = nord::NORD3;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, nord::NORD8);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, nord::NORD6);

    visuals.widgets.active.bg_fill = nord::NORD9;
    visuals.widgets.active.weak_bg_fill = nord::NORD9;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, nord::NORD8);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0, nord::NORD6);

    visuals.widgets.open.bg_fill = nord::NORD2;
    visuals.widgets.open.weak_bg_fill = nord::NORD2;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, nord::NORD8);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, nord::NORD4);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0, nord::NORD3);

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

    // Text colors
    visuals.override_text_color = Some(nord::NORD0);

    // Selection
    visuals.selection.bg_fill = nord::NORD4;
    visuals.selection.stroke = Stroke::new(1.0, nord::NORD10);

    // Hyperlinks
    visuals.hyperlink_color = nord::NORD10;

    // Semantic colors
    visuals.warn_fg_color = nord::NORD12;
    visuals.error_fg_color = nord::NORD11;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = nord::NORD5;
    visuals.widgets.noninteractive.weak_bg_fill = nord::NORD5;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, nord::NORD4);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, nord::NORD1);

    visuals.widgets.inactive.bg_fill = nord::NORD5;
    visuals.widgets.inactive.weak_bg_fill = nord::NORD5;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, nord::NORD4);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, nord::NORD2);

    visuals.widgets.hovered.bg_fill = nord::NORD4;
    visuals.widgets.hovered.weak_bg_fill = nord::NORD4;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, nord::NORD10);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, nord::NORD0);

    visuals.widgets.active.bg_fill = nord::NORD9;
    visuals.widgets.active.weak_bg_fill = nord::NORD9;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, nord::NORD10);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0, nord::NORD6);

    visuals.widgets.open.bg_fill = nord::NORD5;
    visuals.widgets.open.weak_bg_fill = nord::NORD5;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, nord::NORD10);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, nord::NORD1);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0, nord::NORD4);

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

    // Text colors
    visuals.override_text_color = Some(tokyo::FG_DARK);

    // Selection
    visuals.selection.bg_fill = tokyo::SELECTION;
    visuals.selection.stroke = Stroke::new(1.0, tokyo::BLUE);

    // Hyperlinks
    visuals.hyperlink_color = tokyo::CYAN;

    // Semantic colors
    visuals.warn_fg_color = tokyo::ORANGE;
    visuals.error_fg_color = tokyo::RED;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(0x1f, 0x20, 0x2a);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(0x1f, 0x20, 0x2a);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, tokyo::COMMENT);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, tokyo::FG_DARK);

    visuals.widgets.inactive.bg_fill = tokyo::SELECTION;
    visuals.widgets.inactive.weak_bg_fill = tokyo::SELECTION;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, tokyo::COMMENT);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, tokyo::FG_DARK);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x32, 0x38, 0x50);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x32, 0x38, 0x50);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, tokyo::BLUE);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, tokyo::FG_BRIGHT);

    visuals.widgets.active.bg_fill = tokyo::BLUE;
    visuals.widgets.active.weak_bg_fill = tokyo::BLUE;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, tokyo::CYAN);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0, tokyo::BG_NIGHT);

    visuals.widgets.open.bg_fill = tokyo::SELECTION;
    visuals.widgets.open.weak_bg_fill = tokyo::SELECTION;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, tokyo::BLUE);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, tokyo::FG_DARK);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0, tokyo::COMMENT);

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

    // Text colors
    visuals.override_text_color = Some(tokyo::FG_BRIGHT);

    // Selection
    visuals.selection.bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.selection.stroke = Stroke::new(1.0, tokyo::BLUE);

    // Hyperlinks
    visuals.hyperlink_color = tokyo::CYAN;

    // Semantic colors
    visuals.warn_fg_color = tokyo::ORANGE;
    visuals.error_fg_color = tokyo::RED;

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(0x29, 0x2e, 0x42);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(0x29, 0x2e, 0x42);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, tokyo::COMMENT);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, tokyo::FG_BRIGHT);

    visuals.widgets.inactive.bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, tokyo::COMMENT);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, tokyo::FG_BRIGHT);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x38, 0x3e, 0x5a);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x38, 0x3e, 0x5a);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, tokyo::BLUE);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, tokyo::FG_BRIGHT);

    visuals.widgets.active.bg_fill = tokyo::BLUE;
    visuals.widgets.active.weak_bg_fill = tokyo::BLUE;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, tokyo::CYAN);
    visuals.widgets.active.fg_stroke = Stroke::new(2.0, tokyo::BG_STORM);

    visuals.widgets.open.bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.widgets.open.weak_bg_fill = Color32::from_rgb(0x2d, 0x32, 0x4a);
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, tokyo::BLUE);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, tokyo::FG_BRIGHT);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0, tokyo::COMMENT);

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

    // Text colors
    visuals.override_text_color = Some(tokyo::FG_LIGHT);

    // Selection
    visuals.selection.bg_fill = Color32::from_rgb(0xc4, 0xc8, 0xda);
    visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(0x29, 0x59, 0xaa));

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
        Stroke::new(1.0, Color32::from_rgb(0xb0, 0xb1, 0xb6));
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, tokyo::FG_LIGHT);

    visuals.widgets.inactive.bg_fill = Color32::from_rgb(0xca, 0xcb, 0xd0);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0xca, 0xcb, 0xd0);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xb0, 0xb1, 0xb6));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, tokyo::FG_LIGHT);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0xc0, 0xc4, 0xd0);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0xc0, 0xc4, 0xd0);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, light_accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, tokyo::FG_LIGHT);

    visuals.widgets.active.bg_fill = light_accent;
    visuals.widgets.active.weak_bg_fill = light_accent;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x1e, 0x40, 0x80));
    visuals.widgets.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);

    visuals.widgets.open.bg_fill = Color32::from_rgb(0xc8, 0xcc, 0xda);
    visuals.widgets.open.weak_bg_fill = Color32::from_rgb(0xc8, 0xcc, 0xda);
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, light_accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, tokyo::FG_LIGHT);

    // Window stroke
    visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(0xb0, 0xb1, 0xb6));

    visuals
}
