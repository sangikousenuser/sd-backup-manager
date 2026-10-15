//! セッションの日時フォルダ名（4.2）。
//!
//! `YYYYMMDDHHMMSSNN`（ローカル時刻、ASCII 数字のみ）。`NN` は同一秒衝突回避用の
//! 00〜99 の連番で、100 件以上はさらに桁を増やす。一意性は DB の UNIQUE 制約で担保する。

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

/// フォルダ名として妥当か（数字のみ・16 桁以上）。NAS 削除等の境界確認に使う。
pub fn is_valid_folder_name(name: &str) -> bool {
    name.len() >= 16 && name.bytes().all(|b| b.is_ascii_digit())
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
    fn invalid_timezone_falls_back_to_tokyo() {
        assert_eq!(parse_timezone("Nowhere/Bogus"), chrono_tz::Asia::Tokyo);
    }
}
