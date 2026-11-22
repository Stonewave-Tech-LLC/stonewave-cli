# stonewave

The Stonewave server CLI. It connects a company's Linux servers to [Wave](https://stonewavetech.com) (module
"Systems"), so the company's dev agent gets **safe, scoped hands** on them: short-lived SSH certificates, only the
rights an admin gave, every action logged with the person it was done for.

> **Early build.** `stonewave doctor` is read only. `stonewave connect` signs in with a device code, registers the
> server with Wave and sets it up (needs sudo; `connect --sign-in-only` only signs in). `setup`, `revoke` and the daemon
> are real. The Wave side is not switched on for customers yet: the relay is for Stonewave's own servers first.
> `connect --demo` and `status --demo` only show how the finished CLI will look.
> `stonewave agent ...` connects a coding agent on this machine to Wave; see [agent](#agent-connect-a-coding-agent-to-wave).

![connect](media/connect.gif)

## How it will work

1. Wave shows an install command. Its checksum pins `install.sh`, and `install.sh` pins the binary.
2. `sudo stonewave` on the server shows a sign-in code. The admin enters it in Wave.
3. The admin's device makes its own key. **Each admin device is its own SSH certificate authority.** Only the public
   half goes to the server, and both sides show the same four words to compare.
4. A deterministic setup creates an agent user without sudo, a localhost-only SSH for that user (certificates only, a
   forced command), a small daemon that only connects **outbound**, an audit log and a health check. Every step checks
   first and keeps what is already in place. The server's own SSH stays untouched.
5. The agent gets certificates valid for at most one hour, signed by an admin device. Stonewave relays the
   connection but **cannot create access itself**. Right after connecting the agent may only read (logs, health).
   Critical actions (sudo, delete, firewall) always need a click in Wave, which signs a one-time certificate for
   exactly that command.
6. `sudo stonewave revoke` removes all agent access locally, also without internet. The audit log stays.

## doctor

```bash
stonewave doctor          # checks, shown on screen
stonewave doctor --json   # one JSON object, for agents and scripts
```

Read only: it reads `/etc/os-release`, `/proc/self/status` and `/etc/ufw/ufw.conf`, runs `uname -m`, `sshd -V`,
`timedatectl show` and `df`, and opens one TCP connection to `account.stonewavetech.com:443` without sending
anything. Exit code 0 means no check failed.

| Check | Fails when |
|---|---|
| System | no `/etc/os-release` (warns on systems not tested yet) |
| CPU | not x86_64 or aarch64 |
| systemd | not running |
| OpenSSH | the server's sshd is older than 8.2 (warns when sshd is missing) |
| Outbound | port 443 to the account service not reachable (only warns when an HTTPS proxy is set) |
| Root, Clock, Disk, Firewall, Stonewave | only warn or inform |

## Install (not released yet)

When a release exists, Wave shows a command like this one, with the real checksum:

```bash
curl -fsSLO https://stonewavetech.com/install/0.1.0/install.sh \
  && echo "<sha256 of install.sh>  install.sh" | sha256sum -c - \
  && sudo sh install.sh
```

[`install/install.sh.in`](install/install.sh.in) is short so it can be read first. It picks the binary for the CPU,
downloads it over HTTPS only, checks its SHA-256 against the value written into the script, installs it as
`/usr/local/bin/stonewave` and runs `stonewave doctor`. `sh install.sh --dry-run` checks everything and installs
nothing.

## Build and release

```bash
cargo build --release                           # this machine
scripts/release.sh 0.1.0                        # static Linux binaries (x86_64, aarch64) into dist/0.1.0/
scripts/test-install.sh dist/0.1.0              # on a Linux machine: doctor + install.sh --dry-run checks
```

`release.sh` builds static musl binaries with Rust's own linker (no cross toolchain needed, see
`.cargo/config.toml`), writes `SHA256SUMS`, writes the checksums into `install.sh` and prints the command for Wave.
With `STONEWAVE_MINISIGN_KEY` set to a minisign secret key it also signs the files. The key is never part of this
repository; without it the release is unsigned.

## Connect: sign in with a device code

```bash
sudo stonewave connect                   # sign in, register this server with Wave, set it up
sudo stonewave connect --account https://...  # another account address (or STONEWAVE_ACCOUNT_URL); https only, http only for this machine
```

