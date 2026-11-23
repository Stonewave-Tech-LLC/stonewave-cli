//! The long-lived sessions of an agent: one per room (D-254). A room's session is continued on every wake, so the agent keeps its context the way
//! one long working session would. One per room, not one for everything: a session that is shared by all rooms would carry what was said in a
//! narrow room (a team) into answers in a wide one (the company), and Wave keeps rooms apart on purpose.
//!
//! The file holds session ids and counters only: no text, no key. It is private to the user like the key file.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::claude::plain_session_id;
use super::key::atomic_write;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomSession {
    /// The session to continue; none before the first wake and after a deliberate restart.
    pub session: Option<String>,
    /// Wakes the current session has handled.
    pub wakes: u32,
    /// Unix time the session was started.
    pub started: u64,
    /// A handover note of the previous session (a path inside the working folder), read by the first wake of the next.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sessions {
    pub rooms: BTreeMap<String, RoomSession>,
}

impl Sessions {
    pub fn load(path: &Path) -> Sessions {
        let Ok(text) = fs::read_to_string(path) else {
            return Sessions::default();
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return Sessions::default();
        };
        let mut rooms = BTreeMap::new();
        for (room, r) in v
            .get("rooms")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            rooms.insert(
                room.clone(),
                RoomSession {
                    session: r
                        .get("session")
                        .and_then(Value::as_str)
                        .filter(|s| plain_session_id(s))
                        .map(str::to_owned),
                    wakes: r
                        .get("wakes")
                        .and_then(Value::as_u64)
                        .map_or(0, |n| n.min(u32::MAX as u64) as u32),
                    started: r.get("started").and_then(Value::as_u64).unwrap_or(0),
                    note: r.get("note").and_then(Value::as_str).map(str::to_owned),
                },
            );
        }
        Sessions { rooms }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let rooms: serde_json::Map<String, Value> = self
            .rooms
            .iter()
            .map(|(k, r)| (k.clone(), json!({ "session": r.session, "wakes": r.wakes, "started": r.started, "note": r.note })))
            .collect();
        let text = serde_json::to_string_pretty(&json!({ "version": 1, "rooms": rooms }))
            .unwrap_or_default();
        atomic_write(path, text.as_bytes(), 0o600)
    }

    pub fn get(&self, room: &str) -> RoomSession {
        self.rooms.get(room).cloned().unwrap_or_default()
    }

    pub fn put(&mut self, room: &str, s: RoomSession) {
        self.rooms.insert(room.to_owned(), s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_round_trips_and_drops_ids_that_are_not_plain() {
        let dir = std::env::temp_dir().join(format!("stonewave-sessions-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.sessions.json");
        let mut s = Sessions::default();
        s.put(
            "room-1",
            RoomSession {
                session: Some("d273d283-28f0-4a0b-8a50-40a463e7db5f".into()),
                wakes: 3,
                started: 100,
                note: Some("notes/x.md".into()),
            },
        );
        s.save(&p).unwrap();
        assert_eq!(Sessions::load(&p), s);
        fs::write(&p, r#"{"rooms":{"r":{"session":"../evil","wakes":2}}}"#).unwrap();
        let l = Sessions::load(&p);
        assert_eq!(l.get("r").session, None);
        assert_eq!(l.get("r").wakes, 2);
        assert_eq!(
            Sessions::load(&dir.join("missing.json")),
            Sessions::default()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            s.save(&p).unwrap();
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
