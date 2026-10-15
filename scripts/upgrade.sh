#!/usr/bin/env bash
# 指定バージョン（既定: 最新）へ更新する。設定・DB は保持され、更新前に DB をバックアップする。
#   upgrade.sh v0.2.0
set -euo pipefail
VERSION="${1:-latest}"
REPO="${SDBM_REPO:-sangikousenuser/sd-backup-manager}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
if [ "$VERSION" = "latest" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
fi
case "$VERSION" in v[0-9]*) ;; *) echo "バージョンが不正です: $VERSION" >&2; exit 2 ;; esac
curl -fsSL --proto '=https' -o "$WORK/install.sh" "https://github.com/$REPO/releases/download/$VERSION/install.sh"
curl -fsSL --proto '=https' -o "$WORK/SHA256SUMS" "https://github.com/$REPO/releases/download/$VERSION/SHA256SUMS"
( cd "$WORK" && grep ' install.sh$' SHA256SUMS | sha256sum -c - )
bash "$WORK/install.sh" --version "$VERSION" "${@:2}"
