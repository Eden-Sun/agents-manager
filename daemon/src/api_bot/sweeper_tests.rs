use super::sweeper::{self, detect_stuck, sweep_once, StuckInfo};
use serde_json::json;

#[tokio::test]
async fn sweeper_clears_api_pending_and_synthesizes_tool_result_when_owner_turn_fails() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    let bot_id = "sw-bot-1";
    let run_id = "sw-run-1";
    let conv_id = "sw-conv-1";
    let turn_id = "sw-turn-1";
    let session_id = "sw-sess-1";

    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,'sw-bot','claude-api','fake',?)")
        .bind(bot_id).bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'exited',?)")
        .bind(run_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)")
        .bind(conv_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','failed','ok',?)")
        .bind(turn_id).bind(conv_id).bind(run_id).bind(&now).execute(&env.app.db).await.unwrap();

    let initial_chk = json!({
        "messages": [
            {
                "role": "user",
                "content": [{"type": "text", "text": "do something"}]
            },
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "using tool"},
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "echo hi"}}
                ]
            }
        ],
        "responses": [],
        "api_pending": true,
        "tool_pending": "t1",
        "completed_tool_ids": []
    });

    sqlx::query(
        "INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) \
         VALUES(?,?,?,?,1,1,?,?,?,?)"
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(turn_id).bind(initial_chk.to_string())
    .bind(&now).bind(&now).bind(crate::db::iso_in(3600))
    .execute(&env.app.db).await.unwrap();

    sqlx::query(
        "INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) \
         VALUES('att-1',?,?,?,?,1,0,'0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef','standard','reserved',500,'v1',?)"
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(turn_id).bind(&now)
    .execute(&env.app.db).await.unwrap();

    // Verify it is detected as stuck before sweeping:
    let stuck = detect_stuck(&env.app, bot_id).await;
    assert_eq!(
        stuck,
        Some(StuckInfo {
            reason: "checkpoint_pending_after_owner_exit",
            message: "回合結束但仍有未結算標記",
        })
    );

    // Run sweeper:
    let report = sweep_once(&env.app).await.unwrap();
    assert_eq!(report.abandoned_checkpoints, 1);

    // Verify attempt became unknown:
    let att_state: String = sqlx::query_scalar("SELECT state FROM api_bot_attempts WHERE id='att-1'")
        .fetch_one(&env.app.db).await.unwrap();
    assert_eq!(att_state, "unknown");

    // Verify session checkpoint was settled:
    let (chk_raw, rev): (String, i64) = sqlx::query_as(
        "SELECT checkpoint_json, revision FROM api_bot_sessions WHERE id=?"
    )
    .bind(session_id).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(rev, 2);

    let val: serde_json::Value = serde_json::from_str(&chk_raw).unwrap();
    assert_eq!(val["api_pending"], false);
    assert_eq!(val["tool_pending"], serde_json::Value::Null);
    assert_eq!(val["completed_tool_ids"], json!(["t1"]));

    let messages = val["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2]["role"], "user");
    let content = messages[2]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "tool_result");
    assert_eq!(content[0]["tool_use_id"], "t1");
    assert_eq!(content[0]["content"], "not executed: turn ended");
    assert_eq!(content[0]["is_error"], true);

    // Verify it is no longer stuck:
    let after_stuck = detect_stuck(&env.app, bot_id).await;
    assert_eq!(after_stuck, None);
}

#[tokio::test]
async fn sweeper_settles_reserved_attempt_exceeding_hard_deadline() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    let bot_id = "sw-bot-2";
    let run_id = "sw-run-2";
    let conv_id = "sw-conv-2";
    let turn_id = "sw-turn-2";
    let session_id = "sw-sess-2";

    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,'sw-bot-2','claude-api','fake',?)")
        .bind(bot_id).bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
        .bind(run_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)")
        .bind(conv_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
        .bind(turn_id).bind(conv_id).bind(run_id).bind(&now).execute(&env.app.db).await.unwrap();

    let initial_chk = json!({
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        "responses": [],
        "api_pending": true,
        "tool_pending": null,
        "completed_tool_ids": []
    });

    sqlx::query(
        "INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) \
         VALUES(?,?,?,?,1,1,?,?,?,?)"
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(turn_id).bind(initial_chk.to_string())
    .bind(&now).bind(crate::db::iso_in(-400)).bind(crate::db::iso_in(3600))
    .execute(&env.app.db).await.unwrap();

    // Insert reserved attempt created 2500 seconds ago (hard deadline is 1800 + 120 = 1920s)
    let stale_created = crate::db::iso_in(-2500);
    sqlx::query(
        "INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) \
         VALUES('att-reserved-timeout',?,?,?,?,1,0,'0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef','standard','reserved',300,'v1',?)"
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(turn_id).bind(&stale_created)
    .execute(&env.app.db).await.unwrap();

    // Verify stuck detector finds it:
    let stuck = detect_stuck(&env.app, bot_id).await;
    assert_eq!(
        stuck,
        Some(StuckInfo {
            reason: "reserved_attempt_timeout",
            message: "請求預留逾時未有結果",
        })
    );

    // Run sweeper:
    let report = sweep_once(&env.app).await.unwrap();
    assert_eq!(report.reserved_timeouts, 1);

    // Verify attempt settled to unknown:
    let (state, finished): (String, Option<String>) = sqlx::query_as(
        "SELECT state, finished_at FROM api_bot_attempts WHERE id='att-reserved-timeout'"
    ).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(state, "unknown");
    assert!(finished.is_some());

    // Verify session checkpoint api_pending was cleared:
    let chk_raw: String = sqlx::query_scalar("SELECT checkpoint_json FROM api_bot_sessions WHERE id=?")
        .bind(session_id).fetch_one(&env.app.db).await.unwrap();
    let val: serde_json::Value = serde_json::from_str(&chk_raw).unwrap();
    assert_eq!(val["api_pending"], false);
}

#[tokio::test]
async fn sweeper_clears_api_pending_for_stale_unknown_attempt() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    let bot_id = "sw-bot-3";
    let run_id = "sw-run-3";
    let conv_id = "sw-conv-3";
    let turn_id = "sw-turn-3";
    let session_id = "sw-sess-3";

    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,'sw-bot-3','claude-api','fake',?)")
        .bind(bot_id).bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
        .bind(run_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)")
        .bind(conv_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
        .bind(turn_id).bind(conv_id).bind(run_id).bind(&now).execute(&env.app.db).await.unwrap();

    let initial_chk = json!({
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        "responses": [],
        "api_pending": true,
        "tool_pending": null,
        "completed_tool_ids": []
    });

    sqlx::query(
        "INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) \
         VALUES(?,?,?,?,1,1,?,?,?,?)"
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(turn_id).bind(initial_chk.to_string())
    .bind(&now).bind(crate::db::iso_in(-400)).bind(crate::db::iso_in(3600))
    .execute(&env.app.db).await.unwrap();

    // Insert unknown attempt finished 400s ago (> 300s = 5min)
    let stale_finished = crate::db::iso_in(-400);
    sqlx::query(
        "INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at,finished_at) \
         VALUES('att-stale-unk',?,?,?,?,1,0,'0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef','standard','unknown',400,'v1',?,?)"
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(turn_id).bind(&stale_finished).bind(&stale_finished)
    .execute(&env.app.db).await.unwrap();

    // Verify stuck detector finds it:
    let stuck = detect_stuck(&env.app, bot_id).await;
    assert_eq!(
        stuck,
        Some(StuckInfo {
            reason: "unknown_attempt_stuck",
            message: "未知結果請求卡住未結清",
        })
    );

    // Run sweeper:
    let report = sweep_once(&env.app).await.unwrap();
    assert_eq!(report.stale_unknowns, 1);

    // Verify attempt remains unknown:
    let att_state: String = sqlx::query_scalar("SELECT state FROM api_bot_attempts WHERE id='att-stale-unk'")
        .fetch_one(&env.app.db).await.unwrap();
    assert_eq!(att_state, "unknown");

    // Verify session checkpoint api_pending was cleared:
    let chk_raw: String = sqlx::query_scalar("SELECT checkpoint_json FROM api_bot_sessions WHERE id=?")
        .bind(session_id).fetch_one(&env.app.db).await.unwrap();
    let val: serde_json::Value = serde_json::from_str(&chk_raw).unwrap();
    assert_eq!(val["api_pending"], false);

    // Verify it is no longer stuck:
    let after_stuck = detect_stuck(&env.app, bot_id).await;
    assert_eq!(after_stuck, None);
}


/// 2026-11-15 事故重現：舊回合留下的 unknown attempt（ordinal 55）＋新回合進行中（owner turn in_flight、runtime 有回合、
/// 有還在進行的 reserved attempt、checkpoint 剛被寫過）。sweeper 絕不能動這個 session 的 checkpoint，否則回合的 journal 存檔會失敗。
#[tokio::test]
async fn sweeper_never_touches_a_session_with_a_turn_in_flight_because_of_an_old_unknown_attempt() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    let (bot_id, run_id, conv_id, old_turn, new_turn, session_id) = ("sw-bot-9", "sw-run-9", "sw-conv-9", "sw-turn-9-old", "sw-turn-9-new", "sw-sess-9");
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,'sw-bot-9','claude-api','fake',?)")
        .bind(bot_id).bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
        .bind(run_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)")
        .bind(conv_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    for (t, st) in [(old_turn, "failed"), (new_turn, "in_flight")] {
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web',?,'ok',?)")
            .bind(t).bind(conv_id).bind(run_id).bind(st).bind(&now).execute(&env.app.db).await.unwrap();
    }
    let chk = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}], "responses": [], "api_pending": true, "tool_pending": null, "completed_tool_ids": []});
    sqlx::query(
        "INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,812,?,?,?,?)",
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(new_turn).bind(chk.to_string()).bind(&now).bind(&now).bind(crate::db::iso_in(3600))
    .execute(&env.app.db).await.unwrap();
    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let old = crate::db::iso_in(-20 * 3600);
    sqlx::query(
        "INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at,finished_at) \
         VALUES('att-old-unknown',?,?,?,?,1,55,?,'standard','unknown',400,'v1',?,?)",
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(old_turn).bind(hash).bind(&old).bind(&old)
    .execute(&env.app.db).await.unwrap();
    sqlx::query(
        "INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) \
         VALUES('att-live',?,?,?,?,1,144,?,'standard','reserved',500,'v1',?)",
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(new_turn).bind(hash).bind(&now)
    .execute(&env.app.db).await.unwrap();

    let report = sweep_once(&env.app).await.unwrap();
    assert_eq!((report.stale_unknowns, report.abandoned_checkpoints, report.reserved_timeouts), (0, 0, 0), "{report:?}");
    let (raw, rev, upd): (String, i64, String) =
        sqlx::query_as("SELECT checkpoint_json, revision, updated_at FROM api_bot_sessions WHERE id=?").bind(session_id).fetch_one(&env.app.db).await.unwrap();
    let val: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!((val["api_pending"].clone(), rev, upd), (json!(true), 812, now), "checkpoint 一個字都不能動");
    let live: String = sqlx::query_scalar("SELECT state FROM api_bot_attempts WHERE id='att-live'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(live, "reserved", "進行中的 attempt 不被結清");
    assert_eq!(detect_stuck(&env.app, bot_id).await, None, "進行中的回合不是卡住");
}

/// 舊 unknown 只是帳務未知：就算 session 很久沒動，只要後面已經有更新的 attempt，sweeper 也不為那筆舊 unknown 改 checkpoint。
#[tokio::test]
async fn an_old_unknown_attempt_does_not_rewrite_a_checkpoint_when_a_newer_attempt_exists() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    let (bot_id, run_id, conv_id, turn_id, session_id) = ("sw-bot-10", "sw-run-10", "sw-conv-10", "sw-turn-10", "sw-sess-10");
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,'sw-bot-10','claude-api','fake',?)")
        .bind(bot_id).bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
        .bind(run_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)")
        .bind(conv_id).bind(bot_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
        .bind(turn_id).bind(conv_id).bind(run_id).bind(&now).execute(&env.app.db).await.unwrap();
    let chk = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}], "responses": [], "api_pending": true, "tool_pending": null, "completed_tool_ids": []});
    let quiet = crate::db::iso_in(-900);
    sqlx::query(
        "INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,7,?,?,?,?)",
    )
    .bind(session_id).bind(bot_id).bind(run_id).bind(turn_id).bind(chk.to_string()).bind(&now).bind(&quiet).bind(crate::db::iso_in(3600))
    .execute(&env.app.db).await.unwrap();
    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let (t1, t2) = (crate::db::iso_in(-3000), crate::db::iso_in(-2000));
    for (id, ord, state, at) in [("att-a", 1, "unknown", &t1), ("att-b", 2, "recorded", &t2)] {
        sqlx::query(
            "INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at,finished_at) \
             VALUES(?,?,?,?,?,1,?,?,'standard',?,400,'v1',?,?)",
        )
        .bind(id).bind(session_id).bind(bot_id).bind(run_id).bind(turn_id).bind(ord).bind(hash).bind(state).bind(at).bind(at)
        .execute(&env.app.db).await.unwrap();
    }
    let report = sweep_once(&env.app).await.unwrap();
    assert_eq!(report.stale_unknowns, 0);
    let (raw, rev): (String, i64) = sqlx::query_as("SELECT checkpoint_json, revision FROM api_bot_sessions WHERE id=?").bind(session_id).fetch_one(&env.app.db).await.unwrap();
    assert_eq!((serde_json::from_str::<serde_json::Value>(&raw).unwrap()["api_pending"].clone(), rev), (json!(true), 7));
}

