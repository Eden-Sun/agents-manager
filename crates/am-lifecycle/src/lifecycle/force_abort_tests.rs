//! #581：強制中止之後輸入框「讀不到」、每則都 409 `composer_unreadable`，對話裡一次冒出好幾則一樣的中止說明。
//!
//! 真的經過（2026-09-27 wits-pro，隔離 herdr session＋真 claude 2.1.281 重現）：回合還沒吐字就被 `esc` 打斷，
//! claude 把整段 prompt 放回框裡；框長到二十幾列，框頂的 `❯` 落在倒數 24 列之外。清框那一步判成「框是空的」沒按
//! `ctrl+c`，送出前的檢查也找不到框——字明明在框裡，卻回 `composer_unreadable`，網頁連清草稿的動作都拿不到。
//! 草稿一律用中性假字；真畫面是 `fixtures/claude-2.1.281-tall-draft-after-interrupt.ansi`。

use super::super::{delivery, run_state};
use super::*;
use crate::testing as tt;

const TALL_DRAFT: &str = include_str!("fixtures/claude-2.1.281-tall-draft-after-interrupt.ansi");

/// 放回框裡的那段有幾列：比 [`COMPOSER_TAIL`] 高，框頂的 `❯` 才會落在預設的範圍之外。
const DRAFT_ROWS: usize = 30;

fn tall_draft() -> Vec<String> {
    (1..=DRAFT_ROWS).map(|n| format!("fixture restored row {n}")).collect()
}

/// 真畫面：框頂的 `❯` 在倒數 24 列之外，框裡的字照樣讀得出來、判成有草稿（不是讀不到）。
#[test]
fn a_real_tall_restored_draft_reads_as_a_draft() {
    let lines: Vec<&str> = TALL_DRAFT.lines().collect();
    let marker = lines.iter().rposition(|l| delivery::strip_ansi(l).starts_with('❯')).unwrap();
    assert!(lines.len() - marker > COMPOSER_TAIL, "fixture 的框頂要真的在預設範圍之外（不然測不到什麼）");
    assert_eq!(box_state("claude", TALL_DRAFT), BoxState::NonEmpty);
    // `composer_text` 讀到第一個空白列為止（既有行為）：這裡要的是「讀得出草稿」，不是整段。
    let text = composer_text("claude", TALL_DRAFT).unwrap();
    assert!(text.starts_with("fix\n這是一段合成的多行貼上"), "{text}");
}

/// 框頂那列不是 `❯`（例如對話框、或框裡有一整列 `─` 把框切斷）：不猜，照舊判讀不到。
#[test]
fn a_tall_frame_without_the_prompt_mark_on_top_stays_unreadable() {
    let lines: Vec<&str> = TALL_DRAFT.lines().collect();
    let marker = lines.iter().rposition(|l| delivery::strip_ansi(l).starts_with('❯')).unwrap();
    let mut cut: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    cut[marker] = cut[marker].replacen('❯', " ", 1);
    let cut = cut.join("\n");
    assert_eq!(box_state("claude", &cut), BoxState::Unready);
    assert_eq!(composer_text("claude", &cut), None);
}

struct Fx {
    env: tt::Env,
    bot_id: String,
    run_id: String,
    conv: String,
}

/// 一顆 claude bot，run 綁著會反應的 pane（`pane-a`），框裡是 `composer` 那幾列。
async fn claude_with(composer: Vec<String>) -> Fx {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,'abort-bot','claude','[]',0,0,'tok',?)",
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
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
         VALUES (?,?,'running','working','ws-1','pane-a','abort-bot','test',1,?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    env.herdr.live_pane("pane-a", tt::LivePane { composer, width: Some(120), ..Default::default() });
    Fx { env, bot_id, run_id, conv }
}

