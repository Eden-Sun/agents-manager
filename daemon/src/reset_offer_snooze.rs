//! 重置券提示的「不再提醒」：每個額度 key（例 `m4p/codex:co0`）一個暫停到期時刻，存 `{data_dir}/reset-offer-snooze.json`。
//! 網頁對話框與推播共用這一份（跨裝置一致）。**只控制提醒，絕不兌換**。
//! 暫停中的 key：推播不推、也不記入已推紀錄，所以暫停結束後若仍是同一次耗盡、還有券，會再推一次。

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::db;

/// 使用者可選的暫停時長（小時）。其他值一律拒絕。
pub(crate) const SNOOZE_HOURS: [i64; 3] = [1, 24, 72];

fn path(dir: &Path) -> std::path::PathBuf {
    dir.join("reset-offer-snooze.json")
}

/// 讀出仍有效的暫停（過期或讀不懂的順手清掉，有清才寫回）。
pub(crate) fn load(dir: &Path, now: DateTime<Utc>) -> BTreeMap<String, String> {
    let all: BTreeMap<String, String> = std::fs::read_to_string(path(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let now_s = db::iso_at(now);
    let live: BTreeMap<String, String> = all.iter().filter(|(_, until)| db::cmp_ts(until, &now_s).is_gt()).map(|(k, v)| (k.clone(), v.clone())).collect();
    if live.len() != all.len() {
        let _ = crate::webpush::write_private(&path(dir), serde_json::to_string(&live).unwrap_or_default().as_bytes());
    }
    live
}

/// 這個額度 key 目前是否暫停中；是就回傳到期時刻（RFC3339）。
pub(crate) fn snoozed_until(dir: &Path, key: &str, now: DateTime<Utc>) -> Option<String> {
    load(dir, now).remove(key)
}

/// 設定暫停（覆寫舊的）。同時把這個 key 已記下的耗盡事件拿掉，讓暫停結束後同一次耗盡可以再推。
pub(crate) fn set(dir: &Path, key: &str, until: DateTime<Utc>, now: DateTime<Utc>) -> std::io::Result<String> {
    let mut live = load(dir, now);
    let until_s = db::iso_at(until);
    live.insert(key.to_string(), until_s.clone());
    crate::webpush::write_private(&path(dir), serde_json::to_string(&live).unwrap_or_default().as_bytes())?;
    forget_seen(dir, key);
    Ok(until_s)
}

/// 取消暫停。回傳原本是否有暫停。
pub(crate) fn clear(dir: &Path, key: &str, now: DateTime<Utc>) -> std::io::Result<bool> {
    let mut live = load(dir, now);
    if live.remove(key).is_none() {
        return Ok(false);
    }
    crate::webpush::write_private(&path(dir), serde_json::to_string(&live).unwrap_or_default().as_bytes())?;
    Ok(true)
}

/// 把快照每格標上 `reset_offer_snoozed_until`（沒有暫停就省略）。
pub(crate) fn annotate(dir: &Path, snap: &mut Value, now: DateTime<Utc>) {
    let live = load(dir, now);
    if live.is_empty() {
        return;
    }
    let Some(kinds) = snap.get_mut("kinds").and_then(Value::as_object_mut) else { return };
    for (key, until) in &live {
        if let Some(cell) = kinds.get_mut(key).and_then(Value::as_object_mut) {
            cell.insert("reset_offer_snoozed_until".into(), json!(until));
        }
    }
}

fn forget_seen(dir: &Path, key: &str) {
    let file = dir.join("push-reset-seen.json");
    let Some(mut seen) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<BTreeMap<String, i64>>(&t).ok()) else { return };
    let prefix = format!("{key}|");
    let before = seen.len();
    seen.retain(|id, _| !id.starts_with(&prefix));
    if seen.len() != before {
        let _ = crate::webpush::write_private(&file, serde_json::to_string(&seen).unwrap_or_default().as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn tmp() -> std::path::PathBuf {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-snooze-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn expired_snooze_is_pruned_and_live_one_is_reported() {
        let dir = tmp();
        set(&dir, "m4p/codex:co0", t("2026-11-19T11:00:00Z"), t("2026-11-19T10:00:00Z")).unwrap();
        assert_eq!(snoozed_until(&dir, "m4p/codex:co0", t("2026-11-19T10:30:00Z")).as_deref(), Some("2026-11-19T11:00:00.000Z"));
        assert_eq!(snoozed_until(&dir, "m4p/codex:co0", t("2026-11-19T11:00:00Z")), None, "到期即失效");
        assert!(load(&dir, t("2026-11-19T12:00:00Z")).is_empty(), "過期的已清掉");
    }

    #[test]
    fn clear_removes_and_reports_presence() {
        let dir = tmp();
        set(&dir, "codex", t("2026-11-22T10:00:00Z"), t("2026-11-19T10:00:00Z")).unwrap();
        assert!(clear(&dir, "codex", t("2026-11-19T10:00:00Z")).unwrap());
        assert!(!clear(&dir, "codex", t("2026-11-19T10:00:00Z")).unwrap());
    }
}
