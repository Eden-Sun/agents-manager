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

/// 草稿兩段之間空一行：給人看、算 token、被清掉的要是同一整段，不是讀到第一個空白列就停（使用者同意清的只是前半段）。
#[test]
fn a_claude_draft_with_a_blank_row_reads_whole() {
    let screen = CLAUDE_MULTI.replace("fixture line one\r\n", "fixture line one\r\n\r\n");
    assert_ne!(screen, CLAUDE_MULTI, "fixture 換行的寫法變了，這個測試要跟著改");
    assert_eq!(composer_text("claude", &screen).as_deref(), Some("fixture line one"), "舊讀法：停在空白列");
    assert_eq!(composer_text_whole("claude", &screen).as_deref(), Some("fixture line one\n\nfixture line two\nfixture line three"));
    // 沒有空白列的畫面兩種讀法一樣；codex／grok 維持原讀法。
    assert_eq!(composer_text_whole("claude", CLAUDE_MULTI).as_deref(), Some(THREE_LINES));
    for (kind, draft) in [("codex", CODEX_MULTI), ("grok", GROK_MULTI)] {
        assert_eq!(composer_text_whole(kind, draft), composer_text(kind, draft), "{kind}");
    }
}

#[test]
fn only_verified_kinds_get_the_actions() {
    for kind in ["claude", "codex", "grok"] {
        assert_eq!(actions(kind), ["submit", "clear"], "{kind}");
    }
    assert!(actions("shell").is_empty());
}

#[test]
fn a_long_draft_is_cut_for_display_but_identified_by_full_text_and_run_pane() {
    let long = "字".repeat(DRAFT_SHOWN_CHARS + 20);
    let (text, cut) = shown(&long);
    assert_eq!(text.chars().count(), DRAFT_SHOWN_CHARS);
    assert!(cut);
    let token = draft_token("run-1", "pane-1", &long);
    assert_eq!(token.len(), 64);
    assert!(same_draft(&token, "run-1", "pane-1", &long));
    assert!(!same_draft(&token, "run-1", "pane-1", &format!("{long}changed suffix")));
    assert!(!same_draft(&token, "run-2", "pane-1", &long));
    assert!(!same_draft(&token, "run-1", "pane-2", &long));
    assert_eq!(shown("短短一句"), ("短短一句".into(), false));
}

