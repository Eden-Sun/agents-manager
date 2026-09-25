//! 輸入框卡著草稿（409 `composer_busy`）的兩個動作：送出框裡那段、清掉再送我這則。
//! 草稿一律用中性假字；真畫面來自 2026-09-26 隔離 herdr session 的實抓（`fixtures/*-draft.ansi`／`*-cleared.ansi`）。

use super::*;
use crate::testing as tt;

// ---- 真畫面：清框的鍵按下去之前／之後，daemon 讀得出什麼 ----

const CLAUDE_DRAFT: &str = include_str!("fixtures/claude-2.1.281-draft.ansi");
const CLAUDE_CLEARED: &str = include_str!("fixtures/claude-2.1.281-draft-cleared.ansi");
const CLAUDE_MULTI: &str = include_str!("fixtures/claude-2.1.281-multiline-draft.ansi");
const CLAUDE_MULTI_CLEARED: &str = include_str!("fixtures/claude-2.1.281-multiline-cleared.ansi");
const CLAUDE_LONG: &str = include_str!("fixtures/claude-2.1.281-folded-draft.ansi");
const CODEX_DRAFT: &str = include_str!("fixtures/codex-0.155-draft.ansi");
const CODEX_CLEARED: &str = include_str!("fixtures/codex-0.155-draft-cleared.ansi");
const CODEX_MULTI: &str = include_str!("fixtures/codex-0.155-multiline-draft.ansi");
const CODEX_MULTI_CLEARED: &str = include_str!("fixtures/codex-0.155-multiline-cleared.ansi");
const GROK_DRAFT: &str = include_str!("fixtures/grok-1.0.41-draft.ansi");
const GROK_CLEARED: &str = include_str!("fixtures/grok-1.0.41-draft-cleared.ansi");
const GROK_MULTI: &str = include_str!("fixtures/grok-1.0.41-multiline-draft.ansi");
const GROK_MULTI_CLEARED: &str = include_str!("fixtures/grok-1.0.41-multiline-cleared.ansi");

const ONE_LINE: &str = "fixture draft alpha beta gamma";
const THREE_LINES: &str = "fixture line one\nfixture line two\nfixture line three";

/// 三種 kind、單行與多行：框裡的字讀得出來（就是 409 回的 `draft`），`ctrl+c` 之後重讀是空框（清框成功的判準）。
#[test]
fn every_kind_reads_its_real_draft_and_its_real_cleared_box() {
    for (kind, draft, cleared, text) in [
        ("claude", CLAUDE_DRAFT, CLAUDE_CLEARED, ONE_LINE),
        ("claude", CLAUDE_MULTI, CLAUDE_MULTI_CLEARED, THREE_LINES),
        ("codex", CODEX_DRAFT, CODEX_CLEARED, ONE_LINE),
        ("codex", CODEX_MULTI, CODEX_MULTI_CLEARED, THREE_LINES),
        ("grok", GROK_DRAFT, GROK_CLEARED, ONE_LINE),
        ("grok", GROK_MULTI, GROK_MULTI_CLEARED, THREE_LINES),
    ] {
        assert_eq!(box_state(kind, draft), BoxState::NonEmpty, "{kind}: {text:?}");
        assert_eq!(composer_text(kind, draft).as_deref(), Some(text), "{kind}");
        assert_eq!(box_state(kind, cleared), BoxState::Empty, "{kind}: cleared");
        assert_eq!(composer_text(kind, cleared), None, "{kind}: cleared");
    }
}

/// claude 2.1.281 十四列的長段貼上照樣整段畫在框裡：讀得到頭尾，清掉之後跟單行一樣是空框（同一張 cleared）。
#[test]
fn a_long_claude_draft_reads_whole() {
    assert_eq!(box_state("claude", CLAUDE_LONG), BoxState::NonEmpty);
    let text = composer_text("claude", CLAUDE_LONG).unwrap();
    assert!(text.starts_with("fixture folded row 1\n") && text.ends_with("fixture folded row 14"), "{text}");
}

