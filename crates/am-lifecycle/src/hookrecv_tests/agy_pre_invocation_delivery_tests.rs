use super::*;
use crate::testing as tt;

struct TestFixture {
    env: tt::Env,
    bot_id: String,
    run_id: String,
    turn_id: String,
    conv_id: String,
    delivered_at: String,
}

async fn setup_fixture(prompt: &str, delivery: &str, verified: i64, origin: &str) -> TestFixture {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,?,'agy','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(format!("agy-{}", &bot_id[bot_id.len() - 8..]))
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    let conv_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    let turn_id = db::ulid();
    let delivered_at = "2026-10-24T14:00:00.000Z".to_string();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, delivery_verified, auto_resend, resend_count, prompt_text, delivered_at, created_at)
         VALUES (?,?,?,?,'in_flight',?,?,0,0,?,?,?)",
    )
    .bind(&turn_id)
    .bind(&conv_id)
    .bind(&run_id)
    .bind(origin)
    .bind(delivery)
    .bind(verified)
    .bind(prompt)
    .bind(&delivered_at)
    .bind("2026-10-24T13:59:59.000Z")
    .execute(&app.db)
    .await
    .unwrap();

    TestFixture {
        env,
        bot_id,
        run_id,
        turn_id,
        conv_id,
        delivered_at,
    }
}

fn make_body(bot_id: &str, raw_fixture: &str, received_at: Option<&str>, truncated: bool) -> HookBody {
    let mut p: Value = serde_json::from_str(raw_fixture).unwrap();
    p["hookEventName"] = json!("PreInvocation");
    HookBody {
        bot_id: bot_id.into(),
        provider: "agy".into(),
        payload: p,
        received_at: received_at.map(str::to_string),
        truncated,
        run_id: None,
    }
}

// 沿用 #1950 實機 PreInvocation stdin 原文 fixture
const RAW_PRE_FIXTURE: &str = r#"{"artifactDirectoryPath":"/tmp/d/.gemini/antigravity-cli/brain/c-1","conversationId":"d721abb5-bb6b-48ad-a655-24bc51e375a4","initialNumSteps":1,"invocationNum":0,"lastUserInput":"test_last_user_input_ping","modelName":"gemini-3.8-flash-low","transcriptPath":"/tmp/d/.gemini/antigravity-cli/brain/c-1/.system_generated/logs/transcript_full.jsonl","workspacePaths":["/tmp/d"]}"#;

#[tokio::test]
async fn pre_invocation_verifies_delivery_when_strictly_matched() {
    let f = setup_fixture("test_last_user_input_ping", "ok", 0, "web").await;
    let body = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);

    process(&f.env.app, &body).await.unwrap();

    let (verified, auto_resend, resend_count): (i64, i64, i64) = sqlx::query_as(
        "SELECT delivery_verified, auto_resend, resend_count FROM turns WHERE id = ?",
    )
    .bind(&f.turn_id)
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();

    assert_eq!(verified, 1, "嚴格符合時補記 delivery_verified=1");
    assert_eq!(auto_resend, 0, "auto_resend 維持不動");
    assert_eq!(resend_count, 0, "resend_count 維持不動");
}

#[tokio::test]
async fn pre_invocation_supports_crlf_lf_and_trim_normalization() {
    // CRLF 與 LF 正規化及 trim
    let f = setup_fixture("  hello\r\nworld  ", "ok", 0, "web").await;
    let mut raw: Value = serde_json::from_str(RAW_PRE_FIXTURE).unwrap();
    raw["lastUserInput"] = json!("hello\nworld");
    let body = HookBody {
        bot_id: f.bot_id.clone(),
        provider: "agy".into(),
        payload: {
            raw["hookEventName"] = json!("PreInvocation");
            raw
        },
        received_at: Some("2026-10-24T14:00:02.000Z".into()),
        truncated: false,
        run_id: None,
    };

    process(&f.env.app, &body).await.unwrap();

    let verified: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(verified, 1, "CRLF/LF 與 trim 正規化後全等應通過驗證");
}

