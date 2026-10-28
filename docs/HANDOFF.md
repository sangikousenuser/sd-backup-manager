# 引き継ぎ資料（Windows で開発を続ける人・AI 向け）

最終更新: 2026-10-28。このファイルと [CLAUDE.md](../CLAUDE.md) を読めば、Windows 端末上で
開発を再開できるようにまとめています。仕様の正本は
[SD_Backup_Manager_Requirements_v1.0.md](SD_Backup_Manager_Requirements_v1.0.md)（Ubuntu 前提で書かれている。
Windows での読み替えは [windows.md](windows.md)）。

---

## 1. 現在の状態

v0.3.0（2026-10-28）: 新規セッションは SSD/NAS とも `sdN/14桁日時/` に元の全構成を保存し、
毎回全ファイルをコピーする。複数パーティションは `P01/` 等で区別し、空フォルダも DB/manifest に記録する。
カード番号の永続化は `migrations/0002_cards.sql`。旧セッションは旧パスと分類方式を維持して再開する。
SD/MMC シリアルが取得できる場合のみフォーマット後の番号を再利用し、USB リーダー等では新番号になる。
GUI に PC 内の保存先表示・開くボタンを追加。シミュレータで検証済み、Windows 実機・実機 SD は未検証。

| 項目 | 状態 |
|---|---|
| Linux（Ubuntu 26.04）版 | v0.1.0 をリリース済み（.deb / tar.gz / install.sh）。実機にインストールし、デーモン起動と `doctor` まで確認済み。SD カード・NAS での受け入れ試験は未実施 |
| Windows 版 | v0.3.0 でリリース（Setup.exe・zip）。GitHub Actions の windows-latest で clippy・単体テスト・E2E（シミュレータ）・Setup.exe 作成まで成功。CI の成果物 `sd-backup-manager-windows-x64` からも Setup.exe を取得できる。**Windows 実機での動作確認は未実施** |
| macOS | 開発用。シミュレータでのみ動作（実機メディアは扱わない） |
| CI | `.github/workflows/ci.yml`: fmt / clippy + test（Linux・macOS・Windows）/ cargo audit / Ubuntu 26.04 コンテナでのリリースビルド / Windows リリースビルド（zip + Setup.exe） |
| リリース | `v*` タグの push で `.github/workflows/release.yml` が Linux（.deb, tar.gz）と Windows（Setup.exe, zip）を GitHub Releases に公開 |

リポジトリ: https://github.com/sangikousenuser/sd-backup-manager（public）

---

## 2. 構成

```
crates/backup-core     ドメイン（I/O なし）: 状態遷移・分類・識別・パス安全化・マニフェスト・容量/削除ポリシー
crates/backup-db       SQLite（WAL, synchronous=FULL）・マイグレーション（migrations/*.sql）
crates/backup-storage  BLAKE3・原子的コピー・容量・ステージング・NAS 転送・復元
                       └ platform.rs  ← ファイル操作の OS 差分はここに集約
crates/backup-device   メディア検出・アンマウント
                       ├ sim.rs       シミュレータ（全 OS、開発・E2E 用）
                       ├ udisks.rs    Linux（UDisks2 / D-Bus）
                       └ windows.rs   Windows（Win32 IOCTL）
crates/backup-ipc      JSON Lines プロトコル（protocol.rs）
                       └ transport.rs ← Unix ソケット / Windows 名前付きパイプ
crates/backup-config   設定 TOML・パス（XDG / %APPDATA%）・資格情報（keyring）
apps/daemon            常駐サービス: orchestrator.rs（メディア処理）, nas_worker.rs（NAS 転送・完了処理）,
                       nas_backend.rs（gio / WNetAddConnection2W）, recovery.rs, ipc_server.rs, snapshot.rs
                       tests/e2e.rs  シミュレータ + ローカル NAS ディレクトリでのエンドツーエンド試験
apps/gui               Iced 0.14 の日本語 GUI（状態は持たず、デーモンのスナップショットを表示）
apps/cli               sd-backup（status / doctor / sessions / files / restore / export-logs / shutdown ...）
packaging/             systemd ユニット・.desktop（Linux）、windows/（Inno Setup・タスク登録スクリプト）
scripts/               install.sh / package*.sh（Linux）、package-windows.ps1（Windows）
docs/                  architecture / security / operations / acceptance / windows / この資料
```

