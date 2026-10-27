# Changelog

## v0.2.0 — Windows 対応

- Windows 10/11（x64）に対応。インストーラ（`sd-backup-manager-setup-x64.exe`、管理者権限不要）とポータブル zip を配布
  - メディア検出: ドライブレターを走査し、USB / SD 接続のボリュームを物理ディスク単位でまとめる
  - 取り外し: ボリュームのロック → ディスマウント（使用中なら「取り外し不可」）。抜かれるまでロックを保持
  - 常駐: タスク スケジューラ（ログオン時起動・異常終了時は再起動）、GUI はスタートアップに登録
  - NAS: `smb://` を UNC パスに変換し `WNetAddConnection2W` で接続。パスワードは資格情報マネージャー
  - IPC: 名前付きパイプ、日本語フォント（メイリオ等）、通知音（PlaySound）
- `sd-backup shutdown`（デーモンの正常停止）を追加
- `sd-backup doctor`: PipeWire の通知音（pw-play）と .deb 版の自動起動（/etc/xdg/autostart）を正しく判定
- 依存クレート ctutils を 0.4.6 に更新（RUSTSEC-2026-0385）
- CI で Windows の clippy・テスト・インストーラ作成を実行
- 引き継ぎ資料 `docs/HANDOFF.md`・`CLAUDE.md`、Windows 版の説明 `docs/windows.md` を追加

## v0.1.0 — 初回リリース

- メディア自動検出（UDisks2）・読み込み専用マウント・全パーティションを 1 セッションに集約
- Photos / Videos / Other への分類（拡張子定義 + 収録構造 + 同名グループ）、未知形式は Other に保存
- BLAKE3 による SSD 取り込み検証（.part → fsync → 読み戻し → rename）、合計 3 試行
- 安全なアンマウント後のみ「取り外し可能」（緑 / 黄）、通知音
- SSD 最低空き 20GB の容量アドミッションと容量待ち
- NAS（GVfs SMB / CIFS マウントパス）への転送・読み戻し検証・指数バックオフ再試行
- manifest.json と「元の構成で復元」
- 中断・再挿入・再起動からの復旧、永続カタログによる検証済み差分取り込み
- Iced の日本語 GUI、CLI（status / doctor / restore / export-logs）
- systemd ユーザーサービス、.deb パッケージ（apt で依存も自動導入）、1 行インストーラ（SHA256 照合）、GitHub Actions
