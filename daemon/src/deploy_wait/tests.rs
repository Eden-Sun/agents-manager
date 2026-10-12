//! 部署等窗口超過 3 分鐘通知使用者（SPEC §18.10，使用者 2026-10-04）。釘住：3 分鐘門檻、狀態轉換才推 inbox、
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
        auto_escalated: false,
        notified_at: None,
        pending_first: None,
        pending_events: Vec::new(),
        dismissed: false,
        phase: Phase::Waiting,
        rev: 0,
        ended_at: None,
        swap_announced_at: BTreeMap::new(),
    };
    assert_eq!(evaluate(&mut w, None, "2026-10-04T12:02:59.000Z"), None, "2 分 59 秒還不通知");
    assert_eq!(evaluate(&mut w, None, "2026-10-04T12:03:00.000Z"), Some(Announce::First), "滿 3 分鐘通知");
    assert_eq!(evaluate(&mut w, None, "2026-10-04T12:03:15.000Z"), None, "同一次部署不再通知");
    assert_eq!(evaluate(&mut w, None, "2026-10-04T12:20:00.000Z"), None);
    assert_eq!(evaluate(&mut w, Some(Announce::Swapping), "2026-10-04T12:20:15.000Z"), Some(Announce::Swapping), "狀態轉換才更新");
    assert_eq!(w.rev, 2);
    let mut done = Wait { phase: Phase::Swapping, notified_at: None, rev: 0, ..w.clone() };
    assert_eq!(evaluate(&mut done, Some(Announce::Swapping), "2026-10-04T12:30:00.000Z"), None, "還沒通知就拿到窗口：不必告訴使用者");
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
    assert_eq!(rows[0].1["wake"], false, "只有 working 擋著、放寬已排定：自己會好，只記錄不叫醒（issue #1478）");

    // 一樣的人擋著、再試十次：不洗版。tick 也不重送。
    for _ in 0..10 {
        observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
        tick(app).await;
    }
    assert_eq!(inbox(app).await.len(), 1, "同一次部署只通知一次");

    // 換人擋：不單獨推 inbox（issue #856），但 web header / /api/state 看得到即時名單。
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b2", "bravo")]))).await;
    assert_eq!(inbox(app).await.len(), 1, "換人擋不推 inbox");
    assert_eq!(view(app)["blockers"][0]["name"], "bravo", "即時名單在 web header 對得上");

    // main 動了：換 commit、換核准，仍是同一次部署；拿到窗口為狀態轉換。
    let next = crate::swap_window::approval_for(app, OWNER, "1234567abcdef", &actor()).await.unwrap();
    observe(app, OWNER, &next, "1234567abcdef", Ok(false)).await;
    let rows = inbox(app).await;
    assert_eq!(rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), vec![
        format!("deploy_waiting:{first}:1"),
        format!("deploy_waiting:{first}:2"),
    ]);
    assert_eq!(rows[1].1["deploy_wait"]["phase"], "swapping");
    assert_eq!(rows[1].1["deploy_wait"]["commit"], "1234567abcdef");
    assert_eq!(rows[1].1["wake"], false, "拿到窗口等只記錄不叫醒");
    let w = current(app).unwrap();
    assert!(crate::supervisor::maintenance::waited_secs(&w.since, &crate::db::now()) >= 240, "換核准不歸零：{}", w.since);
}

#[tokio::test]
async fn a_failed_first_inbox_insert_is_retried_on_the_next_tick_with_the_same_key_and_wake() {
    let e = tt::env().await;
    let app = &e.app;
    let t3 = "2026-10-04T12:03:00.000Z";
    *app.deploy_wait.lock().unwrap() = Some(retry_wait());
    sqlx::query(
        "CREATE TRIGGER fail_first_deploy_waiting BEFORE INSERT ON supervisor_inbox
         WHEN NEW.kind = 'deploy_waiting'
         BEGIN SELECT RAISE(ABORT, 'injected transient inbox failure'); END",
    )
    .execute(&app.db)
    .await
    .unwrap();

    super::tick_at(app, t3).await;
    assert!(inbox(app).await.is_empty(), "第一次 insert 由 trigger 擋下");
    assert_eq!(current(app).unwrap().notified_at.as_deref(), Some(t3), "首次通知狀態已保存");
    assert!(load(app).unwrap().pending_first.is_some(), "失敗時保留待送通知於狀態檔");

    sqlx::query("DROP TRIGGER fail_first_deploy_waiting").execute(&app.db).await.unwrap();
    super::tick_at(app, t3).await;

    let rows = inbox(app).await;
    assert_eq!(rows.len(), 1, "下一拍補上，event key 冪等：{rows:?}");
    assert_eq!(rows[0].0, "deploy_waiting:retry-first-notice:1");
    assert_eq!(rows[0].1["wake"], true, "送達臨界區仍需叫醒巡檢");
    super::tick_at(app, t3).await;
    assert_eq!(inbox(app).await.len(), 1, "重複 tick 不會多插一筆");
}