/// claude-api 沒有 pane／hook：主力膠囊的「進行中」要靠 `runs.agent_status`。回合開始 working、結束 idle，同欄位同事件
/// （`bot_status`），值沒變不重推，已結束的 run 不碰。
#[tokio::test]
async fn api_turn_state_drives_the_same_run_agent_status_field_cli_bots_use() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    for (bot, run, state) in [("st-bot", "st-run", "running"), ("st-bot-x", "st-run-x", "exited")] {
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api','fake',?)")
            .bind(bot).bind(&env.project_id).bind(bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,?,?)")
            .bind(run).bind(bot).bind(state).bind(&now).execute(&env.app.db).await.unwrap();
    }
    let status = |run: &'static str| {
        let db = env.app.db.clone();
        async move { sqlx::query_scalar::<_, String>("SELECT agent_status FROM runs WHERE id=?").bind(run).fetch_one(&db).await.unwrap() }
    };
    let mut rx = env.app.subscribe();
    // 回傳這顆 bot 收到的 bot_status 事件裡 run.agent_status 的序列。
    let drain = |rx: &mut tokio::sync::broadcast::Receiver<am_base::ws_event::WsEvent>| {
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if ev.kind == "bot_status" && ev.data["bot_id"] == "st-bot" {
                seen.push(ev.data["run"]["agent_status"].as_str().unwrap_or("").to_string());
            }
        }
        seen
    };
    let _ = drain(&mut rx);
    super::completion::set_run_agent_status(&env.app, "st-bot", "st-run", "working").await;
    assert_eq!(status("st-run").await, "working", "回合開始");
    assert_eq!(drain(&mut rx), vec!["working"], "推 bot_status，前端的燈號才會轉");
    super::completion::set_run_agent_status(&env.app, "st-bot", "st-run", "working").await;
    assert!(drain(&mut rx).is_empty(), "值沒變不重推");
    super::completion::set_run_agent_status(&env.app, "st-bot", "st-run", "idle").await;
    assert_eq!(status("st-run").await, "idle", "回合結束");
    assert_eq!(drain(&mut rx), vec!["idle"]);
    super::completion::set_run_agent_status(&env.app, "st-bot-x", "st-run-x", "working").await;
    assert_eq!(status("st-run-x").await, "unknown", "已結束的 run 不碰（預設值不變）");
}
