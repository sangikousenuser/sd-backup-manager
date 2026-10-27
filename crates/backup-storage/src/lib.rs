//! ファイル I/O 層。ソースメディアは読み込み専用でのみ開き、書き込みは
//! アプリ所有のステージング領域と NAS 上のセッションフォルダに限定する。

pub mod atomic;
pub mod capacity;
pub mod hash;
pub mod local;
pub mod nas;
pub mod platform;
pub mod restore;

/// 進捗コールバックの戻り値。`Abort` でチャンク境界で安全に中断する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Continue,
    Abort,
}

/// 処理したバイト数（差分）を受け取るコールバック。
pub type Progress<'a> = &'a mut dyn FnMut(u64) -> Control;

/// 何もしない進捗。
pub fn no_progress() -> impl FnMut(u64) -> Control {
    |_| Control::Continue
}

pub(crate) use platform::{drop_page_cache, sync_dir};