/// restart-window 的拒絕，擋住的是 `delivering`（送達臨界區）而不是 working：放寬也放不行。
fn delivering(name: &str, escalates_at: Option<&str>) -> LcError {
    LcError::conflict(
        "a prompt is in the delivery critical section",
        json!({"reason": "not_idle", "escalates_at": escalates_at,
               "safety": {"working": [], "delivering": [{"bot_id": "d1", "name": name}], "unreadable": [], "held_leases": []}}),
    )
}

/// issue #1478：首次通知只在「自己不會好」時叫醒巡檢。
#[tokio::test]
async fn the_first_notice_wakes_agm_only_when_waiting_will_not_clear_itself() {
    // 擋住的只有 working、放寬已排定 → 不叫醒（上面那條測試的情境）；inbox 照記、WS 照發。
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 1, "inbox 照記：{rows:?}");
    assert_eq!(rows[0].1["wake"], false);
    assert!(view(app)["blockers"][0]["name"] == "alpha", "web header 照常看得到");

    // 送達臨界區擋著：放寬不放行 → 叫醒。
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&delivering("dora", Some("2026-10-04T12:30:00.000Z")))).await;
    assert_eq!(inbox(app).await[0].1["wake"], true, "送達中擋著要人看");

    // working 擋著、但沒有排定放寬（不知道何時會好）→ 叫醒。
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    let no_eta = LcError::conflict(
        "something is still running; wait for a safe window",
        json!({"reason": "not_idle", "escalates_at": null,
               "safety": {"working": [{"bot_id": "b1", "name": "alpha"}], "delivering": [], "unreadable": [], "held_leases": []}}),
    );
    observe(app, OWNER, &id, SHA, Err(&no_eta)).await;
    assert_eq!(inbox(app).await[0].1["wake"], true, "沒有排定放寬時仍叫醒");

    // 擋住名單是 working ＋ 別人的租約：租約放寬不掉 → 叫醒。
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    let leased = LcError::conflict(
        "a lease is held",
        json!({"reason": "not_idle", "escalates_at": "2026-10-04T12:30:00.000Z",
               "safety": {"working": [{"bot_id": "b1", "name": "alpha"}], "delivering": [], "unreadable": [],
                          "held_leases": [{"owner": "someone", "own": false}]}}),
    );
    observe(app, OWNER, &id, SHA, Err(&leased)).await;
    assert_eq!(inbox(app).await[0].1["wake"], true, "別人的租約擋著要人看");
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
    observe(app, OWNER, &id, SHA, Ok(false)).await;
    assert_eq!(current(app).unwrap().phase, Phase::Swapping);
    assert_eq!(esc(id.clone()).await, Some(true), "拿到窗口後複查：放寬還在");
    // 窗口交還、下一輪又沒拿到：還是同一次部署，放寬照舊。
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    assert_eq!(esc(id.clone()).await, Some(true), "同一次部署重試：放寬還在");

    // 這次部署換好了：下一次部署從頭算。
    observe(app, OWNER, &id, SHA, Ok(false)).await;
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
    observe(app, OWNER, &id, SHA, Ok(false)).await;
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
async fn save_replaces_the_file_atomically() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    let mut w = current(app).unwrap();
    w.rev = 7;
    save(app, Some(&w));
    w.rev = 8;
    save(app, Some(&w));
    let back: Wait = serde_json::from_slice(&std::fs::read(path(app)).unwrap()).unwrap();
    assert_eq!(back.rev, 8, "讀回的是第二份");
    let leftovers: Vec<_> = std::fs::read_dir(&app.data_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".deploy-wait.json.") && n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "不留暫存檔：{leftovers:?}");
}

