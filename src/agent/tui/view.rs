//! How each screen of the menu looks: the model in, lines out. Text is German (the people who use the menu read it), the palette is the
//! banner's. Every line is clipped to the width of the terminal before it is coloured, so nothing wraps by accident.

use crate::agent::dirs::Os;
use crate::agent::engine::Engine;
use crate::agent::key::is_invisible;
use crate::banner;
use crate::style::{ColorMode, pal};
use crate::ui::{MARGIN, wave_spinner};

use super::model::{
    Add, AddStep, AgentRow, App, Confirm, KINDS, Kind, Level, Mode, Screen, Snapshot, Step, Wizard,
    agent_actions, connect_engines, engine_choosable, engine_note, engine_rows, install_hint,
};
use super::service::{self, ServiceState};

/// Below this the menu does not draw (it says so instead).
pub const MIN_WIDTH: usize = 50;
pub const MIN_HEIGHT: usize = 16;
/// The big banner needs this many rows on top of the content.
const BIG_BANNER_MIN_HEIGHT: usize = 30;

// ------------------------------------------------------------------ small helpers

/// Cuts a plain text to `n` columns, with an ellipsis.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Words of a text broken into lines of at most `n` columns.
pub fn wrap(text: &str, n: usize) -> Vec<String> {
    let n = n.max(8);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            let mut word = word.to_owned();
            while word.chars().count() > n {
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                }
                let head: String = word.chars().take(n).collect();
                lines.push(head);
                word = word.chars().skip(n).collect();
            }
            if cur.is_empty() {
                cur = word;
            } else if cur.chars().count() + 1 + word.chars().count() <= n {
                cur.push(' ');
                cur.push_str(&word);
            } else {
                lines.push(std::mem::replace(&mut cur, word));
            }
        }
        lines.push(cur);
    }
    lines
}

struct P {
    m: ColorMode,
    /// Columns a line of content may use (after the margin).
    w: usize,
}

impl P {
    fn heading(&self, s: &str) -> String {
        format!(
            "{} {}",
            self.m.fg(pal::CYAN, "●"),
            self.m.bold(&clip(s, self.w - 2))
        )
    }

    fn grey(&self, s: &str) -> String {
        self.m.fg(pal::GREY, &clip(s, self.w))
    }

    fn dim(&self, s: &str) -> String {
        self.m.fg(pal::DIM, &clip(s, self.w))
    }

    fn plain(&self, s: &str) -> String {
        clip(s, self.w)
    }

    fn level(&self, l: Level, s: &str) -> String {
        let (mark, c) = match l {
            Level::Ok => ("✓", pal::GREEN),
            Level::Warn => ("◆", pal::AMBER),
            Level::Bad => ("✗", pal::RED),
        };
        format!("{} {}", self.m.fg_bold(c, mark), clip(s, self.w - 2))
    }

    /// A row of a list; `on` marks the cursor.
    fn row(&self, on: bool, text: &str) -> String {
        if on {
            format!(
                "{} {}",
                self.m.fg_bold(pal::CYAN, "›"),
                self.m.fg_bold(pal::CYAN, &clip(text, self.w - 2))
            )
        } else {
            format!("  {}", clip(text, self.w - 2))
        }
    }

    fn row_off(&self, text: &str) -> String {
        format!("  {}", self.m.fg(pal::DIM, &clip(text, self.w - 2)))
    }

    fn paragraph(&self, text: &str) -> Vec<String> {
        wrap(text, self.w).iter().map(|l| self.plain(l)).collect()
    }

    fn note(&self, text: &str) -> Vec<String> {
        wrap(text, self.w).iter().map(|l| self.grey(l)).collect()
    }

    /// A text field: the label, then the text with a block at the cursor when the field has the focus.
    fn field(&self, label: &str, input: &super::model::Input, focus: bool) -> String {
        let label_w = label.chars().count() + 2;
        let room = self.w.saturating_sub(label_w + 1).max(8);
        // show the end of a long text, where the cursor is
        let chars = input.chars();
        let start = (input.cursor + 1).saturating_sub(room);
        let shown: Vec<char> = chars.iter().skip(start).take(room).copied().collect();
        let cur = input.cursor - start;
        let mut text = String::new();
        for (i, c) in shown.iter().enumerate() {
            if focus && i == cur {
                text.push_str(&self.reverse(&c.to_string()));
            } else {
                text.push(*c);
            }
        }
        if focus && cur >= shown.len() {
            text.push_str(&self.reverse(" "));
        }
        let label = if focus {
            self.m.fg_bold(pal::CYAN, label)
        } else {
            self.m.fg(pal::GREY, label)
        };
        format!("{label}  {text}")
    }

    fn reverse(&self, s: &str) -> String {
        if self.m == ColorMode::None {
            format!("[{s}]")
        } else {
            format!("\x1b[7m{s}\x1b[27m")
        }
    }

