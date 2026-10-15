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
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
            tokio::select! {
                _ = term.recv() => tracing::info!("SIGTERM received"),
                _ = int.recv() => tracing::info!("SIGINT received"),
                _ = hup.recv() => tracing::info!("SIGHUP received"),
            }
            sd.cancel();
        });
        let opts = DaemonOptions {
            staging_override: args.base_dir.as_ref().map(|b| b.join("staging")),
            paths: Some(paths),
            simulate_media: args.simulate_media,
            overrides: Overrides {
                nas_local_dir: args.dev_nas_dir,
                fake_ssd_free_bytes: None,
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
