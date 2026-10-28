//! `sd-backup` — 状態確認・診断・復元・ログ書き出しのコマンドラインツール。

use anyhow::{Context, bail};
use backup_config::{AppPaths, Config};
use backup_core::identity::format_bytes;
use backup_core::manifest::Manifest;
use backup_ipc::{Command, Response, one_shot};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "sd-backup",
    version,
    about = "SD Backup Manager のコマンドラインツール"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 現在の状態（メディア・NAS・容量）を表示
    Status {
        /// JSON で出力
        #[arg(long)]
        json: bool,
    },
    /// セッション一覧
    Sessions {
        #[arg(long, default_value_t = 30)]
        limit: u32,
    },
    /// セッションのファイル一覧（既定は問題のあるファイルのみ）
    Files {
        session_id: String,
        #[arg(long)]
        all: bool,
    },
    /// 操作履歴・エラー（最大 30 日）
    Events {
        #[arg(long, default_value_t = 7)]
        days: u32,
    },
    /// NAS 転送を今すぐ再試行
    RetryNas,
    /// デーモンを正常停止（処理中のコピーは安全に中断され、次回起動時に再開）
    Shutdown,
    /// 処理を一時停止
    Pause,
    /// 処理を再開
    Resume,
    /// 診断情報（機密情報を伏せたもの）を書き出す
    ExportLogs { dest_dir: PathBuf },
    /// マニフェストから元のカード構成に復元する
    Restore {
        /// デーモン経由: セッション ID
        #[arg(long, conflicts_with = "manifest")]
        session: Option<String>,
        /// オフライン: manifest.json のパス（セッションフォルダ内）
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// オフライン: 過去セッション参照を解決するための NAS ルート
        #[arg(long)]
        nas_root: Option<PathBuf>,
        /// 復元先フォルダ
        #[arg(long)]
        to: PathBuf,
    },
    /// manifest.json の形式・パス・ハッシュ形式を検証（--verify-files で実体も照合）
    VerifyManifest {
        manifest: PathBuf,
        #[arg(long)]
        verify_files: bool,
    },
    /// 動作環境の診断（依存コマンド・サービス・容量・ソケット）
    Doctor,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

async fn daemon(command: Command) -> anyhow::Result<Response> {
    let socket = AppPaths::from_env().socket_path();
    one_shot(&socket, "cli", command)
        .await
        .with_context(|| format!("デーモンに接続できません（{}）。`systemctl --user status sd-backup-daemon` を確認してください", socket.display()))
}

