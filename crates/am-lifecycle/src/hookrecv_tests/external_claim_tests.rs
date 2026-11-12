use super::*;
use crate::testing as tt;
use std::time::Duration;

#[test]
fn hook_user_dedups_against_the_scraped_echo() {
    let echo = vec!["Reply with exactly MERGED-OK".to_string()];
    assert!(!hook_user_is_new(&echo, "Reply with exactly MERGED-OK"));
    // The pane wrapped the echo across two columns; the hook sends one line.
    assert!(!hook_user_is_new(&vec!["Reply with\n  exactly MERGED-OK".into()], "Reply with exactly MERGED-OK"));
    // The pane clipped the echo at the column width.
    assert!(!hook_user_is_new(&vec!["Reply with exactly MER".into()], "Reply with exactly MERGED-OK"));
}

#[test]
fn hook_prompt_identity_keeps_word_boundaries_and_rejects_weak_overlap() {
    let separated = "echo a b";
    let joined = "echo ab";
    assert!(answers_another_prompt(Some(separated), Some(joined)));
    assert!(answers_another_prompt(Some(joined), Some(separated)));
    assert!(hook_user_is_new(&[separated.into()], joined));
    assert!(hook_user_is_new(&[joined.into()], separated));

    // A short common prefix is not enough evidence to assign a hook to a prompt.
    assert!(answers_another_prompt(Some("echo a much longer instruction"), Some("echo a")));
    assert!(hook_user_is_new(&["echo a much longer instruction".into()], "echo a"));
}

#[test]
fn hook_prompt_identity_accepts_wrapping_and_repeated_whitespace() {
    assert!(!answers_another_prompt(
        Some("Reply with exactly MERGED-OK"),
        Some("Reply with\n  exactly   MERGED-OK")
    ));
    assert!(!hook_user_is_new(
        &["Reply with\n  exactly   MERGED-OK".into()],
        "Reply with exactly MERGED-OK"
    ));
    // The existing clipped echo remains attributable when enough prompt text survived.
    assert!(!answers_another_prompt(
        Some("Reply with exactly MERGED-OK"),
        Some("Reply with exactly MER")
    ));
}

/// #218：claude Stop 沒帶使用者訊息，從 transcript 尾巴補。CLI 把貼上的 prompt 包成 `<pasted_content id=…>`
/// （真 transcript，2026-09-19 實測）：要拆回原文——不然記成外部回合時標籤會顯示給使用者，`agent_relay::claim` 也對不上。
#[test]
fn the_transcript_user_text_drops_the_cli_pasted_content_wrapper() {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hookrecv-{}", db::ulid())));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.jsonl");
    let log: Vec<&str> = include_str!("../lifecycle/fixtures/claude_2.1.278_pasted_content.jsonl").lines().collect();
    std::fs::write(&path, log[..2].join("\n") + "\n").unwrap();
    assert_eq!(last_transcript_user_text(&path).as_deref(), Some("請只回覆 OK 兩個字母，不要多說任何其他的話，也不要使用任何工具。"));
    std::fs::write(&path, log[8..].join("\n") + "\n").unwrap();
    assert_eq!(
        last_transcript_user_text(&path).as_deref(),
        Some("這段文字裡有字面的 <pasted_content id=\"1234\">x</pasted_content id=\"1234\"> 標籤，請只回覆 OK 兩個字母。"),
        "CLI 跳脫的字面標籤還原"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_user_is_stored_when_it_is_not_the_echo() {
    assert!(hook_user_is_new(&[], "Reply with exactly MERGED-OK"));
    assert!(hook_user_is_new(&vec!["echo 1".into()], "echo 2"));
    // Nothing to store.
    assert!(!hook_user_is_new(&[], "   "));
}

/// `db::open` needs a path; the directory removes itself on drop.
struct TmpDb(std::path::PathBuf);
impl Drop for TmpDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn fixture() -> (TmpDb, sqlx::SqlitePool, String, String) {
    let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hookrecv-{}", db::ulid())));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = crate::app_ports_p1::open(&dir.join("t.db")).await.unwrap();
    let now = db::now();
    for q in [
        "INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp/p','p',?)",
        "INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','tok',?)",
        "INSERT INTO conversations (id,bot_id,created_at) VALUES ('c','b',?)",
        "INSERT INTO runs (id,bot_id,state,agent_status,pane_id,started_at) VALUES ('r','b','running','working','%1',?)",
    ] {
        sqlx::query(q).bind(&now).execute(&pool).await.unwrap();
    }
    (TmpDb(dir), pool, "r".to_string(), "c".to_string())
}

/// 2026-10-02 wits-ops-web：遠端讀不到 transcript，晚到的 hook 拿不出使用者訊息當證據。改看它在那台觸發的時間：
/// 落在備援回合開始之後、中間沒開過別的回合，才算同一回合。
#[tokio::test]
async fn a_remote_late_hook_is_matched_by_when_it_fired() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "remote").await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let mk = |at: &'static str| {
        let app = app.clone();
        let conv = conv.clone();
        async move {
            let id = db::ulid();
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'external','in_flight','ok',?)")
                .bind(&id).bind(&conv).bind(at).execute(&app.db).await.unwrap();
            sqlx::query("UPDATE turns SET status='completed_fallback', completed_at=? WHERE id=?").bind(at).bind(&id).execute(&app.db).await.unwrap();
            sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&id).fetch_one(&app.db).await.unwrap()
        }
    };
    let t = mk("2026-10-01T16:54:43.000Z").await;
    assert!(fired_within(&app, &t, Some("2026-10-01T17:05:39Z")).await.unwrap(), "回合開始後觸發、中間沒別的回合");
    assert!(!fired_within(&app, &t, Some("2026-10-01T16:50:00Z")).await.unwrap(), "比回合還早觸發：是上一回合的");
    assert!(!fired_within(&app, &t, None).await.unwrap(), "沒有時間＝沒有證據");
    let _later = mk("2026-10-01T17:00:00.000Z").await;
    assert!(!fired_within(&app, &t, Some("2026-10-01T17:05:39Z")).await.unwrap(), "中間開過別的回合：不能確定是哪一回合的");
}

/// 2026-09-29（遠端 claude）：hook 走 spool 晚 25 秒到，備援已經存了只有最後一段、還帶狀態列的回覆。
/// 有證據是同一回合就用 hook 的原文原地蓋掉（id 不變）；已經有 hook 寫的回覆、或沒有證據時不動。
#[tokio::test]
async fn a_late_hook_with_evidence_replaces_the_fallback_reply_in_place() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'late-replace','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
         VALUES (?,?,'web','completed_fallback','ok',?,?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let fb = lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "最後一段\n\n✻ Crunched for 40s · done 17:02", "terminal_fallback", true, None)
        .await
        .unwrap();
    let turn = || async {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap()
    };

    fill_or_drop_late_hook(&app, &turn().await, "hook 原文", &Some("s1".into()), &Some("n1".into()), false, None).await.unwrap();
    let (content, source): (String, String) =
        sqlx::query_as("SELECT content, source FROM messages WHERE id=?").bind(&fb.id).fetch_one(&app.db).await.unwrap();
    assert_eq!(source, "terminal_fallback", "沒有證據不蓋：{content}");

    fill_or_drop_late_hook(&app, &turn().await, "第一段\n\n最後一段", &Some("s1".into()), &Some("n1".into()), true, None).await.unwrap();
    let (content, source, incomplete): (String, String, i64) =
        sqlx::query_as("SELECT content, source, incomplete FROM messages WHERE id=?").bind(&fb.id).fetch_one(&app.db).await.unwrap();
    assert_eq!((content.as_str(), source.as_str(), incomplete), ("第一段\n\n最後一段", "hook", 0), "同一個 id 換成 hook 原文");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(n, 1, "不會變成兩則");
    assert_eq!(turn().await.status, "completed");

    // 已經是 hook 寫的（回合也不再是 completed_fallback）：再來一份不動。
    fill_or_drop_late_hook(&app, &turn().await, "又一份", &Some("s1".into()), &Some("n1".into()), true, None).await.unwrap();
    let content: String = sqlx::query_scalar("SELECT content FROM messages WHERE id=?").bind(&fb.id).fetch_one(&app.db).await.unwrap();
    assert_eq!(content, "第一段\n\n最後一段");
}

#[test]
fn idle_drain_gates_are_dropped() {
    let t0 = std::time::Instant::now();
    let mut g = std::collections::HashMap::new();
    g.insert("old".to_string(), DrainGate { last: Some(t0), again: false });
    g.insert("recent".to_string(), DrainGate { last: Some(t0 + DRAIN_GATE_KEEP), again: false });
    g.insert("owes".to_string(), DrainGate { last: Some(t0), again: true });
    prune_gates(&mut g, t0 + DRAIN_GATE_KEEP + std::time::Duration::from_secs(1));
    assert!(!g.contains_key("old"), "久沒用又沒欠補跑：帶走");
    assert!(g.contains_key("recent") && g.contains_key("owes"));
}

async fn late_hook_fixture(env: &tt::Env, name: &str, status: &str) -> (String, String) {
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(name)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&env.app.db, &bot_id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at) VALUES (?,?,'web',?,'ok',?,?)")
        .bind(&turn_id)
        .bind(&conv)
        .bind(status)
        .bind(db::now())
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
    (conv, turn_id)
}

async fn native_ids(app: &Arc<App>, turn_id: &str) -> (Option<String>, Option<String>) {
    sqlx::query_as("SELECT native_session_id, native_turn_id FROM turns WHERE id=?").bind(turn_id).fetch_one(&app.db).await.unwrap()
}

/// 晚到的 hook 手上的回合快照是舊的：回合已經被別的路收成 `completed`、蓋好去重鑰匙（session, turn）。
/// 補回覆之前那句「寫 native id」沒有狀態保護，會把別人的鑰匙蓋掉——該回合 hook 的重送就不再被去重擋住。
#[tokio::test]
async fn a_late_hook_never_overwrites_the_dedup_key_of_a_turn_already_closed() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_conv, turn_id) = late_hook_fixture(&env, "late-keys", "completed_fallback").await;
    let stale = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    // 快照之後，另一則 hook 把它收成 completed、蓋上自己的鑰匙。
    sqlx::query("UPDATE turns SET status='completed', native_session_id='s0', native_turn_id='n0' WHERE id=?").bind(&turn_id).execute(&app.db).await.unwrap();

    fill_or_drop_late_hook(&app, &stale, "晚到的另一則", &Some("s1".into()), &Some("n1".into()), true, None).await.unwrap();
    assert_eq!(native_ids(&app, &turn_id).await, (Some("s0".into()), Some("n0".into())), "已經收好的回合的去重鑰匙不能被蓋掉");
}

/// 沒有證據證明這則 hook 屬於這一回合（`same_turn = false`）而且丟掉它：它的 native id 不能記到這一回合上——
/// 記上去之後 `recent_fallback_turn`（只找 native id 為空的）就再也找不到這一回合，它真正的晚到 hook 沒地方收。
#[tokio::test]
async fn a_dropped_late_hook_that_may_belong_elsewhere_does_not_stamp_its_ids_on_the_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (conv, turn_id) = late_hook_fixture(&env, "late-foreign", "completed_fallback").await;
    lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "備援抓的回覆", "terminal_fallback", true, None).await.unwrap();
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();

    fill_or_drop_late_hook(&app, &turn, "別回合的回覆", &Some("s-other".into()), &Some("n-other".into()), false, None).await.unwrap();
    assert_eq!(native_ids(&app, &turn_id).await, (None, None), "丟掉的 hook 不留鑰匙");
    let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(replies, 1);
}

async fn source_event_of(app: &Arc<App>, turn_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT source_event_id FROM turns WHERE id=?").bind(turn_id).fetch_one(&app.db).await.unwrap()
}

/// issue #1469：遲到 hook 記 native id 的同一個交易也要記收件匣事件 id——否則 `after_turn_end` 失敗後的重試，
/// 3a 認不得這一則、第 3 步又被 native id 去重吞掉。三條會記 native id 的路（補空回覆、蓋掉備援回覆、已有 hook 回覆而丟掉）
/// 都要記；沒有證據、不記 native id 的那條也不能單獨記事件 id。
#[tokio::test]
async fn a_late_hook_records_its_inbox_event_id_wherever_it_records_its_native_ids() {
    let env = tt::env().await;
    let app = env.app.clone();
    let turn_of = |id: String| {
        let app = app.clone();
        async move { sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap() }
    };

    // 補空回覆（回合沒有任何 assistant 訊息）。
    let (_conv, fill) = late_hook_fixture(&env, "late-event-fill", "completed_fallback").await;
    fill_or_drop_late_hook(&app, &turn_of(fill.clone()).await, "遲到的回覆", &Some("s1".into()), &Some("n1".into()), true, Some("ev-fill")).await.unwrap();
    assert_eq!(native_ids(&app, &fill).await, (Some("s1".into()), Some("n1".into())));
    assert_eq!(source_event_of(&app, &fill).await.as_deref(), Some("ev-fill"), "補空回覆那條也記事件 id");

    // 蓋掉備援回覆（有證據、回覆全是備援抓的）。
    let (conv, replace) = late_hook_fixture(&env, "late-event-replace", "completed_fallback").await;
    lifecycle::insert_message(&app, &conv, Some(&replace), "assistant", "備援抓的回覆", "terminal_fallback", true, None).await.unwrap();
    fill_or_drop_late_hook(&app, &turn_of(replace.clone()).await, "hook 原文", &Some("s2".into()), &Some("n2".into()), true, Some("ev-replace")).await.unwrap();
    assert_eq!(native_ids(&app, &replace).await, (Some("s2".into()), Some("n2".into())));
    assert_eq!(source_event_of(&app, &replace).await.as_deref(), Some("ev-replace"), "蓋掉備援回覆那條也記事件 id");

    // 有證據，但回覆已經是 hook 寫的：丟掉這一份，鑰匙照記。
    let (conv, kept) = late_hook_fixture(&env, "late-event-kept", "completed_fallback").await;
    lifecycle::insert_message(&app, &conv, Some(&kept), "assistant", "hook 先到的回覆", "hook", false, None).await.unwrap();
    fill_or_drop_late_hook(&app, &turn_of(kept.clone()).await, "又一份", &Some("s3".into()), &Some("n3".into()), true, Some("ev-kept")).await.unwrap();
    assert_eq!(native_ids(&app, &kept).await, (Some("s3".into()), Some("n3".into())));
    assert_eq!(source_event_of(&app, &kept).await.as_deref(), Some("ev-kept"), "已有 hook 回覆而丟掉的那條也記事件 id");

    // 沒有證據（可能屬於別的回合）而丟掉：什麼鑰匙都不留。
    let (conv, foreign) = late_hook_fixture(&env, "late-event-foreign", "completed_fallback").await;
    lifecycle::insert_message(&app, &conv, Some(&foreign), "assistant", "備援抓的回覆", "terminal_fallback", true, None).await.unwrap();
    fill_or_drop_late_hook(&app, &turn_of(foreign.clone()).await, "別回合的回覆", &Some("s4".into()), &Some("n4".into()), false, Some("ev-foreign")).await.unwrap();
    assert_eq!(native_ids(&app, &foreign).await, (None, None));
    assert_eq!(source_event_of(&app, &foreign).await, None, "沒有證據不留事件 id");
}

/// 2026-09-13（GROK）：備援關掉的回合沒存回覆，遲到 hook 的答案被丟、使用者看到「沒回應」。
#[tokio::test]
async fn a_late_hook_fills_a_fallback_turn_that_has_no_reply() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'late-hook','grok','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
         VALUES (?,?,'web','completed_fallback','ok',?,?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();

    fill_or_drop_late_hook(&app, &turn, "側欄那組徽章已收齊，cdcf165 已推", &Some("s1".into()), &Some("n1".into()), false, None)
        .await
        .unwrap();

    let (status, native): (String, Option<String>) =
        sqlx::query_as("SELECT status, native_turn_id FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(status, "completed", "有 hook 的證據就不再是「只看到畫面」");
    assert_eq!(native.as_deref(), Some("n1"), "native id 照舊蓋上去，重送才去得掉重");
    let replies: Vec<String> =
        sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
    assert_eq!(replies, vec!["側欄那組徽章已收齊，cdcf165 已推".to_string()]);

    // 已有回覆的照舊丟，防一回合兩則。
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    fill_or_drop_late_hook(&app, &turn, "第二份回覆", &Some("s1".into()), &Some("n1".into()), false, None).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(n, 1, "不會變成兩則");
}

/// #831：遲到的 hook 數完回合上的回覆、還沒寫的那一瞬，一個不相干的 writer commit 了一筆。deferred 交易這時升級寫鎖
/// 直接 517，hook 的回覆補不上；寫鎖從讀之前就拿著，插進來的那一筆等，回覆補上一次、回合升 `completed`。
#[tokio::test]
async fn an_unrelated_writer_between_the_reply_count_and_the_fill_does_not_lose_the_late_reply() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "late-hook-831").await;
    let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
         VALUES (?,?,'web','completed_fallback','ok',?,?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    let other = tt::arm_app_foreign_writer(&app, "late_hook_after_reply_count", &turn_id);

    fill_or_drop_late_hook(&app, &turn, "late reply", &Some("s1".into()), &Some("n1".into()), false, None)
        .await
        .expect("an unrelated writer must not make the late hook fail");

    assert_eq!(*other.lock().unwrap(), Some(false), "the fill holds the write lock from its read on; the other writer waits");
    assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
    let replies: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, vec!["late reply".to_string()]);
}

#[tokio::test]
async fn unreadable_late_hook_message_owner_rolls_back_until_retry() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'late-hook-owner-retry','grok','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
         VALUES (?,?,'web','completed_fallback','ok',?,?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    let mut events = app.subscribe();

    tt::make_table_unreadable(&app, "conversations").await;
    let first = fill_or_drop_late_hook(&app, &turn, "late reply", &Some("s1".into()), &Some("n1".into()), false, None).await;
    tt::make_table_readable(&app, "conversations").await;

    assert!(first.is_err(), "an unreadable owner must return a retryable error: {first:?}");
    let mut failed_message_events = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.kind == "message_added" {
            failed_message_events.push(event);
        }
    }
    assert!(failed_message_events.is_empty(), "the failed attempt must not publish a message with an unknown owner");
    let (status, native_turn_id): (String, Option<String>) =
        sqlx::query_as("SELECT status, native_turn_id FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(status, "completed_fallback", "failed owner lookup rolls back the status transition");
    assert_eq!(native_turn_id, None, "failed owner lookup rolls back native ids too");
    let failed_replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(failed_replies, 0, "failed owner lookup cannot leave a durable reply without its event");

    fill_or_drop_late_hook(&app, &turn, "late reply", &Some("s1".into()), &Some("n1".into()), false, None).await.unwrap();
    let message_events: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event.kind == "message_added")
        .collect();
    assert_eq!(message_events.len(), 1, "the replay emits exactly one message_added event");
    assert_eq!(message_events[0].data["bot_id"], bot_id);
    assert_eq!(message_events[0].data["message"]["turn_id"], turn_id);
}

/// 空的 hook 不能把回合改成 completed——那等於宣稱有答案。
#[tokio::test]
async fn an_empty_late_hook_changes_nothing_but_the_native_ids() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'late-hook-empty','grok','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
         VALUES (?,?,'web','completed_fallback','ok',?,?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    fill_or_drop_late_hook(&app, &turn, "   ", &Some("s2".into()), &Some("n2".into()), false, None).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(status, "completed_fallback");
}

