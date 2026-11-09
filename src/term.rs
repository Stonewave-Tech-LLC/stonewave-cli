//! The terminal: what it can do, buffered output, waiting and key input.
//!
//! Output is buffered and written once per frame, so animations do not
//! flicker. In live mode the program redraws in place with cursor moves; in
//! plain mode (output piped, `TERM=dumb`) it only ever appends lines.
//!
//! When the keyboard is used, the terminal is put into raw mode so that a
//! single key answers a prompt and Ctrl-C arrives as a key instead of killing
//! the process with a hidden cursor. [`Term`] restores everything on drop,
//! also during a panic.

use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveUp, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::ResetColor;
use crossterm::terminal::{self, Clear, ClearType};

use crate::style::{ColorMode, ansi, width};

/// Options from the command line that shape the terminal.
#[derive(Default, Debug)]
pub struct Options {
    /// `--no-anim`: draw final states only.
    pub no_anim: bool,
    /// `--autoplay`: answer prompts by itself (for recordings).
    pub autoplay: bool,
    /// `--live`: draw in place even when stdout is not a terminal (for recordings).
    pub force_live: bool,
    /// `--record <file>`: also write an asciicast v2 file.
    pub record: Option<PathBuf>,
    /// `--width <cols>`: width to assume when stdout is not a terminal.
    pub width: Option<u16>,
}

#[derive(Debug)]
pub enum Error {
    /// The person pressed Ctrl-C, Ctrl-D or q.
    Interrupted,
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where answers to prompts come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    /// A person at the keyboard.
    Keyboard,
    /// The program answers by itself after a pause (recordings).
    Autoplay,
    /// No terminal to ask. Prompts answer no: a security question is never
    /// confirmed without a person.
    None,
}

/// What the terminal can do, decided once at start.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub color: ColorMode,
    /// Redraw in place with cursor moves.
    pub live: bool,
    /// Play animations (needs `live` and colour).
    pub animate: bool,
    pub input: Input,
    /// Keys can be read from the terminal (raw mode), also for Ctrl-C during autoplay.
    pub keys: bool,
    pub width: u16,
    pub height: u16,
}

/// Below this many rows a redraw region could scroll out of reach of the
/// cursor (it cannot move above the top of the screen), so output is plain.
const MIN_LIVE_ROWS: u16 = 16;

/// A key press, reduced to what the prompts need.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Yes,
    No,
    Enter,
    Other,
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn env_is(name: &str, value: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v.eq_ignore_ascii_case(value))
}

impl Caps {
    fn detect(opts: &Options) -> Caps {
        let out_tty = io::stdout().is_terminal();
        let in_tty = io::stdin().is_terminal();
        let dumb = env_is("TERM", "dumb");
        let (width, height) = if out_tty {
            terminal::size().unwrap_or((80, 24))
        } else {
            // Recordings: tall enough that nothing scrolls.
            (opts.width.unwrap_or(80), 60)
        };
        let live = (out_tty && !dumb && height >= MIN_LIVE_ROWS) || opts.force_live;

        // https://no-color.org: present and not empty means no colour.
        let color = if env_set("NO_COLOR") || dumb || !live {
            ColorMode::None
        } else if (opts.force_live && !out_tty)
            || env_is("COLORTERM", "truecolor")
            || env_is("COLORTERM", "24bit")
            || env_set("WT_SESSION")
        {
            ColorMode::TrueColor
        } else {
            ColorMode::Ansi256
        };

        let animate =
            live && color != ColorMode::None && !opts.no_anim && !env_set("STONEWAVE_NO_ANIM");

        // On Unix crossterm reads keys from /dev/tty when stdin is redirected
        // (as in `curl ... | sh`); the Windows console needs stdin itself.
        let keys = out_tty && !dumb && (in_tty || cfg!(unix));
        let input = if opts.autoplay {
            Input::Autoplay
        } else if keys {
            Input::Keyboard
        } else {
            Input::None
        };

        Caps {
            color,
            live,
            animate,
            input,
            keys,
            width,
            height,
        }
    }
}

/// Writes an asciicast v2 recording of everything that reaches the screen.
struct Recorder {
    file: BufWriter<File>,
    started: Instant,
}

