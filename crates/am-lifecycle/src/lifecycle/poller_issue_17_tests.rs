use super::*;
use crate::testing as tt;

struct Fixture {
    env: tt::Env,
    bot_id: String,
    conversation_id: String,
    run_id: String,
    turn_id: String,
}

async fn fixture(kind: &str, screen: &str) -> Fixture {
    let env = tt::env().await;
    let bot_id = db::ulid();
    sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?, ?, 'tok', ?)")
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind("issue-17")
        .bind(kind)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
    let conversation_id = db::conversation_id(&env.app.db, &bot_id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-17','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(&run_id)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
             VALUES (?,?,?,'user','Reply with PONG','web',?)",
    )
    .bind(db::ulid())
    .bind(&conversation_id)
    .bind(&turn_id)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    env.herdr.set_screen("pane-17", screen);
    Fixture { env, bot_id, conversation_id, run_id, turn_id }
}

/// 補 Enter 的看門狗（「字還在框裡就再按一次 Enter」）不能把 claude 2.1.280 的 dim「建議下一句」當成我們沒送出的字：
/// 建議句常常就是剛送過的那一句（2026-09-23 AM-2-M 框裡是 09-20 送過的舊 prompt）。真的打在框裡的字照樣補 Enter。
#[tokio::test]
async fn the_enter_nudge_ignores_a_dim_suggestion_that_repeats_the_prompt() {
    const RULE: &str = "────────────────────────";
    let frame = |row: &str| format!("⏺ PONG\n\u{1b}[0m\u{1b}[38;2;136;136;136m{RULE}\u{1b}[0m\n{row}\n\u{1b}[0m\u{1b}[38;2;136;136;136m{RULE}\u{1b}[0m\n  status\n");
    let sent = vec!["Reply with PONG".to_string()];
    let ghost = frame("❯\u{a0}\u{1b}[0m\u{1b}[2mReply with PONG\u{1b}[0m");
    let f = fixture("claude", &ghost).await;
    assert!(!nudge_unsent_prompt(&f.env.app, &f.run_id, &f.turn_id, &sent).await.unwrap(), "建議句不是沒送出的字");
    assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_keys"), "不按 Enter");

    let typed = frame("❯\u{a0}Reply with PONG");
    let f = fixture("claude", &typed).await;
    assert!(nudge_unsent_prompt(&f.env.app, &f.run_id, &f.turn_id, &sent).await.unwrap(), "真的留在框裡的字要補 Enter");
}

async fn event_kinds(mut rx: tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Vec<String> {
    let mut kinds = Vec::new();
    while kinds.len() < 2 {
        kinds.push(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap().kind);
    }
    kinds
}

async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
    sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(id)
        .fetch_one(&app.db)
        .await
        .unwrap()
}

fn count(f: &Fixture, method: &str) -> usize {
    f.env.herdr.methods().iter().filter(|m| *m == method).count()
}

async fn run_and_bot(f: &Fixture) -> (db::Run, db::Bot) {
    let app = &f.env.app;
    (db::run(&app.db, &f.run_id).await.unwrap().unwrap(), db::bot(&app.db, &f.bot_id).await.unwrap().unwrap())
}

/// #1454：「太窄」訊息的欄數要用 `pane_id` 問（`pane.layout` 不認 `workspace_id`，回的是使用者正在看的那個分頁）。
/// 一顆 bot 的 pane 在自己的分頁（workspace `w7`），正式環境要拿到它的寬度；替身只認 `pane_id`，所以未修時這裡回 `None`。
#[tokio::test]
async fn the_too_narrow_message_reads_the_width_by_pane_id() {
    let f = fixture("claude", "").await;
    sqlx::query("UPDATE runs SET pane_id = 'w7:pNarrow', workspace_id = 'w7' WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
    f.env.herdr.live_pane("w7:pNarrow", crate::testing::LivePane { width: Some(24), ..Default::default() });
    let app = f.env.app.clone();
    let (run, _) = run_and_bot(&f).await;
    assert_eq!(pane_columns(&app, &run).await, Some(24));
    let layouts = f.env.herdr.calls_to("pane.layout");
    assert!(!layouts.is_empty(), "要真的問過 pane.layout");
    assert!(layouts.iter().all(|p| p["pane_id"] == "w7:pNarrow" && p.get("workspace_id").is_none()), "{layouts:?}");
}

/// #1454 的對照：替身對這顆 pane 沒有版面資料：欄數拿不到就是 `None`（best effort，訊息照出、只少「目前 N 欄」）。
#[tokio::test]
async fn no_width_when_herdr_has_no_layout_for_the_pane() {
    let f = fixture("claude", "").await;
    sqlx::query("UPDATE runs SET pane_id = 'w7:pNarrow', workspace_id = 'w7' WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
    f.env.herdr.live_pane("w7:pNarrow", crate::testing::LivePane::default());
    let app = f.env.app.clone();
    let (run, _) = run_and_bot(&f).await;
    assert_eq!(pane_columns(&app, &run).await, None);
}

fn live(f: &Fixture, pane: crate::testing::LivePane) {
    f.env.herdr.live_pane("pane-17", pane);
}

fn wide() -> crate::testing::LivePane {
    crate::testing::LivePane { width: Some(120), ..Default::default() }
}

fn not(reason: &'static str, retry: bool) -> Delivered {
    Delivered::NotAttempted { reason, retry }
}

/// A claude run bound to a session whose transcript the live pane writes on Enter.
async fn with_transcript(f: &Fixture, pane: crate::testing::LivePane) -> std::path::PathBuf {
    let config_dir = f.env.dir.join("claude-config");
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
        .bind(serde_json::json!({"CLAUDE_CONFIG_DIR": config_dir.to_string_lossy()}).to_string())
        .bind(&f.bot_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let transcript_dir = config_dir.join("projects/-test");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let t = transcript_dir.join(format!("session-{}.jsonl", db::ulid()));
    std::fs::write(&t, "").unwrap();
    sqlx::query("UPDATE runs SET native_session_id = 'sess-1', transcript_path = ? WHERE id = ?")
        .bind(t.to_str().unwrap())
        .bind(&f.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    live(f, crate::testing::LivePane { transcript_file: Some(t.clone()), ..pane });
    t
}

/// #1483：送出後第一次 session DB 讀取短暫失敗，不應回報 session_changed 而丟棄已可驗證的 transcript 證據。
#[tokio::test]
async fn issue_1483_delivery_retries_after_a_transient_session_read_error() {
    let f = fixture("claude", "").await;
    with_transcript(&f, wide()).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    super::super::delivery::fail_next_session_read_for_test(&f.run_id);

    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "第一行\n第二行", false, false).await.unwrap(), Delivered::Submitted);
}

#[tokio::test]
async fn a_short_line_is_proven_by_its_echo_row_on_a_pane_of_known_width() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    let text = "加一個功能除了按 SKU 之外";
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted);
    let pane = f.env.herdr.pane("pane-17").unwrap();
    assert!(pane.composer.is_empty());
    assert_eq!(pane.transcript.iter().filter(|l| l.contains("SKU")).count(), 1, "只送一次");
    assert_eq!(count(&f, "pane.send_text"), 1);
}

/// 極窄 pane＋很長的單行：有 transcript 就用 transcript 證明，不會因為畫面折行而永遠 unknown（第七輪 #3）。
#[tokio::test]
async fn a_long_line_on_a_narrow_pane_is_proven_by_the_transcript() {
    let f = fixture("claude", "").await;
    with_transcript(&f, crate::testing::LivePane { width: Some(20), ..Default::default() }).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    let text = "please rewrite the offline quote importer so that it is at least three times faster";
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted);
    assert_eq!(count(&f, "pane.send_text"), 1);
}

#[tokio::test]
async fn a_multi_line_prompt_is_proven_by_the_transcript_and_sent_once() {
    let f = fixture("claude", "").await;
    with_transcript(&f, wide()).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    let text = "第一行：先看報告\n第二行：再改程式\n第三行：最後回報";
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted);
    let pane = f.env.herdr.pane("pane-17").unwrap();
    assert_eq!(pane.transcript.iter().filter(|l| l.starts_with('❯')).count(), 1);
    assert_eq!(count(&f, "pane.send_text"), 1);
}

/// 規劃完、打字前證據檔被刪掉：一個字都沒打，回 NotAttempted(transcript_unreadable)（sol 第十一輪）。
#[tokio::test]
async fn an_evidence_file_removed_after_planning_is_not_attempted() {
    let f = fixture("claude", "").await;
    let t = with_transcript(&f, wide()).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let plan = plan_delivery(&app, &client, &run, &bot, "第一行\n第二行", false, false).await.unwrap().unwrap();
    std::fs::remove_file(&t).unwrap();
    let out = execute_delivery(&app, &client, &run, &bot, "第一行\n第二行", plan).await.unwrap();
    assert_eq!(out, not("transcript_unreadable", true));
    assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0);
}

/// herdr 收了 `format: ansi` 卻回 `format: text`：照樣用，但會（節流地）警告一次。
#[tokio::test]
async fn a_plain_answer_to_a_styled_read_is_warned_about() {
    let f = fixture("claude", "").await;
    live(&f, wide());
    f.env.herdr.ignore_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
    assert!(!should_warn_plain_for_ansi("pane-17"), "the read already used this pane's warning");
}

