#!/bin/sh
# Collects the builds of one release into the folder that is put on stonewavetech.com/cli/, and writes the manifest.
#
#   scripts/release-bundle.sh OUT_DIR --x86_64-linux FILE [--aarch64-linux FILE] [--x86_64-macos FILE] [--aarch64-macos FILE] \
#                                     [--x86_64-windows FILE] [--no-latest]
#
# Output (the whole OUT_DIR is what the web server shows under /cli/):
#   install.sh, install.ps1            the installers, with the pinned signer values written in
#   <version>/manifest.txt             version, commit and one line per file: sha256, size, name
#   <version>/stonewave-<version>-<target>[.exe]
#   <version>/SHA256SUMS               for people who want to check by hand
#   latest/manifest.txt                a copy of the newest manifest (not with --no-latest)
#
# The builds come from where they can be made: Linux (static musl) anywhere, Windows on the PC that holds the code-signing certificate
# (the .exe must already be signed), macOS on the Macs of the Away Team (signed with the Developer ID and notarized). Nothing here signs
# anything and no key is involved: the installers check the code signatures of the Windows and macOS builds against what is pinned below.
#
# Environment:
#   STONEWAVE_BUNDLE_TEST=1      TESTS ONLY: allows STONEWAVE_BUNDLE_COMMIT (a made-up commit) and STONEWAVE_ALLOW_DIRTY=1 (a tree with changes);
#                                the bundle then carries a TEST-BUNDLE-NOT-FOR-PUBLISHING file. Without it both are refused.
#   STONEWAVE_MAC_TEAM_ID        the Apple team id (10 characters), needed when a macOS file is given
#   STONEWAVE_WIN_THUMBPRINTS    thumbprint(s) of the code-signing certificate, comma separated, needed when a Windows file is given
#
# Before it is published, run the installers' own check against the folder, on each system it is for:
#   sh OUT_DIR/install.sh --dry-run --from-dir OUT_DIR --bin-dir /tmp/x          (Linux, macOS)
#   powershell -File OUT_DIR\install.ps1 -DryRun -FromDir OUT_DIR -BinDir $env:TEMP\x      (Windows)
# A version folder is never overwritten (a published version does not change); a wrong release is replaced by a new version.
set -eu

cd "$(dirname "$0")/.."
out=${1:?usage: scripts/release-bundle.sh OUT_DIR --x86_64-linux FILE ...}
shift

latest=yes
x86_64_linux= aarch64_linux= x86_64_macos= aarch64_macos= x86_64_windows=
while [ $# -gt 0 ]; do
    case "$1" in
    --no-latest) latest=no ;;
    --x86_64-linux | --aarch64-linux | --x86_64-macos | --aarch64-macos | --x86_64-windows)
        [ $# -ge 2 ] || { echo "release-bundle: $1 needs a file" >&2; exit 1; }
        [ -f "$2" ] || { echo "release-bundle: no such file: $2" >&2; exit 1; }
        case "$1" in
        --x86_64-linux) x86_64_linux=$2 ;;
        --aarch64-linux) aarch64_linux=$2 ;;
        --x86_64-macos) x86_64_macos=$2 ;;
        --aarch64-macos) aarch64_macos=$2 ;;
        --x86_64-windows) x86_64_windows=$2 ;;
        esac
        shift
        ;;
    *) echo "release-bundle: unknown option $1" >&2; exit 1 ;;
    esac
    shift
done
[ -n "$x86_64_linux" ] || { echo "release-bundle: a Linux x86_64 build is the least a release has (--x86_64-linux)" >&2; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | tr -d '\r' | head -n 1)
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "release-bundle: Cargo.toml has no x.y.z version" >&2; exit 1; }
# STONEWAVE_BUNDLE_COMMIT and STONEWAVE_ALLOW_DIRTY exist for the tests of this script and of the installers, which build bundles of made-up programs.
# They only work together with STONEWAVE_BUNDLE_TEST=1, and a bundle made that way says so in a file and in its last line: never publish one.
test_bundle=no
if [ -n "${STONEWAVE_BUNDLE_COMMIT:-}${STONEWAVE_ALLOW_DIRTY:-}" ]; then
    if [ "${STONEWAVE_BUNDLE_TEST:-}" != 1 ]; then
        echo "release-bundle: STONEWAVE_BUNDLE_COMMIT and STONEWAVE_ALLOW_DIRTY are for the tests only (they need STONEWAVE_BUNDLE_TEST=1). A release is made from a clean commit." >&2
        exit 1
    fi
    test_bundle=yes
