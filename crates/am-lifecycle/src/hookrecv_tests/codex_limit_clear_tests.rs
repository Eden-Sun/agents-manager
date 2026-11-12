use super::*;
use crate::quota::{quota_key, LimitHit, Quota};

fn hit() -> Quota {
    Quota {
        five_hour: None,
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: Some(LimitHit { message: "You've hit your usage limit.".into(), until: None, at: db::now(), bucket: None }),
        plan: None,
        updated_at: db::now(),
        source: "codex-limit-hit".into(),
        account: None,
        host: "local".into(),
    }
}

/// H1（review 2026-09-16）：`cx2` 有自己的 `CODEX_HOME`，撞限寫在 `codex:cx2`；以前成功回合一律清裸
/// `codex`，於是 cx2 的撞限（沒寫時間＝永不過期）卡到重啟，反而把預設帳號**真的**撞限清掉。
#[tokio::test]
async fn a_codex_turn_clears_the_limit_on_its_own_account_only() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    e.app
        .cfg
        .update(|cfg| {
            cfg.identities = vec![crate::config::IdentityCfg {
                name: "cx2".into(),
                kind: "codex".into(),
                host: None,
                env: [("CODEX_HOME".to_string(), "$HOME/.codex-cx2".to_string())].into(),
                args: vec![],
            }];
            Ok(())
        })
        .await
        .unwrap();
    let bot = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, identity, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
         VALUES (?,?,'cx2-bot','codex','cx2','[]',0,1,'tok','user',?)",
    )
    .bind(&bot)
    .bind(&e.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    {
        let mut q = app.quotas.lock().await;
        q.insert(quota_key("local", "codex:cx2"), hit());
        q.insert(quota_key("local", "codex"), hit());
    }
    let body = HookBody {
        bot_id: bot.clone(),
        provider: "codex".into(),
        payload: serde_json::json!({"type": "agent-turn-complete", "thread-id": "t", "turn-id": "u",
                                    "input-messages": ["hi"], "last-assistant-message": "done"}),
        received_at: None,
        truncated: false,
        run_id: None,
    };
    let _ = process_locked(&app, &body).await;

    let q = app.quotas.lock().await;
    assert!(q.get("codex:cx2").unwrap().limit_hit.is_none(), "cx2 自己的撞限要被清掉");
    assert!(q.get("codex").unwrap().limit_hit.is_some(), "預設帳號的撞限不是 cx2 的回合能證明解除的");
}
