//! GUI へ配信する状態スナップショットと定期メンテナンス。

use crate::state::Shared;
use backup_ipc::{CapacityView, NasView, SlotDetails, SlotView, Snapshot, WaitingView};
use backup_storage::capacity::fs_space;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn build(shared: &Shared) -> Snapshot {
    let cfg = shared.config();
    let stats = shared.db().nas_queue_stats().unwrap_or_default();
    let sessions_pending = shared
        .db()
        .sessions_in_states(&[
            backup_core::state::SessionState::NasPending,
            backup_core::state::SessionState::NasVerified,
            backup_core::state::SessionState::StagingCleanup,
        ])
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    let rt = shared.rt();
    let now = Instant::now();
    let slots = rt
        .slots
        .iter()
        .enumerate()
        .map(|(i, s)| match s {
            None => SlotView {
                slot_index: i,
                message: backup_ipc::SlotPhase::Empty.label().into(),
                ..Default::default()
            },
            Some(s) => {
                let remaining = s.bytes_total.saturating_sub(s.bytes_done);
                let copying = matches!(s.phase, backup_ipc::SlotPhase::Copying);
                SlotView {
                    slot_index: i,
                    phase: s.phase,
                    media_name: Some(s.media.display_name()),
                    message: s.message.clone(),
                    session_id: s.session_id.clone(),
                    folder_name: s.folder_name.clone(),
                    bytes_done: s.bytes_done,
                    bytes_total: s.bytes_total,
                    files_done: s.files_done,
                    files_total: s.files_total,
                    files_failed: s.files_failed,
                    files_skipped: s.files_skipped,
                    rate_bytes_per_sec: if copying { s.rate.rate() } else { None },
                    eta_secs: if copying {
                        s.rate.eta(remaining).map(|d| d.as_secs())
                    } else {
                        None
                    },
                    removable: s.phase.removable(),
                    details: SlotDetails {
                        device_path: s.media.partitions.first().map(|p| p.device_path.clone()),
                        identity_confidence: s.confidence.clone(),
                        inserted_at_utc: Some(s.inserted_at_utc.clone()),
                        partitions: s
                            .media
                            .partitions
                            .iter()
                            .map(|p| {
                                format!(
                                    "{} {} {} {}",
                                    backup_core::partition_label(p.index),
                                    p.device_path,
                                    p.fs_type.clone().unwrap_or_default(),
                                    p.fs_label.clone().unwrap_or_default()
                                )
                            })
                            .collect(),
                        current_file: s.current_file.clone(),
                        failed_files: s.failed_files.iter().take(100).cloned().collect(),
                        warnings: s.warnings.iter().take(50).cloned().collect(),
                    },
                }
            }
        })
        .collect();
    let waiting = rt
        .waiting
        .iter()
        .map(|w| WaitingView {
            media_name: w.media.display_name(),
            reason: format!(
                "同時処理は最大 {} 台のため待機中（{} 秒）",
                rt.slots.len(),
                now.duration_since(w.since).as_secs()
            ),
        })
        .collect();
    let configured = cfg.nas.mode != backup_config::NasMode::Disabled
        || shared.overrides.nas_local_dir.is_some();
    let nas = NasView {
        cancelled_files: stats.cancelled_files,
        configured,
        connected: rt.nas.connected,
        target: match &shared.overrides.nas_local_dir {
            Some(d) => format!("開発用ディレクトリ {}", d.display()),
            None => cfg.nas.display_target(),
        },
        status_text: if stats.cancelled_files > 0 && stats.queued_files == 0 {
            format!("中止した転送 {} 件／PC に保持中", stats.cancelled_files)
        } else {
            rt.nas.status_text.clone()
        },
        free_bytes: rt.nas.space.map(|s| s.free_bytes),
        total_bytes: rt.nas.space.map(|s| s.total_bytes),
        current_file: rt.nas.current_file.clone(),
        current_bytes_done: rt.nas.current_done,
        current_bytes_total: rt.nas.current_total,
        rate_bytes_per_sec: if rt.nas.current_file.is_some() {
            rt.nas.rate.as_ref().and_then(|r| r.rate())
        } else {
            None
        },
        queued_files: stats.queued_files,
        queued_bytes: stats.queued_bytes,
        retry_wait_files: stats.retry_wait_files,
        uploading_files: stats.uploading_files,
        sessions_pending,
        attempts: rt.nas.attempts.max(stats.max_attempts),
        next_retry_utc: rt.nas.next_retry_utc.clone().or(stats.next_attempt_utc),
        last_error: rt.nas.last_error.clone().or(stats.last_error),
        needs_attention: rt.nas.needs_attention,
    };
    Snapshot {
        generated_at_utc: backup_db::now_utc(),
        daemon_version: backup_core::APP_VERSION.into(),
        paused: shared.is_paused(),
        slots,
        waiting,
        nas,
        ssd: CapacityView {
            path: backup_config::display_path(shared.staging.root()),
            free_bytes: rt.ssd_space.map(|s| s.free_bytes),
            total_bytes: rt.ssd_space.map(|s| s.total_bytes),
            reserve_bytes: cfg.reserve_free_bytes,
        },
        warnings: rt.warnings.iter().map(|(_, m)| m.clone()).collect(),
        notification_seq: rt.notification_seq,
        last_notification: rt.last_notification.clone(),
    }
}

/// スナップショットを 0.5 秒ごとに更新し、容量を 5 秒ごとに取得する。
pub async fn run_publisher(shared: Arc<Shared>) {
    let mut last_capacity = Instant::now() - Duration::from_secs(60);
    loop {
        if last_capacity.elapsed() >= Duration::from_secs(5) {
            let space = match shared.overrides.fake_ssd_free_bytes {
                Some(free) => Some(backup_storage::capacity::FsSpace {
                    free_bytes: free,
                    total_bytes: 256_000_000_000,
                }),
                None => fs_space(shared.staging.root()).ok(),
            };
            shared.rt().ssd_space = space;
            last_capacity = Instant::now();
        }
        let snap = build(&shared);
        shared.snapshot_tx.send_replace(Arc::new(snap));
        tokio::select! {
            _ = shared.shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

/// 1 日 1 回: 30 日より古い履歴・ログを削除（catalog・セッション状態は保持）。
pub async fn run_maintenance(shared: Arc<Shared>) {
    loop {
        let days = shared.config().log_retention_days.max(1) as i64;
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        match shared
            .db()
            .purge_events_before(&backup_db::to_db_time(cutoff))
        {
            Ok(n) if n > 0 => tracing::info!("purged {n} old events"),
            Ok(_) => {}
            Err(e) => tracing::warn!("event purge failed: {e}"),
        }
        crate::telemetry::purge_old_logs(&shared.paths.log_dir, days as u64);
        tokio::select! {
            _ = shared.shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(24 * 3600)) => {}
        }
    }
}
