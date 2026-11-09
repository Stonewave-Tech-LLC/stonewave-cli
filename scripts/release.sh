#!/bin/sh
# Builds a release: static Linux binaries for x86_64 and aarch64, SHA256SUMS,
# optional minisign signatures, and install.sh with the checksums written in.
#
#   scripts/release.sh 0.1.0
#
# Output goes to dist/<version>/. The version must match Cargo.toml.
# Signing: set STONEWAVE_MINISIGN_KEY to the path of the secret key. The key is
# never part of this repository; without it the release is unsigned (fine for
# tests, not for publishing).
set -eu

version=${1:?usage: scripts/release.sh <version>}
cd "$(dirname "$0")/.."

crate_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ "$version" = "$crate_version" ] || {
    echo "release.sh: version $version does not match Cargo.toml ($crate_version)" >&2
    exit 1
}

target_dir=${CARGO_TARGET_DIR:-target}
out="dist/$version"
rm -rf "$out"
mkdir -p "$out"

for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    rustup target add "$target" >/dev/null
    cargo build --release --locked --target "$target"
    arch=${target%%-*}
    cp "$target_dir/$target/release/stonewave" "$out/stonewave-$arch-linux"
done

# Some sha256sum builds mark binary mode with "*"; keep one format everywhere.
(cd "$out" && sha256sum stonewave-x86_64-linux stonewave-aarch64-linux | sed 's/ \*/  /' >SHA256SUMS)
sha_x86_64=$(sed -n 's/ .*stonewave-x86_64-linux$//p' "$out/SHA256SUMS")
sha_aarch64=$(sed -n 's/ .*stonewave-aarch64-linux$//p' "$out/SHA256SUMS")

sed -e "s/@VERSION@/$version/g" \
    -e "s/@SHA256_X86_64@/$sha_x86_64/" \
    -e "s/@SHA256_AARCH64@/$sha_aarch64/" \
    install/install.sh.in >"$out/install.sh"
if grep -q '@[A-Z0-9_]*@' "$out/install.sh"; then
    echo "release.sh: a placeholder was left in install.sh" >&2
    exit 1
fi
sh -n "$out/install.sh"
install_sha=$(sha256sum "$out/install.sh" | cut -d' ' -f1)
echo "$install_sha  install.sh" >>"$out/SHA256SUMS"

if [ -n "${STONEWAVE_MINISIGN_KEY:-}" ]; then
    minisign -S -s "$STONEWAVE_MINISIGN_KEY" -m "$out/stonewave-x86_64-linux" "$out/stonewave-aarch64-linux" "$out/install.sh"
    signed=yes
else
    signed="no (STONEWAVE_MINISIGN_KEY not set)"
fi

cat <<EOF

release $version in $out
  signed: $signed

The command Wave shows (the checksum pins install.sh):

  curl -fsSLO https://stonewavetech.com/install/$version/install.sh \\
    && echo "$install_sha  install.sh" | sha256sum -c - \\
    && sudo sh install.sh
EOF
