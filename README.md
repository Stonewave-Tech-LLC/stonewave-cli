# stonewave

The Stonewave server CLI. It connects a company's Linux servers to [Wave](https://stonewavetech.com) (module
"Systems"), so the company's dev agent gets **safe, scoped hands** on them: short-lived SSH certificates, only the
rights an admin gave, every action logged with the person it was done for.

> **Early build.** `stonewave doctor` is real and read only. `stonewave connect` really signs in at the Stonewave
> account (device code, see below) but changes nothing on the machine yet. `stonewave setup` and `stonewave revoke`
> are real and change the server (with sudo; `--dry-run` only looks). `status` and `connect --demo` only show how the
> finished CLI will look.

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
stonewave connect                        # real sign-in at the account, changes nothing on this machine yet
stonewave connect --account https://...  # another account address (or STONEWAVE_ACCOUNT_URL); https only, http only for this machine
```

`connect` asks the account for a code (RFC 8628) and shows it with four words. The admin opens the printed address,
signed in to their Stonewave account, types the code, checks that the page shows the same four words and that the
server name and address are right, picks the company and confirms. The CLI then receives a short token (15 minutes,
bound to that person and company), asks the account who it belongs to, shows the company and **drops the token**: it is
never written to disk and never printed. Setup, the server key and the daemon come in later releases.

HTTP goes through the system `curl` (the install command needs it anyway), so TLS and the certificate store are the
system's. Secrets are handed to curl on stdin, never as arguments. A plain `http://` address is only accepted for this
machine (`127.0.0.1`, `localhost`, `::1`), for tests.

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
and refuses everything else (and logs both). `stonewave daemon` only keeps `/var/lib/stonewave/health.json` fresh;
the tunnel to Wave comes later.

## Demo screens

```bash
stonewave connect --demo   # the made-up screens: sign-in, key check, setup progress (talks to nobody)
stonewave status     # how every later start looks (demo values)
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
| `src/doctor.rs` | the read-only checks |
| `src/setup.rs`, `src/realsys.rs`, `src/setup_flow.rs` | setup and revoke: steps, the real machine behind them, the screens |
| `src/daemon.rs` | `daemon` and `agent-shell` |
| `src/b64.rs` | base64 without a dependency |
| `src/term.rs` | terminal abilities, buffered output, raw mode, keys, recording |
| `src/style.rs`, `src/ui.rs`, `src/banner.rs` | colours, building blocks, banner and wave animation |
| `src/connect.rs` | the real device code sign-in: account address, curl transport, protocol, wait loop |
| `src/flow.rs` | the screens (real `connect` and the demos) |
| `install/install.sh.in` | install script template |
| `scripts/` | release, install test, media renderer |

## License

Apache-2.0, see [LICENSE](LICENSE). Security issues: please use GitHub's private vulnerability reporting
("Report a vulnerability" under Security), see [SECURITY.md](SECURITY.md).
