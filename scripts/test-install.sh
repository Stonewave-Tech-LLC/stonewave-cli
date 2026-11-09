#!/bin/sh
# Checks a built release on a Linux machine without installing anything:
#   scripts/test-install.sh dist/0.1.0
# Runs `doctor` (read only) and install.sh in --dry-run mode, including a
# file with the wrong checksum, which must be refused.
set -u

dir=${1:?usage: scripts/test-install.sh <dist dir>}
case "$(uname -m)" in
x86_64 | amd64) own=x86_64 other=aarch64 ;;
aarch64 | arm64) own=aarch64 other=x86_64 ;;
*) echo "unsupported CPU" >&2; exit 2 ;;
esac
bin="$dir/stonewave-$own-linux"
failed=0

# Downloads and copies from Windows lose the executable bit: run doctor from a
# private executable copy and leave the release folder as it is.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cp "$bin" "$tmp/stonewave"
chmod 0755 "$tmp/stonewave"

step() { printf '\n== %s\n' "$*"; }
doctor_ok() { # 0 = all checks passed, 1 = a check failed; anything else is a broken binary
    if [ "$1" -gt 1 ]; then
        echo "-- FAILED: doctor exit $1"
        failed=1
    fi
}
expect() { # expect <wanted exit code> <command...>
    want=$1
    shift
    "$@"
    got=$?
    if [ "$got" -eq "$want" ]; then
        echo "-- ok (exit $got)"
    else
        echo "-- FAILED: exit $got, wanted $want"
        failed=1
    fi
}

step "doctor (read only)"
"$tmp/stonewave" doctor --no-anim
got=$?
echo "-- exit $got (1 just means a check failed on this machine)"
doctor_ok "$got"

step "doctor --json"
"$tmp/stonewave" doctor --json
got=$?
echo
doctor_ok "$got"

step "install.sh --dry-run with the right file"
expect 0 sh "$dir/install.sh" --dry-run --from "$bin"

step "install.sh --dry-run with the wrong file (must be refused)"
expect 1 sh "$dir/install.sh" --dry-run --from "$dir/stonewave-$other-linux"

if [ "$(id -u)" -ne 0 ]; then
    step "install.sh without root and without --dry-run (must be refused)"
    expect 1 sh "$dir/install.sh" --from "$bin"
fi

step "result"
if [ "$failed" -eq 0 ]; then echo "all install checks passed"; else echo "some install checks FAILED"; fi
exit "$failed"
