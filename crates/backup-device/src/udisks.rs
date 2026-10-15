//! UDisks2（D-Bus）バックエンド。
//!
//! UDisks2 は udev のイベントを元にオブジェクトを更新するため、本実装では
//! `GetManagedObjects` を短い間隔で取得して差分を取り、2 秒の安定化確認後に通知する。
//! （udev を直接監視するよりも、マウント/アンマウントと同じ API で一貫した情報を得られる）
//!
//! 候補にするのは USB / SD(sdio) 接続またはリムーバブルなドライブのみ。OS ルート・/boot・
//! /home・スワップ・ステージング領域を含むドライブは除外する（11 章-7）。

use crate::{DeviceError, DeviceEvent, ExclusionPolicy, Stabilizer, WatchOptions};
use backup_core::identity::{MediaDescriptor, PartitionDescriptor};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const DEST: &str = "org.freedesktop.UDisks2";
const ROOT: &str = "/org/freedesktop/UDisks2";
const IF_DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const IF_BLOCK: &str = "org.freedesktop.UDisks2.Block";
const IF_PARTITION: &str = "org.freedesktop.UDisks2.Partition";
const IF_FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";

type Props = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<String, Props>>;
type BlockRef<'a> = (&'a OwnedObjectPath, &'a HashMap<String, Props>);

const SYSTEM_MOUNTS: [&str; 6] = ["/", "/boot", "/boot/efi", "/home", "/usr", "/var"];

#[derive(Clone)]
pub struct UDisksBackend {
    conn: Connection,
    protected_devs: HashSet<u64>,
    protected_paths: Vec<PathBuf>,
}

fn s(props: &Props, key: &str) -> Option<String> {
    let v = props.get(key)?;
    let t: &str = v.downcast_ref().ok()?;
    let t = t.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn b(props: &Props, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false)
}

fn u64p(props: &Props, key: &str) -> Option<u64> {
    props.get(key).and_then(|v| v.downcast_ref::<u64>().ok())
}

fn u32p(props: &Props, key: &str) -> Option<u32> {
    props.get(key).and_then(|v| v.downcast_ref::<u32>().ok())
}

fn obj(props: &Props, key: &str) -> Option<String> {
    let v = props.get(key)?;
    let p: zbus::zvariant::ObjectPath<'_> = v.downcast_ref().ok()?;
    let p = p.as_str().to_string();
    if p == "/" { None } else { Some(p) }
}

/// `ay`（NUL 終端のバイト列）を文字列に。
fn bytestring(v: &OwnedValue) -> Option<String> {
    let bytes: Vec<u8> = Vec::<u8>::try_from(v.try_clone().ok()?).ok()?;
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8(bytes[..end].to_vec()).ok()
}

fn mount_points(props: &Props) -> Vec<PathBuf> {
    let Some(v) = props.get("MountPoints") else {
        return Vec::new();
    };
    let Ok(cloned) = v.try_clone() else {
        return Vec::new();
    };
    let Ok(list) = Vec::<Vec<u8>>::try_from(cloned) else {
        return Vec::new();
    };
    list.into_iter()
        .filter_map(|bytes| {
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            String::from_utf8(bytes[..end].to_vec())
                .ok()
                .map(PathBuf::from)
        })
        .collect()
}

fn be(e: impl std::fmt::Display) -> DeviceError {
    DeviceError::Backend(e.to_string())
}

impl UDisksBackend {
    pub async fn connect(policy: ExclusionPolicy) -> Result<Self, DeviceError> {
        let conn = Connection::system().await.map_err(be)?;
        let mut protected_devs = HashSet::new();
        for p in policy
            .protected_paths
            .iter()
            .chain([PathBuf::from("/")].iter())
        {
            if let Ok(m) = std::fs::metadata(p) {
                protected_devs.insert(m.dev());
            }
        }
        Ok(Self {
            conn,
            protected_devs,
            protected_paths: policy.protected_paths,
        })
    }

    async fn objects(&self) -> Result<Objects, DeviceError> {
        let msg = self
            .conn
            .call_method(
                Some(DEST),
                ROOT,
                Some("org.freedesktop.DBus.ObjectManager"),
                "GetManagedObjects",
                &(),
            )
            .await
            .map_err(be)?;
        msg.body().deserialize::<Objects>().map_err(be)
    }

