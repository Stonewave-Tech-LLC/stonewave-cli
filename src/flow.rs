//! The screens: connect, status, revoke.
//!
//! This is a demo. Values that would come from Wave (sign-in code, account,
//! key fingerprint, health, rights) are made up, and nothing on the machine
//! is changed. The only things read from the machine are its host name, OS
//! name and the current user name, for the host rows.

use std::time::{Duration, Instant};

use crate::banner;
use crate::connect::{self, Account, ConnectError, Http, Pace, Wait};
use crate::style::{ColorMode, pal};
use crate::term::{Input, Key, Result, Term};
use crate::ui::{
    DETAIL, INDENT, MARGIN, Step, StepState, kv, meter, progress_bar, pulse_dot, spread, step_row,
    wave_spinner,
};

/// Where the person enters the sign-in code.
const LINK_URL: &str = "account.stonewavetech.com/link";
/// How long a sign-in code stays valid.
const CODE_VALID: Duration = Duration::from_secs(15 * 60);
/// How long the demo waits before it "signs in" by itself.
const DEMO_SIGN_IN: Duration = Duration::from_millis(3600);

const DEMO_ACCOUNT: &str = "lena@acme.example · Acme GmbH";
const DEMO_DEVICE: &str = "Office PC";

/// Width that right-aligned parts of a screen line up to.
const SCREEN_W: usize = 72;

pub enum Outcome {
    Done,
    /// The person said no; nothing happened.
    Declined,
    /// Could not finish (no answer from the account, code expired, ...); the reason was shown. Nothing was changed.
    Failed,
}

/// The machine as shown in the host rows.
pub struct Host {
    pub name: String,
    pub os: String,
    pub user: String,
}

impl Host {
    /// Reads host name, OS and user (read only). Each can be overridden for screenshots.
    pub fn detect(name: Option<String>, os: Option<String>, user: Option<String>) -> Host {
        Host {
            name: name
                .or_else(read_hostname)
                .unwrap_or_else(|| "this-server".into()),
            os: os.unwrap_or_else(read_os),
            user: user
                .or_else(|| env_nonempty("USER"))
                .or_else(|| env_nonempty("USERNAME"))
                .unwrap_or_else(|| "unknown".into()),
        }
    }

    fn is_root(&self) -> bool {
        self.user == "root"
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn read_hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| env_nonempty("HOSTNAME"))
        .or_else(|| env_nonempty("COMPUTERNAME").map(|s| s.to_lowercase()))
}

/// "Ubuntu 24.04" from `/etc/os-release`, else the OS family.
fn read_os() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        if let Some(os) = parse_os_release(&text) {
            return os;
        }
    }
    match std::env::consts::OS {
        "linux" => "Linux",
        "windows" => "Windows",
        "macos" => "macOS",
        other => other,
    }
    .to_owned()
}

fn parse_os_release(text: &str) -> Option<String> {
    let field = |key| crate::doctor::os_release_field(text, key);
    match (field("NAME"), field("VERSION_ID")) {
        (Some(n), Some(v)) => Some(format!("{n} {v}")),
        (Some(n), None) => Some(n),
        _ => field("PRETTY_NAME"),
    }
}

/// Demo randomness: xorshift64*, seeded from the clock or `--seed`.
/// Never used for anything secret; in the real CLI the code and keys come from elsewhere.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: Option<u64>) -> Rng {
        use std::hash::BuildHasher;
        let seed = seed.unwrap_or_else(|| {
            std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now())
        });
        Rng(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// A sign-in code like `WAVE-7K4Q`. Letters that look alike (0/O, 1/I) are left out.
/// In the real flow the account service issues the code, not the server.
fn demo_code(rng: &mut Rng) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let tail: String = (0..4).map(|_| char::from(rng.pick(ALPHABET))).collect();
    format!("WAVE-{tail}")
}

