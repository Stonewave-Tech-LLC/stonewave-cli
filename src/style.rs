//! Colours and text styling.
//!
//! Colour follows the Stonewave style: cyan for actions and links, green for
//! done, amber for things to confirm, grey for secondary text. Everything is
//! written as plain ANSI escapes, and nothing at all in [`ColorMode::None`]
//! (`NO_COLOR`, `TERM=dumb`, output that is not a terminal).

use crossterm::Command;
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorMode {
    /// No escapes at all.
    None,
    /// 256-colour palette, for terminals that do not announce true colour.
    Ansi256,
    /// 24-bit colour.
    TrueColor,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn hex(v: u32) -> Rgb {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// Linear mix: `t = 0` is `self`, `t = 1` is `other`.
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let f = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb(f(self.0, other.0), f(self.1, other.1), f(self.2, other.2))
    }
}

/// The palette. Cyan values are the Stonewave brand accent.
pub mod pal {
    use super::Rgb;

    /// Top of the banner gradient.
    pub const SKY: Rgb = Rgb::hex(0xC4F1FF);
    /// Stonewave accent (`--accent`).
    pub const CYAN: Rgb = Rgb::hex(0x00C2EE);
    /// Deeper accent (`--accent-deep`), bottom of the banner gradient.
    pub const DEEP: Rgb = Rgb::hex(0x0096C7);
    /// The bright crest of the wave in the banner animation.
    pub const FOAM: Rgb = Rgb::hex(0xF4FDFF);
    pub const GREEN: Rgb = Rgb::hex(0x3CCB7F);
    pub const AMBER: Rgb = Rgb::hex(0xF5B82E);
    pub const RED: Rgb = Rgb::hex(0xF0605D);
    /// Secondary text.
    pub const GREY: Rgb = Rgb::hex(0x7D8896);
    /// Things that have not happened yet.
    pub const DIM: Rgb = Rgb::hex(0x4D5866);
}

/// Renders a crossterm command as its ANSI escape string.
pub fn ansi(cmd: impl Command) -> String {
    let mut s = String::new();
    // Writing into a String cannot fail.
    let _ = cmd.write_ansi(&mut s);
    s
}

/// Nearest entry of the 6x6x6 colour cube of the 256-colour palette.
pub fn to_256(c: Rgb) -> u8 {
    let q = |v: u8| -> u8 {
        match v {
            0..48 => 0,
            48..115 => 1,
            _ => (v - 35) / 40,
        }
    };
    16 + 36 * q(c.0) + 6 * q(c.1) + q(c.2)
}

impl ColorMode {
    pub fn color(self, c: Rgb) -> Option<Color> {
        match self {
            ColorMode::None => None,
            ColorMode::TrueColor => Some(Color::Rgb {
                r: c.0,
                g: c.1,
                b: c.2,
            }),
            ColorMode::Ansi256 => Some(Color::AnsiValue(to_256(c))),
        }
    }

    /// `s` in colour `c`.
    pub fn fg(self, c: Rgb, s: &str) -> String {
        match self.color(c) {
            None => s.to_owned(),
            Some(col) => format!("{}{s}{}", ansi(SetForegroundColor(col)), ansi(ResetColor)),
        }
    }

    /// `s` in bold, in the terminal's own text colour (readable on light and dark themes).
    pub fn bold(self, s: &str) -> String {
        match self {
            ColorMode::None => s.to_owned(),
            _ => format!(
                "{}{s}{}",
                ansi(SetAttribute(Attribute::Bold)),
                ansi(SetAttribute(Attribute::NormalIntensity))
            ),
        }
    }

    /// `s` in bold and colour `c`.
    pub fn fg_bold(self, c: Rgb, s: &str) -> String {
        match self.color(c) {
            None => s.to_owned(),
            Some(col) => format!(
                "{}{}{s}{}",
                ansi(SetAttribute(Attribute::Bold)),
                ansi(SetForegroundColor(col)),
                ansi(ResetColor)
            ),
        }
    }

    /// `s` underlined in colour `c`: links.
    pub fn link(self, c: Rgb, s: &str) -> String {
        match self.color(c) {
            None => s.to_owned(),
            Some(col) => format!(
                "{}{}{s}{}",
                ansi(SetAttribute(Attribute::Underlined)),
                ansi(SetForegroundColor(col)),
                ansi(ResetColor)
            ),
        }
    }

    /// One character with a foreground and an optional background colour.
    pub fn cell(self, out: &mut String, ch: char, fg: Rgb, bg: Option<Rgb>) {
        let Some(fg) = self.color(fg) else {
            out.push(ch);
            return;
        };
        out.push_str(&ansi(SetForegroundColor(fg)));
        if let Some(bg) = bg.and_then(|b| self.color(b)) {
            out.push_str(&ansi(SetBackgroundColor(bg)));
        }
        out.push(ch);
        out.push_str(&ansi(ResetColor));
    }
}

/// Display width of a string, counting each `char` as one column.
///
/// Escapes are not counted. All characters this program prints are one
/// column wide, so this is exact here (it is not a general-purpose width).
pub fn width(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI sequence: ESC [ ... final byte in @..~
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_corners() {
        assert_eq!(to_256(Rgb(0, 0, 0)), 16);
        assert_eq!(to_256(Rgb(255, 255, 255)), 231);
        // 0xC2 = 194 is nearer cube level 175 (index 3) than 215.
        assert_eq!(to_256(Rgb(0, 0xC2, 0xEE)), 16 + 6 * 3 + 5);
    }

    #[test]
    fn mix_ends() {
        let a = Rgb(0, 100, 200);
        let b = Rgb(200, 100, 0);
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        assert_eq!(a.mix(b, 0.5), Rgb(100, 100, 100));
    }

    #[test]
    fn width_skips_escapes() {
        let s = ColorMode::TrueColor.fg_bold(pal::CYAN, "Code");
        assert_eq!(width(&s), 4);
        assert_eq!(width(&ColorMode::None.fg(pal::CYAN, "✓ ok")), 4);
    }

    #[test]
    fn no_color_means_no_escapes() {
        let m = ColorMode::None;
        for s in [
            m.fg(pal::GREEN, "a"),
            m.bold("a"),
            m.fg_bold(pal::AMBER, "a"),
            m.link(pal::CYAN, "a"),
        ] {
            assert_eq!(s, "a");
        }
        let mut out = String::new();
        m.cell(&mut out, '▀', pal::SKY, Some(pal::DEEP));
        assert_eq!(out, "▀");
    }
}