#[tokio::test]
async fn pre_invocation_rejects_substring_and_loose_match() {
    // synthetic: 長子字串不可視為匹配
    let f = setup_fixture("test_last_user_input_ping and more text", "ok", 0, "web").await;
    let body = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);

    process(&f.env.app, &body).await.unwrap();

    let verified: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(verified, 0, "長子字串不能當作匹配");
}

#[tokio::test]
async fn pre_invocation_guards_time_bounds() {
    let f = setup_fixture("test_last_user_input_ping", "ok", 0, "web").await;
    // synthetic: 比 delivered_at 早的舊事件晚到
    let before_deliv = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T13:59:59.000Z"), false);
    process(&f.env.app, &before_deliv).await.unwrap();
    let verified: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(verified, 0, "早於 delivered_at 的事件不能補驗證");

    // synthetic: 未來超過 5 秒的時間
    let future_time = (chrono::Utc::now() + chrono::Duration::seconds(10)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let future_body = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some(&future_time), false);
    process(&f.env.app, &future_body).await.unwrap();
    let verified2: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(verified2, 0, "超過 now + 5s 的未來事件不能補驗證");
}

#[tokio::test]
async fn pre_invocation_ignores_when_missing_input_or_truncated() {
    let f = setup_fixture("test_last_user_input_ping", "ok", 0, "web").await;
    // synthetic: 缺少 lastUserInput 欄位
    let mut no_input: Value = serde_json::from_str(RAW_PRE_FIXTURE).unwrap();
    no_input.as_object_mut().unwrap().remove("lastUserInput");
    let body_no_input = HookBody {
        bot_id: f.bot_id.clone(),
        provider: "agy".into(),
        payload: {
            no_input["hookEventName"] = json!("PreInvocation");
            no_input
        },
        received_at: Some("2026-10-24T14:00:01.000Z".into()),
        truncated: false,
        run_id: None,
    };
    process(&f.env.app, &body_no_input).await.unwrap();
    let verified: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(verified, 0, "缺少 lastUserInput 時不變");

    // synthetic: payload 被截斷
    let truncated_body = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), true);
    process(&f.env.app, &truncated_body).await.unwrap();
    let verified2: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(verified2, 0, "truncated 為 true 時不補驗證");
}

#[tokio::test]
async fn pre_invocation_ignores_external_turns() {
    let f = setup_fixture("test_last_user_input_ping", "ok", 0, "external").await;
    let body = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);

    process(&f.env.app, &body).await.unwrap();

    let verified: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(verified, 0, "external turn 不補送達驗證");
}

