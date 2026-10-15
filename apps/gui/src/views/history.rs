//! 履歴（FR-035）: セッション一覧・問題のあるファイル・元の構成で復元・診断エクスポート・30 日の操作履歴。

use super::{local_time, session_state_label};
use crate::app::{App, Message};
use crate::theme;
use backup_core::identity::format_bytes;
use iced::widget::{button, column, container, row, scrollable, text, text_input};
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
        detail = detail.push(text("未完了・失敗・NAS 未検証のファイル").size(16));
        if app.files.is_empty() {
            detail = detail.push(text("問題のあるファイルはありません").color(theme::GREEN));
        }
        for f in app.files.iter().take(200) {
            detail = detail.push(
                text(format!(
                    "{}  [{} / {}]  {}  試行 {} 回{}",
                    f.source_rel_path,
                    f.stage_state,
                    f.nas_state,
                    format_bytes(f.size_bytes),
                    f.copy_attempts,
                    f.error
                        .as_ref()
                        .map(|e| format!("  — {e}"))
                        .unwrap_or_default()
                ))
                .size(13),
            );
        }
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
            container(sessions)
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