データの流れ（詳細は [architecture.md](architecture.md)）:
メディア検出 → 読み込み専用で走査・分類 → 容量アドミッション（SSD 最低 20GB）→ `.part` に BLAKE3 を
計算しながらコピー → fsync → 読み戻し照合 → rename → 全パーティションをアンマウント → 緑/黄の「取り外し可能」
→ NAS に一時名で転送 → 読み戻し照合 → rename → manifest.json → 最終再検証 → SSD のセッションだけ削除。

---

## 3. OS 差分の置き場所

新しい OS 依存処理を足すときは、まずここに置けないか考える。上位コードに `#[cfg]` を散らさない。

| 何 | どこ | Windows の実装 |
|---|---|---|
| リンクを辿らない読み込み専用オープン、mtime、本人専用ファイル/ディレクトリ、dir fsync、ボリューム ID | `backup-storage/src/platform.rs` | `FILE_FLAG_OPEN_REPARSE_POINT` + リパースポイント拒否、dir fsync は no-op、`GetVolumePathNameW` |
| 空き容量 | `backup-storage/src/capacity.rs` | `GetDiskFreeSpaceExW`（UNC 可） |
| NAS の I/O エラー分類 | `backup-storage/src/nas.rs` `classify_os_error` | Win32 エラーコード表 |
| ステージング削除時の境界チェック | `backup-storage/src/local.rs` `crosses_boundary` | リパースポイント（ジャンクション等）があれば削除拒否 |
| IPC | `backup-ipc/src/transport.rs` | 名前付きパイプ（tokio） |
| 保存場所・パイプ名 | `backup-config/src/lib.rs` `AppPaths` | `%APPDATA%`, `%LOCALAPPDATA%`、`\\.\pipe\sd-backup-manager-<runtime_dir の FNV-1a>` |
| メディア検出・アンマウント | `backup-device/src/windows.rs` | 3 章の表を参照 |
| SMB 接続 | `apps/daemon/src/nas_backend.rs` `smb_root` / `wnet_connect` | `WNetAddConnection2W`（パスワードは資格情報マネージャー） |
| 停止シグナル | `apps/daemon/src/main.rs` `wait_for_stop_signal` | Ctrl+C/Break/Close/Logoff/Shutdown（コンソールなしでは届かないことがある → 起動時復旧で担保） |
| フォント・通知音 | `apps/gui/src/fonts.rs`, `sound.rs` | メイリオ等、`PlaySoundW` |
| 診断 | `apps/cli/src/main.rs` `env_checks` / `autostart_checks` | タスク登録・Run キー・フォント・資格情報マネージャー |
| 自動起動・インストール | `packaging/windows/` | Inno Setup（ユーザー単位、管理者不要）+ タスク スケジューラ |

Windows のデバイス処理（`backup-device/src/windows.rs`）:

| 処理 | API |
|---|---|
| 候補列挙 | `GetLogicalDrives` → `GetDriveTypeW`（REMOVABLE/FIXED のみ）→ `GetVolumeInformationW`（ラベル・シリアル・FS） |
| ディスク単位にまとめる | `\\.\X:` に `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`（DiskNumber）、`IOCTL_DISK_GET_PARTITION_INFO_EX`（番号・GPT GUID） |
| 外部メディア判定・識別 | `\\.\PhysicalDriveN` に `IOCTL_STORAGE_QUERY_PROPERTY`（Vendor/Product/Serial/BusType/Removable）。USB・SD・MMC 以外かつ非リムーバブルは内蔵とみなし無視 |
| 除外 | `%SystemDrive%`、ステージング・データ・ホームのボリューム |
| アンマウント | `GENERIC_READ|WRITE` で `\\.\X:` を開き `FlushFileBuffers` → `FSCTL_LOCK_VOLUME`（10 回再試行）→ `FSCTL_DISMOUNT_VOLUME`。ハンドルは抜かれるまで保持（`held`） |
| 抜去 | `held` 中のディスクは `IOCTL_STORAGE_CHECK_VERIFY2` が失敗するか PhysicalDrive が開けなくなるまで「接続中」 |

