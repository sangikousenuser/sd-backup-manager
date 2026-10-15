#!/usr/bin/env bash
# SD Backup Manager インストーラ（FR-039 / 12.2）
#
#   curl -fsSLO https://github.com/sangikousenuser/sd-backup-manager/releases/download/v0.1.0/install.sh
#   bash install.sh --version v0.1.0
#
# - GitHub Release の成果物を取得し、SHA256 を照合してから展開する
# - 一般ユーザー権限で ~/.local/bin に配置する（root での実行は拒否）
# - 依存パッケージの導入だけは、内容を表示して確認したうえで sudo apt-get を使う
# - 既存の設定・DB は保持し、更新前に DB のバックアップを取る
# - systemd ユーザーサービスと GUI の自動起動を登録する
set -euo pipefail

REPO="${SDBM_REPO:-sangikousenuser/sd-backup-manager}"
VERSION="latest"
TARBALL=""
ASSUME_YES=0
SKIP_DEPS=0

BIN_DIR="$HOME/.local/bin"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/sd-backup-manager"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/sd-backup-manager"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICON_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"

# 実行時に必要な Ubuntu パッケージ
DEPS=(
  udisks2            # メディアの検出・読み込み専用マウント・アンマウント
  gvfs-backends      # SMB (smb://) バックエンド
  gvfs-fuse          # GVfs をファイルパスとして扱う
  gnome-keyring      # Secret Service（NAS パスワードの保管）
  pulseaudio-utils   # 通知音 (paplay)
  fonts-noto-cjk     # 日本語フォント
  fontconfig
  libxkbcommon0      # GUI (winit/iced)
  libwayland-client0
  libx11-6
  libxcursor1
  libxi6
  libxrandr2
  libvulkan1
  mesa-vulkan-drivers
)

usage() {
  cat <<EOF
使い方: install.sh [--version vX.Y.Z|latest] [--tarball FILE] [--yes] [--skip-deps]

  --version   インストールするリリースタグ（既定: latest）
  --tarball   ダウンロードせずにローカルの成果物を使う（同じフォルダの SHA256SUMS で照合）
  --yes       確認を省略する
  --skip-deps 依存パッケージの確認・導入を行わない
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --tarball) TARBALL="$2"; shift 2 ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    --skip-deps) SKIP_DEPS=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "不明なオプション: $1" >&2; usage; exit 2 ;;
  esac
