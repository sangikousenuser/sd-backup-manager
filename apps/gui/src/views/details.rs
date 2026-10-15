//! メディアの詳細（10.2 詳細画面）。

use super::{labeled, local_time};
use crate::app::{App, Message, Screen};
use crate::theme;
use iced::Element;
use iced::widget::{button, column, scrollable, text};

pub fn view(app: &App, slot: usize) -> Element<'_, Message> {
    let back = button(text("← 戻る"))
        .on_press(Message::Navigate(Screen::Main))
        .style(button::secondary);
    let Some(s) = app.snapshot.as_ref().and_then(|s| s.slots.get(slot)) else {
        return column![back, text("メディアがありません")]
            .spacing(12)
            .into();
    };
    let d = &s.details;
    let mut col = column![
        back,
        text(format!("メディア {} の詳細", slot + 1)).size(20),
        labeled("状態", format!("{} {}", s.phase.symbol(), s.message)),
        labeled("メディア", s.media_name.clone().unwrap_or_default()),
        labeled(
            "セッション ID",
            s.session_id.clone().unwrap_or_else(|| "-".into())
        ),
        labeled(
            "フォルダ名",
            s.folder_name.clone().unwrap_or_else(|| "-".into())
        ),
        labeled(
            "挿入時刻",
            d.inserted_at_utc
                .as_deref()
                .map(local_time)
                .unwrap_or_default()
        ),
        labeled("デバイス", d.device_path.clone().unwrap_or_default()),
        labeled(
            "識別の信頼度",
            d.identity_confidence.clone().unwrap_or_default()
        ),
        labeled(
            "処理中のファイル",
            d.current_file.clone().unwrap_or_else(|| "-".into())
        ),
    ]
    .spacing(6);
    col = col.push(text("パーティション").size(16));
    for p in &d.partitions {
        col = col.push(text(format!("・{p}")));
    }
    if !d.failed_files.is_empty() {
        col = col.push(
            text(format!(
                "取り込みに失敗したファイル（{} 件）",
                d.failed_files.len()
            ))
            .size(16)
            .color(theme::RED),
        );
        for f in &d.failed_files {
            col = col.push(text(format!("✖ {f}")).size(13));
        }
    }
    if !d.warnings.is_empty() {
        col = col.push(text("警告").size(16).color(theme::YELLOW));
        for w in &d.warnings {
            col = col.push(text(format!("▲ {w}")).size(13));
        }
    }
    scrollable(col).into()
}
