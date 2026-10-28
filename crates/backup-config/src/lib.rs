//! 設定（7 章）。秘密値は TOML に保存せず、Secret Service（libsecret 互換）に置く。

pub mod secrets;

use backup_core::classifier::FormatDefinitions;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const APP_DIR_NAME: &str = "sd-backup-manager";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("設定ファイルを読み込めません: {0}")]
    Io(#[from] io::Error),
    #[error("設定ファイルの形式が不正です: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("設定を書き込めません: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("設定値が不正です: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------------
// パス
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl AppPaths {
    /// Linux/macOS は XDG Base Directory、Windows は %APPDATA% / %LOCALAPPDATA% に従う。
    /// 開発用に `SDBM_BASE_DIR` があればその配下にまとめる。
    pub fn from_env() -> Self {
        if let Some(base) = env_path("SDBM_BASE_DIR") {
            return Self::under(&base);
        }
        #[cfg(windows)]
        {
            let home = home_dir();
            let roaming = env_path("APPDATA").unwrap_or_else(|| home.join("AppData\\Roaming"));
            let local = env_path("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData\\Local"));
            let data_dir = local.join(APP_DIR_NAME);
            Self {
                config_dir: roaming.join(APP_DIR_NAME),
                log_dir: data_dir.join("logs"),
                runtime_dir: data_dir.join("run"),
                data_dir,
            }
        }
        #[cfg(not(windows))]
        {
            let home = home_dir();
            let config_home = env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
            let data_home = env_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
            let runtime = env_path("XDG_RUNTIME_DIR")
                .map(|p| p.join(APP_DIR_NAME))
                .unwrap_or_else(|| {
                    std::env::temp_dir().join(format!("{APP_DIR_NAME}-{}", current_uid()))
                });
            let data_dir = data_home.join(APP_DIR_NAME);
            Self {
                config_dir: config_home.join(APP_DIR_NAME),
                log_dir: data_dir.join("logs"),
                data_dir,
                runtime_dir: runtime,
            }
        }
    }

    /// テスト・シミュレーション用: すべてを `base` 配下に置く。
    pub fn under(base: &Path) -> Self {
        Self {
            config_dir: base.join("config"),
            data_dir: base.join("data"),
            log_dir: base.join("data/logs"),
            runtime_dir: base.join("run"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn formats_file(&self) -> PathBuf {
        self.config_dir.join("formats.toml")
    }

    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("state.sqlite")
    }

    /// IPC の接続先。Unix: `<runtime_dir>/daemon.sock`。
    /// Windows: 名前付きパイプ `\\.\pipe\sd-backup-manager-<runtime_dir のハッシュ>`
    /// （同じ設定のデーモン・GUI・CLI は同じ名前になり、テスト環境同士は衝突しない）。
    pub fn socket_path(&self) -> PathBuf {
        #[cfg(windows)]
        {
            let key = self.runtime_dir.to_string_lossy().to_lowercase();
            PathBuf::from(format!(
                r"\\.\pipe\{APP_DIR_NAME}-{:016x}",
                fnv1a(key.as_bytes())
            ))
        }
        #[cfg(not(windows))]
        {
            self.runtime_dir.join("daemon.sock")
        }
    }

    pub fn ensure_dirs(&self) -> io::Result<()> {
        for d in [
            &self.config_dir,
            &self.data_dir,
            &self.log_dir,
            &self.runtime_dir,
        ] {
            let mut b = fs::DirBuilder::new();
            b.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(d)?;
        }
        // ソケットディレクトリは必ず 0700（6.4）。Windows はユーザープロファイルの ACL に従う
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.runtime_dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }))
}

/// Windows の内部パス表現をエクスプローラーで使える表示にする。
pub fn display_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        path.trim_start_matches(r"\\?\").to_string()
    }
}

#[cfg(not(windows))]
fn current_uid() -> u32 {
    // std だけで uid を得る（/proc/self に依存しない）
    use std::os::unix::fs::MetadataExt;
    fs::metadata(home_dir()).map(|m| m.uid()).unwrap_or(0)
}

