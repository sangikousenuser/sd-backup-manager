//! 「元の構成で復元」（FR-017、AT-27）。
//!
//! マニフェストを使い、分類済みの保存形式（`Photos/P01/...`）からカード本来の相対パス
//! （`P01/DCIM/...`）へ別の保存先に再構成する。マニフェストのパスは信頼せず検証し、
//! 既存ファイルは上書きしない。復元元（NAS・SSD 上のセッション）は読み込み専用で扱う。

use crate::hash::hash_file;
use backup_core::manifest::{Manifest, ManifestFileStatus, hex_decode};
use backup_core::partition_label;
use backup_core::paths::validate_relative;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: usize,
    pub already_present: usize,
    pub from_reference: usize,
    pub failed: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("復元先が復元元の内部にあります")]
    DestInsideSource,
    #[error("復元先を作成できません: {0}")]
    Io(#[from] io::Error),
}

/// `session_root`: セッションフォルダ（NAS の `<folder>/` または SSD の `sessions/<id>/`）。
/// `nas_root`: 過去セッション参照（SKIPPED_VERIFIED）を解決するための NAS ルート。
pub fn restore_layout(
    manifest: &Manifest,
    session_root: &Path,
    nas_root: Option<&Path>,
    dest: &Path,
) -> Result<RestoreReport, RestoreError> {
    fs::create_dir_all(dest)?;
    let dest = fs::canonicalize(dest)?;
    let src_canon = fs::canonicalize(session_root)?;
    if dest.starts_with(&src_canon) {
        return Err(RestoreError::DestInsideSource);
    }
    if let Some(n) = nas_root.and_then(|n| fs::canonicalize(n).ok())
        && dest.starts_with(&n)
    {
        return Err(RestoreError::DestInsideSource);
    }
    let partial_dir = dest.join(".sdbm-restore-partial");
    fs::create_dir_all(&partial_dir)?;

    let mut report = RestoreReport::default();
    for f in &manifest.files {
        let result = (|| -> Result<Outcome, String> {
            let src = match f.status {
                ManifestFileStatus::Copied => join_rel(session_root, &f.dest_rel_path)?,
                ManifestFileStatus::SkippedVerified => {
                    let r = f.reference.as_ref().ok_or("参照情報がありません")?;
                    let nas = nas_root.ok_or("過去セッションの参照には NAS 接続が必要です")?;
                    join_rel(&nas.join(&r.nas_session_folder), &r.dest_rel_path)?
                }
                ManifestFileStatus::Failed => return Err("取り込みに失敗したファイルです".into()),
            };
            let expected = f.blake3.as_deref().ok_or("ハッシュがありません")?;
            let target = restore_target(
                &dest,
                f.partition_index,
                &f.source_rel_path,
                f.source_rel_path_raw_hex.as_deref(),
            )?;
            if target.exists() {
                let h = hash_file(&target, &mut crate::no_progress()).map_err(|e| e.to_string())?;
                return if h.hash == expected {
                    Ok(Outcome::AlreadyPresent)
                } else {
                    Err(format!(
                        "復元先に異なる内容のファイルがあります: {}",
                        target.display()
                    ))
                };
            }
            let partial = partial_dir.join(format!("{}.part", f.file_id.replace('/', "_")));
            let out =
                crate::atomic::copy_and_verify(&src, &partial, &target, &mut crate::no_progress())
                    .map_err(|e| e.to_string())?;
            if out.hash != expected {
                let _ = fs::remove_file(&target);
                return Err("復元したファイルのハッシュがマニフェストと一致しません".into());
            }
            Ok(if f.status == ManifestFileStatus::SkippedVerified {
                Outcome::Reference
            } else {
                Outcome::Restored
            })
        })();
        match result {
            Ok(Outcome::Restored) => report.restored += 1,
            Ok(Outcome::Reference) => {
                report.restored += 1;
                report.from_reference += 1;
            }
            Ok(Outcome::AlreadyPresent) => report.already_present += 1,
            Err(e) => report.failed.push((f.source_rel_path.clone(), e)),
        }
    }
    let _ = fs::remove_dir(&partial_dir);
    Ok(report)
}

enum Outcome {
    Restored,
    Reference,
    AlreadyPresent,
}

fn join_rel(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let comps = validate_relative(rel).map_err(|e| e.to_string())?;
    let mut p = base.to_path_buf();
    for c in comps {
        p.push(c);
    }
    Ok(p)
}

