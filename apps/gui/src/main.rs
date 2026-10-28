// Windows ではコンソールウィンドウを出さない
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! SD Backup Manager GUI（Iced）。
//!
//! GUI はバックアップ状態を持たず、デーモンから配信されるスナップショットを表示するだけ。
//! GUI を閉じてもデーモンは転送を続ける（FR-034）。

mod app;
mod fonts;
mod ipc;
mod platform;
mod sound;
mod theme;
mod views;

fn main() -> iced::Result {
    let mut application = iced::application(app::App::new, app::App::update, app::App::view)
        .title(app::App::title)
        .subscription(app::App::subscription)
        .theme(app::App::theme)
        .window_size(iced::Size::new(1024.0, 720.0));
    if let Some((bytes, family)) = fonts::find_japanese_font() {
        application = application
            .font(bytes)
            .default_font(iced::Font::with_name(family));
    }
    application.run()
}