/// Four words that both sides show, easier to compare than a hash.
/// In the real flow they are derived from the key's fingerprint.
fn demo_words(rng: &mut Rng) -> [&'static str; 4] {
    const WORDS: [&str; 48] = [
        "anchor", "amber", "basalt", "beacon", "birch", "cedar", "comet", "coral", "delta",
        "ember", "fern", "fjord", "garnet", "glacier", "harbor", "hazel", "heron", "island",
        "jade", "juniper", "kelp", "lagoon", "lantern", "maple", "meadow", "nectar", "orbit",
        "otter", "pebble", "pine", "quartz", "raven", "reef", "river", "saffron", "seven", "shale",
        "spruce", "summit", "tidal", "timber", "tundra", "umber", "violet", "willow", "zephyr",
        "slate", "dune",
    ];
    // Four different words: a repeated word reads like a typo and is harder to compare.
    let mut words = [""; 4];
    let mut i = 0;
    while i < words.len() {
        let w = rng.pick(&WORDS);
        if !words[..i].contains(&w) {
            words[i] = w;
            i += 1;
        }
    }
    words
}

/// A made-up OpenSSH-style fingerprint: `SHA256:` and 43 base64 characters.
fn demo_fingerprint(rng: &mut Rng) -> String {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let body: String = (0..43).map(|_| char::from(rng.pick(B64))).collect();
    format!("SHA256:{body}")
}

fn mmss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

fn summary(m: ColorMode, label: &str, detail: &str) -> String {
    let pad = " ".repeat(20usize.saturating_sub(label.chars().count()));
    format!(
        "{INDENT}{} {}{pad}{}",
        m.fg_bold(pal::GREEN, "✓"),
        m.bold(label),
        m.fg(pal::GREY, detail)
    )
}

fn heading(m: ColorMode, icon: &str, text: &str) -> String {
    format!("{INDENT}{icon} {}", m.bold(text))
}

fn dim_note(m: ColorMode, text: &str) -> String {
    format!("{MARGIN}{}", m.fg(pal::DIM, text))
}

fn yes_no_hint(m: ColorMode, question: &str) -> String {
    format!(
        "{DETAIL}{}  {} {}  {}  {} {}",
        m.bold(question),
        m.fg_bold(pal::AMBER, "y"),
        m.fg(pal::GREY, "yes"),
        m.fg(pal::DIM, "·"),
        m.fg_bold(pal::GREY, "n"),
        m.fg(pal::GREY, "no")
    )
}

/// Shows the y/n hint, waits for the answer and echoes it on the prompt line.
fn ask(term: &mut Term, question: &str) -> Result<bool> {
    let m = term.mode();
    let prompt_mark = term.mark();
    term.line(&yes_no_hint(m, question));
    term.show_cursor();
    let yes = term.ask_yes_no()?;
    term.hide_cursor();
    echo_answer(term, prompt_mark, question, yes)?;
    Ok(yes)
}

/// Says why a prompt was answered no when there was nobody to ask.
fn no_terminal_note(term: &mut Term) {
    if term.caps.input == Input::None {
        let m = term.mode();
        term.line(&format!(
            "{DETAIL}{}",
            m.fg(
                pal::GREY,
                "No terminal to ask, so the answer is no. The demo plays through with --autoplay."
            )
        ));
    }
}

/// Shows the answer on the prompt line for a moment, so the choice is visible.
fn echo_answer(term: &mut Term, mark: usize, question: &str, yes: bool) -> Result<()> {
    if !term.caps.live {
        term.line(&format!(
            "{DETAIL}{question}  {}",
            if yes { "yes" } else { "no" }
        ));
        return Ok(());
    }
    let m = term.mode();
    let answer = if yes {
        m.fg_bold(pal::AMBER, "yes")
    } else {
        m.fg_bold(pal::GREY, "no")
    };
    term.repaint(mark, &[format!("{DETAIL}{}  {answer}", m.bold(question))]);
    term.wait(Duration::from_millis(350))?;
    Ok(())
}

// ---------------------------------------------------------------- connect