/// session 在送出途中換掉：transcript 證據不再適用，回 Unproven（不是 Submitted、也不是沒送）。
#[tokio::test]
async fn a_session_change_mid_delivery_is_unproven() {
    let f = fixture("claude", "").await;
    with_transcript(&f, wide()).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let text = "第一行\n第二行";
    let plan = plan_delivery(&app, &client, &run, &bot, text, false, false).await.unwrap().unwrap();
    sqlx::query("UPDATE runs SET native_session_id = 'sess-2' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
    assert_eq!(execute_delivery(&app, &client, &run, &bot, text, plan).await.unwrap(), Delivered::Unproven("session_changed"));
}

/// 沒有無損證據（grok 多行）：照樣打字送出一次，回 Unverified。
#[tokio::test]
async fn a_prompt_with_no_lossless_proof_is_typed_once_and_unverified() {
    let f = fixture("grok", "").await;
    live(&f, crate::testing::LivePane { boxed: true, ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    let out = deliver_prompt(&app, &client, &run, &bot, "第一行\n第二行", false, false).await.unwrap();
    assert_eq!(out, Delivered::Unverified);
    assert_eq!(count(&f, "pane.send_text"), 1);
    let pane = f.env.herdr.pane("pane-17").unwrap();
    assert_eq!(pane.transcript.iter().filter(|l| l.starts_with('❯')).count(), 1);
}

/// 沒有證據的那條路，TUI 晚一幀才把貼上的字畫出來：多等一個 settle 再看，照常按 Enter 送出，
/// 不是判 `nothing_typed` 把字留在框裡（review2 deliv 上一輪 #2 的副作用）。只貼一次。
#[tokio::test]
async fn a_late_redraw_on_an_unverified_run_is_still_submitted_not_left_in_the_box() {
    let f = fixture("grok", "").await;
    // 第一次讀的時候框還是空的：字「晚一幀」才出現。
    live(&f, crate::testing::LivePane { boxed: true, swallow_text: true, ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let (calls, panes) = (f.env.herdr.calls.clone(), f.env.herdr.live.clone());
    let redraw = tokio::spawn(async move {
        // 貼上之後的第一次讀畫面過去了，才把字畫進框裡。
        loop {
            let m: Vec<String> = calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect();
            if let Some(pos) = m.iter().position(|x| x == "pane.send_text") {
                if m[pos..].iter().any(|x| x == "pane.read") {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut live = panes.lock().unwrap();
        let p = live.get_mut("pane-17").unwrap();
        p.swallow_text = false;
        p.composer = vec!["第一行".into(), "第二行".into()];
    });
    let out = deliver_prompt(&app, &client, &run, &bot, "第一行\n第二行", false, false).await.unwrap();
    redraw.await.unwrap();
    assert_eq!(out, Delivered::Unverified, "晚一幀的字照樣送出");
    assert_eq!(count(&f, "pane.send_text"), 1, "沒有證據就不重貼");
    assert!(count(&f, "pane.send_keys") >= 1, "有按 Enter");
    assert!(f.env.herdr.pane("pane-17").unwrap().composer.is_empty(), "字沒有留在框裡");
}

/// 打過字、證不明的 turn 絕不自動重送（`auto_resend = 0`）：它很可能已經被收下了。
#[tokio::test]
async fn a_turn_marked_no_auto_resend_is_never_resent() {
    let f = fixture("grok", "").await;
    live(&f, crate::testing::LivePane { boxed: true, ..wide() });
    let app = f.env.app.clone();
    sqlx::query("UPDATE turns SET delivery_verified = 0, auto_resend = 0 WHERE id = ?")
        .bind(&f.turn_id)
        .execute(&app.db)
        .await
        .unwrap();
    let sent = vec!["Reply with PONG".to_string()];
    assert!(!resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent());
    assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0);
}

/// 有無損證據證明已經送進 session 的 turn，畫面上找不到回音（長段貼上被摺起來）也不自動重送（review3 c3 M4）。
#[tokio::test]
async fn a_delivery_proven_by_evidence_is_never_resent_even_if_the_screen_lost_its_echo() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["❯ [Pasted text #1 +20 lines]".into()], ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    mark_delivery(&app, &f.turn_id, Delivered::Submitted.record().unwrap(), &db::now()).await.unwrap();

    let sent = vec!["Reply with PONG".to_string()];
    assert!(prompt_never_reached_screen("claude", &f.env.herdr.pane("pane-17").unwrap().render(), &sent), "畫面上真的找不到");
    assert!(!resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent());
    assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0, "有證據就不再打一次");
}

/// 沒有證據不等於不能重送（AGM 2026-09-16）：`agent.prompt` 那條路記成未驗證，但畫面證明它沒進去時照樣重送。
#[tokio::test]
async fn an_unverified_but_resendable_turn_is_still_resent() {
    let f = fixture("grok", "").await;
    live(&f, crate::testing::LivePane { boxed: true, ..wide() });
    let app = f.env.app.clone();
    sqlx::query("UPDATE turns SET delivery_verified = 0, auto_resend = 1 WHERE id = ?")
        .bind(&f.turn_id)
        .execute(&app.db)
        .await
        .unwrap();
    let sent = vec!["Reply with PONG".to_string()];
    assert!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent());
    assert_eq!(count(&f, "pane.send_text"), 1);
}

/// issue #1471：hook 已經收掉回合（`completed`＋native turn id）之後才補寫的送達結果（欠著的帳、慢一步的補送）
/// 不能把 `delivery_verified` 寫回 0；還沒被 hook 收掉的照寫。
#[tokio::test]
async fn a_late_delivery_write_does_not_unverify_a_turn_a_hook_already_completed() {
    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let verified = || async { sqlx::query_scalar::<_, i64>("SELECT delivery_verified FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&app.db).await.unwrap() };
    mark_delivery(&app, &f.turn_id, Delivered::Unverified.record().unwrap(), &db::now()).await.unwrap();
    assert_eq!(verified().await, 0, "還在飛、沒有 hook 證據：照寫成未驗證");

    sqlx::query("UPDATE turns SET status='completed', native_turn_id='n1' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE turns SET delivery_verified = 1 WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
    mark_delivery(&app, &f.turn_id, Delivered::Unverified.record().unwrap(), &db::now()).await.unwrap();
    assert_eq!(verified().await, 1, "hook 已經收尾：晚到的送達結果不降回 0");
}

/// codex 本機：找到它自己的 rollout 就用 rollout 逐字證明多行 prompt。
#[tokio::test]
async fn a_codex_multi_line_prompt_is_proven_by_its_rollout() {
    let f = fixture("codex", "").await;
    let app = f.env.app.clone();
    let home = f.env.dir.join("codex-home");
    let day = home.join("sessions/2026/09/14");
    std::fs::create_dir_all(&day).unwrap();
    let log = day.join("rollout-2026-09-14T10-00-00-sess-codex.jsonl");
    std::fs::write(&log, "").unwrap();
    sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
        .bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string())
        .bind(&f.bot_id)
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET native_session_id = 'sess-codex' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
    // codex 的 composer／回音 marker 是 `› `；mock pane 畫的是 `❯`，所以這裡只看 rollout 證據本身。
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let pane_cols = client.pane_size("pane-17").await.ok().flatten().map(|(w, _)| w);
    let inputs = ProofInputs {
        kind: &bot.kind,
        host_is_local: true,
        hooks: true,
        session_id: run.native_session_id.as_deref(),
        transcript_path: None,
        codex_log: codex_home(&app, &bot).await.and_then(|h| codex_session_log(&h, "sess-codex")),
        waited_for_log: false,
        pane_cols,
    };
    assert_eq!(
        choose_proof(&inputs, "第一行\n第二行"),
        Ok(Proof::Transcript { format: LogFormat::Codex, path: std::fs::canonicalize(&log).unwrap(), session_id: "sess-codex".into() }),
    );
}

/// Enter 被吃掉，spinner 一直在重畫：不能判成功；已經按過鍵，所以是 Unproven。
#[tokio::test]
async fn a_swallowed_enter_is_unproven_even_while_the_spinner_redraws() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { swallow_enter: true, ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "go", false, false).await.unwrap(), Delivered::Unproven("still_in_box"));
    assert!(f.env.herdr.pane("pane-17").unwrap().transcript.is_empty());
}

#[tokio::test]
async fn a_swallowed_paste_is_pasted_once_more_and_then_unproven() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { swallow_text: true, ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
    assert_eq!(out, Delivered::Unproven("nothing_typed"));
    assert_eq!(count(&f, "pane.send_text"), 2);
    assert_eq!(count(&f, "pane.send_keys"), 0);
}

/// 框裡有任何東西（空白第二行、行尾空白草稿、建議句）都不代送，而且零寫入（第七輪 #1）。
/// marker 那一列只有空白**不算**：那是終端留下的 padding（2026-09-23，`/model` 套用後每顆閒置 bot 都 composer_busy）。
#[tokio::test]
async fn anything_in_the_box_is_left_alone_with_zero_writes() {
    let cases: Vec<(Vec<String>, &str)> = vec![
        (vec!["我自己在打的草稿".into()], "草稿"),
        (vec!["".into(), "".into()], "空白第二行"),
        (vec!["draft with trailing spaces  ".into()], "行尾空白草稿"),
        (vec!["Reply with PONG please".into()], "跟要送的一模一樣"),
    ];
    for (composer, why) in cases {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { composer: composer.clone(), ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
        assert_eq!(out, not("composer_busy", true), "{why}");
        assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0, "{why}: 零寫入");
        assert_eq!(f.env.herdr.pane("pane-17").unwrap().composer, composer, "{why}: 框原封不動");
    }
}

/// 輸入框只有 dim 的「建議下一句」（2026-09-14 k8bw2f 的交辦卡 queued）：是空框，照樣送出，建議句不會被一起送。
#[tokio::test]
async fn a_dim_suggested_prompt_is_an_empty_box_and_the_prompt_goes_through() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { suggestion: Some("把 4b 和 4c 補做完".into()), ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
    assert_ne!(out, not("composer_busy", true));
    assert!(!matches!(out, Delivered::NotAttempted { .. }), "{out:?}");
    assert_eq!(count(&f, "pane.send_text"), 1);
    let pane = f.env.herdr.pane("pane-17").unwrap();
    assert!(pane.transcript.iter().any(|l| l.contains("Reply with PONG please")), "{:?}", pane.transcript);
    assert!(!pane.transcript.iter().any(|l| l.contains("補做完")));
}

/// 使用者真的打了跟佔位字一字不差的草稿（mock 讀不到 dim）：不能當空框，零寫入（sol 第九輪 #2）。
#[tokio::test]
async fn a_draft_identical_to_the_placeholder_is_left_alone() {
    let f = fixture("claude", "").await;
    let draft = vec!["Try \"fix lint errors\"".to_string()];
    live(&f, crate::testing::LivePane { composer: draft.clone(), ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
    assert_eq!(out, not("composer_busy", true));
    assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0);
    assert_eq!(f.env.herdr.pane("pane-17").unwrap().composer, draft);
}

/// #782：codex 帳號安全提醒橫幅開著、輸入框是空的。打字的話開頭的 `1` 會被當成「選第 1 項」吃掉：一個鍵都不按（也不按
/// Esc 關橫幅），回可重試的 `codex_security_banner`，對話裡留一則通知；佇列重試再被擋不重複通知。
/// #789：AGM inbox 也收到一則 `ops_alert`（帶 bot id、名稱、原因與處理方式），跟對話通知同樣去重。
#[tokio::test]
async fn a_codex_security_banner_blocks_delivery_without_a_single_key() {
    let screen = include_str!("fixtures/codex-0.159.3-security-setup-banner.ansi");
    let f = fixture("codex", screen).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let notes = || async {
        sqlx::query_scalar::<_, String>("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&f.conversation_id)
            .fetch_all(&app.db)
            .await
            .unwrap()
    };
    let alerts = || async { banner_alerts(&app).await };

    for _ in 0..2 {
        let out = deliver_prompt(&app, &client, &run, &bot, "1. 先看 issue 782", false, true).await.unwrap();
        assert_eq!(out, not("codex_security_banner", true));
    }
    assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0, "不打字、不按 Esc");
    let posted = notes().await;
    assert_eq!(posted.len(), 1, "同一次橫幅只通知一次：{posted:?}");
    assert!(posted[0].contains("安全提醒橫幅") && posted[0].contains("沒有送出"), "{posted:?}");
    let inbox = alerts().await;
    assert_eq!(inbox.len(), 1, "同一次橫幅 inbox 也只推一則：{inbox:?}");
    let (row_bot, state, p) = &inbox[0];
    assert_eq!((row_bot.as_deref(), state.as_str()), (Some(bot.id.as_str()), "pending"));
    assert_eq!(p["source"], "daemon");
    assert_eq!(p["reason"], "codex_security_banner");
    assert_eq!(p["bot_id"], bot.id.as_str());
    assert_eq!(p["bot_name"], bot.name.as_str());
    assert_eq!(p["run_id"], run.id.as_str());
    let action = p["action"].as_str().unwrap_or_default();
    assert!(action.contains("pane") && action.contains("橫幅") && action.contains("重送"), "{p}");
    // 巡檢收、叫醒。
    crate::supervisor::roles::classify(&app.db).await.unwrap();
    let routed: (String, i64) = sqlx::query_as(
        "SELECT role, wake FROM supervisor_inbox WHERE kind='ops_alert' AND event_key LIKE 'ops_alert:daemon:codex_security_banner:%'",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(routed, ("patrol".to_string(), 1));

    // 橫幅被人關掉：照常派送，`1` 開頭的整句原封不動貼進去。
    f.env.herdr.set_screen("pane-17", "› Reply with PONG\n\n• PONG\n\n› \n\n  gpt-6.1-sol default · /tmp/x\n");
    let out = deliver_prompt(&app, &client, &run, &bot, "1. 先看 issue 782", false, true).await.unwrap();
    assert!(!matches!(out, Delivered::NotAttempted { .. }), "{out:?}");
    let sent = f.env.herdr.calls_to("pane.send_text");
    assert_eq!(sent.first().map(|c| c["text"].clone()), Some(json!("1. 先看 issue 782")), "{sent:?}");

    // 關掉之後又出現：再通知一次。
    f.env.herdr.set_screen("pane-17", screen);
    let out = deliver_prompt(&app, &client, &run, &bot, "1. 先看 issue 782", false, true).await.unwrap();
    assert_eq!(out, not("codex_security_banner", true));
    assert_eq!(notes().await.len(), 2);
    let inbox = alerts().await;
    assert_eq!(inbox.len(), 2, "橫幅消失又出現：inbox 再推一則：{inbox:?}");
}

/// AGM inbox 裡 #789 的橫幅事件：`(bot_id 欄, state, payload)`，照寫入順序。
async fn banner_alerts(app: &Arc<App>) -> Vec<(Option<String>, String, Value)> {
    sqlx::query_as::<_, (Option<String>, String, String)>(
        "SELECT bot_id, state, payload_json FROM supervisor_inbox
              WHERE kind='ops_alert' AND event_key LIKE 'ops_alert:daemon:codex_security_banner:%' ORDER BY rowid",
    )
    .fetch_all(&app.db)
    .await
    .unwrap()
    .into_iter()
    .map(|(b, s, p)| (b, s, serde_json::from_str(&p).unwrap()))
    .collect()
}

/// #789：沒有橫幅就照常派送，AGM inbox 不多任何一則。
#[tokio::test]
async fn a_codex_screen_without_the_banner_pushes_no_inbox_event() {
    let f = fixture("codex", "› Reply with PONG\n\n• PONG\n\n› \n\n  gpt-6.1-sol default · /tmp/x\n").await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    let out = deliver_prompt(&app, &client, &run, &bot, "1. 先看 issue 789", false, true).await.unwrap();
    assert!(!matches!(out, Delivered::NotAttempted { .. }), "{out:?}");
    assert!(banner_alerts(&app).await.is_empty());
}

#[tokio::test]
async fn an_agent_without_a_session_binding_is_typed_into_not_prompted() {
    let f = fixture("claude", "").await;
    live(&f, wide());
    f.env.herdr.set_agent("issue-17", "pane-17", false);
    let app = f.env.app.clone();
    assert!(!db::pane_typed(&app.db, &f.run_id).await.unwrap());
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
    assert_eq!(count(&f, "agent.prompt"), 0);
    assert!(db::pane_typed(&app.db, &f.run_id).await.unwrap());
}

#[tokio::test]
async fn an_agent_with_a_session_binding_still_goes_through_agent_prompt() {
    let f = fixture("claude", "").await;
    live(&f, wide());
    f.env.herdr.set_agent("issue-17", "pane-17", true);
    let app = f.env.app.clone();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let _ = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await;
    assert_eq!(count(&f, "agent.prompt"), 1);
    assert_eq!(count(&f, "pane.send_text"), 0);
}

/// `agent.prompt` 走完之後 DB 記的是「沒有證據、但可以重送」，而且沒有用掉重送額度。
#[tokio::test]
async fn the_agent_prompt_route_is_recorded_unverified_but_still_resendable() {
    // 路由本身由 `an_agent_with_a_session_binding_still_goes_through_agent_prompt` 蓋；
    // mock 不實作 agent.prompt，這裡只驗那條路的結果怎麼記。
    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    crate::lifecycle::prompt::mark_delivery(&app, &f.turn_id, Delivered::Handed.record().unwrap(), &db::now()).await.unwrap();
    let (delivery, verified, auto, resends): (String, i64, i64, i64) =
        sqlx::query_as("SELECT delivery, delivery_verified, auto_resend, resend_count FROM turns WHERE id = ?")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!((delivery.as_str(), verified, auto), ("ok", 0, 1), "沒有證據，但重送照舊");
    assert_eq!(resends, 0, "沒有用掉重送額度");
}

/// 打字證不明（grok 多行）：一樣沒有證據，但重送關掉、額度也被用掉（回滾到舊 binary 也不會重打）。
#[tokio::test]
async fn a_typed_but_unprovable_prompt_is_recorded_without_auto_resend() {
    let f = fixture("grok", "").await;
    let app = f.env.app.clone();
    crate::lifecycle::prompt::mark_delivery(&app, &f.turn_id, Delivered::Unverified.record().unwrap(), &db::now()).await.unwrap();
    let (delivery, verified, auto, resends): (String, i64, i64, i64) =
        sqlx::query_as("SELECT delivery, delivery_verified, auto_resend, resend_count FROM turns WHERE id = ?")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!((delivery.as_str(), verified, auto), ("ok", 0, 0));
    assert_eq!(resends, crate::lifecycle::MAX_PROMPT_RESENDS);
}

/// 升級前就存在的列：`auto_resend` 預設 1，行為與今天相同——verified=1 的照樣可重送，
/// 舊的 unverified 列因為當時已把 resend_count 頂到上限，還是不會被重送。
#[tokio::test]
async fn rows_written_before_the_split_keep_todays_behaviour() {
    let f = fixture("grok", "").await;
    live(&f, crate::testing::LivePane { boxed: true, ..wide() });
    let app = f.env.app.clone();
    let auto: i64 = sqlx::query_scalar("SELECT auto_resend FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(auto, 1, "既有列的預設");
    sqlx::query("UPDATE turns SET delivery_verified = 0, resend_count = ? WHERE id = ?")
        .bind(crate::lifecycle::MAX_PROMPT_RESENDS)
        .bind(&f.turn_id)
        .execute(&app.db)
        .await
        .unwrap();
    let sent = vec!["Reply with PONG".to_string()];
    assert!(!resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent(), "舊的 unverified 列照舊不重送");
    assert_eq!(count(&f, "pane.send_text"), 0);
}

/// 打字前讀不到畫面：一個字都沒打，是「稍後再試」，不是錯誤也不是 unknown（sol 第十輪 #1）。
#[tokio::test]
async fn a_pane_that_cannot_be_read_before_typing_is_not_attempted() {
    let f = fixture("claude", "__READ_ERROR__").await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let config_dir = f.env.dir.join("claude-config");
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
        .bind(serde_json::json!({"CLAUDE_CONFIG_DIR": config_dir.to_string_lossy()}).to_string())
        .bind(&f.bot_id)
        .execute(&app.db)
        .await
        .unwrap();
    let transcript_dir = config_dir.join("projects/-test");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let t = transcript_dir.join("t.jsonl");
    std::fs::write(&t, "").unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let run = db::Run { native_session_id: Some("s".into()), transcript_path: Some(t.to_str().unwrap().into()), ..run };
    let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
    assert_eq!(out, not("composer_unreadable", true));
    assert_eq!(count(&f, "pane.send_text"), 0);
}

/// herdr 不認得 `format: ansi`：退回純文字讀法照樣送；純文字讀法下佔位字自然當非空。
#[tokio::test]
async fn a_herdr_without_styled_reads_falls_back_to_plain_reads() {
    let f = fixture("claude", "").await;
    live(&f, wide());
    f.env.herdr.reject_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
    assert_eq!(count(&f, "pane.send_text"), 1);

    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { composer: vec!["Try \"fix lint errors\"".into()], ..wide() });
    f.env.herdr.reject_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), not("composer_busy", true));
    assert_eq!(count(&f, "pane.send_text"), 0);
}

/// `runs.pane_typed` 寫不進去時一個字都還沒打：可重試的 NotAttempted，不是 `Err`（→ unknown，review3 c4 L5）。
#[tokio::test]
async fn nothing_is_typed_when_the_pane_typed_marker_cannot_be_stored() {
    let f = fixture("claude", "").await;
    live(&f, wide());
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    sqlx::query("ALTER TABLE runs RENAME TO runs_real").execute(&app.db).await.unwrap();
    sqlx::query("CREATE VIEW runs AS SELECT * FROM runs_real").execute(&app.db).await.unwrap();

    assert_eq!(
        deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(),
        not("pane_typed_unwritable", true),
    );
    assert_eq!(count(&f, "pane.send_text"), 0);
    assert_eq!(count(&f, "agent.prompt"), 0);
}

#[tokio::test]
async fn the_in_process_memo_keeps_a_pane_on_the_typing_path() {
    let f = fixture("claude", "").await;
    live(&f, wide());
    f.env.herdr.set_agent("issue-17", "pane-17", true);
    let app = f.env.app.clone();
    crate::lifecycle::remember_pane_typed(&f.run_id);
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
    assert_eq!(count(&f, "agent.prompt"), 0);
}

#[tokio::test]
async fn an_unreadable_pane_typed_marker_falls_back_to_typing_not_to_agent_prompt() {
    let f = fixture("claude", "").await;
    live(&f, wide());
    f.env.herdr.set_agent("issue-17", "pane-17", true);
    let app = f.env.app.clone();
    sqlx::query("UPDATE runs SET pane_typed = 'broken' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
    assert_eq!(count(&f, "agent.prompt"), 0);
}

/// #198 同類：讀不到主機就不打字——當成遠端會跳過本機 transcript 這種無損證據，改成盲打。一個字都還沒打，可重試。
#[tokio::test]
async fn a_prompt_whose_host_cannot_be_read_is_not_typed() {
    let f = fixture("claude", "").await;
    let _t = with_transcript(&f, wide()).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
    let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
    sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
    assert_eq!(out, not("host_unreadable", true));
    assert_eq!(count(&f, "pane.send_text"), 0, "一個字都沒打");
}

#[tokio::test]
async fn no_pane_is_not_attempted_and_never_an_agent_prompt() {
    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    sqlx::query("UPDATE runs SET pane_id = NULL WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
    assert_eq!(out, not("no_pane_to_type_into", true));
    assert_eq!(count(&f, "agent.prompt"), 0);
}

#[tokio::test]
async fn two_concurrent_resends_send_the_prompt_exactly_once() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let sent = vec!["Reply with PONG".to_string()];
    let (a, b) = tokio::join!(
        resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent),
        resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent),
    );
    assert_eq!([a, b].iter().filter(|x| x.sent()).count(), 1);
    let pane = f.env.herdr.pane("pane-17").unwrap();
    assert_eq!(pane.transcript.iter().filter(|l| l.contains("Reply with PONG")).count(), 1);
    assert!(!db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await.unwrap());
}

/// 群組訊息的泡泡帶著 @mention，實際送給 bot 的是去掉 mention 的字（`turns.prompt_text`）。
async fn group_turn(f: &Fixture, bubble: &str, delivered: &str) {
    let app = &f.env.app;
    sqlx::query("UPDATE messages SET content = ? WHERE turn_id = ? AND role = 'user'")
        .bind(bubble)
        .bind(&f.turn_id)
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE turns SET prompt_text = ? WHERE id = ?").bind(delivered).bind(&f.turn_id).execute(&app.db).await.unwrap();
}

/// review3 c3 M4：畫面上已經有實際送出的那句（沒有 @mention）時，不能因為拿泡泡原文去搜找不到就重送。
#[tokio::test]
async fn a_group_prompt_on_screen_without_its_mentions_is_not_resent() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["❯ 請跑一次完整的測試並回報".into()], ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    group_turn(&f, "@AM-2-M @AM-3-X 請跑一次完整的測試並回報", "請跑一次完整的測試並回報").await;

    let sent = turn_echo_texts(&app, &f.turn_id).await.unwrap();
    assert_eq!(sent.first().map(String::as_str), Some("請跑一次完整的測試並回報"), "實際送出的字排第一：{sent:?}");
    assert_eq!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await, Resend::Skipped);
    assert_eq!(count(&f, "pane.send_text"), 0, "已經到了，不重送");
}

/// 真的沒到時，重送打的是當初實際送出的字，不是帶著路由語法的泡泡原文。
#[tokio::test]
async fn a_lost_group_prompt_is_resent_as_delivered_without_its_mentions() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    group_turn(&f, "@AM-2-M @AM-3-X 請跑一次完整的測試並回報", "請跑一次完整的測試並回報").await;

    let sent = turn_echo_texts(&app, &f.turn_id).await.unwrap();
    assert!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent());
    let pane = f.env.herdr.pane("pane-17").unwrap();
    assert!(pane.transcript.iter().any(|l| l == "❯ 請跑一次完整的測試並回報"), "{:?}", pane.transcript);
    assert!(!pane.transcript.iter().any(|l| l.contains("@AM-2-M")), "路由語法不能打進 pane：{:?}", pane.transcript);
}