    /// 候補メディアと除外デバイスを列挙する。
    fn evaluate(&self, objects: &Objects) -> crate::ScanOutput {
        let mut found = HashMap::new();
        let mut rejected = Vec::new();

        // drive → blocks
        let mut blocks_by_drive: HashMap<String, Vec<BlockRef<'_>>> = HashMap::new();
        for (path, ifaces) in objects {
            if let Some(block) = ifaces.get(IF_BLOCK)
                && let Some(drive) = obj(block, "Drive")
            {
                blocks_by_drive
                    .entry(drive)
                    .or_default()
                    .push((path, ifaces));
            }
        }

        for (drive_path, ifaces) in objects {
            let Some(drive) = ifaces.get(IF_DRIVE) else {
                continue;
            };
            let key = drive_path.as_str().to_string();
            let bus = s(drive, "ConnectionBus").unwrap_or_default();
            let external = matches!(bus.as_str(), "usb" | "sdio" | "ieee1394")
                || b(drive, "Removable")
                || b(drive, "MediaRemovable");
            if !external {
                continue; // 内蔵ディスクは黙って対象外
            }
            if !b(drive, "MediaAvailable") && b(drive, "MediaRemovable") {
                continue; // 空のカードリーダー
            }
            let name = [s(drive, "Vendor"), s(drive, "Model")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let blocks = blocks_by_drive.get(&key).cloned().unwrap_or_default();

            let mut reject: Option<String> = None;
            let mut partitions = Vec::new();
            let mut encrypted = 0;
            for (bpath, bifaces) in &blocks {
                let block = &bifaces[IF_BLOCK];
                if b(block, "HintIgnore") {
                    continue;
                }
                if let Some(dev) = u64p(block, "DeviceNumber")
                    && self.protected_devs.contains(&dev)
                {
                    reject = Some("OS またはステージング領域を含むため対象外です".into());
                }
                let usage = s(block, "IdUsage").unwrap_or_default();
                let id_type = s(block, "IdType").unwrap_or_default();
                if usage == "other" && id_type == "swap" {
                    reject = Some("スワップ領域を含むため対象外です".into());
                }
                if usage == "crypto" {
                    encrypted += 1;
                    continue;
                }
                let Some(fs) = bifaces.get(IF_FILESYSTEM) else {
                    continue;
                };
                let mps = mount_points(fs);
                for mp in &mps {
                    let mps_str = mp.to_string_lossy();
                    if SYSTEM_MOUNTS.contains(&mps_str.as_ref())
                        || self
                            .protected_paths
                            .iter()
                            .any(|p| p.starts_with(mp) && mp != &PathBuf::from("/media"))
                    {
                        reject = Some(format!(
                            "システム領域（{mps_str}）がマウントされているため対象外です"
                        ));
                    }
                }
                if usage != "filesystem" {
                    continue;
                }
                let index = bifaces
                    .get(IF_PARTITION)
                    .and_then(|p| u32p(p, "Number"))
                    .unwrap_or(1);
                partitions.push(PartitionDescriptor {
                    index,
                    device_path: block.get("Device").and_then(bytestring).unwrap_or_default(),
                    backend_id: bpath.as_str().to_string(),
                    fs_type: Some(id_type),
                    fs_uuid: s(block, "IdUUID"),
                    part_guid: bifaces.get(IF_PARTITION).and_then(|p| s(p, "UUID")),
                    fs_label: s(block, "IdLabel"),
                    size_bytes: u64p(block, "Size").unwrap_or(0),
                    mount_point: mps.into_iter().next(),
                });
            }
            if let Some(reason) = reject {
                rejected.push((key, name, reason));
                continue;
            }
            if partitions.is_empty() {
                if encrypted > 0 {
                    rejected.push((
                        key,
                        name,
                        "暗号化されたメディアです（自動で解錠しません）".into(),
                    ));
                } else if !blocks.is_empty() {
                    rejected.push((
                        key,
                        name,
                        "読み込み可能なファイルシステムがありません".into(),
                    ));
                }
                continue;
            }
            partitions.sort_by_key(|p| p.index);
            // 同じ番号が重複する場合（パーティションなし + 全体 FS 等）は安全側で除外
            let mut seen = HashSet::new();
            if !partitions.iter().all(|p| seen.insert(p.index)) {
                rejected.push((key, name, "パーティション構成を判別できません".into()));
                continue;
            }
            found.insert(
                key.clone(),
                MediaDescriptor {
                    device_key: key,
                    vendor: s(drive, "Vendor"),
                    model: s(drive, "Model"),
                    serial: s(drive, "Serial"),
                    bus: Some(bus),
                    size_bytes: u64p(drive, "Size").unwrap_or(0),
                    partitions,
                },
            );
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
                match self.objects().await {
                    Ok(objects) => {
                        let (mut current, rejected) = self.evaluate(&objects);
                        // マウントポイントの変化（GNOME の自動マウント等）で安定化をやり直さない
                        for m in current.values_mut() {
                            for p in &mut m.partitions {
                                p.mount_point = None;
                            }
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
                        let now: HashSet<String> = rejected.iter().map(|r| r.0.clone()).collect();
                        for (device_key, name, reason) in rejected {
                            if rejected_seen.insert(device_key.clone()) {
                                let _ = tx
                                    .send(DeviceEvent::Rejected {
                                        device_key,
                                        name,
                                        reason,
                                    })
                                    .await;
                            }
                        }
                        rejected_seen.retain(|k| now.contains(k));
                    }
                    Err(e) => tracing::warn!("UDisks2 query failed: {e}"),
                }
                tokio::time::sleep(opts.poll_interval).await;
            }
        })
    }

