//! 母 bot 在 `herdr agent list` 時看得到自己底下每顆子 agent 的 prompt cache 還熱多久（使用者 2026-10-04：
//! 「child 優先選擇有在 cache 時間內的做，以節省 token 開銷」）。
//!
//! herdr 的清單只有 `idle`／`done`，看不出誰的 cache 還在；daemon 有 [`cache_clock`] 的 `last_api_at`。
//! shim 在 `agent list` 之後打 `POST /relay/kids`，這裡回一段純文字（shim 原樣印到 stderr，不動 herdr 的 JSON）：
//! 閒置且 cache 熱的排最前、剩最久的先，接著跑著的，最後冷掉或不明的。

use crate::cache_clock::{self, LastTurn};
use crate::db;
use crate::state::App;
use std::sync::Arc;

/// 一顆子 agent 的 cache 狀態（純資料，方便測排序與文字）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kid {
    pub name: String,
    /// `idle`／`working`／`blocked`／`unknown`；沒有 run＝`stopped`。
    pub status: String,
    /// cache 還剩幾秒；`None`＝不知道（grok、沒有任何活動紀錄）；≤ 0＝已冷。
    pub warm_secs: Option<i64>,
    /// 閒置多久（秒）；只在冷掉時用來顯示。
    pub idle_secs: Option<i64>,
}

fn rank(k: &Kid) -> (u8, i64) {
    match (k.status.as_str(), k.warm_secs) {
        ("idle", Some(s)) if s > 0 => (0, -s),
        ("working" | "blocked", _) => (1, 0),
        ("idle", _) => (2, 0),
        _ => (3, 0),
    }
}

fn mins(secs: i64) -> i64 {
    (secs + 59) / 60
}

/// 給母 bot 看的文字；沒有子 agent 回空字串（shim 就不印）。
pub fn render(mut kids: Vec<Kid>) -> String {
    if kids.is_empty() {
        return String::new();
    }
    kids.sort_by_key(rank);
    let mut out = String::from("agents-manager: 你底下的子 agent 的 prompt cache（claude／codex 約 60 分）——派工先用 cache 還熱的閒置 child：\n");
    for k in &kids {
        let state = match (k.status.as_str(), k.warm_secs) {
            ("working", _) => "跑著（cache 熱）".to_string(),
            ("blocked", _) => "停在提示上".to_string(),
            ("stopped", _) => "沒在跑".to_string(),
            (_, Some(s)) if s > 0 => format!("cache 熱，還剩約 {} 分", mins(s)),
            (_, Some(_)) => match k.idle_secs {
                Some(i) => format!("cache 已冷（閒置 {} 分）", i / 60),
                None => "cache 已冷".to_string(),
            },
            (_, None) => "cache 不明".to_string(),
        };
        out.push_str(&format!("  {}  {}  {}\n", k.name, k.status, state));
    }
    out
}

/// 子 agent 的 run 上推 cache 要的幾個欄位。
pub struct RunView<'a> {
    pub id: &'a str,
    pub agent_status: &'a str,
    pub status_since: Option<&'a str>,
}

/// 由 run 的狀態與最後回合推一顆子 agent 的 cache 狀態；沒有 run＝`stopped`。
pub fn kid(
    name: String,
    kind: &str,
    run: Option<RunView<'_>>,
    last_turn: Option<&LastTurn>,
    now: chrono::DateTime<chrono::Utc>,
) -> Kid {
    let Some(run) = run else {
        return Kid { name, status: "stopped".into(), warm_secs: None, idle_secs: None };
    };
    let line = cache_clock::statusline_at(run.id);
    let at = cache_clock::derive(kind, Some(run.agent_status), run.status_since, last_turn, line.as_deref(), &db::iso_at(now));
    let idle = at.as_deref().and_then(db::parse_ts).map(|t| (now - t).num_seconds().max(0));
    let warm = match (cache_clock::ttl_secs(kind), idle) {
        (Some(ttl), Some(i)) => Some(ttl - i),
        _ => None,
    };
    Kid { name, status: run.agent_status.to_string(), warm_secs: warm, idle_secs: idle }
}

/// `bot_id` 底下（`parent_bot_id`）每顆活著的子 agent。
pub async fn text_for(app: &Arc<App>, bot_id: &str) -> anyhow::Result<String> {
    let bots = db::live_bots(&app.db).await?;
    let mut runs = db::active_runs_by_bot(&app.db).await?;
    let last_turns = cache_clock::last_turns_by_bot(&app.db).await?;
    let now = chrono::Utc::now();
    let kids = bots
        .iter()
        .filter(|b| b.parent_bot_id.as_deref() == Some(bot_id))
        .map(|b| {
            let run = runs.remove(&b.id);
            let name = run.as_ref().and_then(|r| r.agent_name.clone()).unwrap_or_else(|| b.name.clone());
            let view = run.as_ref().map(|r| RunView {
                id: &r.id,
                agent_status: &r.agent_status,
                status_since: r.agent_status_since.as_deref(),
            });
            kid(name, &b.kind, view, last_turns.get(&b.id), now)
        })
        .collect();
    Ok(render(kids))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(name: &str, status: &str, warm: Option<i64>, idle: Option<i64>) -> Kid {
        Kid { name: name.into(), status: status.into(), warm_secs: warm, idle_secs: idle }
    }

    #[test]
    fn warm_idle_children_come_first_longest_remaining_first() {
        let text = render(vec![
            k("p-cold", "idle", Some(-60), Some(3660)),
            k("p-run", "working", Some(3600), Some(0)),
            k("p-warm10", "idle", Some(600), Some(3000)),
            k("p-warm50", "idle", Some(3000), Some(600)),
            k("p-off", "stopped", None, None),
        ]);
        let order: Vec<&str> = text.lines().skip(1).map(|l| l.split_whitespace().next().unwrap()).collect();
        assert_eq!(order, ["p-warm50", "p-warm10", "p-run", "p-cold", "p-off"]);
        assert!(text.contains("p-warm50  idle  cache 熱，還剩約 50 分"), "{text}");
        assert!(text.contains("p-cold  idle  cache 已冷（閒置 61 分）"), "{text}");
    }

    #[test]
    fn no_children_prints_nothing() {
        assert_eq!(render(vec![]), "");
    }

    #[test]
    fn a_child_is_warm_for_an_hour_after_its_last_turn() {
        let now = db::parse_ts("2026-10-04T12:00:00.000Z").unwrap();
        let run = || Some(RunView { id: "r-kids-cache-test", agent_status: "idle", status_since: None });
        let turn = LastTurn { status: "done".into(), completed_at: Some("2026-10-04T11:40:00.000Z".into()) };
        let kid = kid("c".into(), "claude", run(), Some(&turn), now);
        assert_eq!(kid.warm_secs, Some(2400));
        assert_eq!(kid.idle_secs, Some(1200));
        // grok 不知道 TTL。
        assert_eq!(super::kid("g".into(), "grok", run(), Some(&turn), now).warm_secs, None);
    }
}
