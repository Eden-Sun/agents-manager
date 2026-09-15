//! Fork 一顆頂層 bot（SPEC §6.10）：開一顆同設定的新 bot，讓它的 CLI 從來源 bot 的對話分出一個新
//! session 接著做——完整的脈絡都在，但之後兩邊各走各的，互不影響。
//!
//! 跟「開同類分身」（前端 `cloneBot`）的差別只在脈絡：分身是全新對話，fork 帶著來源的整段對話。
//! claude／grok 用 `--resume <id> --fork-session`，codex 用 `codex fork <id>`（`lifecycle::fork_args_by_kind`）。

use crate::config::{valid_bot_name, BotCfg, BOT_NAME_RE, LOCAL_HOST};
use crate::db;
use crate::lifecycle::{self, LcError, StartOpts};
use crate::state::App;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Default, Deserialize)]
pub struct ForkReq {
    /// 省略＝`<來源>-fork`；撞名自動往後加 `-N`。
    #[serde(default)]
    pub name: Option<String>,
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// 來源 bot 最近一次有 native session 的 run（跑著的也算：fork 讀的是 CLI 自己的對話檔，不打擾原本那顆）。
async fn source_session(app: &Arc<App>, bot_id: &str) -> Result<Option<(String, Option<String>)>, LcError> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT native_session_id, transcript_path FROM runs
          WHERE bot_id = ? AND native_session_id IS NOT NULL AND native_session_id != ''
          ORDER BY started_at DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(&app.db)
    .await
    .map_err(up)
}

/// `alfa` → `alfa-fork`；名字上限 32 字，超過就先截來源名。
fn default_name(source: &str) -> String {
    const SUFFIX: &str = "-fork";
    let room = 32 - SUFFIX.chars().count();
    let base: String = source.chars().take(room).collect();
    format!("{base}{SUFFIX}")
}