/// 重送在打字前被擋下（證據檔一時讀不到）：額度退回，watchdog 隔一下用退回的額度再試一次，這次送出去
/// （review2 deliv #4：以前退回的額度沒人用得到，回合馬上被判失敗）。
#[cfg(unix)]
#[tokio::test]
async fn a_resend_blocked_before_typing_is_retried_with_the_refunded_budget() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("claude", "").await;
    let t = with_transcript(&f, wide()).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let generation = 4242;
    app.stall_timers.lock().await.insert(f.run_id.clone(), generation);
    let sent = vec!["Reply with PONG please".to_string()];
    std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o000)).unwrap();
    let unlock = {
        let t = t.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
        })
    };
    let out = resend_with_retry(&app, &f.run_id, &f.bot_id, &f.turn_id, &sent, generation, Duration::from_millis(900)).await;
    unlock.await.unwrap();
    assert_eq!(out, Some((Resend::Sent, None)), "第二次用退回的額度送出去");
    assert_eq!(count(&f, "pane.send_text"), 1, "只送了一次");
    assert!(!db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await.unwrap(), "額度這次真的用掉了");
}

/// review3 c3 M5：重送打了字、Enter 被吞，字還在框裡（`Unproven("still_in_box")`）——不是「agent 沒反應」。
/// turn 記成 unknown＋說明，接下來的 `fail_stalled_turn` 不能把它判 failed。
#[tokio::test]
async fn an_unproven_resend_leaves_the_turn_unknown_not_failed() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], swallow_enter: true, ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    app.stall_timers.lock().await.insert(f.run_id.clone(), 11);
    let sent = vec!["Reply with PONG please".to_string()];

    let out = resend_with_retry(&app, &f.run_id, &f.bot_id, &f.turn_id, &sent, 11, Duration::from_millis(1)).await;
    assert_eq!(out, Some((Resend::Unproven, None)));
    assert_eq!(count(&f, "pane.send_text"), 1, "打過一次字");
    let t = turn(&app, &f.turn_id).await;
    assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "unknown"));
    let msg: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(msg.contains("證明不了有送出") && msg.contains("still_in_box") && msg.contains("輸入框"), "{msg}");

    fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false, false, None).await.unwrap();
    assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "送達不明的不判失敗");
}

