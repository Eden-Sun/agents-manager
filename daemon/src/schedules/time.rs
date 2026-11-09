//! 排程的時間計算（純函式，不碰 DB）。
//!
//! - 單次：指定的本地時間＋IANA 時區；本地不存在（DST 跳過）→ 拒絕，要求重新選。
//! - 固定間隔：以建立時的 UTC anchor 為準，`anchor + k * every`，與上次完成時間無關。
//! - 每日／每週：本地時間；不存在的時間（春季跳過）跳到之後的有效時間，重複的時間（秋季回撥）取較早的那次。

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
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
    /// 每天本地時段。`slots` 非空時以它為準；`time` 是第一個時段（舊格式只有 `time`，讀起來仍是單一時段）。
    Daily { time: String, #[serde(default)] slots: Vec<Slot> },
    /// 每週本地時段，`weekdays` 為 0=週一 … 6=週日 的集合；時段規則同 `Daily`。
    Weekly { time: String, weekdays: Vec<u8>, #[serde(default)] slots: Vec<Slot> },
}

/// 一個時段：本地 `time`（`HH:MM[:SS]`）加上給人看的標籤（例：「上班」）。標籤會展開進 prompt 的 `{{slot}}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub time: String,
    #[serde(default)]
    pub label: String,
    /// 隨機區間的結束時間（`HH:MM[:SS]`）；有值時 `time`–`until` 是區間，每次在區間內均勻隨機挑一秒觸發。沒有＝固定時間。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// 隨機區間的錯誤（帶機器可讀的 code，API 回 400 `{"error": code, "message": …}`）。
#[derive(Debug)]
pub struct RangeError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RangeError {}

/// 隨機區間長度上限（12 小時）。
pub const MAX_RANGE_SECS: i64 = 12 * 3600;

fn range_err(code: &'static str, message: String) -> anyhow::Error {
    anyhow::Error::new(RangeError { code, message })
}

/// 挑一個 `[lo, hi]`（含，epoch 秒）內的秒；測試可換成固定值。
pub type Picker<'a> = &'a mut dyn FnMut(i64, i64) -> i64;

fn random_pick(lo: i64, hi: i64) -> i64 {
    use rand::Rng;
    rand::thread_rng().gen_range(lo..=hi)
}

/// 一個每天／每週排程最多幾個時段。
pub const MAX_SLOTS: usize = 8;
/// 時段標籤的字數上限。
pub const MAX_SLOT_LABEL_CHARS: usize = 20;

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

/// 這個規格實際的時段清單：沒有 `slots` 時就是舊格式的單一 `time`（標籤空白）。
fn slot_list(time: &str, slots: &[Slot]) -> Vec<Slot> {
    if slots.is_empty() {
        vec![Slot { time: time.to_string(), label: String::new(), until: None }]
    } else {
        slots.to_vec()
    }
}

/// 時段檢查：數量、時間格式、時間不重複、標籤長度。
fn check_slots(slots: &[Slot]) -> Result<()> {
    if slots.len() > MAX_SLOTS {
        bail!("時段最多 {MAX_SLOTS} 個");
    }
    let mut seen = HashSet::new();
    for slot in slots {
        let t = parse_time(&slot.time)?;
        if let Some(until) = &slot.until {
            let end = parse_time(until).map_err(|_| range_err("slot_until_invalid", format!("隨機區間的結束時間格式不對：{until}")))?;
            if end <= t {
                return Err(range_err("slot_range_not_after_start", format!("隨機區間的結束時間 {until} 必須晚於開始時間 {}（不可跨午夜）", slot.time)));
            }
            if (end - t).num_seconds() > MAX_RANGE_SECS {
                return Err(range_err("slot_range_too_long", format!("隨機區間最長 12 小時：{}–{until}", slot.time)));
            }
        }
        if slot.label.chars().count() > MAX_SLOT_LABEL_CHARS {
            bail!("時段標籤不能超過 {MAX_SLOT_LABEL_CHARS} 字");
        }
        if !seen.insert(t) {
            bail!("時段時間重複：{}", slot.time);
        }
    }
    Ok(())
}

fn weekdays_of(weekdays: &[u8]) -> Vec<Weekday> {
    weekdays.iter().filter_map(|d| weekday_of(*d)).collect()
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
        Spec::Daily { time, slots } => {
            check_slots(&slot_list(time, slots))?;
            next_after(spec, tz_name, now)
        }
        Spec::Weekly { time, weekdays, slots } => {
            check_slots(&slot_list(time, slots))?;
            if weekdays.is_empty() || weekdays.iter().any(|d| weekday_of(*d).is_none()) {
                bail!("每週至少選一天，星期只能是 0（週一）到 6（週日）");
            }
            next_after(spec, tz_name, now)
        }
    }
}

