//! メイン画面（10.1）: 左右にメディア 1・2、下に NAS 転送。

use super::{eta_text, rate_text};
use crate::app::{App, Message, Screen};
use crate::theme;
use backup_core::identity::format_bytes;
use backup_ipc::{Command, SlotPhase, SlotView};
use iced::widget::{button, column, container, progress_bar, row, scrollable, space, text};
use iced::{Element, Fill};

pub fn view(app: &App) -> Element<'_, Message> {
    let Some(snap) = &app.snapshot else {
        return container(text("バックグラウンドサービスの起動を待っています…").size(18))
            .padding(40)
            .into();
    };

    let cards = row(snap.slots.iter().map(slot_card))
        .spacing(12)
        .height(iced::Length::Fixed(330.0));

    let mut col = column![cards].spacing(12);
    if !snap.waiting.is_empty() {
        let mut w = column![text("待機中のメディア").size(16)].spacing(4);
        for m in &snap.waiting {
            w = w.push(text(format!("◌ {} — {}", m.media_name, m.reason)));
        }
        col = col.push(
            container(w)
                .padding(10)
                .width(Fill)
                .style(theme::card(theme::GRAY)),
        );
    }
    col = col.push(nas_card(snap));
    for warn in &snap.warnings {
        col = col.push(text(format!("▲ {warn}")).color(theme::YELLOW));
    }
    col = col.push(
        text("※ 未知の形式を含むすべてのファイルを保存します（写真・動画以外は Other/ に入ります）。元メディアには書き込みません。")
            .size(12)
            .color(theme::GRAY),
    );
    scrollable(col).into()
}

fn slot_card(slot: &SlotView) -> Element<'_, Message> {
    let color = theme::phase_color(slot.phase);
    let title = text(format!("SD / メディア {}", slot.slot_index + 1)).size(18);
    let mut col = column![title].spacing(6);

    if slot.phase == SlotPhase::Empty {
        col = col.push(
            text(format!("{} {}", slot.phase.symbol(), slot.phase.label()))
                .color(color)
                .size(16),
        );
        return container(col)
            .padding(14)
            .width(Fill)
            .height(Fill)
            .style(theme::card(color))
            .into();
    }

    col = col.push(text(slot.media_name.clone().unwrap_or_default()));
    col = col.push(
        text(format!("{} {}", slot.phase.symbol(), slot.message))
            .color(color)
            .size(16),
    );

    let fraction = if slot.bytes_total > 0 {
        slot.bytes_done as f32 / slot.bytes_total as f32
    } else if slot.phase.removable() {
        1.0
    } else {
        0.0
    };
    col = col.push(text(format!("{:.0}%", fraction * 100.0)).size(28));
    col = col.push(
        progress_bar(0.0..=1.0, fraction)
            .girth(14)
            .style(theme::bar(color)),
    );
    col = col.push(text(format!(
        "{} / {}",
        format_bytes(slot.bytes_done),
        format_bytes(slot.bytes_total)
    )));
    if slot.phase == SlotPhase::Copying {
        col = col.push(text(format!(
            "{} | {}",
            rate_text(slot.rate_bytes_per_sec),
            eta_text(slot.eta_secs)
        )));
    }
    let mut files = format!("{} / {} ファイル", slot.files_done, slot.files_total);
    if slot.files_skipped > 0 {
        files.push_str(&format!("（既存 {} 件）", slot.files_skipped));
    }
    col = col.push(text(files));
    if slot.files_failed > 0 {
        col = col.push(text(format!("✖ 失敗 {} 件", slot.files_failed)).color(theme::RED));
    }
    if let Some(folder) = &slot.folder_name {
        col = col.push(
            text(format!("フォルダ {folder}"))
                .size(12)
                .color(theme::GRAY),
        );
    }

    if slot.removable {
        let (msg, c) = if slot.phase == SlotPhase::DoneGreen {
            ("✔ 取り外し可能", theme::GREEN)
        } else {
            ("▲ 取り外し可能（失敗あり・要確認）", theme::YELLOW)
        };
        col = col.push(
            container(text(msg).size(18))
                .padding(8)
                .width(Fill)
                .style(theme::banner(c)),
        );
    } else if slot.phase == SlotPhase::UnmountFailed {
        col = col.push(
            container(text("✖ まだ取り外さないでください").size(18))
                .padding(8)
                .width(Fill)
                .style(theme::banner(theme::RED)),
        );
    }

    let mut buttons = row![
        button(text("詳細を表示"))
            .on_press(Message::Navigate(Screen::Details(slot.slot_index)))
            .style(button::secondary)
    ]
    .spacing(6);
    if slot.phase == SlotPhase::UnmountFailed {
        buttons = buttons.push(button(text("アンマウント再試行")).on_press(Message::Run(
            Command::RetryUnmount {
                slot: slot.slot_index,
            },
        )));
    }
    col = col.push(space::vertical()).push(buttons);
    container(col)
        .padding(14)
        .width(Fill)
        .height(Fill)
        .style(theme::card(color))
        .into()
}

