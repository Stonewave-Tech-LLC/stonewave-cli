# Making a release

How a release of the CLI is built, collected and put online. There is **no release key and no signature of ours over the manifest**: the Windows
and macOS builds carry the code signatures of the company (Authenticode, Developer ID) and the installers check those; Linux builds are checked
by SHA-256 over HTTPS only (see [install.md](install.md) for what that does and does not protect against).

## The matrix

| Target | Built where | Signed with |
|---|---|---|
| `x86_64-linux` | anywhere: `cargo build --release --locked --target x86_64-unknown-linux-musl` (static, Rust's own linker, see `.cargo/config.toml`) | nothing |
| `aarch64-linux` | the same with `aarch64-unknown-linux-musl` (cross build; can only be run on an arm64 machine) | nothing |
| `x86_64-windows` | on the PC that holds the code-signing certificate of Stonewave Tech LLC, as for Wave: build, then **sign the .exe** | Authenticode (the company certificate) |
| `aarch64-macos`, `x86_64-macos` | on the Macs of the Away Team: build, **sign with the Developer ID** (`codesign --options runtime --timestamp`), **notarize** (`notarytool`) | Developer ID Application, team of the company |

Every build of one release is made from the **same clean commit** (`git status` empty, the version in `Cargo.toml` raised and committed). `--version`
of each says `stonewave <version> (<commit>)`; the commit is put in by `build.rs` (a tree with changes says `-dirty`, and the installers refuse
a manifest with such a commit). A source archive without git: set `STONEWAVE_COMMIT` when building.

## Collect and write the manifest

```bash
export STONEWAVE_MAC_TEAM_ID=XXXXXXXXXX                       # the 10-character Apple team id (only with macOS builds)
export STONEWAVE_WIN_CA_THUMBPRINTS=<40 hex>[,<40 hex>]        # thumbprint(s) of the issuing authority (only with a Windows build), see below
scripts/release-bundle.sh out \
  --x86_64-linux  stonewave-x86_64-linux  --aarch64-linux stonewave-aarch64-linux \
  --aarch64-macos stonewave-aarch64-macos --x86_64-macos  stonewave-x86_64-macos \
  --x86_64-windows stonewave.exe
```

`out/` is what the web server shows under `/cli/`:

```
install.sh  install.ps1                       with the team id and the authority's thumbprint written in
latest/manifest.txt                           a copy of the newest manifest (--no-latest leaves it alone)
<version>/manifest.txt  SHA256SUMS
<version>/stonewave-<version>-<target>[.exe]
```

The manifest is plain text (`stonewave-manifest 1`, `version`, `commit`, one `file <target> <sha256> <size> <name>` line per build). The script
refuses to overwrite a version folder (a published version does not change; a wrong release is replaced by a new version), needs a clean tree, and
runs the builds it can run here to see that they say what the manifest says.

**Where to find the pins:**
- **Windows:** the thumbprint of the authority that issued the signing certificate, the second certificate of the chain:
  `$s = Get-AuthenticodeSignature stonewave.exe; $c = New-Object Security.Cryptography.X509Certificates.X509Chain; [void]$c.Build($s.SignerCertificate); $c.ChainElements | % { $_.Certificate.Subject.Split(',')[0] + ' ' + $_.Certificate.Thumbprint }`.
  Pin **"Microsoft ID Verified Code Signing PCA 2021"** (`8E750F459DAF9A79D6370DB747AD2226866AD818` for the 0.2.0 build), not the leaf and not "...AOC CA 0x" (which
  Microsoft rotates): Azure Artifact Signing makes a new signing certificate every few days, and the signature stays valid because it is timestamped. The subject
  is fixed in the template (`CN=Stonewave Tech LLC, O=Stonewave Tech LLC, L=Sheridan, S=Wyoming, C=US`). Release 0.2.0 was signed before the authority was
  pinned: the installer also accepts that one certificate (`8A69FFDF224024FD656E942B043D92D5AA381505`), for version 0.2.0 only. If Microsoft ever issues
  under another authority, list both thumbprints (comma separated) in the release that changes it.
  The pin is matched **between** the signing certificate and the root of the chain, never the certificate itself and never the root (a root pin would be every
  certificate under that root). The real chain is certificate `[0]` → "Microsoft ID Verified Code Signing AOC CA 0x" `[1]` (rotates) → **PCA 2021 `[2]`** →
  "Microsoft Identity Verification Root Certificate Authority 2020" `[3]`. The installer also reads the certificates the signature carries inside the file and offers them
  to Windows as extra material for building the chain, so it works on a machine that is offline and has never seen the authority (they are only candidates: every
  link must still be signed by the one above, up to a root in the machine's own store).
- **macOS:** the team id: `codesign -dv --verbose=4 stonewave | grep TeamIdentifier`. Ours is **`TV5R62BGRZ`** ("Developer ID Application: Stonewave Tech LLC",
  hardened runtime, timestamped, notarized): `STONEWAVE_MAC_TEAM_ID=TV5R62BGRZ`. Like the Windows pin it goes in when the bundle is made, not into the repository.

**Known pin values (0.2.0 and the Azure signing in use):** `STONEWAVE_MAC_TEAM_ID=TV5R62BGRZ`, `STONEWAVE_WIN_CA_THUMBPRINTS=8E750F459DAF9A79D6370DB747AD2226866AD818`.

## The gate before it goes online

**Windows, on the real file** (not the stand-in): `powershell -File scripts/check-windows-signature.ps1 -Path stonewave.exe -Pin 8E750F459DAF9A79D6370DB747AD2226866AD818 [-Version 0.2.0]`
runs the installer's own functions on the file and prints what Windows says (must be `Valid`, also for a program whose three-day certificate has expired: the
timestamp keeps it valid), the exact subject and whether it equals the one in the template, the certificate's validity, the chain with the position of each
certificate and whether the installer would accept the pin (`RESULT: ok`) or refuse. Run it on the file that goes into the bundle **and** on a program freshly
signed with the current Azure account, and compare the subject string and the authority's thumbprint with the values above.

Run the installers' own check against the folder on each system the release is for. It does everything the installer does except installing:

```bash
sh out/install.sh --dry-run --from-dir out --bin-dir /tmp/x                                  # Linux, macOS
```
```powershell
powershell -File out\install.ps1 -DryRun -FromDir out -BinDir $env:TEMP\x                    # Windows
```

A Windows `.exe` that was not signed, or signed with another certificate, and a macOS build without a valid Developer ID signature of the team,
fail here, which is the point.

## Putting it online

The folder is deployed by Ace to the web server, **outside the site's git repository** (binaries do not belong in git), so that
`https://stonewavetech.com/cli/...` serves it as plain static files, only after Neo's yes:

- `install.sh`, `install.ps1`, `latest/manifest.txt`: `Cache-Control: no-cache` (they change; `latest` must never be stale)
- `<version>/...`: long cache is fine (they never change); `Content-Type: application/octet-stream` for the programs, `text/plain; charset=utf-8` for
  `manifest.txt`, `SHA256SUMS` and the scripts
- HTTPS only; no redirect to another host; no directory listing

Upload the new version folder first, then `latest/manifest.txt`, then the scripts: the installers only follow `latest`, so a half-finished upload
does nothing. To pull a bad release, put the previous manifest back as `latest/manifest.txt` (installers never move back by themselves, so someone who
already has the bad one is told how to go back with `--version`).

**Pulling it from `latest` is not enough when the release has a security problem.** `latest/manifest.txt` is what a fresh install is served, and
it is a plain file on our server: anybody (or anything that can write there) can point it at an older release, and every version folder that stays
online stays installable with `--version X.Y.Z` (the checks pass for it: its file matches its own manifest). So a version found vulnerable is
**deleted from the server** (its whole `<version>/` folder), not only taken out of `latest`. The installers refuse to go back by themselves, but only
for a person who already has a newer one.

## Tests

```bash
scripts/test-user-install.sh [path-to-a-linux-build-made-with-STONEWAVE_COMMIT=cafe0123]   # Linux: sh installer + bundle script
```
```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test-user-install.ps1             # Windows PowerShell 5.1: ps1 installer
```

Test-only settings, never for a release: `STONEWAVE_BUNDLE_TEST=1` allows `STONEWAVE_BUNDLE_COMMIT` and `STONEWAVE_ALLOW_DIRTY` in `release-bundle.sh`
(the bundle then carries a `TEST-BUNDLE-NOT-FOR-PUBLISHING` file; without the switch both are refused), and `STONEWAVE_INSTALL_SOURCED=1` makes
`install.sh` only define its functions and end with status 0 without installing anything.

They use no network of ours and only temporary folders (the PowerShell one also a throw-away registry key and a few test certificates in your own certificate store, which it removes again; Windows may ask Microsoft's revocation servers about the stand-in's chain; it never touches your real PATH). The
macOS signature check is tested with a stand-in `codesign`; the Windows signature check with a program that Windows itself signed (its name and
authority are pinned for the test; an unsigned file, a changed copy and a wrong pin must all be refused). Certificates made on the spot (a test authority, a
different certificate under it, the same subject under another authority, a self-signed one with the same subject) test the pins themselves; Windows is asked about
the real chains of the stand-in (and Microsoft's revocation servers when online). The macOS stand-in also checks that the
installer passes **its own requirement** (`-R=`: Apple's root, the Developer ID CA, a Developer ID Application leaf, our team as OU) and refuses a
signature that satisfies only the text checks. The real `irm | iex` path is run (the text, with Invoke-Expression, in a session of its own, aimed at a port
where nothing listens): it must leave no variable, function, strict mode, error preference or TLS setting behind.
