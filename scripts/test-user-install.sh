#!/bin/sh
# Tests of install/cli/install.sh.in and scripts/release-bundle.sh on this machine (Linux; the macOS signature check is tested with a stand-in
# `codesign`). Nothing goes to the internet and nothing outside a temporary folder is changed.
#
#   scripts/test-user-install.sh [PATH-TO-A-REAL-stonewave-BUILT-WITH-STONEWAVE_COMMIT=cafe0123]
#
# With a real program (a Linux build whose --version says `stonewave <Cargo version> (cafe0123)`) the bundle script is tested with it too.
set -u

here=$(cd "$(dirname "$0")/.." && pwd)
real=${1:-}
T=$(mktemp -d)
# no EXIT trap: a subshell that ends with exit would run it too (dash does); the folder is removed at the end
trap 'rm -rf "$T"; exit 130' INT TERM HUP
pass=0
failed=0

ok() { pass=$((pass + 1)); }
bad() {
    failed=$((failed + 1))
    printf 'FAIL: %s\n' "$*" >&2
}
check() { # DESCRIPTION CONDITION-COMMAND...
    desc=$1
    shift
    if "$@" >/dev/null 2>&1; then ok; else bad "$desc"; fi
}
contains() { printf '%s' "$1" | grep -Fq -- "$2"; }
has() { # DESCRIPTION TEXT NEEDLE
    if contains "$2" "$3"; then ok; else bad "$1: missing '$3' in: $2"; fi
}
hasnt() {
    if contains "$2" "$3"; then bad "$1: '$3' must not be in: $2"; else ok; fi
}

case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
aarch64 | arm64) arch=aarch64 ;;
esac
target="$arch-linux"
[ "$(uname -s)" = Linux ] || { echo "this test runs on Linux"; exit 0; }
[ "$(id -u)" -ne 0 ] || { echo "run as a normal user"; exit 0; }

sha() { sha256sum "$1" | cut -d' ' -f1; }

# a release made by hand: a program that only says who it is
mkrelease() { # DIR VERSION COMMIT SAYS [latest]
    d=$1 v=$2 c=$3 says=$4
    mkdir -p "$d/$v"
    f="$d/$v/stonewave-$v-$target"
    printf '#!/bin/sh\necho "%s"\n' "$says" >"$f"
    chmod 755 "$f"
    m="$d/$v/manifest.txt"
    printf 'stonewave-manifest 1\nversion %s\ncommit %s\nfile %s %s %s %s\n' "$v" "$c" "$target" "$(sha "$f")" "$(wc -c <"$f" | tr -d ' ')" "stonewave-$v-$target" >"$m"
    if [ "${5:-}" = latest ]; then
        mkdir -p "$d/latest"
        cp "$m" "$d/latest/manifest.txt"
    fi
}
render() { sed -e 's/@MAC_TEAM_ID@/ABCDE12345/' "$here/install/cli/install.sh.in" >"$1"; }

# ---------------------------------------------------------------- the script itself
sh -n "$here/install/cli/install.sh.in" && ok || bad "install.sh.in is not valid sh"
dash -n "$here/install/cli/install.sh.in" 2>/dev/null && ok || true
script=$(cat "$here/install/cli/install.sh.in")
has "TLS only" "$script" "--proto '=https' --tlsv1.2"
has "wget TLS only" "$script" "--https-only"
hasnt "no plain http download" "$script" 'BASE="http://'
hasnt "no sudo" "$script" "sudo "

# ---------------------------------------------------------------- a normal install, again, and a PATH line
S="$T/site1"
mkrelease "$S" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
render "$S/install.sh"
H="$T/h1"
mkdir -p "$H"
run() { HOME="$H" SHELL=/bin/bash PATH="/usr/bin:/bin" sh "$S/install.sh" --from-dir "$S" "$@" 2>&1 </dev/null; }

