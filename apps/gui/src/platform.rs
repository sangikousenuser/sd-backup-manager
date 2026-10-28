//! ファイルマネージャー起動の OS 差分。
pub fn open_folder(path: &str) -> Result<(), String> {
    let path = std::path::Path::new(path);
    if !path.is_absolute() || !path.is_dir() {
        return Err(
            "保存先フォルダがありません（NAS 転送完了後は一時データが削除されます）".into(),
        );
    }
    #[cfg(windows)]
    let command = "explorer.exe";
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(not(any(windows, target_os = "macos")))]
    let command = "xdg-open";
    let path = backup_config::display_path(path);
    std::process::Command::new(command)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("フォルダを開けません: {e}"))
}