`connect` asks the account for a code (RFC 8628) and shows it with four words. The admin opens the printed address,
signed in to their Stonewave account, types the code, checks that the page shows the same four words and that the
server name and address are right, picks the company and confirms. The CLI then receives a short token (15 minutes,
bound to that person and company), asks the account who it belongs to, shows the company and **drops the token**: it is
never written to disk and never printed.

With `sudo stonewave connect` that sign-in is the first step of the whole connect:

1. make the server's own Ed25519 key (it stays on the server, `/var/lib/stonewave/server.key`),
2. register its public half with Wave at the relay (`--relay <url>` or `STONEWAVE_RELAY_URL`; https only, http only for this
   machine), proving with a signature that the key is ours and for this sign-in only,
3. run the base setup (below), which starts the daemon,
4. the daemon holds one **outbound** HTTPS line to Wave, reports health once a minute and hands byte streams to the agent SSH
   on `127.0.0.1:2222`. A new server starts with no rights for the agent. See [docs/relay-protocol.md](docs/relay-protocol.md).

Nothing on the machine changes before Wave has accepted the key. `--trust-file <file>` trusts admin device keys right away
(until Wave hands them over); `--yes` skips the question. `stonewave status` shows the line, the agent SSH and the last
health report; `sudo stonewave revoke` closes it for good (and Wave can cut the line from its side: the daemon then
stops and is not restarted).

For agents and scripts, `sudo stonewave status --json` prints the same facts as one JSON object (no colour, no secrets;
`connected`, `daemon`, `agent_ssh`, `health`, ...). The exit code is 0 whenever the report could be made, whatever the state
is; it is non-zero only when the command itself failed. Without sudo it says `"readable": false` instead of guessing. `access_revoked` is the local
`sudo stonewave revoke`; when Wave cut the line, `line.state` is `"revoked"` and `connected` is false.

HTTP goes through the system `curl` (the install command needs it anyway), so TLS and the certificate store are the
system's. Secrets are handed to curl on stdin, never as arguments. A plain `http://` address is only accepted for this
machine (`127.0.0.1`, `localhost`, `::1`), for tests.

## agent: connect a coding agent to Wave

```bash
stonewave agent connect --engine grok     # claude-code | codex | grok | other
stonewave agent status [--json]
stonewave agent rotate
stonewave agent disconnect [--yes] [--keep-remote]
stonewave agent run [--agent <name>] -- claude     # start the agent with its key in the environment
```

This turns a coding agent on **any** machine (your Mac, a Linux server, a Windows PC) into an external agent of a Wave company.
It is separate from `connect` above, which connects a *server*. Everything lives in your home folder and runs as you: no
sudo, and root is refused (a key made by root would belong to nobody).

`connect` shows a code and four words, like the server sign-in. You confirm on the account's "connect an agent" page (company,
team, name, engine), and Wave hands the agent's key **to this machine only**: it never passes the account, a chat or any other
service. Then, step by step and each one checking first:

