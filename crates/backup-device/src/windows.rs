//! Windows バックエンド（Win32）。
//!
//! - 検出: ドライブレター（`GetLogicalDrives`）を短い間隔で走査し、USB / SD(MMC) 接続または
//!   リムーバブルのボリュームを、物理ディスク番号（`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`）で
//!   1 台のメディアにまとめる。OS ドライブ・ステージング領域のボリューム・内蔵ディスク・
//!   ネットワークドライブ・光学ドライブは対象外。
//! - 読み込み: Windows にはユーザー権限の読み込み専用マウントがないため、既存のドライブレターを
//!   使い、アプリ自身は読み込み専用でしか開かない（設計書 FR-007 の「既存マウント利用時」）。
//! - 取り外し: 各ボリュームを `FSCTL_LOCK_VOLUME`（開いているファイルがあれば失敗＝使用中）→
//!   `FSCTL_DISMOUNT_VOLUME` し、ロックしたハンドルを物理的に抜かれるまで保持する
//!   （保持中は他のアプリが再マウントできない）。全ボリュームで成功したときだけ成功。
//! - 抜去: 物理ドライブが開けなくなる、または `IOCTL_STORAGE_CHECK_VERIFY2` がメディアなしを返す。

use crate::{DeviceError, DeviceEvent, ExclusionPolicy, ScanOutput, Stabilizer, WatchOptions};
use backup_core::identity::{MediaDescriptor, PartitionDescriptor};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeMmc, BusTypeSd, BusTypeUsb, CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FlushFileBuffers, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_DISK_GET_PARTITION_INFO_EX,
    IOCTL_STORAGE_CHECK_VERIFY2, IOCTL_STORAGE_QUERY_PROPERTY, PARTITION_INFORMATION_EX,
    PARTITION_STYLE_GPT, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    StorageDeviceProperty, VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// `\\.\E:` や `\\.\PhysicalDrive1` を開く（access 0 は問い合わせ専用）。
fn open_device(path: &str, access: u32) -> io::Result<OwnedHandle> {
    let w = wide(path);
    let h: HANDLE = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(h as _) })
}

/// DeviceIoControl の薄いラッパー。成功時は出力バイト数を返す。
fn ioctl<I, O>(
    h: &OwnedHandle,
    code: u32,
    input: Option<&I>,
    output: Option<&mut O>,
    out_len: usize,
) -> io::Result<u32> {
    let mut returned = 0u32;
    let (ip, il) = match input {
        Some(i) => (
            i as *const I as *const core::ffi::c_void,
            std::mem::size_of::<I>() as u32,
        ),
        None => (std::ptr::null(), 0),
    };
    let op = match output {
        Some(o) => o as *mut O as *mut core::ffi::c_void,
        None => std::ptr::null_mut(),
    };
    let ok = unsafe {
        DeviceIoControl(
            h.as_raw_handle() as HANDLE,
            code,
            ip,
            il,
            op,
            out_len as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned)
}

struct DiskInfo {
    vendor: Option<String>,
    product: Option<String>,
    serial: Option<String>,
    bus: i32,
    removable: bool,
}

fn read_cstr(buf: &[u8], offset: u32) -> Option<String> {
    let off = offset as usize;
    if off == 0 || off >= buf.len() {
        return None;
    }
    let end = buf[off..]
        .iter()
        .position(|b| *b == 0)
        .map(|p| off + p)
        .unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[off..end]).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn disk_info(disk: u32) -> io::Result<DiskInfo> {
    let h = open_device(&format!(r"\\.\PhysicalDrive{disk}"), 0)?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut buf = vec![0u8; 4096];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.as_raw_handle() as HANDLE,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if (returned as usize) < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return Err(io::Error::other("short STORAGE_DEVICE_DESCRIPTOR"));
    }
    let desc: STORAGE_DEVICE_DESCRIPTOR =
        unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const _) };
    let buf = &buf[..returned as usize];
    Ok(DiskInfo {
        vendor: read_cstr(buf, desc.VendorIdOffset),
        product: read_cstr(buf, desc.ProductIdOffset),
        serial: read_cstr(buf, desc.SerialNumberOffset),
        bus: desc.BusType,
        removable: desc.RemovableMedia,
    })
}

