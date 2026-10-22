//! codex 非同步提問（`request_user_input_async`，issue #1879）：畫面只剩 `? 1 question · shift+← to answer`，
//! 題目與選項從 rollout 讀（[`crate::codex_async_question`]）。**只看、一個鍵都不按**：作答由網頁一步一步送、每步重讀畫面。
//!
//! 兩個用途：`blocked_reason`（側欄與「需要回應」寫出題目）與 `GET /api/bots/{id}/pending-question`（對話區顯示題目與選項）。
//! 遠端 rollout 經既有 SSH 通道限時、限量讀取；讀不到照舊只寫題數。

use crate::codex_async_question::{async_screen, pending_async_questions, resolve, title_in, AsyncQuestion, AsyncScreen, TAIL_BYTES};
use crate::db;
use crate::state::App;
use std::sync::Arc;

/// 讀畫面＋rollout 的結果。
pub struct Snapshot {
    pub screen: AsyncScreen,
    /// 畫面上排著的那幾題；讀不到 rollout（連線失敗、還沒寫），或 rollout 與畫面對不上（有題目被跳過、session 不一致）
    /// 是空陣列——降級成只講題數，不猜（issue #1879 審查）。
    pub questions: Vec<AsyncQuestion>,
}

/// 這個 codex run 畫面上有沒有非同步提問；沒有、讀不到畫面都是 `None`（讀不到不等於關了，呼叫端自己決定要不要清）。
pub async fn snapshot(app: &Arc<App>, bot: &db::Bot, run: &db::Run) -> Option<Option<Snapshot>> {
    if bot.kind != "codex" {
        return None;
    }
    let pane = run.pane_id.as_deref().filter(|p| !p.trim().is_empty())?;
    let client = app.herdr_for_run(run).await?;
    // 200 行：收合時要在對話紀錄裡找題目原文，分辨哪幾題還排著。
    let mut read = client.pane_read(pane, "visible", 200).await.ok()?;
    let Some(mut screen) = async_screen(&read.text) else { return Some(None) };
    // session 以 herdr 當下綁在這顆 pane 上的為準（/new、resume 後 DB 的 native_session_id 可能還是舊的）；
    // herdr 沒回報才退回 DB，那時每一題的原文都必須在畫面上看得到，否則可能讀到舊 session 的題目，降級。
    let live = live_session(&client, pane).await;
    let session = live.clone().or_else(|| run.native_session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string));
    let mut pending = match session {
        Some(session) => pending_for(app, bot, run, &session).await.unwrap_or_default(),
        None => Vec::new(),
    };
    if db::bot_host(&app.db, &bot.id).await.ok()? != crate::config::LOCAL_HOST {
        // SSH can take time: resolve against the screen after the read, never an old queue count.
        read = client.pane_read(pane, "visible", 200).await.ok()?;
        let Some(now) = async_screen(&read.text) else { return Some(None) };
        screen = now;
        if live_session(&client, pane).await != live {
            pending.clear();
        }
    }
    let mut questions = resolve(pending, &screen, &read.text);
    if live.is_none() && !questions.iter().all(|q| title_in(&q.title, &read.text)) {
        questions.clear();
    }
    Some(Some(Snapshot { screen, questions }))
}

/// herdr 當下綁在這顆 pane 上的 codex session id（沒綁、不是 codex、pane 對不上＝`None`）。
pub(super) async fn live_session(client: &crate::herdr::HerdrClient, pane: &str) -> Option<String> {
    let agent = client.agent_get(pane).await.ok().flatten()?;
    if agent.pane_id != pane || agent.agent.as_deref().is_some_and(|a| a != "codex") {
        return None;
    }
    crate::lifecycle::dead_panes::herdr_session_id(&agent)
}

/// 這個 codex run 的 rollout 裡還沒作答的題目；找不到、遠端逾時回 `None`。
async fn pending_for(app: &Arc<App>, bot: &db::Bot, run: &db::Run, session: &str) -> Option<Vec<AsyncQuestion>> {
    let host = db::bot_host(&app.db, &bot.id).await.ok()?;
    if host != crate::config::LOCAL_HOST {
        return crate::runners::codex_async_question_remote::pending(app, bot, run, &host, session).await;
    }
    let home = crate::lifecycle::codex_home(app, bot).await?;
    let path = crate::lifecycle::codex_session_log_async(home, session.to_string()).await?;
    let log = tokio::task::spawn_blocking(move || crate::transcript_read::read_tail(&path, TAIL_BYTES)).await.ok().flatten()?;
    tokio::task::spawn_blocking(move || pending_async_questions(&log)).await.ok()
}

/// 給人看的一句話（側欄「需要回應」旁的小字）：第一題的題目，多題寫題數。
pub fn reason_text(snap: &Snapshot) -> String {
    let Some(first) = snap.questions.first() else {
        let n = snap.screen.count;
        return format!("codex 有 {n} 題非同步提問等你回答（終端按 shift+← 展開）");
    };
    let mut title: String = first.title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.chars().count() > 80 {
        title = format!("{}…", title.chars().take(80).collect::<String>());
    }
    match snap.questions.len() {
        1 => format!("codex 提問等你回答：{title}"),
        n => format!("codex 提問等你回答（共 {n} 題）：{title}"),
    }
}