/// 2026-09-12 使用者實機：折行讓刮下來的訊息斷在一半，既有那則是原文前綴時要補完。
#[tokio::test]
async fn a_clipped_scraped_prompt_is_upgraded_to_the_hooks_full_text() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'clipped-echo','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at)
         VALUES (?,?,'external','in_flight','ok',?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let clipped = "請問我兩題，第二題 header『功能』請設 multiSelect:";
    let full = "請問我兩題，第二題 header『功能』請設 multiSelect: true，四個選項。問完就停著等我回答。";
    crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "user", clipped, "terminal_fallback", false, None)
        .await
        .unwrap();

    let upgraded = upgrade_clipped_user_message(&mut app.db.acquire().await.unwrap(), &turn_id, full).await.unwrap();
    let upgraded = upgraded.expect("補完了一則");
    assert_eq!((upgraded.content.as_str(), upgraded.source.as_str()), (full, "hook"));

    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT content, source FROM messages WHERE turn_id=? AND role='user'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1, "補完，不是多開一則");
    assert_eq!(rows[0].0, full);
    assert_eq!(rows[0].1, "hook");

    // 不是前綴的就別動：那是另一句話。
    let none = upgrade_clipped_user_message(&mut app.db.acquire().await.unwrap(), &turn_id, "完全不同的一句").await.unwrap();
    assert!(none.is_none(), "不是前綴就不補");
    let after: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='user'")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(after, full);
}

/// #1006：外部回合的回音被 hook 原文補完後，同一則要再推一次（同 id），網頁才不會停在折行截斷的那一句。
#[tokio::test]
async fn a_clipped_prompt_upgrade_is_pushed_to_the_web() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'clipped-push','codex','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
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
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
         VALUES (?,?,?,'external','in_flight','ok',?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let full = "請問我兩題，第二題 header『功能』請設 multiSelect: true，四個選項。問完就停著等我回答。";
    let clipped = &full[..full.char_indices().nth(12).unwrap().0];
    let echo = crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "user", clipped, "terminal_fallback", false, None)
        .await
        .unwrap();

    let mut events = app.subscribe();
    process(&app, &codex_done(&bot_id, full)).await.unwrap();

    let pushed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|e| e.kind == "message_added")
        .filter(|e| e.data["message"]["id"] == echo.id)
        .collect();
    assert_eq!(pushed.len(), 1, "被補完的那一則要再推一次");
    assert_eq!(pushed[0].data["message"]["content"], full);
}

async fn unknown_turn(app: &Arc<App>, project_id: &str, kind: &str, prompt: &str) -> (String, String, String) {
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,?,?,'[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(project_id)
    .bind(format!("hook-{}", &bot_id[bot_id.len() - 6..]))
    .bind(kind)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
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
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','unknown',?,?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(&run_id)
    .bind(prompt)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    (bot_id, conversation_id, turn_id)
}

fn codex_done(bot_id: &str, user: &str) -> HookBody {
    HookBody {
        bot_id: bot_id.to_string(),
        provider: "codex".into(),
        payload: json!({
            "type": "agent-turn-complete",
            "thread-id": format!("thread-{bot_id}"),
            "turn-id": format!("turn-{}", db::ulid()),
            "input-messages": [user],
            "last-assistant-message": "hook reply",
        }),
        received_at: None,
        truncated: false,
        run_id: None,
    }
}

#[tokio::test]
async fn codex_empty_completion_with_matching_native_turn_waits_for_fallback() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _, turn_id) = unknown_turn(&app, &env.project_id, "codex", "test prompt").await;
    sqlx::query("UPDATE turns SET delivery='ok', native_turn_id='native-2026' WHERE id=?")
        .bind(&turn_id).execute(&app.db).await.unwrap();
    let mut hook = codex_done(&bot_id, "test prompt");
    hook.payload["turn-id"] = json!("native-2026");
    hook.payload["last-assistant-message"] = json!("");
    process(&app, &hook).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "in_flight", "empty Codex hook must leave the turn for typed rollout recovery");
}

#[tokio::test]
async fn codex_nonempty_completion_still_finishes_matching_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _, turn_id) = unknown_turn(&app, &env.project_id, "codex", "test prompt").await;
    sqlx::query("UPDATE turns SET delivery='ok', native_turn_id='native-2026' WHERE id=?")
        .bind(&turn_id).execute(&app.db).await.unwrap();
    let mut hook = codex_done(&bot_id, "test prompt");
    hook.payload["turn-id"] = json!("native-2026");
    process(&app, &hook).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "completed");
}

#[tokio::test]
async fn codex_empty_completion_already_bound_to_session_rearms_poller() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _, turn_id) = unknown_turn(&app, &env.project_id, "codex", "test prompt").await;
    let mut hook = codex_done(&bot_id, "test prompt");
    hook.payload["turn-id"] = json!("native-2026");
    hook.payload["last-assistant-message"] = json!("");
    let sid = hook.payload["thread-id"].as_str().unwrap();
    sqlx::query("UPDATE turns SET delivery='ok', native_turn_id='native-2026', native_session_id=? WHERE id=?")
        .bind(sid).bind(&turn_id).execute(&app.db).await.unwrap();
    let run: String = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    process(&app, &hook).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "in_flight");
    assert!(app.progress_pollers.lock().await.contains_key(&run));
}

/// 第二輪 review 送達線 #3：`unknown` 期間使用者在終端手打另一句，答案不能掛到原本那則、也不能把它標成已送達。
#[tokio::test]
async fn a_hook_answering_another_prompt_does_not_claim_the_unknown_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "codex", "echo a b").await;

    process(&app, &codex_done(&bot_id, "echo ab")).await.unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("in_flight", "unknown"), "原本那則原封不動");
    let external: Vec<(String, String)> = sqlx::query_as(
        "SELECT t.id, m.content FROM turns t JOIN messages m ON m.turn_id=t.id
          WHERE t.conversation_id=? AND t.origin='external' AND m.role='user'",
    )
    .bind(&conv)
    .fetch_all(&app.db)
    .await
    .unwrap();
    assert_eq!(external.len(), 1, "答案記在一筆外部回合上");
    assert_eq!(external[0].1, "echo ab");
}

/// 沒有任何 native id 的 claude Stop（遠端、舊版 hook、手寫 body）：回合沒有去重用的 `(session, turn)` 鑰匙。
fn stop_without_native_ids(bot_id: &str, reply: &str) -> HookBody {
    HookBody {
        bot_id: bot_id.to_string(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": "Stop", "last_assistant_message": reply}),
        received_at: None,
        truncated: false,
        run_id: None,
    }
}

async fn turn_count(app: &Arc<App>, conv: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(conv).fetch_one(&app.db).await.unwrap()
}

async fn hook_message_count(app: &Arc<App>, conv: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND source='hook'").bind(conv).fetch_one(&app.db).await.unwrap()
}

/// 發現 2（l8 審查）：外部回合 commit 了、daemon 在收件匣標 `processed_at` 之前重啟，重播同一列不能再開第二個回合與訊息——
/// 沒有 native turn id 時，過去沒有任何穩定的鑰匙擋它。重播只補發 commit 後才發的通知。
#[tokio::test]
async fn an_inbox_row_replayed_after_the_external_turn_committed_opens_no_second_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "inbox-replay").await;
    tt::fake_run(&app, &bot.id).await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let body = stop_without_native_ids(&bot.id, "外部回合的回覆");
    assert!(crate::hook_inbox::accept(&app.db, &body, crate::hook_inbox::Source::Http).await.unwrap().is_new());
    let row = crate::hook_inbox::pending(&app.db, &db::now(), 10).await.unwrap().remove(0);

    // 第一輪：回合與訊息已 commit，但 worker 還沒標 processed_at 就「重啟」了。
    process_for(&app, &body, Some(&row.id)).await.unwrap();
    assert_eq!(turn_count(&app, &conv).await, 1);
    assert_eq!(hook_message_count(&app, &conv).await, 1);
    let stamped: Option<String> = sqlx::query_scalar("SELECT source_event_id FROM turns WHERE conversation_id=?").bind(&conv).fetch_one(&app.db).await.unwrap();
    assert_eq!(stamped.as_deref(), Some(row.id.as_str()), "回合記著是哪則收件匣事件收的");
    let unprocessed: Option<String> = sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE id=?").bind(&row.id).fetch_one(&app.db).await.unwrap();
    assert!(unprocessed.is_none(), "還沒標處理完：模擬重啟在 commit 與 mark_done 之間");

    // 重啟後 worker 把這列再處理一次。
    let mut events = app.subscribe();
    assert_eq!(crate::runners::hook_inbox::drain_once(&app).await.unwrap(), 1);

    assert_eq!(turn_count(&app, &conv).await, 1, "沒有第二個回合");
    assert_eq!(hook_message_count(&app, &conv).await, 1, "沒有第二則訊息");
    let done: Option<String> = sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE id=?").bind(&row.id).fetch_one(&app.db).await.unwrap();
    assert!(done.is_some(), "重播後這列標成處理完");
    // commit 後才發的通知補發了一次（前端靠訊息 id 去重）。
    let mut kinds = Vec::new();
    while let Ok(ev) = events.try_recv() {
        kinds.push(ev.kind);
    }
    assert!(kinds.iter().any(|k| k == "message_added"), "補發 message_added：{kinds:?}");
}

/// 同一個洞的另一條路：Stop 收掉的是 in-flight 的網頁回合，沒有 native id 時重播也不能多開一個外部回合。
#[tokio::test]
async fn an_inbox_row_replayed_after_an_in_flight_turn_was_completed_opens_no_external_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
    let body = stop_without_native_ids(&bot_id, "跑完了");
    assert!(crate::hook_inbox::accept(&app.db, &body, crate::hook_inbox::Source::Http).await.unwrap().is_new());
    let row = crate::hook_inbox::pending(&app.db, &db::now(), 10).await.unwrap().remove(0);

    process_for(&app, &body, Some(&row.id)).await.unwrap();
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(turn.status, "completed");
    let before = turn_count(&app, &conv).await;
    let messages_before = hook_message_count(&app, &conv).await;

    assert_eq!(crate::runners::hook_inbox::drain_once(&app).await.unwrap(), 1);

    assert_eq!(turn_count(&app, &conv).await, before, "重播不開外部回合");
    assert_eq!(hook_message_count(&app, &conv).await, messages_before, "也不多一則訊息");
}

/// 沒有收件匣 id（直接 `process`）時行為跟以前一樣：兩次獨立的、沒有身份的事件各是一個回合。
#[tokio::test]
async fn without_an_inbox_event_id_nothing_is_deduplicated_by_the_new_key() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "no-event-id").await;
    tt::fake_run(&app, &bot.id).await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    process(&app, &stop_without_native_ids(&bot.id, "一")).await.unwrap();
    process(&app, &stop_without_native_ids(&bot.id, "二")).await.unwrap();
    assert_eq!(turn_count(&app, &conv).await, 2);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE source_event_id IS NOT NULL").fetch_one(&app.db).await.unwrap();
    assert_eq!(n, 0);
}

/// 同一句（折疊排版空白後相同）就照舊認領並把 unknown 升成 ok。
#[tokio::test]
async fn a_hook_answering_the_same_prompt_still_resolves_the_unknown_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "codex", "跑一次   測試").await;

    process(&app, &codex_done(&bot_id, "跑一次 測試")).await.unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("completed", "ok"));
}

/// issue #753：遠端 bot 的 Stop 比備援晚 0.1–0.3 秒到，備援已把畫面抓到的字存成回覆。遠端 transcript 在 agm-host 讀不到，
/// Stop 本身又不帶使用者訊息，以前 `seen=None` → 不算同一回合 → 真回覆被丟。遠端 `hook.sh` 現在在 spool 前從本機 transcript
/// 把最後一則使用者訊息塞進 payload（`agm_user_text`）：對得上才蓋；對不上（別句）或沒帶都不蓋。
#[tokio::test]
async fn a_remote_stop_that_carries_the_users_text_replaces_the_fallback_reply() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "現在部 demo").await;
    sqlx::query("UPDATE turns SET status='completed_fallback', delivery='ok', completed_at=? WHERE id=?")
        .bind(db::now())
        .bind(&turn_id)
        .execute(&app.db)
        .await
        .unwrap();
    let fb = db::ulid();
    sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'assistant','Bash(herdr agent get …)','terminal_fallback',?)")
        .bind(&fb)
        .bind(&conv)
        .bind(&turn_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let stop = |sid: &str, carried: Option<&str>| {
        let mut payload = json!({
            "hook_event_name": "Stop",
            "session_id": sid,
            "prompt_id": db::ulid(),
            // mac_builder 的路徑：agm-host 讀不到。
            "transcript_path": "/Users/me/.claude/projects/x/s.jsonl",
            "last_assistant_message": "已經交給 memleak 排查，先看這幾點",
        });
        if let Some(t) = carried {
            payload["agm_user_text"] = json!(t);
        }
        HookBody { bot_id: bot_id.clone(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }
    };
    let reply = || async {
        sqlx::query_as::<_, (String, String)>("SELECT content, source FROM messages WHERE id=?").bind(&fb).fetch_one(&app.db).await.unwrap()
    };

    // 沒帶證據（舊版 hook.sh、沒有 python3）：照舊不蓋。
    process(&app, &stop("s1", None)).await.unwrap();
    assert_eq!(reply().await.1, "terminal_fallback", "沒有證據不蓋");
    assert_eq!(turn_row(&app, &turn_id).await.status, "completed_fallback");

    // 每個事件各自對上同一筆備援回合：清掉前一則記下的 native id（它是「這一回合已有 hook 來過」的去重鑰匙）。
    let forget_native = || async { sqlx::query("UPDATE turns SET native_session_id=NULL, native_turn_id=NULL WHERE id=?").bind(&turn_id).execute(&app.db).await.unwrap() };
    forget_native().await;
    // 帶的是別句：答案不是這一回合的，不蓋。
    process(&app, &stop("s2", Some("完全不同的另一句話"))).await.unwrap();
    assert_eq!(reply().await.1, "terminal_fallback", "別句不蓋");

    forget_native().await;
    // 帶的就是這一回合的 prompt：蓋成 hook 原文。
    process(&app, &stop("s3", Some("現在部 demo"))).await.unwrap();
    assert_eq!(reply().await, ("已經交給 memleak 排查，先看這幾點".to_string(), "hook".to_string()));
    assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
    let on_turn = replies(&app, &conv).await.into_iter().filter(|(t, _)| t.as_deref() == Some(turn_id.as_str())).count();
    assert_eq!(on_turn, 1, "這一回合不會變成兩則（別句那則在它自己的外部回合上）");
}

/// Remote transcript paths are only shape-checked because the real file lives on the remote
/// host. A same-shaped local file must not be read as evidence for a remote bot.
#[tokio::test]
async fn a_remote_stop_never_reads_a_local_file_at_its_transcript_path() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "預期 prompt").await;
    sqlx::query("UPDATE projects SET host='remote-test' WHERE id=?")
        .bind(&env.project_id)
        .execute(&app.db)
        .await
        .unwrap();

    let scratch = tt::track(std::env::temp_dir().join(format!("am-remote-transcript-{}", db::ulid())));
    std::fs::create_dir_all(&scratch).unwrap();
    let local_collision = scratch.join("remote.jsonl");
    let private_text = "LOCAL-ONLY-TRANSCRIPT-9274";
    std::fs::write(
        &local_collision,
        format!("{}\n", json!({"type":"user","message":{"role":"user","content":private_text}})),
    )
    .unwrap();

    process(&app, &HookBody {
        bot_id,
        provider: "claude".into(),
        payload: json!({
            "hook_event_name": "Stop",
            "session_id": "remote-session",
            "prompt_id": "remote-turn",
            "transcript_path": local_collision.to_string_lossy(),
            "last_assistant_message": "remote reply",
        }),
        received_at: None,
        truncated: false,
        run_id: None,
    }).await.unwrap();

    let leaked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE conversation_id=? AND content LIKE '%LOCAL-ONLY-TRANSCRIPT-9274%'",
    )
    .bind(&conv)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(leaked, 0, "daemon host 的同路徑檔案不可以當成 remote transcript 讀取");
    let target = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!((target.status.as_str(), target.delivery.as_str()), ("completed", "ok"), "看不到 remote transcript 時照原本規則認領回合");
}

/// Provider matching is deliberately case-insensitive; accepted casing must still dispatch
/// the event instead of durably consuming it as an unknown provider event.
#[tokio::test]
async fn accepted_provider_case_variants_still_process_stop_events() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _, turn_id) = unknown_turn(&app, &env.project_id, "claude", "prompt").await;

    process(&app, &HookBody {
        bot_id,
        provider: "Claude".into(),
        payload: json!({"hook_event_name":"Stop", "session_id":"case-session", "prompt_id":"case-turn",
            "last_assistant_message":"case reply"}),
        received_at: None,
        truncated: false,
        run_id: None,
    }).await.unwrap();

    let target = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!((target.status.as_str(), target.delivery.as_str()), ("completed", "ok"));
}

/// The hook's custom tail reader must share transcript_read's regular-file guard; a FIFO in
/// the bot's own projects directory must not strand a spawn_blocking worker waiting for a writer.
#[tokio::test]
async fn a_fifo_hook_transcript_does_not_block_stop_processing() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _, _) = unknown_turn(&app, &env.project_id, "claude", "prompt").await;
    let scratch = tt::track(std::env::temp_dir().join(format!("am-hook-fifo-{}", db::ulid())));
    let path = own_projects_file(&app, &bot_id, &scratch, "blocked.jsonl").await;
    assert!(std::process::Command::new("mkfifo").arg(&path).status().unwrap().success());
    let body = HookBody {
        bot_id,
        provider: "claude".into(),
        payload: json!({"hook_event_name":"Stop", "session_id":"fifo-session", "prompt_id":"fifo-turn",
            "transcript_path":path.to_string_lossy(), "last_assistant_message":"done"}),
        received_at: None,
        truncated: false,
        run_id: None,
    };
    let app_for_task = app.clone();
    let mut task = tokio::spawn(async move { process(&app_for_task, &body).await });
    let quickly_finished = tokio::time::timeout(Duration::from_secs(3), &mut task).await;
    if quickly_finished.is_err() {
        // Release a buggy blocking open before asserting, so the red test never leaks a thread.
        let fifo = path.clone();
        std::thread::spawn(move || drop(std::fs::OpenOptions::new().write(true).open(fifo).unwrap())).join().unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut task).await;
    }
    assert!(quickly_finished.is_ok(), "FIFO path held Stop processing for over three seconds");
    quickly_finished.unwrap().unwrap().unwrap();
}