fn bus_name(bus: i32) -> &'static str {
    match bus {
        b if b == BusTypeUsb => "usb",
        b if b == BusTypeSd => "sd",
        b if b == BusTypeMmc => "mmc",
        _ => "other",
    }
}

fn is_external_bus(bus: i32) -> bool {
    bus == BusTypeUsb || bus == BusTypeSd || bus == BusTypeMmc
}

/// 物理ドライブにメディアが入っているか（カードリーダーでカードが抜かれたら false）。
fn media_present(disk: u32) -> bool {
    match open_device(&format!(r"\\.\PhysicalDrive{disk}"), 0) {
        Ok(h) => ioctl::<(), ()>(&h, IOCTL_STORAGE_CHECK_VERIFY2, None, None, 0).is_ok(),
        Err(_) => false,
    }
}

struct VolumeInfo {
    letter: char,
    drive_type: u32,
    label: Option<String>,
    serial: u32,
    fs_type: String,
    disk: u32,
    extents: u32,
    part_number: u32,
    part_guid: Option<String>,
    size: u64,
}

fn format_guid(g: &windows_sys::core::GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

fn volume_info(letter: char, drive_type: u32) -> io::Result<VolumeInfo> {
    let root = wide(&format!("{letter}:\\"));
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let (mut serial, mut maxlen, mut flags) = (0u32, 0u32, 0u32);
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            &mut maxlen,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        // カードの入っていないリーダー等
        return Err(io::Error::last_os_error());
    }
    let h = open_device(&format!(r"\\.\{letter}:"), 0)?;
    // 1 ボリュームが複数ディスクにまたがる場合（ダイナミックディスク等）に備え、余裕を持った領域を使う
    let mut ext_buf = [0u8; 1024];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h.as_raw_handle() as HANDLE,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            std::ptr::null(),
            0,
            ext_buf.as_mut_ptr() as *mut core::ffi::c_void,
            ext_buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let ext: VOLUME_DISK_EXTENTS =
        unsafe { std::ptr::read_unaligned(ext_buf.as_ptr() as *const _) };
    let mut part = PARTITION_INFORMATION_EX::default();
    let size = std::mem::size_of::<PARTITION_INFORMATION_EX>();
    let (part_number, part_guid, part_len) = match ioctl::<(), PARTITION_INFORMATION_EX>(
        &h,
        IOCTL_DISK_GET_PARTITION_INFO_EX,
        None,
        Some(&mut part),
        size,
    ) {
        Ok(_) => {
            let guid = if part.PartitionStyle == PARTITION_STYLE_GPT {
                Some(format_guid(unsafe { &part.Anonymous.Gpt.PartitionId }))
            } else {
                None
            };
            (
                part.PartitionNumber,
                guid,
                part.PartitionLength.max(0) as u64,
            )
        }
        Err(_) => (0, None, 0),
    };
    Ok(VolumeInfo {
        letter,
        drive_type,
        label: Some(from_wide(&label)).filter(|s| !s.is_empty()),
        serial,
        fs_type: from_wide(&fs).to_lowercase(),
        disk: ext.Extents[0].DiskNumber,
        extents: ext.NumberOfDiskExtents,
        part_number,
        part_guid,
        size: if part_len > 0 {
            part_len
        } else {
            ext.Extents[0].ExtentLength.max(0) as u64
        },
    })
}

struct Held {
    descriptor: MediaDescriptor,
    disk: u32,
    _locks: Vec<OwnedHandle>,
}

#[derive(Clone)]
pub struct WinBackend {
    /// 対象外のボリューム（小文字の `c:\` 形式）
    protected_roots: Arc<HashSet<String>>,
    /// アンマウント（ロック）済みで、物理的な取り外しを待っているディスク
    held: Arc<Mutex<HashMap<String, Held>>>,
}

fn device_key(disk: u32) -> String {
    format!("windisk:{disk}")
}