out=$(run); code=$?
check "fresh install exits 0" [ $code -eq 0 ]
has "fresh" "$out" "checksum ok"
has "fresh" "$out" "installed $H/.local/bin/stonewave"
has "fresh" "$out" "is not on your PATH"
check "the program is there and runs" [ "$("$H/.local/bin/stonewave" --version)" = "stonewave 0.2.0 (cafe0123)" ]
check "no startup file was touched without asking" sh -c "[ ! -e '$H/.bashrc' ] && [ ! -e '$H/.profile' ]"
check "no temporary folder is left" [ "$(ls -A "$H/.local/bin")" = "stonewave" ]
inode1=$(ls -i "$H/.local/bin/stonewave" | cut -d' ' -f1)
out=$(run); code=$?
has "second run" "$out" "already up to date"
check "second run exits 0" [ $code -eq 0 ]
inode2=$(ls -i "$H/.local/bin/stonewave" | cut -d' ' -f1)
check "an up to date install is not touched" [ "$inode1" = "$inode2" ]

out=$(run --add-to-path); code=$?
has "add to path" "$out" "added this line to $H/.bashrc"
check "the rc file has the line once" [ "$(grep -c 'added by the Stonewave installer' "$H/.bashrc")" = 1 ]
out=$(run --add-to-path)
has "add to path twice" "$out" "already has the Stonewave PATH line"
check "still once" [ "$(grep -c 'added by the Stonewave installer' "$H/.bashrc")" = 1 ]
check "the line is the plain one" grep -Fq 'export PATH="$HOME/.local/bin:$PATH"' "$H/.bashrc"
# on the path: nothing to say
out=$(HOME="$H" SHELL=/bin/bash PATH="$H/.local/bin:/usr/bin:/bin" sh "$S/install.sh" --from-dir "$S" 2>&1 </dev/null)
hasnt "on PATH" "$out" "is not on your PATH"
# --no-path: no question, no change
H2="$T/h2"
mkdir -p "$H2"
out=$(HOME="$H2" SHELL=/bin/zsh PATH="/usr/bin:/bin" sh "$S/install.sh" --from-dir "$S" --no-path 2>&1 </dev/null)
has "no-path" "$out" "run $H2/.local/bin/stonewave or add the folder yourself"
check "no-path touched nothing" [ ! -e "$H2/.zshrc" ]
HOME="$H2" SHELL=/bin/zsh PATH="/usr/bin:/bin" sh "$S/install.sh" --from-dir "$S" --add-to-path --bin-dir "$H2/my tools" >/dev/null 2>&1
check "zsh gets .zshrc and a folder with a space is quoted" grep -Fq "export PATH=\"$H2/my tools:\$PATH\"" "$H2/.zshrc"

# piped in like `curl | sh`
H3="$T/h3"
mkdir -p "$H3"
out=$(cat "$S/install.sh" | HOME="$H3" SHELL=/bin/bash PATH="/usr/bin:/bin" sh -s -- --from-dir "$S" --no-path 2>&1)
has "piped" "$out" "installed $H3/.local/bin/stonewave"

# ---------------------------------------------------------------- the question about PATH, on a real terminal (`script` gives the command a pty)
if command -v script >/dev/null 2>&1; then
    for answer in y n; do
        Hq="$T/hq-$answer"
        mkdir -p "$Hq"
        printf '%s
' "$answer" | HOME="$Hq" SHELL=/bin/bash PATH="/usr/bin:/bin" script -qec "sh '$S/install.sh' --from-dir '$S'" /dev/null >"$T/q-$answer.txt" 2>&1
        has "pty question $answer" "$(cat "$T/q-$answer.txt")" "Add it to $Hq/.bashrc?"
        if [ "$answer" = y ]; then
            check "yes adds the line" grep -Fq '# added by the Stonewave installer' "$Hq/.bashrc"
        else
            check "no adds nothing" [ ! -e "$Hq/.bashrc" ]
            has "no tells what to do" "$(cat "$T/q-$answer.txt")" 'export PATH='
        fi
    done
fi

