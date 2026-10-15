//! 色とカードのスタイル。状態は色だけでなく記号と文言でも示す（FR-036）。

use backup_ipc::SlotPhase;
use iced::widget::container;
use iced::{Background, Border, Color, Theme};

pub const GREEN: Color = Color::from_rgb(0.18, 0.49, 0.20);
pub const YELLOW: Color = Color::from_rgb(0.85, 0.60, 0.05);
pub const RED: Color = Color::from_rgb(0.78, 0.16, 0.16);
pub const BLUE: Color = Color::from_rgb(0.08, 0.40, 0.75);
pub const GRAY: Color = Color::from_rgb(0.45, 0.45, 0.45);

pub fn phase_color(phase: SlotPhase) -> Color {
    match phase {
        SlotPhase::Empty => GRAY,
        SlotPhase::Reading | SlotPhase::Copying | SlotPhase::Finalizing | SlotPhase::Unmounting => {
            BLUE
        }
        SlotPhase::CapacityWait | SlotPhase::DoneYellow => YELLOW,
        SlotPhase::DoneGreen => GREEN,
        SlotPhase::Blocked
        | SlotPhase::UnmountFailed
        | SlotPhase::Interrupted
        | SlotPhase::Error => RED,
    }
}

pub fn card(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme: &Theme| {
        let palette = theme.extended_palette();
        container::Style {
            background: Some(Background::Color(palette.background.base.color)),
            border: Border {
                color: accent,
                width: 2.0,
                radius: 8.0.into(),
            },
            ..container::Style::default()
        }
    }
}

pub fn banner(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme: &Theme| container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(if color == YELLOW {
            Color::BLACK
        } else {
            Color::WHITE
        }),
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

pub fn bar(color: Color) -> impl Fn(&Theme) -> iced::widget::progress_bar::Style {
    move |theme: &Theme| iced::widget::progress_bar::Style {
        background: Background::Color(theme.extended_palette().background.strong.color),
        bar: Background::Color(color),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
    }
}
