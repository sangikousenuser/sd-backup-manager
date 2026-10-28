//! 履歴（FR-035）: セッション一覧・問題のあるファイル・元の構成で復元・診断エクスポート・30 日の操作履歴。

use super::{local_time, session_state_label};
use crate::app::{App, Message, file_is_actionable};
use crate::theme;
use backup_core::identity::format_bytes;
use backup_ipc::NasFileAction;
use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input};
use iced::{Element, Fill};

pub fn view(app: &App) -> Element<'_, Message> {
    let mut sessions = column![text("セッション").size(18)].spacing(4);
    if app.sessions.is_empty() {
        sessions = sessions.push(text("まだ記録がありません").color(theme::GRAY));
    }
    for s in &app.sessions {
        let selected = app.selected_session.as_deref() == Some(&s.session_id);
        let color = match s.state.as_str() {
            "COMPLETE" => theme::GREEN,
            "NEEDS_ATTENTION" | "SOURCE_INTERRUPTED" | "UNMOUNT_FAILED" => theme::RED,
            "NAS_PENDING" | "MEDIA_YELLOW" | "BLOCKED" => theme::YELLOW,
            _ => theme::BLUE,
        };
        let label = format!(
            "{}  {}  {}  {}/{} 件  NAS {} 件  失敗 {} 件{}",
            local_time(&s.started_at_utc),
            s.folder_name,
            session_state_label(&s.state),
            s.completed_files,
            s.total_files,
            s.nas_verified_files,
            s.failed_files,
            if s.staging_deleted {
                "  （SSD 削除済み）"
            } else {
                ""
            }
        );
        let b = button(text(label).color(color))
            .on_press(Message::SelectSession(s.session_id.clone()))
            .width(Fill);
        sessions = sessions.push(if selected {
            b.style(button::primary)
        } else {
            b.style(button::text)
        });
    }

    let mut detail = column![].spacing(6);
    if let Some(id) = &app.selected_session {
        detail = detail.push(text("NAS 転送ファイルの操作").size(18));
        detail = detail.push(text("チェックしたファイルだけを再試行、または PC コピーと転送キューから削除できます。NAS 上や元の SD のファイルは変更しません。").size(12));
        let interactive = !app.busy && app.pending_nas_action.is_none();
        let can_manage = interactive
            && app.sessions.iter().any(|s| {
                s.session_id == *id
                    && !s.staging_deleted
                    && !matches!(
                        s.state.as_str(),
                        "COMPLETE" | "DISCARDED" | "NAS_VERIFIED" | "STAGING_CLEANUP"
                    )
            });
        let mut filter =
            checkbox(app.only_problem_files).label("未完了・失敗・中止・削除したファイルだけ表示");
        if interactive {
            filter = filter.on_toggle(Message::OnlyProblemFiles);
        }
        detail = detail.push(filter);
        let actionable = app.files.iter().filter(|f| file_is_actionable(f)).count();
        let mut select = checkbox(actionable > 0 && app.selected_files.len() == actionable)
            .label("このページの操作可能なファイルをすべて選択");
        if can_manage && actionable > 0 {
            select = select.on_toggle(Message::SelectPageFiles);
        }
        detail = detail.push(select);
        let enabled = can_manage && !app.files_loading && !app.selected_files.is_empty();
        let can_retry = enabled
            && app
                .files
                .iter()
                .filter(|f| app.selected_files.contains(&f.file_id))
                .all(|f| f.stage_state == "SSD_VERIFIED");
        let can_delete = enabled
            && app.sessions.iter().any(|s| {
                s.session_id == *id
                    && matches!(
                        s.state.as_str(),
                        "NAS_PENDING" | "NEEDS_ATTENTION" | "SOURCE_INTERRUPTED" | "UNMOUNT_FAILED"
                    )
            });
        let mut retry = button(text(format!(
            "選択 {} 件を再試行",
            app.selected_files.len()
        )));
        let mut cancel = button(text("選択分の転送を中止（PC に残す）")).style(button::secondary);
        let mut delete =
            button(text("選択分の PC コピーと転送キューを削除…")).style(button::danger);
        if can_delete {
            delete = delete.on_press(Message::RequestNasAction(NasFileAction::DeleteLocal));
        }
        if can_retry {
            retry = retry.on_press(Message::RequestNasAction(NasFileAction::Retry));
            cancel = cancel.on_press(Message::RequestNasAction(NasFileAction::Cancel));
        }
        detail = detail.push(
            row![
                retry,
                cancel,
                button(text("更新"))
                    .on_press(Message::RefreshFiles)
                    .style(button::secondary)
            ]
            .spacing(8),
        );
        detail = detail.push(delete);
        if let Some((action, ids)) = &app.pending_nas_action {
            let deleting = *action == NasFileAction::DeleteLocal;
            let bytes: u64 = app
                .files
                .iter()
                .filter(|f| ids.contains(&f.file_id))
                .map(|f| f.size_bytes)
                .sum();
            let mut confirm = column![
                text(format!("選択した {} 件（{}）を{}しますか？", ids.len(), format_bytes(bytes), if deleting { "削除" } else { "転送中止" })).size(16),
                text(if deleting { "PC 内のコピーと転送キューを削除します。元に戻せません。NAS と元の SD は変更しません。削除を含むバックアップは全件成功にはしません。" } else { "PC 内のコピーは残り、自動転送と自動削除が止まります。「再試行」で再開できます。NAS 全件成功とは扱いません。" }).size(12),
            ].spacing(6);
            for f in app
                .files
                .iter()
                .filter(|f| ids.contains(&f.file_id))
                .take(5)
            {
                confirm = confirm.push(text(format!("・{}", f.source_rel_path)).size(12));
            }
            confirm = confirm.push(
                row![
                    button(text(if deleting {
                        "PC コピーと転送キューを削除する"
                    } else {
                        "転送を中止する"
                    }))
                    .on_press(Message::ConfirmNasAction)
                    .style(button::danger),
                    button(text("戻る"))
                        .on_press(Message::CancelNasAction)
                        .style(button::secondary),
                ]
                .spacing(8),
            );
            detail = detail.push(
                container(confirm)
                    .padding(10)
                    .width(Fill)
                    .style(theme::card(theme::YELLOW)),
            );
        }
        if app.files.is_empty() {
            detail = detail.push(
                text(if app.files_loading {
                    "ファイルを取得中…"
                } else {
                    "この条件に該当するファイルはありません"
                })
                .color(theme::GRAY),
            );
        }
        for f in &app.files {
            let mut select =
                checkbox(app.selected_files.contains(&f.file_id)).label(f.source_rel_path.clone());
            if can_manage && file_is_actionable(f) {
                let id = f.file_id.clone();
                select = select.on_toggle(move |on| Message::ToggleFile(id.clone(), on));
            }
            let mut info = format!(
                "{} · {} · NAS 試行 {} 回",
                nas_state_label(&f.nas_state),
                format_bytes(f.size_bytes),
                f.nas_attempts
            );
            if let Some(next) = &f.next_retry_utc {
                info.push_str(&format!(" · 次回 {}", local_time(next)));
            }
            let mut file = column![select, text(info).size(12).color(theme::GRAY)].spacing(3);
            if let Some(error) = &f.error {
                file = file.push(text(error.clone()).size(12).color(theme::YELLOW));
            }
            if !matches!(
                f.stage_state.as_str(),
                "SSD_VERIFIED" | "DELETE_PENDING" | "DISCARDED"
            ) {
                file = file.push(
                    text(format!(
                        "PC への取り込み: {}（NAS 再試行には再取り込みが必要）",
                        f.stage_state
                    ))
                    .size(12)
                    .color(theme::GRAY),
                );
            }
            detail = detail.push(
                container(file)
                    .padding(8)
                    .width(Fill)
                    .style(theme::card(theme::GRAY)),
            );
        }
        let mut previous = button(text("前の200件")).style(button::secondary);
        let mut next = button(text("次の200件")).style(button::secondary);
        if interactive && app.files_offset > 0 {
            previous = previous.on_press(Message::FilesPage(app.files_offset.saturating_sub(200)));
        }
        if interactive && (app.files_offset as u64 + app.files.len() as u64) < app.files_total {
            next = next.on_press(Message::FilesPage(app.files_offset + 200));
        }
        detail = detail.push(
            row![
                previous,
                text(format!(
                    "{}〜{} / {} 件",
                    if app.files.is_empty() {
                        0
                    } else {
                        app.files_offset as u64 + 1
                    },
                    app.files_offset as u64 + app.files.len() as u64,
                    app.files_total
                )),
                next
            ]
            .spacing(8)
            .align_y(iced::Center),
        );
        detail = detail.push(text("元の構成で復元（編集ソフト向け）").size(16));
        detail = detail.push(
            row![
                text_input("復元先フォルダ（絶対パス）", &app.restore_dest)
                    .on_input(Message::RestoreDest)
                    .width(Fill),
                button(text("復元")).on_press(Message::RestoreSession(id.clone())),
            ]
            .spacing(6),
        );
        detail = detail.push(
            text("復元元（NAS/SSD）は変更しません。既存ファイルは上書きしません。")
                .size(12)
                .color(theme::GRAY),
        );
        if let Some(session) = app.sessions.iter().find(|s| s.session_id == *id)
            && !session.staging_deleted
            && session.state != "COMPLETE"
            && session.state != "DISCARDED"
        {
            detail = detail.push(
                column![
                    text("NAS 未検証の SSD ストックを削除").size(16),
                    text("NAS に保存・検証されていないデータも含めて、このセッションの SSD データと転送待ちを破棄します。元に戻せません。")
                        .size(12)
                        .color(theme::RED),
                    button(text("SSD ストックを削除（確認）"))
                        .on_press(Message::DiscardSession(id.clone()))
                        .style(button::danger),
                ]
                .spacing(4),
            );
        }
    }

    let export = column![
        text("診断ログのエクスポート（パスワード等は伏せ字）").size(16),
        row![
            text_input("出力先フォルダ（絶対パス）", &app.export_dir)
                .on_input(Message::ExportDir)
                .width(Fill),
            button(text("書き出す")).on_press(Message::ExportLogs),
        ]
        .spacing(6),
    ]
    .spacing(6);

    let mut events = column![text("操作履歴・エラー（30 日）").size(18)].spacing(2);
    for e in &app.events {
        let color = match e.level.as_str() {
            "error" => theme::RED,
            "warn" => theme::YELLOW,
            _ => theme::GRAY,
        };
        events = events.push(
            text(format!("{}  {}", local_time(&e.created_at_utc), e.message))
                .size(13)
                .color(color),
        );
    }

    scrollable(
        column![
            container(scrollable(sessions).height(180))
                .padding(10)
                .width(Fill)
                .style(theme::card(theme::GRAY)),
            detail,
            export,
            container(events)
                .padding(10)
                .width(Fill)
                .style(theme::card(theme::GRAY)),
        ]
        .spacing(12),
    )
    .into()
}

fn nas_state_label(state: &str) -> &str {
    match state {
        "NOT_READY" => "PC への取り込み待ち",
        "QUEUED" => "NAS 転送待ち",
        "UPLOADING" => "NAS 転送中",
        "VERIFYING" => "NAS 検証中",
        "RETRY_WAIT" => "再試行待ち",
        "NAS_FAILED" => "転送失敗",
        "NAS_VERIFIED" => "NAS 検証済み",
        "CANCELLED" => "中止（PC に保持）",
        "DELETE_PENDING" => "削除待ち（削除を再実行できます）",
        "DELETED" => "PC コピー・転送キュー削除済み",
        other => other,
    }
}
