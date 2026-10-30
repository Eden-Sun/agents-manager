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
//! 3. 預先複製 transcript 做停機前檢查 → 4. 停掉 child（停不掉就收回複製、不動）→ 4a. 驗證來源穩定後原子更新本次擁有的 staged 檔 →
//! 5. 建 user bot 並種下 session → 6. native resume 啟動（失敗就把新 bot 與複製收回）→ 7. 收掉 child 紀錄。
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
use serde_json::{json, Value};
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
    let procs = match client.pane_process_info(&pane).await {
        Ok(procs) => procs,
        // #1455：pane 已經不在（herdr 回 pane_not_found）＝裡面找不到 claude session，跟「pane 裡沒有 claude 行程」同一類：409，什麼都不動。
        // 其他 herdr 錯誤仍是 502（不知道 pane 還在不在）。
        Err(e) if e.downcast_ref::<crate::herdr::HerdrError>().is_some_and(|h| h.code == "pane_not_found") => {
            return Err(refuse("session_not_found", json!({"why": "pane_gone", "message": "這顆子 agent 的 pane 已經不在，找不到它的 claude session。"})));
        }
        Err(e) => return Err(up(e)),
    };
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
    source_identity: Option<FileIdentity>,
    dest_identity: Option<FileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileVersion {
    identity: FileIdentity,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

fn file_identity(file: &std::fs::File) -> std::io::Result<FileIdentity> {
    let m = file.metadata()?;
    Ok(FileIdentity { dev: m.dev(), ino: m.ino() })
}

fn file_version(file: &std::fs::File) -> std::io::Result<FileVersion> {
    let m = file.metadata()?;
    Ok(FileVersion {
        identity: FileIdentity { dev: m.dev(), ino: m.ino() },
        len: m.len(),
        mtime: m.mtime(),
        mtime_nsec: m.mtime_nsec(),
        ctime: m.ctime(),
        ctime_nsec: m.ctime_nsec(),
    })
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
    let mut staged = Staged { created: vec![], dest: dest.clone(), source_identity: None, dest_identity: None };
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
    staged.source_identity = Some(file_identity(&src_fd).map_err(bad)?);
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
    staged.created.push(dest.clone());
    staged.dest_identity = Some(file_identity(&crate::trusted_open::open_entry_in(&dest_fd, dest_name).map_err(bad)?).map_err(bad)?);
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

fn validate_complete_transcript(file: &mut std::fs::File) -> std::io::Result<()> {
    use std::io::{BufRead as _, Seek as _, SeekFrom};
    if file.metadata()?.len() == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "empty Claude transcript"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 { return Ok(()); }
        if line.last() != Some(&b'\n') {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "transcript ends with an incomplete JSONL record"));
        }
        line.pop();
        if line.last() == Some(&b'\r') { line.pop(); }
        if !line.is_empty() {
            serde_json::from_slice::<serde_json::Value>(&line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("incomplete JSONL record: {e}")))?;
        }
    }
}

