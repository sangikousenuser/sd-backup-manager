//! パスの安全化（FR-016、11 章-4）。
//!
//! - 相対パスの検証（`..`・絶対パス・空要素・NUL を拒否）
//! - SMB/NAS で使えない文字・予約名の置換
//! - Unicode 正規化 (NFC) と大文字小文字を同一視した衝突検出と一意サフィックス付与

use std::collections::{HashMap, HashSet};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathError {
    #[error("空のパスです")]
    Empty,
    #[error("絶対パスは使用できません: {0}")]
    Absolute(String),
    #[error("不正なパス要素を含みます: {0}")]
    BadComponent(String),
}

/// `/` 区切りの相対パスを検証し、要素の配列を返す。
pub fn validate_relative(path: &str) -> Result<Vec<&str>, PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.starts_with('/') {
        return Err(PathError::Absolute(path.to_string()));
    }
    let comps: Vec<&str> = path.split('/').collect();
    for c in &comps {
        if c.is_empty() || *c == "." || *c == ".." || c.contains('\0') {
            return Err(PathError::BadComponent(path.to_string()));
        }
    }
    Ok(comps)
}

const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// NAS 上で安全な 1 要素名に変換する（NFC 正規化を含む）。
pub fn sanitize_component(name: &str) -> String {
    let nfc: String = name.nfc().collect();
    let mut out: String = nfc
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 || c as u32 == 0x7f => '_',
            c => c,
        })
        .collect();
    // 末尾のドット・空白は SMB で消えるため置換
    let kept = out.trim_end_matches(['.', ' ']).len();
    if kept < out.len() {
        let removed = out.len() - kept;
        out.truncate(kept);
        out.extend(std::iter::repeat_n('_', removed));
    }
    if out.is_empty() || out == "." || out == ".." {
        out = "_".to_string();
    }
    let stem_upper = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED_NAMES.contains(&stem_upper.as_str()) {
        out.insert(0, '_');
    }
    out
}