impl Fx {
    fn cleared(&self) -> bool {
        self.env.herdr.calls_to("pane.send_keys").iter().any(|c| c["keys"] == json!(["ctrl+c"]))
    }
    async fn notes(&self, conv: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'system' ORDER BY created_at, id")
            .bind(conv)
            .fetch_all(&self.env.app.db)
            .await
            .unwrap()
    }
    async fn unknown_turn(&self, conv: &str) -> String {
        let id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','failed','unknown',?)")
            .bind(&id)
            .bind(conv)
            .bind(db::now())
            .execute(&self.env.app.db)
            .await
            .unwrap();
        id
    }
}

async fn bind_turn_prompt(f: &Fx, turn_id: &str, prompt: &str) {
    sqlx::query("UPDATE turns SET prompt_text=? WHERE id=?")
        .bind(prompt)
        .bind(turn_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
}

/// 根因：打斷之後 claude 放回框裡的長 prompt 要清掉——以前框頂出了範圍就判成空框、不按，框從此卡著那段字。
#[tokio::test]
async fn a_force_abort_clears_a_restored_prompt_taller_than_the_default_tail() {
    let draft = tall_draft();
    let f = claude_with(draft.clone()).await;
    let turn = run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    bind_turn_prompt(&f, &turn, &draft.join("\n")).await;
    assert_eq!(f.env.herdr.pane("pane-a").unwrap().composer.len(), DRAFT_ROWS, "中止前：框裡是放回來的那段");

    abort_turns(&f.env.app, &f.bot_id).await.unwrap();

    assert!(f.cleared(), "放回框裡的那段要按 ctrl+c 清掉：{:?}", f.env.herdr.calls_to("pane.send_keys"));
    let pane = f.env.herdr.pane("pane-a").unwrap();
    assert!(pane.composer.is_empty());
    assert_eq!(box_state("claude", &pane.render()), BoxState::Empty, "中止之後框讀得到、是空的，下一則送得進去");
}

/// 網頁的 Esc（`interrupt_turn`）走同一步清框。
#[tokio::test]
async fn an_interrupt_clears_a_restored_prompt_taller_than_the_default_tail() {
    let draft = tall_draft();
    let f = claude_with(draft.clone()).await;
    let turn = run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    bind_turn_prompt(&f, &turn, &draft.join("\n")).await;
    let _ = interrupt_turn(&f.env.app, &f.bot_id, None).await;
    assert!(f.cleared(), "{:?}", f.env.herdr.calls_to("pane.send_keys"));
    assert!(f.env.herdr.pane("pane-a").unwrap().composer.is_empty());
}

/// A new draft typed during the 400 ms settle belongs to the user and survives the abort cleanup.
#[tokio::test]
async fn a_force_abort_preserves_a_new_draft_typed_during_the_settle_window() {
    let f = claude_with(vec!["aborted prompt A".into()]).await;
    let turn = run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    bind_turn_prompt(&f, &turn, "aborted prompt A").await;
    let live = f.env.herdr.live.clone();
    let edit = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let mut live = live.lock().unwrap();
        let pane = live.get_mut("pane-a").unwrap();
        pane.composer = vec!["user draft B".into()];
        pane.revision += 1;
    });

    abort_turns(&f.env.app, &f.bot_id).await.unwrap();
    edit.await.unwrap();

    assert!(!f.cleared(), "new user draft must receive no Ctrl+C: {:?}", f.env.herdr.calls_to("pane.send_keys"));
    assert_eq!(f.env.herdr.pane("pane-a").unwrap().composer, ["user draft B"]);
}

/// The composer can change after the candidate read; a final revision/content check must catch it.
#[tokio::test]
async fn a_force_abort_rechecks_the_composer_before_clearing_it() {
    let f = claude_with(vec!["aborted prompt A".into()]).await;
    let turn = run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    bind_turn_prompt(&f, &turn, "aborted prompt A").await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("abort_before_clear_key", &f.bot_id, move || async move {
        let mut live = live.lock().unwrap();
        let pane = live.get_mut("pane-a").unwrap();
        pane.composer = vec!["user draft B".into()];
        pane.revision += 1;
    });

    abort_turns(&f.env.app, &f.bot_id).await.unwrap();

    assert!(!f.cleared(), "changed composer must receive no Ctrl+C: {:?}", f.env.herdr.calls_to("pane.send_keys"));
    assert_eq!(f.env.herdr.pane("pane-a").unwrap().composer, ["user draft B"]);
}