/// `POST /api/bots/:id/fork` —— 建 bot（設定照抄來源，autostart 關）→ 以 fork 參數啟動。
/// 建好但啟動失敗時仍回 200，`start_error` 帶原因：bot 已經在側欄了，使用者要知道它為什麼沒起來。
pub async fn fork_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    body: Option<Json<ForkReq>>,
) -> Result<Response, LcError> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let source = db::bot(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if source.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    // 只給頂層 bot：child 的 pane 與帳號環境是母 agent 開的，daemon 重建不出來。
    if source.managed_by == "child" {
        return Err(LcError::conflict(
            "fork_child",
            json!({"message": "子 agent 不能 fork：它的 pane 與帳號環境是母 agent 開的。請 fork 它的母 bot。"}),
        ));
    }
    lifecycle::refuse_default_session(&source)?;
    let Some((session_id, transcript)) = source_session(&app, &source.id).await? else {
        return Err(LcError::conflict(
            "no_session",
            json!({"message": format!("{} 還沒有可以接續的對話（沒有記到 session），先跟它說過話再 fork。", source.name)}),
        ));
    };
    if let Err(why) = lifecycle::fork_args_by_kind(&source.kind, &session_id) {
        return Err(LcError::conflict(why, json!({"kind": source.kind})));
    }
    let project = db::project(&app.db, &source.project_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    // 本機才看得到對話檔；不在了 CLI 會說找不到對話然後退出，不如先講清楚。
    if project.host == LOCAL_HOST {
        if let Some(t) = transcript.as_deref().filter(|t| !t.trim().is_empty()) {
            if !std::path::Path::new(t).exists() {
                return Err(LcError::conflict("transcript_missing", json!({"session_id": session_id, "transcript_path": t})));
            }
        }
    }

    let wanted = match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => default_name(&source.name),
    };
    if !valid_bot_name(&wanted) {
        return Err(LcError::Bad(format!("bot name: {BOT_NAME_RE}")));
    }
    let new_id = db::ulid();
    let used_name = std::sync::Mutex::new(wanted.clone());
    let res = app
        .cfg
        .update(|cfg| {
            let p = cfg
                .projects
                .iter_mut()
                .find(|p| p.id.as_deref() == Some(source.project_id.as_str()))
                .ok_or_else(|| anyhow::anyhow!("not-in-config"))?;
            let at = p
                .bots
                .iter()
                .position(|b| b.id.as_deref() == Some(source.id.as_str()))
                .ok_or_else(|| anyhow::anyhow!("not-in-config"))?;
            let src: BotCfg = p.bots[at].clone();
            let taken = |n: &str| p.bots.iter().any(|x| x.name == n);
            let name = if taken(&wanted) { crate::api::next_free_name(&wanted, &taken) } else { wanted.clone() };
            *used_name.lock().unwrap() = name.clone();
            // 設定照抄（模型、強度、身份、env、人設、args）：同一個帳號目錄才找得到那段對話。
            // autostart 不抄——fork 是一次性的分岔，不該每次開 daemon 都多一顆。
            // 插在來源正下方（陣列位置＝側欄順序）：分出來的那顆要看得出是誰分的，不是掉到專案最底下（使用者 2026-09-15）。
            p.bots.insert(at + 1, BotCfg { id: Some(new_id.clone()), name, autostart: false, herdr_session: None, ..src });
            Ok(())
        })
        .await;
    match res {
        Ok(()) => {}
        Err(e) if e.to_string() == "not-in-config" => {
            return Err(LcError::conflict("not_in_config", json!({"bot_id": source.id})));
        }
        Err(e) => return Err(up(e)),
    }
    crate::projection::project_config(&app.cfg, &app.db).await.map_err(up)?;
    app.emit("bot_changed", json!({"bot_id": new_id})).await;
    let name = used_name.into_inner().unwrap_or_default();

    // 分叉之前的訊息不會複製過來（CLI 裡有，AG Man 的對話紀錄在來源那顆）。
    if let Ok(conv) = db::conversation_id(&app.db, &new_id).await {
        let note = format!(
            "從 {} fork 出來：接續它到目前為止的完整對話脈絡（{} session `{session_id}`），之後各走各的。分叉前的訊息請到 {} 看。",
            source.name, source.kind, source.name
        );
        let _ = lifecycle::insert_message(&app, &conv, None, "system", &note, "system", false, None).await;
    }

    let opts = StartOpts { fork_session: Some(session_id.clone()), ..Default::default() };
    let (run_id, start_error) = match lifecycle::start_bot_with(&app, &new_id, opts).await {
        Ok(run) => (Some(run), None),
        Err(e) => {
            tracing::warn!(source = %source.name, fork = %name, error = ?e, "forked bot was created but did not start");
            (None, Some(format!("{e:?}")))
        }
    };
    Ok((
        StatusCode::OK,
        Json(json!({
            "bot_id": new_id,
            "name": name,
            "forked_from": {"bot_id": source.id, "session_id": session_id},
            "run_id": run_id,
            "start_error": start_error,
        })),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{env, Env};
    use serde_json::Value;

    fn started_args(e: &Env) -> Vec<Vec<String>> {
        e.herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "agent.start")
            .filter_map(|(_, p)| p.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()))
            .collect()
    }

    /// 來源 bot 寫進 config（fork 從 TOML 抄設定）並投影，再給它一個有對話檔的 run。
    async fn source_bot(e: &Env, kind: &str, name: &str, managed_by: &str) -> String {
        let id = db::ulid();
        let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
        let mut cfg_bot: BotCfg = toml::from_str(&format!("id = '{id}'\nname = '{name}'\nkind = '{kind}'\n")).unwrap();
        cfg_bot.model = Some("opus".into());
        cfg_bot.persona = Some("審稿人".into());
        cfg_bot.env = [("FOO".to_string(), "bar".to_string())].into();
        cfg_bot.autostart = true;
        if managed_by == "user" {
            e.app
                .cfg
                .update(move |cfg| {
                    if cfg.projects.is_empty() {
                        cfg.projects.push(crate::config::ProjectCfg {
                            id: Some(pid),
                            path: repo,
                            label: "proj".into(),
                            host: LOCAL_HOST.into(),
                            bots: vec![],
                        });
                    }
                    cfg.projects[0].bots.push(cfg_bot);
                    Ok(())
                })
                .await
                .unwrap();
            crate::projection::project_config(&e.app.cfg, &e.app.db).await.unwrap();
        } else {
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
                 VALUES (?,?,?,?,'[]',0,1,'tok','child',?)",
            )
            .bind(&id)
            .bind(&e.project_id)
            .bind(name)
            .bind(kind)
            .bind(db::now())
            .execute(&e.app.db)
            .await
            .unwrap();
        }
        let transcript = e.dir.join(format!("{id}.jsonl"));
        std::fs::write(&transcript, "{}\n").unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle',?,?,?,?)",
        )
        .bind(db::ulid())
        .bind(&id)
        .bind(format!("sid-{name}"))
        .bind(transcript.to_string_lossy().to_string())
        .bind("2026-09-14T00:00:00Z")
        .bind("2026-09-14T00:01:00Z")
        .execute(&e.app.db)
        .await
        .unwrap();
        id
    }

    async fn fork(e: &Env, id: &str, name: Option<&str>) -> Result<Value, LcError> {
        let body = name.map(|n| Json(ForkReq { name: Some(n.into()) }));
        let res = fork_bot(State(e.app.clone()), Path(id.to_string()), body).await?;
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    fn reason(e: LcError) -> String {
        match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn provider_fork_arguments_are_exact() {
        assert_eq!(lifecycle::fork_args_by_kind("claude", "s1").unwrap(), vec!["--resume", "s1", "--fork-session"]);
        assert_eq!(lifecycle::fork_args_by_kind("grok", "s1").unwrap(), vec!["--resume", "s1", "--fork-session"]);
        assert_eq!(lifecycle::fork_args_by_kind("codex", "s1").unwrap(), vec!["fork", "s1"]);
        assert_eq!(lifecycle::fork_args_by_kind("claude", " "), Err("no_session_id"));
        assert_eq!(lifecycle::fork_args_by_kind("gemini", "s1"), Err("unsupported_kind"));
    }

    #[test]
    fn the_default_name_stays_within_32_characters() {
        assert_eq!(default_name("alfa"), "alfa-fork");
        let long = "x".repeat(32);
        assert_eq!(default_name(&long).chars().count(), 32);
        assert!(valid_bot_name(&default_name(&long)));
    }

    /// claude：新 bot 照抄設定（模型、人設、env），autostart 關，啟動參數帶 `--resume <來源 session> --fork-session`；
    /// 原本那顆的設定與對話不動。
    #[tokio::test]
    async fn a_claude_fork_is_a_new_bot_that_resumes_into_a_new_session() {
        let e = env().await;
        let src = source_bot(&e, "claude", "alfa", "user").await;

        let out = fork(&e, &src, None).await.unwrap();
        assert_eq!(out["name"], "alfa-fork");
        assert_eq!(out["forked_from"]["session_id"], "sid-alfa");
        assert!(out["start_error"].is_null(), "{out}");
        let new_id = out["bot_id"].as_str().unwrap().to_string();

        let args = started_args(&e).pop().unwrap();
        assert!(args.windows(3).any(|w| w == ["--resume", "sid-alfa", "--fork-session"]), "{args:?}");

        let cfg = e.app.cfg.get().await;
        let bots = &cfg.projects[0].bots;
        let (s, f) = (bots.iter().find(|b| b.id.as_deref() == Some(&src)).unwrap(), bots.iter().find(|b| b.id.as_deref() == Some(&new_id)).unwrap());
        assert_eq!((f.kind.as_str(), f.model.as_deref(), f.persona.as_deref()), ("claude", Some("opus"), Some("審稿人")));
        assert_eq!(f.env.get("FOO").map(String::as_str), Some("bar"));
        assert!(!f.autostart, "fork 不抄 autostart");
        assert!(s.autostart, "來源不動");

        // 新 bot 的對話裡有一則說明從哪裡分出來的系統訊息。
        let conv = db::conversation_id(&e.app.db, &new_id).await.unwrap();
        let text: String = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'system'")
            .bind(&conv)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert!(text.contains("alfa") && text.contains("sid-alfa"), "{text}");
    }

    /// fork 出來的 bot 排在來源正下方，不是專案最底下（config 陣列位置＝側欄順序）。
    #[tokio::test]
    async fn a_fork_lands_right_below_its_source() {
        let e = env().await;
        let first = source_bot(&e, "claude", "alfa", "user").await;
        source_bot(&e, "claude", "bravo", "user").await;
        source_bot(&e, "claude", "charlie", "user").await;
        let out = fork(&e, &first, None).await.unwrap();
        let names: Vec<String> = e.app.cfg.get().await.projects[0].bots.iter().map(|b| b.name.clone()).collect();
        assert_eq!(names, ["alfa", "alfa-fork", "bravo", "charlie"]);
        let position: i64 = sqlx::query_scalar("SELECT position FROM bots WHERE id = ?")
            .bind(out["bot_id"].as_str().unwrap())
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(position, 1, "DB 的 position 跟著投影");
    }

    /// codex：`fork` 是子命令，排在所有參數最前面；撞名自動加尾碼。
    #[tokio::test]
    async fn a_codex_fork_puts_the_subcommand_first() {
        let e = env().await;
        let src = source_bot(&e, "codex", "bravo", "user").await;
        let first = fork(&e, &src, Some("bravo-2nd")).await.unwrap();
        assert_eq!(first["name"], "bravo-2nd");
        let args = started_args(&e).pop().unwrap();
        assert_eq!(&args[..2], ["fork", "sid-bravo"], "{args:?}");

        let again = fork(&e, &src, Some("bravo-2nd")).await.unwrap();
        assert_eq!(again["name"], "bravo-2nd-1", "撞名自動往後加");
    }

    /// 子 agent、沒有 session、對話檔不在：都拒絕，而且不會先建出一顆 bot。
    #[tokio::test]
    async fn forks_that_cannot_continue_are_refused_before_creating_anything() {
        let e = env().await;
        let child = source_bot(&e, "claude", "kid", "child").await;
        assert_eq!(reason(fork(&e, &child, None).await.unwrap_err()), "fork_child");

        let fresh = source_bot(&e, "claude", "fresh", "user").await;
        sqlx::query("UPDATE runs SET native_session_id = NULL WHERE bot_id = ?").bind(&fresh).execute(&e.app.db).await.unwrap();
        assert_eq!(reason(fork(&e, &fresh, None).await.unwrap_err()), "no_session");

        let gone = source_bot(&e, "claude", "gone", "user").await;
        sqlx::query("UPDATE runs SET transcript_path = '/nonexistent/t.jsonl' WHERE bot_id = ?").bind(&gone).execute(&e.app.db).await.unwrap();
        assert_eq!(reason(fork(&e, &gone, None).await.unwrap_err()), "transcript_missing");

        let names: Vec<String> = e.app.cfg.get().await.projects[0].bots.iter().map(|b| b.name.clone()).collect();
        assert!(!names.iter().any(|n| n.ends_with("-fork")), "{names:?}");
    }
}
