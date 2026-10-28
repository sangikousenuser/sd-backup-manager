# Changelog

## 未リリース — SD 全体の保存・カード別フォルダ

- 新規バックアップを `sd1/20261028112834/` 形式に変更。元のフォルダ構成・空フォルダ・全形式を毎回実体として保存
- SD/MMC 接続のシリアルが取得できる場合はフォーマット後もカード番号を再利用。USB リーダーのシリアルでは推測せず新番号を自動登録
- Windows の PC 内保存先をメイン画面・設定・詳細に表示し、ファイルマネージャーで開くボタンを追加
- Windows の復元先・ログ書き出し先の初期値を USERPROFILE から作成するよう修正
- DB スキーマを v2 に移行。旧セッション・カタログ・格納パスは維持（更新後の DB は旧アプリでは開けない）
- macOS 上のシミュレータで全件コピー・再起動復旧・フォーマット後識別・空フォルダ復元を検証。実機 SD・Windows 実機は未検証

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
