//! #222：grok 撞週限。6d5e74ab 讓掃描（`capture_codex_usage_notices`）認得那兩句，這裡驗**它有沒有被叫到**：
//! herdr 把這張畫面判成 `blocked`（不是 working→idle），備援與掃描都靠 idle 邊觸發，blocked 的 bot 沒人看。
use super::*;
use crate::testing as tt;
use std::time::Duration;

/// 票上貼的網頁「終端擷取」原文（2026-09-19 16:26，grok 1.0.34）。
const TICKET_SCREEN: &str = "\
┃  You hit your weekly limit.
┃
┃  1 (○) Upgrade tier      Upgrade to a higher tier for more usage
┃  2 (○) Buy more credits  Purchase credits to keep using Grok Build
┃  3 (○) Try Again         Resubmit the last prompt once you have usage again
┃
┃  ↑/↓ navigate · y copy
Enter:submit
Tab:next answer  |  Esc:scrollback  |  Shift+x:dismiss
";
/// 6d5e74ab 用的 w168:pB7 真畫面（帶 402 那一句與縮排）。
const PB7_SCREEN: &str = include_str!("fixtures/grok_limit_hit.txt");

async fn grok_turn(screen: &str) -> (tt::Env, db::Bot, String, String) {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "gk").await;
    sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    let run = tt::fake_run(&app, &bot.id).await;
    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    env.herdr.set_screen(&format!("pane-{}", bot.id), screen);
    (env, bot, run, turn)
}

async fn status(app: &Arc<App>, turn: &str) -> String {
    crate::lifecycle::run_state::turn_status(app, turn).await
}

async fn wait_failed(app: &Arc<App>, turn: &str) -> String {
    for _ in 0..250 {
        let s = status(app, turn).await;
        if s != "in_flight" {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    status(app, turn).await
}

fn no_keys(env: &tt::Env) {
    assert!(env.herdr.calls_to("pane.send_keys").is_empty() && env.herdr.calls_to("pane.send_text").is_empty(), "不准替使用者按任何選項（1、2 是付費）");
}

fn blocked_event(bot_id: &str) -> crate::herdr::Event {
    crate::herdr::Event {
        event: "pane_agent_status_changed".into(),
        data: serde_json::json!({"pane_id": format!("pane-{bot_id}"), "agent_status": "blocked"}),
    }
}

/// 撞限要記在 **grok** 那一格：6d5e74ab 讓 grok 走 `mark_codex_limit_hit`，而 `turn_error::record` 對它一律 `resolve_quota_base(.., "codex", ..)`，
/// 於是 grok 撞週限卻把 codex 標成用盡（codex 的派工被擋），grok 自己那格沒事（派工照派）。
#[tokio::test]
async fn the_limit_lands_on_groks_quota_and_not_codexs() {
    for screen in [TICKET_SCREEN, PB7_SCREEN] {
        let (env, bot, run, _turn) = grok_turn(screen).await;
        let app = env.app.clone();
        capture_codex_usage_notices(&app, &bot.id, &run).await.unwrap();
        let quotas = app.quotas.lock().await;
        assert!(quotas.get("codex").is_none_or(|q| q.limit_hit.is_none() && q.five_hour.is_none()), "codex 那格不該被 grok 的撞限動到：{:?}", quotas.get("codex"));
        let grok = quotas.get("grok").expect("grok 那一格");
        assert!(grok.limit_hit.is_some(), "grok 那一格要標成撞限");
        assert_eq!(grok.seven_day.as_ref().map(|w| w.used_pct), Some(100.0), "grok 只有週窗，存在 seven_day");
        drop(quotas);
        assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "派工看得到 grok 已用完");
    }
}

/// herdr 判成 `working -> idle` 時的終端備援：不能把這張畫面當成 agent 的回覆收成 `completed_fallback`（那樣回合「成功」
/// 了，派工不會 park、UI 只顯示「可能不完整」的一坨終端字）。
#[tokio::test]
async fn the_fallback_does_not_store_the_limit_screen_as_the_reply() {
    for screen in [TICKET_SCREEN, PB7_SCREEN] {
        let (env, bot, run, turn) = grok_turn(screen).await;
        let app = env.app.clone();
        assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
        assert_eq!(status(&app, &turn).await, "failed", "撞限是失敗的回合，不是 completed_fallback");
        let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(replies, 0, "選單的字不是 agent 的回覆");
        assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "派工看得到 grok 已用完");
        no_keys(&env);
    }
}

