#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR=/opt/kealight
SERVICE_NAME=kealight
SYSTEMD_UNIT_DIR=/etc/systemd/system
CONFIG_SRC="${CONFIG_SRC:-"$(dirname "$0")/kealight.toml.example"}"

git pull

echo "[1/5] 編譯 release ..."
cargo build --release --manifest-path "$(dirname "$0")/Cargo.toml"

echo "[2/5] 安裝到 $INSTALL_DIR ..."
mkdir -p "$INSTALL_DIR"
cp "$(dirname "$0")/target/release/kealight" "$INSTALL_DIR/kealight"
