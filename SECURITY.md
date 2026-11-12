# Security

This tool will give software agents scoped access to servers, so we take reports seriously.

- **Report privately** through GitHub: Security → "Report a vulnerability" in this repository. Please do not open a
  public issue for a vulnerability.
- Tell us what you found, how to reproduce it and what an attacker could do with it.
- We answer within a few working days and tell you when a fix is out.

## What is in scope

- `install.sh` and how it checks what it installs
- the `stonewave` binary: `doctor` today; setup, the daemon and the agent shell once they exist
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
- `status` and `doctor` read local files and run report-only commands; `doctor` also opens one TCP connection to
  `account.stonewavetech.com:443` without sending data.
