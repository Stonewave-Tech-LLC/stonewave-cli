//! Codex: the `wave` server in `~/.codex/config.toml` (or `$CODEX_HOME/config.toml`), written by hand because `codex mcp add`
//! cannot set a token variable for a remote server.
//!
//! The block is marked, so it can be replaced and removed without touching anything else:
//!
//! ```toml
//! # stonewave: begin wave
//! [mcp_servers.wave]
//! url = "https://wave.example/mcp"
//! bearer_token_env_var = "WAVE_AGENT_KEY"
//! # stonewave: end wave
//! ```
//!
//! No TOML library: only whole lines are looked at, and a `wave` server defined outside the markers is never changed.

use std::fs;
use std::io;
use std::path::Path;

use super::key::{atomic_write, atomic_write_keep_mode};

pub const BEGIN: &str = "# stonewave: begin wave";
pub const END: &str = "# stonewave: end wave";
pub const BACKUP_SUFFIX: &str = ".stonewave-bak";

#[derive(Debug, PartialEq, Eq)]
pub enum Problem {
    /// A `[mcp_servers.wave]` table that is not ours.
    Foreign,
    /// A begin marker without its end marker (or the other way round).
    Damaged,
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn toml_string(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            c if c.is_control() => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The block, lines joined with `eol`, without a trailing line break.
pub fn block(mcp_url: &str, eol: &str) -> String {
    [
        BEGIN.to_owned(),
        "[mcp_servers.wave]".to_owned(),
        format!("url = {}", toml_string(mcp_url)),
        "bearer_token_env_var = \"WAVE_AGENT_KEY\"".to_owned(),
        END.to_owned(),
    ]
    .join(eol)
}

/// Where the block is: the index of the begin and the end line.
fn find_block(lines: &[&str]) -> Result<Option<(usize, usize)>, Problem> {
    let begin = lines.iter().position(|l| l.trim() == BEGIN);
    let end = lines.iter().position(|l| l.trim() == END);
    match (begin, end) {
        (None, None) => Ok(None),
        (Some(b), Some(e)) if b < e => Ok(Some((b, e))),
        _ => Err(Problem::Damaged),
    }
}

/// Does a line (outside our block) start a `wave` server table or key?
fn is_foreign_wave(line: &str) -> bool {
    let t = line.trim();
    let compact: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    for head in [
        "[mcp_servers.wave",
        "[mcp_servers.\"wave\"",
        "[mcp_servers.'wave'",
    ] {
        if let Some(rest) = compact.strip_prefix(head) {
            // `[mcp_servers.wave]` and `[mcp_servers.wave.env]`, not `[mcp_servers.wavefront]`
            if rest.starts_with(']') || rest.starts_with('.') {
                return true;
            }
        }
    }
    if head_matches(&compact, "mcp_servers.wave") {
        return true;
    }
    // inline: `mcp_servers = { wave = { ... } }`
    compact.starts_with("mcp_servers={") && compact.contains("wave=")
}

fn head_matches(compact: &str, key: &str) -> bool {
    match compact.strip_prefix(key) {
        Some(rest) => rest.starts_with('=') || rest.starts_with('.'),
        None => false,
    }
}

/// The text with our block inserted (or replaced).
pub fn insert(existing: &str, mcp_url: &str) -> Result<String, Problem> {
    let eol = eol_of(existing);
    let lines: Vec<&str> = existing.lines().collect();
    let found = find_block(&lines)?;
    let outside = |i: usize| found.is_none_or(|(b, e)| i < b || i > e);
    if lines
        .iter()
        .enumerate()
        .any(|(i, l)| outside(i) && is_foreign_wave(l))
    {
        return Err(Problem::Foreign);
    }
    let new_block = block(mcp_url, eol);
    let mut out = String::new();
    match found {
        Some((b, e)) => {
            for l in &lines[..b] {
                out.push_str(l);
                out.push_str(eol);
            }
            out.push_str(&new_block);
            out.push_str(eol);
            for l in &lines[e + 1..] {
                out.push_str(l);
                out.push_str(eol);
            }
        }
        None => {
            out.push_str(existing);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str(eol);
            }
            if !out.is_empty() && !out.ends_with("\n\n") && !out.ends_with("\r\n\r\n") {
                out.push_str(eol);
            }
            out.push_str(&new_block);
            out.push_str(eol);
        }
    }
    Ok(out)
}

/// The text without our block. None when there is no block. A blank line left behind by the insert is removed with it.
pub fn remove(existing: &str) -> Result<Option<String>, Problem> {
    let eol = eol_of(existing);
    let lines: Vec<&str> = existing.lines().collect();
    let Some((b, e)) = find_block(&lines)? else {
        return Ok(None);
    };
    let mut keep: Vec<&str> = lines[..b].to_vec();
    if keep.last().is_some_and(|l| l.trim().is_empty()) && lines[e + 1..].is_empty() {
        keep.pop();
    }
    keep.extend_from_slice(&lines[e + 1..]);
    let mut out = keep.join(eol);
    if !keep.is_empty() {
        out.push_str(eol);
    }
    Ok(Some(out))
}

/// Is our block in this text?
pub fn has_block(existing: &str) -> bool {
    matches!(
        find_block(&existing.lines().collect::<Vec<_>>()),
        Ok(Some(_))
    )
}

// ------------------------------------------------------------------ the file

#[derive(Debug, PartialEq, Eq)]
pub enum Done {
    Added,
    Replaced,
    Unchanged,
}

#[derive(Debug)]
pub enum FileError {
    Problem(Problem),
    Io(String),
}

impl From<io::Error> for FileError {
    fn from(e: io::Error) -> Self {
        FileError::Io(super::key::scrub(&e.to_string()))
    }
}

/// Adds or updates the block in `config`. The first change to an existing file makes `config.toml.stonewave-bak`.
pub fn install(config: &Path, mcp_url: &str) -> Result<Done, FileError> {
    let existing = match fs::read_to_string(config) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let text = existing.clone().unwrap_or_default();
    let new = insert(&text, mcp_url).map_err(FileError::Problem)?;
    if existing.as_deref() == Some(new.as_str()) {
        return Ok(Done::Unchanged);
    }
    if let Some(dir) = config.parent() {
        fs::create_dir_all(dir)?;
    }
    // the first change to a file that is someone else's; a file that holds our block already was backed up then
    if existing.is_some() && !has_block(&text) {
        let bak = backup_path(config);
        if !bak.exists() {
            // `copy` keeps the content; the copy is as private as the original is not necessarily, so write it 0600
            atomic_write(&bak, text.as_bytes(), 0o600)?;
        }
    }
    atomic_write_keep_mode(config, new.as_bytes(), 0o600)?;
    Ok(if has_block(&text) {
        Done::Replaced
    } else {
        Done::Added
    })
}

/// Removes our block. `Ok(false)` when there was none (or no file).
pub fn uninstall(config: &Path) -> Result<bool, FileError> {
    let text = match fs::read_to_string(config) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    match remove(&text).map_err(FileError::Problem)? {
        None => Ok(false),
        Some(new) => {
            atomic_write_keep_mode(config, new.as_bytes(), 0o600)?;
            Ok(true)
        }
    }
}

pub fn backup_path(config: &Path) -> std::path::PathBuf {
    let mut s = config.as_os_str().to_owned();
    s.push(BACKUP_SUFFIX);
    s.into()
}

#[cfg(test)]
mod tests {
    use super::super::key::testdir::TempDir;
    use super::*;

