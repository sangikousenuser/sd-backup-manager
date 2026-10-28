use crate::models::*;
use crate::{Db, DbError, Result, now_utc};
use backup_core::classifier::MediaKind;
use backup_core::identity::{Confidence, MediaDescriptor, PartitionDescriptor};
use backup_core::session::{folder_base, folder_candidate, new_session_id, parse_timezone};
use backup_core::state::{NasFileState, SessionNasState, SessionState, StageState};
use rusqlite::{OptionalExtension, Row, params};
use std::str::FromStr;

fn parse<T: FromStr>(s: String, what: &str) -> rusqlite::Result<T> {
    T::from_str(&s).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("invalid {what}: {s}").into(),
        )
    })
}

fn confidence(s: String) -> rusqlite::Result<Confidence> {
    Confidence::parse(&s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("invalid confidence {s}").into(),
        )
    })
}

fn kind(s: String) -> rusqlite::Result<MediaKind> {
    MediaKind::parse(&s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("invalid kind {s}").into(),
        )
    })
}

const SESSION_COLS: &str = "session_id, device_id, folder_name, started_at_utc, local_timezone,
    identity_confidence, state, media_state, nas_state, scan_completed, media_unmounted,
    total_files, total_bytes, completed_files, failed_files, nas_verified_files,
    manifest_verified_at_utc, staging_deleted_at_utc, last_error, updated_at_utc";

fn session_row(r: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_id: r.get(0)?,
        device_id: r.get(1)?,
        folder_name: r.get(2)?,
        started_at_utc: r.get(3)?,
        local_timezone: r.get(4)?,
        identity_confidence: confidence(r.get(5)?)?,
        state: parse(r.get(6)?, "session state")?,
        media_state: r.get(7)?,
        nas_state: parse::<SessionNasState>(r.get(8)?, "nas state")?,
        scan_completed: r.get::<_, i64>(9)? != 0,
        media_unmounted: r.get::<_, i64>(10)? != 0,
        total_files: r.get::<_, i64>(11)? as u64,
        total_bytes: r.get::<_, i64>(12)? as u64,
        completed_files: r.get::<_, i64>(13)? as u64,
        failed_files: r.get::<_, i64>(14)? as u64,
        nas_verified_files: r.get::<_, i64>(15)? as u64,
        manifest_verified_at_utc: r.get(16)?,
        staging_deleted_at_utc: r.get(17)?,
        last_error: r.get(18)?,
        updated_at_utc: r.get(19)?,
    })
}

const FILE_COLS: &str = "file_id, session_id, partition_id, partition_index, source_rel_path,
    source_rel_path_raw_hex, destination_rel_path, media_kind, group_key, size_bytes,
    source_mtime_ns, blake3_hash, stage_state, nas_state, copy_attempts, error_code, last_error,
    reference_json, reference_verified, verified_ssd_at_utc, verified_nas_at_utc";

fn file_row(r: &Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        file_id: r.get(0)?,
        session_id: r.get(1)?,
        partition_id: r.get(2)?,
        partition_index: r.get::<_, i64>(3)? as u32,
        source_rel_path: r.get(4)?,
        source_rel_path_raw_hex: r.get(5)?,
        destination_rel_path: r.get(6)?,
        media_kind: kind(r.get(7)?)?,
        group_key: r.get(8)?,
        size_bytes: r.get::<_, i64>(9)? as u64,
        source_mtime_ns: r.get(10)?,
        blake3_hash: r.get(11)?,
        stage_state: parse(r.get(12)?, "stage state")?,
        nas_state: parse(r.get(13)?, "nas state")?,
        copy_attempts: r.get::<_, i64>(14)? as u32,
        error_code: r.get(15)?,
        last_error: r.get(16)?,
        reference_json: r.get(17)?,
        reference_verified: r.get::<_, i64>(18)? != 0,
        verified_ssd_at_utc: r.get(19)?,
        verified_nas_at_utc: r.get(20)?,
    })
}

const CATALOG_COLS: &str =
    "catalog_id, device_identity_hash, partition_fingerprint, source_rel_path,
    size_bytes, source_mtime_ns, blake3_hash, nas_uri, nas_session_folder, nas_dest_rel_path,
    nas_session_id, verified_at_utc";

