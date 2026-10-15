//! 内蔵 SSD 上のステージング領域（7.1）。
//!
//! ```text
//! <root>/sessions/<session_id>/{Photos,Videos,Other}/...
//! <root>/sessions/<session_id>/.partial/<file_id>.part
//! <root>/sessions/<session_id>/manifest.json
//! ```
//!
//! 削除は `sessions/<session_id>` に限定し、symlink・マウント境界・所有者を検証してから行う。

use backup_core::paths::validate_relative;
use backup_core::session::is_valid_session_id;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StagingError {
    #[error(
        "ステージングフォルダがシンボリックリンクです（NAS や SD を指す可能性があるため拒否）: {0}"
    )]
    Symlink(PathBuf),
    #[error("ステージングフォルダがディレクトリではありません: {0}")]
    NotDirectory(PathBuf),
    #[error("ステージングフォルダの所有者が異なります: {0}")]
    WrongOwner(PathBuf),
    #[error("不正なセッション ID です: {0}")]
    InvalidSessionId(String),
    #[error("不正な格納パスです: {0}")]
    InvalidPath(String),
    #[error("ステージング領域の外です: {0}")]
    OutsideRoot(PathBuf),
    #[error("マウント境界を含むため削除しません: {0}")]
    MountBoundary(PathBuf),
    #[error("使用中のファイルがあるため削除しません ({0} 件)")]
    InUse(usize),
    #[error("I/O エラー: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct Staging {
    root: PathBuf,
    root_dev: u64,
}