    fn kv(&self, label: &str, value: &str) -> String {
        let pad = " ".repeat(12usize.saturating_sub(label.chars().count()));
        format!(
            "{}{pad}{}",
            self.m.fg(pal::GREY, label),
            clip(value, self.w.saturating_sub(12))
        )
    }
}

// ------------------------------------------------------------------ what is shown about an agent

fn key_text(a: &AgentRow) -> String {
    use super::model::KeyState;
    match &a.key {
        KeyState::File => "Schlüsseldatei".into(),
        KeyState::FileMissing => "Schlüsseldatei FEHLT".into(),
        KeyState::Env { name, set_here } => {
            format!(
                "Umgebung {name}{}",
                if *set_here {
                    ""
                } else {
                    " (hier nicht gesetzt)"
                }
            )
        }
    }
}

fn station_text(a: &AgentRow) -> &'static str {
    if a.running {
        if a.paused {
            "läuft, pausiert"
        } else {
            "läuft"
        }
    } else if a.paused {
        "pausiert"
    } else if matches!(a.service, ServiceState::Installed { active: true }) {
        "Dienst aktiv"
    } else if a.profile.is_some() {
        "gestoppt"
    } else {
        "keine Station"
    }
}

fn engine_label(name: &str) -> String {
    Engine::parse(name).map_or_else(|| name.to_owned(), |e| e.label().to_owned())
}

/// The text of the Status screen.
pub fn status_lines(s: &Snapshot) -> Vec<String> {
    let mut o = vec![
        format!("Stonewave {} · {}", crate::VERSION, s.exe),
        format!("Rechner: {} ({})", s.host, os_name(s.os)),
        format!("Einstellungen: {}", s.config_dir),
        format!("Zustand: {}", s.state_dir),
        String::new(),
        "Coding-Agenten auf diesem Computer:".into(),
    ];
    for e in &s.engines {
        o.push(format!(
            "  {:<12} {}",
            e.engine.label(),
            if e.found {
                e.version
                    .as_ref()
                    .map_or("gefunden".to_owned(), |v| format!("gefunden ({v})"))
            } else {
                "nicht gefunden".into()
            }
        ));
    }
    o.push(String::new());
    if s.agents.is_empty() {
        o.push("Noch kein Agent mit Wave verbunden.".into());
    } else {
        o.push("Agenten:".into());
        for a in &s.agents {
            o.push(format!(
                "  {} ({}) · {} · Station: {}",
                a.name,
                engine_label(&a.engine),
                key_text(a),
                station_text(a)
            ));
            if let Some(p) = &a.profile {
                o.push(format!("      Räume: {}", p.room_names.join(", ")));
                o.push(format!("      Ordner: {}", p.workdir));
            }
        }
    }
    o
}

fn os_name(os: Os) -> &'static str {
    match os {
        Os::Linux => "Linux",
        Os::Mac => "macOS",
        Os::Windows => "Windows",
    }
}

// ------------------------------------------------------------------ the screen

pub fn too_small(w: usize, h: usize) -> bool {
    w < MIN_WIDTH || h < MIN_HEIGHT
}

/// The whole screen as lines (at most `h`), margin included.
pub fn render(app: &App, w: usize, h: usize, m: ColorMode, t: f32) -> Vec<String> {
    if too_small(w, h) {
        return vec![
            format!("{MARGIN}Das Terminal ist zu klein für das Menü."),
            format!("{MARGIN}Mindestens {MIN_WIDTH}×{MIN_HEIGHT} Zeichen; q beendet."),
        ];
    }
    let p = P {
        m,
        w: w.saturating_sub(MARGIN.len() * 2),
    };
    let mut lines: Vec<String> = Vec::new();

    // header
    let big = h >= BIG_BANNER_MIN_HEIGHT && w >= MARGIN.len() + banner::WIDTH + 2;
    if big {
        for l in banner::frame(m, app.crest) {
            lines.push(format!("{MARGIN}{l}"));
        }
        let left = "Agenten und ihre Stationen";
        let right = format!("v{}", crate::VERSION);
        let gap = banner::WIDTH
            .saturating_sub(left.chars().count() + right.chars().count())
            .max(1);
        lines.push(format!(
            "{MARGIN}{}",
            m.fg(pal::GREY, &format!("{left}{}{right}", " ".repeat(gap)))
        ));
    } else {
        lines.push(banner::compact(m, "Menü"));
    }
    lines.push(String::new());
    let header = lines.len();

    // body
    let footer_rows = 2;
    let body_rows = h.saturating_sub(header + footer_rows).max(4);
    let (mut body, keys) = if let Some(text) = &app.busy {
        (
            vec![format!(
                "{}  {}",
                wave_spinner(m, t, 5),
                m.fg(pal::GREY, text)
            )],
            "Strg-C beendet das Menü",
        )
    } else {
        screen_body(app, &p, body_rows)
    };
    if body.len() > body_rows {
        body.truncate(body_rows);
    }
    for l in body {
        lines.push(format!("{MARGIN}{l}"));
    }
    while lines.len() < h - footer_rows {
        lines.push(String::new());
    }

    // notice and keys
    let notice = app
        .notice
        .as_ref()
        .map_or_else(String::new, |(l, s)| format!("{MARGIN}{}", p.level(*l, s)));
    lines.push(notice);
    lines.push(format!("{MARGIN}{}", p.dim(keys)));
    lines.truncate(h);
    lines
}