#[tokio::test]
async fn a_corrupt_state_file_is_kept_aside_and_logged_not_silently_dropped() {
    let e = tt::env().await;
    let app = &e.app;
    let half = br#"{"commit":"abc"#;
    std::fs::write(path(app), half).unwrap();
    startup_as(app, "abc").await;
    assert!(!path(app).exists(), "壞檔不留在原名");
    let aside: Vec<_> = std::fs::read_dir(&app.data_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("deploy-wait.json.corrupt-"))
        .collect();
    assert_eq!(aside.len(), 1, "改名保留證據");
    assert_eq!(std::fs::read(aside[0].path()).unwrap(), half, "內容等於原本那份半份");
    assert!(current(app).is_none());
}

#[tokio::test]
async fn an_unreadable_state_file_is_not_overwritten() {
    let e = tt::env().await;
    let app = &e.app;
    std::fs::create_dir(path(app)).unwrap(); // 讀會 EISDIR
    startup_as(app, "abc").await;
    assert!(path(app).is_dir(), "讀不了的不動它");
    assert!(current(app).is_none());
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
    observe(app, OWNER, &id, SHA, Ok(false)).await;
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

/// 「拿到窗口→3a ABORT→交還」循環同一 sha 最多每 30 分鐘一則（issue #856）。
#[tokio::test]
async fn abort_cycle_is_throttled_to_at_most_once_per_thirty_minutes() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    assert_eq!(inbox(app).await.len(), 1, "初次通知已等 4 分鐘");
    let wid = current(app).unwrap().id;

    // 第一次拿到窗口：推 inbox (swapping)，wake: false
    observe(app, OWNER, &id, SHA, Ok(false)).await;
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].0, format!("deploy_waiting:{wid}:2"));
    assert_eq!(rows[1].1["deploy_wait"]["phase"], "swapping");
    assert_eq!(rows[1].1["wake"], false);

    // 3a ABORT 交還：observe 回報 Err，回到 waiting，不推 inbox
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    assert_eq!(current(app).unwrap().phase, Phase::Waiting);
    assert_eq!(inbox(app).await.len(), 2, "3a ABORT 交還不推 inbox");

    // 30 分鐘內重試又拿到窗口：循環節流，不推 inbox
    observe(app, OWNER, &id, SHA, Ok(false)).await;
    assert_eq!(current(app).unwrap().phase, Phase::Swapping);
    assert_eq!(inbox(app).await.len(), 2, "30 分鐘內再次拿到窗口被節流，不推 inbox");

    // 又交還：不推 inbox
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    assert_eq!(inbox(app).await.len(), 2);

    // 30 分鐘過去了（把這顆 sha 的時間撥早）：再次拿到窗口就可以推
    app.deploy_wait.lock().unwrap().as_mut().unwrap().swap_announced_at.insert(SHA.into(), crate::db::iso_in(-1801));
    observe(app, OWNER, &id, SHA, Ok(false)).await;
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 3, "滿 30 分鐘後可以再次發出 swapping 通知");
    assert_eq!(rows[2].0, format!("deploy_waiting:{wid}:3"));
    assert_eq!(rows[2].1["deploy_wait"]["phase"], "swapping");
    assert_eq!(rows[2].1["wake"], false);

    // 換到另一顆 sha 可立即記錄；再回到剛通知過的 sha，仍受它自己的 30 分鐘節流。
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    let other_sha = "1234567abcdef";
    observe(app, OWNER, &id, other_sha, Ok(false)).await;
    assert_eq!(inbox(app).await.len(), 4, "新 sha 不受前一顆的節流影響");
    observe(app, OWNER, &id, other_sha, Err(&not_idle(&[("b1", "alpha")]))).await;
    observe(app, OWNER, &id, SHA, Ok(false)).await;
    assert_eq!(inbox(app).await.len(), 4, "換過其他 sha 也不能洗掉原 sha 的節流");
}