/// 嚴格晚於 `after` 的下一次觸發時間。單次模式只有「尚未發生」的那次，過去的回 `None`。
pub fn next_after(spec: &Spec, tz_name: &str, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    Ok(next_slot_after(spec, tz_name, after)?.map(|(when, _)| when))
}

/// 同 [`next_after`]，另回傳該次觸發所屬時段的標籤（單次／固定間隔沒有時段，標籤空白）。
pub fn next_slot_after(spec: &Spec, tz_name: &str, after: DateTime<Utc>) -> Result<Option<(DateTime<Utc>, String)>> {
    next_slot_after_with(spec, tz_name, after, &mut random_pick)
}

/// 同 [`next_slot_after`]，隨機區間用 `pick` 挑秒（測試注入）。
pub fn next_slot_after_with(spec: &Spec, tz_name: &str, after: DateTime<Utc>, pick: Picker<'_>) -> Result<Option<(DateTime<Utc>, String)>> {
    let tz = parse_tz(tz_name)?;
    match spec {
        Spec::Daily { time, slots } => earliest_slot(tz, &slot_list(time, slots), None, after, pick),
        Spec::Weekly { time, weekdays, slots } => earliest_slot(tz, &slot_list(time, slots), Some(weekdays_of(weekdays)), after, pick),
        _ => Ok(next_plain(spec, tz, after)?.map(|when| (when, String::new()))),
    }
}

/// 這一刻（`when`）若是某個時段的觸發時刻，回傳該時段的標籤；不是（或單次／固定間隔）回 `None`。
pub fn slot_label_at(spec: &Spec, tz_name: &str, when: DateTime<Utc>) -> Result<Option<String>> {
    Ok(slot_at(spec, tz_name, when)?.map(|(label, _)| label))
}

/// 同 [`slot_label_at`]，另回傳該時段在當天的結束時刻（固定時段＝`when` 本身；隨機區間＝區間結束）。
/// `when` 落在隨機區間內（含兩端）就算屬於該時段。
pub fn slot_at(spec: &Spec, tz_name: &str, when: DateTime<Utc>) -> Result<Option<(String, DateTime<Utc>)>> {
    let tz = parse_tz(tz_name)?;
    let (days, slots) = match spec {
        Spec::Daily { time, slots } => (None, slot_list(time, slots)),
        Spec::Weekly { time, weekdays, slots } => (Some(weekdays_of(weekdays)), slot_list(time, slots)),
        _ => return Ok(None),
    };
    let local_date = when.with_timezone(&tz).date_naive();
    for slot in &slots {
        let start = parse_time(&slot.time)?;
        if let Some(until) = &slot.until {
            let end = parse_time(until)?;
            if let Some(days) = &days {
                if !days.contains(&local_date.weekday()) {
                    continue;
                }
            }
            let (Some(lo), Some(hi)) = (resolve(tz, local_date.and_time(start)), resolve(tz, local_date.and_time(end))) else { continue };
            if lo <= when && when <= hi {
                return Ok(Some((slot.label.clone(), hi)));
            }
            continue;
        }
        // 往前退 1 毫秒再找「嚴格晚於」的下一次：剛好落在 `when` 的時段就會是 `when` 本身。
        let before = when - Duration::milliseconds(1);
        if daily_like(tz, start, days.clone(), before)? == Some(when) {
            return Ok(Some((slot.label.clone(), when)));
        }
    }
    Ok(None)
}