    const URL: &str = "https://wave.test/mcp";

    #[test]
    fn block_text_is_exact() {
        assert_eq!(
            block(URL, "\n"),
            "# stonewave: begin wave\n[mcp_servers.wave]\nurl = \"https://wave.test/mcp\"\nbearer_token_env_var = \"WAVE_AGENT_KEY\"\n# stonewave: end wave"
        );
        assert!(!block(URL, "\n").contains("wak_"));
        assert!(block("https://a\"b\\c/mcp", "\n").contains(r#"url = "https://a\"b\\c/mcp""#));
    }

    #[test]
    fn inserts_into_empty_and_into_foreign_content() {
        let new = insert("", URL).unwrap();
        assert_eq!(new, format!("{}\n", block(URL, "\n")));
        let old = "model = \"gpt\"\n\n[mcp_servers.other]\nurl = \"https://x/mcp\"\n";
        let new = insert(old, URL).unwrap();
        assert!(new.starts_with(old), "foreign text stays byte for byte");
        assert!(new.ends_with(&format!("\n{}\n", block(URL, "\n"))));
        // no trailing newline in the original
        let new = insert("a = 1", URL).unwrap();
        assert!(
            new.starts_with("a = 1\n\n# stonewave: begin wave\n"),
            "{new:?}"
        );
    }

    #[test]
    fn insert_is_idempotent_and_replaces_the_old_block() {
        let once = insert("a = 1\n", URL).unwrap();
        assert_eq!(insert(&once, URL).unwrap(), once);
        let moved = insert(&once, "https://other.test/mcp").unwrap();
        assert!(moved.contains("https://other.test/mcp"));
        assert!(!moved.contains("https://wave.test/mcp"));
        assert_eq!(moved.matches(BEGIN).count(), 1);
        assert!(moved.starts_with("a = 1\n"));
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let new = insert("a = 1\r\nb = 2\r\n", URL).unwrap();
        assert!(!new.replace("\r\n", "").contains('\n'), "{new:?}");
        assert_eq!(insert(&new, URL).unwrap(), new);
    }

    #[test]
    fn a_foreign_wave_table_is_never_touched() {
        for foreign in [
            "[mcp_servers.wave]\nurl = \"https://mine/mcp\"\n",
            "[mcp_servers.wave.env]\nX = \"1\"\n",
            "[ mcp_servers . wave ]\nurl = \"u\"\n",
            "[mcp_servers.\"wave\"]\nurl = \"u\"\n",
            "mcp_servers.wave.url = \"u\"\n",
            "mcp_servers = { wave = { url = \"u\" } }\n",
        ] {
            assert_eq!(insert(foreign, URL), Err(Problem::Foreign), "{foreign}");
        }
        // names that only start the same way are fine
        let ok = "[mcp_servers.wavefront]\nurl = \"u\"\n[mcp_servers.other.wave]\nurl = \"u\"\n";
        assert!(insert(ok, URL).is_ok());
        // our own block does not count as foreign, a foreign one next to it does
        let ours = insert("", URL).unwrap();
        assert!(insert(&format!("{ours}[mcp_servers.wave.env]\nX=\"1\"\n"), URL).is_err());
    }

    #[test]
    fn a_half_block_is_damaged() {
        assert_eq!(
            insert("# stonewave: begin wave\nx = 1\n", URL),
            Err(Problem::Damaged)
        );
        assert_eq!(
            remove("x = 1\n# stonewave: end wave\n"),
            Err(Problem::Damaged)
        );
        assert_eq!(
            insert("# stonewave: end wave\n# stonewave: begin wave\n", URL),
            Err(Problem::Damaged)
        );
    }

    #[test]
    fn remove_takes_only_the_block() {
        let old = "a = 1\n\n[mcp_servers.other]\nurl = \"x\"\n";
        let with = insert(old, URL).unwrap();
        assert_eq!(remove(&with).unwrap().unwrap(), old);
        let in_the_middle = format!("a = 1\n{}\nb = 2\n", block(URL, "\n"));
        assert_eq!(remove(&in_the_middle).unwrap().unwrap(), "a = 1\nb = 2\n");
        assert_eq!(remove(old).unwrap(), None);
        assert_eq!(remove(&insert("", URL).unwrap()).unwrap().unwrap(), "");
    }

    #[test]
    fn file_install_backup_replace_remove() {
        let t = TempDir::new("codex");
        let cfg = t.path().join(".codex").join("config.toml");
        // no file yet: created, no backup
        assert_eq!(install(&cfg, URL).unwrap(), Done::Added);
        assert!(!backup_path(&cfg).exists());
        assert_eq!(install(&cfg, URL).unwrap(), Done::Unchanged);
        assert_eq!(install(&cfg, "https://b.test/mcp").unwrap(), Done::Replaced);
        assert!(uninstall(&cfg).unwrap());
        assert!(!uninstall(&cfg).unwrap(), "second time: nothing there");
        // a file with content: the first change makes the backup, later ones keep it
        fs::write(&cfg, "model = \"x\"\n").unwrap();
        assert_eq!(install(&cfg, URL).unwrap(), Done::Added);
        assert_eq!(
            fs::read_to_string(backup_path(&cfg)).unwrap(),
            "model = \"x\"\n"
        );
        install(&cfg, "https://c.test/mcp").unwrap();
        assert_eq!(
            fs::read_to_string(backup_path(&cfg)).unwrap(),
            "model = \"x\"\n"
        );
        assert!(uninstall(&cfg).unwrap());
        assert_eq!(fs::read_to_string(&cfg).unwrap(), "model = \"x\"\n");
    }

    #[test]
    fn file_with_a_foreign_wave_table_is_left_alone() {
        let t = TempDir::new("codex-foreign");
        let cfg = t.path().join("config.toml");
        let text = "[mcp_servers.wave]\nurl = \"https://mine/mcp\"\n";
        fs::write(&cfg, text).unwrap();
        assert!(matches!(
            install(&cfg, URL),
            Err(FileError::Problem(Problem::Foreign))
        ));
        assert_eq!(fs::read_to_string(&cfg).unwrap(), text);
        assert!(!backup_path(&cfg).exists());
        assert!(!uninstall(&cfg).unwrap());
        assert_eq!(fs::read_to_string(&cfg).unwrap(), text);
    }
}
