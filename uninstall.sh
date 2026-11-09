#!/bin/sh
set -eu

BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"

if [ -f "$BIN_DIR/bew" ]; then
    rm "$BIN_DIR/bew"
    echo "removed $BIN_DIR/bew"
else
    echo "uninstall: $BIN_DIR/bew not found" >&2
    exit 1
fi
