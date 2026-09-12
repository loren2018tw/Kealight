#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR=/opt/kealight
SERVICE_NAME=kealight
SYSTEMD_UNIT_DIR=/etc/systemd/system
CONFIG_SRC="${CONFIG_SRC:-"$(dirname "$0")/kealight.toml.example"}"

usage() {
    echo "用法: $0 [install|uninstall]"
    echo "  install   （預設）編譯、安裝到 /opt/kealight、註冊 systemd 開機自啟"
    echo "  uninstall 停止服務並移除安裝"
    exit 0
}

cmd="${1:-install}"

if [ "$cmd" = "install" ]; then
    if [ "$(id -u)" -ne 0 ]; then
        echo "錯誤：install 需要 root 權限（sudo $0 install）" >&2
        exit 1
    fi

    echo "[1/5] 編譯 release ..."
    cargo build --release --manifest-path "$(dirname "$0")/Cargo.toml"

    echo "[2/5] 安裝到 $INSTALL_DIR ..."
    mkdir -p "$INSTALL_DIR"
    cp "$(dirname "$0")/target/release/kealight" "$INSTALL_DIR/kealight"

    if [ ! -f "$INSTALL_DIR/kealight.toml" ]; then
        if [ -f "$CONFIG_SRC" ]; then
            cp "$CONFIG_SRC" "$INSTALL_DIR/kealight.toml"
        else
            cat > "$INSTALL_DIR/kealight.toml" <<'EOF'
# kea 設定檔路徑（必填）
kea_config = "/etc/kea/kea-dhcp4.conf"
# HTTP 綁定位址與埠
bind = "127.0.0.1"
port = 7777
# 寫回前保留的備份份數
backup_keep = 10
EOF
        fi
        echo "    已建立 $INSTALL_DIR/kealight.toml，請確認 kea_config 路徑後再啟動。"
    fi

    echo "[3/5] 註冊 systemd 服務 ..."
    cat > "$SYSTEMD_UNIT_DIR/$SERVICE_NAME.service" <<EOF
[Unit]
Description=Kealight - Kea DHCP config editor
After=network.target

[Service]
Type=simple
ExecStart=$INSTALL_DIR/kealight
WorkingDirectory=$INSTALL_DIR
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload

    echo "[4/5] 開機自動啟動 ..."
    systemctl enable "$SERVICE_NAME.service"

    echo "[5/5] 完成。"
    echo ""
    echo "下一步："
    echo "  1. 編輯 $INSTALL_DIR/kealight.toml（確認 kea_config 路徑）"
    echo "  2. sudo systemctl start $SERVICE_NAME"
    echo "  3. 瀏覽器開啟 http://$(hostname -I 2>/dev/null | awk '{print $1}'):$(grep -oP '(?<=^port = ).*' "$INSTALL_DIR/kealight.toml" 2>/dev/null || echo 7777)"

elif [ "$cmd" = "uninstall" ]; then
    if [ "$(id -u)" -ne 0 ]; then
        echo "錯誤：uninstall 需要 root 權限（sudo $0 uninstall）" >&2
        exit 1
    fi
    echo "停止並移除服務 ..."
    systemctl stop "$SERVICE_NAME.service" 2>/dev/null || true
    systemctl disable "$SERVICE_NAME.service" 2>/dev/null || true
    rm -f "$SYSTEMD_UNIT_DIR/$SERVICE_NAME.service"
    systemctl daemon-reload
    echo "移除 $INSTALL_DIR ..."
    rm -rf "$INSTALL_DIR"
    echo "已完成。"

else
    usage
fi