/// 自動放寬生效只推一則，且不設 wake（issue #856）。
#[tokio::test]
async fn auto_escalation_notifies_once_without_wake() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    assert_eq!(inbox(app).await.len(), 1, "初次通知已發出");

    // 模擬已過門檻（escalated: true, escalates_at: null）
    let escalated_err = LcError::conflict(
        "something is still running; wait for a safe window",
        json!({"reason": "not_idle", "escalates_at": null,
               "safety": {"working": [{"bot_id": "b1", "name": "alpha", "is_supervisor": false}],
                          "escalated": true, "delivering": [], "unreadable": [], "held_leases": []}}),
    );
    // §3a race 回覆沒有 safety 快照，不能把 ETA 消失誤判成自動放寬。
    let raced_err = LcError::conflict(
        "a prompt entered the delivery critical section while this window was being taken",
        json!({"reason": "not_idle", "raced": true}),
    );
    observe(app, OWNER, &id, SHA, Err(&raced_err)).await;
    let before_escalation = current(app).unwrap();
    assert!(!before_escalation.auto_escalated);
    assert_eq!(before_escalation.blockers[0].name, "alpha", "沒有 safety 快照時保留最後一份名單");
    assert_eq!(before_escalation.escalates_at.as_deref(), Some("2026-10-04T12:30:00.000Z"), "沒有 safety 快照時保留 ETA");
    assert_eq!(inbox(app).await.len(), 1, "race 回覆不是狀態轉換");

    observe(app, OWNER, &id, SHA, Err(&escalated_err)).await;
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 2, "自動放寬生效推一則");
    assert_eq!(rows[1].1["deploy_wait"]["auto_escalated"], true);
    assert_eq!(rows[1].1["wake"], false, "只記錄類的 deploy_waiting 不設 wake");
    let summary = rows[1].1["text"].as_str().unwrap();
    assert!(summary.contains("已自動放寬"), "{summary}");

    // 再試多次不重複推
    for _ in 0..5 {
        observe(app, OWNER, &id, SHA, Err(&escalated_err)).await;
    }
    assert_eq!(inbox(app).await.len(), 2, "自動放寬生效不重複推");

    // 如果第一次觀察到放寬時已經拿到窗口，成功回應的 safety 也要記進狀態。
    observe(app, OWNER, &id, SHA, Ok(true)).await;
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 3, "拿到窗口是下一個狀態轉換");
    assert_eq!(rows[2].1["deploy_wait"]["phase"], "swapping");
    assert_eq!(rows[2].1["deploy_wait"]["auto_escalated"], true);
    assert!(rows[2].1["text"].as_str().unwrap().contains("已自動放寬"));
    assert_eq!(rows[2].1["wake"], false);
}

/// 使用者調度（現在換版、先等）推 inbox 記錄但不設 wake（issue #856）。
#[tokio::test]
async fn user_dispatch_actions_push_records_without_wake() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    assert_eq!(inbox(app).await.len(), 1);

    let wid = current(app).unwrap().id;
    // 使用者按「現在換版」
    escalate(app, &wid).await.unwrap();
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].1["deploy_wait"]["user_escalated"], true);
    assert_eq!(rows[1].1["wake"], false, "使用者操作不叫醒巡檢");

    // 使用者按「先等」
    act(app, &wid, |w| w.dismissed = true).await.unwrap();
    let rows = inbox(app).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].1["deploy_wait"]["dismissed"], true);
    assert_eq!(rows[2].1["wake"], false, "先等記錄不叫醒巡檢");
}

fn retry_wait() -> Wait {
    Wait {
        id: "retry-first-notice".into(),
        owner: OWNER.into(),
        approval_id: "approval".into(),
        commit: SHA.into(),
        since: "2026-10-04T12:00:00.000Z".into(),
        last_attempt_at: "2026-10-04T12:03:00.000Z".into(),
        blockers: vec![Blocker { bot_id: Some("delivering".into()), name: "alpha".into(), why: "delivering".into() }],
        escalates_at: None,
        user_escalated: false,
        auto_escalated: false,
        notified_at: None,
        pending_first: None,
        pending_events: Vec::new(),
        dismissed: false,
        phase: Phase::Waiting,
        rev: 0,
        ended_at: None,
        swap_announced_at: BTreeMap::new(),
    }
}

#[tokio::test]
async fn pending_first_survives_reload_and_keeps_its_payload_even_after_state_changes() {
    for wake in [false, true] {
        let e = tt::env().await;
        let app = &e.app;
        let mut w = retry_wait();
        if !wake {
            w.blockers[0].why = "working".into();
            w.escalates_at = Some("2026-10-04T12:05:00.000Z".into());
        }
        assert_eq!(evaluate(&mut w, None, "2026-10-04T12:03:00.000Z"), Some(Announce::First));
        let pending = w.pending_first.clone().unwrap();
        assert_eq!(pending.payload["wake"], wake);
        save(app, Some(&w));
        let mut reloaded = load(app).unwrap();
        assert_eq!(reloaded.pending_first.as_ref(), Some(&pending));
        reloaded.rev = 2;
        reloaded.blockers.clear();
        reloaded.user_escalated = true;
        *app.deploy_wait.lock().unwrap() = Some(reloaded);
        super::tick_at(app, "2026-10-04T12:03:15.000Z").await;
        let rows = inbox(app).await;
        assert_eq!(rows, vec![("deploy_waiting:retry-first-notice:1".into(), pending.payload.clone())]);
        assert!(current(app).unwrap().pending_first.is_none());
        assert!(load(app).unwrap().pending_first.is_none(), "成功才將清除狀態落盤");

        // insert 成功、清 pending 前重啟：同 key 已存在仍算送達，不產生第二筆。
        save(app, Some(&w));
        *app.deploy_wait.lock().unwrap() = load(app);
        super::tick_at(app, "2026-10-04T12:03:15.000Z").await;
        assert_eq!(inbox(app).await, rows);
        assert!(load(app).unwrap().pending_first.is_none());
    }
}