/// The real sign-in (device code, RFC 8628) against the account. Changes nothing on this machine.
pub fn connect_real(
    term: &mut Term,
    host: &Host,
    account: &Account,
    http: &dyn Http,
) -> Result<Outcome> {
    let m = term.mode();
    banner::show(term)?;
    term.line(&kv(
        m,
        INDENT,
        "host",
        7,
        &format!("{} ({})", host.name, host.os),
    ));
    term.line(&kv(m, INDENT, "user", 7, &host.user));
    term.line(&kv(m, INDENT, "to", 7, &account.host_text()));
    term.blank();
    term.flush()?;

    let started = match connect::start(http, account, &host.name, &host.os) {
        Ok(s) => s,
        Err(e) => return fail(term, "Could not start the sign-in", &e),
    };
    let mark = term.mark();
    term.line(&heading(
        m,
        &m.fg(pal::CYAN, "●"),
        "Sign in to connect this server",
    ));
    term.line(&kv(
        m,
        DETAIL,
        "Open",
        6,
        &m.link(pal::CYAN, &account.link_text()),
    ));
    term.line(&kv(
        m,
        DETAIL,
        "Code",
        6,
        &m.fg_bold(pal::AMBER, &started.user_code),
    ));
    term.line(&kv(
        m,
        DETAIL,
        "Words",
        6,
        &m.bold(&started.words.join(" · ")),
    ));
    term.line(&format!(
        "{DETAIL}{}",
        m.fg(
            pal::GREY,
            "The page shows the same four words. Confirm only if they match and you started this yourself."
        )
    ));
    term.blank();
    term.flush()?;

    let waited = {
        let mut pace = TermPace::new(term);
        connect::wait_for_approval(http, account, &started, &mut pace)
    };
    term.show_cursor();
    let waited = waited?;
    term.erase_since(mark);
    let waited = match waited {
        Ok(w) => w,
        Err(e) => return fail(term, "Lost the connection to the account", &e),
    };
    match waited.end {
        Wait::Approved => {
            let approved = waited.approved.expect("approved comes with the answer");
            // Proof that the token works and who it belongs to; the token is dropped right after.
            let company = match connect::whoami(http, account, &approved.token) {
                Ok(name) if !name.is_empty() => name,
                Ok(_) => approved.company_name.clone(),
                Err(e) => return fail(term, "The account did not accept the new sign-in", &e),
            };
            term.line(&summary(
                m,
                "Signed in",
                &format!("{company} · {}", approved.company),
            ));
            term.blank();
            term.line(&format!(
                "{DETAIL}{}",
                m.fg(
                    pal::GREY,
                    "This build stops here: setup comes in a later release."
                )
            ));
            term.line(&format!(
                "{DETAIL}{}",
                m.fg(
                    pal::GREY,
                    "Nothing was changed on this server, and the sign-in was not saved."
                )
            ));
            term.flush()?;
            Ok(Outcome::Done)
        }
        Wait::Denied => {
            term.line(&format!(
                "{INDENT}{} {}",
                m.fg_bold(pal::RED, "✗"),
                m.bold("Not connected · the sign-in was declined")
            ));
            term.line(&format!(
                "{DETAIL}{}",
                m.fg(pal::GREY, "Nothing was changed on this server.")
            ));
            term.flush()?;
            Ok(Outcome::Declined)
        }
        Wait::Expired => {
            term.line(&format!(
                "{INDENT}{} {}",
                m.fg_bold(pal::RED, "✗"),
                m.bold("The code ran out")
            ));
            term.line(&format!(
                "{DETAIL}{}",
                m.fg(
                    pal::GREY,
                    "Run `sudo stonewave connect` again for a new code. Nothing was changed."
                )
            ));
            term.flush()?;
            Ok(Outcome::Failed)
        }
    }
}

fn fail(term: &mut Term, what: &str, e: &ConnectError) -> Result<Outcome> {
    let m = term.mode();
    term.line(&format!(
        "{INDENT}{} {}",
        m.fg_bold(pal::RED, "✗"),
        m.bold(what)
    ));
    term.line(&format!("{DETAIL}{}", m.fg(pal::GREY, &e.to_string())));
    term.line(&format!(
        "{DETAIL}{}",
        m.fg(pal::GREY, "Nothing was changed on this server.")
    ));
    term.flush()?;
    Ok(Outcome::Failed)
}

/// Draws the waiting line and listens for Ctrl-C while the poll loop runs.
struct TermPace<'a> {
    term: &'a mut Term,
    started: Instant,
    mark: usize,
}

impl<'a> TermPace<'a> {
    fn new(term: &'a mut Term) -> Self {
        term.hide_cursor();
        let mark = term.mark();
        TermPace {
            term,
            started: Instant::now(),
            mark,
        }
    }
}

