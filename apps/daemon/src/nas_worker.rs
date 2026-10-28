//! NAS 転送ワーカー（3.4）とセッションの完了処理（9.4）。
//!
//! - SSD で検証済みのファイルを順に NAS へ転送し、読み戻しハッシュで検証してから確定
//! - 接続断・容量不足・認証失敗は指数バックオフ + ジッタで無期限に再試行（SSD の原本は保持）
//! - セッションの全ファイル + manifest.json が NAS で検証済みになったときだけ SSD を削除

use crate::nas_backend;
use crate::state::Shared;
use backup_core::manifest::{
    MANIFEST_FILE_NAME, MANIFEST_SCHEMA_VERSION, Manifest, ManifestDevice, ManifestFile,
    ManifestFileStatus, ManifestPartition, ManifestReference,
};
use backup_core::policy::{
    DeletionGuardInput, FileGuardInfo, RateEstimator, backoff_delay, check_staging_deletion,
};
use backup_core::state::{NasFileState, SessionNasState, SessionState, StageState};
use backup_db::{SessionRow, TransferJob, now_utc, to_db_time};
use backup_ipc::NotificationKind;
use backup_storage::Control;
use backup_storage::nas::{NasError, NasRoot};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn jitter() -> f64 {
    (uuid::Uuid::new_v4().as_u128() % 10_000) as f64 / 10_000.0
}

pub async fn run(shared: Arc<Shared>) {
    let mut conn_attempts: u32 = 0;
    loop {
        if shared.shutdown.is_cancelled() {
            return;
        }
        if shared.is_paused() {
            set_status(&shared, |n| n.status_text = "一時停止中".into());
            if wait(&shared, Duration::from_secs(1)).await {
                return;
            }
            continue;
        }
        let cfg = shared.config();
        if cfg.nas.mode == backup_config::NasMode::Disabled
            && shared.overrides.nas_local_dir.is_none()
        {
            set_status(&shared, |n| {
                n.connected = false;
                n.status_text = "NAS 未設定（SSD に保持中）".into();
                n.space = None;
            });
            if wait(&shared, Duration::from_secs(5)).await {
                return;
            }
            continue;
        }

        // 接続（識別ファイル照合を含む）
        let nas = match nas_backend::connect(&shared, &cfg, true).await {
            Ok(n) => {
                if conn_attempts > 0 {
                    shared.event(None, "info", "nas_connected", "NAS に接続しました");
                }
                conn_attempts = 0;
                let space = n.space();
                set_status(&shared, |s| {
                    s.connected = true;
                    s.status_text = "接続中".into();
                    s.space = space;
                    s.attempts = 0;
                    s.next_retry_utc = None;
                    s.needs_attention = false;
                    s.last_error = None;
                });
                n
            }
            Err(e) => {
                conn_attempts = conn_attempts.saturating_add(1);
                let delay = backoff_delay(
                    conn_attempts,
                    Duration::from_secs(cfg.transfer.backoff_base_secs),
                    Duration::from_secs(cfg.transfer.backoff_max_secs),
                    jitter(),
                );
                let next =
                    chrono::Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default();
                if conn_attempts == 1 || conn_attempts.is_multiple_of(20) {
                    shared.event(
                        None,
                        "warn",
                        "nas_offline",
                        &format!("NAS に接続できません: {e}"),
                    );
                }
                set_status(&shared, |s| {
                    s.connected = false;
                    s.status_text = "NAS 転送待機／SSD 保持中".into();
                    s.space = None;
                    s.attempts = conn_attempts;
                    s.next_retry_utc = Some(to_db_time(next));
                    s.last_error = Some(e.to_string());
                    s.needs_attention = e.needs_attention();
                });
                if wait(&shared, delay).await {
                    return;
                }
                continue;
            }
        };

        // 転送
        let job = shared.db().next_due_transfer(&now_utc());
        match job {
            Ok(Some(job)) => {
                transfer_one(&shared, &nas, &cfg, job).await;
                continue;
            }
            Ok(None) => {}
            Err(e) => tracing::error!("transfer query failed: {e}"),
        }

        // 転送待ちがなければセッションの完了処理
        if let Err(e) = finalize_sessions(&shared, &nas, &cfg).await {
            tracing::error!("session finalization failed: {e:#}");
        }
        if wait(&shared, Duration::from_secs(3)).await {
            return;
        }
    }
}

