use super::*;

#[tokio::test]
async fn messages_page_by_insert_order_when_ids_are_not_monotonic() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let bot_id = "messages-test-bot";
    sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,'claude','tok',?)")
        .bind(bot_id)
        .bind(&e.project_id)
        .bind(bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
    for id in ["m3", "m1", "m2"] {
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?, 'user', ?, 'web', ?)",
        )
        .bind(id)
        .bind(&conv)
        .bind(id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    }

    let mut before: Option<String> = None;
    let expected = ["m2", "m1", "m3"];
    for (page, want) in expected.iter().enumerate() {
        let mut q = HashMap::from([(String::from("limit"), String::from("1"))]);
        if let Some(cursor) = &before {
            q.insert("before".into(), cursor.clone());
        }
        let Json(body) = get_messages(State(app.clone()), Path(bot_id.into()), Query(q)).await.unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], *want);
        assert_eq!(body["has_more"], page + 1 < expected.len());
        before = Some(messages[0]["id"].as_str().unwrap().to_string());
    }

    let q = HashMap::from([(String::from("before"), String::from("missing"))]);
    assert!(matches!(
        get_messages(State(app), Path(bot_id.into()), Query(q)).await,
        Err(LcError::NotFoundValue(_))
    ));
}

/// 代發來源的 bot 已收掉（`deleted_at` 有值）仍回得出名字；Bot principal 看不到樹外的名字。
#[tokio::test]
async fn relay_from_keeps_the_name_of_a_retired_bot_for_users_only() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (listener, relay, target) = ("relay-listener-bot", "relay-retired-child", "relay-target-bot");
    for (id, name, deleted) in [(relay, "lx1752", Some(db::now())), (listener, "listener", None), (target, "target", None)] {
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, deleted_at, created_at) VALUES (?,?,?,'claude','tok',?,?)")
            .bind(id)
            .bind(&e.project_id)
            .bind(name)
            .bind(deleted)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
    }
    let conv = db::conversation_id(&app.db, target).await.unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, relay_from, created_at) VALUES ('relayed', ?, 'user', 'x', 'web', ?, ?)")
        .bind(&conv)
        .bind(relay)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

    let Json(user) = get_messages_with_relay_names(State(app.clone()), Path(target.into()), Query(HashMap::new()), Extension(RequestPrincipal::User)).await.unwrap();
    let m = &user["messages"][0];
    assert_eq!(m["relay_from_name"], "lx1752");
    assert_eq!(m["relay_from_deleted"], true);

    let Json(bot) = get_messages_with_relay_names(State(app), Path(target.into()), Query(HashMap::new()), Extension(RequestPrincipal::Bot(listener.into()))).await.unwrap();
    assert!(bot["messages"][0].get("relay_from_name").is_none(), "{bot}");
    assert_eq!(bot["messages"][0]["relay_from"], relay);
}

/// 群集派工：成員那邊的 relay 訊息與管理者那邊的結果通知都帶 `via_cluster`／`assignee_bot_id`／`assignee_name`；
/// 一般派工（`cluster IS NULL`）不帶。
#[tokio::test]
async fn cluster_dispatch_messages_carry_cluster_and_assignee_but_plain_relay_does_not() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    for (id, name) in [("ct-mgr", "ops"), ("ct-member", "hk2")] {
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,'claude','tok',?)")
            .bind(id).bind(&e.project_id).bind(name).bind(db::now()).execute(&app.db).await.unwrap();
    }
    let (mc, kc) = (db::conversation_id(&app.db, "ct-mgr").await.unwrap(), db::conversation_id(&app.db, "ct-member").await.unwrap());
    for (req, cluster) in [("rq-cluster", Some("haiku")), ("rq-plain", None)] {
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,?,'x','[]','answered',?,?)")
            .bind(req).bind("ct-mgr").bind("ct-member").bind(req).bind(db::now()).bind(cluster).execute(&app.db).await.unwrap();
        // 成員那邊收到的 relay 訊息、管理者那邊的結果通知。
        for (conv, turn, crid, from) in [(&kc, format!("t-member-{req}"), format!("relay-request:{req}"), "ct-mgr"), (&mc, format!("t-notice-{req}"), format!("relay-result:{req}:r0"), "ct-member")] {
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, client_request_id, created_at) VALUES (?,?,'external','completed',?,?)")
                .bind(&turn).bind(conv).bind(crid).bind(db::now()).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, created_at) VALUES (?,?,?,'user','x','web',?,?)")
                .bind(format!("m-{turn}")).bind(conv).bind(&turn).bind(from).bind(db::now()).execute(&app.db).await.unwrap();
        }
    }
    for (bot, req_ok) in [("ct-member", "t-member-"), ("ct-mgr", "t-notice-")] {
        let Json(body) = get_messages_with_relay_names(State(app.clone()), Path(bot.into()), Query(HashMap::new()), Extension(RequestPrincipal::User)).await.unwrap();
        let find = |turn: String| body["messages"].as_array().unwrap().iter().find(|m| m["turn_id"] == turn.as_str()).unwrap().clone();
        let hit = find(format!("{req_ok}rq-cluster"));
        assert_eq!(hit["via_cluster"], "haiku", "{hit}");
        assert_eq!(hit["assignee_bot_id"], "ct-member");
        assert_eq!(hit["assignee_name"], "hk2");
        let plain = find(format!("{req_ok}rq-plain"));
        assert!(plain.get("via_cluster").is_none() && plain.get("assignee_bot_id").is_none(), "{plain}");
    }
}