/// 停止 transcript writer 後，把這次 promote 擁有的 staged inode 原子更新成最後一致的來源。
/// 舊 intent 沒有 ownership metadata 時，只能在目標已等於來源時完成，不猜測是否可以覆寫。
pub(crate) fn finalize_staged_transcript(
    src: &FsPath,
    dest: &FsPath,
    session_id: &str,
    source_identity: Option<FileIdentity>,
    staged_identity: Option<FileIdentity>,
) -> Result<(), LcError> {
    use lifecycle::transcript_stage as ts;
    use std::io::Write as _;
    let bad = |e: std::io::Error| refuse("transcript_copy_failed", json!({"message": e.to_string()}));
    if !ts::is_claude_transcript(src) || src.file_stem() != Some(std::ffi::OsStr::new(session_id)) {
        return Err(refuse("transcript_copy_failed", json!({"message": "最終來源不是預期 session 的一般 transcript"})));
    }
    let mut source = crate::transcript_read::open_regular(src).map_err(bad)?;
    let before = file_version(&source).map_err(bad)?;
    if source_identity.is_some_and(|identity| identity != before.identity) {
        return Err(refuse("transcript_copy_failed", json!({"message": "來源 transcript 身份在 staging 後改變"})));
    }
    let Some(dest_dir) = dest.parent() else { return Err(refuse("transcript_copy_failed", json!({"message": "目的 transcript 沒有目錄"}))) };
    let Some(projects_dir) = dest_dir.parent().filter(|p| p.file_name() == Some(std::ffi::OsStr::new("projects"))) else {
        return Err(refuse("transcript_copy_failed", json!({"message": "目的目錄不是 projects/<cwd>"})));
    };
    let Some(config_dir) = projects_dir.parent() else { return Err(refuse("transcript_copy_failed", json!({"message": "目的 projects 沒有帳號根目錄"}))) };
    let Some(cwd_key) = dest_dir.file_name() else { return Err(refuse("transcript_copy_failed", json!({"message": "目的 cwd 不存在"}))) };
    let parts = [std::ffi::OsStr::new("projects"), cwd_key];
    let dest_fd = crate::trusted_open::open_bound_dir(config_dir, &parts, None).map_err(bad)?;
    let dest_name = dest.file_name().ok_or_else(|| refuse("transcript_copy_failed", json!({"message": "目的 transcript 名稱不存在"})))?;
    let existing = crate::trusted_open::open_entry_in(&dest_fd, dest_name).map_err(bad)?;
    let current_identity = file_identity(&existing).map_err(bad)?;
    let contents_equal = |left: &std::fs::File, right: &std::fs::File| -> Result<bool, LcError> {
        let (lm, rm) = (left.metadata().map_err(bad)?, right.metadata().map_err(bad)?);
        if lm.len() != rm.len() { return Ok(false); }
        Ok(ts::is_prefix_of(left, right).map_err(bad)? && ts::is_prefix_of(right, left).map_err(bad)?)
    };

    // Already-final copies make retries idempotent; source==destination is a no-op too.
    if contents_equal(&source, &existing)? {
        validate_complete_transcript(&mut source).map_err(bad)?;
        if file_version(&source).map_err(bad)? != before {
            return Err(refuse("transcript_copy_failed", json!({"message": "來源 transcript 在最終驗證期間仍有變動"})));
        }
        return Ok(());
    }
    let Some(expected_dest) = staged_identity else {
        return Err(refuse("transcript_copy_failed", json!({"message": "目的 transcript 不屬於這次 promote，且尚未包含最後來源"})));
    };
    if current_identity != expected_dest {
        return Err(refuse("transcript_copy_failed", json!({"message": "staged transcript 身份在 promote 後改變"})));
    }
    if !ts::is_prefix_of(&existing, &source).map_err(bad)? {
        return Err(refuse("transcript_copy_failed", json!({"message": "staged transcript 與來源內容分岔，拒絕覆寫"})));
    }

    let mut tmp_name = std::ffi::OsString::from(".promote-final-");
    tmp_name.push(db::ulid());
    tmp_name.push(".tmp");
    let copied = (|| {
        let mut out = crate::trusted_open::create_new_file_in(&dest_fd, &tmp_name, 0o600)?;
        std::io::copy(&mut source, &mut out)?;
        out.flush()?;
        out.sync_all()?;
        drop(out);
        let mut staged = crate::trusted_open::open_entry_in(&dest_fd, &tmp_name)?;
        validate_complete_transcript(&mut staged)?;
        if file_version(&source)? != before {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "source transcript changed during final copy"));
        }
        let source_now = crate::transcript_read::open_regular(src)?;
        if file_version(&source_now)? != before {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "source transcript path changed during final copy"));
        }
        let current = crate::trusted_open::open_entry_in(&dest_fd, dest_name)?;
        if file_identity(&current)? != expected_dest {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "staged transcript identity changed before publish"));
        }
        crate::trusted_open::rename_in(&dest_fd, &tmp_name, dest_name)?;
        let published = crate::trusted_open::open_entry_in(&dest_fd, dest_name)?;
        if file_identity(&published)? != file_identity(&staged)? {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "final transcript publication could not be verified"));
        }
        Ok::<(), std::io::Error>(())
    })();
    if let Err(e) = copied {
        let _ = crate::trusted_open::unlink_in(&dest_fd, &tmp_name);
        return Err(bad(e));
    }
    Ok(())
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
    // 停不下來（`stop_bot` 寫不進 `stopping`、agent 可能還在跑）就不算收乾淨，回傳要反映它。
    let stopped = lifecycle::stop_bot(app, new_id).await.is_ok();
    let removed = crate::projection::delete_from_config(&app.cfg, &app.db, crate::projection::DeleteTarget::Bot(new_id)).await;
    if removed.is_ok() {
        staged.undo();
    }
    app.emit("bot_changed", json!({"bot_id": new_id})).await;
    stopped && removed.is_ok()
}