impl Pace for TermPace<'_> {
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn tick(&mut self, left: Duration) -> Result<()> {
        let m = self.term.mode();
        if self.term.caps.live {
            let t = if self.term.caps.animate {
                self.started.elapsed().as_secs_f32()
            } else {
                0.0
            };
            let line = format!(
                "{DETAIL}{}  {}",
                wave_spinner(m, t, 5),
                m.fg(
                    pal::GREY,
                    &format!("Waiting for you to confirm · code valid for {}", mmss(left))
                )
            );
            self.term.repaint(self.mark, &[line]);
            self.term.wait(Duration::from_millis(50))?;
        } else {
            // no terminal to draw on: wait quietly
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(())
    }
}

pub fn connect(term: &mut Term, host: &Host, rng: &mut Rng) -> Result<Outcome> {
    let m = term.mode();
    banner::show(term)?;
    term.line(&dim_note(
        m,
        "demo · this prototype changes nothing on this machine",
    ));
    term.blank();

    term.line(&kv(
        m,
        INDENT,
        "host",
        7,
        &format!("{} ({})", host.name, host.os),
    ));
    term.line(&kv(m, INDENT, "user", 7, &host.user));
    if !host.is_root() {
        term.line(&format!(
            "{INDENT}{}",
            m.fg(
                pal::AMBER,
                "◆ the real setup needs root once (sudo stonewave); this demo does not"
            )
        ));
    }
    term.blank();
    term.flush()?;

    sign_in(term, rng)?;
    if !confirm_key(term, rng)? {
        return Ok(Outcome::Declined);
    }
    let steps = vec![
        Step::new("Agent user", "stonewave-agent · no sudo · no password", 600),
        Step::new("Admin key", "1 device trusted · Office PC", 450),
        Step::new("Agent SSH", "certificates only · listens on 127.0.0.1", 800),
        Step::new("Daemon", "stonewaved · outbound only · no open port", 1000),
        Step::kept("Firewall", "ufw already active · kept as it is"),
        Step::new("Audit log", "/var/log/stonewave · every agent action", 450),
        Step::new("Health check", "tunnel · sshd · disk · clock", 1100),
    ];
    run_steps(
        term,
        &format!("Setting up {}", host.name),
        &format!("Set up {}", host.name),
        steps,
    )?;
    connected(term, host)?;
    Ok(Outcome::Done)
}

fn sign_in(term: &mut Term, rng: &mut Rng) -> Result<()> {
    let m = term.mode();
    let code = demo_code(rng);
    let mark = term.mark();

    term.line(&heading(
        m,
        &m.fg(pal::CYAN, "●"),
        "Sign in to connect this server",
    ));
    term.line(&kv(m, DETAIL, "Code", 6, &m.fg_bold(pal::AMBER, &code)));
    term.line(&kv(m, DETAIL, "Open", 6, &m.link(pal::CYAN, LINK_URL)));
    term.blank();

    if term.caps.live {
        term.hide_cursor();
        let wait_mark = term.mark();
        let started = Instant::now();
        loop {
            let elapsed = started.elapsed();
            if elapsed >= DEMO_SIGN_IN {
                break;
            }
            let t = if term.caps.animate {
                elapsed.as_secs_f32()
            } else {
                0.0
            };
            let lines = [
                format!(
                    "{DETAIL}{}  {}",
                    wave_spinner(m, t, 5),
                    m.fg(
                        pal::GREY,
                        &format!(
                            "Waiting for sign-in · code valid for {}",
                            mmss(CODE_VALID.saturating_sub(elapsed))
                        )
                    )
                ),
                format!(
                    "{DETAIL}{}",
                    m.fg(pal::DIM, "demo: signs in by itself · Enter skips")
                ),
            ];
            term.repaint(wait_mark, &lines);
            if let Some(Key::Enter | Key::Yes) = term.wait(Duration::from_millis(50))? {
                break;
            }
        }
    } else {
        term.line(&format!(
            "{DETAIL}waiting for sign-in (demo: signs in by itself)"
        ));
    }

    term.erase_since(mark);
    term.line(&summary(m, "Signed in", DEMO_ACCOUNT));
    term.flush()?;
    Ok(())
}

