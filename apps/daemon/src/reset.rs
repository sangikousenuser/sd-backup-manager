//! 明示確認された PC 内データの一括削除。SD・NAS には書き込まない。

use crate::state::{NasRuntime, Shared};
use backup_core::state::SessionState;
use backup_ipc::{LocalDataSummary, Response};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub async fn preview(shared: &Arc<Shared>) -> Result<Response, String> {
    let shared2 = shared.clone();
    let mut summary = tokio::task::spawn_blocking(move || {
        let (counts, sessions, pending) = {
            let db = shared2.db();
            (
                db.local_data_counts()?,
                db.sessions_in_states(SessionState::ALL)?,
                db.local_reset_pending()?,
            )
        };
        let local_bytes = sessions
            .iter()
            .map(|s| shared2.staging.session_bytes(&s.session_id))
            .sum();
        Ok::<_, backup_db::DbError>(LocalDataSummary {
            sessions: counts.sessions,
            files: counts.files,
            transfers: counts.transfers,
            catalog_entries: counts.catalog_entries,
            devices: counts.devices,
            events: counts.events,
            local_bytes,
            reset_pending: pending,
            confirmation_token: String::new(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let token = uuid::Uuid::new_v4().to_string();
    *shared
        .local_reset_confirmation
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some((token.clone(), Instant::now()));
    summary.confirmation_token = token;
    Ok(Response::LocalDataSummary { summary })
}

pub async fn execute(shared: &Arc<Shared>, token: String) -> Result<Response, String> {
    let _maintenance = tokio::time::timeout(Duration::from_secs(10), shared.maintenance.write())
        .await
        .map_err(|_| {
            "他の操作の終了待ちです。少し待って一括削除を再実行してください".to_string()
        })?;
    let confirmed = shared
        .local_reset_confirmation
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if !confirmed.is_some_and(|(expected, created)| {
        expected == token && created.elapsed() < Duration::from_secs(300)
    }) {
        return Err("削除対象をもう一度確認してください（確認の有効期限は5分です）".into());
    }
    let was_pending = shared.local_reset_pending.swap(true, Ordering::SeqCst);
    {
        // on_added のフラグ検査と同じロックの中で、書き込み中の SD がないことを確認する。
        let rt = shared.rt();
        if rt.slots.iter().any(Option::is_some) || !rt.waiting.is_empty() {
            shared
                .local_reset_pending
                .store(was_pending, Ordering::SeqCst);
            return Err("SD・USBメディアをすべて取り外し、取り込み処理が終了してから一括削除してください（まだ削除していません）".into());
        }
    }
    let was_paused = shared.paused.swap(true, Ordering::SeqCst);
    shared.nas_wakeup.notify_one();
    let operation =
        tokio::time::timeout(Duration::from_secs(10), shared.nas_operation.lock()).await;
    let _operation = match operation {
        Ok(operation) => operation,
        Err(_) => {
            shared
                .local_reset_pending
                .store(was_pending, Ordering::SeqCst);
            shared.paused.store(was_paused, Ordering::SeqCst);
            return Err("NAS転送を停止できなかったため削除していません。NASの応答を確認して再実行してください".into());
        }
    };
    let _source = shared.media_session_operation.lock().await;
    if let Err(e) = shared.db().begin_local_reset() {
        shared
            .local_reset_pending
            .store(was_pending, Ordering::SeqCst);
        shared.paused.store(was_paused, Ordering::SeqCst);
        return Err(e.to_string());
    }
    let shared2 = shared.clone();
    tokio::task::spawn_blocking(move || finish(&shared2))
        .await
        .map_err(|e| e.to_string())??;
    shared.capacity_wakeup.notify_waiters();
    Ok(Response::Ok { message: "PC内の一時コピー・転送キュー・送信履歴・識別キャッシュ・過去の診断ログをすべて削除しました。接続設定とパスワードは保持しています。処理は一時停止中です。再開するにはメイン画面で「再開」を押し、SDを挿し直してください".into() })
}

/// 通常処理が停止した状態で呼ぶ。DB の記録を残し、途中失敗・再起動時に再実行する。
pub fn finish(shared: &Shared) -> Result<(), String> {
    if !shared
        .db()
        .local_reset_pending()
        .map_err(|e| e.to_string())?
    {
        return Err("一括削除が開始されていません".into());
    }
    let sessions = shared
        .db()
        .sessions_in_states(SessionState::ALL)
        .map_err(|e| e.to_string())?;
    for session in sessions {
        shared
            .staging
            .register_session(&session.session_id, &session.folder_name)
            .map_err(|e| e.to_string())?;
        shared.staging.remove_session(&session.session_id).map_err(|e| format!(
            "一括削除を保留しました: {e}。転送は停止しています。ファイルを使用中のアプリを閉じて再実行してください。次回起動時も削除を再開します"
        ))?;
    }
    crate::telemetry::clear_logs(&shared.paths.log_dir).map_err(|e| {
        format!(
            "過去のログを削除できません: {e}。一括削除は未完了のため、設定から再実行してください"
        )
    })?;
    // 全実体が消えるまでは履歴・復旧情報を消さない。
    shared
        .db()
        .finish_local_reset()
        .map_err(|e| e.to_string())?;
    shared.staging.forget_sessions();
    {
        let mut rt = shared.rt();
        rt.nas = NasRuntime::default();
        rt.nas.status_text = "一括削除完了・一時停止中".into();
        rt.warnings.clear();
        rt.last_notification = None;
        rt.connected_identities.clear();
    }
    shared.local_reset_pending.store(false, Ordering::SeqCst);
    Ok(())
}
