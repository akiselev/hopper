//! The viewer's colours and metrics (VS Code's Dark Modern, which the chrome's theme also is).

use skelter_base::color::Color;

/// `0xRRGGBB` as an opaque colour.
pub const fn rgb(hex: u32) -> Color {
    Color::rgba(
        ((hex >> 16) & 0xFF) as f32 / 255.0,
        ((hex >> 8) & 0xFF) as f32 / 255.0,
        (hex & 0xFF) as f32 / 255.0,
        1.0,
    )
}

pub const fn rgba(hex: u32, a: f32) -> Color {
    let c = rgb(hex);
    Color::rgba(c.r, c.g, c.b, a)
}

pub const BG: Color = rgb(0x1F1F1F);
pub const FG: Color = rgb(0xCCCCCC);
pub const MUTED: Color = rgb(0x7A7A7A);
pub const ACCENT: Color = rgb(0x4FC1FF);
pub const WARNING: Color = rgb(0xCCA700);
pub const ERROR: Color = rgb(0xF48771);
pub const HEADING: Color = rgb(0xE0E0E0);
pub const SELECTION: Color = rgb(0x04395E);
pub const SELECTION_FOCUSED: Color = rgb(0x0F4A85);
pub const ROW_STRIPE: Color = rgba(0xFFFFFF, 0.025);
pub const DISCLOSURE: Color = rgb(0xC5C5C5);
pub const BORDER: Color = rgb(0x2B2B2B);
/// The stopped line's band in the source.
pub const EXEC_LINE: Color = rgba(0xFFCC00, 0.16);
pub const EXEC_ARROW: Color = rgb(0xFFCC00);
pub const FRAME_LINE: Color = rgba(0x4EC9B0, 0.14);
pub const FRAME_ARROW: Color = rgb(0x4EC9B0);
pub const GUTTER_BG: Color = rgb(0x1A1A1A);
pub const HIT_COUNT: Color = rgb(0x858585);

/// The UI font and the code font, as CSS family lists.
pub const UI_FONT: &str = "Inter, Noto Sans, DejaVu Sans, sans-serif";
pub const MONO_FONT: &str = "JetBrains Mono, DejaVu Sans Mono, Liberation Mono, monospace";
