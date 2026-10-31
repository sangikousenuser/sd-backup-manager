//! シミュレータとローカルディレクトリの NAS を使ったエンドツーエンド試験。
//! （実機の SD 挿入・UDisks2・SMB は Ubuntu 実機での受け入れ試験で確認する）

use backup_config::AppPaths;
use backup_core::state::{SessionState, StageState};
use backup_device::WatchOptions;
use backup_ipc::SlotPhase;
use sd_backup_daemon::state::{Overrides, Shared};
use sd_backup_daemon::{DaemonOptions, start, stop};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

struct Env {
    _dir: tempfile::TempDir,
    base: PathBuf,
    media: PathBuf,
    nas: PathBuf,
    staging: PathBuf,
}

fn env() -> Env {
    // macOS の Unix ソケットパス長制限を避けるため短いパスを使う
    let dir = tempfile::Builder::new()
        .prefix("sdbm")
        .tempdir_in(if cfg!(unix) {
            std::path::PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        })
        .unwrap();
    let base = dir.path().to_path_buf();
    let media = base.join("media");
    let nas = base.join("nas");
    let staging = base.join("staging");
    fs::create_dir_all(&media).unwrap();
    fs::create_dir_all(&nas).unwrap();
    Env {
        _dir: dir,
        base,
        media,
        nas,
        staging,
    }
}

fn options(e: &Env, overrides: Overrides) -> DaemonOptions {
    DaemonOptions {
        paths: Some(AppPaths::under(&e.base)),
        simulate_media: Some(e.media.clone()),
        staging_override: Some(e.staging.clone()),
        overrides,
        watch: Some(WatchOptions {
            poll_interval: Duration::from_millis(50),
            stable_for: Duration::from_millis(200),
        }),
    }
}

/// Windows ではウォッチャーがフォルダを列挙している瞬間に名前変更・削除が失敗することがあるため再試行する。
fn rename_retry(from: impl AsRef<Path>, to: impl AsRef<Path>) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..100 {
        match fs::rename(from.as_ref(), to.as_ref()) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(last.unwrap())
}

fn remove_retry(p: impl AsRef<Path>) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..100 {
        match fs::remove_dir_all(p.as_ref()) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(last.unwrap())
}