/// #1483：重送框已空但回音列延遲時，mark_resend_unproven 寫成 unknown 後必須認領 delivery 記下的 Pending。
#[tokio::test]
async fn issue_1483_watchdog_resend_arms_its_late_proof() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane {
        transcript: vec!["⏺ 先前的回覆".into()],
        hide_echo: true,
        no_spinner: true,
        ..wide()
    });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let sent = vec!["Reply with PONG please".to_string()];

    assert_eq!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await, Resend::Unproven);
    assert_eq!(turn(&app, &f.turn_id).await.delivery, "unknown");
    assert!(late_proof::armed_in_tests().lock().unwrap().iter().any(|(t, _)| *t == f.turn_id), "watchdog 寫成 unknown 後要 arm 同一則 Pending");
}

/// #193：重送打過字、證明不了，`delivery='unknown'` 卻寫不進去——以前只記一行 warning，接著照 `delivery='ok'`
/// 判 failed，打過的字可能已經被收下，AGM 重派同一件事。現在欠著、這一輪不判；寫得進去的下一輪補上，不判失敗、不重打。
#[tokio::test]
async fn an_unproven_resend_that_cannot_be_recorded_is_owed_not_failed() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], swallow_enter: true, no_spinner: true, ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    app.stall_timers.lock().await.insert(f.run_id.clone(), 12);
    sqlx::query(
        "CREATE TRIGGER refuse_unknown BEFORE UPDATE OF delivery ON turns WHEN NEW.delivery='unknown'
             BEGIN SELECT RAISE(ABORT, 'injected: cannot record unknown delivery'); END",
    )
    .execute(&app.db)
    .await
    .unwrap();
    let mut st = Stall::default();
    assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 12, &mut st, Duration::from_millis(1)).await, StallRound::Unsure);
    assert_eq!(count(&f, "pane.send_text"), 1, "打過一次字");
    let t = turn(&app, &f.turn_id).await;
    assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"), "記不成 unknown：欠著，不判失敗");

    sqlx::query("DROP TRIGGER refuse_unknown").execute(&app.db).await.unwrap();
    assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 12, &mut st, Duration::from_millis(1)).await, StallRound::Done);
    let t = turn(&app, &f.turn_id).await;
    assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "unknown"), "補上了：送達不明，不判失敗");
    assert_eq!(count(&f, "pane.send_text"), 1, "不重打第二次");
}

#[tokio::test]
async fn stall_write_failure_keeps_watchdog_until_transaction_commits() {
    let f = fixture("claude", "not logged in\n").await;
    let app = f.env.app.clone();
    app.stall_timers.lock().await.insert(f.run_id.clone(), 1487);
    sqlx::query("CREATE TRIGGER refuse_stall_message BEFORE INSERT ON messages WHEN NEW.role='system' BEGIN SELECT RAISE(ABORT, 'injected stall write failure'); END")
        .execute(&app.db).await.unwrap();
    let mut st = Stall { resent: true, ..Stall::default() };
    assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 1487, &mut st, Duration::ZERO).await, StallRound::Unsure);
    assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    assert_eq!(app.stall_timers.lock().await.get(&f.run_id), Some(&1487));
    sqlx::query("DROP TRIGGER refuse_stall_message").execute(&app.db).await.unwrap();
    assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 1487, &mut st, Duration::ZERO).await, StallRound::Done);
    assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'")
        .bind(&f.turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn stall_refund_failure_keeps_watchdog_and_restores_unused_budget() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], no_spinner: true, ..wide() });
    let app = f.env.app.clone();
    app.stall_timers.lock().await.insert(f.run_id.clone(), 1487);
    sqlx::query("CREATE TRIGGER refuse_pane_marker BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'injected before typing'); END")
        .execute(&app.db).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_refund BEFORE UPDATE OF resend_count ON turns WHEN NEW.resend_count < OLD.resend_count BEGIN SELECT RAISE(ABORT, 'injected refund failure'); END")
        .execute(&app.db).await.unwrap();
    let mut st = Stall::default();
    for _ in 0..2 {
        let result = stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 1487, &mut st, Duration::ZERO).await;
        let budget: i64 = sqlx::query_scalar("SELECT resend_count FROM turns WHERE id=?")
            .bind(&f.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(budget, 1, "認領成功，但退款仍欠著");
        assert_eq!(result, StallRound::Unsure);
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
        assert_eq!(count(&f, "pane.send_text"), 0);
        assert_eq!(count(&f, "agent.prompt"), 0);
    }
    sqlx::query("DROP TRIGGER refuse_refund").execute(&app.db).await.unwrap();
    sqlx::query("DROP TRIGGER refuse_pane_marker").execute(&app.db).await.unwrap();
    assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 1487, &mut st, Duration::ZERO).await, StallRound::Done);
    assert_eq!(count(&f, "pane.send_text"), 1);
    assert!(!db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await.unwrap());
}

/// #193：期限那一輪讀不到送了什麼——以前當成什麼都沒送：框裡的字不補 Enter、沒到的不重送，接著判失敗。
/// 現在這一輪不判（`Unsure`）；讀得到的下一輪照常，字還在框裡就補 Enter。
#[tokio::test]
async fn a_stall_round_that_cannot_read_what_was_sent_does_not_fail_the_turn() {
    let rule = "─".repeat(82);
    let screen = format!("⏺ 先前的回覆\n\n✻ Worked for 2s · done 3:34 PM\n{rule}\n❯ Reply with PONG\n{rule}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n");
    let f = fixture("claude", &screen).await;
    let app = f.env.app.clone();
    app.stall_timers.lock().await.insert(f.run_id.clone(), 31);
    sqlx::query("ALTER TABLE messages RENAME TO messages_unreadable").execute(&app.db).await.unwrap();
    // 讀完之後 DB 就恢復：接下來判失敗是寫得進去的。
    let a = app.clone();
    crate::lifecycle::race_point::arm("stall_after_nudge", &f.turn_id, move || async move {
        sqlx::query("ALTER TABLE messages_unreadable RENAME TO messages").execute(&a.db).await.unwrap();
    });
    let mut st = Stall::default();
    assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 31, &mut st, Duration::from_millis(1)).await, StallRound::Unsure);
    assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "判斷不了就不判失敗");
    assert_eq!(count(&f, "pane.send_keys"), 0);

    assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 31, &mut st, Duration::from_millis(1)).await, StallRound::Done);
    assert_eq!(count(&f, "pane.send_keys"), 1, "讀得到了：字還在框裡，補 Enter");
    assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "補了 Enter 還是沒反應：照常判失敗");
    let msg: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(msg.contains("已嘗試補送一次 Enter"), "{msg}");
}

/// #193：重送前讀不到 run／bot、重送額度寫不進去，都是 `Unreadable`，不是 `Skipped`（`Skipped` 之後就判失敗了）。
#[tokio::test]
async fn a_resend_that_cannot_read_or_claim_is_unreadable_not_skipped() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let sent = vec!["Reply with PONG".to_string()];

    sqlx::query("ALTER TABLE bots RENAME TO bots_unreadable").execute(&app.db).await.unwrap();
    assert_eq!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await, Resend::Unreadable, "讀不到 bot");
    sqlx::query("ALTER TABLE bots_unreadable RENAME TO bots").execute(&app.db).await.unwrap();

    sqlx::query("CREATE TRIGGER refuse_claim BEFORE UPDATE OF resend_count ON turns BEGIN SELECT RAISE(ABORT, 'injected: cannot claim a resend'); END")
        .execute(&app.db)
        .await
        .unwrap();
    assert_eq!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await, Resend::Unreadable, "額度寫不進去不是「用完了」");
    assert_eq!(count(&f, "pane.send_text"), 0);
    sqlx::query("DROP TRIGGER refuse_claim").execute(&app.db).await.unwrap();
    assert!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent(), "寫得進去了：照常重送");
}

/// 兩次都被擋：回合照樣判失敗，但訊息說出試過重送、被什麼擋下，不是「agent 沒反應」。
#[cfg(unix)]
#[tokio::test]
async fn a_resend_blocked_twice_says_what_blocked_it() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture("claude", "").await;
    let t = with_transcript(&f, crate::testing::LivePane { no_spinner: true, ..wide() }).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    app.stall_timers.lock().await.insert(f.run_id.clone(), 7);
    let sent = vec!["Reply with PONG please".to_string()];
    std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = resend_with_retry(&app, &f.run_id, &f.bot_id, &f.turn_id, &sent, 7, Duration::from_millis(50)).await;
    std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
    let Some((Resend::Blocked { .. }, Some(why))) = out else { panic!("expected blocked twice, got {out:?}") };
    assert_eq!(count(&f, "pane.send_text"), 0, "一個字都沒打");
    assert!(db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await.unwrap(), "額度還在（兩次都退回了）");

    fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false, false, Some(why)).await.unwrap();
    let msg: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(msg.contains("試著自動重送時被擋下") && msg.contains(why), "{msg}");

    // 計時器被換掉（新的回合、取消）就停手，不動任何東西。
    assert_eq!(resend_with_retry(&app, &f.run_id, &f.bot_id, &f.turn_id, &sent, 8, Duration::from_millis(1)).await, None);
}

