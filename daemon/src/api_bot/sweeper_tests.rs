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
    .bind(&now).bind(&now).bind(crate::db::iso_in(3600))
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
    .bind(&now).bind(&now).bind(crate::db::iso_in(3600))
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