/// blocked 的 codex run：讀一次畫面，有非同步提問就把題目記進 `blocked_reason`，沒有就清掉；有變才推 `bot_status`。
pub async fn observe(app: &Arc<App>, run: &db::Run) {
    if run.agent_status != "blocked" {
        return;
    }
    let Ok(Some(bot)) = db::bot(&app.db, &run.bot_id).await else { return };
    if bot.kind != "codex" { return; }
    if db::bot_host(&app.db, &bot.id).await.ok().is_some_and(|h| h != crate::config::LOCAL_HOST) {
        crate::runners::codex_async_question_remote::observe(app, &bot, run);
        return;
    }
    observe_now(app, &bot, run).await;
}

pub(super) async fn observe_now(app: &Arc<App>, bot: &db::Bot, run: &db::Run) {
    let Some(seen) = snapshot(app, bot, run).await else { return };
    // SSH may have yielded while the run resumed or ended; do not restore an obsolete reason.
    let Ok(Some(current)) = db::run(&app.db, &run.id).await else { return };
    if current.agent_status != "blocked" || current.state != run.state || current.pane_id != run.pane_id || current.started_at != run.started_at {
        return;
    }
    let changed = crate::blocked_reason::set_codex_async(&run.id, seen.as_ref().map(reason_text));
    if changed {
        app.emit_bot_status(&run.bot_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use serde_json::json;

    const COLLAPSED: &str = "• Working (39s • esc to interrupt)\n• Queued follow-up inputs\n  ? 1 question · 7s\n    shift+← to answer\n› Ask Codex to do anything\n";

    async fn blocked_codex(env: &tt::Env, name: &str, pane: &str) -> (db::Bot, db::Run) {
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET pane_id=?, agent_status='blocked' WHERE id=?").bind(pane).bind(&run_id).execute(&env.app.db).await.unwrap();
        (db::bot(&env.app.db, &bot.id).await.unwrap().unwrap(), db::run(&env.app.db, &run_id).await.unwrap().unwrap())
    }

    async fn reason(env: &tt::Env, bot_id: &str) -> serde_json::Value {
        let state = crate::api::state_json(&env.app).await.unwrap();
        state["projects"].as_array().unwrap().iter().flat_map(|p| p["bots"].as_array().unwrap().clone()).find(|b| b["id"] == bot_id).unwrap()["run"]["blocked_reason"].clone()
    }

    /// synthetic：沒有 rollout（遠端／還沒寫）時也要說「有提問」，不能讓使用者什麼都看不到；一個鍵都不按。
    #[tokio::test]
    async fn the_collapsed_prompt_without_a_rollout_still_says_there_is_a_question() {
        let env = tt::env().await;
        let (bot, run) = blocked_codex(&env, "async-q", "pane-async-q").await;
        env.herdr.set_screen("pane-async-q", COLLAPSED);
        observe(&env.app, &run).await;
        let r = reason(&env, &bot.id).await;
        assert_eq!(r["code"], "codex_async_question");
        assert!(r["text"].as_str().unwrap().contains("1 題非同步提問"), "{r}");
        assert!(env.herdr.calls_to("pane.send_keys").is_empty() && env.herdr.calls_to("pane.send_text").is_empty());

        // 畫面上不再有提問（作答了、跳過了）：原因清掉。
        env.herdr.set_screen("pane-async-q", "› Ask Codex to do anything\n");
        observe(&env.app, &run).await;
        assert_eq!(reason(&env, &bot.id).await, json!(null));
    }

    /// 不是 blocked 的 run、不是 codex 的 bot 都不動。
    #[tokio::test]
    async fn only_blocked_codex_runs_are_observed() {
        let env = tt::env().await;
        let (bot, mut run) = blocked_codex(&env, "async-q2", "pane-async-q2").await;
        env.herdr.set_screen("pane-async-q2", COLLAPSED);
        run.agent_status = "working".into();
        observe(&env.app, &run).await;
        assert_eq!(reason(&env, &bot.id).await, json!(null));

        let claude = tt::claude_bot(&env.app, &env.project_id, "async-q3").await;
        let claude_run = tt::fake_run(&env.app, &claude.id).await;
        sqlx::query("UPDATE runs SET pane_id='pane-async-q3', agent_status='blocked' WHERE id=?").bind(&claude_run).execute(&env.app.db).await.unwrap();
        env.herdr.set_screen("pane-async-q3", COLLAPSED);
        let claude_run = db::run(&env.app.db, &claude_run).await.unwrap().unwrap();
        observe(&env.app, &claude_run).await;
        assert_eq!(reason(&env, &claude.id).await, json!(null));
    }

    #[test]
    fn the_reason_names_the_question() {
        let q = |title: &str| AsyncQuestion { call_id: "c".into(), index: 0, title: title.into(), options: vec![] };
        let screen = AsyncScreen { expanded: false, count: 2 };
        let one = Snapshot { screen: AsyncScreen { expanded: false, count: 1 }, questions: vec![q("你說的\n選單是哪個？")] };
        assert_eq!(reason_text(&one), "codex 提問等你回答：你說的 選單是哪個？");
        let two = Snapshot { screen, questions: vec![q("第一題？"), q("第二題？")] };
        assert_eq!(reason_text(&two), "codex 提問等你回答（共 2 題）：第一題？");
        let long = Snapshot { screen, questions: vec![q(&"長".repeat(120))] };
        assert!(reason_text(&long).ends_with('…'));
    }
}