/// An unchanged string still loses authority if the pane revision changed after the candidate read.
#[tokio::test]
async fn a_force_abort_rejects_a_revision_change_with_the_same_composer_text() {
    let f = claude_with(vec!["aborted prompt A".into()]).await;
    let turn = run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    bind_turn_prompt(&f, &turn, "aborted prompt A").await;
    let live = f.env.herdr.live.clone();
    super::super::race_point::arm("abort_before_clear_key", &f.bot_id, move || async move {
        let mut live = live.lock().unwrap();
        let pane = live.get_mut("pane-a").unwrap();
        pane.revision = pane.revision.max(1) + 1;
    });

    abort_turns(&f.env.app, &f.bot_id).await.unwrap();

    assert!(!f.cleared(), "a revised pane must receive no Ctrl+C: {:?}", f.env.herdr.calls_to("pane.send_keys"));
    assert_eq!(f.env.herdr.pane("pane-a").unwrap().composer, ["aborted prompt A"]);
}

/// No composer text is authorized when the aborted turn has no known restored prompt.
#[tokio::test]
async fn a_force_abort_leaves_an_empty_composer_alone() {
    let f = claude_with(Vec::new()).await;
    let turn = run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    bind_turn_prompt(&f, &turn, "aborted prompt A").await;

    abort_turns(&f.env.app, &f.bot_id).await.unwrap();

    assert!(!f.cleared(), "empty composer must receive no key");
}

/// A replacement run and pane during the settle window are outside this abort's authority.
#[tokio::test]
async fn a_force_abort_does_not_clear_after_its_run_and_pane_are_replaced() {
    let f = claude_with(vec!["aborted prompt A".into()]).await;
    let turn = run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    bind_turn_prompt(&f, &turn, "aborted prompt A").await;
    let app = f.env.app.clone();
    let bot_id = f.bot_id.clone();
    let old_run = f.run_id.clone();
    let live = f.env.herdr.live.clone();
    // Land the replacement inside the settle window at a fixed point: a timed task loses to the
    // 400 ms settle on a loaded host and lets the old binding authorize the key (#781).
    super::super::race_point::arm("abort_after_settle", &f.bot_id, move || async move {
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?")
            .bind(old_run)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (? ,?,'running','idle','ws-1','pane-b','abort-bot','test',1,?)",
        )
        .bind(db::ulid())
        .bind(bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        live.lock().unwrap().insert("pane-b".into(), tt::LivePane::default());
    });

    abort_turns(&f.env.app, &f.bot_id).await.unwrap();

    assert!(!f.cleared(), "replacement run must receive no key: {:?}", f.env.herdr.calls_to("pane.send_keys"));
    assert_eq!(f.env.herdr.pane("pane-a").unwrap().composer, ["aborted prompt A"]);
}

