//! メディアシミュレータ。
//!
//! ```text
//! <root>/<media-name>/media.toml   (任意: vendor/model/serial/fs_uuid など)
//! <root>/<media-name>/p1/...       パーティション 1 の内容
//! <root>/<media-name>/p2/...       パーティション 2 の内容
//! <root>/<media-name>/.unmount-fail  (あればアンマウント失敗を模擬)
//! ```
//!
//! メディアディレクトリの作成 = 挿入、削除/リネーム = 抜去。
//! 実機の UDisks2 と同じく、アンマウント後も物理的に抜かれるまでは「接続中」として扱う。

use crate::{DeviceError, DeviceEvent, Stabilizer, WatchOptions};
use backup_core::identity::{MediaDescriptor, PartitionDescriptor};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SimMediaToml {
    vendor: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    bus: Option<String>,
    size_bytes: Option<u64>,
    /// 全パーティション共通の FS UUID 接頭辞（p1 → "<uuid>-1"）
    fs_uuid: Option<String>,
    fs_type: Option<String>,
    /// システムディスクとして扱う（除外テスト用）
    system: bool,
    /// 暗号化パーティションとして扱う（除外テスト用）
    encrypted: bool,
}

#[derive(Clone)]
pub struct SimBackend {
    root: PathBuf,
    unmounted: Arc<Mutex<HashSet<String>>>,
}

impl SimBackend {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            unmounted: Arc::default(),
        }
    }

    fn media_dir(&self, device_key: &str) -> Option<PathBuf> {
        let name = device_key.strip_prefix("sim:")?;
        if name.contains('/') || name.starts_with('.') {
            return None;
        }
        Some(self.root.join(name))
    }

    fn scan(&self) -> crate::ScanOutput {
        let mut found = HashMap::new();
        let mut rejected = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return (found, rejected);
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let key = format!("sim:{name}");
            match describe(&entry.path(), &key) {
                Ok(Some(m)) => {
                    found.insert(key, m);
                }
                Ok(None) => {}
                Err(reason) => rejected.push((key, name, reason)),
            }
        }
        (found, rejected)
    }

    pub fn spawn_watcher(
        self,
        tx: mpsc::Sender<DeviceEvent>,
        opts: WatchOptions,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut stab = Stabilizer::<MediaDescriptor>::new();
            let mut rejected_seen: HashSet<String> = HashSet::new();
            loop {
                let (current, rejected) = self.scan();
                // アンマウント済み集合は物理的に消えたら解除
                {
                    let mut um = self.unmounted.lock().unwrap();
                    um.retain(|k| current.contains_key(k));
                }
                let (added, removed) =
                    stab.observe(std::time::Instant::now(), current, opts.stable_for);
                for k in removed {
                    if tx
                        .send(DeviceEvent::Removed { device_key: k })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                for m in added {
                    if tx.send(DeviceEvent::Added(m)).await.is_err() {
                        return;
                    }
                }
                let now_rejected: HashSet<String> = rejected.iter().map(|r| r.0.clone()).collect();
                for (key, name, reason) in rejected {
                    if rejected_seen.insert(key.clone()) {
                        let _ = tx
                            .send(DeviceEvent::Rejected {
                                device_key: key,
                                name,
                                reason,
                            })
                            .await;
                    }
                }
                rejected_seen.retain(|k| now_rejected.contains(k));
                tokio::time::sleep(opts.poll_interval).await;
            }
        })
    }

    pub fn prepare(&self, media: &MediaDescriptor) -> Result<MediaDescriptor, DeviceError> {
        let dir = self.media_dir(&media.device_key).ok_or(DeviceError::Gone)?;
        if !dir.is_dir() {
            return Err(DeviceError::Gone);
        }
        let mut m = media.clone();
        for p in &mut m.partitions {
            let path = dir.join(format!("p{}", p.index));
            if !path.is_dir() {
                return Err(DeviceError::Mount {
                    partition: p.device_path.clone(),
                    reason: "not found".into(),
                });
            }
            p.mount_point = Some(path);
        }
        if m.partitions.is_empty() {
            return Err(DeviceError::NoReadablePartition(m.display_name()));
        }
        Ok(m)
    }

    pub fn unmount_all(&self, media: &MediaDescriptor) -> Result<(), DeviceError> {
        let dir = self.media_dir(&media.device_key).ok_or(DeviceError::Gone)?;
        if dir.join(".unmount-fail").exists() {
            return Err(DeviceError::Unmount {
                partition: media
                    .partitions
                    .first()
                    .map(|p| p.device_path.clone())
                    .unwrap_or_default(),
                reason: "target is busy (simulated)".into(),
            });
        }
        self.unmounted
            .lock()
            .unwrap()
            .insert(media.device_key.clone());
        Ok(())
    }

    pub fn is_present(&self, device_key: &str) -> bool {
        self.media_dir(device_key).is_some_and(|d| d.is_dir())
    }
}

