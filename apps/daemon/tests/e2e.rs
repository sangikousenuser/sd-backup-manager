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
        .tempdir_in("/tmp")
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
    fs::rename(&card, &final_path).unwrap();
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
                out.push(e.path().strip_prefix(root).unwrap().display().to_string());
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
    assert_eq!(s.folder_name.len(), 16);
    assert!(s.folder_name.bytes().all(|b| b.is_ascii_digit()));
    let nas_session = e.nas.join(&s.folder_name);
    let files = find_files(&nas_session);
    assert!(
        files.contains(&"Photos/P01/DCIM/100MSDCF/DSC0001.JPG".to_string()),
        "{files:?}"
    );
    assert!(files.contains(&"Photos/P01/DCIM/100MSDCF/DSC0001.ARW".to_string()));
    assert!(files.contains(&"Videos/P01/PRIVATE/M4ROOT/CLIP/C0001.MP4".to_string()));
    assert!(files.contains(&"Videos/P01/PRIVATE/M4ROOT/CLIP/C0001M01.XML".to_string()));
    assert!(files.contains(&"Other/P01/README.TXT".to_string()));
    assert!(files.contains(&"Other/P02/DATA/notes.xyz".to_string()));
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
        fs::read(nas_session.join("Videos/P01/PRIVATE/M4ROOT/CLIP/C0001.MP4")).unwrap(),
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
    assert!(!e.staging.join("sessions").join(&sid).exists());
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
    fs::remove_dir_all(e.media.join("cardA")).unwrap();
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
            .join(&sid)
            .join("Videos/P01/PRIVATE/M4ROOT/CLIP/C0001.MP4")
            .exists()
    );
    assert!(!shared.rt().nas.connected);
    shutdown(shared, token).await;

    // NAS が復帰した状態でデーモンを再起動 → 転送されて完了
    fs::rename(e.base.join("nas"), &offline_nas).unwrap();
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

    // 正常終了後、同じカードに新しい撮影データを追加して再挿入（AT-11）
    fs::rename(e.media.join("cardB"), e.base.join("cardB-out")).unwrap();
    wait_until("slot freed", Duration::from_secs(10), || {
        slot_phase(&shared, 0).is_none()
    })
    .await;
    write(
        &e.base.join("cardB-out/p1/DCIM/100MSDCF/DSC0002.JPG"),
        b"new shot",
    );
    // 同じパス・同じサイズのまま内容だけ上書き（AT-12: 偽スキップしない）
    write(&e.base.join("cardB-out/p1/README.TXT"), b"HELLO");
    tokio::time::sleep(Duration::from_millis(1100)).await; // 同一秒のフォルダ名を避ける必要はないが時刻を進める
    fs::rename(e.base.join("cardB-out"), e.media.join("cardB")).unwrap();
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
        skipped,
        expected - 1,
        "previously verified, unchanged files are referenced, not re-copied"
    );
    let mut copied_paths: Vec<_> = copied.iter().map(|f| f.source_rel_path.as_str()).collect();
    copied_paths.sort();
    assert_eq!(
        copied_paths,
        vec!["DCIM/100MSDCF/DSC0002.JPG", "README.TXT"]
    );
    let nas_files = find_files(&offline_nas.join(&s2.folder_name));
    assert_eq!(
        nas_files,
        vec![
            "Other/P01/README.TXT".to_string(),
            "Photos/P01/DCIM/100MSDCF/DSC0002.JPG".to_string(),
            "manifest.json".to_string()
        ]
    );
    assert_eq!(
        fs::read(
            offline_nas
                .join(&s2.folder_name)
                .join("Other/P01/README.TXT")
        )
        .unwrap(),
        b"HELLO"
    );
    let m = backup_core::manifest::Manifest::parse_and_validate(
        &fs::read(offline_nas.join(&s2.folder_name).join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        m.files.iter().filter(|f| f.reference.is_some()).count(),
        expected - 1
    );
    shutdown(shared, token).await;
}

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
    fs::rename(&card, e.media.join("cardC")).unwrap();

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
            .join(&sid)
            .join("Photos/P01/DCIM/100/OK.JPG")
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
    fs::rename(&card, e.media.join("cardD")).unwrap();
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
    fs::rename(&card, e.media.join("cardE")).unwrap();
    wait_until("copying", Duration::from_secs(20), || {
        shared.rt().slots[0]
            .as_ref()
            .is_some_and(|s| s.files_done >= 2)
    })
    .await;
    let sid = session_of_slot(&shared, 0).unwrap();
    let folder = shared.db().get_session(&sid).unwrap().unwrap().folder_name;
    fs::rename(e.media.join("cardE"), e.base.join("cardE-out")).unwrap();
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

    fs::rename(e.base.join("cardE-out"), e.media.join("cardE")).unwrap();
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
        fs::rename(&card, e.media.join(name)).unwrap();
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
        fs::rename(&card, e.media.join(name)).unwrap();
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
