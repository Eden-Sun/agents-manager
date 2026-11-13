//! 閒置 bot 的輸入框卡著一段沒送出的字（例如 claude「Session paused」選了「Edit prompt and retry」，CLI 把上一則放回框裡）：
//! daemon 在 idle 邊讀一次畫面、推 WS `composer_draft`，網頁收到才呼叫 `GET /bots/:id/composer-draft` 顯示草稿列。
//!
//! 不輪詢 pane：讀畫面只發生在 idle 邊（herdr 事件或 Session paused 選單還原）之後的 [`PROBES`] 幾個時間點，看到草稿或畫面變空就停。
//! 事件只帶 `bot_id`／`run_id`／`present`，**不帶草稿內容**（內容走 GET，受權限保護，也不進 WS 重播環）。
//! 草稿消失：回合開始（run 不再 idle）或之後的探測讀到空框，就推 `present:false`；使用者在終端自己清掉而 bot 一直閒置，
//! 要等下一次 idle 邊、或網頁動作回 `draft_gone` 才會知道（刻意不為這個去輪詢）。只讀、不按任何鍵。

use crate::db;
use crate::state::App;
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// idle 邊之後第幾毫秒讀一次：CLI 放回草稿、畫面畫完都要一點時間。
const PROBES: [u64; 3] = if cfg!(test) { [10, 20, 30] } else { [1500, 4000, 9000] };
/// 已推過 `present:true` 的 run；超過上限整張清掉（只是少推一次 `false`，網頁動作回 `draft_gone` 兜底）。
const ANNOUNCED_MAX: usize = 256;

fn announced() -> &'static Mutex<HashSet<String>> {
    static A: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    A.get_or_init(Default::default)
}

/// 讀一次：`Some(true)`＝框裡有字（且這個 kind 有可用動作），`Some(false)`＝空框／沒畫好，`None`＝讀不到（什麼都不動）。
async fn probe(app: &Arc<App>, run: &db::Run) -> Option<bool> {
    let bot = db::bot(&app.db, &run.bot_id).await.ok().flatten()?;
    if crate::lifecycle::composer_draft::actions(&bot.kind).is_empty() {
        return None;
    }
    let client = app.herdr_for_run(run).await?;
    let snap = crate::lifecycle::composer_draft::snapshot(&client, run, &bot).await.ok()?;
    match snap["state"].as_str()? {
        "draft" => Some(snap["draft_token"].is_string()),
        _ => Some(false),
    }
}

async fn announce(app: &Arc<App>, run: &db::Run, present: bool) {
    app.emit("composer_draft", json!({"bot_id": run.bot_id, "run_id": run.id, "present": present})).await;
}

/// AG Man／使用者的送字正在進行：這顆 bot 有 `queued`（等著排進框）或 `in_flight`（已建好、正在打或等回覆）的回合。
/// idle 邊同時會 `schedule_flush_queued`，daemon 剛打進框、還沒按 Enter 的字不是「卡住的草稿」，這時讀到的不能推；
/// 查不到（DB 出錯）當成有，不探測。
async fn delivery_in_progress(app: &Arc<App>, bot_id: &str) -> bool {
    let Ok(conv) = db::conversation_id(&app.db, bot_id).await else { return true };
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status IN ('queued','in_flight')")
        .bind(conv)
        .fetch_one(&app.db)
        .await
        .map(|n| n > 0)
        .unwrap_or(true)
}

/// 這個 run 回到 idle（herdr 事件，或 Session paused 選單還原成 idle）：背景探測，不擋呼叫端。
/// 送字進行中的那幾次跳過（不算探測次數）；沒推過、讀到空框最多再讀一次就停，省 pane 讀取。
pub fn on_idle(app: &Arc<App>, run: &db::Run) {
    let (app, run_id) = (app.clone(), run.id.clone());
    tokio::spawn(async move {
        let mut waited = 0;
        let mut empties = 0;
        for at in PROBES {
            tokio::time::sleep(Duration::from_millis(at - waited)).await;
            waited = at;
            // 事件帶來的 Run 是更新前的複本：每次重讀，狀態才是現在的。
            let Ok(Some(run)) = db::run(&app.db, &run_id).await else { return };
            if run.state != "running" {
                forget(&run_id);
                return;
            }
            if run.agent_status != "idle" {
                return; // 不再 idle：`on_not_idle` 負責收尾
            }
            if delivery_in_progress(&app, &run.bot_id).await {
                continue;
            }
            match probe(&app, &run).await {
                Some(true) => {
                    if mark_announced(&run_id) {
                        announce(&app, &run, true).await;
                    }
                    return;
                }
                Some(false) => {
                    if forget(&run_id) {
                        announce(&app, &run, false).await;
                        return;
                    }
                    empties += 1;
                    if empties >= 2 {
                        return;
                    }
                }
                None => {}
            }
        }
    });
}

