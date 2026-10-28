# 受け入れ試験（設計書 13 章）の実施状況

区分:
- **自動（E2E）**: `apps/daemon/tests/e2e.rs`。メディアシミュレータ + ローカルディレクトリの NAS で、
  デーモン全体（走査・分類・コピー・検証・アンマウント・NAS 転送・完了処理・IPC）を実行する。
  CI の Linux（ubuntu-latest・Ubuntu 26.04 コンテナ）・macOS・Windows で実行。
- **自動（単体）**: 各 crate の `#[cfg(test)]`。
- **実機未検証**: Ubuntu 26.04 実機・実際の SD カード・UDisks2・SMB NAS が必要。下の手順で確認すること。
  シミュレーションで代替している項目も、実機での確認が済むまで合格とはみなさない。

| ID | シナリオ | 状況 | 根拠 / 残課題 |
|---|---|---|---|
| AT-01 | ログイン後の自動起動 | 実機未検証 | systemd ユーザーサービス・XDG autostart を同梱。実機で再ログイン確認が必要 |
| AT-02 | 128GB SD を挿入 | E2E（シミュレータ）／実機未検証 | `happy_path_*`: 操作なしで走査・コピー開始、`sd1/14桁日時/` を作成 |
| AT-03 | 2 枚同時 | E2E（シミュレータ）／実機未検証 | `cloned_identity_*`・`capacity_wait_*` で 2 スロット同時処理。実機で速度・順序の確認が必要 |
| AT-04 | 3 台目 | E2E | `capacity_wait_*`: 3 台目は待機キュー |
| AT-05 | 2 パーティション | E2E | `happy_path_*`: P01/P02 を同一セッションに、マニフェストに両方記録 |
| AT-06 | FAT32/exFAT/NTFS/ext4 | 実機未検証 | Ubuntu の UDisks2 に依存。読めない FS・暗号化は除外して警告表示（実装済み） |
| AT-07 | 各社サンプル | 単体（構造）／実機未検証 | Sony XAVC・AVCHD・GoPro・Canon・RED・CinemaDNG の構造で分類テスト。実カードのサンプルで要確認 |
| AT-08 | 大容量動画・連番 RAW・大量小ファイル | 一部 | 固定 4MiB バッファ・ストリーミングハッシュ・衝突回避は実装/テスト済み。数十 GB 規模の性能は実機で計測 |
| AT-09 | 完了前に抜去 | E2E | `removal_mid_copy_*`: 成功表示しない・検証済みを保持 |
| AT-10 | 同じ SD を再挿入 | E2E | `removal_mid_copy_*`: 同じセッション・同じフォルダで再開し全件完了 |
| AT-11 | 正常終了後に追加撮影して再挿入 | E2E | `nas_offline_*`: 新しい日時フォルダに全ファイルを実体として保存し、既存ファイルを参照のみで済ませない |
| AT-12 | 同一パスを上書き | E2E | `nas_offline_*`: 同サイズで内容を変えた README.TXT を再取り込み |
| AT-13 | 破損ファイルで 3 回失敗 | E2E | `unreadable_file_*`: 合計 3 試行、他は継続、黄色・取り外し可能、SSD 削除なし |
| AT-14 | 外部アプリがマウント保持 | E2E（模擬）／実機未検証 | `unmount_failure_*`: 取り外し可能を表示しない・再試行で回復 |
| AT-15 | コピー中に NAS 切断 | E2E（NAS 不在）／実機未検証 | `nas_offline_*`: SSD 保持・NAS 復帰後に転送完了。実 SMB の切断試験が必要 |
| AT-16 | NAS 容量不足 | 実装済み・未試験 | ENOSPC/EDQUOT と事前の空き確認で再試行待ち。実 NAS で要確認 |
| AT-17 | SSD 20GB 境界 | E2E + 単体 | `capacity_wait_*`・`policy::admission_respects_reserve` |
| AT-18 | 128GB × 2 満杯 | 単体 | アドミッションで 2 枚目は容量待ち（先着優先）。実機で要確認 |
| AT-19 | NAS 一時ファイル作成中の切断 | 単体 | 中断時に一時ファイル削除・最終名を作らない。再送は先頭から |
| AT-20 | NAS 側の改変 | 単体 + 実装 | `nas::tampered_*`。削除前の最終再ハッシュで不一致なら再転送・SSD 保持 |
| AT-21 | NAS 全転送検証成功 | E2E | `happy_path_*`: そのセッションの SSD データだけ削除、DB・カタログ保持 |
| AT-22 | kill -9 / 停電 | 単体 + E2E（再起動） | `db::recovery_resets_in_flight_work`、`.part` 破棄、`nas_offline_*` の再起動。実機で kill -9 / 電源断試験が必要 |
| AT-23 | GUI だけ再起動 | 設計上保証 | GUI は状態を持たずスナップショットで再同期。実機で確認 |
| AT-24 | ログアウト/再ログイン | 実機未検証 | SIGTERM で安全に中断・保存、次回起動で再開 |
| AT-25 | 同じ UUID の別メディア | E2E | `cloned_identity_*`: ambiguous として別セッション |
| AT-26 | リンク・`..`・予約名・大文字小文字・Unicode | 単体 + E2E | `paths::*`・`scanner::*`・`restore::*`、`happy_path_*`（大文字小文字は大文字小文字を区別する FS で） |
| AT-27 | manifest から復元 | E2E + 単体 | `happy_path_*`（IPC 経由で復元しバイト一致）、`restore::*` |
| AT-28 | ログ 30 日経過 | 単体 | `db::events_purge_keeps_catalog`、ログファイルの日次削除 |
| AT-29 | GitHub Actions | CI | `.github/workflows/ci.yml` の結果を参照 |
| AT-30 | インストール・更新・アンインストール | 実機未検証 | `scripts/install.sh` ほか。実機で新規・更新・削除を確認 |
| AT-31 | NAS 認証誤り・共有すり替え | 単体 + 実装 | 保存先識別ファイル不一致で転送しない（`nas::marker_lifecycle`）、マウントパスのデバイス確認 |
| AT-32 | NAS 正常だがハッシュ検証不可 | 単体 + 実装 | 読み戻し不一致は確定しない・SSD 削除しない |
| AT-33 | 空き容量取得失敗 | 実装 + 単体 | GUI は「取得不可」表示、アドミッションは開始しない（`Admission::Unknown`） |
| AT-34 | 既知/未知形式 | 単体 + E2E | 未知拡張子は Other/ に保存 |