---

## 4. Windows での開発環境

1. **Git** と **GitHub CLI**（`winget install Git.Git GitHub.cli`）→ `gh auth login`
2. **Rust**（`winget install Rustlang.Rustup`）→ `rustup default stable-x86_64-pc-windows-msvc`
3. **Visual Studio 2022 Build Tools**（「C++ によるデスクトップ開発」ワークロード）
   — SQLite（rusqlite bundled）と BLAKE3 の C 部分のビルドに必要
   `winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`
4. （インストーラを作る場合）**Inno Setup 6**: `winget install JRSoftware.InnoSetup`

```powershell
git clone https://github.com/sangikousenuser/sd-backup-manager.git
cd sd-backup-manager
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### シミュレータで動かす（実機メディア・NAS なし）

```powershell
$env:SDBM_BASE_DIR = "$env:TEMP\sdbm-dev"     # 設定・DB・SSD 一時領域をここにまとめる
New-Item -ItemType Directory -Force "$env:SDBM_BASE_DIR\media", "$env:SDBM_BASE_DIR\nas" | Out-Null
Start-Process cargo -ArgumentList 'run -p sd-backup-daemon -- --base-dir', "$env:SDBM_BASE_DIR", '--simulate-media', "$env:SDBM_BASE_DIR\media", '--dev-nas-dir', "$env:SDBM_BASE_DIR\nas"
cargo run -p sd-backup-gui
# 「挿入」: 中身を用意したフォルダを media\ に移動する（p1, p2 ... がパーティション）
New-Item -ItemType Directory -Force "$env:TEMP\card\p1\DCIM\100MSDCF" | Out-Null
Set-Content "$env:TEMP\card\p1\DCIM\100MSDCF\DSC0001.JPG" "test"
Move-Item "$env:TEMP\card" "$env:SDBM_BASE_DIR\media\card1"
# 「抜去」: media\card1 を移動・削除する
```

シミュレータの書式は `crates/backup-device/src/sim.rs`（`media.toml` でシリアル・FS UUID、
`.unmount-fail` でアンマウント失敗を模擬）。デバッグビルドの daemon/gui はコンソール付きで起動する
（リリースビルドだけ `windows_subsystem = "windows"`）。

### 実機で動かす

```powershell
cargo build --release --workspace
pwsh scripts/package-windows.ps1 -Tag v0.0.0-dev -BinDir target\release   # dist\ に Setup.exe と zip
dist\sd-backup-manager-setup-x64.exe
& "$env:LOCALAPPDATA\Programs\SD Backup Manager\sd-backup.exe" doctor
```

ログ: `%LOCALAPPDATA%\sd-backup-manager\logs\daemon.log.<日付>`。
デーモンの停止: `sd-backup.exe shutdown`（または `Stop-ScheduledTask "SD Backup Manager Daemon"`）。

---

## 5. 検証状況と未検証事項

CI（windows-latest）で確認できるのは、コンパイル・clippy・単体テスト・シミュレータを使った E2E
（名前付きパイプ経由の IPC・コピー・検証・NAS ディレクトリへの転送・削除ガード・復元）まで。
次は **Windows 実機でしか確認できない**ので、最初にやること:

1. **SD カード（USB リーダー・内蔵リーダー）、USB メモリ、USB 外付け SSD/HDD の検出**
   - `IOCTL_STORAGE_QUERY_PROPERTY` の BusType が想定どおりか（内蔵 SD リーダーが SD/MMC/USB 以外を返すと無視される）
   - パーティションが 2 つある外付けディスクが 1 セッションにまとまるか
2. **一般ユーザー権限での `FSCTL_LOCK_VOLUME` / `FSCTL_DISMOUNT_VOLUME`**（リムーバブル以外の USB HDD で拒否されないか）
3. **抜去検出**: カードリーダーでカードを抜いたときに `IOCTL_STORAGE_CHECK_VERIFY2` が失敗するか
   （成功し続けるとスロットが空かない）
4. **SMB 接続**: `WNetAddConnection2W` の認証エラー・既存セッションとの資格情報の衝突（1219）・切断からの復帰
5. **タスク スケジューラ登録**（管理者なしで `Register-ScheduledTask` が通るか）、ログオン時の自動起動、ログオフ時の挙動
6. フォント（メイリオがない英語版 Windows）、通知音、SmartScreen
7. [acceptance.md](acceptance.md) の AT-01〜34 を Windows 実機で（Linux 向けの手順を読み替えて）実施

## 6. 既知の課題・次の作業（優先順）

1. 上記の実機検証と、見つかった不具合の修正
2. **USB HDD の電源停止**: 現在はロック + ディスマウントのみ。`CM_Request_Device_EjectW`（SetupAPI でディスクの
   DevInst を求め、親の USB デバイスに対して実行）を追加すると「安全に取り外す」と同等になる
3. **名前付きパイプのなりすまし対策の強化**: クライアント側で `GetNamedPipeServerProcessId` → プロセスの
   所有者 SID が自分と一致するか確認する。サーバー側も明示的な DACL（SDDL）を設定する
4. **ログオフ時の正常停止**: コンソールのないプロセスには Ctrl イベントが届かない。隠しウィンドウを作って
   `WM_QUERYENDSESSION` / `WM_ENDSESSION` を受け、`shutdown` をキャンセルする
5. Windows でも途中抜去を模擬できる E2E（シミュレータに「抜去フラグ」ファイルを設ける等）
6. Windows の保存先表示・フォルダを開く操作の実機確認（内部の `\\?\` 表現は表示時に除去済み）
7. アプリアイコン（.ico）、コード署名（SmartScreen 対策）
8. `doctor` の Windows チェック拡充（AutoPlay の状態、NAS への到達性）

## 7. 守ること

- **絶対ルール**（README の表）を弱める変更をしない。迷ったら「安全に止まってエラー表示」を選ぶ。
  特に: 元メディアへの書き込み・削除をしない／NAS 全件検証前に SSD を消さない／アンマウント成功時だけ
  「取り外し可能」／未検証ファイルを成功扱いしない。
- `cargo fmt`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` を通す。
  OS 依存コードは Linux と Windows の両方の CI で通ること（片方だけ直して壊さない）。
