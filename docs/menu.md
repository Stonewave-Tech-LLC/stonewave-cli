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
hinzufügen**, **Agent verbinden**, **Status** and **Beenden**. The lists are read again every few seconds.

An agent's page offers what fits its state: status of the station, a check of the line to Wave, running the station here (in the foreground; Ctrl-C ends
it and brings you back), pause / resume, stopping or starting the service, setting up or removing the service, and disconnecting.

## "Agent hinzufügen"

The first entry under the agents. Arrow keys and Enter, German screens, Esc goes back one step:

1. **Kind**: Claude Code, Codex, Grok Build, or Gemini (gehostet). A program that is not installed here says so and how to install it; nothing goes on.
2. **Name**: a suggestion made of the program and this computer ("Claude Code (box)"), changeable, 1 to 30 characters (Wave keeps 30). The name is
   only **a suggestion for Wave's page** (`stonewave agent connect --name`, `want.agent_name` in the account); the person confirms the agent there. It
   is never a right, and the menu reads back what the agent is really called.
3. **Sign in**: the wizard's sign-in step (see below), with the program and the name chosen. Enter starts `stonewave agent connect --engine <program>
   --name <name> --no-station --lang de`.

**What it cannot do yet** (as far as the account and Wave offer today): it does not create the agent in Wave. The agent has to exist in Wave as an "own
agent" (Team, Agent hinzufügen) with its connect window open ("Gerät verbinden", 15 minutes); the account's page offers only those. The screen says
so before Enter. For **Gemini (gehostet)** the menu says exactly what Wave still lacks and starts nothing: a hosted kind when an agent is made, the
preparation of the machine the agent runs on, and the Google sign-in done on that machine. The spec of that server part is
`docs/add-agent-server.md`.

## The wizard "Agent verbinden"

### The simple way (for every normal computer)

The first entry, "Neuen Agenten anmelden", is the default. A first connect is a few screens:

1. **Program**: the one that is installed (Claude Code, Codex, Grok Build); with several, a short list. Nothing else is asked: **no name is typed**. The
   agent is the one the person picks in Wave, and the menu reads its name back afterwards. Enter starts `stonewave agent connect --engine <program>
   --no-station --lang de`.
2. **Sign in**: the menu steps aside and the sign-in runs in the terminal, in German: the **browser is opened** on the account's page (not on a server
   without a display, not with `--no-browser`; then the link is shown), the **code and the four words stay on the screen** (the person checks that the
   page shows the same words), and while it waits it says what for and how long: "Warte auf deine Bestätigung im Browser · 0:42 vergangen · der Code gilt
   noch 9:18", then "Wave richtet den Agenten ein · 0:35 von höchstens 10:00". After 90 seconds of "not ready" it says plainly "Wave wartet auf einen
   Admin, der den Plan anwendet" and, at the end of the ten minutes, what to do (tell an admin; the confirmation stays valid; run it again afterwards).
3. **Station?**: "Später" is the default, and ends the wizard: the agent is connected (**Fertig**). "Ja, jetzt mit den üblichen Einstellungen" goes on:
   - **Defaults** (one screen): Wave's address (the one the sign-in made), the rooms (**all** the agent may read, it is only woken where a person
     mentions it; `r` changes the choice), the folder (`~/wave-agents/<name>`, `o`), the limits (20 minutes and 40 turns, 2 USD estimate, 10 per hour,
     50 per day, one session per room, only people of the company, no command tools; shown, not changed here), `w` changes the address. Enter sets it
     up: the preview (`station run ... --dry-run`, nothing started, nothing created) must be **clean before anything is remembered**; then the profile is
     saved.
   - **Service** (Linux, macOS): the whole file is shown, *Nein* is the default, Enter installs nothing. Details below.

The test run (looking into the inbox once, `--once`) and everything else (pause, service, other rooms) are on the agent's page.

### "Mehr …": a server with a secret store

Behind "Mehr …" there is "Auf einem Server mit Secret-Store", for an agent that exists in Wave already and whose key is in a secret store (typical for a
server). It says on every screen what it is for and how to leave it. It is the long way, with numbered steps: program, name, Wave address, key source
(the name of the environment variable, and for a service the command of the secret store), rooms (none chosen at first), folder, limits, preview, test
(`--once`), service.

### Details that apply to both

- **Wave address** (server way, or `w` on the defaults): proposed from the address the sign-in made, then `STONEWAVE_WAVE_URL`, the address of another
  agent here, else `https://wave-dev.preview.stonewavetech.com`. Editable; `https` only (plain `http` only for this machine).
- **Service**: the whole file is shown (it scrolls above the answers on a small terminal), then *Nein* or *Ja*, and **the cursor starts on Nein**: Enter
  without a choice installs nothing. The same question appears on the agent's page.
  - Linux: a systemd user unit `stonewave-station-<name>.service`, no root. `loginctl enable-linger` is mentioned, never done.
  - macOS: a launchd agent `~/Library/LaunchAgents/tech.stonewave.station.<name>.plist` (loaded with `launchctl bootstrap gui/<uid>`, started at login,
    started again after an error, unloaded by "Station stoppen"). launchd has no journal, so the station's output (ids and counts, never message text)
    goes to a private log file next to the other state files. `gui/<uid>` needs a logged-in graphical session. **Not run on a real Mac yet.**
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
`dumb`, a small terminal, the default Wave address, and the simple way against a small account and Wave (the browser opener, the German sign-in, the name read back, the key file) and the whole server way for an agent with a key in the environment (rooms from a small Wave on the loopback address, the preview, a real `--once`
test with a stand-in for Claude Code, no key written anywhere).

**Not tested:** a real Claude Code, a real Wave, the sign-in step with the real account service, writing the service on a real systemd or launchd (the
calls and the files are checked with a recording stand-in), a real Mac, the panic hook (a panic is not provoked), and Windows beyond the unit tests (the terminal code compiles and its screens are tested; no real Windows
console session was driven).
