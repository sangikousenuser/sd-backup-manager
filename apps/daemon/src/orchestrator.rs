//! メディア 1 台分の処理（検知 → 走査 → 容量確保 → コピー/検証 → アンマウント）。
//!
//! 「取り外し可能」は、全対象パーティションのアンマウントに成功したときだけ表示する（絶対ルール 3）。
//! NAS 転送はこの処理と独立して `nas_worker` が並行実行する。

use crate::scanner::{ScanResult, scan_partition};
use crate::state::{Shared, SlotRuntime, WaitingMedia};
use backup_core::classifier::{ClassifyInput, classify_partition};
use backup_core::identity::{Confidence, MediaDescriptor};
use backup_core::partition_label;
use backup_core::paths::DestinationAllocator;
use backup_core::policy::{Admission, CapacityInput, admit, can_write_next, safety_margin};
use backup_core::state::{SessionState, StageState};
use backup_core::{MAX_COPY_ATTEMPTS, identity::format_bytes};
use backup_db::{FileRow, NewFile, PartitionRow, SessionRow};
use backup_device::DeviceEvent;
use backup_ipc::{NotificationKind, SlotPhase};
use backup_storage::Control;
use backup_storage::atomic::{CopyError, copy_and_verify};
use backup_storage::capacity::fs_space;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// デバイスイベントを処理し、スロットへの割り当て・待機キュー管理を行う。
pub async fn run_device_loop(shared: Arc<Shared>, mut rx: mpsc::Receiver<DeviceEvent>) {
    loop {
        let ev = tokio::select! {
            _ = shared.shutdown.cancelled() => return,
            ev = rx.recv() => match ev { Some(ev) => ev, None => return },
        };
        match ev {
            DeviceEvent::Added(media) => on_added(&shared, media),
            DeviceEvent::Removed { device_key } => on_removed(&shared, &device_key),
            DeviceEvent::Rejected { name, reason, .. } => {
                let msg = format!(
                    "{} は自動取り込みの対象外です: {reason}",
                    if name.is_empty() {
                        "デバイス"
                    } else {
                        &name
                    }
                );
                shared.warn(msg.clone());
                shared.event(None, "warn", "device_rejected", &msg);
            }
        }
    }
}

fn on_added(shared: &Arc<Shared>, media: MediaDescriptor) {
    let identity = media.identity_hash();
    let name = media.display_name();
    let slot = {
        let mut rt = shared.rt();
        if shared
            .local_reset_pending
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return; // 削除中の挿入は受け付けない。完了後に挿し直してもらう。
        }
        if rt.slot_of(&media.device_key).is_some()
            || rt
                .waiting
                .iter()
                .any(|w| w.media.device_key == media.device_key)
        {
            return; // 重複イベント
        }
        rt.connected_identities
            .entry(identity)
            .or_default()
            .push(media.device_key.clone());
        match rt.free_slot() {
            Some(i) => {
                let cancel = shared.shutdown.child_token();
                rt.slots[i] = Some(SlotRuntime::new(media.clone(), cancel.clone()));
                Some((i, cancel))
            }
            None => {
                rt.waiting.push_back(WaitingMedia {
                    media: media.clone(),
                    since: Instant::now(),
                });
                None
            }
        }
    };
    match slot {
        Some((i, cancel)) => {
            shared.event(
                None,
                "info",
                "media_inserted",
                &format!("メディア {} を検知しました: {name}", i + 1),
            );
            spawn_media_task(shared.clone(), i, media, cancel);
        }
        None => shared.event(
            None,
            "info",
            "media_queued",
            &format!("{name} は待機中です（同時処理は最大 2 台）"),
        ),
    }
}

fn on_removed(shared: &Arc<Shared>, device_key: &str) {
    let mut freed = None;
    {
        let mut rt = shared.rt();
        for list in rt.connected_identities.values_mut() {
            list.retain(|k| k != device_key);
        }
        rt.connected_identities.retain(|_, v| !v.is_empty());
        rt.waiting.retain(|w| w.media.device_key != device_key);
        if let Some(i) = rt.slot_of(device_key) {
            let slot = rt.slots[i].as_mut().unwrap();
            slot.removed = true;
            slot.cancel.cancel();
            if slot.task_finished {
                freed = Some((i, slot.phase, slot.session_id.clone()));
            }
        }
    }
    if let Some((i, phase, session_id)) = freed {
        // アンマウント失敗のまま抜かれた場合: 読み込みは完了しているので NAS 処理へ進める
        if phase == SlotPhase::UnmountFailed
            && let Some(sid) = &session_id
        {
            let db = shared.db();
            let failed = db
                .get_session(sid)
                .ok()
                .flatten()
                .map(|s| s.failed_files)
                .unwrap_or(0);
            let next = if failed > 0 {
                SessionState::NeedsAttention
            } else {
                SessionState::NasPending
            };
            let _ = db.transition_session(sid, next);
            drop(db);
            shared.nas_wakeup.notify_one();
        }
        free_slot(shared, i);
    }
}