/// run 不再 idle（回合開始、停在對話框）：框裡那段若推過 `present:true`，現在不是「卡住的草稿」了，推 `present:false`。不讀畫面。
pub fn on_not_idle(app: &Arc<App>, run: &db::Run) {
    if !forget(&run.id) {
        return;
    }
    let (app, run) = (app.clone(), run.clone());
    tokio::spawn(async move { announce(&app, &run, false).await });
}

/// 記下這個 run 推過 `present:true`（容量檢查與寫入同一把鎖）；回是不是第一次記（第一次才推）。
fn mark_announced(run_id: &str) -> bool {
    let mut a = announced().lock().unwrap();
    if a.len() >= ANNOUNCED_MAX && !a.contains(run_id) {
        a.clear();
    }
    a.insert(run_id.to_string())
}

/// 忘掉這個 run 的推送記錄；回之前有沒有推過。
fn forget(run_id: &str) -> bool {
    announced().lock().unwrap().remove(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use tokio::sync::broadcast::Receiver;

    async fn run_of(app: &Arc<App>, id: &str) -> db::Run {
        db::run(&app.db, id).await.unwrap().unwrap()
    }

    /// 收下之後 `wait_ms` 內所有 `composer_draft` 事件的 `present`。
    async fn drain(rx: &mut Receiver<crate::state::WsEvent>, wait_ms: u64) -> Vec<(bool, serde_json::Value)> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if ev.kind == "composer_draft" {
                out.push((ev.data["present"].as_bool().unwrap(), ev.data));
            }
        }
        out
    }

    fn pane(composer: Vec<String>) -> tt::LivePane {
        tt::LivePane { width: Some(120), boxed: true, composer, ..Default::default() }
    }

    /// idle 邊讀到框裡有字：推一次 `present:true`（只有 id，不帶草稿內容）；之後的探測不重推；一個鍵都不按。
    #[tokio::test]
    async fn an_idle_edge_with_a_stuck_draft_announces_it_once_without_the_text() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "draft-watch").await;
        let run_id = tt::fake_run(&e.app, &bot.id).await;
        e.herdr.live_pane(&format!("pane-{}", bot.id), pane(vec!["上一則 prompt（機密內容）".into()]));
        let mut rx = e.app.subscribe();
        on_idle(&e.app, &run_of(&e.app, &run_id).await);
        let got = drain(&mut rx, 400).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].0);
        assert_eq!(got[0].1["bot_id"], bot.id.as_str());
        assert_eq!(got[0].1["run_id"], run_id.as_str());
        assert!(!got[0].1.to_string().contains("機密"), "事件不帶草稿內容");
        for m in ["pane.send_keys", "pane.send_text", "agent.prompt"] {
            assert!(e.herdr.calls_to(m).is_empty(), "只讀、不按任何鍵：{m}");
        }
    }

    /// 空輸入框什麼都不推。
    #[tokio::test]
    async fn an_idle_edge_with_an_empty_box_announces_nothing() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "draft-watch-empty").await;
        let run_id = tt::fake_run(&e.app, &bot.id).await;
        e.herdr.live_pane(&format!("pane-{}", bot.id), pane(vec![]));
        let mut rx = e.app.subscribe();
        on_idle(&e.app, &run_of(&e.app, &run_id).await);
        assert!(drain(&mut rx, 300).await.is_empty());
    }

    /// 草稿消失：推過 `true` 之後，下一個 idle 邊讀到空框推 `present:false`；回合開始（不再 idle）也推，而且不讀畫面。
    #[tokio::test]
    async fn the_draft_going_away_is_announced() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "draft-watch-gone").await;
        let run_id = tt::fake_run(&e.app, &bot.id).await;
        let pane_id = format!("pane-{}", bot.id);
        e.herdr.live_pane(&pane_id, pane(vec!["草稿".into()]));
        let mut rx = e.app.subscribe();
        on_idle(&e.app, &run_of(&e.app, &run_id).await);
        assert_eq!(drain(&mut rx, 300).await.iter().map(|g| g.0).collect::<Vec<_>>(), vec![true]);

        // 使用者在終端清掉，下一個 idle 邊讀到空框。
        e.herdr.live_pane(&pane_id, pane(vec![]));
        on_idle(&e.app, &run_of(&e.app, &run_id).await);
        assert_eq!(drain(&mut rx, 300).await.iter().map(|g| g.0).collect::<Vec<_>>(), vec![false]);

        // 再來一輪：先有草稿 → true，回合開始（working）→ false，不用讀畫面。
        e.herdr.live_pane(&pane_id, pane(vec!["又一段".into()]));
        on_idle(&e.app, &run_of(&e.app, &run_id).await);
        assert_eq!(drain(&mut rx, 300).await.iter().map(|g| g.0).collect::<Vec<_>>(), vec![true]);
        let reads_before = e.herdr.pane(&pane_id).unwrap().reads;
        on_not_idle(&e.app, &run_of(&e.app, &run_id).await);
        assert_eq!(drain(&mut rx, 300).await.iter().map(|g| g.0).collect::<Vec<_>>(), vec![false]);
        assert_eq!(e.herdr.pane(&pane_id).unwrap().reads, reads_before, "回合開始只推事件，不讀 pane");
        // 沒推過 true 的 run，回合開始什麼都不推。
        on_not_idle(&e.app, &run_of(&e.app, &run_id).await);
        assert!(drain(&mut rx, 200).await.is_empty());
    }

    /// run 已經不是 idle（探測前就開始了新回合）：不讀、不推。
    #[tokio::test]
    async fn a_run_that_is_no_longer_idle_is_not_probed() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "draft-watch-busy").await;
        let run_id = tt::fake_run(&e.app, &bot.id).await;
        e.herdr.live_pane(&format!("pane-{}", bot.id), pane(vec!["草稿".into()]));
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();
        let mut rx = e.app.subscribe();
        on_idle(&e.app, &run_of(&e.app, &run_id).await);
        assert!(drain(&mut rx, 300).await.is_empty());
    }

    async fn insert_turn(app: &Arc<App>, bot_id: &str, run_id: &str, status: &str) -> String {
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        let id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web',?,'pending',?)")
            .bind(&id)
            .bind(&conv)
            .bind(run_id)
            .bind(status)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    /// AG Man／使用者的送字進行中（queued 或 in_flight 的回合）：idle 邊同時排佇列，框裡可能是 daemon 剛打進去、還沒按 Enter 的字，
    /// 不能推成「卡住的草稿」（使用者一按清除就動到正在送的訊息）。回合結束後的下一個 idle 邊才照常探測。
    #[tokio::test]
    async fn a_delivery_in_progress_is_never_announced_as_a_stuck_draft() {
        for status in ["queued", "in_flight"] {
            let e = tt::env().await;
            let bot = tt::claude_bot(&e.app, &e.project_id, &format!("draft-watch-{status}")).await;
            let run_id = tt::fake_run(&e.app, &bot.id).await;
            let pane_id = format!("pane-{}", bot.id);
            e.herdr.live_pane(&pane_id, pane(vec!["daemon 正在打的字".into()]));
            let turn = insert_turn(&e.app, &bot.id, &run_id, status).await;
            let mut rx = e.app.subscribe();
            on_idle(&e.app, &run_of(&e.app, &run_id).await);
            assert!(drain(&mut rx, 300).await.is_empty(), "{status}：送字中不推");
            assert_eq!(e.herdr.pane(&pane_id).unwrap().reads, 0, "{status}：送字中不讀 pane");
            // 回合結束後再來一個 idle 邊：那段字還在框裡，這時才是卡住的草稿。
            sqlx::query("UPDATE turns SET status=? WHERE id=?").bind(if status == "queued" { "failed" } else { "completed" }).bind(&turn).execute(&e.app.db).await.unwrap();
            on_idle(&e.app, &run_of(&e.app, &run_id).await);
            assert_eq!(drain(&mut rx, 300).await.iter().map(|g| g.0).collect::<Vec<_>>(), vec![true], "{status}");
        }
    }

    /// 空框、之前沒推過：最多讀兩次就停（不把三個探測點都讀完）。
    #[tokio::test]
    async fn an_empty_box_is_read_at_most_twice() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "draft-watch-reads").await;
        let run_id = tt::fake_run(&e.app, &bot.id).await;
        let pane_id = format!("pane-{}", bot.id);
        e.herdr.live_pane(&pane_id, pane(vec![]));
        // 一次探測花幾次 pane 讀取。
        let before = e.herdr.pane(&pane_id).unwrap().reads;
        assert_eq!(probe(&e.app, &run_of(&e.app, &run_id).await).await, Some(false));
        let per_probe = e.herdr.pane(&pane_id).unwrap().reads - before;
        assert!(per_probe > 0);
        let base = e.herdr.pane(&pane_id).unwrap().reads;
        on_idle(&e.app, &run_of(&e.app, &run_id).await);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(e.herdr.pane(&pane_id).unwrap().reads - base, per_probe * 2, "空框讀兩次就停");
    }
}
