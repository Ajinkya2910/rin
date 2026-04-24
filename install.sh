#!/bin/sh
# install.sh — One-line installer for rv
#
# Usage:
#   curl -sSf https://raw.githubusercontent.com/Ajinkya2910/rv/main/install.sh | sh
#
# What this does:
#   1. Detects your OS (Linux or macOS) and CPU (x86_64 or ARM)
#   2. Downloads the correct pre-built rv binary from GitHub Releases
#   3. Installs it to ~/.rv/bin/rv
#   4. Tells you how to add it to your PATH
#
# No root/sudo required. No Rust installation needed.

set -e

REPO="Ajinkya2910/rv"
INSTALL_DIR="$HOME/.rv/bin"

# --- Detect platform ---

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64) TARGET="x86_64-unknown-linux-musl" ;;
            *) echo "Error: Unsupported Linux architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    Darwin)
        case "$ARCH" in
            arm64) TARGET="aarch64-apple-darwin" ;;
            x86_64) TARGET="x86_64-apple-darwin" ;;
            *) echo "Error: Unsupported macOS architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    *)
        echo "Error: Unsupported OS: $OS"
        exit 1
        ;;
esac

echo "Detected platform: $TARGET"

# --- Find latest release ---

LATEST_URL="https://github.com/$REPO/releases/latest/download/rv-$TARGET.tar.gz"

echo "Downloading rv from $LATEST_URL..."

# --- Download and install ---

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

curl -sL "$LATEST_URL" -o "$TMP_DIR/rv.tar.gz"

# Verify we got a real file, not a 404 HTML page
FILE_SIZE=$(wc -c < "$TMP_DIR/rv.tar.gz" | tr -d ' ')
if [ "$FILE_SIZE" -lt 1000 ]; then
    echo "Error: Download failed. No release found for $TARGET."
    echo "Check https://github.com/$REPO/releases for available binaries."
    exit 1
fi

tar xzf "$TMP_DIR/rv.tar.gz" -C "$TMP_DIR"

# Create install directory and copy binary
mkdir -p "$INSTALL_DIR"
mv "$TMP_DIR/rv" "$INSTALL_DIR/rv"
chmod +x "$INSTALL_DIR/rv"

echo ""
echo "✓ rv installed to $INSTALL_DIR/rv"

# --- Check if already in PATH ---

case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        echo "✓ $INSTALL_DIR is already in your PATH"
        echo ""
        echo "Try it:"
        echo "  rv resolve DESeq2"
        ;;
    *)
        echo ""
        echo "Add rv to your PATH by adding this line to your shell config:"
        echo ""
        # Detect shell config file
        SHELL_NAME="$(basename "$SHELL")"
        case "$SHELL_NAME" in
            zsh)  CONFIG_FILE="~/.zshrc" ;;
            bash) CONFIG_FILE="~/.bashrc" ;;
            *)    CONFIG_FILE="your shell config" ;;
        esac
        echo "  echo 'export PATH=\"\$HOME/.rv/bin:\$PATH\"' >> $CONFIG_FILE"
        echo ""
        echo "Then restart your shell or run:"
        echo "  export PATH=\"\$HOME/.rv/bin:\$PATH\""
        echo ""
        echo "Try it:"
        echo "  rv resolve DESeq2"
        ;;
esac