//! 使用者時區的顯示用辦法：UTC 偏移標籤（`+8`、`-5`、`+5:30`）。時區名稱本身存在 `[agents] user_time_zone`。

use chrono::{DateTime, Offset, Utc};

/// 解析 IANA 時區名稱，回該時區在 `now` 這一刻的 UTC 偏移標籤。解析不了回 `None`。
pub fn utc_offset_label(tz: &str, now: DateTime<Utc>) -> Option<String> {
    let tz: chrono_tz::Tz = tz.trim().parse().ok()?;
    let secs = now.with_timezone(&tz).offset().fix().local_minus_utc();
    let sign = if secs < 0 { '-' } else { '+' };
    let abs = secs.abs();
    let (h, m) = (abs / 3600, (abs % 3600) / 60);
    Some(if m == 0 { format!("{sign}{h}") } else { format!("{sign}{h}:{m:02}") })
}

/// 時區名稱是否是 chrono-tz 認得的 IANA 名稱。
pub fn is_valid_time_zone(tz: &str) -> bool {
    tz.trim().parse::<chrono_tz::Tz>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    #[test]
    fn offset_labels_follow_the_zone() {
        assert_eq!(utc_offset_label("Asia/Taipei", now()).as_deref(), Some("+8"));
        assert_eq!(utc_offset_label("Asia/Kolkata", now()).as_deref(), Some("+5:30"));
        assert_eq!(utc_offset_label("America/New_York", now()).as_deref(), Some("-4"));
        assert_eq!(utc_offset_label("Europe/London", now()).as_deref(), Some("+1"));
        assert_eq!(utc_offset_label("Etc/UTC", now()).as_deref(), Some("+0"));
    }

    #[test]
    fn unknown_zone_has_no_label() {
        assert_eq!(utc_offset_label("Mars/Olympus", now()), None);
        assert!(!is_valid_time_zone("Mars/Olympus"));
        assert!(is_valid_time_zone("Asia/Taipei"));
    }
}
