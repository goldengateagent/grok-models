#!/usr/bin/env bash
# Build a release package: the native grok-models binary plus README.md.
#
# Usage:
#   rust/make-release.sh                       build for host, package into dist/
#   TARGET=<rust-target> rust/make-release.sh  cross-build (needs target installed),
#                                              e.g. TARGET=x86_64-unknown-linux-musl
#
# Output:
#   dist/grok-models-<version>-<target>.zip      (Windows) containing grok-models.exe and README.md
#   dist/grok-models-<version>-<target>.tar.gz   (Unix) containing grok-models and README.md
#   dist/<archive>.sha256                        checksum for the archive
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$HERE/grok-models.rs"
DIST="$HERE/dist"
TARGET="${TARGET:-}"

# Version comes from Cargo.toml so the archive name always matches the crate.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CRATE/Cargo.toml" | head -n1)"
if [[ -z "$VERSION" ]]; then
  echo "error: could not read version from Cargo.toml" >&2
  exit 1
fi

# Reuse the standard build; it leaves the binary at rust/grok-models.
"$HERE/build-grok-models.sh"
BIN="$HERE/grok-models"

# Name the package after the platform it was built for.
if [[ -n "$TARGET" ]]; then
  PLATFORM="$TARGET"
else
  PLATFORM="$(rustc -vV | sed -n 's/^host: //p')"
fi

# Create dist/ if missing and clear any previous output for this platform.
STAGE="$DIST/grok-models-$VERSION-$PLATFORM"
rm -rf "$STAGE"
mkdir -p "$STAGE"

# Windows builds emit .exe; handle both
if [[ -f "${BIN}.exe" ]]; then
  cp "${BIN}.exe" "$STAGE/grok-models.exe"
else
  cp "$BIN" "$STAGE/grok-models"
  chmod 755 "$STAGE/grok-models"
fi

cp "$HERE/../README.md" "$STAGE/README.md"

# .zip for Windows, .tar.gz for Unix
if [[ "$PLATFORM" == *"-windows-"* ]]; then
  ARCHIVE="$(basename "${STAGE}.zip")"
  powershell -Command "Compress-Archive -Path '${STAGE}' -DestinationPath '${DIST}/${ARCHIVE}' -Force"
else
  ARCHIVE="$(basename "${STAGE}.tar.gz")"
  tar -czf "$DIST/$ARCHIVE" -C "$DIST" "$(basename "$STAGE")"
fi

(
  cd "$DIST"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$ARCHIVE" > "${ARCHIVE}.sha256"
  else
    shasum -a 256 "$ARCHIVE" > "${ARCHIVE}.sha256"
  fi
)

rm -rf "$STAGE"
echo "release: dist/$ARCHIVE"
