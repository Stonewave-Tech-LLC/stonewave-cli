# Security

This tool will give software agents scoped access to servers, so we take reports seriously.

- **Report privately** through GitHub: Security → "Report a vulnerability" in this repository. Please do not open a
  public issue for a vulnerability.
- Tell us what you found, how to reproduce it and what an attacker could do with it.
- We answer within a few working days and tell you when a fix is out.

## What is in scope

- `install.sh` and how it checks what it installs
- the `stonewave` binary: `doctor`, `connect`, `setup`, the daemon, the agent shell and the `agent` commands (key handling, files, service files)
- the release process (`scripts/release.sh`)

## Known limits

- **Frames from Wave to the server are not signed** (only TLS protects them). Someone inside the TLS connection can keep the
  daemon from holding a line, and cannot log in to anything. Details and the plan: [docs/relay-protocol.md](docs/relay-protocol.md)
  ("Known limit").

## What the commands do

- `connect --demo` only plays the screens: it changes nothing and talks to no server.
- `connect` signs in to the Stonewave account with a device code (`POST /v1/device/code`, `POST /v1/device/token`,
  `GET /v1/cli/whoami`), makes a key for this server and registers it with Wave, then runs `setup`. It sends the host name,
  OS name and CLI version, which the admin sees on the confirmation page. The sign-in token is neither saved nor printed.
- `setup` (as root; `--dry-run` shows the plan first) creates the agent user, a second SSH that listens on 127.0.0.1 only
  and accepts certificates only, the daemon service and an audit log. It does not change the firewall and opens no port.
- The daemon connects outbound only. Its requests are signed with this server's key, and it reports a short health summary
  (the `doctor` checks and a few numbers) once a minute.
- `revoke` (as root) stops the daemon and the agent SSH, empties the trust list and locks the agent user, also without
  internet. It keeps the audit log. If a step cannot finish, it says so and exits with an error.
- `agent connect` signs in with a device code (`purpose: "agent"`), then asks Wave once for the agent's key
  (`POST /v1/agent-connect/mint`, with the sign-in token as Bearer). The key comes back in that answer only. It is written to
  `<config>/wave/<agent>.key` (folder 0700, file 0600, atomically through a temporary file created exclusively and without
  following links; on Windows only your account has access, set with `icacls`) and nowhere else: it is not printed, not put into a
  command line, an engine's configuration, the state file, a service file or a log, and every text that is shown is filtered for
  anything shaped like a key (`wak_...`). Claude Code and Grok are given the header `Authorization: Bearer ${WAVE_AGENT_KEY}` and Codex
  `bearer_token_env_var = "WAVE_AGENT_KEY"`, so the engine reads the key from its environment; `agent run` sets that variable for one
  child process. The key is sent to Wave only as a Bearer header, handed to `curl` on stdin. A Wave address that is not https is only
  accepted for this machine, and the MCP address Wave reports must be on the same host as the API address it was asked on.
  The agent commands refuse to run as root and never use sudo.
- `agent status` asks Wave (`GET /v1/agent-connect/status`) with the key; `agent rotate` gets a new key and Wave revokes the old one;
  `agent disconnect` revokes the key first (`POST /v1/agent-connect/revoke`) and then removes exactly the entries, files and service
  recorded in `<config>/wave/<agent>.json`. It changes the Claude Code and Grok configuration only through their own `mcp add` and
  `mcp remove` commands, and `~/.codex/config.toml` only inside the marked block.
- The optional wake service runs `node` with the bundled `fleet-watch.mjs` as your user (systemd `--user`, a LaunchAgent, or a
  scheduled task at logon). It reads the key from the key file, asks Wave's MCP address for what was addressed to the agent, and writes
  one line per new thing to a private log. On Windows, stopping the service also stops the batch file and node processes that belong to
  this one service (found by the batch file's path and by the script with this agent's session name), because the Task Scheduler's own
  stop leaves them running.
- `status` and `doctor` read local files and run report-only commands; `doctor` also opens one TCP connection to
  `account.stonewavetech.com:443` without sending data.
