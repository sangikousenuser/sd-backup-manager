//! NAS への接続（6.3 の採用方針）。
//!
//! - `gvfs`: ログインセッション内で `gio mount` により SMB 共有をユーザー権限でマウントし、
//!   GVfs の FUSE パス（`gio info` の `local path`）をファイルとして読み書きする。
//!   パスワードは Secret Service から取り出して `gio mount` の標準入力へ渡す（引数には載せない）。
//! - `mounted_path`: 管理者が用意した CIFS マウント（systemd mount 等）を使う。ルート FS 上の
//!   ただのディレクトリへ誤って書き込まないよう、ルートやステージングと別デバイスであることを確認する。
//!
//! どちらの場合も最後に保存先識別ファイル（target marker）を照合する。

use crate::state::Shared;
use backup_config::{Config, NasMode, parse_smb_url, secrets};
use backup_storage::capacity::device_of;
use backup_storage::nas::{NasError, NasRoot};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// 識別ファイルの照合まで行った接続済みルートを返す。
pub async fn connect(
    shared: &Shared,
    cfg: &Config,
    check_marker: bool,
) -> Result<NasRoot, NasError> {
    let root = resolve_root(shared, cfg, None).await?;
    let test_override = shared.overrides.nas_local_dir.is_some() && cfg.nas.target_id.is_empty();
    if check_marker && !test_override {
        if cfg.nas.target_id.is_empty() {
            return Err(NasError::MarkerMissing);
        }
        root.verify_marker(&cfg.nas.target_id)?;
    }
    Ok(root)
}

/// 接続先のルートを解決する（識別ファイルは見ない）。`password` 指定時は保存済みより優先。
pub async fn resolve_root(
    shared: &Shared,
    cfg: &Config,
    password: Option<String>,
) -> Result<NasRoot, NasError> {
    if let Some(dir) = &shared.overrides.nas_local_dir {
        if !dir.is_dir() {
            return Err(NasError::Offline(format!(
                "{} が見つかりません",
                dir.display()
            )));
        }
        return Ok(NasRoot::new(
            dir.clone(),
            format!("file://{}", dir.display()),
        ));
    }
    match cfg.nas.mode {
        NasMode::Disabled => Err(NasError::Offline("NAS が設定されていません".into())),
        NasMode::MountedPath => {
            let path = PathBuf::from(&cfg.nas.mounted_path);
            check_mounted_path(&path, &cfg.staging_path(), cfg.nas.require_mount_point)?;
            Ok(NasRoot::new(path, cfg.nas.display_target()))
        }
        NasMode::Gvfs => {
            let url =
                parse_smb_url(&cfg.nas.url).map_err(|e| NasError::InvalidPath(e.to_string()))?;
            let share_uri = url.share_uri();
            let local = match gio_local_path(&share_uri).await {
                Some(p) => p,
                None => {
                    let password = match password {
                        Some(p) => Some(p),
                        None => {
                            let account = cfg.nas.secret_account();
                            tokio::task::spawn_blocking(move || secrets::load_password(&account))
                                .await
                                .map_err(|e| NasError::Io(e.to_string()))?
                                .map_err(|e| NasError::Permission(e.to_string()))?
                        }
                    };
                    gio_mount(
                        &share_uri,
                        &cfg.nas.username,
                        &cfg.nas.domain,
                        password.as_deref(),
                    )
                    .await?;
                    gio_local_path(&share_uri).await.ok_or_else(|| {
                        NasError::Offline(
                            "GVfs のローカルパスを取得できません (gvfs-fuse が必要)".into(),
                        )
                    })?
                }
            };
            let root = if url.subpath.is_empty() {
                local
            } else {
                local.join(&url.subpath)
            };
            if !root.is_dir() {
                return Err(NasError::Offline(format!(
                    "共有内のフォルダが見つかりません: {}",
                    url.subpath
                )));
            }
            Ok(NasRoot::new(root, cfg.nas.url.clone()))
        }
    }
}

fn check_mounted_path(path: &Path, staging: &Path, require_mount: bool) -> Result<(), NasError> {
    if !path.is_absolute() {
        return Err(NasError::InvalidPath("絶対パスで指定してください".into()));
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| NasError::Offline(e.to_string()))?;
    if !meta.is_dir() {
        return Err(NasError::Offline(format!(
            "{} がディレクトリではありません",
            path.display()
        )));
    }
    if require_mount {
        let dev = device_of(path).map_err(NasError::from_io)?;
        let root_dev = device_of(Path::new("/")).map_err(NasError::from_io)?;
        let staging_dev = device_of(staging).ok();
        if dev == root_dev || Some(dev) == staging_dev {
            return Err(NasError::Offline(format!(
                "{} が NAS としてマウントされていません（ローカルディスクへの誤書き込みを防ぐため停止）",
                path.display()
            )));
        }
    }
    Ok(())
}

async fn gio_local_path(uri: &str) -> Option<PathBuf> {
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new("gio")
            .arg("info")
            .arg(uri)
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.trim().strip_prefix("local path:"))
        .map(|p| PathBuf::from(p.trim()))
        .filter(|p| p.is_dir())
}

/// `gio mount` に資格情報を標準入力で渡す（User / Domain / Password の順で尋ねられる）。
async fn gio_mount(
    uri: &str,
    username: &str,
    domain: &str,
    password: Option<&str>,
) -> Result<(), NasError> {
    let mut cmd = Command::new("gio");
    cmd.arg("mount");
    if password.is_none() && username.is_empty() {
        cmd.arg("--anonymous");
    }
    cmd.arg(uri)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| NasError::Offline(format!("gio を実行できません: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let mut input = String::new();
        input.push_str(username);
        input.push('\n');
        input.push_str(domain);
        input.push('\n');
        input.push_str(password.unwrap_or(""));
        input.push('\n');
        let _ = stdin.write_all(input.as_bytes()).await;
        drop(stdin);
    }
    let out = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .map_err(|_| NasError::Offline("NAS への接続がタイムアウトしました".into()))?
        .map_err(|e| NasError::Offline(e.to_string()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let lower = err.to_lowercase();
    if lower.contains("already mounted") {
        return Ok(());
    }
    // エラー文に秘密値が含まれないよう、そのまま返さず分類する
    if lower.contains("password")
        || lower.contains("permission")
        || lower.contains("logon")
        || lower.contains("denied")
    {
        Err(NasError::Permission(
            "NAS の認証に失敗しました（ユーザー名・パスワードを確認してください）".into(),
        ))
    } else {
        Err(NasError::Offline(format!(
            "NAS をマウントできません: {}",
            first_line(&err)
        )))
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}
