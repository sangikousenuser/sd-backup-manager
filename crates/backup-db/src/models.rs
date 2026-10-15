use backup_core::classifier::MediaKind;
use backup_core::identity::Confidence;
use backup_core::state::{NasFileState, SessionNasState, SessionState, StageState};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRow {
    pub device_id: String,
    pub identity_hash: String,
    pub identity_confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionRow {
    pub partition_id: String,
    pub device_id: String,
    pub part_index: u32,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    pub session_id: String,
    pub device_id: String,
    pub folder_name: String,
    pub started_at_utc: String,
    pub local_timezone: String,
    pub identity_confidence: Confidence,
    pub state: SessionState,
    pub media_state: String,
    pub nas_state: SessionNasState,
    pub scan_completed: bool,
    pub media_unmounted: bool,
    pub total_files: u64,
    pub total_bytes: u64,
    pub completed_files: u64,
    pub failed_files: u64,
    pub nas_verified_files: u64,
    pub manifest_verified_at_utc: Option<String>,
    pub staging_deleted_at_utc: Option<String>,
    pub last_error: Option<String>,
    pub updated_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRow {
    pub file_id: String,
    pub session_id: String,
    pub partition_id: String,
    pub partition_index: u32,
    pub source_rel_path: String,
    pub source_rel_path_raw_hex: Option<String>,
    pub destination_rel_path: String,
    pub media_kind: MediaKind,
    pub group_key: Option<String>,
    pub size_bytes: u64,
    pub source_mtime_ns: Option<i64>,
    pub blake3_hash: Option<String>,
    pub stage_state: StageState,
    pub nas_state: NasFileState,
    pub copy_attempts: u32,
    pub error_code: Option<String>,
    pub last_error: Option<String>,
    pub reference_json: Option<String>,
    pub reference_verified: bool,
    pub verified_ssd_at_utc: Option<String>,
    pub verified_nas_at_utc: Option<String>,
}

/// 新規ファイル登録用。
#[derive(Debug, Clone)]
pub struct NewFile {
    pub session_id: String,
    pub partition_id: String,
    pub partition_index: u32,
    pub source_rel_path: String,
    pub source_rel_path_raw_hex: Option<String>,
    pub destination_rel_path: String,
    pub media_kind: MediaKind,
    pub group_key: Option<String>,
    pub size_bytes: u64,
    pub source_mtime_ns: Option<i64>,
}

/// NAS 転送ジョブ（transfers + files + sessions の結合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferJob {
    pub transfer_id: String,
    pub file_id: String,
    pub session_id: String,
    pub folder_name: String,
    pub destination_rel_path: String,
    pub blake3_hash: String,
    pub size_bytes: u64,
    pub attempts: u32,
}

pub mod transfer_state {
    pub const QUEUED: &str = "QUEUED";
    pub const UPLOADING: &str = "UPLOADING";
    pub const RETRY_WAIT: &str = "RETRY_WAIT";
    pub const DONE: &str = "DONE";
    pub const FAILED: &str = "FAILED";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRow {
    pub catalog_id: String,
    pub device_identity_hash: String,
    pub partition_fingerprint: String,
    pub source_rel_path: String,
    pub size_bytes: u64,
    pub source_mtime_ns: Option<i64>,
    pub blake3_hash: String,
    pub nas_uri: String,
    pub nas_session_folder: String,
    pub nas_dest_rel_path: String,
    pub nas_session_id: String,
    pub verified_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRow {
    pub event_id: i64,
    pub session_id: Option<String>,
    pub level: String,
    pub event_type: String,
    pub message: String,
    pub created_at_utc: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NasQueueStats {
    pub queued_files: u64,
    pub queued_bytes: u64,
    pub retry_wait_files: u64,
    pub failed_files: u64,
    pub uploading_files: u64,
    pub next_attempt_utc: Option<String>,
    pub max_attempts: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub sessions_interrupted: Vec<String>,
    pub files_reset: u64,
    pub transfers_reset: u64,
}