fn screen_body(app: &App, p: &P, rows: usize) -> (Vec<String>, &'static str) {
    match app.top() {
        Screen::Main { cursor } => main_screen(app, p, *cursor, rows),
        Screen::Agent { name, cursor } => agent_screen(app, p, name, *cursor, rows),
        Screen::Wizard(w) => wizard_screen(app, p, w, rows),
        Screen::Add(a) => add_screen(app, p, a),
        Screen::Output {
            title,
            lines,
            scroll,
        } => output_screen(p, title, lines, *scroll, rows),
        Screen::Confirm(c) => confirm_screen(p, c, rows),
    }
}

/// The part of a list that is visible: `rows` entries around the cursor.
fn window(len: usize, cursor: usize, rows: usize) -> std::ops::Range<usize> {
    if len <= rows {
        return 0..len;
    }
    let start = cursor.saturating_sub(rows / 2).min(len - rows);
    start..start + rows
}

fn main_screen(app: &App, p: &P, cursor: usize, rows: usize) -> (Vec<String>, &'static str) {
    let s = &app.snap;
    if s.root {
        let mut o = vec![p.heading("Du bist als root angemeldet")];
        o.extend(p.paragraph("Agenten werden unter deinem normalen Benutzer verbunden, damit ihr Schlüssel nicht root gehört. Beende das Menü und starte `stonewave` ohne sudo."));
        o.push(String::new());
        o.extend(p.paragraph(
            "Um diesen SERVER mit Wave zu verbinden (Modul Systems), nimm: sudo stonewave connect",
        ));
        return (o, "q beenden");
    }
    let mut o = vec![p.heading("Agenten auf diesem Computer")];
    let mut entries: Vec<(bool, String)> = Vec::new();
    for a in &s.agents {
        entries.push((
            true,
            format!(
                "{:<14} {:<12} {:<26} {}",
                clip(&a.name, 14),
                engine_label(&a.engine),
                clip(&key_text(a), 26),
                station_text(a)
            ),
        ));
    }
    if s.agents.is_empty() {
        o.push(p.grey("Noch kein Agent verbunden."));
    }
    entries.push((false, "Agent hinzufügen …".into()));
    entries.push((false, "Agent verbinden …".into()));
    entries.push((false, "Status".into()));
    entries.push((false, "Beenden".into()));
    let room = rows.saturating_sub(3);
    for i in window(entries.len(), cursor, room) {
        if i == s.agents.len() {
            o.push(String::new());
        }
        o.push(p.row(i == cursor, &entries[i].1));
    }
    (o, "↑↓ wählen · Enter öffnen · q beenden")
}

/// "Agent hinzufügen": the kind, the name, and what is missing for a hosted agent.
fn add_screen(app: &App, p: &P, a: &Add) -> (Vec<String>, &'static str) {
    let mut o = vec![p.heading("Agent hinzufügen")];
    match a.step {
        AddStep::Kind => {
            o.push(p.grey("Was für ein Agent soll es sein?"));
            o.push(String::new());
            for (i, k) in KINDS.iter().enumerate() {
                let state = match k {
                    Kind::Engine(e) => match app.snap.engine(*e) {
                        Some(r) if r.found => r
                            .version
                            .as_ref()
                            .map_or("gefunden".to_owned(), |v| format!("gefunden ({v})")),
                        _ => format!("nicht gefunden · {}", install_hint(*e)),
                    },
                    Kind::GeminiHosted => "läuft in Wave · hier noch nicht möglich".to_owned(),
                };
                o.push(p.row(i == a.cursor, &format!("{:<18} {}", k.label(), state)));
            }
            o.extend(message_lines(p, &a.message));
            o.push(String::new());
            o.extend(p.note("Claude Code, Codex und Grok laufen auf diesem Computer. Ein gehosteter Agent läuft bei Wave."));
            (o, "↑↓ wählen · Enter weiter · Esc zurück")
        }
        AddStep::Name => {
            let label = a.kind.map_or("", Kind::label);
            o.push(p.grey(&format!("{label}: Wie soll der Agent heißen?")));
            o.push(String::new());
            o.push(p.field("Name", &a.name, true));
            o.extend(message_lines(p, &a.message));
            o.push(String::new());
            o.extend(p.note("Der Name ist ein Vorschlag für Wave. Du bestätigst ihn dort im Browser; angelegt wird der Agent in Wave, nicht hier."));
            o.extend(p.note("Höchstens 30 Zeichen."));
            (o, "tippen · Enter weiter · Esc zurück")
        }
        AddStep::Hosted => {
            o.push(p.grey("Gemini (gehostet)"));
            o.push(String::new());
            o.extend(p.paragraph("Ein gehosteter Agent läuft nicht auf diesem Computer, sondern in Wave. Das Menü kann ihn noch nicht anlegen. Auf der Seite von Wave fehlt dafür:"));
            for l in [
                "· eine Art „gehostet“ beim Anlegen eines Agenten (heute gibt es „Wave-Agent“ und „Eigener Agent“)",
                "· die Vorbereitung des Rechners, auf dem der Agent läuft",
                "· die Google-Anmeldung für Gemini, die auf diesem Rechner abgeschlossen wird",
            ] {
                o.extend(p.note(l));
            }
            o.push(String::new());
            o.extend(
                p.paragraph("Bis dahin richtet ein Betreiber von Wave gehostete Agenten ein."),
            );
            (o, "Esc zurück")
        }
    }
}