/// WS 的 `message_added` 幀也要帶來源名字（已收掉的 child 照樣認得出）；其他事件原樣。
#[tokio::test]
async fn ws_message_added_frames_carry_the_relay_name_of_a_retired_bot() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, deleted_at, created_at) VALUES ('ws-retired-child',?,'lx1752','claude','tok',?,?)")
        .bind(&e.project_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let frame = |kind: &str, relay: &str| crate::state::WsEvent { seq: 1, kind: kind.into(), data: json!({"bot_id": "x", "message": {"id": "m", "relay_from": relay}}) };
    let v: Value = serde_json::from_str(&ws_frame_text(&app, &frame("message_added", "ws-retired-child")).await).unwrap();
    assert_eq!(v["data"]["message"]["relay_from_name"], "lx1752");
    assert_eq!(v["data"]["message"]["relay_from_deleted"], true);
    let daemon: Value = serde_json::from_str(&ws_frame_text(&app, &frame("message_added", "daemon")).await).unwrap();
    assert!(daemon["data"]["message"].get("relay_from_name").is_none());
    let other: Value = serde_json::from_str(&ws_frame_text(&app, &frame("turn_updated", "ws-retired-child")).await).unwrap();
    assert!(other["data"]["message"].get("relay_from_name").is_none());
}

/// 同一毫秒的訊息、id 不照插入序：每一則帶 `seq`（SQLite rowid，單調遞增），前端在 created_at 相同時靠它排，
/// 不必退回 id（ULID 的隨機段在同一毫秒內不單調）。REST 分頁與 WS `message_added` 都要帶。
#[tokio::test]
async fn every_message_carries_the_monotonic_insert_seq_on_rest_and_ws() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let bot = crate::testing::claude_bot(&app, &e.project_id, "seq-bot").await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let at = db::now();
    for id in ["m3", "m1", "m2"] {
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?, 'user', ?, 'web', ?)")
            .bind(id)
            .bind(&conv)
            .bind(id)
            .bind(&at)
            .execute(&app.db)
            .await
            .unwrap();
    }
    let seqs = |v: &Value| -> Vec<(String, i64)> {
        v["messages"].as_array().unwrap().iter().map(|m| (m["id"].as_str().unwrap().to_string(), m["seq"].as_i64().unwrap_or(-1))).collect()
    };
    let Json(body) = get_messages(State(app.clone()), Path(bot.id.clone()), Query(HashMap::new())).await.unwrap();
    let rest = seqs(&body);
    assert_eq!(rest.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(), ["m3", "m1", "m2"], "REST 照插入序");
    assert!(rest.windows(2).all(|w| w[0].1 >= 0 && w[0].1 < w[1].1), "REST 的 seq 要單調遞增：{rest:?}");
    let rowid: i64 = sqlx::query_scalar("SELECT rowid FROM messages WHERE id='m1'").fetch_one(&app.db).await.unwrap();
    assert_eq!(rest[1].1, rowid, "seq 就是 rowid，跟 before= 分頁用的同一把尺");

    let mut rx = app.subscribe();
    let m = crate::lifecycle::insert_message(&app, &conv, None, "assistant", "new", "hook", false, None).await.unwrap();
    let mut pushed = None;
    while let Ok(ev) = rx.try_recv() {
        if ev.kind == "message_added" && ev.data["message"]["id"] == json!(m.id) {
            pushed = ev.data["message"]["seq"].as_i64();
        }
    }
    let last = rest.last().unwrap().1;
    assert!(pushed.is_some_and(|s| s > last), "message_added 的 seq 要比既有的大：{pushed:?} vs {last}");
}