/// 起こされるかタイムアウトまで待つ。シャットダウンなら true。
async fn wait(shared: &Shared, d: Duration) -> bool {
    tokio::select! {
        _ = shared.shutdown.cancelled() => true,
        _ = shared.nas_wakeup.notified() => false,
        _ = tokio::time::sleep(d) => false,
    }
}

fn set_status(shared: &Shared, f: impl FnOnce(&mut crate::state::NasRuntime)) {
    f(&mut shared.rt().nas);
}

async fn transfer_one(
    shared: &Arc<Shared>,
    nas: &NasRoot,
    cfg: &backup_config::Config,
    job: TransferJob,
) {
    let local = match shared
        .staging
        .final_path(&job.session_id, &job.destination_rel_path)
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("invalid staging path: {e}");
            return;
        }
    };
    // NAS 空き容量（取得できる場合のみ事前確認）
    if let Some(space) = nas.space()
        && space.free_bytes < job.size_bytes.saturating_add(1_000_000_000)
    {
        retry_later(shared, cfg, &job, &NasError::NoSpace);
        return;
    }
    let temp_note = format!("{}/{}", job.folder_name, job.destination_rel_path);
    if let Err(e) = shared.db().transfer_started(&job, &temp_note) {
        tracing::error!("db: {e}");
        return;
    }
    set_status(shared, |s| {
        s.current_file = Some(job.destination_rel_path.clone());
        s.current_done = 0;
        s.current_total = job.size_bytes;
        s.status_text = "転送中".into();
        if s.rate.is_none() {
            s.rate = Some(RateEstimator::new(Duration::from_secs(15)));
        }
    });

    let result = {
        let shared2 = shared.clone();
        let nas2 = nas.clone();
        let job2 = job.clone();
        tokio::task::spawn_blocking(move || {
            let mut done = 0u64;
            nas2.upload_verified(
                &local,
                &job2.folder_name,
                &job2.destination_rel_path,
                &job2.blake3_hash,
                &mut |n| {
                    done += n;
                    if shared2.shutdown.is_cancelled() || shared2.is_paused() {
                        return Control::Abort;
                    }
                    let mut rt = shared2.rt();
                    rt.nas.current_done = done;
                    rt.nas.cumulative_bytes += n;
                    let total = rt.nas.cumulative_bytes;
                    if let Some(r) = rt.nas.rate.as_mut() {
                        r.record(Instant::now(), total);
                    }
                    Control::Continue
                },
            )
        })
        .await
    };
    set_status(shared, |s| {
        s.current_file = None;
        s.current_done = 0;
        s.current_total = 0;
    });

    match result {
        Ok(Ok(outcome)) => {
            let _ = shared.db().transfer_verifying(&job);
            if let Err(e) = shared.db().transfer_verified(&job, nas.display_uri()) {
                tracing::error!("db: {e}");
                return;
            }
            tracing::debug!(?outcome, file = job.destination_rel_path, "NAS verified");
            update_session_nas_state(shared, &job.session_id);
        }
        Ok(Err(NasError::Aborted)) => {
            // 一時停止・停止による中断は失敗に数えない
            let _ = shared.db().requeue_file_transfer(&job.file_id, "中断");
        }
        Ok(Err(e @ (NasError::StagingCorrupt { .. } | NasError::StagingMissing(_)))) => {
            // SSD 上の実体が記録と一致しない → NAS へは送らず、再取り込みが必要
            let msg = format!("{}: {e}", job.destination_rel_path);
            shared.event(Some(&job.session_id), "error", "staging_corrupt", &msg);
            let _ = shared
                .db()
                .reset_file_to_discovered(&job.file_id, &e.to_string());
            mark_needs_attention(shared, &job.session_id);
        }
        Ok(Err(e)) => retry_later(shared, cfg, &job, &e),
        Err(join) => retry_later(shared, cfg, &job, &NasError::Io(join.to_string())),
    }
}

