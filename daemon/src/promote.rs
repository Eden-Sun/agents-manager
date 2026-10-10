//! 子 agent 升級成頂層 bot（SPEC §6.10a，issue #248）：保留同一段 claude 對話，讓它從母 bot 底下的 child
//! 變成 `managed_by = "user"`、進 config.toml 的頂層 bot。
//!
//! 對話能保留靠的是 claude 的 `--resume <session>`：child 沒有 hook，`run.native_session_id` 是 null，所以
//! session 要從 pane 的行程找——`<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` 記著那顆行程的 sessionId 與 cwd，
//! transcript 在 `<CLAUDE_CONFIG_DIR>/projects/<cwd 編碼>/<sessionId>.jsonl`。新 bot 的 cwd 是專案路徑（不是 child
//! 的 worktree），CLI 只會去新 cwd 對應的 projects 目錄找，所以要把 transcript **複製**過去（不搬、不覆寫）。
//!
//! 順序刻意把「回不去的一步」排最後，前面每一步失敗都原樣收回：
//! 1. 驗證（只收 child、claude、本機、沒有孫 agent、名字可用）→ 2. 找 session 與 transcript →
//! 3. 複製 transcript → 4. 停掉 child（停不掉就收回複製、不動）→ 5. 建 user bot 並種下 session →
//! 6. native resume 啟動（失敗就把新 bot 與複製收回）→ 7. 收掉 child 紀錄。
//!
//! child 的 pane 是母 agent 開的、daemon 重開不了它，所以第 4 步之後 child 不會自己回來；但 session 已經記在
//! child 最後一個 run 上（步驟 4 之前寫），同一個請求可以原樣再送一次。

use crate::config::{valid_bot_name, BotCfg, BOT_NAME_RE, LOCAL_HOST};
use crate::db;
use crate::lifecycle::{self, LcError, StartOpts};
use crate::state::App;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

#[derive(Debug, Default, Deserialize)]
pub struct PromoteReq {
    /// 省略＝沿用 child 的名字（撞名就往後加 `-N`）。
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

fn refuse(reason: &str, extra: serde_json::Value) -> LcError {
    LcError::conflict(reason, extra)
}

/// claude 把 cwd 編成 projects 下的目錄名：每個非英數字元換成 `-`。
pub(crate) fn cwd_key(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

#[derive(Debug, Clone)]
struct Located {
    session_id: String,
    /// 來源 transcript（child 那邊，已確認存在）。
    src: PathBuf,
}

fn config_dir_of(env: &std::collections::BTreeMap<String, String>) -> Option<String> {
    let home = dirs::home_dir()?.to_string_lossy().to_string();
    Some(match env.get("CLAUDE_CONFIG_DIR").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(d) => crate::config::expand_home(d, &home),
        None => format!("{home}/.claude"),
    })
}

/// 從 pane 裡活著的 claude 行程找 session：`sessions/<pid>.json` 的 pid 要對得上、sessionId 與 cwd 不能空，
/// transcript 檔要在。找不到、或找到不只一段都回 `None`（fail closed）。
async fn locate_live(app: &Arc<App>, run: &db::Run) -> Result<Located, LcError> {
    let pane = run.pane_id.clone().filter(|p| !p.is_empty()).ok_or_else(|| refuse("session_not_found", json!({"why": "no_pane"})))?;
    let client = app.herdr_for_run(run).await.ok_or_else(|| LcError::Upstream("herdr unavailable".into()))?;
    let procs = client.pane_process_info(&pane).await.map_err(up)?;
    let reader = app.proc_env.reader();
    let mut found: Vec<Located> = Vec::new();
    for p in procs {
        let Some(pid) = p.pid.filter(|p| *p > 0) else { continue };
        let Some(env) = reader.env_of(app, LOCAL_HOST, pid).await else { continue };
        let Some(dir) = config_dir_of(&env) else { continue };
        let session_path = FsPath::new(&dir).join("sessions").join(format!("{pid}.json"));
        let Ok(mut session_file) = crate::transcript_read::open_regular(&session_path) else { continue };
        let Ok(bytes) = crate::trusted_open::read_limited(&mut session_file, 1024 * 1024) else { continue };
        let Ok(raw) = String::from_utf8(bytes) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
        if v.get("pid").and_then(|x| x.as_i64()) != Some(pid) {
            continue;
        }
        let sid = v.get("sessionId").and_then(|x| x.as_str()).map(str::trim).unwrap_or("");
        let cwd = v.get("cwd").and_then(|x| x.as_str()).map(str::trim).unwrap_or("");
        if sid.is_empty() || cwd.is_empty() || !sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }
        let src = FsPath::new(&dir).join("projects").join(cwd_key(cwd)).join(format!("{sid}.jsonl"));
        if src.is_file() && !found.iter().any(|f| f.session_id == sid) {
            found.push(Located { session_id: sid.to_string(), src });
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(refuse("session_not_found", json!({"message": "找不到這顆子 agent 的 claude session（pane 裡沒有可對上的 claude 行程或 transcript）。"}))),
        _ => Err(refuse("session_ambiguous", json!({"sessions": found.iter().map(|f| f.session_id.clone()).collect::<Vec<_>>()}))),
    }
}

/// 上一次升級做到一半留下的紀錄（步驟 4 之前寫在 child 最後一個 run 上）。
async fn locate_recorded(app: &impl crate::capabilities::Db, bot_id: &str) -> Result<Located, LcError> {
    let last = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT native_session_id, transcript_path FROM runs
          WHERE bot_id = ? AND native_session_id IS NOT NULL AND native_session_id != '' AND transcript_path IS NOT NULL
          ORDER BY started_at DESC, rowid DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(app.db())
    .await
    .map_err(up)?;
    match last {
        Some((sid, Some(t))) if FsPath::new(&t).is_file() => Ok(Located { session_id: sid, src: PathBuf::from(t) }),
        _ => Err(refuse("session_not_found", json!({"message": "子 agent 已經停了，而且沒有記到它的 claude session。"}))),
    }
}

#[derive(Debug, Default)]
struct Staged {
    /// 這次真的新建的檔（收回時只刪這些）。
    created: Vec<PathBuf>,
    dest: PathBuf,
}

impl Staged {
    fn undo(&self) {
        let Some(cwd_dir) = self.dest.parent() else { return };
        let Some(projects_dir) = cwd_dir.parent() else { return };
        let Some(config_dir) = projects_dir.parent() else { return };
        let Some(cwd_key) = cwd_dir.file_name() else { return };
        let parts = [std::ffi::OsStr::new("projects"), cwd_key];
        let Ok(parent) = crate::trusted_open::open_bound_dir(config_dir, &parts, None) else { return };
        for p in self.created.iter().rev() {
            if let Some(name) = p.file_name() { let _ = crate::trusted_open::remove_tree_in(&parent, name); }
        }
    }
}

/// 複製到 `dest_dir/<sid>.jsonl`：來源與目標是同一個檔就什麼都不做；目標已存在且內容相同視為上次留下的
/// （冪等）；內容不同就拒絕，不覆寫。
fn stage_transcript(src: &FsPath, dest_dir: &FsPath, session_id: &str) -> Result<Staged, LcError> {
    use lifecycle::transcript_stage as ts;
    let dest = dest_dir.join(format!("{session_id}.jsonl"));
    let mut staged = Staged { created: vec![], dest: dest.clone() };
    let bad = |e: std::io::Error| refuse("transcript_copy_failed", json!({"message": e.to_string()}));
    if !ts::is_claude_transcript(src) {
        return Err(refuse("transcript_copy_failed", json!({"message": "來源不是 projects/<cwd>/<id>.jsonl 的一般檔"})));
    }
    let Some(projects_dir) = dest_dir.parent().filter(|p| p.file_name() == Some(std::ffi::OsStr::new("projects"))) else {
        return Err(refuse("transcript_copy_failed", json!({"message": "目的目錄不是 projects/<cwd>"})));
    };
    let Some(config_dir) = projects_dir.parent() else { return Err(refuse("transcript_copy_failed", json!({"message": "目的 projects 沒有帳號根目錄"}))) };
    let Some(cwd_key) = dest_dir.file_name() else { return Err(refuse("transcript_copy_failed", json!({"message": "目的 cwd 不存在"}))) };
    let parts = [std::ffi::OsStr::new("projects"), cwd_key];
    let dest_fd = crate::trusted_open::create_private_bound_dirs(config_dir, &parts).map_err(bad)?;
    let src_fd = crate::transcript_read::open_regular(src).map_err(bad)?;
    let dest_name = dest.file_name().ok_or_else(|| refuse("transcript_copy_failed", json!({"message": "目的 transcript 名稱不存在"})))?;
    let current = match crate::trusted_open::open_entry_in(&dest_fd, dest_name) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(bad(e)),
    };
    if let Some(existing) = current {
        let (sm, dm) = (src_fd.metadata().map_err(bad)?, existing.metadata().map_err(bad)?);
        if sm.dev() == dm.dev() && sm.ino() == dm.ino()
            || sm.len() == dm.len() && ts::is_prefix_of(&src_fd, &existing).map_err(bad)?
        {
            return Ok(staged);
        }
        return Err(refuse("transcript_exists", json!({"path": dest.to_string_lossy()})));
    }
    let mut tmp_name = std::ffi::OsString::from(".promote-");
    tmp_name.push(db::ulid());
    tmp_name.push(".tmp");
    let copied = (|| {
        use std::io::Write as _;
        let mut from = src_fd;
        let mut to = crate::trusted_open::create_new_file_in(&dest_fd, &tmp_name, 0o600)?;
        std::io::copy(&mut from, &mut to)?;
        to.flush()?;
        to.sync_all()?;
        crate::trusted_open::link_in(&dest_fd, &tmp_name, dest_name)?;
        crate::trusted_open::unlink_in(&dest_fd, &tmp_name)
    })();
    if let Err(e) = copied {
        let _ = crate::trusted_open::unlink_in(&dest_fd, &tmp_name);
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(refuse("transcript_exists", json!({"path": dest.to_string_lossy()})));
        }
        return Err(bad(e));
    }
    staged.created.push(dest);
    // 同名附屬目錄（subagent 對話等）：沒有就跳過，複製失敗不影響主對話，但不留下半份目錄。
    if let (Some(parent), Some(stem)) = (src.parent(), src.file_stem()) {
        let cs = parent.join(stem);
        let source_is_dir = std::fs::symlink_metadata(&cs).is_ok_and(|m| m.file_type().is_dir());
        let destination_exists = crate::trusted_open::open_dir_entry_in(&dest_fd, stem).is_ok()
            || crate::trusted_open::open_entry_in(&dest_fd, stem).is_ok();
        if source_is_dir && !destination_exists {
            let cd = dest_dir.join(stem);
            match ts::copy_dir_private(&cs, &cd) {
                Ok(()) => staged.created.push(cd),
                Err(_) => {
                    let _ = crate::trusted_open::remove_tree_in(&dest_fd, stem);
                }
            }
        }
    }
    Ok(staged)
}

