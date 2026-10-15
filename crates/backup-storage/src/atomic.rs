//! SSD への検証付き原子的コピー（FR-019、FR-020）。
//!
//! 1. ソースを読み込み専用（O_NOFOLLOW）で開き、BLAKE3 を計算しながら `.part` へ書き込む
//! 2. flush + fsync、ページキャッシュを破棄
//! 3. ソースのサイズ・mtime が開始時から変わっていないことを確認
//! 4. `.part` を開き直して BLAKE3 を再計算し、ソースのハッシュと照合
//! 5. 同一 FS 内で最終名へ rename し、親ディレクトリを fsync

use crate::hash::{hash_file, open_read_only};
use crate::{Control, Progress};
use backup_core::IO_BUFFER_BYTES;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CopyError {
    #[error("コピー元を読み込めません: {0}")]
    SourceRead(#[source] io::Error),
    #[error("コピー元が通常ファイルではありません")]
    NotRegularFile,
    #[error("コピー中にコピー元が変更されました")]
    SourceChanged,
    #[error("SSD への書き込みに失敗しました: {0}")]
    DestWrite(#[source] io::Error),
    #[error("ハッシュが一致しません (コピー元 {source_hash}, SSD {dest_hash})")]
    Mismatch {
        source_hash: String,
        dest_hash: String,
    },
    #[error("中断しました")]
    Aborted,
}

impl CopyError {
    /// DB に保存するエラーコード。
    pub fn code(&self) -> &'static str {
        match self {
            CopyError::SourceRead(_) => "SOURCE_READ",
            CopyError::NotRegularFile => "NOT_REGULAR_FILE",
            CopyError::SourceChanged => "SOURCE_CHANGED",
            CopyError::DestWrite(_) => "DEST_WRITE",
            CopyError::Mismatch { .. } => "HASH_MISMATCH",
            CopyError::Aborted => "ABORTED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyOutcome {
    pub hash: String,
    pub size: u64,
    pub mtime_ns: i64,
}

/// ファイルのメタデータ（シンボリックリンクは辿らない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceStat {
    pub size: u64,
    pub mtime_ns: i64,
}

pub fn source_stat(path: &Path) -> io::Result<SourceStat> {
    let m = fs::symlink_metadata(path)?;
    Ok(SourceStat {
        size: m.len(),
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
    })
}

pub fn copy_and_verify(
    src: &Path,
    partial: &Path,
    dest: &Path,
    progress: Progress<'_>,
) -> Result<CopyOutcome, CopyError> {
    let before = fs::symlink_metadata(src).map_err(CopyError::SourceRead)?;
    if !before.file_type().is_file() {
        return Err(CopyError::NotRegularFile);
    }
    let before_stat = SourceStat {
        size: before.len(),
        mtime_ns: before.mtime() * 1_000_000_000 + before.mtime_nsec(),
    };

    if let Some(parent) = partial.parent() {
        fs::create_dir_all(parent).map_err(CopyError::DestWrite)?;
    }
    // 以前の未完了 .part は再開情報がないため破棄して先頭からやり直す（9.3-3）
    match fs::remove_file(partial) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(CopyError::DestWrite(e)),
    }

    let source_hash = {
        let mut input = open_read_only(src).map_err(CopyError::SourceRead)?;
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(partial)
            .map_err(CopyError::DestWrite)?;
        let result = stream_copy(&mut input, &mut out, progress);
        let hash = match result {
            Ok(h) => h,
            Err(e) => {
                drop(out);
                let _ = fs::remove_file(partial);
                return Err(e);
            }
        };
        out.flush().map_err(CopyError::DestWrite)?;
        out.sync_all().map_err(CopyError::DestWrite)?;
        crate::drop_page_cache(&out);
        hash
    };

    // コピー中の変更検知
    let after = source_stat(src).map_err(CopyError::SourceRead)?;
    if after != before_stat || after.size != source_hash.1 {
        let _ = fs::remove_file(partial);
        return Err(CopyError::SourceChanged);
    }

    // 読み戻し検証
    let dest_hash = hash_file(partial, &mut crate::no_progress()).map_err(CopyError::DestWrite)?;
    if dest_hash.hash != source_hash.0 || dest_hash.size != source_hash.1 {
        let _ = fs::remove_file(partial);
        return Err(CopyError::Mismatch {
            source_hash: source_hash.0,
            dest_hash: dest_hash.hash,
        });
    }

    // 原子的確定
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(CopyError::DestWrite)?;
    }
    fs::rename(partial, dest).map_err(CopyError::DestWrite)?;
    if let Some(parent) = dest.parent() {
        crate::sync_dir(parent).map_err(CopyError::DestWrite)?;
    }
    if let Some(parent) = partial.parent() {
        let _ = crate::sync_dir(parent);
    }

    Ok(CopyOutcome {
        hash: source_hash.0,
        size: source_hash.1,
        mtime_ns: before_stat.mtime_ns,
    })
}

