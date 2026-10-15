//! 物理メディアの識別（FR-006）。
//!
//! USB シリアル・機種・容量・パーティション GUID・FS UUID を組み合わせて
//! `identity_hash` を作り、情報量から信頼度を評価する。SD カードリーダー経由では
//! シリアルがリーダー側のものになるため、FS UUID を必須の要素として扱う。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Ambiguous,
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
            Confidence::Ambiguous => "ambiguous",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "high" => Confidence::High,
            "medium" => Confidence::Medium,
            "low" => Confidence::Low,
            "ambiguous" => Confidence::Ambiguous,
            _ => return None,
        })
    }

    /// 中断セッションへの合流・履歴によるスキップを許可できる水準か。
    pub fn allows_resume_and_skip(self) -> bool {
        matches!(self, Confidence::High | Confidence::Medium)
    }
}

/// OS から取得した 1 パーティションの情報。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionDescriptor {
    /// 1 始まりのパーティション番号（パーティションテーブルなしは 1）。
    pub index: u32,
    /// `/dev/sdb1` 等。
    pub device_path: String,
    /// UDisks2 のオブジェクトパス等、バックエンド固有の識別子。
    pub backend_id: String,
    pub fs_type: Option<String>,
    pub fs_uuid: Option<String>,
    pub part_guid: Option<String>,
    pub fs_label: Option<String>,
    pub size_bytes: u64,
    /// 実際のマウントポイント（OS から取得）。
    pub mount_point: Option<PathBuf>,
}

impl PartitionDescriptor {
    /// パーティション単位の指紋。カタログ照合に使う。
    pub fn fingerprint(&self) -> String {
        let mut h = blake3::Hasher::new();
        for part in [
            self.index.to_string(),
            norm(&self.fs_uuid),
            norm(&self.part_guid),
            norm(&self.fs_type),
            self.size_bytes.to_string(),
        ] {
            h.update(part.as_bytes());
            h.update(b"\x1f");
        }
        h.finalize().to_hex().to_string()
    }
}

/// 1 台の物理メディア。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDescriptor {
    /// 接続中だけ有効なキー（UDisks2 の Drive オブジェクトパス等）。
    pub device_key: String,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub bus: Option<String>,
    pub size_bytes: u64,
    pub partitions: Vec<PartitionDescriptor>,
}

impl MediaDescriptor {
    /// 表示用の名前（例: "SanDisk Extreme (128.0GB)"）。
    pub fn display_name(&self) -> String {
        let mut name = [self.vendor.as_deref(), self.model.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if name.is_empty() {
            name = "外部メディア".to_string();
        }
        format!("{name} ({})", format_bytes(self.size_bytes))
    }

    /// 識別ハッシュ。シリアル・機種・容量・全パーティションの UUID 群から算出する。
    pub fn identity_hash(&self) -> String {
        let mut parts: Vec<&PartitionDescriptor> = self.partitions.iter().collect();
        parts.sort_by_key(|p| p.index);
        let mut h = blake3::Hasher::new();
        h.update(b"sdbm-identity-v1\x1e");
        for v in [
            norm(&self.serial),
            norm(&self.vendor),
            norm(&self.model),
            self.size_bytes.to_string(),
        ] {
            h.update(v.as_bytes());
            h.update(b"\x1f");
        }
        for p in parts {
            h.update(b"\x1e");
            h.update(p.fingerprint().as_bytes());
        }
        h.finalize().to_hex().to_string()
    }

    /// 情報量に基づく信頼度。同一ハッシュの同時接続（クローン）は呼び出し側で Ambiguous にする。
    pub fn base_confidence(&self) -> Confidence {
        if self.partitions.is_empty() {
            return Confidence::Low;
        }
        let all_have_fs_uuid = self
            .partitions
            .iter()
            .all(|p| p.fs_uuid.as_deref().is_some_and(|u| !u.trim().is_empty()));
        let has_serial = self.serial.as_deref().is_some_and(|s| !s.trim().is_empty());
        match (all_have_fs_uuid, has_serial) {
            (true, true) => Confidence::High,
            (true, false) => Confidence::Medium,
            _ => Confidence::Low,
        }
    }
}

fn norm(v: &Option<String>) -> String {
    v.as_deref()
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_default()
}

/// 10 進表記のバイト数（GUI と同一規則）。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(index: u32, uuid: Option<&str>) -> PartitionDescriptor {
        PartitionDescriptor {
            index,
            device_path: format!("/dev/sdx{index}"),
            backend_id: String::new(),
            fs_type: Some("exfat".into()),
            fs_uuid: uuid.map(Into::into),
            part_guid: None,
            fs_label: None,
            size_bytes: 1000,
            mount_point: None,
        }
    }

    fn media(serial: Option<&str>, parts: Vec<PartitionDescriptor>) -> MediaDescriptor {
        MediaDescriptor {
            device_key: "k".into(),
            vendor: Some("Sony".into()),
            model: Some("SF-G".into()),
            serial: serial.map(Into::into),
            bus: Some("usb".into()),
            size_bytes: 128_000_000_000,
            partitions: parts,
        }
    }

    #[test]
    fn confidence_levels() {
        assert_eq!(
            media(Some("S1"), vec![part(1, Some("AB-CD"))]).base_confidence(),
            Confidence::High
        );
        assert_eq!(
            media(None, vec![part(1, Some("AB-CD"))]).base_confidence(),
            Confidence::Medium
        );
        assert_eq!(
            media(Some("S1"), vec![part(1, None)]).base_confidence(),
            Confidence::Low
        );
        assert_eq!(media(Some("S1"), vec![]).base_confidence(), Confidence::Low);
    }

    #[test]
    fn identity_changes_with_fs_uuid_and_ignores_device_key() {
        let a = media(Some("S1"), vec![part(1, Some("AB-CD"))]);
        let mut b = a.clone();
        b.device_key = "other".into();
        assert_eq!(a.identity_hash(), b.identity_hash());
        let c = media(Some("S1"), vec![part(1, Some("AB-CE"))]);
        assert_ne!(a.identity_hash(), c.identity_hash());
        // 大文字小文字・前後空白は同一視
        let d = media(Some(" s1 "), vec![part(1, Some("ab-cd"))]);
        assert_eq!(a.identity_hash(), d.identity_hash());
    }

    #[test]
    fn bytes_formatting() {
        assert_eq!(format_bytes(999), "999B");
        assert_eq!(format_bytes(20_000_000_000), "20.0GB");
        assert_eq!(format_bytes(128_000_000_000), "128.0GB");
    }
}
