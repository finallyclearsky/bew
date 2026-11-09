#!/bin/sh
set -eu

REPO="https://github.com/finallyblueskies/beads-explorer"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"

if ! command -v cargo >/dev/null 2>&1; then
    echo "install: cargo is required (https://rustup.rs)" >&2
    exit 1
fi

src="$(dirname "$0")"
if ! { [ -f "$src/Cargo.toml" ] && grep -q '^name = "bew"' "$src/Cargo.toml"; }; then
    if ! command -v git >/dev/null 2>&1; then
        echo "install: git is required" >&2
        exit 1
    fi
    src="$(mktemp -d)"
    trap 'rm -rf "$src"' EXIT INT TERM
    git clone --quiet --depth 1 "$REPO" "$src"
fi

cargo build --release --manifest-path "$src/Cargo.toml"
mkdir -p "$BIN_DIR"
install -m 755 "$src/target/release/bew" "$BIN_DIR/bew"
echo "installed $BIN_DIR/bew"

# bew was called be before 0.2.
if [ -f "$BIN_DIR/be" ] && "$BIN_DIR/be" --version 2>/dev/null | grep -q '^be 0\.1\.'; then
    rm "$BIN_DIR/be"
    echo "removed the old $BIN_DIR/be"
fi

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on your PATH" ;;
esac