fn stream_copy(
    input: &mut File,
    out: &mut File,
    progress: Progress<'_>,
) -> Result<(String, u64), CopyError> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; IO_BUFFER_BYTES];
    let mut size = 0u64;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CopyError::SourceRead(e)),
        };
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(CopyError::DestWrite)?;
        size += n as u64;
        if progress(n as u64) == Control::Abort {
            return Err(CopyError::Aborted);
        }
    }
    Ok((hasher.finalize().to_hex().to_string(), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let data = vec![7u8; 5_000_123];
        fs::write(&src, &data).unwrap();
        let partial = dir.path().join("stage/.partial/x.part");
        let dest = dir.path().join("stage/Photos/P01/a/src.bin");
        let out = copy_and_verify(&src, &partial, &dest, &mut crate::no_progress()).unwrap();
        assert_eq!(out.size, data.len() as u64);
        assert_eq!(out.hash, blake3::hash(&data).to_hex().to_string());
        assert_eq!(fs::read(&dest).unwrap(), data);
        assert!(!partial.exists());
        // ソースは変更されていない
        assert_eq!(fs::read(&src).unwrap(), data);
    }

    #[test]
    fn stale_partial_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("s");
        fs::write(&src, b"hello").unwrap();
        let partial = dir.path().join("p.part");
        fs::write(&partial, b"garbage-from-crash").unwrap();
        let dest = dir.path().join("d");
        copy_and_verify(&src, &partial, &dest, &mut crate::no_progress()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn abort_leaves_no_partial_or_dest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("s");
        fs::write(&src, vec![1u8; 10_000_000]).unwrap();
        let partial = dir.path().join("p.part");
        let dest = dir.path().join("d");
        let err = copy_and_verify(&src, &partial, &dest, &mut |_| Control::Abort).unwrap_err();
        assert!(matches!(err, CopyError::Aborted));
        assert!(!partial.exists());
        assert!(!dest.exists());
    }

    #[test]
    fn rejects_symlink_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("t");
        fs::write(&t, b"x").unwrap();
        let l = dir.path().join("l");
        std::os::unix::fs::symlink(&t, &l).unwrap();
        let err = copy_and_verify(
            &l,
            &dir.path().join("p"),
            &dir.path().join("d"),
            &mut crate::no_progress(),
        )
        .unwrap_err();
        assert!(matches!(err, CopyError::NotRegularFile));
        let err = copy_and_verify(
            &dir.path().join("nope"),
            &dir.path().join("p"),
            &dir.path().join("d"),
            &mut crate::no_progress(),
        )
        .unwrap_err();
        assert!(matches!(err, CopyError::SourceRead(_)));
    }

    #[test]
    fn detects_source_change_during_copy() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("s");
        fs::write(&src, vec![1u8; 9_000_000]).unwrap();
        let partial = dir.path().join("p.part");
        let dest = dir.path().join("d");
        let src2 = src.clone();
        let mut first = true;
        let err = copy_and_verify(&src, &partial, &dest, &mut |_| {
            if first {
                first = false;
                // コピー途中で別プロセスが追記した状況を模擬
                let mut f = OpenOptions::new().append(true).open(&src2).unwrap();
                f.write_all(b"more").unwrap();
            }
            Control::Continue
        })
        .unwrap_err();
        assert!(matches!(err, CopyError::SourceChanged), "{err:?}");
        assert!(!dest.exists());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_source_is_error() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            return; // root は権限を無視するためスキップ
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("s");
        fs::write(&src, b"secret").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();
        let err = copy_and_verify(
            &src,
            &dir.path().join("p"),
            &dir.path().join("d"),
            &mut crate::no_progress(),
        )
        .unwrap_err();
        assert!(matches!(err, CopyError::SourceRead(_)));
    }
}