#[test]
fn only_verified_kinds_get_the_actions() {
    for kind in ["claude", "codex", "grok"] {
        assert_eq!(actions(kind), ["submit", "clear"], "{kind}");
    }
    assert!(actions("shell").is_empty());
}

#[test]
fn a_long_draft_is_cut_for_display_and_matched_on_the_cut() {
    let long = "字".repeat(DRAFT_SHOWN_CHARS + 20);
    let (text, cut) = shown(&long);
    assert_eq!(text.chars().count(), DRAFT_SHOWN_CHARS);
    assert!(cut);
    assert!(same_draft(&text, &long));
    assert!(!same_draft(&text, &format!("x{long}")));
    assert_eq!(shown("短短一句"), ("短短一句".into(), false));
}

// ---- 走 prompt API：fake herdr 的框 ----

struct Fx {
    env: tt::Env,
    bot_id: String,
    conv: String,
}

/// 閒著的 bot；`transcript` 時 run 綁著本機 session log（claude 的無損證據）。
async fn idle(kind: &str, composer: &[&str], transcript: bool) -> Fx {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'draft-bot',?,'[]',0,?,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(kind)
    .bind(i64::from(transcript))
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let log = transcript.then(|| {
        let p = env.dir.join(format!("session-{}.jsonl", db::ulid()));
        std::fs::write(&p, "").unwrap();
        p
    });
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, native_session_id, transcript_path, started_at)
         VALUES (?,?,'running','idle','ws-1','pane-d','draft-bot','test',1,?,?,?)",
    )
    .bind(db::ulid())
    .bind(&bot_id)
    .bind(log.as_ref().map(|_| "sess-d"))
    .bind(log.as_ref().map(|p| p.to_str().unwrap().to_string()))
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    env.herdr.live_pane(
        "pane-d",
        tt::LivePane {
            composer: composer.iter().map(|s| s.to_string()).collect(),
            width: Some(120),
            transcript_file: log,
            codex: kind == "codex",
            boxed: kind == "grok",
            ..Default::default()
        },
    );
    Fx { env, bot_id, conv }
}

impl Fx {
    async fn turns(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?").bind(&self.conv).fetch_one(&self.env.app.db).await.unwrap()
    }
    fn keys(&self) -> Vec<Value> {
        self.env.herdr.calls_to("pane.send_keys").into_iter().filter_map(|c| c.get("keys").cloned()).collect()
    }
    fn typed(&self) -> usize {
        self.env.herdr.calls_to("pane.send_text").len()
    }
    fn composer(&self) -> Vec<String> {
        self.env.herdr.pane("pane-d").unwrap().composer
    }
    async fn send(&self, text: &str, crid: &str, clear: Option<&str>) -> LcResult<PromptOut> {
        prompt_from_api(&self.env.app, &self.bot_id, text, crid, &[], RelaySrc::default(), false, clear).await
    }
    async fn user_message(&self) -> String {
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'user'").bind(&self.conv).fetch_one(&self.env.app.db).await.unwrap()
    }
}

fn conflict(r: LcResult<PromptOut>) -> Value {
    match r {
        Err(LcError::Conflict(v)) => v,
        other => panic!("expected a 409, got {:?}", other.map(|o| o.delivery)),
    }
}

/// 以前網頁只拿到「清掉或送出之後再送一次」，看不到框裡是什麼、也沒地方處理（2026-09-26 w16T:p3）。
#[tokio::test]
async fn a_busy_box_says_what_is_in_it_and_what_can_be_done() {
    let f = idle("claude", &["一段留在框裡的假草稿"], false).await;
    let body = conflict(f.send("我自己要送的", "c1", None).await);
    assert_eq!(body["reason"], "composer_busy", "{body}");
    assert_eq!(body["draft"], "一段留在框裡的假草稿", "{body}");
    assert_eq!(body["draft_truncated"], false, "{body}");
    assert_eq!(body["draft_actions"], json!(["submit", "clear"]), "{body}");
    assert_eq!(f.turns().await, 0);
    assert!(f.keys().is_empty() && f.typed() == 0, "只看、不按");
}