#[test]
fn a_full_draft_token_changes_when_only_the_unshown_suffix_changes() {
    let prefix = "a".repeat(DRAFT_SHOWN_CHARS);
    let one = draft_token("run-1", "pane-1", &format!("{prefix}suffix one"));
    let two = draft_token("run-1", "pane-1", &format!("{prefix}suffix two"));
    assert_ne!(one, two);
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
            revision: 1,
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

async fn current_draft_token(f: &Fx, crid: &str) -> String {
    let body = conflict(f.send("unused", crid, None).await);
    body["draft_token"].as_str().expect("composer_busy includes a full-draft token").to_string()
}

/// 以前網頁只拿到「清掉或送出之後再送一次」，看不到框裡是什麼、也沒地方處理（2026-09-26 w16T:p3）。
#[tokio::test]
async fn a_busy_box_says_what_is_in_it_and_what_can_be_done() {
    let f = idle("claude", &["一段留在框裡的假草稿"], false).await;
    let body = conflict(f.send("我自己要送的", "c1", None).await);
    assert_eq!(body["reason"], "composer_busy", "{body}");
    assert_eq!(body["draft"], "一段留在框裡的假草稿", "{body}");
    assert_eq!(body["draft_truncated"], false, "{body}");
    assert_eq!(body["draft_token"].as_str().unwrap().len(), 64, "{body}");
    assert_eq!(body["draft_actions"], json!(["submit", "clear"]), "{body}");
    assert_eq!(f.turns().await, 0);
    assert!(f.keys().is_empty() && f.typed() == 0, "只看、不按");
}

/// A suffix changed after the 409 cannot be authorized by the display prefix alone.
#[tokio::test]
async fn an_unseen_long_suffix_change_blocks_submit_even_when_the_prefix_matches() {
    let original = format!("{}suffix one", "a".repeat(DRAFT_SHOWN_CHARS));
    let changed = format!("{}suffix two", "a".repeat(DRAFT_SHOWN_CHARS));
    let f = idle("claude", &[&original], true).await;
    let body = conflict(f.send("unused", "observe", None).await);
    assert_eq!(body["draft"], "a".repeat(DRAFT_SHOWN_CHARS));
    let old_token = body["draft_token"].as_str().unwrap().to_string();

    f.env.herdr.live.lock().unwrap().get_mut("pane-d").unwrap().composer = vec![changed];
    let result = submit(&f.env.app, &f.bot_id, &old_token, "submit-long").await;
    let body = conflict(result);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_ne!(body["draft_token"], old_token, "the changed suffix gets a different token");
    assert!(f.keys().is_empty(), "suffix mismatch must not send Enter");
    assert_eq!(f.turns().await, 0);
}

/// The same hidden-suffix mismatch must not clear B while replacing it with the user's prompt.
#[tokio::test]
async fn an_unseen_long_suffix_change_blocks_clear_even_when_the_prefix_matches() {
    let original = format!("{}suffix one", "a".repeat(DRAFT_SHOWN_CHARS));
    let changed = format!("{}suffix two", "a".repeat(DRAFT_SHOWN_CHARS));
    let f = idle("claude", &[&original], false).await;
    let body = conflict(f.send("unused", "observe", None).await);
    assert_eq!(body["draft"], "a".repeat(DRAFT_SHOWN_CHARS));
    let old_token = body["draft_token"].as_str().unwrap().to_string();

    f.env.herdr.live.lock().unwrap().get_mut("pane-d").unwrap().composer = vec![changed.clone()];
    let body = conflict(f.send("my prompt", "clear-long", Some(&old_token)).await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_ne!(body["draft_token"], old_token, "the changed suffix gets a different token");
    assert!(f.keys().is_empty() && f.typed() == 0, "must neither clear nor type over the changed suffix");
    assert_eq!(f.composer(), [changed]);
    assert_eq!(f.turns().await, 0);
}

/// The post-commit reread must stop stale Enter on both transcript-backed and Unverified paths.
#[tokio::test]
async fn submit_rechecks_the_composer_after_commit_and_retracts_before_enter() {
    for (kind, transcript) in [("claude", true), ("grok", false)] {
        let f = idle(kind, &["draft A"], transcript).await;
        let old_token = current_draft_token(&f, "observe").await;
        let live = f.env.herdr.live.clone();
        super::super::race_point::arm("draft_before_enter", &f.bot_id, move || async move {
            let mut live = live.lock().unwrap();
            let pane = live.get_mut("pane-d").unwrap();
            pane.composer = vec!["draft B".into()];
            pane.revision = pane.revision.max(1) + 1;
        });

        let body = conflict(submit(&f.env.app, &f.bot_id, &old_token, "stale-submit").await);
        assert_eq!(body["reason"], "draft_changed", "{kind}: {body}");
        assert_eq!(body["draft"], "draft B", "{kind}: {body}");
        assert_ne!(body["draft_token"], old_token, "{kind}: changed draft has a new token");
        assert!(f.keys().is_empty(), "{kind}: changed composer must receive no Enter");
        assert_eq!(f.composer(), ["draft B"], "{kind}");
        assert_eq!(f.turns().await, 0, "{kind}: provisional A turn must be retracted");

        let next_token = body["draft_token"].as_str().unwrap();
        let out = submit(&f.env.app, &f.bot_id, next_token, "confirmed-submit").await.unwrap_or_else(|e| panic!("{kind}: {e:?}"));
        assert!(out.delivery == "ok" || out.delivery == "unverified", "{kind}: {}", out.delivery);
        assert_eq!(f.keys(), [json!(["Enter"])], "{kind}: only the explicitly retried B gets Enter");
        assert_eq!(f.user_message().await, "draft B", "{kind}");
        assert_eq!(f.turns().await, 1, "{kind}: B is recorded once");
    }
}

/// A revision change also fences Enter when the visible composer text happens to be identical.
#[tokio::test]
async fn submit_rejects_a_revision_change_even_when_the_draft_text_matches() {
    let f = idle("grok", &["draft A"], false).await;
    let token = current_draft_token(&f, "observe").await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("draft_before_enter", &f.bot_id, move || async move {
        let mut live = live.lock().unwrap();
        let pane = live.get_mut("pane-d").unwrap();
        pane.revision = pane.revision.max(1) + 1;
    });

    let body = conflict(submit(&f.env.app, &f.bot_id, &token, "revision-submit").await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["draft"], "draft A", "{body}");
    assert_eq!(body["draft_token"], token, "content identity is unchanged");
    assert!(f.keys().is_empty(), "a changed pane revision must receive no Enter");
    assert_eq!(f.composer(), ["draft A"]);
    assert_eq!(f.turns().await, 0);
}

/// Session identity is part of the authorization fence, not just the composer contents.
#[tokio::test]
async fn submit_rejects_a_session_change_before_enter() {
    let f = idle("claude", &["draft A"], true).await;
    let token = current_draft_token(&f, "observe").await;
    let app = f.env.app.clone();
    let bot_id = f.bot_id.clone();
    super::super::race_point::arm("draft_before_enter", &f.bot_id, move || async move {
        sqlx::query("UPDATE runs SET native_session_id='replaced-session' WHERE bot_id=? AND state='running'")
            .bind(bot_id)
            .execute(&app.db)
            .await
            .unwrap();
    });

    let body = conflict(submit(&f.env.app, &f.bot_id, &token, "session-submit").await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["draft"], "draft A", "{body}");
    assert!(f.keys().is_empty(), "changed session must receive no Enter");
    assert_eq!(f.composer(), ["draft A"]);
    assert_eq!(f.turns().await, 0);
}

/// 清掉再送我這則：按一次清框鍵、重讀是空框，才照一般流程打字送出；框裡只剩（送出去的）我這則。
#[tokio::test]
async fn clearing_the_confirmed_draft_then_sends_the_prompt() {
    for kind in ["claude", "codex", "grok"] {
        let f = idle(kind, &["一段留在框裡的假草稿"], false).await;
        let token = current_draft_token(&f, "observe").await;
        let out = f.send("我自己要送的", "c1", Some(&token)).await.unwrap_or_else(|e| panic!("{kind}: {e:?}"));
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

/// A race between clear validation and Ctrl+C leaves B untouched and returns B for confirmation.
#[tokio::test]
async fn clear_rechecks_the_composer_after_validation_and_preserves_a_new_draft() {
    let f = idle("claude", &["draft A"], false).await;
    let old_token = current_draft_token(&f, "observe").await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("draft_before_clear_key", &f.bot_id, move || async move {
        let mut live = live.lock().unwrap();
        let pane = live.get_mut("pane-d").unwrap();
        pane.composer = vec!["draft B".into()];
        pane.revision = pane.revision.max(1) + 1;
    });

    let body = conflict(f.send("my prompt", "stale-clear", Some(&old_token)).await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["draft"], "draft B", "{body}");
    assert_ne!(body["draft_token"], old_token);
    assert!(f.keys().is_empty(), "changed composer must receive no Ctrl+C");
    assert_eq!(f.composer(), ["draft B"]);
    assert_eq!(f.typed(), 0);
    assert_eq!(f.turns().await, 0);
}

/// The clear revision fence rejects a changed pane even if the composer text is unchanged.
#[tokio::test]
async fn clear_rejects_a_revision_change_even_when_the_draft_text_matches() {
    let f = idle("claude", &["draft A"], false).await;
    let token = current_draft_token(&f, "observe").await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("draft_before_clear_key", &f.bot_id, move || async move {
        let mut live = live.lock().unwrap();
        let pane = live.get_mut("pane-d").unwrap();
        pane.revision = pane.revision.max(1) + 1;
    });

    let body = conflict(f.send("my prompt", "revision-clear", Some(&token)).await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["draft"], "draft A", "{body}");
    assert_eq!(body["draft_token"], token, "content identity is unchanged");
    assert!(f.keys().is_empty(), "changed pane revision must receive no Ctrl+C");
    assert_eq!(f.composer(), ["draft A"]);
    assert_eq!(f.typed(), 0);
    assert_eq!(f.turns().await, 0);
}

/// Clear must not act through a run that is no longer active, even if its pane text is unchanged.
#[tokio::test]
async fn clear_rejects_a_run_that_stopped_before_ctrl_c() {
    let f = idle("claude", &["draft A"], false).await;
    let token = current_draft_token(&f, "observe").await;
    let app = f.env.app.clone();
    let bot_id = f.bot_id.clone();
    super::super::race_point::arm("draft_before_clear_key", &f.bot_id, move || async move {
        sqlx::query("UPDATE runs SET state='stopping' WHERE bot_id=? AND state='running'")
            .bind(bot_id)
            .execute(&app.db)
            .await
            .unwrap();
    });

    let body = conflict(f.send("my prompt", "stopped-clear", Some(&token)).await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert_eq!(body["draft"], "draft A", "{body}");
    assert!(f.keys().is_empty(), "a stopped run must receive no Ctrl+C");
    assert_eq!(f.composer(), ["draft A"]);
    assert_eq!(f.typed(), 0);
    assert_eq!(f.turns().await, 0);
}

/// 清框鍵按了、重讀框裡還有字：回錯、一個字都不打（兩段字接在一起送出去才是最糟的）。
#[tokio::test]
async fn a_draft_that_will_not_clear_blocks_the_prompt() {
    let f = idle("claude", &["一段留在框裡的假草稿"], false).await;
    let token = current_draft_token(&f, "observe").await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("draft_after_clear_key", &f.bot_id, move || async move {
        live.lock().unwrap().get_mut("pane-d").unwrap().composer = vec!["清不掉的殘字".into()];
    });
    let body = conflict(f.send("我自己要送的", "c1", Some(&token)).await);
    assert_eq!(body["reason"], "draft_uncleared", "{body}");
    assert_eq!(body["sent"], false, "{body}");
    assert_eq!(body["draft"], "清不掉的殘字", "{body}");
    assert_eq!(f.typed(), 0);
    assert_eq!(f.turns().await, 0);
}

/// 框本來就空了（使用者已經在終端清掉）：舊 token 不再授權「清掉再送」，也不按 `ctrl+c`。
#[tokio::test]
async fn an_empty_box_rejects_the_stale_clear_token_without_sending_the_prompt() {
    let f = idle("claude", &["一段留在框裡的假草稿"], false).await;
    let token = current_draft_token(&f, "observe").await;
    f.env.herdr.live.lock().unwrap().get_mut("pane-d").unwrap().composer.clear();
    let body = conflict(f.send("我自己要送的", "c1", Some(&token)).await);
    assert_eq!(body["reason"], "draft_gone", "{body}");
    assert_eq!(body["sent"], false, "{body}");
    assert!(f.keys().is_empty(), "empty composer must receive no key");
    assert_eq!(f.typed(), 0, "stale clear authorization must not turn into a plain send");
    assert_eq!(f.turns().await, 0, "stale clear authorization must not create a turn");
}

/// A token from R1/P1 cannot turn an empty R2/P2 composer into permission to send replacement text.
#[tokio::test]
async fn an_empty_new_run_rejects_a_clear_token_from_the_replaced_run() {
    let f = idle("claude", &["draft A"], false).await;
    let token = current_draft_token(&f, "observe-old-run").await;
    let next_run = db::ulid();
    sqlx::query("UPDATE runs SET state='exited' WHERE bot_id=? AND state='running'")
        .bind(&f.bot_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
         VALUES (?,?,'running','idle','ws-1','pane-e','draft-bot','test',1,?)",
    )
    .bind(&next_run)
    .bind(&f.bot_id)
    .bind(db::now())
    .execute(&f.env.app.db)
    .await
    .unwrap();
    f.env.herdr.live_pane("pane-e", tt::LivePane { revision: 1, ..Default::default() });

    let body = conflict(f.send("replacement prompt", "stale-cross-run", Some(&token)).await);
    assert_eq!(body["reason"], "draft_gone", "{body}");
    assert_eq!(body["sent"], false, "{body}");
    assert!(f.keys().is_empty(), "replacement run must receive no key");
    assert_eq!(f.typed(), 0, "replacement run must receive no prompt text");
    assert_eq!(f.turns().await, 0, "a stale clear must not create a replacement-run turn");
}

/// 送出框裡那段：按 Enter、不重打；開一個回合，訊息就是框裡那段，transcript 多出來那一則就是證據。
#[tokio::test]
async fn submitting_the_draft_presses_enter_and_opens_a_proven_turn() {
    let f = idle("claude", &["一段留在框裡的假草稿"], true).await;
    let token = current_draft_token(&f, "observe").await;
    let out = submit(&f.env.app, &f.bot_id, &token, "s1").await.unwrap();
    assert_eq!(out.delivery, "ok");
    assert_eq!(f.typed(), 0, "不重打字");
    assert_eq!(f.keys(), [json!(Submit::Enter.keys())], "只按一次送出鍵");
    assert!(f.composer().is_empty());
    assert_eq!(f.turns().await, 1);
    assert_eq!(f.user_message().await, "一段留在框裡的假草稿");
    let delivery: String = sqlx::query_scalar("SELECT delivery FROM turns WHERE id = ?").bind(&out.turn_id).fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(delivery, "ok");
    // 同一個 request id 再問：拿到同一筆，不再按一次。
    let again = submit(&f.env.app, &f.bot_id, &token, "s1").await.unwrap();
    assert_eq!(again.turn_id, out.turn_id);
    assert_eq!(f.keys().len(), 1);
}

/// A token is tied to its original run and pane, even when the composer text is identical.
#[tokio::test]
async fn a_token_from_another_run_and_pane_cannot_submit_this_draft() {
    let original = idle("claude", &["same draft"], true).await;
    let token = current_draft_token(&original, "observe").await;
    let other = idle("claude", &["same draft"], true).await;
    let body = conflict(submit(&other.env.app, &other.bot_id, &token, "cross-run").await);
    assert_eq!(body["reason"], "draft_changed", "{body}");
    assert!(other.keys().is_empty());
    assert_eq!(other.turns().await, 0);
}

/// 畫面讀來的字不是原文（這裡：行尾空白被畫面吃掉）：對話裡記 session log 那一則的原文。
#[tokio::test]
async fn the_conversation_keeps_the_session_logs_text_not_the_screen_reading() {
    let f = idle("claude", &["尾巴有空白的假草稿   "], true).await;
    let token = current_draft_token(&f, "observe").await;
    let out = submit(&f.env.app, &f.bot_id, &token, "s1").await.unwrap();
    assert_eq!(out.delivery, "ok");
    assert_eq!(f.user_message().await, "尾巴有空白的假草稿   ");
}

/// 沒有無損證據（沒有 transcript）：框在 Enter 後清空就是送出去了，記成 unverified。
#[tokio::test]
async fn submitting_without_a_session_log_is_unverified() {
    for kind in ["codex", "grok"] {
        let f = idle(kind, &["一段留在框裡的假草稿"], false).await;
        let token = current_draft_token(&f, "observe").await;
        let out = submit(&f.env.app, &f.bot_id, &token, "s1").await.unwrap_or_else(|e| panic!("{kind}: {e:?}"));
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
    let token = current_draft_token(&f, "observe").await;
    f.env.herdr.fail_next("pane.send_keys", tt::Fault::Refuse);
    match submit(&f.env.app, &f.bot_id, &token, "s1").await {
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
    let body = match clear(&f.env.app, &client, &run, &bot, "一段留在框裡的假草稿", true).await {
        Err(LcError::Conflict(v)) => v,
        other => panic!("expected a 409, got {other:?}"),
    };
    assert_eq!(body["reason"], "draft_clear_while_busy", "{body}");
    assert!(f.keys().is_empty());
    assert_eq!(f.composer(), ["一段留在框裡的假草稿"]);
}