fn disk_of_key(key: &str) -> Option<u32> {
    key.strip_prefix("windisk:")?.parse().ok()
}

impl WinBackend {
    pub fn new(policy: ExclusionPolicy) -> Self {
        let mut protected = HashSet::new();
        if let Some(sys) = std::env::var_os("SystemDrive") {
            protected.insert(format!("{}\\", sys.to_string_lossy()).to_lowercase());
        }
        for p in &policy.protected_paths {
            if let Some(root) = volume_root(p) {
                protected.insert(root);
            }
        }
        Self {
            protected_roots: Arc::new(protected),
            held: Arc::default(),
        }
    }

    fn scan(&self) -> ScanOutput {
        let mut rejected = Vec::new();
        let mask = unsafe { GetLogicalDrives() };
        let mut volumes: Vec<VolumeInfo> = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let dtype = unsafe { GetDriveTypeW(wide(&root).as_ptr()) };
            if dtype != DRIVE_REMOVABLE && dtype != DRIVE_FIXED {
                continue; // ネットワーク・光学・RAM ディスク等
            }
            let Ok(v) = volume_info(letter, dtype) else {
                continue;
            };
            volumes.push(v);
        }

        let mut by_disk: HashMap<u32, Vec<VolumeInfo>> = HashMap::new();
        for v in volumes {
            by_disk.entry(v.disk).or_default().push(v);
        }

