//! Read-only classification of terminal Applied history. Never join, restore or rebind.
use super::{receipt::{self, Pending, State}, startup_recovery::Error};
use std::future::Future;

pub(super) enum StartupHistoricalApplied { Skip, Live, Unresolved }

pub(super) fn classify<'a>(pool: &'a sqlx::SqlitePool, item: &'a Pending)
    -> impl Future<Output = Result<StartupHistoricalApplied, Error>> + Send + 'a {
    Box::pin(classify_body(pool, item))
}

/// 歷史 Applied receipt 是否與目前 session 一致。`states` 是允許的 run 狀態，`include_owner` 表示也收「session 目前的 owner」：
/// - run 已終止：owner 與被取代的舊 receipt 都收；
/// - run 還活著（running）：只收被後來 turn 取代的舊 receipt（目前 owner 走 checkpoint 檢查）；
/// - run 卡在 stopping／starting：`owner_on` 要求 running，目前 owner 會被判 Fenced 而隔離整個 API，所以 owner 也收
///   （Applied 代表 turn 已是終態，之後 abandon 只會把還在飛的 turn 與 reserved 收尾）。
fn valid_history_sql(states: &str, include_owner: bool) -> String {
    let owner = if include_owner {
        "((j.run_id=s.owner_run_id AND j.turn_id=s.owner_turn_id AND j.generation=s.generation AND j.revision=s.revision) OR (j.turn_id!=s.owner_turn_id AND j.revision<s.revision))"
    } else {
        "(j.turn_id!=s.owner_turn_id AND j.revision<s.revision)"
    };
    // session 目前的 owner 已結清：有對上的 applied receipt；或 owner turn 已是終態、沒有任何未 applied 的 receipt
    // （例如回合被其他機制收尾、完成被 fence 掉而沒留 receipt，之後 sweeper 清掉 pending checkpoint）。
    // 否則一個沒 receipt 的 owner 會讓這個 session 所有歷史 receipt 都判 unresolved，隔離全部 API bot。
    let owner_settled = "EXISTS(SELECT 1 FROM api_bot_join_receipts latest WHERE latest.session_id=s.id AND latest.bot_id=b.id AND latest.run_id=s.owner_run_id AND latest.turn_id=s.owner_turn_id AND latest.generation=s.generation AND latest.revision=s.revision AND latest.state='applied') OR (EXISTS(SELECT 1 FROM turns ot WHERE ot.id=s.owner_turn_id AND ot.status IN ('completed','completed_fallback','failed')) AND NOT EXISTS(SELECT 1 FROM api_bot_join_receipts p WHERE p.session_id=s.id AND p.turn_id=s.owner_turn_id AND p.state!='applied'))";
    format!("SELECT EXISTS(SELECT 1 FROM api_bot_sessions s JOIN bots b ON b.id=s.bot_id JOIN runs r ON r.id=? AND r.bot_id=b.id JOIN turns t ON t.id=? AND t.run_id=r.id JOIN conversations c ON c.id=t.conversation_id AND c.bot_id=b.id JOIN api_bot_join_receipts j ON j.session_id=s.id AND j.bot_id=b.id AND j.run_id=r.id AND j.turn_id=t.id WHERE ({owner_settled}) AND s.id=? AND b.id=? AND b.kind='claude-api' AND r.state IN {states} AND j.state='applied' AND j.generation=? AND j.revision=? AND j.generation<=s.generation AND {owner} AND t.delivery='ok' AND ((j.outcome='completed' AND t.status='completed' AND j.final_text IS NOT NULL AND (SELECT count(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant')=1 AND EXISTS(SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.conversation_id=c.id AND m.role='assistant' AND m.source='transcript' AND m.incomplete=0 AND m.rewound_at IS NULL AND m.content=j.final_text)) OR (j.outcome IN ('failed','cancelled') AND t.status='failed' AND j.final_text IS NULL AND (SELECT count(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant')=0)))")
}

async fn history_valid(tx: &mut sqlx::SqliteConnection, item: &Pending, states: &str, include_owner: bool) -> Result<bool, Error> {
    sqlx::query_scalar(&valid_history_sql(states, include_owner))
        .bind(&item.identity.run_id).bind(&item.identity.turn_id).bind(&item.identity.session_id).bind(&item.identity.bot_id).bind(item.identity.generation).bind(item.revision)
        .fetch_one(tx).await.map_err(|_| Error::Storage)
}

