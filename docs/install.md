# Install the Stonewave CLI

One command, for you only (no root, no administrator rights):

```bash
# Linux and macOS
curl --proto '=https' --tlsv1.2 -fsSL https://stonewavetech.com/cli/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://stonewavetech.com/cli/install.ps1 | iex
```

Then run `stonewave` (the menu) in a new terminal. `stonewave --version` says the release and the commit it was built from, for example
`stonewave 0.2.0 (fcc1b3b7)`.

Run the same command again to update. It never goes back to an older release unless you ask for one by number.

## What the installer does

Both installers are short on purpose, so you can read them before running them (`curl --proto '=https' --tlsv1.2 -fsSL https://stonewavetech.com/cli/install.sh | less`).
They print each step.

1. Work out your system: Linux or macOS on x86_64 or arm64; Windows on x86_64.
2. Download the manifest of the newest release (`/cli/latest/manifest.txt`) over HTTPS, TLS 1.2 or better. It names the release, the commit and,
   per system, the file with its SHA-256 and size.
3. Download the program and check it: size and SHA-256 against the manifest, and the code signature (below).
4. Run the downloaded program once with `--version`. It must say exactly what the manifest says. Only then it is put in place.
5. Install into your own folder, replacing an older copy in one step (a running copy keeps working):
   - Linux and macOS: `~/.local/bin/stonewave`
   - Windows: `%LOCALAPPDATA%\Stonewave\bin\stonewave.exe`
6. Tell you about PATH (below). Nothing else on your computer is changed. No telemetry: the only requests are the manifest and the file.

Any check that fails ends the installer with a message, and **nothing is installed**.

## What protects the download, per system

| System | Checks | What it protects against |
|---|---|---|
| Windows | SHA-256 and size from the manifest; the **Authenticode signature** must be valid (Windows' own verdict), the signer certificate's subject must be exactly **`CN=Stonewave Tech LLC, O=Stonewave Tech LLC, L=Sheridan, S=Wyoming, C=US`**, and the certificate must have been **issued under the Microsoft authority pinned in the script** ("Microsoft ID Verified Code Signing PCA 2021"), checked as an intermediate of the chain Windows builds up to a trusted root (not the certificate itself, not the root), with revocation asked online (when that cannot be had, it falls back to what is cached and then says so). The authority is pinned, not the certificate: Azure's signing certificates last three days. The certificates carried inside the file's signature are given to Windows to build the chain, so this also works offline | a damaged or swapped file, a file somebody else signed, the same name under another authority, a self-made certificate with the same name |
| macOS | SHA-256 and size; `codesign --verify --strict -R=<our requirement>`: the signature must chain to **Apple's root** through the Developer ID Certification Authority, the leaf must be a **Developer ID Application** certificate, and its organizational unit must be our **Team ID** (pinned in the script). The requirement is ours, not the one written into the file (a file's own requirement would let a certificate of the maker's own pass). Gatekeeper's notarization check (`spctl --type install`) is shown as `gatekeeper: accepted (…)` when it accepts and left out when it has no verdict (it never decides; for a bare command-line tool `--type execute` says "rejected" even for a notarized file) | the same, and a file signed with some other certificate |
| Linux | SHA-256 and size from the manifest, which comes from the same HTTPS site as the program | a damaged or swapped download. **Not** against someone who controls stonewavetech.com: they could change the manifest too. There is no signature on Linux builds. |

The honest limit for all three: the installer itself also comes from stonewavetech.com. If that site were taken over, the attacker could serve
a different installer. Whoever wants more certainty downloads `install.sh` / `install.ps1`, reads it, and checks the program with the pinned
values (Windows: `Get-AuthenticodeSignature`; macOS: `codesign -dv --verbose=4`). `https` is enforced (`curl --proto '=https' --tlsv1.2`,
`wget --https-only`, TLS 1.2 in PowerShell).

## PATH

The folder is on your PATH or it is not; the installer says which.

- **Linux and macOS:** if it is not, the installer prints the line to add. It changes a shell startup file only when you say so: with
  `--add-to-path`, or by answering **y** to its question (asked on your terminal, also when the script is piped in; never asked when its output is
  captured by a script). It says which file it changed (`~/.zshrc`, `~/.bashrc` / `~/.bash_profile`, or `~/.profile`) and what it added, once.
- **Windows:** the folder is added to **your** user PATH (not the machine's) and the installer says so. The existing entries are kept exactly as
  they were (`%VARIABLES%` are not expanded). `-NoPath` leaves PATH alone.

## Options

Linux and macOS (`sh -s -- ...` passes them when the script is piped in):

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://stonewavetech.com/cli/install.sh | sh -s -- --version 0.2.0 --add-to-path
```

| Option | Meaning |
|---|---|
| `--version X.Y.Z` | exactly this release, also an older one |
| `--add-to-path` / `--no-path` | add the folder to PATH without asking / never touch or ask |
| `--dry-run` | download and check everything, install nothing |
| `--bin-dir DIR` | install here instead of `~/.local/bin` |
| `--from-dir DIR` | take manifest and program from a folder instead of the internet (same checks) |

Windows (a script block, because `iex` cannot take parameters):

```powershell
& ([scriptblock]::Create((irm https://stonewavetech.com/cli/install.ps1))) -Version 0.2.0 -NoPath
```

`-Version`, `-NoPath`, `-DryRun`, `-BinDir`, `-FromDir` mean the same.

## Uninstall

Delete the file (`~/.local/bin/stonewave` or `%LOCALAPPDATA%\Stonewave\bin\stonewave.exe`) and, if you let the installer add it, the line marked
`# added by the Stonewave installer` in your shell startup file, or the folder in your user PATH. Agents and services set up by the menu
are removed from the menu first (agent page, "Trennen"). Keys and settings are in `~/.config/wave` and `~/.local/state/wave` (Windows:
`%LOCALAPPDATA%\wave`).

## The server install is another thing

The command Wave shows for a **server** (`install.sh` with a pinned checksum, installs `/usr/local/bin/stonewave` as root, then `stonewave doctor`)
is unchanged: see the README. This page is for a person's own computer.