#[tokio::test]
async fn stall_commits_system_message_before_turn_updated() {
    let f = fixture("claude", "not logged in\n").await;
    let app = f.env.app.clone();
    let rx = app.subscribe();

    fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false, false, None).await.unwrap();
    let message: (String, String, String) = sqlx::query_as(
        "SELECT role, content, source FROM messages WHERE conversation_id=? AND turn_id=? AND role='system'",
    )
    .bind(&f.conversation_id)
    .bind(&f.turn_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(message.0, "system");
    assert!(message.1.contains("not logged in"));
    assert_eq!(message.2, "system");
    assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
    let kinds = event_kinds(rx).await;
    assert_eq!(kinds, vec!["message_added", "turn_updated"]);
}

/// codex 撞限時終端備援要把回合收成 failed 並說明原因。以前先改成 `completed_fallback` 再改 `failed`，
/// 而 turn 的轉移 trigger 沒有 `completed_fallback -> failed` 這條邊：整個交易被擋掉，回合留在
/// in_flight 直到 stuck watchdog，撞限的說明也沒寫進對話。
#[tokio::test]
async fn a_codex_limit_banner_fails_the_turn_through_the_fallback() {
    let banner = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2026 6:43 PM.";
    let f = fixture("codex", &format!("› Reply with PONG\n\n{banner}\n\n› \n")).await;
    let app = f.env.app.clone();

    assert!(try_fallback(&app, &f.run_id, Some(&f.turn_id)).await.expect("備援不能因為轉移被擋而失敗"), "回合要被收掉");

    assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
    let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
        .bind(&f.turn_id)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert!(notes.iter().any(|n| n.contains("hit your usage limit")), "{notes:?}");
}

/// 開著 `[judge]`、專案在名單裡、Jev 是一個只數呼叫的假端點（回 `is_live_ui`＝0.93）。
async fn judge_on(f: &Fixture) -> Arc<std::sync::Mutex<Vec<Value>>> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(body);
            axum::Json(json!({"model": "jev-1.13.0", "answers": {"is_live_ui": {"type": "noul", "noul": 0.93}}, "usage": {"input_tokens": 300}}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
    let key = tt::track(std::env::temp_dir().join(format!("am-judge-fallback-{}", db::ulid())));
    std::fs::write(&key, "k-test\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (pid, key) = (f.env.project_id.clone(), key.to_string_lossy().into_owned());
    f.env
        .app
        .cfg
        .update(move |c| {
            c.judge.enabled = true;
            c.judge.projects = vec![pid];
            c.judge.key_file = key;
            c.judge.endpoint = url;
            Ok(())
        })
        .await
        .unwrap();
    seen
}

/// 把 fixture 的專案放到遠端主機 `mac_builder`（同一個假 herdr 當那台的 socket）。
async fn on_remote_host(f: &Fixture) {
    let cfg = crate::config::HostCfg { daemon_url: None,
        shared_session: false,
        name: "mac_builder".into(),
        ssh: "mac_builder@100.64.0.2".into(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "test".into(),
        remote_path: String::new(),
    };
    let sock = f.env.dir.join("data").join("herdr.sock");
    f.env.app.hosts.insert_remote_with_client_for_test(cfg, crate::herdr::HerdrClient::new(sock)).await;
    sqlx::query("UPDATE projects SET host='mac_builder' WHERE id=?").bind(&f.env.project_id).execute(&f.env.app.db).await.unwrap();
}

async fn limit_hit_rows(app: &Arc<crate::state::App>) -> Vec<(String, Option<f64>, Option<String>)> {
    sqlx::query_as("SELECT bot_id, jev_is_live_ui, error FROM judge_shadow WHERE regex_verdict='limit_hit'").fetch_all(&app.db).await.unwrap()
}

/// #1467：終端備援把「畫面上的撞限橫幅」收成失敗回合時，也要把這次命中交給 Jev 做第二意見——跟
/// `capture_codex_usage_notices` 同一個命中、同一個閘門。以前備援先下手：它寫了訊息、標了額度，之後
/// `capture_codex_usage_notices` 看到的是「已寫過、回合也不在飛」，直接跳過，帳本一筆都沒有
/// （2026-10-03～10-10 的 12 則橫幅，本機與 mac_builder 都一樣，只有 1 筆 `limit_hit`）。遠端專案走同一條路。
#[tokio::test]
async fn a_banner_the_fallback_closes_a_remote_turn_on_is_put_to_jev_once() {
    let banner = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2026 6:43 PM.";
    let f = fixture("codex", &format!("› Reply with PONG\n\n{banner}\n\n› \n")).await;
    let app = f.env.app.clone();
    on_remote_host(&f).await;
    let seen = judge_on(&f).await;

    assert!(try_fallback(&app, &f.run_id, Some(&f.turn_id)).await.unwrap(), "回合要被收掉");
    assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
    assert!(tt::eventually!(!limit_hit_rows(&app).await.is_empty()), "備援收掉的撞限橫幅要進帳本");
    assert!(tt::eventually!(limit_hit_rows(&app).await.iter().all(|r| r.2.is_none() && r.1.is_some())), "答案要補上，不留 pending");
    let rows = limit_hit_rows(&app).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0.as_str(), rows[0].1), (f.bot_id.as_str(), Some(0.93)));

    // 備援收完之後，畫面掃描照樣會跑一次：橫幅已經寫過、回合也不在飛，不能再問第二次。
    capture_codex_usage_notices(&app, &f.bot_id, &f.run_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(limit_hit_rows(&app).await.len(), 1, "同一則橫幅只問一次");
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(seen.lock().unwrap()[0]["state"]["screen"].as_str().is_some_and(|s| s.contains("hit your usage limit")));
}

/// 關著（預設）什麼都不送、不寫帳本：備援這條路也不例外。
#[tokio::test]
async fn a_banner_the_fallback_closes_is_not_put_to_jev_while_it_is_off() {
    let banner = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2026 6:43 PM.";
    let f = fixture("codex", &format!("› Reply with PONG\n\n{banner}\n\n› \n")).await;
    let app = f.env.app.clone();
    on_remote_host(&f).await;
    let seen = judge_on(&f).await;
    app.cfg.update(|c| {
        c.judge.enabled = false;
        Ok(())
    })
    .await
    .unwrap();

    assert!(try_fallback(&app, &f.run_id, Some(&f.turn_id)).await.unwrap());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(limit_hit_rows(&app).await.is_empty() && seen.lock().unwrap().is_empty());
}

const ORCHESTRATING: &str = "✽ Orchestrating… (13s · ↓ 537 tokens · thinking with high effort)\n  ⎿ Tip: Run /install-github-app to tag @claude right from your Github issues and PRs\n─────────────────────────────────────────────────────────────────────────────\n❯\n─────────────────────────────────────────────────────────────────────────────\n";
const IDLE_BOX: &str = "─────────────────────────────────────────────────────────────────────────────\n❯\n─────────────────────────────────────────────────────────────────────────────\n";

async fn turn_status(f: &Fixture) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&f.env.app.db).await.unwrap()
}

async fn hook_event(f: &Fixture, id: &str, body: &str, at: &str) {
    let _ = am_base::hook_inbox::migrate(&f.env.app.db).await;
    sqlx::query("INSERT INTO hook_events (id, bot_id, provider, source, body_json, received_at) VALUES (?, ?, 'claude', 'http', ?, ?)")
        .bind(id)
        .bind(&f.bot_id)
        .bind(body)
        .bind(at)
        .execute(&f.env.app.db)
        .await
        .unwrap();
}

/// issue #956（pane w9FW:p36）：Claude 2.1.x 長思考時畫面是 `✽ Orchestrating… (13s · ↓ 537 tokens · …)` 加一個空的 `❯`，
/// herdr 偵測報 idle。期限到了不能判 STALL，也不能把同一則 prompt 重送一次。
#[tokio::test]
async fn a_stall_round_does_not_fail_or_resend_while_the_screen_shows_a_live_spinner() {
    let f = fixture("claude", ORCHESTRATING).await;
    let app = f.env.app.clone();
    app.stall_timers().lock().await.insert(f.run_id.clone(), 956);
    let mut st = Stall::default();
    let res = stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 956, &mut st, Duration::from_millis(1)).await;
    assert_eq!(res, StallRound::Done);
    assert_eq!(turn_status(&f).await, "in_flight", "進行中的 spinner 不可判定為 failed");
    assert!(f.env.herdr.calls_to("pane.send_text").is_empty() && f.env.herdr.calls_to("pane.send_keys").is_empty(), "不重送、不按鍵");
}

/// 最後一關（`fail_stalled_turn`）也看：畫面忙就不判失敗；同樣的回合畫面是空框就照舊判失敗（沒有被放過頭）。
#[tokio::test]
async fn the_final_stall_check_spares_a_busy_screen_but_still_fails_an_idle_one() {
    let busy = fixture("claude", ORCHESTRATING).await;
    fail_stalled_turn(&busy.env.app, &busy.run_id, &busy.bot_id, &busy.turn_id, false, false, None).await.unwrap();
    assert_eq!(turn_status(&busy).await, "in_flight");

    let idle = fixture("claude", IDLE_BOX).await;
    fail_stalled_turn(&idle.env.app, &idle.run_id, &idle.bot_id, &idle.turn_id, false, false, None).await.unwrap();
    assert_eq!(turn_status(&idle).await, "failed", "沒有任何活動跡象：照舊判 STALL");
}

/// Codex: fail_stalled_turn 前若 rollout 已有 task_complete，走 try_fallback 補收而不判 failed
#[tokio::test]
async fn codex_stall_recovers_via_rollout_fallback_instead_of_failing() {
    let f = fixture("codex", "› Reply with PONG\n\n• idle\n").await;
    let app = f.env.app.clone();
    let session = "01a18888-1994-7000-8000-000000000099";
    let home = f.env.dir.join("codex");
    let logs = home.join("sessions/2026/10/25");
    std::fs::create_dir_all(&logs).unwrap();
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
        .bind(serde_json::json!({"CODEX_HOME": home}).to_string())
        .bind(&f.bot_id)
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?")
        .bind(session)
        .bind(&f.run_id)
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE turns SET prompt_text='Reply with PONG',delivery='ok',created_at='2026-10-25T10:00:00Z',delivered_at='2026-10-25T10:00:01Z' WHERE id=?")
        .bind(&f.turn_id)
        .execute(&app.db)
        .await
        .unwrap();
    let log_file = logs.join(format!("rollout-2026-10-25T10-00-00-{session}.jsonl"));
    let start = serde_json::json!({
        "type": "event_msg",
        "timestamp": "2026-10-25T10:00:01Z",
        "payload": {"type": "task_started", "turn_id": "native-1"}
    }).to_string();
    let user = serde_json::json!({
        "type": "response_item",
        "timestamp": "2026-10-25T10:00:02Z",
        "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Reply with PONG"}]}
    }).to_string();
    let event = serde_json::json!({
        "type": "event_msg",
        "timestamp": "2026-10-25T10:00:05Z",
        "payload": {"type": "task_complete", "turn_id": "native-1", "last_agent_message": "work finished successfully"}
    }).to_string();
    std::fs::write(&log_file, format!("{start}\n{user}\n{event}\n")).unwrap();

    fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false, false, None).await.unwrap();
    assert_eq!(turn_status(&f).await, "completed", "rollout 已完成時由 fallback 補收為 completed，不判 failed");
    let reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(reply, "work finished successfully");
}

/// issue #1769：遠端 hook 先躺在那台的 spool，要等一輪掃描才進 `hook_events`；本機的 12 秒不能原封不動套給遠端。
/// 遠端的期限至少要等得到一輪 spool 掃描（[`crate::hookrecv::SCAN_EVERY`]），本機不變。
#[test]
fn a_remote_bot_gets_a_deadline_that_outlasts_one_spool_scan() {
    assert_eq!(stall_deadline_secs(false), STALL_SECS, "本機的 hook 直接進 DB，期限不變");
    assert!(stall_deadline_secs(true) >= STALL_SECS + crate::hookrecv::SCAN_EVERY.as_secs(), "遠端要等得到下一輪 spool 掃描");
}