/// スロットを空け、待機中のメディアがあれば割り当てる。
pub fn free_slot(shared: &Arc<Shared>, i: usize) {
    let next = {
        let mut rt = shared.rt();
        rt.slots[i] = None;
        match rt.waiting.pop_front() {
            Some(w) => {
                let cancel = shared.shutdown.child_token();
                rt.slots[i] = Some(SlotRuntime::new(w.media.clone(), cancel.clone()));
                Some((w.media, cancel))
            }
            None => None,
        }
    };
    if let Some((media, cancel)) = next {
        spawn_media_task(shared.clone(), i, media, cancel);
    }
    shared.capacity_wakeup.notify_waiters();
}

fn spawn_media_task(
    shared: Arc<Shared>,
    slot: usize,
    media: MediaDescriptor,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let outcome = process_media(&shared, slot, media, &cancel).await;
        if let Err(e) = &outcome {
            shared.event(
                None,
                "error",
                "media_error",
                &format!("メディア {} の処理でエラー: {e:#}", slot + 1),
            );
            set_phase(&shared, slot, SlotPhase::Error, format!("エラー: {e}"));
            shared.notify(
                NotificationKind::Error,
                format!("メディア {}: エラー", slot + 1),
            );
        }
        // 処理終了。抜去済みならスロットを空ける
        let removed = {
            let mut rt = shared.rt();
            match rt.slots[slot].as_mut() {
                Some(s) => {
                    s.task_finished = true;
                    s.remaining_bytes = 0;
                    s.removed
                }
                None => false,
            }
        };
        shared.capacity_wakeup.notify_waiters();
        if removed {
            on_removed_after_finish(&shared, slot);
        }
    });
}

fn on_removed_after_finish(shared: &Arc<Shared>, slot: usize) {
    let key = shared.rt().slots[slot]
        .as_ref()
        .map(|s| s.media.device_key.clone());
    if let Some(key) = key {
        on_removed(shared, &key);
    }
}

pub fn set_phase(shared: &Shared, slot: usize, phase: SlotPhase, message: impl Into<String>) {
    if let Some(s) = shared.rt().slots.get_mut(slot).and_then(|s| s.as_mut()) {
        s.phase = phase;
        s.message = message.into();
    }
}

fn with_slot(shared: &Shared, slot: usize, f: impl FnOnce(&mut SlotRuntime)) {
    if let Some(s) = shared.rt().slots.get_mut(slot).and_then(|s| s.as_mut()) {
        f(s);
    }
}

enum Interrupt {
    Removed,
    Shutdown,
}

fn interrupted(shared: &Shared, cancel: &CancellationToken) -> Option<Interrupt> {
    if shared.shutdown.is_cancelled() {
        Some(Interrupt::Shutdown)
    } else if cancel.is_cancelled() {
        Some(Interrupt::Removed)
    } else {
        None
    }
}

