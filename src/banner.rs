//! The STONEWAVE banner and its one-time wave animation.
//!
//! The word is drawn from a 5x7 pixel font. Two pixel rows share one text row
//! through the half-block characters `▀` and `▄`, so pixels come out roughly
//! square and every pixel row gets its own shade of the cyan gradient.
//!
//! The animation: a wave front runs once from left to right. Letters appear
//! as it passes, the columns at the crest lift by one pixel and light up in
//! foam white, and the wave line underneath fills in with it. About 1.2 s;
//! any key skips to the end.

use std::time::{Duration, Instant};

use crate::VERSION;
use crate::style::{ColorMode, Rgb, pal};
use crate::term::{Result, Term};
use crate::ui::MARGIN;

const GLYPH_W: usize = 5;
const GLYPH_H: usize = 7;
const WORD: [char; 9] = ['S', 'T', 'O', 'N', 'E', 'W', 'A', 'V', 'E'];

/// Banner width in columns.
pub const WIDTH: usize = WORD.len() * GLYPH_W + (WORD.len() - 1);
/// Pixel rows on the canvas: the 7 font rows plus one row of headroom for the lift.
const CANVAS_H: usize = GLYPH_H + 1;
/// Text rows of the word.
const ROWS: usize = CANVAS_H / 2;

const SWEEP: Duration = Duration::from_millis(1250);
const FRAME: Duration = Duration::from_millis(16);

