use super::{pending_fork, start_bot_with, StartOpts};
use crate::capabilities::Db;
use crate::db;
use crate::testing::{claude_bot, env, Env};
use serde_json::Value;

fn started_args(e: &Env) -> Vec<Vec<String>> {
    e.herdr
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| method == "agent.start")
        .filter_map(|(_, params)| params.get("args").and_then(Value::as_array).map(|args| args.iter().filter_map(Value::as_str).map(String::from).collect()))
        .collect()
}

async fn codex_with_session(e: &Env, name: &str, sid: &str) -> db::Bot {
    let bot = claude_bot(&e.app, &e.project_id, name).await;
    sqlx::query("UPDATE bots SET kind='codex', identity=NULL WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
         VALUES (?,?,'stopped','idle',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
    )
    .bind(db::ulid())
    .bind(&bot.id)
    .bind(sid)
    .execute(e.app.db())
    .await
    .unwrap();
    db::bot(e.app.db(), &bot.id).await.unwrap().unwrap()
}

/// 測試的 bot 只在 DB；`fork_from` 住在 config，所以這裡照投影的樣子把它放進 config。
async fn set_fork_from(e: &Env, bot: &db::Bot, source: &str) {
    let (bot_id, name, project_id, source) = (bot.id.clone(), bot.name.clone(), bot.project_id.clone(), source.to_string());
    e.app.cfg.update(move |cfg| {
        if !cfg.projects.iter().any(|p| p.id.as_deref() == Some(project_id.as_str())) {
            cfg.projects.push(crate::config::ProjectCfg { id: Some(project_id.clone()), path: "/tmp".into(), label: "p".into(), host: crate::config::LOCAL_HOST.into(), ..Default::default() });
        }
        let p = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(project_id.as_str())).expect("pushed above");
        p.bots.retain(|b| b.id.as_deref() != Some(bot_id.as_str()));
        p.bots.push(crate::config::BotCfg { id: Some(bot_id), name, kind: "codex".into(), fork_from: Some(source), ..Default::default() });
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_first_start_forks_the_source_and_later_starts_resume_their_own_session() {
    let e = env().await;
    let source = codex_with_session(&e, "fork-src", "sid-src").await;
    let fresh = codex_with_session(&e, "fork-new", "unused").await;
    sqlx::query("DELETE FROM runs WHERE bot_id = ?").bind(&fresh.id).execute(e.app.db()).await.unwrap();
    set_fork_from(&e, &fresh, &source.id).await;

    assert_eq!(pending_fork(&e.app, &fresh).await.unwrap(), Some(("sid-src".into(), source.id.clone())));
    start_bot_with(&e.app, &fresh.id, StartOpts::default()).await.unwrap();
    let first = started_args(&e);
    let last = first.last().expect("the first start launches the CLI");
    assert!(last.windows(2).any(|w| w == ["fork", "sid-src"]), "第一次啟動用 fork <來源 session>：{last:?}");

    // 這顆有了自己的 session（啟動後 hook 回報）：之後不再分岔。
    sqlx::query("UPDATE runs SET ended_at = '2026-09-01T00:02:00Z', native_session_id = 'sid-own', state = 'stopped' WHERE bot_id = ?")
        .bind(&fresh.id)
        .execute(e.app.db())
        .await
        .unwrap();
    assert_eq!(pending_fork(&e.app, &fresh).await.unwrap(), None, "有自己的 session 就不再 fork 來源");
}

#[tokio::test]
async fn no_pending_fork_without_a_config_source_or_a_source_session() {
    let e = env().await;
    let bot = codex_with_session(&e, "fork-none", "sid-x").await;
    assert_eq!(pending_fork(&e.app, &bot).await.unwrap(), None, "沒有 fork_from");
    let no_session = codex_with_session(&e, "fork-empty-src", "sid-tmp").await;
    sqlx::query("DELETE FROM runs WHERE bot_id = ?").bind(&no_session.id).execute(e.app.db()).await.unwrap();
    let fresh = claude_bot(&e.app, &e.project_id, "fork-new2").await;
    set_fork_from(&e, &fresh, &no_session.id).await;
    assert_eq!(pending_fork(&e.app, &fresh).await.unwrap(), None, "來源沒有 session：不分岔");
}