/// メディア 1 台の処理本体。
async fn process_media(
    shared: &Arc<Shared>,
    slot: usize,
    media: MediaDescriptor,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    set_phase(shared, slot, SlotPhase::Reading, "読み込み準備中");

    // 1. 読み込み専用で準備（既存マウントがあればそれを使う）
    let media = match shared.devices.prepare(&media).await {
        Ok(m) => m,
        Err(e) => {
            let msg = format!("メディアを読み込めません: {e}");
            set_phase(shared, slot, SlotPhase::Blocked, msg.clone());
            shared.event(None, "error", "media_prepare_failed", &msg);
            shared.notify(NotificationKind::Error, msg);
            return Ok(());
        }
    };
    with_slot(shared, slot, |s| s.media = media.clone());

    let session_operation = shared.media_session_operation.lock().await;
    // 2. 識別と信頼度（同じ識別子のメディアが同時に接続されていればクローン扱い）
    let identity = media.identity_hash();
    let mut confidence = media.base_confidence();
    if shared
        .rt()
        .connected_identities
        .get(&identity)
        .is_some_and(|v| v.len() > 1)
    {
        confidence = Confidence::Ambiguous;
        shared.warn(format!(
            "{} と同じ識別情報のメディアが同時に接続されています（別セッションとして扱います）",
            media.display_name()
        ));
    }

    // 3. セッション（中断セッションの再開 or 新規）
    let (device, partitions, resumable) = {
        let db = shared.db();
        let device = db.upsert_device(&media, confidence)?;
        let partitions = db.upsert_partitions(&device.device_id, &media.partitions)?;
        let resumable = if confidence.allows_resume_and_skip() {
            db.find_resumable_session(&device.device_id)?
        } else {
            None
        };
        (device, partitions, resumable)
    };
    let session = match resumable {
        Some(s) => s,
        None => {
            let stable_key = media.stable_card_key();
            let known = shared
                .db()
                .known_card(&device.device_id, stable_key.as_deref())?;
            let card = match known {
                Some(card)
                    if confidence != Confidence::Ambiguous
                        && (confidence.allows_resume_and_skip() || stable_key.is_some()) =>
                {
                    shared
                        .db()
                        .assign_card(&device.device_id, Some(&card), None)?;
                    card
                }
                _ => shared.db().assign_card(
                    &device.device_id,
                    None,
                    stable_key
                        .as_deref()
                        .filter(|_| confidence != Confidence::Ambiguous),
                )?,
            };
            shared.db().create_card_session(
                &device.device_id,
                &card,
                confidence,
                chrono::Utc::now(),
                &shared.config().timezone,
            )?
        }
    };
    let resumed = session.state != SessionState::Detected;
    let sid = session.session_id.clone();
    shared
        .staging
        .register_session(&sid, &session.folder_name)?;
    with_slot(shared, slot, |s| {
        s.session_id = Some(sid.clone());
        s.folder_name = Some(session.folder_name.clone());
        s.confidence = Some(confidence.as_str().into());
    });
    drop(session_operation);
    shared.event(
        Some(&sid),
        "info",
        if resumed {
            "session_resumed"
        } else {
            "session_created"
        },
        &format!(
            "{} セッション {}（{}、識別信頼度 {}）",
            if resumed { "中断した" } else { "新しい" },
            session.folder_name,
            media.display_name(),
            confidence.as_str()
        ),
    );

    // アンマウント失敗のまま再起動した場合はアンマウントだけやり直す
    if session.state == SessionState::UnmountFailed {
        return finalize(shared, slot, &media, &sid, 0, 0).await;
    }

    // 4. 走査
    shared
        .db()
        .transition_session(&sid, SessionState::Scanning)?;
    shared.db().set_media_state(&sid, "SCANNING")?;
    set_phase(shared, slot, SlotPhase::Reading, "ファイルを確認中");
    let mut scans: Vec<ScanResult> = Vec::new();
    for p in &media.partitions {
        let Some(mp) = p.mount_point.clone() else {
            continue;
        };
        let token = cancel.clone();
        let index = p.index;
        let r = tokio::task::spawn_blocking(move || scan_partition(&mp, index, &token)).await?;
        if r.cancelled {
            return interrupt(shared, slot, &sid, Interrupt::Removed).await;
        }
        scans.push(r);
    }
    let scan_errors: Vec<String> = scans.iter().flat_map(|r| r.errors.clone()).collect();
    let scan_warnings: Vec<String> = scans.iter().flat_map(|r| r.warnings.clone()).collect();
    for w in scan_warnings.iter().take(20) {
        shared.event(Some(&sid), "warn", "scan_warning", w);
    }
    for e in &scan_errors {
        shared.event(Some(&sid), "error", "scan_error", e);
    }
    with_slot(shared, slot, |s| {
        s.warnings = scan_warnings
            .iter()
            .chain(scan_errors.iter())
            .take(50)
            .cloned()
            .collect();
    });

    // 5. 分類・格納パス割り当て・DB 登録
    let files = register_files(shared, &sid, &partitions, scans)?;
    shared.db().set_scan_completed(&sid, true)?;

    let pending: Vec<FileRow> = files
        .iter()
        .filter(|f| f.stage_state.needs_copy())
        .cloned()
        .collect();
    let required: u64 = pending.iter().map(|f| f.size_bytes).sum();
    let max_file = pending.iter().map(|f| f.size_bytes).max().unwrap_or(0);
    let done_bytes: u64 = files
        .iter()
        .filter(|f| f.stage_state.is_done())
        .map(|f| f.size_bytes)
        .sum();
    with_slot(shared, slot, |s| {
        s.files_total = files.len() as u64;
        s.files_done = files.iter().filter(|f| f.stage_state.is_done()).count() as u64;
        s.files_skipped = files
            .iter()
            .filter(|f| f.stage_state == StageState::SkippedVerified)
            .count() as u64;
        s.files_failed = files.iter().filter(|f| f.stage_state.is_problem()).count() as u64;
        s.bytes_total = files.iter().map(|f| f.size_bytes).sum();
        s.bytes_done = done_bytes;
        s.remaining_bytes = required;
    });
    shared.event(
        Some(&sid),
        "info",
        "scan_completed",
        &format!(
            "{} ファイル（コピー対象 {} 件 / {}）",
            files.len(),
            pending.len(),
            format_bytes(required)
        ),
    );

    // 6. 容量アドミッション（開始したセッションが完遂できる見込みがあるときだけ開始）
    if let Some(i) = wait_for_capacity(shared, slot, &sid, cancel, required, max_file).await? {
        return interrupt(shared, slot, &sid, i).await;
    }

    // 7. コピー
    {
        let db = shared.db();
        db.transition_session(&sid, SessionState::Copying)?;
        db.set_media_state(&sid, "COPYING")?;
    }
    set_phase(shared, slot, SlotPhase::Copying, "コピー中");
    let identity_hash = identity.clone();
    let fingerprints: HashMap<u32, String> = partitions
        .iter()
        .map(|p| (p.part_index, p.fingerprint.clone()))
        .collect();
    let mut failed_count = files.iter().filter(|f| f.stage_state.is_problem()).count() as u64;
    let mut current_done = done_bytes;

    for f in pending {
        if let Some(i) = interrupted(shared, cancel) {
            return interrupt(shared, slot, &sid, i).await;
        }
        while shared.is_paused() {
            set_phase(shared, slot, SlotPhase::Copying, "一時停止中");
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
        // ファイル単位の容量確認（チャンク境界で安全に停止）
        if let Some(i) = wait_for_file_capacity(shared, slot, cancel, f.size_bytes).await {
            return interrupt(shared, slot, &sid, i).await;
        }
        set_phase(shared, slot, SlotPhase::Copying, "コピー中");
        with_slot(shared, slot, |s| {
            s.current_file = Some(f.source_rel_path.clone())
        });

        let Some(mount) = media
            .partitions
            .iter()
            .find(|p| p.index == f.partition_index)
            .and_then(|p| p.mount_point.clone())
        else {
            continue;
        };
        let src = source_path(&mount, &f);

        // 過去の検証済み実体があればスキップ（元ファイルと NAS 実体の両方を照合できた場合のみ）
        if !backup_core::session::is_card_folder(&session.folder_name)
            && confidence.allows_resume_and_skip()
            && let Some(fp) = fingerprints.get(&f.partition_index)
            && try_skip_verified(shared, &identity_hash, fp, &f, &src).await
        {
            current_done += f.size_bytes;
            with_slot(shared, slot, |s| {
                s.files_done += 1;
                s.files_skipped += 1;
                s.bytes_done = current_done;
                s.remaining_bytes = s.remaining_bytes.saturating_sub(f.size_bytes);
            });
            continue;
        }

        let outcome = loop {
            match copy_with_retries(shared, slot, cancel, &sid, &f, &src, current_done).await? {
                FileOutcome::CapacityPaused => {
                    // 空き容量が回復するまで待ってから同じファイルをやり直す（試行回数には数えない）
                    if let Some(i) =
                        wait_for_file_capacity(shared, slot, cancel, f.size_bytes).await
                    {
                        break FileOutcome::Interrupted(i);
                    }
                    set_phase(shared, slot, SlotPhase::Copying, "コピー中");
                }
                other => break other,
            }
        };
        match outcome {
            FileOutcome::Verified => {
                current_done += f.size_bytes;
                with_slot(shared, slot, |s| {
                    s.files_done += 1;
                    s.bytes_done = current_done;
                    s.remaining_bytes = s.remaining_bytes.saturating_sub(f.size_bytes);
                });
                shared.nas_wakeup.notify_one();
            }
            FileOutcome::Failed(msg) => {
                failed_count += 1;
                current_done += f.size_bytes;
                with_slot(shared, slot, |s| {
                    s.files_failed += 1;
                    s.bytes_done = current_done;
                    s.remaining_bytes = s.remaining_bytes.saturating_sub(f.size_bytes);
                    s.failed_files.push(format!("{}: {msg}", f.source_rel_path));
                });
            }
            FileOutcome::Interrupted(i) => return interrupt(shared, slot, &sid, i).await,
            FileOutcome::CapacityPaused => unreachable!("handled above"),
        }
    }
    with_slot(shared, slot, |s| s.current_file = None);

    let scan_problem = !scan_errors.is_empty() as u64;
    finalize(shared, slot, &media, &sid, failed_count, scan_problem).await
}

/// 走査結果を DB に登録する（再開時は既存行と突き合わせる）。
fn register_files(
    shared: &Shared,
    sid: &str,
    partitions: &[PartitionRow],
    scans: Vec<ScanResult>,
) -> anyhow::Result<Vec<FileRow>> {
    let formats = shared
        .formats
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let db = shared.db();
    let existing = db.files_for_session(sid)?;
    let source_tree = db
        .get_session(sid)?
        .is_some_and(|s| backup_core::session::is_card_folder(&s.folder_name));
    let mut allocator = DestinationAllocator::new();
    // アプリの管理ファイルとSD内の同名ファイルを衝突させない。
    if source_tree {
        allocator.register_existing("manifest.json");
        allocator.register_existing(".partial");
        if partitions.len() > 1 {
            for p in partitions {
                let dir = allocator.allocate_directory("", &[partition_label(p.part_index)]);
                std::fs::create_dir_all(shared.staging.final_path(sid, &dir)?)?;
            }
        }
    }
    for f in &existing {
        allocator.register_existing(&f.destination_rel_path);
    }
    let mut by_key: HashMap<(u32, String), FileRow> = existing
        .into_iter()
        .map(|f| ((f.partition_index, f.source_rel_path.clone()), f))
        .collect();
    let part_ids: HashMap<u32, &PartitionRow> =
        partitions.iter().map(|p| (p.part_index, p)).collect();

    let mut seen: std::collections::HashSet<(u32, String)> = std::collections::HashSet::new();
    for scan in scans {
        if source_tree {
            for (index, components) in &scan.directories {
                let prefix = if partitions.len() > 1 {
                    partition_label(*index)
                } else {
                    String::new()
                };
                let dest = allocator.allocate_directory(&prefix, components);
                db.save_directory(sid, *index, &components.join("/"), &dest)?;
                std::fs::create_dir_all(shared.staging.final_path(sid, &dest)?)?;
            }
        }
        let inputs: Vec<ClassifyInput> = scan
            .files
            .iter()
            .map(|f| ClassifyInput {
                components: f.components.clone(),
            })
            .collect();
        let classes = classify_partition(&formats, &inputs);
        for (f, class) in scan.files.iter().zip(classes) {
            let key = (f.partition_index, f.rel_path.clone());
            seen.insert(key.clone());
            match by_key.get(&key) {
                Some(row) => {
                    if row.stage_state.is_done() {
                        if row.size_bytes != f.size || row.source_mtime_ns != Some(f.mtime_ns) {
                            db.set_file_error(
                                &row.file_id,
                                row.stage_state,
                                "SOURCE_CHANGED_AFTER_COPY",
                                "コピー後に元ファイルが変更されています（再挿入後に新しいセッションで取り込みます）",
                            )?;
                        }
                    } else {
                        db.refresh_discovered_file(&row.file_id, f.size, Some(f.mtime_ns))?;
                    }
                }
                None => {
                    let Some(part) = part_ids.get(&f.partition_index) else {
                        continue;
                    };
                    let prefix = if source_tree {
                        if partitions.len() > 1 {
                            partition_label(f.partition_index)
                        } else {
                            String::new()
                        }
                    } else {
                        format!(
                            "{}/{}",
                            class.kind.folder(),
                            partition_label(f.partition_index)
                        )
                    };
                    let dest = allocator.allocate(&prefix, &f.components);
                    let row = db.insert_file(&NewFile {
                        session_id: sid.to_string(),
                        partition_id: part.partition_id.clone(),
                        partition_index: f.partition_index,
                        source_rel_path: f.rel_path.clone(),
                        source_rel_path_raw_hex: f.raw_hex.clone(),
                        destination_rel_path: dest,
                        media_kind: class.kind,
                        group_key: class.group.clone(),
                        size_bytes: f.size,
                        source_mtime_ns: Some(f.mtime_ns),
                    })?;
                    by_key.insert(key, row);
                }
            }
        }
    }
    // 再開時、以前あったのに見つからない未完了ファイルは失敗として残す（成功扱いしない）
    for (key, row) in &by_key {
        if !seen.contains(key)
            && !row.stage_state.is_done()
            && !matches!(
                row.stage_state,
                StageState::DeletePending | StageState::Discarded
            )
        {
            db.set_file_error(
                &row.file_id,
                StageState::FailedTerminal,
                "SOURCE_MISSING",
                "元メディアに見つかりません",
            )?;
        }
    }
    db.refresh_session_counters(sid)?;
    Ok(db.files_for_session(sid)?)
}

fn source_path(mount: &std::path::Path, f: &FileRow) -> std::path::PathBuf {
    #[cfg(unix)]
    if let Some(bytes) = f
        .source_rel_path_raw_hex
        .as_deref()
        .and_then(backup_core::manifest::hex_decode)
    {
        let mut p = mount.to_path_buf();
        for c in bytes.split(|b| *b == b'/') {
            use std::os::unix::ffi::OsStringExt;
            p.push(std::ffi::OsString::from_vec(c.to_vec()));
        }
        return p;
    }
    let mut p = mount.to_path_buf();
    for c in f.source_rel_path.split('/') {
        p.push(c);
    }
    p
}

fn ssd_free(shared: &Shared) -> Option<u64> {
    if let Some(v) = shared.overrides.fake_ssd_free_bytes {
        return Some(v);
    }
    fs_space(shared.staging.root()).ok().map(|s| s.free_bytes)
}

/// セッション開始前の容量待ち（5.2）。中断なら Some を返す。
async fn wait_for_capacity(
    shared: &Arc<Shared>,
    slot: usize,
    sid: &str,
    cancel: &CancellationToken,
    required: u64,
    max_file: u64,
) -> anyhow::Result<Option<Interrupt>> {
    let reserve = shared.config().reserve_free_bytes;
    let margin = safety_margin(max_file, required);
    with_slot(shared, slot, |s| s.queued_at = Instant::now());
    let mut announced = false;
    loop {
        if let Some(i) = interrupted(shared, cancel) {
            return Ok(Some(i));
        }
        let free = ssd_free(shared);
        let (others, earlier_waiting, total) = {
            let rt = shared.rt();
            (
                rt.reserved_by_others(slot),
                rt.someone_waiting_before(slot),
                rt.ssd_space.map(|s| s.total_bytes),
            )
        };
        let decision = if required == 0 {
            Admission::Admit
        } else {
            admit(CapacityInput {
                free_bytes: free,
                reserve_bytes: reserve,
                required_bytes: required,
                margin_bytes: margin,
                reserved_by_others: others,
            })
        };
        if decision == Admission::Admit && !earlier_waiting {
            with_slot(shared, slot, |s| s.admitted = true);
            return Ok(None);
        }
        let message = match decision {
            Admission::Unknown => "SSD の空き容量を取得できないため待機中".to_string(),
            Admission::Wait { shortfall } => {
                if total.is_some_and(|t| required.saturating_add(reserve) > t) {
                    format!(
                        "SSD の容量を超えるため取り込めません（必要 {} + 最低空き {}）。手動対応が必要です",
                        format_bytes(required),
                        format_bytes(reserve)
                    )
                } else {
                    format!(
                        "容量確保待ち（あと {} 必要。NAS 転送完了後に自動開始）",
                        format_bytes(shortfall)
                    )
                }
            }
            Admission::Admit => "先に接続されたメディアの開始待ち".to_string(),
        };
        if !announced {
            let db = shared.db();
            db.transition_session(sid, SessionState::Blocked)?;
            db.set_media_state(sid, "CAPACITY_WAIT")?;
            drop(db);
            shared.event(Some(sid), "warn", "capacity_wait", &message);
            announced = true;
        }
        set_phase(shared, slot, SlotPhase::CapacityWait, message);
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = shared.capacity_wakeup.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

/// ファイル単位の容量確認。足りなければ待つ。中断なら Some。
async fn wait_for_file_capacity(
    shared: &Arc<Shared>,
    slot: usize,
    cancel: &CancellationToken,
    size: u64,
) -> Option<Interrupt> {
    let reserve = shared.config().reserve_free_bytes;
    let margin = 64 * 1024 * 1024;
    let mut waiting = false;
    loop {
        if let Some(i) = interrupted(shared, cancel) {
            return Some(i);
        }
        if can_write_next(ssd_free(shared), reserve, size, margin) {
            return None;
        }
        if !waiting {
            shared.warn("SSD の空き容量が最低確保量に近づいたため、コピーを一時停止しました");
            waiting = true;
        }
        set_phase(
            shared,
            slot,
            SlotPhase::CapacityWait,
            "SSD 空き容量の回復待ち（一時停止中）",
        );
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = shared.capacity_wakeup.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

enum FileOutcome {
    Verified,
    Failed(String),
    Interrupted(Interrupt),
    CapacityPaused,
}

async fn copy_with_retries(
    shared: &Arc<Shared>,
    slot: usize,
    cancel: &CancellationToken,
    sid: &str,
    f: &FileRow,
    src: &std::path::Path,
    base_done: u64,
) -> anyhow::Result<FileOutcome> {
    let partial = shared.staging.partial_path(sid, &f.file_id)?;
    let dest = shared.staging.final_path(sid, &f.destination_rel_path)?;
    let mut attempts = f.copy_attempts;
    let mut last_error = String::new();
    while attempts < MAX_COPY_ATTEMPTS {
        if let Some(i) = interrupted(shared, cancel) {
            return Ok(FileOutcome::Interrupted(i));
        }
        attempts = shared.db().begin_copy_attempt(&f.file_id)?;
        let capacity_abort = Arc::new(AtomicBool::new(false));
        let result = {
            let shared2 = shared.clone();
            let cancel2 = cancel.clone();
            let cap = capacity_abort.clone();
            let src = src.to_path_buf();
            let partial = partial.clone();
            let dest = dest.clone();
            let reserve = shared.config().reserve_free_bytes;
            tokio::task::spawn_blocking(move || {
                let mut done = 0u64;
                let mut chunks = 0u32;
                copy_and_verify(&src, &partial, &dest, &mut |n| {
                    done += n;
                    chunks += 1;
                    if cancel2.is_cancelled() {
                        return Control::Abort;
                    }
                    // 256MiB ごとに空き容量を再確認
                    if chunks.is_multiple_of(64)
                        && !can_write_next(ssd_free(&shared2), reserve, 0, 0)
                    {
                        cap.store(true, Ordering::SeqCst);
                        return Control::Abort;
                    }
                    let now = Instant::now();
                    with_slot(&shared2, slot, |s| {
                        s.bytes_done = base_done + done;
                        s.rate.record(now, s.bytes_done);
                    });
                    Control::Continue
                })
            })
            .await?
        };
        match result {
            Ok(out) => {
                shared.db().mark_file_ssd_verified(&f.file_id, &out.hash)?;
                return Ok(FileOutcome::Verified);
            }
            Err(CopyError::Aborted) => {
                shared.db().cancel_copy_attempt(&f.file_id)?;
                // 中断は試行回数に数えない
                if capacity_abort.load(Ordering::SeqCst) {
                    return Ok(FileOutcome::CapacityPaused);
                }
                return Ok(FileOutcome::Interrupted(
                    interrupted(shared, cancel).unwrap_or(Interrupt::Removed),
                ));
            }
            Err(e) => {
                // 抜去による読み込みエラーは失敗として数えない
                let present = shared
                    .devices
                    .is_present(&shared_device_key(shared, slot))
                    .await
                    && src_root_exists(src);
                if !present || cancel.is_cancelled() {
                    shared
                        .db()
                        .set_file_stage(&f.file_id, StageState::SourceRemoved)?;
                    return Ok(FileOutcome::Interrupted(
                        interrupted(shared, cancel).unwrap_or(Interrupt::Removed),
                    ));
                }
                last_error = e.to_string();
                shared.event(
                    Some(sid),
                    "warn",
                    "copy_retry",
                    &format!(
                        "{} の取り込みに失敗（{attempts}/{MAX_COPY_ATTEMPTS} 回目）: {e}",
                        f.source_rel_path
                    ),
                );
                let stage = if attempts >= MAX_COPY_ATTEMPTS {
                    StageState::FailedTerminal
                } else {
                    StageState::FailedRetryable
                };
                shared
                    .db()
                    .set_file_error(&f.file_id, stage, e.code(), &e.to_string())?;
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    }
    Ok(FileOutcome::Failed(last_error))
}

fn shared_device_key(shared: &Shared, slot: usize) -> String {
    shared.rt().slots[slot]
        .as_ref()
        .map(|s| s.media.device_key.clone())
        .unwrap_or_default()
}

fn src_root_exists(src: &std::path::Path) -> bool {
    src.parent().is_some_and(|p| p.exists())
}

/// カタログに候補があれば、元ファイルと NAS 実体の両方を検証してスキップする（FR-021）。
async fn try_skip_verified(
    shared: &Arc<Shared>,
    identity: &str,
    fp: &str,
    f: &FileRow,
    src: &std::path::Path,
) -> bool {
    let candidates =
        match shared
            .db()
            .catalog_candidates(identity, fp, &f.source_rel_path, f.size_bytes)
        {
            Ok(c) if !c.is_empty() => c,
            _ => return false,
        };
    let cfg = shared.config();
    let Ok(nas) = crate::nas_backend::connect(shared, &cfg, true).await else {
        return false; // NAS オフライン → 安全側で再コピー
    };
    let src = src.to_path_buf();
    let source_hash = match tokio::task::spawn_blocking(move || {
        backup_storage::hash::hash_file(&src, &mut backup_storage::no_progress())
    })
    .await
    {
        Ok(Ok(h)) => h,
        _ => return false,
    };
    for c in candidates {
        if c.blake3_hash != source_hash.hash || source_hash.size != f.size_bytes {
            continue; // 同じ名前でも内容が違う → 再取得（FR-022）
        }
        let nas2 = nas.clone();
        let c2 = c.clone();
        let ok = tokio::task::spawn_blocking(move || {
            nas2.verify_existing(
                &c2.nas_session_folder,
                &c2.nas_dest_rel_path,
                &c2.blake3_hash,
                &mut backup_storage::no_progress(),
            )
        })
        .await;
        match ok {
            Ok(Ok(true)) => {
                let reference = backup_core::manifest::ManifestReference {
                    nas_session_folder: c.nas_session_folder.clone(),
                    dest_rel_path: c.nas_dest_rel_path.clone(),
                    nas_uri: c.nas_uri.clone(),
                };
                let json = serde_json::to_string(&reference).unwrap_or_default();
                if shared
                    .db()
                    .mark_file_skipped(&f.file_id, &source_hash.hash, &json)
                    .is_ok()
                {
                    return true;
                }
            }
            Ok(Ok(false)) => {
                let _ = shared.db().mark_catalog_unavailable(&c.catalog_id);
            }
            _ => return false,
        }
    }
    false
}

/// 中断処理: 未完了ファイルを SOURCE_REMOVED にし、セッションを SOURCE_INTERRUPTED へ。
async fn interrupt(
    shared: &Arc<Shared>,
    slot: usize,
    sid: &str,
    why: Interrupt,
) -> anyhow::Result<()> {
    {
        let db = shared.db();
        for f in db.files_for_session(sid)? {
            if matches!(
                f.stage_state,
                StageState::Discovered
                    | StageState::Copying
                    | StageState::Verifying
                    | StageState::FailedRetryable
            ) {
                db.set_file_stage(&f.file_id, StageState::SourceRemoved)?;
            }
        }
        db.refresh_session_counters(sid)?;
        let _ = db.transition_session(sid, SessionState::SourceInterrupted);
        db.set_media_state(sid, "SOURCE_INTERRUPTED")?;
    }
    match why {
        Interrupt::Removed => {
            let msg = "処理中断／同じメディアを再接続してください";
            set_phase(shared, slot, SlotPhase::Interrupted, msg);
            shared.event(
                Some(sid),
                "error",
                "source_interrupted",
                "コピー完了前にメディアが取り外されました（検証済みファイルは SSD に保持）",
            );
            shared.notify(NotificationKind::Error, msg);
        }
        Interrupt::Shutdown => {
            set_phase(
                shared,
                slot,
                SlotPhase::Interrupted,
                "停止のため中断（次回起動時に再開）",
            );
            shared.event(
                Some(sid),
                "warn",
                "shutdown_interrupted",
                "サービス停止のため処理を中断しました",
            );
        }
    }
    Ok(())
}

/// 読み込み終了後の仕上げ: アンマウント → 緑/黄/赤（FR-008、FR-033）。
async fn finalize(
    shared: &Arc<Shared>,
    slot: usize,
    media: &MediaDescriptor,
    sid: &str,
    failed: u64,
    scan_problems: u64,
) -> anyhow::Result<()> {
    {
        let db = shared.db();
        let s: SessionRow = db
            .get_session(sid)?
            .ok_or_else(|| anyhow::anyhow!("session vanished"))?;
        if s.state == SessionState::Copying {
            db.transition_session(sid, SessionState::SourceFinalizing)?;
        }
        if matches!(
            s.state,
            SessionState::Copying | SessionState::SourceFinalizing | SessionState::UnmountFailed
        ) {
            db.transition_session(sid, SessionState::Unmounting)?;
        }
        db.set_media_state(sid, "UNMOUNTING")?;
        db.refresh_session_counters(sid)?;
    }
    set_phase(
        shared,
        slot,
        SlotPhase::Unmounting,
        "取り外し準備中（アンマウント中）",
    );
    let failed = failed.max(
        shared
            .db()
            .get_session(sid)?
            .map(|s| s.failed_files)
            .unwrap_or(0),
    );
    let has_problem = failed > 0 || scan_problems > 0;
    if has_problem && scan_problems > 0 {
        shared
            .db()
            .set_session_error(sid, Some("一部のフォルダを読み込めませんでした"))?;
    }

    let unmount = if shared.config().auto_unmount {
        shared.devices.unmount_all(media).await
    } else {
        Err(backup_device::DeviceError::Unmount {
            partition: "-".into(),
            reason: "自動アンマウントが無効".into(),
        })
    };
    match unmount {
        Ok(()) => {
            let db = shared.db();
            db.set_media_unmounted(sid, true)?;
            if has_problem {
                db.transition_session(sid, SessionState::MediaYellow)?;
                db.set_media_state(sid, "MEDIA_YELLOW")?;
                db.transition_session(sid, SessionState::NeedsAttention)?;
                drop(db);
                let msg = format!("未完了・取り外し可能（失敗 {failed} 件）");
                set_phase(shared, slot, SlotPhase::DoneYellow, msg.clone());
                shared.event(Some(sid), "warn", "media_yellow", &msg);
                shared.notify(NotificationKind::Warning, msg);
            } else {
                db.transition_session(sid, SessionState::MediaGreen)?;
                db.set_media_state(sid, "MEDIA_GREEN")?;
                db.transition_session(sid, SessionState::NasPending)?;
                drop(db);
                set_phase(
                    shared,
                    slot,
                    SlotPhase::DoneGreen,
                    "コピー完了／取り外し可能",
                );
                shared.event(
                    Some(sid),
                    "info",
                    "media_green",
                    "全ファイルの SSD 取り込みと検証が完了し、アンマウントしました",
                );
                shared.notify(NotificationKind::Success, "コピー完了／取り外し可能");
            }
        }
        Err(e) => {
            {
                let db = shared.db();
                db.transition_session(sid, SessionState::UnmountFailed)?;
                db.set_media_state(sid, "UNMOUNT_FAILED")?;
            }
            let msg = format!("安全な取り外し不可／使用中: {e}");
            set_phase(shared, slot, SlotPhase::UnmountFailed, msg.clone());
            shared.event(Some(sid), "error", "unmount_failed", &msg);
            shared.notify(NotificationKind::Error, "安全な取り外し不可／使用中");
        }
    }
    with_slot(shared, slot, |s| {
        s.remaining_bytes = 0;
        s.rate.reset();
    });
    shared.nas_wakeup.notify_one();
    Ok(())
}

/// GUI からの「アンマウント再試行」。
pub async fn retry_unmount(shared: &Arc<Shared>, slot: usize) -> Result<String, String> {
    let (media, sid, phase) = {
        let rt = shared.rt();
        let s = rt
            .slots
            .get(slot)
            .and_then(|s| s.as_ref())
            .ok_or("メディアがありません")?;
        (s.media.clone(), s.session_id.clone(), s.phase)
    };
    if phase != SlotPhase::UnmountFailed {
        return Err("アンマウント失敗の状態ではありません".into());
    }
    let sid = sid.ok_or("セッションがありません")?;
    let failed = shared
        .db()
        .get_session(&sid)
        .ok()
        .flatten()
        .map(|s| s.failed_files)
        .unwrap_or(0);
    finalize(shared, slot, &media, &sid, failed, 0)
        .await
        .map_err(|e| e.to_string())?;
    let phase = shared
        .rt()
        .slots
        .get(slot)
        .and_then(|s| s.as_ref())
        .map(|s| s.phase);
    if phase.is_some_and(|p| p.removable()) {
        Ok("取り外し可能になりました".into())
    } else {
        Err("まだアンマウントできません".into())
    }
}