fn print_ok(r: Response) {
    match r {
        Response::Ok { message } => println!("{message}"),
        other => println!("{other:?}"),
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Cmd::Status { json } => {
            let Response::State { snapshot } = daemon(Command::GetState).await? else {
                bail!("unexpected response")
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
                return Ok(());
            }
            println!(
                "デーモン {}{}",
                snapshot.daemon_version,
                if snapshot.paused {
                    "（一時停止中）"
                } else {
                    ""
                }
            );
            println!(
                "SSD 空き: {}（最低確保 {}）",
                snapshot
                    .ssd
                    .free_bytes
                    .map(format_bytes)
                    .unwrap_or_else(|| "取得不可".into()),
                format_bytes(snapshot.ssd.reserve_bytes)
            );
            for s in &snapshot.slots {
                print!(
                    "メディア {}: {} {}",
                    s.slot_index + 1,
                    s.phase.symbol(),
                    s.message
                );
                if let Some(name) = &s.media_name {
                    print!(
                        " [{name}] {}/{} {}/{} 件",
                        format_bytes(s.bytes_done),
                        format_bytes(s.bytes_total),
                        s.files_done,
                        s.files_total
                    );
                }
                if s.files_failed > 0 {
                    print!(" 失敗 {}", s.files_failed);
                }
                println!(
                    "{}",
                    if s.removable {
                        "  → 取り外し可能"
                    } else {
                        ""
                    }
                );
            }
            for w in &snapshot.waiting {
                println!("待機: {} ({})", w.media_name, w.reason);
            }
            let n = &snapshot.nas;
            println!(
                "NAS: {}（{}）空き {} / 残り {} 件 {} / 再試行待ち {} 件",
                n.status_text,
                n.target,
                n.free_bytes
                    .map(format_bytes)
                    .unwrap_or_else(|| "取得不可".into()),
                n.queued_files,
                format_bytes(n.queued_bytes),
                n.retry_wait_files
            );
            if let Some(e) = &n.last_error {
                println!("NAS 最終エラー: {e}");
            }
            for w in &snapshot.warnings {
                println!("警告: {w}");
            }
        }
        Cmd::Sessions { limit } => {
            let Response::Sessions { sessions } = daemon(Command::ListSessions { limit }).await?
            else {
                bail!("unexpected response")
            };
            for s in sessions {
                println!(
                    "{}  {}  {:<18} files {}/{} nas {} failed {}{}  {}",
                    s.started_at_utc,
                    s.folder_name,
                    s.state,
                    s.completed_files,
                    s.total_files,
                    s.nas_verified_files,
                    s.failed_files,
                    if s.staging_deleted {
                        " (SSD 削除済)"
                    } else {
                        ""
                    },
                    s.session_id
                );
            }
        }
        Cmd::Files { session_id, all } => {
            let Response::Files { files, .. } = daemon(Command::GetFiles {
                session_id,
                only_problems: !all,
                offset: 0,
                limit: 5000,
            })
            .await?
            else {
                bail!("unexpected response")
            };
            for f in files {
                println!(
                    "{:<16} {:<13} {:>10}  {} -> {}{}",
                    f.stage_state,
                    f.nas_state,
                    format_bytes(f.size_bytes),
                    f.source_rel_path,
                    f.destination_rel_path,
                    f.error.map(|e| format!("  ({e})")).unwrap_or_default()
                );
            }
        }
        Cmd::Events { days } => {
            let Response::Events { events } =
                daemon(Command::ListEvents { days, limit: 1000 }).await?
            else {
                bail!("unexpected response")
            };
            for e in events.iter().rev() {
                println!(
                    "{}  {:<5} {:<22} {}",
                    e.created_at_utc, e.level, e.event_type, e.message
                );
            }
        }
        Cmd::RetryNas => print_ok(daemon(Command::RetryNas).await?),
        Cmd::Shutdown => print_ok(daemon(Command::Shutdown).await?),
        Cmd::Pause => print_ok(daemon(Command::Pause).await?),
        Cmd::Resume => print_ok(daemon(Command::Resume).await?),
        Cmd::ExportLogs { dest_dir } => {
            let dest = std::fs::canonicalize(&dest_dir).context("出力先フォルダがありません")?;
            print_ok(
                daemon(Command::ExportLogs {
                    dest_dir: dest.display().to_string(),
                })
                .await?,
            )
        }
        Cmd::Restore {
            session,
            manifest,
            nas_root,
            to,
        } => {
            std::fs::create_dir_all(&to)?;
            let to = std::fs::canonicalize(&to)?;
            match (session, manifest) {
                (Some(session_id), None) => print_ok(
                    daemon(Command::RestoreLayout {
                        session_id,
                        dest_dir: to.display().to_string(),
                    })
                    .await?,
                ),
                (None, Some(manifest)) => {
                    let bytes = std::fs::read(&manifest).context("マニフェストを読み込めません")?;
                    let m = Manifest::parse_and_validate(&bytes)?;
                    let session_root = manifest
                        .parent()
                        .context("マニフェストの親フォルダがありません")?;
                    let r = backup_storage::restore::restore_layout(
                        &m,
                        session_root,
                        nas_root.as_deref(),
                        &to,
                    )?;
                    println!(
                        "復元 {} 件（既存 {} 件、参照 {} 件）→ {}",
                        r.restored,
                        r.already_present,
                        r.from_reference,
                        to.display()
                    );
                    for (p, e) in &r.failed {
                        eprintln!("失敗: {p}: {e}");
                    }
                    if !r.failed.is_empty() {
                        bail!("{} 件の復元に失敗しました", r.failed.len());
                    }
                }
                _ => bail!("--session または --manifest のどちらかを指定してください"),
            }
        }
        Cmd::VerifyManifest {
            manifest,
            verify_files,
        } => verify_manifest(&manifest, verify_files)?,
        Cmd::Doctor => doctor().await,
    }
    Ok(())
}

