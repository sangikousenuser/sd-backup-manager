//! NAS 資格情報の保管（11 章-2）。
//!
//! Linux では Secret Service（GNOME Keyring 等）、macOS（開発時）では Keychain を使う。
//! パスワードを TOML・ログ・コマンドライン引数に出さない。
//! keyring の Secret Service 実装は内部で非同期ランタイムを使うため、
//! tokio のワーカースレッドからは `spawn_blocking` 経由で呼ぶこと。

use thiserror::Error;

pub const SERVICE: &str = "sd-backup-manager";

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("資格情報ストア（Secret Service）を利用できません: {0}")]
    Unavailable(String),
    #[error("資格情報の操作に失敗しました: {0}")]
    Failed(String),
}

fn entry(account: &str) -> Result<keyring::Entry, SecretError> {
    keyring::Entry::new(SERVICE, account).map_err(|e| SecretError::Unavailable(e.to_string()))
}

pub fn store_password(account: &str, password: &str) -> Result<(), SecretError> {
    entry(account)?
        .set_password(password)
        .map_err(|e| SecretError::Failed(e.to_string()))
}

pub fn load_password(account: &str) -> Result<Option<String>, SecretError> {
    match entry(account)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(SecretError::Failed(e.to_string())),
    }
}

pub fn delete_password(account: &str) -> Result<(), SecretError> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(SecretError::Failed(e.to_string())),
    }
}

/// ログ・エクスポート用に秘密らしき値を伏せる（11 章-9）。
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let lower = line.to_lowercase();
        let redacted = if let Some(pos) = ["password", "passwd", "secret", "token", "credential"]
            .iter()
            .filter_map(|k| lower.find(k))
            .min()
        {
            format!("{}[REDACTED]", &line[..pos])
        } else {
            redact_url_userinfo(line)
        };
        out.push_str(&redacted);
        out.push('\n');
    }
    out
}

/// `smb://user:pass@host` の userinfo を伏せる。
fn redact_url_userinfo(line: &str) -> String {
    let mut result = String::new();
    let mut rest = line;
    while let Some(pos) = rest.find("://") {
        let (head, tail) = rest.split_at(pos + 3);
        result.push_str(head);
        let end = tail.find(['/', ' ', '"', '\'']).unwrap_or(tail.len());
        let authority = &tail[..end];
        if let Some(at) = authority.rfind('@') {
            result.push_str("[REDACTED]@");
            result.push_str(&authority[at + 1..]);
        } else {
            result.push_str(authority);
        }
        rest = &tail[end..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction() {
        let r = redact("nas password=hunter2\nurl smb://bob:pw@nas/share ok\nplain line");
        assert!(!r.contains("hunter2"));
        assert!(!r.contains("bob:pw"));
        assert!(r.contains("smb://[REDACTED]@nas/share ok"));
        assert!(r.contains("plain line"));
    }
}