fn write(p: &Path, data: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

/// カメラ風のメディアを作る（2 パーティション）。
fn make_card(media_root: &Path, name: &str) -> PathBuf {
    let card = media_root.join(format!(".{name}.building"));
    write(
        &card.join("media.toml"),
        format!("serial = \"SER-{name}\"\nfs_uuid = \"UUID-{name}\"\n").as_bytes(),
    );
    write(&card.join("p1/DCIM/100MSDCF/DSC0001.JPG"), b"jpeg-1");
    write(
        &card.join("p1/DCIM/100MSDCF/DSC0001.ARW"),
        &vec![3u8; 300_000],
    );
    write(
        &card.join("p1/PRIVATE/M4ROOT/CLIP/C0001.MP4"),
        &vec![7u8; 2_000_000],
    );
    write(&card.join("p1/PRIVATE/M4ROOT/CLIP/C0001M01.XML"), b"<xml/>");
    write(&card.join("p1/README.TXT"), b"hello");
    if case_sensitive(media_root) {
        write(&card.join("p1/dcim/100msdcf/dsc0001.jpg"), b"lowercase-dup"); // NAS 大文字小文字衝突
    }
    write(&card.join("p2/DATA/notes.xyz"), b"unknown format");
    // 準備ができてから一度に「挿入」する
    let final_path = media_root.join(name);
    rename_retry(&card, &final_path).unwrap();
    final_path
}

async fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        if start.elapsed() > timeout {
            panic!("timeout waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn slot_phase(shared: &Shared, i: usize) -> Option<SlotPhase> {
    shared.rt().slots[i].as_ref().map(|s| s.phase)
}

fn session_of_slot(shared: &Shared, i: usize) -> Option<String> {
    shared.rt().slots[i]
        .as_ref()
        .and_then(|s| s.session_id.clone())
}

fn session_state(shared: &Shared, sid: &str) -> SessionState {
    shared.db().get_session(sid).unwrap().unwrap().state
}

/// テスト用 FS が大文字小文字を区別するか（macOS の既定 APFS は区別しない）。
fn case_sensitive(dir: &Path) -> bool {
    let a = dir.join(".case-probe-A");
    let b = dir.join(".case-probe-a");
    fs::write(&a, b"A").unwrap();
    let sensitive = !b.exists();
    let _ = fs::remove_file(&a);
    sensitive
}

fn find_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                // Windows の区切り文字 `\\` を `/` にそろえて比較する
                out.push(
                    e.path()
                        .strip_prefix(root)
                        .unwrap()
                        .display()
                        .to_string()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out.sort();
    out
}

async fn shutdown(shared: Arc<Shared>, token: CancellationToken) {
    token.cancel();
    stop(&shared).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn happy_path_copies_verifies_transfers_and_cleans_up() {
    let e = env();
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(e.nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    make_card(&e.media, "cardA");

    wait_until("green", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    assert!(shared.rt().slots[0].as_ref().unwrap().phase.removable());
    wait_until("complete", Duration::from_secs(30), || {
        session_state(&shared, &sid) == SessionState::Complete
    })
    .await;

    let s = shared.db().get_session(&sid).unwrap().unwrap();
    assert!(s.folder_name.starts_with("sd1/"));
    assert_eq!(s.folder_name[4..].len(), 14);
    assert!(backup_core::session::is_valid_folder_name(&s.folder_name));
    let nas_session = e.nas.join(&s.folder_name);
    let files = find_files(&nas_session);
    assert!(
        files.contains(&"P01/DCIM/100MSDCF/DSC0001.JPG".to_string()),
        "{files:?}"
    );
    assert!(files.contains(&"P01/DCIM/100MSDCF/DSC0001.ARW".to_string()));
    assert!(files.contains(&"P01/PRIVATE/M4ROOT/CLIP/C0001.MP4".to_string()));
    assert!(files.contains(&"P01/PRIVATE/M4ROOT/CLIP/C0001M01.XML".to_string()));
    assert!(files.contains(&"P01/README.TXT".to_string()));
    assert!(files.contains(&"P02/DATA/notes.xyz".to_string()));
    assert!(files.contains(&"manifest.json".to_string()));
    let cs = case_sensitive(&e.base);
    // 大文字小文字の衝突はサフィックスで回避（上書きしない）
    if cs {
        assert!(
            files.iter().any(|f| f.contains("dsc0001~2.jpg")),
            "{files:?}"
        );
    }
    assert_eq!(
        fs::read(nas_session.join("P01/PRIVATE/M4ROOT/CLIP/C0001.MP4")).unwrap(),
        vec![7u8; 2_000_000]
    );
    assert!(!files.iter().any(|f| f.contains(".sdbm-tmp-")));

    // マニフェストは検証可能で、元パスを保持
    let m = backup_core::manifest::Manifest::parse_and_validate(
        &fs::read(nas_session.join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(m.files.len(), if cs { 7 } else { 6 });
    assert!(
        m.files
            .iter()
            .any(|f| f.source_rel_path == "PRIVATE/M4ROOT/CLIP/C0001.MP4")
    );
    assert_eq!(m.partitions.len(), 2);

    // SSD のセッションデータは削除済み、DB とカタログは保持
    assert!(!shared.staging.session_dir(&sid).unwrap().exists());
    assert!(shared.db().catalog_count().unwrap() >= 6);
    // 元メディアは変更されていない
    assert_eq!(
        fs::read(e.media.join("cardA/p1/README.TXT")).unwrap(),
        b"hello"
    );

    // 元の構成で復元（AT-27）
    let out = e.base.join("restore");
    let resp = backup_ipc::one_shot(
        &AppPaths::under(&e.base).socket_path(),
        "test",
        backup_ipc::Command::RestoreLayout {
            session_id: sid.clone(),
            dest_dir: out.display().to_string(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(resp, backup_ipc::Response::Ok { .. }), "{resp:?}");
    assert_eq!(
        fs::read(out.join("P01/PRIVATE/M4ROOT/CLIP/C0001.MP4")).unwrap(),
        vec![7u8; 2_000_000]
    );
    if cs {
        assert_eq!(
            fs::read(out.join("P01/dcim/100msdcf/dsc0001.jpg")).unwrap(),
            b"lowercase-dup"
        );
    }
    assert_eq!(
        fs::read(out.join("P02/DATA/notes.xyz")).unwrap(),
        b"unknown format"
    );

    // 抜去でスロットが空く
    remove_retry(e.media.join("cardA")).unwrap();
    wait_until("slot freed", Duration::from_secs(10), || {
        slot_phase(&shared, 0).is_none()
    })
    .await;
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nas_offline_keeps_staging_and_reinsert_after_completion_creates_new_folder() {
    let e = env();
    let offline_nas = e.base.join("nas-offline"); // 存在しない = 接続不可
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(offline_nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    make_card(&e.media, "cardB");
    wait_until("green", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    // NAS 未完了 → SSD は削除しない
    assert_eq!(session_state(&shared, &sid), SessionState::NasPending);
    assert!(
        e.staging
            .join("sessions")
            .join(&shared.db().get_session(&sid).unwrap().unwrap().folder_name)
            .join("P01/PRIVATE/M4ROOT/CLIP/C0001.MP4")
            .exists()
    );
    assert!(!shared.rt().nas.connected);
    shutdown(shared, token).await;
    // カードは抜いた状態で再起動する。挿したままだと、再起動直後に NAS 側の完了が先に済んだ場合
    // 「完了後の再挿入」として古い内容の新セッションが作られ、後の書き換え確認と競合する
    rename_retry(e.media.join("cardB"), e.base.join("cardB-out")).unwrap();

    // NAS が復帰した状態でデーモンを再起動 → 転送されて完了
    rename_retry(e.base.join("nas"), &offline_nas).unwrap();
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(offline_nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    wait_until(
        "complete after NAS recovery",
        Duration::from_secs(30),
        || session_state(&shared, &sid) == SessionState::Complete,
    )
    .await;
    let folder1 = shared.db().get_session(&sid).unwrap().unwrap().folder_name;

    assert!(
        shared.db().list_sessions(100).unwrap().len() == 1,
        "カードを挿していない間に新しいセッションは作られない"
    );
    // 正常終了後、同じカードに新しい撮影データを追加して再挿入（AT-11）
    write(
        &e.base.join("cardB-out/p1/DCIM/100MSDCF/DSC0002.JPG"),
        b"new shot",
    );
    // 同じパス・同じサイズのまま内容だけ上書き（AT-12: 偽スキップしない）
    write(&e.base.join("cardB-out/p1/README.TXT"), b"HELLO");
    tokio::time::sleep(Duration::from_millis(1100)).await; // 同一秒のフォルダ名を避ける必要はないが時刻を進める
    rename_retry(e.base.join("cardB-out"), e.media.join("cardB")).unwrap();
    wait_until("second session green", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
            && session_of_slot(&shared, 0).as_deref() != Some(sid.as_str())
    })
    .await;
    let sid2 = session_of_slot(&shared, 0).unwrap();
    wait_until("second complete", Duration::from_secs(30), || {
        session_state(&shared, &sid2) == SessionState::Complete
    })
    .await;
    let s2 = shared.db().get_session(&sid2).unwrap().unwrap();
    assert_ne!(s2.folder_name, folder1);
    let files = shared.db().files_for_session(&sid2).unwrap();
    let skipped = files
        .iter()
        .filter(|f| f.stage_state == StageState::SkippedVerified)
        .count();
    let copied: Vec<_> = files
        .iter()
        .filter(|f| f.stage_state == StageState::SsdVerified)
        .collect();
    let expected = if case_sensitive(&e.base) { 7 } else { 6 };
    assert_eq!(
        skipped, 0,
        "毎回の日時フォルダに全ファイルを実体として保存する"
    );
    assert_eq!(copied.len(), expected + 1);
    let nas_files = find_files(&offline_nas.join(&s2.folder_name));
    assert_eq!(nas_files.len(), expected + 2); // 全ファイル + manifest
    assert!(nas_files.contains(&"P01/PRIVATE/M4ROOT/CLIP/C0001.MP4".into()));
    assert_eq!(
        fs::read(offline_nas.join(&s2.folder_name).join("P01/README.TXT")).unwrap(),
        b"HELLO"
    );
    let m = backup_core::manifest::Manifest::parse_and_validate(
        &fs::read(offline_nas.join(&s2.folder_name).join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(m.files.iter().filter(|f| f.reference.is_some()).count(), 0);
    shutdown(shared, token).await;
}

// Windows ではファイル権限で読み込み失敗を作れないため Unix のみ
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreadable_file_turns_yellow_and_blocks_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let e = env();
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(e.nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    let card = e.base.join(".cardC");
    write(&card.join("p1/DCIM/100/OK.JPG"), b"fine");
    write(&card.join("p1/DCIM/100/BAD.JPG"), b"broken");
    fs::set_permissions(
        card.join("p1/DCIM/100/BAD.JPG"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    rename_retry(&card, e.media.join("cardC")).unwrap();

    wait_until("yellow", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneYellow)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    assert!(
        shared.rt().slots[0].as_ref().unwrap().phase.removable(),
        "yellow is removable only after unmount succeeded"
    );
    let bad = shared
        .db()
        .files_for_session(&sid)
        .unwrap()
        .into_iter()
        .find(|f| f.source_rel_path.ends_with("BAD.JPG"))
        .unwrap();
    assert_eq!(bad.stage_state, StageState::FailedTerminal);
    assert_eq!(bad.copy_attempts, 3, "exactly 3 attempts in total");
    // OK.JPG は NAS へ送られるが、セッションは削除されない
    wait_until("ok file on NAS", Duration::from_secs(20), || {
        shared
            .db()
            .get_session(&sid)
            .unwrap()
            .unwrap()
            .nas_verified_files
            == 1
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(session_state(&shared, &sid), SessionState::NeedsAttention);
    assert!(
        e.staging
            .join("sessions")
            .join(&shared.db().get_session(&sid).unwrap().unwrap().folder_name)
            .join("DCIM/100/OK.JPG")
            .exists()
    );
    fs::set_permissions(
        e.media.join("cardC/p1/DCIM/100/BAD.JPG"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmount_failure_is_never_removable() {
    let e = env();
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(e.nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    let card = e.base.join(".cardD");
    write(&card.join("p1/A.JPG"), b"a");
    write(&card.join(".unmount-fail"), b"");
    rename_retry(&card, e.media.join("cardD")).unwrap();
    wait_until("unmount failed", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::UnmountFailed)
    })
    .await;
    assert!(!shared.rt().slots[0].as_ref().unwrap().phase.removable());
    let sid = session_of_slot(&shared, 0).unwrap();
    assert_eq!(session_state(&shared, &sid), SessionState::UnmountFailed);

    // 使用中のプロセスが終了した → 再試行で取り外し可能に
    fs::remove_file(e.media.join("cardD/.unmount-fail")).unwrap();
    let r = sd_backup_daemon::orchestrator::retry_unmount(&shared, 0).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(slot_phase(&shared, 0), Some(SlotPhase::DoneGreen));
    shutdown(shared, token).await;
}

// Windows では開いているファイルを含むフォルダの名前を変更できず、この方法では
// 途中抜去を模擬できないため Unix のみ（Windows は実機試験で確認する）
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removal_mid_copy_then_reinsert_resumes_same_session() {
    let e = env();
    let token = CancellationToken::new();
    // NAS なし（SSD 取り込みの中断・再開だけを見る）
    let shared = start(options(&e, Overrides::default()), token.clone())
        .await
        .unwrap();
    let card = e.base.join(".cardE");
    for i in 0..40 {
        write(
            &card.join(format!("p1/PRIVATE/M4ROOT/CLIP/C{i:04}.MP4")),
            &vec![i as u8; 8_000_000],
        );
    }
    write(
        &card.join("media.toml"),
        b"serial = \"E1\"\nfs_uuid = \"EEEE\"\n",
    );
    rename_retry(&card, e.media.join("cardE")).unwrap();
    wait_until("copying", Duration::from_secs(20), || {
        shared.rt().slots[0]
            .as_ref()
            .is_some_and(|s| s.files_done >= 2)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    let folder = shared.db().get_session(&sid).unwrap().unwrap().folder_name;
    rename_retry(e.media.join("cardE"), e.base.join("cardE-out")).unwrap();
    wait_until("interrupted state", Duration::from_secs(20), || {
        session_state(&shared, &sid) == SessionState::SourceInterrupted
    })
    .await;
    wait_until("slot freed", Duration::from_secs(10), || {
        slot_phase(&shared, 0).is_none()
    })
    .await;
    let partial = shared.db().get_session(&sid).unwrap().unwrap();
    assert!(partial.completed_files < 40, "not falsely complete");

    rename_retry(e.base.join("cardE-out"), e.media.join("cardE")).unwrap();
    wait_until("resumed green", Duration::from_secs(60), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    assert_eq!(
        session_of_slot(&shared, 0).as_deref(),
        Some(sid.as_str()),
        "same session reused"
    );
    let s = shared.db().get_session(&sid).unwrap().unwrap();
    assert_eq!(s.folder_name, folder);
    assert_eq!(s.completed_files, 40);
    assert_eq!(s.failed_files, 0);
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capacity_wait_blocks_start_and_third_media_waits() {
    let e = env();
    let token = CancellationToken::new();
    // 空き 20.1GB: 最低確保 20GB + マージン 256MB を満たさない
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: None,
                fake_ssd_free_bytes: Some(20_100_000_000),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    for name in ["c1", "c2", "c3"] {
        let card = e.base.join(format!(".{name}"));
        write(&card.join("p1/A.JPG"), b"a");
        write(
            &card.join("media.toml"),
            format!("serial = \"{name}\"\n").as_bytes(),
        );
        rename_retry(&card, e.media.join(name)).unwrap();
    }
    wait_until(
        "two capacity waits and one queued",
        Duration::from_secs(20),
        || {
            let rt = shared.rt();
            rt.slots.iter().all(|s| {
                s.as_ref()
                    .is_some_and(|s| s.phase == SlotPhase::CapacityWait)
            }) && rt.waiting.len() == 1
        },
    )
    .await;
    // SSD には何も書かれていない
    let sessions_dir = e.staging.join("sessions");
    let written = fs::read_dir(&sessions_dir)
        .map(|rd| rd.count())
        .unwrap_or(0);
    assert_eq!(written, 0);
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloned_identity_is_ambiguous_and_never_merged() {
    let e = env();
    let token = CancellationToken::new();
    let shared = start(options(&e, Overrides::default()), token.clone())
        .await
        .unwrap();
    // 同じシリアル・機種・FS UUID を持つ 2 枚（クローン）
    for name in ["x1", "x2"] {
        let card = e.base.join(format!(".{name}"));
        write(&card.join("p1/A.JPG"), name.as_bytes());
        write(
            &card.join("media.toml"),
            b"serial = \"SAME\"\nmodel = \"Clone\"\nfs_uuid = \"DUP\"\n",
        );
        rename_retry(&card, e.media.join(name)).unwrap();
    }
    wait_until("both done", Duration::from_secs(20), || {
        let rt = shared.rt();
        rt.slots
            .iter()
            .all(|s| s.as_ref().is_some_and(|s| s.phase == SlotPhase::DoneGreen))
    })
    .await;
    let (a, b, conf) = {
        let rt = shared.rt();
        let a = rt.slots[0].as_ref().unwrap();
        let b = rt.slots[1].as_ref().unwrap();
        (
            a.session_id.clone(),
            b.session_id.clone(),
            [a.confidence.clone(), b.confidence.clone()],
        )
    };
    assert_ne!(a, b, "clones must get separate sessions");
    assert!(
        conf.iter().any(|c| c.as_deref() == Some("ambiguous")),
        "{conf:?}"
    );
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whole_card_preserves_unknown_files_empty_folders_and_reserved_source_names() {
    let e = env();
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(e.nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    let card = e.base.join("card-building");
    write(
        &card.join("media.toml"),
        b"serial = \"reader\"\nfs_uuid = \"whole-card\"\n",
    );
    write(&card.join("p1/DCIM/100/PHOTO.JPG"), b"photo");
    write(&card.join("p1/PRIVATE/CLIP.MP4"), b"video");
    write(&card.join("p1/unknown.bin"), b"unknown");
    write(&card.join("p1/manifest.json"), b"original manifest");
    write(
        &card.join("p1/.partial/keep.txt"),
        b"original partial directory",
    );
    write(
        &card.join("p1/.sdbm-tmp-original.part"),
        b"original temp name",
    );
    fs::create_dir_all(card.join("p1/EMPTY/CHILD")).unwrap();
    rename_retry(&card, e.media.join("whole")).unwrap();
    wait_until(
        "complete whole-card snapshot",
        Duration::from_secs(30),
        || {
            session_of_slot(&shared, 0)
                .is_some_and(|sid| session_state(&shared, &sid) == SessionState::Complete)
        },
    )
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    let s = shared.db().get_session(&sid).unwrap().unwrap();
    let root = e.nas.join(&s.folder_name);
    assert!(root.join("DCIM/100/PHOTO.JPG").exists());
    assert!(root.join("PRIVATE/CLIP.MP4").exists());
    assert_eq!(fs::read(root.join("unknown.bin")).unwrap(), b"unknown");
    assert_eq!(
        fs::read(root.join("manifest~2.json")).unwrap(),
        b"original manifest"
    );
    assert_eq!(
        fs::read(root.join(".partial~2/keep.txt")).unwrap(),
        b"original partial directory"
    );
    assert_eq!(
        fs::read(root.join("_.sdbm-tmp-original.part")).unwrap(),
        b"original temp name"
    );
    assert!(root.join("EMPTY/CHILD").is_dir());
    assert!(!root.join("Photos").exists());
    let manifest = backup_core::manifest::Manifest::parse_and_validate(
        &fs::read(root.join("manifest.json")).unwrap(),
    )
    .unwrap();
    let restored = e.base.join("restored-whole");
    let report =
        backup_storage::restore::restore_layout(&manifest, &root, Some(&e.nas), &restored).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(restored.join("P01/EMPTY/CHILD").is_dir());
    assert_eq!(
        fs::read(restored.join("P01/manifest.json")).unwrap(),
        b"original manifest"
    );
    assert_eq!(
        fs::read(restored.join("P01/.partial/keep.txt")).unwrap(),
        b"original partial directory"
    );
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn formatted_cards_reuse_only_native_card_serial_and_never_resume_old_session() {
    for (bus, expected_card) in [("usb", "sd2"), ("sd", "sd1")] {
        let e = env();
        let token = CancellationToken::new();
        // NAS オフラインのまま、前回分の SSD データも維持されることを確認する。
        let shared = start(options(&e, Overrides::default()), token.clone())
            .await
            .unwrap();
        let card = e.base.join("format-card");
        write(
            &card.join("media.toml"),
            format!("serial = \"fixed-serial\"\nfs_uuid = \"before\"\nbus = \"{bus}\"\n")
                .as_bytes(),
        );
        write(&card.join("p1/OLD.DAT"), b"old data");
        rename_retry(&card, e.media.join("card")).unwrap();
        wait_until("first card green", Duration::from_secs(20), || {
            slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
        })
        .await;
        let first_sid = session_of_slot(&shared, 0).unwrap();
        let first_folder = shared
            .db()
            .get_session(&first_sid)
            .unwrap()
            .unwrap()
            .folder_name;
        assert!(first_folder.starts_with("sd1/"));
        rename_retry(e.media.join("card"), &card).unwrap();
        wait_until("removed before format", Duration::from_secs(10), || {
            slot_phase(&shared, 0).is_none()
        })
        .await;
        fs::remove_file(card.join("p1/OLD.DAT")).unwrap();
        write(
            &card.join("media.toml"),
            format!("serial = \"fixed-serial\"\nfs_uuid = \"after\"\nbus = \"{bus}\"\n").as_bytes(),
        );
        write(&card.join("p1/NEW.DAT"), b"new data");
        rename_retry(&card, e.media.join("card")).unwrap();
        wait_until("formatted card green", Duration::from_secs(20), || {
            slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
        })
        .await;
        let second_sid = session_of_slot(&shared, 0).unwrap();
        let second = shared.db().get_session(&second_sid).unwrap().unwrap();
        assert_ne!(first_sid, second_sid);
        assert_ne!(first_folder, second.folder_name);
        assert!(
            second.folder_name.starts_with(&format!("{expected_card}/")),
            "{bus}: {}",
            second.folder_name
        );
        assert_eq!(
            fs::read(
                e.staging
                    .join("sessions")
                    .join(first_folder)
                    .join("OLD.DAT")
            )
            .unwrap(),
            b"old data"
        );
        assert_eq!(
            fs::read(
                e.staging
                    .join("sessions")
                    .join(&second.folder_name)
                    .join("NEW.DAT")
            )
            .unwrap(),
            b"new data"
        );
        assert!(
            !e.staging
                .join("sessions")
                .join(second.folder_name)
                .join("OLD.DAT")
                .exists()
        );
        shutdown(shared, token).await;
    }
}

async fn nas_file_command(
    e: &Env,
    sid: &str,
    ids: Vec<String>,
    action: backup_ipc::NasFileAction,
) -> Result<backup_ipc::Response, String> {
    backup_ipc::one_shot(
        &AppPaths::under(&e.base).socket_path(),
        "nas-file-test",
        backup_ipc::Command::ManageNasFiles {
            session_id: sid.into(),
            file_ids: ids,
            action,
        },
    )
    .await
    .map_err(|e| e.to_string())
}

async fn reset_preview(e: &Env) -> backup_ipc::LocalDataSummary {
    let response = backup_ipc::one_shot(
        &AppPaths::under(&e.base).socket_path(),
        "reset-test",
        backup_ipc::Command::GetLocalDataSummary,
    )
    .await
    .unwrap();
    let backup_ipc::Response::LocalDataSummary { summary } = response else {
        panic!("reset summary expected")
    };
    summary
}

async fn reset_confirm(
    e: &Env,
    token: String,
) -> Result<backup_ipc::Response, backup_ipc::IpcError> {
    backup_ipc::one_shot(
        &AppPaths::under(&e.base).socket_path(),
        "reset-test",
        backup_ipc::Command::ResetLocalData {
            confirmation_token: token,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reset_all_local_data_keeps_nas_source_settings_and_allows_fresh_import() {
    let e = env();
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(e.nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    // 一度送信済みの履歴・カタログと、その後の未送信コピーを両方作る。
    make_card(&e.media, "sent");
    wait_until("sent", Duration::from_secs(30), || {
        session_of_slot(&shared, 0)
            .is_some_and(|sid| session_state(&shared, &sid) == SessionState::Complete)
    })
    .await;
    remove_retry(e.media.join("sent")).unwrap();
    wait_until("removed", Duration::from_secs(10), || {
        slot_phase(&shared, 0).is_none()
    })
    .await;
    let saved_nas = e.base.join("saved-nas");
    rename_retry(&e.nas, &saved_nas).unwrap();
    let nas_files = find_files(&saved_nas)
        .into_iter()
        .map(|p| {
            let bytes = fs::read(saved_nas.join(&p)).unwrap();
            (p, bytes)
        })
        .collect::<Vec<_>>();
    make_card(&e.media, "pending");
    wait_until("pending local copy", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    let source_bytes = fs::read(e.media.join("pending/p1/README.TXT")).unwrap();
    let source_saved = e.base.join("source-kept");
    let mut config = shared.config();
    config.nas.mode = backup_config::NasMode::Gvfs;
    config.nas.url = "smb://kept.invalid/backup".into();
    config.nas.username = "kept-user".into();
    config.nas.target_id = "kept-target".into();
    config.save(&shared.paths.config_file()).unwrap();
    *shared.config.write().unwrap() = config.clone();
    let config_bytes = fs::read(shared.paths.config_file()).unwrap();
    fs::write(
        shared.paths.log_dir.join("daemon.log.old"),
        "old diagnostic history",
    )
    .unwrap();
    fs::write(e.staging.join("unrelated.txt"), "keep unrelated data").unwrap();
    let summary = reset_preview(&e).await;
    assert_eq!(summary.sessions, 2);
    assert!(summary.catalog_entries > 0 && summary.transfers > 0 && summary.local_bytes > 0);
    assert!(reset_confirm(&e, "invalid".into()).await.is_err());
    let summary = reset_preview(&e).await;
    assert!(
        reset_confirm(&e, summary.confirmation_token).await.is_err(),
        "接続中の SD があれば削除しない"
    );
    assert!(shared.staging.session_dir(&sid).unwrap().exists());
    rename_retry(e.media.join("pending"), &source_saved).unwrap();
    wait_until(
        "source removed before reset",
        Duration::from_secs(10),
        || slot_phase(&shared, 0).is_none(),
    )
    .await;
    // NAS の処理がロックを解放するまで、PC の削除は始めない。
    let operation = shared.nas_operation.lock().await;
    let summary = reset_preview(&e).await;
    let socket = shared.paths.socket_path();
    let confirmation_token = summary.confirmation_token;
    let reset = tokio::spawn(async move {
        backup_ipc::one_shot(
            &socket,
            "reset-test",
            backup_ipc::Command::ResetLocalData { confirmation_token },
        )
        .await
    });
    wait_until("reset stops workers", Duration::from_secs(5), || {
        shared.is_paused()
    })
    .await;
    assert!(shared.staging.session_dir(&sid).unwrap().exists());
    drop(operation);
    reset.await.unwrap().unwrap();
    assert_eq!(
        shared.db().local_data_counts().unwrap(),
        backup_db::LocalDataCounts::default()
    );
    assert_eq!(shared.db().schema_version().unwrap(), 4);
    assert!(shared.is_paused());
    assert!(!shared.db().local_reset_pending().unwrap());
    assert!(!shared.staging.session_dir(&sid).unwrap().exists());
    assert_eq!(fs::read(shared.paths.config_file()).unwrap(), config_bytes);
    assert_eq!(shared.config(), config);
    assert!(
        fs::read(shared.paths.log_dir.join("daemon.log.old"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read(source_saved.join("p1/README.TXT")).unwrap(),
        source_bytes
    );
    assert_eq!(
        fs::read_to_string(e.staging.join("unrelated.txt")).unwrap(),
        "keep unrelated data"
    );
    for (p, bytes) in nas_files {
        assert_eq!(fs::read(saved_nas.join(p)).unwrap(), bytes);
    }
    backup_ipc::one_shot(
        &shared.paths.socket_path(),
        "reset-test",
        backup_ipc::Command::Resume,
    )
    .await
    .unwrap();
    make_card(&e.media, "fresh");
    wait_until("fresh import after reset", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let fresh = shared
        .db()
        .get_session(&session_of_slot(&shared, 0).unwrap())
        .unwrap()
        .unwrap();
    assert!(
        fresh.folder_name.starts_with("sd3/"),
        "{}",
        fresh.folder_name
    );
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_local_reset_stays_stopped_until_deletion_can_finish() {
    let e = env();
    let opts = || {
        options(
            &e,
            Overrides {
                nas_local_dir: Some(e.base.join("offline")),
                ..Default::default()
            },
        )
    };
    let token = CancellationToken::new();
    let shared = start(opts(), token.clone()).await.unwrap();
    make_card(&e.media, "reset-crash");
    wait_until("local copy", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    let session_dir = shared.staging.session_dir(&sid).unwrap();
    remove_retry(e.media.join("reset-crash")).unwrap();
    wait_until("source removed", Duration::from_secs(10), || {
        slot_phase(&shared, 0).is_none()
    })
    .await;
    // 削除意図を記録した直後の停止と、起動時の削除失敗を再現する。
    shared.db().begin_local_reset().unwrap();
    shutdown(shared, token).await;
    let parked = e.base.join("parked-copy");
    rename_retry(&session_dir, &parked).unwrap();
    fs::write(&session_dir, "deletion blocker").unwrap();
    let token = CancellationToken::new();
    let shared = start(opts(), token.clone()).await.unwrap();
    assert!(shared.is_paused());
    assert!(shared.db().local_reset_pending().unwrap());
    assert_eq!(shared.db().local_data_counts().unwrap().sessions, 1);
    assert!(
        backup_ipc::one_shot(
            &shared.paths.socket_path(),
            "reset-test",
            backup_ipc::Command::Resume
        )
        .await
        .is_err()
    );
    let summary = reset_preview(&e).await;
    assert!(summary.reset_pending);
    assert!(reset_confirm(&e, summary.confirmation_token).await.is_err());
    assert!(shared.db().get_session(&sid).unwrap().is_some());
    fs::remove_file(&session_dir).unwrap();
    rename_retry(&parked, &session_dir).unwrap();
    // 次の起動で残りを完了する。履歴は実体を削除した後にだけ消える。
    shutdown(shared, token).await;
    let token = CancellationToken::new();
    let shared = start(opts(), token.clone()).await.unwrap();
    assert!(!shared.db().local_reset_pending().unwrap());
    assert_eq!(
        shared.db().local_data_counts().unwrap(),
        backup_db::LocalDataCounts::default()
    );
    assert!(!session_dir.exists());
    assert!(shared.is_paused());
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_nas_setup_preserves_pending_copies_and_transfers_after_restart() {
    use backup_ipc::{Command, Response};
    let e = env();
    let token = CancellationToken::new();
    let shared = start(options(&e, Overrides::default()), token.clone())
        .await
        .unwrap();
    make_card(&e.media, "before-nas-setup");
    wait_until(
        "SSD backup before NAS setup",
        Duration::from_secs(20),
        || slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen),
    )
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    let files = shared.db().files_for_session(&sid).unwrap();
    let staged = files
        .iter()
        .map(|f| {
            let p = shared
                .staging
                .final_path(&sid, &f.destination_rel_path)
                .unwrap();
            let bytes = fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect::<Vec<_>>();
    let socket = AppPaths::under(&e.base).socket_path();
    let request = |command| backup_ipc::one_shot(&socket, "initial-nas-test", command);
    // 保存先の設定テスト中に、実在しない SMB ホストへの接続を試みない。
    request(Command::Pause).await.unwrap();
    let Response::Settings { mut settings, .. } = request(Command::GetSettings).await.unwrap()
    else {
        panic!("settings response expected");
    };
    assert_eq!(settings.nas_mode, "disabled");
    settings.nas_mode = "gvfs".into();
    settings.nas_url = "smb://initial-nas.invalid/backup/".into();
    request(Command::UpdateSettings {
        settings: settings.clone(),
        password: None,
    })
    .await
    .expect("initial NAS setup must be allowed with queued files");
    assert_eq!(
        shared.db().nas_queue_stats().unwrap().queued_files,
        files.len() as u64
    );
    for (p, bytes) in &staged {
        assert_eq!(&fs::read(p).unwrap(), bytes);
    }
    let saved = backup_config::Config::load(&AppPaths::under(&e.base).config_file()).unwrap();
    assert_eq!(saved.nas.url, settings.nas_url);
    // 最初の設定を済ませた後は、未初期化でも転送先の変更・無効化は禁止する。
    let mut changed = settings.clone();
    changed.nas_url = "smb://other-nas.invalid/backup/".into();
    let error = request(Command::UpdateSettings {
        settings: changed,
        password: None,
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("未完了の NAS 転送"), "{error}");
    let mut disabled = settings.clone();
    disabled.nas_mode = "disabled".into();
    assert!(
        request(Command::UpdateSettings {
            settings: disabled,
            password: None
        })
        .await
        .is_err()
    );
    // 同じ接続先のユーザー名などは、キューを消さずに修正できる。
    settings.nas_username = "camera-backup".into();
    request(Command::UpdateSettings {
        settings,
        password: None,
    })
    .await
    .unwrap();
    remove_retry(e.media.join("before-nas-setup")).unwrap();
    shutdown(shared, token).await;

    // 再起動後に接続先を初期化し、元の転送キューが最後まで処理される。
    let nas = e.base.join("new-nas");
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        shared.db().nas_queue_stats().unwrap().queued_files,
        files.len() as u64
    );
    fs::create_dir(&nas).unwrap();
    request(Command::InitializeNasTarget).await.unwrap();
    assert!(!shared.config().nas.target_id.is_empty());
    wait_until(
        "NAS completed after first setup",
        Duration::from_secs(30),
        || session_state(&shared, &sid) == SessionState::Complete,
    )
    .await;
    let session = shared.db().get_session(&sid).unwrap().unwrap();
    for (f, (_, bytes)) in files.iter().zip(&staged) {
        let p = nas.join(&session.folder_name).join(&f.destination_rel_path);
        assert_eq!(&fs::read(p).unwrap(), bytes);
    }
    assert!(session.staging_deleted_at_utc.is_some());
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selected_nas_files_can_cancel_keep_pc_restart_and_resume_without_blocking_other_files() {
    use backup_core::state::NasFileState;
    use backup_ipc::NasFileAction;
    let e = env();
    let offline = e.base.join("offline-nas");
    let opts = || {
        options(
            &e,
            Overrides {
                nas_local_dir: Some(offline.clone()),
                ..Default::default()
            },
        )
    };
    let token = CancellationToken::new();
    let shared = start(opts(), token.clone()).await.unwrap();
    make_card(&e.media, "selected-nas");
    wait_until("SSD backup complete", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    let files = shared.db().files_for_session(&sid).unwrap();
    let selected = files
        .iter()
        .find(|f| f.source_rel_path == "README.TXT")
        .unwrap();
    let selected_id = selected.file_id.clone();
    let selected_path = shared
        .staging
        .final_path(&sid, &selected.destination_rel_path)
        .unwrap();
    nas_file_command(&e, &sid, vec![selected_id.clone()], NasFileAction::Cancel)
        .await
        .unwrap();
    assert_eq!(fs::read(&selected_path).unwrap(), b"hello");
    assert_eq!(
        shared
            .db()
            .get_file(&selected_id)
            .unwrap()
            .unwrap()
            .nas_state,
        NasFileState::Cancelled
    );
    assert_eq!(shared.db().nas_queue_stats().unwrap().cancelled_files, 1);
    assert_eq!(
        shared.db().nas_queue_stats().unwrap().queued_files,
        files.len() as u64 - 1
    );
    // 一括再試行や再起動で、個別に中止した転送を復活させない。
    sd_backup_daemon::nas_worker::retry_now(&shared);
    remove_retry(e.media.join("selected-nas")).unwrap();
    wait_until("source removed", Duration::from_secs(10), || {
        slot_phase(&shared, 0).is_none()
    })
    .await;
    shutdown(shared, token).await;
    let token = CancellationToken::new();
    let shared = start(opts(), token.clone()).await.unwrap();
    assert_eq!(
        shared
            .db()
            .get_file(&selected_id)
            .unwrap()
            .unwrap()
            .nas_state,
        NasFileState::Cancelled
    );
    let response = backup_ipc::one_shot(
        &AppPaths::under(&e.base).socket_path(),
        "nas-page-test",
        backup_ipc::Command::GetFiles {
            session_id: sid.clone(),
            only_problems: true,
            offset: 1,
            limit: 2,
        },
    )
    .await
    .unwrap();
    let backup_ipc::Response::Files {
        files: page,
        total_files,
    } = response
    else {
        panic!("unexpected response");
    };
    assert_eq!(page.len(), 2);
    assert_eq!(total_files, files.len() as u64);
    fs::create_dir_all(&offline).unwrap();
    shared.nas_wakeup.notify_one();
    wait_until("other files reach NAS", Duration::from_secs(30), || {
        shared
            .db()
            .files_for_session(&sid)
            .unwrap()
            .iter()
            .all(|f| f.file_id == selected_id || f.nas_state == NasFileState::NasVerified)
    })
    .await;
    assert!(
        selected_path.exists(),
        "NAS 全件成功ではないので PC 内のコピーを消さない"
    );
    assert_eq!(session_state(&shared, &sid), SessionState::NasPending);
    let verified = files.iter().find(|f| f.file_id != selected_id).unwrap();
    assert!(
        nas_file_command(
            &e,
            &sid,
            vec![selected_id.clone(), verified.file_id.clone()],
            NasFileAction::Cancel
        )
        .await
        .is_err()
    );
    assert_eq!(
        shared
            .db()
            .get_file(&selected_id)
            .unwrap()
            .unwrap()
            .nas_state,
        NasFileState::Cancelled
    );
    nas_file_command(&e, &sid, vec![selected_id.clone()], NasFileAction::Retry)
        .await
        .unwrap();
    wait_until(
        "selected file resumed and session complete",
        Duration::from_secs(30),
        || session_state(&shared, &sid) == SessionState::Complete,
    )
    .await;
    let session = shared.db().get_session(&sid).unwrap().unwrap();
    assert!(!selected_path.exists());
    assert_eq!(
        fs::read(
            offline
                .join(session.folder_name)
                .join(selected.destination_rel_path.clone())
        )
        .unwrap(),
        b"hello"
    );
    shutdown(shared, token).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selected_local_deletion_removes_only_pc_copy_and_queue_and_recovers_after_restart() {
    use backup_core::state::NasFileState;
    use backup_ipc::NasFileAction;
    let e = env();
    let offline = e.base.join("offline-delete-nas");
    let opts = || {
        options(
            &e,
            Overrides {
                nas_local_dir: Some(offline.clone()),
                ..Default::default()
            },
        )
    };
    let token = CancellationToken::new();
    let shared = start(opts(), token.clone()).await.unwrap();
    make_card(&e.media, "delete-files");
    wait_until("SSD files ready", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    let session = shared.db().get_session(&sid).unwrap().unwrap();
    let files = shared.db().files_for_session(&sid).unwrap();
    let selected = files
        .iter()
        .find(|f| f.source_rel_path == "README.TXT")
        .unwrap();
    let other = files
        .iter()
        .find(|f| f.source_rel_path == "DATA/notes.xyz")
        .unwrap();
    let selected_path = shared
        .staging
        .final_path(&sid, &selected.destination_rel_path)
        .unwrap();
    let other_path = shared
        .staging
        .final_path(&sid, &other.destination_rel_path)
        .unwrap();
    let nas_existing = e
        .nas
        .join(&session.folder_name)
        .join(&selected.destination_rel_path);
    write(&nas_existing, b"existing NAS copy");
    let part = shared
        .staging
        .partial_path(&sid, &selected.file_id)
        .unwrap();
    write(&part, b"stale partial");
    let card_out = e.base.join("delete-card-out");
    rename_retry(e.media.join("delete-files"), &card_out).unwrap();
    wait_until("SD removed before delete", Duration::from_secs(10), || {
        slot_phase(&shared, 0).is_none()
    })
    .await;
    nas_file_command(
        &e,
        &sid,
        vec![selected.file_id.clone()],
        NasFileAction::DeleteLocal,
    )
    .await
    .unwrap();
    assert!(!selected_path.exists());
    assert!(!part.exists());
    assert!(other_path.exists());
    assert_eq!(fs::read(&nas_existing).unwrap(), b"existing NAS copy");
    assert_eq!(fs::read(card_out.join("p1/README.TXT")).unwrap(), b"hello");
    assert_eq!(
        shared.db().pending_transfer_count(&sid).unwrap(),
        files.len() as u64 - 1
    );
    assert_eq!(
        shared
            .db()
            .get_file(&selected.file_id)
            .unwrap()
            .unwrap()
            .nas_state,
        NasFileState::Deleted
    );
    assert_eq!(session_state(&shared, &sid), SessionState::NeedsAttention);
    assert!(
        nas_file_command(
            &e,
            &sid,
            vec![selected.file_id.clone()],
            NasFileAction::Retry
        )
        .await
        .is_err()
    );
    let manifest = backup_core::manifest::Manifest::parse_and_validate(
        &fs::read(shared.staging.manifest_path(&sid).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest
            .files
            .iter()
            .find(|f| f.file_id == selected.file_id)
            .unwrap()
            .status,
        backup_core::manifest::ManifestFileStatus::Failed
    );
    // DB への削除意図の記録直後に停止した状態を模擬し、次回起動で続きを実行する。
    shared
        .db()
        .prepare_nas_file_deletion(&sid, std::slice::from_ref(&other.file_id))
        .unwrap();
    assert!(other_path.exists());
    shutdown(shared, token).await;
    let token = CancellationToken::new();
    let shared = start(opts(), token.clone()).await.unwrap();
    assert!(!other_path.exists());
    assert_eq!(
        shared
            .db()
            .get_file(&other.file_id)
            .unwrap()
            .unwrap()
            .stage_state,
        StageState::Discarded
    );
    assert_eq!(
        shared.db().pending_transfer_count(&sid).unwrap(),
        files.len() as u64 - 2
    );
    assert_eq!(fs::read(&nas_existing).unwrap(), b"existing NAS copy");
    let kept = files
        .iter()
        .find(|f| f.file_id != selected.file_id && f.file_id != other.file_id)
        .unwrap();
    assert!(
        shared
            .staging
            .final_path(&sid, &kept.destination_rel_path)
            .unwrap()
            .exists()
    );
    fs::create_dir_all(&offline).unwrap();
    shared.nas_wakeup.notify_one();
    wait_until(
        "non-deleted files transferred",
        Duration::from_secs(30),
        || {
            shared
                .db()
                .files_for_session(&sid)
                .unwrap()
                .iter()
                .all(|f| {
                    f.stage_state == StageState::Discarded
                        || f.nas_state == NasFileState::NasVerified
                })
        },
    )
    .await;
    assert!(
        !offline
            .join(&session.folder_name)
            .join(&selected.destination_rel_path)
            .exists()
    );
    assert!(
        !offline
            .join(&session.folder_name)
            .join(&other.destination_rel_path)
            .exists()
    );
    assert_eq!(session_state(&shared, &sid), SessionState::NeedsAttention);
    shutdown(shared, token).await;
}

fn has_event(shared: &Shared, event_type: &str) -> bool {
    shared
        .db()
        .recent_events("2000-01-01T00:00:00.000Z", 10_000)
        .unwrap()
        .iter()
        .any(|e| e.event_type == event_type)
}

/// 転送中に NAS が応答しなくなっても（SMB 切断でブロックし続ける状態）、自動で切り離して再送し、
/// 最後まで完了する。止まったまま 0 件で放置されない。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unresponsive_nas_is_detached_and_transfer_still_completes() {
    let e = env();
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(e.nas.clone()),
                nas_stall_timeout: Some(Duration::from_secs(2)),
                nas_hang_once: Some("C0001.MP4".into()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    make_card(&e.media, "hang");
    wait_until("green", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    // 固まった転送が検知・切り離されたこと
    wait_until("stall detected", Duration::from_secs(20), || {
        has_event(&shared, "nas_stalled")
    })
    .await;
    // 自動再試行（バックオフ後）で最後まで完了する
    wait_until("complete after stall", Duration::from_secs(60), || {
        session_state(&shared, &sid) == SessionState::Complete
    })
    .await;
    let s = shared.db().get_session(&sid).unwrap().unwrap();
    assert_eq!(s.failed_files, 0);
    assert!(
        find_files(&e.nas.join(&s.folder_name))
            .iter()
            .any(|f| f.ends_with("C0001.MP4"))
    );
    shutdown(shared, token).await;
}

/// NAS が応答しない間でも「今すぐ再試行」がすぐ効き、ユーザーのファイル操作のロックも数秒で取れる。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_and_user_actions_work_while_nas_is_unresponsive() {
    let e = env();
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(e.nas.clone()),
                // 自動検知には任せず（10 分）、再試行・操作要求で切り離されることを確かめる
                nas_stall_timeout: Some(Duration::from_secs(600)),
                nas_hang_once: Some("C0001.MP4".into()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    make_card(&e.media, "retry");
    wait_until("green", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    // 固まっている（進捗が止まっている）ことを確認
    wait_until("stalled", Duration::from_secs(20), || {
        shared.rt().nas.stalled_secs.is_some()
            || shared
                .rt()
                .nas
                .current_file
                .as_deref()
                .is_some_and(|f| f.ends_with("C0001.MP4"))
    })
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;

    // ユーザー操作は 30 秒以内（実際は数秒）に NAS 処理の排他を取れる
    let started = Instant::now();
    let guard = shared
        .lock_nas_operation(Duration::from_secs(30))
        .await
        .expect("user action must not be blocked by an unresponsive NAS");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    drop(guard);

    // 「今すぐ再試行」で待ち時間を飛ばしてやり直し、完了する
    sd_backup_daemon::nas_worker::retry_now(&shared);
    wait_until("complete after retry", Duration::from_secs(40), || {
        session_state(&shared, &sid) == SessionState::Complete
    })
    .await;
    shutdown(shared, token).await;
}

/// 「転送中」のまま取り残されたレコード（ワーカーが戻ってこなかった等）があっても、
/// 自己修復で再開し、0 件のまま止まらずに完了する。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_upload_records_are_repaired_instead_of_stalling_at_zero() {
    let e = env();
    let nas = e.base.join("nas-later"); // 最初は NAS に接続できない
    let token = CancellationToken::new();
    let shared = start(
        options(
            &e,
            Overrides {
                nas_local_dir: Some(nas.clone()),
                ..Default::default()
            },
        ),
        token.clone(),
    )
    .await
    .unwrap();
    make_card(&e.media, "stale");
    wait_until("green", Duration::from_secs(20), || {
        slot_phase(&shared, 0) == Some(SlotPhase::DoneGreen)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    // NAS オフライン中に、転送中のまま中断された状態を作る（以前は二度と取り出されなかった）
    {
        let db = shared.db();
        let job = db
            .next_due_transfer(&backup_db::now_utc())
            .unwrap()
            .expect("queued transfer");
        db.transfer_started(&job, "stale").unwrap();
        assert_eq!(db.nas_queue_stats().unwrap().uploading_files, 1);
    }
    fs::create_dir_all(&nas).unwrap();
    sd_backup_daemon::nas_worker::retry_now(&shared);
    wait_until("complete after repair", Duration::from_secs(40), || {
        session_state(&shared, &sid) == SessionState::Complete
    })
    .await;
    assert!(has_event(&shared, "nas_queue_repaired"));
    shutdown(shared, token).await;
}