fn agent_screen(
    app: &App,
    p: &P,
    name: &str,
    cursor: usize,
    rows: usize,
) -> (Vec<String>, &'static str) {
    let Some(a) = app.snap.agent(name) else {
        return (
            vec![p.grey("Diesen Agenten gibt es nicht mehr.")],
            "Esc zurück",
        );
    };
    let mut o = vec![p.heading(&format!("{} · {}", a.name, engine_label(&a.engine)))];
    o.push(p.kv("Schlüssel", &key_text(a)));
    if let Some(u) = a
        .profile
        .as_ref()
        .map(|x| x.wave_url.clone())
        .or_else(|| a.wave_url.clone())
    {
        o.push(p.kv("Wave", &u));
    }
    if let Some(pr) = &a.profile {
        o.push(p.kv("Räume", &pr.room_names.join(", ")));
        o.push(p.kv("Ordner", &pr.workdir));
    }
    o.push(p.kv("Station", station_text(a)));
    o.push(p.kv(
        "Dienst",
        match a.service {
            ServiceState::None => {
                if service::supported(app.snap.os) {
                    "nicht eingerichtet"
                } else {
                    "unter diesem System noch nicht möglich"
                }
            }
            ServiceState::Installed { active: true } => "eingerichtet, läuft",
            ServiceState::Installed { active: false } => "eingerichtet, angehalten",
        },
    ));
    o.push(String::new());
    let actions = agent_actions(a, app.snap.os);
    let room = rows.saturating_sub(o.len());
    for i in window(actions.len(), cursor, room) {
        o.push(p.row(i == cursor, actions[i].label()));
    }
    (o, "↑↓ wählen · Enter ausführen · Esc zurück")
}

fn output_screen(
    p: &P,
    title: &str,
    lines: &[String],
    scroll: usize,
    rows: usize,
) -> (Vec<String>, &'static str) {
    let mut o = vec![p.heading(title), String::new()];
    let room = rows.saturating_sub(2).max(1);
    let max_scroll = lines.len().saturating_sub(room);
    let scroll = scroll.min(max_scroll);
    for l in lines.iter().skip(scroll).take(room) {
        o.push(
            p.plain(
                &l.chars()
                    .filter(|c| !is_invisible(*c) || *c == '\t')
                    .collect::<String>(),
            ),
        );
    }
    (o, "↑↓ blättern · Enter oder Esc zurück")
}

/// A text, then a question with two answers (the first is the default). A text that is longer than `rows` scrolls above the answers, which
/// always stay on the screen. Returns the lines and whether the text scrolls.
fn ask_block(
    p: &P,
    lines: &[String],
    scroll: usize,
    cursor: usize,
    rows: usize,
    answers: [&str; 2],
) -> (Vec<String>, bool) {
    let mut text: Vec<String> = Vec::new();
    for l in lines {
        if l.is_empty() {
            text.push(String::new());
        } else {
            text.extend(p.paragraph(l));
        }
    }
    let room = rows.saturating_sub(3).max(1);
    let scroll = scroll.min(text.len().saturating_sub(room));
    let mut o: Vec<String> = text.iter().skip(scroll).take(room).cloned().collect();
    let more = text.len() > scroll + room;
    o.push(if more {
        p.grey("▼ mehr: Bild ↓")
    } else {
        String::new()
    });
    o.push(p.row(cursor == 0, answers[0]));
    o.push(p.row(cursor == 1, answers[1]));
    (o, text.len() > room)
}

fn confirm_screen(p: &P, c: &Confirm, rows: usize) -> (Vec<String>, &'static str) {
    let mut o = vec![p.heading(&c.title), String::new()];
    let (block, scrolls) = ask_block(
        p,
        &c.lines,
        c.scroll,
        c.cursor,
        rows.saturating_sub(2),
        ["Nein", "Ja"],
    );
    o.extend(block);
    (
        o,
        if scrolls {
            "Bild ↑↓ blättert · ↑↓ wählen · Enter bestätigt · j ja · n oder Esc nein"
        } else {
            "↑↓ wählen · Enter bestätigt · j ja · n oder Esc nein"
        },
    )
}

// ------------------------------------------------------------------ the wizard