/// 清掉再送我這則：按一次清框鍵、重讀是空框，才照一般流程打字送出；框裡只剩（送出去的）我這則。
#[tokio::test]
async fn clearing_the_confirmed_draft_then_sends_the_prompt() {
    for kind in ["claude", "codex", "grok"] {
        let f = idle(kind, &["一段留在框裡的假草稿"], false).await;
        let out = f.send("我自己要送的", "c1", Some("一段留在框裡的假草稿")).await.unwrap_or_else(|e| panic!("{kind}: {e:?}"));
        assert!(out.delivery == "ok" || out.delivery == "unverified", "{kind}: {}", out.delivery);
        assert_eq!(f.keys().first(), Some(&json!(["ctrl+c"])), "{kind}: 先清框");
        assert_eq!(f.typed(), 1, "{kind}");
        let sent = f.env.herdr.pane("pane-d").unwrap().transcript;
        assert!(sent.iter().any(|r| r.contains("我自己要送的")) && !sent.iter().any(|r| r.contains("假草稿")), "{kind}: {sent:?}");
        assert!(f.composer().is_empty(), "{kind}");
    }
}

/// 框在使用者按下去之前換了字：清掉的會是沒看過的東西——不按、不打，回新的草稿讓使用者再確認。
#[tokio::test]
async fn a_draft_that_changed_is_neither_cleared_nor_typed_over() {
    let f = idle("claude", &["別人剛打的另一段"], false).await;
    let body = conflict(f.send("我自己要送的", "c1", Some("一段留在框裡的假草稿")).await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["draft"], "別人剛打的另一段", "{body}");
    assert!(f.keys().is_empty() && f.typed() == 0);
    assert_eq!(f.composer(), ["別人剛打的另一段"]);
    assert_eq!(f.turns().await, 0);
}

/// 清框鍵按了、重讀框裡還有字：回錯、一個字都不打（兩段字接在一起送出去才是最糟的）。
#[tokio::test]
async fn a_draft_that_will_not_clear_blocks_the_prompt() {
    let f = idle("claude", &["一段留在框裡的假草稿"], false).await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("draft_after_clear_key", &f.bot_id, move || async move {
        live.lock().unwrap().get_mut("pane-d").unwrap().composer = vec!["清不掉的殘字".into()];
    });
    let body = conflict(f.send("我自己要送的", "c1", Some("一段留在框裡的假草稿")).await);
    assert_eq!(body["reason"], "draft_uncleared", "{body}");
    assert_eq!(body["sent"], false, "{body}");
    assert_eq!(body["draft"], "清不掉的殘字", "{body}");
    assert_eq!(f.typed(), 0);
    assert_eq!(f.turns().await, 0);
}

/// 框本來就空了（使用者已經在終端清掉）：沒有東西要清，不按 `ctrl+c`——空框的 `ctrl+c` 是「再按一次離開」。
#[tokio::test]
async fn an_empty_box_gets_no_clear_key() {
    let f = idle("claude", &[], false).await;
    f.send("我自己要送的", "c1", Some("一段留在框裡的假草稿")).await.unwrap();
    assert_eq!(f.typed(), 1);
    assert!(!f.keys().iter().any(|k| k == &json!(["ctrl+c"])), "{:?}", f.keys());
}

