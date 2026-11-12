use super::start_bot;
use crate::capabilities::Db;
use crate::db;
use crate::testing as tt;

const GROK_HIGH: &str = "  ╭──────────────────────────────────────────╮\n  │ ❯                                        │\n  ╰──────────────── Grok 4.6 (high) · always-approve ─╯\n";
const GROK_MEDIUM: &str = "  ╭──────────────────────────────────────────╮\n  │ ❯                                        │\n  ╰──────────────── Grok 4.6 (medium) · always-approve ─╯\n";

async fn grok_bot(app: &std::sync::Arc<crate::state::App>, project_id: &str, effort: Option<&str>) -> db::Bot {
    let id = db::ulid();
    let grok_home = tt::scratch_dir("am-grok-startup-effort-home");
    let env_json = serde_json::json!({ "GROK_HOME": grok_home.to_string_lossy() }).to_string();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, model, effort, args_json, autostart, inject_hooks, hook_token, managed_by, env_json, created_at)
         VALUES (?,?,?,'grok','grok-4.6',?,'[]',0,1,'tok','user',?,?)",
    )
    .bind(&id)
    .bind(project_id)
    .bind("g-effort")
    .bind(effort)
    .bind(&env_json)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    db::bot(app.db(), &id).await.unwrap().unwrap()
}

fn sent_effort_slash(e: &tt::Env) -> Vec<String> {
    e.herdr
        .calls_to("pane.send_text")
        .into_iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .filter(|t| t.contains("/effort "))
        .collect()
}

/// grok 1.0.46 拋棄式 `GROK_HOME`：`/effort` 會把 `[models] default_reasoning_effort` 寫進
/// 使用者的 config.toml；`--reasoning-effort` 只改這一輪框底、不寫檔。框底還是 high 也不准補 slash。
#[tokio::test]
async fn a_grok_bot_set_to_medium_does_not_persist_effort_with_a_slash() {
    let e = tt::env().await;
    e.herdr.set_screen("*", GROK_HIGH);
    let bot = grok_bot(&e.app, &e.project_id, Some("medium")).await;
    start_bot(&e.app, &bot.id).await.unwrap();
    assert!(sent_effort_slash(&e).is_empty(), "不准送 /effort，那會寫進 ~/.grok/config.toml");
    let run = db::active_run(e.app.db(), &bot.id).await.unwrap().unwrap();
    assert_eq!(run.runtime_effort.as_deref(), Some("medium"), "等級只來自 --reasoning-effort");
}

/// 畫面已經是設定的等級：不要再打字（pane_typed 會改 prompt 路徑）。
#[tokio::test]
async fn a_grok_bot_does_not_slash_when_the_tui_already_matches() {
    let e = tt::env().await;
    e.herdr.set_screen("*", GROK_MEDIUM);
    let bot = grok_bot(&e.app, &e.project_id, Some("medium")).await;
    start_bot(&e.app, &bot.id).await.unwrap();
    assert!(sent_effort_slash(&e).is_empty(), "已經 medium 就不要再 /effort");
}

/// 沒設 effort：不要自作主張改 TUI。
#[tokio::test]
async fn a_grok_bot_without_effort_is_left_on_the_tui_default() {
    let e = tt::env().await;
    e.herdr.set_screen("*", GROK_HIGH);
    let bot = grok_bot(&e.app, &e.project_id, None).await;
    start_bot(&e.app, &bot.id).await.unwrap();
    assert!(sent_effort_slash(&e).is_empty());
}