/// 票上的現象：herdr 說 `blocked`，沒有 working->idle 那條邊，備援與掃描都不會跑——bot 就停在 blocked，回合在飛。
#[tokio::test]
async fn a_blocked_grok_on_the_limit_screen_is_marked_exhausted() {
    for screen in [TICKET_SCREEN, PB7_SCREEN] {
        let (env, bot, _run, turn) = grok_turn(screen).await;
        let app = env.app.clone();
        app_ports_p4obs::handle_status(&app, crate::config::LOCAL_HOST, "test", &blocked_event(&bot.id)).await;
        assert_eq!(wait_failed(&app, &turn).await, "failed", "blocked 那條邊要看畫面");
        assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
        no_keys(&env);
    }
}

/// blocked 邊漏了（daemon 重啟時 bot 已經停在那裡，不會再有邊）：回合進行中的 poller 是安全網，`blocked` 時它整個跳過。
#[tokio::test]
async fn the_progress_poller_reads_a_blocked_limit_screen_too() {
    let (env, bot, run, turn) = grok_turn(TICKET_SCREEN).await;
    let app = env.app.clone();
    sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
    arm_progress(&app, &run, &bot.id, &turn).await;
    assert_eq!(wait_failed(&app, &turn).await, "failed");
    assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
    no_keys(&env);
}

const WEEKLY: &str = "You hit your weekly limit.";
const BALANCE: &str = "Turn failed: Request failed (402): Grok Build usage balance exhausted";

/// 同一張畫面的兩句（402 credits 用完、週限）不管誰先來，撞限都是週限的那個窗與 7 天保底：後到的不能把它縮成 5 小時；
/// 同一句重複看到，撞的那一刻不往後推。
#[tokio::test]
async fn the_two_lines_of_one_screen_never_shorten_each_other() {
    let at = |t: &Option<String>| chrono::DateTime::parse_from_rfc3339(t.as_deref().unwrap()).unwrap();
    for order in [[WEEKLY, BALANCE], [BALANCE, WEEKLY]] {
        let (env, bot, _run, _turn) = grok_turn(TICKET_SCREEN).await;
        let app = env.app.clone();
        for line in order {
            crate::turn_error::mark_codex_limit_hit(&app, &bot, line).await.unwrap();
        }
        let hit = app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().unwrap();
        assert_eq!(hit.bucket.as_deref(), Some("seven_day"), "{order:?}");
        assert_eq!(hit.message, WEEKLY, "{order:?}");
        assert!(at(&hit.until) > chrono::Utc::now() + chrono::Duration::days(6), "{order:?}：7 天保底，不是 5 小時");
        assert_eq!(app.quotas.lock().await.get("grok").and_then(|q| q.seven_day.as_ref()).map(|w| w.used_pct), Some(100.0));

        crate::turn_error::mark_codex_limit_hit(&app, &bot, WEEKLY).await.unwrap();
        let again = app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().unwrap();
        assert_eq!((again.at, again.until), (hit.at, hit.until), "同一句再看到不重算");
    }
}

/// 只有 402（credits 用完、沒有窗）：撞限照記在 grok 那一格、擋派工，但不亂標窗、保底只有 5 小時。
#[tokio::test]
async fn a_balance_only_screen_blocks_grok_without_inventing_a_window() {
    let (env, bot, _run, _turn) = grok_turn(TICKET_SCREEN).await;
    let app = env.app.clone();
    crate::turn_error::mark_codex_limit_hit(&app, &bot, BALANCE).await.unwrap();
    let hit = app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("擋派工");
    assert_eq!(hit.bucket, None);
    let until = chrono::DateTime::parse_from_rfc3339(hit.until.as_deref().unwrap()).unwrap();
    assert!(until < chrono::Utc::now() + chrono::Duration::hours(6), "沒有窗：保底只有 5 小時，不擋七天");
    let quotas = app.quotas.lock().await;
    let q = quotas.get("grok").unwrap();
    assert!(q.five_hour.is_none() && q.seven_day.is_none(), "沒有窗就不標窗");
    assert!(quotas.get("codex").is_none());
}