impl Recorder {
    fn create(path: &PathBuf, width: u16, height: u16) -> io::Result<Recorder> {
        let mut file = BufWriter::new(File::create(path)?);
        writeln!(
            file,
            r#"{{"version": 2, "width": {width}, "height": {height}, "env": {{"TERM": "xterm-256color"}}}}"#
        )?;
        Ok(Recorder {
            file,
            started: Instant::now(),
        })
    }

    fn event(&mut self, data: &str) -> io::Result<()> {
        let t = self.started.elapsed().as_secs_f64();
        writeln!(self.file, "[{t:.6}, \"o\", \"{}\"]", json_escape(data))
    }
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub struct Term {
    pub caps: Caps,
    out: io::Stdout,
    buf: String,
    /// Screen rows written since start, minus rows moved back up. Used to redraw
    /// regions. Wrapped rows count, so long lines on narrow terminals still redraw in place.
    lines: usize,
    /// Columns written on the current line.
    col: usize,
    raw: bool,
    cursor_hidden: bool,
    rec: Option<Recorder>,
}

impl Term {
    pub fn open(opts: &Options) -> io::Result<Term> {
        let caps = Caps::detect(opts);
        let mut out = io::stdout();
        if caps.live && out.is_terminal() {
            // Running one command through crossterm turns on ANSI support in
            // the Windows console. On other systems this only writes a reset.
            crossterm::execute!(out, ResetColor)?;
        }
        // Everything that can fail comes before raw mode: once raw mode is on,
        // only a constructed `Term` (and its Drop) turns it off again.
        let rec = match &opts.record {
            Some(path) => Some(Recorder::create(path, caps.width, caps.height)?),
            None => None,
        };
        let raw = caps.keys;
        if raw {
            terminal::enable_raw_mode()?;
        }
        Ok(Term {
            caps,
            out,
            buf: String::new(),
            lines: 0,
            col: 0,
            raw,
            cursor_hidden: false,
            rec,
        })
    }

    pub fn mode(&self) -> ColorMode {
        self.caps.color
    }

    pub fn write(&mut self, s: &str) {
        self.buf.push_str(s);
        self.col += width(s);
    }

    pub fn newline(&mut self) {
        // Raw mode turns off the terminal's own carriage return on Unix.
        self.buf
            .push_str(if self.caps.live { "\r\n" } else { "\n" });
        self.lines += rows(self.col, usize::from(self.caps.width));
        self.col = 0;
    }

    pub fn line(&mut self, s: &str) {
        self.write(s);
        self.newline();
    }

    pub fn blank(&mut self) {
        self.newline();
    }

    /// Position for [`Term::repaint`] and [`Term::erase_since`].
    pub fn mark(&self) -> usize {
        self.lines
    }

    fn up(&mut self, n: usize) {
        if !self.caps.live || n == 0 {
            return;
        }
        // `ESC [ 0 A` would move one line, so n == 0 is handled above.
        let n16 = u16::try_from(n).unwrap_or(u16::MAX);
        self.buf.push('\r');
        self.buf.push_str(&ansi(MoveUp(n16)));
        self.lines = self.lines.saturating_sub(n);
    }

    /// Replaces everything drawn since `mark` with `lines` (live mode).
    /// In plain mode it only appends, so callers draw plain output once.
    pub fn repaint(&mut self, mark: usize, lines: &[String]) {
        let before = self.lines.saturating_sub(mark);
        self.up(before);
        for l in lines {
            self.write(l);
            if self.caps.live {
                self.buf.push_str(&ansi(Clear(ClearType::UntilNewLine)));
            }
            self.newline();
        }
        if self.caps.live && lines.len() < before {
            self.buf.push_str(&ansi(Clear(ClearType::FromCursorDown)));
        }
    }

    /// Removes everything drawn since `mark` (live mode only).
    pub fn erase_since(&mut self, mark: usize) {
        if !self.caps.live {
            return;
        }
        let n = self.lines.saturating_sub(mark);
        self.up(n);
        self.buf.push_str(&ansi(Clear(ClearType::FromCursorDown)));
    }

    pub fn hide_cursor(&mut self) {
        if self.caps.live && !self.cursor_hidden {
            self.buf.push_str(&ansi(Hide));
            self.cursor_hidden = true;
        }
    }