# ---------------------------------------------------------------- dry run
H4="$T/h4"
mkdir -p "$H4"
out=$(HOME="$H4" PATH="/usr/bin:/bin" sh "$S/install.sh" --from-dir "$S" --dry-run 2>&1 </dev/null)
has "dry run" "$out" "dry run: would install"
check "dry run installs nothing" [ ! -e "$H4/.local/bin/stonewave" ]

# ---------------------------------------------------------------- everything that must be refused
refuse() { # NAME SITE-DIR NEEDLE [extra args]
    name=$1 site=$2 needle=$3
    shift 3
    hh="$T/h-$name"
    rm -rf "$hh"
    mkdir -p "$hh"
    render "$site/install.sh"
    o=$(HOME="$hh" PATH="/usr/bin:/bin" sh "$site/install.sh" --from-dir "$site" --no-path "$@" 2>&1 </dev/null)
    c=$?
    if [ "$c" -ne 0 ]; then ok; else bad "$name: must fail"; fi
    has "$name" "$o" "$needle"
    if [ -e "$hh/.local/bin/stonewave" ]; then bad "$name: something was installed"; else ok; fi
    if [ -n "$(ls -A "$hh/.local/bin" 2>/dev/null)" ]; then bad "$name: leftovers in the install folder"; else ok; fi
}

# a changed byte, same size
S2="$T/site-tamper"
mkrelease "$S2" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
printf '#!/bin/sh\necho "stonewave 0.2.0 (cafe0123)"\necho evil \n' >"$S2/0.2.0/stonewave-0.2.0-$target"
refuse tamper "$S2" "the size is"
# the same size with other content
S2b="$T/site-tamper2"
mkrelease "$S2b" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
sz=$(wc -c <"$S2b/0.2.0/stonewave-0.2.0-$target" | tr -d ' ')
head -c "$sz" /dev/zero | tr '\0' '#' >"$S2b/0.2.0/stonewave-0.2.0-$target"
refuse tamper-same-size "$S2b" "checksum does not match"
# a program that says something else than the manifest
S3="$T/site-says"
mkrelease "$S3" 0.2.0 cafe0123 "stonewave 0.2.0 (deadbeef)" latest
refuse says-other-commit "$S3" "Nothing was installed"
S3b="$T/site-says2"
mkrelease "$S3b" 0.2.0 cafe0123 "something else entirely" latest
refuse says-nothing-useful "$S3b" "says 'something else entirely'"
# a manifest that is not what it should be
S4="$T/site-badmanifest"
mkrelease "$S4" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
sed -i 's/^version 0.2.0/version 0.2.0; touch x/' "$S4/latest/manifest.txt"
refuse bad-version "$S4" "no usable version"
S5="$T/site-badname"
mkrelease "$S5" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
sed -i "s#stonewave-0.2.0-$target\$#../../etc/passwd#" "$S5/latest/manifest.txt"
refuse path-traversal "$S5" "does not accept"
S6="$T/site-badsha"
mkrelease "$S6" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
sed -i "s/ $(sha "$S6/0.2.0/stonewave-0.2.0-$target") / ABCDEF /" "$S6/latest/manifest.txt"
refuse bad-sha "$S6" "no usable checksum"
S7="$T/site-wrongformat"
mkrelease "$S7" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
sed -i '1s/.*/stonewave-manifest 2/' "$S7/latest/manifest.txt"
refuse format "$S7" "not in a format"
S8="$T/site-dirty"
mkrelease "$S8" 0.2.0 cafe0123-dirty "stonewave 0.2.0 (cafe0123-dirty)" latest
refuse dirty "$S8" "no usable commit"
S9="$T/site-nobuild"
mkrelease "$S9" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
sed -i "s/ $target / mips-linux /" "$S9/latest/manifest.txt"
refuse no-build "$S9" "has no build for $target"
S10="$T/site-missingfile"
mkrelease "$S10" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
rm "$S10/0.2.0/stonewave-0.2.0-$target"
refuse missing-file "$S10" "not found in"
S11="$T/site-wrongversion"
mkrelease "$S11" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)" latest
refuse asked-other-version "$S11" "not found in" --version 0.3.0
refuse bad-version-arg "$S11" "must look like" --version "0.2"
# root is refused: tested by looking, running as root here is not possible
has "root is refused" "$script" 'not as root'

