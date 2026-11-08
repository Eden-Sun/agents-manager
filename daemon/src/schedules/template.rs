//! 排程 prompt 的變數展開（純函式，不碰 DB）。
//!
//! 送出時（排進 Bot 佇列前）才展開，用的是這次 occurrence 的觸發時間與排程時區；
//! 手動／試跑用按下的當下，時段標籤用下一個排定時段。未知的 `{{…}}` 原樣保留。

use chrono::{DateTime, Datelike, Utc, Weekday};
use chrono_tz::Tz;

/// 可用的變數名稱（編輯器也用同一份清單）。
pub const VARS: &[&str] = &["time", "date", "weekday", "name", "slot"];

/// 展開用的值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ctx {
    pub time: String,
    pub date: String,
    pub weekday: String,
    pub name: String,
    pub slot: String,
}

impl Ctx {
    /// `when` 換到排程時區之後的 HH:MM、YYYY-MM-DD、星期。
    pub fn new(tz: Tz, when: DateTime<Utc>, name: &str, slot: &str) -> Self {
        let local = when.with_timezone(&tz);
        Self {
            time: local.format("%H:%M").to_string(),
            date: local.format("%Y-%m-%d").to_string(),
            weekday: weekday_zh(local.weekday()).to_string(),
            name: name.to_string(),
            slot: slot.to_string(),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        Some(match key {
            "time" => &self.time,
            "date" => &self.date,
            "weekday" => &self.weekday,
            "name" => &self.name,
            "slot" => &self.slot,
            _ => return None,
        })
    }
}

fn weekday_zh(w: Weekday) -> &'static str {
    ["一", "二", "三", "四", "五", "六", "日"][w.num_days_from_monday() as usize]
}

/// 把 `{{name}}` 之類換成值；不認得的 `{{…}}` 原樣留下。
pub fn expand(tpl: &str, ctx: &Ctx) -> String {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after[..end];
        match ctx.get(key) {
            Some(v) => out.push_str(v),
            None => {
                out.push_str("{{");
                out.push_str(key);
                out.push_str("}}");
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx(slot: &str) -> Ctx {
        // 2026-11-09 00:30Z = 台北 2026-11-09（週一）08:30。
        let when = Utc.with_ymd_and_hms(2026, 11, 9, 0, 30, 0).unwrap();
        Ctx::new(chrono_tz::Asia::Taipei, when, "打卡", slot)
    }

    #[test]
    fn expands_known_variables_in_schedule_timezone() {
        let c = ctx("上班");
        assert_eq!(expand("{{date}} {{weekday}} {{time}} {{slot}}（{{name}}）", &c), "2026-11-09 一 08:30 上班（打卡）");
    }

    #[test]
    fn unknown_and_unclosed_placeholders_are_kept_verbatim() {
        let c = ctx("");
        assert_eq!(expand("a {{foo}} b {{ time }} c {{time", &c), "a {{foo}} b {{ time }} c {{time");
    }

    #[test]
    fn plain_text_without_placeholders_is_unchanged() {
        assert_eq!(expand("只是文字 }} {", &ctx("")), "只是文字 }} {");
    }
}