/// `~/` を展開する。
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        home_dir()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(p)
    }
}

// ---------------------------------------------------------------------------
// 設定本体
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NasMode {
    /// 未設定（転送しない。SSD に保持し続ける）
    #[default]
    Disabled,
    /// GIO/GVfs の SMB バックエンドでユーザー権限接続（第一候補）
    Gvfs,
    /// 管理者が設定済みの CIFS マウントポイントを使う（systemd mount 等）
    MountedPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NasConfig {
    pub mode: NasMode,
    /// `smb://server/share/subpath/`（資格情報を含めない）
    pub url: String,
    pub username: String,
    pub domain: String,
    /// `mounted_path` モードのマウントポイント配下のパス
    pub mounted_path: String,
    /// `mounted_path` モードでマウントポイントであることを必須にする（誤書き込み防止）
    pub require_mount_point: bool,
    /// 保存先識別ファイルの ID（初回の保存先初期化で生成）
    pub target_id: String,
    /// NAS 読み戻し検証（無効化不可。設定として明示するだけ）
    pub verify_readback: bool,
    /// SSD 削除前に NAS 上の全ファイルを再ハッシュする
    pub final_rehash_before_cleanup: bool,
}

impl Default for NasConfig {
    fn default() -> Self {
        Self {
            mode: NasMode::Disabled,
            url: String::new(),
            username: String::new(),
            domain: String::new(),
            mounted_path: String::new(),
            require_mount_point: true,
            target_id: String::new(),
            verify_readback: true,
            final_rehash_before_cleanup: true,
        }
    }
}

impl NasConfig {
    /// 保存先を一意に表す文字列（Secret Service のアカウント名・表示用、秘密値なし）。
    pub fn display_target(&self) -> String {
        match self.mode {
            NasMode::Disabled => "未設定".into(),
            NasMode::Gvfs => self.url.clone(),
            NasMode::MountedPath => format!("file://{}", self.mounted_path),
        }
    }

    pub fn secret_account(&self) -> String {
        format!("nas:{}@{}", self.username, self.url.trim_end_matches('/'))
    }

    /// 書き込み済みジョブに影響する接続先の同一性（FR-037）。
    pub fn same_destination(&self, other: &NasConfig) -> bool {
        self.mode == other.mode
            && self.url.trim_end_matches('/') == other.url.trim_end_matches('/')
            && self.mounted_path.trim_end_matches('/') == other.mounted_path.trim_end_matches('/')
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub autostart: bool,
    pub window_mode: String,
    pub show_transfer_rate: bool,
    pub play_success_sound: bool,
    pub play_warning_sound: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            autostart: true,
            window_mode: "normal".into(),
            show_transfer_rate: true,
            play_success_sound: true,
            play_warning_sound: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferConfig {
    pub backoff_base_secs: u64,
    pub backoff_max_secs: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            backoff_base_secs: 5,
            backoff_max_secs: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub locale: String,
    pub timezone: String,
    pub max_active_media: u32,
    pub reserve_free_bytes: u64,
    pub staging_directory: String,
    pub log_retention_days: u32,
    pub max_copy_attempts: u32,
    pub copy_priority: String,
    pub auto_unmount: bool,
    pub unknown_file_policy: String,
    pub partition_policy: String,
    pub nas: NasConfig,
    pub ui: UiConfig,
    pub transfer: TransferConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: 1,
            locale: "ja-JP".into(),
            timezone: backup_core::session::DEFAULT_TIMEZONE.into(),
            max_active_media: 2,
            reserve_free_bytes: backup_core::DEFAULT_RESERVE_FREE_BYTES,
            staging_directory: "~/SDBackupStaging".into(),
            log_retention_days: 30,
            max_copy_attempts: backup_core::MAX_COPY_ATTEMPTS,
            copy_priority: "source_first".into(),
            auto_unmount: true,
            unknown_file_policy: "save_to_other".into(),
            partition_policy: "all_in_one_session".into(),
            nas: NasConfig::default(),
            ui: UiConfig::default(),
            transfer: TransferConfig::default(),
        }
    }
}

impl Config {
    pub fn staging_path(&self) -> PathBuf {
        expand_tilde(&self.staging_directory)
    }

    /// 安全側に補正し、明らかに不正な値はエラーにする。
    pub fn validate(&mut self) -> Result<(), ConfigError> {
        if self.schema_version != 1 {
            return Err(ConfigError::Invalid(format!(
                "schema_version {}",
                self.schema_version
            )));
        }
        if !(1..=2).contains(&self.max_active_media) {
            return Err(ConfigError::Invalid("max_active_media は 1〜2 です".into()));
        }
        // 設計上の固定値（設定で緩めることはできない）
        self.reserve_free_bytes = self
            .reserve_free_bytes
            .max(backup_core::DEFAULT_RESERVE_FREE_BYTES);
        self.max_copy_attempts = backup_core::MAX_COPY_ATTEMPTS;
        self.nas.verify_readback = true;
        self.unknown_file_policy = "save_to_other".into();
        self.partition_policy = "all_in_one_session".into();
        if self.log_retention_days == 0 {
            self.log_retention_days = 30;
        }
        if self.transfer.backoff_base_secs == 0 {
            self.transfer.backoff_base_secs = 5;
        }
        if self.transfer.backoff_max_secs < self.transfer.backoff_base_secs {
            self.transfer.backoff_max_secs = self.transfer.backoff_base_secs;
        }
        match self.nas.mode {
            NasMode::Disabled => {}
            NasMode::Gvfs => {
                parse_smb_url(&self.nas.url)?;
            }
            NasMode::MountedPath => {
                let p = Path::new(&self.nas.mounted_path);
                if !p.is_absolute() {
                    return Err(ConfigError::Invalid(
                        "NAS のマウントパスは絶対パスで指定してください".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut cfg = match fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e.into()),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// 一時ファイル + rename で原子的に保存（0600）。
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self)?;
        let dir = path
            .parent()
            .ok_or_else(|| ConfigError::Invalid("bad path".into()))?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".config.toml.{}.tmp", uuid::Uuid::new_v4()));
        {
            let mut o = fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            let mut f = o.open(&tmp)?;
            f.write_all(b"# SD Backup Manager settings (no secrets are stored here)\n")?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SMB URL
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmbUrl {
    pub host: String,
    pub share: String,
    /// 共有内のサブパス（先頭・末尾の `/` なし）。
    pub subpath: String,
}

impl SmbUrl {
    /// 共有ルートの URI（`gio mount` 用）。
    pub fn share_uri(&self) -> String {
        format!("smb://{}/{}/", self.host, self.share)
    }

    /// Windows の UNC パス（共有ルート）: `\\server\share`
    pub fn unc_share(&self) -> String {
        format!(r"\\{}\{}", self.host, self.share)
    }

    /// Windows の UNC パス（サブパスを含む保存先）
    pub fn unc_path(&self) -> String {
        if self.subpath.is_empty() {
            self.unc_share()
        } else {
            format!(r"{}\{}", self.unc_share(), self.subpath.replace('/', "\\"))
        }
    }
}

pub fn parse_smb_url(url: &str) -> Result<SmbUrl, ConfigError> {
    let rest = url
        .strip_prefix("smb://")
        .ok_or_else(|| ConfigError::Invalid("NAS のアドレスは smb:// で始めてください".into()))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.contains('@') || authority.contains(';') {
        return Err(ConfigError::Invalid(
            "アドレスにユーザー名やパスワードを含めないでください（別欄で入力）".into(),
        ));
    }
    if authority.is_empty()
        || !authority
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '_'))
    {
        return Err(ConfigError::Invalid(format!(
            "NAS のサーバー名が不正です: {authority}"
        )));
    }
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    let share = parts
        .next()
        .ok_or_else(|| {
            ConfigError::Invalid("共有名を指定してください（smb://サーバー/共有名/）".into())
        })?
        .to_string();
    let sub: Vec<&str> = parts.collect();
    for s in std::iter::once(share.as_str()).chain(sub.iter().copied()) {
        if s == "." || s == ".." || s.chars().any(|c| c.is_control()) {
            return Err(ConfigError::Invalid(format!("NAS のパスが不正です: {s}")));
        }
    }
    Ok(SmbUrl {
        host: authority.to_string(),
        share,
        subpath: sub.join("/"),
    })
}

// ---------------------------------------------------------------------------
// 形式定義
// ---------------------------------------------------------------------------

/// ユーザー定義の formats.toml があればそれを、なければ同梱定義を使う。
pub fn load_formats(paths: &AppPaths) -> Result<FormatDefinitions, String> {
    match fs::read_to_string(paths.formats_file()) {
        Ok(text) => FormatDefinitions::parse(&text).map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FormatDefinitions::default()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_and_contain_no_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let mut c = Config::default();
        c.nas.mode = NasMode::Gvfs;
        c.nas.url = "smb://nas.local/Festival/2026/".into();
        c.nas.username = "festival".into();
        c.save(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(!text.to_lowercase().contains("password"));
        let back = Config::load(&p).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load(&dir.path().join("none.toml")).unwrap();
        assert_eq!(c.max_active_media, 2);
        assert_eq!(c.reserve_free_bytes, 20_000_000_000);
    }

    #[test]
    fn safety_values_cannot_be_weakened() {
        let mut c = Config {
            reserve_free_bytes: 1,
            max_copy_attempts: 10,
            ..Config::default()
        };
        c.nas.verify_readback = false;
        c.validate().unwrap();
        assert_eq!(c.reserve_free_bytes, 20_000_000_000);
        assert_eq!(c.max_copy_attempts, 3);
        assert!(c.nas.verify_readback);
        let mut bad = Config {
            max_active_media: 3,
            ..Config::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn smb_url_parsing() {
        let u = parse_smb_url("smb://nas.example.local/FestivalBackup/").unwrap();
        assert_eq!(u.host, "nas.example.local");
        assert_eq!(u.share, "FestivalBackup");
        assert_eq!(u.subpath, "");
        assert_eq!(u.share_uri(), "smb://nas.example.local/FestivalBackup/");
        assert_eq!(u.unc_share(), r"\\nas.example.local\FestivalBackup");
        let u = parse_smb_url("smb://192.168.1.10/share/a/b").unwrap();
        assert_eq!(u.subpath, "a/b");
        assert_eq!(u.unc_path(), r"\\192.168.1.10\share\a\b");
        assert!(parse_smb_url("smb://user:pass@nas/share").is_err());
        assert!(parse_smb_url("http://nas/share").is_err());
        assert!(parse_smb_url("smb://nas/").is_err());
        assert!(parse_smb_url("smb://nas/share/../x").is_err());
        assert!(parse_smb_url("smb://na s/share").is_err());
    }

    #[test]
    fn destination_identity() {
        let mut a = NasConfig {
            mode: NasMode::Gvfs,
            url: "smb://a/s/".into(),
            ..NasConfig::default()
        };
        let mut b = a.clone();
        b.url = "smb://a/s".into();
        b.username = "other".into();
        assert!(a.same_destination(&b));
        a.url = "smb://b/s".into();
        assert!(!a.same_destination(&b));
    }

    #[test]
    fn tilde_expansion() {
        let home = home_dir();
        assert_eq!(
            expand_tilde("~/SDBackupStaging"),
            home.join("SDBackupStaging")
        );
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }

    #[test]
    fn windows_internal_paths_are_readable_and_keep_unc_roots() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\Taro\SDBackupStaging")),
            r"C:\Users\Taro\SDBackupStaging"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\backup")),
            r"\\server\share\backup"
        );
    }
}
