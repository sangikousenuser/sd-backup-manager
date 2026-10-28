//! 設定画面（FR-025、FR-037）。パスワードは伏せ字で入力し、Secret Service に保存される。

use super::labeled;
use crate::app::{App, Message};
use crate::theme;
use backup_core::identity::format_bytes;
use backup_ipc::NotificationKind;
use iced::widget::{button, checkbox, column, container, radio, row, scrollable, text, text_input};
use iced::{Element, Fill};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Disabled,
    Gvfs,
    Mounted,
}

impl Mode {
    fn key(self) -> &'static str {
        match self {
            Mode::Disabled => "disabled",
            Mode::Gvfs => "gvfs",
            Mode::Mounted => "mounted_path",
        }
    }
}

pub fn view(app: &App) -> Element<'_, Message> {
    let s = &app.settings;
    let current = match s.nas_mode.as_str() {
        "gvfs" => Mode::Gvfs,
        "mounted_path" => Mode::Mounted,
        _ => Mode::Disabled,
    };
    let pick = |label: &str, m: Mode| {
        radio(label, m, Some(current), |m: Mode| {
            Message::NasMode(m.key().to_string())
        })
    };

    let mut nas = column![
        text("NAS 接続").size(18),
        pick("未設定（SSD に保持のみ）", Mode::Disabled),
        pick("SMB 共有（推奨: GVfs でユーザー権限接続）", Mode::Gvfs),
        pick("管理者が設定済みのマウントパス（CIFS）", Mode::Mounted),
    ]
    .spacing(8);

    match current {
        Mode::Gvfs => {
            nas = nas
                .push(text_input("smb://サーバー/共有名/フォルダ/", &s.nas_url).on_input(Message::NasUrl))
                .push(text_input("ユーザー名", &s.nas_username).on_input(Message::NasUser))
                .push(text_input("ドメイン（不要なら空欄）", &s.nas_domain).on_input(Message::NasDomain))
                .push(
                    text_input(
                        if app.password_stored { "パスワード（保存済み・変更時のみ入力）" } else { "パスワード" },
                        &app.password,
                    )
                    .on_input(Message::NasPassword)
                    .secure(true),
                )
                .push(text("アドレスにユーザー名・パスワードを含めないでください。パスワードはキーリングに保存されます。").size(12).color(theme::GRAY));
        }
        Mode::Mounted => {
            nas = nas
                .push(text_input("/mnt/nas/FestivalBackup", &s.nas_mounted_path).on_input(Message::NasMountedPath))
                .push(text("ローカルディスクへの誤書き込みを防ぐため、NAS としてマウントされていない場合は転送しません。").size(12).color(theme::GRAY));
        }
        Mode::Disabled => {}
    }
    nas = nas.push(
        checkbox(s.final_rehash_before_cleanup)
            .label("SSD 削除前に NAS 上の全ファイルを再検証する（推奨・時間がかかります）")
            .on_toggle(Message::FinalRehash),
    );
    nas = nas.push(
        text(if s.nas_target_initialized {
            "✔ 保存先は初期化済みです"
        } else {
            "▲ 保存先が未初期化です"
        })
        .color(if s.nas_target_initialized {
            theme::GREEN
        } else {
            theme::YELLOW
        }),
    );
    nas = nas.push(
        row![
            button(text("接続テスト"))
                .on_press(Message::TestNas)
                .style(button::secondary),
            button(text("保存")).on_press(Message::SaveSettings),
            button(text("保存先を初期化"))
                .on_press(Message::InitTarget)
                .style(button::secondary),
        ]
        .spacing(8),
    );
    if let Some(r) = &app.test_report {
        let mut rep = column![
            text(if r.success {
                "✔ 接続テスト成功"
            } else {
                "✖ 接続テスト失敗"
            })
            .color(if r.success { theme::GREEN } else { theme::RED })
        ]
        .spacing(2);
        for (step, ok, msg) in &r.steps {
            rep = rep.push(text(format!("{} {step}: {msg}", if *ok { "✔" } else { "✖" })).size(13));
        }
        nas = nas.push(
            container(rep)
                .padding(8)
                .width(Fill)
                .style(theme::card(if r.success {
                    theme::GREEN
                } else {
                    theme::RED
                })),
        );
    }

    let sounds = column![
        text("通知音").size(18),
        checkbox(s.play_success_sound)
            .label("完了音を鳴らす")
            .on_toggle(Message::SuccessSound),
        checkbox(s.play_warning_sound)
            .label("注意音・警告音を鳴らす")
            .on_toggle(Message::WarningSound),
        row![
            button(text("完了音を試聴"))
                .on_press(Message::PlaySound(NotificationKind::Success))
                .style(button::secondary),
            button(text("注意音を試聴"))
                .on_press(Message::PlaySound(NotificationKind::Warning))
                .style(button::secondary),
            button(text("警告音を試聴"))
                .on_press(Message::PlaySound(NotificationKind::Error))
                .style(button::secondary),
        ]
        .spacing(8),
        text("音の設定は「保存」で反映されます。")
            .size(12)
            .color(theme::GRAY),
    ]
    .spacing(8);

    let info = column![
        text("保存先・安全設定").size(18),
        labeled("SSD 一時保存先", app.snapshot.as_ref().map(|s| s.ssd.path.clone()).unwrap_or_else(|| backup_config::display_path(&backup_config::expand_tilde(&s.staging_directory)))),
        button(text("PC 内の保存先を開く")).on_press(Message::OpenFolder(
            app.snapshot.as_ref().map(|s| s.ssd.path.clone()).unwrap_or_else(|| backup_config::expand_tilde(&s.staging_directory).display().to_string())
        )).style(button::secondary),
        text("通常の Windows 保存先: C:\\Users\\ユーザー名\\SDBackupStaging\\sessions\\sd1\\日時\\").size(12),
        text("NAS の検証完了後に PC 内の一時データを自動削除します。保存先・最低空き容量・同時処理数は変更できません。").size(12).color(theme::GRAY),
        labeled(
            "最低確保空き容量",
            format!("{}（10 進表記）", format_bytes(s.reserve_free_bytes))
        ),
        labeled(
            "同時処理メディア数",
            format!("最大 {} 台", s.max_active_media)
        ),
        labeled("バージョン", backup_core::APP_VERSION.to_string()),
    ]
    .spacing(6);

    scrollable(
        column![
            container(nas)
                .padding(12)
                .width(Fill)
                .style(theme::card(theme::GRAY)),
            container(sounds)
                .padding(12)
                .width(Fill)
                .style(theme::card(theme::GRAY)),
            container(info)
                .padding(12)
                .width(Fill)
                .style(theme::card(theme::GRAY)),
        ]
        .spacing(12),
    )
    .into()
}