# ---------------------------------------------------------------- downgrade
S12="$T/site-down"
mkrelease "$S12" 0.2.0 cafe0123 "stonewave 0.2.0 (cafe0123)"
mkrelease "$S12" 0.1.9 beef4567 "stonewave 0.1.9 (beef4567)" latest
render "$S12/install.sh"
H5="$T/h5"
mkdir -p "$H5"
mkdir -p "$H5/.local/bin"
cp "$S12/0.2.0/stonewave-0.2.0-$target" "$H5/.local/bin/stonewave"
out=$(HOME="$H5" PATH="/usr/bin:/bin" sh "$S12/install.sh" --from-dir "$S12" --no-path 2>&1 </dev/null); code=$?
check "an older latest is refused" [ $code -ne 0 ]
has "downgrade" "$out" "newer than the latest release 0.1.9"
check "the newer one is still there" [ "$("$H5/.local/bin/stonewave" --version)" = "stonewave 0.2.0 (cafe0123)" ]
out=$(HOME="$H5" PATH="/usr/bin:/bin" sh "$S12/install.sh" --from-dir "$S12" --no-path --version 0.1.9 2>&1 </dev/null); code=$?
check "asking for the older one by number is allowed" [ $code -eq 0 ]
check "and it is the older one now" [ "$("$H5/.local/bin/stonewave" --version)" = "stonewave 0.1.9 (beef4567)" ]
# an update
out=$(HOME="$H5" PATH="/usr/bin:/bin" sh "$S12/install.sh" --from-dir "$S12" --no-path --version 0.2.0 2>&1 </dev/null)
mkrelease "$S12" 0.3.0 abcd0123 "stonewave 0.3.0 (abcd0123)" latest
out=$(HOME="$H5" PATH="/usr/bin:/bin" sh "$S12/install.sh" --from-dir "$S12" --no-path 2>&1 </dev/null); code=$?
check "an update works" [ $code -eq 0 ]
check "to the newest" [ "$("$H5/.local/bin/stonewave" --version)" = "stonewave 0.3.0 (abcd0123)" ]

# the version comparison
STONEWAVE_INSTALL_SOURCED=1 . "$here/install/cli/install.sh.in"
set +e # the installer turns it on
for t in "0.1.9 0.2.0 1" "0.2.0 0.1.9 0" "0.2.0 0.2.0 0" "0.2.9 0.10.0 1" "1.0.0 0.99.99 0" "0.2.0 0.2.1 1" "9.9.9 10.0.0 1"; do
    # shellcheck disable=SC2086
    set -- $t
    got=$(ver_older "$1" "$2")
    [ "$got" = "$3" ] && ok || bad "ver_older $1 $2 = $got, expected $3"
done
for v in "0.2.0" "10.20.30"; do is_semver "$v" && ok || bad "is_semver $v"; done
for v in "0.2" "0.2.0-rc1" "a.b.c" "0.2.0;x" "" "01.2.3 x"; do is_semver "$v" && bad "is_semver accepts '$v'" || ok; done

# ---------------------------------------------------------------- the macOS signature check, with a stand-in for codesign
fakebin="$T/fakebin"
mkdir -p "$fakebin"
cat >"$fakebin/codesign" <<'EOF'
#!/bin/sh
# stand-in: behaviour set by FAKE_CS_* in the environment
case "$1" in
--verify) [ "${FAKE_CS_VALID:-yes}" = yes ] && exit 0 || { echo "invalid signature" >&2; exit 1; } ;;
-dv)
    echo "Executable=/x/stonewave" >&2
    echo "Authority=${FAKE_CS_AUTHORITY:-Developer ID Application: Stonewave Tech LLC (ABCDE12345)}" >&2
    echo "Authority=Developer ID Certification Authority" >&2
    echo "TeamIdentifier=${FAKE_CS_TEAM:-ABCDE12345}" >&2
    ;;
