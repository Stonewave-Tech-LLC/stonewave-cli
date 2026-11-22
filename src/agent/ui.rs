//! What the agent commands show. The commands talk to a [`Ui`]; the real one draws with the terminal blocks of the other
//! screens (plain lines when output is not a terminal), the one in the tests records what was said.
//!
//! Every text passes through [`scrub`] on its way out, so a key can not be printed by accident.

use std::time::Duration;

use super::key::scrub;
use crate::connect::Pace;
use crate::flow;
use crate::style::pal;
use crate::term::{Input, Result, Term};
use crate::ui::{DETAIL, INDENT, kv};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Heading,
    Ok,
    Warn,
    Fail,
    Note,
    Kv,
    Blank,
}

pub trait Ui {
    fn emit(&mut self, kind: Kind, a: &str, b: &str);
    fn ask_raw(&mut self, question: &str) -> Result<bool>;
    /// Waits `d` with a "working" line showing `msg`. Fails with Ctrl-C.
    fn pause(&mut self, d: Duration, msg: &str) -> Result<()>;
    /// The clock and spinner for the sign-in wait.
    fn pace(&mut self) -> Box<dyn Pace + '_>;
    fn banner(&mut self) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
    /// A position to come back to: see [`Ui::finish_wait`].
    fn mark(&self) -> usize;
    /// After the sign-in wait: shows the cursor again and wipes what was drawn since `mark` (live terminals).
    fn finish_wait(&mut self, mark: usize);
    /// Is there a person who can answer a question?
    fn can_ask(&self) -> bool;

    fn heading(&mut self, text: &str) {
        self.emit(Kind::Heading, &scrub(text), "");
    }
    fn ok(&mut self, label: &str, detail: &str) {
        self.emit(Kind::Ok, &scrub(label), &scrub(detail));
    }
    fn warn(&mut self, text: &str) {
        self.emit(Kind::Warn, &scrub(text), "");
    }
    fn fail(&mut self, text: &str) {
        self.emit(Kind::Fail, &scrub(text), "");
    }
    fn note(&mut self, text: &str) {
        self.emit(Kind::Note, &scrub(text), "");
    }
    fn kv(&mut self, label: &str, value: &str) {
        self.emit(Kind::Kv, &scrub(label), &scrub(value));
    }
    fn blank(&mut self) {
        self.emit(Kind::Blank, "", "");
    }
    fn ask(&mut self, question: &str) -> Result<bool> {
        self.ask_raw(&scrub(question))
    }
}

/// The real thing: the terminal.
pub struct TermUi<'a> {
    pub term: &'a mut Term,
    last_pause: String,
}

impl<'a> TermUi<'a> {
    pub fn new(term: &'a mut Term) -> TermUi<'a> {
        TermUi {
            term,
            last_pause: String::new(),
        }
    }
}

impl Ui for TermUi<'_> {
    fn emit(&mut self, kind: Kind, a: &str, b: &str) {
        let m = self.term.mode();
        let line = match kind {
            Kind::Heading => format!("{INDENT}{} {}", m.fg(pal::CYAN, "●"), m.bold(a)),
            Kind::Ok => {
                let pad = " ".repeat(20usize.saturating_sub(a.chars().count()));
                format!(
                    "{INDENT}{} {}{pad}{}",
                    m.fg_bold(pal::GREEN, "✓"),
                    m.bold(a),
                    m.fg(pal::GREY, b)
                )
            }
            Kind::Warn => format!("{INDENT}{} {}", m.fg_bold(pal::AMBER, "◆"), a),
            Kind::Fail => format!("{INDENT}{} {}", m.fg_bold(pal::RED, "✗"), m.bold(a)),
            Kind::Note => format!("{DETAIL}{}", m.fg(pal::GREY, a)),
            Kind::Kv => kv(m, DETAIL, a, 9, b),
            Kind::Blank => {
                self.term.blank();
                return;
            }
        };
        self.term.line(&line);
    }

    fn ask_raw(&mut self, question: &str) -> Result<bool> {
        flow::ask(self.term, question)
    }

    fn pause(&mut self, d: Duration, msg: &str) -> Result<()> {
        let m = self.term.mode();
        if !self.term.caps.live {
            if self.last_pause != msg {
                self.term.line(&format!("{DETAIL}{msg}"));
                self.last_pause = msg.to_owned();
            }
            self.term.flush()?;
            std::thread::sleep(d);
            return Ok(());
        }
        let mark = self.term.mark();
        let started = std::time::Instant::now();
        self.term.hide_cursor();
        while started.elapsed() < d {
            let t = if self.term.caps.animate {
                started.elapsed().as_secs_f32()
            } else {
                0.0
            };
            let line = format!(
                "{DETAIL}{}  {}",
                crate::ui::wave_spinner(m, t, 5),
                m.fg(pal::GREY, msg)
            );
            self.term.repaint(mark, &[line]);
            self.term.wait(Duration::from_millis(50))?;
        }
        self.term.erase_since(mark);
        self.term.show_cursor();
        Ok(())
    }

    fn pace(&mut self) -> Box<dyn Pace + '_> {
        Box::new(flow::TermPace::new(self.term))
    }

    fn banner(&mut self) -> Result<()> {
        crate::banner::show(self.term)
    }

    fn flush(&mut self) -> Result<()> {
        self.term.flush()?;
        Ok(())
    }

    fn mark(&self) -> usize {
        self.term.mark()
    }

    fn finish_wait(&mut self, mark: usize) {
        self.term.show_cursor();
        self.term.erase_since(mark);
    }

    fn can_ask(&self) -> bool {
        self.term.caps.input != Input::None
    }
}