fn nas_card(snap: &backup_ipc::Snapshot) -> Element<'_, Message> {
    let n = &snap.nas;
    let color = if !n.configured {
        theme::GRAY
    } else if n.needs_attention {
        theme::RED
    } else if !n.connected {
        theme::YELLOW
    } else if n.queued_files == 0 && n.sessions_pending == 0 {
        theme::GREEN
    } else {
        theme::BLUE
    };
    let symbol = if !n.connected { "▲" } else { "●" };
    let mut col = column![
        row![
            text("NAS 転送").size(18),
            space::horizontal().width(12),
            text(format!("{symbol} {}", n.status_text)).color(color),
            space::horizontal(),
            text(n.target.clone()).size(12).color(theme::GRAY),
        ]
        .align_y(iced::Center)
    ]
    .spacing(6);

    let fraction = if n.current_bytes_total > 0 {
        n.current_bytes_done as f32 / n.current_bytes_total as f32
    } else {
        0.0
    };
    col = col.push(
        progress_bar(0.0..=1.0, fraction)
            .girth(10)
            .style(theme::bar(color)),
    );
    let current = n
        .current_file
        .clone()
        .map(|f| format!("転送中: {f}"))
        .unwrap_or_else(|| "転送中のファイルなし".into());
    col = col.push(text(current).size(13));
    col = col.push(text(format!(
        "{} | 残り {} 件 / {}（再試行待ち {} 件）| NAS 処理待ちセッション {} 件",
        rate_text(n.rate_bytes_per_sec),
        n.queued_files,
        format_bytes(n.queued_bytes),
        n.retry_wait_files,
        n.sessions_pending
    )));
    if let Some(e) = &n.last_error {
        let mut msg = format!("最終エラー: {e}");
        if n.attempts > 0 {
            msg.push_str(&format!("（試行 {} 回", n.attempts));
            if let Some(next) = &n.next_retry_utc {
                msg.push_str(&format!("、次回 {}", super::local_time(next)));
            }
            msg.push('）');
        }
        col = col.push(text(msg).size(13).color(if n.needs_attention {
            theme::RED
        } else {
            theme::YELLOW
        }));
    }
    let pause = if snap.paused {
        button(text("再開")).on_press(Message::Run(Command::Resume))
    } else {
        button(text("一時停止"))
            .on_press(Message::Run(Command::Pause))
            .style(button::secondary)
    };
    col = col.push(
        row![
            button(text("今すぐ再試行")).on_press(Message::Run(Command::RetryNas)),
            pause,
            button(text("履歴"))
                .on_press(Message::Navigate(Screen::History))
                .style(button::secondary),
            button(text("設定"))
                .on_press(Message::Navigate(Screen::Settings))
                .style(button::secondary),
        ]
        .spacing(6),
    );
    container(col)
        .padding(14)
        .width(Fill)
        .style(theme::card(color))
        .into()
}