/// 同一個 issue 的另一半：畫面是空框、herdr 沒報 working，但 hook 已經有動靜（`PostToolUse`）＝agent 在處理，不判失敗也不重送。
#[tokio::test]
async fn a_stall_round_does_not_fail_or_resend_when_the_turn_has_hook_activity() {
    let f = fixture("claude", IDLE_BOX).await;
    let app = f.env.app.clone();
    hook_event(&f, "h956", r#"{"hook_event_name":"PostToolUse"}"#, &db::now()).await;
    app.stall_timers().lock().await.insert(f.run_id.clone(), 957);
    let mut st = Stall::default();
    let res = stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 957, &mut st, Duration::from_millis(1)).await;
    assert_eq!(res, StallRound::Done);
    assert_eq!(turn_status(&f).await, "in_flight", "有 hook 活動時不可判定為 failed");
    assert!(f.env.herdr.calls_to("pane.send_text").is_empty() && f.env.herdr.calls_to("pane.send_keys").is_empty(), "不重送、不按鍵");
    fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false, false, None).await.unwrap();
    assert_eq!(turn_status(&f).await, "in_flight", "最後一關也認 hook 活動");
}

/// 什麼不算「有動靜」：回合建立之前的事件、`SessionStart`（自動啟動的 bot 在 prompt 送達前就會送它）、別顆 bot 的事件。
#[tokio::test]
async fn only_hook_events_after_the_turn_began_from_this_bot_count_as_activity() {
    let f = fixture("claude", IDLE_BOX).await;
    let since = db::now();
    let before = "2000-01-01T00:00:00.000Z";
    hook_event(&f, "h-old", r#"{"hook_event_name":"PostToolUse"}"#, before).await;
    hook_event(&f, "h-start", r#"{"hook_event_name":"SessionStart"}"#, &db::now()).await;
    let other = tt::claude_bot(&f.env.app, &f.env.project_id, "someone-else").await;
    sqlx::query("INSERT INTO hook_events (id, bot_id, provider, source, body_json, received_at) VALUES ('h-other', ?, 'claude', 'http', '{\"hook_event_name\":\"PostToolUse\"}', ?)")
        .bind(&other.id)
        .bind(db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
    assert!(!has_hook_activity(&f.env.app.db, &f.bot_id, &since, &f.turn_id).await, "舊事件、SessionStart、別顆 bot 都不算");
    hook_event(&f, "h-tool", r#"{"hook_event_name":"PostToolUse"}"#, &db::now()).await;
    assert!(has_hook_activity(&f.env.app.db, &f.bot_id, &since, &f.turn_id).await);
}

/// issue #1479：herdr 的 styled 讀取把 claude 的灰底回音列補成整列寬，剝掉 ANSI 之後 `❯ <text>` 後面接了一排空白。
/// 遠端 claude 的證據只有這一列，以前逐字相等永遠不成立——回音明明在畫面上，每則都 `not_proven_submitted`。
#[tokio::test]
async fn an_echo_row_padded_to_the_pane_width_is_still_proven() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { pad_echo: true, ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    let text = "Reply with PONG please";
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted);
    let screen = f.env.herdr.pane("pane-17").unwrap().render();
    let echo = screen.lines().find(|l| l.starts_with("❯ Reply")).expect("回音列在畫面上");
    assert!(echo.len() > "❯ ".len() + text.len() + 50, "回音列真的被補白了：{echo:?}");
    assert_eq!(count(&f, "pane.send_text"), 1, "只送一次");
}

#[tokio::test]
async fn a_long_codex_dispatch_retries_the_submit_key_without_retyping() {
    use super::super::delivery::{execute_delivery, Plan, Submit};
    for swallow in [false, true] {
        let f = fixture("codex", "").await;
        let rows: Vec<String> = (0..48).map(|i| if i == 17 { String::new() } else { format!("dispatch task step {i}") }).collect();
        let text = rows.join("\n");
        live(&f, tt::LivePane { codex: true, swallow_enter: swallow, ..wide() });
        let app = f.env.app.clone();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let plan = Plan::Type { pane: "pane-17".into(), proof: Proof::Unverified, submit: Submit::Enter };
        let result = execute_delivery(&app, &client, &run, &bot, &text, plan).await.unwrap();
        assert_eq!(result, if swallow { Delivered::Unproven("still_in_box") } else { Delivered::Unverified });
        assert_eq!(count(&f, "pane.send_keys"), if swallow { 2 } else { 1 }, "at most one additional Enter, even if swallowed again");
        let pasted: String = f.env.herdr.calls_to("pane.send_text").iter()
            .map(|call| call["text"].as_str().unwrap().to_owned()).collect();
        assert_eq!(pasted, text, "paste chunks must spell the dispatch exactly once");
    }
}

/// issue #1571：TUI 把長句折成多列，框裡還是同一則。`confirm_submitted` 的精確比對早退掉補按（不能只靠 `may_press_retry_submit` 的單元測試）。
#[tokio::test]
async fn issue_1571_a_folded_draft_gets_its_retry_enter_through_confirm() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { enter_redraws: Some(vec!["alpha beta".into(), "  gamma delta".into()]), ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    let out = deliver_prompt(&app, &client, &run, &bot, "alpha beta gamma delta", false, false).await.unwrap();
    assert_eq!(out, Delivered::Unproven("still_in_box"));
    assert_eq!(count(&f, "pane.send_keys"), 2, "框裡還是同一則（只是折了行）：補按一次 Enter");
}

/// issue #1666：第一次 Enter 沒生效，使用者把框裡的字改成只差一個行內空白的另一則（`ann a`→`anna`、`a b`→`ab`），
/// 沒有任何 transcript 證據。補按的 Enter 會把使用者剛改的字送出去：只按過一次。
#[tokio::test]
async fn issue_1666_a_whitespace_only_edit_after_the_first_enter_is_never_submitted() {
    for (text, edited) in [("send to ann a", "send to anna"), ("處理 a b", "處理 ab")] {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { enter_redraws: Some(vec![edited.into()]), ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        let out = deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap();
        assert_eq!(out, Delivered::Unproven("composer_changed_or_unreadable"), "{text:?} → {edited:?}");
        assert_eq!(count(&f, "pane.send_keys"), 1, "{text:?} → {edited:?}：框裡是改過的字，只按過一次 Enter");
    }
}

/// issue #1666 的對照：沒被改過的草稿照補按。中文被硬折（接縫沒有字元）、原文自己有換行與縮排，畫面讀回的列逐列對得上。
#[tokio::test]
async fn issue_1666_an_unedited_draft_is_still_pressed_again_however_it_was_wrapped() {
    for (text, rows) in [
        ("這是一則很長的中文草稿沒有空白所以會折行", vec!["這是一則很長的中文草稿", "沒有空白所以會折行"]),
        ("第一行\n    縮排的程式\n結尾", vec!["第一行", "    縮排的程式", "結尾"]),
        ("send to ann a", vec!["send to ann a"]),
    ] {
        let f = fixture("claude", "").await;
        // 多行草稿只認 transcript 證據（沒有 transcript 會停在 `transcript_not_ready`）；Enter 被重畫吃掉，transcript 沒有新的一則。
        with_transcript(&f, crate::testing::LivePane { enter_redraws: Some(rows.iter().map(|r| r.to_string()).collect()), ..wide() }).await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        let out = deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap();
        assert_eq!(out, Delivered::Unproven("still_in_box"), "{text:?}");
        assert_eq!(count(&f, "pane.send_keys"), 2, "{text:?}：框裡還是同一則：補按一次 Enter");
    }
}

/// issue #1577：送出後第一次檢查框裡已是別人的草稿，transcript 其實已經有本則。不按 Enter（絕不補按別人的草稿），
/// 證據直接認成已送出（#1599：證據不分框的狀態看），不必等背景補證。
#[tokio::test]
async fn issue_1577_a_draft_typed_over_the_sent_text_counts_the_transcript_proof() {
    let f = fixture("claude", "").await;
    with_transcript(&f, crate::testing::LivePane { after_enter_composer: Some(vec!["別人的草稿".into()]), ..wide() }).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let text = "Reply with PONG please";

    assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted, "transcript 已經有本則：證據直接認");
    assert_eq!(count(&f, "pane.send_keys"), 1, "框裡是別人的草稿：只按過一次 Enter，絕不補按");
}

/// issue #1599：第一次 Enter 已經把本則寫進 transcript，框裡還是同一則字。證據已經到了＝已送出，不能再補按 Enter（重複送出）。
#[tokio::test]
async fn issue_a_proven_submit_with_the_same_text_still_in_the_box_is_not_pressed_again() {
    let f = fixture("claude", "").await;
    let text = "Reply with PONG please";
    with_transcript(&f, crate::testing::LivePane { after_enter_composer: Some(vec![text.into()]), ..wide() }).await;
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();

    assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted);
    assert_eq!(count(&f, "pane.send_keys"), 1, "transcript 已經有本則：不補按 Enter");
}

/// issue #1600：Enter 沒生效，框裡還找得到本則原文，只是後面被接了別的字。字還在框裡、沒送出，不能記 Pending
/// （之後框被清掉、agent 還在忙，背景會把它升成 unverified，等於宣告一則沒送出的 prompt 送達了）。
#[tokio::test]
async fn issue_b_text_still_in_the_box_with_more_typed_after_it_is_not_left_for_the_late_proof() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { enter_redraws: Some(vec!["Reply with PONG please 還要補充".into()]), ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    let text = "Reply with PONG please";

    assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Unproven("composer_changed_or_unreadable"));
    super::super::late_proof::arm(&app, &f.run_id, &f.bot_id, &f.turn_id);
    let armed = super::super::late_proof::armed_in_tests().lock().unwrap().iter().any(|(t, _)| *t == f.turn_id);
    assert!(!armed, "字還在框裡：不記 Pending");
}

use super::super::late_proof::{self, Look};

/// 打字送出、框空了、回音列看不到（`hide_echo`）：同步窗口判不出，照呼叫端把 turn 寫成 unknown，再認領背景這一筆。
async fn unproven_and_armed(f: &Fixture, pane: crate::testing::LivePane, text: &str) -> late_proof::Pending {
    live(f, pane);
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    assert_eq!(
        deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(),
        Delivered::Unproven("not_proven_submitted")
    );
    sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0, auto_resend=1 WHERE id=?")
        .bind(&f.turn_id)
        .execute(&app.db)
        .await
        .unwrap();
    late_proof::arm(&app, &f.run_id, &f.bot_id, &f.turn_id);
    let armed = late_proof::armed_in_tests().lock().unwrap().iter().find(|(t, _)| *t == f.turn_id).map(|(_, p)| p.clone());
    armed.expect("not_proven_submitted 的那一則被認領了")
}

/// #1483：`same_session` 的一次暫時 DB 讀取錯誤應回 Wait；讀取恢復後仍能用 transcript 證據升級。
#[tokio::test]
async fn issue_1483_late_proof_retries_after_a_transient_session_read_error() {
    let f = fixture("claude", "").await;
    let path = with_transcript(&f, quiet()).await;
    let text = "Reply with PONG please";
    std::fs::write(&path, format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n", serde_json::to_string(text).unwrap())).unwrap();
    let app = f.env.app.clone();
    sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?").bind(&f.turn_id).execute(&app.db).await.unwrap();
    let proof = Proof::Transcript { format: LogFormat::Claude, path, session_id: "sess-1".into() };
    let p = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, text, db::now());
    super::super::delivery::fail_next_session_read_for_test(&f.run_id);

    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Wait, "暫時讀取錯誤不能終止 watcher");
    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Submitted, "讀取恢復後補到證據");
}