    pub fn show_cursor(&mut self) {
        if self.cursor_hidden {
            self.buf.push_str(&ansi(Show));
            self.cursor_hidden = false;
        }
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.out.write_all(self.buf.as_bytes())?;
        self.out.flush()?;
        if let Some(rec) = &mut self.rec {
            rec.event(&self.buf)?;
        }
        self.buf.clear();
        Ok(())
    }

    /// Flushes, then waits up to `d`. Returns early with the key if one is pressed.
    ///
    /// Without a keyboard it just sleeps (live) or returns at once (plain:
    /// there is nothing to watch, so the demo does not pretend to work).
    pub fn wait(&mut self, d: Duration) -> Result<Option<Key>> {
        self.flush()?;
        match self.caps.input {
            Input::Keyboard => self.poll_key(d),
            Input::Autoplay if self.raw => {
                self.wait_interruptible(d)?;
                Ok(None)
            }
            Input::Autoplay => {
                std::thread::sleep(d);
                Ok(None)
            }
            Input::None => {
                if self.caps.live {
                    std::thread::sleep(d);
                }
                Ok(None)
            }
        }
    }

    /// Waits `d` but still reacts to Ctrl-C and q; other keys are ignored.
    fn wait_interruptible(&mut self, d: Duration) -> Result<()> {
        let deadline = Instant::now() + d;
        while self
            .poll_key(deadline.saturating_duration_since(Instant::now()))?
            .is_some()
        {}
        Ok(())
    }

    fn poll_key(&mut self, d: Duration) -> Result<Option<Key>> {
        let deadline = Instant::now() + d;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if !event::poll(left)? {
                return Ok(None);
            }
            let Event::Key(k) = event::read()? else {
                continue;
            };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            return match k.code {
                KeyCode::Char('c' | 'd') if ctrl => Err(Error::Interrupted),
                KeyCode::Char('q') => Err(Error::Interrupted),
                KeyCode::Char('y' | 'Y' | 'j' | 'J') => Ok(Some(Key::Yes)),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => Ok(Some(Key::No)),
                KeyCode::Enter => Ok(Some(Key::Enter)),
                _ => Ok(Some(Key::Other)),
            };
        }
    }

    /// Waits for y or n. There is no default: Enter alone does not confirm.
    ///
    /// Keys pressed before the question was shown are dropped first, so a `y`
    /// typed ahead cannot confirm something the person has not read. Without a
    /// terminal to ask, the answer is no.
    pub fn ask_yes_no(&mut self) -> Result<bool> {
        self.flush()?;
        match self.caps.input {
            Input::Keyboard => {
                // Drain type-ahead (Ctrl-C and q still stop).
                while self.poll_key(Duration::ZERO)?.is_some() {}
                loop {
                    match self.poll_key(Duration::from_secs(3600))? {
                        Some(Key::Yes) => return Ok(true),
                        Some(Key::No) => return Ok(false),
                        _ => {}
                    }
                }
            }
            Input::Autoplay => {
                self.wait(Duration::from_millis(1600))?;
                Ok(true)
            }
            Input::None => Ok(false),
        }
    }
}

/// Screen rows a line of `cols` columns takes in a terminal `width` wide.
/// A line exactly as wide as the terminal still takes one row.
fn rows(cols: usize, width: usize) -> usize {
    if width == 0 {
        1
    } else {
        cols.div_ceil(width).max(1)
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        if self.caps.live && self.caps.color != ColorMode::None {
            self.buf.push_str(&ansi(ResetColor));
        }
        self.show_cursor();
        let _ = self.flush();
        if self.raw {
            let _ = terminal::disable_raw_mode();
        }
        if let Some(rec) = &mut self.rec {
            let _ = rec.file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{json_escape, rows};

    #[test]
    fn wrapped_lines_count_as_rows() {
        assert_eq!(rows(0, 80), 1);
        assert_eq!(rows(80, 80), 1);
        assert_eq!(rows(81, 80), 2);
        assert_eq!(rows(72, 60), 2);
        assert_eq!(rows(10, 0), 1);
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(
            json_escape("a\"b\\c\r\n\x1b[1m"),
            "a\\\"b\\\\c\\r\\n\\u001b[1m"
        );
    }
}