// AGM 新增必驗 ①：與 late_proof::promote 互斥與兩種順序驗證
#[tokio::test]
async fn late_proof_promote_and_hook_order_isolation() {
    // 順序 A: helper 面對 unknown 先略過 → promote(Unverified) 轉為 delivery='ok' → hook 補 verified=1
    let f1 = setup_fixture("test_last_user_input_ping", "unknown", 0, "web").await;
    let body1 = make_body(&f1.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);

    let initial_turns_count1: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id = ?").bind(&f1.run_id).fetch_one(&f1.env.app.db).await.unwrap();

    // helper 先遇到 unknown：略過不改
    process(&f1.env.app, &body1).await.unwrap();
    let (deliv1, ver1, resend1): (String, i64, i64) = sqlx::query_as("SELECT delivery, delivery_verified, auto_resend FROM turns WHERE id = ?")
        .bind(&f1.turn_id)
        .fetch_one(&f1.env.app.db)
        .await
        .unwrap();
    assert_eq!((deliv1.as_str(), ver1, resend1), ("unknown", 0, 0), "helper 只比對 delivery='ok'，unknown 略過");

    // 正式 late_proof::promote 轉成 Unverified (delivery='ok', delivery_verified=0, auto_resend=0)
    let promoted1 = crate::lifecycle::late_proof::promote(&f1.env.app, &f1.turn_id, crate::lifecycle::delivery::Delivered::Unverified).await.unwrap();
    assert!(promoted1, "unknown 狀態應被成功 promote");

    // hook 再到：補上 verified=1
    process(&f1.env.app, &body1).await.unwrap();
    let (deliv1_after, ver1_after, auto_resend1, count1): (String, i64, i64, i64) = sqlx::query_as(
        "SELECT delivery, delivery_verified, auto_resend, resend_count FROM turns WHERE id = ?",
    )
    .bind(&f1.turn_id)
    .fetch_one(&f1.env.app.db)
    .await
    .unwrap();
    assert_eq!((deliv1_after.as_str(), ver1_after, auto_resend1), ("ok", 1, 0), "promote 後 hook 正確補上 verified=1");
    assert_eq!(count1, crate::lifecycle::MAX_PROMPT_RESENDS, "promote 設定之 resend_count 不被 hook 覆蓋");

    let final_turns_count1: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id = ?").bind(&f1.run_id).fetch_one(&f1.env.app.db).await.unwrap();
    assert_eq!(final_turns_count1, initial_turns_count1, "未新增任何多餘 turn");

    // 順序 B: 已經 delivery='ok', verified=0, auto_resend=1（模擬已決定補送之快照）
    let f2 = setup_fixture("test_last_user_input_ping", "ok", 0, "web").await;
    sqlx::query("UPDATE turns SET auto_resend = 1, resend_count = 1 WHERE id = ?").bind(&f2.turn_id).execute(&f2.env.app.db).await.unwrap();
    let body2 = make_body(&f2.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);

    process(&f2.env.app, &body2).await.unwrap();
    let (ver2, auto_resend2, resend_count2): (i64, i64, i64) = sqlx::query_as(
        "SELECT delivery_verified, auto_resend, resend_count FROM turns WHERE id = ?",
    )
    .bind(&f2.turn_id)
    .fetch_one(&f2.env.app.db)
    .await
    .unwrap();
    assert_eq!(ver2, 1, "hook 先補上 verified=1");
    assert_eq!(auto_resend2, 1, "hook 不動 auto_resend");
    assert_eq!(resend_count2, 1, "hook 不動 resend_count");

    // promote 條件限制 delivery='unknown'，面對 delivery='ok' 的 turn 必傳回 false，不覆蓋 verified 或 auto_resend
    let promoted2 = crate::lifecycle::late_proof::promote(&f2.env.app, &f2.turn_id, crate::lifecycle::delivery::Delivered::Unverified).await.unwrap();
    assert!(!promoted2, "非 unknown 的 turn 無法被 promote");

    let (deliv2_after, ver2_after, resend2, count2): (String, i64, i64, i64) = sqlx::query_as(
        "SELECT delivery, delivery_verified, auto_resend, resend_count FROM turns WHERE id = ?",
    )
    .bind(&f2.turn_id)
    .fetch_one(&f2.env.app.db)
    .await
    .unwrap();
    assert_eq!((deliv2_after.as_str(), ver2_after, resend2, count2), ("ok", 1, 1, 1), "promote 失敗不影響已被 hook 驗證的狀態與重送旗標");
}

