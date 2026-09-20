//! Fork 一顆頂層 bot（SPEC §6.10）：開一顆同設定的新 bot，讓它的 CLI 從來源 bot 的對話分出一個新
//! session 接著做——完整的脈絡都在，但之後兩邊各走各的，互不影響。
//!
//! 跟「開同類分身」（前端 `cloneBot`）的差別只在脈絡：分身是全新對話，fork 帶著來源的整段對話。
//! claude／grok 用 `--resume <id> --fork-session`，codex 用 `codex fork <id>`（`lifecycle::fork_args_by_kind`）。

use crate::config::{valid_bot_name, BotCfg, BOT_NAME_RE, LOCAL_HOST};
use crate::db;
use crate::fork_ops::{self, ForkOp};
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
    /// 冪等鍵（issue #348）：同一個 id 重送回同一個目標與結果；省略＝每次都是新的一次 fork。
    #[serde(default)]
    pub client_request_id: Option<String>,
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

/// 同一時間只處理一個 fork：重送與原請求並發時，後到的等前一個做完再看它留下的紀錄（fork 很少見，不需要更細的鎖）。
static FORK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `POST /api/bots/:id/fork` —— 建 bot（設定照抄來源，autostart 關）→ 以 fork 參數啟動。
/// 建好但啟動失敗時仍回 200，`start_error` 帶原因：bot 已經在側欄了，使用者要知道它為什麼沒起來。
///
/// 冪等（issue #348）：body 帶 `client_request_id`，同一個 id 重送（回應遺失、daemon 中途死掉）回同一個目標與結果，
/// 不會再建一顆或再分岔一次；同一個 id 但來源或名字不同 → 409 `request_mismatch`。想再 fork 一次就換一個 id。
pub async fn fork_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    body: Option<Json<ForkReq>>,
) -> Result<Response, LcError> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let request_id = req.client_request_id.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(String::from).unwrap_or_else(db::ulid);
    let requested = req.name.as_deref().map(str::trim).unwrap_or("").to_string();
    let _serial = FORK_LOCK.lock().await;

    // 先看這個請求是不是已經做過（或做到一半）：DB 讀不到就是錯誤，不能當成新請求再建一顆。
    if let Some(op) = fork_ops::get(&app.db, &request_id).await.map_err(up)? {
        if op.source_bot_id != id || op.requested_name != requested {
            return Err(LcError::conflict(
                "request_mismatch",
                json!({"client_request_id": request_id, "message": "這個 client_request_id 已經用在另一個 fork 請求（來源或名字不同）。要再 fork 一次請換一個 id。"}),
            ));
        }
        return finish(&app, op).await;
    }

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

    let wanted = if requested.is_empty() { default_name(&source.name) } else { requested.clone() };
    if !valid_bot_name(&wanted) {
        return Err(LcError::Bad(format!("bot name: {BOT_NAME_RE}")));
    }
    // 先把「要建誰、從哪個 session 分」記下來再動 config：之後任何一步中斷，重送都拿同一個目標 id 接著做。
    let op = ForkOp {
        client_request_id: request_id,
        source_bot_id: source.id.clone(),
        requested_name: requested,
        target_bot_id: db::ulid(),
        session_id,
        name: wanted,
        state: "planned".into(),
        run_id: None,
        start_error: None,
    };
    fork_ops::insert(&app.db, &op).await.map_err(up)?;
    finish(&app, op).await
}