fn earliest_slot(tz: Tz, slots: &[Slot], days: Option<Vec<Weekday>>, after: DateTime<Utc>, pick: Picker<'_>) -> Result<Option<(DateTime<Utc>, String)>> {
    // 先比出最早的候選（固定時段 lo==hi；隨機區間是還沒抽的 [lo, hi]），只對勝出的那個抽籤。
    let mut best: Option<(DateTime<Utc>, DateTime<Utc>, String)> = None;
    for slot in slots {
        let start = parse_time(&slot.time)?;
        let cand = match &slot.until {
            None => daily_like(tz, start, days.clone(), after)?.map(|w| (w, w)),
            Some(until) => window_like(tz, start, parse_time(until)?, days.clone(), after)?,
        };
        if let Some((lo, hi)) = cand {
            if best.as_ref().is_none_or(|(b, _, _)| lo < *b) {
                best = Some((lo, hi, slot.label.clone()));
            }
        }
    }
    Ok(best.map(|(lo, hi, label)| {
        let when = if lo == hi { lo } else { DateTime::from_timestamp(pick(lo.timestamp(), hi.timestamp()).clamp(lo.timestamp(), hi.timestamp()), 0).unwrap_or(lo) };
        (when, label)
    }))
}

/// 隨機區間：從 `after` 的本地日期起，第一個「還有可抽秒數」的區間，回傳可抽範圍 `[lo, hi]`（整秒，`lo` 嚴格晚於 `after`）。
fn window_like(tz: Tz, start: NaiveTime, end: NaiveTime, days: Option<Vec<Weekday>>, after: DateTime<Utc>) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
    let first: NaiveDate = after.with_timezone(&tz).date_naive();
    // 嚴格晚於 `after` 的第一個整秒。
    let floor = DateTime::from_timestamp(after.timestamp() + 1, 0).context("時間超出範圍")?;
    for offset in 0..400 {
        let date = first + Duration::days(offset);
        if let Some(days) = &days {
            if !days.contains(&date.weekday()) {
                continue;
            }
        }
        let (Some(lo), Some(hi)) = (resolve(tz, date.and_time(start)), resolve(tz, date.and_time(end))) else { continue };
        let lo = lo.max(floor);
        if lo <= hi {
            return Ok(Some((lo, hi)));
        }
    }
    Ok(None)
}