async fn classify_body(pool: &sqlx::SqlitePool, item: &Pending) -> Result<StartupHistoricalApplied, Error> {
    let mut tx = pool.begin().await.map_err(|_| Error::Storage)?;
    let run_state: Option<String> = sqlx::query_scalar("SELECT r.state FROM runs r JOIN bots b ON b.id=r.bot_id WHERE b.id=? AND b.kind='claude-api' AND r.id=?")
        .bind(&item.identity.bot_id).bind(&item.identity.run_id).fetch_optional(&mut *tx).await.map_err(|_| Error::Storage)?;
    let result = match run_state.as_deref() {
        // 同一個還活著的 run 裡已被後來 turn 取代的舊 receipt 是歷史，不是「目前的 owner」：
        // 一律當 Live 會讓任何跑過兩則以上的 run 在重啟時被隔離（api_startup_unresolved）。
        Some(state @ ("running" | "starting" | "stopping")) => {
            let exact = receipt::matches_on(&mut tx, item, State::Applied).await.map_err(|_| Error::Storage)?;
            if exact && history_valid(&mut *tx, item, &format!("('{state}')"), state != "running").await? { StartupHistoricalApplied::Skip } else { StartupHistoricalApplied::Live }
        }
        Some("stopped" | "exited") => {
            let exact = receipt::matches_on(&mut tx, item, State::Applied).await.map_err(|_| Error::Storage)?;
            let valid = history_valid(&mut *tx, item, "('stopped','exited')", true).await?;
            if exact && valid { StartupHistoricalApplied::Skip } else { StartupHistoricalApplied::Unresolved }
        }
        _ => StartupHistoricalApplied::Unresolved,
    };
    tx.rollback().await.map_err(|_| Error::Storage)?;
    Ok(result)
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_startup_history_checks_all_applied_receipts_without_writes() {
        let env=crate::testing::env().await;
        let bot=crate::db::ulid(); let run=crate::db::ulid(); let conv=crate::db::ulid(); let session=crate::db::ulid();
        let first=crate::db::ulid(); let latest=crate::db::ulid(); let first_message=crate::db::ulid(); let latest_message=crate::db::ulid(); let attempt=crate::db::ulid();
        let now=crate::db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api','fake',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'stopped',?)").bind(&run).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(&conv).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        for turn in [&first,&latest] { sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','completed','ok',?)").bind(turn).bind(&conv).bind(&run).bind(&now).execute(&env.app.db).await.unwrap(); }
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,2,'{}',?,?,?)").bind(&session).bind(&bot).bind(&run).bind(&latest).bind(&now).bind(&now).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        for (turn, msg, revision, text) in [(&first,&first_message,1_i64,"first"),(&latest,&latest_message,2_i64,"latest")] {
            sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,?,?,?,1,?,'completed',?,'applied',?,?)").bind(&session).bind(&bot).bind(&run).bind(turn).bind(revision).bind(text).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
            sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'assistant',?,'transcript',?)").bind(msg).bind(&conv).bind(turn).bind(text).bind(&now).execute(&env.app.db).await.unwrap();
        }
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES(?,?,?,?,?,1,0,'fixture','standard','unknown',123,'fixture',?)").bind(&attempt).bind(&session).bind(&bot).bind(&run).bind(&latest).bind(&now).execute(&env.app.db).await.unwrap();
        let app=crate::testing::restart_app(&env).await;
        app.set_startup_ready(false);
        assert!(super::super::startup_recovery::recover_all(&app).await.is_ok());
        let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM messages WHERE conversation_id=? AND role='assistant'),(SELECT reserved_microusd FROM api_bot_attempts WHERE id=?)").bind(&conv).bind(&attempt).fetch_one(&app.db).await.unwrap();
        assert_eq!(counts,(2,123));
        sqlx::query("UPDATE messages SET content='conflict' WHERE id=?").bind(&first_message).execute(&app.db).await.unwrap();
        assert_eq!(super::super::startup_recovery::recover_all(&app).await.err().unwrap().code(),"api_startup_unresolved");
        sqlx::query("UPDATE messages SET content='first' WHERE id=?").bind(&first_message).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_join_receipts SET revision=3 WHERE turn_id=?").bind(&first).execute(&app.db).await.unwrap();
        assert_eq!(super::super::startup_recovery::recover_all(&app).await.err().unwrap().code(),"api_startup_unresolved");
        sqlx::query("UPDATE api_bot_join_receipts SET revision=1,generation=2 WHERE turn_id=?").bind(&first).execute(&app.db).await.unwrap();
        assert_eq!(super::super::startup_recovery::recover_all(&app).await.err().unwrap().code(),"api_startup_unresolved");
        sqlx::query("UPDATE api_bot_join_receipts SET generation=1 WHERE turn_id=?").bind(&first).execute(&app.db).await.unwrap();
        sqlx::query("ALTER TABLE messages RENAME TO synthetic_history_messages").execute(&app.db).await.unwrap();
        assert_eq!(super::super::startup_recovery::recover_all(&app).await.err().unwrap().code(),"api_startup_storage");
    }

    /// 正式環境實際卡住的狀態：同一個 stopping 的 run 跑過兩個 turn（第二個是失敗的 session owner），又有第三則停在 in_flight/pending。
    /// 舊 receipt 被當成 Live → Fenced → api_startup_unresolved 隔離；之後也無法 resume（owner 是失敗的 turn）。
    #[tokio::test]
    async fn superseded_receipt_in_a_stopping_run_does_not_quarantine_and_failed_owner_can_resume() {
        let env=crate::testing::env().await;
        let bot=crate::db::ulid(); let run=crate::db::ulid(); let conv=crate::db::ulid(); let session=crate::db::ulid();
        let (a,b,c)=(crate::db::ulid(),crate::db::ulid(),crate::db::ulid());
        let now=crate::db::now();
        let checkpoint = r#"{"messages":[],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":[]}"#;
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api','fake',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'stopping',?)").bind(&run).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(&conv).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        for (turn, status, delivery) in [(&a,"completed","ok"),(&b,"failed","ok"),(&c,"in_flight","pending")] {
            sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web',?,?,?)").bind(turn).bind(&conv).bind(&run).bind(status).bind(delivery).bind(&now).execute(&env.app.db).await.unwrap();
        }
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,2,?,?,?,?)").bind(&session).bind(&bot).bind(&run).bind(&b).bind(checkpoint).bind(&now).bind(&now).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,?,?,?,1,1,'completed','first','applied',?,?)").bind(&session).bind(&bot).bind(&run).bind(&a).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,?,?,?,1,2,'failed',NULL,'applied',?,?)").bind(&session).bind(&bot).bind(&run).bind(&b).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'assistant','first','transcript',?)").bind(crate::db::ulid()).bind(&conv).bind(&a).bind(&now).execute(&env.app.db).await.unwrap();
        let app=crate::testing::restart_app(&env).await;
        app.set_startup_ready(false);
        let r = super::super::startup_recovery::recover_all(&app).await;
        assert!(r.is_ok(), "{:?}：stopping run 的 receipt 是已結清的歷史，不該讓整個 API admission 被隔離", r.as_ref().err().map(|e| e.code()));
        let (state, c_status): (String,String)=sqlx::query_as("SELECT (SELECT state FROM runs WHERE id=?),(SELECT status FROM turns WHERE id=?)").bind(&run).bind(&c).fetch_one(&app.db).await.unwrap();
        assert_eq!((state.as_str(), c_status.as_str()), ("exited","failed"));
        let mut tx = crate::db::begin_write(&app.db).await.unwrap();
        assert!(super::super::native_resume::prove_on(&mut tx, &bot, &run, super::super::native_resume::ProofMode::StartBeforeRun).await.is_ok(), "失敗的 owner turn 也要能 resume");
        tx.rollback().await.unwrap();
    }

    /// 2026-11-15 pt-hub 事故的狀態：run 還在 `running`（重啟前的行程當掉，DB 沒來得及收），session 的 owner turn 是失敗的
    /// （journal 寫失敗，receipt 已 applied），checkpoint 已無 pending，只剩帳務上的 unknown attempt；更早還有已完成與失敗的舊 turn。
    /// 這種狀態不能讓整個 API 被隔離（api_startup_unresolved）。
    #[tokio::test]
    async fn a_running_run_with_a_failed_owner_and_only_unknown_accounting_attempts_does_not_quarantine() {
        let env=crate::testing::env().await;
        let bot=crate::db::ulid(); let run=crate::db::ulid(); let conv=crate::db::ulid(); let session=crate::db::ulid();
        let (a,b,c)=(crate::db::ulid(),crate::db::ulid(),crate::db::ulid());
        let now=crate::db::now();
        // 長 session：`responses` 累積了 3 筆各帶一個工具，但 API 壓縮之後 `messages` 只剩最近一組 tool_use／tool_result。
        let resp = |n: u32| serde_json::json!({"id": format!("msg_{n}"), "model": crate::config::API_BOT_MODEL, "content": [{"type":"tool_use","id": format!("t{n}"),"name":"bash","input":{}}], "stop_reason":"tool_use", "usage": {"input_tokens":1,"output_tokens":1}});
        let checkpoint = serde_json::json!({
            "messages": [
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"t3","name":"bash","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t3","content":"ok","is_error":false}]}
            ],
            "responses": [resp(1), resp(2), resp(3)],
            "api_pending": false, "tool_pending": null, "completed_tool_ids": ["t1","t2","t3"]
        }).to_string();
        let checkpoint = checkpoint.as_str();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api','fake',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)").bind(&run).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(&conv).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        for (turn, status) in [(&a,"completed"),(&b,"failed"),(&c,"failed")] {
            sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web',?,'ok',?)").bind(turn).bind(&conv).bind(&run).bind(status).bind(&now).execute(&env.app.db).await.unwrap();
        }
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,11,812,?,?,?,?)").bind(&session).bind(&bot).bind(&run).bind(&c).bind(checkpoint).bind(&now).bind(&now).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        for (turn, rev, outcome, text) in [(&a,800_i64,"completed",Some("first")),(&b,805_i64,"failed",None),(&c,812_i64,"failed",None)] {
            sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,?,?,?,11,?,?,?,'applied',?,?)").bind(&session).bind(&bot).bind(&run).bind(turn).bind(rev).bind(outcome).bind(text).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        }
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'assistant','first','transcript',?)").bind(crate::db::ulid()).bind(&conv).bind(&a).bind(&now).execute(&env.app.db).await.unwrap();
        for (ord, turn) in [(55_i64,&a),(82,&b),(144,&c)] {
            sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at,finished_at) VALUES(?,?,?,?,?,11,?,'fixture','standard','unknown',123,'fixture',?,?)").bind(crate::db::ulid()).bind(&session).bind(&bot).bind(&run).bind(turn).bind(ord).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        }
        let app=crate::testing::restart_app(&env).await;
        app.set_startup_ready(false);
        let r = super::super::startup_recovery::recover_all(&app).await;
        assert!(r.is_ok(), "{:?}", r.as_ref().err().map(|e| e.code()));
    }

    /// 2026-11-15 pt-hub：長回合被 5 分鐘閒置 reconcile 收尾（turn=completed_fallback），runner 完成時被 fence 掉而沒有 receipt，
    /// sweeper 再清掉 pending checkpoint。session owner 沒有 receipt 不能讓這個 session 所有歷史 receipt 都判 unresolved
    /// （隔離全部 API bot），之後也要能 resume。
    #[tokio::test]
    async fn an_owner_turn_closed_without_a_receipt_does_not_quarantine_and_can_resume() {
        let env=crate::testing::env().await;
        let bot=crate::db::ulid(); let old_run=crate::db::ulid(); let run=crate::db::ulid(); let conv=crate::db::ulid(); let session=crate::db::ulid();
        let (a,b)=(crate::db::ulid(),crate::db::ulid());
        let now=crate::db::now();
        let checkpoint = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"bash","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":["t1"]}"#;
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api','fake',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'exited',?)").bind(&old_run).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'exited',?)").bind(&run).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(&conv).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','completed','ok',?)").bind(&a).bind(&conv).bind(&old_run).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','completed_fallback','ok',?)").bind(&b).bind(&conv).bind(&run).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,2,9,?,?,?,?)").bind(&session).bind(&bot).bind(&run).bind(&b).bind(checkpoint).bind(&now).bind(&now).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,?,?,?,1,4,'completed','first','applied',?,?)").bind(&session).bind(&bot).bind(&old_run).bind(&a).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'assistant','first','transcript',?)").bind(crate::db::ulid()).bind(&conv).bind(&a).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'system','closed by reconcile','system',?)").bind(crate::db::ulid()).bind(&conv).bind(&b).bind(&now).execute(&env.app.db).await.unwrap();
        let app=crate::testing::restart_app(&env).await;
        app.set_startup_ready(false);
        let r = super::super::startup_recovery::recover_all(&app).await;
        assert!(r.is_ok(), "{:?}：沒有 receipt 的終態 owner 不該讓歷史 receipt 判 unresolved", r.as_ref().err().map(|e| e.code()));
        let mut tx = crate::db::begin_write(&app.db).await.unwrap();
        assert!(super::super::native_resume::prove_on(&mut tx, &bot, &run, super::super::native_resume::ProofMode::StartBeforeRun).await.is_ok(), "沒有 receipt 的終態 owner 也要能 resume");
        tx.rollback().await.unwrap();
        // owner 還有未 applied 的 receipt 時仍要擋（不放寬真正未結清的情況）。
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at) VALUES(?,?,?,?,2,9,'completed','late','pending',?)").bind(&session).bind(&bot).bind(&run).bind(&b).bind(&now).execute(&app.db).await.unwrap();
        let mut tx = crate::db::begin_write(&app.db).await.unwrap();
        assert!(super::super::native_resume::prove_on(&mut tx, &bot, &run, super::super::native_resume::ProofMode::StartBeforeRun).await.is_err());
        tx.rollback().await.unwrap();
    }
}
