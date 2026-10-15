-- SD Backup Manager schema v1（設計書 8.2 をベースに実装上必要な列を追加）

CREATE TABLE IF NOT EXISTS devices (
  device_id TEXT PRIMARY KEY,
  identity_hash TEXT NOT NULL UNIQUE,
  identity_confidence TEXT NOT NULL
    CHECK(identity_confidence IN ('high','medium','low','ambiguous')),
  bus TEXT, vendor TEXT, model TEXT, serial TEXT,
  size_bytes INTEGER,
  first_seen_utc TEXT NOT NULL,
  last_seen_utc TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_devices_identity ON devices(identity_hash);

CREATE TABLE IF NOT EXISTS partitions (
  partition_id TEXT PRIMARY KEY,
  device_id TEXT NOT NULL REFERENCES devices(device_id),
  part_index INTEGER NOT NULL,
  part_guid TEXT,
  fs_uuid TEXT,
  fs_type TEXT,
  fs_label TEXT,
  size_bytes INTEGER,
  fingerprint TEXT NOT NULL,
  UNIQUE(device_id, part_index)
);

CREATE TABLE IF NOT EXISTS sessions (
  session_id TEXT PRIMARY KEY,
  device_id TEXT NOT NULL REFERENCES devices(device_id),
  folder_name TEXT NOT NULL UNIQUE,
  started_at_utc TEXT NOT NULL,
  local_timezone TEXT NOT NULL,
  identity_confidence TEXT NOT NULL,
  state TEXT NOT NULL,
  media_state TEXT NOT NULL,
  nas_state TEXT NOT NULL,
  scan_completed INTEGER NOT NULL DEFAULT 0,
  media_unmounted INTEGER NOT NULL DEFAULT 0,
  total_files INTEGER NOT NULL DEFAULT 0,
  total_bytes INTEGER NOT NULL DEFAULT 0,
  completed_files INTEGER NOT NULL DEFAULT 0,
  failed_files INTEGER NOT NULL DEFAULT 0,
  nas_verified_files INTEGER NOT NULL DEFAULT 0,
  manifest_verified_at_utc TEXT,
  staging_deleted_at_utc TEXT,
  last_error TEXT,
  updated_at_utc TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_state ON sessions(state, updated_at_utc);
CREATE INDEX IF NOT EXISTS idx_sessions_device ON sessions(device_id, state);

CREATE TABLE IF NOT EXISTS files (
  file_id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
  partition_id TEXT NOT NULL REFERENCES partitions(partition_id),
  partition_index INTEGER NOT NULL,
  source_rel_path TEXT NOT NULL,
  source_rel_path_raw_hex TEXT,
  destination_rel_path TEXT NOT NULL,
  media_kind TEXT NOT NULL CHECK(media_kind IN ('photo','video','other')),
  group_key TEXT,
  size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
  source_mtime_ns INTEGER,
  blake3_hash TEXT,
  stage_state TEXT NOT NULL,
  nas_state TEXT NOT NULL,
  copy_attempts INTEGER NOT NULL DEFAULT 0,
  error_code TEXT,
  last_error TEXT,
  reference_json TEXT,
  reference_verified INTEGER NOT NULL DEFAULT 0,
  verified_ssd_at_utc TEXT,
  verified_nas_at_utc TEXT,
  UNIQUE(session_id, partition_id, source_rel_path),
  UNIQUE(session_id, destination_rel_path)
);
CREATE INDEX IF NOT EXISTS idx_files_stage ON files(session_id, stage_state);
CREATE INDEX IF NOT EXISTS idx_files_nas ON files(session_id, nas_state);

CREATE TABLE IF NOT EXISTS transfers (
  transfer_id TEXT PRIMARY KEY,
  file_id TEXT NOT NULL UNIQUE REFERENCES files(file_id) ON DELETE RESTRICT,
  session_id TEXT NOT NULL REFERENCES sessions(session_id),
  state TEXT NOT NULL,
  nas_temp_rel_path TEXT,
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_utc TEXT,
  last_error TEXT,
  created_at_utc TEXT NOT NULL,
  updated_at_utc TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_transfers_retry ON transfers(state, next_attempt_utc);

CREATE TABLE IF NOT EXISTS catalog (
  catalog_id TEXT PRIMARY KEY,
  device_identity_hash TEXT NOT NULL,
  partition_fingerprint TEXT NOT NULL,
  source_rel_path TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  source_mtime_ns INTEGER,
  blake3_hash TEXT NOT NULL,
  nas_uri TEXT NOT NULL,
  nas_session_folder TEXT NOT NULL,
  nas_dest_rel_path TEXT NOT NULL,
  nas_session_id TEXT NOT NULL,
  verified_at_utc TEXT NOT NULL,
  availability_state TEXT NOT NULL DEFAULT 'verified'
);
CREATE INDEX IF NOT EXISTS idx_catalog_lookup
  ON catalog(device_identity_hash, partition_fingerprint, source_rel_path, size_bytes);

CREATE TABLE IF NOT EXISTS events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT REFERENCES sessions(session_id),
  level TEXT NOT NULL,
  event_type TEXT NOT NULL,
  message TEXT NOT NULL,
  created_at_utc TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at_utc);

CREATE TABLE IF NOT EXISTS app_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
