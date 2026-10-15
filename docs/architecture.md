# アーキテクチャ

## プロセス構成

```
sd-backup-gui (Iced)  ──┐
sd-backup (CLI)       ──┤  $XDG_RUNTIME_DIR/sd-backup-manager/daemon.sock
                        │  JSON Lines / ディレクトリ 0700・ソケット 0600・peer UID 照合
                        ▼
sd-backup-daemon (systemd --user)
  ├─ device loop      UDisks2 監視（1 秒間隔 + 2 秒の安定化確認）→ スロット割り当て / 待機キュー
  ├─ media task ×≤2   prepare(ro mount) → 走査 → 分類 → 容量アドミッション → コピー/検証 → アンマウント
  ├─ nas worker       接続(marker 照合) → 転送/読み戻し検証 → manifest → 最終検証 → SSD 削除
  ├─ snapshot         0.5 秒ごとに状態を集約して購読中の GUI へ配信、5 秒ごとに容量取得
  ├─ maintenance      30 日を過ぎた events / ログの削除（catalog は保持）
  └─ SQLite (WAL, synchronous=FULL) + ~/SDBackupStaging
```

GUI は状態を保持しません。再接続時は最新スナップショットで再同期します。GUI を閉じても
デーモンは動作を続けます。ログアウト時は SIGTERM を受けてコピーを中断し（試行回数に数えない）、
セッションを `SOURCE_INTERRUPTED` として保存、次回ログインで同じメディアが接続されていれば再開します。

## メディア処理（apps/daemon/src/orchestrator.rs）

1. **準備**: 既にマウント済みならそのマウントポイントを使い、未マウントなら UDisks2 で
   `ro,nosuid,nodev,noexec` マウント。アプリは常に読み込み専用でしか開かない。
2. **識別**: シリアル・機種・容量・各パーティションの FS UUID / パーティション GUID から
   `identity_hash`。同じハッシュのメディアが同時に接続されたら `ambiguous`（クローン）として、
   中断セッションへの合流とカタログによるスキップを禁止する。
3. **セッション**: `high/medium` かつ再開可能な状態（中断・要対応・アンマウント失敗）の
   セッションがあれば同じ日時フォルダを再利用。なければ `YYYYMMDDHHMMSSNN`（JST）で新規作成。
   フォルダ名の一意性は DB の UNIQUE 制約で保証。
4. **走査**: symlink・特殊ファイルは取り込まず警告。読めないフォルダはエラーとして記録し、
   そのセッションは緑にしない。
5. **分類**: `config/formats.toml` の拡張子定義 + 既知の収録構造（`PRIVATE/M4ROOT` 等）+ 同名グループ。
   未知の拡張子は必ず `Other/`。格納パスは `<Kind>/P<nn>/<元の相対パス>`。NAS の大文字小文字
   非区別・NFC 正規化・予約文字で衝突する場合は `name~2.ext` のように一意化（マニフェストで対応を保持）。
6. **容量アドミッション**: `空き ≥ 20GB + 必要量 + マージン(最大ファイル + 1%, ≥256MB) + 他セッションの残り予約`
   を満たすまで開始しない。先着順。SSD 容量自体を超える場合は「手動対応が必要」と表示。
7. **コピー**: `.partial/<file_id>.part` へ書き込みながら BLAKE3 → fsync → ページキャッシュ破棄 →
   元ファイルの size/mtime 再確認 → 再オープンして BLAKE3 → 一致したら rename + 親 fsync →
   DB を `SSD_VERIFIED` にして NAS キューへ。失敗は合計 3 試行まで。抜去・停止・容量待ちによる中断は
   試行回数に数えない。
8. **仕上げ**: 全パーティションを UDisks2 でアンマウントし、成功時のみ `取り外し可能`。

## NAS 処理（apps/daemon/src/nas_worker.rs）

- 接続: GVfs（`gio mount` に資格情報を標準入力で渡し、`gio info` の local path を使う）または
  管理者が用意した CIFS マウントパス。どちらも共有ルートの `.sd-backup-manager-target.json` が
  設定の `target_id` と一致しない限り書き込まない（共有すり替え・未マウント時の誤書き込み防止）。
- 転送: `.sdbm-tmp-<uuid>.part` に書き込み（SSD 側も同時にハッシュして記録と照合）→ fsync →
  読み戻しハッシュ → 同じディレクトリで rename。失敗は指数バックオフ（5 秒〜5 分、±20% ジッタ）で
  無期限に再試行。
- 完了: 全ファイルが NAS 検証済み（またはスキップ参照が検証済み）→ `manifest.json` を作成・転送・検証 →
  （既定で）NAS 上の全ファイルを再ハッシュ → 9.4 の削除ガード → `sessions/<id>` だけを削除。

## 状態

セッション状態は設計書 9.1 の遷移に次を追加しています（`backup_core::state`）。

| 追加遷移 | 理由 |
|---|---|
| DETECTED/SCANNING/BLOCKED/SOURCE_FINALIZING/UNMOUNTING → SOURCE_INTERRUPTED | 走査中・待機中の抜去、デーモン停止 |
| SOURCE_INTERRUPTED/NEEDS_ATTENTION → SCANNING | 再挿入時に不足分を確認するため再走査 |
| UNMOUNT_FAILED → UNMOUNTING / NAS_PENDING / NEEDS_ATTENTION | 再試行、または抜去後の NAS 処理継続 |
| NAS_VERIFIED/STAGING_CLEANUP → NAS_PENDING, NAS_PENDING → NEEDS_ATTENTION | 最終検証での不一致・SSD 実体の欠落 |
| BLOCKED → COPYING, COPYING → BLOCKED | 容量待ち後の開始 |

ファイルは SSD 側 `stage_state` と NAS 側 `nas_state` を独立に管理します（設計書 9.2）。

## 設計書からの具体化・差分

- **udev 監視**: UDisks2（udev のイベントで更新される）の `GetManagedObjects` を 1 秒間隔で取得して
  差分を取る方式にした。マウント/アンマウントと同じ API で一貫した情報が得られ、libudev への
  依存が不要になる。
- **SSD 確定ファイルの起動時再検証**: 起動時は存在とサイズを確認し、内容のハッシュは NAS 転送時に
  SSD 側を読み直して記録と照合する（不一致なら NAS へ送らず再取り込み対象）。
- **NAS の一時ファイル再開**: 途中再開は行わず、常に先頭から再送する（9.3-4 の安全側）。
- **CinemaDNG 判定**: 同一フォルダに 24 個以上の DNG があれば動画シーケンスとみなす（設定変更可）。
