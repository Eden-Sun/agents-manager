//! codex 的撞限辨識（`codex_limit_hit_line`）一樣是「整行含有」。grok 那邊（#227）證明過別人的輸出會把在飛的回合收成 failed、
//! 額度標用盡；codex 有 `status_line_says_headroom`（狀態列顯示還有餘裕就當舊橫幅），這裡驗它擋不擋得住。
use crate::lifecycle_tests::prelude::*;
use crate::lifecycle::*;
use crate::app_ports_p4obs;
use crate::lifecycle::poller::*;
use crate::lifecycle::poller::test_seams::*;

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