#[rustfmt::skip]
fn glyph(c: char) -> [&'static str; GLYPH_H] {
    match c {
        'S' => [".####", "#....", "#....", ".###.", "....#", "....#", "####."],
        'T' => ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
        'O' => [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        'N' => ["#...#", "##..#", "##..#", "#.#.#", "#..##", "#..##", "#...#"],
        'E' => ["#####", "#....", "#....", "####.", "#....", "#....", "#####"],
        'W' => ["#...#", "#...#", "#...#", "#.#.#", "#.#.#", "##.##", "#...#"],
        'A' => [".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
        'V' => ["#...#", "#...#", "#...#", "#...#", ".#.#.", ".#.#.", "..#.."],
        _ => ["....."; GLYPH_H],
    }
}

/// The word as pixels: `bits[y][x]`.
fn bitmap() -> [[bool; WIDTH]; GLYPH_H] {
    let mut bits = [[false; WIDTH]; GLYPH_H];
    for (i, &c) in WORD.iter().enumerate() {
        let x0 = i * (GLYPH_W + 1);
        for (y, row) in glyph(c).iter().enumerate() {
            for (dx, b) in row.bytes().enumerate() {
                bits[y][x0 + dx] = b == b'#';
            }
        }
    }
    bits
}

/// Resting colour of a pixel row: light sky at the top, deep cyan at the bottom.
fn shade(y: usize) -> Rgb {
    let t = y as f32 / (GLYPH_H - 1) as f32;
    if t < 0.5 {
        pal::SKY.mix(pal::CYAN, t * 2.0)
    } else {
        pal::CYAN.mix(pal::DEEP, (t - 0.5) * 2.0)
    }
}

/// How column `x` looks while the crest is at `crest`: hidden, or lifted and how bright.
struct Column {
    visible: bool,
    lift: usize,
    glow: f32,
}

fn column(x: usize, crest: Option<f32>) -> Column {
    let Some(crest) = crest else {
        return Column {
            visible: true,
            lift: 0,
            glow: 0.0,
        };
    };
    let d = x as f32 - crest;
    Column {
        visible: d <= 0.0,
        lift: usize::from(d > -3.0),
        glow: (-(d * d) / 10.0).exp(),
    }
}

/// One frame: the word (`ROWS` lines) and the wave line under it.
/// `crest = None` draws the banner at rest.
pub fn frame(mode: ColorMode, crest: Option<f32>) -> Vec<String> {
    let bits = bitmap();
    let cols: Vec<Column> = (0..WIDTH).map(|x| column(x, crest)).collect();

    // canvas[r][x]: colour of pixel row r (0..CANVAS_H), or None.
    let mut canvas = [[None::<Rgb>; WIDTH]; CANVAS_H];
    for (x, col) in cols.iter().enumerate() {
        if !col.visible {
            continue;
        }
        for (y, row) in bits.iter().enumerate() {
            if row[x] {
                // At rest the word sits one pixel down; a lifted column sits at the top.
                let r = y + 1 - col.lift;
                canvas[r][x] = Some(shade(y).mix(pal::FOAM, col.glow * 0.9));
            }
        }
    }

    let mut lines = Vec::with_capacity(ROWS + 1);
    for pair in canvas.chunks_exact(2) {
        let mut s = String::new();
        for (&top, &bottom) in pair[0].iter().zip(&pair[1]) {
            match (top, bottom) {
                (None, None) => s.push(' '),
                (Some(t), None) => mode.cell(&mut s, '▀', t, None),
                (None, Some(b)) => mode.cell(&mut s, '▄', b, None),
                (Some(t), Some(b)) => {
                    if mode == ColorMode::None {
                        s.push('█');
                    } else {
                        mode.cell(&mut s, '▀', t, Some(b));
                    }
                }
            }
        }
        lines.push(s);
    }

    let mut wave = String::new();
    for (x, col) in cols.iter().enumerate() {
        if !col.visible {
            wave.push(' ');
            continue;
        }
        let base = pal::DEEP.mix(pal::CYAN, x as f32 / WIDTH as f32);
        mode.cell(&mut wave, '≈', base.mix(pal::FOAM, col.glow * 0.8), None);
    }
    lines.push(wave);
    lines
}

/// "server setup for your dev agent" and the version, right-aligned under the banner.
fn subtitle(mode: ColorMode) -> String {
    let left = "server setup for your dev agent";
    let right = format!("v{VERSION}");
    let gap = WIDTH.saturating_sub(left.len() + right.len()).max(1);
    mode.fg(pal::GREY, &format!("{left}{}{right}", " ".repeat(gap)))
}

/// Whether the big banner fits next to the margin.
pub fn fits(term: &Term) -> bool {
    usize::from(term.caps.width) >= MARGIN.len() + WIDTH + 2
}

/// One line for narrow terminals and for every start after the first.
pub fn compact(mode: ColorMode, context: &str) -> String {
    let mut s = format!(
        "{MARGIN}{} {}  {}",
        mode.fg_bold(pal::CYAN, "≈"),
        mode.bold("stonewave"),
        mode.fg(pal::GREY, &format!("v{VERSION}"))
    );
    if !context.is_empty() {
        s.push_str(&mode.fg(pal::GREY, &format!(" · {context}")));
    }
    s
}

fn with_margin(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().map(|l| format!("{MARGIN}{l}")).collect()
}

/// Shows the banner, animated when the terminal allows it.
pub fn show(term: &mut Term) -> Result<()> {
    let mode = term.mode();
    if !fits(term) {
        term.line(&compact(mode, "server setup for your dev agent"));
        return Ok(());
    }

    term.blank();
    if term.caps.animate {
        term.hide_cursor();
        let mark = term.mark();
        let start = Instant::now();
        loop {
            let t = start.elapsed().as_secs_f32() / SWEEP.as_secs_f32();
            if t >= 1.0 {
                break;
            }
            // Ease out: fast in, settling at the end. The front runs past the
            // last column so the glow fades out instead of stopping on a letter.
            let eased = 1.0 - (1.0 - t) * (1.0 - t);
            let crest = eased * (WIDTH as f32 + 10.0) - 2.0;
            term.repaint(mark, &with_margin(frame(mode, Some(crest))));
            if term.wait(FRAME)?.is_some() {
                // Any key skips the rest of the animation.
                break;
            }
        }
        term.repaint(mark, &with_margin(frame(mode, None)));
    } else {
        for l in with_margin(frame(mode, None)) {
            term.line(&l);
        }
    }
    term.line(&format!("{MARGIN}{}", subtitle(mode)));
    term.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::width;

    #[test]
    fn glyphs_are_complete() {
        for c in WORD {
            for row in glyph(c) {
                assert_eq!(row.len(), GLYPH_W, "glyph {c}");
                assert!(row.bytes().all(|b| b == b'#' || b == b'.'), "glyph {c}");
            }
            assert!(
                glyph(c).iter().any(|r| r.contains('#')),
                "glyph {c} is empty"
            );
        }
    }

    #[test]
    fn resting_frame_has_fixed_width() {
        let lines = frame(ColorMode::None, None);
        assert_eq!(lines.len(), ROWS + 1);
        for l in &lines {
            assert_eq!(width(l), WIDTH);
        }
        // Coloured frames are the same width once escapes are skipped.
        for l in frame(ColorMode::TrueColor, Some(20.0)) {
            assert_eq!(width(&l), WIDTH);
        }
    }

    #[test]
    fn front_hides_what_it_has_not_reached() {
        let lines = frame(ColorMode::None, Some(-1.0));
        assert!(lines.iter().all(|l| l.trim().is_empty()));
        let full = frame(ColorMode::None, Some(WIDTH as f32 + 10.0));
        assert_eq!(full, frame(ColorMode::None, None));
    }

    #[test]
    fn subtitle_is_as_wide_as_the_banner() {
        assert_eq!(width(&subtitle(ColorMode::None)), WIDTH);
    }
}
