//! The agent key and the files it lives in.
//!
//! The key is a [`Key`]: it prints as `Key(..)`, has no `Display`, and only [`Key::expose`] gives the text (to curl's stdin, to the
//! key file, to the environment of a child). Every text shown to a person goes through [`scrub`], which cuts out anything that
//! looks like a key.
//!
//! Files: the folder is private (0700; on Windows only the current user via `icacls`), the key file is written atomically
//! (a temporary file in the same folder, created exclusively with mode 0600 and without following links, synced, then renamed).

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::exec::Exec;

pub const KEY_PREFIX: &str = "wak_";

/// An agent key (`wak_...`).
#[derive(Clone, PartialEq, Eq)]
pub struct Key(String);

impl Key {
    /// Shape check only: `wak_` and then letters, digits, `_`, `-`.
    pub fn parse(s: &str) -> Option<Key> {
        let s = s.trim();
        let body = s.strip_prefix(KEY_PREFIX)?;
        if (8..=400).contains(&body.len()) && body.bytes().all(is_key_byte) {
            Some(Key(s.to_owned()))
        } else {
            None
        }
    }

    /// The secret text. Only for the key file, curl's stdin and a child's environment.
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub fn for_test(s: &str) -> Key {
        Key::parse(s).expect("a test key")
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(..)")
    }
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Cuts every `wak_...` out of a text and drops control characters. Used on everything printed.
pub fn scrub(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(KEY_PREFIX) {
        out.push_str(&rest[..i]);
        let tail = &rest[i + KEY_PREFIX.len()..];
        let n = tail.bytes().take_while(|b| is_key_byte(*b)).count();
        out.push_str("wak_[removed]");
        rest = &tail[n..];
    }
    out.push_str(rest);
    out.chars()
        .map(|c| if c.is_control() && c != '\n' { '?' } else { c })
        .collect()
}

// ------------------------------------------------------------------ files

/// How a private file or folder is protected on Windows (where the mode bits mean nothing).
pub struct Acl<'a> {
    pub exec: &'a dyn Exec,
    pub user: &'a str,
}

fn icacls(acl: &Acl<'_>, path: &Path, dir: bool) -> io::Result<()> {
    if acl.user.is_empty() {
        return Err(io::Error::other("the user name is not known"));
    }
    check_text("account name", acl.user).map_err(io::Error::other)?;
    let grant = if dir {
        format!("{}:(OI)(CI)F", acl.user)
    } else {
        format!("{}:F", acl.user)
    };
    let out = acl.exec.run(
        "icacls",
        &[
            path.to_string_lossy().into_owned(),
            "/inheritance:r".into(),
            "/grant:r".into(),
            grant,
        ],
    )?;
    if out.ok {
        Ok(())
    } else {
        Err(io::Error::other(format!("icacls: {}", out.problem())))
    }
}

fn refuse_symlink(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is a link; refusing to write through a link",
                p.display()
            ),
        )),
        _ => Ok(()),
    }
}

