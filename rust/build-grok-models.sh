#!/usr/bin/env bash
# Build the native grok-models binary and install it as rust/grok-models.
#
# Usage:
#   rust/build-grok-models.sh                 build for the host machine (macOS)
#   TARGET=<rust-target> rust/build-grok-models.sh   cross-build (needs target installed)
#
# Linux/WSL: run this script on that machine; it produces a static binary when
# built against the musl target there:
#   TARGET=x86_64-unknown-linux-musl rust/build-grok-models.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$HERE/grok-models.rs"
TARGET="${TARGET:-}"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
SYSROOT="$(rustc --print sysroot)"
# Strip builder machine prefixes from panic/file!() paths in the binary.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=${CRATE}= --remap-path-prefix=${CARGO_HOME}= --remap-path-prefix=${SYSROOT}="

if [[ -n "$TARGET" ]]; then
  cargo build --release --manifest-path "$CRATE/Cargo.toml" --target "$TARGET"
  BIN="$CRATE/target/$TARGET/release/grok-models"
else
  cargo build --release --manifest-path "$CRATE/Cargo.toml"
  BIN="$CRATE/target/release/grok-models"
fi

cp "$BIN" "$HERE/grok-models"
echo "installed: $HERE/grok-models"