/// #910：transcript 尾巴最後一列是 auto-compact 的摘要（`type:"user"`、`isCompactSummary`，內容是一大段
/// `This session is being continued…`）時，Stop 讀到的「最後一則使用者訊息」不能是它：`unknown` 的 in-flight 回合照樣被認領，
/// 不開外部回合，對話裡也不會多一則以摘要開頭的 user 訊息。
#[tokio::test]
async fn a_compact_summary_at_the_transcript_tail_does_not_hide_the_prompt_from_the_stop() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "做完那份長任務").await;
    let scratch = tt::track(std::env::temp_dir().join(format!("am-hook-compact-{}", db::ulid())));
    let path = own_projects_file(&app, &bot_id, &scratch, "compact.jsonl").await;
    let rows = [
        json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "做完那份長任務"}}),
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "做完了"}], "stop_reason": "end_turn"}}),
        json!({"type": "user", "uuid": "s1", "isCompactSummary": true, "isVisibleInTranscriptOnly": true,
            "message": {"role": "user", "content": "This session is being continued from a previous conversation that ran out of context. Summary: ..."}}),
    ];
    std::fs::write(&path, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
    assert_eq!(last_transcript_user_text(&path).as_deref(), Some("做完那份長任務"), "摘要列不是使用者訊息");

    process(
        &app,
        &HookBody {
            bot_id,
            provider: "claude".into(),
            payload: json!({"hook_event_name":"Stop", "session_id":"compact-session", "prompt_id":"compact-turn",
                "transcript_path": path.to_string_lossy(), "last_assistant_message":"做完了"}),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(turn.status, "completed", "原本的回合被認領");
    let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND origin='external'").bind(&conv).fetch_one(&app.db).await.unwrap();
    assert_eq!(external, 0, "沒有外部回合");
    let summaries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='user' AND content LIKE 'This session is being continued%'")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(summaries, 0, "對話裡沒有摘要當 user 訊息");
}

/// 備援剛把回合關掉、沒存回覆（這個 run 已經沒有 in-flight 回合）。
async fn fallback_closed_turn(app: &Arc<App>, project_id: &str, prompt: &str) -> (String, String, String) {
    let (bot_id, conv, turn_id) = unknown_turn(app, project_id, "codex", prompt).await;
    sqlx::query("UPDATE turns SET status='completed_fallback', delivery='ok', completed_at=? WHERE id=?")
        .bind(db::now())
        .bind(&turn_id)
        .execute(&app.db)
        .await
        .unwrap();
    (bot_id, conv, turn_id)
}

/// issue #1035：run 在主機 A 上起來（記下 A 的世代），`?confirm=repoint` 把同名主機改指到 B（世代換了），
/// 舊 run 的 Stop 這時才到。它是上一代主機的事件：一個欄位都不准動，也要留下 `hook_fenced`。
#[tokio::test]
async fn a_run_started_before_a_repoint_cannot_touch_the_repointed_host() {
    let env = tt::env().await;
    let app = env.app.clone();
    let host = format!("repoint-{}", db::ulid());
    let host_cfg = |ssh: &str| crate::config::HostCfg { daemon_url: None,
        shared_session: false,
        name: host.clone(),
        ssh: ssh.into(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "test".into(),
        remote_path: String::new(),
    };
    app.hosts
        .insert_remote_with_client_for_test(host_cfg("target-a"), crate::herdr::HerdrClient::new(env.dir.join("a.sock")))
        .await;
    sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id).execute(&app.db).await.unwrap();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'repointed','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();

    // 起在 A 上的 run：啟動當下記下 A 的目標指紋（start 會這樣寫，這裡直接種）。
    let gen_a = app.hosts.fence(&host).await.unwrap().target_epoch().unwrap();
    let run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, host, host_generation)
         VALUES (?,?,'running','working','pane-a','s-a',?,?,?)",
    )
    .bind(&run)
    .bind(&bot_id)
    .bind(db::now())
    .bind(&host)
    .bind(gen_a)
    .execute(&app.db)
    .await
    .unwrap();
    let turn = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','改指前問的',?)",
    )
    .bind(&turn)
    .bind(&conv)
    .bind(&run)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    assert_eq!(db::run_host_generation(&app.db, &run).await.unwrap(), Some(gen_a));

    // 同名主機改指到 B：世代換了。
    app.hosts
        .replace_remote_with_client_for_test(&app, host_cfg("target-b"), crate::herdr::HerdrClient::new(env.dir.join("b.sock")))
        .await;

    let mut events = app.subscribe();
    process(
        &app,
        &HookBody {
            bot_id: bot_id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-a", "prompt_id": "p-a",
                            "last_assistant_message": "改指前的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
    assert_eq!(t.status, "in_flight", "改指之前的 run 的 Stop 不准收回合");
    let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='assistant'").fetch_one(&app.db).await.unwrap();
    assert_eq!(replies, 0, "上一代主機的回覆不能貼進對話");
    let ev = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await.unwrap().unwrap();
    assert_eq!(ev.kind, "hook_fenced");
    assert_eq!(ev.data["why"], "host_generation");
}

fn stop_hook(bot_id: &str, session: &str, prompt: &str, reply: &str) -> HookBody {
    HookBody {
        bot_id: bot_id.into(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": "Stop", "session_id": session, "prompt_id": prompt, "last_assistant_message": reply}),
        received_at: None,
        truncated: false,
        run_id: None,
    }
}

async fn insert_in_flight_turn(pool: &sqlx::SqlitePool, turn: &str, conv: &str, run: &str, prompt: &str) {
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok',?,?)",
    )
    .bind(turn)
    .bind(conv)
    .bind(run)
    .bind(prompt)
    .bind(db::now())
    .execute(pool)
    .await
    .unwrap();
}

/// 回合是 hook 收的：狀態 `completed`，回覆的來源是 `hook`（不是終端備援）。
async fn assert_hook_completed(pool: &sqlx::SqlitePool, turn: &str, reply: &str) {
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn).fetch_one(pool).await.unwrap();
    assert_eq!(status, "completed", "hook 收得到的 Stop 不准被圍掉");
    let replies: Vec<(String, String)> =
        sqlx::query_as("SELECT content, source FROM messages WHERE turn_id=? AND role='assistant'").bind(turn).fetch_all(pool).await.unwrap();
    assert_eq!(replies, vec![(reply.to_string(), "hook".to_string())]);
}

/// issue #1773：遠端 run 在 mac_builder 上跑著，之後只改了 `remote_path`（同一台機器，連線換了一條、世代變了），或單純重連，
/// 這個 run 的 Stop 照常收成回合與原文。舊版拿連線世代比，這兩步一做，之後每一則 Stop 都被丟，回合只好由終端備援收尾（incomplete）。
#[tokio::test]
async fn a_same_machine_reconfigure_or_reconnect_keeps_the_run_hooks() {
    let env = tt::env().await;
    let app = env.app.clone();
    let host = format!("samemachine-{}", db::ulid());
    let host_cfg = |remote_path: &str| crate::config::HostCfg { daemon_url: None,
        shared_session: false,
        name: host.clone(),
        ssh: "mac_builder@100.64.0.2".into(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "test".into(),
        remote_path: remote_path.into(),
    };
    app.hosts
        .insert_remote_with_client_for_test(host_cfg(""), crate::herdr::HerdrClient::new(env.dir.join("a.sock")))
        .await;
    sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id).execute(&app.db).await.unwrap();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'same-machine','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();

    // run 啟動時記下的是目標指紋（start 會這樣寫）。
    let epoch = app.hosts.fence(&host).await.unwrap().target_epoch().unwrap();
    let run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, host, host_generation)
         VALUES (?,?,'running','working','pane-mac_builder','s-mac_builder',?,?,?)",
    )
    .bind(&run)
    .bind(&bot_id)
    .bind(db::now())
    .bind(&host)
    .bind(epoch)
    .execute(&app.db)
    .await
    .unwrap();

    // 1. 只改 remote_path：連線換了一條（世代變了），機器沒變。
    app.hosts
        .replace_remote_with_client_for_test(&app, host_cfg("/opt/bin"), crate::herdr::HerdrClient::new(env.dir.join("b.sock")))
        .await;
    let first = db::ulid();
    insert_in_flight_turn(&app.db, &first, &conv, &run, "第一句").await;
    process(&app, &stop_hook(&bot_id, "s-mac_builder", "p-1", "第一則的回覆")).await.unwrap();
    assert_hook_completed(&app.db, &first, "第一則的回覆").await;

    // 2. 重連：同一條設定，世代再加一。
    app.hosts.bump_generation_for_test(&host).await;
    let second = db::ulid();
    insert_in_flight_turn(&app.db, &second, &conv, &run, "第二句").await;
    process(&app, &stop_hook(&bot_id, "s-mac_builder", "p-2", "第二則的回覆")).await.unwrap();
    assert_hook_completed(&app.db, &second, "第二則的回覆").await;
}

/// issue #69 的 race，整條走一遍：使用者 interrupt → bot 重啟（舊 run 收掉、新 run 起來並開了新回合）
/// → 舊 CLI session 的 Stop 這時候才到。它是上一代的，一個欄位都不准動新回合。
#[tokio::test]
async fn a_late_stop_from_the_previous_run_cannot_touch_the_new_runs_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'fenced','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();

    // 上一代：跑在 `s-old` 上，使用者 interrupt 之後收掉（回合標 failed），run 結束。
    let old_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, ended_at)
         VALUES (?,?,'exited','unknown','pane-old','s-old',?,?)",
    )
    .bind(&old_run)
    .bind(&bot_id)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let old_turn = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, completed_at, created_at)
         VALUES (?,?,?,'web','failed','ok','上一代問的',?,?)",
    )
    .bind(&old_turn)
    .bind(&conv)
    .bind(&old_run)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    // 這一代：重啟起來的新 run，已經開了新回合，還在跑。世代圍籬（`lifecycle::fence`）認的是
    // **寫入順序**（rowid），不是這裡的 ULID 字典序（issue #98：兩者不保證一致，這行以前斷言
    // `new_run > old_run` 會在同一毫秒巧合下偶爾紅——跟這支測試實際要驗的東西無關，拿掉）。
    let new_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at)
         VALUES (?,?,'running','working','pane-new',?)",
    )
    .bind(&new_run)
    .bind(&bot_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let new_turn = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','這一代問的',?)",
    )
    .bind(&new_turn)
    .bind(&conv)
    .bind(&new_run)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    let mut events = app.subscribe();
    // 舊 session 的 Stop 現在才抵達。
    process(
        &app,
        &HookBody {
            bot_id: bot_id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-old", "prompt_id": "p-old",
                            "last_assistant_message": "上一代的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&new_turn).fetch_one(&app.db).await.unwrap();
    assert_eq!(t.status, "in_flight", "上一代的 Stop 不准收這一代的回合");
    assert_eq!(t.native_session_id, None, "也不准把舊 session 蓋到新回合上");
    let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='assistant'")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, 0, "上一代的回覆不能貼進這一代的對話");
    let run_session: Option<String> =
        sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&new_run).fetch_one(&app.db).await.unwrap();
    assert_eq!(run_session, None, "新 run 的 session 不能被舊事件寫進去");

    // 丟掉一則事件要看得見，不是只活在函式裡。
    let ev = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await.unwrap().unwrap();
    assert_eq!(ev.kind, "hook_fenced");
    assert_eq!(ev.data["prior_run_id"], serde_json::json!(old_run));
    assert_eq!(ev.data["session_id"], "s-old");

    // 重播同一則：還是什麼都不改（idempotent）。
    process(
        &app,
        &HookBody {
            bot_id: bot_id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "StopFailure", "session_id": "s-old", "prompt_id": "p-old",
                            "reason": "API Error: 500"}),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&new_turn).fetch_one(&app.db).await.unwrap();
    assert_eq!(t.status, "in_flight", "上一代的 StopFailure 也不准把這一代標成失敗");
}

/// 反過來的那一半：**這一代自己**的遲到 hook 照舊生效（`4fac036` 特意保留的行為）。
/// 圍籬只擋證明得出來是舊世代的，不是「遲到就丟」。
#[tokio::test]
async fn a_late_hook_from_this_same_run_still_fills_its_fallback_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次測試").await;
    // 這個 run 已經回報過自己的 session：hook 帶同一個，就是這一代自己的。
    sqlx::query("UPDATE runs SET native_session_id='s-live' WHERE bot_id=?")
        .bind(&bot_id)
        .execute(&app.db)
        .await
        .unwrap();

    let mut body = codex_done(&bot_id, "跑一次測試");
    body.payload["thread-id"] = json!("s-live");
    process(&app, &body).await.unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(turn.status, "completed", "同一代的遲到 hook 照舊補得進去");
    let replies: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, vec!["hook reply".to_string()]);
}

/// issue #92：撞額度 → 換身份 → `--resume` 接回**同一個** session。換身份前那個行程的 `StopFailure`
/// （撞額度那一則）這時候才到：它帶的 session 跟新 run 一模一樣，只看 session 會被認成這一代的，
/// 把換身份之後剛送出去的那一回合收成失敗。它自己帶的 run id 才分得開。
#[tokio::test]
async fn a_late_hook_from_the_process_before_a_resume_cannot_touch_the_resumed_runs_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "resumed").await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    // 身份 A 的那一代：session `s-same`，撞額度之後被換身份重啟收掉。
    let old_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, ended_at)
         VALUES (?,?,'stopped','unknown','pane-a','s-same',?,?)",
    )
    .bind(&old_run)
    .bind(&bot.id)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    // 身份 B 的這一代：`--resume s-same` 起來、SessionStart 已經對上（`native_session_id` 同一個），
    // 並且已經送出一則新的回合。
    let new_run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at)
         VALUES (?,?,'running','working','pane-b','s-same',?)",
    )
    .bind(&new_run)
    .bind(&bot.id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let new_turn = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','換身份之後問的',?)",
    )
    .bind(&new_turn)
    .bind(&conv)
    .bind(&new_run)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn_now = |id: String| {
        let app = app.clone();
        async move { sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap() }
    };
    let from = |run: &str, payload: Value| HookBody {
        bot_id: bot.id.clone(),
        provider: "claude".into(),
        payload,
        received_at: None,
        truncated: false,
        run_id: Some(run.to_string()),
    };

    let mut events = app.subscribe();
    let limit = json!({"hook_event_name": "StopFailure", "session_id": "s-same", "prompt_id": "p-a",
                       "reason": "You've hit your usage limit · resets 5pm"});
    process(&app, &from(&old_run, limit)).await.unwrap();
    assert_eq!(turn_now(new_turn.clone()).await.status, "in_flight", "身份 A 遲到的撞額度不准收掉身份 B 的回合");
    let ev = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await.unwrap().unwrap();
    assert_eq!((ev.kind.as_str(), ev.data["prior_run_id"].as_str()), ("hook_fenced", Some(old_run.as_str())));

    let reply = json!({"hook_event_name": "Stop", "session_id": "s-same", "prompt_id": "p-a",
                       "last_assistant_message": "身份 A 那一回合的半截回覆"});
    process(&app, &from(&old_run, reply)).await.unwrap();
    let t = turn_now(new_turn.clone()).await;
    assert_eq!((t.status.as_str(), t.native_turn_id.as_deref()), ("in_flight", None), "也不准拿舊行程的 Stop 收尾");
    let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='assistant'")
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, 0, "舊行程的回覆不能貼進來，也不能變成一筆外部回合");

    // 對照：同一個 session、這一代自己的行程送的——照常收（證明擋下前兩則的是 run id，不是別的條件）。
    let own = json!({"hook_event_name": "StopFailure", "session_id": "s-same", "prompt_id": "p-b",
                     "reason": "You've hit your usage limit · resets 5pm"});
    process(&app, &from(&new_run, own)).await.unwrap();
    assert_eq!(turn_now(new_turn).await.status, "failed");
}

/// review3 c1 L13：沒有 in-flight 回合時，遲到 hook 帶來的是**別句**的答案（使用者改到 pane 裡直接打）——
/// 不能補進 120 秒內那筆備援回合、更不能把它標成 completed，答案記在外部回合上。
#[tokio::test]
async fn a_late_hook_answering_another_prompt_does_not_fill_the_fallback_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次測試").await;

    process(&app, &codex_done(&bot_id, "順便看一下 lint")).await.unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(turn.status, "completed_fallback", "T1 原封不動");
    let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, 0, "別句的答案不能寫成 T1 的回覆");
    let external: Vec<String> = sqlx::query_scalar(
        "SELECT m.content FROM turns t JOIN messages m ON m.turn_id=t.id
          WHERE t.conversation_id=? AND t.origin='external' ORDER BY m.role DESC",
    )
    .bind(&conv)
    .fetch_all(&app.db)
    .await
    .unwrap();
    assert_eq!(external, vec!["順便看一下 lint".to_string(), "hook reply".to_string()], "答案跟它的問題一起記在外部回合");
}

/// #297：備援關掉 T1、使用者接著送 T2（已送達、in-flight），T1 的真回覆這時才到——答案屬於 T1，
/// 不能把 T2 收成 completed 還貼上 T1 的回覆（以前只有 `delivery = unknown` 才比對使用者訊息）。
#[tokio::test]
async fn a_late_hook_for_the_fallback_turn_does_not_complete_the_next_in_flight_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, t1) = fallback_closed_turn(&app, &env.project_id, "第一句").await;
    let run_id: String = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&t1).fetch_one(&app.db).await.unwrap();
    let t2 = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','第二句',?)",
    )
    .bind(&t2)
    .bind(&conv)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    process(&app, &codex_done(&bot_id, "第一句")).await.unwrap();

    let turn = |id: String| {
        let app = app.clone();
        async move { sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap() }
    };
    assert_eq!(turn(t2.clone()).await.status, "in_flight", "T2 還在跑，T1 的回覆不能收掉它");
    let on_t2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&t2)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(on_t2, 0, "T1 的回覆不能貼在 T2 上");
    assert_eq!(turn(t1).await.status, "completed", "回覆補回它真正的回合");
}

/// 同一句就照舊補進去（2026-09-13 GROK 那種）。
#[tokio::test]
async fn a_late_hook_answering_the_same_prompt_still_fills_the_fallback_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次   測試").await;

    process(&app, &codex_done(&bot_id, "跑一次 測試")).await.unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(turn.status, "completed");
    let replies: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, vec!["hook reply".to_string()]);
}

/// review3 c1 M3：交辦先帶著「沒有回覆」結算（備援關掉回合），遲到 hook 補上回覆之後，回覆要寫回交辦並通知驗收者。
#[tokio::test]
async fn a_late_hook_reply_reaches_the_assignment_it_answers() {
    use crate::supervisor::{controller, store};
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次測試").await;
    store::get_or_init(&app.db).await.unwrap();
    let a = store::insert_assignment(&app.db, None, &bot_id, "crid-late", "跑一次測試", &[], None, true).await.unwrap();
    store::mark_delivered(&app.db, &a.id, &turn_id, "ok").await.unwrap();
    // controller 在備援收掉回合時的結算：沒有回覆、證據不完整。
    store::settle_and_notify(&app.db, &a.id, "completed_fallback", false, None, None, "k-fallback", "assignment_completed", &json!({}))
        .await
        .unwrap();

    process(&app, &codex_done(&bot_id, "跑一次測試")).await.unwrap();
    // 補寫會推 turn_updated（completed）；controller 的事件迴圈收到後呼叫這一支。
    controller::late_reply_for_turn(&app, &turn_id, "completed").await;

    let row = store::assignment(&app.db, &a.id).await.unwrap().unwrap();
    assert_eq!(row.status, "awaiting_review", "還是等驗收，只是結果到了");
    assert_eq!(row.result.as_deref(), Some("hook reply"));
    assert_eq!(row.turn_status.as_deref(), Some("completed"));
    assert_eq!(row.evidence_complete, Some(1));
    let events: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, payload_json FROM supervisor_inbox WHERE assignment_id=? AND event_key LIKE 'assignment_late_reply:%'")
            .bind(&a.id)
            .fetch_all(&app.db)
            .await
            .unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    let p: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(events[0].0, "assignment_completed");
    assert_eq!((p["result"].as_str(), p["late_reply"].as_bool(), p["needs_review"].as_bool()), (Some("hook reply"), Some(true), Some(true)));

    // 同一個事件再來一次（重播、reconcile）：不再推第二則。
    controller::late_reply_for_turn(&app, &turn_id, "completed").await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE assignment_id=?").bind(&a.id).fetch_one(&app.db).await.unwrap();
    assert_eq!(n, 2, "結算那則＋結果到了那則");
}

/// 一顆 claude bot＋一筆已經送達（`delivery='ok'`）、還在飛的回合。StopFailure 要收的就是這種。
async fn delivered_turn(app: &Arc<App>, project_id: &str) -> (String, String, String) {
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(project_id)
    // ULID 的前段是時間戳：同一毫秒建的兩顆 bot 前六碼一樣，撞 `bots.project_id, bots.name`。取尾段。
    .bind(format!("hook-{}", &bot_id[bot_id.len() - 8..]))
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
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
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','跑一次測試',?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    (bot_id, conversation_id, turn_id)
}