/// The numbered steps of the way for a server with a secret store (the simple way has few enough screens not to be numbered).
fn steps_of(_w: &Wizard) -> Vec<Step> {
    vec![
        Step::Mode,
        Step::Engine,
        Step::Name,
        Step::WaveUrl,
        Step::KeySource,
        Step::Rooms,
        Step::Workdir,
        Step::Limits,
        Step::Preview,
        Step::Test,
        Step::Service,
        Step::Done,
    ]
}

fn step_title(s: Step) -> &'static str {
    match s {
        Step::Mode => "Wie kommt der Agent zu Wave?",
        Step::Engine => "Welches Programm soll der Agent sein?",
        Step::Name => "Wie heißt der Agent?",
        Step::WaveUrl => "Wo liegt Wave?",
        Step::KeySource => "Woher kommt der Schlüssel?",
        Step::Connect => "Agent mit Wave verbinden",
        Step::Defaults => "Die Station mit den üblichen Einstellungen",
        Step::StationAsk => "Station einrichten?",
        Step::Rooms => "In welchen Räumen soll der Agent geweckt werden?",
        Step::Workdir => "Der Arbeitsordner des Agenten",
        Step::Limits => "Die Grenzen jedes Weckrufs",
        Step::Preview => "Vorschau: was ein Weckruf starten würde",
        Step::Test => "Test: einmal in den Posteingang schauen",
        Step::Service => "Dauerhaft starten?",
        Step::Done => "Fertig",
    }
}

