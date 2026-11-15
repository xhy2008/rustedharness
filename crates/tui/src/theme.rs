//! Palette for the full-screen TUI.
//!
//! Deliberately low-chroma and quiet, in the spirit of Pi: the transcript is
//! carried by two shades of text and a lot of empty space, borders are almost
//! invisible, and colour is reserved for state that the eye must find
//! instantly (ok / failed tool call, context pressure, cost). Nothing here
//! paints large areas except the user-message band and inline code, and both
//! stay within a few points of the terminal background.

use ratatui::style::Color;

/// Primary reading colour for prose.
pub const TEXT: Color = Color::Rgb(0xe4, 0xe5, 0xea);
/// Secondary text: tool bodies, notices, list rows.
pub const MUTED: Color = Color::Rgb(0xa6, 0xa9, 0xb2);
/// Metadata that should be legible but never compete with content.
pub const DIM: Color = Color::Rgb(0x77, 0x7a, 0x84);
/// Rules, gutters, hints. Barely there on purpose.
pub const FAINT: Color = Color::Rgb(0x4b, 0x4e, 0x57);

pub const ACCENT: Color = Color::Rgb(0xb2, 0x9c, 0xe6);
pub const BLUE: Color = Color::Rgb(0x93, 0xb0, 0xe6);
pub const GREEN: Color = Color::Rgb(0x84, 0xcf, 0xac);
pub const RED: Color = Color::Rgb(0xe8, 0x8d, 0x80);
pub const YELLOW: Color = Color::Rgb(0xdf, 0xb8, 0x77);

/// Frame colour for the editor box, overlays and cards.
pub const BORDER: Color = Color::Rgb(0x3d, 0x40, 0x49);
/// Frame colour when the surface is not focused.
pub const BORDER_DIM: Color = Color::Rgb(0x2e, 0x31, 0x38);

/// Band behind the echoed user message.
pub const USER_BG: Color = Color::Rgb(0x26, 0x28, 0x2f);
/// Inline code and fenced blocks inside assistant prose.
pub const CODE_BG: Color = Color::Rgb(0x21, 0x23, 0x29);
/// Opaque backdrop for modal overlays so the transcript never bleeds through.
pub const BG_BASE: Color = Color::Rgb(0x1b, 0x1c, 0x22);
/// Background of the selected row in pickers and overlays.
pub const SELECTED_BG: Color = Color::Rgb(0x2c, 0x2f, 0x3a);