/// The fingerprint check. Returns whether the person confirmed the key.
fn confirm_key(term: &mut Term, rng: &mut Rng) -> Result<bool> {
    let m = term.mode();
    let words = demo_words(rng).join(" · ");
    let fingerprint = demo_fingerprint(rng);
    let question = "Same words?";

    let mark = term.mark();
    term.blank();
    term.line(&heading(
        m,
        &m.fg_bold(pal::AMBER, "◆"),
        "Is this your admin key?",
    ));
    term.line(&format!(
        "{DETAIL}{}",
        m.fg(
            pal::GREY,
            &format!("Wave on “{DEMO_DEVICE}” made this key. Only its public half came here.")
        )
    ));
    term.line(&format!(
        "{DETAIL}{}",
        m.fg(pal::GREY, "Wave shows four words. Check that they match:")
    ));
    term.blank();
    term.line(&format!("{DETAIL}    {}", m.bold(&words)));
    term.line(&format!("{DETAIL}    {}", m.fg(pal::DIM, &fingerprint)));
    term.blank();
    let yes = ask(term, question)?;
    term.erase_since(mark);

    if yes {
        term.line(&summary(
            m,
            "Admin key trusted",
            &format!("{words} · {DEMO_DEVICE}"),
        ));
    } else {
        term.line(&format!(
            "{INDENT}{} {}",
            m.fg_bold(pal::RED, "✗"),
            m.bold("Key not trusted · nothing was changed")
        ));
        term.line(&format!(
            "{DETAIL}{}",
            m.fg(
                pal::GREY,
                "Start again from Wave → Systems → Connect server."
            )
        ));
        no_terminal_note(term);
    }
    term.flush()?;
    Ok(yes)
}

/// Runs a list of steps with a live progress view. Steps run one after another;
/// each first checks whether it is already in place (shown as "kept").
fn run_steps(term: &mut Term, running: &str, done: &str, mut steps: Vec<Step>) -> Result<()> {
    let m = term.mode();
    let label_w = steps
        .iter()
        .map(|s| s.label.chars().count())
        .max()
        .unwrap_or(0);
    let finish = |s: &mut Step| {
        s.state = if s.already_done {
            StepState::Kept
        } else {
            StepState::Done
        };
    };

    term.blank();
    if !term.caps.live {
        term.line(&heading(m, "·", running));
        for s in &mut steps {
            finish(s);
            term.line(&step_row(m, s, label_w, 0.0));
        }
        term.flush()?;
        return Ok(());
    }

    term.hide_cursor();
    let mark = term.mark();
    let t0 = Instant::now();
    let total = steps.len();
    for i in 0..total {
        steps[i].state = StepState::Running;
        let started = Instant::now();
        loop {
            let elapsed = started.elapsed();
            let part = (elapsed.as_secs_f32() / steps[i].takes.as_secs_f32()).min(1.0);
            let t = if term.caps.animate {
                t0.elapsed().as_secs_f32()
            } else {
                0.0
            };
            let head = spread(
                &heading(m, &pulse_dot(m, t), running),
                &format!(
                    "{} {}",
                    progress_bar(m, (i as f32 + part) / total as f32, 20),
                    m.fg(pal::GREY, &format!("{}/{total}", i + 1))
                ),
                SCREEN_W,
            );
            let mut lines = vec![head];
            lines.extend(steps.iter().map(|s| step_row(m, s, label_w, t)));
            term.repaint(mark, &lines);
            if elapsed >= steps[i].takes {
                break;
            }
            term.wait(Duration::from_millis(40))?;
        }
        finish(&mut steps[i]);
    }

    let took = format!("{:.1} s", t0.elapsed().as_secs_f32());
    let head = spread(
        &heading(m, &m.fg_bold(pal::GREEN, "✓"), done),
        &format!("{} {}", progress_bar(m, 1.0, 20), m.fg(pal::GREY, &took)),
        SCREEN_W,
    );
    let mut lines = vec![head];
    lines.extend(steps.iter().map(|s| step_row(m, s, label_w, 0.0)));
    term.repaint(mark, &lines);
    term.flush()?;
    Ok(())
}

