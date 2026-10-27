//! NAS への接続（6.3 の採用方針）。
//!
//! - `gvfs`（設定値の名前は互換のため維持。意味は「SMB 共有に直接接続」）:
//!   - Linux: ログインセッション内で `gio mount` により SMB 共有をユーザー権限でマウントし、
//!     GVfs の FUSE パス（`gio info` の `local path`）をファイルとして読み書きする。
//!     パスワードは Secret Service から取り出して `gio mount` の標準入力へ渡す（引数には載せない）。
//!   - Windows: `smb://server/share/sub` を UNC パス `\\server\share\sub` に変換し、
//!     `WNetAddConnection2W` で資格情報マネージャーのパスワードを使って接続する（ドライブレターは割り当てない）。
//! - `mounted_path`: 管理者が用意したマウント（Linux: CIFS、Windows: 割り当て済みドライブや UNC）を使う。
//!   ローカルディスクのただのディレクトリへ誤って書き込まないよう、OS ドライブやステージングと
//!   別ボリュームであることを確認する。
//!
//! どちらの場合も最後に保存先識別ファイル（target marker）を照合する。

use crate::state::Shared;
use backup_config::{Config, NasMode, parse_smb_url, secrets};
use backup_storage::capacity::device_of;
use backup_storage::nas::{NasError, NasRoot};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
#[cfg(unix)]
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
            let root = smb_root(cfg, &url, password).await?;
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

async fn stored_password(
    cfg: &Config,
    password: Option<String>,
) -> Result<Option<String>, NasError> {
    if password.is_some() {
        return Ok(password);
    }
    let account = cfg.nas.secret_account();
    tokio::task::spawn_blocking(move || secrets::load_password(&account))
        .await
        .map_err(|e| NasError::Io(e.to_string()))?
        .map_err(|e| NasError::Permission(e.to_string()))
}

#[cfg(unix)]
async fn smb_root(
    cfg: &Config,
    url: &backup_config::SmbUrl,
    password: Option<String>,
) -> Result<PathBuf, NasError> {
    let share_uri = url.share_uri();
    let local = match gio_local_path(&share_uri).await {
        Some(p) => p,
        None => {
            let password = stored_password(cfg, password).await?;
            gio_mount(
                &share_uri,
                &cfg.nas.username,
                &cfg.nas.domain,
                password.as_deref(),
            )
            .await?;
            gio_local_path(&share_uri).await.ok_or_else(|| {
                NasError::Offline("GVfs のローカルパスを取得できません (gvfs-fuse が必要)".into())
            })?
        }
    };
    Ok(if url.subpath.is_empty() {
        local
    } else {
        local.join(&url.subpath)
    })
}

#[cfg(windows)]
async fn smb_root(
    cfg: &Config,
    url: &backup_config::SmbUrl,
    password: Option<String>,
) -> Result<PathBuf, NasError> {
    let target = PathBuf::from(url.unc_path());
    // 既に接続済み（同じ資格情報のセッションがある）ならそのまま使う
    if password.is_none() && target.is_dir() {
        return Ok(target);
    }
    let password = stored_password(cfg, password).await?;
    let user = match (cfg.nas.domain.is_empty(), cfg.nas.username.is_empty()) {
        (_, true) => None,
        (true, false) => Some(cfg.nas.username.clone()),
        (false, false) => Some(format!("{}\\{}", cfg.nas.domain, cfg.nas.username)),
    };
    let share = url.unc_share();
    tokio::task::spawn_blocking(move || wnet_connect(&share, user.as_deref(), password.as_deref()))
        .await
        .map_err(|e| NasError::Io(e.to_string()))??;
    Ok(target)
}

/// `WNetAddConnection2W` で UNC 共有に接続する（ドライブレターなし、一時接続）。
#[cfg(windows)]
fn wnet_connect(share: &str, user: Option<&str>, password: Option<&str>) -> Result<(), NasError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::NetworkManagement::WNet::{
        CONNECT_TEMPORARY, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W,
    };
    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let mut remote = wide(share);
    let user_w = user.map(wide);
    let mut pass_w = password.map(wide);
    let res = NETRESOURCEW {
        dwScope: 0,
        dwType: RESOURCETYPE_DISK,
        dwDisplayType: 0,
        dwUsage: 0,
        lpLocalName: std::ptr::null_mut(),
        lpRemoteName: remote.as_mut_ptr(),
        lpComment: std::ptr::null_mut(),
        lpProvider: std::ptr::null_mut(),
    };
    let code = unsafe {
        WNetAddConnection2W(
            &res,
            pass_w.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            user_w.as_ref().map_or(std::ptr::null(), |u| u.as_ptr()),
            CONNECT_TEMPORARY,
        )
    };
    // パスワードのコピーをメモリから消す
    if let Some(p) = pass_w.as_mut() {
        p.iter_mut().for_each(|c| *c = 0);
    }
    match code {
        // NO_ERROR / ERROR_SESSION_CREDENTIAL_CONFLICT（既に別の資格情報で接続済み）
        0 | 1219 => Ok(()),
        // ERROR_ACCESS_DENIED / ERROR_INVALID_PASSWORD / ERROR_LOGON_FAILURE / ERROR_ACCOUNT_*
        5 | 86 | 1326 | 1327 | 1330 | 1331 | 1909 => Err(NasError::Permission(
            "NAS の認証に失敗しました（ユーザー名・パスワードを確認してください）".into(),
        )),
        other => Err(NasError::Offline(format!(
            "NAS に接続できません（Windows エラー {other}: {}）",
            std::io::Error::from_raw_os_error(other as i32)
        ))),
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
        let root_dev = device_of(&os_root()).map_err(NasError::from_io)?;
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

/// OS のルート（Unix: `/`、Windows: `%SystemDrive%\\`）。
fn os_root() -> PathBuf {
    #[cfg(windows)]
    {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        PathBuf::from(format!("{drive}\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/")
    }
}

#[cfg(unix)]
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
#[cfg(unix)]
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

#[cfg(unix)]
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}
