//! ログ（journal 向け stderr + 日次ローテーションのファイル）と診断エクスポート（FR-035）。

use crate::state::Shared;
use backup_config::secrets::redact;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub fn init_logging(
    log_dir: &Path,
    verbose: bool,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_from_env("SDBM_LOG")
        .unwrap_or_else(|_| EnvFilter::new(if verbose { "debug" } else { "info,zbus=warn" }));
    use std::io::IsTerminal;
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal());
    match std::fs::create_dir_all(log_dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(log_dir, "daemon.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file_layer)
                .try_init();
            Some(guard)
        }
        Err(_) => {
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .try_init();
            None
        }
    }
}

pub fn purge_old_logs(log_dir: &Path, days: u64) {
    let Ok(rd) = std::fs::read_dir(log_dir) else {
        return;
    };
    let cutoff = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("daemon.log") {
            continue;
        }
        if let Ok(m) = e.metadata()
            && m.is_file()
            && m.modified().map(|t| t < cutoff).unwrap_or(false)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// 一括削除用。アプリ自身のログだけを空にする（動作中のログは以後も記録される）。
pub fn clear_logs(log_dir: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(log_dir)?;
    if backup_storage::platform::is_link_like(&meta) || !meta.is_dir() {
        return Err(std::io::Error::other(
            "ログフォルダが通常のディレクトリではありません",
        ));
    }
    for entry in std::fs::read_dir(log_dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("daemon.log")
        {
            backup_storage::platform::truncate_owned_regular(&entry.path())?;
        }
    }
    Ok(())
}

/// 機密情報を除いた診断情報を `dest_dir/sdbm-diagnostics-<時刻>/` に書き出す。
pub fn export_diagnostics(shared: &Shared, dest_dir: &Path) -> anyhow::Result<PathBuf> {
    if !dest_dir.is_absolute() || !dest_dir.is_dir() {
        anyhow::bail!("出力先フォルダが存在しません: {}", dest_dir.display());
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let out = dest_dir.join(format!("sdbm-diagnostics-{stamp}"));
    std::fs::create_dir(&out)?;

    let since = backup_db::to_db_time(chrono::Utc::now() - chrono::Duration::days(30));
    let (events, sessions, catalog_count, schema) = {
        let db = shared.db();
        (
            db.recent_events(&since, 100_000)?,
            db.list_sessions(1000)?,
            db.catalog_count()?,
            db.schema_version()?,
        )
    };
    let mut ev = String::new();
    for e in events.iter().rev() {
        ev.push_str(&serde_json::to_string(e)?);
        ev.push('\n');
    }
    std::fs::write(out.join("events.jsonl"), redact(&ev))?;
    std::fs::write(
        out.join("sessions.json"),
        redact(&serde_json::to_string_pretty(&sessions)?),
    )?;

    let cfg = shared.config();
    let mut summary = String::new();
    summary.push_str(&format!("app_version = {}\n", backup_core::APP_VERSION));
    summary.push_str(&format!("db_schema = {schema}\n"));
    summary.push_str(&format!("catalog_entries = {catalog_count}\n"));
    summary.push_str(&format!("device_backend = {}\n", shared.devices.name()));
    summary.push_str(&format!("staging = {}\n", shared.staging.root().display()));
    summary.push_str(&format!(
        "os = {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        for l in text
            .lines()
            .filter(|l| l.starts_with("PRETTY_NAME") || l.starts_with("VERSION_ID"))
        {
            summary.push_str(l);
            summary.push('\n');
        }
    }
    summary.push_str("\n[config]\n");
    summary.push_str(&toml::to_string_pretty(&cfg).unwrap_or_default());
    std::fs::write(out.join("summary.txt"), redact(&summary))?;

    if let Ok(rd) = std::fs::read_dir(&shared.paths.log_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("daemon.log")
                && let Ok(text) = std::fs::read_to_string(e.path())
            {
                std::fs::write(out.join(&name), redact(&text))?;
            }
        }
    }
    Ok(out)
}
