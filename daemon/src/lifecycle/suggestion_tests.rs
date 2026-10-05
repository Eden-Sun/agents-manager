//! 一鍵送出建議下一句（Tab → 確認 → Enter）：fake herdr 的 claude 框會把灰字畫成 dim，Tab 把它收進框裡，Enter 送進 transcript。
//! 文字一律用中性假字。

use super::*;
use crate::testing as tt;

struct Fx {
    env: tt::Env,
    bot_id: String,
    conv: String,
    run_id: String,
}

const SUGGESTION: &str = "跑一次完整測試再收尾";

/// 閒著的 claude bot，輸入框是空的、畫著灰字建議；run 綁著本機 session log（無損證據）。
async fn idle(suggestion: Option<&str>) -> Fx {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'sugg-bot','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let log = env.dir.join("claude-config/projects/-test").join(format!("session-{}.jsonl", db::ulid()));
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "").unwrap();
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
        .bind(json!({"CLAUDE_CONFIG_DIR": env.dir.join("claude-config").to_string_lossy()}).to_string())
        .bind(&bot_id)
        .execute(&app.db)
        .await
        .unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, native_session_id, transcript_path, started_at)
         VALUES (?,?,'running','idle','ws-1','pane-s','sugg-bot','test',1,'sess-s',?,?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(log.to_str().unwrap())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    env.herdr.live_pane(
        "pane-s",
        tt::LivePane {
            suggestion: suggestion.map(Into::into),
            width: Some(120),
            revision: 1,
            transcript_file: Some(log),
            ..Default::default()
        },
    );
    Fx { env, bot_id, conv, run_id }
}

impl Fx {
    async fn accept(&self, text: &str, crid: &str) -> LcResult<PromptOut> {
        accept(&self.env.app, &self.bot_id, text, Some(&self.run_id), crid).await
    }
    fn keys(&self) -> Vec<Value> {
        self.env.herdr.calls_to("pane.send_keys").into_iter().filter_map(|c| c.get("keys").cloned()).collect()
    }
    fn typed(&self) -> usize {
        self.env.herdr.calls_to("pane.send_text").len()
    }
    fn pane(&self) -> tt::LivePane {
        self.env.herdr.pane("pane-s").unwrap()
    }
    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE conversation_id = ?")).bind(&self.conv).fetch_one(&self.env.app.db).await.unwrap()
    }
}

fn conflict(r: LcResult<PromptOut>) -> Value {
    match r {
        Err(LcError::Conflict(v)) => v,
        other => panic!("expected a 409, got {:?}", other.map(|o| o.delivery)),
    }
}

