# Changelog

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
- systemd ユーザーサービス、インストーラ（SHA256 照合）、GitHub Actions
