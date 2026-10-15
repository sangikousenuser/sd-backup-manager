#!/usr/bin/env bash
# .deb パッケージを作る（CI から呼ぶ）: dist/sd-backup-manager_<version>_<arch>.deb
#   scripts/package-deb.sh v0.1.0 target/release
#
# システム全体にインストールする形式:
#   /usr/bin/                         sd-backup-daemon, sd-backup-gui, sd-backup
#   /usr/lib/systemd/user/            sd-backup-daemon.service（全ユーザーで有効化）
#   /etc/xdg/autostart/               GUI の自動起動
# データ・設定・SSD 一時領域は従来どおり各ユーザーのホーム配下（~/.config, ~/.local/share, ~/SDBackupStaging）。
set -euo pipefail
TAG="$1"
BIN="${2:-target/release}"
VERSION="${TAG#v}"
case "$VERSION" in [0-9]*) ;; *) VERSION="0.0.0~${VERSION//-/+}" ;; esac
ARCH="$(dpkg --print-architecture)"
PKG="sd-backup-manager"
ROOT="dist/deb/${PKG}_${VERSION}_${ARCH}"

rm -rf "$ROOT" && mkdir -p "$ROOT/DEBIAN"
install -d "$ROOT/usr/bin" "$ROOT/usr/lib/systemd/user" "$ROOT/etc/xdg/autostart" \
  "$ROOT/usr/share/applications" "$ROOT/usr/share/icons/hicolor/scalable/apps" \
  "$ROOT/usr/share/$PKG" "$ROOT/usr/share/doc/$PKG"
for b in sd-backup-daemon sd-backup-gui sd-backup; do
  install -m 0755 "$BIN/$b" "$ROOT/usr/bin/$b"
done
sed 's|^ExecStart=.*|ExecStart=/usr/bin/sd-backup-daemon|' packaging/sd-backup-daemon.service \
  > "$ROOT/usr/lib/systemd/user/sd-backup-daemon.service"
sed 's|@BINDIR@|/usr/bin|g' packaging/sd-backup-gui.desktop > "$ROOT/etc/xdg/autostart/sd-backup-gui.desktop"
sed 's|@BINDIR@|/usr/bin|g' packaging/sd-backup-gui.desktop > "$ROOT/usr/share/applications/sd-backup-gui.desktop"
install -m 0644 assets/icons/sd-backup-manager.svg "$ROOT/usr/share/icons/hicolor/scalable/apps/"
install -m 0644 config/formats.toml "$ROOT/usr/share/$PKG/formats.toml"
install -m 0644 README.md CHANGELOG.md "$ROOT/usr/share/doc/$PKG/"
install -m 0644 LICENSE "$ROOT/usr/share/doc/$PKG/copyright"
chmod 0644 "$ROOT"/usr/lib/systemd/user/* "$ROOT"/etc/xdg/autostart/* "$ROOT"/usr/share/applications/*

SIZE_KB="$(du -sk "$ROOT" | cut -f1)"
cat > "$ROOT/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Installed-Size: $SIZE_KB
Maintainer: sangikousenuser <168276910+sangikousenuser@users.noreply.github.com>
Homepage: https://github.com/sangikousenuser/sd-backup-manager
Depends: libc6, udisks2, libglib2.0-bin, gvfs-backends, gvfs-fuse, gnome-keyring, fontconfig, fonts-noto-cjk,
 pulseaudio-utils | pipewire-bin, libxkbcommon0, libwayland-client0, libx11-6, libxcursor1,
 libxi6, libxrandr2, libvulkan1, mesa-vulkan-drivers
Description: Safe ingest of camera media to SSD and NAS (SD Backup Manager)
 Detects SD cards and USB drives, copies them read-only to the internal SSD with
 BLAKE3 verification, shows when the media can be removed, then transfers the
 files to an SMB NAS with read-back verification. Japanese GUI (Iced).
EOF

cat > "$ROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
  # 全ユーザーのログイン時に自動起動
  systemctl --global enable sd-backup-daemon.service >/dev/null 2>&1 || true
  # ログイン中のユーザーにはすぐ反映（失敗しても次回ログインで起動する）
  if command -v loginctl >/dev/null 2>&1; then
    for user in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
      [ "$user" = "root" ] && continue
      systemctl --user -M "$user@" daemon-reload >/dev/null 2>&1 || true
      systemctl --user -M "$user@" restart sd-backup-daemon.service >/dev/null 2>&1 || true
    done
  fi
  echo "SD Backup Manager をインストールしました。アプリ一覧の「SD バックアップ」から起動し、［設定］で NAS を登録してください。"
fi
exit 0
EOF

cat > "$ROOT/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] || [ "$1" = "upgrade" ]; then
  # 処理中のコピーは SIGTERM で安全に中断され、次回起動時に再開する
  if command -v loginctl >/dev/null 2>&1; then
    for user in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
      systemctl --user -M "$user@" stop sd-backup-daemon.service >/dev/null 2>&1 || true
    done
  fi
fi
if [ "$1" = "remove" ]; then
  systemctl --global disable sd-backup-daemon.service >/dev/null 2>&1 || true
fi
exit 0
EOF
chmod 0755 "$ROOT/DEBIAN/postinst" "$ROOT/DEBIAN/prerm"

dpkg-deb --root-owner-group --build "$ROOT" "dist/${PKG}_${VERSION}_${ARCH}.deb"
rm -rf dist/deb
# バージョンなしの名前でも置く（releases/latest/download/ から常に最新を取得できるように）
cp "dist/${PKG}_${VERSION}_${ARCH}.deb" "dist/${PKG}_${ARCH}.deb"
( cd dist && sha256sum "${PKG}_${VERSION}_${ARCH}.deb" "${PKG}_${ARCH}.deb" >> SHA256SUMS && cat SHA256SUMS )
