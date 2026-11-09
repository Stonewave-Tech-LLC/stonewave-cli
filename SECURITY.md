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

## What this build does not do yet

`connect`, `status` and `revoke` are demos of the screens. They change nothing on the machine and talk to no server,
so they cannot leak or grant anything. `doctor` reads system files, runs a few report-only commands and opens one TCP
connection to `account.stonewavetech.com:443` without sending data.