fn next_plain(spec: &Spec, tz: Tz, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
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
        Spec::Daily { .. } | Spec::Weekly { .. } => unreachable!("每天／每週由 next_slot_after 處理"),
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
        let spec = Spec::Daily { time: "09:00".into(), slots: vec![] };
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 3, 7, 14, 0)).unwrap(), Some(utc(2026, 3, 8, 13, 0)));
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 3, 9, 13, 0)).unwrap(), Some(utc(2026, 3, 10, 13, 0)));
        // 冬令 → 夏令之後仍是本地 09:00（EDT, UTC-4）。
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 3, 14, 13, 0)).unwrap(), Some(utc(2026, 3, 15, 13, 0)));
    }

    #[test]
    fn daily_skips_nonexistent_local_time_and_does_not_double_run_overlap() {
        // 美東每天 02:30：2026-03-08 不存在 → 跳過當天。
        let spec = Spec::Daily { time: "02:30".into(), slots: vec![] };
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 3, 8, 6, 0)).unwrap(), Some(utc(2026, 3, 9, 6, 30)));
        // 每天 01:30：2026-11-01 重複，只跑較早那次（EDT, 05:30Z），不再於 06:30Z 補跑。
        let spec = Spec::Daily { time: "01:30".into(), slots: vec![] };
        assert_eq!(next_after(&spec, "America/New_York", utc(2026, 11, 1, 5, 30)).unwrap(), Some(utc(2026, 11, 2, 6, 30)));
    }

    #[test]
    fn weekly_picks_next_selected_weekday() {
        // 2026-10-13 是週二。選週一(0)、週五(4)，09:00 台北。
        let spec = Spec::Weekly { time: "09:00".into(), weekdays: vec![0, 4], slots: vec![] };
        // 台北 09:00 = UTC 01:00；週二之後下一個是週五 10-16。
        assert_eq!(next_after(&spec, "Asia/Taipei", utc(2026, 10, 13, 2, 0)).unwrap(), Some(utc(2026, 10, 16, 1, 0)));
    }

    #[test]
    fn weekly_rejects_empty_or_bad_weekday() {
        assert!(validate_and_first(&Spec::Weekly { time: "09:00".into(), weekdays: vec![], slots: vec![] }, "UTC", utc(2026, 1, 1, 0, 0)).is_err());
        assert!(validate_and_first(&Spec::Weekly { time: "09:00".into(), weekdays: vec![7], slots: vec![] }, "UTC", utc(2026, 1, 1, 0, 0)).is_err());
    }

    #[test]
    fn unknown_zone_is_rejected() {
        assert!(validate_and_first(&Spec::Daily { time: "09:00".into(), slots: vec![] }, "Mars/Olympus", utc(2026, 1, 1, 0, 0)).is_err());
    }

    fn slot(time: &str, label: &str) -> Slot {
        Slot { time: time.into(), label: label.into(), until: None }
    }

    #[test]
    fn multi_slot_daily_picks_earliest_and_labels_it() {
        // 台北每天 08:30「上班」、18:05「下班」。
        let spec = Spec::Daily { time: "08:30".into(), slots: vec![slot("08:30", "上班"), slot("18:05", "下班")] };
        // 台北 08:30 = UTC 00:30；10:00 台北之後的下一次是 18:05 台北 = UTC 10:05。
        assert_eq!(next_slot_after(&spec, "Asia/Taipei", utc(2026, 11, 9, 2, 0)).unwrap(), Some((utc(2026, 11, 9, 10, 5), "下班".into())));
        // 18:05 之後才是隔天 08:30。
        assert_eq!(next_after(&spec, "Asia/Taipei", utc(2026, 11, 9, 10, 5)).unwrap(), Some(utc(2026, 11, 10, 0, 30)));
        assert_eq!(slot_label_at(&spec, "Asia/Taipei", utc(2026, 11, 9, 0, 30)).unwrap(), Some("上班".into()));
        assert_eq!(slot_label_at(&spec, "Asia/Taipei", utc(2026, 11, 9, 0, 31)).unwrap(), None);
    }

    #[test]
    fn multi_slot_weekly_applies_weekdays_to_every_slot() {
        let spec = Spec::Weekly { time: "08:30".into(), weekdays: vec![0], slots: vec![slot("08:30", "上班"), slot("18:05", "下班")] };
        // 2026-11-09 是週一；週一 18:05 台北（10:05Z）之後，下一個是週一 08:30 的下一週（11-16）。
        assert_eq!(next_slot_after(&spec, "Asia/Taipei", utc(2026, 11, 9, 10, 5)).unwrap(), Some((utc(2026, 11, 16, 0, 30), "上班".into())));
        assert_eq!(slot_label_at(&spec, "Asia/Taipei", utc(2026, 11, 9, 10, 5)).unwrap(), Some("下班".into()));
    }

    #[test]
    fn legacy_spec_json_without_slots_still_reads_as_one_unlabeled_slot() {
        let spec: Spec = serde_json::from_str(r#"{"mode":"daily","time":"09:00"}"#).unwrap();
        assert_eq!(spec, Spec::Daily { time: "09:00".into(), slots: vec![] });
        assert_eq!(next_slot_after(&spec, "UTC", utc(2026, 1, 1, 0, 0)).unwrap(), Some((utc(2026, 1, 1, 9, 0), String::new())));
        assert_eq!(slot_label_at(&spec, "UTC", utc(2026, 1, 1, 9, 0)).unwrap(), Some(String::new()));
    }

    #[test]
    fn slots_reject_duplicates_too_many_and_long_labels() {
        let dup = Spec::Daily { time: "08:30".into(), slots: vec![slot("08:30", "a"), slot("08:30:00", "b")] };
        assert!(validate_and_first(&dup, "UTC", utc(2026, 1, 1, 0, 0)).is_err());
        let many = Spec::Daily { time: "00:00".into(), slots: (0..=MAX_SLOTS).map(|i| slot(&format!("{:02}:00", i), "")).collect() };
        assert!(validate_and_first(&many, "UTC", utc(2026, 1, 1, 0, 0)).is_err());
        let long = Spec::Daily { time: "09:00".into(), slots: vec![slot("09:00", &"字".repeat(MAX_SLOT_LABEL_CHARS + 1))] };
        assert!(validate_and_first(&long, "UTC", utc(2026, 1, 1, 0, 0)).is_err());
    }

    fn range(time: &str, until: &str) -> Slot {
        Slot { time: time.into(), label: String::new(), until: Some(until.into()) }
    }
    fn range_spec(time: &str, until: &str) -> Spec {
        Spec::Daily { time: time.into(), slots: vec![range(time, until)] }
    }

    #[test]
    fn range_draw_stays_inside_the_window_and_covers_it_uniformly() {
        // 台北 08:55–09:00 = UTC 00:55–01:00。抽 2000 次：全落在區間內、整秒、頭尾都抽得到、分布大致均勻。
        let spec = range_spec("08:55", "09:00");
        let after = utc(2026, 11, 9, 0, 0);
        let lo = utc(2026, 11, 9, 0, 55).timestamp();
        let hi = utc(2026, 11, 9, 1, 0).timestamp();
        let mut buckets = [0u32; 5];
        let (mut saw_lo, mut saw_hi) = (false, false);
        for _ in 0..2000 {
            let (when, _) = next_slot_after(&spec, "Asia/Taipei", after).unwrap().unwrap();
            let t = when.timestamp();
            assert!((lo..=hi).contains(&t), "{when}");
            assert_eq!(when.timestamp_subsec_nanos(), 0);
            saw_lo |= t == lo;
            saw_hi |= t == hi;
            buckets[(((t - lo) * 5 / 301) as usize).min(4)] += 1;
        }
        assert!(saw_lo && saw_hi, "兩端都該抽得到");
        assert!(buckets.iter().all(|&c| c > 250), "五等分都該約 400 次：{buckets:?}");
    }

    #[test]
    fn range_pick_gets_the_whole_window_and_is_clamped() {
        let spec = range_spec("08:55", "09:00");
        let mut seen = None;
        let got = next_slot_after_with(&spec, "Asia/Taipei", utc(2026, 11, 9, 0, 0), &mut |lo, hi| {
            seen = Some((lo, hi));
            i64::MAX
        })
        .unwrap()
        .unwrap();
        assert_eq!(seen, Some((utc(2026, 11, 9, 0, 55).timestamp(), utc(2026, 11, 9, 1, 0).timestamp())));
        assert_eq!(got.0, utc(2026, 11, 9, 1, 0));
    }

    #[test]
    fn range_created_inside_the_window_only_draws_from_the_remainder() {
        let spec = range_spec("08:55", "09:00");
        let after = utc(2026, 11, 9, 0, 58);
        let mut seen = None;
        next_slot_after_with(&spec, "Asia/Taipei", after, &mut |lo, hi| {
            seen = Some((lo, hi));
            lo
        })
        .unwrap();
        assert_eq!(seen, Some((after.timestamp() + 1, utc(2026, 11, 9, 1, 0).timestamp())));
    }

    #[test]
    fn range_redraws_on_the_next_day_after_the_window_closes() {
        let spec = range_spec("08:55", "09:00");
        // 窗口結束後（含剛好結束那一秒）：下一次落在隔天的區間。
        for after in [utc(2026, 11, 9, 1, 0), utc(2026, 11, 9, 5, 0)] {
            let (when, _) = next_slot_after_with(&spec, "Asia/Taipei", after, &mut |lo, _| lo).unwrap().unwrap();
            assert_eq!(when, utc(2026, 11, 10, 0, 55));
        }
        // 兩天各抽各的：固定 picker 回不同值，結果跟著變，不會沿用前一天。
        let a = next_slot_after_with(&spec, "Asia/Taipei", utc(2026, 11, 9, 5, 0), &mut |lo, _| lo + 10).unwrap().unwrap().0;
        let b = next_slot_after_with(&spec, "Asia/Taipei", utc(2026, 11, 10, 5, 0), &mut |lo, _| lo + 20).unwrap().unwrap().0;
        assert_eq!((a, b), (utc(2026, 11, 10, 0, 55) + Duration::seconds(10), utc(2026, 11, 11, 0, 55) + Duration::seconds(20)));
    }

    #[test]
    fn slot_at_reports_window_end_for_a_drawn_time_and_nothing_outside() {
        let spec = range_spec("08:55", "09:00");
        let drawn = utc(2026, 11, 9, 0, 57);
        assert_eq!(slot_at(&spec, "Asia/Taipei", drawn).unwrap(), Some((String::new(), utc(2026, 11, 9, 1, 0))));
        assert_eq!(slot_at(&spec, "Asia/Taipei", utc(2026, 11, 9, 1, 1)).unwrap(), None);
    }

    #[test]
    fn range_validation_errors_carry_codes() {
        let code = |spec: &Spec| {
            validate_and_first(spec, "UTC", utc(2026, 1, 1, 0, 0)).unwrap_err().downcast_ref::<RangeError>().map(|e| e.code)
        };
        assert_eq!(code(&range_spec("09:00", "09:00")), Some("slot_range_not_after_start"));
        assert_eq!(code(&range_spec("09:00", "08:00")), Some("slot_range_not_after_start"));
        assert_eq!(code(&range_spec("09:00", "21:00:01")), Some("slot_range_too_long"));
        assert_eq!(code(&range_spec("09:00", "xx")), Some("slot_until_invalid"));
        // 剛好 12 小時可以；不跨午夜（結束只能是當天時間）。
        assert!(validate_and_first(&range_spec("09:00", "21:00"), "UTC", utc(2026, 1, 1, 0, 0)).is_ok());
        assert!(validate_and_first(&range_spec("00:00", "23:59:59"), "UTC", utc(2026, 1, 1, 0, 0)).is_err());
    }

    #[test]
    fn slots_without_until_behave_exactly_as_before_and_serialize_unchanged() {
        let spec = Spec::Daily { time: "09:00".into(), slots: vec![slot("09:00", "上班")] };
        assert_eq!(next_after(&spec, "UTC", utc(2026, 1, 1, 0, 0)).unwrap(), Some(utc(2026, 1, 1, 9, 0)));
        assert_eq!(slot_at(&spec, "UTC", utc(2026, 1, 1, 9, 0)).unwrap(), Some(("上班".into(), utc(2026, 1, 1, 9, 0))));
        assert!(!serde_json::to_string(&spec).unwrap().contains("until"));
    }

    #[test]
    fn range_mixes_with_fixed_slots_and_weekly_days() {
        // 週一（2026-11-09）：固定 08:30（台北），隨機 18:00–18:10。
        let spec = Spec::Weekly { time: "08:30".into(), weekdays: vec![0], slots: vec![slot("08:30", "上班"), range("18:00", "18:10")] };
        let (when, label) = next_slot_after_with(&spec, "Asia/Taipei", utc(2026, 11, 9, 2, 0), &mut |lo, _| lo).unwrap().unwrap();
        assert_eq!((when, label.as_str()), (utc(2026, 11, 9, 10, 0), ""));
        let (when, label) = next_slot_after_with(&spec, "Asia/Taipei", utc(2026, 11, 9, 10, 10), &mut |lo, _| lo).unwrap().unwrap();
        assert_eq!((when, label.as_str()), (utc(2026, 11, 16, 0, 30), "上班"));
    }
}