/// 升級出來的 user bot 的設定（設定從 child 帶：模型、強度、身份；args 不帶——那是收編時看到的舊 argv）。
/// handler 與開機補完（`promote_intents`）共用同一份，補完出來的跟 handler 做的一模一樣。
pub(crate) fn user_bot_cfg(child: &db::Bot, new_id: &str, name: &str, model: &Option<String>, effort: &Option<String>) -> BotCfg {
    BotCfg { role: None,
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
    // #1425：移交出去的專案在複製對話、停 child 之前就拒絕（以前要等 start_bot_with 被擋才回收）。
    crate::handoff::refuse(&app.db, &id).await?;
    // 脫離請求（#1358）：複製、停 child、建 user bot、啟動都在脫離出去的 task 裡做完；client 斷線不會停在半路。
    // bot 鎖也在裡面拿，握著直到做完。
    let response = crate::api::detach(&id, {
        let (app, id) = (app.clone(), id.clone());
        async move { promote_detached(app, id, req).await }
    })
    .await?;
    Ok((StatusCode::OK, Json(response)).into_response())
}

/// promote 的本體（見 `promote_bot`）：在脫離出去的 task 裡跑，回傳要給 client 的 body。
async fn promote_detached(app: Arc<App>, id: String, req: PromoteReq) -> Result<Value, LcError> {
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
        "session_id": located.session_id, "source": located.src.to_string_lossy(), "dest": staged.dest.to_string_lossy(),
        "source_identity": staged.source_identity, "staged_identity": staged.dest_identity,
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
    lifecycle::race_point::hit("promote_before_final_transcript", &id).await;

    // 確認 writer 已停止後再讀來源，並原子更新本次 promote 擁有的 staged 檔。
    let (src, dest, session, source_identity, dest_identity) = (
        located.src.clone(), staged.dest.clone(), located.session_id.clone(), staged.source_identity, staged.dest_identity,
    );
    let finalized = match tokio::task::spawn_blocking(move || finalize_staged_transcript(&src, &dest, &session, source_identity, dest_identity)).await {
        Ok(result) => result,
        Err(e) => {
            crate::promote_intents::spawn_retry(app.clone(), intent.clone(), format!("final transcript sync task failed: {e}"));
            return Err(refuse("transcript_copy_failed", json!({"message": e.to_string()})));
        }
    };
    if let Err(e) = finalized {
        crate::promote_intents::spawn_retry(app.clone(), intent.clone(), format!("final transcript sync failed: {e:?}"));
        return Err(e);
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
        retired = crate::child_retire::retire(&app, &id, "promoted", crate::child_retire::Mode::Explicit).await.map(|outcome| {
            if matches!(outcome, crate::child_retire::Outcome::Retired) {
                crate::api_bot::schedule_retention_after_delete(&app);
            }
        }).map_err(|e| e.to_string());
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
    Ok(response)
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
        /// 身份 cc-a 的 CLAUDE_CONFIG_DIR。
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
                c.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
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
        promote_owned(r.e.app.clone(), r.child.clone(), req).await
    }

    async fn promote_owned(app: Arc<App>, child: String, req: PromoteReq) -> Result<Value, LcError> {
        let res = promote_bot(State(app), Path(child), Extension(crate::api::RequestPrincipal::User), Some(Json(req))).await?;
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    /// #1425：專案已移交給別的主機、child 已經停了（只剩上次記下的 session 與 transcript）：promote 在複製對話、
    /// 寫 config 之前就 409 handed_off。停了的 child 不需要 pane，所以以前會一路走到複製與寫 config，等啟動被擋才回收。
    #[tokio::test]
    async fn a_promote_in_a_handed_off_project_is_refused_before_anything_is_copied_or_written() {
        let r = rig(true).await;
        sqlx::query("UPDATE runs SET state = 'stopped', native_session_id = ?, transcript_path = ? WHERE bot_id = ?")
            .bind(SID)
            .bind(r.src().to_string_lossy().to_string())
            .bind(&r.child)
            .execute(&r.e.app.db)
            .await
            .unwrap();
        // 移交狀態要寫進設定檔：projection 每次寫 config 都會照它重寫 projects 表，只改 DB 的話會在 promote 中途被洗掉。
        let pid = r.e.project_id.clone();
        r.e.app
            .cfg
            .update(move |c| {
                c.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).expect("rig 的專案在").handed_off_to = Some("agm-host".into());
                Ok(())
            })
            .await
            .unwrap();
        sqlx::query("UPDATE projects SET handed_off_to = 'agm-host' WHERE id = ?").bind(&r.e.project_id).execute(&r.e.app.db).await.unwrap();

        assert_eq!(reason(promote(&r, PromoteReq::default()).await.unwrap_err()), "handed_off");
        assert!(!r.dest().exists(), "沒有複製對話");
        assert!(cfg_bot_names(&r).await.is_empty(), "config 沒有新 bot");
        assert_eq!(child_state(&r).await, (false, None), "child 的紀錄沒被動");
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
    // An independent API child is a sentinel for the global retention scan; the promoted child remains Claude CLI.
    async fn r2_api_sentinel(r: &Rig) {
        let now = db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,managed_by,created_at) VALUES('r2-sentinel',?,'r2-sentinel','claude-api','fake','child',?)").bind(&r.e.project_id).bind(&now).execute(&r.e.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('r2-old-run','r2-sentinel','exited',?)").bind(&now).execute(&r.e.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('r2-old-conv','r2-sentinel',?)").bind(&now).execute(&r.e.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r2-old-turn','r2-old-conv','r2-old-run','web','completed','ok',?)").bind(&now).execute(&r.e.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES('r2-sentinel-session','r2-sentinel','r2-old-run','r2-old-turn',1,1,'{}',?,?,?)").bind(&now).bind(&now).bind(db::iso_in(-60)).execute(&r.e.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('r2-sentinel-session','r2-sentinel','r2-old-run','r2-old-turn',1,1,'completed','fixture','applied',?,?)").bind(&now).bind(&now).execute(&r.e.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES('r2-old-message','r2-old-conv','r2-old-turn','assistant','fixture','transcript',?)").bind(&now).execute(&r.e.app.db).await.unwrap();
    }

    #[tokio::test]
    async fn r2_promote_retired_only_enqueues_scan() {
        for already_gone in [false, true] {
            let r = rig(true).await;
            r2_api_sentinel(&r).await;
            if already_gone {
                let pool = r.e.app.db.clone(); let child = r.child.clone();
                lifecycle::race_point::arm("promote_after_start", &r.child, move || async move {
                    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(child).execute(&pool).await.unwrap();
                });
            }
            let out = promote(&r, PromoteReq { name: Some("kid-top".into()), ..Default::default() }).await.unwrap();
            assert_eq!(out["promoted_from"]["session_id"], SID);
            assert_eq!(std::fs::read_to_string(r.dest()).unwrap(), std::fs::read_to_string(r.src()).unwrap());
            assert_eq!(intent_status(&r.e.app, &r.child).await, vec!["done"]);
            r.e.app.background_tasks.close();
            tokio::time::timeout(std::time::Duration::from_secs(10), r.e.app.background_tasks.wait()).await.unwrap();
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-sentinel-session'").fetch_one(&r.e.app.db).await.unwrap();
            assert_eq!(count, i64::from(already_gone));
        }
    }

    #[tokio::test]
    async fn r2_promote_recovery_retired_only_enqueues_scan() {
        use std::os::unix::fs::MetadataExt as _;
        for already_gone in [false, true] {
            let r = rig(true).await;
            r2_api_sentinel(&r).await;
            let staged = stage_transcript(&r.src(), r.dest().parent().unwrap(), SID).unwrap();
            let source = std::fs::metadata(r.src()).unwrap(); let dest = std::fs::metadata(r.dest()).unwrap();
            sqlx::query("UPDATE runs SET state='stopped',native_session_id=?,transcript_path=? WHERE bot_id=?").bind(SID).bind(r.src().to_string_lossy().to_string()).bind(&r.child).execute(&r.e.app.db).await.unwrap();
            let new_id = db::ulid();
            let payload = json!({"new_id":new_id,"name":"kid-recovered","project_id":r.e.project_id,"session_id":SID,"source":r.src().to_string_lossy(),"source_identity":{"dev":source.dev(),"ino":source.ino()},"staged_identity":{"dev":dest.dev(),"ino":dest.ino()},"dest":staged.dest.to_string_lossy(),"created":staged.created.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>()});
            let intent = crate::delete_intents::begin(&r.e.app, "promote", &r.child, LOCAL_HOST, &payload).await.unwrap();
            if already_gone {
                let pool = r.e.app.db.clone(); let child = r.child.clone();
                lifecycle::race_point::arm("promote_seed", &r.child, move || async move {
                    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(child).execute(&pool).await.unwrap();
                });
            }
            assert!(matches!(crate::promote_intents::drive_once(&r.e.app, &intent).await, crate::runners::restart_intents::Outcome::Finished));
            assert_eq!(intent_status(&r.e.app, &r.child).await, vec!["done"]);
            assert_eq!(std::fs::read_to_string(r.dest()).unwrap(), std::fs::read_to_string(r.src()).unwrap());
            r.e.app.background_tasks.close();
            tokio::time::timeout(std::time::Duration::from_secs(10), r.e.app.background_tasks.wait()).await.unwrap();
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-sentinel-session'").fetch_one(&r.e.app.db).await.unwrap();
            assert_eq!(count, i64::from(already_gone));
        }
    }

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

    #[tokio::test]
    async fn a_transcript_append_after_staging_is_included_in_the_promoted_session() {
        let r = rig(true).await;
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (r2, go) = (reached.clone(), release.clone());
        lifecycle::race_point::arm("promote_after_intent", &r.child, move || async move {
            r2.notify_one();
            go.notified().await;
        });
        let (app, child) = (r.e.app.clone(), r.child.clone());
        let task = tokio::spawn(async move { promote_owned(app, child, PromoteReq::default()).await });
        reached.notified().await;
        use std::io::Write as _;
        let mut src = std::fs::OpenOptions::new().append(true).open(r.src()).unwrap();
        writeln!(src, "{{\"type\":\"assistant\",\"message\":\"last-before-stop\"}}").unwrap();
        drop(src);
        release.notify_one();

        let out = task.await.unwrap().unwrap();
        assert!(out["run_id"].is_string(), "{out}");
        assert_eq!(std::fs::read_to_string(r.dest()).unwrap(), std::fs::read_to_string(r.src()).unwrap(), "seeded resume copy includes the final complete message");
        assert!(std::fs::read_to_string(r.dest()).unwrap().contains("last-before-stop"));
        assert!(r.src().is_file(), "source transcript is retained");
    }

    #[tokio::test]
    async fn staging_a_transcript_onto_itself_is_a_noop() {
        let r = rig(true).await;
        let before = std::fs::read(&r.src()).unwrap();
        let staged = stage_transcript(&r.src(), r.src().parent().unwrap(), SID).unwrap();
        assert_eq!(staged.dest, r.src());
        assert!(staged.created.is_empty());
        assert_eq!(std::fs::read(r.src()).unwrap(), before);
    }

    #[tokio::test]
    async fn promote_recovery_finalizes_the_owned_copy_before_native_resume() {
        use std::os::unix::fs::MetadataExt as _;

        let r = rig(true).await;
        let staged = stage_transcript(&r.src(), r.dest().parent().unwrap(), SID).unwrap();
        let source_meta = std::fs::metadata(r.src()).unwrap();
        let staged_meta = std::fs::metadata(r.dest()).unwrap();
        use std::io::Write as _;
        let mut src = std::fs::OpenOptions::new().append(true).open(r.src()).unwrap();
        writeln!(src, "{{\"type\":\"assistant\",\"message\":\"recovered-final-message\"}}").unwrap();
        drop(src);
        sqlx::query("UPDATE runs SET state = 'stopped', native_session_id = ?, transcript_path = ? WHERE bot_id = ?")
            .bind(SID)
            .bind(r.src().to_string_lossy().to_string())
            .bind(&r.child)
            .execute(&r.e.app.db)
            .await
            .unwrap();

        let new_id = db::ulid();
        let payload = json!({
            "new_id": new_id,
            "name": "kid-recovered",
            "model": null,
            "effort": null,
            "project_id": r.e.project_id,
            "session_id": SID,
            "source": r.src().to_string_lossy(),
            "source_identity": {"dev": source_meta.dev(), "ino": source_meta.ino()},
            "staged_identity": {"dev": staged_meta.dev(), "ino": staged_meta.ino()},
            "dest": staged.dest.to_string_lossy(),
            "created": staged.created.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
        });
        let intent = crate::delete_intents::begin(&r.e.app, "promote", &r.child, LOCAL_HOST, &payload).await.unwrap();
        assert!(matches!(crate::promote_intents::drive_once(&r.e.app, &intent).await, crate::runners::restart_intents::Outcome::Finished));

        assert_eq!(std::fs::read_to_string(r.dest()).unwrap(), std::fs::read_to_string(r.src()).unwrap(), "recovery must refresh before seeding or starting");
        assert!(std::fs::read_to_string(r.dest()).unwrap().contains("recovered-final-message"));
        let seeded: Option<String> = sqlx::query_scalar("SELECT transcript_path FROM runs WHERE bot_id = ? AND native_session_id = ? ORDER BY rowid DESC LIMIT 1")
            .bind(&new_id)
            .bind(SID)
            .fetch_optional(&r.e.app.db)
            .await
            .unwrap();
        let dest_path = r.dest().to_string_lossy().to_string();
        assert_eq!(seeded.as_deref(), Some(dest_path.as_str()));
        assert!(r.src().is_file());
    }

    #[tokio::test]
    async fn a_stopped_promote_recovered_before_final_sync_includes_the_last_message() {
        let r = rig(true).await;
        let src_path = r.src();
        lifecycle::race_point::arm("promote_after_intent", &r.child, move || async move {
            use std::io::Write as _;
            let mut src = std::fs::OpenOptions::new().append(true).open(src_path).unwrap();
            writeln!(src, "{{\"type\":\"assistant\",\"message\":\"last-before-recovery\"}}").unwrap();
        });

        die_at(&r, "promote_before_final_transcript").await;
        assert_eq!(intent_status(&r.e.app, &r.child).await, vec!["running"]);
        assert!(!std::fs::read_to_string(r.dest()).unwrap().contains("last-before-recovery"));
        assert_eq!(child_state(&r).await, (false, None), "child stopped before recovery");

        let app2 = crate::testing::restart_app(&r.e).await;
        let intent: String = sqlx::query_scalar("SELECT id FROM intents WHERE subject_id = ? AND kind = 'promote'")
            .bind(&r.child)
            .fetch_one(&app2.db)
            .await
            .unwrap();
        assert!(matches!(crate::promote_intents::drive_once(&app2, &intent).await, crate::runners::restart_intents::Outcome::Finished));
        assert_eq!(intent_status(&app2, &r.child).await, vec!["done"]);
        assert_eq!(std::fs::read_to_string(r.dest()).unwrap(), std::fs::read_to_string(r.src()).unwrap());
        assert!(std::fs::read_to_string(r.dest()).unwrap().contains("last-before-recovery"));
        let new_id = app2.cfg.get().await.projects[0].bots[0].id.clone().unwrap();
        assert!(db::active_run(&app2.db, &new_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_destination_replaced_after_staging_is_never_overwritten() {
        let r = rig(true).await;
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (r2, go) = (reached.clone(), release.clone());
        lifecycle::race_point::arm("promote_after_intent", &r.child, move || async move {
            r2.notify_one();
            go.notified().await;
        });
        let (app, child) = (r.e.app.clone(), r.child.clone());
        let task = tokio::spawn(async move { promote_owned(app, child, PromoteReq::default()).await });
        reached.notified().await;
        std::fs::remove_file(r.dest()).unwrap();
        std::fs::write(r.dest(), "owned by someone else\n").unwrap();
        release.notify_one();

        let error = task.await.unwrap().unwrap_err();
        assert_eq!(reason(error), "transcript_copy_failed");
        assert_eq!(std::fs::read_to_string(r.dest()).unwrap(), "owned by someone else\n");
        assert_eq!(child_state(&r).await, (false, None));
        assert!(cfg_bot_names(&r).await.is_empty());
    }

    #[tokio::test]
    async fn an_incomplete_final_jsonl_record_cannot_be_promoted() {
        let r = rig(true).await;
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (r2, go) = (reached.clone(), release.clone());
        lifecycle::race_point::arm("promote_after_intent", &r.child, move || async move {
            r2.notify_one();
            go.notified().await;
        });
        let (app, child) = (r.e.app.clone(), r.child.clone());
        let task = tokio::spawn(async move { promote_owned(app, child, PromoteReq::default()).await });
        reached.notified().await;
        use std::io::Write as _;
        let mut src = std::fs::OpenOptions::new().append(true).open(r.src()).unwrap();
        src.write_all(b"{\"type\":\"assistant\"").unwrap();
        drop(src);
        release.notify_one();

        let error = task.await.unwrap().unwrap_err();
        assert_eq!(reason(error), "transcript_copy_failed");
        assert_eq!(child_state(&r).await, (false, None));
        assert!(cfg_bot_names(&r).await.is_empty());
        assert!(r.src().is_file());
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

    /// #1455：child 的 run 還是 running、pane 已經不在（herdr 回 pane_not_found）：回 409 session_not_found、什麼都不動，
    /// 跟「pane 裡找不到 claude」同一類，不是 502 upstream。
    #[tokio::test]
    async fn a_child_whose_pane_is_gone_is_refused_not_an_upstream_error() {
        let r = rig(true).await;
        r.e.herdr.fail_next("pane.process_info", crate::testing::Fault::RefuseWith("pane_not_found"));
        assert_eq!(reason(promote(&r, PromoteReq::default()).await.unwrap_err()), "session_not_found");
        assert_eq!(child_state(&r).await, (false, Some("running".into())));
        assert!(cfg_bot_names(&r).await.is_empty());
        assert!(!r.dest().exists());
    }

    /// #1455：process_info 的其他 herdr 錯誤照舊是 502（只有 pane_not_found 才改成 409），什麼都不動。
    #[tokio::test]
    async fn other_herdr_failures_on_process_info_stay_upstream() {
        let r = rig(true).await;
        r.e.herdr.fail_next("pane.process_info", crate::testing::Fault::Refuse);
        let err = promote(&r, PromoteReq::default()).await.unwrap_err();
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
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

    /// 收回時停不下來（`stopping` 寫不進 DB、agent 還在跑）：`roll_back_new_bot` 不能回報「收乾淨」，
    /// 不然呼叫端會把 `rolled_back: true` 當成真的，而且 `abandon` 之後沒有任何東西會再停它。
    #[tokio::test]
    async fn a_rollback_that_could_not_stop_the_new_bot_is_not_reported_as_rolled_back() {
        let r = rig(true).await;
        let app = r.e.app.clone();
        let new_id = db::ulid();
        let child = db::bot(&app.db, &r.child).await.unwrap().unwrap();
        let user_cfg = user_bot_cfg(&child, &new_id, "kid-top", &None, &None);
        let pid = r.e.project_id.clone();
        crate::projection::update_and_project(&app.cfg, &app.db, move |cfg| {
            cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).unwrap().bots.push(user_cfg.clone());
            Ok(())
        })
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at) VALUES (?,?,'running','idle','pane-new','test',?)",
        )
        .bind(db::ulid())
        .bind(&new_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER refuse_stopping BEFORE UPDATE OF state ON runs WHEN NEW.state = 'stopping' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let staged = Staged { created: vec![], dest: r.dest(), ..Staged::default() };
        let rolled_back = roll_back_new_bot(&app, &new_id, &staged).await;
        assert!(!rolled_back, "停不下來的新 bot 不能算已收回：agent 還在跑");
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

    /// #1358：promote 的 handler 在 child 已經停掉、目標 bot 還沒建的那一瞬被 drop（client 斷線）：脫離出去的工作照樣做完。
    #[tokio::test]
    async fn a_promote_whose_handler_is_dropped_after_the_child_is_stopped_still_finishes() {
        let r = rig(true).await;
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (r2, go) = (reached.clone(), release.clone());
        lifecycle::race_point::arm("promote_after_stop", &r.child, move || async move {
            r2.notify_one();
            go.notified().await;
        });
        let (app, id) = (r.e.app.clone(), r.child.clone());
        let h = tokio::spawn(async move { promote_bot(State(app), Path(id), Extension(crate::api::RequestPrincipal::User), None).await.map(|_| ()) });
        reached.notified().await;
        h.abort(); // 瀏覽器斷線：handler future 被 drop（脫離出去的工作不受影響）
        let _ = h.await;
        release.notify_one();
        crate::api::wait_work_done_for_test(&r.child).await;
        assert_eq!(intent_status(&r.e.app, &r.child).await, vec!["done"], "intent 收成 done，不停在 running");
        assert!(child_state(&r).await.0, "child 收掉了");
        let bots = r.e.app.cfg.get().await.projects[0].bots.clone();
        let new_id = bots.iter().filter_map(|b| b.id.clone()).find(|i| *i != r.child).expect("config 裡多了目標 user bot");
        assert!(db::active_run(&r.e.app.db, &new_id).await.unwrap().is_some(), "目標 bot 有 active run");
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