fn wizard_screen(app: &App, p: &P, w: &Wizard, rows: usize) -> (Vec<String>, &'static str) {
    let steps = steps_of(w);
    let at = steps.iter().position(|s| *s == w.step).map_or(1, |i| i + 1);
    let server_way = w.mode == Mode::Existing && w.step != Step::Mode;
    let mut o = vec![p.heading(&if server_way {
        format!(
            "Agent verbinden · Server mit Secret-Store · Schritt {at} von {}",
            steps.len()
        )
    } else if w.preset {
        "Agent hinzufügen".to_owned()
    } else {
        "Agent verbinden".to_owned()
    })];
    o.push(p.grey(step_title(w.step)));
    if server_way && w.step != Step::Done && w.step != Step::Service {
        // nobody should land here by mistake and not know
        o.extend(p.note("Dieser Weg ist für einen Server, dessen Agent schon in Wave besteht und dessen Schlüssel in einem Secret-Store liegt. Auf einem normalen Computer brauchst du ihn nicht: Esc bis zum Anfang, dann \"Neuen Agenten anmelden\"."));
    }
    o.push(String::new());
    let mut keys = "Enter weiter · Esc zurück";
    match w.step {
        Step::Mode => {
            let items: [(&str, &str); 2] = if w.more {
                [
                    (
                        "Auf einem Server mit Secret-Store",
                        "Für einen Agenten, der in Wave schon besteht und dessen Schlüssel in einem Secret-Store liegt (typisch ein Server). Der Schlüssel wird nie angezeigt oder gespeichert; du trägst nur den Namen der Umgebungsvariable ein. Auf einem normalen Computer brauchst du das nicht.",
                    ),
                    ("Zurück", ""),
                ]
            } else {
                [
                    (
                        "Neuen Agenten anmelden",
                        "Dein Browser öffnet sich, du gibst den Code ein und wählst in Wave den Agenten. Danach ist dieser Computer verbunden. Das ist der Weg für jeden normalen Computer.",
                    ),
                    (
                        "Mehr …",
                        "Weitere Wege, zum Beispiel für einen Server mit Secret-Store.",
                    ),
                ]
            };
            for (i, (t, _)) in items.iter().enumerate() {
                o.push(p.row(i == w.cursor, t));
            }
            o.push(String::new());
            o.extend(p.note(items[w.cursor.min(1)].1));
            keys = if w.more {
                "↑↓ wählen · Enter weiter · Esc zurück"
            } else {
                "↑↓ wählen · Enter weiter · Esc zurück zum Menü"
            };
        }
        Step::Connect => {
            let list = connect_engines(&app.snap);
            if let (true, Some(e)) = (w.preset, w.engine) {
                let found = app.snap.engine(e).and_then(|r| r.version.clone());
                o.push(p.kv(
                    "Programm",
                    &format!(
                        "{}{}",
                        e.label(),
                        found.map_or(String::new(), |v| format!(" ({v})"))
                    ),
                ));
            } else if list.is_empty() {
                o.extend(p.paragraph("Auf diesem Computer ist kein Coding-Agent installiert, den das Menü verbinden kann."));
                o.push(String::new());
                o.push(p.kv("Installieren", install_hint(Engine::ClaudeCode)));
            } else if list.len() == 1 {
                let found = app.snap.engine(list[0]).and_then(|r| r.version.clone());
                o.push(p.kv(
                    "Programm",
                    &format!(
                        "{}{}",
                        list[0].label(),
                        found.map_or(String::new(), |v| format!(" ({v})"))
                    ),
                ));
            } else {
                for (i, e) in list.iter().enumerate() {
                    o.push(p.row(i == w.cursor, e.label()));
                }
            }
            if w.preset {
                o.push(p.kv("Name", &w.agent()));
                o.push(String::new());
                o.extend(p.paragraph("Angelegt wird der Agent in Wave, nicht von diesem Menü. Lege ihn dort zuerst an, falls es ihn noch nicht gibt (Team, Agent hinzufügen, „Eigener Agent“) und drücke dort „Gerät verbinden“. Dann hier Enter:"));
                for l in [
                    "1. Dein Browser öffnet die Anmeldeseite von Wave (gibt es keinen, steht der Link hier).",
                    "2. Gib dort den Code ein, der hier erscheint. Prüfe, dass dieselben vier Wörter dort stehen. Der Name oben ist nur ein Vorschlag.",
                    "3. Wähle in Wave den Agenten. Danach ist dieser Computer verbunden.",
                ] {
                    o.extend(p.note(l));
                }
            } else {
                o.push(String::new());
                o.extend(p.paragraph("Wenn du Enter drückst:"));
                for l in [
                    "1. Dein Browser öffnet die Anmeldeseite von Wave (gibt es keinen, steht der Link hier).",
                    "2. Gib dort den Code ein, der hier erscheint. Prüfe, dass dieselben vier Wörter dort stehen, und wähle in Wave den Agenten.",
                    "3. Danach ist dieser Computer verbunden. Den Namen des Agenten musst du nicht tippen.",
                ] {
                    o.extend(p.note(l));
                }
            }
            if let Some((l, m)) = &w.message {
                o.push(String::new());
                for line in wrap(m, p.w.saturating_sub(2)) {
                    o.push(p.level(*l, &line));
                }
            }
            keys = "Enter anmelden · Esc zurück";
        }
        Step::Engine => {
            for (i, e) in engine_rows().into_iter().enumerate() {
                let found = app.snap.engine(e);
                let ok = engine_choosable(e, w.mode);
                let state = match found {
                    Some(r) if r.found => r
                        .version
                        .as_ref()
                        .map_or("gefunden".to_owned(), |v| format!("gefunden ({v})")),
                    _ => format!("nicht gefunden · {}", install_hint(e)),
                };
                let text = format!(
                    "{:<12} {}",
                    e.label(),
                    if ok {
                        state
                    } else {
                        engine_note(e, w.mode).to_owned()
                    }
                );
                let usable = ok && found.is_some_and(|r| r.found);
                if usable || i == w.cursor {
                    o.push(if usable {
                        p.row(i == w.cursor, &text)
                    } else {
                        greyed(p, i == w.cursor, &text)
                    });
                } else {
                    o.push(p.row_off(&text));
                }
            }
            o.push(p.row_off(&format!("{:<12} noch nicht anbindbar", "Gemini CLI")));
            o.push(String::new());
            o.extend(p.note("Ausgegraut heißt: hier nicht installiert oder auf diesem Weg noch nicht möglich. Die Station (Wecken per Erwähnung in Wave) gibt es bisher für Claude Code."));
            keys = "↑↓ wählen · Enter weiter · Esc zurück";
        }
        Step::Name => {
            o.push(p.field("Name", &w.name, true));
            o.extend(message_lines(p, &w.message));
            o.push(String::new());
            o.extend(p.note("Kleinbuchstaben, Ziffern und Bindestriche. Unter diesem Namen führt das Menü den Agenten, seine Dateien und seinen Dienst."));
            keys = "tippen · Enter weiter · Esc zurück";
        }
        Step::WaveUrl => {
            o.push(p.field("Wave", &w.wave_url, true));
            o.extend(message_lines(p, &w.message));
            o.push(String::new());
            o.extend(p.note("Die Adresse von Wave (https). Das Menü fragt nach den Räumen des Agenten unter dieser Adresse."));
            keys = "tippen · Enter weiter · Esc zurück";
        }
        Step::KeySource => {
            o.push(p.field("Variable", &w.key_env, w.field == 0));
            let set = (app.env_probe)(w.key_env.value().trim());
            o.push(format!(
                "            {}",
                if set {
                    p.level(Level::Ok, "ist in dieser Shell gesetzt")
                } else {
                    p.level(Level::Warn, "ist in dieser Shell nicht gesetzt")
                }
            ));
            o.push(String::new());
            o.push(p.field("Start-Befehl", &w.launcher, w.field == 1));
            o.extend(message_lines(p, &w.message));
            o.push(String::new());
            o.extend(p.note("Variable: der NAME, unter dem der Schlüssel in der Umgebung liegt (nie der Schlüssel selbst)."));
            o.extend(p.note("Start-Befehl: der Befehl deines Secret-Stores, der ein Programm mit den Geheimnissen in der Umgebung startet, zum Beispiel `secrets-run run --env=prod --`. Der Dienst startet die Station dadurch. Ohne ihn geht nur der Start von Hand."));
            if !set {
                o.extend(p.note("Für Raumliste, Vorschau und Test muss die Variable jetzt gesetzt sein: beende das Menü und starte es durch deinen Secret-Store-Befehl."));
            }
            keys = "Tab wechselt das Feld · Enter weiter · Esc zurück";
        }
        Step::StationAsk => {
            o.push(p.level(Level::Ok, &format!("Verbunden: {}", w.agent())));
            o.push(String::new());
            o.extend(p.paragraph("Soll der Agent auch aufwachen, wenn ihn jemand in Wave erwähnt? Dafür läuft hier eine Station: eine lange Sitzung je Raum, ohne Befehls-Werkzeuge, mit festen Grenzen."));
            o.push(String::new());
            o.push(p.row(
                w.cursor == 0,
                "Später (geht jederzeit auf der Seite des Agenten)",
            ));
            o.push(p.row(w.cursor == 1, "Ja, jetzt mit den üblichen Einstellungen"));
            keys = "↑↓ wählen · Enter weiter";
        }
        Step::Defaults => {
            o.extend(p.paragraph(
                "Das nimmt das Menü, wenn du Enter drückst (die Buchstaben ändern es):",
            ));
            o.push(String::new());
            o.push(p.kv(
                "Wave",
                &format!("{}   · w ändert", w.wave_url.value().trim()),
            ));
            if w.rooms_loaded {
                let chosen = w.chosen_rooms();
                let names: Vec<&str> = chosen.iter().map(|r| r.name.as_str()).collect();
                let head = if chosen.len() == w.rooms.len() {
                    format!("alle {}", w.rooms.len())
                } else {
                    format!("{} von {}", chosen.len(), w.rooms.len())
                };
                o.push(p.kv(
                    "Räume",
                    &format!("{head}: {}   · r ändert", names.join(", ")),
                ));
            } else {
                o.push(p.kv("Räume", "werden von Wave geholt …"));
            }
            o.push(p.kv(
                "Ordner",
                &format!("{}   · o ändert", w.workdir.value().trim()),
            ));
            o.push(p.kv(
                "Grenzen",
                "20 Min · 40 Züge · 2 USD je Weckruf, 10 pro Stunde, 50 pro Tag",
            ));
            o.push(p.kv(
                "",
                "geweckt nur von Personen der Firma, ohne Befehls-Werkzeuge",
            ));
            o.push(String::new());
            o.extend(p.note("Danach prüft das Menü alles mit einer Vorschau (nichts wird gestartet), merkt sich die Einrichtung und fragt, ob die Station dauerhaft laufen soll."));
            if let Some((l, m)) = &w.message {
                o.push(String::new());
                for line in wrap(m, p.w.saturating_sub(2)) {
                    o.push(p.level(*l, &line));
                }
            }
            keys = if w.rooms_loaded {
                "Enter einrichten · r Räume · o Ordner · w Wave · Esc zurück"
            } else {
                "Enter versucht es noch einmal · w Wave · o Ordner · Esc zurück"
            };
        }
        Step::Rooms => {
            if w.rooms_loaded {
                o.extend(p.note("Der Agent wird nur in diesen Räumen geweckt (Leertaste wählt). Nur Personen der Firma wecken ihn, nie andere Agenten."));
                o.push(String::new());
                let room = rows.saturating_sub(o.len() + 3).max(3);
                for i in window(w.rooms.len(), w.cursor, room) {
                    let r = &w.rooms[i];
                    o.push(p.row(
                        i == w.cursor,
                        &format!("[{}] {}", if r.on { "x" } else { " " }, r.name),
                    ));
                }
                keys = "↑↓ · Leertaste wählt · Enter weiter · Esc zurück";
            } else {
                o.extend(p.paragraph("Das Menü fragt Wave, welche Räume der Agent lesen darf."));
                keys = "Enter versucht es noch einmal · Esc zurück";
            }
        }
        Step::Workdir => {
            o.push(p.field("Ordner", &w.workdir, true));
            o.extend(message_lines(p, &w.message));
            o.push(String::new());
            o.extend(p.note("Der Agent liest und schreibt nur hier (je Raum ein Unterordner). Die Station prüft vor jedem Weckruf, dass der Ordner kein Ort mit Schlüsseln oder Einstellungen ist, und lehnt sonst ab."));
            keys = "tippen · Enter weiter · Esc zurück";
        }
        Step::Limits => {
            for l in [
                "Weckruf dauert höchstens 20 Minuten und 40 Züge",
                "Kosten je Weckruf: höchstens 2 US-Dollar (Schätzung von Claude Code)",
                "höchstens 10 Weckrufe pro Stunde und 50 pro Tag, einer nach dem anderen",
                "eine Sitzung je Raum, nach 200 Weckrufen mit Übergabenotiz erneuert",
                "keine Befehls-Werkzeuge, kein Netz-Zugriff außer Wave",
            ] {
                o.push(format!("  {}", p.plain(l)));
            }
            o.push(String::new());
            o.extend(p.note("Das sind die Grenzen der Station; das Menü ändert sie nicht. Mit Optionen von `stonewave agent station run --help` lassen sie sich von Hand anpassen."));
        }
        Step::Preview => {
            let room = rows.saturating_sub(o.len() + 2).max(3);
            let max_scroll = w.output.len().saturating_sub(room);
            for l in w.output.iter().skip(w.scroll.min(max_scroll)).take(room) {
                o.push(p.plain(l));
            }
            if let Some((l, m)) = &w.message {
                o.push(p.level(*l, m));
            }
            keys = if w.preview_ok {
                "↑↓ blättern · Enter weiter · r wiederholt · Esc zurück"
            } else {
                "↑↓ blättern · r wiederholt · Esc zurück"
            };
        }
        Step::Test => {
            o.extend(p.paragraph("Die Station schaut einmal in den Posteingang von Wave und hört wieder auf. Beim allerersten Start merkt sie sich nur den Stand und weckt niemanden. Gibt es schon einen Stand, kann ein offener Aufruf jetzt den Agenten wecken."));
            o.push(String::new());
            o.push(p.kv(
                "Befehl",
                &format!(
                    "stonewave {}",
                    super::model::station_command(&w.profile(), None, &["--once"]).join(" ")
                ),
            ));
            if let Some((l, m)) = &w.message {
                o.push(String::new());
                o.push(p.level(*l, m));
            }
            keys = "Enter testen · s überspringen · Esc zurück";
        }
        Step::Service => {
            let prof = w.profile();
            if !service::supported(app.snap.os) {
                o.extend(p.paragraph("Einen Dienst kann das Menü unter diesem System noch nicht einrichten. Du startest die Station von Hand: Seite des Agenten, \"Station jetzt hier starten\"."));
                keys = "Enter weiter · Esc zurück";
            } else if !app.service_possible(&prof) {
                o.extend(p.paragraph("Für einen Dienst fehlt der Start-Befehl deines Secret-Stores (Schritt \"Woher kommt der Schlüssel?\"): ohne ihn hätte der Dienst keinen Schlüssel. Du kannst zurückgehen und ihn eintragen oder die Station von Hand starten."));
                keys = "Enter weiter · Esc zurück";
            } else if let Ok((_, lines)) = app.install_confirm(&prof) {
                // the same question as on the agent's page: the whole file, "Nein" is where the cursor starts
                let mut intro = vec![
                    "Ein Benutzer-Dienst (ohne Root) startet die Station jetzt und bei jedem Anmelden und startet sie neu, wenn sie abstürzt.".to_owned(),
                    String::new(),
                ];
                intro.extend(lines);
                let (block, scrolls) = ask_block(
                    p,
                    &intro,
                    w.scroll,
                    w.cursor,
                    rows.saturating_sub(o.len()),
                    [
                        "Nein, ich starte die Station selbst",
                        "Ja, einrichten und starten",
                    ],
                );
                o.extend(block);
                keys = if scrolls {
                    "Bild ↑↓ blättert · ↑↓ wählen · Enter bestätigt · Esc zurück"
                } else {
                    "↑↓ wählen · Enter bestätigt · Esc zurück"
                };
            }
            if let Some((l, m)) = &w.message {
                o.push(p.level(*l, m));
            }
        }
        Step::Done => {
            o.push(p.level(
                Level::Ok,
                &format!(
                    "{} ist {}.",
                    w.agent(),
                    if w.mode == Mode::New && w.chosen_rooms().is_empty() {
                        "verbunden"
                    } else {
                        "eingerichtet"
                    }
                ),
            ));
            for n in &w.notes {
                o.extend(p.note(n));
            }
            if !w.chosen_rooms().is_empty() {
                o.push(String::new());
                o.push(
                    p.kv(
                        "Räume",
                        &w.chosen_rooms()
                            .iter()
                            .map(|r| r.name.clone())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                );
                o.push(p.kv("Ordner", w.workdir.value().trim()));
            }
            if w.mode == Mode::New {
                o.push(String::new());
                o.extend(p.note("Weiteres (Testlauf, Dienst, Räume ändern, Trennen) steht auf der Seite des Agenten."));
            }
            keys = "Enter zurück zum Menü";
        }
    }
    if matches!(w.step, Step::Mode | Step::Engine | Step::Rooms) {
        if let Some((l, m)) = &w.message {
            o.push(String::new());
            for line in wrap(m, p.w.saturating_sub(2)) {
                o.push(p.level(*l, &line));
            }
        }
    }
    (o, keys)
}

/// The message of a step (a refusal, an error), wrapped, right under the field it is about.
fn message_lines(p: &P, m: &Option<(Level, String)>) -> Vec<String> {
    m.as_ref()
        .map(|(l, t)| {
            wrap(t, p.w.saturating_sub(2))
                .iter()
                .map(|x| p.level(*l, x))
                .collect()
        })
        .unwrap_or_default()
}

fn greyed(p: &P, on: bool, text: &str) -> String {
    if on {
        format!(
            "{} {}",
            p.m.fg(pal::AMBER, "›"),
            p.m.fg(pal::GREY, &clip(text, p.w - 2))
        )
    } else {
        p.row_off(text)
    }
}