fn stop_failure(bot_id: &str, payload: Value) -> HookBody {
    HookBody { bot_id: bot_id.to_string(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }
}

async fn turn_row(app: &Arc<App>, id: &str) -> db::Turn {
    sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
}

async fn system_notes(app: &Arc<App>, turn_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
        .bind(turn_id)
        .fetch_all(&app.db)
        .await
        .unwrap()
}

/// issue #79：`StopFailure` 一到就把回合收成失敗，而且看得出為什麼——以前要等 §4.3 備援或
/// stuck watchdog 才發現，中間一直掛在 in_flight。`delivery` 不動：字是送出去了，失敗的是回合。
#[tokio::test]
async fn a_stop_failure_closes_the_turn_as_failed_with_a_readable_reason() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;

    process(
        &app,
        &stop_failure(
            &bot_id,
            json!({"hook_event_name": "StopFailure", "session_id": "s1", "prompt_id": "p1",
                   "reason": "API Error: 429 rate_limit_error"}),
        ),
    )
    .await
    .unwrap();

    let t = turn_row(&app, &turn_id).await;
    assert_eq!(t.status, "failed", "回合當場收成失敗，不必等 watchdog");
    assert_eq!(t.delivery, "ok", "送達與回合成敗是兩件事");
    assert!(t.completed_at.is_some());
    assert_eq!((t.native_session_id.as_deref(), t.native_turn_id.as_deref()), (Some("s1"), Some("p1")), "認得出是哪一回合");
    let notes = system_notes(&app, &turn_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("額度或速率限制"), "分類要看得出來：{notes:?}");
    assert!(notes[0].contains("429 rate_limit_error"), "原文也要留著：{notes:?}");
}

/// 使用者自己按停不是 provider 失敗。兩條路都要擋：payload 說得出是中斷時照它說的；
/// 說不出來時看 daemon 自己的紀錄（`interrupt_bot` 先記下被中斷的是哪一回合才收 turn）。
#[tokio::test]
async fn a_user_interrupt_is_never_recorded_as_a_provider_failure() {
    let env = tt::env().await;
    let app = env.app.clone();

    // (a) payload 自己說是中斷。
    let (bot_a, _c, turn_a) = delivered_turn(&app, &env.project_id).await;
    process(
        &app,
        &stop_failure(
            &bot_a,
            json!({"hook_event_name": "StopFailure", "session_id": "sa", "prompt_id": "pa",
                   "reason": "[Request interrupted by user]"}),
        ),
    )
    .await
    .unwrap();
    let t = turn_row(&app, &turn_a).await;
    assert_eq!(t.status, "in_flight", "使用者中斷不由這條路收尾");
    assert!(system_notes(&app, &turn_a).await.is_empty(), "也不寫「失敗」的說明");

    // (b) payload 說不出原因，但 daemon 記得使用者剛按停的就是這一筆（`fail_in_flight` 還沒收掉它）。
    let (bot_b, _c, turn_b) = delivered_turn(&app, &env.project_id).await;
    let run_b = turn_row(&app, &turn_b).await.run_id.unwrap();
    lifecycle::expect_interrupt_echo(
        &bot_b,
        lifecycle::InterruptedTurn {
            run_id: run_b,
            turn_id: Some(turn_b.clone()),
            session_id: None,
            prompt_id: None,
            at: chrono::Utc::now(),
        },
    );
    process(&app, &stop_failure(&bot_b, json!({"hook_event_name": "StopFailure", "session_id": "sb", "prompt_id": "pb"})))
        .await
        .unwrap();
    let t = turn_row(&app, &turn_b).await;
    assert_eq!(t.status, "in_flight", "剛按過停：這則 StopFailure 是那次 Esc 的回聲");
    assert!(system_notes(&app, &turn_b).await.is_empty());
}

/// 後到的 StopFailure 不會把已經收好的回合再收一次，重播的同一則也不會（issue #79 驗收第二條）。
#[tokio::test]
async fn a_late_or_replayed_stop_failure_does_not_close_a_turn_twice() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
    // Stop 先到，回合已經答完。
    sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?")
        .bind(db::now())
        .bind(&turn_id)
        .execute(&app.db)
        .await
        .unwrap();

    let ev = stop_failure(
        &bot_id,
        json!({"hook_event_name": "StopFailure", "session_id": "s9", "prompt_id": "p9", "reason": "API Error: 500"}),
    );
    process(&app, &ev).await.unwrap();

    let t = turn_row(&app, &turn_id).await;
    assert_eq!(t.status, "completed", "已經收好的不再被改成 failed");
    assert!(system_notes(&app, &turn_id).await.is_empty(), "也不補一則失敗說明");

    // 同一則重播（spool 重送）：照樣什麼都不動。
    process(&app, &ev).await.unwrap();
    assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
    assert!(system_notes(&app, &turn_id).await.is_empty());
}

/// 去重是「這顆 bot 自己」的事：另一顆 bot 的 token 驗得過，但它不能靠先送一則帶著別人 session／turn id 的 hook，
/// 把那組 id 先佔走，讓真正屬於那顆 bot 的完成事件被當成「重複」吞掉（回合卡 in_flight 到備援才收）。
#[tokio::test]
async fn another_bots_hook_cannot_pre_claim_my_session_and_turn_ids() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (attacker, _, attacker_turn) = delivered_turn(&app, &env.project_id).await;
    let (victim, _, victim_turn) = delivered_turn(&app, &env.project_id).await;
    let stop = |bot: &str, msg: &str| {
        stop_failure(bot, json!({"hook_event_name": "Stop", "session_id": "s-shared", "prompt_id": "p-shared", "last_assistant_message": msg}))
    };
    process(&app, &stop(&attacker, "我先到")).await.unwrap();
    assert_eq!(turn_row(&app, &attacker_turn).await.status, "completed");

    process(&app, &stop(&victim, "我才是被問的那顆")).await.unwrap();
    assert_eq!(turn_row(&app, &victim_turn).await.status, "completed", "別顆 bot 先用過同一組 id，不能讓我的完成事件被當成重複");

    // 失敗事件同一道理。
    let (attacker2, _, _) = delivered_turn(&app, &env.project_id).await;
    let (victim2, _, victim2_turn) = delivered_turn(&app, &env.project_id).await;
    let fail = |bot: &str| stop_failure(bot, json!({"hook_event_name": "StopFailure", "session_id": "s-f", "prompt_id": "p-f", "reason": "API Error: 500"}));
    process(&app, &fail(&attacker2)).await.unwrap();
    process(&app, &fail(&victim2)).await.unwrap();
    assert_eq!(turn_row(&app, &victim2_turn).await.status, "failed");
}

/// 外部回合（終端手打、沒有 in-flight 的回合）那條路一樣：別顆 bot 佔過的 id 不能讓這一筆 insert 撞唯一索引、一直重試。
#[tokio::test]
async fn an_external_turn_survives_native_ids_another_bot_already_used() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (attacker, _, attacker_turn) = delivered_turn(&app, &env.project_id).await;
    let (victim, victim_conv, victim_turn) = delivered_turn(&app, &env.project_id).await;
    // 受害者那顆沒有 in-flight 的回合（使用者在終端手打的）。
    sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&victim_turn).execute(&app.db).await.unwrap();
    let stop = |bot: &str| stop_failure(bot, json!({"hook_event_name": "Stop", "session_id": "s-ext", "prompt_id": "p-ext", "last_assistant_message": "終端那句的回答"}));
    process(&app, &stop(&attacker)).await.unwrap();
    assert_eq!(turn_row(&app, &attacker_turn).await.status, "completed");
    process(&app, &stop(&victim)).await.expect("撞到別顆 bot 的 native id 也要收得下");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='assistant' AND content='終端那句的回答'")
        .bind(&victim_conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(n, 1, "這顆 bot 的外部回合與回覆都記下來了");
}

/// StatusLine 是「最新的贏」的單槽訊號（每次重繪一則）：bot 鎖被長時間握著時，不能每一則各丟一個背景 task 排隊等鎖
/// （每個都抱著一份 body，高頻時 task 與記憶體一路疊上去）。同一顆 bot 最多一個在等，後到的取代先到的。
#[tokio::test]
async fn statusline_hooks_queued_behind_a_busy_bot_lock_are_coalesced() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "status-flood").await;
    let run_id = tt::fake_run(&app, &bot.id).await;
    let lock = app.bot_lock(&bot.id).await;
    let held = lock.lock().await;
    let spawned_before = statusline_tasks_spawned();
    for i in 0..50 {
        let mut headers = HeaderMap::new();
        headers.insert("X-AM-Bot-Token", bot.hook_token.parse().unwrap());
        let body = HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "StatusLine", "session_id": "s-flood", "model": {"id": format!("claude-model-{i}")}}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        let (status, _) = receive(State(app.clone()), Path("claude".to_string()), headers, Json(body)).await;
        assert_eq!(status, StatusCode::OK);
    }
    let spawned = statusline_tasks_spawned() - spawned_before;
    assert!(spawned <= 2, "50 則 StatusLine 排在忙碌的 bot 鎖後面，背景 task 只該有 1～2 個，不是 {spawned}");
    drop(held);
    // 放鎖之後最後一則（最新的）贏。
    assert!(
        crate::testing::eventually!(db::run(&app.db, &run_id).await.unwrap().unwrap().runtime_model.as_deref() == Some("claude-model-49")),
        "最新的那一則要生效"
    );
}

/// 型別怪的 payload（字串、數字、陣列、null、很深的巢狀）不能 panic，也不能讓整批卡住：處理完就是處理完。
#[tokio::test]
async fn payloads_of_the_wrong_shape_are_handled_without_panicking() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
    let mut deep = json!("x");
    for _ in 0..200 {
        deep = json!({ "a": deep });
    }
    for payload in [
        json!("a string"),
        json!(5),
        json!([1, 2, 3]),
        json!(null),
        json!({"hook_event_name": 5}),
        json!({"hook_event_name": "PostToolUse", "tool_use_id": null, "tool_response": deep}),
        json!({"hook_event_name": "UserPromptSubmit", "prompt": 12}),
    ] {
        let ev = stop_failure(&bot_id, payload.clone());
        process(&app, &ev).await.unwrap_or_else(|e| panic!("{payload}: {e:#}"));
    }
    // 這一串怪事件沒有誤收掉那顆還在跑的回合。
    assert_eq!(turn_row(&app, &turn_id).await.status, "in_flight");
    // Stop 的欄位型別全錯：照樣只是「這一回合結束了」，不 panic、也沒有把垃圾當成回覆。
    let weird_stop = stop_failure(
        &bot_id,
        json!({"hook_event_name": "Stop", "session_id": 7, "prompt_id": ["x"], "last_assistant_message": {"not": "text"}}),
    );
    process(&app, &weird_stop).await.unwrap();
    assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
}

/// 收件匣的重試（issue #70）要能補回第一次只做了一半的處理。第一次認領回合、寫下 native id 之後，
/// 回覆那句 insert 失敗（這裡用 trigger 模擬）：重試時同一組 native id 會被去重擋掉——認領跟回覆
/// 不在同一個交易裡的話，這則回覆就永遠不見了，回合卻已經是 completed。
async fn fail_next_insert(app: &Arc<App>, role: &str) {
    sqlx::query(&format!(
        "CREATE TRIGGER flaky_insert BEFORE INSERT ON messages WHEN NEW.role='{role}'
         BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END"
    ))
    .execute(&app.db)
    .await
    .unwrap();
}

async fn heal(app: &Arc<App>) {
    sqlx::query("DROP TRIGGER flaky_insert").execute(&app.db).await.unwrap();
}

async fn replies(app: &Arc<App>, conv: &str) -> Vec<(Option<String>, String)> {
    sqlx::query_as("SELECT turn_id, content FROM messages WHERE conversation_id=? AND role='assistant'")
        .bind(conv)
        .fetch_all(&app.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_reply_whose_first_attempt_failed_halfway_survives_the_retry() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = delivered_turn(&app, &env.project_id).await;
    let ev = stop_failure(
        &bot_id,
        json!({"hook_event_name": "Stop", "session_id": "s-r", "prompt_id": "p-r", "last_assistant_message": "測試全綠"}),
    );

    fail_next_insert(&app, "assistant").await;
    assert!(process(&app, &ev).await.is_err(), "第一次寫不進回覆：錯誤要往上傳，收件匣才會重試");
    heal(&app).await;
    process(&app, &ev).await.expect("重試");

    assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
    assert_eq!(replies(&app, &conv).await, vec![(Some(turn_id), "測試全綠".to_string())], "回覆不能在重試時被去重吃掉");
}

/// 同一件事，外部回合那條路（沒有 in-flight 的回合，hook 自己開一筆 completed 的）。
#[tokio::test]
async fn an_external_reply_whose_first_attempt_failed_halfway_survives_the_retry() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = delivered_turn(&app, &env.project_id).await;
    sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&turn_id).execute(&app.db).await.unwrap();
    let ev = stop_failure(
        &bot_id,
        json!({"hook_event_name": "Stop", "session_id": "s-x", "prompt_id": "p-x", "last_assistant_message": "終端手打那句的回答"}),
    );

    fail_next_insert(&app, "assistant").await;
    assert!(process(&app, &ev).await.is_err());
    heal(&app).await;
    process(&app, &ev).await.expect("重試");

    let got = replies(&app, &conv).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].1, "終端手打那句的回答");
}

/// 同一件事，`StopFailure`：回合收成 failed 之後說明寫不進去，重試不能只剩一筆沒有原因的失敗回合。
#[tokio::test]
async fn a_stop_failure_note_whose_first_attempt_failed_survives_the_retry() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
    let ev = stop_failure(
        &bot_id,
        json!({"hook_event_name": "StopFailure", "session_id": "s-f", "prompt_id": "p-f", "reason": "API Error: 500"}),
    );

    fail_next_insert(&app, "system").await;
    assert!(process(&app, &ev).await.is_err());
    heal(&app).await;
    process(&app, &ev).await.expect("重試");

    assert_eq!(turn_row(&app, &turn_id).await.status, "failed");
    let notes = system_notes(&app, &turn_id).await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("API Error: 500"), "{notes:?}");
}

/// 同一件事，遲到的 hook 補備援關掉的回合（`fill_or_drop_late_hook`）：先寫 native id 再補回覆的話，
/// 回覆失敗後重試被去重擋掉，回合還被升級成 completed、卻沒有回覆。
#[tokio::test]
async fn a_late_fill_whose_first_attempt_failed_survives_the_retry() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次 測試").await;
    let ev = codex_done(&bot_id, "跑一次 測試");

    fail_next_insert(&app, "assistant").await;
    assert!(process(&app, &ev).await.is_err());
    heal(&app).await;
    process(&app, &ev).await.expect("重試");

    assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
    assert_eq!(replies(&app, &conv).await, vec![(Some(turn_id), "hook reply".to_string())]);
}

/// issue #108：撞額度的 StopFailure 在推回合結束**之前**就把撞限記下來——那個事件會叫醒 queue flush，
/// flush 要看得到這個身份沒額度。一下就好的限流（overloaded）不記：記了會把派工壓上好幾個小時。
#[tokio::test]
async fn a_quota_stop_failure_records_the_limit_before_the_turn_ends() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
    let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
    process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "StopFailure", "session_id": "s1", "prompt_id": "p1",
                                                "reason": "API Error: 529 overloaded_error"})))
        .await
        .unwrap();
    assert_eq!(turn_row(&app, &turn_id).await.status, "failed");
    assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "過載不是額度用完");

    let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
    let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
    process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "StopFailure", "session_id": "s2", "prompt_id": "p2",
                                                "reason": "You've hit your session limit · resets 5pm"})))
        .await
        .unwrap();
    assert_eq!(turn_row(&app, &turn_id).await.status, "failed");
    let hit = crate::runners::quota::limit_hit_for_bot(&app, &bot).await.expect("撞限記下來了");
    assert_eq!(hit.bucket.as_deref(), Some("five_hour"));
    assert!(hit.until.is_some(), "有期限，不會永遠擋著");
}

/// #108 重開：撞額度的 `StopFailure` 到的時候讀不到這顆（遠端）bot 在哪台主機。以前退回 `local`：撞限寫進本機帳號
/// 那一格，回合照收、推回合結束，flush 查遠端那把 key 看不到撞限，排著的派工當場送進用盡的身份。現在這一則失敗、
/// 由收件匣重試：哪一格都不寫、回合不收（在飛的回合本身擋著 flush），撞限記成欠著——就算回合被別的路收掉，排著的
/// 照欠著的那一筆擋。讀得到之後重試：撞限進 `remote1/claude`，回合照常收成失敗。讀不到 bot 本身也一樣是這一則失敗。
#[tokio::test]
async fn a_quota_stop_failure_whose_host_cannot_be_read_fails_and_holds_the_queue() {
    let env = tt::env().await;
    let app = env.app.clone();
    let remote = db::ulid();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
        .bind(&remote)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let (bot_id, conv, turn_id) = delivered_turn(&app, &remote).await;
    let queued = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','下一件派工',?)")
        .bind(&queued)
        .bind(&conv)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let ev = stop_failure(&bot_id, json!({"hook_event_name": "StopFailure", "session_id": "s-far", "prompt_id": "p-far",
                                          "reason": "You've hit your session limit · resets 5pm"}));
    let limit_hits = || {
        let app = app.clone();
        async move { app.quotas.lock().await.iter().filter(|(_, q)| q.limit_hit.is_some()).map(|(k, _)| k.clone()).collect::<Vec<_>>() }
    };

    sqlx::query("ALTER TABLE bots RENAME TO bots_unreadable").execute(&app.db).await.unwrap();
    assert!(process(&app, &ev).await.is_err(), "讀不到 bot：這一則失敗、收件匣重試");
    sqlx::query("ALTER TABLE bots_unreadable RENAME TO bots").execute(&app.db).await.unwrap();
    assert_eq!(turn_row(&app, &turn_id).await.status, "in_flight");

    sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
    assert!(process(&app, &ev).await.is_err(), "讀不到主機：不當作沒撞限");
    assert_eq!(limit_hits().await, Vec::<String>::new(), "哪一格都沒寫，尤其不是本機的 `claude`");
    assert!(crate::turn_error::owes_limit_hit(&app, &bot_id), "撞限記成欠著");
    let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
    let owed_at = crate::runners::quota::limit_hit_for_bot(&app, &bot).await.expect("派送前照欠著的那一筆擋").at;
    assert_eq!(turn_row(&app, &turn_id).await.status, "in_flight", "回合不收、不推回合結束");
    // 回合被別的路收掉（卡住的回合被看門狗收了）：沒有在飛的回合擋著，排著的照欠著的那一筆擋。
    let run: String = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=?").bind(db::now()).bind(&turn_id).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
    lifecycle::forget_queue_retry_timer(&bot_id);
    lifecycle::flush_queued_locked(&app, &bot_id).await.unwrap();
    let q = turn_row(&app, &queued).await;
    assert_eq!((q.status.as_str(), q.flush_retries, q.run_id.as_deref()), ("queued", 0, None), "派工沒有送進用盡的身份");
    sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();

    // 收件匣重試：讀得到了。
    process(&app, &ev).await.expect("重試");
    assert_eq!(limit_hits().await, vec!["remote1/claude".to_string()], "記在遠端那一格");
    assert_eq!(app.quotas.lock().await["remote1/claude"].limit_hit.as_ref().map(|h| h.at.clone()), Some(owed_at), "撞限時刻是當初那一刻，不是重試的時刻");
    assert!(!crate::turn_error::owes_limit_hit(&app, &bot_id));
    lifecycle::forget_queue_retry_timer(&bot_id);
    lifecycle::flush_queued_locked(&app, &bot_id).await.unwrap();
    let q = turn_row(&app, &queued).await;
    assert_eq!((q.status.as_str(), q.flush_retries), ("queued", 0), "照撞限擋");
    let hold: Option<String> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(&queued).fetch_one(&app.db).await.unwrap();
    assert!(hold.is_some(), "憑據落地");
    lifecycle::forget_queue_retry_timer(&bot_id);
    crate::lifecycle::quota_hold::forget_held(&bot_id);
}

