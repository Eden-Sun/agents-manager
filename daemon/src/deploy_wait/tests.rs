//! 部署等窗口超過 3 分鐘通知使用者（SPEC §18.10，使用者 2026-10-04）。釘住：3 分鐘門檻、同一次部署只通知一次（變化才更新）、
//! 「現在換版」只放寬這一次部署的自動核准。

use super::*;
use crate::testing as tt;

const OWNER: &str = "daemon-update-kick";
const SHA: &str = "abcdef1234567";

fn actor() -> String {
    format!("service({})", crate::service_auth::DAEMON_SWAP)
}

/// 自動部署的核准，等待起點撥到 `ago` 秒前。
async fn approval_waiting(app: &Arc<App>, commit: &str, ago: i64) -> String {
    let id = crate::swap_window::approval_for(app, OWNER, commit, &actor()).await.unwrap();
    sqlx::query("UPDATE supervisor_approvals SET decided_at = ? WHERE id = ?").bind(crate::db::iso_in(-ago)).bind(&id).execute(&app.db).await.unwrap();
    id
}

/// restart-window 回的 `not_idle`（形狀同 `maintenance::acquire`）。
fn not_idle(working: &[(&str, &str)]) -> LcError {
    let w: Vec<Value> = working.iter().map(|(id, name)| json!({"bot_id": id, "name": name, "is_supervisor": false})).collect();
    LcError::conflict(
        "something is still running; wait for a safe window",
        json!({"reason": "not_idle", "escalates_at": "2026-10-04T12:30:00.000Z",
               "safety": {"working": w, "delivering": [], "unreadable": [], "held_leases": []}}),
    )
}

async fn inbox(app: &Arc<App>) -> Vec<(String, Value)> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT event_key, payload_json FROM supervisor_inbox WHERE kind = 'deploy_waiting' ORDER BY rowid")
        .fetch_all(&app.db)
        .await
        .unwrap();
    rows.into_iter().map(|(k, p)| (k, serde_json::from_str(&p).unwrap())).collect()
}

fn current(app: &App) -> Option<Wait> {
    app.deploy_wait.lock().unwrap().clone()
}

#[test]
fn the_notice_goes_out_at_three_minutes_and_only_changes_update_it() {
    let t0 = "2026-10-04T12:00:00.000Z";
    let mut w = Wait {
        id: "w".into(),
        owner: OWNER.into(),
        approval_id: "a".into(),
        commit: SHA.into(),
        since: t0.into(),
        last_attempt_at: t0.into(),
        blockers: vec![],
        escalates_at: None,
        user_escalated: false,
        notified_at: None,
        dismissed: false,
        phase: Phase::Waiting,
        rev: 0,
        ended_at: None,
    };
    assert_eq!(evaluate(&mut w, true, "2026-10-04T12:02:59.000Z"), None, "2 分 59 秒還不通知，有變化也不算");
    assert_eq!(evaluate(&mut w, false, "2026-10-04T12:03:00.000Z"), Some(Announce::First), "滿 3 分鐘通知");
    assert_eq!(evaluate(&mut w, false, "2026-10-04T12:03:15.000Z"), None, "同一次部署不再通知");
    assert_eq!(evaluate(&mut w, false, "2026-10-04T12:20:00.000Z"), None);
    assert_eq!(evaluate(&mut w, true, "2026-10-04T12:20:15.000Z"), Some(Announce::Update), "有變化才更新");
    assert_eq!(w.rev, 2);
    let mut done = Wait { phase: Phase::Swapping, notified_at: None, rev: 0, ..w.clone() };
    assert_eq!(evaluate(&mut done, true, "2026-10-04T12:30:00.000Z"), None, "還沒通知就拿到窗口：不必告訴使用者");
}