/// Makes the folder (and its parents) and locks it down to the current user.
pub fn ensure_private_dir(dir: &Path, acl: &Acl<'_>) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    refuse_symlink(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        let _ = acl;
        match fs::symlink_metadata(dir) {
            Ok(m) => {
                if !m.is_dir() {
                    return Err(io::Error::other(format!(
                        "{} is not a folder",
                        dir.display()
                    )));
                }
                // SAFETY: geteuid has no arguments and cannot fail.
                if m.uid() != unsafe { libc::geteuid() } {
                    return Err(io::Error::other(format!(
                        "{} belongs to someone else",
                        dir.display()
                    )));
                }
                if m.permissions().mode() & 0o777 != 0o700 {
                    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(dir)?;
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let fresh = !dir.exists();
        if fresh {
            fs::create_dir(dir)?;
        }
        // always: a folder that was there before may have inherited wide rights
        icacls(acl, dir, true)
            .map_err(|e| io::Error::other(format!("cannot restrict {}: {e}", dir.display())))
    }
}

/// Writes `data` to `path` atomically, and the file has exactly `mode` afterwards (Unix), whatever mode the old file had.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    atomic_write_inner(path, data, mode, false)
}

/// Like [`atomic_write`] for a file that is somebody else's (an engine's config): an existing file keeps its own mode,
/// `mode` is only for a new one.
pub fn atomic_write_keep_mode(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    atomic_write_inner(path, data, mode, true)
}

fn atomic_write_inner(path: &Path, data: &[u8], mode: u32, keep_existing: bool) -> io::Result<()> {
    refuse_symlink(path)?;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no folder to write to"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?
        .to_string_lossy()
        .into_owned();
    let existing_mode = fs::metadata(path).ok().filter(|_| keep_existing);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut attempt = 0u32;
    let (tmp, mut file) = loop {
        let tmp = dir.join(format!(
            ".{name}.{}.{nanos}.{attempt}.tmp",
            std::process::id()
        ));
        match open_new(&tmp, mode) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 20 => attempt += 1,
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        file.write_all(data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // exactly the wanted mode (the umask may have made the new file stricter), or the old file's own
            let wanted = existing_mode
                .as_ref()
                .map_or(mode, |m| m.permissions().mode() & 0o777);
            file.set_permissions(fs::Permissions::from_mode(wanted))?;
        }
        #[cfg(not(unix))]
        let _ = (&existing_mode, mode);
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn open_new(path: &Path, mode: u32) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_new(path: &Path, _mode: u32) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Writes a private file (0600; on Windows only the current user may read it). A failure to restrict a file on Windows
/// after the write is returned as a warning, because the private folder already keeps others out.
pub fn write_private_file(path: &Path, data: &[u8], acl: &Acl<'_>) -> io::Result<Option<String>> {
    atomic_write(path, data, 0o600)?;
    if cfg!(windows) {
        if let Err(e) = icacls(acl, path, false) {
            return Ok(Some(format!(
                "could not tighten the rights of {}: {e}",
                path.display()
            )));
        }
    }
    Ok(None)
}

/// Checks that a file can be created next to `path` (so a write later will not fail for a boring reason).
pub fn check_writable(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".write-check.{}.tmp", std::process::id()));
    let f = open_new(&probe, 0o600)?;
    drop(f);
    fs::remove_file(&probe)
}

#[derive(Debug)]
pub enum ReadError {
    Missing,
    Unreadable(String),
    NotAKey,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Missing => write!(f, "the key file is missing"),
            ReadError::Unreadable(e) => write!(f, "the key file cannot be read ({e})"),
            ReadError::NotAKey => write!(f, "the key file does not hold a key"),
        }
    }
}

pub fn read_key_file(path: &Path) -> Result<Key, ReadError> {
    match fs::read_to_string(path) {
        Ok(t) => Key::parse(&t).ok_or(ReadError::NotAKey),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(ReadError::Missing),
        Err(e) => Err(ReadError::Unreadable(scrub(&e.to_string()))),
    }
}

/// The Unix mode of a path as `0600`, for reports. None where there are no mode bits.
pub fn mode_text(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = fs::metadata(path).ok()?;
        Some(format!("{:04o}", m.permissions().mode() & 0o7777))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Is a file private? Unix: no rights for group and others. Elsewhere: unknown (None).
pub fn is_private(path: &Path) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = fs::metadata(path).ok()?;
        Some(m.permissions().mode() & 0o077 == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

pub fn remove_if_there(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// A character that cannot go safely into a service file, a script or a command line: control and invisible (format) characters,
/// the double quote, and the curly look-alikes of the single quote that PowerShell reads as one (U+2018 to U+201B). The straight apostrophe is not in here: every writer quotes it.
pub fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '"' | '\u{2018}'..='\u{201B}' | '\u{FFFD}' | '\u{AD}' | '\u{61C}' | '\u{180E}'
                | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}'
        )
}

/// The first character of `s` that `unsafe_char` refuses, if any.
fn first_unsafe(s: &str) -> Option<char> {
    s.chars().find(|c| unsafe_char(*c))
}

/// A path that stays out of the way of anything a shell, a script or a config file could take apart. The straight apostrophe is
/// allowed (every writer quotes it); the curly quotes, the double quote, control and invisible characters are refused.
pub fn plain_path(p: &Path) -> Result<PathBuf, String> {
    check_text("path", &p.to_string_lossy())?;
    Ok(p.to_path_buf())
}

/// The same check for a name that ends up in a file or a command (for example the Windows account name).
pub fn check_text(what: &str, s: &str) -> Result<(), String> {
    match first_unsafe(s) {
        None => Ok(()),
        Some(c) => Err(format!(
            "the {what} {} has the character U+{:04X}, which cannot be used safely in a service file or script; use a {what} without it",
            scrub(&s.replace(unsafe_char, "?")),
            c as u32
        )),
    }
}

#[cfg(test)]
pub mod testdir {
    //! A folder under the system's temp folder, removed again at the end of the test.
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    pub struct TempDir(PathBuf);
    static N: AtomicU32 = AtomicU32::new(0);