impl Staging {
    /// ルートを作成/検証する。ルート自体がシンボリックリンクなら拒否。
    pub fn open(root: &Path) -> Result<Self, StagingError> {
        match fs::symlink_metadata(root) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(StagingError::Symlink(root.to_path_buf()));
            }
            Ok(m) if !m.is_dir() => return Err(StagingError::NotDirectory(root.to_path_buf())),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(root)?;
            }
            Err(e) => return Err(e.into()),
        }
        let meta = fs::metadata(root)?;
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(StagingError::WrongOwner(root.to_path_buf()));
        }
        let root = fs::canonicalize(root)?;
        let sessions = root.join("sessions");
        match fs::symlink_metadata(&sessions) {
            Ok(m) if m.file_type().is_symlink() => return Err(StagingError::Symlink(sessions)),
            Ok(_) => {}
            Err(_) => fs::DirBuilder::new().mode(0o700).create(&sessions)?,
        }
        Ok(Self {
            root_dev: meta.dev(),
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn session_dir(&self, session_id: &str) -> Result<PathBuf, StagingError> {
        if !is_valid_session_id(session_id) {
            return Err(StagingError::InvalidSessionId(session_id.to_string()));
        }
        Ok(self.root.join("sessions").join(session_id))
    }

    pub fn partial_path(&self, session_id: &str, file_id: &str) -> Result<PathBuf, StagingError> {
        if !file_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') || file_id.is_empty() {
            return Err(StagingError::InvalidPath(file_id.to_string()));
        }
        Ok(self
            .session_dir(session_id)?
            .join(".partial")
            .join(format!("{file_id}.part")))
    }

    pub fn final_path(&self, session_id: &str, dest_rel: &str) -> Result<PathBuf, StagingError> {
        let comps =
            validate_relative(dest_rel).map_err(|e| StagingError::InvalidPath(e.to_string()))?;
        let mut p = self.session_dir(session_id)?;
        for c in comps {
            p.push(c);
        }
        Ok(p)
    }

    pub fn manifest_path(&self, session_id: &str) -> Result<PathBuf, StagingError> {
        Ok(self
            .session_dir(session_id)?
            .join(backup_core::manifest::MANIFEST_FILE_NAME))
    }

    /// 全セッションの `.partial` 内の残骸を破棄する（起動時の復旧）。
    pub fn clear_partials(&self) -> Result<usize, StagingError> {
        let mut removed = 0;
        let sessions = self.root.join("sessions");
        for entry in fs::read_dir(&sessions)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_valid_session_id(name) {
                continue;
            }
            let partial = entry.path().join(".partial");
            let Ok(meta) = fs::symlink_metadata(&partial) else {
                continue;
            };
            if !meta.is_dir() || meta.file_type().is_symlink() {
                continue;
            }
            for f in fs::read_dir(&partial)? {
                let f = f?;
                if f.file_type()?.is_file() && f.file_name().to_string_lossy().ends_with(".part") {
                    fs::remove_file(f.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// セッションのステージングデータを削除する（9.4 / 11-5）。
    /// 呼び出し前に `policy::check_staging_deletion` を通過していること。
    pub fn remove_session(&self, session_id: &str) -> Result<(), StagingError> {
        let dir = self.session_dir(session_id)?;
        let meta = match fs::symlink_metadata(&dir) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if meta.file_type().is_symlink() {
            return Err(StagingError::Symlink(dir));
        }
        if !meta.is_dir() {
            return Err(StagingError::NotDirectory(dir));
        }
        let canonical = fs::canonicalize(&dir)?;
        if canonical.parent() != Some(&self.root.join("sessions")) {
            return Err(StagingError::OutsideRoot(canonical));
        }
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(StagingError::WrongOwner(dir));
        }
        self.check_no_mount_boundary(&canonical)?;
        let open = open_handles_under(&canonical);
        if open > 0 {
            return Err(StagingError::InUse(open));
        }
        // std::fs::remove_dir_all はシンボリックリンクを辿らない
        fs::remove_dir_all(&canonical)?;
        crate::sync_dir(&self.root.join("sessions"))?;
        Ok(())
    }

    fn check_no_mount_boundary(&self, dir: &Path) -> Result<(), StagingError> {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let m = fs::symlink_metadata(&d)?;
            if m.dev() != self.root_dev {
                return Err(StagingError::MountBoundary(d));
            }
            if m.is_dir() {
                for e in fs::read_dir(&d)? {
                    let e = e?;
                    let ft = e.file_type()?;
                    if ft.is_dir() {
                        stack.push(e.path());
                    } else {
                        let em = fs::symlink_metadata(e.path())?;
                        if em.dev() != self.root_dev {
                            return Err(StagingError::MountBoundary(e.path()));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// セッションディレクトリの使用量（表示・診断用）。
    pub fn session_bytes(&self, session_id: &str) -> u64 {
        let Ok(dir) = self.session_dir(session_id) else {
            return 0;
        };
        dir_size(&dir)
    }
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// `dir` 配下のファイルを開いているプロセス数（Linux: /proc/*/fd を走査）。
pub fn open_handles_under(dir: &Path) -> usize {
    #[cfg(target_os = "linux")]
    {
        let mut count = 0;
        let Ok(procs) = fs::read_dir("/proc") else {
            return 0;
        };
        for p in procs.flatten() {
            let name = p.file_name();
            if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(fds) = fs::read_dir(p.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                if let Ok(target) = fs::read_link(fd.path())
                    && target.starts_with(dir)
                {
                    count += 1;
                }
            }
        }
        count
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "0192a1b2-0000-7000-8000-000000000001";

    #[test]
    fn creates_root_and_rejects_symlink_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("stage");
        let s = Staging::open(&root).unwrap();
        assert!(s.root().join("sessions").is_dir());

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(matches!(
            Staging::open(&link),
            Err(StagingError::Symlink(_))
        ));
    }

    #[test]
    fn path_validation() {
        let dir = tempfile::tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        assert!(s.session_dir("../../etc").is_err());
        assert!(s.final_path(SID, "Photos/../../x").is_err());
        assert!(s.final_path(SID, "/abs").is_err());
        let p = s.final_path(SID, "Photos/P01/A.JPG").unwrap();
        assert!(p.starts_with(s.root()));
        assert!(s.partial_path(SID, "../x").is_err());
    }

    #[test]
    fn removes_only_the_session_and_not_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let s = Staging::open(&dir.path().join("stage")).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious.txt"), b"keep").unwrap();

        let sess = s.session_dir(SID).unwrap();
        fs::create_dir_all(sess.join("Photos/P01")).unwrap();
        fs::write(sess.join("Photos/P01/a.jpg"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, sess.join("evil-link")).unwrap();

        let other = s
            .session_dir("0192a1b2-0000-7000-8000-000000000002")
            .unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("keep.jpg"), b"y").unwrap();

        s.remove_session(SID).unwrap();
        assert!(!sess.exists());
        assert!(
            outside.join("precious.txt").exists(),
            "symlink target must survive"
        );
        assert!(
            other.join("keep.jpg").exists(),
            "other sessions must survive"
        );
        // 存在しないセッションの削除は成功扱い
        s.remove_session(SID).unwrap();
    }

    #[test]
    fn refuses_symlinked_session_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = Staging::open(&dir.path().join("stage")).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, s.session_dir(SID).unwrap()).unwrap();
        assert!(matches!(
            s.remove_session(SID),
            Err(StagingError::Symlink(_))
        ));
        assert!(outside.join("precious.txt").exists());
    }

    #[test]
    fn clears_partials() {
        let dir = tempfile::tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        let p = s
            .partial_path(SID, "0192a1b2-0000-7000-8000-00000000000a")
            .unwrap();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"half").unwrap();
        let done = s.final_path(SID, "Photos/P01/a.jpg").unwrap();
        fs::create_dir_all(done.parent().unwrap()).unwrap();
        fs::write(&done, b"ok").unwrap();
        assert_eq!(s.clear_partials().unwrap(), 1);
        assert!(!p.exists());
        assert!(done.exists());
    }
}