/// 網頁看到的那句還在 → Tab、Enter 兩個鍵（不是打字），記成一個網頁回合與一則使用者訊息，內容是 session log 的原文。
#[tokio::test]
async fn accepting_presses_tab_then_enter_and_records_one_web_turn() {
    let f = idle(Some(SUGGESTION)).await;
    crate::prompt_suggestion::set(&f.run_id, Some(SUGGESTION.into()));
    let out = f.accept(SUGGESTION, "c1").await.unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(out.delivery, "ok", "claude 有 transcript 證據");
    assert_eq!(f.keys(), [json!(["tab"]), json!(["Enter"])], "對 pane 送的就是 Tab 與 Enter");
    assert_eq!(f.typed(), 0, "一個字都沒打進去");
    assert!(f.pane().composer.is_empty() && f.pane().suggestion.is_none());
    assert!(f.pane().transcript.iter().any(|r| r.contains(SUGGESTION)));

    assert_eq!(f.count("turns").await, 1, "一個回合");
    let (origin, delivery, prompt): (String, String, Option<String>) =
        sqlx::query_as("SELECT origin, delivery, prompt_text FROM turns WHERE id = ?").bind(&out.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!((origin.as_str(), delivery.as_str(), prompt.as_deref()), ("web", "ok", Some(SUGGESTION)), "不是外部回合");
    let msgs: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT role, content, source, sent_via FROM messages WHERE conversation_id = ?").bind(&f.conv).fetch_all(&f.env.app.db).await.unwrap();
    assert_eq!(msgs, [("user".into(), SUGGESTION.into(), "web".into(), None)], "只記一則，跟網頁送出的 prompt 一樣");
    assert_eq!(crate::prompt_suggestion::of(&f.run_id), None, "用掉了就忘掉");
}

/// 網頁重送同一個 client_request_id（回應遺失）：回原本那一筆，不再按任何鍵。
#[tokio::test]
async fn a_retry_with_the_same_request_id_does_not_press_again() {
    let f = idle(Some(SUGGESTION)).await;
    let first = f.accept(SUGGESTION, "c1").await.unwrap();
    let keys = f.keys();
    let again = f.accept(SUGGESTION, "c1").await.unwrap();
    assert_eq!(again.turn_id, first.turn_id);
    assert_eq!(f.keys(), keys, "沒有多按");
    assert_eq!(f.count("turns").await, 1);
}

/// 畫面上的建議跟網頁看到的不同：不按任何鍵，409 帶新的那句，記憶帳換成新的。
#[tokio::test]
async fn a_changed_suggestion_is_refused_without_any_key() {
    let f = idle(Some("換成別句了")).await;
    let body = conflict(f.accept(SUGGESTION, "c1").await);
    assert_eq!(body["reason"], "suggestion_changed", "{body}");
    assert_eq!(body["suggestion"], "換成別句了", "{body}");
    assert_eq!(body["sent"], false);
    assert_eq!(body["tab_sent"], false);
    assert!(f.keys().is_empty() && f.typed() == 0);
    assert_eq!(crate::prompt_suggestion::of(&f.run_id).as_deref(), Some("換成別句了"));
    assert_eq!(f.count("turns").await, 0);
}

/// 建議已經不在（被用掉、CLI 收掉）：409，不按。框裡若是使用者打的字，附草稿與動作讓網頁處理。
#[tokio::test]
async fn a_gone_suggestion_is_refused_without_any_key_and_shows_a_typed_draft() {
    let f = idle(None).await;
    crate::prompt_suggestion::set(&f.run_id, Some(SUGGESTION.into()));
    let body = conflict(f.accept(SUGGESTION, "c1").await);
    assert_eq!(body["reason"], "suggestion_gone", "{body}");
    assert_eq!(body["tab_sent"], false);
    assert!(f.keys().is_empty());
    assert_eq!(crate::prompt_suggestion::of(&f.run_id), None, "網頁看到的那句不在了：帳也清掉");

    f.env.herdr.live.lock().unwrap().get_mut("pane-s").unwrap().composer = vec!["使用者自己打的字".into()];
    let body = conflict(f.accept(SUGGESTION, "c2").await);
    assert_eq!(body["reason"], "suggestion_gone", "{body}");
    assert_eq!(body["draft"], "使用者自己打的字", "{body}");
    assert_eq!(body["draft_actions"], json!(["submit", "clear"]));
    assert!(f.keys().is_empty(), "使用者的字一個鍵都不碰");
    assert_eq!(f.pane().composer, ["使用者自己打的字"]);
}

/// 一般送出的閘門在 Tab **之前**擋：回合在飛、run 對不上、不是 claude、對話框開著，都是一個鍵都不按。
#[tokio::test]
async fn the_gates_refuse_before_tab() {
    // run 對不上（bot 重啟過）。
    let f = idle(Some(SUGGESTION)).await;
    let body = conflict(accept(&f.env.app, &f.bot_id, SUGGESTION, Some("some-other-run"), "c1").await);
    assert_eq!(body["reason"], "run mismatch", "{body}");
    assert!(f.keys().is_empty());

    // 有回合在飛。
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES ('t-fly',?,?,'web','in_flight','ok',?)",
    )
    .bind(&f.conv)
    .bind(&f.run_id)
    .bind(db::now())
    .execute(&f.env.app.db)
    .await
    .unwrap();
    let body = conflict(f.accept(SUGGESTION, "c2").await);
    assert_eq!(body["reason"], "a turn is already in flight", "{body}");
    assert!(f.keys().is_empty() && f.pane().suggestion.as_deref() == Some(SUGGESTION), "灰字原封不動");

    // 不是 claude。
    let g = idle(Some(SUGGESTION)).await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&g.bot_id).execute(&g.env.app.db).await.unwrap();
    let body = conflict(g.accept(SUGGESTION, "c3").await);
    assert_eq!(body["reason"], "suggestion_unsupported", "{body}");
    assert!(g.keys().is_empty());

    // 空字串是 400，不是 409。
    assert!(matches!(g.accept("   ", "c4").await, Err(LcError::Bad(_))));
}

