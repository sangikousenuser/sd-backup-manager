//! NAS 上のファイル操作（FR-026〜FR-030）。
//!
//! NAS はローカルファイルシステムとして見えるパス（GVfs の FUSE パス、または
//! CIFS マウントポイント）として扱う。接続方法は daemon 側のバックエンドが担当し、
//! ここでは次の安全規則を守った転送だけを実装する:
//!
//! - 保存先ルートに置いた識別ファイル（target marker）が設定と一致しない限り書き込まない
//!   （共有のすり替え・未マウント時のローカルディレクトリへの誤書き込みを防ぐ）
//! - 一時名で書き込み → fsync → 読み戻しハッシュ照合 → 同じディレクトリ内で rename
//! - 書き込みはセッションフォルダ（数字のみの名前）配下に限定

use crate::capacity::{FsSpace, fs_space};
use crate::hash::{hash_file, open_read_only};
use crate::{Control, Progress};
use backup_core::IO_BUFFER_BYTES;
use backup_core::paths::validate_relative;
use backup_core::session::is_valid_folder_name;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const MARKER_FILE: &str = ".sd-backup-manager-target.json";
pub const TEMP_PREFIX: &str = ".sdbm-tmp-";
const OLD_PREFIX: &str = ".sdbm-old-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetMarker {
    pub app: String,
    pub target_id: String,
    pub created_at_utc: String,
}

#[derive(Debug, Error)]
pub enum NasError {
    #[error("NAS に接続できません: {0}")]
    Offline(String),
    #[error("NAS の保存先識別ファイルがありません（設定画面で保存先を初期化してください）")]
    MarkerMissing,
    #[error("NAS の保存先が設定と異なります（共有のすり替え・誤マウントの可能性）")]
    MarkerMismatch,
    #[error("NAS の空き容量が不足しています")]
    NoSpace,
    #[error("NAS への書き込み権限がありません: {0}")]
    Permission(String),
    #[error("NAS 上のハッシュが一致しません")]
    Mismatch,
    #[error("SSD 上のファイルが記録と一致しません（期待 {expected}, 実際 {actual}）")]
    StagingCorrupt { expected: String, actual: String },
    #[error("SSD 上のファイルがありません: {0}")]
    StagingMissing(PathBuf),
    #[error("不正なパスです: {0}")]
    InvalidPath(String),
    #[error("中断しました")]
    Aborted,
    #[error("NAS I/O エラー: {0}")]
    Io(String),
}

impl NasError {
    pub fn code(&self) -> &'static str {
        match self {
            NasError::Offline(_) => "NAS_OFFLINE",
            NasError::MarkerMissing => "NAS_MARKER_MISSING",
            NasError::MarkerMismatch => "NAS_MARKER_MISMATCH",
            NasError::NoSpace => "NAS_NO_SPACE",
            NasError::Permission(_) => "NAS_PERMISSION",
            NasError::Mismatch => "NAS_HASH_MISMATCH",
            NasError::StagingCorrupt { .. } => "STAGING_CORRUPT",
            NasError::StagingMissing(_) => "STAGING_MISSING",
            NasError::InvalidPath(_) => "INVALID_PATH",
            NasError::Aborted => "ABORTED",
            NasError::Io(_) => "NAS_IO",
        }
    }

    /// 人手が必要（自動再試行しても直らない）か。再試行自体は止めない。
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            NasError::MarkerMissing
                | NasError::MarkerMismatch
                | NasError::Permission(_)
                | NasError::StagingCorrupt { .. }
                | NasError::StagingMissing(_)
                | NasError::InvalidPath(_)
        )
    }

    pub fn from_io(e: io::Error) -> Self {
        match classify_os_error(e.raw_os_error()) {
            ErrClass::NoSpace => NasError::NoSpace,
            ErrClass::Permission => NasError::Permission(e.to_string()),
            ErrClass::Offline => NasError::Offline(e.to_string()),
            ErrClass::Other => NasError::Io(e.to_string()),
        }
    }
}

