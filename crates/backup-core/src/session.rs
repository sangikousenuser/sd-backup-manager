//! セッションの日時フォルダ名（4.2）。
//!
//! 新規は `sdN/YYYYMMDDHHMMSS`（ローカル時刻）。同一秒の衝突時のみ連番を追加する。
//! 旧形式 `YYYYMMDDHHMMSSNN` も再開・参照用に受け付ける。一意性は DB で担保する。

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;

/// 既定のタイムゾーン。
pub const DEFAULT_TIMEZONE: &str = "Asia/Tokyo";

pub fn parse_timezone(name: &str) -> Tz {
    name.parse().unwrap_or(chrono_tz::Asia::Tokyo)
}

/// 日時部分（14 桁）。
pub fn folder_base(now: DateTime<Utc>, tz: Tz) -> String {
    tz.from_utc_datetime(&now.naive_utc())
        .format("%Y%m%d%H%M%S")
        .to_string()
}

/// `n` 番目の候補名。
pub fn folder_candidate(base: &str, n: u32) -> String {
    if n < 100 {
        format!("{base}{n:02}")
    } else {
        format!("{base}{n}")
    }
}

/// `sdN/日時` または旧形式の数字のみ16桁以上。NAS 操作の境界確認に使う。
pub fn is_valid_folder_name(name: &str) -> bool {
    if let Some((card, time)) = name.split_once('/') {
        return is_valid_card_name(card)
            && time.len() >= 14
            && time.bytes().all(|b| b.is_ascii_digit());
    }
    name.len() >= 16 && name.bytes().all(|b| b.is_ascii_digit())
}

pub fn is_valid_card_name(name: &str) -> bool {
    name.strip_prefix("sd").is_some_and(|n| {
        !n.is_empty() && !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit())
    })
}

/// 新方式のセッションは分類・過去ファイル参照を行わず、毎回全内容を保存する。
pub fn is_card_folder(name: &str) -> bool {
    name.contains('/') && is_valid_folder_name(name)
}

/// UUIDv7 のセッション ID。
pub fn new_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// セッション ID として妥当か（ステージング削除の境界確認）。
pub fn is_valid_session_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok() && id.len() == 36
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn jst_folder_name() {
        let utc = Utc.with_ymd_and_hms(2026, 10, 15, 0, 32, 15).unwrap();
        let base = folder_base(utc, parse_timezone("Asia/Tokyo"));
        assert_eq!(base, "20261015093215");
        assert_eq!(folder_candidate(&base, 1), "2026101509321501");
        assert_eq!(folder_candidate(&base, 0), "2026101509321500");
        assert_eq!(folder_candidate(&base, 100), "20261015093215100");
        assert!(is_valid_folder_name("2026101509321501"));
        assert!(is_valid_folder_name("20261015093215100"));
        assert!(!is_valid_folder_name("20261015093215"));
        assert!(!is_valid_folder_name("2026101509321a01"));
        assert!(!is_valid_folder_name("../../etc/passwd"));
    }

    #[test]
    fn session_ids() {
        let id = new_session_id();
        assert!(is_valid_session_id(&id));
        assert!(!is_valid_session_id("../x"));
        assert!(!is_valid_session_id(""));
    }

    #[test]
    fn card_folders_are_bounded_and_legacy_names_remain_valid() {
        assert!(is_card_folder("sd1/20261028112834"));
        assert!(is_card_folder("sd12/2026102811283401"));
        assert!(is_valid_folder_name("2026101509321501"));
        for name in [
            "../20261028112834",
            "sd0/20261028112834",
            "sd01/20261028112834",
            "sd1/../20261028112834",
            "sd1\\20261028112834",
            "sd1/20261028112834/x",
            "sd1/2026102811283",
        ] {
            assert!(!is_valid_folder_name(name), "{name}");
        }
    }

    #[test]
    fn invalid_timezone_falls_back_to_tokyo() {
        assert_eq!(parse_timezone("Nowhere/Bogus"), chrono_tz::Asia::Tokyo);
    }
}