done

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m警告:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31mエラー:\033[0m %s\n' "$*" >&2; exit 1; }
confirm() {
  [ "$ASSUME_YES" = 1 ] && return 0
  read -r -p "$1 [y/N] " ans
  [ "$ans" = "y" ] || [ "$ans" = "Y" ]
}

# --- 1. 環境確認 -----------------------------------------------------------
[ "$(id -u)" -ne 0 ] || die "root では実行しないでください（アプリは一般ユーザーで動作します）"
[ "$(uname -s)" = "Linux" ] || die "Linux 専用です"
ARCH="$(uname -m)"
[ "$ARCH" = "x86_64" ] || die "未対応のアーキテクチャです: $ARCH（x86_64 のみ配布しています）"
if [ -r /etc/os-release ]; then
  . /etc/os-release
  if [ "${ID:-}" != "ubuntu" ] || [ "${VERSION_ID:-}" != "26.04" ]; then
    warn "Ubuntu 26.04 以外（${PRETTY_NAME:-unknown}）は実機未検証です"
    confirm "続行しますか？" || exit 1
  fi
fi
command -v systemctl >/dev/null || die "systemd が見つかりません"
systemctl --user show-environment >/dev/null 2>&1 || die "systemd ユーザーセッションに接続できません（デスクトップにログインした状態で実行してください）"
for c in curl sha256sum tar; do command -v "$c" >/dev/null || die "$c が必要です"; done

# --- 2. 依存パッケージ -----------------------------------------------------
if [ "$SKIP_DEPS" = 0 ] && command -v dpkg-query >/dev/null; then
  missing=()
  for p in "${DEPS[@]}"; do
    dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q "install ok installed" || missing+=("$p")
  done
  if [ ${#missing[@]} -gt 0 ]; then
    info "次のパッケージが必要です（管理者権限で apt-get install を実行します）:"
    printf '    %s\n' "${missing[@]}"
    if confirm "インストールしますか？"; then
      sudo apt-get update
      sudo apt-get install -y "${missing[@]}"
    else
      warn "依存パッケージが不足しています。一部機能が動作しない可能性があります"
    fi
  fi
fi

# --- 3. 取得と検証 ---------------------------------------------------------
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [ -n "$TARBALL" ]; then
  [ -f "$TARBALL" ] || die "$TARBALL がありません"
  cp "$TARBALL" "$WORK/"
  SUMS="$(dirname "$TARBALL")/SHA256SUMS"
  [ -f "$SUMS" ] || die "$SUMS がありません（チェックサムなしではインストールしません）"
  cp "$SUMS" "$WORK/SHA256SUMS"
  ASSET="$(basename "$TARBALL")"
else
  if [ "$VERSION" = "latest" ]; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
    [ -n "$VERSION" ] || die "最新リリースを取得できません"
  fi
  case "$VERSION" in v[0-9]*) ;; *) die "バージョンは v1.2.3 の形式で指定してください: $VERSION" ;; esac
  ASSET="sd-backup-manager-${VERSION}-x86_64-linux.tar.gz"
  BASE="https://github.com/$REPO/releases/download/$VERSION"
  info "$VERSION を取得しています"
  curl -fSL --proto '=https' --tlsv1.2 -o "$WORK/$ASSET" "$BASE/$ASSET"
  curl -fsSL --proto '=https' --tlsv1.2 -o "$WORK/SHA256SUMS" "$BASE/SHA256SUMS"
fi

info "SHA256 を照合しています"
( cd "$WORK" && grep " ${ASSET}\$" SHA256SUMS | sha256sum -c - ) || die "チェックサムが一致しません。インストールを中止しました"
mkdir -p "$WORK/x"
tar -xzf "$WORK/$ASSET" -C "$WORK/x" --no-same-owner
PKG="$(find "$WORK/x" -mindepth 1 -maxdepth 1 -type d | head -1)"
[ -x "$PKG/bin/sd-backup-daemon" ] || die "成果物の形式が不正です"

# --- 4. 停止・バックアップ -------------------------------------------------
if systemctl --user is-active --quiet sd-backup-daemon.service; then
  info "サービスを停止しています（処理中のコピーは安全に中断され、再起動後に再開します）"
  systemctl --user stop sd-backup-daemon.service
fi
pkill -x sd-backup-gui 2>/dev/null || true

mkdir -p "$BIN_DIR" "$DATA_DIR" "$CONFIG_DIR" "$UNIT_DIR" "$AUTOSTART_DIR" "$APPS_DIR" "$ICON_DIR"
chmod 700 "$DATA_DIR" "$CONFIG_DIR"
if [ -f "$DATA_DIR/state.sqlite" ]; then
  STAMP="$(date +%Y%m%d%H%M%S)"
  mkdir -p "$DATA_DIR/backups"
  for f in state.sqlite state.sqlite-wal state.sqlite-shm; do
    [ -f "$DATA_DIR/$f" ] && cp -p "$DATA_DIR/$f" "$DATA_DIR/backups/$f.$STAMP"
  done
  info "データベースをバックアップしました: $DATA_DIR/backups/state.sqlite.$STAMP"
fi

# --- 5. 配置 ---------------------------------------------------------------
for b in sd-backup-daemon sd-backup-gui sd-backup; do
  install -m 0755 "$PKG/bin/$b" "$BIN_DIR/$b.new"
  mv -f "$BIN_DIR/$b.new" "$BIN_DIR/$b"
done
install -m 0644 "$PKG/config/formats.toml" "$DATA_DIR/formats.default.toml"
[ -f "$CONFIG_DIR/formats.toml" ] || install -m 0600 "$PKG/config/formats.toml" "$CONFIG_DIR/formats.toml"
install -m 0644 "$PKG/packaging/sd-backup-daemon.service" "$UNIT_DIR/sd-backup-daemon.service"
sed "s|@BINDIR@|$BIN_DIR|g" "$PKG/packaging/sd-backup-gui.desktop" > "$APPS_DIR/sd-backup-gui.desktop"
cp "$APPS_DIR/sd-backup-gui.desktop" "$AUTOSTART_DIR/sd-backup-gui.desktop"
install -m 0644 "$PKG/assets/icons/sd-backup-manager.svg" "$ICON_DIR/sd-backup-manager.svg"
install -m 0755 "$PKG/scripts/uninstall.sh" "$DATA_DIR/uninstall.sh"
install -m 0755 "$PKG/scripts/upgrade.sh" "$DATA_DIR/upgrade.sh"
echo "${VERSION}" > "$DATA_DIR/installed-version"

# --- 6. サービス登録・起動 -------------------------------------------------
systemctl --user daemon-reload
systemctl --user enable --now sd-backup-daemon.service

# --- 7. 確認 ---------------------------------------------------------------
sleep 2
if systemctl --user is-active --quiet sd-backup-daemon.service; then
  info "サービスが起動しました"
else
  warn "サービスが起動していません: journalctl --user -u sd-backup-daemon を確認してください"
fi
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) warn "$BIN_DIR が PATH に含まれていません（再ログイン後に有効になります）" ;; esac
"$BIN_DIR/sd-backup" doctor || true
if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
  nohup "$BIN_DIR/sd-backup-gui" >/dev/null 2>&1 &
fi
info "インストール完了（${VERSION}）。GUI の［設定］で NAS を設定し、接続テスト→保存してください"