/// #108 重開：遠端 bot 的 statusLine 到的時候讀不到它在哪台主機。以前退回 `local`：遠端帳號的讀數寫進本機那一格，
/// 還拿它去校正本機帳號的撞限——那個窗是撞限之後才開的，本機真的撞限就被作廢了。現在丟掉這一份，下一次重繪再來。
#[tokio::test]
async fn a_status_line_whose_host_cannot_be_read_is_dropped_not_written_to_the_local_key() {
    let env = tt::env().await;
    let app = env.app.clone();
    let remote = db::ulid();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
        .bind(&remote)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let (bot_id, _conv, _turn) = delivered_turn(&app, &remote).await;
    let until = db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2));
    assert!(crate::quota::seed_limit_hit(&app, crate::config::LOCAL_HOST, "claude", &until, "You've hit your session limit", Some("five_hour".into())).await);
    let reopened = (chrono::Utc::now() + chrono::Duration::hours(5) + chrono::Duration::seconds(30)).timestamp();
    let line = HookBody {
        bot_id: bot_id.clone(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": "StatusLine", "rate_limits": {"five_hour": {"used_percentage": 3.0, "resets_at": reopened}}}),
        received_at: None,
        truncated: false,
        run_id: None,
    };

    sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
    assert!(process(&app, &line).await.is_err(), "讀不到主機：丟掉這一份");
    sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
    {
        let q = app.quotas.lock().await;
        assert!(q["claude"].limit_hit.is_some(), "本機帳號的撞限沒被遠端的讀數作廢");
        assert!(q["claude"].five_hour.is_none(), "遠端的讀數沒寫進本機那一格");
    }
    process(&app, &line).await.unwrap();
    let q = app.quotas.lock().await;
    assert_eq!(q.get("remote1/claude").and_then(|x| x.five_hour.as_ref()).map(|w| w.used_pct), Some(3.0), "讀得到：寫進遠端那一格");
    assert!(q["claude"].limit_hit.is_some());
}

/// issue #150：撞額度是帳號的事實，跟這一則還有沒有回合可收無關。Esc 把回合收掉之後，同一個身份晚到的真撞額度
/// （對不上中斷的那一則、或就是被中斷那一則的回聲）都要記下撞限，下一件派工才不會送進沒額度的身份；
/// 回合本身一個欄位都不改。沒有 run（說不準是哪個身份）與已經收過的同一則（重播）不記。
#[tokio::test]
async fn a_quota_stop_failure_after_an_esc_still_records_the_limit() {
    let env = tt::env().await;
    let app = env.app.clone();
    const LIMIT: &str = "You've hit your session limit · resets 5pm";

    // (a) Esc 已經把回合收掉（`fail_in_flight`），晚到的撞額度對不上被中斷的那一則：沒有 in-flight 回合可掛。
    let (bot_a, conv_a, turn_a) = delivered_turn(&app, &env.project_id).await;
    let run_a = turn_row(&app, &turn_a).await.run_id.unwrap();
    lifecycle::expect_interrupt_echo(
        &bot_a,
        lifecycle::InterruptedTurn { run_id: run_a.clone(), turn_id: Some(turn_a.clone()), session_id: None, prompt_id: Some("p-esc".into()), at: chrono::Utc::now() },
    );
    lifecycle::fail_in_flight(&app, &run_a, "user interrupt").await.unwrap();
    let closed = turn_row(&app, &turn_a).await;
    process(&app, &stop_failure(&bot_a, json!({"hook_event_name": "StopFailure", "session_id": "sa", "prompt_id": "p-late", "reason": LIMIT})))
        .await
        .unwrap();
    let bot = db::bot(&app.db, &bot_a).await.unwrap().unwrap();
    assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "沒有回合可掛，撞限照樣記下");
    let after = turn_row(&app, &turn_a).await;
    assert_eq!((after.status, after.completed_at), (closed.status, closed.completed_at), "已經收掉的回合不改");
    // 下一件派工：排在佇列的那一則不會被送進 A。
    let queued = db::ulid();
    sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run_a).execute(&app.db).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','下一件派工',?)")
        .bind(&queued)
        .bind(&conv_a)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    lifecycle::forget_queue_retry_timer(&bot_a);
    lifecycle::flush_queued_locked(&app, &bot_a).await.unwrap();
    let q = turn_row(&app, &queued).await;
    assert_eq!((q.status.as_str(), q.flush_retries, q.run_id.as_deref()), ("queued", 0, None), "派工沒有送進沒額度的 A");
    lifecycle::forget_queue_retry_timer(&bot_a);

    // (b) 就是被中斷那一則的回聲（同一個 prompt id）：回合照舊一個欄位都不動，撞限一樣記。
    let (bot_b, _c, turn_b) = delivered_turn(&app, &env.project_id).await;
    let run_b = turn_row(&app, &turn_b).await.run_id.unwrap();
    lifecycle::expect_interrupt_echo(
        &bot_b,
        lifecycle::InterruptedTurn { run_id: run_b, turn_id: Some(turn_b.clone()), session_id: None, prompt_id: Some("p-b".into()), at: chrono::Utc::now() },
    );
    process(&app, &stop_failure(&bot_b, json!({"hook_event_name": "StopFailure", "session_id": "sb", "prompt_id": "p-b", "reason": LIMIT})))
        .await
        .unwrap();
    assert_eq!(turn_row(&app, &turn_b).await.status, "in_flight", "回聲：回合不動");
    let bot = db::bot(&app.db, &bot_b).await.unwrap().unwrap();
    assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "回聲裡的撞額度也是真的");

    // (c) 沒有 run：說不準是哪個身份送的（停機後可能換過身份），不記。
    let (bot_c, _c, turn_c) = delivered_turn(&app, &env.project_id).await;
    let run_c = turn_row(&app, &turn_c).await.run_id.unwrap();
    sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&run_c).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE bots SET identity='cc9' WHERE id=?").bind(&bot_c).execute(&app.db).await.unwrap();
    process(&app, &stop_failure(&bot_c, json!({"hook_event_name": "StopFailure", "session_id": "sc", "prompt_id": "p-c", "reason": LIMIT})))
        .await
        .unwrap();
    let bot = db::bot(&app.db, &bot_c).await.unwrap().unwrap();
    assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "沒有 run 的不記到現在的身份上");

    // (d) 已經收過的同一則重播：不再記一次（不把撞限時刻往後推）。
    let (bot_d, _c, turn_d) = delivered_turn(&app, &env.project_id).await;
    let ev = stop_failure(&bot_d, json!({"hook_event_name": "StopFailure", "session_id": "sd", "prompt_id": "p-d", "reason": LIMIT}));
    process(&app, &ev).await.unwrap();
    assert_eq!(turn_row(&app, &turn_d).await.status, "failed");
    let bot = db::bot(&app.db, &bot_d).await.unwrap().unwrap();
    let first = crate::runners::quota::limit_hit_for_bot(&app, &bot).await.expect("第一次記下");
    crate::runners::quota::clear_limit_hit_for_bot(&app, &bot).await;
    process(&app, &ev).await.unwrap();
    assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "重播不再記：{first:?}");
}

/// 分類：rate limit 跟其他錯誤要分得開，中斷排在最前面（寧可少收一次也不要把使用者按的停說成失敗）。
#[test]
fn the_failure_classifier_keeps_rate_limit_auth_and_interrupts_apart() {
    use FailureReason::*;
    assert_eq!(classify_failure(Some("API Error: 429 rate_limit_error")), RateLimit);
    assert_eq!(classify_failure(Some("You've hit your usage limit")), RateLimit);
    // claude 真正的撞額度橫幅沒有 rate／usage 字樣（issue #108）：以前被分成「API 錯誤」。
    assert_eq!(classify_failure(Some("You've hit your session limit · resets 5pm")), RateLimit);
    assert_eq!(classify_failure(Some("API Error: You've hit your weekly limit")), RateLimit);
    assert_eq!(classify_failure(Some("overloaded_error")), RateLimit);
    assert_eq!(classify_failure(Some("401 Unauthorized: invalid api key")), Auth);
    assert_eq!(classify_failure(Some("OAuth token expired; please login")), Auth);
    assert_eq!(classify_failure(Some("API Error: 500 Internal Server Error")), Api);
    assert_eq!(classify_failure(Some("[Request interrupted by user]")), Interrupted);
    assert_eq!(classify_failure(Some("cancelled")), Interrupted);
    assert_eq!(classify_failure(None), Unknown);
    assert_eq!(classify_failure(Some("   ")), Unknown);
}

/// `esc` 這個 needle 是用純字串 `contains` 比對的：`unescaped`／`description`／`prescribed`
/// 這種一般英文字都含著 `esc` 三個字母，不該被誤判成使用者按了 Esc——那樣一來真正的失敗（API／
/// 額度）會走進「中斷」那條回傳早退，回合永遠卡在 in_flight，額度也不會被記下來（issue #79／#108
/// 想防的正是這件事，「esc」子字串比對把它繞回去了）。
#[test]
fn a_generic_word_that_merely_contains_esc_is_not_a_user_interrupt() {
    use FailureReason::*;
    assert_eq!(
        classify_failure(Some("invalid_request_error: unescaped control character in string")),
        Api,
        "「unescaped」含 esc，但這是一個 API 驗證錯誤，不是使用者按了 Esc"
    );
    assert_eq!(
        classify_failure(Some("A description of the rate limit reached for this account")),
        RateLimit,
        "「description」含 esc，不該蓋掉後面真正的 rate limit 分類"
    );
    // 真的是 Esc 觸發的中斷（獨立一個字，前後被非英數字元包住）照樣要抓到。
    assert_eq!(classify_failure(Some("Cancelled (Esc)")), Interrupted);
    assert_eq!(classify_failure(Some("esc")), Interrupted);
}

/// payload 的欄位名還在動：撈得到就留原文，撈不到也照樣收回合（只是說不出原因）。
#[test]
fn the_failure_detail_is_read_from_whichever_field_carries_it() {
    assert_eq!(failure_detail(&json!({"reason": "rate_limit_error"})).as_deref(), Some("rate_limit_error"));
    assert_eq!(failure_detail(&json!({"error": {"type": "overloaded_error"}})).as_deref(), Some("overloaded_error"));
    assert_eq!(
        failure_detail(&json!({"error": {"message": "Internal server error", "type": "api_error"}})).as_deref(),
        Some("Internal server error"),
    );
    assert_eq!(failure_detail(&json!({"hook_event_name": "StopFailure", "session_id": "s"})), None);
    assert_eq!(failure_detail(&json!({"reason": "  "})), None, "空白不算原因");
}

/// 分類本身：`StopFailure` 走 `TurnFailed`，一般 `Stop` 不受影響。
#[test]
fn stop_failure_classifies_as_a_failed_turn_and_plain_stop_is_untouched() {
    let v = json!({"hook_event_name": "StopFailure", "session_id": "s1", "prompt_id": "p1", "reason": "429 rate_limit"});
    match classify("claude", &v) {
        HookKind::TurnFailed { session_id, turn_id, reason, detail, .. } => {
            assert_eq!(session_id.as_deref(), Some("s1"));
            assert_eq!(turn_id.as_deref(), Some("p1"));
            assert_eq!(reason, FailureReason::RateLimit);
            assert_eq!(detail.as_deref(), Some("429 rate_limit"));
        }
        other => panic!("expected TurnFailed, got {other:?}"),
    }
    let v = json!({"hook_event_name": "Stop", "session_id": "s1", "prompt_id": "p1"});
    assert!(matches!(classify("claude", &v), HookKind::TurnComplete { .. }), "一般 Stop 照舊");
}

/// issue #82：`SubagentStart`／`SubagentStop` 分類成 `SubagentEvent`，帶得到 `agent_id`／
/// `agent_type`；只有 `SubagentStop` 帶 `agent_transcript_path`（`SubagentStart` 的原生 payload
/// 本來就沒有這個欄位）。不認得的事件名字（例如 claude 才有的 `TeammateIdle`）照舊被 `Ignore`，
/// 不會意外冒出一種新的 Turn 副作用。
#[test]
fn subagent_start_and_stop_classify_as_a_pure_snapshot() {
    let start = json!({"hook_event_name": "SubagentStart", "session_id": "s1", "agent_id": "a1", "agent_type": "general-purpose"});
    match classify("claude", &start) {
        HookKind::SubagentEvent { event, agent_id, agent_type, transcript_path } => {
            assert_eq!(event, "start");
            assert_eq!(agent_id.as_deref(), Some("a1"));
            assert_eq!(agent_type.as_deref(), Some("general-purpose"));
            assert_eq!(transcript_path, None);
        }
        other => panic!("expected SubagentEvent, got {other:?}"),
    }
    let stop = json!({"hook_event_name": "SubagentStop", "agent_id": "a1", "agent_type": "general-purpose",
                       "agent_transcript_path": "/tmp/a1.jsonl"});
    match classify("claude", &stop) {
        HookKind::SubagentEvent { event, transcript_path, .. } => {
            assert_eq!(event, "stop");
            assert_eq!(transcript_path.as_deref(), Some("/tmp/a1.jsonl"));
        }
        other => panic!("expected SubagentEvent, got {other:?}"),
    }
    assert!(matches!(classify("claude", &json!({"hook_event_name": "TeammateIdle"})), HookKind::Ignore(_)));
}

/// issue #82：`process` 把 `SubagentStart`／`SubagentStop` 整筆寫進這顆 run 的 `subagent_json`，
/// 不建立、不動任何 Turn（跟 turn 完全脫鉤）；沒有活著的 run 就安靜丟掉，不報錯。
#[tokio::test]
async fn subagent_events_update_the_runs_snapshot_without_touching_any_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;

    process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "SubagentStart", "agent_id": "a1", "agent_type": "Explore"})))
        .await
        .unwrap();
    let snap: Option<String> = sqlx::query_scalar("SELECT subagent_json FROM runs WHERE bot_id = ?").bind(&bot_id).fetch_one(&app.db).await.unwrap();
    let snap: Value = serde_json::from_str(&snap.expect("snapshot written")).unwrap();
    assert_eq!(snap["event"], json!("start"));
    assert_eq!(snap["agent_id"], json!("a1"));
    assert_eq!(snap["agent_type"], json!("Explore"));
    assert_eq!(snap["transcript_path"], Value::Null);

    process(
        &app,
        &stop_failure(
            &bot_id,
            json!({"hook_event_name": "SubagentStop", "agent_id": "a1", "agent_type": "Explore", "agent_transcript_path": "/tmp/a1.jsonl"}),
        ),
    )
    .await
    .unwrap();
    let snap: Option<String> = sqlx::query_scalar("SELECT subagent_json FROM runs WHERE bot_id = ?").bind(&bot_id).fetch_one(&app.db).await.unwrap();
    let snap: Value = serde_json::from_str(&snap.unwrap()).unwrap();
    assert_eq!(snap["event"], json!("stop"), "後到的 stop 蓋掉 start，只留最新的快照");
    assert_eq!(snap["transcript_path"], json!("/tmp/a1.jsonl"));

    // 完全脫鉤：這顆 run 底下的 turn 沒有被碰過。
    let t = turn_row(&app, &turn_id).await;
    assert_eq!(t.status, "in_flight", "subagent 事件不該動到任何 turn");
}

/// issue #1770：`PreCompact` 分類成 `CompactStarting`，不認得的 `PostCompact` 照舊被 `Ignore`。
#[test]
fn pre_compact_classifies_as_a_pure_compacting_marker() {
    assert!(matches!(classify("claude", &json!({"hook_event_name": "PreCompact", "trigger": "auto"})), HookKind::CompactStarting));
    assert!(matches!(classify("claude", &json!({"hook_event_name": "PostCompact"})), HookKind::Ignore(_)));
}

/// issue #1770：`PreCompact` 把這顆 run 標成「壓縮中」，投影帶出 `compacting_since`；turn 不被碰。
/// 壓縮做完的 `SessionStart`（source=compact）收掉標記；回合失敗收尾（`StopFailure`）也收。
#[tokio::test]
async fn pre_compact_marks_the_run_until_the_compaction_session_or_the_turn_ends() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
    let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id = ?").bind(&bot_id).fetch_one(&app.db).await.unwrap();
    let marked = |now| am_base::compacting::json_at(&run_id, now) != Value::Null;

    process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "PreCompact", "trigger": "manual"}))).await.unwrap();
    assert!(marked(chrono::Utc::now()), "PreCompact 標成壓縮中");
    assert_eq!(turn_row(&app, &turn_id).await.status, "in_flight", "PreCompact 不收、不開 turn");

    process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "SessionStart", "source": "compact", "session_id": "s-cmp"}))).await.unwrap();
    assert!(!marked(chrono::Utc::now()), "壓縮做完的 SessionStart 收掉標記");

    process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "PreCompact", "trigger": "auto"}))).await.unwrap();
    assert!(marked(chrono::Utc::now()));
    process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "StopFailure", "session_id": "s-cmp", "prompt_id": turn_id, "reason": "429 rate_limit"}))).await.unwrap();
    assert!(!marked(chrono::Utc::now()), "回合失敗收尾也收掉標記");
}

/// 沒有活著的 run（例如 bot 剛好在重啟中間）：安靜丟掉，不報錯、也不憑空造一列。
#[tokio::test]
async fn a_subagent_event_with_no_active_run_is_a_quiet_no_op() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
    process(&env.app, &stop_failure(&bot.id, json!({"hook_event_name": "SubagentStart", "agent_id": "a1"}))).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ?").bind(&bot.id).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(n, 0, "沒有 run 就沒有地方寫，也不該無中生有");
}

/// issue #94：`PostToolUse` 的 Bash 輸出剛好是 `herdr agent start` 的 JSON 回應時分類成
/// `SpawnHint`；一般指令的輸出（不是 herdr 的 JSON 信封）照舊被 `Ignore`。
#[test]
fn post_tool_use_classifies_a_herdr_spawn_as_a_hint() {
    let v = json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
        "tool_response": {"stdout": r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2"}}}"#, "stderr": ""},
    });
    match classify("claude", &v) {
        HookKind::SpawnHint { pane_ids } => assert_eq!(pane_ids, ["w1:p2"]),
        other => panic!("expected SpawnHint, got {other:?}"),
    }

    let ls = json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "ls -la"},
        "tool_response": {"stdout": "total 0\n", "stderr": ""},
    });
    assert!(matches!(classify("claude", &ls), HookKind::Ignore(_)));
}

/// issue #94：`process` 把偵測到的 `pane_id` 記進 `spawn_hints`，指到送這則 hook 的那顆 bot；
/// 不需要活著的 run（這是這顆 bot 自己剛做的事，不是它的 Turn 的事），也不動任何 Turn。
#[tokio::test]
async fn a_spawn_hint_is_recorded_against_the_bot_that_sent_it() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;

    process(
        &env.app,
        &stop_failure(
            &bot.id,
            json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
                "tool_response": {"stdout": r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2"}}}"#, "stderr": ""},
            }),
        ),
    )
    .await
    .unwrap();

    let recorded: Option<String> =
        sqlx::query_scalar("SELECT bot_id FROM spawn_hints WHERE pane_id = 'w1:p2'").fetch_optional(&env.app.db).await.unwrap();
    assert_eq!(recorded.as_deref(), Some(bot.id.as_str()));
}