fn connected(term: &mut Term, host: &Host) -> Result<()> {
    let m = term.mode();
    term.blank();
    term.line(&heading(
        m,
        &m.fg_bold(pal::GREEN, "✓"),
        &format!("{} is connected to Wave", host.name),
    ));
    term.line(&format!(
        "{DETAIL}{}",
        m.fg(
            pal::GREY,
            "Your dev agent can work here now, with the rights set in Wave → Systems."
        )
    ));
    term.blank();
    let w = 10;
    term.line(&kv(
        m,
        DETAIL,
        "Rights",
        w,
        &format!("read · logs · health  {}", m.fg(pal::GREY, "default")),
    ));
    term.line(&kv(
        m,
        DETAIL,
        "Critical",
        w,
        &format!(
            "sudo, delete, firewall  {}",
            m.fg(pal::GREY, "always a click in Wave")
        ),
    ));
    term.line(&kv(
        m,
        DETAIL,
        "Revoke",
        w,
        &format!(
            "{}  {}",
            m.fg(pal::CYAN, "sudo stonewave revoke"),
            m.fg(pal::GREY, "works offline")
        ),
    ));
    term.line(&kv(m, DETAIL, "Log", w, &m.fg(pal::CYAN, "stonewave log")));
    term.blank();
    term.line(&dim_note(
        m,
        "demo finished · nothing on this machine was changed",
    ));
    term.flush()?;
    Ok(())
}

// ---------------------------------------------------------------- status