1. the key goes into `~/.config/wave/<agent>.key` (Windows `%LOCALAPPDATA%\wave\`, never the roaming profile: nothing secret may roam), the folder 0700, the file 0600 (Windows: only
   your account, via `icacls`), written atomically, never through a link,
2. Wave is added to the agent **without the key in its configuration**: Claude Code and Grok get the header
   `Authorization: Bearer ${WAVE_AGENT_KEY}` (the variable name is written out, not its value), Codex gets
   `bearer_token_env_var = "WAVE_AGENT_KEY"`; `stonewave agent run -- <command>` sets that variable for that one process,
3. optionally the **wake service** (below).

| Engine | What `connect` does |
|---|---|
| `claude-code` | `claude mcp add --transport http --scope user wave <wave>/mcp --header "Authorization: Bearer ${WAVE_AGENT_KEY}"`; an old `wave` entry is removed first (and you are told) |
| `grok` | `grok mcp add --transport http -H "Authorization: Bearer ${WAVE_AGENT_KEY}" wave <wave>/mcp` |
| `codex` | a marked block in `~/.codex/config.toml` (`# stonewave: begin wave` ... `# stonewave: end wave`); a `[mcp_servers.wave]` that is not ours is never touched (the step stops with exit code 1); the first change makes `config.toml.stonewave-bak` |
| `other` | prints the address and the header line |

Without `--engine` it takes the one program it finds on the PATH (`claude`, `codex`, `grok`) and asks when there are several.
A missing program stops the run before anything is written. What the person confirms on the page wins over `--engine`.

**Addresses.** `--account <url>` / `STONEWAVE_ACCOUNT_URL` for the account; `--wave <url>` / `STONEWAVE_WAVE_URL` for Wave's API,
else the account tells it. Both need https (http only for this machine). If no Wave address is known, `connect` stops before the key
is asked for. While Wave is still setting up a new agent, `connect` waits (every 5 seconds, up to 10 minutes) and honours a
`Retry-After`.

**The key.** It is never printed, not even with an error, never in a command line, a log, a state file or an engine's
configuration, and every text shown passes a filter that cuts out anything shaped like a key. It expires after 90 days:
`status` shows the date and warns 14 days before; `rotate` gets a new key with the old one and restarts the wake service.
`disconnect` revokes the key in Wave **first** (without a connection it refuses, unless `--keep-remote`; a 401 means it was
already dead), then removes exactly what this tool made.

**Files.** `~/.config/wave/<agent>.key`, `~/.config/wave/<agent>.json` (what was created, for `disconnect`; no key inside),
`~/.config/wave/fleet-watch.mjs` and `~/.local/state/wave/<agent>.wake.log` (on Windows all of them are in `%LOCALAPPDATA%\wave\`), plus the engine's own
entry and the service files. `XDG_CONFIG_HOME` and `XDG_STATE_HOME` are honoured; `CODEX_HOME` for Codex.

**Wake service (optional, `--yes` accepts, `--no-station` skips).** A small program (`fleet-watch.mjs`, part of this tool, needs
Node.js 18 or newer) asks Wave what was addressed to the agent and writes one line per new thing to the wake log (0600, cut to
the last 1000 lines when the service is installed again). A running session follows that file (`tail -n 0 -F <file>`; PowerShell:
`Get-Content -Wait -Tail 0 -LiteralPath <file>`) and wakes up on each line. It only wakes sessions that are already running. It runs as you:

| System | How |
|---|---|
| Linux | `~/.config/systemd/user/stonewave-wave-<agent>.service` (`systemctl --user enable --now`); to keep it running while you are logged out, run `loginctl enable-linger <user>` yourself, it is only suggested |
| macOS | `~/Library/LaunchAgents/tech.stonewave.wave.<agent>.plist` (`launchctl bootstrap gui/<uid>`) |
| Windows | a scheduled task `StonewaveWave-<agent>` at logon, made from a task definition (no admin needed), running a small PowerShell wrapper (hidden window) that keeps node running and records its own process id and node's, with start times, in a file with no secrets; `disconnect`, `rotate` and a second `connect` end exactly those two processes by id, after checking that each id still has the same start time and program name (otherwise it says "stale pid, left alone" and does nothing to it) |

If Node.js is missing, `connect` still finishes and says the wake service is missing and why.

**What was and was not tested.** The protocol, the files, the rights, the Codex block, the argument lists for `claude`/`grok`/`codex`
and the text of the three service files are covered by tests, including the real `curl` against a local stand-in for the account
and Wave. The Windows task (create, run, restart, delete, with the process ids compared before and after), `icacls` and the key rights were also run on a real Windows machine. **launchd and systemd units were
only checked as text and as the commands that would be run; they have not been started on a Mac or a Linux machine. The Codex block
follows its documentation but has not been run against Codex, and `claude mcp add` has not been run by this tool on a machine
with Claude Code.** Grok 1.0.62 was seen to fill the `${WAVE_AGENT_KEY}` header in from its environment (against a local test server, not against Wave itself).

## setup and revoke

```bash
sudo stonewave setup --dry-run                      # the plan: what would change, nothing is touched
sudo stonewave setup --trust-file admin-keys.pub    # does it (asks first; --yes to skip the question)
sudo stonewave revoke                               # the local kill switch, works without internet
sudo stonewave revoke --device "Office PC"          # removes only one admin device
```

`setup` is deterministic: every step looks first and changes only what is missing, so a second run changes nothing.
It needs the installed binary at `/usr/local/bin/stonewave` (`install.sh` puts it there) and OpenSSH on the server.

| Step | What | Looks first |
|---|---|---|
| 0 Precheck | the `doctor` checks; a failed one stops, outbound access is only needed later | – |
| 1 Users | `stonewave-agent` (no password, no sudo, never in `sudo`, `admin`, `wheel`, `docker`...) and `stonewave` for the daemon | user there? |
| 2 Admin keys | `/etc/stonewave/ssh/trusted_admin_cas`: keys are added, never removed here. Only `ssh-ed25519` and `ecdsa-sha2-nistp256`, one per line, no options | key already in? |
| 3 Agent SSH | a second `sshd` on `127.0.0.1:2222` only: certificates only, user `stonewave-agent` only, forced command `stonewave agent-shell`, no forwarding, no terminal, its own host key and revocation list. Allowed principals: `sw.read`, `sw.logs`, `sw.health`. A new config is tested with `sshd -t` before it replaces the old one. **The server's own sshd is not touched** | config the same? |
| 4 Daemon | `stonewaved.service` as user `stonewave`, in a systemd cage (no capabilities, `ProtectSystem=strict`, only IPv4/IPv6/Unix sockets) | running, right version? |
| 5 Firewall | reports only. This build never changes a firewall: a wrong rule can lock the admin out | – |
| 6 Logbook | `/var/log/stonewave/audit.jsonl` and logrotate | there? |
| 7 Health | agent sshd and daemon run, port answers, config passes `sshd -t`, disk and clock | – |

The state is in `/var/lib/stonewave/state.json`. Where a step cannot go on (no OpenSSH, binary not installed, agent
user in a powerful group) it stops and says why; what ran is in place and running setup again is safe.

`revoke` stops the daemon and the agent sshd, ends the agent's sessions (only that user's processes), empties the
trust list, and locks `stonewave-agent` (no shell, account expired). The audit log stays. `setup` opens it again.

`stonewave agent-shell` is the forced command of the agent sshd. In this build it knows one operation, `health`,
and refuses everything else (and logs both). `stonewave daemon` keeps the line to Wave (see
[docs/relay-protocol.md](docs/relay-protocol.md)) and `/var/lib/stonewave/health.json` fresh. Without a server key it only
keeps the local health report.

## Demo screens

```bash
stonewave connect --demo   # the made-up screens: sign-in, key check, setup progress (talks to nobody)
stonewave status --demo   # how every later start looks (made-up values)
stonewave banner
```

Keys: `y`/`n` answer, Enter skips the sign-in wait, `q` or Ctrl-C stops. The terminal is always restored. Keys typed
before a question appears are dropped, so a `y` typed ahead never confirms the key check.

| Situation | Result |
|---|---|
| Terminal with true colour (`COLORTERM=truecolor`, Windows Terminal) | 24-bit colour, animations |
| Other terminals | 256 colours, animations |
| `NO_COLOR` set | no colour, no animation |
| `--no-anim`, `STONEWAVE_NO_ANIM=1` | colour, final states only |
| Output not a terminal, `TERM=dumb`, fewer than 16 rows | plain lines, no escapes |
| No terminal to read keys from | every question is answered **no**; `--autoplay` plays the demo through |
| Narrower than 57 columns | one-line header instead of the banner |

The GIF and screenshots in `media/` are made from the program's own recordings (`--record`, asciicast v2), replayed
by `scripts/cast2gif.py` with Pillow.

## Layout

| Path | What |
|---|---|
| `src/main.rs` | arguments, commands |
| `src/doctor.rs` | the read-only checks (account and relay reachable) |
| `src/setup.rs`, `src/realsys.rs`, `src/setup_flow.rs` | setup and revoke: steps, the real machine behind them, the screens |
| `src/daemon.rs`, `src/tunnel.rs`, `src/relay.rs`, `src/health.rs` | `daemon` and `agent-shell`; the line to Wave; the server key, signing and frames; the health report |
| `src/status.rs` | `stonewave status` |
| `src/b64.rs` | base64 without a dependency |
| `src/term.rs` | terminal abilities, buffered output, raw mode, keys, recording |
| `src/style.rs`, `src/ui.rs`, `src/banner.rs` | colours, building blocks, banner and wave animation |
| `src/connect.rs` | the real device code sign-in: account address, curl transport, protocol, wait loop |
| `src/agent/` | `stonewave agent ...`: Wave API calls, the key and its files, engine entries, Codex block, wake service, commands |
| `assets/fleet-watch.mjs` | the wake script that the wake service runs (embedded into the binary) |
| `src/flow.rs` | the screens (real `connect` and the demos) |
| `install/install.sh.in` | install script template |
| `scripts/` | release, install test, media renderer |

## License

Apache-2.0, see [LICENSE](LICENSE). Security issues: please use GitHub's private vulnerability reporting
("Report a vulnerability" under Security), see [SECURITY.md](SECURITY.md).