#[tokio::test]
async fn legacy_state_without_pending_first_loads_without_reannouncing() {
    let e = tt::env().await;
    let app = &e.app;
    let mut w = retry_wait();
    w.notified_at = Some("2026-10-04T12:03:00.000Z".into());
    w.rev = 1;
    let mut legacy = serde_json::to_value(&w).unwrap();
    legacy.as_object_mut().unwrap().remove("pending_first");
    std::fs::write(path(app), serde_json::to_vec(&legacy).unwrap()).unwrap();
    *app.deploy_wait.lock().unwrap() = load(app);
    assert_eq!(current(app).unwrap(), w);
    super::tick_at(app, "2026-10-04T12:03:15.000Z").await;
    assert!(inbox(app).await.is_empty(), "舊檔沒有 pending，不重新叫醒巡檢");
}

/// 把 deploy_waiting 的 inbox 寫入全部擋下（暫時性 DB 錯誤，issue #1507）。
async fn fail_deploy_waiting_inserts(app: &Arc<App>) {
    sqlx::query(
        "CREATE TRIGGER fail_later_deploy_waiting BEFORE INSERT ON supervisor_inbox
         WHEN NEW.kind = 'deploy_waiting'
         BEGIN SELECT RAISE(ABORT, 'injected transient inbox failure'); END",
    )
    .execute(&app.db)
    .await
    .unwrap();
}

async fn heal_deploy_waiting_inserts(app: &Arc<App>) {
    sqlx::query("DROP TRIGGER fail_later_deploy_waiting").execute(&app.db).await.unwrap();
}

/// 狀態檔裡還排著的待送通知 event key（落盤的，重啟後也在）。
fn pending_keys(app: &Arc<App>) -> Vec<String> {
    load(app).map(|w| w.pending_events.iter().map(|p| p.key.clone()).collect()).unwrap_or_default()
}