#[tokio::test]
async fn a_wait_under_three_minutes_says_nothing_and_one_over_says_it_once() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 120).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    assert!(inbox(app).await.is_empty(), "2 分鐘：不通知");
    assert_eq!(view(app), Value::Null, "/api/state 也還沒有");
    let first = current(app).unwrap().id;

    // 同一張核准等到 4 分鐘（把起點撥早），下一次試窗口就通知。
    sqlx::query("UPDATE supervisor_approvals SET decided_at = ? WHERE id = ?").bind(crate::db::iso_in(-240)).bind(&id).execute(&app.db).await.unwrap();
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, format!("deploy_waiting:{first}:1"), "同一次部署、同一個 id");
    let text = rows[0].1["text"].as_str().unwrap();
    assert!(text.contains("abcdef12") && text.contains("alpha") && text.contains("4 分鐘") && text.contains("2026-10-04T12:30:00.000Z"), "{text}");
    assert_eq!(view(app)["blockers"][0]["name"], "alpha");

    // 一樣的人擋著、再試十次：不洗版。tick 也不重送。
    for _ in 0..10 {
        observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
        tick(app).await;
    }
    assert_eq!(inbox(app).await.len(), 1, "同一次部署只通知一次");

    // 換人擋：更新同一則（同一個 id、rev 遞增）。
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b2", "bravo")]))).await;
    // main 動了：換 commit、換核准，仍是同一次部署。
    let next = crate::swap_window::approval_for(app, OWNER, "1234567abcdef", &actor()).await.unwrap();
    observe(app, OWNER, &next, "1234567abcdef", Ok(())).await;
    let rows = inbox(app).await;
    assert_eq!(rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), vec![
        format!("deploy_waiting:{first}:1"),
        format!("deploy_waiting:{first}:2"),
        format!("deploy_waiting:{first}:3"),
    ]);
    assert_eq!(rows[1].1["deploy_wait"]["blockers"][0]["name"], "bravo");
    assert_eq!(rows[2].1["deploy_wait"]["phase"], "swapping");
    assert_eq!(rows[2].1["deploy_wait"]["commit"], "1234567abcdef");
    let w = current(app).unwrap();
    assert!(crate::supervisor::maintenance::waited_secs(&w.since, &crate::db::now()) >= 240, "換核准不歸零：{}", w.since);
}

#[tokio::test]
async fn now_please_relaxes_only_this_deployment() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    let w = current(app).unwrap();
    let esc = |aid: String| {
        let app = app.clone();
        async move { crate::supervisor::maintenance::escalation_for(&app, Some(&aid)).await.unwrap().map(|e| e.escalated) }
    };
    assert_eq!(esc(id.clone()).await, Some(false), "4 分鐘：還沒放寬");
    assert!(matches!(escalate(app, "not-this-one").await, Err(LcError::NotFound(_))));
    let v = escalate(app, &w.id).await.unwrap();
    assert_eq!(v["user_escalated"], true);
    assert_eq!(esc(id.clone()).await, Some(true), "按了「現在換版」：這次部署的核准放寬");

    // 別人的核准（AGM 親手核的、別的 owner）不跟著放寬。
    let theirs = crate::supervisor::store::create_approval(&app.db, "ops", "restart", "AGM 核的", Some("aaaaaaa1"), Some(&crate::db::iso_in(3600)), None).await.unwrap().approval;
    crate::supervisor::store::decide_approval_from(&app.db, &theirs.id, "pending", "approved", "AGM", None, None).await.unwrap();
    assert_eq!(esc(theirs.id.clone()).await, Some(false), "別人的核准不受影響");

    // 拿到窗口、還在換（Swapping）：daemon-swap 換 binary 前用同一張核准複查（§3a），放寬不能在這裡消失（issue #840）。
    observe(app, OWNER, &id, SHA, Ok(())).await;
    assert_eq!(current(app).unwrap().phase, Phase::Swapping);
    assert_eq!(esc(id.clone()).await, Some(true), "拿到窗口後複查：放寬還在");
    // 窗口交還、下一輪又沒拿到：還是同一次部署，放寬照舊。
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    assert_eq!(esc(id.clone()).await, Some(true), "同一次部署重試：放寬還在");

    // 這次部署換好了：下一次部署從頭算。
    observe(app, OWNER, &id, SHA, Ok(())).await;
    app.deploy_wait.lock().unwrap().as_mut().unwrap().phase = Phase::Done;
    assert_eq!(esc(id.clone()).await, Some(false), "這次部署結束，放寬跟著結束");
    app.deploy_wait.lock().unwrap().as_mut().unwrap().last_attempt_at = crate::db::iso_in(-(STALE_SECS + 1));
    let fresh = approval_waiting(app, "fedcba9876543", 10).await;
    observe(app, OWNER, &fresh, "fedcba9876543", Err(&not_idle(&[("b1", "alpha")]))).await;
    let n = current(app).unwrap();
    assert_ne!(n.id, w.id, "新的一次部署");
    assert!(!n.user_escalated);
    assert_eq!(esc(fresh).await, Some(false), "下一次部署不繼承「現在換版」");
}

#[tokio::test]
async fn wait_for_now_hides_the_notice_without_relaxing_anything() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    let w = current(app).unwrap();
    let v = act(app, &w.id, |w| w.dismissed = true).await.unwrap();
    assert_eq!(v["dismissed"], true);
    assert_eq!(view(app)["dismissed"], true, "/api/state 對得上");
    assert_eq!(crate::supervisor::maintenance::escalation_for(app, Some(&id)).await.unwrap().map(|e| e.escalated), Some(false));
}

