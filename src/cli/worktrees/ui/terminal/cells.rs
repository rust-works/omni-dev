//! Maps one emulator grid cell's colours and attributes onto a ratatui
//! [`Style`] (issue #1585 Phase 3).
//!
//! Colours resolve in this order: a palette entry the child has overridden
//! at runtime (OSC 4/10/11, tracked in the emulator's [`Colors`]) wins and
//! becomes an exact RGB; otherwise a named ANSI colour maps to the host
//! terminal's own palette slot (so the user's theme applies), an indexed
//! colour passes through as an index, and a truecolour spec passes through
//! as RGB. `Foreground`/`Background` map to the host default — never a
//! hardcoded black/white.

use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use ratatui::style::{Color, Modifier, Style};

/// The ratatui style for `cell`, honouring the emulator's live palette.
pub fn cell_style(cell: &Cell, colors: &Colors) -> Style {
    let mut style = Style::default()
        .fg(to_ratatui(cell.fg, colors))
        .bg(to_ratatui(cell.bg, colors));
    let flags = cell.flags;
    if flags.contains(Flags::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if flags.contains(Flags::DIM) {
        style = style.add_modifier(Modifier::DIM);
    }
    if flags.contains(Flags::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if flags.contains(Flags::STRIKEOUT) {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if flags.contains(Flags::HIDDEN) {
        style = style.add_modifier(Modifier::HIDDEN);
    }
    if flags.contains(Flags::INVERSE) {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// One emulator colour → one ratatui colour.
pub fn to_ratatui(color: AnsiColor, colors: &Colors) -> Color {
    match color {
        AnsiColor::Spec(rgb) => rgb_color(rgb),
        AnsiColor::Indexed(index) => match colors[usize::from(index)] {
            Some(rgb) => rgb_color(rgb),
            None => Color::Indexed(index),
        },
        AnsiColor::Named(named) => match colors[named] {
            Some(rgb) => rgb_color(rgb),
            None => named_color(named),
        },
    }
}

fn rgb_color(rgb: Rgb) -> Color {
    Color::Rgb(rgb.r, rgb.g, rgb.b)
}

/// A named ANSI colour → the host terminal's matching palette slot. The
/// `Dim*` variants map to their base colour (the `DIM` flag already carries
/// the dimming); the foreground/background/cursor defaults map to `Reset`
/// so the host's own defaults apply.
fn named_color(named: NamedColor) -> Color {
    match named {
        NamedColor::Black | NamedColor::DimBlack => Color::Black,
        NamedColor::Red | NamedColor::DimRed => Color::Red,
        NamedColor::Green | NamedColor::DimGreen => Color::Green,
        NamedColor::Yellow | NamedColor::DimYellow => Color::Yellow,
        NamedColor::Blue | NamedColor::DimBlue => Color::Blue,
        NamedColor::Magenta | NamedColor::DimMagenta => Color::Magenta,
        NamedColor::Cyan | NamedColor::DimCyan => Color::Cyan,
        NamedColor::White | NamedColor::DimWhite => Color::Gray,
        NamedColor::BrightBlack => Color::DarkGray,
        NamedColor::BrightRed => Color::LightRed,
        NamedColor::BrightGreen => Color::LightGreen,
        NamedColor::BrightYellow => Color::LightYellow,
        NamedColor::BrightBlue => Color::LightBlue,
        NamedColor::BrightMagenta => Color::LightMagenta,
        NamedColor::BrightCyan => Color::LightCyan,
        NamedColor::BrightWhite => Color::White,
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => Color::Reset,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_default_cell_maps_to_the_host_defaults_with_no_modifiers() {
        let style = cell_style(&Cell::default(), &Colors::default());
        assert_eq!(style.fg, Some(Color::Reset));
        assert_eq!(style.bg, Some(Color::Reset));
        assert!(style.add_modifier.is_empty());
    }

    #[test]
    fn named_colors_map_to_the_host_palette_slots() {
        let colors = Colors::default();
        assert_eq!(
            to_ratatui(AnsiColor::Named(NamedColor::Red), &colors),
            Color::Red
        );
        assert_eq!(
            to_ratatui(AnsiColor::Named(NamedColor::BrightBlue), &colors),
            Color::LightBlue
        );
        assert_eq!(
            to_ratatui(AnsiColor::Named(NamedColor::DimGreen), &colors),
            Color::Green
        );
    }

    #[test]
    fn indexed_and_spec_colors_pass_through() {
        let colors = Colors::default();
        assert_eq!(
            to_ratatui(AnsiColor::Indexed(42), &colors),
            Color::Indexed(42)
        );
        assert_eq!(
            to_ratatui(AnsiColor::Spec(Rgb { r: 1, g: 2, b: 3 }), &colors),
            Color::Rgb(1, 2, 3)
        );
    }

    #[test]
    fn a_runtime_palette_override_wins_as_exact_rgb() {
        let mut colors = Colors::default();
        colors[NamedColor::Red] = Some(Rgb {
            r: 200,
            g: 10,
            b: 10,
        });
        colors[7usize] = Some(Rgb { r: 9, g: 9, b: 9 });
        assert_eq!(
            to_ratatui(AnsiColor::Named(NamedColor::Red), &colors),
            Color::Rgb(200, 10, 10)
        );
        assert_eq!(
            to_ratatui(AnsiColor::Indexed(7), &colors),
            Color::Rgb(9, 9, 9)
        );
    }

    #[test]
    fn attribute_flags_become_modifiers() {
        let cell = Cell {
            flags: Flags::BOLD | Flags::ITALIC | Flags::UNDERCURL | Flags::INVERSE,
            ..Default::default()
        };
        let style = cell_style(&cell, &Colors::default());
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(style.add_modifier.contains(Modifier::REVERSED));
        assert!(!style.add_modifier.contains(Modifier::DIM));
    }
}
