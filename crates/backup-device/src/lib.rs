//! リムーバブルメディアの検出・読み込み専用マウント・安全なアンマウント（3.1）。
//!
//! - `udisks`（Linux）: UDisks2 D-Bus を監視し、外部メディアのみを候補にする
//! - `sim`: ディレクトリをメディアに見立てたシミュレータ（macOS での開発・CI の E2E 試験用）
//!
//! どちらも「2 秒間同じ構成が観測されたら追加」とする安定化確認（FR-005）を行う。

pub mod sim;
#[cfg(target_os = "linux")]
pub mod udisks;

use backup_core::identity::MediaDescriptor;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// 安定化確認済みの外部メディア。
    Added(MediaDescriptor),
    /// 物理的に取り外された（またはバックエンドから消えた）。
    Removed { device_key: String },
    /// 自動除外したデバイス（システムディスク等）。GUI に警告表示する。
    Rejected {
        device_key: String,
        name: String,
        reason: String,
    },
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("マウントできません ({partition}): {reason}")]
    Mount { partition: String, reason: String },
    #[error("アンマウントできません ({partition}): {reason}")]
    Unmount { partition: String, reason: String },
    #[error("読み込み可能なパーティションがありません: {0}")]
    NoReadablePartition(String),
    #[error("デバイスがありません")]
    Gone,
    #[error("デバイス管理サービスに接続できません: {0}")]
    Backend(String),
}

/// 走査結果: (候補メディア, 除外デバイス (device_key, 名前, 理由))。
pub(crate) type ScanOutput = (
    std::collections::HashMap<String, MediaDescriptor>,
    Vec<(String, String, String)>,
);

/// 自動除外の条件。
#[derive(Debug, Clone, Default)]
pub struct ExclusionPolicy {
    /// これらのパスを含むデバイス（ステージング・ホーム・ルート等）は対象外。
    pub protected_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    pub poll_interval: Duration,
    pub stable_for: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(1000),
            stable_for: Duration::from_secs(2),
        }
    }
}

/// デバイスバックエンド（静的ディスパッチ）。
#[derive(Clone)]
pub enum DeviceBackend {
    Simulated(sim::SimBackend),
    #[cfg(target_os = "linux")]
    UDisks(udisks::UDisksBackend),
}

impl DeviceBackend {
    pub fn name(&self) -> &'static str {
        match self {
            DeviceBackend::Simulated(_) => "simulator",
            #[cfg(target_os = "linux")]
            DeviceBackend::UDisks(_) => "udisks2",
        }
    }

    /// 監視を開始する。起動時に接続済みのメディアも通知する（FR-001）。
    pub fn spawn_watcher(
        &self,
        tx: mpsc::Sender<DeviceEvent>,
        opts: WatchOptions,
    ) -> tokio::task::JoinHandle<()> {
        match self {
            DeviceBackend::Simulated(b) => b.clone().spawn_watcher(tx, opts),
            #[cfg(target_os = "linux")]
            DeviceBackend::UDisks(b) => b.clone().spawn_watcher(tx, opts),
        }
    }

    /// 読み込み用に準備する（必要なら読み込み専用でマウント）。マウントポイントを埋めて返す。
    pub async fn prepare(&self, media: &MediaDescriptor) -> Result<MediaDescriptor, DeviceError> {
        match self {
            DeviceBackend::Simulated(b) => b.prepare(media),
            #[cfg(target_os = "linux")]
            DeviceBackend::UDisks(b) => b.prepare(media).await,
        }
    }

    /// 全パーティションをアンマウントする。1 つでも失敗したら Err（FR-008）。
    pub async fn unmount_all(&self, media: &MediaDescriptor) -> Result<(), DeviceError> {
        match self {
            DeviceBackend::Simulated(b) => b.unmount_all(media),
            #[cfg(target_os = "linux")]
            DeviceBackend::UDisks(b) => b.unmount_all(media).await,
        }
    }

    /// まだ物理的に接続されているか。
    pub async fn is_present(&self, device_key: &str) -> bool {
        match self {
            DeviceBackend::Simulated(b) => b.is_present(device_key),
            #[cfg(target_os = "linux")]
            DeviceBackend::UDisks(b) => b.is_present(device_key).await,
        }
    }
}

/// 安定化確認用の観測履歴。
pub(crate) struct Stabilizer<T: PartialEq + Clone> {
    pending: std::collections::HashMap<String, (T, std::time::Instant)>,
    announced: std::collections::HashMap<String, T>,
}

impl<T: PartialEq + Clone> Stabilizer<T> {
    pub fn new() -> Self {
        Self {
            pending: Default::default(),
            announced: Default::default(),
        }
    }

    /// 現在観測されている集合を渡し、(新規確定, 消失) を返す。
    pub fn observe(
        &mut self,
        now: std::time::Instant,
        current: std::collections::HashMap<String, T>,
        stable_for: Duration,
    ) -> (Vec<T>, Vec<String>) {
        let mut added = Vec::new();
        let removed: Vec<String> = self
            .announced
            .keys()
            .filter(|k| !current.contains_key(*k))
            .cloned()
            .collect();
        for k in &removed {
            self.announced.remove(k);
        }
        self.pending.retain(|k, _| current.contains_key(k));
        for (key, value) in current {
            if self.announced.contains_key(&key) {
                continue;
            }
            match self.pending.get(&key) {
                Some((prev, since)) if *prev == value => {
                    if now.duration_since(*since) >= stable_for {
                        self.pending.remove(&key);
                        self.announced.insert(key, value.clone());
                        added.push(value);
                    }
                }
                _ => {
                    self.pending.insert(key, (value, now));
                }
            }
        }
        (added, removed)
    }

    #[cfg(test)]
    pub fn is_announced(&self, key: &str) -> bool {
        self.announced.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Instant;

    #[test]
    fn stabilizer_requires_stable_observation_and_dedups() {
        let mut s = Stabilizer::<u32>::new();
        let t0 = Instant::now();
        let d = Duration::from_secs(2);
        let one = HashMap::from([("a".to_string(), 1u32)]);
        assert_eq!(s.observe(t0, one.clone(), d), (vec![], vec![]));
        assert_eq!(
            s.observe(t0 + Duration::from_secs(1), one.clone(), d),
            (vec![], vec![])
        );
        assert_eq!(
            s.observe(t0 + Duration::from_secs(2), one.clone(), d),
            (vec![1], vec![])
        );
        // 重複イベントは再通知しない
        assert_eq!(
            s.observe(t0 + Duration::from_secs(3), one.clone(), d),
            (vec![], vec![])
        );
        assert!(s.is_announced("a"));
        // 消失
        assert_eq!(
            s.observe(t0 + Duration::from_secs(4), HashMap::new(), d),
            (vec![], vec!["a".to_string()])
        );
        // 構成が変わり続ける間は確定しない
        let mut t = t0 + Duration::from_secs(5);
        for v in 0..5u32 {
            assert_eq!(
                s.observe(t, HashMap::from([("b".to_string(), v)]), d).0,
                Vec::<u32>::new()
            );
            t += Duration::from_secs(1);
        }
    }
}
