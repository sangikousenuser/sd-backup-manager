#!/usr/bin/env bash
# リリース成果物を作る（CI から呼ぶ）: dist/sd-backup-manager-<tag>-x86_64-linux.tar.gz と SHA256SUMS
#   scripts/package.sh v0.1.0 target/release
set -euo pipefail
TAG="$1"
BIN="${2:-target/release}"
NAME="sd-backup-manager-${TAG}-x86_64-linux"
rm -rf dist && mkdir -p "dist/$NAME/bin"
for b in sd-backup-daemon sd-backup-gui sd-backup; do
  install -m 0755 "$BIN/$b" "dist/$NAME/bin/$b"
done
mkdir -p "dist/$NAME/config" "dist/$NAME/packaging" "dist/$NAME/scripts" "dist/$NAME/assets/icons"
cp config/formats.toml "dist/$NAME/config/"
cp packaging/sd-backup-daemon.service packaging/sd-backup-gui.desktop packaging/install-manifest.toml "dist/$NAME/packaging/"
cp scripts/install.sh scripts/uninstall.sh scripts/upgrade.sh "dist/$NAME/scripts/"
cp assets/icons/sd-backup-manager.svg "dist/$NAME/assets/icons/"
cp README.md LICENSE CHANGELOG.md Cargo.lock "dist/$NAME/"
tar -C dist -czf "dist/$NAME.tar.gz" --owner=0 --group=0 "$NAME"
cp scripts/install.sh dist/install.sh
( cd dist && sha256sum "$NAME.tar.gz" install.sh > SHA256SUMS && cat SHA256SUMS )
