//! ファイル I/O 層。ソースメディアは読み込み専用でのみ開き、書き込みは
//! アプリ所有のステージング領域と NAS 上のセッションフォルダに限定する。

pub mod atomic;
pub mod capacity;
pub mod hash;
pub mod local;
pub mod nas;
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

/// ページキャッシュを捨てて、読み戻しが実際のストレージから行われるよう促す（Linux のみ）。
pub(crate) fn drop_page_cache(file: &std::fs::File) {
    #[cfg(target_os = "linux")]
    {
        use nix::fcntl::{PosixFadviseAdvice, posix_fadvise};
        let _ = posix_fadvise(file, 0, 0, PosixFadviseAdvice::POSIX_FADV_DONTNEED);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = file;
}

/// ディレクトリエントリの永続化（親ディレクトリの fsync）。
pub(crate) fn sync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}