/// issue #1790：遠端 shim 開完子 agent 寫進 spool 的 `AmSpawnClaim`，`process` 記成「這顆 pane 是送這則的 bot 開的」；
/// 沒有 pane_id 的丟掉；parent run 僅作紀錄，不動 Turn。
#[tokio::test]
async fn a_spooled_spawn_claim_is_recorded_against_the_bot_that_wrote_it() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
    assert!(matches!(classify("claude", &json!({"hook_event_name": "AmSpawnClaim"})), HookKind::Ignore(_)), "沒有 pane_id 不收");

    let run = tt::fake_run(&env.app, &bot.id).await;
    process(&env.app, &stop_failure(&bot.id, json!({"hook_event_name": "AmSpawnClaim", "pane_id": "w1:p7", "parent_run_id": run}))).await.unwrap();

    let recorded: Option<String> =
        sqlx::query_scalar("SELECT bot_id FROM spawn_claims WHERE pane_id = 'w1:p7'").fetch_optional(&env.app.db).await.unwrap();
    assert_eq!(recorded.as_deref(), Some(bot.id.as_str()));
}

#[test]
fn nr_relay_legacy_name_only_spool_is_unknown() {
    assert!(matches!(classify("claude", &json!({"hook_event_name": RELAY_ANNOUNCE_EVENT,
        "to_agent": "old-name", "text": "不能以舊名猜測這句話的收件身份"})), HookKind::Ignore(_)));
}

#[tokio::test]
async fn nr_relay_spool_survives_rename_and_does_not_follow_the_reused_name() {
    let env = tt::env().await;
    let from = tt::claude_bot(&env.app, &env.project_id, "nr-relay-from").await;
    let target = tt::claude_bot(&env.app, &env.project_id, "nr-relay-target").await;
    let target_run = tt::fake_run(&env.app, &target.id).await;
    sqlx::query("UPDATE runs SET agent_name='renamed-target' WHERE id=?").bind(&target_run).execute(&env.app.db).await.unwrap();
    let other = tt::claude_bot(&env.app, &env.project_id, "nr-relay-reused").await;
    let other_run = tt::fake_run(&env.app, &other.id).await;
    let text = "延遲報備只歸穩定 run，舊名字被重用也不能改變收件者";
    let body = HookBody { bot_id: from.id.clone(), provider: "claude".into(), received_at: None,
        truncated: false, run_id: None, payload: json!({"hook_event_name":RELAY_ANNOUNCE_EVENT,
            "to_agent":"agent", "to_bot_id":target.id, "to_run_id":target_run, "delivery_id":"nr-stable-delivery", "text":text}) };
    process(&env.app, &body).await.unwrap();
    let other_run = db::run(&env.app.db, &other_run).await.unwrap().unwrap();
    assert_eq!(relay_source(&env.app.db, Some(&other_run), text).await, None);
    let target_run = db::run(&env.app.db, &target_run).await.unwrap().unwrap();
    assert_eq!(relay_source(&env.app.db, Some(&target_run), text).await.unwrap().from_bot, from.id);
    process(&env.app, &body).await.unwrap();
    assert_eq!(relay_source(&env.app.db, Some(&target_run), text).await, None, "同一 delivery 的重播不能再認領");
}

#[tokio::test]
async fn nr_claim_remote_spool_survives_parent_sleep_and_restart() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "original").await;
    let old_run = tt::fake_run(&env.app, &bot.id).await;
    sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&old_run).execute(&env.app.db).await.unwrap();
    process(&env.app, &stop_failure(&bot.id, json!({"hook_event_name":"AmSpawnClaim", "pane_id":"w1:p7", "parent_run_id":old_run}))).await.unwrap();
    assert_eq!(crate::spawn_hints::claims_for_host(&env.app,"local").await.unwrap().get("w1:p7"), Some(&bot.id));
    tt::fake_run(&env.app, &bot.id).await;
    sqlx::query("UPDATE bots SET name='renamed' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    let _reused = tt::claude_bot(&env.app, &env.project_id, "original").await;
    assert_eq!(crate::spawn_hints::claims_for_host(&env.app,"local").await.unwrap().get("w1:p7"), Some(&bot.id));
    // The real shim also puts the captured run ID in the outer hook envelope.
    let mut delayed = stop_failure(&bot.id, json!({"hook_event_name":"AmSpawnClaim", "pane_id":"w1:p-replayed", "parent_run_id":old_run}));
    delayed.run_id = Some(old_run.clone());
    process(&env.app, &delayed).await.unwrap();
    assert_eq!(crate::spawn_hints::claims_for_host(&env.app,"local").await.unwrap().get("w1:p-replayed"), Some(&bot.id));
    // Old spool writers may omit the audit run ID entirely.
    process(&env.app, &stop_failure(&bot.id, json!({"hook_event_name":"AmSpawnClaim", "pane_id":"w1:p8"}))).await.unwrap();
    assert_eq!(crate::spawn_hints::claims_for_host(&env.app,"local").await.unwrap().get("w1:p8"), Some(&bot.id));
}

/// 2026-09-30：遠端 bot 直送的一句，回音先被存成使用者訊息，報備之後才從寄件者的 spool 收到——補標 `relay_from`。
/// 同一句不標兩次；對不上的、別人的對話都不動。
#[tokio::test]
async fn a_late_relay_announce_attributes_the_echo_already_stored() {
    let env = tt::env().await;
    let from = tt::claude_bot(&env.app, &env.project_id, "rpa").await;
    let to = tt::claude_bot(&env.app, &env.project_id, "cicd").await;
    let run = tt::fake_run(&env.app, &to.id).await;
    sqlx::query("UPDATE runs SET agent_name = 'robins-hub-3b84sb' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
    let conv = db::conversation_id(&env.app.db, &to.id).await.unwrap();
    let text = "我是 robins-hub-bf3xq3。console PR #95 的 test／lint 卡在排隊";
    let turn = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES (?,?,?,'external','in_flight','ok',?)")
        .bind(&turn).bind(&conv).bind(&run).bind(db::now()).execute(&env.app.db).await.unwrap();
    let echo = lifecycle::insert_message(&env.app, &conv, Some(&turn), "user", text, "hook", false, None).await.unwrap();
    let web = lifecycle::insert_message(&env.app, &conv, None, "user", text, "web", false, None).await.unwrap();
    sqlx::query("UPDATE messages SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ','now','+1 second') WHERE id = ?")
        .bind(&web.id)
        .execute(&env.app.db)
        .await
        .unwrap();
    let announce = |t: &str| HookBody {
        bot_id: from.id.clone(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_bot_id": to.id, "to_run_id": run, "delivery_id": "delivery-robins-hub-3b84sb", "text": t}),
        received_at: None,
        truncated: false,
        run_id: None,
    };

    process(&env.app, &announce(text)).await.unwrap();
    let relay = |id: String| {
        let db = env.app.db.clone();
        async move { sqlx::query_scalar::<_, Option<String>>("SELECT relay_from FROM messages WHERE id = ?").bind(id).fetch_one(&db).await.unwrap() }
    };
    assert_eq!(relay(echo.id.clone()).await.as_deref(), Some(from.id.as_str()), "回音補標寄件者");
    assert_eq!(relay(web.id.clone()).await, None, "完全相同的 web 使用者訊息不動");
    assert_eq!(crate::agent_relay::claim(crate::config::LOCAL_HOST, &run, text), None, "補標後報備已用掉，不會再標一次");
}

/// 同一毫秒的兩則備援回覆：遲到的 hook 要蓋「最新」那則（寫入順序），不是 id 字典序大的那則。
#[tokio::test]
async fn a_late_hook_replaces_the_fallback_reply_written_last_when_two_share_a_millisecond() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "late-hook-tie").await;
    let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES ('t-late', ?, 'web','completed_fallback','ok','2026-10-01T00:00:00.000Z')")
        .bind(&conv)
        .execute(&env.app.db)
        .await
        .unwrap();
    for (id, content) in [("m-zzz-first", "備援一"), ("m-aaa-second", "備援二")] {
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?, 't-late', 'assistant', ?, 'terminal_fallback', '2026-10-01T00:00:01.000Z')")
            .bind(id)
            .bind(&conv)
            .bind(content)
            .execute(&env.app.db)
            .await
            .unwrap();
    }
    let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id='t-late'").fetch_one(&env.app.db).await.unwrap();
    let tx = env.app.db.begin().await.unwrap();
    replace_fallback_reply(&env.app, tx, &turn, "hook 原文", &None, &None, None).await.unwrap();
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, content FROM messages WHERE turn_id='t-late' ORDER BY rowid").fetch_all(&env.app.db).await.unwrap();
    assert_eq!(rows, vec![("m-zzz-first".to_string(), "備援一".to_string()), ("m-aaa-second".to_string(), "hook 原文".to_string())]);
}

/// 同一毫秒、寫在 `t` 後面的另一個回合，也算「`t` 之後有開過別的回合」：這則 hook 不能認領 `t`。
#[tokio::test]
async fn a_turn_opened_in_the_same_millisecond_after_this_one_counts_as_in_between() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "fired-tie").await;
    let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
    for id in ["t-zzz-first", "t-aaa-second"] {
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?, 'web','in_flight','ok','2026-10-01T00:00:00.000Z')")
            .bind(id)
            .bind(&conv)
            .execute(&env.app.db)
            .await
            .unwrap();
    }
    let first: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id='t-zzz-first'").fetch_one(&env.app.db).await.unwrap();
    let second: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id='t-aaa-second'").fetch_one(&env.app.db).await.unwrap();
    let fired = Some("2026-10-01T00:00:05Z");
    assert!(!fired_within(&env.app, &first, fired).await.unwrap(), "同毫秒、後寫的那個回合在它之後開");
    assert!(fired_within(&env.app, &second, fired).await.unwrap(), "最後一個回合之後沒有別的");
}

/// 稽核：報備表只用 agent 名字當 key。名字是 `<專案>-<bot>`，兩台主機各有同名 agent 並不稀奇；
/// 寄件者那台的報備不能被另一台同名 agent 的同一句回音認領（錯標＋用掉真收件方的那一筆）。
#[tokio::test]
async fn a_relay_announce_is_not_claimed_by_a_same_named_agent_on_another_host() {
    let env = tt::env().await;
    let from = tt::claude_bot(&env.app, &env.project_id, "relay-host-from").await;
    let local = tt::claude_bot(&env.app, &env.project_id, "relay-host-local").await;
    let local_run = tt::fake_run(&env.app, &local.id).await;
    let remote_project = db::ulid();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'remote', 'mac2', ?)")
        .bind(&remote_project)
        .bind(format!("{}/remote", env.dir.display()))
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
    let remote = tt::claude_bot(&env.app, &remote_project, "relay-host-remote").await;
    let remote_run = tt::fake_run(&env.app, &remote.id).await;
    for run in [&local_run, &remote_run] {
        sqlx::query("UPDATE runs SET agent_name = 'proj-same-name-host' WHERE id = ?").bind(run).execute(&env.app.db).await.unwrap();
    }
    let text = "請幫我看一下這個同名 agent 的跨主機報備是不是會被認錯";
    let announce = HookBody {
        bot_id: from.id.clone(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_bot_id": local.id, "to_run_id": local_run, "delivery_id": "delivery-proj-same-name-host", "text": text}),
        received_at: None,
        truncated: false,
        run_id: None,
    };
    process(&env.app, &announce).await.unwrap();

    let remote_run = db::run(&env.app.db, &remote_run).await.unwrap().unwrap();
    assert_eq!(relay_source(&env.app.db, Some(&remote_run), text).await, None, "另一台主機的同名 agent 不能認領");
    let local_run = db::run(&env.app.db, &local_run).await.unwrap().unwrap();
    assert_eq!(relay_source(&env.app.db, Some(&local_run), text).await.map(|r| r.from_bot).as_deref(), Some(from.id.as_str()), "真正的收件方照常認領");
}

/// #927：報備記下寄件 bot 送它當下正在跑的回合，收件方的回音認領時一起帶出來（child_done 靠它認「這一回合已經自己回報」）。
#[tokio::test]
async fn a_relay_announce_carries_the_sender_turn_into_the_echo_it_attributes() {
    let env = tt::env().await;
    let from = tt::claude_bot(&env.app, &env.project_id, "relay-turn-from").await;
    let local = tt::claude_bot(&env.app, &env.project_id, "relay-turn-local").await;
    let local_run = tt::fake_run(&env.app, &local.id).await;
    sqlx::query("UPDATE runs SET agent_name = 'proj-relay-turn' WHERE id = ?").bind(&local_run).execute(&env.app.db).await.unwrap();
    let sender_conv = db::conversation_id(&env.app.db, &from.id).await.unwrap();
    let sender_turn = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','in_flight','ok',?)")
        .bind(&sender_turn)
        .bind(&sender_conv)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
    let text = "請核准這一則，寄件的時候我還在跑這一回合";
    let announce = HookBody {
        bot_id: from.id.clone(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_bot_id": local.id, "to_run_id": local_run, "delivery_id": "delivery-proj-relay-turn", "text": text}),
        received_at: None,
        truncated: false,
        run_id: None,
    };
    process(&env.app, &announce).await.unwrap();

    let local_run = db::run(&env.app.db, &local_run).await.unwrap().unwrap();
    let relayed = relay_source(&env.app.db, Some(&local_run), text).await.expect("收件方的回音認得出來");
    assert_eq!(relayed.from_bot, from.id);
    assert_eq!(relayed.from_turn.as_deref(), Some(sender_turn.as_str()));
}

#[tokio::test]
async fn a_late_relay_announce_attributes_a_terminal_fallback_echo() {
    let env = tt::env().await;
    let from = tt::claude_bot(&env.app, &env.project_id, "rpa-terminal").await;
    let to = tt::claude_bot(&env.app, &env.project_id, "cicd-terminal").await;
    let run = tt::fake_run(&env.app, &to.id).await;
    sqlx::query("UPDATE runs SET agent_name = 'robins-hub-terminal-734' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
    let conv = db::conversation_id(&env.app.db, &to.id).await.unwrap();
    let text = "請幫我檢查 PR #95，終端截取的 prompt echo";
    let turn = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES (?,?,?,'external','in_flight','ok',?)")
        .bind(&turn).bind(&conv).bind(&run).bind(db::now()).execute(&env.app.db).await.unwrap();
    let echo = lifecycle::insert_message(&env.app, &conv, Some(&turn), "user", text, "terminal_fallback", false, None).await.unwrap();
    let announce = HookBody {
        bot_id: from.id.clone(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_bot_id": to.id, "to_run_id": run, "delivery_id": "delivery-robins-hub-terminal-734", "text": text}),
        received_at: None,
        truncated: false,
        run_id: None,
    };

    process(&env.app, &announce).await.unwrap();

    let relay: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE id = ?")
        .bind(&echo.id)
        .fetch_one(&env.app.db)
        .await
        .unwrap();
    assert_eq!(relay.as_deref(), Some(from.id.as_str()), "terminal_fallback echo 仍可由 late announce 補標");
}

#[tokio::test]
async fn a_late_relay_announce_does_not_attribute_a_matching_web_prefix() {
    let env = tt::env().await;
    let from = tt::claude_bot(&env.app, &env.project_id, "rpa-prefix").await;
    let to = tt::claude_bot(&env.app, &env.project_id, "cicd-prefix").await;
    let run = tt::fake_run(&env.app, &to.id).await;
    sqlx::query("UPDATE runs SET agent_name = 'robins-hub-prefix-734' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
    let conv = db::conversation_id(&env.app.db, &to.id).await.unwrap();
    let text = "請幫我檢查 PR #95，先跑 test 再回報，附上失敗摘要";
    let web_prefix = "請幫我檢查 PR #95，先跑 test";
    let web = lifecycle::insert_message(&env.app, &conv, None, "user", web_prefix, "web", false, None).await.unwrap();
    sqlx::query("UPDATE messages SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ','now','+1 second') WHERE id = ?")
        .bind(&web.id)
        .execute(&env.app.db)
        .await
        .unwrap();
    let announce = HookBody {
        bot_id: from.id,
        provider: "claude".into(),
        payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_bot_id": to.id, "to_run_id": run, "delivery_id": "delivery-robins-hub-prefix-734", "text": text}),
        received_at: None,
        truncated: false,
        run_id: None,
    };

    process(&env.app, &announce).await.unwrap();

    let relay: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE id = ?")
        .bind(&web.id)
        .fetch_one(&env.app.db)
        .await
        .unwrap();
    assert_eq!(relay, None, ">=12 字元的相符前綴也不能把 web 訊息標成 relay");
}

/// 2026-10-01 cf-ox-2：claude 自己接著做的那一輪（背景 shell 跑完），hook 回報的「使用者訊息」是上一則已經回答過的 prompt，
/// 不能再存一次；真的是新的一句照存。
#[tokio::test]
async fn an_external_turn_does_not_restore_the_prompt_that_was_already_answered() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "cfox").await;
    let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
    let answered = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','in_flight','ok',?)")
        .bind(&answered).bind(&conv).bind(db::now()).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE turns SET status='completed' WHERE id=?").bind(&answered).execute(&env.app.db).await.unwrap();
    lifecycle::insert_message(&env.app, &conv, Some(&answered), "user", "ui 審查你自己做", "web", false, None).await.unwrap();
    let external = db::ulid();
    let mut conn = env.app.db.acquire().await.unwrap();
    assert!(repeats_answered_prompt(&mut conn, &conv, &external, "ui 審查你自己做").await.unwrap(), "同一句、那一回合已收掉：是舊的");
    assert!(!repeats_answered_prompt(&mut conn, &conv, &external, "另一句新的話").await.unwrap(), "新的一句照存");
}

/// issue #754 的場景：使用者送過「現在部 demo」（已答完），之後 claude 又開了一輪沒有新 prompt 的外部回合
/// （`begin_external_turn` 因為回音跟上一句一樣沒存）。回傳 (bot, conv, 外部回合)。
async fn an_external_turn_after_an_answered_prompt(app: &Arc<App>, project_id: &str, fallback_closed: bool) -> (String, String, String) {
    let (bot_id, conv, answered) = unknown_turn(app, project_id, "claude", "現在部 demo").await;
    lifecycle::insert_message(app, &conv, Some(&answered), "user", "現在部 demo", "web", false, None).await.unwrap();
    sqlx::query("UPDATE turns SET status='completed', delivery='ok', completed_at=? WHERE id=?")
        .bind(db::now()).bind(&answered).execute(&app.db).await.unwrap();
    let run_id: String = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&answered).fetch_one(&app.db).await.unwrap();
    let external = db::ulid();
    let status = if fallback_closed { "completed_fallback" } else { "in_flight" };
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'external',?,'ok',?)",
    )
    .bind(&external).bind(&conv).bind(&run_id).bind(status).bind(db::now()).execute(&app.db).await.unwrap();
    if fallback_closed {
        sqlx::query("UPDATE turns SET completed_at=? WHERE id=?").bind(db::now()).bind(&external).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'assistant','⏺ 畫面備援','terminal_fallback',?)")
            .bind(db::ulid()).bind(&conv).bind(&external).bind(db::now()).execute(&app.db).await.unwrap();
    }
    (bot_id, conv, external)
}