fn retry_later(shared: &Shared, cfg: &backup_config::Config, job: &TransferJob, e: &NasError) {
    let attempts = job.attempts + 1;
    let delay = backoff_delay(
        attempts,
        Duration::from_secs(cfg.transfer.backoff_base_secs),
        Duration::from_secs(cfg.transfer.backoff_max_secs),
        jitter(),
    );
    let next = chrono::Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default();
    let _ = shared
        .db()
        .transfer_retry(job, &e.to_string(), &to_db_time(next));
    if attempts == 1 || attempts.is_multiple_of(10) {
        shared.event(
            Some(&job.session_id),
            "warn",
            "nas_retry",
            &format!(
                "{} の NAS 転送を再試行します（{attempts} 回目）: {e}",
                job.destination_rel_path
            ),
        );
    }
    set_status(shared, |s| {
        s.last_error = Some(e.to_string());
        s.needs_attention = e.needs_attention();
        if matches!(e, NasError::Offline(_)) {
            s.connected = false;
        }
        if matches!(e, NasError::NoSpace) {
            s.status_text = "NAS 容量不足のため待機中／SSD 保持中".into();
        }
    });
}

fn mark_needs_attention(shared: &Shared, session_id: &str) {
    let db = shared.db();
    if let Ok(Some(s)) = db.get_session(session_id) {
        if s.state == SessionState::NasPending {
            let _ = db.transition_session(session_id, SessionState::NeedsAttention);
        }
        let _ = db.set_session_error(
            session_id,
            Some("SSD 上のファイルに問題があります。同じメディアを再接続してください"),
        );
    }
}

fn update_session_nas_state(shared: &Shared, session_id: &str) {
    let db = shared.db();
    if db.refresh_session_counters(session_id).is_ok() {
        let pending = db.pending_transfer_count(session_id).unwrap_or(0);
        let state = if pending == 0 {
            SessionNasState::Verified
        } else {
            SessionNasState::InProgress
        };
        let _ = db.set_session_nas_state(session_id, state);
    }
}

/// NAS 待ちのセッションを確認し、条件が揃えばマニフェスト確定 → SSD 削除まで進める。
async fn finalize_sessions(
    shared: &Arc<Shared>,
    nas: &NasRoot,
    cfg: &backup_config::Config,
) -> anyhow::Result<()> {
    let sessions = shared.db().sessions_in_states(&[
        SessionState::NasPending,
        SessionState::NasVerified,
        SessionState::StagingCleanup,
    ])?;
    for s in sessions {
        if shared.shutdown.is_cancelled() || shared.is_paused() {
            return Ok(());
        }
        if let Err(e) = finalize_session(shared, nas, cfg, &s).await {
            shared.event(
                Some(&s.session_id),
                "warn",
                "finalize_deferred",
                &format!("完了処理を保留しました: {e:#}"),
            );
        }
    }
    Ok(())
}

