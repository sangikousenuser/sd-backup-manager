//! OS 依存のファイル操作をまとめた層（Linux/macOS と Windows）。
//!
//! 上位のコードはここ経由で次を行う:
//! - シンボリックリンク・リパースポイントを辿らない読み込み専用オープン
//! - mtime の取得（ns、UNIX エポック基準）
//! - 本人だけが読めるファイル/ディレクトリの作成
//! - ディレクトリエントリの永続化（親ディレクトリの fsync）
//! - ボリューム（デバイス）の識別

use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// シンボリックリンク（Windows ではリパースポイント全般: シンボリックリンク・ジャンクション・
/// マウントポイント）か。これらは取り込み・削除の対象として辿らない。
pub fn is_link_like(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

/// リンクを辿らずに読み込み専用で開く。
pub fn open_read_only(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
            .open(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT でリンク先ではなくリンク自体を開き、リンクなら拒否する
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if is_link_like(&f.metadata()?) {
            return Err(io::Error::other(
                "refusing to follow a symbolic link or reparse point",
            ));
        }
        Ok(f)
    }
}

/// 本人だけが読み書きできる新規ファイル（既存なら失敗）。
pub fn create_new_private(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    // Windows: ユーザープロファイル配下の既定 ACL（本人・SYSTEM・Administrators）に従う
    o.open(path)
}

/// 本人だけが入れるディレクトリを再帰的に作る。
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(path)
}

/// ファイルの mtime（UNIX エポックからの ns）。
pub fn mtime_ns(meta: &Metadata) -> i64 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos().min(i64::MAX as u128) as i64,
            Err(e) => -(e.duration().as_nanos().min(i64::MAX as u128) as i64),
        },
        Err(_) => 0,
    }
}

/// 現在のユーザーが所有しているか（Windows では ACL に委ね、常に true）。
pub fn owned_by_current_user(meta: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.uid() == nix::unistd::geteuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

/// ページキャッシュを捨てて、読み戻しが実際のストレージから行われるよう促す（Linux のみ）。
pub fn drop_page_cache(file: &File) {
    #[cfg(target_os = "linux")]
    {
        use nix::fcntl::{PosixFadviseAdvice, posix_fadvise};
        let _ = posix_fadvise(file, 0, 0, PosixFadviseAdvice::POSIX_FADV_DONTNEED);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = file;
}

/// ディレクトリエントリの永続化（親ディレクトリの fsync）。
/// Windows（NTFS/exFAT）はディレクトリの fsync を提供しないため、メタデータは
/// ファイルシステムのジャーナルに任せる（ファイル本体は sync_all 済み）。
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// 同じボリューム上かを比べるための識別子。
/// Unix: st_dev。Windows: ボリュームのマウント先パス（`C:\`、`\\server\share\` 等）の FNV-1a。
pub fn volume_id(path: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::metadata(path)?.dev())
    }
    #[cfg(windows)]
    {
        let root = windows::volume_path_name(path)?;
        Ok(fnv1a(root.to_lowercase().as_bytes()))
    }
}

/// エントリ単位の識別子（ディレクトリ走査中のマウント境界チェック用。Unix は st_dev を直接使う）。
pub fn meta_volume_id(meta: &Metadata, path: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        Ok(meta.dev())
    }
    #[cfg(windows)]
    {
        let _ = meta;
        volume_id(path)
    }
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(windows)]
pub mod windows {
    //! Win32 ヘルパー。
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumePathNameW};

    pub fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// `C:\foo\bar` → `C:\`、`\\server\share\x` → `\\server\share\`
    pub fn volume_path_name(path: &Path) -> io::Result<String> {
        let w = wide(path.as_os_str());
        let mut buf = vec![0u16; 1024];
        let ok = unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(from_wide(&buf))
    }

    /// (利用可能な空き, 総容量)
    pub fn disk_space(path: &Path) -> io::Result<(u64, u64)> {
        let mut p = path.as_os_str().to_owned();
        // UNC パスは末尾に区切りが必要
        if !p.to_string_lossy().ends_with('\\') {
            p.push("\\");
        }
        let w = wide(&p);
        let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, &mut total_free) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((free, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtime_is_positive_for_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a");
        fs::write(&p, b"x").unwrap();
        assert!(mtime_ns(&fs::metadata(&p).unwrap()) > 1_600_000_000_000_000_000);
    }

    #[test]
    fn private_file_is_create_new() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        create_new_private(&p).unwrap();
        assert!(create_new_private(&p).is_err());
    }

    #[test]
    fn same_volume_for_siblings() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        assert_eq!(
            volume_id(dir.path()).unwrap(),
            volume_id(&dir.path().join("a")).unwrap()
        );
    }
}
