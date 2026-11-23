# The menu

`stonewave` on a terminal opens a full-screen menu. It is the one place to connect a coding agent to Wave and to set up its station, on a PC and on a
server (over ssh). The text on the screens is German; this page and the code are English.

```
stonewave            # the full-screen menu (a terminal is needed)
stonewave --plain    # one plain screen with the same lists, nothing is asked: for scripts and agents
```

Keys: arrow keys (or `j`/`k`) move, Enter chooses, Esc goes back one step, `q` quits on the main screen, Ctrl-C quits anywhere. In a text field the
keys type, Ctrl-U clears the field. Any key ends the banner's wave early.

The full screen is not used (the plain menu is shown instead, with a line saying why) when `TERM=dumb`, when the terminal is smaller than 50x16, or
when the terminal cannot be taken over. Without a terminal on stdin and stdout, `stonewave` keeps its old behaviour.

The terminal is always given back: on `q` and Ctrl-C, on SIGTERM and SIGHUP (a closed ssh session), and on a panic (before its message is printed).

## What it never does

The menu **only starts commands that exist**: `stonewave agent connect`, `agent status`, `agent disconnect`, `agent station run|status|pause|resume`,
as separate processes with separate arguments (never through a shell). It does not bypass what they check: the limits, the folder checks, the lock and
the restrictions of the wake command are all in `station run`, and the preview is that command's own `--dry-run`.

- The agent's **key is never typed, shown or written** by the menu. A key from a sign-in goes into the private file `agent connect` makes. For an agent
  whose key lives in a secret store, the menu asks for the *name* of the environment variable (a value that looks like a key is refused as a name), and
  the station reads the variable of its own process.
- Every step that changes something is a question with **Nein** as the default; Enter on it does nothing. The service file is shown whole before it is
  written.
- The only files the menu writes by itself are `<agent>.profile.json` (what the wizard decided: rooms, folder, where the key comes from) and, when you
  say yes, the user service file.

## The main screen

The agents on this computer (name, program, where the key comes from, whether the station runs, is paused, or only has a service), then **Agent
verbinden**, **Status** and **Beenden**. The lists are read again every few seconds.

An agent's page offers what fits its state: status of the station, a check of the line to Wave, running the station here (in the foreground; Ctrl-C ends
it and brings you back), pause / resume, stopping or starting the service, setting up or removing the service, and disconnecting.

## The wizard "Agent verbinden"

A handful of decisions; each step says what it will do first.

1. **Way**: *sign in with the browser* (a new agent; Wave gives it a key for this computer) or *connect an existing agent* (its key is in a secret
   store, typical for a server).
2. **Program**: Claude Code, Codex, Grok Build. Those not installed are greyed out with an install hint. The station (waking by mention) exists for
   Claude Code only so far; Codex and Grok are connected, their station follows.
3. **Name**: lower case letters, digits, dashes; a free name is proposed.
4. *New agent*: **sign in**: the menu steps aside and runs `stonewave agent connect --engine <program> --name <name> --no-station`: the code and the
   four words, you confirm in the browser, the menu comes back. Then **Station**: *Ja* or *Später*.
   *Existing agent*: **Wave address**, then **key source**: the name of the environment variable and, for a service, the command of the secret store
   that starts a program with its secrets in the environment (for example `secrets-run run --env=prod --`; only plain words are accepted).
5. **Wave address** (both ways, before any room is asked): proposed from what is known, in this order: the address the sign-in made, `STONEWAVE_WAVE_URL`,
   the address of another agent here, else `https://wave-dev.preview.stonewavetech.com`. Editable; `https` only (plain `http` only for this machine).
6. **Rooms**: fetched from Wave with the agent's key (a multi-select). A station wakes only in the rooms chosen, and only for people of the company.
7. **Working folder**: the only place the agent may read and write; default `~/wave-agents/<name>`.
8. **Limits**: shown, not changed here (20 minutes and 40 turns per wake, 2 USD estimate, 10 per hour, 50 per day, one session per room).
9. **Preview**: `station run ... --dry-run`, the exact wake command and everything around it. Starts nothing and creates nothing. If it would be
   refused (a working folder that holds keys or is your whole home folder, no Claude Code to start) it ends with an error, and **nothing is remembered
   until a preview is clean**: no profile, so no service can be built on a setup that does not start. `r` repeats it.
10. **Test**: `station run ... --once`: looks into the inbox once and stops. The first start only remembers the inbox position and wakes nobody; if a
    position exists already, an open mention can wake the agent now. The menu steps aside so you see the output.
11. **Service**: the whole file is shown (it scrolls above the answers on a small terminal), then *Nein* or *Ja*, and **the cursor starts on Nein**: Enter
    without a choice installs nothing. The same question appears on the agent's page.
    - Linux: a systemd user unit `stonewave-station-<name>.service`, no root. `loginctl enable-linger` is mentioned, never done.
    - macOS: a launchd agent `~/Library/LaunchAgents/tech.stonewave.station.<name>.plist` (loaded with `launchctl bootstrap gui/<uid>`, started at login,
      started again after an error, unloaded by "Station stoppen"). launchd has no journal, so the station's output (ids and counts, never message text)
      goes to a private log file next to the other state files. **Not run on a real Mac yet.**
    - Both hold the same options as the commands above, the launcher words in front, the full path of Claude Code and the PATH of the shell the menu runs
      in (a service has a short PATH; relative and empty entries are left out). Neither ever holds the key.
    - Windows has no service installer yet: the menu says so and the station is started from the agent's page.

## A server with a secret store (for example the main server over ssh)

Start the menu through the secret-store command so the key is in its environment, and name the variable in the wizard:

```bash
<secret-store command that runs a program with secrets in its environment> -- stonewave
```

For the service, give the same command (its words) in the wizard's key source step. How the secret-store command signs in on that server is the
existing setup there.

## What was and was not tested

Tested: every screen and step of the wizard by pressing keys against a scripted machine (`src/agent/tui/tests.rs`), and the real binary on a real
pseudo-terminal (`tests/menu_pty.rs`, Unix): opening, navigating, leaving with `q`, Ctrl-C, SIGTERM and SIGHUP with the terminal given back, `--plain`,
`dumb`, a small terminal, the default Wave address, and the whole wizard for an agent with a key in the environment (rooms from a small Wave on the loopback address, the preview, a real `--once`
test with a stand-in for Claude Code, no key written anywhere).

**Not tested:** a real Claude Code, a real Wave, the sign-in step with the real account service, writing the service on a real systemd or launchd (the
calls and the files are checked with a recording stand-in), a real Mac, the panic hook (a panic is not provoked), and Windows beyond the unit tests (the terminal code compiles and its screens are tested; no real Windows
console session was driven).