async fn finalize_session(
    shared: &Arc<Shared>,
    nas: &NasRoot,
    cfg: &backup_config::Config,
    s: &SessionRow,
) -> anyhow::Result<()> {
    let sid = &s.session_id;
    let files = shared.db().files_for_session(sid)?;
    let pending = shared.db().pending_transfer_count(sid)?;
    if backup_core::session::is_card_folder(&s.folder_name) {
        let partitions = shared.db().partition_details(&s.device_id)?;
        if partitions.len() > 1 {
            for p in partitions {
                nas.ensure_directory(&s.folder_name, &backup_core::partition_label(p.0))?;
            }
        }
    }
    // 再起動後の完了処理でも空フォルダを含めて存在を確認する。
    for (_, _, dest) in shared.db().directories(sid)? {
        nas.ensure_directory(&s.folder_name, &dest)?;
    }

    if s.state == SessionState::NasPending {
        // すべてのファイルが SSD 検証済み + NAS 検証済み（またはスキップ参照検証済み）か
        let all_ok = s.scan_completed
            && pending == 0
            && files.iter().all(|f| match f.stage_state {
                StageState::SsdVerified => f.nas_state == NasFileState::NasVerified,
                StageState::SkippedVerified => f.reference_verified,
                _ => false,
            });
        if !all_ok {
            return Ok(());
        }

        // manifest.json を作成 → SSD に確定 → NAS へ転送・検証
        let manifest = build_manifest(shared, s, &files)?;
        let bytes = manifest.to_json_pretty();
        let local = shared.staging.manifest_path(sid)?;
        write_atomic(&local, &bytes)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let nas2 = nas.clone();
        let folder = s.folder_name.clone();
        let local2 = local.clone();
        let h2 = hash.clone();
        tokio::task::spawn_blocking(move || {
            nas2.upload_verified(
                &local2,
                &folder,
                MANIFEST_FILE_NAME,
                &h2,
                &mut backup_storage::no_progress(),
            )
        })
        .await??;
        {
            let db = shared.db();
            db.mark_manifest_verified(sid, true)?;
            db.transition_session(sid, SessionState::NasVerified)?;
            db.set_session_nas_state(sid, SessionNasState::Verified)?;
        }
        shared.event(
            Some(sid),
            "info",
            "manifest_verified",
            "manifest.json を NAS に保存・検証しました",
        );
    }

    let s = shared
        .db()
        .get_session(sid)?
        .ok_or_else(|| anyhow::anyhow!("session vanished"))?;
    if s.state == SessionState::NasVerified {
        // 削除前の最終確認: NAS 上の全実体（参照含む）を再検証
        let files = shared.db().files_for_session(sid)?;
        let mut bad = Vec::new();
        for f in &files {
            if shared.shutdown.is_cancelled() {
                return Ok(());
            }
            let Some(hash) = f.blake3_hash.clone() else {
                bad.push((f.file_id.clone(), false));
                continue;
            };
            let (folder, rel) = match f.stage_state {
                StageState::SsdVerified => (s.folder_name.clone(), f.destination_rel_path.clone()),
                StageState::SkippedVerified => {
                    let r: ManifestReference =
                        serde_json::from_str(f.reference_json.as_deref().unwrap_or("{}"))?;
                    (r.nas_session_folder, r.dest_rel_path)
                }
                _ => {
                    bad.push((f.file_id.clone(), false));
                    continue;
                }
            };
            let nas2 = nas.clone();
            let full = cfg.nas.final_rehash_before_cleanup;
            let ok = tokio::task::spawn_blocking(move || -> Result<bool, NasError> {
                if full {
                    nas2.verify_existing(&folder, &rel, &hash, &mut backup_storage::no_progress())
                } else {
                    let p = nas2.root().join(&folder).join(&rel);
                    Ok(p.is_file())
                }
            })
            .await??;
            if !ok {
                bad.push((f.file_id.clone(), f.stage_state == StageState::SsdVerified));
            }
        }
        if !bad.is_empty() {
            let db = shared.db();
            for (file_id, can_resend) in &bad {
                if *can_resend {
                    db.requeue_file_transfer(
                        file_id,
                        "最終確認で NAS 上の内容が一致しませんでした",
                    )?;
                } else {
                    db.set_reference_verified(file_id, false)?;
                }
            }
            db.transition_session(sid, SessionState::NasPending)?;
            if bad.iter().any(|(_, r)| !r) {
                db.transition_session(sid, SessionState::NeedsAttention)?;
            }
            drop(db);
            shared.event(
                Some(sid),
                "error",
                "nas_verify_mismatch",
                &format!("NAS 上の {} 件のファイルが検証に失敗したため再転送します（SSD は削除しません）", bad.len()),
            );
            shared.nas_wakeup.notify_one();
            return Ok(());
        }
        nas.cleanup_temp_files(&s.folder_name);
        shared
            .db()
            .transition_session(sid, SessionState::StagingCleanup)?;
    }

    let s = shared
        .db()
        .get_session(sid)?
        .ok_or_else(|| anyhow::anyhow!("session vanished"))?;
    if s.state == SessionState::StagingCleanup {
        // 9.4 のガード条件をすべて確認してから削除
        let files = shared.db().files_for_session(sid)?;
        let session_dir = shared.staging.session_dir(sid)?;
        let marker_ok = (cfg.nas.target_id.is_empty() && shared.overrides.nas_local_dir.is_some())
            || nas.verify_marker(&cfg.nas.target_id).is_ok();
        let input = DeletionGuardInput {
            session_state: s.state,
            scan_completed: s.scan_completed,
            files: files
                .iter()
                .map(|f| FileGuardInfo {
                    stage_state: f.stage_state,
                    nas_state: f.nas_state,
                    reference_verified: f.reference_verified,
                })
                .collect(),
            manifest_verified_on_nas: s.manifest_verified_at_utc.is_some(),
            nas_target_verified: marker_ok,
            pending_transfers: shared.db().pending_transfer_count(sid)? as usize,
            open_handles: backup_storage::local::open_handles_under(&session_dir),
            path_inside_staging_root: session_dir.starts_with(shared.staging.root()),
        };
        if let Err(violations) = check_staging_deletion(&input) {
            anyhow::bail!("削除条件を満たしていません: {violations:?}");
        }
        let staging = shared.staging.clone();
        let sid2 = sid.clone();
        tokio::task::spawn_blocking(move || staging.remove_session(&sid2)).await??;
        {
            let db = shared.db();
            db.mark_staging_deleted(sid)?;
            db.transition_session(sid, SessionState::Complete)?;
        }
        let msg = format!(
            "{} の NAS バックアップが完了し、SSD の一時データを削除しました",
            s.folder_name
        );
        shared.event(Some(sid), "info", "session_complete", &msg);
        shared.notify(
            NotificationKind::NasComplete,
            format!("NAS バックアップ完了: {}", s.folder_name),
        );
        shared.capacity_wakeup.notify_waiters();
    }
    Ok(())
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    backup_storage::platform::sync_dir(dir)?;
    Ok(())
}