/// 把一筆 fork 操作從它停下的那一步做完（新請求與重送共用）：每一步都先看做過沒有。
async fn finish(app: &Arc<App>, mut op: ForkOp) -> Result<Response, LcError> {
    let source = db::bot(&app.db, &op.source_bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let new_id = op.target_bot_id.clone();

    if op.state == "planned" {
        let used_name = std::sync::Mutex::new(op.name.clone());
        let wanted = op.name.clone();
        let res = crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
            let p = cfg
                .projects
                .iter_mut()
                .find(|p| p.id.as_deref() == Some(source.project_id.as_str()))
                .ok_or_else(|| anyhow::anyhow!("not-in-config"))?;
            // 上次已經寫進 config（寫完、記狀態之前死掉）：沿用那一顆，不再插第二顆。
            if let Some(existing) = p.bots.iter().find(|b| b.id.as_deref() == Some(new_id.as_str())) {
                *used_name.lock().unwrap() = existing.name.clone();
                return Ok(());
            }
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
        op.name = used_name.into_inner().unwrap_or_default();
        app.emit("bot_changed", json!({"bot_id": new_id})).await;

        // 分叉之前的訊息不會複製過來（CLI 裡有，AG Man 的對話紀錄在來源那顆）。
        if let Ok(conv) = db::conversation_id(&app.db, &new_id).await {
            let note = format!(
                "從 {} fork 出來：接續它到目前為止的完整對話脈絡（{} session `{}`），之後各走各的。分叉前的訊息請到 {} 看。",
                source.name, source.kind, op.session_id, source.name
            );
            // 中斷後重送會再走到這裡：說明已經寫過就不再寫一則。
            let seen: Option<i64> = sqlx::query_scalar("SELECT 1 FROM messages WHERE conversation_id = ? AND role = 'system' AND content = ? LIMIT 1")
                .bind(&conv)
                .bind(&note)
                .fetch_optional(&app.db)
                .await
                .map_err(up)?;
            if seen.is_none() {
                let _ = lifecycle::insert_message(app, &conv, None, "system", &note, "system", false, None).await;
            }
        }
        fork_ops::set_state(&app.db, &op.client_request_id, "created", &op.name, None, None).await.map_err(up)?;
        op.state = "created".into();
    }

    if op.state == "created" {
        // 啟動送出後、記結果之前死掉：目標已經有 run 就是啟動過了，再開一次會對 provider 再分岔一次。
        let existing: Option<String> = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id = ? ORDER BY started_at DESC, id DESC LIMIT 1")
            .bind(&new_id)
            .fetch_optional(&app.db)
            .await
            .map_err(up)?;
        let (run_id, start_error) = match existing {
            Some(run) => (Some(run), None),
            None => {
                let opts = StartOpts { fork_session: Some(op.session_id.clone()), ..Default::default() };
                match lifecycle::start_bot_with(app, &new_id, opts).await {
                    Ok(run) => (Some(run), None),
                    Err(e) => {
                        tracing::warn!(source = %source.name, fork = %op.name, error = ?e, "forked bot was created but did not start");
                        (None, Some(format!("{e:?}")))
                    }
                }
            }
        };
        let state = if run_id.is_some() { "started" } else { "failed" };
        fork_ops::set_state(&app.db, &op.client_request_id, state, &op.name, run_id.as_deref(), start_error.as_deref()).await.map_err(up)?;
        op.state = state.into();
        op.run_id = run_id;
        op.start_error = start_error;
    } else {
        // 已經有結果的重送：目標被刪掉了就明說，不假裝還在。
        match db::bot(&app.db, &new_id).await.map_err(up)? {
            Some(b) if b.deleted_at.is_none() => {}
            _ => return Err(LcError::conflict("fork_target_deleted", json!({"bot_id": new_id, "client_request_id": op.client_request_id}))),
        }
    }
    Ok((
        StatusCode::OK,
        Json(json!({
            "bot_id": new_id,
            "name": op.name,
            "forked_from": {"bot_id": op.source_bot_id, "session_id": op.session_id},
            "run_id": op.run_id,
            "start_error": op.start_error,
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
        fork_with(e, id, name, None).await
    }

    async fn fork_with(e: &Env, id: &str, name: Option<&str>, request_id: Option<&str>) -> Result<Value, LcError> {
        let body = Some(Json(ForkReq { name: name.map(String::from), client_request_id: request_id.map(String::from) }));
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
    fn fork_starts(e: &Env) -> usize {
        started_args(e).iter().filter(|a| a.iter().any(|x| x == "--fork-session")).count()
    }

    async fn fork_bot_count(e: &Env) -> usize {
        e.app.cfg.get().await.projects[0].bots.len()
    }

    /// #348：回應遺失後用同一個 request id 重送——同一個目標、同一個結果，只 fork 一次。
    #[tokio::test]
    async fn a_retry_with_the_same_request_id_returns_the_same_fork() {
        let e = env().await;
        let src = source_bot(&e, "claude", "alfa", "user").await;
        let first = fork_with(&e, &src, None, Some("req-1")).await.unwrap();
        let again = fork_with(&e, &src, None, Some("req-1")).await.unwrap();
        assert_eq!(first, again, "重送拿到原本的結果");
        assert_eq!(fork_bot_count(&e).await, 2, "只有來源與一顆 fork");
        assert_eq!(fork_starts(&e), 1, "provider 只被分岔一次");
        // 明確的第二次 fork：換一個 id 才會有。
        let second = fork_with(&e, &src, None, Some("req-2")).await.unwrap();
        assert_ne!(second["bot_id"], first["bot_id"]);
        assert_eq!(second["name"], "alfa-fork-1");
    }

    /// 同一個 id、來源或名字不同：409，什麼都不建。
    #[tokio::test]
    async fn the_same_request_id_with_different_parameters_is_a_mismatch() {
        let e = env().await;
        let src = source_bot(&e, "claude", "alfa", "user").await;
        let other = source_bot(&e, "claude", "bravo", "user").await;
        fork_with(&e, &src, Some("one"), Some("req-1")).await.unwrap();
        let n = fork_bot_count(&e).await;
        assert_eq!(reason(fork_with(&e, &src, Some("two"), Some("req-1")).await.unwrap_err()), "request_mismatch");
        assert_eq!(reason(fork_with(&e, &other, Some("one"), Some("req-1")).await.unwrap_err()), "request_mismatch");
        assert_eq!(fork_bot_count(&e).await, n);
        assert_eq!(fork_starts(&e), 1);
    }

    fn planned_op(src: &str, target: &str) -> ForkOp {
        ForkOp {
            client_request_id: "req-crash".into(),
            source_bot_id: src.into(),
            requested_name: String::new(),
            target_bot_id: target.into(),
            session_id: "sid-alfa".into(),
            name: "alfa-fork".into(),
            state: "planned".into(),
            run_id: None,
            start_error: None,
        }
    }

    /// 只記了操作、config 還沒寫就死掉：重送用**記下的**目標 id 建出來，不另配一個。
    #[tokio::test]
    async fn a_crash_before_the_config_write_converges_on_the_recorded_target() {
        let e = env().await;
        let src = source_bot(&e, "claude", "alfa", "user").await;
        let target = db::ulid();
        fork_ops::insert(&e.app.db, &planned_op(&src, &target)).await.unwrap();
        let out = fork_with(&e, &src, None, Some("req-crash")).await.unwrap();
        assert_eq!(out["bot_id"], target.as_str());
        assert_eq!(fork_bot_count(&e).await, 2);
        assert_eq!(fork_starts(&e), 1);
    }

    /// bot 已建好、還沒啟動就死掉（config 有、op 還是 planned／created）：重送接著啟動同一顆，不再插第二顆。
    #[tokio::test]
    async fn a_crash_after_the_config_write_before_the_start_continues_the_same_bot() {
        let e = env().await;
        let src = source_bot(&e, "claude", "alfa", "user").await;
        let target = db::ulid();
        let t = target.clone();
        e.app
            .cfg
            .update(move |cfg| {
                let bots = &mut cfg.projects[0].bots;
                let src_cfg = bots[0].clone();
                bots.push(BotCfg { id: Some(t), name: "alfa-fork".into(), autostart: false, ..src_cfg });
                Ok(())
            })
            .await
            .unwrap();
        crate::projection::project_config(&e.app.cfg, &e.app.db).await.unwrap();
        fork_ops::insert(&e.app.db, &planned_op(&src, &target)).await.unwrap();
        let out = fork_with(&e, &src, None, Some("req-crash")).await.unwrap();
        assert_eq!(out["bot_id"], target.as_str());
        assert_eq!(fork_bot_count(&e).await, 2, "沒有插第二顆");
        assert_eq!(fork_starts(&e), 1);
    }

    /// 啟動成功、回應與結果紀錄之前死掉：目標已經有 run，重送回那個 run，不再對 provider 分岔一次。
    #[tokio::test]
    async fn a_crash_after_a_successful_start_does_not_fork_again() {
        let e = env().await;
        let src = source_bot(&e, "claude", "alfa", "user").await;
        let first = fork_with(&e, &src, None, Some("req-crash")).await.unwrap();
        let (target, run) = (first["bot_id"].as_str().unwrap().to_string(), first["run_id"].as_str().unwrap().to_string());
        // 把紀錄倒回「啟動完、還沒記結果」。
        fork_ops::set_state(&e.app.db, "req-crash", "created", "alfa-fork", None, None).await.unwrap();
        let again = fork_with(&e, &src, None, Some("req-crash")).await.unwrap();
        assert_eq!((again["bot_id"].as_str().unwrap(), again["run_id"].as_str().unwrap()), (target.as_str(), run.as_str()));
        assert_eq!(fork_starts(&e), 1, "沒有第二次分岔");
    }
}