/// #1483：transcript 暫時讀不到時不可提早降級成 Unverified，下一輪讀得到就採用原證據。
#[tokio::test]
async fn issue_1483_late_proof_retries_after_a_transient_transcript_read_error() {
    let f = fixture("claude", "").await;
    let path = with_transcript(&f, quiet()).await;
    std::fs::remove_file(&path).unwrap();
    let app = f.env.app.clone();
    sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?").bind(&f.turn_id).execute(&app.db).await.unwrap();
    let text = "Reply with PONG please";
    let proof = Proof::Transcript { format: LogFormat::Claude, path: path.clone(), session_id: "sess-1".into() };
    let p = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, text, db::now());

    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Wait, "暫時 I/O 錯誤不能用較弱結果結案");
    std::fs::write(&path, format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n", serde_json::to_string(text).unwrap())).unwrap();
    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Submitted, "transcript 可讀後照原證據升級");
}

/// #1493：daemon 重啟會清掉進程內 registry；持久化的 proof 基準仍能恢復 unknown turn，並以 CAS 補證而不重送。
#[tokio::test]
async fn issue_1493_startup_restores_a_persisted_late_proof_without_resending() {
    use super::super::delivery::{LogFormat, Proof};

    let f = fixture("claude", "").await;
    let path = with_transcript(&f, quiet()).await;
    let text = "Reply with PONG please";
    let proof = Proof::Transcript { format: LogFormat::Claude, path: path.clone(), session_id: "sess-1".into() };
    let since = "2026-10-12T12:00:00.000Z".to_string();
    let pending = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, text, since);
    late_proof::note(&f.run_id, pending.clone());
    late_proof::persist_pending_at(&f.env.app.db, &f.run_id, &pending, "2026-10-12T12:00:00.000Z").await.unwrap();
    assert_eq!(late_proof::watch(&f.env.app, &f.run_id, &f.bot_id, &f.turn_id, &pending, 0, NEVER).await, None);
    let durable_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM late_proof_pending WHERE turn_id=?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(durable_count, 1, "無結論的 watcher 到期後仍保留基準，讓下次重啟能重試");
    sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?")
        .bind(&f.turn_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();

    let pane_sends_before = f.env.herdr.calls_to("pane.send_text").len();
    late_proof::forget_pending_for_restart_test(&f.run_id);
    let restarted = tt::restart_app(&f.env).await;
    assert_eq!(
        late_proof::recover_at_startup_at(&restarted, "2026-10-12T12:01:00.000Z").await.unwrap(),
        1,
        "有效且仍為 in_flight/unknown 的 durable watcher 要在開機接回"
    );
    let restored = late_proof::armed_in_tests()
        .lock()
        .unwrap()
        .iter()
        .find(|(turn_id, _)| turn_id == &f.turn_id)
        .map(|(_, pending)| pending.clone())
        .expect("開機恢復原 proof 基準");
    assert_eq!((restored.offset, restored.baseline, restored.text.as_str()), (0, 0, text));

    std::fs::write(&path, format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n", serde_json::to_string(text).unwrap())).unwrap();
    assert_eq!(late_proof::look(&restarted, &f.bot_id, &f.turn_id, &restored).await, Look::Submitted, "重啟後仍採用原 transcript baseline");
    assert!(late_proof::promote(&restarted, &f.turn_id, Delivered::Submitted).await.unwrap(), "still-in-flight unknown 的 CAS 可補升級");
    assert_eq!(delivery_row(&f).await.1, "ok");
    assert_eq!(f.env.herdr.calls_to("pane.send_text").len(), pane_sends_before, "恢復補證不能重新送 prompt");
}

/// #1494：durable prompt 只保留到 TTL；run 已結束時下一輪 sweep 就清掉，不等到期。
#[tokio::test]
async fn issue_1494_sweeps_expired_unknown_write_and_gone_run_rows() {
    use super::super::delivery::{LogFormat, Proof};
    use std::time::Instant;

    let expired = fixture("claude", "").await;
    let expired_path = with_transcript(&expired, quiet()).await;
    let proof = Proof::Transcript { format: LogFormat::Claude, path: expired_path, session_id: "sess-1".into() };
    let pending = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, "unknown write", "2026-10-12T12:00:00.000Z".into());
    late_proof::persist_pending_at(&expired.env.app.db, &expired.run_id, &pending, "2026-10-12T12:00:00.000Z").await.unwrap();
    let expiry = "2026-10-12T12:10:01.000Z";
    let (memory_removed, durable_removed) = late_proof::sweep_at(&expired.env.app, expiry, Instant::now()).await.unwrap();
    assert_eq!(memory_removed, 0);
    assert_eq!(durable_removed, 1, "unknown 寫回失敗後的 prompt 到期就刪除");
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM late_proof_pending WHERE run_id=?")
        .bind(&expired.run_id)
        .fetch_one(&expired.env.app.db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);

    let gone = fixture("claude", "").await;
    let gone_path = with_transcript(&gone, quiet()).await;
    let proof = Proof::Transcript { format: LogFormat::Claude, path: gone_path, session_id: "sess-1".into() };
    let pending = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, "stopped run", "2026-10-12T12:00:00.000Z".into());
    late_proof::persist_pending_at(&gone.env.app.db, &gone.run_id, &pending, "2026-10-12T12:00:00.000Z").await.unwrap();
    sqlx::query("UPDATE runs SET state='stopped', ended_at='2026-10-12T12:00:01.000Z' WHERE id=?")
        .bind(&gone.run_id)
        .execute(&gone.env.app.db)
        .await
        .unwrap();
    let (_, durable_removed) = late_proof::sweep_at(&gone.env.app, "2026-10-12T12:00:01.000Z", Instant::now()).await.unwrap();
    assert_eq!(durable_removed, 1, "run 消失就不再保留 prompt");
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM late_proof_pending WHERE run_id=?")
        .bind(&gone.run_id)
        .fetch_one(&gone.env.app.db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

/// 重送中的回合 delivery 仍是 `ok`（寫 unknown 的那一步還沒成功），欠帳期間的清掃不能把補證基準刪掉。
#[tokio::test]
async fn owed_resend_keeps_its_late_proof_through_a_sweep() {
    use super::super::delivery::{LogFormat, Proof};
    use std::time::Instant;

    let f = fixture("claude", "").await;
    let path = with_transcript(&f, quiet()).await;
    let proof = Proof::Transcript { format: LogFormat::Claude, path, session_id: "sess-1".into() };
    let now = "2026-10-12T12:00:00.000Z";
    let pending = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, "Reply with PONG please", now.into());
    late_proof::note(&f.run_id, pending.clone());
    late_proof::persist_pending_at(&f.env.app.db, &f.run_id, &pending, now).await.unwrap();

    late_proof::sweep_at(&f.env.app, "2026-10-12T12:00:05.000Z", Instant::now()).await.unwrap();
    let durable: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM late_proof_pending WHERE turn_id=?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    assert_eq!(durable, 1, "unknown 寫回還欠著、尚未到期：15 秒清掃不能刪掉補證基準");

    sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?")
        .bind(&f.turn_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let app = f.env.app.clone();
    late_proof::arm_after_owed_write(&app, &f.run_id, &f.bot_id, &f.turn_id);
    assert!(late_proof::armed_in_tests().lock().unwrap().iter().any(|(t, _)| t == &f.turn_id), "欠帳補寫成 unknown 後要認領同一則基準");
}

/// 開機恢復只接回 session 仍相符的基準（SPEC：pane 與 session 都相符才恢復）；session 已換，就不為它開 watcher。
#[tokio::test]
async fn startup_does_not_restore_a_baseline_whose_session_changed() {
    use super::super::delivery::{LogFormat, Proof};

    let f = fixture("claude", "").await;
    let path = with_transcript(&f, quiet()).await;
    let proof = Proof::Transcript { format: LogFormat::Claude, path, session_id: "sess-1".into() };
    let now = "2026-10-12T12:00:00.000Z";
    let pending = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, "Reply with PONG please", now.into());
    late_proof::note(&f.run_id, pending.clone());
    late_proof::persist_pending_at(&f.env.app.db, &f.run_id, &pending, now).await.unwrap();
    sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?")
        .bind(&f.turn_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET native_session_id='sess-2' WHERE id=?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();

    late_proof::forget_pending_for_restart_test(&f.run_id);
    let restarted = tt::restart_app(&f.env).await;
    assert_eq!(late_proof::recover_at_startup_at(&restarted, "2026-10-12T12:01:00.000Z").await.unwrap(), 0, "session 已換：不恢復");
    assert!(!late_proof::armed_in_tests().lock().unwrap().iter().any(|(t, _)| t == &f.turn_id), "不能為換過的 session 開 watcher");
}

/// #1654：另起一顆 bot、一個 running run、一個 in-flight 回合（`runs_one_active` 限制每顆 bot 只能有一個 active run）。
async fn seed_in_flight_run(f: &Fixture) -> (String, String) {
    let bot_id = db::ulid();
    sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?, 'claude', 'tok', ?)")
        .bind(&bot_id)
        .bind(&f.env.project_id)
        .bind(format!("seed-{bot_id}"))
        .bind(db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let conversation_id = db::conversation_id(&f.env.app.db, &bot_id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at) VALUES (?,?,'running','idle','pane-17','test',?)")
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let turn_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
    (run_id, turn_id)
}

async fn durable_rows(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM late_proof_pending").fetch_one(&f.env.app.db).await.unwrap()
}

fn late_proof_armed(turn_id: &str) -> bool {
    late_proof::armed_in_tests().lock().unwrap().iter().any(|(t, _)| t == turn_id)
}

/// #1654：開機恢復的篩選（pane 不符、run 已停、回合已不在飛、delivery 已不是 unknown、已過 10 分鐘 TTL）各自都不接回；對照組仍接回。
#[tokio::test]
async fn issue_1654_startup_skips_durable_rows_that_no_longer_match() {
    use super::super::delivery::{LogFormat, Proof};

    let now = "2026-10-12T12:00:00.000Z";
    // (情境, 打字後套用的 SQL（{run}／{turn} 代入），開機時間，預期接回幾筆)
    let cases: [(&str, &str, &str, usize); 6] = [
        ("對照：沒有變動，仍接回", "", "2026-10-12T12:01:00.000Z", 1),
        ("pane 不符", "UPDATE runs SET pane_id='pane-99' WHERE id='{run}'", "2026-10-12T12:01:00.000Z", 0),
        ("run 已停", "UPDATE runs SET state='stopped', ended_at='2026-10-12T12:00:30.000Z' WHERE id='{run}'", "2026-10-12T12:01:00.000Z", 0),
        ("回合已完成", "UPDATE turns SET status='completed', completed_at='2026-10-12T12:00:30.000Z' WHERE id='{turn}'", "2026-10-12T12:01:00.000Z", 0),
        ("delivery 已是 ok", "UPDATE turns SET delivery='ok' WHERE id='{turn}'", "2026-10-12T12:01:00.000Z", 0),
        ("已過 TTL", "", "2026-10-12T12:10:01.000Z", 0),
    ];
    for (label, sql, restart_at, expected) in cases {
        let f = fixture("claude", "").await;
        let app = f.env.app.clone();
        let path = with_transcript(&f, quiet()).await;
        let proof = Proof::Transcript { format: LogFormat::Claude, path, session_id: "sess-1".into() };
        let pending = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, "Reply with PONG please", now.into());
        late_proof::persist_pending_at(&app.db, &f.run_id, &pending, now).await.unwrap();
        sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        if !sql.is_empty() {
            let sql = sql.replace("{run}", &f.run_id).replace("{turn}", &f.turn_id);
            sqlx::query(&sql).execute(&app.db).await.unwrap();
        }
        let restarted = tt::restart_app(&f.env).await;
        assert_eq!(late_proof::recover_at_startup_at(&restarted, restart_at).await.unwrap(), expected, "{label}");
        assert_eq!(late_proof_armed(&f.turn_id), expected == 1, "{label}：watcher 與接回筆數一致");
    }
}