pub fn build_manifest(
    shared: &Shared,
    s: &SessionRow,
    files: &[backup_db::FileRow],
) -> anyhow::Result<Manifest> {
    let db = shared.db();
    let (identity_hash, vendor, model, serial, bus, size) = db.device_info(&s.device_id)?;
    let partitions = db.partition_details(&s.device_id)?;
    drop(db);
    Ok(Manifest {
        directories: shared
            .db()
            .directories(&s.session_id)?
            .into_iter()
            .map(|(partition_index, source_rel_path, dest_rel_path)| {
                backup_core::manifest::ManifestDirectory {
                    partition_index,
                    source_rel_path,
                    dest_rel_path,
                }
            })
            .collect(),
        schema_version: MANIFEST_SCHEMA_VERSION,
        app_version: backup_core::APP_VERSION.into(),
        session_id: s.session_id.clone(),
        folder_name: s.folder_name.clone(),
        created_at_utc: s.started_at_utc.clone(),
        local_timezone: s.local_timezone.clone(),
        device: ManifestDevice {
            identity_hash,
            identity_confidence: s.identity_confidence.as_str().into(),
            vendor,
            model,
            serial,
            bus,
            size_bytes: size.unwrap_or(0) as u64,
        },
        partitions: partitions
            .into_iter()
            .map(|p| ManifestPartition {
                index: p.0,
                label: backup_core::partition_label(p.0),
                fs_type: p.1,
                fs_uuid: p.2,
                part_guid: p.3,
                fs_label: p.4,
                size_bytes: p.5,
                fingerprint: p.6,
            })
            .collect(),
        files: files
            .iter()
            .map(|f| ManifestFile {
                file_id: f.file_id.clone(),
                partition_index: f.partition_index,
                source_rel_path: f.source_rel_path.clone(),
                source_rel_path_raw_hex: f.source_rel_path_raw_hex.clone(),
                dest_rel_path: f.destination_rel_path.clone(),
                kind: f.media_kind,
                size_bytes: f.size_bytes,
                mtime_ns: f.source_mtime_ns,
                blake3: f.blake3_hash.clone(),
                group: f.group_key.clone(),
                status: match f.stage_state {
                    StageState::SsdVerified => ManifestFileStatus::Copied,
                    StageState::SkippedVerified => ManifestFileStatus::SkippedVerified,
                    _ => ManifestFileStatus::Failed,
                },
                reference: f
                    .reference_json
                    .as_deref()
                    .and_then(|j| serde_json::from_str(j).ok()),
                error: f.last_error.clone(),
            })
            .collect(),
    })
}

/// 「今すぐ再試行」。
pub fn retry_now(shared: &Shared) -> usize {
    let n = shared.db().retry_all_transfers_now().unwrap_or(0);
    shared.nas_wakeup.notify_one();
    n
}