fn catalog_row(r: &Row<'_>) -> rusqlite::Result<CatalogRow> {
    Ok(CatalogRow {
        catalog_id: r.get(0)?,
        device_identity_hash: r.get(1)?,
        partition_fingerprint: r.get(2)?,
        source_rel_path: r.get(3)?,
        size_bytes: r.get::<_, i64>(4)? as u64,
        source_mtime_ns: r.get(5)?,
        blake3_hash: r.get(6)?,
        nas_uri: r.get(7)?,
        nas_session_folder: r.get(8)?,
        nas_dest_rel_path: r.get(9)?,
        nas_session_id: r.get(10)?,
        verified_at_utc: r.get(11)?,
    })
}

fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

impl Db {
    pub fn card_names(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT card_id FROM cards ORDER BY card_id")?;
        Ok(stmt
            .query_map([], |r| Ok(format!("sd{}", r.get::<_, i64>(0)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn known_card(&self, device_id: &str, stable_key: Option<&str>) -> Result<Option<String>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT card_id FROM device_cards WHERE device_id = ?1",
                [device_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = match id {
            Some(id) => Some(id),
            None => match stable_key {
                Some(key) => self
                    .conn
                    .query_row(
                        "SELECT card_id FROM cards WHERE stable_key = ?1",
                        [key],
                        |r| r.get(0),
                    )
                    .optional()?,
                None => None,
            },
        };
        Ok(id.map(|id| format!("sd{id}")))
    }

    /// 保存先だけを紐付ける。device_id やパーティション指紋は変更しない。
    pub fn assign_card(
        &self,
        device_id: &str,
        name: Option<&str>,
        stable_key: Option<&str>,
    ) -> Result<String> {
        let tx = self.conn.unchecked_transaction()?;
        let id = match name {
            Some(name) => {
                if !backup_core::session::is_valid_card_name(name) {
                    return Err(DbError::Corrupt("カード名が不正です".into()));
                }
                let id: i64 = name[2..]
                    .parse()
                    .map_err(|_| DbError::Corrupt("カード番号が不正です".into()))?;
                tx.query_row("SELECT card_id FROM cards WHERE card_id = ?1", [id], |r| {
                    r.get::<_, i64>(0)
                })?
            }
            None => {
                tx.execute("INSERT INTO cards(stable_key) VALUES(?1)", [stable_key])?;
                tx.last_insert_rowid()
            }
        };
        tx.execute("INSERT INTO device_cards(device_id, card_id) VALUES(?1, ?2) ON CONFLICT(device_id) DO UPDATE SET card_id = excluded.card_id", params![device_id, id])?;
        tx.commit()?;
        Ok(format!("sd{id}"))
    }

    /// 旧セッションの日時フォルダには触れず、新規だけ sdN/日時 にする。
    pub fn create_card_session(
        &self,
        device_id: &str,
        card: &str,
        confidence: Confidence,
        now: chrono::DateTime<chrono::Utc>,
        timezone: &str,
    ) -> Result<SessionRow> {
        if !backup_core::session::is_valid_card_name(card) {
            return Err(DbError::Corrupt("カード名が不正です".into()));
        }
        let base = folder_base(now, parse_timezone(timezone));
        let sid = new_session_id();
        let started = crate::to_db_time(now);
        for n in 0..10_000u32 {
            let time = if n == 0 {
                base.clone()
            } else {
                folder_candidate(&base, n)
            };
            let folder = format!("{card}/{time}");
            match self.conn.execute(
                "INSERT INTO sessions(session_id, device_id, folder_name, started_at_utc, local_timezone, identity_confidence, state, media_state, nas_state, updated_at_utc) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'DETECTED', 'DETECTED', 'WAITING', ?4)",
                params![sid, device_id, folder, started, timezone, confidence.as_str()],
            ) {
                Ok(_) => return self.get_session(&sid)?.ok_or(DbError::NotFound(sid)),
                Err(rusqlite::Error::SqliteFailure(e, _)) if e.extended_code == 2067 => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(DbError::Corrupt("日時フォルダを確保できません".into()))
    }

    pub fn save_directory(
        &self,
        sid: &str,
        partition: u32,
        source: &str,
        dest: &str,
    ) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO session_directories(session_id, partition_index, source_rel_path, destination_rel_path) VALUES(?1, ?2, ?3, ?4)", params![sid, partition, source, dest])?;
        Ok(())
    }

    pub fn directories(&self, sid: &str) -> Result<Vec<(u32, String, String)>> {
        let mut stmt = self.conn.prepare("SELECT partition_index, source_rel_path, destination_rel_path FROM session_directories WHERE session_id = ?1 ORDER BY source_rel_path")?;
        Ok(stmt
            .query_map([sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ------------------------------------------------------------------
    // devices / partitions
    // ------------------------------------------------------------------

    /// デバイスを登録（既存なら最終接続日時・信頼度を更新）し device_id を返す。
    pub fn upsert_device(
        &self,
        media: &MediaDescriptor,
        confidence: Confidence,
    ) -> Result<DeviceRow> {
        let identity = media.identity_hash();
        let now = now_utc();
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT device_id FROM devices WHERE identity_hash = ?1",
                [&identity],
                |r| r.get(0),
            )
            .optional()?;
        let device_id = match existing {
            Some(id) => {
                self.conn.execute(
                    "UPDATE devices SET last_seen_utc = ?2, identity_confidence = ?3 WHERE device_id = ?1",
                    params![id, now, confidence.as_str()],
                )?;
                id
            }
            None => {
                let id = new_id();
                self.conn.execute(
                    "INSERT INTO devices(device_id, identity_hash, identity_confidence, bus, vendor, model,
                        serial, size_bytes, first_seen_utc, last_seen_utc)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                    params![
                        id,
                        identity,
                        confidence.as_str(),
                        media.bus,
                        media.vendor,
                        media.model,
                        media.serial,
                        media.size_bytes as i64,
                        now
                    ],
                )?;
                id
            }
        };
        Ok(DeviceRow {
            device_id,
            identity_hash: identity,
            identity_confidence: confidence,
        })
    }

    pub fn upsert_partitions(
        &self,
        device_id: &str,
        parts: &[PartitionDescriptor],
    ) -> Result<Vec<PartitionRow>> {
        let mut out = Vec::new();
        for p in parts {
            let fp = p.fingerprint();
            let existing: Option<(String, String)> = self
                .conn
                .query_row(
                    "SELECT partition_id, fingerprint FROM partitions WHERE device_id = ?1 AND part_index = ?2",
                    params![device_id, p.index as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let partition_id = match existing {
                Some((id, old_fp)) => {
                    if old_fp != fp {
                        // identity_hash が同じならパーティション指紋も同じはず
                        return Err(DbError::Corrupt(format!(
                            "partition fingerprint mismatch for {device_id}/{}",
                            p.index
                        )));
                    }
                    id
                }
                None => {
                    let id = new_id();
                    self.conn.execute(
                        "INSERT INTO partitions(partition_id, device_id, part_index, part_guid, fs_uuid, fs_type,
                            fs_label, size_bytes, fingerprint)
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            id,
                            device_id,
                            p.index as i64,
                            p.part_guid,
                            p.fs_uuid,
                            p.fs_type,
                            p.fs_label,
                            p.size_bytes as i64,
                            fp
                        ],
                    )?;
                    id
                }
            };
            out.push(PartitionRow {
                partition_id,
                device_id: device_id.to_string(),
                part_index: p.index,
                fingerprint: fp,
            });
        }
        Ok(out)
    }

    pub fn device_identity_hash(&self, device_id: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT identity_hash FROM devices WHERE device_id = ?1",
                [device_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| DbError::NotFound(device_id.to_string()))
    }

    /// (identity_hash, vendor, model, serial, bus, size_bytes)
    #[allow(clippy::type_complexity)]
    pub fn device_info(
        &self,
        device_id: &str,
    ) -> Result<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    )> {
        self.conn
            .query_row(
                "SELECT identity_hash, vendor, model, serial, bus, size_bytes FROM devices WHERE device_id = ?1",
                [device_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?
            .ok_or_else(|| DbError::NotFound(device_id.to_string()))
    }

    /// (index, fs_type, fs_uuid, part_guid, fs_label, size_bytes, fingerprint)
    #[allow(clippy::type_complexity)]
    pub fn partition_details(
        &self,
        device_id: &str,
    ) -> Result<
        Vec<(
            u32,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            u64,
            String,
        )>,
    > {
        let mut stmt = self.conn.prepare(
            "SELECT part_index, fs_type, fs_uuid, part_guid, fs_label, size_bytes, fingerprint
             FROM partitions WHERE device_id = ?1 ORDER BY part_index",
        )?;
        let rows = stmt
            .query_map([device_id], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u32,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0) as u64,
                    r.get(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn partitions_for_device(&self, device_id: &str) -> Result<Vec<PartitionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT partition_id, device_id, part_index, fingerprint FROM partitions WHERE device_id = ?1 ORDER BY part_index",
        )?;
        let rows = stmt
            .query_map([device_id], |r| {
                Ok(PartitionRow {
                    partition_id: r.get(0)?,
                    device_id: r.get(1)?,
                    part_index: r.get::<_, i64>(2)? as u32,
                    fingerprint: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------------------
    // sessions
    // ------------------------------------------------------------------

    /// 新しいセッションを作る。フォルダ名の一意性は UNIQUE 制約で排他的に保証する。
    pub fn create_session(
        &self,
        device_id: &str,
        confidence: Confidence,
        now: chrono::DateTime<chrono::Utc>,
        timezone: &str,
    ) -> Result<SessionRow> {
        let base = folder_base(now, parse_timezone(timezone));
        let session_id = new_session_id();
        let started = crate::to_db_time(now);
        for n in 1..10_000u32 {
            let folder = folder_candidate(&base, n);
            let res = self.conn.execute(
                "INSERT INTO sessions(session_id, device_id, folder_name, started_at_utc, local_timezone,
                    identity_confidence, state, media_state, nas_state, updated_at_utc)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'DETECTED', 'DETECTED', 'WAITING', ?4)",
                params![session_id, device_id, folder, started, timezone, confidence.as_str()],
            );
            match res {
                Ok(_) => {
                    return self
                        .get_session(&session_id)?
                        .ok_or_else(|| DbError::NotFound(session_id.clone()));
                }
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(DbError::Corrupt(
            "could not allocate a unique folder name".into(),
        ))
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE session_id = ?1"),
                [session_id],
                session_row,
            )
            .optional()?)
    }

    /// 同一デバイスの再開可能なセッション（最新）。
    pub fn find_resumable_session(&self, device_id: &str) -> Result<Option<SessionRow>> {
        let states: Vec<&str> = SessionState::ALL
            .iter()
            .filter(|s| s.is_resumable())
            .map(|s| s.as_str())
            .collect();
        let placeholders = states
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",");
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {SESSION_COLS} FROM sessions WHERE device_id = ?1 AND state IN ({placeholders})
                     ORDER BY started_at_utc DESC LIMIT 1"
                ),
                [device_id],
                session_row,
            )
            .optional()?)
    }

    pub fn list_sessions(&self, limit: u32) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions ORDER BY started_at_utc DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map([limit as i64], session_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn sessions_in_states(&self, states: &[SessionState]) -> Result<Vec<SessionRow>> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = states
            .iter()
            .map(|s| format!("'{}'", s.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE state IN ({placeholders}) ORDER BY started_at_utc"
        ))?;
        let rows = stmt
            .query_map([], session_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 状態遷移（許可された遷移のみ）。同一状態への遷移は何もしない。
    pub fn transition_session(&self, session_id: &str, to: SessionState) -> Result<SessionState> {
        let current: String = self
            .conn
            .query_row(
                "SELECT state FROM sessions WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| DbError::NotFound(session_id.to_string()))?;
        let from = SessionState::from_str(&current).map_err(|e| DbError::Corrupt(e.to_string()))?;
        if from == to {
            return Ok(from);
        }
        if !from.can_transition_to(to) {
            return Err(DbError::InvalidTransition {
                from: from.to_string(),
                to: to.to_string(),
            });
        }
        self.conn.execute(
            "UPDATE sessions SET state = ?2, updated_at_utc = ?3 WHERE session_id = ?1 AND state = ?4",
            params![session_id, to.as_str(), now_utc(), from.as_str()],
        )?;
        Ok(from)
    }

    pub fn set_media_state(&self, session_id: &str, media_state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET media_state = ?2, updated_at_utc = ?3 WHERE session_id = ?1",
            params![session_id, media_state, now_utc()],
        )?;
        Ok(())
    }

    pub fn set_session_nas_state(&self, session_id: &str, state: SessionNasState) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET nas_state = ?2, updated_at_utc = ?3 WHERE session_id = ?1",
            params![session_id, state.as_str(), now_utc()],
        )?;
        Ok(())
    }

    pub fn set_scan_completed(&self, session_id: &str, done: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET scan_completed = ?2, updated_at_utc = ?3 WHERE session_id = ?1",
            params![session_id, done as i64, now_utc()],
        )?;
        Ok(())
    }

    pub fn set_media_unmounted(&self, session_id: &str, done: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET media_unmounted = ?2, updated_at_utc = ?3 WHERE session_id = ?1",
            params![session_id, done as i64, now_utc()],
        )?;
        Ok(())
    }

    pub fn set_session_error(&self, session_id: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_error = ?2, updated_at_utc = ?3 WHERE session_id = ?1",
            params![session_id, error, now_utc()],
        )?;
        Ok(())
    }

    pub fn mark_manifest_verified(&self, session_id: &str, verified: bool) -> Result<()> {
        let v = if verified { Some(now_utc()) } else { None };
        self.conn.execute(
            "UPDATE sessions SET manifest_verified_at_utc = ?2, updated_at_utc = ?3 WHERE session_id = ?1",
            params![session_id, v, now_utc()],
        )?;
        Ok(())
    }

    pub fn mark_staging_deleted(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET staging_deleted_at_utc = ?2, updated_at_utc = ?2 WHERE session_id = ?1",
            params![session_id, now_utc()],
        )?;
        Ok(())
    }

    /// files テーブルから集計値を再計算して保存する。
    pub fn refresh_session_counters(&self, session_id: &str) -> Result<SessionRow> {
        self.conn.execute(
            "UPDATE sessions SET
                total_files = (SELECT COUNT(*) FROM files WHERE session_id = ?1),
                total_bytes = (SELECT COALESCE(SUM(size_bytes), 0) FROM files WHERE session_id = ?1),
                completed_files = (SELECT COUNT(*) FROM files WHERE session_id = ?1
                    AND stage_state IN ('SSD_VERIFIED', 'SKIPPED_VERIFIED')),
                failed_files = (SELECT COUNT(*) FROM files WHERE session_id = ?1
                    AND stage_state = 'FAILED_TERMINAL'),
                nas_verified_files = (SELECT COUNT(*) FROM files WHERE session_id = ?1
                    AND (nas_state = 'NAS_VERIFIED' OR (stage_state = 'SKIPPED_VERIFIED' AND reference_verified = 1))),
                updated_at_utc = ?2
             WHERE session_id = ?1",
            params![session_id, now_utc()],
        )?;
        self.get_session(session_id)?
            .ok_or_else(|| DbError::NotFound(session_id.to_string()))
    }

    // ------------------------------------------------------------------
    // files
    // ------------------------------------------------------------------

    pub fn insert_file(&self, f: &NewFile) -> Result<FileRow> {
        let id = new_id();
        self.conn.execute(
            "INSERT INTO files(file_id, session_id, partition_id, partition_index, source_rel_path,
                source_rel_path_raw_hex, destination_rel_path, media_kind, group_key, size_bytes,
                source_mtime_ns, stage_state, nas_state)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'DISCOVERED', 'NOT_READY')",
            params![
                id,
                f.session_id,
                f.partition_id,
                f.partition_index as i64,
                f.source_rel_path,
                f.source_rel_path_raw_hex,
                f.destination_rel_path,
                f.media_kind.as_str(),
                f.group_key,
                f.size_bytes as i64,
                f.source_mtime_ns
            ],
        )?;
        self.get_file(&id)?.ok_or(DbError::NotFound(id))
    }

    pub fn get_file(&self, file_id: &str) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {FILE_COLS} FROM files WHERE file_id = ?1"),
                [file_id],
                file_row,
            )
            .optional()?)
    }

    pub fn files_for_session(&self, session_id: &str) -> Result<Vec<FileRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_COLS} FROM files WHERE session_id = ?1 ORDER BY partition_index, source_rel_path"
        ))?;
        let rows = stmt
            .query_map([session_id], file_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 走査で見つかったファイルのサイズ/mtime を更新し、未完了状態に戻す（再挿入時の再走査用）。
    pub fn refresh_discovered_file(
        &self,
        file_id: &str,
        size: u64,
        mtime_ns: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET size_bytes = ?2, source_mtime_ns = ?3, stage_state = 'DISCOVERED',
                error_code = NULL, last_error = NULL, copy_attempts = 0
             WHERE file_id = ?1 AND stage_state NOT IN ('SSD_VERIFIED', 'SKIPPED_VERIFIED')",
            params![file_id, size as i64, mtime_ns],
        )?;
        Ok(())
    }

    pub fn set_file_stage(&self, file_id: &str, stage: StageState) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET stage_state = ?2 WHERE file_id = ?1",
            params![file_id, stage.as_str()],
        )?;
        Ok(())
    }

    /// 試行回数を 1 増やし、COPYING にする。新しい試行回数を返す。
    pub fn begin_copy_attempt(&self, file_id: &str) -> Result<u32> {
        self.conn.execute(
            "UPDATE files SET stage_state = 'COPYING', copy_attempts = copy_attempts + 1 WHERE file_id = ?1",
            [file_id],
        )?;
        let n: i64 = self.conn.query_row(
            "SELECT copy_attempts FROM files WHERE file_id = ?1",
            [file_id],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// 抜去・停止・容量待ちによる中断は試行回数に数えない。
    pub fn cancel_copy_attempt(&self, file_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET stage_state = 'DISCOVERED', copy_attempts = MAX(copy_attempts - 1, 0) WHERE file_id = ?1",
            [file_id],
        )?;
        Ok(())
    }

    pub fn set_file_error(
        &self,
        file_id: &str,
        stage: StageState,
        code: &str,
        message: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET stage_state = ?2, error_code = ?3, last_error = ?4 WHERE file_id = ?1",
            params![file_id, stage.as_str(), code, message],
        )?;
        Ok(())
    }

    /// SSD 検証成功（実体の確定・fsync 後に呼ぶ）。NAS 転送キューへ登録する。
    pub fn mark_file_ssd_verified(&self, file_id: &str, hash: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let now = now_utc();
        tx.execute(
            "UPDATE files SET stage_state = 'SSD_VERIFIED', blake3_hash = ?2, verified_ssd_at_utc = ?3,
                nas_state = 'QUEUED', error_code = NULL, last_error = NULL
             WHERE file_id = ?1",
            params![file_id, hash, now],
        )?;
        let session_id: String = tx.query_row(
            "SELECT session_id FROM files WHERE file_id = ?1",
            [file_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO transfers(transfer_id, file_id, session_id, state, attempts, created_at_utc, updated_at_utc)
             VALUES(?1, ?2, ?3, 'QUEUED', 0, ?4, ?4)
             ON CONFLICT(file_id) DO UPDATE SET state = 'QUEUED', attempts = 0, next_attempt_utc = NULL,
                last_error = NULL, updated_at_utc = excluded.updated_at_utc",
            params![new_id(), file_id, session_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 過去の検証済み実体があるためコピー不要（参照情報を保存）。
    pub fn mark_file_skipped(&self, file_id: &str, hash: &str, reference_json: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET stage_state = 'SKIPPED_VERIFIED', blake3_hash = ?2, reference_json = ?3,
                reference_verified = 1, nas_state = 'NOT_READY', verified_nas_at_utc = ?4
             WHERE file_id = ?1",
            params![file_id, hash, reference_json, now_utc()],
        )?;
        Ok(())
    }

    pub fn set_reference_verified(&self, file_id: &str, verified: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET reference_verified = ?2 WHERE file_id = ?1",
            params![file_id, verified as i64],
        )?;
        Ok(())
    }

    pub fn set_file_nas_state(&self, file_id: &str, state: NasFileState) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET nas_state = ?2 WHERE file_id = ?1",
            params![file_id, state.as_str()],
        )?;
        Ok(())
    }

    /// SSD の実体が失われていた場合など: 再取り込みが必要な状態に戻す。
    pub fn reset_file_to_discovered(&self, file_id: &str, reason: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE files SET stage_state = 'DISCOVERED', nas_state = 'NOT_READY', blake3_hash = NULL,
                verified_ssd_at_utc = NULL, verified_nas_at_utc = NULL, copy_attempts = 0,
                error_code = 'RESET', last_error = ?2
             WHERE file_id = ?1",
            params![file_id, reason],
        )?;
        tx.execute("DELETE FROM transfers WHERE file_id = ?1", [file_id])?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // transfers
    // ------------------------------------------------------------------

    pub fn next_due_transfer(&self, now: &str) -> Result<Option<TransferJob>> {
        Ok(self
            .conn
            .query_row(
                "SELECT t.transfer_id, t.file_id, t.session_id, s.folder_name, f.destination_rel_path,
                        f.blake3_hash, f.size_bytes, t.attempts
                 FROM transfers t
                 JOIN files f ON f.file_id = t.file_id
                 JOIN sessions s ON s.session_id = t.session_id
                 WHERE t.state IN ('QUEUED', 'RETRY_WAIT')
                   AND (t.next_attempt_utc IS NULL OR t.next_attempt_utc <= ?1)
                   AND f.stage_state = 'SSD_VERIFIED' AND f.blake3_hash IS NOT NULL
                 ORDER BY t.created_at_utc
                 LIMIT 1",
                [now],
                |r| {
                    Ok(TransferJob {
                        transfer_id: r.get(0)?,
                        file_id: r.get(1)?,
                        session_id: r.get(2)?,
                        folder_name: r.get(3)?,
                        destination_rel_path: r.get(4)?,
                        blake3_hash: r.get(5)?,
                        size_bytes: r.get::<_, i64>(6)? as u64,
                        attempts: r.get::<_, i64>(7)? as u32,
                    })
                },
            )
            .optional()?)
    }

    pub fn transfer_started(&self, job: &TransferJob, temp_rel: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE transfers SET state = 'UPLOADING', nas_temp_rel_path = ?2, attempts = attempts + 1,
                updated_at_utc = ?3 WHERE transfer_id = ?1",
            params![job.transfer_id, temp_rel, now_utc()],
        )?;
        tx.execute(
            "UPDATE files SET nas_state = 'UPLOADING' WHERE file_id = ?1",
            [&job.file_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn transfer_verifying(&self, job: &TransferJob) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET nas_state = 'VERIFYING' WHERE file_id = ?1",
            [&job.file_id],
        )?;
        Ok(())
    }

    /// NAS 側の検証成功。files・transfers・catalog を 1 トランザクションで更新する。
    pub fn transfer_verified(&self, job: &TransferJob, nas_uri: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let now = now_utc();
        tx.execute(
            "UPDATE transfers SET state = 'DONE', last_error = NULL, next_attempt_utc = NULL, updated_at_utc = ?2
             WHERE transfer_id = ?1",
            params![job.transfer_id, now],
        )?;
        tx.execute(
            "UPDATE files SET nas_state = 'NAS_VERIFIED', verified_nas_at_utc = ?2 WHERE file_id = ?1",
            params![job.file_id, now],
        )?;
        // 永続カタログ（ログとは独立）
        tx.execute(
            "INSERT INTO catalog(catalog_id, device_identity_hash, partition_fingerprint, source_rel_path,
                size_bytes, source_mtime_ns, blake3_hash, nas_uri, nas_session_folder, nas_dest_rel_path,
                nas_session_id, verified_at_utc)
             SELECT ?1, d.identity_hash, p.fingerprint, f.source_rel_path, f.size_bytes, f.source_mtime_ns,
                    f.blake3_hash, ?2, s.folder_name, f.destination_rel_path, s.session_id, ?3
             FROM files f
             JOIN sessions s ON s.session_id = f.session_id
             JOIN devices d ON d.device_id = s.device_id
             JOIN partitions p ON p.partition_id = f.partition_id
             WHERE f.file_id = ?4",
            params![new_id(), nas_uri, now, job.file_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn transfer_retry(
        &self,
        job: &TransferJob,
        error: &str,
        next_attempt_utc: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE transfers SET state = 'RETRY_WAIT', last_error = ?2, next_attempt_utc = ?3, updated_at_utc = ?4
             WHERE transfer_id = ?1",
            params![job.transfer_id, error, next_attempt_utc, now_utc()],
        )?;
        tx.execute(
            "UPDATE files SET nas_state = 'RETRY_WAIT' WHERE file_id = ?1",
            [&job.file_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// NAS 側で検証済みだったファイルの再送（最終確認での不一致など）。
    pub fn requeue_file_transfer(&self, file_id: &str, reason: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let now = now_utc();
        tx.execute(
            "UPDATE transfers SET state = 'QUEUED', next_attempt_utc = NULL, last_error = ?2, updated_at_utc = ?3
             WHERE file_id = ?1",
            params![file_id, reason, now],
        )?;
        tx.execute(
            "UPDATE files SET nas_state = 'QUEUED', verified_nas_at_utc = NULL WHERE file_id = ?1",
            [file_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 「今すぐ再試行」。
    pub fn retry_all_transfers_now(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE transfers SET next_attempt_utc = NULL, state = 'QUEUED' WHERE state IN ('RETRY_WAIT', 'FAILED')",
            [],
        )?)
    }

    pub fn pending_transfer_count(&self, session_id: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM transfers WHERE session_id = ?1 AND state != 'DONE'",
            [session_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    pub fn nas_queue_stats(&self) -> Result<NasQueueStats> {
        let (queued_files, queued_bytes, retry, failed, uploading, max_attempts): (i64, i64, i64, i64, i64, i64) =
            self.conn.query_row(
                "SELECT
                    COALESCE(SUM(CASE WHEN t.state IN ('QUEUED','RETRY_WAIT','UPLOADING') THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN t.state IN ('QUEUED','RETRY_WAIT','UPLOADING') THEN f.size_bytes ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN t.state = 'RETRY_WAIT' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN t.state = 'FAILED' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN t.state = 'UPLOADING' THEN 1 ELSE 0 END), 0),
                    COALESCE(MAX(CASE WHEN t.state != 'DONE' THEN t.attempts ELSE 0 END), 0)
                 FROM transfers t JOIN files f ON f.file_id = t.file_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )?;
        let (next, last_error): (Option<String>, Option<String>) = self
            .conn
            .query_row(
                "SELECT next_attempt_utc, last_error FROM transfers WHERE state = 'RETRY_WAIT'
                 ORDER BY next_attempt_utc LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));
        Ok(NasQueueStats {
            queued_files: queued_files as u64,
            queued_bytes: queued_bytes as u64,
            retry_wait_files: retry as u64,
            failed_files: failed as u64,
            uploading_files: uploading as u64,
            next_attempt_utc: next,
            max_attempts: max_attempts as u32,
            last_error,
        })
    }

    // ------------------------------------------------------------------
    // catalog
    // ------------------------------------------------------------------

    /// 同一デバイス・同一パーティション・同一パス・同一サイズのカタログ候補（最新順）。
    /// 呼び出し側は必ず元ファイルと NAS 実体のハッシュを確認してからスキップすること。
    pub fn catalog_candidates(
        &self,
        identity_hash: &str,
        fingerprint: &str,
        source_rel_path: &str,
        size: u64,
    ) -> Result<Vec<CatalogRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CATALOG_COLS} FROM catalog
             WHERE device_identity_hash = ?1 AND partition_fingerprint = ?2 AND source_rel_path = ?3
               AND size_bytes = ?4 AND availability_state = 'verified'
             ORDER BY verified_at_utc DESC LIMIT 5"
        ))?;
        let rows = stmt
            .query_map(
                params![identity_hash, fingerprint, source_rel_path, size as i64],
                catalog_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn mark_catalog_unavailable(&self, catalog_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE catalog SET availability_state = 'unavailable' WHERE catalog_id = ?1",
            [catalog_id],
        )?;
        Ok(())
    }

    pub fn catalog_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM catalog", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    // ------------------------------------------------------------------
    // events
    // ------------------------------------------------------------------

    pub fn log_event(
        &self,
        session_id: Option<&str>,
        level: &str,
        event_type: &str,
        message: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events(session_id, level, event_type, message, created_at_utc) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![session_id, level, event_type, message, now_utc()],
        )?;
        Ok(())
    }

    pub fn recent_events(&self, since_utc: &str, limit: u32) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_id, session_id, level, event_type, message, created_at_utc FROM events
             WHERE created_at_utc >= ?1 ORDER BY event_id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![since_utc, limit as i64], |r| {
                Ok(EventRow {
                    event_id: r.get(0)?,
                    session_id: r.get(1)?,
                    level: r.get(2)?,
                    event_type: r.get(3)?,
                    message: r.get(4)?,
                    created_at_utc: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 30 日より古いイベントを削除（catalog・sessions は保持）。
    pub fn purge_events_before(&self, cutoff_utc: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM events WHERE created_at_utc < ?1", [cutoff_utc])?)
    }

    // ------------------------------------------------------------------
    // recovery
    // ------------------------------------------------------------------

    /// デーモン起動時の不確定状態の整理（9.3）。
    /// - メディア処理中だったセッション → SOURCE_INTERRUPTED
    /// - COPYING/VERIFYING のファイル → DISCOVERED（.part は呼び出し側で破棄）
    /// - UPLOADING の転送 → QUEUED（NAS 上の一時ファイルは採用しない）
    pub fn recover_after_restart(&self) -> Result<RecoveryReport> {
        let mut report = RecoveryReport::default();
        let active: Vec<SessionRow> = self.sessions_in_states(
            &SessionState::ALL
                .iter()
                .copied()
                .filter(|s| s.is_media_active())
                .collect::<Vec<_>>(),
        )?;
        for s in active {
            self.transition_session(&s.session_id, SessionState::SourceInterrupted)?;
            self.set_media_state(&s.session_id, SessionState::SourceInterrupted.as_str())?;
            report.sessions_interrupted.push(s.session_id);
        }
        report.files_reset = self.conn.execute(
            "UPDATE files SET stage_state = 'DISCOVERED' WHERE stage_state IN ('COPYING', 'VERIFYING')",
            [],
        )? as u64;
        let tx = self.conn.unchecked_transaction()?;
        report.transfers_reset = tx.execute(
            "UPDATE transfers SET state = 'QUEUED', nas_temp_rel_path = NULL WHERE state = 'UPLOADING'",
            [],
        )? as u64;
        tx.execute(
            "UPDATE files SET nas_state = 'QUEUED' WHERE nas_state IN ('UPLOADING', 'VERIFYING')",
            [],
        )?;
        tx.commit()?;
        Ok(report)
    }
}