/// A stale UI can still carry an old suggestion after the run starts working. The daemon must
/// recheck the live run state before pressing Tab, even if the pane still paints the old hint.
#[tokio::test]
async fn a_busy_run_cannot_accept_a_stale_suggestion() {
    let f = idle(Some(SUGGESTION)).await;
    sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?")
        .bind(&f.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();

    let body = conflict(f.accept(SUGGESTION, "c1").await);
    assert_eq!(body["reason"], "agent is busy", "{body}");
    assert_eq!(body["sent"], false, "{body}");
    assert!(f.keys().is_empty(), "忙碌的 run 一個鍵都不按");
    assert_eq!(f.count("turns").await, 0, "不建立回合");
}

/// Tab 沒生效（框還是空的）：只按過 Tab，沒有東西要還原，不按 Enter、不開回合。
#[tokio::test]
async fn a_tab_the_cli_ignores_is_reported_and_never_followed_by_enter() {
    let f = idle(Some(SUGGESTION)).await;
    f.env.herdr.live.lock().unwrap().get_mut("pane-s").unwrap().swallow_tab = true;
    let body = conflict(f.accept(SUGGESTION, "c1").await);
    assert_eq!(body["reason"], "tab_not_accepted", "{body}");
    assert_eq!(body["tab_sent"], true);
    assert_eq!(f.keys(), [json!(["tab"])], "只有 Tab：沒有 Enter、沒有 ctrl+c");
    assert_eq!(f.count("turns").await, 0);
}

/// 讀完建議到按 Tab 之間使用者在終端打了字：Tab 之後框裡是別的字——不認得、不碰（不按 Enter、不清），409 `draft_changed` 帶那段草稿。
#[tokio::test]
async fn a_box_that_holds_something_else_after_tab_is_left_alone() {
    let f = idle(Some(SUGGESTION)).await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("suggestion_before_tab", &f.bot_id, move || async move {
        let mut live = live.lock().unwrap();
        let p = live.get_mut("pane-s").unwrap();
        p.suggestion = None;
        p.composer = vec!["他剛打的另一句".into()];
        p.revision += 1;
    });
    let body = conflict(f.accept(SUGGESTION, "c1").await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["draft"], "他剛打的另一句", "{body}");
    assert_eq!(body["tab_sent"], true);
    assert_eq!(f.keys(), [json!(["tab"])], "不再按任何鍵");
    assert_eq!(f.pane().composer, ["他剛打的另一句"]);
    assert_eq!(f.count("turns").await, 0);
}

/// Tab 收下之後、Enter 之前閘門又擋下（這裡用「框的 revision 變了」代表）：回合撤回、一個字都沒送出；
/// 框裡還是那一句 → 補一個 ctrl+c 清成乾淨的空框，講清楚。
#[tokio::test]
async fn when_enter_is_refused_after_tab_the_accepted_text_is_cleared_again() {
    let f = idle(Some(SUGGESTION)).await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("draft_before_enter", &f.bot_id, move || async move {
        let mut live = live.lock().unwrap();
        live.get_mut("pane-s").unwrap().revision += 1;
    });
    let body = conflict(f.accept(SUGGESTION, "c1").await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["tab_sent"], true);
    assert_eq!(body["suggestion_restored"], true, "{body}");
    assert_eq!(f.keys(), [json!(["tab"]), json!(["ctrl+c"])], "沒有 Enter；多的那一句用 ctrl+c 還原");
    assert!(f.pane().composer.is_empty(), "回到乾淨的空框，不留會擋住之後每則 prompt 的草稿");
    assert_eq!(f.count("turns").await, 0, "回合撤回");
    assert_eq!(f.count("messages").await, 0);
}

/// 同上，但框裡已經變成別人的字：還原用的 ctrl+c 自己會拒絕（框裡不是那一句），別人的字原封不動、`suggestion_restored: false`。
#[tokio::test]
async fn the_restore_never_clears_text_that_is_not_the_accepted_suggestion() {
    let f = idle(Some(SUGGESTION)).await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("draft_before_enter", &f.bot_id, move || async move {
        let mut live = live.lock().unwrap();
        let p = live.get_mut("pane-s").unwrap();
        p.composer = vec!["別人剛打的字".into()];
        p.revision += 1;
    });
    let body = conflict(f.accept(SUGGESTION, "c1").await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["suggestion_restored"], false, "{body}");
    assert_eq!(f.keys(), [json!(["tab"])], "沒有 Enter、沒有 ctrl+c");
    assert_eq!(f.pane().composer, ["別人剛打的字"]);
}

/// Tab 收下後、按 Enter 之前發生暫態 DB 錯誤（例如交易開始／INSERT 失敗）：
/// 1. Tab 已送出；
/// 2. Enter 確定從未送出；
/// 3. API 回傳 retryable 錯誤；
/// 4. 框裡確認還是原接受的建議字句，並以 ctrl+c 還原清成空框；
/// 5. 沒有殘留的 active / in-flight turn；
/// 6. 後續的一般 prompt 不會被殘留的草稿阻擋（composer_busy）。
#[tokio::test]
async fn pre_enter_db_failure_clears_accepted_draft_and_leaves_no_turn() {
    let f = idle(Some(SUGGESTION)).await;
    crate::prompt_suggestion::set(&f.run_id, Some(SUGGESTION.into()));
    let app = f.env.app.clone();
    super::super::race_point::arm("draft_before_turn_tx", &f.bot_id, move || async move {
        sqlx::query("CREATE TRIGGER fault_turns BEFORE INSERT ON turns BEGIN SELECT RAISE(ABORT, 'transient db error'); END")
            .execute(&app.db)
            .await
            .unwrap();
    });

    let res = f.accept(SUGGESTION, "c-fault").await;
    // 3. API fails retryably
    assert!(res.as_ref().is_err_and(|e| e.is_retryable()), "expected retryable error, got: {res:?}");

    // 1. Tab was sent
    assert!(f.keys().contains(&json!(["tab"])), "Tab 必須已送出");

    // 2. Enter was never sent
    assert!(!f.keys().contains(&json!(["Enter"])), "Enter 絕不可送出");

    // 4. exact accepted suggestion is cleared (Ctrl+C only after revalidation)
    assert!(f.keys().contains(&json!(["ctrl+c"])), "必須送出 ctrl+c 清框還原");
    assert!(f.pane().composer.is_empty(), "輸入框必須已清空");

    // 5. no active/in-flight turn remains
    assert_eq!(f.count("turns").await, 0, "不可殘留任何 turn");
    assert_eq!(f.count("messages").await, 0, "不可殘留任何 message");
    assert!(db::in_flight_turn(&f.env.app.db, &f.run_id).await.unwrap().is_none());

    // 6. next ordinary prompt is not blocked by a stale composer draft
    sqlx::query("DROP TRIGGER fault_turns").execute(&f.env.app.db).await.unwrap();
    let next = crate::lifecycle::prompt(&f.env.app, &f.bot_id, "下一則一般 prompt", "c-next").await;
    assert!(next.is_ok(), "下一則一般 prompt 必須順利送出，不被草稿阻擋: {next:?}");
}

/// Enter 送出後（或可能已送出）的簿記失敗：絕不可清理輸入框或打斷已送出的回合。
#[tokio::test]
async fn post_enter_delivery_failure_never_clears_composer_or_interrupts_turn() {
    let f = idle(Some(SUGGESTION)).await;
    crate::prompt_suggestion::set(&f.run_id, Some(SUGGESTION.into()));
    // 在 Enter 送達後的 delivery 寫回失敗
    sqlx::query("CREATE TRIGGER lost_delivery_write BEFORE UPDATE OF delivery ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
        .execute(&f.env.app.db)
        .await
        .unwrap();

    let res = f.accept(SUGGESTION, "c-post-enter").await;
    assert!(res.is_err(), "送達寫回失敗應回錯: {res:?}");

    // Tab 與 Enter 均已送出
    assert_eq!(f.keys(), [json!(["tab"]), json!(["Enter"])], "只有 Tab 與 Enter，絕無 ctrl+c");
    assert!(!f.keys().contains(&json!(["ctrl+c"])), "post-enter 失敗絕不可送 ctrl+c");

    // turn 已送出並留在 DB 中
    assert_eq!(f.count("turns").await, 1, "回合必須保留在 DB 中");
}