#[tokio::test]
async fn the_new_daemon_reports_whether_the_swap_landed() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    observe(app, OWNER, &id, SHA, Ok(())).await;
    let wid = current(app).unwrap().id;
    // 換版＝重啟：新 daemon 只看得到檔案。
    *app.deploy_wait.lock().unwrap() = None;
    startup_as(app, &SHA[..7]).await;
    let w = current(app).unwrap();
    assert_eq!((w.id.as_str(), w.phase), (wid.as_str(), Phase::Done));
    assert_eq!(inbox(app).await.last().unwrap().1["deploy_wait"]["phase"], "done");

    // 回滾：跑的不是要上的那顆。
    let mut s = w.clone();
    s.phase = Phase::Swapping;
    save(app, Some(&s));
    *app.deploy_wait.lock().unwrap() = None;
    startup_as(app, "0000000").await;
    assert_eq!(current(app).unwrap().phase, Phase::Abandoned);
}

#[tokio::test]
async fn a_wait_nobody_retries_is_given_up_and_says_so_once() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    app.deploy_wait.lock().unwrap().as_mut().unwrap().last_attempt_at = crate::db::iso_in(-(STALE_SECS + 1));
    tick(app).await;
    tick(app).await;
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].1["deploy_wait"]["phase"], "abandoned");
}

/// issue #840 實況（2026-10-04 06:28／06:32／06:37）：使用者按了「現在換版」，restart 窗口每次都拿到，
/// daemon-swap 換 binary 前的 §3a 複查卻又因為有人 working 而 ABORT——拿到窗口後 phase 變成 `Swapping`，放寬跟著不見。
/// 複查用同一張核准、同一個 owner 問 safety，要跟 acquire 給出同一個答案；送達臨界區照樣擋。
#[tokio::test]
async fn the_recheck_after_taking_the_window_keeps_the_users_go_ahead() {
    let e = tt::env().await;
    let app = &e.app;
    let now = crate::db::now();
    // 一顆正在思考的 bot（working、turn 已送達）：全靜止模式會擋。
    sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('busy',?,'busy','claude','tok-busy',?)")
        .bind(&e.project_id).bind(&now).execute(&app.db).await.unwrap();
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('busy','busy','running','working',?)")
        .bind(&now).execute(&app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('busy','busy',?)").bind(&now).execute(&app.db).await.unwrap();
    sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('busy','busy','busy','web','in_flight','ok',?)")
        .bind(&now).execute(&app.db).await.unwrap();

    let id = approval_waiting(app, SHA, 240).await;
    let recheck = |app: Arc<App>, id: String| async move {
        crate::supervisor::maintenance::safety_as(&app, &[], Some(&id), Some(OWNER)).await.unwrap()
    };
    // 沒放寬：working 照樣擋，窗口拿不到。
    let refused = crate::supervisor::maintenance::acquire(app, "restart", OWNER, &id, Some(SHA), 900, true, &[]).await.unwrap_err();
    observe(app, OWNER, &id, SHA, Err(&refused)).await;
    assert_eq!(recheck(app.clone(), id.clone()).await["safe"], false, "沒放寬時 working 照擋");

    // 使用者按「現在換版」→ 拿到窗口（phase 變 Swapping）→ 複查：還是安全的。
    escalate(app, &current(app).unwrap().id).await.unwrap();
    crate::supervisor::maintenance::acquire(app, "restart", OWNER, &id, Some(SHA), 900, true, &[]).await.unwrap();
    observe(app, OWNER, &id, SHA, Ok(())).await;
    assert_eq!(current(app).unwrap().phase, Phase::Swapping);
    let s = recheck(app.clone(), id.clone()).await;
    assert_eq!((s["safe"].as_bool(), s["escalated"].as_bool()), (Some(true), Some(true)), "複查不能推翻 acquire 給的放寬：{s}");
    assert_eq!(s["working"][0]["bot_id"], "busy", "working 還在，只是不擋");

    // 送達臨界區照擋：daemon 正在往 pane 打字。
    sqlx::query("UPDATE turns SET delivery='pending' WHERE id='busy'").execute(&app.db).await.unwrap();
    let s = recheck(app.clone(), id.clone()).await;
    assert_eq!(s["safe"], false, "放寬了，送達臨界區仍然要擋：{s}");
    assert_eq!(s["delivering"][0]["bot_id"], "busy");
}
