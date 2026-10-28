pub mod details;
pub mod history;
pub mod main;
pub mod settings;

use crate::app::{App, Message, Screen};
use crate::theme;
use backup_core::identity::format_bytes;
use iced::widget::{button, column, container, row, rule, space, text};
use iced::{Element, Fill, Length};

/// 共通の外枠: 上部の状態バー・ナビゲーション・結果表示。
pub fn frame<'a>(app: &'a App, body: Element<'a, Message>) -> Element<'a, Message> {
    let status: Element<'a, Message> = match (&app.snapshot, &app.connection_error) {
        (_, Some(e)) => text(format!("✖ バックグラウンドサービスに接続できません（{e}）"))
            .color(theme::RED)
            .into(),
        (Some(s), None) => {
            let ssd = match (s.ssd.free_bytes, s.ssd.total_bytes) {
                (Some(f), Some(t)) => format!("SSD 空き {} / {}", format_bytes(f), format_bytes(t)),
                _ => "SSD 空き 取得不可".into(),
            };
            let nas = if !s.nas.configured {
                "NAS 未設定".to_string()
            } else {
                match s.nas.free_bytes {
                    Some(f) => format!("NAS 空き {}", format_bytes(f)),
                    None => "NAS 空き 取得不可".into(),
                }
            };
            row![
                text(if s.paused {
                    "⏸ 一時停止中"
                } else {
                    "● 稼働中"
                })
                .color(if s.paused {
                    theme::YELLOW
                } else {
                    theme::GREEN
                }),
                space::horizontal().width(24),
                text(ssd),
                text(format!(
                    "（最低確保 {}）",
                    format_bytes(s.ssd.reserve_bytes)
                ))
                .size(12)
                .color(theme::GRAY),
                space::horizontal().width(24),
                text(nas),
            ]
            .spacing(6)
            .align_y(iced::Center)
            .into()
        }
        (None, None) => text("接続中…").into(),
    };

    let nav = row![
        nav_button("メイン", Screen::Main, app.screen),
        nav_button("履歴", Screen::History, app.screen),
        nav_button("設定", Screen::Settings, app.screen),
    ]
    .spacing(6);

    let mut col = column![
        row![status, space::horizontal(), nav].align_y(iced::Center),
        rule::horizontal(1)
    ]
    .spacing(8);
    if let Some((ok, msg)) = &app.toast {
        col = col.push(
            container(text(format!("{} {msg}", if *ok { "✔" } else { "✖" })))
                .padding(8)
                .width(Fill)
                .style(theme::banner(if *ok { theme::GREEN } else { theme::RED })),
        );
    }
    if app.busy {
        col = col.push(text("処理中…").color(theme::BLUE));
    }
    col = col.push(body);
    container(col).padding(16).width(Fill).height(Fill).into()
}

fn nav_button(label: &str, target: Screen, current: Screen) -> Element<'_, Message> {
    let active = std::mem::discriminant(&target) == std::mem::discriminant(&current);
    let b = button(text(label)).on_press(Message::Navigate(target));
    if active {
        b.style(button::primary).into()
    } else {
        b.style(button::secondary).into()
    }
}

pub fn eta_text(secs: Option<u64>) -> String {
    match secs {
        None => "残り 計算中".into(),
        Some(s) if s < 60 => format!("残り {s}秒"),
        Some(s) if s < 3600 => format!("残り {}分", s.div_ceil(60)),
        Some(s) => format!("残り {}時間{}分", s / 3600, (s % 3600) / 60),
    }
}

pub fn rate_text(rate: Option<f64>) -> String {
    match rate {
        Some(r) => format!("{:.1} MB/s", r / 1_000_000.0),
        None => "-- MB/s".into(),
    }
}

/// UTC の DB 時刻を日本時間で表示。
pub fn local_time(utc: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(utc)
        .map(|t| {
            t.with_timezone(&chrono_tz::Asia::Tokyo)
                .format("%m/%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| utc.to_string())
}

pub fn session_state_label(state: &str) -> &'static str {
    match state {
        "DETECTED" => "検知",
        "SCANNING" => "走査中",
        "BLOCKED" => "待機中",
        "COPYING" => "コピー中",
        "SOURCE_INTERRUPTED" => "中断（再接続待ち）",
        "SOURCE_FINALIZING" => "仕上げ中",
        "UNMOUNTING" => "アンマウント中",
        "MEDIA_GREEN" => "取り込み完了",
        "MEDIA_YELLOW" => "取り込み未完了",
        "UNMOUNT_FAILED" => "アンマウント失敗",
        "NAS_PENDING" => "NAS 転送待ち",
        "NEEDS_ATTENTION" => "要対応",
        "NAS_VERIFIED" => "NAS 検証済み",
        "STAGING_CLEANUP" => "SSD 削除中",
        "COMPLETE" => "完了",
        "DISCARDED" => "SSD データ破棄済み",
        _ => "不明",
    }
}

pub fn labeled<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    row![
        text(label).width(Length::Fixed(160.0)).color(theme::GRAY),
        text(value)
    ]
    .spacing(8)
    .into()
}
