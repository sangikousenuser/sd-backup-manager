//! SD Backup Manager デーモン（`systemd --user` で常駐）。

pub mod file_actions;
pub mod ipc_server;
pub mod nas_backend;
pub mod nas_worker;
pub mod orchestrator;
pub mod recovery;
pub mod reset;
pub mod scanner;
pub mod snapshot;
pub mod state;
pub mod telemetry;

use anyhow::Context;
use backup_config::{AppPaths, Config, load_formats};
use backup_db::Db;
use backup_device::{DeviceBackend, WatchOptions};
use backup_storage::local::Staging;
use state::{Overrides, Shared};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub struct DaemonOptions {
    pub paths: Option<AppPaths>,
    /// 実機の代わりにディレクトリベースのメディアシミュレータを使う。
    pub simulate_media: Option<PathBuf>,
    /// 設定より優先するステージングフォルダ。
    pub staging_override: Option<PathBuf>,
    pub overrides: Overrides,
    pub watch: Option<WatchOptions>,
}

/// デーモンを起動し、`shutdown` がキャンセルされるまで動作する。
pub async fn run(opts: DaemonOptions, shutdown: CancellationToken) -> anyhow::Result<Arc<Shared>> {
    let shared = start(opts, shutdown.clone()).await?;
    shutdown.cancelled().await;
    stop(&shared).await;
    Ok(shared)
}

/// 起動のみ（テストから状態を参照するため分離）。
pub async fn start(
    opts: DaemonOptions,
    shutdown: CancellationToken,
) -> anyhow::Result<Arc<Shared>> {
    let paths = opts.paths.clone().unwrap_or_else(AppPaths::from_env);
    paths
        .ensure_dirs()
        .context("アプリ用フォルダを作成できません")?;

    let config_file = paths.config_file();
    let mut config = Config::load(&config_file).context("設定ファイルを読み込めません")?;
    if !config_file.exists() {
        config.save(&config_file)?;
    }
    if let Some(s) = &opts.staging_override {
        config.staging_directory = s.display().to_string();
    }
    let formats =
        load_formats(&paths).map_err(|e| anyhow::anyhow!("形式定義を読み込めません: {e}"))?;
    let staging =
        Staging::open(&config.staging_path()).context("ステージングフォルダを準備できません")?;
    let db = Db::open(&paths.db_file()).context("データベースを開けません")?;
    tracing::info!(version = backup_core::APP_VERSION, staging = %staging.root().display(), "starting daemon");

    let devices = match &opts.simulate_media {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            DeviceBackend::Simulated(backup_device::sim::SimBackend::new(dir.clone()))
        }
        None => native_backend(&paths, &staging).await?,
    };

    let shared = Shared::new(
        paths.clone(),
        config,
        formats,
        db,
        staging,
        devices,
        opts.overrides.clone(),
        shutdown.clone(),
    );
    recovery::recover(&shared)?;

    let listener = backup_ipc::transport::Listener::bind(&paths.socket_path())
        .await
        .context("IPC ソケットを作成できません")?;
    tokio::spawn(ipc_server::run(shared.clone(), listener));
    tokio::spawn(snapshot::run_publisher(shared.clone()));
    tokio::spawn(snapshot::run_maintenance(shared.clone()));
    tokio::spawn(nas_worker::run(shared.clone()));
    tokio::spawn(recovery::settle_unmount_failed(
        shared.clone(),
        Duration::from_secs(20),
    ));

    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let watcher = shared
        .devices
        .spawn_watcher(tx, opts.watch.clone().unwrap_or_default());
    let sd = shutdown.clone();
    tokio::spawn(async move {
        sd.cancelled().await;
        watcher.abort();
    });
    tokio::spawn(orchestrator::run_device_loop(shared.clone(), rx));
    Ok(shared)
}

/// 停止処理: 進行中のコピーを安全に中断して状態を保存する（ログアウト・シャットダウン時）。
pub async fn stop(shared: &Arc<Shared>) {
    for _ in 0..100 {
        let busy = shared.rt().slots.iter().flatten().any(|s| !s.task_finished);
        if !busy {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    backup_ipc::transport::cleanup(&shared.paths.socket_path());
    tracing::info!("daemon stopped");
}

#[cfg(target_os = "linux")]
async fn native_backend(paths: &AppPaths, staging: &Staging) -> anyhow::Result<DeviceBackend> {
    let policy = backup_device::ExclusionPolicy {
        protected_paths: vec![
            staging.root().to_path_buf(),
            paths.data_dir.clone(),
            backup_config::home_dir(),
            PathBuf::from("/"),
        ],
    };
    let b = backup_device::udisks::UDisksBackend::connect(policy)
        .await
        .context("UDisks2 に接続できません")?;
    Ok(DeviceBackend::UDisks(b))
}

#[cfg(windows)]
async fn native_backend(paths: &AppPaths, staging: &Staging) -> anyhow::Result<DeviceBackend> {
    let policy = backup_device::ExclusionPolicy {
        protected_paths: vec![
            staging.root().to_path_buf(),
            paths.data_dir.clone(),
            backup_config::home_dir(),
        ],
    };
    Ok(DeviceBackend::Windows(
        backup_device::windows::WinBackend::new(policy),
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
async fn native_backend(_: &AppPaths, _: &Staging) -> anyhow::Result<DeviceBackend> {
    anyhow::bail!(
        "この OS では実機メディアを扱えません。開発時は --simulate-media <DIR> を指定してください"
    )
}