fi
commit=${STONEWAVE_BUNDLE_COMMIT:-$(git rev-parse --short=8 HEAD)}
printf '%s\n' "$commit" | grep -Eq '^[0-9a-f]{8}$' || { echo "release-bundle: no commit" >&2; exit 1; }
if [ "${STONEWAVE_ALLOW_DIRTY:-}" != 1 ] && [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "release-bundle: the working tree has changes; a release is built from a clean commit" >&2
    exit 1
fi
expect="stonewave $version ($commit)"

if [ -e "$out/$version" ]; then
    echo "release-bundle: $out/$version exists. A published version does not change: make a new version." >&2
    exit 1
fi
mkdir -p "$out/$version"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

manifest="$out/$version/manifest.txt"
{
    echo "stonewave-manifest 1"
    echo "version $version"
    echo "commit $commit"
} >"$manifest"
: >"$out/$version/SHA256SUMS"

add() { # TARGET FILE EXTENSION
    [ -n "$2" ] || return 0
    name="stonewave-$version-$1$3"
    cp "$2" "$out/$version/$name"
    chmod 755 "$out/$version/$name"
    # a build that can run here must say what the manifest says; one for another system is checked by the installer on that system
    ran=$("$out/$version/$name" --version 2>/dev/null | head -n 1 || true)
    if [ -n "$ran" ] && [ "$ran" != "$expect" ]; then
        echo "release-bundle: $2 says '$ran', expected '$expect'" >&2
        exit 1
    fi
    if [ -z "$ran" ]; then echo "  $1: could not be run here, not checked" >&2; fi
    sum=$(sha256_of "$out/$version/$name")
    size=$(wc -c <"$out/$version/$name" | tr -d ' ')
    echo "file $1 $sum $size $name" >>"$manifest"
    echo "$sum  $name" >>"$out/$version/SHA256SUMS"
    echo "  $1: $name ($size bytes)"
}
add x86_64-linux "$x86_64_linux" ""
add aarch64-linux "$aarch64_linux" ""
add x86_64-macos "$x86_64_macos" ""
add aarch64-macos "$aarch64_macos" ""
add x86_64-windows "$x86_64_windows" ".exe"

# the installers, with what they pin written in
mac_id=@MAC_TEAM_ID@
win_prints=@WIN_THUMBPRINTS@
if [ -n "$x86_64_macos$aarch64_macos" ]; then
    mac_id=${STONEWAVE_MAC_TEAM_ID:-}
    printf '%s\n' "$mac_id" | grep -Eq '^[A-Z0-9]{10}$' || { echo "release-bundle: STONEWAVE_MAC_TEAM_ID (10 characters) is needed with a macOS build" >&2; exit 1; }
fi
if [ -n "$x86_64_windows" ]; then
    win_prints=${STONEWAVE_WIN_THUMBPRINTS:-}
    printf '%s\n' "$win_prints" | grep -Eq '^[0-9A-Fa-f]{40}(,[0-9A-Fa-f]{40})*$' || { echo "release-bundle: STONEWAVE_WIN_THUMBPRINTS (40 hex characters each, comma separated) is needed with a Windows build" >&2; exit 1; }
fi
sed -e "s/@MAC_TEAM_ID@/$mac_id/" install/cli/install.sh.in >"$out/install.sh"
sed -e "s/@WIN_THUMBPRINTS@/$win_prints/" install/cli/install.ps1.in >"$out/install.ps1"
chmod 644 "$out/install.sh" "$out/install.ps1"

if [ "$latest" = yes ]; then
    mkdir -p "$out/latest"
    cp "$manifest" "$out/latest/manifest.txt"
fi
if [ "$test_bundle" = yes ]; then
    echo "TEST BUNDLE: made with test-only settings, the commit is not the one the programs were built from. NOT FOR PUBLISHING." >"$out/TEST-BUNDLE-NOT-FOR-PUBLISHING"
    echo "release-bundle: $out is a TEST BUNDLE (STONEWAVE_BUNDLE_TEST=1). Do not publish it."
    exit 0
fi
echo "release-bundle: $out ($version, $commit) is ready. Check it with the installers before it goes online (see the top of this script)."