fn claude_stop_for(bot_id: &str, extra: Value) -> HookBody {
    let mut payload = json!({
        "hook_event_name": "Stop",
        "session_id": format!("s-{}", db::ulid()),
        "prompt_id": db::ulid(),
        "transcript_path": "/Users/me/.claude/projects/x/s.jsonl",
        "last_assistant_message": "背景工作做完了",
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        payload[k] = v;
    }
    HookBody { bot_id: bot_id.to_string(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }
}

async fn user_texts_on(app: &Arc<App>, turn_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='user' ORDER BY created_at")
        .bind(turn_id).fetch_all(&app.db).await.unwrap()
}

/// issue #754：背景工作完成喚醒的外部回合不能掛上一句使用者訊息；但使用者真的在 pane 裡重送同一句時要照記。
/// 遠端 transcript 在 agm-host 讀不到，起點由遠端 hook.sh 從本機 transcript 讀出、跟 Stop 一起帶來（`agm_origin_kind`）：
/// `human` 才補記（`begin_external_turn` 開回合時為了擋喚醒那一輪而沒存的回音），其他起點或沒證據都不存。
#[tokio::test]
async fn a_resent_prompt_is_stored_but_a_background_wake_up_is_not() {
    let env = tt::env().await;
    let app = env.app.clone();
    for (origin, expect_stored) in [(Some("human"), true), (Some("task-notification"), false), (None, false)] {
        let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, false).await;
        let mut extra = json!({"agm_user_text": "現在部 demo"});
        if let Some(k) = origin {
            extra["agm_origin_kind"] = json!(k);
        }
        process(&app, &claude_stop_for(&bot_id, extra)).await.unwrap();
        let stored = user_texts_on(&app, &external).await;
        assert_eq!(stored, if expect_stored { vec!["現在部 demo".to_string()] } else { vec![] }, "origin={origin:?}");
        assert_eq!(turn_row(&app, &external).await.status, "completed", "origin={origin:?}");
    }
}

#[tokio::test]
async fn stop_and_late_stop_need_only_one_available_pool_connection() {
    let env = tt::env().await;
    let app = env.app.clone();
    for fallback_closed in [false, true] {
        let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, fallback_closed).await;
        let hook = claude_stop_for(&bot_id, json!({"agm_user_text": "現在部 demo", "agm_origin_kind": "human"}));
        let mut held = Vec::new();
        for _ in 1..app.db.options().get_max_connections() {
            held.push(app.db.acquire().await.unwrap());
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), process(&app, &hook)).await
            .expect("hook transaction must not acquire a second pool connection").unwrap();
        drop(held);
        assert_eq!(user_texts_on(&app, &external).await, vec!["現在部 demo".to_string()]);
        assert_eq!(turn_row(&app, &external).await.status, "completed");
    }
}

/// 同一題，Stop 比終端備援晚到（#753 的競態）：備援已把外部回合收掉，使用者那一句一樣要補上。
#[tokio::test]
async fn a_late_stop_after_the_fallback_still_stores_a_resent_prompt() {
    let env = tt::env().await;
    let app = env.app.clone();
    for (origin, expect_stored) in [("human", true), ("task-notification", false)] {
        let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, true).await;
        let extra = json!({"agm_user_text": "現在部 demo", "agm_origin_kind": origin});
        process(&app, &claude_stop_for(&bot_id, extra)).await.unwrap();
        let stored = user_texts_on(&app, &external).await;
        assert_eq!(stored, if expect_stored { vec!["現在部 demo".to_string()] } else { vec![] }, "origin={origin}");
    }
}

/// #831：Stop 比備援晚到、補記使用者那一句時，數完回合上的使用者訊息、還沒寫的那一瞬，一個不相干的 writer commit 了一筆。
/// deferred 交易這時升級寫鎖直接 517，那一句補不上、整個 hook 回錯；寫鎖從讀之前就拿著，照樣補上一次。
#[tokio::test]
async fn an_unrelated_writer_between_the_user_count_and_the_insert_does_not_lose_the_resent_prompt() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, true).await;
    let other = tt::arm_app_foreign_writer(&app, "resent_prompt_after_count_read", &external);

    let extra = json!({"agm_user_text": "現在部 demo", "agm_origin_kind": "human"});
    process(&app, &claude_stop_for(&bot_id, extra)).await.expect("an unrelated writer must not make the Stop fail");

    assert_eq!(*other.lock().unwrap(), Some(false), "the insert holds the write lock from its read on; the other writer waits");
    assert_eq!(user_texts_on(&app, &external).await, vec!["現在部 demo".to_string()]);
}

/// hook 送來的 transcript 路徑要在 bot 自己身份的 `projects/` 底下才算數（`transcript_read`）：把這顆 bot 的 `CLAUDE_CONFIG_DIR` 指到 `root`，
/// 回傳 `root/projects/-t/<name>`（目錄已建好）。
async fn own_projects_file(app: &Arc<App>, bot_id: &str, root: &std::path::Path, name: &str) -> std::path::PathBuf {
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
        .bind(json!({"CLAUDE_CONFIG_DIR": root.to_string_lossy()}).to_string())
        .bind(bot_id)
        .execute(&app.db)
        .await
        .unwrap();
    let dir = root.join("projects/-t");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// #1759：本機 claude 閒置壓縮做完的 SessionStart(compact)：讀 transcript 寫一則壓縮說明、壓縮開出來的空 external 回合
/// 收成 completed（沒有回覆泡泡）；收件匣重播不寫第二則；遠端帶來的 `agm_compact` 一樣認得。
#[tokio::test]
async fn a_compact_session_start_notes_the_compaction_and_closes_the_empty_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cmp").await;
    let run_id = tt::fake_run(&app, &bot.id).await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'external','in_flight','ok',?)")
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let path = own_projects_file(&app, &bot.id, &env.dir, "cmp.jsonl").await;
    let lines = [
        json!({"type": "system", "subtype": "compact_boundary", "compactMetadata": {"trigger": "auto", "preTokens": 286047, "postTokens": 11362}}),
        json!({"type": "system", "subtype": "informational", "content": "Compacted while idle, before the prompt cache expired"}),
    ];
    std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    let start = |extra: Value| {
        let mut payload = json!({"hook_event_name": "SessionStart", "source": "compact", "session_id": "s-cmp", "transcript_path": path.to_string_lossy()});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            payload[k] = v;
        }
        HookBody { bot_id: bot.id.clone(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }
    };
    process(&app, &start(json!({}))).await.unwrap();
    process(&app, &start(json!({}))).await.unwrap();

    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "completed");
    let msgs: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT role, content, turn_id FROM messages WHERE conversation_id=? ORDER BY created_at, rowid")
            .bind(&conv)
            .fetch_all(&app.db)
            .await
            .unwrap();
    assert_eq!(msgs, vec![("system".into(), "Claude 閒置壓縮：286K tokens → 摘要 11K tokens".into(), Some(turn_id.clone()))]);

    // 遠端：transcript 讀不到，用 hook.sh 帶來的 `agm_compact`（兩分鐘內已有說明就不寫，先把舊的推到更早）。
    sqlx::query("UPDATE messages SET created_at='2026-01-01T00:00:00.000Z' WHERE conversation_id=?").bind(&conv).execute(&app.db).await.unwrap();
    process(&app, &start(json!({"transcript_path": "/Users/me/.claude/projects/x/s.jsonl", "agm_compact": {"trigger": "manual", "idle": false, "pre_tokens": 900}})))
        .await
        .unwrap();
    let last: String = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? ORDER BY created_at DESC, rowid DESC LIMIT 1")
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(last, "手動 /compact：900 tokens → 摘要");
}

/// #1762：使用者在終端打 `/compact`（UserPromptSubmit 記成一則 user 訊息、沒有 Stop）。遠端 hook.sh 舊版沒帶 `agm_compact`：
/// 照樣認出是手動 `/compact`、把那個回合收掉（終端備援不會再把「Compacted / Referenced file …」存成回覆）。
#[tokio::test]
async fn a_typed_compact_command_turn_is_closed_and_noted_as_manual() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cmp-manual").await;
    let run_id = tt::fake_run(&app, &bot.id).await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'external','in_flight','ok',?)")
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    lifecycle::insert_message(&app, &conv, Some(&turn_id), "user", "/compact", "hook", false, None).await.unwrap();
    let payload = json!({"hook_event_name": "SessionStart", "source": "compact", "session_id": "s-m", "transcript_path": "/Users/me/.claude/projects/x/s.jsonl"});
    process(&app, &HookBody { bot_id: bot.id.clone(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }).await.unwrap();

    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "completed");
    let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&turn_id).fetch_all(&app.db).await.unwrap();
    assert_eq!(notes, vec!["手動 /compact。".to_string()]);

    // 使用者回合中間的自動壓縮（user 訊息不是 /compact）：回合不動。
    let busy = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
        .bind(&busy)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    lifecycle::insert_message(&app, &conv, Some(&busy), "user", "修 bug", "web", false, None).await.unwrap();
    let payload = json!({"hook_event_name": "SessionStart", "source": "compact", "session_id": "s-m", "transcript_path": "/Users/me/.claude/projects/x/s.jsonl"});
    process(&app, &HookBody { bot_id: bot.id.clone(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&busy).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "in_flight");
}

/// 本機 bot：transcript 在這台，daemon 自己讀 `origin.kind`，不需要 hook.sh 帶。
#[tokio::test]
async fn a_local_transcript_decides_whether_the_stop_stores_a_resent_prompt() {
    let env = tt::env().await;
    let app = env.app.clone();
    let line = |origin: Option<&str>, text: &str| {
        let mut v = json!({"type": "user", "message": {"role": "user", "content": text}});
        if let Some(k) = origin {
            v["origin"] = json!({"kind": k});
        }
        format!("{v}\n")
    };
    for (origin, expect_stored) in [("human", true), ("task-notification", false)] {
        let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, false).await;
        let path = own_projects_file(&app, &bot_id, &env.dir, &format!("t-{origin}.jsonl")).await;
        std::fs::write(&path, [line(Some("human"), "現在部 demo"), line(Some(origin), if origin == "human" { "現在部 demo" } else { "<task-notification/>" })].concat()).unwrap();
        process(&app, &claude_stop_for(&bot_id, json!({"transcript_path": path.to_string_lossy()}))).await.unwrap();
        let stored = user_texts_on(&app, &external).await;
        if expect_stored {
            assert_eq!(stored, vec!["現在部 demo".to_string()], "origin={origin}");
        } else {
            assert!(stored.is_empty(), "喚醒那一輪不掛上一句：{stored:?}");
        }
    }
}

/// issue #634: a single Bash `PostToolUse` hook records every herdr response against its caller.
#[tokio::test]
async fn a_bash_loop_spawn_records_every_hint() {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
    let stdout = format!(
        "{}\n{}\n{}\n",
        json!({"id": "cli:agent:start", "result": {"agent": {"pane_id": "w1:p2"}}}),
        json!({"id": "cli:pane:split", "result": {"pane": {"pane_id": "w1:p3"}}}),
        json!({"id": "cli:agent:start", "result": {"agent": {"pane_id": "w1:p4"}}}),
    );

    process(
        &env.app,
        &stop_failure(
            &bot.id,
            json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "for n in a b c; do herdr agent start $n --kind claude; done"},
                "tool_response": {"stdout": stdout, "stderr": ""},
            }),
        ),
    )
    .await
    .unwrap();

    let recorded: Vec<String> = sqlx::query_scalar("SELECT pane_id FROM spawn_hints WHERE bot_id = ? ORDER BY pane_id")
        .bind(&bot.id)
        .fetch_all(&env.app.db)
        .await
        .unwrap();
    assert_eq!(recorded, ["w1:p2", "w1:p3", "w1:p4"]);
}

fn ask_messages_sql() -> &'static str {
    "SELECT content FROM messages WHERE id LIKE 'ask:%' ORDER BY created_at, rowid"
}

/// 2026-10-02 wits-pro：問了三題、答完，網頁看不到。回合結束時從 transcript 補進對話；同一筆 Stop 重送（hook 重送、spool replay）
/// 與 PostToolUse 同一個 `tool_use_id` 之後到，都不能多一則。
#[tokio::test]
async fn a_finished_ask_user_question_lands_in_the_conversation_exactly_once() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "做 WA 流程").await;
    let at = |secs: i64| (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let qs = json!([
        {"question": "拋單怎麼處理？", "header": "拋單倉庫", "options": [{"label": "保留拋單"}, {"label": "拿掉拋單"}]},
        {"question": "go API 放哪？", "header": "go API", "options": [{"label": "stock-server"}]},
    ]);
    let ask = |id: &str| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion", "input": {"questions": qs}}]}});
    let result = |id: &str, secs: i64, body: Value| {
        let mut v = json!({"type": "user", "timestamp": at(secs), "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "…"}]}});
        v["toolUseResult"] = body;
        v
    };
    let path = own_projects_file(&app, &bot_id, &env.dir, "asks.jsonl").await;
    let lines = [
        // 這一回合開始之前就答完的（fork／resume 帶來的舊提問）：不記。
        ask("old"),
        result("old", -3600, json!({"questions": qs, "answers": {"拋單怎麼處理？": "舊的"}})),
        ask("t1"),
        result("t1", 1, json!({"questions": qs, "answers": {"拋單怎麼處理？": "拿掉拋單", "go API 放哪？": "自訂：gateway"}})),
        ask("t2"),
        // 取消（Cancel／Esc）：content 是固定那句。
        json!({"type": "user", "timestamp": at(2), "message": {"content": [{"type": "tool_result", "tool_use_id": "t2", "content": "The user did not answer the questions."}]}, "toolUseResult": {"questions": qs}}),
    ];
    std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();

    let stop = claude_stop_for(&bot_id, json!({"transcript_path": path.to_string_lossy()}));
    process(&app, &stop).await.unwrap();
    process(&app, &stop).await.unwrap(); // 重送
    let rows: Vec<String> = sqlx::query_scalar(ask_messages_sql()).fetch_all(&app.db).await.unwrap();
    assert_eq!(rows.len(), 2, "t1 與 t2 各一則，舊的不補：{rows:?}");
    let first: Value = serde_json::from_str(&rows[0]).unwrap();
    assert_eq!((first["type"].as_str(), first["tool_use_id"].as_str(), first["answered"].as_bool()), (Some("ask_answers"), Some("t1"), Some(true)));
    assert_eq!(first["items"][0], json!({"header": "拋單倉庫", "question": "拋單怎麼處理？", "answer": "拿掉拋單"}));
    assert_eq!(first["items"][1]["answer"], "自訂：gateway");
    let second: Value = serde_json::from_str(&rows[1]).unwrap();
    assert_eq!(second["answered"], json!(false), "取消＝沒有回答");
    assert!(second["items"].as_array().unwrap().iter().all(|i| i["answer"].is_null()));
    // 掛在這一回合、是系統訊息，不是使用者打的字，也沒有新開任何回合。
    let (role, source, turn): (String, String, Option<String>) =
        sqlx::query_as("SELECT role, source, turn_id FROM messages WHERE id LIKE 'ask:%' ORDER BY created_at LIMIT 1").fetch_one(&app.db).await.unwrap();
    assert_eq!((role.as_str(), source.as_str(), turn.as_deref()), ("system", "system", Some(turn_id.as_str())));
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns WHERE conversation_id = ?").bind(&_conv).fetch_one(&app.db).await.unwrap(), 1);

    // 同一個 tool_use_id 的 PostToolUse 晚到：一樣只有一列。
    let post = stop_failure(&bot_id, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
        "tool_input": {"questions": qs}, "tool_response": {"questions": qs, "answers": {"拋單怎麼處理？": "拿掉拋單"}}}));
    process(&app, &post).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE id LIKE 'ask:%'").fetch_one(&app.db).await.unwrap(), 2);
}

/// 使用者在終端打字開的回合（外部回合）要等 Stop 才建立：問答要掛在那一個新回合上，不是前一個已經收掉的。
#[tokio::test]
async fn an_ask_answered_in_an_external_turn_belongs_to_that_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, old_turn) = unknown_turn(&app, &env.project_id, "claude", "舊的一句").await;
    let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE turns SET status='completed', delivery='ok', created_at=?, completed_at=? WHERE id=?")
        .bind(&long_ago).bind(&long_ago).bind(&old_turn).execute(&app.db).await.unwrap();
    let qs = json!([{"question": "要不要？", "header": "確認", "options": []}]);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let path = own_projects_file(&app, &bot_id, &env.dir, "ext.jsonl").await;
    std::fs::write(&path, format!("{}\n{}\n",
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "AskUserQuestion", "input": {"questions": qs}}]}}),
        json!({"type": "user", "timestamp": now, "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "x"}]}, "toolUseResult": {"questions": qs, "answers": {"要不要？": "要"}}}),
    )).unwrap();
    process(&app, &claude_stop_for(&bot_id, json!({"transcript_path": path.to_string_lossy()}))).await.unwrap();
    let on: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id LIKE 'ask:%'").fetch_optional(&app.db).await.unwrap();
    let newest: String = sqlx::query_scalar("SELECT id FROM turns WHERE conversation_id=? ORDER BY created_at DESC, rowid DESC LIMIT 1").bind(&conv).fetch_one(&app.db).await.unwrap();
    assert_ne!(newest, old_turn, "Stop 開了一個外部回合");
    assert_eq!(on.as_deref(), Some(newest.as_str()), "問答掛在新的外部回合上");
}

#[tokio::test]
async fn post_tool_use_does_not_pin_an_external_question_to_the_previous_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, old_turn) = unknown_turn(&app, &env.project_id, "claude", "舊的一句").await;
    let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE turns SET status='completed', delivery='ok', created_at=?, completed_at=? WHERE id=?")
        .bind(&long_ago)
        .bind(&long_ago)
        .bind(&old_turn)
        .execute(&app.db)
        .await
        .unwrap();

    let qs = json!([{"question": "要不要？", "header": "確認", "options": []}]);
    let post = stop_failure(&bot_id, json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "AskUserQuestion",
        "tool_use_id": "external-tool-use",
        "tool_input": {"questions": qs},
        "tool_response": {"answers": {"要不要？": "要"}}
    }));
    process(&app, &post).await.unwrap();
    let early: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id LIKE 'ask:%'")
        .fetch_optional(&app.db)
        .await
        .unwrap();
    assert!(early.is_none(), "沒有當前 in-flight turn 時先不要掛到舊回合：{early:?}");

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let path = own_projects_file(&app, &bot_id, &env.dir, "external-ask.jsonl").await;
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "external-tool-use", "name": "AskUserQuestion", "input": {"questions": qs}}]}}),
            json!({"type": "user", "timestamp": now, "message": {"content": [{"type": "tool_result", "tool_use_id": "external-tool-use", "content": "answered"}]}, "toolUseResult": {"questions": qs, "answers": {"要不要？": "要"}}}),
        ),
    )
    .unwrap();
    process(&app, &claude_stop_for(&bot_id, json!({"transcript_path": path.to_string_lossy()})))
        .await
        .unwrap();

    let on: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id LIKE 'ask:%'")
        .fetch_optional(&app.db)
        .await
        .unwrap();
    let newest: String = sqlx::query_scalar("SELECT id FROM turns WHERE conversation_id=? ORDER BY created_at DESC, rowid DESC LIMIT 1")
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_ne!(newest, old_turn, "Stop 建立了外部回合");
    assert_eq!(on.as_deref(), Some(newest.as_str()), "PostToolUse 與 Stop 冪等合併時要掛在外部回合");
}

