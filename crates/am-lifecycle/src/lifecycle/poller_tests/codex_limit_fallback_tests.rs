//! #198：備援看到 codex 撞限橫幅，記撞限走 `turn_error::mark_codex_limit_hit`（讀不到主機、算不準 key 都欠著），不退回 `local`、不猜 key。
use super::*;
use crate::testing as tt;

#[tokio::test]
async fn a_limit_banner_whose_key_is_unknown_is_owed_by_the_fallback() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cx").await;
    sqlx::query("UPDATE bots SET kind='codex', identity='cx0' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    let run = tt::fake_run(&app, &bot.id).await;
    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    let banner = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.";
    env.herdr.set_screen(&format!("pane-{}", bot.id), &format!("› 派工\n\n{banner}\n\n› \n"));

    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap(), "回合照樣收");
    assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "failed");
    assert!(app.quotas.lock().await.values().all(|q| q.limit_hit.is_none()), "身份表還沒偵測完：沒有猜一格 `codex:cx0` 寫下去");
    assert!(app_ports_p4obs::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");
}