fn restore_target(
    dest: &Path,
    partition: u32,
    rel: &str,
    raw_hex: Option<&str>,
) -> Result<PathBuf, String> {
    let mut p = dest.join(partition_label(partition));
    match raw_hex.and_then(hex_decode) {
        Some(bytes) => {
            // 生バイト列のパスも `/` 区切り・`..` 禁止を検証
            for comp in bytes.split(|b| *b == b'/') {
                if comp.is_empty() || comp == b"." || comp == b".." || comp.contains(&0) {
                    return Err("不正な元パスです".into());
                }
                p.push(OsString::from_vec(comp.to_vec()));
            }
        }
        None => {
            for c in validate_relative(rel).map_err(|e| e.to_string())? {
                p.push(c);
            }
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use backup_core::classifier::MediaKind;
    use backup_core::manifest::*;

    fn manifest(files: Vec<ManifestFile>) -> Manifest {
        Manifest {
            schema_version: 1,
            app_version: "t".into(),
            session_id: "s".into(),
            folder_name: "2026101509321501".into(),
            created_at_utc: "now".into(),
            local_timezone: "Asia/Tokyo".into(),
            device: ManifestDevice {
                identity_hash: "x".into(),
                identity_confidence: "high".into(),
                vendor: None,
                model: None,
                serial: None,
                bus: None,
                size_bytes: 0,
            },
            partitions: vec![ManifestPartition {
                index: 1,
                label: "P01".into(),
                fs_type: None,
                fs_uuid: None,
                part_guid: None,
                fs_label: None,
                size_bytes: 0,
                fingerprint: "f".into(),
            }],
            files,
        }
    }

    fn file(id: &str, src: &str, dst: &str, data: &[u8]) -> ManifestFile {
        ManifestFile {
            file_id: id.into(),
            partition_index: 1,
            source_rel_path: src.into(),
            source_rel_path_raw_hex: None,
            dest_rel_path: dst.into(),
            kind: MediaKind::Video,
            size_bytes: data.len() as u64,
            mtime_ns: None,
            blake3: Some(blake3::hash(data).to_hex().to_string()),
            group: None,
            status: ManifestFileStatus::Copied,
            reference: None,
            error: None,
        }
    }

    #[test]
    fn restores_original_layout_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("2026101509321501");
        let a = b"video-bytes";
        let x = b"<xml/>";
        fs::create_dir_all(session.join("Videos/P01/PRIVATE/M4ROOT/CLIP")).unwrap();
        fs::write(session.join("Videos/P01/PRIVATE/M4ROOT/CLIP/C0001.MP4"), a).unwrap();
        fs::write(
            session.join("Videos/P01/PRIVATE/M4ROOT/CLIP/C0001M01.XML"),
            x,
        )
        .unwrap();
        let m = manifest(vec![
            file(
                "1",
                "PRIVATE/M4ROOT/CLIP/C0001.MP4",
                "Videos/P01/PRIVATE/M4ROOT/CLIP/C0001.MP4",
                a,
            ),
            file(
                "2",
                "PRIVATE/M4ROOT/CLIP/C0001M01.XML",
                "Videos/P01/PRIVATE/M4ROOT/CLIP/C0001M01.XML",
                x,
            ),
        ]);
        let out = dir.path().join("restore");
        let r = restore_layout(&m, &session, None, &out).unwrap();
        assert_eq!(r.restored, 2, "{r:?}");
        assert_eq!(
            fs::read(out.join("P01/PRIVATE/M4ROOT/CLIP/C0001.MP4")).unwrap(),
            a
        );
        // 2 回目は既存として扱い、上書きしない
        let r = restore_layout(&m, &session, None, &out).unwrap();
        assert_eq!(r.already_present, 2);
        // 異なる内容が既にある場合は失敗として報告
        fs::write(out.join("P01/PRIVATE/M4ROOT/CLIP/C0001.MP4"), b"other").unwrap();
        let r = restore_layout(&m, &session, None, &out).unwrap();
        assert_eq!(r.failed.len(), 1);
        assert_eq!(
            fs::read(out.join("P01/PRIVATE/M4ROOT/CLIP/C0001.MP4")).unwrap(),
            b"other"
        );
    }

    #[test]
    fn rejects_dest_inside_source_and_bad_paths() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s");
        fs::create_dir_all(&session).unwrap();
        let m = manifest(vec![]);
        assert!(matches!(
            restore_layout(&m, &session, None, &session.join("inner")),
            Err(RestoreError::DestInsideSource)
        ));
        let mut evil = file("1", "../../etc/x", "Videos/P01/a", b"a");
        fs::create_dir_all(session.join("Videos/P01")).unwrap();
        fs::write(session.join("Videos/P01/a"), b"a").unwrap();
        evil.source_rel_path = "../../escape".into();
        let r = restore_layout(
            &manifest(vec![evil]),
            &session,
            None,
            &dir.path().join("out"),
        )
        .unwrap();
        assert_eq!(r.failed.len(), 1);
        assert!(!dir.path().join("escape").exists());
    }
}
