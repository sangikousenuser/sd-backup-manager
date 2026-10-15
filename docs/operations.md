# 運用手順

## 当日の操作

1. PC にログインすると GUI が自動で開きます（デーモンは常駐）。
2. カードを挿します。左右の「メディア 1 / 2」にそれぞれの進捗が出ます。
3. **緑の「✔ 取り外し可能」** が出たら抜いてかまいません。
   - **黄の「▲ 取り外し可能（失敗あり）」**: 一部のファイルが 3 回試しても読めませんでした。
     ［詳細を表示］で失敗ファイルを確認し、カードは保管してください。同じカードを挿し直すと
     同じセッションで失敗分を再試行します。
   - **赤の「✖ まだ取り外さないでください」**: アンマウントに失敗しました（ファイルを開いている
     アプリがある等）。そのアプリを閉じて［アンマウント再試行］を押してください。
4. NAS 転送は下のカードに表示されます。カードを抜いた後も続きます。

| 表示 | 意味・対応 |
|---|---|
| 容量確保待ち | SSD の空きが 20GB + 必要量に足りません。NAS 転送が終わって SSD が空けば自動で始まります |
| 処理中断／同じメディアを再接続 | コピー完了前に抜かれました。同じカードを挿すと続きから再開します |
| NAS 転送待機／SSD 保持中 | NAS に接続できません。SSD にデータを保持したまま自動で再試行しています |
| ▲ 保存先が未初期化 | 設定画面で［保存先を初期化］を押してください |

## コマンド

```bash
sd-backup status            # 状態
sd-backup doctor            # 依存・サービス・容量・NAS 設定の診断
sd-backup sessions          # セッション一覧
sd-backup files <SESSION>   # 問題のあるファイル（--all で全件）
sd-backup retry-nas         # NAS 転送を今すぐ再試行
sd-backup export-logs ~/Desktop
sd-backup restore --session <SESSION> --to ~/Restore     # 元のカード構成で復元
sd-backup restore --manifest /path/to/<folder>/manifest.json --nas-root /path/to/share --to ~/Restore
journalctl --user -u sd-backup-daemon -f                  # ログ
systemctl --user restart sd-backup-daemon                 # 再起動（処理中のコピーは安全に中断・再開）
```

## ファイルの場所

| パス | 内容 |
|---|---|
| `~/.config/sd-backup-manager/config.toml` | 設定（秘密値なし） |
| `~/.config/sd-backup-manager/formats.toml` | 分類用の拡張子定義（編集してデーモン再起動で反映） |
| `~/.local/share/sd-backup-manager/state.sqlite` | 状態 DB・永続カタログ |
| `~/.local/share/sd-backup-manager/logs/` | 日次ログ（30 日保持） |
| `~/SDBackupStaging/sessions/<UUID>/` | SSD 一時保存（NAS 検証後に自動削除） |
| NAS `<共有>/<YYYYMMDDHHMMSSNN>/` | `Photos/` `Videos/` `Other/` と `manifest.json` |

## 本番前の推奨事項

- NAS 側でスナップショット、または別媒体へのバックアップを設定する（NAS が唯一の長期保存先になるため）。
- 本番と同じカード・カードリーダー・NAS で [acceptance.md](acceptance.md) の実機試験を行う。
- `sd-backup doctor` がすべて ✔ になることを確認する。