fn verify_manifest(path: &Path, verify_files: bool) -> anyhow::Result<()> {
    let m = Manifest::parse_and_validate(&std::fs::read(path)?)?;
    println!(
        "マニフェストは有効です: {} ファイル / パーティション {} / セッション {}",
        m.files.len(),
        m.partitions.len(),
        m.folder_name
    );
    if !verify_files {
        return Ok(());
    }
    let root = path.parent().context("no parent")?;
    let mut bad = 0;
    for f in m
        .files
        .iter()
        .filter(|f| f.status == backup_core::manifest::ManifestFileStatus::Copied)
    {
        let p = root.join(&f.dest_rel_path);
        match backup_storage::hash::hash_file(&p, &mut backup_storage::no_progress()) {
            Ok(h) if Some(&h.hash) == f.blake3.as_ref() => {}
            Ok(_) => {
                bad += 1;
                println!("不一致: {}", f.dest_rel_path);
            }
            Err(e) => {
                bad += 1;
                println!("読めません: {} ({e})", f.dest_rel_path);
            }
        }
    }
    if bad > 0 {
        bail!("{bad} 件のファイルが一致しません");
    }
    println!("全ファイルのハッシュが一致しました");
    Ok(())
}

fn check(ok: bool, label: &str, detail: impl AsRef<str>) {
    println!(
        "{} {label}: {}",
        if ok { "✔" } else { "✖" },
        detail.as_ref()
    );
}

#[cfg(not(windows))]
fn has_command(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

async fn doctor() {
    let paths = AppPaths::from_env();
    env_checks();

    match Config::load(&paths.config_file()) {
        Ok(cfg) => {
            check(true, "設定", paths.config_file().display().to_string());
            let staging = cfg.staging_path();
            match backup_storage::capacity::fs_space(&staging) {
                Ok(s) => check(
                    s.free_bytes > cfg.reserve_free_bytes,
                    "SSD 空き容量",
                    format!(
                        "{} / {}（最低確保 {}）",
                        format_bytes(s.free_bytes),
                        format_bytes(s.total_bytes),
                        format_bytes(cfg.reserve_free_bytes)
                    ),
                ),
                Err(e) => check(false, "SSD 空き容量", format!("{}: {e}", staging.display())),
            }
            let symlink = std::fs::symlink_metadata(&staging)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            check(
                !symlink,
                "ステージングフォルダ",
                if symlink {
                    "シンボリックリンクは使用できません".to_string()
                } else {
                    staging.display().to_string()
                },
            );
            check(
                cfg.nas.mode != backup_config::NasMode::Disabled,
                "NAS 設定",
                cfg.nas.display_target(),
            );
            check(
                !cfg.nas.target_id.is_empty() || cfg.nas.mode == backup_config::NasMode::Disabled,
                "NAS 保存先の初期化",
                if cfg.nas.target_id.is_empty() {
                    "未初期化"
                } else {
                    "済み"
                },
            );
        }
        Err(e) => check(false, "設定", e.to_string()),
    }
    match daemon(Command::GetState).await {
        Ok(_) => check(true, "デーモン", paths.socket_path().display().to_string()),
        Err(e) => check(false, "デーモン", e.to_string()),
    }
    autostart_checks();
}

/// OS 固有の依存関係（Linux/macOS）。
#[cfg(not(windows))]
fn env_checks() {
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        let pretty = text
            .lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .unwrap_or("?")
            .trim_matches('"');
        let ok = text.contains("VERSION_ID=\"26.04\"");
        check(
            ok,
            "OS",
            format!(
                "{pretty}{}",
                if ok {
                    ""
                } else {
                    "（Ubuntu 26.04 以外は実機未検証）"
                }
            ),
        );
    } else {
        check(
            false,
            "OS",
            "Linux ではありません（開発用のシミュレータのみ動作）",
        );
    }
    for (cmd, why) in [
        ("udisksctl", "udisks2（メディアのマウント/アンマウント）"),
        ("gio", "GVfs（SMB 接続）"),
        ("systemctl", "systemd ユーザーサービス"),
        ("fc-match", "日本語フォント検出（fontconfig）"),
    ] {
        check(has_command(cmd), cmd, why);
    }
    // GUI は paplay → pw-play → aplay の順に試す（Ubuntu 26.04 の既定は PipeWire）
    let player = ["paplay", "pw-play", "aplay"]
        .into_iter()
        .find(|c| has_command(c));
    check(
        player.is_some(),
        "通知音",
        match player {
            Some(p) => format!("{p} で再生"),
            None => "再生コマンドがありません（pipewire-bin または pulseaudio-utils）".into(),
        },
    );
    let gvfs_fuse = std::path::Path::new("/usr/libexec/gvfsd-fuse").exists();
    check(
        gvfs_fuse,
        "gvfs-fuse",
        if gvfs_fuse {
            "あり"
        } else {
            "なし（GVfs の SMB をファイルとして扱うのに必要）"
        },
    );
}

