use super::*;
use crate::testing::{claude_bot, env, fake_run, restart_app, Env};

fn stop_body(bot_id: &str, prompt_id: &str) -> HookBody {
    HookBody {
        bot_id: bot_id.into(),
        provider: "claude".into(),
        payload: json!({
            "hook_event_name": "Stop",
            "session_id": "sess-1",
            "prompt_id": prompt_id,
            "last_assistant_message": "做完了",
        }),
        received_at: Some("2026-09-17T12:00:00.000Z".into()),
        truncated: false,
        run_id: None,
    }
}

fn headers(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("X-AM-Bot-Token", token.parse().unwrap());
    h
}

async fn inbox_rows(e: &Env) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM hook_events").fetch_one(&e.app.db).await.unwrap()
}

/// 寫不進收件匣就**不准回 200**。回 200 再掉事件是無聲的資料遺失；回 503 送端會把同一份 body
/// 寫進 hook-spool.jsonl，replay 補得回來。
#[tokio::test]
async fn a_hook_that_cannot_be_persisted_is_not_answered_with_200() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
    // 收件匣壞掉（磁碟滿、schema 沒跟上……都是同一種下場）。
    sqlx::query("DROP TABLE hook_events").execute(&e.app.db).await.unwrap();

    let (code, _) = receive(
        State(e.app.clone()),
        Path("claude".into()),
        headers("tok"),
        Json(stop_body(&bot.id, "p1")),
    )
    .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "寫不進去就不是 200");
    assert!(!code.is_success(), "送端要看得出來該 spool");
}

/// #304：沒帶對 token 的人分不出「有過這顆 bot」與「沒有」——不存在、已刪、token 不對都是同一個 401；
/// 410 只給 token 正確者（被刪 bot 還活著的 agent 要知道自己該停）。
#[tokio::test]
async fn the_hook_endpoint_does_not_reveal_whether_a_bot_id_exists() {
    let e = env().await;
    let live = claude_bot(&e.app, &e.project_id, "live").await;
    let gone = claude_bot(&e.app, &e.project_id, "gone").await;
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&gone.id).execute(&e.app.db).await.unwrap();
    let mut seen = Vec::new();
    for (id, token) in [(&live.id, "wrong"), (&gone.id, "wrong"), (&gone.id, ""), (&"no-such-bot".to_string(), "wrong")] {
        let (code, Json(body)) =
            receive(State(e.app.clone()), Path("claude".into()), headers(token), Json(stop_body(id, "p1"))).await;
        seen.push((code, body));
    }
    assert!(seen.iter().all(|s| *s == seen[0]), "沒有有效 token：存在、已刪、不存在的回應要一模一樣：{seen:?}");
    assert_eq!(seen[0].0, StatusCode::UNAUTHORIZED);
    let (code, _) =
        receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(stop_body(&gone.id, "p1"))).await;
    assert_eq!(code, StatusCode::GONE, "token 正確的已刪 bot 仍是 410");
    assert_eq!(inbox_rows(&e).await, 0, "任何一種都不能寫進收件匣");
}

/// 收下了才 200；而且 200 的當下那一列已經在 DB 裡（不是「待會背景寫」）。
#[tokio::test]
async fn a_persisted_hook_is_in_the_database_before_the_200_comes_back() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
    let (code, _) =
        receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(stop_body(&bot.id, "p1"))).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(inbox_rows(&e).await, 1, "200 回來的時候列已經在了");
    let pending = crate::hook_inbox::pending(&e.app.db, &db::now(), 10).await.unwrap();
    assert_eq!(pending.len(), 1, "還沒處理，但已經耐久收下");
}

/// 重送同一則 hook 不會變成兩列、也不會變成第二次完成。
#[tokio::test]
async fn the_same_hook_sent_twice_does_not_become_two_events() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
    for _ in 0..3 {
        let (code, _) = receive(
            State(e.app.clone()),
            Path("claude".into()),
            headers("tok"),
            Json(stop_body(&bot.id, "p1")),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "重送照樣是 200：那則事件的確已經收下了");
    }
    assert_eq!(inbox_rows(&e).await, 1, "三次重送只有一列");

    // 處理過一輪之後再重送，也不會多出一列（去重看的是事件身份，不是有沒有處理過）。
    crate::runners::hook_inbox::drain_once(&e.app).await.unwrap();
    let (code, _) =
        receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(stop_body(&bot.id, "p1"))).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(inbox_rows(&e).await, 1, "處理完之後重送還是同一列");
}

/// 兩則**不同**的 hook 不會被去重吃掉。
#[tokio::test]
async fn two_different_hooks_are_both_accepted() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
    for pid in ["p1", "p2"] {
        let (code, _) = receive(
            State(e.app.clone()),
            Path("claude".into()),
            headers("tok"),
            Json(stop_body(&bot.id, pid)),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
    }
    assert_eq!(inbox_rows(&e).await, 2);
}

/// ACK 之後、處理之前 daemon 掛掉：重啟仍然處理得到那則事件（驗收第 2、4 條）。
///
/// 「重啟」在這裡就是 `restart_app`（同一個資料目錄開一顆新的 App）＋ worker 開工做的第一件事
/// （`drain_once`）——正式路徑上 `spawn_worker` 也是先 drain 再等。
#[tokio::test]
async fn an_accepted_hook_survives_a_restart_before_it_is_processed() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
    let run = fake_run(&e.app, &bot.id).await;

    let (code, _) =
        receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(stop_body(&bot.id, "p1"))).await;
    assert_eq!(code, StatusCode::OK);
    // 沒有人處理它就「掛掉」了。
    assert_eq!(crate::hook_inbox::pending(&e.app.db, &db::now(), 10).await.unwrap().len(), 1);

    let restarted = restart_app(&e).await;
    let done = crate::runners::hook_inbox::drain_once(&restarted).await.unwrap();
    assert_eq!(done, 1, "重啟後補處理");
    assert!(
        crate::hook_inbox::pending(&restarted.db, &db::now(), 10).await.unwrap().is_empty(),
        "處理完就不在待辦裡了"
    );
    // 事件真的走到了 §6.7（session id 回填到 run 上）。
    let sid: Option<String> =
        sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&restarted.db).await.unwrap();
    assert_eq!(sid.as_deref(), Some("sess-1"), "hook 真的被處理了，不是只被標成完成");
}

/// StatusLine 是單槽訊號，刻意不進收件匣（掉一格只是晚一次重繪）。
#[tokio::test]
async fn statusline_stays_fire_and_forget() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
    let body = HookBody {
        bot_id: bot.id.clone(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": "StatusLine", "status_line": "…"}),
        received_at: Some("2026-09-17T12:00:00.000Z".into()),
        truncated: false,
        run_id: None,
    };
    let (code, _) = receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(body)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(inbox_rows(&e).await, 0, "重繪訊號不佔佇列");
}

/// 壞 token / 已刪除的 bot 在收下之前就被擋掉：收件匣不會被沒有身份的事件灌爆。
#[tokio::test]
async fn a_rejected_hook_never_reaches_the_inbox() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
    let (code, _) = receive(
        State(e.app.clone()),
        Path("claude".into()),
        headers("wrong"),
        Json(stop_body(&bot.id, "p1")),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert_eq!(inbox_rows(&e).await, 0);
}
