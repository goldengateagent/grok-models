#!/bin/zsh
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

if [[ -n "$TARGET" ]]; then
  cargo build --release --manifest-path "$CRATE/Cargo.toml" --target "$TARGET"
  BIN="$CRATE/target/$TARGET/release/grok-models"
else
  cargo build --release --manifest-path "$CRATE/Cargo.toml"
  BIN="$CRATE/target/release/grok-models"
fi

cp "$BIN" "$HERE/grok-models"
echo "installed: $HERE/grok-models"
