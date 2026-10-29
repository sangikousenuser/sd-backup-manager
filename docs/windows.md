# Windows 版

SD Backup Manager は Windows 10 / 11（x64）でも動作します。仕様書（Ubuntu 前提）の各要素は
次のように Windows の仕組みに置き換えています。安全ルール（元メディアを変更しない・NAS 検証前に
SSD を消さない・アンマウント成功時だけ「取り外し可能」など）は Linux 版と同じコードで守られます。

| 仕様書（Ubuntu） | Windows 版 | 実装 |
|---|---|---|
| udev / UDisks2 で検出 | ドライブレターを 1 秒ごとに走査し、USB / SD(MMC) 接続またはリムーバブルのボリュームを物理ディスク単位でまとめる | `crates/backup-device/src/windows.rs` |
| UDisks2 で読み込み専用マウント | Windows の自動マウントをそのまま使い、アプリは読み込み専用でしか開かない（リパースポイントは辿らない） | `crates/backup-storage/src/platform.rs` |
| UDisks2 でアンマウント | 各ボリュームを `FSCTL_LOCK_VOLUME` → `FSCTL_DISMOUNT_VOLUME`。使用中ならロックできず失敗＝「取り外し不可」。ロックは抜かれるまで保持 | `windows.rs` の `lock_and_dismount` |
| 抜去の検出 | 物理ドライブが消える、または `IOCTL_STORAGE_CHECK_VERIFY2` でメディアなし | `windows.rs` の `media_present` |
| systemd --user | タスク スケジューラ（ログオン時に起動、異常終了時は 1 分後に再起動） | `packaging/windows/register-task.ps1` |
| XDG autostart | `HKCU\...\Run` の `SDBackupManagerGUI` | `packaging/windows/installer.iss` |
| Secret Service | Windows 資格情報マネージャー（keyring クレート） | `crates/backup-config/src/secrets.rs` |
| GVfs で SMB | `smb://server/share/sub` を `\\server\share\sub` に変換し、`WNetAddConnection2W` で接続（ドライブレターなし） | `apps/daemon/src/nas_backend.rs` |
| CIFS マウントパス | 割り当て済みドライブ（`Z:\...`）または UNC パス。OS ドライブ・SSD と別ボリュームであることを確認 | 同上 |
| Unix ソケット（0600 + peer UID） | 名前付きパイプ `\\.\pipe\sd-backup-manager-<id>`（リモート拒否・同名パイプの先取り検出） | `crates/backup-ipc/src/transport.rs` |
| `~/.config`・`~/.local/share` | `%APPDATA%\sd-backup-manager`（設定）・`%LOCALAPPDATA%\sd-backup-manager`（DB・ログ） | `crates/backup-config/src/lib.rs` |
| SSD 一時保存 `~/SDBackupStaging` | `%USERPROFILE%\SDBackupStaging` | 同上 |
| paplay | `PlaySoundW` | `apps/gui/src/sound.rs` |
| Noto Sans CJK | メイリオ → 游ゴシック → MS ゴシック | `apps/gui/src/fonts.rs` |

## インストール