/// 算不準落在哪把 key（身份表還沒偵測完）：不猜 `grok:<身份>` 寫下去，欠著、照擋——跟 claude／codex 同一條規則（#198）。
#[tokio::test]
async fn a_grok_limit_whose_key_is_unknown_is_owed() {
    let (env, bot, run, turn) = grok_turn(TICKET_SCREEN).await;
    let app = env.app.clone();
    sqlx::query("UPDATE bots SET identity='gk0' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    assert!(capture_codex_usage_notices(&app, &bot.id, &run).await.is_err(), "記不進去回錯");
    assert_eq!(status(&app, &turn).await, "failed", "回合照樣收");
    assert!(app.quotas.lock().await.values().all(|q| q.limit_hit.is_none()), "沒有猜一格寫下去");
    assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");
}

/// 別的 CLI 的 bot 停在 blocked、畫面上剛好有同一句：不動（這是 grok 的畫面）。
#[tokio::test]
async fn a_claude_bot_showing_the_same_words_is_left_alone() {
    let (env, bot, _run, turn) = grok_turn(TICKET_SCREEN).await;
    let app = env.app.clone();
    sqlx::query("UPDATE bots SET kind='claude' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    app_ports_p4obs::handle_status(&app, crate::config::LOCAL_HOST, "test", &blocked_event(&bot.id)).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(status(&app, &turn).await, "in_flight");
    assert!(app.quotas.lock().await.is_empty());
}

/// #227（r1 的重現）：grok 畫面上印出的別人的輸出——原始碼、diff、測試斷言訊息——含有那幾句，不是撞限。
#[test]
fn grok_source_lines_are_not_limit_hits() {
    let lines = [
        r#"daemon/src/lifecycle/screen.rs:439:        || low.contains("you hit your weekly limit")"#,
        r#"+  ┃  Turn failed: Request failed (402): Grok Build usage balance exhausted"#,
        r#"thread 'x' panicked at src/a.rs:1:1: expected no hit: ["You hit your weekly limit."]"#,
        r#"    assert!(lines.iter().any(|l| l.contains("usage balance exhausted")), "{lines:?}");"#,
        "Request failed (402): Grok Build usage balance exhausted",
        "The 402 error says usage balance exhausted, so you hit your weekly limit.",
    ];
    let hits: Vec<&str> = lines.iter().copied().filter(|l| grok_limit_hit_line(l).is_some()).collect();
    assert!(hits.is_empty(), "{hits:#?}");
    // 真畫面的形狀照舊認：框線、行首就是那句。
    assert!(grok_limit_hit_line("  ┃  You hit your weekly limit.").is_some());
    assert!(grok_limit_hit_line("  ┃  You've hit your weekly limit.").is_some());
    assert!(grok_limit_hit_line("  ┃  Turn failed: Request failed (402): Grok Build usage balance exhausted").is_some());
}

/// #227：cat 出來的 fixture 一字不差（連框線都在），但它不是畫面最底下那張會擋住輸入列的選單——底下還有 bot 自己的回覆與輸入列。
/// 真選單是 modal：標題、兩個以上的選項、之後只剩選單自己的說明列。
#[test]
fn output_that_quotes_the_limit_screen_is_not_a_limit() {
    let cat = format!("❯ cat daemon/src/lifecycle/fixtures/grok_limit_hit.txt\n{PB7_SCREEN}\n⏺ 這是 6d5e74ab 的 fixture，我在讀它。\n────\n❯\n────\n");
    let ticket = format!("❯ 貼一下票上的原文\n{TICKET_SCREEN}\n⏺ 好了。\n❯\n");
    let diff = "❯ git diff\n+  ┃  You hit your weekly limit.\n+  ┃  Turn failed: Request failed (402): Grok Build usage balance exhausted\n⏺ 看完了\n❯\n";
    let test_out = "❯ cargo test\nthread 'x' panicked at src/a.rs:1:1: expected no hit: [\"You hit your weekly limit.\"]\n⏺ 紅了\n❯\n";
    // 只有句子、沒有選項：不是那張選單（後面接別的行、或後面只剩說明列都一樣；只有一個選項也不夠）。
    let bare = "❯ echo\n┃  You hit your weekly limit.\n⏺ 我把這句印出來看看\n";
    let title_only = "┃  You hit your weekly limit.\n┃\n┃  ↑/↓ navigate · y copy   Enter:submit\n";
    let one_option = "┃  You hit your weekly limit.\n┃  3 (○) Try Again         Resubmit the last prompt once you have usage again\n┃  ↑/↓ navigate · y copy\n";
    for (what, screen) in [
        ("cat", cat.as_str()),
        ("票上原文貼在對話裡", ticket.as_str()),
        ("diff", diff),
        ("測試輸出", test_out),
        ("只有句子", bare),
        ("標題加說明列、沒有選項", title_only),
        ("只有一個選項", one_option),
    ] {
        let lines = grok_limit_notice_lines(screen);
        assert!(lines.is_empty(), "{what}：{lines:?}");
    }
}

/// 真選單前面就算捲著引用同一句話的輸出，還是要認（而且只記那張選單，不記上面引用的）。
#[test]
fn the_real_menu_is_read_under_quoted_scrollback() {
    let filler: String = (0..24).map(|i| format!("⏺ 之後又做了第 {i} 件事\n")).collect();
    let screen = format!("❯ rg limit\n┃  You hit your daily limit.\n┃  Turn failed: Request failed (402): Grok Build usage balance exhausted\n{filler}{TICKET_SCREEN}");
    assert_eq!(grok_limit_notice_lines(&screen), vec![WEEKLY.to_string()]);
    // 真的 pB7 畫面：402 那句在選單上面十幾行，屬於同一個框，要跟著記。
    let pb7 = grok_limit_notice_lines(PB7_SCREEN);
    assert_eq!(pb7.len(), 2, "{pb7:?}");
}

/// #227 的傷害：在飛的回合其實正常做完了（畫面上只是引用那幾句），被收成 failed、額度標 7 天。備援與 blocked 邊都不能。
#[tokio::test]
async fn a_turn_that_only_quotes_the_limit_screen_finishes_normally() {
    let quoted = format!("❯ cat fixtures/grok_limit_hit.txt\n{PB7_SCREEN}\n⏺ 這是 fixture，我在讀它。\n────\n❯\n────\n");
    let (env, bot, run, turn) = grok_turn(&quoted).await;
    let app = env.app.clone();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert_eq!(status(&app, &turn).await, "completed_fallback", "正常做完，不是撞限");
    assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_none());
    assert!(app.quotas.lock().await.is_empty(), "額度沒被標");

    let (env, bot, _run, turn) = grok_turn(&quoted).await;
    let app = env.app.clone();
    app_ports_p4obs::handle_status(&app, crate::config::LOCAL_HOST, "test", &blocked_event(&bot.id)).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(status(&app, &turn).await, "in_flight", "blocked 邊掃了也不動");
    assert!(app.quotas.lock().await.is_empty());
}

/// 票上要求：同一選單的其他變體（session／daily limit）也要認。6d5e74ab 的辨識只有 `weekly` 與 402 兩句。
#[test]
fn the_other_limit_variants_the_ticket_names_are_read_too() {
    for (title, window) in [
        ("You hit your session limit.", (Some("five_hour"), 5)),
        ("You hit your 5-hour limit.", (Some("five_hour"), 5)),
        ("You hit your daily limit.", (None, 24)),
        ("You hit your monthly limit.", (None, 5)),
        ("You've hit your weekly limit.", (Some("seven_day"), 168)),
    ] {
        let screen = TICKET_SCREEN.replace("You hit your weekly limit.", title);
        let lines = grok_limit_notice_lines(&screen);
        assert_eq!(lines, vec![title.to_string()], "{title}");
        assert_eq!(grok_limit_window(&lines[0]), window, "{title}");
    }
    // 只認畫面上那一句標題：回覆裡談到、或標題後面接了字母的不算。
    assert!(grok_limit_hit_line("Then you hit your daily limit and stopped").is_none());
    assert!(grok_limit_hit_line("You hit your daily limits, apparently").is_none());
    assert!(grok_limit_hit_line("You hit your limit").is_none(), "沒有說是哪個窗");
}

#[test]
fn the_window_a_grok_line_names() {
    assert_eq!(grok_limit_window("┃  You hit your weekly limit."), (Some("seven_day"), 168));
    assert_eq!(grok_limit_window("You hit your session limit."), (Some("five_hour"), 5));
    assert_eq!(grok_limit_window(BALANCE), (None, 5));
    // 有窗的那句排前面：一次讀畫面只有第一句算新的。
    let lines = grok_limit_notice_lines(PB7_SCREEN);
    assert_eq!(lines.first().map(String::as_str), Some(WEEKLY), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("usage balance exhausted")));
}

