//! codex 的撞限辨識（`codex_limit_hit_line`）一樣是「整行含有」。grok 那邊（#227）證明過別人的輸出會把在飛的回合收成 failed、
//! 額度標用盡；codex 有 `status_line_says_headroom`（狀態列顯示還有餘裕就當舊橫幅），這裡驗它擋不擋得住。
use super::*;
use crate::testing as tt;

const BANNER: &str = "■ You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 19th, 2099 6:43 PM.";
/// 真的 codex 0.155.1 頁尾（`fixtures/codex-0.155-finished.txt`）：只有模型與目錄，沒有 `5h N% left`。
const FOOTER_0155: &str = "  gpt-6-astra low · /private/tmp/am-g1-codex0155-cwd";
/// 有額度的頁尾（`fixtures/codex_astra_particles_empty.ansi`）：橫幅之後還有餘裕就當舊橫幅。
const FOOTER_QUOTA: &str = "  gpt-6-astra medium · /private/tmp · Context 0% used · 5h 100% left · weekly 66% left";

async fn codex_turn(screen: &str) -> (tt::Env, db::Bot, String, String) {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cx").await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    let run = tt::fake_run(&app, &bot.id).await;
    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    env.herdr.set_screen(&format!("pane-{}", bot.id), screen);
    (env, bot, run, turn)
}

/// codex 自己讀了一份印著撞限句子的檔案／diff／測試輸出之後，正常做完的畫面。
fn quoting(kind: &str, footer: &str) -> String {
    let quoted = match kind {
        "cat" => format!("  └ {BANNER}"),
        "plain" => "  └ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.".to_string(),
        // 回覆散文裡逐行貼出來的橫幅：行首就是 `■`，單行判斷認得，只能靠「它不是最後一個 cell」分辨。
        "bullet" => format!("  {BANNER}"),
        "diff" => "  └ +ERROR: You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.".to_string(),
        "source" => "  └ daemon/src/lifecycle/screen.rs:566:    let hit = lower.contains(\"hit your usage limit\")".to_string(),
        "source2" => "  └ daemon/src/lifecycle/screen.rs:567:        || (lower.contains(\"usage limit\") && (lower.contains(\"try again\") || lower.contains(\"upgrade to\")));".to_string(),
        "panic" => "  └ thread 'x' panicked at src/a.rs:1:1: expected no hit: [\"ERROR: You've hit your usage limit. try again later\"]".to_string(),
        _ => unreachable!(),
    };
    format!("› 讀一下這個\n\n• Ran cat notes.txt\n{quoted}\n\n• 讀完了，這是舊的紀錄，跟現在無關。\n\n  done Aug 2 at 3:00 AM\n\n› Ask Codex to do anything\n\n{footer}\n")
}

const KINDS: [&str; 7] = ["cat", "plain", "bullet", "diff", "source", "source2", "panic"];

/// #237：codex 讀到含撞限句子的東西、正常答完的畫面——掃描不能把它當成撞限。
#[tokio::test]
async fn a_turn_that_only_quotes_the_limit_banner_finishes_normally() {
    for footer in [FOOTER_0155, FOOTER_QUOTA] {
        for kind in KINDS {
            let screen = quoting(kind, footer);
            let (env, bot, run, turn) = codex_turn(&screen).await;
            let app = env.app.clone();
            capture_codex_usage_notices(&app, &bot.id, &run).await.unwrap();
            let status = crate::lifecycle::run_state::turn_status(&app, &turn).await;
            assert_eq!(status, "in_flight", "{kind}／{footer}：掃描不動這一回合");
            assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_none(), "{kind}／{footer}：額度不標");

            assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
            assert_eq!(
                crate::lifecycle::run_state::turn_status(&app, &turn).await,
                "completed_fallback",
                "{kind}／{footer}：備援照一般回覆收，不是撞限"
            );
            assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_none(), "{kind}／{footer}");
        }
    }
}