/// What `stonewave status` shows on every start after the first: no banner.
pub fn status(term: &mut Term, host: &Host) -> Result<Outcome> {
    let m = term.mode();
    term.blank();
    term.line(&banner::compact(m, &host.name));
    term.blank();

    let w = 13;
    let rows = |fill: f32| -> Vec<String> {
        let pct = |p: u8| (f32::from(p) * fill).round() as u8;
        vec![
            kv(
                m,
                INDENT,
                "Connection",
                w,
                &format!(
                    "{} {}",
                    m.fg_bold(pal::GREEN, "● online"),
                    m.fg(pal::GREY, "· relay eu-central · 38 ms · since 09.11. 14:02")
                ),
            ),
            kv(
                m,
                INDENT,
                "Agent",
                w,
                &format!(
                    "elara for Acme GmbH {}",
                    m.fg(pal::GREY, "· read, logs, health")
                ),
            ),
            kv(
                m,
                INDENT,
                "Admin keys",
                w,
                &format!("Office PC · iPhone {}", m.fg(pal::GREY, "· 2 devices")),
            ),
            kv(m, INDENT, "Disk", w, &meter(m, pct(41), 16)),
            kv(m, INDENT, "Memory", w, &meter(m, pct(38), 16)),
            kv(
                m,
                INDENT,
                "Load",
                w,
                &format!("0.31 {}", m.fg(pal::GREY, "· 4 cores")),
            ),
            kv(
                m,
                INDENT,
                "Last action",
                w,
                &format!(
                    "14:12  elara read the nginx log {}",
                    m.fg(pal::GREY, "· for Lena")
                ),
            ),
        ]
    };

    if term.caps.animate {
        term.hide_cursor();
        let mark = term.mark();
        let started = Instant::now();
        let fill_in = Duration::from_millis(600);
        loop {
            let t = (started.elapsed().as_secs_f32() / fill_in.as_secs_f32()).min(1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            term.repaint(mark, &rows(eased));
            if t >= 1.0 {
                break;
            }
            term.wait(Duration::from_millis(16))?;
        }
    } else {
        for r in rows(1.0) {
            term.line(&r);
        }
    }
    term.blank();
    term.line(&dim_note(
        m,
        "demo values · nothing was read from this machine",
    ));
    term.flush()?;
    Ok(Outcome::Done)
}

// ---------------------------------------------------------------- revoke

/// The local kill switch, as a demo.
pub fn revoke(term: &mut Term, host: &Host) -> Result<Outcome> {
    let m = term.mode();
    let question = "Revoke?";
    term.blank();
    term.line(&banner::compact(m, &host.name));

    let mark = term.mark();
    term.blank();
    term.line(&heading(
        m,
        &m.fg_bold(pal::AMBER, "◆"),
        &format!("Revoke all agent access on {}?", host.name),
    ));
    term.line(&format!(
        "{DETAIL}{}",
        m.fg(
            pal::GREY,
            "Ends agent sessions, removes the trusted admin keys, stops the daemon"
        )
    ));
    term.line(&format!(
        "{DETAIL}{}",
        m.fg(
            pal::GREY,
            "and locks stonewave-agent. The audit log stays. Works without internet."
        )
    ));
    term.blank();
    let yes = ask(term, question)?;
    term.erase_since(mark);

    if !yes {
        term.blank();
        term.line(&format!(
            "{INDENT}{}",
            m.fg(pal::GREY, "Nothing was changed.")
        ));
        no_terminal_note(term);
        term.flush()?;
        return Ok(Outcome::Declined);
    }

    let steps = vec![
        Step::new("Agent sessions", "2 ended", 500),
        Step::new("Admin keys", "2 removed from the trust list", 400),
        Step::new("Daemon", "stopped and disabled", 600),
        Step::new("Agent user", "stonewave-agent locked", 400),
        Step::new("Audit log", "revoke noted · log kept", 300),
    ];
    run_steps(term, "Revoking agent access", "Agent access revoked", steps)?;
    term.blank();
    term.line(&kv(
        m,
        DETAIL,
        "Again",
        7,
        &format!(
            "{}  {}",
            m.fg(pal::CYAN, "sudo stonewave"),
            m.fg(pal::GREY, "connects this server again")
        ),
    ));
    term.blank();
    term.line(&dim_note(
        m,
        "demo finished · nothing on this machine was changed",
    ));
    term.flush()?;
    Ok(Outcome::Done)
}

// ---------------------------------------------------------------- doctor

/// `stonewave doctor`: the real, read-only checks, with the compact header.
pub fn doctor(term: &mut Term, host: &Host) -> Result<Outcome> {
    let m = term.mode();
    term.blank();
    term.line(&banner::compact(m, &host.name));
    let checks = crate::doctor::show(term, &host.name, &crate::doctor::SYSTEM)?;
    crate::doctor::verdict(term, &checks)?;
    Ok(if crate::doctor::passed(&checks) {
        Outcome::Done
    } else {
        Outcome::Declined
    })
}

// ---------------------------------------------------------------- banner

pub fn banner_only(term: &mut Term) -> Result<Outcome> {
    banner::show(term)?;
    term.blank();
    term.flush()?;
    Ok(Outcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_name_and_version() {
        let text = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n";
        assert_eq!(parse_os_release(text).as_deref(), Some("Ubuntu 24.04"));
        assert_eq!(
            parse_os_release("PRETTY_NAME=\"Arch Linux\"\n").as_deref(),
            Some("Arch Linux")
        );
        assert_eq!(parse_os_release("").as_deref(), None);
    }

    #[test]
    fn os_release_ignores_similar_keys() {
        // NAME must not match PRETTY_NAME or CPE_NAME.
        let text = "CPE_NAME=\"cpe:/o:x\"\nNAME=Debian GNU/Linux\nVERSION_ID=\"12\"\n";
        assert_eq!(
            parse_os_release(text).as_deref(),
            Some("Debian GNU/Linux 12")
        );
    }

    #[test]
    fn demo_code_shape() {
        let mut rng = Rng::new(Some(7));
        for _ in 0..50 {
            let code = demo_code(&mut rng);
            assert_eq!(code.len(), 9);
            assert!(code.starts_with("WAVE-"));
            assert!(!code[5..].contains(['0', 'O', '1', 'I']));
        }
    }

    #[test]
    fn demo_is_reproducible_with_a_seed() {
        let a = demo_code(&mut Rng::new(Some(42)));
        let b = demo_code(&mut Rng::new(Some(42)));
        assert_eq!(a, b);
    }

    #[test]
    fn demo_words_differ() {
        let mut rng = Rng::new(Some(7));
        for _ in 0..200 {
            let w = demo_words(&mut rng);
            for i in 0..w.len() {
                assert!(!w[i + 1..].contains(&w[i]), "{w:?}");
            }
        }
    }

    #[test]
    fn fingerprint_shape() {
        let fp = demo_fingerprint(&mut Rng::new(Some(1)));
        assert_eq!(fp.len(), 7 + 43);
    }

    #[test]
    fn countdown_format() {
        assert_eq!(mmss(Duration::from_secs(15 * 60)), "15:00");
        assert_eq!(mmss(Duration::from_secs(61)), "1:01");
    }
}