/// 把 session 記在 child 的 run 上。已經有記錄就必須是同一段 session、transcript 還在；否則不放行。
async fn checkpoint_session(app: &impl crate::capabilities::Db, r: &db::Run, located: &Located) -> Result<(), String> {
    let res = sqlx::query("UPDATE runs SET native_session_id = ?, transcript_path = ? WHERE id = ? AND (native_session_id IS NULL OR native_session_id = '')")
        .bind(&located.session_id)
        .bind(located.src.to_string_lossy().to_string())
        .bind(&r.id)
        .execute(app.db())
        .await
        .map_err(|e| e.to_string())?;
    if res.rows_affected() > 0 {
        return Ok(());
    }
    // 沒更新到任何列：只有這個 run 早就記著同一段 session 才算數。
    let (sid, path) = sqlx::query_as::<_, (Option<String>, Option<String>)>("SELECT native_session_id, transcript_path FROM runs WHERE id = ?")
        .bind(&r.id)
        .fetch_optional(app.db())
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "run 不見了".to_string())?;
    match (sid, path) {
        (Some(s), Some(p)) if s == located.session_id && FsPath::new(&p).is_file() => Ok(()),
        _ => Err("child 的 run 上沒有可用的 session 記錄".to_string()),
    }
}

/// 新 bot 起來之後才失敗的收回：停掉它、從 config 拿掉、複製的檔刪掉。回傳是否收乾淨。
async fn roll_back_new_bot(app: &Arc<App>, new_id: &str, staged: &Staged) -> bool {
    let _ = lifecycle::stop_bot(app, new_id).await;
    let removed = crate::projection::delete_from_config(&app.cfg, &app.db, crate::projection::DeleteTarget::Bot(new_id)).await;
    if removed.is_ok() {
        staged.undo();
    }
    app.emit("bot_changed", json!({"bot_id": new_id})).await;
    removed.is_ok()
}

