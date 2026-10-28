//! メディアのファイル走査（読み込みのみ）。
//!
//! シンボリックリンク・ソケット・デバイスファイル等は辿らず警告に記録する（FR-015）。
//! 異常に深い階層・大量ファイルにも上限を設ける（11 章-6）。

use backup_core::manifest::hex_encode;
use backup_storage::platform::{is_link_like, mtime_ns};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const MAX_DEPTH: usize = 64;
const MAX_FILES: usize = 2_000_000;

#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub partition_index: u32,
    pub components: Vec<String>,
    pub rel_path: String,
    pub raw_hex: Option<String>,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    pub directories: Vec<(u32, Vec<String>)>,
    /// 取り込まない特殊ファイル等（情報）。
    pub warnings: Vec<String>,
    /// 読めなかったディレクトリ等（取りこぼしの可能性 → 全件成功とは表示しない）。
    pub errors: Vec<String>,
    pub cancelled: bool,
}

pub fn scan_partition(root: &Path, index: u32, cancel: &CancellationToken) -> ScanResult {
    let mut result = ScanResult::default();
    let mut stack: Vec<(PathBuf, Vec<Vec<u8>>)> = vec![(root.to_path_buf(), Vec::new())];
    while let Some((dir, prefix)) = stack.pop() {
        if cancel.is_cancelled() {
            result.cancelled = true;
            return result;
        }
        if prefix.len() > MAX_DEPTH {
            result.errors.push(format!(
                "階層が深すぎるため読み込みを中止しました: {}",
                display(&prefix)
            ));
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                result.errors.push(format!(
                    "フォルダを読み込めません ({}): {e}",
                    display_or_root(&prefix)
                ));
                continue;
            }
        };
        let mut children = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => children.push(entry),
                Err(e) => result.errors.push(format!(
                    "フォルダ内の項目を読み込めません ({}): {e}",
                    display_or_root(&prefix)
                )),
            }
        }
        children.sort_by_key(|e| e.file_name());
        for entry in children {
            let name = entry.file_name();
            let mut comps = prefix.clone();
            comps.push(name_bytes(&name));
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    result
                        .errors
                        .push(format!("情報を取得できません ({}): {e}", display(&comps)));
                    continue;
                }
            };
            let ft = meta.file_type();
            if ft.is_symlink() || is_link_like(&meta) {
                result.warnings.push(format!(
                    "シンボリックリンクは取り込みません: {}",
                    display(&comps)
                ));
            } else if ft.is_dir() {
                result.directories.push((
                    index,
                    comps
                        .iter()
                        .map(|c| String::from_utf8_lossy(c).replace('/', "_"))
                        .collect(),
                ));
                stack.push((path, comps));
            } else if ft.is_file() {
                if result.files.len() >= MAX_FILES {
                    result
                        .errors
                        .push("ファイル数が上限を超えたため走査を中止しました".into());
                    return result;
                }
                let utf8: Option<Vec<String>> = comps
                    .iter()
                    .map(|c| String::from_utf8(c.clone()).ok())
                    .collect();
                let (components, raw_hex) = match utf8 {
                    Some(c) => (c, None),
                    None => (
                        comps
                            .iter()
                            .map(|c| String::from_utf8_lossy(c).replace('/', "_"))
                            .collect(),
                        Some(hex_encode(&comps.join(&b'/'))),
                    ),
                };
                result.files.push(ScannedFile {
                    partition_index: index,
                    rel_path: components.join("/"),
                    components,
                    raw_hex,
                    size: meta.len(),
                    mtime_ns: mtime_ns(&meta),
                });
            } else {
                result
                    .warnings
                    .push(format!("特殊ファイルは取り込みません: {}", display(&comps)));
            }
        }
    }
    result.files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    result.directories.sort_by(|a, b| a.1.cmp(&b.1));
    result
}

/// ファイル名のバイト列。Unix は生バイト、Windows は UTF-16 を UTF-8 に変換したもの
/// （不正なサロゲートを含む名前は置換文字になり、コピーは失敗として報告される）。
fn name_bytes(name: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().into_owned().into_bytes()
    }
}

fn display(comps: &[Vec<u8>]) -> String {
    comps
        .iter()
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn display_or_root(comps: &[Vec<u8>]) -> String {
    if comps.is_empty() {
        "/".into()
    } else {
        display(comps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn scans_regular_files_and_skips_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("DCIM/100")).unwrap();
        fs::write(root.join("DCIM/100/A.JPG"), b"abc").unwrap();
        fs::write(root.join("x.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("DCIM"), root.join("DCIM/100/loop")).unwrap();
        let r = scan_partition(root, 1, &CancellationToken::new());
        let paths: Vec<_> = r.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["DCIM/100/A.JPG", "x.txt"]);
        assert_eq!(r.warnings.len(), 2);
        assert!(r.errors.is_empty());
        assert_eq!(r.files[0].size, 3);
    }

    #[test]
    fn cancel_stops_scan() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), b"a").unwrap();
        let t = CancellationToken::new();
        t.cancel();
        assert!(scan_partition(dir.path(), 1, &t).cancelled);
    }
}
