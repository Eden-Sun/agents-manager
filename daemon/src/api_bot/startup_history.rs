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
    format!("SELECT EXISTS(SELECT 1 FROM api_bot_sessions s JOIN bots b ON b.id=s.bot_id JOIN runs r ON r.id=? AND r.bot_id=b.id JOIN turns t ON t.id=? AND t.run_id=r.id JOIN conversations c ON c.id=t.conversation_id AND c.bot_id=b.id JOIN api_bot_join_receipts j ON j.session_id=s.id AND j.bot_id=b.id AND j.run_id=r.id AND j.turn_id=t.id JOIN api_bot_join_receipts latest ON latest.session_id=s.id AND latest.bot_id=b.id AND latest.run_id=s.owner_run_id AND latest.turn_id=s.owner_turn_id AND latest.generation=s.generation AND latest.revision=s.revision AND latest.state='applied' WHERE s.id=? AND b.id=? AND b.kind='claude-api' AND r.state IN {states} AND j.state='applied' AND j.generation=? AND j.revision=? AND j.generation<=s.generation AND {owner} AND t.delivery='ok' AND ((j.outcome='completed' AND t.status='completed' AND j.final_text IS NOT NULL AND (SELECT count(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant')=1 AND EXISTS(SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.conversation_id=c.id AND m.role='assistant' AND m.source='transcript' AND m.incomplete=0 AND m.rewound_at IS NULL AND m.content=j.final_text)) OR (j.outcome IN ('failed','cancelled') AND t.status='failed' AND j.final_text IS NULL AND (SELECT count(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant')=0)))")
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
}
