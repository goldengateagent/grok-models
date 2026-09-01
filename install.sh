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