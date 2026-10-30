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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    /// NAS 処理の無応答とみなすまでの時間（テスト用。既定は設定の `transfer.stall_timeout_secs`）。
    pub nas_stall_timeout: Option<std::time::Duration>,
    /// テスト用: 格納パスにこの文字列を含むファイルの初回転送を「応答なし」で固める（SMB 切断の模擬）。
    pub nas_hang_once: Option<String>,
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
    /// 転送・完了処理とユーザー操作を直列化する。接続待ちの間は保持しない。
    pub nas_operation: tokio::sync::Mutex<()>,
    /// 削除と、SD 再挿入時のセッション再開の確定を排他にする。
    pub media_session_operation: tokio::sync::Mutex<()>,
    /// 操作待ちのファイルはチャンク境界で転送を中断する。
    pub nas_cancel_files: Mutex<HashMap<String, usize>>,
    /// `nas_operation` を待っているユーザー操作の数。転送・完了処理は区切りで譲る。
    pub nas_op_waiters: AtomicUsize,
    /// 「今すぐ再試行」の回数。増えたら応答のない NAS 処理を切り離してやり直す。
    pub nas_retry_epoch: AtomicU64,
    /// 応答がなく切り離した NAS 処理（OS 側で戻るのを待っているスレッド）の数。
    pub nas_hung_ops: Arc<AtomicUsize>,
    /// テスト用フック（`Overrides::nas_hang_once`）を一度だけ発動させる。
    pub nas_hang_fired: AtomicBool,
    /// 容量待ちのメディアを起こす（SSD 削除で空きが増えた等）。
    pub capacity_wakeup: Notify,
    pub shutdown: CancellationToken,
    pub snapshot_tx: watch::Sender<Arc<Snapshot>>,
    pub overrides: Overrides,
    pub paused: AtomicBool,
    /// 一括削除と IPC 操作を直列化する。
    pub maintenance: tokio::sync::RwLock<()>,
    /// 削除途中で失敗した場合も通常処理を再開させない（DB の意図と対応）。
    pub local_reset_pending: AtomicBool,
    pub local_reset_confirmation: Mutex<Option<(String, Instant)>>,
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
            nas_operation: tokio::sync::Mutex::new(()),
            media_session_operation: tokio::sync::Mutex::new(()),
            nas_cancel_files: Mutex::default(),
            nas_op_waiters: AtomicUsize::new(0),
            nas_retry_epoch: AtomicU64::new(0),
            nas_hung_ops: Arc::new(AtomicUsize::new(0)),
            nas_hang_fired: AtomicBool::new(false),
            capacity_wakeup: Notify::new(),
            shutdown,
            snapshot_tx,
            overrides,
            paused: AtomicBool::new(false),
            maintenance: tokio::sync::RwLock::new(()),
            local_reset_pending: AtomicBool::new(false),
            local_reset_confirmation: Mutex::new(None),
        })
    }

    /// NAS 処理との排他を取る。待っている間は転送・完了処理に区切りで譲ってもらい、
    /// 応答のない NAS 処理は切り離される（`watchdog`）ため、通常は数秒で取得できる。
    pub async fn lock_nas_operation(
        &self,
        timeout: std::time::Duration,
    ) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        self.nas_op_waiters.fetch_add(1, Ordering::SeqCst);
        self.nas_wakeup.notify_one();
        let guard = tokio::time::timeout(timeout, self.nas_operation.lock())
            .await
            .ok();
        self.nas_op_waiters.fetch_sub(1, Ordering::SeqCst);
        guard
    }

    pub fn nas_op_waiting(&self) -> bool {
        self.nas_op_waiters.load(Ordering::SeqCst) > 0
    }

    /// NAS 処理を無応答とみなすまでの時間。
    pub fn nas_stall_timeout(&self) -> std::time::Duration {
        self.overrides.nas_stall_timeout.unwrap_or_else(|| {
            std::time::Duration::from_secs(self.config().transfer.stall_timeout_secs.max(10))
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
        self.paused.load(Ordering::SeqCst) || self.local_reset_pending.load(Ordering::SeqCst)
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
    /// 転送以外の処理内容（最終確認・マニフェスト保存・SSD 削除など）。
    pub phase_detail: Option<String>,
    /// 現在の NAS 処理が進捗していない秒数（10 秒以上のとき）。
    pub stalled_secs: Option<u64>,
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