/// 單行：只認行首。工具輸出的前綴（`└`、`+`）、句子中間都不算；真橫幅的兩種寫法照舊。
#[test]
fn only_a_line_that_starts_with_the_banner_is_a_limit_hit() {
    for no in [
        r#"daemon/src/lifecycle/screen.rs:566:    let hit = lower.contains("hit your usage limit")"#,
        r#"daemon/src/lifecycle/screen.rs:567:        || (lower.contains("usage limit") && (lower.contains("try again") || lower.contains("upgrade to")));"#,
        "  └ +ERROR: You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.",
        "  └ ■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.",
        "  └ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.",
        r#"thread 'x' panicked at src/a.rs:1:1: expected no hit: ["ERROR: You've hit your usage limit. try again later"]"#,
        "後來 codex 說 You've hit your usage limit，所以我停了",
    ] {
        assert!(codex_limit_hit_line(no).is_none(), "{no}");
    }
    for yes in [
        "■ You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), or try again at Sep 19th, 2099 6:43 PM.",
        "  ■ You've hit your usage limit.",
        "ERROR: You've hit your usage limit. Upgrade to Pro, or try again at Sep 6th, 2026 3:00 PM.",
        "• You've hit your usage limit. Try again in 4 days.",
        "You've hit your usage limit.",
    ] {
        assert!(codex_limit_hit_line(yes).is_some(), "{yes}");
    }
}

/// 窄 pane 折行的橫幅（`■ You've hit your usage` / `limit. …`）也一樣：後面還有 codex 自己的回覆，就是引用。
#[test]
fn a_wrapped_banner_followed_by_the_reply_is_a_quote() {
    let wrapped = "■ You've hit your usage\nlimit. Upgrade to Pro\n(https://chatgpt.com/ex\nplore/pro),\nto purchase more\ncredits or try again at\n1:32 PM.\n";
    let quoted = format!("› 讀一下這個\n\n{wrapped}\n• 讀完了，這是舊的紀錄。\n\n› Ask Codex to do anyt\n\n  gpt-5.6-luna max fas…\n");
    assert!(codex_usage_notice_lines(&quoted).is_empty(), "{:?}", codex_usage_notice_lines(&quoted));
    let real = format!("› ping\n\n{wrapped}\n› Ask Codex to do anyt\n\n  gpt-5.6-luna max fas…\n");
    assert_eq!(codex_usage_notice_lines(&real).len(), 1, "真的折行橫幅照舊認");
}

/// 真橫幅前面捲著引用同一句的輸出，還是要認，而且只記真的那一張。
#[test]
fn the_real_banner_is_read_under_quoted_scrollback() {
    // 引用的日期跟真橫幅不同，才分得出 `codex_usage_notice_lines` 記的是哪一張（同一句會被去重）。
    let quoted = quoting("bullet", FOOTER_0155).replace("Sep 19th, 2099 6:43 PM", "Sep 1st, 2026 1:00 AM");
    let screen = format!("{}\n\n■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.\n\n\n› Ask Codex to do anything\n\n{FOOTER_0155}\n", quoted.trim_end());
    // 引用（`bullet`）後面接著 codex 自己的回覆，不算；最後那張才是。
    let lines = codex_usage_notice_lines(&screen);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("hit your usage limit") && lines[0].contains("2099 6:43 PM"), "{lines:?}");
}

/// issue #956: 終端畫面在 Orchestrating / thinking with high effort / 計時中時視為運算中；已完成的行與空框不是。
#[test]
fn is_screen_busy_recognizes_a_live_spinner_but_not_a_finished_one() {
    let p36_screen = "✽ Orchestrating… (13s · ↓ 537 tokens · thinking with high effort)\n  ⎿ Tip: Run /install-github-app to tag @claude right from your Github issues and PRs\n─────────────────────────────────────────────────────────────────────────────\n❯\n─────────────────────────────────────────────────────────────────────────────\n  eddie | mixture-tracker | OP5.5 H 35% | ...\n";
    assert!(is_screen_busy(p36_screen), "Claude p36 orchestrating 應判定為 busy");
    assert!(is_screen_busy("⠦ Thinking… 52s\n"));
    assert!(is_screen_busy("• Reviewing (1h 02m 03s • ctrl + c to interrupt)\n"));
    assert!(is_screen_busy("✢ Baking… (3m 18s · ↓ 11.0k tokens)\n❯\n"), "換一個動詞與 glyph 也認得");

    // 過去已完成的回覆或空框不是 busy。
    assert!(!is_screen_busy("✻ Crunched for 9s · done 11:35 PM\n❯ \n"));
    assert!(!is_screen_busy("────────────────────\n❯ \n────────────────────\n"));
    assert!(!is_screen_busy("使用者貼了一句 Thinking (about X)\n"));
}