/// Even the same run and pane lose cleanup authority when their native session changes.
#[tokio::test]
async fn a_force_abort_does_not_clear_after_its_native_session_changes() {
    let f = claude_with(vec!["aborted prompt A".into()]).await;
    let turn = run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    bind_turn_prompt(&f, &turn, "aborted prompt A").await;
    sqlx::query("UPDATE runs SET native_session_id='session-A' WHERE id=?")
        .bind(&f.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let app = f.env.app.clone();
    let run_id = f.run_id.clone();
    // The session changes after the candidate read matched: only the re-check right before Ctrl+C
    // can catch it. A timed task raced the settle sleep and lost under load (#781).
    super::super::race_point::arm("abort_before_clear_key", &f.bot_id, move || async move {
        sqlx::query("UPDATE runs SET native_session_id='session-B' WHERE id=?")
            .bind(run_id)
            .execute(&app.db)
            .await
            .unwrap();
    });

    abort_turns(&f.env.app, &f.bot_id).await.unwrap();

    assert!(!f.cleared(), "new native session must receive no key: {:?}", f.env.herdr.calls_to("pane.send_keys"));
    assert_eq!(f.env.herdr.pane("pane-a").unwrap().composer, ["aborted prompt A"]);
}

#[test]
fn restored_prompt_identity_preserves_word_boundaries_and_only_allows_known_tab_loss() {
    assert!(same_restored_prompt("echo a b", "echo a b"));
    assert!(!same_restored_prompt("echo\n  a b", "echo a b"), "hard line breaks remain part of the prompt");
    assert!(!same_restored_prompt("echo ab", "echo a b"), "ordinary spaces separate tokens");
    assert!(same_restored_prompt("進貨單每張", "進貨單\t每張"), "the Claude TUI is known to drop a literal Tab");
    assert!(!same_restored_prompt("進貨單每張", "進貨單 每張"), "ordinary spaces are not dropped");
}

/// 止血：框真的卡著一段高過預設範圍的字（不管怎麼來的），送出回的是 `composer_busy` 帶草稿與動作，
/// 網頁能清掉或送出——不是一直 `composer_unreadable`、什麼都不能做。
#[tokio::test]
async fn a_tall_draft_is_a_busy_box_with_actions_not_an_unreadable_one() {
    let f = claude_with(tall_draft()).await;
    sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
    let r = prompt_from_api(&f.env.app, &f.bot_id, "我自己要送的", "c-581", &[], RelaySrc::default(), false, None).await;
    let body = match r {
        Err(LcError::Conflict(v)) => v,
        other => panic!("expected a 409, got {:?}", other.map(|o| o.delivery)),
    };
    assert_eq!(body["reason"], "composer_busy", "{body}");
    assert_eq!(body["draft_actions"], json!(["submit", "clear"]), "{body}");
    assert!(body["draft"].as_str().unwrap().starts_with("fixture restored row 1\nfixture restored row 2\n"), "{body}");
}

/// 一次收掉好幾筆：同一段對話只留一則說明、帶筆數（wits-pro 一次收 5 筆，以前連著 5 則一樣的話）。
#[tokio::test]
async fn a_force_abort_of_many_turns_leaves_one_note_with_the_count() {
    let f = claude_with(Vec::new()).await;
    run_state::a_turn(&f.env.app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    for _ in 0..4 {
        f.unknown_turn(&f.conv).await;
    }
    let out = abort_turns(&f.env.app, &f.bot_id).await.unwrap();
    assert_eq!(out["aborted"].as_array().unwrap().len(), 5, "{out}");
    assert_eq!(f.notes(&f.conv).await, ["回合已由使用者強制中止（共 5 筆）"]);
}

/// 沒有在飛的那一筆也一樣：只留一則；只收一筆時照舊不帶筆數。
#[tokio::test]
async fn leftover_unknown_turns_get_one_note() {
    let f = claude_with(Vec::new()).await;
    for _ in 0..3 {
        f.unknown_turn(&f.conv).await;
    }
    let out = abort_turns(&f.env.app, &f.bot_id).await.unwrap();
    assert_eq!(out["aborted"].as_array().unwrap().len(), 3, "{out}");
    assert_eq!(f.notes(&f.conv).await, ["回合已由使用者強制中止（共 3 筆）"]);

    f.unknown_turn(&f.conv).await;
    abort_turns(&f.env.app, &f.bot_id).await.unwrap();
    assert_eq!(f.notes(&f.conv).await, ["回合已由使用者強制中止（共 3 筆）", "回合已由使用者強制中止"]);
}