async fn a_bot_with_conv(e: &crate::testing::Env, name: &str) -> (String, String) {
    let id = db::ulid();
    sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,'claude','tok',?)")
        .bind(&id)
        .bind(&e.project_id)
        .bind(name)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    let conv = db::conversation_id(&e.app.db, &id).await.unwrap();
    (id, conv)
}

async fn a_turn(db: &sqlx::SqlitePool, conv: &str, id: &str) {
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','completed','ok',?)")
        .bind(id)
        .bind(conv)
        .bind(db::now())
        .execute(db)
        .await
        .unwrap();
}

async fn a_message(db: &sqlx::SqlitePool, conv: &str, id: &str, turn: &str, role: &str, group: Option<&str>) {
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, created_at) VALUES (?,?,?,?,?, 'web', ?, ?)",
    )
    .bind(id)
    .bind(conv)
    .bind(turn)
    .bind(role)
    .bind(id)
    .bind(group)
    .bind(db::now())
    .execute(db)
    .await
    .unwrap();
}

async fn not_found_reason(r: Result<Json<Value>, LcError>) -> Option<String> {
    let resp = r.err().expect("應該是錯誤").into_response();
    if resp.status() != StatusCode::NOT_FOUND {
        return None;
    }
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice::<Value>(&body).unwrap()["reason"].as_str().map(str::to_string)
}

/// #766：游標指到已經被刪掉的訊息、或別顆 bot 的訊息，不能 400 也不能默默用全域 rowid 切出錯的一頁，
/// 一律 404 並帶 reason，呼叫端才知道要重載而不是重試。
#[tokio::test]
async fn a_before_cursor_that_is_gone_or_foreign_is_a_404_with_a_reason() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (mine, my_conv) = a_bot_with_conv(&e, "cursor-mine").await;
    let (_other, other_conv) = a_bot_with_conv(&e, "cursor-other").await;
    a_turn(&app.db, &my_conv, "t-mine").await;
    a_turn(&app.db, &other_conv, "t-other").await;
    a_message(&app.db, &my_conv, "m-mine-1", "t-mine", "user", None).await;
    a_message(&app.db, &other_conv, "m-other-1", "t-other", "user", None).await;
    a_message(&app.db, &my_conv, "m-mine-2", "t-mine", "assistant", None).await;
    a_message(&app.db, &my_conv, "m-gone", "t-mine", "assistant", None).await;
    sqlx::query("DELETE FROM messages WHERE id='m-gone'").execute(&app.db).await.unwrap();

    let page = |before: &str| Query(HashMap::from([("before".to_string(), before.to_string())]));
    // 自己的訊息照舊。
    let Json(ok) = get_messages(State(app.clone()), Path(mine.clone()), page("m-mine-2")).await.unwrap();
    assert_eq!(ok["messages"].as_array().unwrap().len(), 1);
    assert_eq!(not_found_reason(get_messages(State(app.clone()), Path(mine.clone()), page("m-gone")).await).await.as_deref(), Some("before_message_gone"));
    assert_eq!(not_found_reason(get_messages(State(app.clone()), Path(mine.clone()), page("never-existed")).await).await.as_deref(), Some("before_message_gone"));
    assert_eq!(
        not_found_reason(get_messages(State(app.clone()), Path(mine.clone()), page("m-other-1")).await).await.as_deref(),
        Some("before_message_not_in_conversation")
    );

}

