//! A small, Pi-inspired dark palette for the full-screen TUI.
//!
//! Values approximate Pi's `dark` theme (OKHSL) in sRGB so the UI keeps the
//! same feel without pulling in a colour-management crate. The palette is
//! intentionally flat: borders are dim, muted text carries metadata, and the
//! accent is reserved for focus, the selected row and the user message block.

use ratatui::style::Color;

pub const TEXT: Color = Color::Rgb(0xe3, 0xe4, 0xe9);
pub const MUTED: Color = Color::Rgb(0xa4, 0xa6, 0xaf);
pub const DIM: Color = Color::Rgb(0x82, 0x84, 0x8e);
pub const THINKING: Color = Color::Rgb(0x9a, 0x9c, 0xa6);

pub const ACCENT: Color = Color::Rgb(0xc2, 0x8d, 0xd8);
pub const BLUE: Color = Color::Rgb(0x8e, 0xa9, 0xea);
pub const GREEN: Color = Color::Rgb(0x7f, 0xd8, 0xb0);
pub const RED: Color = Color::Rgb(0xf0, 0x94, 0x7a);
pub const YELLOW: Color = Color::Rgb(0xcf, 0xe8, 0x7a);

pub const BORDER: Color = Color::Rgb(0x84, 0x9c, 0xd8);
pub const BORDER_DIM: Color = Color::Rgb(0x55, 0x57, 0x5f);

pub const USER_BG: Color = Color::Rgb(0x23, 0x2c, 0x55);
pub const TOOL_BG: Color = Color::Rgb(0x32, 0x33, 0x38);
pub const TOOL_OK_BG: Color = Color::Rgb(0x22, 0x40, 0x36);
pub const TOOL_ERR_BG: Color = Color::Rgb(0x46, 0x2a, 0x22);

/// Background used for the selected row in pickers and overlays.
pub const SELECTED_BG: Color = Color::Rgb(0x2a, 0x34, 0x60);
