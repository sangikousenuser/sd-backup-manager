//! デーモン全体の共有状態。
//!
//! 永続状態は SQLite（`Db`）、画面表示用の揮発状態は `Runtime` に置く。GUI へは
//! `Runtime` + DB 集計から作るスナップショットだけを配信する。

use backup_config::{AppPaths, Config};
use backup_core::classifier::FormatDefinitions;
use backup_core::identity::MediaDescriptor;
use backup_core::policy::RateEstimator;
use backup_db::Db;
use backup_device::DeviceBackend;
use backup_ipc::{Notification, NotificationKind, SlotPhase, Snapshot};
use backup_storage::capacity::FsSpace;
use backup_storage::local::Staging;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

/// テスト・シミュレーション用の差し替え。
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// NAS 接続の代わりにこのディレクトリを NAS ルートとして使う（マウントポイント検査なし）。
    pub nas_local_dir: Option<PathBuf>,
    /// 空き容量の代わりに使う値（容量制御のテスト用）。
    pub fake_ssd_free_bytes: Option<u64>,
}

pub struct Shared {
    pub paths: AppPaths,
    pub config: RwLock<Config>,
    pub formats: RwLock<FormatDefinitions>,
    db: Mutex<Db>,
    pub staging: Staging,
    pub devices: DeviceBackend,
    pub runtime: Mutex<Runtime>,
    /// NAS ワーカーを起こす（新しい検証済みファイル・今すぐ再試行など）。
    pub nas_wakeup: Notify,
    /// 容量待ちのメディアを起こす（SSD 削除で空きが増えた等）。
    pub capacity_wakeup: Notify,
    pub shutdown: CancellationToken,
    pub snapshot_tx: watch::Sender<Arc<Snapshot>>,
    pub overrides: Overrides,
    pub paused: AtomicBool,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: AppPaths,
        config: Config,
        formats: FormatDefinitions,
        db: Db,
        staging: Staging,
        devices: DeviceBackend,
        overrides: Overrides,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let slots = config.max_active_media as usize;
        let (snapshot_tx, _) = watch::channel(Arc::new(Snapshot::default()));
        Arc::new(Self {
            paths,
            config: RwLock::new(config),
            formats: RwLock::new(formats),
            db: Mutex::new(db),
            staging,
            devices,
            runtime: Mutex::new(Runtime::new(slots)),
            nas_wakeup: Notify::new(),
            capacity_wakeup: Notify::new(),
            shutdown,
            snapshot_tx,
            overrides,
            paused: AtomicBool::new(false),
        })
    }

    /// DB を短時間ロックする（長い I/O の間は保持しないこと）。
    pub fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn rt(&self) -> MutexGuard<'_, Runtime> {
        self.runtime.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn config(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// 履歴（events）とトレースログの両方に記録する。
    pub fn event(&self, session_id: Option<&str>, level: &str, event_type: &str, message: &str) {
        match level {
            "error" => tracing::error!(session = session_id, event_type, "{message}"),
            "warn" => tracing::warn!(session = session_id, event_type, "{message}"),
            _ => tracing::info!(session = session_id, event_type, "{message}"),
        }
        if let Err(e) = self.db().log_event(session_id, level, event_type, message) {
            tracing::error!("failed to record event: {e}");
        }
    }

    pub fn notify(&self, kind: NotificationKind, message: impl Into<String>) {
        let mut rt = self.rt();
        rt.notification_seq += 1;
        let seq = rt.notification_seq;
        rt.last_notification = Some(Notification {
            seq,
            kind,
            message: message.into(),
        });
    }

    pub fn warn(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::warn!("{message}");
        let mut rt = self.rt();
        rt.warnings.retain(|(_, m)| m != &message);
        rt.warnings.push_back((Instant::now(), message));
        while rt.warnings.len() > 5 {
            rt.warnings.pop_front();
        }
    }
}