/// `before=` 空字串是第一頁，不是 404 before_message_gone（同 turn_id／role 的空字串也當沒帶）。
#[tokio::test]
async fn an_empty_before_cursor_is_the_first_page_not_a_404() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (mine, conv) = a_bot_with_conv(&e, "cursor-empty").await;
    a_turn(&app.db, &conv, "t-e").await;
    a_message(&app.db, &conv, "m-e-1", "t-e", "user", None).await;
    a_message(&app.db, &conv, "m-e-2", "t-e", "assistant", None).await;
    let page = |before: &str| Query(HashMap::from([("before".to_string(), before.to_string())]));
    let Json(with_empty) = get_messages(State(app.clone()), Path(mine.clone()), page("")).await.unwrap();
    let Json(without) = get_messages(State(app.clone()), Path(mine.clone()), Query(HashMap::new())).await.unwrap();
    assert_eq!(with_empty["messages"], without["messages"]);
    assert_eq!(with_empty["messages"].as_array().unwrap().len(), 2);
}

async fn deleted_bot_with_hits(e: &crate::testing::Env, name: &str, hits: usize) -> String {
    let (id, conv) = a_bot_with_conv(e, name).await;
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&id).execute(&e.app.db).await.unwrap();
    let turn = format!("t-{name}");
    a_turn(&e.app.db, &conv, &turn).await;
    for i in 0..hits {
        a_message(&e.app.db, &conv, &format!("{name}-{i}"), &turn, "user", None).await;
    }
    id
}

/// #766：已刪 bot 仍在結果裡（AGM 查證據要），但不能用較多的命中數把活 bot 擠出 `limit`；每列帶 `bot_deleted`。
#[tokio::test]
async fn search_keeps_live_bots_ahead_of_deleted_ones() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (live, live_conv) = a_bot_with_conv(&e, "needle-live").await;
    a_turn(&app.db, &live_conv, "t-live").await;
    a_message(&app.db, &live_conv, "needle-live-0", "t-live", "user", None).await;
    let big = deleted_bot_with_hits(&e, "needle-big", 5).await;
    let mid = deleted_bot_with_hits(&e, "needle-mid", 3).await;

    let search = |limit| search_messages(State(app.clone()), Query(SearchQuery { q: Some("needle".into()), limit: Some(limit) }));
    let Json(two) = search(2).await.unwrap();
    let rows = two["bots"].as_array().unwrap();
    assert_eq!(rows.iter().map(|r| r["bot_id"].as_str().unwrap()).collect::<Vec<_>>(), [live.as_str(), big.as_str()]);
    assert_eq!(rows.iter().map(|r| r["bot_deleted"].as_bool().unwrap()).collect::<Vec<_>>(), [false, true]);
    let Json(all) = search(10).await.unwrap();
    assert_eq!(all["bots"].as_array().unwrap().iter().map(|r| r["bot_id"].as_str().unwrap()).collect::<Vec<_>>(), [live.as_str(), big.as_str(), mid.as_str()]);
}

/// 同一顆 bot 兩則命中同一毫秒：snippet 取後寫入的那則（rowid 較大），不是任一則。
#[tokio::test]
async fn search_snippet_is_the_last_written_hit_when_timestamps_tie() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (_bot, conv) = a_bot_with_conv(&e, "tie-bot").await;
    let at = db::now();
    for (id, text) in [("zz-first", "tiehit 先寫的"), ("aa-second", "tiehit 後寫的")] {
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'user',?,'web',?)")
            .bind(id).bind(&conv).bind(text).bind(&at).execute(&app.db).await.unwrap();
    }
    let Json(r) = search_messages(State(app), Query(SearchQuery { q: Some("tiehit".into()), limit: None })).await.unwrap();
    assert_eq!(r["bots"][0]["hits"], 2);
    assert!(r["bots"][0]["snippet"].as_str().unwrap().contains("後寫的"), "{r}");
}