- 実機で確認していないことを「動作確認済み」と書かない。`acceptance.md` と本資料を更新する。
- コミットの作者は GitHub の noreply アドレス（`168276910+sangikousenuser@users.noreply.github.com`）。
  個人のメールアドレスを公開リポジトリに残さない。
- ドキュメント・UI 文言は日本語。
- リリース: `Cargo.toml` の `[workspace.package] version` を上げてから `vX.Y.Z` タグを push。
  タグとバージョンが違うと release.yml が失敗する。

## 8. 履歴

| 日付 | 内容 |
|---|---|
| 2026-10-15 | 仕様書から Rust ワークスペースを実装、CI、public リポジトリ作成 |
| 2026-10-16 | v0.1.0 リリース（Linux .deb / 1 行インストーラ）、Ubuntu 実機にインストール |
| 2026-10-16 | `doctor` の誤判定修正（PipeWire の通知音、/etc/xdg の自動起動）— 未リリース |
| 2026-10-27 | Windows へ移植（名前付きパイプ・Win32 デバイス・UNC/WNet・Inno Setup・Windows CI） |
| 2026-10-27 | cargo audit が RUSTSEC-2026-0385（ctutils 0.4.3）で失敗 → `cargo update -p ctutils` で 0.4.6 に更新 |
| 2026-10-27 | v0.2.0 リリース（Windows 版の初回公開） |
| 2026-10-28 | v0.3.0 リリース（SD 全体の保存・カード別フォルダ、PC 内保存先を開くボタン） |