    impl TempDir {
        pub fn new(label: &str) -> TempDir {
            let p = std::env::temp_dir().join(format!(
                "swagent-{label}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdir::TempDir;
    use super::*;
    use crate::agent::exec::fake::Fake;

    const KEY: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_s3cretS3cretS3cret";

    #[test]
    fn key_is_secret_in_debug_and_shape_checked() {
        let k = Key::parse(KEY).unwrap();
        assert_eq!(format!("{k:?}"), "Key(..)");
        assert!(!format!("{:?}", Some(&k)).contains("s3cret"));
        assert_eq!(k.expose(), KEY);
        assert!(
            Key::parse(&format!("{KEY}\n")).is_some(),
            "a trailing newline is fine"
        );
        for bad in [
            "",
            "wak_",
            "wak_short",
            "tok_aaaaaaaaaaaa",
            "wak_aaaa aaaaaaaa",
            "wak_aaaaaaaa\"x",
        ] {
            assert!(Key::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn scrub_cuts_keys_out_of_any_text() {
        assert_eq!(
            scrub(&format!("failed with {KEY} in the middle")),
            "failed with wak_[removed] in the middle"
        );
        assert_eq!(
            scrub("a wak_x and wak_y-z_1.b"),
            "a wak_[removed] and wak_[removed].b"
        );
        assert_eq!(scrub("no key here"), "no key here");
        assert_eq!(scrub("a\u{1b}[31mb"), "a?[31mb");
        assert_eq!(scrub("wak_"), "wak_[removed]");
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let t = TempDir::new("aw");
        let f = t.path().join("x.key");
        atomic_write(&f, b"one", 0o600).unwrap();
        atomic_write(&f, b"two", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "two");
        let names: Vec<_> = fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["x.key".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn modes_are_0700_and_0600_and_fixed_when_too_wide() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new("modes");
        let fake = Fake::new(&[]);
        let acl = Acl {
            exec: &fake,
            user: "x",
        };
        let dir = t.path().join("a").join("wave");
        ensure_private_dir(&dir, &acl).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir, &acl).unwrap();
        assert_eq!(mode(&dir), 0o700, "a wide folder is tightened");
        let f = dir.join("agent-x.key");
        assert_eq!(write_private_file(&f, KEY.as_bytes(), &acl).unwrap(), None);
        assert_eq!(mode(&f), 0o600);
        // an existing file keeps its stricter or own mode on replace
        write_private_file(&f, b"again", &acl).unwrap();
        assert_eq!(mode(&f), 0o600);
        assert_eq!(is_private(&f), Some(true));
        assert!(fake.lines().is_empty(), "no icacls on Unix");
    }

    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlink_is_refused() {
        let t = TempDir::new("link");
        let fake = Fake::new(&[]);
        let acl = Acl {
            exec: &fake,
            user: "x",
        };
        let victim = t.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        let link = t.path().join("agent-x.key");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let e = write_private_file(&link, b"secret", &acl).unwrap_err();
        assert!(e.to_string().contains("link"), "{e}");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        // a folder that is a link is refused too
        let real = t.path().join("real");
        fs::create_dir(&real).unwrap();
        let dlink = t.path().join("wave");
        std::os::unix::fs::symlink(&real, &dlink).unwrap();
        assert!(ensure_private_dir(&dlink, &acl).is_err());
    }

    #[test]
    fn windows_rights_go_through_icacls_only_for_the_current_user() {
        let t = TempDir::new("acl");
        let fake = Fake::new(&[]);
        let acl = Acl {
            exec: &fake,
            user: "PC\\ada",
        };
        if cfg!(windows) {
            let dir = t.path().join("wave");
            ensure_private_dir(&dir, &acl).unwrap();
            let f = dir.join("agent-x.key");
            write_private_file(&f, b"k", &acl).unwrap();
            let lines = fake.lines();
            assert_eq!(lines.len(), 2, "{lines:?}");
            assert!(
                lines[0].ends_with("/inheritance:r /grant:r PC\\ada:(OI)(CI)F"),
                "{lines:?}"
            );
            assert!(
                lines[1].ends_with("/inheritance:r /grant:r PC\\ada:F"),
                "{lines:?}"
            );
        }
    }

    /// The real icacls of this Windows machine, on a folder in the temp folder: afterwards only this account is in the list.
    #[cfg(windows)]
    #[test]
    fn windows_real_icacls_leaves_only_the_current_user() {
        use crate::agent::exec::System;
        let user = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").unwrap(),
            std::env::var("USERNAME").unwrap()
        );
        let acl = Acl {
            exec: &System,
            user: &user,
        };
        let t = TempDir::new("icacls");
        let dir = t.path().join("wave");
        ensure_private_dir(&dir, &acl).unwrap();
        let file = dir.join("agent-x.key");
        assert_eq!(
            write_private_file(&file, KEY.as_bytes(), &acl).unwrap(),
            None
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), KEY);
        for p in [&dir, &file] {
            let out = System
                .run("icacls", &[p.to_string_lossy().into_owned()])
                .unwrap();
            assert!(out.ok, "{}", out.problem());
            let entries: Vec<&str> = out
                .stdout
                .lines()
                .map(str::trim)
                .filter(|l| l.contains(":(") && l.contains(')'))
                .collect();
            assert_eq!(entries.len(), 1, "only one account: {:?}", out.stdout);
            assert!(
                entries[0]
                    .to_lowercase()
                    .contains(&std::env::var("USERNAME").unwrap().to_lowercase()),
                "{entries:?}"
            );
        }
    }

    #[test]
    fn read_key_file_tells_what_is_wrong_without_the_content() {
        let t = TempDir::new("rd");
        let f = t.path().join("k.key");
        assert!(matches!(read_key_file(&f), Err(ReadError::Missing)));
        fs::write(&f, "not a key at all, wak_ no").unwrap();
        let e = read_key_file(&f).unwrap_err();
        assert!(matches!(e, ReadError::NotAKey));
        assert!(!e.to_string().contains("not a key at all"));
        fs::write(&f, format!("{KEY}\n")).unwrap();
        assert!(read_key_file(&f).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_loosened_file_is_written_back_as_0600_but_somebody_elses_config_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new("loose");
        let fake = Fake::new(&[]);
        let acl = Acl {
            exec: &fake,
            user: "x",
        };
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let f = t.path().join("agent-x.key");
        write_private_file(&f, KEY.as_bytes(), &acl).unwrap();
        for loose in [0o644, 0o666, 0o755, 0o400] {
            fs::set_permissions(&f, fs::Permissions::from_mode(loose)).unwrap();
            write_private_file(&f, b"wak_newnewnewnew", &acl).unwrap();
            assert_eq!(mode(&f), 0o600, "after {loose:o}");
        }
        assert_eq!(fs::read_to_string(&f).unwrap(), "wak_newnewnewnew");
        // the state file and the others written with atomic_write get the mode asked for
        let g = t.path().join("state.json");
        fs::write(&g, "{}").unwrap();
        fs::set_permissions(&g, fs::Permissions::from_mode(0o666)).unwrap();
        atomic_write(&g, b"{}", 0o600).unwrap();
        assert_eq!(mode(&g), 0o600);
        // an engine's own config is not ours to tighten or loosen
        let c = t.path().join("config.toml");
        fs::write(&c, "a = 1\n").unwrap();
        fs::set_permissions(&c, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write_keep_mode(&c, b"a = 2\n", 0o600).unwrap();
        assert_eq!(mode(&c), 0o644);
        let d = t.path().join("new.toml");
        atomic_write_keep_mode(&d, b"a = 2\n", 0o600).unwrap();
        assert_eq!(mode(&d), 0o600);
    }

    #[test]
    fn plain_path_refuses_what_breaks_service_files() {
        assert!(plain_path(Path::new("/home/a b/x")).is_ok());
        assert!(plain_path(Path::new("/home/a\"b")).is_err());
        assert!(plain_path(Path::new("/home/a\nb")).is_err());
        // the apostrophe is fine (it is quoted everywhere); the look-alikes PowerShell takes for one are not
        assert!(plain_path(Path::new("/home/it's")).is_ok());
        for c in [
            '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}', '\u{200B}', '\u{202E}', '\u{FEFF}',
            '\u{AD}', '\0',
        ] {
            let p = format!("/home/O{c}Brien");
            let e = plain_path(Path::new(&p)).unwrap_err();
            assert!(e.contains("U+"), "{e}");
        }
        // a crafted path is refused, and the message does not print the quote
        let crafted = format!("C:\\x{}; calc; {}", '\u{2019}', '\u{2019}');
        let e = plain_path(Path::new(&crafted)).unwrap_err();
        assert!(e.contains("U+2019") && !e.contains('\u{2019}'), "{e}");
        assert!(check_text("account name", "PC\\ada").is_ok());
        assert!(check_text("account name", "PC\\a\u{2019}da").is_err());
    }
}