// AGM 新增必驗 ②：多候選同文時唯一性守衛生效，一筆都不補（透過 DROP INDEX turns_one_in_flight 構造同 run 雙 in_flight 候選並呼叫正式 process）
#[tokio::test]
async fn multiple_candidates_refuse_verification() {
    let f = setup_fixture("test_last_user_input_ping", "ok", 0, "web").await;

    // 在 test-only 中移除 turns_one_in_flight 唯一索引，以構造同 run 同文的兩筆 in_flight turns
    sqlx::query("DROP INDEX IF EXISTS turns_one_in_flight").execute(&f.env.app.db).await.unwrap();

    let turn_id2 = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, delivery_verified, auto_resend, resend_count, prompt_text, delivered_at, created_at)
         VALUES (?,?,?,?,'in_flight','ok',0,0,0,?,?,?)",
    )
    .bind(&turn_id2)
    .bind(&f.conv_id)
    .bind(&f.run_id)
    .bind("web")
    .bind("test_last_user_input_ping")
    .bind(&f.delivered_at)
    .bind("2026-10-24T13:59:59.000Z")
    .execute(&f.env.app.db)
    .await
    .unwrap();

    let mut events = f.env.app.subscribe();
    let body = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);

    // 呼叫正式 process
    process(&f.env.app, &body).await.unwrap();

    // 斷言兩筆的 delivery_verified 都維持 0
    let (v1, v2): (i64, i64) = (
        sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap(),
        sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&turn_id2).fetch_one(&f.env.app.db).await.unwrap(),
    );
    assert_eq!((v1, v2), (0, 0), "存在同文雙候選時，唯一性守衛生效，兩筆都不得補記 verified");

    // 斷言零 turn_updated 事件
    let turn_updated_events: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|e| e.kind == "turn_updated")
        .filter(|e| {
            let tid = e.data["turn"]["id"].as_str().unwrap_or_default();
            tid == f.turn_id || tid == turn_id2
        })
        .collect();
    assert_eq!(turn_updated_events.len(), 0, "雙候選被拒絕時，絕不發出 turn_updated 事件");

    // 補充驗證：其中一筆已被 verified=1，同文候選集合仍為 2，不可將未 verified 的那一筆補記
    sqlx::query("UPDATE turns SET delivery_verified = 1 WHERE id = ?").bind(&f.turn_id).execute(&f.env.app.db).await.unwrap();
    process(&f.env.app, &body).await.unwrap();
    let v2_after: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&turn_id2).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(v2_after, 0, "即使一筆已 verified=1，同文候選為 2 筆時仍嚴格拒絕補記第二筆");
}

// AGM 新增必驗 ③：重播只 emit 一次（UPDATE rows_affected == 1 才 emit），且透過 app.subscribe 嚴格斷言
#[tokio::test]
async fn replay_hook_emits_turn_only_once() {
    let f = setup_fixture("test_last_user_input_ping", "ok", 0, "web").await;
    let body = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);

    let mut events = f.env.app.subscribe();

    // 第一次處理：0 -> 1，應觸發 1 次 emit
    process(&f.env.app, &body).await.unwrap();
    let v1: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(v1, 1);

    let first_emits: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|e| e.kind == "turn_updated" && e.data["turn"]["id"] == f.turn_id)
        .collect();
    assert_eq!(first_emits.len(), 1, "第一次處理成功時必發出 1 次 turn_updated");

    // 第二次處理（重播同一個 PreInvocation hook）：0 -> 1 不成立（rows_affected == 0），不應 emit
    process(&f.env.app, &body).await.unwrap();
    let v2: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(v2, 1, "重播後狀態維持 1");

    let second_emits: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|e| e.kind == "turn_updated" && e.data["turn"]["id"] == f.turn_id)
        .collect();
    assert_eq!(second_emits.len(), 0, "重播時不應發出任何 turn_updated 事件");
}