/// 依回合與角色篩選訊息，不必翻整段歷史。
#[tokio::test]
async fn turn_id_and_role_filter_the_page() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (bot_id, conv) = a_bot_with_conv(&e, "filter-bot").await;
    a_turn(&app.db, &conv, "t-old").await;
    a_turn(&app.db, &conv, "t-group").await;
    a_message(&app.db, &conv, "m-old", "t-old", "user", None).await;
    a_message(&app.db, &conv, "m-group", "t-group", "user", Some("grp-1")).await;
    // prompt 後面跟著一長串 assistant：以前靠翻頁找就會被擠出去。
    for i in 0..250 {
        a_message(&app.db, &conv, &format!("m-a{i:03}"), "t-group", "assistant", None).await;
    }

    let q = HashMap::from([("turn_id".to_string(), "t-group".to_string()), ("role".to_string(), "user".to_string())]);
    let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], "m-group");
    assert!(messages[0].get("group_id").is_none(), "legacy group metadata is not exposed");
    assert_eq!(body["has_more"], false);

    // turn 只過濾 turn，不順便挑 role。
    let q = HashMap::from([("turn_id".to_string(), "t-group".to_string()), ("limit".to_string(), "500".to_string())]);
    let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(251));

    // 別的回合、別顆 bot 的同名 turn 都查不到（查詢限定在這個 conversation）。
    let q = HashMap::from([("turn_id".to_string(), "t-nope".to_string())]);
    let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(0));
    let (other_id, other_conv) = a_bot_with_conv(&e, "other-bot").await;
    a_turn(&app.db, &other_conv, "t-other").await;
    a_message(&app.db, &other_conv, "m-other", "t-other", "user", Some("grp-9")).await;
    let q = HashMap::from([("turn_id".to_string(), "t-group".to_string()), ("role".to_string(), "user".to_string())]);
    let Json(body) = get_messages(State(app.clone()), Path(other_id.clone()), Query(q)).await.unwrap();
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(0));
    let q = HashMap::from([("turn_id".to_string(), "t-other".to_string()), ("role".to_string(), "user".to_string())]);
    let Json(body) = get_messages(State(app.clone()), Path(other_id), Query(q)).await.unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], "m-other");

    // 沒帶過濾＝現狀（整段、依插入順序）。
    let q = HashMap::from([("limit".to_string(), "500".to_string())]);
    let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(252));

    // role 打錯要說，不能默默當作沒過濾。
    let q = HashMap::from([("role".to_string(), "assistant ".to_string())]);
    assert!(matches!(
        get_messages(State(app), Path(bot_id), Query(q)).await,
        Err(LcError::Bad(msg)) if msg.contains("role")
    ));
}

/// 同一回合的 user 訊息超過一頁：`before` 要能在過濾後繼續往前翻。
#[tokio::test]
async fn filtered_pages_keep_paging_with_before() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (bot_id, conv) = a_bot_with_conv(&e, "filter-page-bot").await;
    a_turn(&app.db, &conv, "t-g").await;
    a_message(&app.db, &conv, "u-group", "t-g", "user", Some("grp-1")).await;
    for i in 0..3 {
        a_message(&app.db, &conv, &format!("u-more{i}"), "t-g", "user", None).await;
    }

    let q = HashMap::from([
        ("turn_id".to_string(), "t-g".to_string()),
        ("role".to_string(), "user".to_string()),
        ("limit".to_string(), "2".to_string()),
    ]);
    let Json(first) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
    assert_eq!(first["has_more"], true);
    let oldest = first["messages"][0]["id"].as_str().unwrap().to_string();

    let q = HashMap::from([
        ("turn_id".to_string(), "t-g".to_string()),
        ("role".to_string(), "user".to_string()),
        ("limit".to_string(), "2".to_string()),
        ("before".to_string(), oldest),
    ]);
    let Json(second) = get_messages(State(app), Path(bot_id), Query(q)).await.unwrap();
    assert_eq!(second["has_more"], false);
    assert_eq!(second["messages"][0]["id"], "u-group");
}

fn turn_ids(body: &Value) -> Vec<String> {
    body["turns"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_string()).collect()
}

