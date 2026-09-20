#!/usr/bin/env bash
# Bump the release version across the files that carry it.
#
# Usage:
#   ./bump-version.sh 1.2.0
#
# Writes NEW into Cargo.toml, install.sh, install.ps1, and README.md,
# then refreshes Cargo.lock via cargo. Git commit, tag, and push stay manual.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$HERE/rust/grok-models.rs"

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <new-version>" >&2
  exit 1
fi
NEW="$1"
if [[ ! "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: version must look like 1.2.0, got '$NEW'" >&2
  exit 1
fi

OLD="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CRATE/Cargo.toml" | head -n1)"
if [[ -z "$OLD" ]]; then
  echo "error: could not read version from $CRATE/Cargo.toml" >&2
  exit 1
fi
if [[ "$OLD" == "$NEW" ]]; then
  echo "already at $NEW; nothing to do."
  exit 0
fi

perl -pi -e "s/^version = \".*\"/version = \"$NEW\"/" "$CRATE/Cargo.toml"
perl -pi -e "s/^VERSION=\".*\"/VERSION=\"$NEW\"/" "$HERE/install.sh"
perl -pi -e "s/^\\\$Version = \".*\"/\\\$Version = \"$NEW\"/" "$HERE/install.ps1"
perl -pi -e "s{raw/v\\d+\\.\\d+\\.\\d+/}{raw/v$NEW/}g" "$HERE/README.md"

# Refresh Cargo.lock from the new Cargo.toml (no hand edits to the lock).
(cd "$CRATE" && cargo check --quiet)

# Verify: old dotted version must be gone from the hand-edited files.
if grep -rn --fixed-strings "$OLD" \
  "$CRATE/Cargo.toml" "$HERE/install.sh" "$HERE/install.ps1" "$HERE/README.md"; then
  echo "error: old version $OLD still present above" >&2
  exit 1
fi

echo "bumped $OLD -> $NEW"
echo "files: Cargo.toml install.sh install.ps1 README.md Cargo.lock(regenerated)"
echo "next (manual): git diff, git commit, git tag v$NEW, git push, git push origin v$NEW"
