// リリースビルドの Windows ではコンソールウィンドウを出さない（ログはファイルへ）
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use clap::Parser;
use sd_backup_daemon::state::Overrides;
use sd_backup_daemon::{DaemonOptions, run, telemetry};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// SD Backup Manager バックグラウンドサービス
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// 開発用: ディレクトリをメディアに見立てて動作する（DIR/<name>/p1/... が 1 台）
    #[arg(long, value_name = "DIR")]
    simulate_media: Option<PathBuf>,
    /// 開発用: NAS の代わりにこのディレクトリへ転送する
    #[arg(long, value_name = "DIR")]
    dev_nas_dir: Option<PathBuf>,
    /// 開発用: 設定・DB・ソケット・ステージングをすべてこの配下に置く
    #[arg(long, value_name = "DIR")]
    base_dir: Option<PathBuf>,
    /// 詳細ログ
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let paths = match &args.base_dir {
        Some(b) => backup_config::AppPaths::under(b),
        None => backup_config::AppPaths::from_env(),
    };
    let _guard = telemetry::init_logging(&paths.log_dir, args.verbose);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let shutdown = CancellationToken::new();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            wait_for_stop_signal().await;
            sd.cancel();
        });
        let opts = DaemonOptions {
            staging_override: args.base_dir.as_ref().map(|b| b.join("staging")),
            paths: Some(paths),
            simulate_media: args.simulate_media,
            overrides: Overrides {
                nas_local_dir: args.dev_nas_dir,
                ..Default::default()
            },
            watch: None,
        };
        match run(opts, shutdown).await {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::error!("{e:#}");
                Err(e)
            }
        }
    })
}

/// 停止要求を待つ。Unix: SIGTERM/SIGINT/SIGHUP（systemd の停止・ログアウト）。
/// Windows: Ctrl+C / Ctrl+Break / コンソールを閉じる / ログオフ / シャットダウン。
/// コンソールを持たない常駐プロセスは強制終了されることがあるが、その場合も起動時の復旧で
/// 安全にやり直せる（設計書 9.3）。インストーラは `sd-backup shutdown` で正常停止を要求する。
async fn wait_for_stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received"),
            _ = int.recv() => tracing::info!("SIGINT received"),
            _ = hup.recv() => tracing::info!("SIGHUP received"),
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
        let mut c = ctrl_c().expect("ctrl_c handler");
        let mut b = ctrl_break().expect("ctrl_break handler");
        let mut close = ctrl_close().expect("ctrl_close handler");
        let mut logoff = ctrl_logoff().expect("ctrl_logoff handler");
        let mut shutdown = ctrl_shutdown().expect("ctrl_shutdown handler");
        tokio::select! {
            _ = c.recv() => tracing::info!("Ctrl+C received"),
            _ = b.recv() => tracing::info!("Ctrl+Break received"),
            _ = close.recv() => tracing::info!("console close received"),
            _ = logoff.recv() => tracing::info!("logoff received"),
            _ = shutdown.recv() => tracing::info!("system shutdown received"),
        }
    }
}