/// 撞限要有出口：grok 沒有 Stop hook 之外的「額度回來了」訊號，橫幅又沒寫時間。`/usage` 探測回來說這個窗是撞限**之後**才開的
/// （已經重置），撞限要作廢，不能擋到重啟 daemon。
#[tokio::test]
async fn the_limit_ends_when_a_fresh_usage_reading_says_the_window_reset() {
    let (env, bot, run, _turn) = grok_turn(TICKET_SCREEN).await;
    let app = env.app.clone();
    capture_codex_usage_notices(&app, &bot.id, &run).await.unwrap();
    let hit = app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("前提：撞限記下了（6d5e74ab 的掃描）");
    assert_eq!(hit.bucket.as_deref(), Some("seven_day"), "撞限帶額度窗，`/usage` 探測才校正得到");
    let reading = crate::quota::Quota {
        five_hour: None,
        seven_day: Some(crate::quota::Window { observed_at: None, used_pct: 10.0, resets_at: Some(db::iso_at(chrono::Utc::now() + chrono::Duration::days(8))) }),
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: Some("SuperGrok".into()),
        updated_at: db::now(),
        source: "grok-usage".into(),
        account: None,
        host: crate::config::LOCAL_HOST.into(),
    };
    app_ports_p4obs::set_quota(&app, crate::config::LOCAL_HOST, "grok", reading).await;
    assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_none(), "窗已經重置：撞限作廢");
}
