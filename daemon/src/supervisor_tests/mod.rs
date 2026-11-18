//! #1862 S1：從 `crates/am-supervisor/src/supervisor/mod.rs` 搬來的 daemon 整合測試（要 `state::App`／`testing::env`／runner）；
//! 測試名 `supervisor::tests::…` → `supervisor_tests::tests::…`。測試本文與斷言不變，只有 `use super::*` 改成 `use crate::supervisor::{…}`。

mod health;
mod setup;
mod watchdog;
mod cli_refresh;
mod role_faults;
mod failover;
mod responder;
mod controller;
pub(crate) mod idle_sleep;
mod maintenance;
mod incidents;
mod timing;
mod bot_requests;
mod build_info;
pub(crate) mod flow_fixture;
mod store;

mod tests {
    use crate::lifecycle::LcError;
    use crate::supervisor::{sanitized_state, start_manager, store};
    use std::sync::Arc;

    fn queued_turns(state: &serde_json::Value, bot_id: &str) -> i64 {
        state["bots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|bot| bot["id"] == bot_id)
            .unwrap_or_else(|| panic!("missing bot {bot_id}: {state}"))["queued_turns"]
            .as_i64()
            .unwrap()
    }

    #[tokio::test]
    async fn nr1892_sanitized_state_preserves_run_identity_without_private_fields() {
        let e = crate::testing::env().await;
        for (i, (adopted, native, terminal, reason)) in [
            (1, None, None, Some("missing_instance_identity")),
            (1, Some(" "), Some("\t"), Some("missing_instance_identity")),
            (1, Some("native"), None, None),
            (1, None, Some("terminal"), None),
            (0, None, None, None),
        ].into_iter().enumerate() {
            let bot = crate::testing::claude_bot(&e.app, &e.project_id, &format!("identity-{i}")).await;
            let run_id = crate::testing::fake_run(&e.app, &bot.id).await;
            sqlx::query("UPDATE bots SET env_json='{\"SECRET\":\"private-fixture\"}', args_json='[\"private-fixture\"]', hook_token='private-fixture' WHERE id=?")
                .bind(&bot.id).execute(&e.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET adopted=?, native_session_id=?, terminal_id=?, runtime_identity='private-fixture' WHERE id=?")
                .bind(adopted).bind(native).bind(terminal).bind(&run_id).execute(&e.app.db).await.unwrap();
            let state = sanitized_state(&e.app).await.unwrap();
            let row = state["bots"].as_array().unwrap().iter().find(|b| b["id"] == bot.id).unwrap();
            assert_eq!(row["run"]["adopted"], serde_json::json!(adopted));
            assert_eq!(row["run"]["native_session_id"], serde_json::json!(native));
            assert_eq!(row["run"]["terminal_id"], serde_json::json!(terminal));
            assert_eq!(row["run"]["identity_recovery_reason"], serde_json::json!(reason));
            assert!(!state.to_string().contains("private-fixture"), "sanitization must still omit env/args/token/runtime identity");
        }
    }

    #[tokio::test]
    async fn an_unreadable_queued_turn_count_fails_the_snapshot_and_recovers() {
        let e = crate::testing::env().await;
        let app: Arc<crate::state::App> = e.app.clone();
        let bot_id = "queue-count-owner";
        let now = crate::db::now();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, hook_token, created_at)
             VALUES (?, ?, 'queue-count-owner', 'claude', 'token', ?)",
        )
        .bind(bot_id)
        .bind(&e.project_id)
        .bind(&now)
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = crate::db::conversation_id(&app.db, bot_id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, created_at)
             VALUES ('queued-count-test', ?, 'web', 'queued', ?)",
        )
        .bind(conversation_id)
        .bind(&now)
        .execute(&app.db)
        .await
        .unwrap();

        assert_eq!(queued_turns(&sanitized_state(&app).await.unwrap(), bot_id), 1, "test setup has queued work");
        crate::testing::make_table_unreadable(&app, "turns").await;
        let result = sanitized_state(&app).await;
        crate::testing::make_table_readable(&app, "turns").await;
        assert!(result.is_err(), "an unreadable queued-turn count must not publish a false idle snapshot: {result:?}");
        assert_eq!(queued_turns(&sanitized_state(&app).await.unwrap(), bot_id), 1, "after DB recovery the real count is visible");
    }

    #[tokio::test]
    async fn starting_while_the_previous_run_is_still_stopping_is_a_conflict_not_a_fake_success() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "agm-stopping").await.id;
        crate::testing::fake_run(&app, &bot).await;
        store::get_or_init(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisors SET bot_id=?, generation=1, status_detail='before' WHERE id=?")
            .bind(&bot).bind(store::SUPERVISOR_ID).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET state='stopping' WHERE bot_id=?").bind(&bot).execute(&app.db).await.unwrap();

        let err = start_manager(&app, None).await.unwrap_err();
        let LcError::Conflict(v) = &err else { panic!("要是 409：{err:?}") };
        assert_eq!(v["reason"], "run_stopping");
        let s = store::get_or_init(&app.db).await.unwrap();
        assert_eq!(s.status_detail.as_deref(), Some("before"), "沒啟動就不能改狀態");
    }

    /// #1477：手機 Remote Control 已廢，`GET /api/supervisor` 不再有遠端入口的狀態（頂層與協調者那一格都一樣）。
    #[tokio::test]
    async fn the_status_payload_no_longer_carries_a_remote_entry() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let v = crate::supervisor::status_json(&app).await.unwrap();
        for key in ["remote", "remote_provider"] {
            assert!(v.get(key).is_none(), "{key} 已拿掉：{v}");
        }
        assert!(v["responder"].get("remote_control").is_none(), "協調者那格也不再講 remote_control：{v}");
    }
}
