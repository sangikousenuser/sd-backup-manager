//! 空き容量（statvfs）とファイルシステム境界の判定。

use serde::{Deserialize, Serialize};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSpace {
    /// 一般ユーザーが利用可能な空き（f_bavail × f_frsize）。
    pub free_bytes: u64,
    pub total_bytes: u64,
}

pub fn fs_space(path: &Path) -> io::Result<FsSpace> {
    let st = nix::sys::statvfs::statvfs(path).map_err(io::Error::from)?;
    let frsize = st.fragment_size() as u64;
    let frsize = if frsize == 0 {
        st.block_size() as u64
    } else {
        frsize
    };
    Ok(FsSpace {
        free_bytes: (st.blocks_available() as u64).saturating_mul(frsize),
        total_bytes: (st.blocks() as u64).saturating_mul(frsize),
    })
}

/// パスの属するデバイス番号（st_dev）。
pub fn device_of(path: &Path) -> io::Result<u64> {
    Ok(std::fs::metadata(path)?.dev())
}

/// `path` がマウントポイントか（親ディレクトリとデバイスが異なる、またはルート）。
pub fn is_mount_point(path: &Path) -> io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(false);
    }
    let parent = path.join("..");
    let pmeta = std::fs::metadata(&parent)?;
    Ok(meta.dev() != pmeta.dev() || meta.ino() == pmeta.ino())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_space_for_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let s = fs_space(dir.path()).unwrap();
        assert!(s.total_bytes > 0);
        assert!(s.free_bytes <= s.total_bytes);
    }

    #[test]
    fn root_is_mount_point_and_tempdir_child_is_not() {
        assert!(is_mount_point(Path::new("/")).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("c");
        std::fs::create_dir(&child).unwrap();
        assert!(!is_mount_point(&child).unwrap());
    }
}