/// #954：`turns` 是這一頁訊息所屬的回合（∪ 還沒結束的），翻到更早的頁也查得到那一則 prompt 的 delivery。
#[tokio::test]
async fn every_page_carries_the_turns_of_its_own_messages() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (bot_id, conv) = a_bot_with_conv(&e, "turns-page-bot").await;
    for t in ["t-1", "t-2", "t-3"] {
        a_turn(&app.db, &conv, t).await;
    }
    sqlx::query("UPDATE turns SET delivery='failed' WHERE id='t-1'").execute(&app.db).await.unwrap();
    for (m, t) in [("m-1", "t-1"), ("m-2", "t-2"), ("m-3", "t-3")] {
        a_message(&app.db, &conv, m, t, "user", None).await;
    }

    let page = |before: Option<&str>| {
        let mut q = HashMap::from([("limit".to_string(), "1".to_string())]);
        if let Some(b) = before {
            q.insert("before".to_string(), b.to_string());
        }
        q
    };
    let Json(first) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(page(None))).await.unwrap();
    assert_eq!(first["messages"][0]["id"], "m-3");
    assert_eq!(turn_ids(&first), ["t-3"], "第一頁只帶自己的回合");
    let Json(second) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(page(Some("m-3")))).await.unwrap();
    assert_eq!(second["messages"][0]["id"], "m-2");
    assert_eq!(turn_ids(&second), ["t-2"]);
    let Json(third) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(page(Some("m-2")))).await.unwrap();
    assert_eq!(third["messages"][0]["id"], "m-1");
    assert_eq!(turn_ids(&third), ["t-1"], "最舊那頁也查得到回合");
    assert_eq!(third["turns"][0]["delivery"], "failed", "失敗 prompt 的送達警示不會因為翻頁消失");

    // 帶 turn_id= 只回那一個回合（加上還沒結束的，這裡沒有）。
    let q = HashMap::from([("turn_id".to_string(), "t-1".to_string())]);
    let Json(filtered) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
    assert_eq!(turn_ids(&filtered), ["t-1"]);
    // 沒有訊息的頁（查不到的 turn）：不帶任何已結束的回合，也不出錯。
    let q = HashMap::from([("turn_id".to_string(), "t-nope".to_string())]);
    let Json(none) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
    assert!(turn_ids(&none).is_empty());

    // 還在 queued／in_flight 的回合不管在哪一頁都要帶（判斷還在不在跑）。
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES ('t-live',?,'web','in_flight','ok',?)")
        .bind(&conv)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let Json(old_page) = get_messages(State(app), Path(bot_id), Query(page(Some("m-2")))).await.unwrap();
    let ids = turn_ids(&old_page);
    assert!(ids.contains(&"t-1".to_string()) && ids.contains(&"t-live".to_string()), "{ids:?}");
    assert!(!ids.contains(&"t-3".to_string()), "別頁已結束的回合不帶：{ids:?}");
}

/// #954：同毫秒的回合要用 `rowid` 倒序，`turns` 的順序在重複查詢與翻頁邊界上穩定。
#[tokio::test]
async fn turns_created_in_the_same_millisecond_are_ordered_by_rowid_desc() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let (bot_id, conv) = a_bot_with_conv(&e, "turns-same-ms-bot").await;
    let at = db::now();
    for t in ["t-a", "t-b", "t-c"] {
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','in_flight','ok',?)")
            .bind(t)
            .bind(&conv)
            .bind(&at)
            .execute(&app.db)
            .await
            .unwrap();
    }
    a_message(&app.db, &conv, "m-a", "t-a", "user", None).await;
    let expected = ["t-c", "t-b", "t-a"];
    for _ in 0..3 {
        let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(HashMap::new())).await.unwrap();
        assert_eq!(turn_ids(&body), expected, "同一毫秒：rowid 倒序，每次一樣");
    }
    let q = HashMap::from([("limit".to_string(), "1".to_string()), ("before".to_string(), "m-a".to_string())]);
    let Json(edge) = get_messages(State(app), Path(bot_id), Query(q)).await.unwrap();
    assert!(edge["messages"].as_array().unwrap().is_empty(), "m-a 是最舊的一則：往前是空頁");
    assert_eq!(turn_ids(&edge), expected, "空頁也帶還沒結束的回合，順序不變");
}

/// review 2026-09-12 #9; deleted bots still answer (API.md §10.4).
#[tokio::test]
async fn messages_for_an_unknown_bot_are_not_found() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    assert!(matches!(
        get_messages(State(app.clone()), Path("no-such-bot".into()), Query(HashMap::new())).await,
        Err(LcError::NotFound(what)) if what == "bot"
    ));
    let gone = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, hook_token, deleted_at, created_at) VALUES (?,?,'gone','claude','tok',?,?)",
    )
    .bind(&gone)
    .bind(&e.project_id)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let Json(body) = get_messages(State(app), Path(gone.clone()), Query(HashMap::new())).await.unwrap();
    assert_eq!(body["bot_id"], gone);
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(0));
}