/// 被使用者中斷的回合沒有 Stop：PostToolUse 當下就記；遠端 bot 的 Stop 帶 `agm_asks`（`hook.sh` 從本機 transcript 讀的），
/// 讀不到檔案也照記。認不出答案的 PostToolUse 不記（不能先記一筆「沒有回答」把正確的擋掉）。
#[tokio::test]
async fn post_tool_use_and_the_carried_asks_record_without_reading_a_transcript() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, _turn) = unknown_turn(&app, &env.project_id, "claude", "做 WA 流程").await;
    let qs = json!([{"question": "拋單怎麼處理？", "header": "拋單倉庫", "options": []}]);

    let unreadable = stop_failure(&bot_id, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "t0",
        "tool_input": {"questions": qs}, "tool_response": "something claude changed"}));
    process(&app, &unreadable).await.unwrap();
    assert!(sqlx::query_scalar::<_, String>(ask_messages_sql()).fetch_all(&app.db).await.unwrap().is_empty());

    let live = stop_failure(&bot_id, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
        "tool_input": {"questions": qs}, "tool_response": {"answers": {"拋單怎麼處理？": "拿掉拋單"}}}));
    process(&app, &live).await.unwrap();
    process(&app, &live).await.unwrap();
    let rows: Vec<String> = sqlx::query_scalar(ask_messages_sql()).fetch_all(&app.db).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(serde_json::from_str::<Value>(&rows[0]).unwrap()["items"][0]["answer"], "拿掉拋單");

    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let carried = claude_stop_for(&bot_id, json!({"transcript_path": "/Users/me/none.jsonl", "agm_asks": [
        {"id": "t1", "at": at, "items": [{"header": "拋單倉庫", "question": "拋單怎麼處理？", "answer": "拿掉拋單"}]},
        {"id": "t9", "at": at, "items": [{"question": "要不要？", "answer": null}]},
    ]}));
    process(&app, &carried).await.unwrap();
    let rows: Vec<String> = sqlx::query_scalar(ask_messages_sql()).fetch_all(&app.db).await.unwrap();
    assert_eq!(rows.len(), 2, "t1 已記過不重複，t9 是新的：{rows:?}");
}

/// claude 的 Stop 沒帶使用者訊息：從 transcript 尾巴讀。對不上一樣不認領；讀不到（沒有 transcript）就照舊認領。
#[tokio::test]
async fn claude_reads_the_prompt_from_the_transcript_before_claiming_an_unknown_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
    let transcript = own_projects_file(&app, &bot_id, &app.data_dir, &format!("t-{}.jsonl", db::ulid())).await;
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type": "user", "message": {"role": "user", "content": "跑一次測試"}}),
            json!({"type": "user", "message": {"role": "user", "content": "算了，先幫我看 README"}}),
        ),
    )
    .unwrap();
    let stop = |sid: &str| HookBody {
        bot_id: bot_id.clone(),
        provider: "claude".into(),
        payload: json!({
            "hook_event_name": "Stop",
            "session_id": sid,
            "prompt_id": db::ulid(),
            "transcript_path": transcript.to_string_lossy(),
            "last_assistant_message": "hook reply",
        }),
        received_at: None,
        truncated: false,
        run_id: None,
    };
    process(&app, &stop("s1")).await.unwrap();
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(turn.status, "in_flight", "transcript 最後一則是別句：不認領");

    assert!(!answers_another_prompt(Some("跑一次測試"), None), "讀不到就不下判斷");
    assert!(!answers_another_prompt(None, Some("x")));
    assert!(answers_another_prompt(Some("跑一次測試"), Some("算了")));
}

/// 使用者 2026-09-28：回合中「補充」的那句被 claude 記成 transcript 最後一則使用者訊息。記下的補充要算成這一回合的 prompt：
/// 照常認領（unknown 升 ok）、不開外部回合、也不多一則重複的使用者訊息。
#[tokio::test]
async fn a_supplement_typed_mid_turn_does_not_steal_the_turns_reply() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
    sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user','跑一次測試','web',?)")
        .bind(db::ulid())
        .bind(&conv)
        .bind(&turn_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    env.herdr.live_pane("pane-1", tt::LivePane::default());
    let supp = crate::lifecycle::send_text_recorded(&app, &bot_id, "順便把 log 也貼上來", true, None, true)
        .await
        .unwrap()
        .expect("記成這一回合的訊息");
    assert_eq!(supp.turn_id.as_deref(), Some(turn_id.as_str()));

    let transcript = own_projects_file(&app, &bot_id, &app.data_dir, &format!("t-{}.jsonl", db::ulid())).await;
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type": "user", "message": {"role": "user", "content": "跑一次測試"}}),
            json!({"type": "user", "message": {"role": "user", "content": "順便把 log 也貼上來"}}),
        ),
    )
    .unwrap();
    let stop = HookBody {
        bot_id: bot_id.clone(),
        provider: "claude".into(),
        payload: json!({
            "hook_event_name": "Stop",
            "session_id": "s-supp",
            "prompt_id": db::ulid(),
            "transcript_path": transcript.to_string_lossy(),
            "last_assistant_message": "測試過了，log 在這",
        }),
        received_at: None,
        truncated: false,
        run_id: None,
    };
    process(&app, &stop).await.unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("completed", "ok"), "補充之後照常認領");
    let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND origin='external'")
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(external, 0, "不開外部回合");
    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT role, content, turn_id FROM messages WHERE conversation_id=? ORDER BY created_at, rowid")
            .bind(&conv)
            .fetch_all(&app.db)
            .await
            .unwrap();
    let t = Some(turn_id.clone());
    assert_eq!(
        rows,
        vec![
            ("user".to_string(), "跑一次測試".to_string(), t.clone()),
            ("user".to_string(), "順便把 log 也貼上來".to_string(), t.clone()),
            ("assistant".to_string(), "測試過了，log 在這".to_string(), t),
        ],
        "兩則使用者訊息各一則、回覆掛在原回合"
    );
}

/// 沒記下的一句（使用者在終端手打）照舊分得出來：補充只放寬「這一回合記過的字」。
#[tokio::test]
async fn only_recorded_supplements_count_as_the_turns_prompt() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (_bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
    let sent = with_supplements(&app, &turn_id, Some("跑一次測試")).await.unwrap();
    assert!(answers_none_of(sent.as_deref(), Some("順便把 log 也貼上來")));
    assert!(!answers_none_of(sent.as_deref(), Some("跑一次測試")));
    assert!(!answers_none_of(None, Some("x")), "沒有 prompt 不下判斷");
    assert!(!answers_none_of(sent.as_deref(), None), "讀不到使用者訊息不下判斷");
}

/// 2026-09-27 wits-pro：貼上的 prompt 帶 Tab，送進 TUI 時 Tab 被吃掉；只在原文有 Tab 時試去 Tab 候選。
#[test]
fn a_prompt_whose_tab_the_tui_swallowed_is_still_the_same_prompt() {
    let sent = "條碼列印・進貨單\t每張 INBSHIP 一個批號 => 這樣一批最多能夠幾個item";
    let seen = "條碼列印・進貨單每張 INBSHIP 一個批號 => 這樣一批最多能夠幾個item";
    assert!(!answers_another_prompt(Some(sent), Some(seen)));
    assert!(!hook_user_is_new(&[sent.to_string()], seen), "同一句不再多一則使用者訊息");
    assert!(answers_another_prompt(Some(sent), Some("另一件事")), "真的是另一句照樣分得出來");
}

/// #217：`unknown` 那一則在 transcript 裡被 CLI 包成 `<pasted_content>`（真 transcript，2026-09-19 實測）還是同一則，照樣認領。
/// prompt 本身寫著字面的 `<pasted_content …>` 時 CLI 還把它跳脫成 `<\…`，兩邊去空白後互不包含——以前被當成「回答的是別句」：
/// 答案記到一筆外部回合、那一則一直掛在 unknown。
#[tokio::test]
async fn an_unknown_turn_whose_prompt_the_cli_wrapped_as_pasted_content_is_still_claimed() {
    let env = tt::env().await;
    let app = env.app.clone();
    let prompt = "這段文字裡有字面的 <pasted_content id=\"1234\">x</pasted_content id=\"1234\"> 標籤，請只回覆 OK 兩個字母。";
    let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", prompt).await;
    let transcript = own_projects_file(&app, &bot_id, &app.data_dir, &format!("t-{}.jsonl", db::ulid())).await;
    let log: Vec<&str> = include_str!("../lifecycle/fixtures/claude_2.1.278_pasted_content.jsonl").lines().collect();
    assert!(log[8].contains(r#"<\\pasted_content id=\"1234\">"#), "fixture 這一列是 CLI 跳脫過又包起來的");
    std::fs::write(&transcript, log[8..].join("\n") + "\n").unwrap();
    let stop = HookBody {
        bot_id: bot_id.clone(),
        provider: "claude".into(),
        payload: json!({
            "hook_event_name": "Stop",
            "session_id": "2c0bdae3-b75f-4dad-a24c-ed9b751cb14d",
            "prompt_id": db::ulid(),
            "transcript_path": transcript.to_string_lossy(),
            "last_assistant_message": "OK",
        }),
        received_at: None,
        truncated: false,
        run_id: None,
    };
    process(&app, &stop).await.unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("completed", "ok"), "同一則：認領並升成 ok");
    let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND origin='external'")
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(external, 0, "沒有多一筆外部回合");
}

#[tokio::test]
async fn stop_hook_resolves_unknown_delivery_when_it_completes_a_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'hook-unknown','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
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
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
         VALUES (?,?,?,'web','in_flight','unknown',?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    process(
        &app,
        &HookBody {
            bot_id,
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "native-session",
                "prompt_id": "native-turn",
                "last_assistant_message": "hook reply",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(turn.status, "completed");
    assert_eq!(turn.delivery, "ok");
    let assistant_messages: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'",
    )
    .bind(&turn_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(assistant_messages, 1);
}

/// issue #1471：補送過一次（`auto_resend=0`、`resend_count=1`）而且送出當下沒有逐字證據（`delivery_verified=0`）的回合，
/// 之後 hook 回報並完成——hook 就是事後證據，daemon 在收尾那一刻把 `delivery_verified` 寫成 1，
/// 其他消費者（turn JSON、AGM 交辦的 `delivery`）一起變對。還沒完成的照舊 0。
#[tokio::test]
async fn a_hook_that_completes_a_resent_unverified_turn_marks_its_delivery_verified() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'hook-resent','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
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
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, delivery_verified, auto_resend, resend_count, created_at)
         VALUES (?,?,?,'web','in_flight','ok',0,0,1,?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let read = || async {
        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        (t.status.clone(), serde_json::to_value(&t).unwrap()["delivery_verified"].clone(), t.native_turn_id.clone())
    };
    assert_eq!(read().await, ("in_flight".into(), json!(0), None), "還沒有 hook 回報：照舊標未驗證");

    process(
        &app,
        &HookBody {
            bot_id,
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "native-session",
                "prompt_id": "native-turn",
                "last_assistant_message": "hook reply",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(read().await, ("completed".into(), json!(1), Some("native-turn".into())), "hook 收尾就是事後證據");
}

/// #1760：遠端 claude 不送 UserPromptSubmit；補送過、沒有逐字證據的回合，送出之後收到 PostToolUse 就補 `delivery_verified=1`，
/// 不等 Stop。送出之前的工具活動（上一則的尾巴、spool 晚到）不算。
#[tokio::test]
async fn a_tool_hook_after_delivery_verifies_an_in_flight_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "tool-verify").await;
    let run_id = tt::fake_run(&app, &bot.id).await;
    let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, delivery_verified, auto_resend, resend_count, delivered_at, created_at)
         VALUES (?,?,?,'web','in_flight','ok',0,0,1,'2026-10-14T08:35:33.557Z',?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let verified = || async { sqlx::query_scalar::<_, i64>("SELECT delivery_verified FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap() };
    let tool = |at: &str| HookBody {
        bot_id: bot.id.clone(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": "PostToolUse", "tool_name": "Read", "tool_input": {}, "tool_response": {}}),
        received_at: Some(at.into()),
        truncated: false,
        run_id: None,
    };
    process(&app, &tool("2026-10-14T08:35:20Z")).await.unwrap();
    assert_eq!(verified().await, 0, "送出之前的工具活動不算");
    process(&app, &tool("2026-10-14T08:35:43Z")).await.unwrap();
    assert_eq!(verified().await, 1, "送出之後跑了工具＝收到了");
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(status, "in_flight", "只補證據，不收回合");
}

/// issue #1471 的反面：備援（終端畫面）收尾、沒有 hook 證據的回合維持原判；晚到 hook 帶著 native turn id
/// 才算證據，沒有 turn id 的（只知道 session）不算。
#[tokio::test]
async fn only_a_late_hook_that_binds_a_native_turn_id_verifies_a_fallback_turn() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (conv, turn_id) = late_hook_fixture(&env, "late-verify", "completed_fallback").await;
    sqlx::query("UPDATE turns SET delivery_verified=0, auto_resend=0, resend_count=1 WHERE id=?").bind(&turn_id).execute(&app.db).await.unwrap();
    lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "備援抓的回覆", "terminal_fallback", true, None).await.unwrap();
    let verified = || async { sqlx::query_scalar::<_, i64>("SELECT delivery_verified FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap() };
    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(verified().await, 0, "備援收尾、沒有 hook 證據");

    // 同一回合的晚到 hook，但沒有 native turn id：沒有證據可以綁。
    fill_or_drop_late_hook(&app, &turn, "", &Some("s1".into()), &None, true, None).await.unwrap();
    assert_eq!(verified().await, 0, "只有 session id 不是這一回合的證據");

    // 同一回合的晚到 hook 帶著 native turn id：綁上了就是證據。
    fill_or_drop_late_hook(&app, &turn, "", &Some("s1".into()), &Some("n1".into()), true, None).await.unwrap();
    assert_eq!(verified().await, 1, "hook 綁上 native turn id 後視為已送達");
}

/// Hook vs. fallback race (review 2026-09-12 #5). The BEFORE UPDATE trigger stands in for the
/// fallback winning the CAS. Losing the CAS is evidence the hook belongs to that very turn (8e7ef5fa),
/// so its transcript text replaces the fallback's reply in place — still one answer, not two.
#[tokio::test]
async fn a_hook_that_loses_the_cas_to_the_fallback_adds_no_second_reply() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'hook-race','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
         VALUES (?,?,?,'web','in_flight','ok',?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let fallback = crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "from the pane", "terminal_fallback", true, None)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fallback_wins BEFORE UPDATE OF status ON turns
         WHEN OLD.status='in_flight' AND NEW.status='completed'
         BEGIN
           UPDATE turns SET status='completed_fallback', completed_at=NEW.completed_at WHERE id=OLD.id;
           SELECT RAISE(IGNORE);
         END",
    )
    .execute(&app.db)
    .await
    .unwrap();

    process(
        &app,
        &HookBody {
            bot_id,
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "native-session",
                "prompt_id": "native-turn",
                "last_assistant_message": "from the hook",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(turn.status, "completed", "the hook's reply supersedes the fallback's claim");
    assert_eq!(turn.native_turn_id.as_deref(), Some("native-turn"), "the ids land on that turn, so a retry dedups");
    assert_eq!(turn.native_session_id.as_deref(), Some("native-session"));
    let replies: Vec<(String, String, String, i64)> =
        sqlx::query_as("SELECT id, source, content, incomplete FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
    assert_eq!(
        replies,
        [(fallback.id.clone(), "hook".to_string(), "from the hook".to_string(), 0)],
        "one answer, not two: the fallback's row is overwritten in place"
    );
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(turns, 1, "and no external turn was opened for the dropped payload");
}

/// #76 item 3/8：使用者 interrupt 跟 reconcile 判定 run 已消失，最終都靠 `fail_in_flight`
/// 把 in-flight 回合標 failed——這是兩條呼叫端共用的那支函式。這支測的是標完之後的窄窗：
/// 舊 session 的 Stop hook 才姍姍來遲。`in_flight_turn` 只認 `status='in_flight'`，所以已經
/// failed 的回合救不回來；遲到的回覆改記成一筆新的 external turn，不會憑空消失、也不會接到
/// 已經結案的回合上（跟 review 2026-09-12 #5 同一個 CAS 精神，這裡換一個把回合收掉的呼叫端）。
#[tokio::test]
async fn a_late_stop_hook_does_not_resurrect_a_turn_that_interrupt_already_failed() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "interrupt-race").await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
    )
    .bind(&run_id)
    .bind(&bot.id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
         VALUES (?,?,?,'web','in_flight','ok',?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    // `interrupt_bot` 本身要碰 herdr pane；直接呼叫它跟 `mark_run_exited` 共用的那支收尾函式，
    // 單獨驗證 DB 這一段的不變量（herdr 那半邊 `interrupt_bot`/`abort_turns` 自己的測試已經蓋到）。
    lifecycle::fail_in_flight(&app, &run_id, "interrupted by user").await.unwrap();
    let before = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(before.status, "failed");

    process(
        &app,
        &HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "native-session",
                "prompt_id": "native-turn",
                "last_assistant_message": "遲到的回覆",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(turn.status, "failed", "已經被 fail_in_flight 收掉的回合不能被遲到的 hook 救回來");
    let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, 0, "遲到的回覆不會接到已經結案的回合上");
    let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND origin='external'")
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(external, 1, "回覆不會憑空消失，改記成一筆外部回合");
}

/// 同一個不變量，換 reconcile 判定 run 已消失那條路（`mark_run_exited`）：run 整個轉成
/// `exited`，之後 `db::active_run` 找不到它，process_locked 的 `run` 變 `None`——跟上一支
/// 是不同的程式分支，要分開測。
#[tokio::test]
async fn a_late_stop_hook_does_not_resurrect_a_turn_whose_run_reconcile_already_exited() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "reconcile-race").await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
    )
    .bind(&run_id)
    .bind(&bot.id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
         VALUES (?,?,?,'web','in_flight','ok',?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    lifecycle::mark_run_exited(&app, &run_id, "agent not found during reconcile").await;
    let run = sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(run.state, "exited");
    let before = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(before.status, "failed");

    process(
        &app,
        &HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "native-session",
                "prompt_id": "native-turn",
                "last_assistant_message": "遲到的回覆",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        },
    )
    .await
    .unwrap();

    let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(turn.status, "failed", "run 已經 exited，舊 session 的 hook 不能把它的回合救回來");
    let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(replies, 0, "遲到的回覆不會接到已經結案的回合上");
}

/// Step 4 must find the `external` turn, or step 5 inserts a duplicate.
#[tokio::test]
async fn stop_hook_finds_the_open_external_turn() {
    let (_tmp, pool, run, conv) = fixture().await;
    let now = db::now();
    sqlx::query(
        "INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at)
         VALUES ('t',?,?,'external','in_flight','ok',?)",
    )
    .bind(&conv)
    .bind(&run)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    let found = db::in_flight_turn(&pool, &run).await.unwrap().expect("step 4 must claim the external turn");
    assert_eq!(found.id, "t");
    assert_eq!(found.origin, "external");
    // `delivery` must be `ok` or the §4.3 terminal fallback refuses to close the turn.
    assert_eq!(found.delivery, "ok");

    // Afterwards nothing is in flight, so a retried hook dedups.
    sqlx::query("UPDATE turns SET status='completed', completed_at=?, native_turn_id='u' WHERE id=? AND status='in_flight'")
        .bind(&now)
        .bind("t")
        .execute(&pool)
        .await
        .unwrap();
    assert!(db::in_flight_turn(&pool, &run).await.unwrap().is_none());
}

#[tokio::test]
async fn turn_user_messages_returns_the_scraped_echo() {
    let (_tmp, pool, run, conv) = fixture().await;
    let now = db::now();
    sqlx::query(
        "INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at)
         VALUES ('t',?,?,'external','in_flight','ok',?)",
    )
    .bind(&conv)
    .bind(&run)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at)
         VALUES ('m',?, 't','user','echo 1','hook',?)",
    )
    .bind(&conv)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    let have = db::turn_user_messages(&pool, "t").await.unwrap();
    assert_eq!(have, vec!["echo 1".to_string()]);
    assert!(!hook_user_is_new(&have, "echo 1"));
    assert!(hook_user_is_new(&have, "echo 2"));
}
