//! 排程的時間計算（純函式，不碰 DB）。
//!
//! - 單次：指定的本地時間＋IANA 時區；本地不存在（DST 跳過）→ 拒絕，要求重新選。
//! - 固定間隔：以建立時的 UTC anchor 為準，`anchor + k * every`，與上次完成時間無關。
//! - 每日／每週：本地時間；不存在的時間（春季跳過）跳到之後的有效時間，重複的時間（秋季回撥）取較早的那次。

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// 排程的時間規格；序列化成 `schedules.spec_json`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Spec {
    /// `at` 是本地（`tz`）的 `YYYY-MM-DDTHH:MM[:SS]`。
    Once { at: String },
    /// `every_secs` 秒一次；`anchor` 是建立時的 UTC（RFC3339）。
    Interval { every_secs: i64, anchor: DateTime<Utc> },
    /// 每天本地 `time`（`HH:MM[:SS]`）。
    Daily { time: String },
    /// 每週本地 `time`，`weekdays` 為 0=週一 … 6=週日 的集合。
    Weekly { time: String, weekdays: Vec<u8> },
}

/// 固定間隔下限（一分鐘），避免誤設成幾秒一次把 bot 排滿。
pub const MIN_INTERVAL_SECS: i64 = 60;
/// 固定間隔上限（一年）；超過的值 API 回 400，也避免 `anchor + k * every` 溢出。
pub const MAX_INTERVAL_SECS: i64 = 366 * 24 * 3600;

fn check_interval(every_secs: i64) -> Result<()> {
    if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&every_secs) {
        bail!("固定間隔要在 {MIN_INTERVAL_SECS} 秒到 {MAX_INTERVAL_SECS} 秒之間");
    }
    Ok(())
}

/// 秒可省略：補 `:00` 後交給 chrono 的 FromStr（不寫格式字串）。
fn parse_time(s: &str) -> Result<NaiveTime> {
    let full = if s.len() == 5 { format!("{s}:00") } else { s.to_string() };
    full.parse::<NaiveTime>().with_context(|| format!("時間格式不對：{s}"))
}

fn parse_local(s: &str) -> Result<NaiveDateTime> {
    let full = if s.len() == 16 { format!("{s}:00") } else { s.to_string() };
    full.parse::<NaiveDateTime>().with_context(|| format!("日期時間格式不對：{s}"))
}

/// 本地時間換成 UTC；`None` 代表該本地時間不存在（DST 春季跳過）。重複時取較早的那次。
fn resolve(tz: Tz, local: NaiveDateTime) -> Option<DateTime<Utc>> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) => Some(t.with_timezone(&Utc)),
        LocalResult::Ambiguous(earlier, _later) => Some(earlier.with_timezone(&Utc)),
        LocalResult::None => None,
    }
}

fn parse_tz(name: &str) -> Result<Tz> {
    name.parse::<Tz>().map_err(|_| anyhow::anyhow!("不認得的時區：{name}"))
}

fn weekday_of(n: u8) -> Option<Weekday> {
    Some(match n {
        0 => Weekday::Mon,
        1 => Weekday::Tue,
        2 => Weekday::Wed,
        3 => Weekday::Thu,
        4 => Weekday::Fri,
        5 => Weekday::Sat,
        6 => Weekday::Sun,
        _ => return None,
    })
}

/// 驗證規格並回傳第一次觸發時間（嚴格晚於 `now`）。
/// 單次若本地時間不存在、或已過去，回錯誤；其餘模式回 `None` 以外的有效時間。
pub fn validate_and_first(spec: &Spec, tz_name: &str, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let tz = parse_tz(tz_name)?;
    match spec {
        Spec::Once { at } => {
            let local = parse_local(at)?;
            let Some(when) = resolve(tz, local) else {
                bail!("{at} 在 {tz_name} 時區不存在（夏令時間跳過），請重新選時間");
            };
            if when <= now {
                bail!("單次時間 {at} 已經過去，請選未來的時間");
            }
            Ok(Some(when))
        }
        Spec::Interval { every_secs, .. } => {
            check_interval(*every_secs)?;
            next_after(spec, tz_name, now)
        }
        Spec::Daily { time } => {
            parse_time(time)?;
            next_after(spec, tz_name, now)
        }
        Spec::Weekly { time, weekdays } => {
            parse_time(time)?;
            if weekdays.is_empty() || weekdays.iter().any(|d| weekday_of(*d).is_none()) {
                bail!("每週至少選一天，星期只能是 0（週一）到 6（週日）");
            }
            next_after(spec, tz_name, now)
        }
    }
}

