//! SQLite 永続化層（8 章）。
//!
//! - WAL + `synchronous=FULL`、外部キー有効
//! - セッション状態は `SessionState::can_transition_to` で検証してから更新
//! - 実体ファイルの確定後に DB を更新する（呼び出し側の責務）。DB 更新に失敗しても
//!   再走査・再検証で回復できるよう、DB は「成功の主張」だけを保持する。

mod models;
mod queries;

pub use models::*;

use rusqlite::Connection;
use std::path::Path;
use thiserror::Error;

const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../../../migrations/0001_init.sql"))];

#[derive(Debug, Error)]
pub enum DbError {
    #[error("SQLite エラー: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("許可されていない状態遷移です: {from} → {to}")]
    InvalidTransition { from: String, to: String },
    #[error("見つかりません: {0}")]
    NotFound(String),
    #[error("データが不正です: {0}")]
    Corrupt(String),
    #[error("このデータベースは新しいバージョンのアプリで作成されています (schema {0})")]
    TooNew(i64),
}

pub type Result<T> = std::result::Result<T, DbError>;

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // in-memory DB では WAL にならないが問題ない
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let mut db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS app_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )?;
        let current = self.schema_version()?;
        let latest = MIGRATIONS.last().map(|m| m.0).unwrap_or(0);
        if current > latest {
            return Err(DbError::TooNew(current));
        }
        for (version, sql) in MIGRATIONS {
            if *version <= current {
                continue;
            }
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO app_meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [version.to_string()],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM app_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    /// 整合性チェック（`PRAGMA quick_check`）。
    pub fn quick_check(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))?)
    }

    /// オンラインバックアップ（アップグレード前の退避用）。
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
        Ok(())
    }
}

/// DB に保存する UTC 時刻（ミリ秒、辞書順＝時刻順）。
pub fn now_utc() -> String {
    to_db_time(chrono::Utc::now())
}

pub fn to_db_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn parse_db_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests;