1. [Releases](https://github.com/sangikousenuser/sd-backup-manager/releases/latest) から
   `sd-backup-manager-setup-x64.exe` をダウンロードして実行します（管理者権限は不要）。
   - 署名のないアプリのため、SmartScreen が表示されたら「詳細情報」→「実行」を選びます。
2. インストールが終わると GUI が開きます。［設定］で NAS（`smb://サーバー/共有名/フォルダ/`、
   ユーザー名、パスワード）を入力し、［接続テスト］→［保存］します。
3. スタートメニューの「SD Backup Manager 診断」で、すべて ✔ になることを確認します。

インストール先は `%LOCALAPPDATA%\Programs\SD Backup Manager`。次回ログオンからはデーモン
（タスク スケジューラ）と GUI（スタートアップ）が自動で起動します。

アンインストールは「設定 → アプリ」から。設定・履歴・SSD の一時保存は削除しません。

## 運用上の注意（Windows 固有）

### PC 内の保存場所

エクスプローラーのアドレス欄に **`%USERPROFILE%\SDBackupStaging`** と入力して開けます。
通常は `C:\Users\ユーザー名\SDBackupStaging\sessions\sd1\20261028112834\` のように保存されます。
アプリのメイン画面・設定の［PC 内の保存先を開く］からも開けます。設定で保存先を変更済みの場合は画面の実際のパスを確認してください。
NAS が未設定・オフラインの間は PC 内に保持し、NAS 側の全件検証完了後に一時データを自動削除します。

「NAS 未設定（SSD に保持中）」は、まだ NAS 保存先を登録していない状態です。
［設定］で「SMB 共有」を選び、`smb://サーバー/共有名/フォルダ/` と認証情報を入力し、
［接続テスト］→［保存］します。未初期化と表示された場合は［保存先を初期化］を押します。
メイン画面の［今すぐ再試行］で転送を促せます。一時停止中なら［再開］を押してください。
v0.4.0 以前では転送待ちがあると初回の設定も保存できないため、v0.4.1 以降へ更新してください。
PC 内のデータや転送キューを削除する必要はありません。

旧形式のバックアップは `sessions\セッションUUID\` にあります。

新規バックアップは写真・動画に振り分けず、空フォルダを含めて元の構成で毎回全ファイルをコピーします。
NAS 側も `共有\sd1\日時\DCIM\...` の構成です。複数パーティションの場合のみ日時の下を `P01`・`P02` に分けます。
日時は日本時間の14桁で、同じ秒に重なったときだけ末尾に連番を追加します。
`manifest.json` は検証・復元用の管理ファイルです。同名ファイルや Windows で使えない名前は退避名にして対応を記録します。

### フォーマット後の識別

SD/MMC 接続でシリアルが取得できる場合は、フォーマット後も同じ `sd1` にまとめます。
USB カードリーダーのシリアルはリーダーのものかもしれないため使いません。
カード固有の番号を取得できず、フォーマットで FS UUID が変わった場合は、新しい `sd2` 等を自動割り当てします。
手動選択で処理を止めません。保存先をまとめられる場合も、フォーマット前の中断セッションとは別セッションにします。

### PC 内データ・履歴の一括削除（v0.4.2 以降）

1. SD・USB メディアをすべて取り外し、アプリの取り込み処理が終了するまで待ちます。
2. ［設定］の［PC 内データをまとめて削除］で［削除対象を確認…］を押します。
3. 件数・容量を確認して「削除」と入力し、［PC 内データと履歴をすべて削除する］を押します。

未送信分を含む一時コピー、全転送キュー、送信履歴・カタログ・メディア識別、過去の診断ログを消去します。
NAS 接続設定・保存済みパスワード・通知設定、元の SD と NAS 上のファイルは保持します。
アプリが管理するセッションだけが対象で、保存先の無関係なファイルや別途書き出した復元・診断ファイルは削除しません。
削除中に終了した場合は次回起動で続行し、使用中ファイルなどで失敗した場合も通常の転送は停止したままにします。
原因を解消し、設定から再実行してください。削除完了後は一時停止するため、［再開］を押して SD を挿し直します。
NAS の既存フォルダとの衝突を避けるため、カード番号の通し番号は保持し、識別をやり直した SD には新しい番号を割り当てます。
診断ログは過去の内容を空にし、その後の新しい動作ログは引き続き記録します。

### 取り外し・常駐

- **自動再生（AutoPlay）をオフに**: カードを挿したときにエクスプローラーや写真アプリが開くと、
  ファイルを開いたままになり「使用中」で取り外し可能にならないことがあります。
  「設定 → Bluetooth とデバイス → 自動再生」でオフにしてください。
- **取り外し可能の表示後**: ボリュームはロックされた状態になります（他のアプリから見えなくなります）。
  そのまま抜いてください。タスクトレイの「安全に取り外す」は不要です。
- **USB の外付け HDD/SSD**: ロックとディスマウントでデータは確定しますが、ドライブ自体の
  電源停止（「安全に取り外す」相当）は行いません。抜く前にアクセスランプが消えていることを確認してください。
- **ログオフ・シャットダウン**: 常駐プロセスは Windows に強制終了されることがありますが、
  次回起動時の復旧処理で中断したコピーから安全にやり直します（未検証ファイルを成功扱いしません）。
- **コマンド**: `"%LOCALAPPDATA%\Programs\SD Backup Manager\sd-backup.exe" status` などで状態を確認できます
  （`doctor`・`sessions`・`retry-nas`・`export-logs`・`restore`・`shutdown`）。

## 開発

[docs/HANDOFF.md](HANDOFF.md) の「Windows での開発」を参照してください。