/// 嚴格晚於 `after` 的下一次觸發時間。單次模式只有「尚未發生」的那次，過去的回 `None`。
pub fn next_after(spec: &Spec, tz_name: &str, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let tz = parse_tz(tz_name)?;
    match spec {
        Spec::Once { at } => {
            let Some(when) = resolve(tz, parse_local(at)?) else { return Ok(None) };
            Ok((when > after).then_some(when))
        }
        Spec::Interval { every_secs, anchor } => {
            check_interval(*every_secs)?;
            // 先比較時刻再算秒數：anchor 前不到一秒時 num_seconds() 會截成 0，不能當成「已過 anchor」。
            if after < *anchor {
                return Ok(Some(*anchor));
            }
            let step = *every_secs;
            let k = (after - *anchor).num_seconds() / step + 1;
            let next = k
                .checked_mul(step)
                .and_then(|offset| anchor.checked_add_signed(Duration::seconds(offset)))
                .context("固定間隔的下一次時間超出可表示的範圍")?;
            Ok(Some(next))
        }
        Spec::Daily { time } => daily_like(tz, parse_time(time)?, None, after),
        Spec::Weekly { time, weekdays } => {
            let days: Vec<Weekday> = weekdays.iter().filter_map(|d| weekday_of(*d)).collect();
            daily_like(tz, parse_time(time)?, Some(days), after)
        }
    }
}

