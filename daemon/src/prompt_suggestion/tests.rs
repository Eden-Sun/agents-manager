//! 建議下一句的記憶帳、投影與讀取（畫面辨識本身的測試在 `lifecycle/delivery.rs`，一鍵送出在 `lifecycle/suggestion_tests.rs`）。

use super::*;
use crate::testing as tt;
use serde_json::json;

async fn idle_claude(env: &tt::Env, name: &str, suggestion: Option<&str>) -> (String, db::Run) {
    let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
    let run_id = tt::fake_run(&env.app, &bot.id).await;
    let run = db::run(&env.app.db, &run_id).await.unwrap().unwrap();
    env.herdr.live_pane(
        run.pane_id.as_deref().unwrap(),
        tt::LivePane { suggestion: suggestion.map(Into::into), width: Some(120), revision: 1, ..Default::default() },
    );
    (bot.id, run)
}

async fn run_json_of(env: &tt::Env, bot_id: &str) -> Value {
    let state = crate::api::state_json(&env.app).await.unwrap();
    for p in state["projects"].as_array().unwrap() {
        for b in p["bots"].as_array().unwrap() {
            if b["id"] == bot_id {
                return b["run"].clone();
            }
        }
    }
    panic!("bot not in state");
}

#[test]
fn the_store_reports_changes_and_forgets() {
    let id = format!("run-{}", db::ulid());
    assert!(!forget(&id), "沒有東西可忘：沒變");
    assert!(set(&id, Some("甲".into())));
    assert!(!set(&id, Some("甲".into())), "同一句：沒變");
    assert!(set(&id, Some("乙".into())), "換句：變了");
    assert_eq!(of(&id).as_deref(), Some("乙"));
    assert!(forget(&id));
    assert_eq!(of(&id), None);
    set(&id, Some("丙".into()));
    retain_runs(&["other".to_string()]);
    assert_eq!(of(&id), None, "run 結束（不在名單上）就清帳");
}

#[test]
fn the_projection_only_carries_it_while_idle() {
    let id = format!("run-{}", db::ulid());
    set(&id, Some("跑一次完整測試".into()));
    assert_eq!(json(&id, Some("idle")), json!("跑一次完整測試"));
    for status in [Some("working"), Some("blocked"), Some("unknown"), None] {
        assert_eq!(json(&id, status), Value::Null, "{status:?}：不是閒著，不帶");
    }
    forget(&id);
    assert_eq!(json(&id, Some("idle")), Value::Null);
}

/// 讀到樣式畫面上的灰字：記下、`/api/state` 與 `bot_status` 事件都帶；換句才推、一樣不推；框裡有使用者打的字就清掉。
#[tokio::test]
async fn observing_a_styled_screen_publishes_the_suggestion_and_follows_it() {
    let env = tt::env().await;
    let (bot_id, run) = idle_claude(&env, "sugg-observe", Some("跑一次完整測試")).await;
    let pane = run.pane_id.clone().unwrap();
    assert_eq!(run_json_of(&env, &bot_id).await["prompt_suggestion"], Value::Null, "還沒讀過：欄位在、值是 null");

    let mut rx = env.app.subscribe();
    assert_eq!(observe(&env.app, &run).await, Some(true));
    assert_eq!(run_json_of(&env, &bot_id).await["prompt_suggestion"], "跑一次完整測試");
    let frames: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).filter(|f| f.kind == "bot_status").collect();
    assert_eq!(frames.len(), 1, "變了才推一次");
    assert_eq!(frames[0].data["run"]["prompt_suggestion"], "跑一次完整測試");

    // 讀到同一句：不再推。
    assert_eq!(observe(&env.app, &run).await, Some(true));
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).all(|f| f.kind != "bot_status"), "同一句不重推");

    // 換了一句。
    env.herdr.live.lock().unwrap().get_mut(&pane).unwrap().suggestion = Some("再補一個測試".into());
    assert_eq!(observe(&env.app, &run).await, Some(true));
    assert_eq!(run_json_of(&env, &bot_id).await["prompt_suggestion"], "再補一個測試");

    // 使用者在終端打字：灰字沒了、框裡是實字 → 清掉。
    {
        let mut live = env.herdr.live.lock().unwrap();
        let p = live.get_mut(&pane).unwrap();
        p.suggestion = None;
        p.composer = vec!["我自己打的".into()];
    }
    assert_eq!(observe(&env.app, &run).await, Some(false));
    assert_eq!(run_json_of(&env, &bot_id).await["prompt_suggestion"], Value::Null);
}

/// 不是 claude、不是 idle：都是 null（純文字讀分不出灰字，畫面辨識的測試在 `lifecycle/delivery.rs`）。
#[tokio::test]
async fn it_is_null_for_other_kinds_and_busy_runs() {
    let env = tt::env().await;
    // codex：什麼都不讀。
    let (codex_bot, codex_run) = idle_claude(&env, "sugg-codex", Some("甲")).await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&codex_bot).execute(&env.app.db).await.unwrap();
    assert_eq!(observe(&env.app, &codex_run).await, Some(false));
    assert_eq!(of(&codex_run.id), None);

    // 回合中：不是 idle，忘掉。
    let (_, run) = idle_claude(&env, "sugg-busy", Some("乙")).await;
    assert_eq!(observe(&env.app, &run).await, Some(true));
    sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run.id).execute(&env.app.db).await.unwrap();
    let busy = db::run(&env.app.db, &run.id).await.unwrap().unwrap();
    assert_eq!(observe(&env.app, &busy).await, Some(false));
    assert_eq!(of(&run.id), None, "離開 idle 就忘掉");
}