## Windows での状況

E2E（シミュレータ）と単体テストは GitHub Actions の windows-latest でも実行している（途中抜去の
E2E と読み込み不可ファイルの E2E は Windows では模擬できないため Unix のみ）。上表の「実機未検証」に
加えて、Windows では **メディア検出・ロック/ディスマウント・抜去検出・SMB 接続・タスク スケジューラ・
インストーラのすべてが実機未検証**。確認項目は [HANDOFF.md](HANDOFF.md) の 5 章。

## 実機試験の手順（Ubuntu 26.04）

1. `bash install.sh --version <tag>` → `sd-backup doctor` が全て ✔。
2. GUI で NAS を設定 →［接続テスト］→［保存］。NAS 上に `.sd-backup-manager-target.json` ができる。
3. 128GB カードを 2 枚（RAW+JPEG・長尺動画・多数の小ファイル）用意し、同時に挿入。
   速度・残り時間・CPU/RAM・温度・SSD 空きの推移を記録（`sd-backup status --json` を 10 秒ごとに保存）。
4. 途中抜去 → 同じカードを再挿入して再開すること。
5. NAS の LAN ケーブルを抜く / NAS を停止する → 復帰後に自動再送されること。SSD が削除されないこと。
6. カードのファイルを開いたままのアプリ（ファイルマネージャ等）がある状態でアンマウント失敗を確認。
7. `systemctl --user kill -s KILL sd-backup-daemon` → 再起動後に復旧すること。電源断も同様に確認。
8. ログアウト → 再ログインで再開すること。
9. 完了後、NAS の `manifest.json` を使って `sd-backup restore --manifest ... --to ~/Restore` で
   元の構成が再現され、`sd-backup verify-manifest --verify-files` が一致すること。
10. 合否はデータ整合性と失敗時の安全性を優先して判断する（速度基準は計測後に決める）。

## 2026-10-28 の追加確認（未リリース）

macOS 上で単体テスト・シミュレータ E2E を実施。Windows 向けは GNU ターゲットのクロスコンパイルによる型チェックを実施。実機での確認とは区別する。

- `whole_card_preserves_*`: 単一パーティションの元の構成、未知形式、空フォルダ、同名の管理ファイルの退避と復元。
- `formatted_cards_*`: SD/MMC シリアルが変わらなければ同じ番号、USB リーダーではフォーマット後に新番号。どちらも前回の中断セッションには合流せず、前回分の SSD データを保持。
- `nas_offline_*`: NAS オフライン時の SSD 保持と再起動復旧、新しい日時フォルダへの全件保存。
- `card_folders_recover_*`: 新しい階層の `.part` 復旧・対象日時だけの削除・他の日時フォルダの保護。
- Windows 実機では、設定画面の［PC 内の保存先を開く］で表示された実際のフォルダがエクスプローラーで開くこと、SD/MMC 接続のシリアルがカードごとに異なること、USB リーダーでカード交換時に取り違えないことを追加確認する。