enum ErrClass {
    NoSpace,
    Permission,
    Offline,
    Other,
}

#[cfg(unix)]
fn classify_os_error(code: Option<i32>) -> ErrClass {
    use nix::errno::Errno;
    match code.map(Errno::from_raw) {
        Some(Errno::ENOSPC) | Some(Errno::EDQUOT) => ErrClass::NoSpace,
        Some(Errno::EACCES) | Some(Errno::EPERM) | Some(Errno::EROFS) => ErrClass::Permission,
        Some(
            Errno::ENOTCONN
            | Errno::EHOSTDOWN
            | Errno::EHOSTUNREACH
            | Errno::ENETDOWN
            | Errno::ENETUNREACH
            | Errno::ETIMEDOUT
            | Errno::ECONNRESET
            | Errno::ECONNABORTED
            | Errno::ESTALE,
        ) => ErrClass::Offline,
        _ => ErrClass::Other,
    }
}

/// Win32 エラーコード（SMB/UNC で返るもの）の分類。
#[cfg(windows)]
fn classify_os_error(code: Option<i32>) -> ErrClass {
    match code {
        // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL, ERROR_DISK_QUOTA_EXCEEDED
        Some(39) | Some(112) | Some(1295) => ErrClass::NoSpace,
        // ERROR_ACCESS_DENIED, ERROR_WRITE_PROTECT, ERROR_LOGON_FAILURE, ERROR_NETWORK_ACCESS_DENIED
        Some(5) | Some(19) | Some(1326) | Some(65) => ErrClass::Permission,
        // ERROR_BAD_NETPATH, ERROR_NETNAME_DELETED, ERROR_UNEXP_NET_ERR, ERROR_DEV_NOT_EXIST,
        // ERROR_BAD_NET_NAME, ERROR_SEM_TIMEOUT, ERROR_NETWORK_UNREACHABLE, ERROR_HOST_UNREACHABLE,
        // ERROR_NO_NET_OR_BAD_PATH, ERROR_CONNECTION_ABORTED, ERROR_NOT_CONNECTED, ERROR_NETWORK_BUSY,
        // ERROR_REM_NOT_LIST, ERROR_BAD_NET_RESP
        Some(53) | Some(64) | Some(59) | Some(55) | Some(67) | Some(121) | Some(1231)
        | Some(1232) | Some(1222) | Some(1236) | Some(2250) | Some(54) | Some(51) | Some(58) => {
            ErrClass::Offline
        }
        _ => ErrClass::Other,
    }
}

/// 接続済み NAS のルート（ローカルから見えるパス）。
#[derive(Debug, Clone)]
pub struct NasRoot {
    root: PathBuf,
    display_uri: String,
}