esac
EOF
chmod 755 "$fakebin/codesign"
macfile="$T/macfile"
echo x >"$macfile"
# each check runs in a shell of its own (it ends with `exit` when it refuses)
mac_with() { # PATH TEAM
    env STONEWAVE_INSTALL_SOURCED=1 PATH="$1" /bin/sh -c '. "$1"; MAC_TEAM_ID=$2; check_macos_signature "$3"' sh "$here/install/cli/install.sh.in" "$2" "$macfile" 2>&1
}
mac() { mac_with "$fakebin:$PATH" "${MAC_TEAM_ID_FOR_TEST:-ABCDE12345}"; }
out=$(mac); code=$?
check "a good Developer ID signature of our team passes" [ $code -eq 0 ]
has "mac good" "$out" "signature ok (Developer ID, team ABCDE12345)"
out=$(FAKE_CS_VALID=no mac); code=$?
check "an invalid signature is refused" [ $code -ne 0 ]
has "mac invalid" "$out" "not valid"
out=$(FAKE_CS_TEAM=ZZZZZ99999 mac); code=$?
check "another team is refused" [ $code -ne 0 ]
has "mac team" "$out" "signed by another team (ZZZZZ99999)"
out=$(FAKE_CS_AUTHORITY="Apple Development: someone" mac); code=$?
check "a development certificate is refused" [ $code -ne 0 ]
has "mac authority" "$out" "not signed with a Developer ID"
out=$(MAC_TEAM_ID_FOR_TEST="@MAC_TEAM_ID@" mac); code=$?
check "an installer that was not prepared for macOS refuses" [ $code -ne 0 ]
has "mac unprepared" "$out" "not prepared for macOS"
out=$(mac_with /nonexistent ABCDE12345); code=$?
check "no codesign means no install" [ $code -ne 0 ]

