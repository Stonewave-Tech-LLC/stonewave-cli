//! How each screen of the menu looks: the model in, lines out. Text is German (the people who use the menu read it), the palette is the
//! banner's. Every line is clipped to the width of the terminal before it is coloured, so nothing wraps by accident.

use crate::agent::dirs::Os;
use crate::agent::engine::Engine;
use crate::banner;
use crate::style::{ColorMode, pal};
use crate::ui::{MARGIN, wave_spinner};

use super::model::{
    AgentRow, App, Confirm, Level, Mode, Screen, Snapshot, Step, Wizard, agent_actions,
    engine_choosable, engine_note, engine_rows, install_hint,
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
                    .filter(|c| !c.is_control() || *c == '\t')
                    .collect::<String>(),
            ),
        );
    }
    (o, "↑↓ blättern · Enter oder Esc zurück")
}

fn confirm_screen(p: &P, c: &Confirm, rows: usize) -> (Vec<String>, &'static str) {
    let mut text: Vec<String> = Vec::new();
    for l in &c.lines {
        if l.is_empty() {
            text.push(String::new());
        } else {
            text.extend(p.paragraph(l));
        }
    }
    // the question and its two answers always stay on the screen; a long text scrolls above them
    let room = rows.saturating_sub(2 + 3).max(1);
    let scroll = c.scroll.min(text.len().saturating_sub(room));
    let mut o = vec![p.heading(&c.title), String::new()];
    o.extend(text.iter().skip(scroll).take(room).cloned());
    let more = text.len() > scroll + room;
    o.push(if more {
        p.grey("▼ mehr: Bild ↓")
    } else {
        String::new()
    });
    o.push(p.row(c.cursor == 0, "Nein"));
    o.push(p.row(c.cursor == 1, "Ja"));
    (
        o,
        if text.len() > room {
            "Bild ↑↓ blättert · ↑↓ wählen · Enter bestätigt · j ja · n oder Esc nein"
        } else {
            "↑↓ wählen · Enter bestätigt · j ja · n oder Esc nein"
        },
    )
}

// ------------------------------------------------------------------ the wizard

fn steps_of(w: &Wizard) -> Vec<Step> {
    let mut v = vec![Step::Mode, Step::Engine, Step::Name];
    match w.mode {
        Mode::New => {
            v.push(Step::SignIn);
            if w.engine.is_none_or(|e| e == Engine::ClaudeCode) {
                v.push(Step::StationAsk);
            }
        }
        Mode::Existing => v.extend([Step::WaveUrl, Step::KeySource]),
    }
    if w.engine.is_none_or(|e| e == Engine::ClaudeCode) {
        v.extend([
            Step::Rooms,
            Step::Workdir,
            Step::Limits,
            Step::Preview,
            Step::Test,
            Step::Service,
        ]);
    }
    v.push(Step::Done);
    v
}

fn step_title(s: Step) -> &'static str {
    match s {
        Step::Mode => "Wie kommt der Agent zu Wave?",
        Step::Engine => "Welches Programm soll der Agent sein?",
        Step::Name => "Wie heißt der Agent?",
        Step::WaveUrl => "Wo liegt Wave?",
        Step::KeySource => "Woher kommt der Schlüssel?",
        Step::SignIn => "Bei Wave anmelden",
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
    let mut o = vec![p.heading(&format!(
        "Agent verbinden · Schritt {at} von {}",
        steps.len()
    ))];
    o.push(p.grey(step_title(w.step)));
    o.push(String::new());
    let mut keys = "Enter weiter · Esc zurück";
    match w.step {
        Step::Mode => {
            let items = [
                (
                    "Neuen Agenten anmelden",
                    "Du bestätigst die Anmeldung im Browser; Wave legt den Agenten an und gibt ihm einen Schlüssel für diesen Computer.",
                ),
                (
                    "Vorhandenen Agenten anbinden",
                    "Der Agent besteht in Wave schon und sein Schlüssel liegt in einem Secret-Store (typisch: ein Server). Der Schlüssel wird nie angezeigt und nie gespeichert; hier trägst du nur den Namen der Umgebungsvariable ein.",
                ),
            ];
            for (i, (t, _)) in items.iter().enumerate() {
                o.push(p.row(i == w.cursor, t));
            }
            o.push(String::new());
            o.extend(p.note(items[w.cursor.min(1)].1));
            keys = "↑↓ wählen · Enter weiter · Esc zurück zum Menü";
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
        Step::SignIn => {
            o.extend(p.paragraph("Gleich macht das Menü Platz für die bekannte Anmeldung: du bekommst einen Code und vier Wörter, bestätigst im Browser und das Menü kommt zurück."));
            o.push(String::new());
            o.push(p.kv(
                "Befehl",
                &format!(
                    "stonewave agent connect --engine {} --name {} --no-station",
                    w.engine.unwrap_or(Engine::ClaudeCode).name(),
                    w.agent()
                ),
            ));
            o.push(String::new());
            o.extend(p.note("Der Schlüssel wird in eine private Datei geschrieben und nie angezeigt. Bestätige nur, wenn die vier Wörter im Browser dieselben sind und du die Anmeldung selbst gestartet hast."));
            if let Some((l, m)) = &w.message {
                o.push(String::new());
                o.push(p.level(*l, m));
            }
            keys = "Enter anmelden · Esc zurück";
        }
        Step::StationAsk => {
            o.extend(p.paragraph("Die Station weckt den Agenten, wenn eine Person der Firma ihn in Wave erwähnt: eine lange Sitzung je Raum, ohne Befehls-Werkzeuge, mit festen Grenzen."));
            o.push(String::new());
            o.push(p.row(w.cursor == 0, "Ja, jetzt einrichten"));
            o.push(p.row(w.cursor == 1, "Später"));
            keys = "↑↓ wählen · Enter weiter";
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
            keys = "↑↓ blättern · Enter weiter · Esc zurück";
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
            } else {
                o.extend(p.paragraph("Ein Benutzer-Dienst (ohne Root) startet die Station jetzt und bei jedem Anmelden und startet sie neu, wenn sie abstürzt. Die Datei, die das Menü schreibt:"));
                o.push(String::new());
                o.push(p.kv(
                    "Datei",
                    &format!("~/.config/systemd/user/{}", service::unit_name(&prof.agent)),
                ));
                o.push(
                    p.kv(
                        "Befehl",
                        &service::exec_words(&prof, &app.snap.exe, app.snap.claude.as_deref())
                            .join(" "),
                    ),
                );
                o.push(String::new());
                o.push(p.row(w.cursor == 0, "Ja, einrichten und starten"));
                o.push(p.row(w.cursor == 1, "Nein, ich starte die Station selbst"));
                keys = "↑↓ wählen · Enter bestätigt · Esc zurück";
            }
            if let Some((l, m)) = &w.message {
                o.push(String::new());
                o.push(p.level(*l, m));
            }
        }
        Step::Done => {
            o.push(p.level(Level::Ok, &format!("{} ist eingerichtet.", w.agent())));
            if w.step == Step::Done && !w.rooms.is_empty() && w.chosen_rooms().is_empty() {
                // skipped the station
            }
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