impl NasRoot {
    pub fn new(root: PathBuf, display_uri: impl Into<String>) -> Self {
        Self {
            root,
            display_uri: display_uri.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn display_uri(&self) -> &str {
        &self.display_uri
    }

    pub fn read_marker(&self) -> Result<Option<TargetMarker>, NasError> {
        match fs::read(self.root.join(MARKER_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| NasError::MarkerMismatch),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // ルート自体が見えない場合はオフライン扱い
                if fs::metadata(&self.root).is_err() {
                    Err(NasError::Offline(format!(
                        "{} が見つかりません",
                        self.root.display()
                    )))
                } else {
                    Ok(None)
                }
            }
            Err(e) => Err(NasError::from_io(e)),
        }
    }

    pub fn verify_marker(&self, expected_target_id: &str) -> Result<(), NasError> {
        match self.read_marker()? {
            None => Err(NasError::MarkerMissing),
            Some(m) if m.target_id == expected_target_id && m.app == "sd-backup-manager" => Ok(()),
            Some(_) => Err(NasError::MarkerMismatch),
        }
    }

    /// 識別ファイルを新規作成する（既存は上書きしない）。
    pub fn create_marker(&self, target_id: &str, created_at_utc: &str) -> Result<(), NasError> {
        let marker = TargetMarker {
            app: "sd-backup-manager".into(),
            target_id: target_id.into(),
            created_at_utc: created_at_utc.into(),
        };
        let bytes = serde_json::to_vec_pretty(&marker).map_err(|e| NasError::Io(e.to_string()))?;
        let path = self.root.join(MARKER_FILE);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    NasError::MarkerMismatch
                } else {
                    NasError::from_io(e)
                }
            })?;
        f.write_all(&bytes).map_err(NasError::from_io)?;
        f.sync_all().map_err(NasError::from_io)?;
        Ok(())
    }

    pub fn space(&self) -> Option<FsSpace> {
        let s = fs_space(&self.root).ok()?;
        // FUSE 等で 0 を返す場合は「取得不可」扱い
        if s.total_bytes == 0 { None } else { Some(s) }
    }

    fn final_path(&self, folder: &str, dest_rel: &str) -> Result<PathBuf, NasError> {
        if !is_valid_folder_name(folder) {
            return Err(NasError::InvalidPath(folder.to_string()));
        }
        let comps =
            validate_relative(dest_rel).map_err(|e| NasError::InvalidPath(e.to_string()))?;
        let mut p = self.root.join(folder);
        for c in comps {
            p.push(c);
        }
        let mut current = self.root.clone();
        for component in p
            .strip_prefix(&self.root)
            .map_err(|_| NasError::InvalidPath(dest_rel.into()))?
        {
            current.push(component);
            if fs::symlink_metadata(&current).is_ok_and(|m| crate::platform::is_link_like(&m)) {
                return Err(NasError::InvalidPath(current.display().to_string()));
            }
        }
        Ok(p)
    }

    pub fn ensure_directory(&self, folder: &str, dest_rel: &str) -> Result<(), NasError> {
        let path = self.final_path(folder, dest_rel)?;
        fs::create_dir_all(&path).map_err(NasError::from_io)?;
        if !fs::symlink_metadata(&path)
            .map_err(NasError::from_io)?
            .is_dir()
        {
            return Err(NasError::InvalidPath(dest_rel.into()));
        }
        Ok(())
    }

    /// NAS 上の実体を全読み込みしてハッシュ照合する。存在しなければ Ok(false)。
    pub fn verify_existing(
        &self,
        folder: &str,
        dest_rel: &str,
        expected_hash: &str,
        progress: Progress<'_>,
    ) -> Result<bool, NasError> {
        let path = self.final_path(folder, dest_rel)?;
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if fs::metadata(&self.root).is_err() {
                    return Err(NasError::Offline("NAS が見えません".into()));
                }
                return Ok(false);
            }
            Err(e) => return Err(NasError::from_io(e)),
        }
        let out = hash_file(&path, progress).map_err(|e| {
            if e.kind() == io::ErrorKind::Interrupted {
                NasError::Aborted
            } else {
                NasError::from_io(e)
            }
        })?;
        Ok(out.hash == expected_hash)
    }

    /// ステージングファイルを NAS のセッションフォルダへ転送し、読み戻し検証して確定する。
    pub fn upload_verified(
        &self,
        local: &Path,
        folder: &str,
        dest_rel: &str,
        expected_hash: &str,
        progress: Progress<'_>,
    ) -> Result<UploadOutcome, NasError> {
        let final_path = self.final_path(folder, dest_rel)?;
        let parent = final_path
            .parent()
            .ok_or_else(|| NasError::InvalidPath(dest_rel.into()))?
            .to_path_buf();
        if fs::metadata(&self.root).is_err() {
            return Err(NasError::Offline("NAS が見えません".into()));
        }

        // 既に同じ内容が確定済み（クラッシュ後の再送など）
        let existed = match fs::symlink_metadata(&final_path) {
            Ok(m) if m.is_file() => true,
            Ok(_) => {
                return Err(NasError::InvalidPath(format!(
                    "{dest_rel} がファイルではありません"
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(NasError::from_io(e)),
        };
        if existed
            && self.verify_existing(folder, dest_rel, expected_hash, &mut crate::no_progress())?
        {
            return Ok(UploadOutcome::AlreadyPresent);
        }

        fs::create_dir_all(&parent).map_err(NasError::from_io)?;
        let temp = parent.join(format!("{TEMP_PREFIX}{}.part", uuid::Uuid::new_v4()));

        let result = (|| -> Result<(), NasError> {
            let mut input = match open_read_only(local) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(NasError::StagingMissing(local.to_path_buf()));
                }
                Err(e) => return Err(NasError::Io(format!("SSD 読み込み: {e}"))),
            };
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(NasError::from_io)?;
            let mut hasher = blake3::Hasher::new();
            let mut buf = vec![0u8; IO_BUFFER_BYTES];
            loop {
                let n = match input.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(NasError::Io(format!("SSD 読み込み: {e}"))),
                };
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n]).map_err(NasError::from_io)?;
                if progress(n as u64) == Control::Abort {
                    return Err(NasError::Aborted);
                }
            }
            out.flush().map_err(NasError::from_io)?;
            out.sync_all().map_err(NasError::from_io)?;
            crate::drop_page_cache(&out);
            drop(out);

            let local_hash = hasher.finalize().to_hex().to_string();
            if local_hash != expected_hash {
                return Err(NasError::StagingCorrupt {
                    expected: expected_hash.into(),
                    actual: local_hash,
                });
            }
            // 読み戻し
            let back = hash_file(&temp, &mut crate::no_progress()).map_err(NasError::from_io)?;
            if back.hash != expected_hash {
                return Err(NasError::Mismatch);
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }

        // 確定: 既存（不一致）の最終名は自セッションの壊れた実体なので退避してから置換
        if existed {
            let old = parent.join(format!("{OLD_PREFIX}{}", uuid::Uuid::new_v4()));
            fs::rename(&final_path, &old).map_err(NasError::from_io)?;
            if let Err(e) = fs::rename(&temp, &final_path) {
                let _ = fs::rename(&old, &final_path);
                let _ = fs::remove_file(&temp);
                return Err(NasError::from_io(e));
            }
            let _ = fs::remove_file(&old);
        } else if let Err(e) = fs::rename(&temp, &final_path) {
            let _ = fs::remove_file(&temp);
            return Err(NasError::from_io(e));
        }
        let _ = crate::sync_dir(&parent);

        // 確定後の存在・サイズ確認
        let size = fs::metadata(&final_path).map_err(NasError::from_io)?.len();
        let local_size = fs::metadata(local).map(|m| m.len()).unwrap_or(u64::MAX);
        if size != local_size {
            return Err(NasError::Mismatch);
        }
        Ok(if existed {
            UploadOutcome::Replaced
        } else {
            UploadOutcome::Uploaded
        })
    }

    /// セッションフォルダ内に残った自アプリの一時ファイルを削除する。
    pub fn cleanup_temp_files(&self, folder: &str) -> usize {
        if !is_valid_folder_name(folder) {
            return 0;
        }
        let mut removed = 0;
        let mut stack = vec![self.root.join(folder)];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().to_string();
                if ft.is_dir() {
                    stack.push(e.path());
                } else if ft.is_file()
                    && (name.starts_with(TEMP_PREFIX) || name.starts_with(OLD_PREFIX))
                    && fs::remove_file(e.path()).is_ok()
                {
                    removed += 1;
                }
            }
        }
        removed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadOutcome {
    Uploaded,
    AlreadyPresent,
    Replaced,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOLDER: &str = "2026101509321501";

    fn setup() -> (tempfile::TempDir, NasRoot, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let nas_dir = dir.path().join("nas");
        fs::create_dir(&nas_dir).unwrap();
        let nas = NasRoot::new(nas_dir, "smb://test/share");
        let local = dir.path().join("local.bin");
        let data = vec![42u8; 3_000_000];
        fs::write(&local, &data).unwrap();
        let hash = blake3::hash(&data).to_hex().to_string();
        (dir, nas, local, hash)
    }

    #[test]
    fn marker_lifecycle() {
        let (_d, nas, _, _) = setup();
        assert!(matches!(
            nas.verify_marker("id-1"),
            Err(NasError::MarkerMissing)
        ));
        nas.create_marker("id-1", "now").unwrap();
        nas.verify_marker("id-1").unwrap();
        assert!(matches!(
            nas.verify_marker("id-2"),
            Err(NasError::MarkerMismatch)
        ));
        // 上書きしない
        assert!(nas.create_marker("id-2", "now").is_err());
        nas.verify_marker("id-1").unwrap();
    }

    #[test]
    fn missing_root_is_offline() {
        let nas = NasRoot::new(PathBuf::from("/nonexistent/sdbm-test-root"), "x");
        assert!(matches!(nas.verify_marker("a"), Err(NasError::Offline(_))));
    }

    #[test]
    fn upload_verify_and_idempotent() {
        let (_d, nas, local, hash) = setup();
        let out = nas
            .upload_verified(
                &local,
                FOLDER,
                "Videos/P01/C0001.MP4",
                &hash,
                &mut crate::no_progress(),
            )
            .unwrap();
        assert_eq!(out, UploadOutcome::Uploaded);
        assert!(
            nas.verify_existing(
                FOLDER,
                "Videos/P01/C0001.MP4",
                &hash,
                &mut crate::no_progress()
            )
            .unwrap()
        );
        let again = nas
            .upload_verified(
                &local,
                FOLDER,
                "Videos/P01/C0001.MP4",
                &hash,
                &mut crate::no_progress(),
            )
            .unwrap();
        assert_eq!(again, UploadOutcome::AlreadyPresent);
        assert_eq!(nas.cleanup_temp_files(FOLDER), 0);
    }

    #[test]
    fn tampered_nas_file_is_detected_and_replaced() {
        let (_d, nas, local, hash) = setup();
        nas.upload_verified(&local, FOLDER, "a.bin", &hash, &mut crate::no_progress())
            .unwrap();
        let p = nas.root().join(FOLDER).join("a.bin");
        fs::write(&p, b"tampered").unwrap();
        assert!(
            !nas.verify_existing(FOLDER, "a.bin", &hash, &mut crate::no_progress())
                .unwrap()
        );
        let out = nas
            .upload_verified(&local, FOLDER, "a.bin", &hash, &mut crate::no_progress())
            .unwrap();
        assert_eq!(out, UploadOutcome::Replaced);
        assert!(
            nas.verify_existing(FOLDER, "a.bin", &hash, &mut crate::no_progress())
                .unwrap()
        );
    }

    #[test]
    fn corrupt_staging_is_not_uploaded() {
        let (_d, nas, local, _hash) = setup();
        let wrong = "0".repeat(64);
        let err = nas
            .upload_verified(&local, FOLDER, "a.bin", &wrong, &mut crate::no_progress())
            .unwrap_err();
        assert!(matches!(err, NasError::StagingCorrupt { .. }));
        assert!(
            !nas.root().join(FOLDER).join("a.bin").exists(),
            "no final file on failure"
        );
        assert_eq!(nas.cleanup_temp_files(FOLDER), 0, "temp removed on failure");
    }

    #[test]
    fn rejects_bad_paths() {
        let (_d, nas, local, hash) = setup();
        assert!(matches!(
            nas.upload_verified(&local, "../etc", "a", &hash, &mut crate::no_progress()),
            Err(NasError::InvalidPath(_))
        ));
        assert!(matches!(
            nas.upload_verified(&local, FOLDER, "../../a", &hash, &mut crate::no_progress()),
            Err(NasError::InvalidPath(_))
        ));
    }

    #[test]
    fn aborted_upload_leaves_no_final() {
        let (_d, nas, local, hash) = setup();
        let err = nas
            .upload_verified(&local, FOLDER, "x.bin", &hash, &mut |_| Control::Abort)
            .unwrap_err();
        assert!(matches!(err, NasError::Aborted));
        assert!(!nas.root().join(FOLDER).join("x.bin").exists());
        assert_eq!(nas.cleanup_temp_files(FOLDER), 0);
    }
}