/// 升級出來的 user bot 的設定（設定從 child 帶：模型、強度、身分；args 不帶——那是收編時看到的舊 argv）。
/// handler 與開機補完（`promote_intents`）共用同一份，補完出來的跟 handler 做的一模一樣。
pub(crate) fn user_bot_cfg(child: &db::Bot, new_id: &str, name: &str, model: &Option<String>, effort: &Option<String>) -> BotCfg {
    BotCfg {
        id: Some(new_id.to_string()),
        name: name.to_string(),
        kind: "claude".into(),
        model: model.as_deref().map(|m| crate::models::canonical_model("claude", m).to_string()),
        effort: effort.clone(),
        fast: false,
        persona: child.persona.clone(),
        args: vec![],
        autostart: false,
        inject_hooks: true,
        auto_approve: child.auto_approve != 0,
        identity: child.identity.clone(),
        env: child.env(),
        herdr_session: None,
        create_request_id: None,
        create_fingerprint: None,
    }
}

/// `POST /api/bots/:id/promote` —— 見模組說明。
pub async fn promote_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<crate::api::RequestPrincipal>,
    body: Option<Json<PromoteReq>>,
) -> Result<Response, LcError> {
    if principal != crate::api::RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let lock = app.bot_lock(&id).await;
    let _g = lock.lock().await;

    let child = db::bot(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if child.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    if child.managed_by != "child" {
        return Err(refuse("not_child", json!({"message": "只有子 agent 需要升級；這顆本來就是頂層 bot。"})));
    }
    if child.kind != "claude" {
        return Err(refuse("unsupported_kind", json!({"kind": child.kind})));
    }
    lifecycle::refuse_default_session(&child)?;
    let project = db::project(&app.db, &child.project_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    if project.host != LOCAL_HOST {
        return Err(refuse("remote_not_supported", json!({"host": project.host})));
    }
    if !crate::runners::app_ports_p11::descendant_children(&app, &id).await.map_err(up)?.is_empty() {
        return Err(refuse("has_children", json!({"message": "它自己還有子 agent；先處理掉孫 agent 再升級。"})));
    }

    // 名字、模型、強度：全部在動任何東西之前驗完。
    let live_names: Vec<String> = db::live_bots(&app.db).await.map_err(up)?.into_iter().filter(|b| b.project_id == child.project_id).map(|b| b.name).collect();
    let cfg_names: Vec<String> = app
        .cfg
        .get()
        .await
        .projects
        .iter()
        .find(|p| p.id.as_deref() == Some(child.project_id.as_str()))
        .map(|p| p.bots.iter().map(|b| b.name.clone()).collect())
        .ok_or_else(|| refuse("not_in_config", json!({"project_id": child.project_id})))?;
    let taken = |n: &str| live_names.iter().chain(cfg_names.iter()).any(|x| x == n);
    let explicit = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let wanted = explicit.unwrap_or(child.name.as_str()).to_string();
    if !valid_bot_name(&wanted) {
        return Err(LcError::Bad(format!("bot name: {BOT_NAME_RE}")));
    }
    let name = if !taken(&wanted) {
        wanted
    } else if explicit.is_some() {
        return Err(refuse("bot name already in use", json!({"name": wanted})));
    } else {
        // 預設沿用 child 的名字，而它自己還占著這個名字（同專案 live 唯一），所以退到 `-N`。
        crate::runners::app_ports_p11::next_free_name(&wanted, &taken)
    };
    let requested_model = req.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).or(child.model.as_deref());
    let remapped_model = requested_model.and_then(|m| crate::models::remap_deprecated_model("claude", m).map(|to| (m.to_string(), to.to_string())));
    let model = requested_model.map(|m| crate::models::canonical_model("claude", m).to_string());
    let effort = crate::config::normalize_effort("claude", req.effort.as_deref().or(child.effort.as_deref())).map_err(LcError::Bad)?;

    // 找 session：活著就看 pane 的行程；已經停了（上一次做到一半）就用記下來的。
    let run = db::active_run(&app.db, &id).await.map_err(up)?;
    let located = match &run {
        Some(r) => locate_live(&app, r).await?,
        None => locate_recorded(&app, &id).await?,
    };
    let dest_dir = FsPath::new(&lifecycle::identity_config_dir(&app, LOCAL_HOST, child.identity.as_deref()).await.map_err(up)?)
        .join("projects")
        .join(cwd_key(&project.path));

    // 3. 複製 transcript。
    // transcript 可能有幾百 MB：複製放到 blocking pool，不占 tokio worker。
    let staged = {
        let (src, dest_dir, sid) = (located.src.clone(), dest_dir.clone(), located.session_id.clone());
        tokio::task::spawn_blocking(move || stage_transcript(&src, &dest_dir, &sid))
            .await
            .map_err(|e| refuse("transcript_copy_failed", json!({"message": e.to_string()})))??
    };

    // 持久 intent（#355 P4）：第一個回不去的一步（停 child）**之前**先 commit；承諾點＝目標 user bot 進 config。承諾點之後 daemon 死掉，
    // 開機由 `promote_intents::recover_host` **往前補完**（不回滾，使用者 2026-09-20 裁示）；停 child 之前死掉＝`abandoned`（收回複製）。寫不進去就不繼續。
    let new_id = db::ulid();
    let payload = json!({
        "child_id": id, "new_id": new_id, "project_id": child.project_id, "name": name, "model": model, "effort": effort,
        "session_id": located.session_id, "dest": staged.dest.to_string_lossy(),
        "created": staged.created.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
    });
    let intent = match crate::delete_intents::begin(&app, "promote", &id, LOCAL_HOST, &payload).await {
        Ok(i) => i,
        Err(e) => {
            staged.undo();
            return Err(e);
        }
    };
    #[cfg(test)]
    lifecycle::race_point::hit("promote_after_intent", &id).await;

    // 4. 停 child：先把 session 記在它的 run 上（讓這個請求停到一半還能重送），停不掉就收回複製、不動。
    if let Some(r) = &run {
        // 檢查點是停 child 的硬前提：寫不進去（或寫進去的不是這段 session）就收回複製、child 照跑，可重送。
        if let Err(why) = checkpoint_session(&app, r, &located).await {
            staged.undo();
            crate::delete_intents::abandon(&app, &intent, "checkpoint failed; rolled back").await;
        return Err(refuse("promote_checkpoint_failed", json!({"message": why, "child_stopped": false})));
        }
        let stopped = lifecycle::stop_bot_locked(&app, &id).await;
        let still = db::active_run(&app.db, &id).await;
        if stopped.is_err() || !matches!(still, Ok(None)) {
            staged.undo();
        crate::delete_intents::abandon(&app, &intent, "stop failed; rolled back").await;
            return Err(refuse("stop_failed", json!({"message": "子 agent 停不掉，什麼都沒動。"})));
        }
    }
    #[cfg(test)]
    lifecycle::race_point::hit("promote_after_stop", &id).await;

    // 5. 建 user bot（設定見 `user_bot_cfg`）。
    let created = crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
        let p = cfg
            .projects
            .iter_mut()
            .find(|p| p.id.as_deref() == Some(child.project_id.as_str()))
            .ok_or_else(|| anyhow::anyhow!("not-in-config"))?;
        p.bots.push(user_bot_cfg(&child, &new_id, &name, &model, &effort));
        Ok(())
    })
    .await;
    if let Err(e) = created {
        staged.undo();
        crate::delete_intents::abandon(&app, &intent, "create failed; rolled back").await;
        return Err(refuse("promote_create_failed", json!({"message": e.to_string(), "child_stopped": run.is_some()})));
    }

    #[cfg(test)]
    lifecycle::race_point::hit("promote_after_create", &id).await;

    // 種下 session：native resume 讀「這顆 bot 最近一個結束的 run 的 native_session_id」。
    let seeded = crate::promote_intents::seed_session_run(&app.db, &id, &new_id, &located.session_id, &staged.dest.to_string_lossy()).await;
    let mut failure: Option<String> = seeded.err().map(|e| e.to_string());
    let mut run_id = None;
    if failure.is_none() {
        // 6. native resume：接不回原本的對話就整個不啟動。
        match lifecycle::start_bot_with(&app, &new_id, StartOpts { resume_native: true, resume_required: true, ..Default::default() }).await {
            Ok(r) => run_id = Some(r),
            Err(e) => failure = Some(format!("{e:?}")),
        }
    }
    if let Some(why) = failure {
        // 收回：child 已經停了，紀錄還在，可以重送。
        let rolled_back = roll_back_new_bot(&app, &new_id, &staged).await;
        crate::delete_intents::abandon(&app, &intent, "start failed; rolled back").await;
        return Err(refuse(
            "promote_start_failed",
            json!({"message": why, "child_stopped": run.is_some(), "rolled_back": rolled_back}),
        ));
    }

    #[cfg(test)]
    lifecycle::race_point::hit("promote_after_start", &id).await;

    // 分叉前的訊息留在 child 那顆的紀錄裡；新 bot 的對話寫一則說明。
    if let Ok(conv) = db::conversation_id(&app.db, &new_id).await {
        let note = format!("從子 agent {} 升級成頂層 bot：接續它的 claude session `{}`。", child.name, located.session_id);
        let _ = lifecycle::insert_message(&app, &conv, None, "system", &note, "system", false, None).await;
    }

    // 7. 收掉 child 紀錄（它的 run 已經停了）。瞬間的 DB 錯誤重試幾次；還是寫不進去就把新 bot 收回，
    // 不留「兩顆都活著」的半套——child 已停、session 記在它的 run 上，同一個請求可以原樣重送。
    let mut retired = Err(String::new());
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        retired = crate::child_retire::retire(&app, &id, "promoted", crate::child_retire::Mode::Explicit).await.map(|_| ()).map_err(|e| e.to_string());
        if retired.is_ok() {
            break;
        }
    }
    if let Err(why) = retired {
        let rolled_back = roll_back_new_bot(&app, &new_id, &staged).await;
        crate::delete_intents::abandon(&app, &intent, "child not removed; rolled back").await;
        return Err(refuse("promote_child_not_removed", json!({"message": why, "child_id": id, "rolled_back": rolled_back})));
    }
    lifecycle::purge_bot_dir(&app, &id, LOCAL_HOST).await;
    crate::delete_intents::complete(&app, &intent).await;
    app.emit("bot_changed", json!({"bot_id": id})).await;
    app.emit("bot_changed", json!({"bot_id": new_id})).await;
    app.emit("project_changed", json!({"project_id": child.project_id})).await;

    let mut response = json!({
            "bot_id": new_id,
            "name": name,
            "promoted_from": {"bot_id": id, "session_id": located.session_id},
            "transcript_path": staged.dest.to_string_lossy(),
            "run_id": run_id,
        });
    if let Some((from, to)) = remapped_model {
        response["remapped"] = json!({"model": {"from": from, "to": to}});
    }
    Ok((
        StatusCode::OK,
        Json(response),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{env, Env};
    use serde_json::Value;

    /// 稽核：記下來的 session 取「最後一個有記的 run」，`started_at` 只到毫秒，同一毫秒的兩個 run 要看寫入順序（`rowid`）——
    /// 挑錯就是把 child 升級成接著**另一段對話**的頂層 bot（#100／#461 同一個坑）。
    #[tokio::test]
    async fn the_recorded_session_is_the_run_written_last_when_two_share_a_millisecond() {
        let e = env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "promote-tie").await;
        let file = e.dir.join("tie.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        for (id, session) in [("r-zzz-earlier", "native-earlier"), ("r-aaa-latest", "native-latest")] {
            sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at) VALUES (?,?, 'exited','idle',?,?, '2026-10-01T00:00:00.000Z')")
                .bind(id)
                .bind(&bot.id)
                .bind(session)
                .bind(file.to_string_lossy().to_string())
                .execute(&e.app.db)
                .await
                .unwrap();
        }
        let got = locate_recorded(&e.app, &bot.id).await.unwrap();
        assert_eq!(got.session_id, "native-latest");
    }

    const SID: &str = "sess-promote-1";
    const CHILD_CWD: &str = "/tmp/promote-child-worktree";

    struct OneEnv(std::collections::BTreeMap<String, String>);
    impl crate::runners::pane_identity::ProcEnv for OneEnv {
        fn env_of<'a>(&'a self, _: &'a Arc<App>, _: &'a str, _: i64) -> futures::future::BoxFuture<'a, Option<std::collections::BTreeMap<String, String>>> {
            Box::pin(async move { Some(self.0.clone()) })
        }
    }

    struct Rig {
        e: Env,
        child: String,
        /// 身分 cc-a 的 CLAUDE_CONFIG_DIR。
        dir: PathBuf,
    }

    impl Rig {
        fn src(&self) -> PathBuf {
            self.dir.join("projects").join(cwd_key(CHILD_CWD)).join(format!("{SID}.jsonl"))
        }
        fn dest(&self) -> PathBuf {
            // 專案路徑入庫時會 canonicalize（macOS 的 /var → /private/var），claude 的 cwd 也是真實路徑。
            let repo = std::fs::canonicalize(&self.e.repo).unwrap_or_else(|_| self.e.repo.clone());
            self.dir.join("projects").join(cwd_key(&repo.to_string_lossy())).join(format!("{SID}.jsonl"))
        }
    }

    /// 一顆活著的 child：pane 裡有 claude 行程（pid 4242）、`sessions/4242.json` 與 transcript 都在 cc-a 的目錄底下。
    async fn rig(with_session: bool) -> Rig {
        let e = env().await;
        let dir = e.dir.join("cc-a");
        let (pid, repo, d) = (e.project_id.clone(), e.repo.to_string_lossy().to_string(), dir.to_string_lossy().to_string());
        e.app
            .cfg
            .update(move |c| {
                c.identities.push(crate::config::IdentityCfg {
                    name: "cc-a".into(),
                    kind: "claude".into(),
                    host: None,
                    env: [("CLAUDE_CONFIG_DIR".to_string(), d.clone())].into(),
                    args: vec![],
                });
                c.projects.push(crate::config::ProjectCfg { id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
                Ok(())
            })
            .await
            .unwrap();
        crate::projection::project_config(&e.app.cfg, &e.app.db).await.unwrap();
        let child = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, model, args_json, autostart, inject_hooks, hook_token, managed_by, identity, created_at)
             VALUES (?,?,'kid','claude','opus','[]',0,0,'tok','child','cc-a',?)",
        )
        .bind(&child)
        .bind(&e.project_id)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at) VALUES (?,?,'running','idle','pane-kid','test',?)",
        )
        .bind(db::ulid())
        .bind(&child)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        e.herdr.set_argv("pane-kid", &["claude", "--dangerously-skip-permissions"]);
        e.herdr.set_pid("pane-kid", 4242);
        e.app.proc_env.set(Arc::new(OneEnv([("CLAUDE_CONFIG_DIR".to_string(), dir.to_string_lossy().to_string())].into())));
        let r = Rig { e, child, dir };
        if with_session {
            std::fs::create_dir_all(r.src().parent().unwrap()).unwrap();
            std::fs::write(r.src(), "{\"type\":\"user\",\"message\":\"先做 X\"}\n").unwrap();
            std::fs::create_dir_all(r.dir.join("sessions")).unwrap();
            std::fs::write(r.dir.join("sessions/4242.json"), json!({"pid": 4242, "sessionId": SID, "cwd": CHILD_CWD}).to_string()).unwrap();
        }
        r
    }

    async fn promote(r: &Rig, req: PromoteReq) -> Result<Value, LcError> {
        let res = promote_bot(State(r.e.app.clone()), Path(r.child.clone()), Extension(crate::api::RequestPrincipal::User), Some(Json(req))).await?;
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn a_bot_token_cannot_promote_a_child() {
        let r = rig(true).await;
        let err = promote_bot(
            State(r.e.app.clone()),
            Path(r.child.clone()),
            Extension(crate::api::RequestPrincipal::Bot(r.child.clone())),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LcError::Forbidden(_)), "only the UI user can promote a child: {err:?}");
        assert!(!r.dest().exists());
        assert_eq!(db::active_run(&r.e.app.db, &r.child).await.unwrap().unwrap().state, "running");
        assert!(r.e.app.cfg.get().await.projects[0].bots.is_empty());
    }

    fn reason(e: LcError) -> String {
        match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => format!("{other:?}"),
        }
    }

    async fn cfg_bot_names(r: &Rig) -> Vec<String> {
        r.e.app.cfg.get().await.projects[0].bots.iter().map(|b| b.name.clone()).collect()
    }

    async fn child_state(r: &Rig) -> (bool, Option<String>) {
        let deleted: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id = ?").bind(&r.child).fetch_one(&r.e.app.db).await.unwrap();
        let run = db::active_run(&r.e.app.db, &r.child).await.unwrap().map(|x| x.state);
        (deleted.is_some(), run)
    }

    #[test]
    fn the_cwd_directory_name_is_every_non_alphanumeric_turned_into_a_dash() {
        assert_eq!(cwd_key("/Users/m4p/.config/agents-manager"), "-Users-m4p--config-agents-manager");
    }

    /// 完整一趟：session 從 pane 行程找到 → transcript 複製到專案路徑的 projects 目錄（來源還在）→ child 停掉 →
    /// user bot 進 config.toml、以 `--resume <session>` 啟動 → child 紀錄收掉。
    #[tokio::test]
    async fn a_child_becomes_a_top_level_bot_on_the_same_session() {
        let r = rig(true).await;
        let out = promote(&r, PromoteReq { name: Some("kid-top".into()), model: Some("opus".into()), ..Default::default() }).await.unwrap();
        assert_eq!(out["name"], "kid-top");
        assert_eq!(out["remapped"]["model"]["to"], "claude-opus-5-5");
        assert_eq!(out["promoted_from"]["session_id"], SID);
        assert!(out["run_id"].is_string(), "{out}");
        let new_id = out["bot_id"].as_str().unwrap().to_string();

        assert!(r.dest().is_file(), "transcript 複製到新 cwd 對應的 projects 目錄");
        assert!(r.src().is_file(), "複製、不搬");
        assert_eq!(std::fs::read_to_string(r.dest()).unwrap(), std::fs::read_to_string(r.src()).unwrap());

        let bots = r.e.app.cfg.get().await.projects[0].bots.clone();
        let cfg = bots.iter().find(|b| b.id.as_deref() == Some(&new_id)).expect("進了 config.toml");
        assert_eq!((cfg.model.as_deref(), cfg.identity.as_deref(), cfg.inject_hooks), (Some("claude-opus-5-5"), Some("cc-a"), true));
        let row = db::bot(&r.e.app.db, &new_id).await.unwrap().unwrap();
        assert_eq!(row.managed_by, "user");
        assert!(row.parent_bot_id.is_none());

        let args: Vec<String> = r.e.herdr.calls_to("agent.start").pop().unwrap()["args"].as_array().unwrap().iter().filter_map(|a| a.as_str().map(String::from)).collect();
        assert!(args.windows(2).any(|w| w == ["--resume", SID]), "{args:?}");
        let resume: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id = ?").bind(out["run_id"].as_str().unwrap()).fetch_one(&r.e.app.db).await.unwrap();
        assert_eq!(resume.as_deref(), Some(SID));

        assert_eq!(child_state(&r).await, (true, None), "child 收掉、沒有活的 run");
    }

    /// #413：收掉 child 走退役的唯一入口，log 留得下是 promote 收的、從哪一行。
    #[tokio::test]
    async fn retiring_the_promoted_child_is_logged_with_its_reason() {
        let r = rig(true).await;
        let (buf, _guard) = crate::config_audit::capture::start();
        promote(&r, PromoteReq { name: Some("kid-top".into()), ..Default::default() }).await.unwrap();
        let log = buf.text();
        let line = log.lines().find(|l| l.contains("child retired")).unwrap_or_else(|| panic!("no retire line: {log}"));
        assert!(line.contains("why=\"promoted\"") && line.contains(&r.child) && line.contains("promote.rs:"), "{line}");
    }

    #[tokio::test]
    async fn without_a_name_the_child_name_is_reused_with_a_suffix_while_the_child_still_holds_it() {
        let r = rig(true).await;
        let out = promote(&r, PromoteReq::default()).await.unwrap();
        assert_eq!(out["name"], "kid-1");
    }

    /// pane 裡找不到對得上的 claude 行程／sessions 檔：409，child 照跑、config 沒多一顆、沒有複製。
    #[tokio::test]
    async fn no_session_means_nothing_moves() {
        let r = rig(false).await;
        assert_eq!(reason(promote(&r, PromoteReq::default()).await.unwrap_err()), "session_not_found");
        assert_eq!(child_state(&r).await, (false, Some("running".into())));
        assert!(cfg_bot_names(&r).await.is_empty());
        assert!(!r.dest().exists());
    }

    /// 目標已經有一份不同內容的同名檔：不覆寫，整個不動。
    #[tokio::test]
    async fn an_existing_different_transcript_is_never_overwritten() {
        let r = rig(true).await;
        std::fs::create_dir_all(r.dest().parent().unwrap()).unwrap();
        std::fs::write(r.dest(), "別人的對話\n").unwrap();
        assert_eq!(reason(promote(&r, PromoteReq::default()).await.unwrap_err()), "transcript_exists");
        assert_eq!(std::fs::read_to_string(r.dest()).unwrap(), "別人的對話\n");
        assert_eq!(child_state(&r).await, (false, Some("running".into())));
        assert!(cfg_bot_names(&r).await.is_empty());
    }

    /// 對抗式審查（資料安全）：transcript 是整段對話內容。複製出去的檔 0600、這次新建的目錄 0700，不管來源是 0644。
    #[tokio::test]
    async fn the_promoted_copy_and_its_new_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let r = rig(true).await;
        std::fs::set_permissions(r.src(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let companion = r.src().with_extension("");
        std::fs::create_dir_all(companion.join("sub")).unwrap();
        std::fs::write(companion.join("sub/n.txt"), "x").unwrap();
        std::fs::set_permissions(companion.join("sub/n.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
        promote(&r, PromoteReq { name: Some("kid-priv".into()), ..Default::default() }).await.unwrap();
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(r.dest()), 0o600);
        assert_eq!(mode(r.dest().parent().unwrap().to_path_buf()), 0o700, "新建的 cwd 目錄");
        let cd = r.dest().with_extension("");
        assert_eq!(mode(cd.join("sub/n.txt")), 0o600);
        assert_eq!(mode(cd.join("sub")), 0o700);
    }

    /// 來源 transcript 是符號連結（指到別的檔）：不能被複製成新 bot 的對話檔。
    #[tokio::test]
    async fn a_symlinked_source_transcript_is_refused() {
        let r = rig(true).await;
        let secret = r.e.dir.join("secret.txt");
        std::fs::write(&secret, "TOKEN\n").unwrap();
        std::fs::remove_file(r.src()).unwrap();
        std::os::unix::fs::symlink(&secret, r.src()).unwrap();
        assert_eq!(reason(promote(&r, PromoteReq::default()).await.unwrap_err()), "transcript_copy_failed");
        assert!(!r.dest().exists(), "符號連結被跟著複製");
        assert_eq!(child_state(&r).await, (false, Some("running".into())), "整個不動");
    }

    /// 附屬目錄複製到一半失敗：不留下半份目錄（它不在 `created` 裡，收回時也不會被清）。
    #[tokio::test]
    async fn a_half_copied_companion_directory_is_not_left_behind() {
        use std::os::unix::fs::PermissionsExt;
        let r = rig(true).await;
        let companion = r.src().with_extension("");
        std::fs::create_dir_all(&companion).unwrap();
        for n in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(companion.join(n), "x").unwrap();
        }
        std::fs::set_permissions(companion.join("b.txt"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let _ = promote(&r, PromoteReq { name: Some("kid-half".into()), ..Default::default() }).await;
        std::fs::set_permissions(companion.join("b.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!r.dest().with_extension("").exists(), "半份附屬目錄被留下來了");
    }

    #[tokio::test]
    async fn only_children_are_accepted() {
        let r = rig(true).await;
        sqlx::query("UPDATE bots SET managed_by = 'user' WHERE id = ?").bind(&r.child).execute(&r.e.app.db).await.unwrap();
        assert_eq!(reason(promote(&r, PromoteReq::default()).await.unwrap_err()), "not_child");
        assert!(!r.dest().exists());
    }

    #[tokio::test]
    async fn a_child_with_its_own_children_is_refused() {
        let r = rig(true).await;
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
             VALUES (?,?,'grandkid','claude','[]',0,0,'tok','child',?,?)",
        )
        .bind(db::ulid())
        .bind(&r.e.project_id)
        .bind(&r.child)
        .bind(db::now())
        .execute(&r.e.app.db)
        .await
        .unwrap();
        assert_eq!(reason(promote(&r, PromoteReq::default()).await.unwrap_err()), "has_children");
        assert!(!r.dest().exists());
    }

    /// 啟動失敗：新 bot 從 config 拿掉、複製的檔收回、child 已停但紀錄還在（session 記在它的 run 上）。
    #[tokio::test]
    async fn a_failed_start_rolls_back_the_new_bot_and_the_copy() {
        let r = rig(true).await;
        r.e.herdr.fail_next("agent.start", crate::testing::Fault::Refuse);
        let err = promote(&r, PromoteReq { name: Some("kid-top".into()), ..Default::default() }).await.unwrap_err();
        assert_eq!(reason(err), "promote_start_failed");
        assert!(cfg_bot_names(&r).await.is_empty(), "新 bot 收回");
        assert!(!r.dest().exists(), "複製的檔收回");
        assert!(r.src().is_file(), "來源不動");
        let (deleted, active) = child_state(&r).await;
        assert!(!deleted && active.is_none(), "child 紀錄還在、已停");
        let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE deleted_at IS NULL AND managed_by = 'user'").fetch_one(&r.e.app.db).await.unwrap();
        assert_eq!(live, 0, "沒有留下半套的第二顆 bot");

        // 修好之後同一個請求原樣再送一次就成功（session 已記在 child 的 run 上，pane 行程已經不在了）。
        let out = promote(&r, PromoteReq { name: Some("kid-top".into()), ..Default::default() }).await.unwrap();
        assert_eq!(out["promoted_from"]["session_id"], SID);
        assert!(r.dest().is_file());
        assert_eq!(child_state(&r).await, (true, None));
    }

    /// 檢查點寫不進去：child 不能被停、複製收回、config 沒多一顆；修好後重送成功。
    #[tokio::test]
    async fn a_failed_checkpoint_leaves_the_child_running_and_can_be_retried() {
        let r = rig(true).await;
        sqlx::query("CREATE TRIGGER refuse_checkpoint BEFORE UPDATE OF native_session_id ON runs BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END").execute(&r.e.app.db).await.unwrap();
        assert_eq!(reason(promote(&r, PromoteReq::default()).await.unwrap_err()), "promote_checkpoint_failed");
        assert_eq!(child_state(&r).await, (false, Some("running".into())), "child 沒被停");
        assert!(!r.dest().exists(), "複製收回");
        assert!(cfg_bot_names(&r).await.is_empty());
        sqlx::query("DROP TRIGGER refuse_checkpoint").execute(&r.e.app.db).await.unwrap();
        promote(&r, PromoteReq { name: Some("kid-top".into()), ..Default::default() }).await.unwrap();
        assert_eq!(child_state(&r).await, (true, None));
    }

    /// 最後收 child 紀錄寫不進去：新 bot 要收回（不留兩顆活的），child 已停、可重送。
    #[tokio::test]
    async fn a_failed_child_retirement_rolls_the_new_bot_back_instead_of_leaving_two() {
        let r = rig(true).await;
        let sql = format!("CREATE TRIGGER refuse_retire BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END", r.child);
        sqlx::query(&sql).execute(&r.e.app.db).await.unwrap();
        let err = promote(&r, PromoteReq { name: Some("kid-top".into()), ..Default::default() }).await.unwrap_err();
        assert_eq!(reason(err), "promote_child_not_removed");
        assert!(cfg_bot_names(&r).await.is_empty(), "新 bot 收回");
        assert!(!r.dest().exists());
        let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE deleted_at IS NULL AND managed_by = 'user'").fetch_one(&r.e.app.db).await.unwrap();
        assert_eq!(live, 0, "沒有第二顆活的 bot");
        let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE state IN ('running','starting') AND bot_id != ?").bind(&r.child).fetch_one(&r.e.app.db).await.unwrap();
        assert_eq!(active, 0, "新 bot 的 run 已停");
        assert_eq!(child_state(&r).await, (false, None));
        sqlx::query("DROP TRIGGER refuse_retire").execute(&r.e.app.db).await.unwrap();
        promote(&r, PromoteReq { name: Some("kid-top".into()), ..Default::default() }).await.unwrap();
        assert_eq!(child_state(&r).await, (true, None));
    }

    // ---- #355 P4：promote 在承諾點前後行程死掉，開機補完 ----

    /// 模擬行程死亡：promote 走到 `point` 就卡住，再把整個 future abort 掉。
    async fn die_at(r: &Rig, point: &'static str) {
        let reached = Arc::new(tokio::sync::Notify::new());
        let r2 = reached.clone();
        lifecycle::race_point::arm(point, &r.child, move || async move {
            r2.notify_one();
            std::future::pending::<()>().await
        });
        let (app, id) = (r.e.app.clone(), r.child.clone());
        let h = tokio::spawn(async move {
            promote_bot(State(app), Path(id), Extension(crate::api::RequestPrincipal::User), None).await.map(|_| ())
        });
        reached.notified().await;
        h.abort();
        let _ = h.await;
    }

    /// promote 那一件的狀態（同一顆 child 退役時另有一筆 `retire_child` 紀錄，#554，不算在這裡）。
    async fn intent_status(app: &Arc<App>, child: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT status FROM intents WHERE subject_id = ? AND kind = 'promote' ORDER BY created_at").bind(child).fetch_all(&app.db).await.unwrap()
    }

    /// #248：停 child 之後（承諾點前後三個位置）行程死掉。以前留下「child 已停、目標 bot 半建」的半套；現在開機往前補完：
    /// 結果跟沒中斷一樣——child 收掉、新 user bot 以 --resume 起來、session 只種一次，補兩次也一樣。
    #[tokio::test]
    async fn a_promote_killed_after_the_child_stopped_is_completed_on_boot() {
        for point in ["promote_after_stop", "promote_after_create", "promote_after_start"] {
            let r = rig(true).await;
            die_at(&r, point).await;
            assert_eq!(intent_status(&r.e.app, &r.child).await, vec!["running"], "{point}：intent 還開著");

            let app2 = crate::testing::restart_app(&r.e).await;
            crate::promote_intents::recover_host(&app2, LOCAL_HOST).await;
            let (deleted, _) = {
                let d: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id = ?").bind(&r.child).fetch_one(&app2.db).await.unwrap();
                (d.is_some(), ())
            };
            assert!(deleted, "{point}：child 收掉了");
            let new_bots = app2.cfg.get().await.projects[0].bots.clone();
            assert_eq!(new_bots.len(), 1, "{point}：config 裡只有一顆新的 user bot");
            let new_id = new_bots[0].id.clone().unwrap();
            // `recover_host` 只同步跑第一次嘗試；那一次若因為暫時原因（例如高負載下的一次性錯誤）失敗，
            // intent 仍是 `running`、背景的重試迴圈才會真的把 native resume 起起來（`drive_once` 回 `Retry`
            // 時 `recover_host` 會 spawn `retry_loop`，不等它）。等的是這個**最後一個副作用**（run 起來），
            // 不是固定睡一段時間；已經成功的話這裡幾乎立刻就過。
            let ok = crate::testing::eventually!(db::active_run(&app2.db, &new_id).await.unwrap().is_some());
            if !ok {
                // 30 秒都等不到：把 intent 記的重試狀態與 last_error 一起丟進斷言訊息（整樹高負載下曾經連著重試都失敗過一次，
                // 沒抓到根因；下次真的再紅，訊息裡就有東西可查，不用只看到「新 bot 起來了」）。
                let intent_row: Option<(String, i64, Option<String>)> = sqlx::query_as("SELECT status, attempts, last_error FROM intents WHERE subject_id=? AND kind='promote'")
                    .bind(&r.child).fetch_optional(&app2.db).await.unwrap();
                let runs_dump: Vec<(String, String, Option<String>)> = sqlx::query_as("SELECT id, state, ended_at FROM runs WHERE bot_id=?")
                    .bind(&new_id).fetch_all(&app2.db).await.unwrap();
                panic!("{point}：新 bot 起來了 —— intent={intent_row:?} runs={runs_dump:?}");
            }
            let _ = crate::testing::eventually!(intent_status(&app2, &r.child).await == vec!["done"]);
            assert_eq!(intent_status(&app2, &r.child).await, vec!["done"], "{point}");
            let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ?").bind(&new_id).fetch_one(&app2.db).await.unwrap();
            crate::promote_intents::recover_host(&app2, LOCAL_HOST).await;
            tokio::join!(crate::promote_intents::recover_host(&app2, LOCAL_HOST), crate::promote_intents::recover_host(&app2, LOCAL_HOST));
            let runs_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ?").bind(&new_id).fetch_one(&app2.db).await.unwrap();
            assert_eq!(runs, runs_after, "{point}：冪等，沒有多種／多起");
            assert_eq!(app2.cfg.get().await.projects[0].bots.len(), 1, "{point}：沒有多建");
        }
    }

    /// 承諾之前（intent 寫了、child 還沒停）死掉：收回複製、abandoned，child 照跑，可原樣重送。
    #[tokio::test]
    async fn a_promote_killed_before_the_child_was_stopped_is_abandoned_and_the_copy_removed() {
        let r = rig(true).await;
        die_at(&r, "promote_after_intent").await;
        assert!(r.dest().exists(), "死的當下複製還在");
        let app2 = crate::testing::restart_app(&r.e).await;
        crate::promote_intents::recover_host(&app2, LOCAL_HOST).await;
        assert_eq!(intent_status(&app2, &r.child).await, vec!["abandoned"]);
        assert!(!r.dest().exists(), "複製收回");
        let deleted: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id = ?").bind(&r.child).fetch_one(&app2.db).await.unwrap();
        assert!(deleted.is_none() && db::active_run(&app2.db, &r.child).await.unwrap().is_some(), "child 照跑");
    }

    /// 補不成（新 bot 接不回原對話）：5 次後 failed＋AGM inbox，不回滾（child 已停、session 記著）。
    #[tokio::test]
    async fn a_promote_that_cannot_be_completed_gives_up_and_tells_agm() {
        let r = rig(true).await;
        die_at(&r, "promote_after_stop").await;
        // 之後 native resume 一定接不回：複製過去的 transcript 沒了。
        let _ = std::fs::remove_file(r.dest());
        // #401：種 run 的時間戳之後拉開一段，確定性地跨過毫秒邊界（以前兩欄各叫一次 now()，偶發被當成「起過了」→ done）。
        straddle_the_seed(&r);
        let app2 = crate::testing::restart_app(&r.e).await;
        crate::promote_intents::recover_host(&app2, LOCAL_HOST).await;
        let mut st = vec![];
        for _ in 0..200 {
            st = intent_status(&app2, &r.child).await;
            if st == vec!["failed"] {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(st, vec!["failed"]);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'intent_failed' AND bot_id = ?").bind(&r.child).fetch_one(&app2.db).await.unwrap();
        assert_eq!(n, 1, "AGM inbox 有一則 intent_failed");
    }

    /// #1060：補做失敗、而且失敗記不下來：intent 停在這顆 boot 的 running。同一顆 daemon 之後的重試要續做，不能當成「別人收了」就安靜結束。
    #[tokio::test]
    async fn a_promote_whose_failure_cannot_be_recorded_is_still_retried_by_the_same_boot() {
        use crate::runners::restart_intents::Outcome;
        let r = rig(true).await;
        die_at(&r, "promote_after_stop").await;
        let _ = std::fs::remove_file(r.dest());
        straddle_the_seed(&r);
        sqlx::query("CREATE TRIGGER am_test_refuse_pending BEFORE UPDATE OF status ON intents WHEN OLD.status = 'running' AND NEW.status = 'pending' BEGIN SELECT RAISE(ABORT, 'injected'); END")
            .execute(&r.e.app.db)
            .await
            .unwrap();
        let app2 = crate::testing::restart_app(&r.e).await;
        let id: String = sqlx::query_scalar("SELECT id FROM intents WHERE subject_id = ? AND kind = 'promote'").bind(&r.child).fetch_one(&app2.db).await.unwrap();

        assert!(matches!(crate::promote_intents::drive_once(&app2, &id).await, Outcome::Retry(_)));
        assert_eq!(intent_status(&app2, &r.child).await, vec!["running"]);
        assert!(matches!(crate::promote_intents::drive_once(&app2, &id).await, Outcome::Retry(_)), "同一顆 boot 的下一輪要續做，不是 Finished");

        sqlx::query("DROP TRIGGER am_test_refuse_pending").execute(&app2.db).await.unwrap();
        assert!(matches!(crate::promote_intents::drive_once(&app2, &id).await, Outcome::Retry(_)));
        assert_eq!(intent_status(&app2, &r.child).await, vec!["pending"], "失敗記下來了，之後照 MAX_ATTEMPTS 收斂");
    }

    /// 種 run 取了時間戳之後停 5ms 再寫：跨過毫秒邊界是確定的，不靠整樹負載碰運氣。
    fn straddle_the_seed(r: &Rig) {
        lifecycle::race_point::arm("promote_seed", &r.child, || async { tokio::time::sleep(std::time::Duration::from_millis(5)).await });
    }

    /// #401：種 run 跨過毫秒邊界時，補完仍要把目標 bot 真的以 native resume 起起來，不能直接記 done。
    #[tokio::test]
    async fn a_seed_straddling_a_millisecond_still_starts_the_promoted_bot() {
        let r = rig(true).await;
        die_at(&r, "promote_after_stop").await;
        straddle_the_seed(&r);
        let app2 = crate::testing::restart_app(&r.e).await;
        crate::promote_intents::recover_host(&app2, LOCAL_HOST).await;
        let new_id = app2.cfg.get().await.projects[0].bots[0].id.clone().unwrap();
        let seeded: (String, String) = sqlx::query_as("SELECT started_at, ended_at FROM runs WHERE bot_id = ? AND pane_id IS NULL").bind(&new_id).fetch_one(&app2.db).await.unwrap();
        assert_eq!(seeded.0, seeded.1, "種的那列兩欄同一個時間戳");
        assert!(crate::testing::eventually!(db::active_run(&app2.db, &new_id).await.unwrap().is_some()), "目標 bot 真的起來了");
        assert!(crate::testing::eventually!(intent_status(&app2, &r.child).await == vec!["done"]));
    }

    /// intent 寫不進去＝什麼都還沒停：不能繼續（child 照跑、複製收回）。
    #[tokio::test]
    async fn a_promote_that_cannot_record_its_intent_stops_nothing() {
        let r = rig(true).await;
        sqlx::query("CREATE TRIGGER am_test_no_intents BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END").execute(&r.e.app.db).await.unwrap();
        assert!(promote(&r, PromoteReq::default()).await.is_err());
        let (deleted, run) = child_state(&r).await;
        assert!(!deleted && run.is_some(), "child 照跑");
        assert!(!r.dest().exists(), "複製收回");
    }
}
