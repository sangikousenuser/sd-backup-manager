//! 空き容量（statvfs）とファイルシステム境界の判定。

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSpace {
    /// 一般ユーザーが利用可能な空き（f_bavail × f_frsize）。
    pub free_bytes: u64,
    pub total_bytes: u64,
}

pub fn fs_space(path: &Path) -> io::Result<FsSpace> {
    #[cfg(unix)]
    {
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
    #[cfg(windows)]
    {
        let (free_bytes, total_bytes) = crate::platform::windows::disk_space(path)?;
        Ok(FsSpace {
            free_bytes,
            total_bytes,
        })
    }
}

/// パスの属するボリュームの識別子（Unix: st_dev、Windows: ボリュームのマウント先）。
pub fn device_of(path: &Path) -> io::Result<u64> {
    crate::platform::volume_id(path)
}

/// `path` がマウントポイント（ボリュームのルート）か。
pub fn is_mount_point(path: &Path) -> io::Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.file_type().is_symlink() {
            return Ok(false);
        }
        let parent = path.join("..");
        let pmeta = std::fs::metadata(&parent)?;
        Ok(meta.dev() != pmeta.dev() || meta.ino() == pmeta.ino())
    }
    #[cfg(windows)]
    {
        let _ = meta;
        let root = crate::platform::windows::volume_path_name(path)?;
        let norm = |s: &str| s.trim_end_matches('\\').to_lowercase();
        Ok(norm(&root) == norm(&path.to_string_lossy()))
    }
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
        #[cfg(unix)]
        assert!(is_mount_point(Path::new("/")).unwrap());
        #[cfg(windows)]
        assert!(is_mount_point(Path::new("C:\\")).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("c");
        std::fs::create_dir(&child).unwrap();
        assert!(!is_mount_point(&child).unwrap());
    }
}
