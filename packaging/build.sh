#!/bin/sh
# Builds a DEB and an RPM of the daemon into target/package/.
#
#   sh packaging/build.sh
#
# Needs cargo-deb and cargo-generate-rpm, and a host that can run the target's
# binary, since the packaged unit is the one that binary prints. TARGET picks
# another musl target. Neither package enables or starts the service.
set -eu
cd "$(dirname "$0")/.."
target=${TARGET:-x86_64-unknown-linux-musl}
out=target/package
cargo build --release --target "$target" --features bin --bin unquic
rm -rf "$out"
mkdir -p "$out"
# One template: the unit the daemon prints, for the installed paths.
"target/$target/release/unquic" systemd-unit /etc/unquic/unquic.toml /run/unquic.pid /usr/bin/unquic \
    > "$out/unquic.service"
cargo deb --no-build --target "$target" --output "$out/"
cargo generate-rpm --auto-req disabled --target "$target" --output "$out/"
ls -l "$out"