        let mut found = HashMap::new();
        for (disk, mut vols) in by_disk {
            let key = device_key(disk);
            let info = disk_info(disk).ok();
            let external = vols.iter().any(|v| v.drive_type == DRIVE_REMOVABLE)
                || info
                    .as_ref()
                    .is_some_and(|i| is_external_bus(i.bus) || i.removable);
            let name = info
                .as_ref()
                .map(|i| {
                    [i.vendor.clone(), i.product.clone()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let protected = vols.iter().any(|v| {
                self.protected_roots
                    .contains(&format!("{}:\\", v.letter).to_lowercase())
            });
            if protected {
                if external {
                    rejected.push((
                        key,
                        name,
                        "OS またはステージング領域を含むため対象外です".into(),
                    ));
                }
                continue;
            }
            if !external {
                continue; // 内蔵ディスクは黙って対象外
            }
            if vols.iter().any(|v| v.extents != 1) {
                rejected.push((
                    key,
                    name,
                    "複数のディスクにまたがるボリュームは扱えません".into(),
                ));
                continue;
            }
            vols.sort_by_key(|v| (v.part_number, v.letter));
            // パーティション番号が取れない（0）・重複する場合はドライブレター順に振り直す
            let mut seen = HashSet::new();
            let renumber = vols
                .iter()
                .any(|v| v.part_number == 0 || !seen.insert(v.part_number));
            let partitions = vols
                .iter()
                .enumerate()
                .map(|(i, v)| PartitionDescriptor {
                    index: if renumber {
                        i as u32 + 1
                    } else {
                        v.part_number
                    },
                    device_path: format!("{}:", v.letter),
                    backend_id: format!("{}:", v.letter),
                    fs_type: Some(v.fs_type.clone()),
                    fs_uuid: Some(format!("{:04X}-{:04X}", v.serial >> 16, v.serial & 0xffff)),
                    part_guid: v.part_guid.clone(),
                    fs_label: v.label.clone(),
                    size_bytes: v.size,
                    mount_point: Some(PathBuf::from(format!("{}:\\", v.letter))),
                })
                .collect();
            found.insert(
                key.clone(),
                MediaDescriptor {
                    device_key: key,
                    vendor: info.as_ref().and_then(|i| i.vendor.clone()),
                    model: info.as_ref().and_then(|i| i.product.clone()),
                    serial: info.as_ref().and_then(|i| i.serial.clone()),
                    bus: info.as_ref().map(|i| bus_name(i.bus).to_string()),
                    size_bytes: vols.iter().map(|v| v.size).sum(),
                    partitions,
                },
            );
        }

        // ロック中（取り外し待ち）のディスクは、物理的に抜かれるまで「接続中」とみなす
        let mut held = self.held.lock().unwrap();
        held.retain(|_, h| media_present(h.disk));
        for (key, h) in held.iter() {
            found
                .entry(key.clone())
                .or_insert_with(|| h.descriptor.clone());
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
                let me = self.clone();
                let scanned = tokio::task::spawn_blocking(move || me.scan()).await;
                if let Ok((current, rejected)) = scanned {
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
                tokio::time::sleep(opts.poll_interval).await;
            }
        })
    }

    /// ドライブレターがまだ有効かを確認する（マウントポイントは走査時に設定済み）。
    pub fn prepare(&self, media: &MediaDescriptor) -> Result<MediaDescriptor, DeviceError> {
        for p in &media.partitions {
            let Some(mp) = &p.mount_point else {
                return Err(DeviceError::Mount {
                    partition: p.device_path.clone(),
                    reason: "no drive letter".into(),
                });
            };
            if std::fs::metadata(mp).is_err() {
                return Err(DeviceError::Gone);
            }
        }
        if media.partitions.is_empty() {
            return Err(DeviceError::NoReadablePartition(media.display_name()));
        }
        Ok(media.clone())
    }

    /// 全ボリュームをロック → ディスマウントし、取り外されるまでロックを保持する。
    pub async fn unmount_all(&self, media: &MediaDescriptor) -> Result<(), DeviceError> {
        let Some(disk) = disk_of_key(&media.device_key) else {
            return Err(DeviceError::Gone);
        };
        let letters: Vec<String> = media
            .partitions
            .iter()
            .map(|p| p.backend_id.clone())
            .collect();
        let locks = tokio::task::spawn_blocking(move || lock_and_dismount(&letters))
            .await
            .map_err(|e| DeviceError::Backend(e.to_string()))??;
        let mut descriptor = media.clone();
        for p in &mut descriptor.partitions {
            p.mount_point = None;
        }
        self.held.lock().unwrap().insert(
            media.device_key.clone(),
            Held {
                descriptor,
                disk,
                _locks: locks,
            },
        );
        Ok(())
    }

    pub async fn is_present(&self, device_key: &str) -> bool {
        match disk_of_key(device_key) {
            Some(disk) => tokio::task::spawn_blocking(move || media_present(disk))
                .await
                .unwrap_or(true),
            None => false,
        }
    }
}

/// ボリュームのルート（小文字の `c:\`）。
fn volume_root(path: &std::path::Path) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = vec![0u16; 1024];
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return None;
    }
    let s = from_wide(&buf);
    // `\\?\C:\` 形式でも比較できるようにする
    Some(s.trim_start_matches(r"\\?\").to_lowercase())
}

fn lock_and_dismount(letters: &[String]) -> Result<Vec<OwnedHandle>, DeviceError> {
    let mut held = Vec::new();
    for l in letters {
        let path = format!(r"\\.\{l}");
        let h =
            open_device(&path, GENERIC_READ | GENERIC_WRITE).map_err(|e| DeviceError::Unmount {
                partition: l.clone(),
                reason: if e.kind() == io::ErrorKind::PermissionDenied {
                    "ボリュームを開く権限がありません".into()
                } else {
                    e.to_string()
                },
            })?;
        unsafe {
            FlushFileBuffers(h.as_raw_handle() as HANDLE);
        }
        // 他のアプリがファイルを開いているとロックできない（＝使用中）。少し待って再試行する
        let mut locked = false;
        let mut last = String::new();
        for _ in 0..10 {
            match ioctl::<(), ()>(&h, FSCTL_LOCK_VOLUME, None, None, 0) {
                Ok(_) => {
                    locked = true;
                    break;
                }
                Err(e) => {
                    last = e.to_string();
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
        if !locked {
            return Err(DeviceError::Unmount {
                partition: l.clone(),
                reason: format!("使用中です（{last}）"),
            });
        }
        ioctl::<(), ()>(&h, FSCTL_DISMOUNT_VOLUME, None, None, 0).map_err(|e| {
            DeviceError::Unmount {
                partition: l.clone(),
                reason: e.to_string(),
            }
        })?;
        held.push(h);
    }
    Ok(held)
}
