## v0.4.2 の変更

- 設定画面に［PC 内データをまとめて削除］を追加しました。一時コピー（未送信分を含む）、全転送キュー、送信履歴・キャッシュ、メディア識別、過去の診断ログを削除できます。
- 削除対象の件数・容量を表示し、「削除」と入力して確定します。SD・USB メディアをすべて取り外してから実行してください。
- NAS の接続設定・保存済みパスワード、元の SD、NAS 上のファイルは保持します。途中失敗や再起動後も削除を続行し、完了まで通常の転送は再開しません。
- 完了後は一時停止します。メイン画面の［再開］を押し、SD を挿し直して取り込みを再開してください。

## v0.4.1 で修正した問題

- 「NAS 未設定（SSD に保持中）」で転送待ちのファイルがあると、最初の NAS 保存先を設定できない問題を修正しました。
- PC 内のコピー・転送キュー・履歴は保持されます。更新後に［設定］で NAS 接続を選び、接続先と認証情報を入力して［接続テスト］→［保存］してください。
- 保存先が未初期化の場合は［保存先を初期化］を押してください。PC 内に保持していたファイルも転送します。

## v0.4.0 で追加した機能

- NAS 転送のファイル一覧で複数選択し、選択分だけ再試行・転送中止・PC 内コピーと転送キューの削除ができます。
- 削除前に対象・件数・容量を確認します。元の SD と NAS 上の既存ファイルは変更しません。
- 削除途中の終了から復旧し、中止したファイルは明示的に再試行するまで保持します。
- NAS 試行回数・エラー・次回再試行時刻、ページ切り替え・自動更新を追加しました。

更新は以下のインストーラーを実行してください（アプリ内での自動更新はありません）。
更新インストールでは設定・履歴を保持します（一括削除を実行した場合のみ対象データを削除します）。
履歴 DB は v4 に更新されるため、更新後は v0.4.1 以前のアプリでは開けません。

## インストール（Windows 10 / 11 / x64）

`sd-backup-manager-setup-x64.exe` をダウンロードして実行します（管理者権限・開発環境は不要）。
署名のないアプリのため、SmartScreen が表示されたら「詳細情報」→「実行」を選んでください。
インストール後に開く画面の［設定］で NAS を登録し、スタートメニューの「SD Backup Manager 診断」で確認します。
自動再生（AutoPlay）はオフにしておくことをおすすめします。
詳しくは [docs/windows.md](https://github.com/sangikousenuser/sd-backup-manager/blob/main/docs/windows.md)。

インストーラを使わない場合は `sd-backup-manager-v*-x86_64-windows.zip` を展開して使えます。

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

- このバージョンは Windows / Ubuntu 26.04 の実機・実際の SD カード・SMB NAS での受け入れ試験がまだです。
  本番の前に [docs/acceptance.md](https://github.com/sangikousenuser/sd-backup-manager/blob/main/docs/acceptance.md) の実機試験を行ってください。
- 各ファイルのチェックサムは `SHA256SUMS` にあります。