#[cfg(not(windows))]
fn autostart_checks() {
    if has_command("systemctl") {
        let out = std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", "sd-backup-daemon.service"])
            .output();
        let enabled = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
        check(
            enabled,
            "自動起動（systemd --user）",
            if enabled { "有効" } else { "無効" },
        );
    }
    // .deb 版は /etc/xdg/autostart、install.sh 版は ~/.config/autostart に登録する
    let autostart = [
        backup_config::home_dir().join(".config/autostart/sd-backup-gui.desktop"),
        std::path::PathBuf::from("/etc/xdg/autostart/sd-backup-gui.desktop"),
    ]
    .into_iter()
    .find(|p| p.exists());
    check(
        autostart.is_some(),
        "GUI 自動起動",
        autostart
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "登録されていません".into()),
    );
}

/// OS 固有の依存関係（Windows）。
#[cfg(windows)]
fn env_checks() {
    check(true, "OS", format!("Windows ({})", std::env::consts::ARCH));
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let font = ["meiryo.ttc", "YuGothM.ttc", "msgothic.ttc"]
        .iter()
        .map(|f| std::path::Path::new(&windir).join("Fonts").join(f))
        .find(|p| p.exists());
    check(
        font.is_some(),
        "日本語フォント",
        font.map(|p| p.display().to_string())
            .unwrap_or_else(|| "見つかりません（設定 → 言語で日本語の補助フォントを追加）".into()),
    );
    let creds = backup_config::secrets::load_password("doctor-probe").is_ok();
    check(
        creds,
        "資格情報マネージャー",
        if creds {
            "利用可能"
        } else {
            "利用できません"
        },
    );
}

#[cfg(windows)]
fn autostart_checks() {
    let task = std::process::Command::new("schtasks")
        .args(["/Query", "/TN", WINDOWS_TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    check(
        task,
        "自動起動（タスク スケジューラ）",
        if task { "登録済み" } else { "未登録" },
    );
    let run = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            WINDOWS_GUI_RUN_VALUE,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    check(run, "GUI 自動起動", if run { "登録済み" } else { "未登録" });
}

/// インストーラ（packaging/windows）と同じ名前。
#[cfg(windows)]
const WINDOWS_TASK_NAME: &str = "SD Backup Manager Daemon";
#[cfg(windows)]
const WINDOWS_GUI_RUN_VALUE: &str = "SDBackupManagerGUI";
