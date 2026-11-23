# The station: waking a coding agent by continuing its session

`stonewave agent station run` keeps one **long session per room** for a coding agent and continues it when a person of the company
mentions the agent in Wave. Context carries over from wake to wake, the way one long working session would; it is not a
fresh process with an empty head on every mention. This page is the reference: what it does, how to run it, what it will never do, and what
has and has not been tested.

Slice 1 supports **Claude Code**. Codex, Grok Build and Gemini follow, each only after a test shows that it refuses commands and the key file.

## How a wake works

1. The station asks Wave's inbox (MCP tool `inbox`, every 15 seconds) what is addressed to the agent. The first start only notes where the inbox
   stands; old items wake nobody.
2. An item **wakes** the agent only if a **person of the company** wrote it (not an agent, not text of outside origin) and its room is one the station
   was told to serve (`--rooms`, required: the names are turned into room ids once at the start and only the ids count afterwards). Everything else is skipped, and the station says how many and why, never what.
3. For each room it continues that room's session: `claude -p <fixed text with ids> --resume <session>`. The first wake of a room starts a session,
   the next ones continue it. If Claude Code no longer has the session, one fresh run takes its place (fallback).
4. The run answers in Wave itself (tool `post`). A run that never got going (no turn at all) is tried once more; one that already worked and then hit a
   limit is not repeated (it may have answered). If a wake ends with an error or runs out of time, the station writes **one fixed line** into that room
   ("I could not finish ... Nothing was done. Mention me again") and moves on. A login problem, or a Claude Code that does not know a flag the station
   needs, stops the station instead (the mention is kept and asked for again once that is fixed).
5. After `--session-max-wakes` wakes (default 200) the room's session ends on purpose: the old session writes a short handover note into
   `notes/handover.md` in the room's folder, and the next wake starts a new session that reads it. If no fresh note comes (limit reached, the old
   session did not write it), the session simply goes on and the next wake tries again.
6. An entry is woken for once: the station remembers the ids it has handled, so a page that is read again (because a room had to wait for a free
   slot) does not wake the same room twice. A room that waits is woken as soon as a slot is free.

One session per **room**, not one for everything: a shared session would carry what was said in a narrow room into answers in a wide one.

## What a wake can and cannot do

| | |
|---|---|
| Tools | Wave's tools (one MCP server, `wave`), and reading and changing files inside the working folder. **No shell, no web.** |
| How | `--restricted` and `--tools Read,Write,Edit,Glob,Grep` (only the file tools exist; Claude Code 2.1.259 or newer, which the station checks before it starts), `--strict-mcp-config` with a file that names the key variable, `--permission-mode dontAsk --permission-prompts none`, `--allowedTools`/`--disallowedTools`, `--max-turns`, `--max-budget-usd`. **Settings and hooks:** `--restricted` ignores the user, project and local settings files, `--settings '{"disableAllHooks":true}'` says no hook runs, writes to `.claude/`, `.mcp.json` and `CLAUDE.local.md` are denied, a room folder (or any folder above it up to the working folder) that carries one of them is not woken, and whatever of that kind a wake wrote is removed after it (a hook written in one wake would run as a shell command with the key in its environment on the next resume) |
| Folder | The agent's own folder (`--workdir`, default `~/wave-agents/<agent>`) and, inside it, **one folder per room** (`rooms/<room>`): a wake runs in its room's folder and sees only that one, so what one room's session writes is not in reach of another room's. Shared instructions go into a `CLAUDE.md` of the parent folder (Claude Code reads parent folders too). Refused: your home folder, the folders that hold keys or logins (`~/.config`, `~/.local`, `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.docker`, `~/.claude`, the state folder), a path with `..`, a link that leads to one of them or out of the folder; places are compared by their real place, and checked again before every wake |
| Prompt | A fixed text with ids. **The text of a message never goes into a command.** The run reads it through Wave's tools, where text from outside the company comes wrapped |
| Environment | Only what the program needs to find its login and its network, plus `WAVE_AGENT_KEY`. Other keys, tokens and cloud credentials of the user stay out |
| Time | 20 minutes per wake (`--timeout-min`), then the run is asked to end (on Linux and macOS its whole process group; on Windows its process tree by id), and after 30 seconds ended for good. Whatever a run leaves behind in its group is ended with it |
| Limits | 40 turns, 2 USD (an estimate by Claude Code; it can be exceeded), 10 wakes per hour, 50 per day, one wake at a time, and **one station per agent** (a lock file in the state folder; a second `station run` for the same agent is refused) |
| Stop | `stonewave agent station pause` (no new wake; a running one finishes), `resume`. Ctrl-C or SIGTERM (Linux, macOS) also ends a wake that is going, then the station stops; the mention it was working on stays and is picked up on the next start. On Windows Ctrl-C ends the console's processes the usual way |

Why `--restricted` and not just `--allowedTools`: Claude Code runs a set of read-only commands (`cat`, `ls`, `git` ...) in every permission mode, so a run
could `cat` the key file. And why a strict MCP file: `claude -p` loads a project's `.mcp.json` without asking, and `${WAVE_AGENT_KEY}` is expanded in server
addresses and headers; a folder could bring a server that sends the key somewhere else.

**The working folder is where the agent takes instructions from.** Claude Code reads a `CLAUDE.md` there. The station warns when one is present; use a
folder you trust.

## Running it