// 必補各項 guard 表格對照真 process 測試
#[tokio::test]
async fn pre_invocation_comprehensive_guards_table() {
    let f = setup_fixture("test_last_user_input_ping", "ok", 0, "web").await;

    // 1. remote 主機（非 LOCAL_HOST）
    sqlx::query("UPDATE projects SET host = 'remote-box' WHERE id = ?").bind(&f.env.project_id).execute(&f.env.app.db).await.unwrap();
    let body_remote = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);
    process(&f.env.app, &body_remote).await.unwrap();
    let v_remote: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(v_remote, 0, "遠端主機不補送達驗證");
    sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(crate::config::LOCAL_HOST).bind(&f.env.project_id).execute(&f.env.app.db).await.unwrap();

    // 2. stale run（已 exited）
    sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
    let body_stale = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);
    process(&f.env.app, &body_stale).await.unwrap();
    let v_stale: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(v_stale, 0, "stale run 不補送達驗證");
    sqlx::query("UPDATE runs SET state = 'running' WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();

    // Synthetic stale event explicitly naming a previous run must not touch the active run.
    let mut old_run_body = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);
    old_run_body.run_id = Some(db::ulid());
    process(&f.env.app, &old_run_body).await.unwrap();
    let unchanged: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id=?")
        .bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(unchanged, 0, "a previous run's event cannot verify the current turn");

    // A terminal turn never transitions back to in_flight. Use an independent
    // fixture for this case, preserving the production transition trigger.
    let completed = setup_fixture("test_last_user_input_ping", "ok", 0, "web").await;
    sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?")
        .bind(&completed.turn_id).execute(&completed.env.app.db).await.unwrap();
    let body_comp = make_body(&completed.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);
    process(&completed.env.app, &body_comp).await.unwrap();
    let v_comp: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?")
        .bind(&completed.turn_id).fetch_one(&completed.env.app.db).await.unwrap();
    assert_eq!(v_comp, 0, "已 completed 的 turn 不補送達驗證");

    // 4. 缺少 / 無效 received_at
    let body_no_rec = make_body(&f.bot_id, RAW_PRE_FIXTURE, None, false);
    process(&f.env.app, &body_no_rec).await.unwrap();
    let v_no_rec: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(v_no_rec, 0, "缺少 received_at 不補送達驗證");

    let body_bad_rec = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("not-a-datetime"), false);
    process(&f.env.app, &body_bad_rec).await.unwrap();
    let v_bad_rec: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(v_bad_rec, 0, "無效格式之 received_at 不補送達驗證");

    // 5. delivered_at 欄位為 NULL
    sqlx::query("UPDATE turns SET delivered_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&f.env.app.db).await.unwrap();
    let body_no_deliv = make_body(&f.bot_id, RAW_PRE_FIXTURE, Some("2026-10-24T14:00:01.000Z"), false);
    process(&f.env.app, &body_no_deliv).await.unwrap();
    let v_no_deliv: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(v_no_deliv, 0, "delivered_at 為 NULL 不補送達驗證");
    sqlx::query("UPDATE turns SET delivered_at = ? WHERE id = ?").bind(&f.delivered_at).bind(&f.turn_id).execute(&f.env.app.db).await.unwrap();

    // 6. lastUserInput 型別錯誤（非字串，例如整數）
    let mut raw_num: Value = serde_json::from_str(RAW_PRE_FIXTURE).unwrap();
    raw_num["lastUserInput"] = json!(123456);
    let body_num = HookBody {
        bot_id: f.bot_id.clone(),
        provider: "agy".into(),
        payload: {
            raw_num["hookEventName"] = json!("PreInvocation");
            raw_num
        },
        received_at: Some("2026-10-24T14:00:01.000Z".into()),
        truncated: false,
        run_id: None,
    };
    process(&f.env.app, &body_num).await.unwrap();
    let v_num: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(v_num, 0, "lastUserInput 為數字時忽略");

    // 7. lastUserInput 為純空白
    let mut raw_blank: Value = serde_json::from_str(RAW_PRE_FIXTURE).unwrap();
    raw_blank["lastUserInput"] = json!("   \n\t  ");
    let body_blank = HookBody {
        bot_id: f.bot_id.clone(),
        provider: "agy".into(),
        payload: {
            raw_blank["hookEventName"] = json!("PreInvocation");
            raw_blank
        },
        received_at: Some("2026-10-24T14:00:01.000Z".into()),
        truncated: false,
        run_id: None,
    };
    process(&f.env.app, &body_blank).await.unwrap();
    let v_blank: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(v_blank, 0, "lastUserInput 為純空白時忽略");
}
