//! `manifest.json`（FR-013、FR-017、8 章）。
//!
//! 元パスと格納パスの対応・サイズ・mtime・BLAKE3・パーティション・デバイス識別を記録し、
//! 「元の構成で復元」に使う。復元側はマニフェストのパスを信頼せず必ず検証する。

use crate::classifier::MediaKind;
use crate::paths::{PathError, validate_relative};
use serde::{Deserialize, Serialize};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const MANIFEST_FILE_NAME: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub app_version: String,
    pub session_id: String,
    pub folder_name: String,
    pub created_at_utc: String,
    pub local_timezone: String,
    pub device: ManifestDevice,
    pub partitions: Vec<ManifestPartition>,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDevice {
    pub identity_hash: String,
    pub identity_confidence: String,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub bus: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPartition {
    pub index: u32,
    /// `P01` 等。
    pub label: String,
    pub fs_type: Option<String>,
    pub fs_uuid: Option<String>,
    pub part_guid: Option<String>,
    pub fs_label: Option<String>,
    pub size_bytes: u64,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestFileStatus {
    /// このセッションのフォルダに保存済み。
    Copied,
    /// 過去のセッションに検証済み実体があるため参照のみ。
    SkippedVerified,
    /// 取り込みに失敗（要対応）。
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestReference {
    pub nas_session_folder: String,
    pub dest_rel_path: String,
    pub nas_uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    pub file_id: String,
    pub partition_index: u32,
    /// パーティションルートからの元の相対パス（`/` 区切り）。
    pub source_rel_path: String,
    /// UTF-8 でない元ファイル名のバイト列（16 進）。復元時に優先して使う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_rel_path_raw_hex: Option<String>,
    /// セッションフォルダからの格納パス（`Photos/P01/...`）。
    pub dest_rel_path: String,
    pub kind: MediaKind,
    pub size_bytes: u64,
    pub mtime_ns: Option<i64>,
    pub blake3: Option<String>,
    pub group: Option<String>,
    pub status: ManifestFileStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<ManifestReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("マニフェストの形式が不正です: {0}")]
    Json(#[from] serde_json::Error),
    #[error("未対応のマニフェストスキーマです: {0}")]
    Schema(u32),
    #[error("マニフェスト内のパスが不正です ({path}): {source}")]
    Path { path: String, source: PathError },
    #[error("マニフェストの内容が不正です: {0}")]
    Invalid(String),
}

impl Manifest {
    pub fn to_json_pretty(&self) -> Vec<u8> {
        let mut v = serde_json::to_vec_pretty(self).expect("manifest serializes");
        v.push(b'\n');
        v
    }

    /// 読み込みと検証（パス・重複・ハッシュ形式）。
    pub fn parse_and_validate(bytes: &[u8]) -> Result<Self, ManifestError> {
        let m: Manifest = serde_json::from_slice(bytes)?;
        if m.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::Schema(m.schema_version));
        }
        let mut seen_src = std::collections::HashSet::new();
        let mut seen_dst = std::collections::HashSet::new();
        for f in &m.files {
            validate_relative(&f.source_rel_path).map_err(|source| ManifestError::Path {
                path: f.source_rel_path.clone(),
                source,
            })?;
            validate_relative(&f.dest_rel_path).map_err(|source| ManifestError::Path {
                path: f.dest_rel_path.clone(),
                source,
            })?;
            if let Some(r) = &f.reference {
                validate_relative(&r.dest_rel_path).map_err(|source| ManifestError::Path {
                    path: r.dest_rel_path.clone(),
                    source,
                })?;
                if !crate::session::is_valid_folder_name(&r.nas_session_folder) {
                    return Err(ManifestError::Invalid(format!(
                        "参照フォルダ名が不正です: {}",
                        r.nas_session_folder
                    )));
                }
            }
            if let Some(h) = &f.blake3
                && (h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err(ManifestError::Invalid(format!(
                    "ハッシュ形式が不正です: {h}"
                )));
            }
            if !m.partitions.iter().any(|p| p.index == f.partition_index) {
                return Err(ManifestError::Invalid(format!(
                    "未定義のパーティション番号です: {}",
                    f.partition_index
                )));
            }
            if !seen_src.insert((f.partition_index, f.source_rel_path.clone())) {
                return Err(ManifestError::Invalid(format!(
                    "元パスが重複しています: {}",
                    f.source_rel_path
                )));
            }
            if f.status == ManifestFileStatus::Copied
                && !seen_dst.insert(crate::paths::collision_key(&f.dest_rel_path))
            {
                return Err(ManifestError::Invalid(format!(
                    "格納パスが重複しています: {}",
                    f.dest_rel_path
                )));
            }
        }
        Ok(m)
    }
}

/// UTF-8 でないパスを 16 進で保存するための補助。
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample() -> Manifest {
        Manifest {
            schema_version: 1,
            app_version: "0.1.0".into(),
            session_id: "0192a1b2-0000-7000-8000-000000000000".into(),
            folder_name: "2026101509321501".into(),
            created_at_utc: "2026-10-15T00:32:15Z".into(),
            local_timezone: "Asia/Tokyo".into(),
            device: ManifestDevice {
                identity_hash: "abc".into(),
                identity_confidence: "high".into(),
                vendor: None,
                model: None,
                serial: None,
                bus: None,
                size_bytes: 1,
            },
            partitions: vec![ManifestPartition {
                index: 1,
                label: "P01".into(),
                fs_type: Some("exfat".into()),
                fs_uuid: None,
                part_guid: None,
                fs_label: None,
                size_bytes: 1,
                fingerprint: "f".into(),
            }],
            files: vec![ManifestFile {
                file_id: "1".into(),
                partition_index: 1,
                source_rel_path: "DCIM/100/A.JPG".into(),
                source_rel_path_raw_hex: None,
                dest_rel_path: "Photos/P01/DCIM/100/A.JPG".into(),
                kind: MediaKind::Photo,
                size_bytes: 3,
                mtime_ns: Some(1),
                blake3: Some("a".repeat(64)),
                group: None,
                status: ManifestFileStatus::Copied,
                reference: None,
                error: None,
            }],
        }
    }

    #[test]
    fn roundtrip() {
        let m = sample();
        let back = Manifest::parse_and_validate(&m.to_json_pretty()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn rejects_traversal() {
        let mut m = sample();
        m.files[0].source_rel_path = "../../etc/passwd".into();
        assert!(Manifest::parse_and_validate(&m.to_json_pretty()).is_err());
        let mut m = sample();
        m.files[0].dest_rel_path = "/abs".into();
        assert!(Manifest::parse_and_validate(&m.to_json_pretty()).is_err());
    }

    #[test]
    fn rejects_bad_hash_and_duplicates() {
        let mut m = sample();
        m.files[0].blake3 = Some("zz".into());
        assert!(Manifest::parse_and_validate(&m.to_json_pretty()).is_err());
        let mut m = sample();
        let mut dup = m.files[0].clone();
        dup.file_id = "2".into();
        dup.source_rel_path = "DCIM/100/a.jpg".into();
        dup.dest_rel_path = "Photos/P01/dcim/100/a.jpg".into();
        m.files.push(dup);
        assert!(Manifest::parse_and_validate(&m.to_json_pretty()).is_err());
    }

    #[test]
    fn hex() {
        let raw = b"caf\xe9.jpg";
        assert_eq!(hex_decode(&hex_encode(raw)).unwrap(), raw);
        assert!(hex_decode("abc").is_none());
    }
}