fn describe(dir: &Path, key: &str) -> Result<Option<MediaDescriptor>, String> {
    let cfg: SimMediaToml = match std::fs::read_to_string(dir.join("media.toml")) {
        Ok(t) => toml::from_str(&t).map_err(|e| format!("media.toml: {e}"))?,
        Err(_) => SimMediaToml::default(),
    };
    if cfg.system {
        return Err("システムディスクのため対象外です".into());
    }
    if cfg.encrypted {
        return Err("暗号化されたメディアです（自動で解錠しません）".into());
    }
    let mut partitions = Vec::new();
    let mut indices: Vec<u32> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix('p')?
                .parse::<u32>()
                .ok()
        })
        .filter(|i| *i > 0)
        .collect();
    indices.sort_unstable();
    if indices.is_empty() {
        return Ok(None);
    }
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let uuid_base = cfg.fs_uuid.clone().unwrap_or_else(|| format!("SIM-{name}"));
    for i in indices {
        partitions.push(PartitionDescriptor {
            index: i,
            device_path: format!("/dev/sim/{name}{i}"),
            backend_id: format!("{key}/p{i}"),
            fs_type: Some(cfg.fs_type.clone().unwrap_or_else(|| "exfat".into())),
            fs_uuid: Some(format!("{uuid_base}-{i}")),
            part_guid: None,
            fs_label: Some(format!("SIM{i}")),
            size_bytes: 128_000_000_000,
            mount_point: Some(dir.join(format!("p{i}"))),
        });
    }
    Ok(Some(MediaDescriptor {
        device_key: key.to_string(),
        vendor: Some(cfg.vendor.unwrap_or_else(|| "Simulated".into())),
        model: Some(cfg.model.unwrap_or_else(|| name.clone())),
        serial: cfg.serial,
        bus: Some(cfg.bus.unwrap_or_else(|| "usb".into())),
        size_bytes: cfg.size_bytes.unwrap_or(128_000_000_000),
        partitions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn detects_insert_and_remove_with_debounce() {
        let dir = tempfile::tempdir().unwrap();
        let backend = SimBackend::new(dir.path().to_path_buf());
        let (tx, mut rx) = mpsc::channel(16);
        let opts = WatchOptions {
            poll_interval: Duration::from_millis(20),
            stable_for: Duration::from_millis(100),
        };
        let h = backend.clone().spawn_watcher(tx, opts);

        let card = dir.path().join("card1");
        std::fs::create_dir_all(card.join("p1/DCIM")).unwrap();
        std::fs::create_dir_all(card.join("p2")).unwrap();
        std::fs::write(
            card.join("media.toml"),
            "serial = \"X1\"\nfs_uuid = \"ABCD\"\n",
        )
        .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let DeviceEvent::Added(m) = ev else {
            panic!("{ev:?}")
        };
        assert_eq!(m.partitions.len(), 2);
        assert_eq!(m.serial.as_deref(), Some("X1"));
        assert_eq!(m.partitions[0].fs_uuid.as_deref(), Some("ABCD-1"));
        let prepared = backend.prepare(&m).unwrap();
        assert!(
            prepared.partitions[0]
                .mount_point
                .as_ref()
                .unwrap()
                .ends_with("p1")
        );
        backend.unmount_all(&m).unwrap();
        assert!(backend.is_present(&m.device_key));

        std::fs::remove_dir_all(&card).unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ev,
            DeviceEvent::Removed {
                device_key: "sim:card1".into()
            }
        );
        assert!(!backend.is_present("sim:card1"));
        h.abort();
    }

    #[tokio::test]
    async fn rejects_system_disk_and_simulates_unmount_failure() {
        let dir = tempfile::tempdir().unwrap();
        let backend = SimBackend::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path().join("sys/p1")).unwrap();
        std::fs::write(dir.path().join("sys/media.toml"), "system = true\n").unwrap();
        std::fs::create_dir_all(dir.path().join("busy/p1")).unwrap();
        std::fs::write(dir.path().join("busy/.unmount-fail"), "").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let opts = WatchOptions {
            poll_interval: Duration::from_millis(20),
            stable_for: Duration::from_millis(50),
        };
        let h = backend.clone().spawn_watcher(tx, opts);
        let mut got_reject = false;
        let mut busy = None;
        while !(got_reject && busy.is_some()) {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                DeviceEvent::Rejected { name, .. } => {
                    assert_eq!(name, "sys");
                    got_reject = true;
                }
                DeviceEvent::Added(m) => busy = Some(m),
                other => panic!("{other:?}"),
            }
        }
        let busy = busy.unwrap();
        assert!(matches!(
            backend.unmount_all(&busy),
            Err(DeviceError::Unmount { .. })
        ));
        h.abort();
    }
}