# ---------------------------------------------------------------- the bundle script
B="$T/bundle"
fake1="$T/fake-linux"
printf '#!/bin/sh\necho "stonewave 0.2.0 (cafe0123)"\n' >"$fake1"
chmod 755 "$fake1"
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$here/Cargo.toml" | head -n 1)
printf '#!/bin/sh\necho "stonewave %s (cafe0123)"\n' "$ver" >"$fake1"
bundle() { STONEWAVE_BUNDLE_COMMIT=cafe0123 STONEWAVE_ALLOW_DIRTY=1 sh "$here/scripts/release-bundle.sh" "$@" 2>&1; }
out=$(bundle "$B" --x86_64-linux "$fake1"); code=$?
check "a bundle with one Linux build" [ $code -eq 0 ]
mf="$B/$ver/manifest.txt"
check "manifest header" [ "$(head -n 3 "$mf")" = "stonewave-manifest 1
version $ver
commit cafe0123" ]
has "manifest line" "$(cat "$mf")" "file x86_64-linux $(sha "$fake1") $(wc -c <"$fake1" | tr -d ' ') stonewave-$ver-x86_64-linux"
check "latest is a copy" cmp -s "$mf" "$B/latest/manifest.txt"
check "installers are there" sh -c "[ -f '$B/install.sh' ] && [ -f '$B/install.ps1' ]"
check "SHA256SUMS can be checked" sh -c "cd '$B/$ver' && sha256sum -c SHA256SUMS"
out=$(bundle "$B" --x86_64-linux "$fake1"); code=$?
check "a published version is never overwritten" [ $code -ne 0 ]
has "bundle twice" "$out" "A published version does not change"
out=$(bundle "$T/b2" --x86_64-macos "$fake1"); code=$?
check "a bundle without a Linux x86_64 build is refused" [ $code -ne 0 ]
out=$(bundle "$T/b3" --x86_64-linux "$fake1" --x86_64-macos "$fake1"); code=$?
check "a macOS build without a team id is refused" [ $code -ne 0 ]
has "mac needs team" "$out" "STONEWAVE_MAC_TEAM_ID"
out=$(STONEWAVE_MAC_TEAM_ID="abc; rm" bundle "$T/b4" --x86_64-linux "$fake1" --x86_64-macos "$fake1"); code=$?
check "a team id with odd characters is refused" [ $code -ne 0 ]
out=$(bundle "$T/b5" --x86_64-linux "$fake1" --x86_64-windows "$fake1"); code=$?
check "a Windows build without thumbprints is refused" [ $code -ne 0 ]
out=$(STONEWAVE_WIN_THUMBPRINTS=nothex bundle "$T/b6" --x86_64-linux "$fake1" --x86_64-windows "$fake1"); code=$?
check "bad thumbprints are refused" [ $code -ne 0 ]
good_print=0123456789ABCDEF0123456789ABCDEF01234567
out=$(STONEWAVE_MAC_TEAM_ID=ABCDE12345 STONEWAVE_WIN_THUMBPRINTS=$good_print bundle "$T/b7" --x86_64-linux "$fake1" --aarch64-linux "$fake1" --x86_64-macos "$fake1" --aarch64-macos "$fake1" --x86_64-windows "$fake1"); code=$?
check "a full bundle" [ $code -eq 0 ]
has "pins written" "$(cat "$T/b7/install.sh")" 'MAC_TEAM_ID="ABCDE12345"'
has "pins written (windows)" "$(cat "$T/b7/install.ps1")" "SignerThumbprints = '$good_print'"
has "windows name" "$(cat "$T/b7/$ver/manifest.txt")" "stonewave-$ver-x86_64-windows.exe"
out=$(bundle "$T/b8" --x86_64-linux "$fake1" --no-latest); code=$?
check "--no-latest" sh -c "[ $code -eq 0 ] && [ ! -e '$T/b8/latest' ]"
printf '#!/bin/sh\necho "stonewave 9.9.9 (cafe0123)"\n' >"$T/fake-wrong"
chmod 755 "$T/fake-wrong"
out=$(bundle "$T/b9" --x86_64-linux "$T/fake-wrong"); code=$?
check "a program that says another version is refused" [ $code -ne 0 ]
out=$(STONEWAVE_BUNDLE_COMMIT=cafe0123 sh "$here/scripts/release-bundle.sh" "$T/b10" --x86_64-linux "$fake1" 2>&1); code=$?
if [ -n "$(cd "$here" && git status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    check "a dirty tree is refused" [ $code -ne 0 ]
fi

# the bundle made here installs through the installer, end to end
H6="$T/h6"
mkdir -p "$H6"
out=$(HOME="$H6" PATH="/usr/bin:/bin" sh "$B/install.sh" --from-dir "$B" --no-path 2>&1 </dev/null); code=$?
check "a bundle installs through its own installer" [ $code -eq 0 ]
has "end to end" "$out" "release   $ver (cafe0123)"

# ---------------------------------------------------------------- the real program, when given
if [ -n "$real" ]; then
    R="$T/real"
    mkdir -p "$R"
    out=$(STONEWAVE_BUNDLE_COMMIT=cafe0123 STONEWAVE_ALLOW_DIRTY=1 sh "$here/scripts/release-bundle.sh" "$R" --x86_64-linux "$real" 2>&1); code=$?
    check "the real program goes into a bundle" [ $code -eq 0 ]
    H7="$T/h7"
    mkdir -p "$H7"
    out=$(HOME="$H7" PATH="/usr/bin:/bin" sh "$R/install.sh" --from-dir "$R" --no-path 2>&1 </dev/null); code=$?
    check "the real program installs" [ $code -eq 0 ]
    has "real" "$out" "runs      stonewave $ver (cafe0123)"
    check "and answers" [ "$("$H7/.local/bin/stonewave" --version)" = "stonewave $ver (cafe0123)" ]
    # no terminal: `stonewave` does not open a menu, it ends (this is what a script sees)
    timeout 10 "$H7/.local/bin/stonewave" </dev/null >/dev/null 2>&1
    check "stonewave without a terminal does not block" [ $? -ne 124 ]
fi

rm -rf "$T"
printf '%s passed, %s failed\n' "$pass" "$failed"
[ "$failed" -eq 0 ]
