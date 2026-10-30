//! 型付き JSON Lines プロトコル（6.4）。
//!
//! 1 行 = 1 メッセージ。接続直後にクライアントが `Hello` を送り、プロトコル版を確認する。
//! GUI は `Subscribe` で状態スナップショットの継続配信を受け、再接続時は最新スナップショットで
//! 再同期する（GUI はバックアップ状態を単独で保持しない）。

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
/// 1 メッセージの最大長（不正クライアント対策）。
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        client: String,
    },
    Request {
        id: u64,
        command: Command,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    GetState,
    Subscribe,
    ListSessions {
        limit: u32,
    },
    GetFiles {
        session_id: String,
        only_problems: bool,
        #[serde(default)]
        offset: u32,
        #[serde(default = "default_file_limit")]
        limit: u32,
    },
    ManageNasFiles {
        session_id: String,
        file_ids: Vec<String>,
        action: NasFileAction,
    },
    ListEvents {
        days: u32,
        limit: u32,
    },
    RetryNas,
    RetryUnmount {
        slot: usize,
    },
    Pause,
    Resume,
    GetSettings,
    GetLocalDataSummary,
    ResetLocalData {
        confirmation_token: String,
    },
    UpdateSettings {
        settings: SettingsView,
        password: Option<String>,
    },
    TestNas {
        settings: SettingsView,
        password: Option<String>,
    },
    InitializeNasTarget,
    ExportLogs {
        dest_dir: String,
    },
    RestoreLayout {
        session_id: String,
        dest_dir: String,
    },
    /// NAS 未検証のセッションを破棄し、SSD のステージングデータも削除する。
    DiscardSession {
        session_id: String,
    },
    /// デーモンの正常停止（インストーラ・更新時に使う。処理中のコピーは安全に中断される）
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        protocol_version: u32,
        daemon_version: String,
    },
    Response {
        id: u64,
        result: Result<Response, String>,
    },
    Snapshot {
        snapshot: Box<Snapshot>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok {
        message: String,
    },
    State {
        snapshot: Box<Snapshot>,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    Files {
        files: Vec<FileSummary>,
        #[serde(default)]
        total_files: u64,
    },
    Events {
        events: Vec<EventSummary>,
    },
    Settings {
        settings: SettingsView,
        password_stored: bool,
    },
    NasTest {
        report: NasTestReport,
    },
    LocalDataSummary {
        summary: LocalDataSummary,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalDataSummary {
    pub sessions: u64,
    pub files: u64,
    pub transfers: u64,
    pub catalog_entries: u64,
    pub devices: u64,
    pub events: u64,
    pub local_bytes: u64,
    pub reset_pending: bool,
    pub confirmation_token: String,
}

// ---------------------------------------------------------------------------
// スナップショット
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub generated_at_utc: String,
    pub daemon_version: String,
    pub paused: bool,
    pub slots: Vec<SlotView>,
    pub waiting: Vec<WaitingView>,
    pub nas: NasView,
    pub ssd: CapacityView,
    pub warnings: Vec<String>,
    /// GUI の通知音用: スロットの結果が変わるたびに増える番号。
    pub notification_seq: u64,
    pub last_notification: Option<Notification>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SlotPhase {
    #[default]
    Empty,
    Reading,
    Copying,
    CapacityWait,
    Blocked,
    Finalizing,
    Unmounting,
    DoneGreen,
    DoneYellow,
    UnmountFailed,
    Interrupted,
    Error,
}

impl SlotPhase {
    /// 表示文言（10.2）。色だけに頼らないよう必ず文言と記号を併記する。
    pub fn label(self) -> &'static str {
        match self {
            SlotPhase::Empty => "メディアを接続してください",
            SlotPhase::Reading => "読み込み中",
            SlotPhase::Copying => "コピー中／検証中",
            SlotPhase::CapacityWait => "容量確保待ち",
            SlotPhase::Blocked => "処理できません",
            SlotPhase::Finalizing => "仕上げ中",
            SlotPhase::Unmounting => "取り外し準備中",
            SlotPhase::DoneGreen => "コピー完了／取り外し可能",
            SlotPhase::DoneYellow => "未完了・取り外し可能（失敗あり）",
            SlotPhase::UnmountFailed => "安全な取り外し不可／使用中",
            SlotPhase::Interrupted => "処理中断／同じメディアを再接続してください",
            SlotPhase::Error => "エラー",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            SlotPhase::Empty => "◌",
            SlotPhase::Reading
            | SlotPhase::Copying
            | SlotPhase::Finalizing
            | SlotPhase::Unmounting => "●",
            SlotPhase::CapacityWait => "⏸",
            SlotPhase::DoneGreen => "✔",
            SlotPhase::DoneYellow => "▲",
            SlotPhase::Blocked
            | SlotPhase::UnmountFailed
            | SlotPhase::Interrupted
            | SlotPhase::Error => "✖",
        }
    }

    /// 「取り外し可能」を表示してよいのはアンマウント成功時のみ（FR-033）。
    pub fn removable(self) -> bool {
        matches!(self, SlotPhase::DoneGreen | SlotPhase::DoneYellow)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SlotView {
    pub slot_index: usize,
    pub phase: SlotPhase,
    pub media_name: Option<String>,
    pub message: String,
    pub session_id: Option<String>,
    pub folder_name: Option<String>,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub files_failed: u64,
    pub files_skipped: u64,
    pub rate_bytes_per_sec: Option<f64>,
    pub eta_secs: Option<u64>,
    pub removable: bool,
    pub details: SlotDetails,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SlotDetails {
    pub device_path: Option<String>,
    pub identity_confidence: Option<String>,
    pub inserted_at_utc: Option<String>,
    pub partitions: Vec<String>,
    pub current_file: Option<String>,
    pub failed_files: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WaitingView {
    pub media_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NasView {
    #[serde(default)]
    pub cancelled_files: u64,
    /// 転送以外に行っている処理（最終確認・マニフェスト保存・SSD 削除・応答待ちなど）
    #[serde(default)]
    pub phase_detail: Option<String>,
    /// 現在の NAS 処理が進捗していない秒数（10 秒以上のとき）
    #[serde(default)]
    pub stalled_secs: Option<u64>,
    /// 応答がなく切り離した NAS 処理の数（OS 側で戻るのを待っている）
    #[serde(default)]
    pub hung_operations: u64,
    /// 転送待ちが 0 件なのに完了していない理由（あれば）
    #[serde(default)]
    pub waiting_reason: Option<String>,
    pub configured: bool,
    pub connected: bool,
    pub target: String,
    pub status_text: String,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub current_file: Option<String>,
    pub current_bytes_done: u64,
    pub current_bytes_total: u64,
    pub rate_bytes_per_sec: Option<f64>,
    pub queued_files: u64,
    pub queued_bytes: u64,
    pub retry_wait_files: u64,
    pub uploading_files: u64,
    pub sessions_pending: u64,
    pub attempts: u32,
    pub next_retry_utc: Option<String>,
    pub last_error: Option<String>,
    pub needs_attention: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CapacityView {
    pub path: String,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub reserve_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Success,
    Warning,
    Error,
    NasComplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub seq: u64,
    pub kind: NotificationKind,
    pub message: String,
}

// ---------------------------------------------------------------------------
// 一覧・設定
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub folder_name: String,
    pub started_at_utc: String,
    pub state: String,
    pub nas_state: String,
    pub total_files: u64,
    pub total_bytes: u64,
    pub completed_files: u64,
    pub failed_files: u64,
    pub nas_verified_files: u64,
    pub staging_deleted: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSummary {
    pub file_id: String,
    pub source_rel_path: String,
    pub destination_rel_path: String,
    pub kind: String,
    pub size_bytes: u64,
    pub stage_state: String,
    pub nas_state: String,
    pub copy_attempts: u32,
    #[serde(default)]
    pub nas_attempts: u32,
    #[serde(default)]
    pub next_retry_utc: Option<String>,
    pub error: Option<String>,
}

fn default_file_limit() -> u32 {
    5000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NasFileAction {
    Retry,
    Cancel,
    DeleteLocal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventSummary {
    pub created_at_utc: String,
    pub level: String,
    pub event_type: String,
    pub message: String,
    pub session_id: Option<String>,
}

/// GUI で編集する設定（秘密値なし）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SettingsView {
    /// "disabled" | "gvfs" | "mounted_path"
    pub nas_mode: String,
    pub nas_url: String,
    pub nas_username: String,
    pub nas_domain: String,
    pub nas_mounted_path: String,
    pub nas_target_initialized: bool,
    pub final_rehash_before_cleanup: bool,
    pub play_success_sound: bool,
    pub play_warning_sound: bool,
    pub staging_directory: String,
    pub reserve_free_bytes: u64,
    pub max_active_media: u32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NasTestReport {
    pub success: bool,
    pub steps: Vec<(String, bool, String)>,
    pub free_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_roundtrip() {
        let m = ClientMessage::Request {
            id: 7,
            command: Command::GetFiles {
                session_id: "s".into(),
                only_problems: true,
                offset: 0,
                limit: 200,
            },
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains('\n'));
        assert_eq!(serde_json::from_str::<ClientMessage>(&s).unwrap(), m);

        let r = ServerMessage::Response {
            id: 7,
            result: Err("boom".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<ServerMessage>(&s).unwrap(), r);

        let snap = ServerMessage::Snapshot {
            snapshot: Box::new(Snapshot::default()),
        };
        let s = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<ServerMessage>(&s).unwrap(), snap);
    }

    #[test]
    fn removable_only_after_unmount_success() {
        assert!(SlotPhase::DoneGreen.removable());
        assert!(SlotPhase::DoneYellow.removable());
        assert!(!SlotPhase::UnmountFailed.removable());
        assert!(!SlotPhase::Copying.removable());
        assert!(!SlotPhase::Interrupted.removable());
    }
}