/// #1654：durable 登錄最多 256 筆（剛好 256 不刪；第 257 筆進來才刪掉最舊的那一筆）。
#[tokio::test]
async fn issue_1654_durable_registry_keeps_256_rows_and_drops_only_the_oldest() {
    use super::super::delivery::Proof;

    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let at = |i: usize| format!("2026-10-12T12:{:02}:{:02}.000Z", i / 60, i % 60);
    let mut runs = vec![f.run_id.clone()];
    while runs.len() < 257 {
        runs.push(seed_in_flight_run(&f).await.0);
    }
    let pending = late_proof::Pending::new("claude", "pane-17", &Proof::EchoRow, 0, 0, "Reply with PONG please", at(0));
    for (i, run) in runs.iter().enumerate().take(256) {
        late_proof::persist_pending_at(&app.db, run, &pending, &at(i)).await.unwrap();
    }
    assert_eq!(durable_rows(&f).await, 256, "剛好 256 筆不刪");

    late_proof::persist_pending_at(&app.db, &runs[256], &pending, &at(256)).await.unwrap();
    assert_eq!(durable_rows(&f).await, 256, "第 257 筆進來要裁回 256");
    for (run, kept) in [(&runs[0], 0), (&runs[1], 1), (&runs[256], 1)] {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM late_proof_pending WHERE run_id = ?").bind(run).fetch_one(&app.db).await.unwrap();
        assert_eq!(n, kept, "最舊的那筆被刪、其餘保留（run {run}）");
    }
}

/// #1654：欠帳補寫成 unknown 後，記憶體登錄已經沒有這一筆時，改從 durable 列接回同一回合的基準；寫回前不接。
#[tokio::test]
async fn issue_1654_owed_unknown_write_restores_its_baseline_from_durable_storage() {
    use super::super::delivery::{LogFormat, Proof};

    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let path = with_transcript(&f, quiet()).await;
    let proof = Proof::Transcript { format: LogFormat::Claude, path, session_id: "sess-1".into() };
    let text = "Reply with PONG please";
    // 列的 TTL 以注入的時間判斷，不靠牆鐘：寫入與接回都用同一個固定時間。
    let now = "2026-10-12T12:00:00.000Z";
    let pending = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, text, now.into());
    late_proof::persist_pending_at(&app.db, &f.run_id, &pending, now).await.unwrap();

    assert!(!late_proof::restore_owed_from_durable_at(&app, &f.run_id, &f.bot_id, &f.turn_id, now).await.unwrap(), "delivery 仍是 ok：還沒寫成 unknown，不接回");
    assert!(!late_proof_armed(&f.turn_id));
    sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?").bind(&f.turn_id).execute(&app.db).await.unwrap();
    assert!(late_proof::restore_owed_from_durable_at(&app, &f.run_id, &f.bot_id, &f.turn_id, now).await.unwrap(), "欠帳寫成 unknown 後要從 durable 列接回");
    let restored = late_proof::armed_in_tests().lock().unwrap().iter().find(|(t, _)| t == &f.turn_id).map(|(_, p)| p.clone()).expect("接回的基準已認領");
    assert_eq!((restored.kind.as_str(), restored.text.as_str()), ("claude", text));
}

/// #1654：開機恢復時一筆清理失敗（DELETE 出錯）只影響它自己；同一批其餘的基準仍要接回。
#[tokio::test]
async fn issue_1654_one_failed_cleanup_does_not_stop_the_startup_restore() {
    use super::super::delivery::{LogFormat, Proof};

    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let path = with_transcript(&f, quiet()).await;
    // 好的那筆（EchoRow 不看 session）較舊；壞的那筆（transcript，session 已換）較新，所以先被處理。
    let (good_run, good_turn) = seed_in_flight_run(&f).await;
    let good = late_proof::Pending::new("claude", "pane-17", &Proof::EchoRow, 0, 0, "good", "2026-10-12T12:00:00.000Z".into());
    late_proof::persist_pending_at(&app.db, &good_run, &good, "2026-10-12T12:00:00.000Z").await.unwrap();
    let bad_proof = Proof::Transcript { format: LogFormat::Claude, path, session_id: "sess-1".into() };
    let bad = late_proof::Pending::new("claude", "pane-17", &bad_proof, 0, 0, "bad", "2026-10-12T12:00:05.000Z".into());
    late_proof::persist_pending_at(&app.db, &f.run_id, &bad, "2026-10-12T12:00:05.000Z").await.unwrap();
    sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id IN (?, ?)").bind(&f.turn_id).bind(&good_turn).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE runs SET native_session_id='sess-2' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
    // 只讓壞的那筆 DELETE 失敗。
    sqlx::query(&format!(
        "CREATE TRIGGER issue_1654_fail_cleanup BEFORE DELETE ON late_proof_pending WHEN OLD.turn_id = '{}' BEGIN SELECT RAISE(ABORT, 'injected cleanup failure'); END",
        f.turn_id
    ))
    .execute(&app.db)
    .await
    .unwrap();

    let restarted = tt::restart_app(&f.env).await;
    assert_eq!(
        late_proof::recover_at_startup_at(&restarted, "2026-10-12T12:01:00.000Z").await.unwrap(),
        1,
        "壞的那筆清不掉，不能中止整批；好的那筆仍要接回"
    );
    assert!(late_proof_armed(&good_turn), "同一批排在後面的好基準要接回");
    assert!(!late_proof_armed(&f.turn_id), "session 已換的壞基準不為它開 watcher");
}

/// #1654：補證 watcher 要登記在關機屏障內：關機時要能取消，不能活到 60 次觀察跑完。
#[tokio::test]
async fn issue_1654_late_proof_watcher_is_tracked_and_stops_at_shutdown() {
    use super::super::delivery::{LogFormat, Proof};

    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let path = with_transcript(&f, quiet()).await;
    let proof = Proof::Transcript { format: LogFormat::Claude, path, session_id: "sess-1".into() };
    let pending = late_proof::Pending::new("claude", "pane-17", &proof, 0, 0, "Reply with PONG please", db::now());
    late_proof::spawn_watch(&app, &f.run_id, &f.bot_id, &f.turn_id, pending);
    assert_eq!(app.background_tasks.len(), 1, "補證 watcher 要登記在 background_tasks");

    app.shutdown.cancel();
    app.background_tasks.close();
    tokio::time::timeout(Duration::from_secs(5), app.background_tasks.wait()).await.expect("關機時補證 watcher 要立即離開");
    assert_eq!(app.background_tasks.len(), 0);
}

async fn delivery_row(f: &Fixture) -> (String, String, i64, i64) {
    sqlx::query_as("SELECT status, delivery, delivery_verified, auto_resend FROM turns WHERE id = ?")
        .bind(&f.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap()
}

fn quiet() -> crate::testing::LivePane {
    crate::testing::LivePane { hide_echo: true, no_spinner: true, ..wide() }
}

const NEVER: Duration = Duration::ZERO;

/// 同步窗口之後回音列才出現：背景升成 ok（已驗證）、不自動重送、推 turn 事件。
#[tokio::test]
async fn an_echo_row_that_shows_up_after_the_window_promotes_the_unknown_turn() {
    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let text = "Reply with PONG please";
    let p = unproven_and_armed(&f, quiet(), text).await;
    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Wait, "還沒有證據");
    assert_eq!(late_proof::watch(&app, &f.run_id, &f.bot_id, &f.turn_id, &p, 2, NEVER).await, None);
    assert_eq!(delivery_row(&f).await, ("in_flight".into(), "unknown".into(), 0, 1), "看不到就維持 unknown");

    let mut pane = f.env.herdr.pane("pane-17").unwrap();
    pane.transcript.push(format!("❯ {text}"));
    live(&f, pane);
    let mut events = app.subscribe();
    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Submitted);
    assert_eq!(late_proof::watch(&app, &f.run_id, &f.bot_id, &f.turn_id, &p, 2, NEVER).await, Some(Delivered::Submitted));
    assert_eq!(delivery_row(&f).await, ("in_flight".into(), "ok".into(), 1, 0), "ok＋已驗證＋不重送");
    let kind = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.unwrap().unwrap().kind;
    assert_eq!(kind, "turn_updated", "網頁要收到 turn 事件才解得開鎖");
}

/// 框空了、bot 在做事（herdr 說 working）但沒有無損證據：升成 ok、**未驗證**、不重送。
#[tokio::test]
async fn an_empty_box_and_a_working_agent_promote_to_unverified_not_verified() {
    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let p = unproven_and_armed(&f, quiet(), "Reply with PONG please").await;
    sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();

    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Active);
    assert_eq!(late_proof::watch(&app, &f.run_id, &f.bot_id, &f.turn_id, &p, 1, NEVER).await, Some(Delivered::Unverified));
    assert_eq!(delivery_row(&f).await, ("in_flight".into(), "ok".into(), 0, 0));
    let resend: i64 = sqlx::query_scalar("SELECT resend_count FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(resend, MAX_PROMPT_RESENDS, "不能因為升級就被 stall watchdog 再打一次");
}

/// 動靜的另外兩種：回合送出之後 hook 有事件；畫面上有進行中的 spinner。
#[tokio::test]
async fn hook_activity_and_a_live_spinner_each_count_as_the_agent_working() {
    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let p = unproven_and_armed(&f, quiet(), "Reply with PONG please").await;
    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Wait, "idle、沒有 hook、沒有 spinner");
    hook_event(&f, "h-late", r#"{"hook_event_name":"PostToolUse"}"#, &db::now()).await;
    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Active, "hook 有動靜");

    let g = fixture("claude", "").await;
    let q = unproven_and_armed(&g, crate::testing::LivePane { no_spinner: false, ..quiet() }, "Reply with PONG please").await;
    assert_eq!(late_proof::look(&g.env.app, &g.bot_id, &g.turn_id, &q).await, Look::Active, "畫面上有 spinner");
}

/// 回合已經被別的路收掉（Stop hook、放棄、run 結束）：背景什麼都不寫。
#[tokio::test]
async fn a_turn_closed_meanwhile_is_left_alone() {
    let f = fixture("claude", "").await;
    let app = f.env.app.clone();
    let p = unproven_and_armed(&f, quiet(), "Reply with PONG please").await;
    sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE turns SET status = 'failed' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();

    assert_eq!(late_proof::look(&app, &f.bot_id, &f.turn_id, &p).await, Look::Stop);
    assert_eq!(late_proof::watch(&app, &f.run_id, &f.bot_id, &f.turn_id, &p, 1, NEVER).await, None);
    assert_eq!(delivery_row(&f).await, ("failed".into(), "unknown".into(), 0, 1));
    assert!(!late_proof::promote(&app, &f.turn_id, Delivered::Submitted).await.unwrap(), "CAS 擋下");
}

/// 字還留在框裡（Enter 被吃掉）不是 `not_proven_submitted`：沒有記下任何東西，認領不到、背景不看。
#[tokio::test]
async fn a_prompt_still_in_the_box_is_not_watched_in_the_background() {
    let f = fixture("claude", "").await;
    live(&f, crate::testing::LivePane { swallow_enter: true, ..wide() });
    let app = f.env.app.clone();
    db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
    let (run, bot) = run_and_bot(&f).await;
    let client = client_for_run(&app, &run).await.unwrap();
    assert_eq!(deliver_prompt(&app, &client, &run, &bot, "go", false, false).await.unwrap(), Delivered::Unproven("still_in_box"));

    late_proof::arm(&app, &f.run_id, &f.bot_id, &f.turn_id);
    assert!(late_proof::armed_in_tests().lock().unwrap().iter().all(|(t, _)| *t != f.turn_id));
}
