#!/usr/bin/env bash
# SD Backup Manager アンインストール（12.2-8）
#
# アプリ本体・サービス・自動起動エントリだけを削除する。
# SSD の一時データ・DB・設定・NAS 上のデータ・キーリングのパスワードは削除しない
# （NAS へ未転送のデータを失わないため）。必要なら案内に従って手動で削除する。
set -euo pipefail

[ "$(id -u)" -ne 0 ] || { echo "root では実行しないでください" >&2; exit 1; }

BIN_DIR="$HOME/.local/bin"
CONF="${XDG_CONFIG_HOME:-$HOME/.config}"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"

if command -v systemctl >/dev/null; then
  systemctl --user disable --now sd-backup-daemon.service 2>/dev/null || true
fi
pkill -x sd-backup-gui 2>/dev/null || true

rm -f "$BIN_DIR/sd-backup-daemon" "$BIN_DIR/sd-backup-gui" "$BIN_DIR/sd-backup"
rm -f "$CONF/systemd/user/sd-backup-daemon.service"
rm -f "$CONF/autostart/sd-backup-gui.desktop" "$DATA/applications/sd-backup-gui.desktop"
rm -f "$DATA/icons/hicolor/scalable/apps/sd-backup-manager.svg"
command -v systemctl >/dev/null && systemctl --user daemon-reload || true

cat <<EOF
アプリを削除しました。次のデータは残しています:
  設定:          $CONF/sd-backup-manager
  DB・ログ:      $DATA/sd-backup-manager
  SSD 一時保存:  ~/SDBackupStaging（NAS へ未転送のファイルが含まれる可能性があります）
  NAS パスワード: キーリング（「パスワードと鍵」アプリで "sd-backup-manager" を削除）
不要であることを確認してから手動で削除してください。
EOF