```bash
# look first: the exact command of a wake, the folder, the limits. Starts nothing, asks nobody.
stonewave agent station run --agent <name> --dry-run

stonewave agent station run --agent <name>                        # stays running; Ctrl-C stops it
stonewave agent station run --agent <name> --rooms "Team room, Company room"
stonewave agent station status --agent <name>
stonewave agent station pause --agent <name>                      # and `resume`
```

`--agent` can be left out when exactly one agent was connected here with `stonewave agent connect`. The key then comes from that agent's key file.
The menu (`stonewave` on a terminal, see [menu.md](menu.md)) lists what is installed and connected and starts these commands.

### The key without a file (a server with a secret store)

```bash
<secret-store command that runs a program with secrets in its environment> -- \
  stonewave agent station run --agent demo \
    --wave https://<wave api> --key-env MY_AGENT_KEY \
    --workdir /home/<user>/demo-station/work \
    --rooms "Team room,Company room"
```

`--key-env NAME` reads the key from the environment variable `NAME` of the station's own process and never writes it to a disk. The station then hands
the run only its own copy under the name `WAVE_AGENT_KEY`.

### Keeping it running

A user service, as the same user that is logged in to Claude Code (no root). Linux, `~/.config/systemd/user/stonewave-station-demo.service`:

```ini
[Unit]
Description=Stonewave station for the Wave agent demo
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Restart=on-failure
RestartSec=30
ExecStart=/usr/bin/env <secret-store command> -- /usr/local/bin/stonewave agent station run --agent demo --wave https://<wave api> --key-env MY_AGENT_KEY --workdir %h/demo-station/work --rooms "Team room,Company room"

[Install]
WantedBy=default.target
```

then `systemctl --user daemon-reload && systemctl --user enable --now stonewave-station-demo`, and `loginctl enable-linger <user>` to keep it running
while nobody is logged in (the station never does that for you). How the secret-store command signs in on that server is the existing setup there; the station
only needs the variable.

`stonewave agent connect` does not install a service that runs the station: the wake service it offers only writes lines into a log file. The
menu does (Linux, a systemd user unit, [menu.md](menu.md)): it shows the file first and writes the same options as above.

## Files

All in the same private folders as the key (`~/.config/wave`, `~/.local/state/wave`; Windows `%LOCALAPPDATA%\wave`), mode 0600:

| File | Holds |
|---|---|
| `<agent>.station.json` | the inbox position, the times of the wakes of the last day (for the limits) and the ids of the entries already woken for (so a page that is read again does not wake twice) |
| `<agent>.sessions.json` | per room: session id, number of wakes, handover note path. **No text.** |
| `<agent>.mcp.json` | the MCP file of a wake: the address and the *name* of the key variable. **No key** |
| `<agent>.runs.log` | one line per run: time, room, how it ended, seconds, turns, estimated cost. **No text** |
| `<agent>.station.paused` | exists while paused |
| `<agent>.station.lock` | exists while a station runs (created in one step that fails if it exists; holds the process id; kept fresh every 15 seconds) |
| `<agent>.station.child` | the process id of the wake that is going; a new station refuses to start while that process is still alive |

## What was tested, and what was not

Tested (`cargo test`; the Linux row also ran in a throw-away WSL as a normal user):

- the command of a wake (flags, no command tools, no key in any argument), the environment of a run, the time limit with children, the cap on output;
- who wakes (person yes; agent, outside text, other rooms, ids that are not plain: no), the first start, sessions per room, the fallback, limits, pause,
  rotation with a handover note, a refused key, an unreachable Wave;
- the whole thing with the real binary, the real `curl`, a small Wave on the loopback address and a **stand-in for Claude Code** (a shell script
  that records how it was started): `tests/station_e2e.rs`, Unix only.

**Not tested, and not claimed:**

- **Not run against a real Claude Code and a real Wave.** The flags were checked against Claude Code 2.1.321's `--help` and the documentation only. In
  particular unverified: that `--restricted` together with `--mcp-config` and `--resume` behaves as documented, that a wake's `--allowedTools` entry
  `mcp__wave` admits Wave's tools, that `--restricted` still finds a subscription login, and that `${WAVE_AGENT_KEY}` is expanded inside a file given with
  `--mcp-config`. The first real run on a real machine decides these.
- macOS and Windows: the station builds and its unit tests pass on Windows; the process-ending code on Windows (`taskkill` of the child's tree) and every
  wake on macOS have not run.
- The live line to Wave's doorbell (a wake in about a second instead of up to 15): slice 2.
- Codex, Grok, Gemini: slice 2.
- Commands for a wake (git, tests), the key proxy and a separate user or sandbox: slice 3. Until then a wake cannot run a command at all.

## Known limits of slice 1

- **At least once, not exactly once.** A mention whose wake was interrupted (the station was stopped, or a login problem stopped it) is woken again after the
  restart; the run may already have posted before it was interrupted. A mention that was woken for is remembered by its id (the last 300), so removing a
  mention and adding it back right away is not woken again.
- A folder that is not acceptable at wake time (for example a link that leads out of the working folder, brought by a cloned repository) stops only that
  room: the station writes one note into the room and goes on.
- The folder check is a deny-list of places that hold keys or logins (home, `~/.config`, `~/.ssh`, `~/.aws`, ...), not a list of places that are allowed.
  It is written for Linux; folders such as macOS `Library` and Windows `AppData` are guarded by name, other places are not.
- The lock is one file made in a single step, with the process id of the station. On Linux and macOS a lock whose process is gone is taken over at once, and a
  new station also refuses to start while a wake left behind by a killed station is still running (it ends nothing itself: you decide). On Windows the age of the
  file decides (90 seconds), and the left-behind wake is not detected.