/// 画面上の 1 枠（メディア 1 / メディア 2）。
pub struct SlotRuntime {
    pub media: MediaDescriptor,
    pub inserted_at_utc: String,
    pub phase: SlotPhase,
    pub message: String,
    pub session_id: Option<String>,
    pub folder_name: Option<String>,
    pub confidence: Option<String>,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub files_failed: u64,
    pub files_skipped: u64,
    pub rate: RateEstimator,
    pub current_file: Option<String>,
    pub failed_files: Vec<String>,
    pub warnings: Vec<String>,
    /// 物理的な抜去・シャットダウンで中断する。
    pub cancel: CancellationToken,
    /// 処理タスクが終了したか（以後は抜去を待つだけ）。
    pub task_finished: bool,
    pub removed: bool,
    /// 容量予約: これから SSD に書き込む見込みのバイト数。
    pub remaining_bytes: u64,
    pub admitted: bool,
    pub queued_at: Instant,
}

impl SlotRuntime {
    pub fn new(media: MediaDescriptor, cancel: CancellationToken) -> Self {
        Self {
            media,
            inserted_at_utc: backup_db::now_utc(),
            phase: SlotPhase::Reading,
            message: "読み込み中".into(),
            session_id: None,
            folder_name: None,
            confidence: None,
            bytes_done: 0,
            bytes_total: 0,
            files_done: 0,
            files_total: 0,
            files_failed: 0,
            files_skipped: 0,
            rate: RateEstimator::new(Duration::from_secs(15)),
            current_file: None,
            failed_files: Vec::new(),
            warnings: Vec::new(),
            cancel,
            task_finished: false,
            removed: false,
            remaining_bytes: 0,
            admitted: false,
            queued_at: Instant::now(),
        }
    }
}

pub struct WaitingMedia {
    pub media: MediaDescriptor,
    pub since: Instant,
}

#[derive(Default)]
pub struct NasRuntime {
    pub connected: bool,
    pub status_text: String,
    pub space: Option<FsSpace>,
    pub current_file: Option<String>,
    pub current_done: u64,
    pub current_total: u64,
    pub rate: Option<RateEstimator>,
    pub cumulative_bytes: u64,
    pub attempts: u32,
    pub next_retry_utc: Option<String>,
    pub last_error: Option<String>,
    pub needs_attention: bool,
}

pub struct Runtime {
    pub slots: Vec<Option<SlotRuntime>>,
    pub waiting: VecDeque<WaitingMedia>,
    pub nas: NasRuntime,
    pub warnings: VecDeque<(Instant, String)>,
    pub ssd_space: Option<FsSpace>,
    pub notification_seq: u64,
    pub last_notification: Option<Notification>,
    /// identity_hash → 接続中の device_key（クローン検出）
    pub connected_identities: HashMap<String, Vec<String>>,
}

impl Runtime {
    pub fn new(slots: usize) -> Self {
        Self {
            slots: (0..slots.max(1)).map(|_| None).collect(),
            waiting: VecDeque::new(),
            nas: NasRuntime {
                status_text: "未接続".into(),
                ..Default::default()
            },
            warnings: VecDeque::new(),
            ssd_space: None,
            notification_seq: 0,
            last_notification: None,
            connected_identities: HashMap::new(),
        }
    }

    pub fn slot_of(&self, device_key: &str) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|s| s.media.device_key == device_key))
    }

    pub fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(Option::is_none)
    }

    /// 他の（開始済み）セッションの残り書き込み予約の合計。
    pub fn reserved_by_others(&self, me: usize) -> u64 {
        self.slots
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != me)
            .filter_map(|(_, s)| s.as_ref())
            .filter(|s| s.admitted && !s.task_finished)
            .map(|s| s.remaining_bytes)
            .sum()
    }

    /// 自分より先に容量待ちに入ったスロットがあるか（先着優先）。
    pub fn someone_waiting_before(&self, me: usize) -> bool {
        let Some(my) = self.slots.get(me).and_then(|s| s.as_ref()) else {
            return false;
        };
        self.slots.iter().enumerate().any(|(i, s)| {
            i != me
                && s.as_ref().is_some_and(|s| {
                    !s.admitted
                        && !s.task_finished
                        && s.phase == SlotPhase::CapacityWait
                        && s.queued_at < my.queued_at
                })
        })
    }
}