/// 送出框裡那段：按 Enter、不重打；開一個回合，訊息就是框裡那段，transcript 多出來那一則就是證據。
#[tokio::test]
async fn submitting_the_draft_presses_enter_and_opens_a_proven_turn() {
    let f = idle("claude", &["一段留在框裡的假草稿"], true).await;
    let out = submit(&f.env.app, &f.bot_id, "一段留在框裡的假草稿", "s1").await.unwrap();
    assert_eq!(out.delivery, "ok");
    assert_eq!(f.typed(), 0, "不重打字");
    assert_eq!(f.keys(), [json!(Submit::Enter.keys())], "只按一次送出鍵");
    assert!(f.composer().is_empty());
    assert_eq!(f.turns().await, 1);
    assert_eq!(f.user_message().await, "一段留在框裡的假草稿");
    let delivery: String = sqlx::query_scalar("SELECT delivery FROM turns WHERE id = ?").bind(&out.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(delivery, "ok");
    // 同一個 request id 再問：拿到同一筆，不再按一次。
    let again = submit(&f.env.app, &f.bot_id, "一段留在框裡的假草稿", "s1").await.unwrap();
    assert_eq!(again.turn_id, out.turn_id);
    assert_eq!(f.keys().len(), 1);
}

/// 畫面讀來的字不是原文（這裡：行尾空白被畫面吃掉）：對話裡記 session log 那一則的原文。
#[tokio::test]
async fn the_conversation_keeps_the_session_logs_text_not_the_screen_reading() {
    let f = idle("claude", &["尾巴有空白的假草稿   "], true).await;
    let out = submit(&f.env.app, &f.bot_id, "尾巴有空白的假草稿", "s1").await.unwrap();
    assert_eq!(out.delivery, "ok");
    assert_eq!(f.user_message().await, "尾巴有空白的假草稿   ");
}

/// 沒有無損證據（沒有 transcript）：框在 Enter 後清空就是送出去了，記成 unverified。
#[tokio::test]
async fn submitting_without_a_session_log_is_unverified() {
    for kind in ["codex", "grok"] {
        let f = idle(kind, &["一段留在框裡的假草稿"], false).await;
        let out = submit(&f.env.app, &f.bot_id, "一段留在框裡的假草稿", "s1").await.unwrap_or_else(|e| panic!("{kind}: {e:?}"));
        assert_eq!(out.delivery, "unverified", "{kind}");
        assert_eq!(f.typed(), 0, "{kind}");
        assert!(f.composer().is_empty(), "{kind}");
    }
}

/// 框裡換了字、或已經空了：不按 Enter，不開回合。
#[tokio::test]
async fn submitting_a_draft_that_changed_or_is_gone_presses_nothing() {
    let f = idle("claude", &["別人剛打的另一段"], true).await;
    let body = conflict(submit(&f.env.app, &f.bot_id, "一段留在框裡的假草稿", "s1").await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["draft"], "別人剛打的另一段", "{body}");
    assert!(f.keys().is_empty());
    assert_eq!(f.turns().await, 0);

    let g = idle("claude", &[], true).await;
    let body = conflict(submit(&g.env.app, &g.bot_id, "一段留在框裡的假草稿", "s2").await);
    assert_eq!(body["reason"], "draft_gone", "{body}");
    assert!(g.keys().is_empty());
    assert_eq!(g.turns().await, 0);
}

/// herdr 沒收下 Enter、框裡還是那一段：沒送出去，撤回回合（不留一筆 unknown 擋住之後每一則）。
#[tokio::test]
async fn an_enter_herdr_refused_withdraws_the_turn() {
    let f = idle("claude", &["一段留在框裡的假草稿"], true).await;
    f.env.herdr.fail_next("pane.send_keys", tt::Fault::Refuse);
    match submit(&f.env.app, &f.bot_id, "一段留在框裡的假草稿", "s1").await {
        Err(LcError::Upstream(m)) => assert!(m.contains("Enter"), "{m}"),
        other => panic!("expected a 502, got {:?}", other.map(|o| o.delivery)),
    }
    assert_eq!(f.turns().await, 0);
    assert_eq!(f.composer(), ["一段留在框裡的假草稿"]);
}

/// 有回合在跑（插隊送出那條路）：`ctrl+c` 會打斷它，不清框、不按任何鍵。
#[tokio::test]
async fn a_running_turn_never_gets_the_clear_key() {
    let f = idle("claude", &["一段留在框裡的假草稿"], false).await;
    let app = &f.env.app;
    let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
    let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
    let client = app.herdr_for_run(&run).await.unwrap();
    let body = match clear(&client, &run, &bot, "一段留在框裡的假草稿", true).await {
        Err(LcError::Conflict(v)) => v,
        other => panic!("expected a 409, got {other:?}"),
    };
    assert_eq!(body["reason"], "draft_clear_while_busy", "{body}");
    assert!(f.keys().is_empty());
    assert_eq!(f.composer(), ["一段留在框裡的假草稿"]);
}
