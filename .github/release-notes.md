## インストール（Ubuntu 26.04 / x86_64）

Rust などの開発環境は不要です。次のどちらかを選んでください（両方は入れないでください）。

### A. .deb パッケージ（おすすめ）

依存パッケージ（udisks2・gvfs・日本語フォントなど）も自動で入ります。

```bash
wget https://github.com/sangikousenuser/sd-backup-manager/releases/latest/download/sd-backup-manager_amd64.deb
sudo apt install ./sd-backup-manager_amd64.deb
```

アプリ一覧の「SD バックアップ」を開き、［設定］で NAS を登録します。次回ログインからは自動で起動します。
更新は新しい .deb を同じ手順で入れるだけです（設定・履歴は保持されます）。削除は `sudo apt remove sd-backup-manager`
（SSD 上の未転送データ・設定・履歴は削除されません）。

### B. 1 行インストール（ホームフォルダに入れる）

```bash
curl -fsSL https://github.com/sangikousenuser/sd-backup-manager/releases/latest/download/install.sh | bash
```

SHA256 を照合してから `~/.local/bin` に入れ、不足している依存パッケージは確認してから `sudo apt` で入れます。

## 注意

- このバージョンは Ubuntu 26.04 実機・実際の SD カード・SMB NAS での受け入れ試験がまだです。
  本番の前に [docs/acceptance.md](https://github.com/sangikousenuser/sd-backup-manager/blob/main/docs/acceptance.md) の実機試験を行ってください。
- 各ファイルのチェックサムは `SHA256SUMS` にあります。