/// NAS（大文字小文字非区別・正規化非区別）での衝突判定キー。
pub fn collision_key(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

/// 1 セッション内の格納パスを一意に割り当てる。
#[derive(Debug, Default)]
pub struct DestinationAllocator {
    files: HashSet<String>,
    dirs: HashSet<String>,
    /// 元ディレクトリ（衝突キー）→ 実際に使うディレクトリパス
    dir_map: HashMap<String, String>,
}

impl DestinationAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 既に割り当て済みのパス（再開時の DB 内容）を登録する。
    pub fn register_existing(&mut self, dest: &str) {
        self.insert(dest);
    }

    /// `prefix`（例 `Photos/P01`）と元の相対パス要素から格納パスを決める。
    /// 戻り値は `/` 区切りの相対パス。
    pub fn allocate(&mut self, prefix: &str, source_components: &[String]) -> String {
        let sanitized: Vec<String> = source_components
            .iter()
            .map(|c| sanitize_component(c))
            .collect();
        let (dir_comps, file_name) = sanitized.split_at(sanitized.len().saturating_sub(1));
        let file_name = file_name
            .first()
            .cloned()
            .unwrap_or_else(|| "_".to_string());

        // ディレクトリ要素ごとに、既存「ファイル」名との衝突を避ける（同じ元ディレクトリは同じ名前へ）
        let mut current = prefix.trim_matches('/').to_string();
        for comp in dir_comps {
            let wanted = join(&current, comp);
            let wanted_key = collision_key(&wanted);
            if let Some(mapped) = self.dir_map.get(&wanted_key) {
                current = mapped.clone();
                continue;
            }
            let mut chosen = wanted;
            let mut n = 2u32;
            while self.files.contains(&collision_key(&chosen)) {
                chosen = join(&current, &with_suffix(comp, n));
                n += 1;
            }
            self.dir_map.insert(wanted_key, chosen.clone());
            current = chosen;
        }

        let mut candidate = join(&current, &file_name);
        let mut n = 2u32;
        loop {
            let key = collision_key(&candidate);
            if !self.files.contains(&key) && !self.dirs.contains(&key) {
                break;
            }
            candidate = join(&current, &with_suffix(&file_name, n));
            n += 1;
        }
        self.insert(&candidate);
        candidate
    }

    fn insert(&mut self, path: &str) {
        let key = collision_key(path);
        let parts: Vec<&str> = key.split('/').collect();
        for i in 1..parts.len() {
            self.dirs.insert(parts[..i].join("/"));
        }
        self.files.insert(key);
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// `DSC0001.JPG` → `DSC0001~2.JPG`
fn with_suffix(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(pos) if pos > 0 => format!("{}~{n}{}", &name[..pos], &name[pos..]),
        _ => format!("{name}~{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute() {
        assert!(validate_relative("DCIM/100/A.JPG").is_ok());
        assert_eq!(validate_relative(""), Err(PathError::Empty));
        assert!(matches!(
            validate_relative("/etc/passwd"),
            Err(PathError::Absolute(_))
        ));
        assert!(validate_relative("a/../b").is_err());
        assert!(validate_relative("a//b").is_err());
        assert!(validate_relative("./a").is_err());
        assert!(validate_relative("a/\0b").is_err());
    }

    #[test]
    fn sanitizes_reserved_and_illegal() {
        assert_eq!(sanitize_component("a:b*c?.jpg"), "a_b_c_.jpg");
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_component("name. "), "name__");
        assert_eq!(sanitize_component("tab\there"), "tab_here");
        assert_eq!(sanitize_component(".."), "__");
        assert_eq!(sanitize_component("ok.MP4"), "ok.MP4");
    }

    #[test]
    fn nfc_normalizes() {
        // NFD の「が」(か + 濁点) は NFC の「が」と同一キー
        let nfd = "\u{304b}\u{3099}.jpg";
        let nfc = "\u{304c}.jpg";
        assert_eq!(collision_key(nfd), collision_key(nfc));
        assert_eq!(sanitize_component(nfd), nfc);
    }

    #[test]
    fn case_insensitive_collisions_get_suffix() {
        let mut a = DestinationAllocator::new();
        let p1 = a.allocate("Photos/P01", &["DCIM".into(), "IMG.JPG".into()]);
        let p2 = a.allocate("Photos/P01", &["DCIM".into(), "img.jpg".into()]);
        let p3 = a.allocate("Photos/P01", &["dcim".into(), "Img.Jpg".into()]);
        assert_eq!(p1, "Photos/P01/DCIM/IMG.JPG");
        assert_eq!(p2, "Photos/P01/DCIM/img~2.jpg");
        assert_eq!(p3, "Photos/P01/DCIM/Img~3.Jpg");
    }

    #[test]
    fn file_vs_directory_collision() {
        let mut a = DestinationAllocator::new();
        let f = a.allocate("Other/P01", &["data".into()]);
        let g = a.allocate("Other/P01", &["DATA".into(), "x.txt".into()]);
        assert_eq!(f, "Other/P01/data");
        assert_eq!(g, "Other/P01/DATA~2/x.txt");
        let g2 = a.allocate("Other/P01", &["data".into(), "y.txt".into()]);
        assert_eq!(g2, "Other/P01/DATA~2/y.txt");
        // 逆順（ディレクトリが先）
        let mut b = DestinationAllocator::new();
        b.allocate("Other/P01", &["dir".into(), "x.txt".into()]);
        let h = b.allocate("Other/P01", &["DIR".into()]);
        assert_eq!(h, "Other/P01/DIR~2");
    }

    #[test]
    fn sanitized_names_that_collide() {
        let mut a = DestinationAllocator::new();
        let x = a.allocate("Other/P01", &["a:b".into()]);
        let y = a.allocate("Other/P01", &["a?b".into()]);
        assert_eq!(x, "Other/P01/a_b");
        assert_eq!(y, "Other/P01/a_b~2");
    }

    #[test]
    fn existing_registration_is_respected() {
        let mut a = DestinationAllocator::new();
        a.register_existing("Videos/P01/C0001.MP4");
        let p = a.allocate("Videos/P01", &["c0001.mp4".into()]);
        assert_eq!(p, "Videos/P01/c0001~2.mp4");
    }
}
