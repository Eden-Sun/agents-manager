use super::*;

struct Fx {
    e: crate::testing::Env,
    alfa: String,
    bravo: String,
    target: String,
}

/// 寄件的 `alfa`（token `tok-alfa`）、收件的 `target`（grok、閒著、pane 活著）、旁觀的 `bravo`（`tok-bravo`）。
async fn fx() -> Fx {
    let e = crate::testing::env().await;
    let mut ids = vec![];
    for name in ["alfa", "bravo", "target"] {
        let b = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
        sqlx::query("UPDATE bots SET hook_token=? WHERE id=?").bind(format!("tok-{name}")).bind(&b.id).execute(&e.app.db).await.unwrap();
        ids.push(b.id);
    }
    let target = ids[2].clone();
    sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&target).execute(&e.app.db).await.unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-relay','target','test',1,?)",
    )
    .bind(db::ulid())
    .bind(&target)
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    e.herdr.live_pane("pane-relay", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
    Fx { e, alfa: ids[0].clone(), bravo: ids[1].clone(), target }
}

/// `send` 只做得出「有帶一個合法字串」與「完全不帶」。空字串與非 UTF-8 的 header 值要另外拼。
async fn send_raw_token(app: &Arc<App>, to: &str, relay_from: &str, raw: &[u8], crid: &str) -> (StatusCode, Value) {
    let mut h = HeaderMap::new();
    h.insert("X-AM-Bot-Token", axum::http::HeaderValue::from_bytes(raw).unwrap());
    let body = PromptIn {
        expect_result: false,
        text: "幫我看一下".into(),
        client_request_id: Some(crid.into()),
        attachments: vec![],
        relay_from: Some(relay_from.into()),
        ack: false,
        reply_to: None,
        send_now: false,
        start_if_stopped: false,
        queue_if_busy: false,
        clear_draft: false,
        submit_draft: false,
        expect_draft_token: None,
        share_reply_visible: false,
        track: None, cluster: None,
    };
    let resp = match prompt_bot(State(app.clone()), Path(to.to_string()), h, Extension(RequestPrincipal::User), Json(body)).await {
        Ok(r) => r,
        Err(err) => err.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn send(app: &Arc<App>, to: &str, relay_from: &str, token: Option<&str>, crid: &str) -> (StatusCode, Value) {
    let mut h = HeaderMap::new();
    if let Some(t) = token {
        h.insert("X-AM-Bot-Token", t.parse().unwrap());
    }
    let body = PromptIn {
        expect_result: false,
        text: "幫我看一下".into(),
        client_request_id: Some(crid.into()),
        attachments: vec![],
        relay_from: Some(relay_from.into()),
        ack: false,
        reply_to: None,
        send_now: false,
        start_if_stopped: false,
        queue_if_busy: false,
        clear_draft: false,
        submit_draft: false,
        expect_draft_token: None,
        share_reply_visible: false,
        track: None, cluster: None,
    };
    let resp = match prompt_bot(State(app.clone()), Path(to.to_string()), h, Extension(RequestPrincipal::User), Json(body)).await {
        Ok(r) => r,
        Err(err) => err.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn counts(app: &Arc<App>) -> (i64, i64) {
    let t = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
    let m = sqlx::query_scalar("SELECT COUNT(*) FROM messages").fetch_one(&app.db).await.unwrap();
    (t, m)
}

async fn stored(app: &Arc<App>, message_id: &str) -> (Option<String>, i64) {
    sqlx::query_as("SELECT relay_from, relay_unverified FROM messages WHERE id=?").bind(message_id).fetch_one(&app.db).await.unwrap()
}

/// issue 的紅測試：拿自己（bravo）的 token 冒 alfa 的名 → 403，一個字都沒寫。
#[tokio::test]
async fn another_bots_token_cannot_speak_as_alfa() {
    let f = fx().await;
    let (status, body) = send(&f.e.app, &f.target, &f.alfa, Some("tok-bravo"), "mismatch").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "relay_from_mismatch");
    assert_eq!(counts(&f.e.app).await, (0, 0), "被拒的冒名不留 turn、不留訊息");
}

/// `daemon` 是 daemon 自己的哨符：HTTP 帶進來一律 403（帶什麼 token 都一樣）。
#[tokio::test]
async fn nobody_can_claim_to_be_the_daemon_over_http() {
    let f = fx().await;
    for (token, crid) in [(None, "d-none"), (Some("tok-alfa"), "d-alfa")] {
        let (status, body) = send(&f.e.app, &f.target, crate::agent_relay::DAEMON_SENDER, token, crid).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{token:?}: {body}");
        assert_eq!(body["reason"], "relay_from_reserved");
    }
    assert_eq!(counts(&f.e.app).await, (0, 0));
}

/// 帶自己的 token：照送，記成已驗證。
#[tokio::test]
async fn a_bot_with_its_own_token_is_a_verified_relay() {
    let f = fx().await;
    let (status, body) = send(&f.e.app, &f.target, &f.alfa, Some("tok-alfa"), "verified").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored(&f.e.app, body["message_id"].as_str().unwrap()).await, (Some(f.alfa.clone()), 0));
}

/// #410 相容期結束：沒帶 token 的 `relay_from` 一律 403 `relay_from_token_required`，
/// 什麼都不寫（沒有 turn、沒有訊息、沒有 `message_added`）。
#[tokio::test]
async fn an_unsigned_relay_is_refused_and_writes_nothing() {
    let f = fx().await;
    let mut rx = f.e.app.subscribe();
    let (status, body) = send(&f.e.app, &f.target, &f.alfa, None, "unsigned").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "relay_from_token_required", "{body}");
    assert_eq!(counts(&f.e.app).await, (0, 0));
    while let Ok(ev) = rx.try_recv() {
        assert_ne!(ev.kind, "message_added", "被拒絕的 relay 不能推訊息");
    }
}

/// `start_if_stopped`（另一個寫訊息的地方）一樣拒絕，不留下排隊中的訊息。
#[tokio::test]
async fn the_start_if_stopped_path_refuses_an_unsigned_relay_too() {
    let f = fx().await;
    let stopped = crate::testing::claude_bot(&f.e.app, &f.e.project_id, "sleeper").await;
    let body = PromptIn {
        expect_result: false,
        text: "起來後看一下".into(),
        client_request_id: Some("sis".into()),
        attachments: vec![],
        relay_from: Some(f.alfa.clone()),
        ack: false,
        reply_to: None,
        send_now: false,
        start_if_stopped: true,
        queue_if_busy: false,
        clear_draft: false,
        submit_draft: false,
        expect_draft_token: None,
        share_reply_visible: false,
        track: None, cluster: None,
    };
    let err = prompt_bot(State(f.e.app.clone()), Path(stopped.id.clone()), HeaderMap::new(), Extension(RequestPrincipal::User), Json(body)).await.expect_err("unsigned relay");
    assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");
    assert_eq!(counts(&f.e.app).await, (0, 0));
}

/// 框裡的草稿（409 `composer_busy`）只有使用者自己處理：`clear_draft`／`submit_draft` 沒帶 `expect_draft_token` 是 400，
/// bot 轉送的也是 400——都在碰 pane 之前擋下，一個鍵都不按。
#[tokio::test]
async fn draft_actions_need_the_confirmed_draft_and_the_user_themself() {
    let f = fx().await;
    // Keep the default changing spinner: background redraws must not revoke a draft action.
    // Real herdr returns zero revision; authorization comes from the composer frame.
    f.e.herdr.live_pane("pane-relay", crate::testing::LivePane { zero_revision: true, width: Some(120), boxed: true, composer: vec!["假草稿".into()], ..Default::default() });
    let call = |clear: bool, submit: bool, expect: Option<&str>, relay: Option<String>| {
        let body = PromptIn {
        expect_result: false,
            text: if submit { String::new() } else { "我的".into() },
            client_request_id: Some(db::ulid()),
            attachments: vec![],
            relay_from: relay,
            ack: false,
            reply_to: None,
            send_now: false,
            start_if_stopped: false,
            queue_if_busy: false,
            clear_draft: clear,
            submit_draft: submit,
            expect_draft_token: expect.map(str::to_string),
            share_reply_visible: false,
            track: None, cluster: None,
        };
        // 轉送要帶那顆 bot 自己的 token（#410 後沒帶就是 403），這樣測的才是「草稿動作不給轉送」那一條。
        let mut headers = HeaderMap::new();
        if body.relay_from.is_some() {
            headers.insert("X-AM-Bot-Token", "tok-alfa".parse().unwrap());
        }
        prompt_bot(State(f.e.app.clone()), Path(f.target.clone()), headers, Extension(RequestPrincipal::User), Json(body))
    };
    let busy = call(false, false, None, None).await.expect_err("composer_busy");
    let token = match busy {
        LcError::Conflict(body) => body["draft_token"].as_str().expect("full draft token").to_string(),
        other => panic!("expected composer_busy, got {other:?}"),
    };
    for (clear, submit) in [(true, false), (false, true)] {
        let err = call(clear, submit, None, None).await.expect_err("no expect_draft_token");
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        let err = call(clear, submit, Some(&token), Some(f.alfa.clone())).await.expect_err("relayed");
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
    }
    assert!(f.e.herdr.calls_to("pane.send_keys").is_empty());
    // A matching draft token cannot authorize Enter when the composer draft changes,
    // even though pane.read keeps returning revision 0. The provisional turn is withdrawn.
    let live = f.e.herdr.live.clone();
    crate::lifecycle::race_point::arm("draft_before_enter", &f.target, move || async move {
        live.lock().unwrap().get_mut("pane-relay").unwrap().composer = vec!["未確認的新草稿".into()];
    });
    let err = call(false, true, Some(&token), None).await.expect_err("changed snapshot");
    match err {
        LcError::Conflict(body) => {
            assert_eq!(body["reason"], "draft_changed", "{body}");
            assert_eq!(body["draft"], "未確認的新草稿", "{body}");
        }
        other => panic!("expected draft_changed, got {other:?}"),
    }
    assert!(f.e.herdr.calls_to("pane.send_keys").is_empty());
    assert_eq!(counts(&f.e.app).await, (0, 0), "no stale draft turn or message survives");
    f.e.herdr.live.lock().unwrap().get_mut("pane-relay").unwrap().composer = vec!["假草稿".into()];
    // 帶齊了就真的送出框裡那段（grok：沒有無損證據，unverified）。
    let resp = call(false, true, Some(&token), None).await.unwrap();
    let out: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(out["delivery"], "unverified", "{out}");
}

/// 寫給 AGM（協調者存在）的申請：對不上的 token 一樣 403、不進收件匣；沒帶也 403（#410 相容期已結束）。
#[tokio::test]
async fn requests_to_agm_follow_the_same_rule() {
    use crate::supervisor::bot_requests::flow_tests::{app, configure_responder};
    let app = app().await;
    configure_responder(&app).await;
    let inbox = || async { sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM supervisor_inbox").fetch_one(&app.db).await.unwrap() };
    let (status, body) = send(&app, "patrol", "w1", Some("tok-w2"), "agm-mismatch").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(inbox().await, 0);
    let (status, body) = send(&app, "patrol", "w1", Some("tok-w1"), "agm-signed").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = send(&app, "patrol", "w1", None, "agm-unsigned").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let verified: Vec<bool> = sqlx::query_scalar("SELECT json_extract(payload_json,'$.sender_verified') FROM supervisor_inbox ORDER BY created_at, id")
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(verified, vec![true]);
}

// --------------------------------------------- review 7ed32d94：同一張 #339 的收尾

/// 「有帶 `X-AM-Bot-Token` 但值是空的／非 UTF-8」以前掉進「沒帶」那一格，於是送一個壞掉的
/// header 就能走相容期冒名放行。有帶就得對得上：一律 403，一個字都不寫。
#[tokio::test]
async fn a_present_but_unusable_token_is_a_mismatch_not_a_missing_one() {
    let f = fx().await;
    // 空字串、只有空白、非 UTF-8（0xff 不是合法 UTF-8，但是合法的 header 位元組）。
    for (raw, label) in [(&b""[..], "empty"), (&b"   "[..], "blank"), (&b"\xff\xfe"[..], "not-utf8")] {
        let (status, body) = send_raw_token(&f.e.app, &f.target, &f.alfa, raw, label).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {body}");
        assert_eq!(body["reason"], "relay_from_mismatch", "{label}: {body}");
    }
    assert_eq!(counts(&f.e.app).await, (0, 0), "三種壞 token 都不能留下 turn 或訊息");
}

/// 帶錯 token 的呼叫端不該從狀態碼讀出「這個 bot id 存不存在」：不存在／已刪的 relay_from
/// 以前先查 bot 回 400、存在的才 403，一個一個試就掃得出 id。有帶 token 就一律先 403。
#[tokio::test]
async fn a_wrong_token_never_reveals_whether_the_bot_exists() {
    let f = fx().await;
    let gone = crate::testing::claude_bot(&f.e.app, &f.e.project_id, "gone").await;
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&gone.id).execute(&f.e.app.db).await.unwrap();

    let mut seen = vec![];
    for (from, crid) in [(f.alfa.as_str(), "probe-live"), ("01MNOSUCHBOTIDATALL0000000", "probe-absent"), (gone.id.as_str(), "probe-deleted")] {
        let (status, body) = send(&f.e.app, &f.target, from, Some("tok-bravo"), crid).await;
        seen.push((status, body["reason"].as_str().unwrap_or_default().to_string()));
    }
    assert_eq!(
        seen,
        vec![(StatusCode::FORBIDDEN, "relay_from_mismatch".to_string()); 3],
        "活著的、不存在的、已刪的：帶錯 token 一律同一個回應"
    );
    assert_eq!(counts(&f.e.app).await, (0, 0));

    // 沒帶 token、bot 不存在照舊是 400（那條路沒有 token 可以對，也沒有神諭可讀）。
    let (status, _) = send(&f.e.app, &f.target, "01MNOSUCHBOTIDATALL0000000", None, "absent-nosig").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// 票上要的 sender ≠ target：`relay_from` 指向收件的那顆 bot 自己，UI 會畫出「A → A」，
/// 而「這句話不是你自己想的」對自己沒有意義。400 `relay_self`，什麼都不寫。
#[tokio::test]
async fn a_bot_cannot_relay_a_message_to_itself() {
    let f = fx().await;
    // 連帶對自己那顆的正確 token 也不行：驗得出身份不代表這個來源標示講得通。
    sqlx::query("UPDATE bots SET hook_token='tok-target' WHERE id=?").bind(&f.target).execute(&f.e.app.db).await.unwrap();
    for (token, crid) in [(None, "self-none"), (Some("tok-target"), "self-signed")] {
        let (status, body) = send(&f.e.app, &f.target, &f.target, token, crid).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{token:?}: {body}");
        assert_eq!(body["reason"], "relay_self", "{token:?}: {body}");
    }
    assert_eq!(counts(&f.e.app).await, (0, 0), "自己送給自己不留 turn、不留訊息");

    // 送給**別顆** bot 照舊通：擋的是 from == to，不是「有 relay_from」。
    let (status, body) = send(&f.e.app, &f.target, &f.bravo, Some("tok-bravo"), "self-ok").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
