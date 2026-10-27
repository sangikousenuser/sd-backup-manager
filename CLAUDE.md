# SD Backup Manager — AI 向けの作業メモ

文化祭の撮影メディア（SD / USB）を内蔵 SSD 経由で NAS に安全に取り込む常駐アプリ（Rust / Iced）。
**Windows と Linux（Ubuntu 26.04）の両方**を同じコードで対応している。今後の開発は Windows 端末で行う。

最初に読むもの:
- [docs/HANDOFF.md](docs/HANDOFF.md) — 現状・構成・OS 差分の置き場所・未検証事項・次の作業
- [docs/SD_Backup_Manager_Requirements_v1.0.md](docs/SD_Backup_Manager_Requirements_v1.0.md) — 仕様（Ubuntu 前提）
- [docs/windows.md](docs/windows.md) — 仕様の Windows での読み替え

## コマンド

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo run -p sd-backup-daemon -- --base-dir <DIR> --simulate-media <DIR>\media --dev-nas-dir <DIR>\nas
cargo run -p sd-backup-gui            # $env:SDBM_BASE_DIR を daemon と同じ <DIR> にする
pwsh scripts/package-windows.ps1 -Tag v0.0.0-dev -BinDir target\release   # Setup.exe と zip
```

## ルール

- 安全ルールを弱めない: 元メディアに書き込まない／NAS で全件検証するまで SSD を消さない／
  全パーティションのアンマウント成功時だけ「取り外し可能」／未検証を成功扱いしない／履歴だけでスキップしない。
- OS 依存処理は `crates/backup-storage/src/platform.rs`、`crates/backup-ipc/src/transport.rs`、
  `crates/backup-device/src/{windows,udisks}.rs` など既存の置き場所に入れる。上位に `#[cfg]` を散らさない。
- Linux・macOS・Windows の CI（`.github/workflows/ci.yml`）がすべて通る状態を保つ。
- 実機で確認していないことを「確認済み」と書かない。確認したら `docs/acceptance.md` と `docs/HANDOFF.md` を更新する。
- コミット作者は noreply アドレス（`168276910+sangikousenuser@users.noreply.github.com`）。個人メールを使わない。
- ドキュメント・UI 文言は日本語。
- リリースは `Cargo.toml` の version を上げてから `vX.Y.Z` タグを push（タグと version が一致しないと失敗する）。