/// 從 `after` 的本地日期起往後找，最多 400 天（涵蓋每週與跳過的日子）。
fn daily_like(tz: Tz, time: NaiveTime, days: Option<Vec<Weekday>>, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let start: NaiveDate = after.with_timezone(&tz).date_naive();
    for offset in 0..400 {
        let date = start + Duration::days(offset);
        if let Some(days) = &days {
            if !days.contains(&date.weekday()) {
                continue;
            }
        }
        // 本地不存在的時間就跳過這一天；這樣 DST 跳過當天不會補跑，也不會重送到隔天同一時刻。
        let Some(when) = resolve(tz, date.and_time(time)) else { continue };
        if when > after {
            return Ok(Some(when));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    #[test]
    fn once_is_converted_from_local_zone() {
        let spec = Spec::Once { at: "2026-10-20T09:00".into() };
        let got = validate_and_first(&spec, "Asia/Taipei", utc(2026, 10, 13, 0, 0)).unwrap();
        assert_eq!(got, Some(utc(2026, 10, 20, 1, 0)));
    }

    #[test]
    fn once_in_the_past_is_rejected() {
        let spec = Spec::Once { at: "2026-10-01T09:00".into() };
        assert!(validate_and_first(&spec, "Asia/Taipei", utc(2026, 10, 13, 0, 0)).is_err());
    }

    #[test]
    fn once_in_dst_gap_is_rejected() {
        // 美東 2026-03-08 凌晨 2:00 → 3:00 跳過。
        let spec = Spec::Once { at: "2026-03-08T02:30".into() };
        let err = validate_and_first(&spec, "America/New_York", utc(2026, 1, 1, 0, 0)).unwrap_err();
        assert!(err.to_string().contains("不存在"), "{err}");
    }

    #[test]
    fn once_in_dst_overlap_takes_earlier() {
        // 美東 2026-11-01 凌晨 1:30 重複；取較早（EDT, UTC-4）。
        let spec = Spec::Once { at: "2026-11-01T01:30".into() };
        let got = validate_and_first(&spec, "America/New_York", utc(2026, 10, 1, 0, 0)).unwrap();
        assert_eq!(got, Some(utc(2026, 11, 1, 5, 30)));
    }

    #[test]
    fn interval_is_anchored_not_drifting() {
        let spec = Spec::Interval { every_secs: 3600, anchor: utc(2026, 10, 13, 0, 0) };
        // 任務慢跑完也不影響下一次：永遠是 anchor 的整數倍。
        assert_eq!(next_after(&spec, "UTC", utc(2026, 10, 13, 0, 0)).unwrap(), Some(utc(2026, 10, 13, 1, 0)));
        assert_eq!(next_after(&spec, "UTC", utc(2026, 10, 13, 1, 20)).unwrap(), Some(utc(2026, 10, 13, 2, 0)));
    }

    #[test]
    fn interval_before_anchor_by_under_a_second_returns_anchor() {
        let anchor = utc(2026, 10, 13, 0, 0);
        let spec = Spec::Interval { every_secs: 3600, anchor };
        let just_before = anchor - Duration::milliseconds(500);
        assert_eq!(next_after(&spec, "UTC", just_before).unwrap(), Some(anchor));
        // 晚 500ms 時下一次是 anchor+1h，不能被截斷成跳過 anchor 或重跑 anchor。
        let just_after = anchor + Duration::milliseconds(500);
        assert_eq!(next_after(&spec, "UTC", just_after).unwrap(), Some(utc(2026, 10, 13, 1, 0)));
    }

    #[test]
    fn extreme_interval_is_rejected_without_panic() {
        for every in [i64::MAX, MAX_INTERVAL_SECS + 1] {
            let spec = Spec::Interval { every_secs: every, anchor: utc(2026, 10, 13, 0, 0) };
            assert!(validate_and_first(&spec, "UTC", utc(2026, 10, 13, 0, 0)).is_err());
        }
    }

    #[test]
    fn interval_overflowing_date_range_is_an_error_not_a_panic() {
        // 逼近 chrono 可表示上限：下一次時間超出範圍，應回錯誤。
        let spec = Spec::Interval { every_secs: MAX_INTERVAL_SECS, anchor: DateTime::<Utc>::MAX_UTC - Duration::days(1) };
        assert!(next_after(&spec, "UTC", DateTime::<Utc>::MAX_UTC - Duration::hours(1)).is_err());
    }

    #[test]
    fn interval_below_minimum_is_rejected() {
        let spec = Spec::Interval { every_secs: 5, anchor: utc(2026, 10, 13, 0, 0) };
        assert!(validate_and_first(&spec, "UTC", utc(2026, 10, 13, 0, 0)).is_err());
    }

    #[test]
    fn daily_respects_local_wall_clock_across_dst() {
        // 美東每天 09:00：夏令（UTC-4）→ 13:00Z，冬令（UTC-5）→ 14:00Z。
        let spec = Spec::Daily { time: "09:00".into() };
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 3, 7, 14, 0)).unwrap(), Some(utc(2026, 3, 8, 13, 0)));
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 3, 9, 13, 0)).unwrap(), Some(utc(2026, 3, 10, 13, 0)));
        // 冬令 → 夏令之後仍是本地 09:00（EDT, UTC-4）。
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 3, 14, 13, 0)).unwrap(), Some(utc(2026, 3, 15, 13, 0)));
    }

    #[test]
    fn daily_skips_nonexistent_local_time_and_does_not_double_run_overlap() {
        // 美東每天 02:30：2026-03-08 不存在 → 跳過當天。
        let spec = Spec::Daily { time: "02:30".into() };
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 3, 8, 6, 0)).unwrap(), Some(utc(2026, 3, 9, 6, 30)));
        // 每天 01:30：2026-11-01 重複，只跑較早那次（EDT, 05:30Z），不再於 06:30Z 補跑。
        let spec = Spec::Daily { time: "01:30".into() };
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 11, 1, 5, 30)).unwrap(), Some(utc(2026, 11, 2, 6, 30)));
    }

    #[test]
    fn weekly_picks_next_selected_weekday() {
        // 2026-10-13 是週二。選週一(0)、週五(4)，09:00 台北。
        let spec = Spec::Weekly { time: "09:00".into(), weekdays: vec![0, 4] };
        // 台北 09:00 = UTC 01:00；週二之後下一個是週五 10-16。
        assert_eq!(next_after(&spec, "Asia/Taipei", utc(2026, 10, 13, 2, 0)).unwrap(), Some(utc(2026, 10, 16, 1, 0)));
    }

    #[test]
    fn weekly_rejects_empty_or_bad_weekday() {
        assert!(validate_and_first(&Spec::Weekly { time: "09:00".into(), weekdays: vec![] }, "UTC", utc(2026, 1, 1, 0, 0)).is_err());
        assert!(validate_and_first(&Spec::Weekly { time: "09:00".into(), weekdays: vec![7] }, "UTC", utc(2026, 1, 1, 0, 0)).is_err());
    }

    #[test]
    fn unknown_zone_is_rejected() {
        assert!(validate_and_first(&Spec::Daily { time: "09:00".into() }, "Mars/Olympus", utc(2026, 1, 1, 0, 0)).is_err());
    }
}
