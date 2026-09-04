#!/bin/bash
set -e

REPO="goldengateagent/grok-models"
VERSION="1.0.0"
ARTIFACT="grok-models"

INSTALL_DIR="$HOME/.grok-models"
BIN_DIR="$INSTALL_DIR/bin"

case "$(uname -s)" in
    Darwin)
        case "$(uname -m)" in
            arm64)  TARGET="aarch64-apple-darwin" ;;
            x86_64) TARGET="x86_64-apple-darwin" ;;
            *) echo "Unsupported macOS architecture: $(uname -m)"; exit 1 ;;
        esac
        ;;
    Linux)
        case "$(uname -m)" in
            aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
            x86_64|amd64)  TARGET="x86_64-unknown-linux-gnu" ;;
            *) echo "Unsupported Linux architecture: $(uname -m)"; exit 1 ;;
        esac
        ;;
    *)
        echo "Unsupported OS: $(uname -s)"
        exit 1
        ;;
esac

FILE="${ARTIFACT}-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/v${VERSION}/${FILE}"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $FILE..."

if ! curl -fsSL "$URL" -o "$TMP/$FILE"; then
    echo "Error: no release available for $TARGET"
    echo "Expected asset: $FILE"
    exit 1
fi

tar -xzf "$TMP/$FILE" -C "$TMP"

STAGE="$TMP/${ARTIFACT}-${VERSION}-${TARGET}"
mkdir -p "$BIN_DIR"
cp "$STAGE/$ARTIFACT" "$BIN_DIR/$ARTIFACT"
chmod +x "$BIN_DIR/$ARTIFACT"

PATH_LINE='export PATH="$HOME/.grok-models/bin:$PATH"'
PATH_ADDED=0

case "$(basename "${SHELL:-}")" in
    zsh)  RC="$HOME/.zshrc" ;;
    bash) RC="$HOME/.bashrc" ;;
    *)    RC="$HOME/.profile" ;;
esac

if [ ! -f "$RC" ]; then
    echo "Error: $RC not found; not creating a shell rc file."
    echo "Binary is at $BIN_DIR/$ARTIFACT"
    echo "Add this to your PATH:"
    echo "  $PATH_LINE"
    exit 1
fi

if grep -Fq '.grok-models/bin' "$RC"; then
    PATH_ADDED=0
else
    printf '\n# grok-models\n%s\n' "$PATH_LINE" >> "$RC"
    PATH_ADDED=1
fi

# WSL: point GROK_HOME/CODEX_HOME at the Windows profile home, not the
# WSL Linux home. The binary and grok-models.py already honor these vars.
WSL_HOMES_ADDED=0
is_wsl() {
    grep -qi microsoft /proc/version 2>/dev/null || grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null
}
if is_wsl; then
    if ! command -v wslpath >/dev/null 2>&1 || [ -z "${USERPROFILE:-}" ]; then
        echo "Warning: WSL detected but wslpath/USERPROFILE is unavailable;"
        echo "set GROK_HOME and CODEX_HOME manually to your Windows profile's .grok and .codex dirs."
    elif grep -Fq '# grok-models WSL homes' "$RC"; then
        WSL_HOMES_ADDED=0
    else
        cat >> "$RC" << 'EOF'

# grok-models WSL homes
if [ -z "$GROK_HOME" ] && command -v wslpath >/dev/null 2>&1 && [ -n "$USERPROFILE" ]; then
    export GROK_HOME="$(wslpath "$USERPROFILE")/.grok"
fi
if [ -z "$CODEX_HOME" ] && command -v wslpath >/dev/null 2>&1 && [ -n "$USERPROFILE" ]; then
    export CODEX_HOME="$(wslpath "$USERPROFILE")/.codex"
fi
EOF
        WSL_HOMES_ADDED=1
    fi
fi

export PATH="$BIN_DIR:$PATH"

echo
echo "Installed $ARTIFACT to:"
echo "  $BIN_DIR/$ARTIFACT"
echo
if [ "$PATH_ADDED" -eq 1 ]; then
    echo "PATH updated in $RC."
    echo "Restart your shell or run:"
    echo "  export PATH=\"$BIN_DIR:\$PATH\""
else
    echo "PATH already contains $BIN_DIR (found in $RC)."
fi
if [ "$WSL_HOMES_ADDED" -eq 1 ]; then
    echo "WSL detected: added GROK_HOME/CODEX_HOME exports to $RC."
    echo "Restart your shell to apply them."
fi