/// issue #1507：首次以後的通知（使用者操作、換版完成、放棄）inbox 寫入失敗，也要排隊、照原本的 event key 與內容補送：
/// 不回滾已成功的操作、不因為等待記錄過期或中間重啟就消失，補上之後恰好一筆、重複 tick 不再多插。
#[tokio::test]
async fn a_failed_later_notice_is_queued_and_resent_with_its_own_rev_and_payload() {
    // 等待記錄已經過期（結束後超過 10 分鐘）：有待送的就不能清掉。
    let much_later = "2099-01-01T00:00:00.000Z";

    // 使用者按「現在換版」，inbox 寫入失敗。
    {
        let e = tt::env().await;
        let app = &e.app;
        let id = approval_waiting(app, SHA, 240).await;
        observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
        let wid = current(app).unwrap().id;
        fail_deploy_waiting_inserts(app).await;
        escalate(app, &wid).await.unwrap();
        assert!(current(app).unwrap().user_escalated, "成功的使用者操作照樣生效，不回滾");
        assert_eq!(inbox(app).await.len(), 1, "寫入失敗：inbox 還沒有");
        assert_eq!(pending_keys(app), vec![format!("deploy_waiting:{wid}:2")], "待送通知落盤");
        let outage_now = current(app).unwrap().last_attempt_at;
        super::tick_at(app, &outage_now).await;
        assert_eq!(pending_keys(app).len(), 1, "tick 重送還是失敗就保留");

        heal_deploy_waiting_inserts(app).await;
        *app.deploy_wait.lock().unwrap() = load(app);
        super::tick_at(app, &outage_now).await;
        let rows = inbox(app).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[1].0, format!("deploy_waiting:{wid}:2"), "同一個 event key");
        assert_eq!(rows[1].1["deploy_wait"]["rev"], 2);
        assert_eq!(rows[1].1["deploy_wait"]["user_escalated"], true, "內容是當時的狀態");
        assert_eq!(rows[1].1["wake"], false, "使用者操作不叫醒巡檢");
        assert!(pending_keys(app).is_empty(), "送達才清");
        super::tick_at(app, &outage_now).await;
        assert_eq!(inbox(app).await.len(), 2, "重複 tick 不會多插一筆");
    }

    // 換版完成（新 daemon 開機時寫的 Done），inbox 寫入失敗，之後還重啟了一次。
    {
        let e = tt::env().await;
        let app = &e.app;
        let id = approval_waiting(app, SHA, 240).await;
        observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
        let wid = current(app).unwrap().id;
        observe(app, OWNER, &id, SHA, Ok(false)).await;
        assert_eq!(inbox(app).await.len(), 2, "拿到窗口那則先正常送出");
        fail_deploy_waiting_inserts(app).await;
        *app.deploy_wait.lock().unwrap() = None;
        startup_as(app, &SHA[..7]).await;
        assert_eq!(current(app).unwrap().phase, Phase::Done);
        assert_eq!(pending_keys(app), vec![format!("deploy_waiting:{wid}:3")]);
        super::tick_at(app, much_later).await;
        assert!(current(app).is_some(), "還有待送的，等待記錄不能因為過期被清掉");
        assert_eq!(pending_keys(app).len(), 1);

        heal_deploy_waiting_inserts(app).await;
        *app.deploy_wait.lock().unwrap() = load(app);
        super::tick_at(app, much_later).await;
        let rows = inbox(app).await;
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[2].0, format!("deploy_waiting:{wid}:3"));
        assert_eq!(rows[2].1["deploy_wait"]["phase"], "done");
        assert_eq!(rows[2].1["deploy_wait"]["rev"], 3);
        assert!(pending_keys(app).is_empty());
        assert!(current(app).is_none(), "送達後、過期的結束記錄照常清掉");
    }

    // 沒有人再試而放棄，inbox 寫入失敗。
    {
        let e = tt::env().await;
        let app = &e.app;
        let id = approval_waiting(app, SHA, 240).await;
        observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
        let wid = current(app).unwrap().id;
        app.deploy_wait.lock().unwrap().as_mut().unwrap().last_attempt_at = "2026-10-04T12:00:00.000Z".into();
        fail_deploy_waiting_inserts(app).await;
        super::tick_at(app, "2026-10-04T13:00:00.000Z").await;
        assert_eq!(pending_keys(app), vec![format!("deploy_waiting:{wid}:2")], "放棄的那則排隊");
        assert_eq!(inbox(app).await.len(), 1);
        super::tick_at(app, much_later).await;
        assert!(current(app).is_some(), "待送的放棄通知還在，不清");

        heal_deploy_waiting_inserts(app).await;
        super::tick_at(app, much_later).await;
        let rows = inbox(app).await;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[1].0, format!("deploy_waiting:{wid}:2"));
        assert_eq!(rows[1].1["deploy_wait"]["phase"], "abandoned");
        assert!(pending_keys(app).is_empty());
        assert!(current(app).is_none());
    }
}

/// issue #1507：前一則還沒送進 inbox 時，後面的狀態轉換不越過它先送，也不拿較新的狀態代替它。
#[tokio::test]
async fn a_later_notice_does_not_overtake_one_that_failed() {
    let e = tt::env().await;
    let app = &e.app;
    let id = approval_waiting(app, SHA, 240).await;
    observe(app, OWNER, &id, SHA, Err(&not_idle(&[("b1", "alpha")]))).await;
    let wid = current(app).unwrap().id;
    fail_deploy_waiting_inserts(app).await;
    escalate(app, &wid).await.unwrap();
    observe(app, OWNER, &id, SHA, Ok(false)).await;
    assert_eq!(pending_keys(app), vec![format!("deploy_waiting:{wid}:2"), format!("deploy_waiting:{wid}:3")]);

    heal_deploy_waiting_inserts(app).await;
    let now = current(app).unwrap().last_attempt_at;
    super::tick_at(app, &now).await;
    let rows = inbox(app).await;
    assert_eq!(rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), vec![
        format!("deploy_waiting:{wid}:1"),
        format!("deploy_waiting:{wid}:2"),
        format!("deploy_waiting:{wid}:3"),
    ]);
    assert_eq!(rows[1].1["deploy_wait"]["phase"], "waiting", "rev 2 送的是當時的內容");
    assert_eq!(rows[1].1["deploy_wait"]["user_escalated"], true);
    assert_eq!(rows[2].1["deploy_wait"]["phase"], "swapping");
}