    async fn filesystem_props(&self, block_path: &str) -> Result<Option<Props>, DeviceError> {
        let objects = self.objects().await?;
        Ok(objects
            .iter()
            .find(|(p, _)| p.as_str() == block_path)
            .and_then(|(_, ifaces)| ifaces.get(IF_FILESYSTEM))
            .map(|fs| {
                fs.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.try_clone().ok()?)))
                    .collect()
            }))
    }

    /// 既存のマウントを使うか、読み込み専用でマウントする（FR-007）。
    pub async fn prepare(&self, media: &MediaDescriptor) -> Result<MediaDescriptor, DeviceError> {
        let mut m = media.clone();
        for p in &mut m.partitions {
            let mut last_err = String::new();
            for _attempt in 0..5 {
                let Some(fs) = self.filesystem_props(&p.backend_id).await? else {
                    return Err(DeviceError::Gone);
                };
                if let Some(mp) = mount_points(&fs).into_iter().next() {
                    p.mount_point = Some(mp);
                    break;
                }
                let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
                opts.insert("options", Value::from("ro,nosuid,nodev,noexec"));
                opts.insert("auth.no_user_interaction", Value::from(true));
                match self
                    .conn
                    .call_method(
                        Some(DEST),
                        p.backend_id.as_str(),
                        Some(IF_FILESYSTEM),
                        "Mount",
                        &(opts,),
                    )
                    .await
                {
                    Ok(reply) => {
                        if let Ok(path) = reply.body().deserialize::<String>() {
                            p.mount_point = Some(PathBuf::from(path));
                            break;
                        }
                    }
                    Err(e) => {
                        // 自動マウントとの競合（AlreadyMounted）は再読込で解決する
                        last_err = e.to_string();
                        tokio::time::sleep(Duration::from_millis(800)).await;
                    }
                }
            }
            if p.mount_point.is_none() {
                return Err(DeviceError::Mount {
                    partition: p.device_path.clone(),
                    reason: last_err,
                });
            }
        }
        Ok(m)
    }

    /// 全パーティションを UDisks2 でアンマウント（FR-008）。1 つでも失敗すれば Err。
    pub async fn unmount_all(&self, media: &MediaDescriptor) -> Result<(), DeviceError> {
        let mut errors = Vec::new();
        for p in &media.partitions {
            let Some(fs) = self.filesystem_props(&p.backend_id).await? else {
                errors.push((p.device_path.clone(), "device disappeared".to_string()));
                continue;
            };
            if mount_points(&fs).is_empty() {
                continue;
            }
            let opts: HashMap<&str, Value<'_>> =
                HashMap::from([("auth.no_user_interaction", Value::from(true))]);
            if let Err(e) = self
                .conn
                .call_method(
                    Some(DEST),
                    p.backend_id.as_str(),
                    Some(IF_FILESYSTEM),
                    "Unmount",
                    &(opts,),
                )
                .await
            {
                errors.push((p.device_path.clone(), e.to_string()));
                continue;
            }
            // 実際にマウントが外れたことを確認
            if let Some(fs) = self.filesystem_props(&p.backend_id).await?
                && !mount_points(&fs).is_empty()
            {
                errors.push((p.device_path.clone(), "still mounted".into()));
            }
        }
        match errors.into_iter().next() {
            None => Ok(()),
            Some((partition, reason)) => Err(DeviceError::Unmount { partition, reason }),
        }
    }

    pub async fn is_present(&self, device_key: &str) -> bool {
        match self.objects().await {
            Ok(objects) => objects.keys().any(|p| p.as_str() == device_key),
            Err(_) => true, // 不明な場合は「まだある」とみなし、抜去扱いにしない
        }
    }
}
