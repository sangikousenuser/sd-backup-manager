//! 起動時の復旧（9.3）。
//!
//! 1. DB の不確定状態（コピー中・転送中）を安全側に戻す
//! 2. SSD の `.part` 残骸を破棄（再開情報がないため先頭から再コピー）
//! 3. SSD_VERIFIED の確定ファイルが実在しサイズが一致するか確認。失われていれば未取り込みに戻す
//!    （内容のハッシュは NAS 転送時に SSD 側も再計算して記録と照合する）
//! 4. アンマウント失敗のまま停止したセッションは、メディアが再検出されなければ NAS 処理へ進める

use crate::state::Shared;
use backup_core::state::{SessionState, StageState};
use std::sync::Arc;
use std::time::Duration;

pub fn recover(shared: &Shared) -> anyhow::Result<()> {
    if shared.db().local_reset_pending()? {
        shared
            .local_reset_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        shared
            .paused
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Err(e) = crate::reset::finish(shared) {
            shared.warn(format!(
                "一括削除は未完了です。設定から再実行してください: {e}"
            ));
        }
        return Ok(());
    }
    let report = shared.db().recover_after_restart()?;
    for s in shared.db().sessions_in_states(SessionState::ALL)? {
        shared
            .staging
            .register_session(&s.session_id, &s.folder_name)?;
    }
    let partials = shared.staging.clear_partials()?;

    let mut missing = 0usize;
    let sessions = {
        let db = shared.db();
        let states: Vec<SessionState> = SessionState::ALL
            .iter()
            .copied()
            .filter(|s| *s != SessionState::Complete)
            .collect();
        db.sessions_in_states(&states)?
    };
    for s in &sessions {
        if s.staging_deleted_at_utc.is_some() {
            continue;
        }
        let files = shared.db().files_for_session(&s.session_id)?;
        let deleting: Vec<_> = files
            .iter()
            .filter(|f| f.stage_state == StageState::DeletePending)
            .collect();
        for file in &deleting {
            if let Err(e) = crate::file_actions::finish_deletion(shared, file) {
                shared.event(Some(&s.session_id), "warn", "file_delete_pending", &e);
            }
        }
        if !deleting.is_empty()
            && let Err(e) = crate::file_actions::refresh_manifest(shared, &s.session_id)
        {
            shared.event(
                Some(&s.session_id),
                "warn",
                "manifest_update_failed",
                &e.to_string(),
            );
        }
        for f in files
            .iter()
            .filter(|f| f.stage_state == StageState::SsdVerified)
        {
            let path = shared
                .staging
                .final_path(&s.session_id, &f.destination_rel_path)?;
            let ok = std::fs::symlink_metadata(&path)
                .map(|m| m.is_file() && m.len() == f.size_bytes)
                .unwrap_or(false);
            if !ok {
                missing += 1;
                shared.db().reset_file_to_discovered(
                    &f.file_id,
                    "SSD 上の確定ファイルが見つからないか、サイズが一致しません",
                )?;
            }
        }
        if missing > 0 && s.state.is_media_done() && s.state != SessionState::NeedsAttention {
            let db = shared.db();
            if db
                .transition_session(&s.session_id, SessionState::NeedsAttention)
                .is_err()
            {
                // NasVerified / StagingCleanup からは一度 NasPending に戻す
                let _ = db.transition_session(&s.session_id, SessionState::NasPending);
                let _ = db.transition_session(&s.session_id, SessionState::NeedsAttention);
            }
        }
        shared.db().refresh_session_counters(&s.session_id)?;
    }

    let msg = format!(
        "起動時復旧: 中断セッション {} 件、コピー中ファイル {} 件・転送中 {} 件を再試行待ちに戻し、未完了の一時ファイル {} 件を破棄、SSD 上で見つからないファイル {} 件",
        report.sessions_interrupted.len(),
        report.files_reset,
        report.transfers_reset,
        partials,
        missing
    );
    shared.event(
        None,
        if missing > 0 { "warn" } else { "info" },
        "recovery",
        &msg,
    );
    Ok(())
}

/// アンマウント失敗のまま停止したセッションの後始末（起動後しばらく待ってから）。
pub async fn settle_unmount_failed(shared: Arc<Shared>, after: Duration) {
    tokio::select! {
        _ = shared.shutdown.cancelled() => return,
        _ = tokio::time::sleep(after) => {}
    }
    let in_slots: Vec<String> = shared
        .rt()
        .slots
        .iter()
        .flatten()
        .filter_map(|s| s.session_id.clone())
        .collect();
    let sessions = match shared
        .db()
        .sessions_in_states(&[SessionState::UnmountFailed])
    {
        Ok(s) => s,
        Err(_) => return,
    };
    for s in sessions {
        if in_slots.contains(&s.session_id) {
            continue;
        }
        let next = if s.failed_files > 0 {
            SessionState::NeedsAttention
        } else {
            SessionState::NasPending
        };
        let _ = shared.db().transition_session(&s.session_id, next);
    }
    shared.nas_wakeup.notify_one();
}
