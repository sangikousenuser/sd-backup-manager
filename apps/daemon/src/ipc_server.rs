//! IPC サーバー（6.4）。同一 UID のクライアントだけを受け付ける。

use crate::state::Shared;
use crate::{nas_backend, nas_worker, orchestrator, telemetry};
use backup_config::{Config, NasMode, secrets};
use backup_ipc::transport::{Listener, Reader, Writer};
use backup_ipc::{
    ClientMessage, Command, EventSummary, FileSummary, NasTestReport, PROTOCOL_VERSION, Response,
    ServerMessage, SessionSummary, SettingsView, read_message, write_message,
};
use backup_storage::nas::NasRoot;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub async fn run(shared: Arc<Shared>, mut listener: Listener) {
    loop {
        let accepted = tokio::select! {
            _ = shared.shutdown.cancelled() => return,
            a = listener.accept() => a,
        };
        match accepted {
            Ok((reader, writer)) => {
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(shared, reader, writer).await {
                        tracing::debug!("ipc connection closed: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("ipc accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle_connection(
    shared: Arc<Shared>,
    mut reader: Reader,
    mut w: Writer,
) -> anyhow::Result<()> {
    match read_message::<ClientMessage, _>(&mut reader).await? {
        Some(ClientMessage::Hello {
            protocol_version, ..
        }) => {
            write_message(
                &mut w,
                &ServerMessage::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: backup_core::APP_VERSION.into(),
                },
            )
            .await?;
            if protocol_version != PROTOCOL_VERSION {
                return Ok(());
            }
        }
        _ => return Ok(()),
    }

    // 読み込みは専用タスクで行い（途中まで読んだ行を失わないため）、チャネル経由で受け取る
    let (tx, mut rx) = mpsc::channel::<ClientMessage>(16);
    let reader_task = tokio::spawn(async move {
        while let Ok(Some(msg)) = read_message::<ClientMessage, _>(&mut reader).await {
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut snapshots = shared.snapshot_tx.subscribe();
    let mut subscribed = false;
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                _ = shared.shutdown.cancelled() => return Ok(()),
                msg = rx.recv() => {
                    let Some(msg) = msg else { return Ok(()) };
                    let ClientMessage::Request { id, command } = msg else { continue };
                    if matches!(command, Command::Subscribe) {
                        subscribed = true;
                        let snap = snapshots.borrow_and_update().clone();
                        write_message(&mut w, &ServerMessage::Response { id, result: Ok(Response::Ok { message: "subscribed".into() }) }).await?;
                        write_message(&mut w, &ServerMessage::Snapshot { snapshot: Box::new((*snap).clone()) }).await?;
                        continue;
                    }
                    let result = handle_command(&shared, command).await;
                    write_message(&mut w, &ServerMessage::Response { id, result }).await?;
                }
                changed = snapshots.changed(), if subscribed => {
                    if changed.is_err() { return Ok(()); }
                    let snap = snapshots.borrow_and_update().clone();
                    write_message(&mut w, &ServerMessage::Snapshot { snapshot: Box::new((*snap).clone()) }).await?;
                }
            }
        }
    }
    .await;
    reader_task.abort();
    result
}

async fn handle_command(shared: &Arc<Shared>, command: Command) -> Result<Response, String> {
    match command {
        Command::GetState | Command::Subscribe => Ok(Response::State {
            snapshot: Box::new(crate::snapshot::build(shared)),
        }),
        Command::ListSessions { limit } => {
            let rows = shared
                .db()
                .list_sessions(limit.clamp(1, 1000))
                .map_err(|e| e.to_string())?;
            Ok(Response::Sessions {
                sessions: rows
                    .into_iter()
                    .map(|s| SessionSummary {
                        session_id: s.session_id,
                        folder_name: s.folder_name,
                        started_at_utc: s.started_at_utc,
                        state: s.state.to_string(),
                        nas_state: s.nas_state.to_string(),
                        total_files: s.total_files,
                        total_bytes: s.total_bytes,
                        completed_files: s.completed_files,
                        failed_files: s.failed_files,
                        nas_verified_files: s.nas_verified_files,
                        staging_deleted: s.staging_deleted_at_utc.is_some(),
                        last_error: s.last_error,
                    })
                    .collect(),
            })
        }
        Command::GetFiles {
            session_id,
            only_problems,
            offset,
            limit,
        } => {
            let page = shared
                .db()
                .files_page(&session_id, only_problems, offset, limit)
                .map_err(|e| e.to_string())?;
            Ok(Response::Files {
                total_files: page.total,
                files: page
                    .files
                    .into_iter()
                    .map(|(f, transfer)| FileSummary {
                        file_id: f.file_id,
                        source_rel_path: f.source_rel_path,
                        destination_rel_path: f.destination_rel_path,
                        kind: f.media_kind.as_str().into(),
                        size_bytes: f.size_bytes,
                        stage_state: f.stage_state.to_string(),
                        nas_state: f.nas_state.to_string(),
                        copy_attempts: f.copy_attempts,
                        nas_attempts: transfer.attempts,
                        next_retry_utc: transfer.next_attempt_utc,
                        error: transfer.last_error.or(f.last_error),
                    })
                    .collect(),
            })
        }
        Command::ManageNasFiles {
            session_id,
            file_ids,
            action,
        } => manage_nas_files(shared, session_id, file_ids, action).await,
        Command::ListEvents { days, limit } => {
            let since = backup_db::to_db_time(
                chrono::Utc::now() - chrono::Duration::days(days.clamp(1, 30) as i64),
            );
            let rows = shared
                .db()
                .recent_events(&since, limit.clamp(1, 10_000))
                .map_err(|e| e.to_string())?;
            Ok(Response::Events {
                events: rows
                    .into_iter()
                    .map(|e| EventSummary {
                        created_at_utc: e.created_at_utc,
                        level: e.level,
                        event_type: e.event_type,
                        message: e.message,
                        session_id: e.session_id,
                    })
                    .collect(),
            })
        }
        Command::RetryNas => {
            let n = nas_worker::retry_now(shared);
            shared.event(
                None,
                "info",
                "nas_retry_now",
                "NAS 転送の今すぐ再試行が要求されました",
            );
            Ok(Response::Ok {
                message: format!("{n} 件の転送を再試行します"),
            })
        }
        Command::RetryUnmount { slot } => orchestrator::retry_unmount(shared, slot)
            .await
            .map(|message| Response::Ok { message }),
        Command::Pause => {
            shared
                .paused
                .store(true, std::sync::atomic::Ordering::SeqCst);
            shared.event(None, "info", "paused", "処理を一時停止しました");
            Ok(Response::Ok {
                message: "一時停止しました".into(),
            })
        }
        Command::Resume => {
            shared
                .paused
                .store(false, std::sync::atomic::Ordering::SeqCst);
            shared.nas_wakeup.notify_one();
            shared.event(None, "info", "resumed", "処理を再開しました");
            Ok(Response::Ok {
                message: "再開しました".into(),
            })
        }
        Command::GetSettings => {
            let cfg = shared.config();
            let account = cfg.nas.secret_account();
            let stored = tokio::task::spawn_blocking(move || {
                secrets::load_password(&account).ok().flatten().is_some()
            })
            .await
            .unwrap_or(false);
            Ok(Response::Settings {
                settings: to_view(&cfg),
                password_stored: stored,
            })
        }
        Command::UpdateSettings { settings, password } => {
            update_settings(shared, settings, password).await
        }
        Command::TestNas { settings, password } => test_nas(shared, settings, password).await,
        Command::InitializeNasTarget => initialize_target(shared).await,
        Command::ExportLogs { dest_dir } => {
            let shared2 = shared.clone();
            let out = tokio::task::spawn_blocking(move || {
                telemetry::export_diagnostics(&shared2, &PathBuf::from(dest_dir))
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
            Ok(Response::Ok {
                message: format!("診断情報を書き出しました: {}", out.display()),
            })
        }
        Command::RestoreLayout {
            session_id,
            dest_dir,
        } => restore(shared, session_id, dest_dir).await,
        Command::DiscardSession { session_id } => discard_session(shared, session_id).await,
        Command::Shutdown => {
            shared.event(None, "info", "shutdown_requested", "停止が要求されました");
            let sd = shared.shutdown.clone();
            // 応答を返してから停止する
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                sd.cancel();
            });
            Ok(Response::Ok {
                message: "停止します".into(),
            })
        }
    }
}

struct NasFileIntents {
    shared: Arc<Shared>,
    ids: Vec<String>,
}

impl NasFileIntents {
    fn new(shared: &Arc<Shared>, ids: Vec<String>) -> Self {
        {
            let mut requested = shared
                .nas_cancel_files
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for id in &ids {
                *requested.entry(id.clone()).or_default() += 1;
            }
        }
        Self {
            shared: shared.clone(),
            ids,
        }
    }
}

impl Drop for NasFileIntents {
    fn drop(&mut self) {
        let mut requested = self
            .shared
            .nas_cancel_files
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for id in &self.ids {
            if let Some(count) = requested.get_mut(id) {
                *count -= 1;
                if *count == 0 {
                    requested.remove(id);
                }
            }
        }
        self.shared.nas_wakeup.notify_one();
    }
}

async fn manage_nas_files(
    shared: &Arc<Shared>,
    sid: String,
    ids: Vec<String>,
    action: backup_ipc::NasFileAction,
) -> Result<Response, String> {
    if ids.is_empty() || ids.len() > 1000 {
        return Err("ファイルは 1〜1000 件を選択してください".into());
    }
    {
        let db = shared.db();
        for id in &ids {
            let f = db
                .get_file(id)
                .map_err(|e| e.to_string())?
                .ok_or("ファイルが見つかりません")?;
            if f.stage_state == backup_core::state::StageState::Discarded {
                return Err("PC 内のコピーは削除済みです。NAS 再試行の対象にはできません".into());
            }
            if f.session_id != sid
                || (if action == backup_ipc::NasFileAction::DeleteLocal {
                    matches!(
                        f.stage_state,
                        backup_core::state::StageState::SkippedVerified
                            | backup_core::state::StageState::Discarded
                    )
                } else {
                    f.stage_state != backup_core::state::StageState::SsdVerified
                })
                || f.nas_state == backup_core::state::NasFileState::NasVerified
            {
                return Err("別のセッションや NAS 検証済みファイルは操作できません".into());
            }
        }
    }
    // 意図を先に登録し、転送中ならチャンク境界で安全に停止させる。
    let _intents = NasFileIntents::new(shared, ids.clone());
    let _operation = tokio::time::timeout(std::time::Duration::from_secs(10), shared.nas_operation.lock()).await
        .map_err(|_| "転送の停止待ちがタイムアウトしました。NAS の応答を確認して再実行してください（操作は確定していません）".to_string())?;
    if action == backup_ipc::NasFileAction::DeleteLocal {
        let _session_operation = shared.media_session_operation.lock().await;
        if shared
            .rt()
            .slots
            .iter()
            .flatten()
            .any(|s| s.session_id.as_deref() == Some(&sid) && !s.task_finished)
        {
            return Err("SD の取り込みが終わってから削除してください".into());
        }
        let files = shared
            .db()
            .prepare_nas_file_deletion(&sid, &ids)
            .map_err(|e| e.to_string())?;
        let shared2 = shared.clone();
        let sid2 = sid.clone();
        let (deleted, mut errors) = tokio::task::spawn_blocking(move || {
            let mut deleted = 0;
            let mut errors = Vec::new();
            for file in files {
                match crate::file_actions::finish_deletion(&shared2, &file) {
                    Ok(()) => deleted += 1,
                    Err(e) => errors.push(format!("{}: {e}", file.source_rel_path)),
                }
            }
            if let Err(e) = crate::file_actions::refresh_manifest(&shared2, &sid2) {
                errors.push(format!("一覧ファイルを更新できません: {e}"));
            }
            (deleted, errors)
        })
        .await
        .map_err(|e| e.to_string())?;
        shared
            .db()
            .refresh_session_counters(&sid)
            .map_err(|e| e.to_string())?;
        shared.capacity_wakeup.notify_waiters();
        drop(_intents);
        let message = format!(
            "{} 件の PC コピーと転送キューを削除しました（NAS 上のコピーは変更しません）",
            deleted
        );
        shared.event(Some(&sid), "warn", "nas_files_deleted", &message);
        if !errors.is_empty() {
            errors.truncate(3);
            return Err(format!(
                "{message}。削除待ち・更新エラー: {}",
                errors.join(" / ")
            ));
        }
        return Ok(Response::Ok { message });
    }
    shared
        .db()
        .manage_nas_files(&sid, &ids, action == backup_ipc::NasFileAction::Retry)
        .map_err(|e| e.to_string())?;
    let message = match action {
        backup_ipc::NasFileAction::Retry => {
            format!("選択した {} 件の NAS 転送を再試行待ちにしました", ids.len())
        }
        backup_ipc::NasFileAction::Cancel => format!(
            "選択した {} 件を転送待ちから削除しました（PC 内のコピーは保持）",
            ids.len()
        ),
        backup_ipc::NasFileAction::DeleteLocal => unreachable!(),
    };
    drop(_intents);
    shared.event(Some(&sid), "info", "nas_files_managed", &message);
    Ok(Response::Ok { message })
}

async fn discard_session(shared: &Arc<Shared>, session_id: String) -> Result<Response, String> {
    let session = shared
        .db()
        .get_session(&session_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "セッションが見つかりません".to_string())?;
    if session.staging_deleted_at_utc.is_some()
        || session.state == backup_core::state::SessionState::Complete
    {
        return Err("このセッションの SSD データはすでに削除済みです".into());
    }
    if session.state.is_media_active() {
        return Err("メディア処理中のため削除できません。処理完了後に実行してください".into());
    }
    let in_slot = shared
        .rt()
        .slots
        .iter()
        .flatten()
        .any(|slot| slot.session_id.as_deref() == Some(&session_id));
    if in_slot {
        return Err("メディアが処理枠に残っているため削除できません".into());
    }

    let was_paused = shared.is_paused();
    shared
        .paused
        .store(true, std::sync::atomic::Ordering::SeqCst);
    shared.nas_wakeup.notify_one();
    let _operation = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        shared.nas_operation.lock(),
    )
    .await
    {
        Ok(operation) => operation,
        Err(_) => {
            if !was_paused {
                shared
                    .paused
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            return Err("NAS 転送を停止できなかったため、SSD データは削除しませんでした".into());
        }
    };
    let _session_operation = shared.media_session_operation.lock().await;
    // 排他を取るまでに、完了・再挿入で状態が変わっていないか再確認する。
    let current = match shared.db().get_session(&session_id) {
        Ok(current) => current,
        Err(e) => {
            if !was_paused {
                shared
                    .paused
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            return Err(e.to_string());
        }
    };
    let still_valid = current.is_some_and(|s| {
        s.staging_deleted_at_utc.is_none()
            && !s.state.is_media_active()
            && s.state != backup_core::state::SessionState::Complete
    }) && !shared
        .rt()
        .slots
        .iter()
        .flatten()
        .any(|s| s.session_id.as_deref() == Some(&session_id));
    if !still_valid {
        if !was_paused {
            shared
                .paused
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        return Err(
            "セッションの状態が変わったため削除しませんでした。画面を更新してください".into(),
        );
    }

    let staging = shared.staging.clone();
    let sid = session_id.clone();
    let remove_result =
        match tokio::task::spawn_blocking(move || staging.remove_session(&sid)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("SSD のセッションフォルダを削除できません: {e}")),
            Err(e) => Err(format!("SSD の削除処理を実行できません: {e}")),
        };
    if let Err(e) = remove_result {
        if !was_paused {
            shared
                .paused
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        return Err(e);
    }

    if let Err(e) = shared.db().discard_session(&session_id) {
        if !was_paused {
            shared
                .paused
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        return Err(format!("削除済みデータの履歴を更新できません: {e}"));
    }
    if !was_paused {
        shared
            .paused
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
    shared.event(
        Some(&session_id),
        "warn",
        "session_discarded",
        &format!(
            "{} の NAS 未検証データを SSD から削除しました",
            session.folder_name
        ),
    );
    shared.capacity_wakeup.notify_waiters();
    shared.nas_wakeup.notify_one();
    Ok(Response::Ok {
        message: format!(
            "{} の SSD データと NAS 転送待ちを削除しました",
            session.folder_name
        ),
    })
}

pub fn to_view(cfg: &Config) -> SettingsView {
    SettingsView {
        nas_mode: match cfg.nas.mode {
            NasMode::Disabled => "disabled",
            NasMode::Gvfs => "gvfs",
            NasMode::MountedPath => "mounted_path",
        }
        .into(),
        nas_url: cfg.nas.url.clone(),
        nas_username: cfg.nas.username.clone(),
        nas_domain: cfg.nas.domain.clone(),
        nas_mounted_path: cfg.nas.mounted_path.clone(),
        nas_target_initialized: !cfg.nas.target_id.is_empty(),
        final_rehash_before_cleanup: cfg.nas.final_rehash_before_cleanup,
        play_success_sound: cfg.ui.play_success_sound,
        play_warning_sound: cfg.ui.play_warning_sound,
        staging_directory: cfg.staging_directory.clone(),
        reserve_free_bytes: cfg.reserve_free_bytes,
        max_active_media: cfg.max_active_media,
    }
}

fn apply_view(cfg: &mut Config, v: &SettingsView) -> Result<(), String> {
    cfg.nas.mode = match v.nas_mode.as_str() {
        "disabled" => NasMode::Disabled,
        "gvfs" => NasMode::Gvfs,
        "mounted_path" => NasMode::MountedPath,
        other => return Err(format!("不明な NAS 方式です: {other}")),
    };
    cfg.nas.url = v.nas_url.trim().to_string();
    cfg.nas.username = v.nas_username.trim().to_string();
    cfg.nas.domain = v.nas_domain.trim().to_string();
    cfg.nas.mounted_path = v.nas_mounted_path.trim().to_string();
    cfg.nas.final_rehash_before_cleanup = v.final_rehash_before_cleanup;
    cfg.ui.play_success_sound = v.play_success_sound;
    cfg.ui.play_warning_sound = v.play_warning_sound;
    // ステージング・最低空き容量・同時処理数は GUI からは変更しない（進行中ジョブとの整合のため）
    cfg.validate().map_err(|e| e.to_string())
}

/// NAS 転送が進行中のセッションがあるか（接続先の変更を拒否する条件、FR-037）。
fn nas_work_in_progress(shared: &Shared) -> bool {
    use backup_core::state::SessionState::*;
    let db = shared.db();
    let stats = db.nas_queue_stats().unwrap_or_default();
    let sessions = db
        .sessions_in_states(&[NasPending, NasVerified, StagingCleanup])
        .map(|v| v.len())
        .unwrap_or(0);
    stats.queued_files > 0 || stats.failed_files > 0 || sessions > 0
}

async fn update_settings(
    shared: &Arc<Shared>,
    view: SettingsView,
    password: Option<String>,
) -> Result<Response, String> {
    let old = shared.config();
    let mut cfg = old.clone();
    apply_view(&mut cfg, &view)?;
    let destination_changed = !old.nas.same_destination(&cfg.nas);
    if destination_changed {
        // NAS 未設定の間に SSD へ取り込んだファイルにも、最初の保存先を設定できる。
        // 一度設定した接続先の変更は、従来どおり未完了の転送がある間は禁止する。
        let first_destination = old.nas.mode == NasMode::Disabled && old.nas.target_id.is_empty();
        if !first_destination && nas_work_in_progress(shared) {
            return Err(
                "未完了の NAS 転送があるため、接続先は変更できません（完了後に変更してください）"
                    .into(),
            );
        }
        cfg.nas.target_id.clear();
    }
    if let Some(pw) = password.filter(|p| !p.is_empty()) {
        let account = cfg.nas.secret_account();
        tokio::task::spawn_blocking(move || secrets::store_password(&account, &pw))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
    }
    cfg.save(&shared.paths.config_file())
        .map_err(|e| e.to_string())?;
    *shared.config.write().unwrap_or_else(|e| e.into_inner()) = cfg.clone();
    shared.event(
        None,
        "info",
        "settings_updated",
        &format!("設定を保存しました（NAS: {}）", cfg.nas.display_target()),
    );
    shared.nas_wakeup.notify_one();
    Ok(Response::Ok {
        message: if destination_changed && cfg.nas.mode != NasMode::Disabled {
            "保存しました。続けて保存先を初期化してください".into()
        } else {
            "保存しました".into()
        },
    })
}

async fn initialize_target(shared: &Arc<Shared>) -> Result<Response, String> {
    let mut cfg = shared.config();
    if cfg.nas.mode == NasMode::Disabled && shared.overrides.nas_local_dir.is_none() {
        return Err("NAS が設定されていません".into());
    }
    let root = nas_backend::resolve_root(shared, &cfg, None)
        .await
        .map_err(|e| e.to_string())?;
    let (id, adopted) = match root.read_marker().map_err(|e| e.to_string())? {
        Some(m) => (m.target_id, true),
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            root.create_marker(&id, &backup_db::now_utc())
                .map_err(|e| e.to_string())?;
            (id, false)
        }
    };
    cfg.nas.target_id = id;
    cfg.save(&shared.paths.config_file())
        .map_err(|e| e.to_string())?;
    *shared.config.write().unwrap_or_else(|e| e.into_inner()) = cfg;
    shared.nas_wakeup.notify_one();
    let msg = if adopted {
        "既存の保存先識別ファイルを使用します"
    } else {
        "保存先を初期化しました"
    };
    shared.event(None, "info", "nas_target_initialized", msg);
    Ok(Response::Ok {
        message: msg.into(),
    })
}

async fn test_nas(
    shared: &Arc<Shared>,
    view: SettingsView,
    password: Option<String>,
) -> Result<Response, String> {
    let mut cfg = shared.config();
    let mut report = NasTestReport::default();
    if let Err(e) = apply_view(&mut cfg, &view) {
        report.steps.push(("設定値".into(), false, e));
        return Ok(Response::NasTest { report });
    }
    let password = password.filter(|p| !p.is_empty());
    let root: NasRoot = match nas_backend::resolve_root(shared, &cfg, password).await {
        Ok(r) => {
            report
                .steps
                .push(("接続".into(), true, r.display_uri().to_string()));
            r
        }
        Err(e) => {
            report.steps.push(("接続".into(), false, e.to_string()));
            return Ok(Response::NasTest { report });
        }
    };
    // 書き込み・読み戻し・削除（自分のテストファイルのみ）
    let root2 = root.clone();
    let write = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write;
        let p = root2
            .root()
            .join(format!(".sdbm-write-test-{}", uuid::Uuid::new_v4()));
        let data = b"sd-backup-manager write test";
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        let back = std::fs::read(&p);
        let _ = std::fs::remove_file(&p);
        if back? != data {
            return Err(std::io::Error::other("read-back mismatch"));
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?;
    report.steps.push(match write {
        Ok(()) => ("書き込み・読み戻し".into(), true, "OK".into()),
        Err(e) => ("書き込み・読み戻し".into(), false, e.to_string()),
    });
    let marker = match root.read_marker() {
        Ok(None) => (true, "未初期化（保存後に初期化します）".to_string()),
        Ok(Some(m)) if !cfg.nas.target_id.is_empty() && m.target_id == cfg.nas.target_id => {
            (true, "設定済みの保存先と一致".into())
        }
        Ok(Some(_)) => (
            true,
            "既存の保存先識別ファイルがあります（初期化時に引き継ぎます）".into(),
        ),
        Err(e) => (false, e.to_string()),
    };
    report.steps.push(("保存先識別".into(), marker.0, marker.1));
    report.free_bytes = root.space().map(|s| s.free_bytes);
    report.steps.push((
        "空き容量".into(),
        true,
        report
            .free_bytes
            .map(backup_core::identity::format_bytes)
            .unwrap_or_else(|| "取得不可".into()),
    ));
    report.success = report.steps.iter().all(|s| s.1);
    Ok(Response::NasTest { report })
}

async fn restore(
    shared: &Arc<Shared>,
    session_id: String,
    dest_dir: String,
) -> Result<Response, String> {
    let dest = PathBuf::from(&dest_dir);
    if !dest.is_absolute() {
        return Err("復元先は絶対パスで指定してください".into());
    }
    let session = shared
        .db()
        .get_session(&session_id)
        .map_err(|e| e.to_string())?
        .ok_or("セッションがありません")?;
    let files = shared
        .db()
        .files_for_session(&session_id)
        .map_err(|e| e.to_string())?;
    let manifest =
        nas_worker::build_manifest(shared, &session, &files).map_err(|e| e.to_string())?;
    let staging_dir = shared
        .staging
        .session_dir(&session_id)
        .map_err(|e| e.to_string())?;
    let cfg = shared.config();
    let nas = nas_backend::connect(shared, &cfg, true).await.ok();
    let (source, nas_root) = if session.staging_deleted_at_utc.is_none() && staging_dir.is_dir() {
        (staging_dir, nas.as_ref().map(|n| n.root().to_path_buf()))
    } else {
        let n = nas.ok_or("NAS に接続できないため復元できません")?;
        (
            n.root().join(&session.folder_name),
            Some(n.root().to_path_buf()),
        )
    };
    let report = tokio::task::spawn_blocking(move || {
        backup_storage::restore::restore_layout(&manifest, &source, nas_root.as_deref(), &dest)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let msg = format!(
        "復元しました: {} 件（既存 {} 件、失敗 {} 件）→ {dest_dir}",
        report.restored,
        report.already_present,
        report.failed.len()
    );
    shared.event(
        Some(&session_id),
        if report.failed.is_empty() {
            "info"
        } else {
            "warn"
        },
        "restore_layout",
        &msg,
    );
    if report.failed.is_empty() {
        Ok(Response::Ok { message: msg })
    } else {
        Err(msg)
    }
}
