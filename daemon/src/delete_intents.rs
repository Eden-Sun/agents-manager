//! 開機補完被打斷的刪除（#355 P3，設計見該票；#284／#296）。
//!
//! `delete_bot`／`delete_project` 的「定案」（config 與 DB 軟刪）之後還有一串收尾：停 child、軟刪 child、清目錄、停母 bot。定案之後 daemon 死掉，
//! 留下的是「母 bot／專案已刪、child 還活著」——reconcile 不掃（要專案活著）、重按只得 not_found／not_in_config。handler 在**定案之前**先 commit 一件
//! `delete_bot`／`delete_project` intent（payload＝**當時**的 bot 快照，之後才出現的 child 不在授權範圍）；開機／主機重連對帳成功後
//! （`recover_host`）檢查世界：
//! - 母 bot／專案還活著（定案沒發生）→ `abandoned`；
//! - 已定案 → 往前補完：逐顆（快照裡的）停、軟刪、清目錄，全部做完 `done`。每一步先驗世界（已軟刪就不再軟刪、沒 run 停機是 no-op），所以重跑安全。
//!
//! 補不成（軟刪寫不進去…）最多試 [`crate::intents::MAX_ATTEMPTS`] 次，用完 `failed` 並同交易推 AGM inbox。目錄清不掉（ssh 失敗、run 還在）不算補不成：
//! 那條路本來就有自己的帳（`kept_dirs`、開機清掃、`remote_purge`）。

use crate::api::{lock_bots_in_order, soft_delete_child, stop_for_delete_locked};
use crate::db;
use crate::intents::{self, Inserted, Intent};
use crate::lifecycle;
use crate::restart_intents::Outcome;
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;

/// 放置多久還沒補完就放棄（`failed`＋通知）。
const DELETE_INTENT_TTL_SECS: i64 = 60 * 60;

/// handler 在定案**之前**呼叫：寫 intent 並立刻認領（這個行程自己在做，同一個 boot 的 recovery 不會來搶）。
/// 寫不進去＝什麼都還沒動，呼叫端不能繼續（fail closed）。
pub async fn begin(app: &Arc<App>, kind: &str, subject: &str, host: &str, payload: &Value) -> Result<String, crate::lifecycle::LcError> {
    let id = match intents::insert(&app.db, kind, subject, host, payload, DELETE_INTENT_TTL_SECS).await {
        // 已經有一件開著：上一個行程留下的（我們持著鎖、這次自己來做），沿用它。
        Ok(Inserted::New(i)) | Ok(Inserted::AlreadyOpen(i)) => i.id,
        Err(e) => return Err(crate::lifecycle::LcError::Upstream(format!("cannot record the {kind} intent: {e:#}"))),
    };
    // 認領失敗（讀不到）不擋刪除：intent 已經在，最壞是開機時由 recovery 驗證世界後收掉。
    if let Err(e) = intents::claim(&app.db, &id, &app.boot_id).await {
        tracing::warn!(intent = %id, error = %e, "could not claim the delete intent");
    }
    Ok(id)
}

/// 定案沒成（世界沒變）：收成 `abandoned`。
pub async fn abandon(app: &Arc<App>, id: &str, why: &str) {
    if let Err(e) = intents::abandon(&app.db, id, why).await {
        tracing::warn!(intent = id, error = %e, "could not abandon the delete intent");
    }
}

/// 做完了：收成 `done`。標不成不影響結果（開機驗證世界後會收）。
pub async fn complete(app: &Arc<App>, id: &str) {
    if let Err(e) = intents::complete(&app.db, id).await {
        tracing::warn!(intent = id, error = %e, "could not complete the delete intent; boot recovery will verify it");
    }
}

/// handler 有東西沒做成（軟刪寫不進去）：intent 留著、背景用 recovery 的重試補完（取代各路徑自己的記憶體重試 task）。
pub fn spawn_retry(app: Arc<App>, id: String, why: String) {
    tokio::spawn(async move {
        match intents::record_failure(&app.db, &id, &why).await {
            Ok(true) => return, // 已放棄並通知
            Ok(false) => {}
            Err(e) => tracing::warn!(intent = %id, error = %e, "could not record the delete failure; retrying anyway"),
        }
        retry_loop(&app, &id).await;
    });
}

async fn retry_loop(app: &Arc<App>, id: &str) {
    for attempt in 0.. {
        tokio::time::sleep(crate::reconcile::recovery_retry_delay(attempt)).await;
        if drive_once(app, id).await == Outcome::Finished {
            return;
        }
    }
}

/// 開機／主機重連對帳成功之後：這台主機上還開著的刪除 intent 各補一次（內嵌）；補不成的丟背景重試。
/// （過期的收尾與已結束的清理在 `restart_intents::recover_host` 做過了。）
pub async fn recover_host(app: &Arc<App>, host: &str) {
    let open = match intents::open(&app.db).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, host, "cannot list open delete intents yet");
            return;
        }
    };
    for i in open.into_iter().filter(|i| (i.kind == "delete_bot" || i.kind == "delete_project") && i.host == host) {
        if let Outcome::Retry(why) = drive_once(app, &i.id).await {
            tracing::warn!(intent = %i.id, subject = %i.subject_id, error = %why, "interrupted delete could not be completed yet; retrying in the background");
            let (app, id) = (app.clone(), i.id.clone());
            tokio::spawn(async move { retry_loop(&app, &id).await });
        }
    }
}

/// 認領一次並補一輪。
pub async fn drive_once(app: &Arc<App>, id: &str) -> Outcome {
    match intents::claim(&app.db, id, &app.boot_id).await {
        Ok(true) => {}
        Ok(false) => return Outcome::Finished,
        Err(e) => return Outcome::Retry(format!("cannot claim the intent: {e:#}")),
    }
    let intent = match intents::get(&app.db, id).await {
        Ok(Some(i)) => i,
        Ok(None) => return Outcome::Finished,
        Err(e) => return fail_attempt(app, id, format!("cannot read the intent: {e:#}")).await,
    };
    let res = match intent.kind.as_str() {
        "delete_bot" => resume_bot(app, &intent).await,
        "delete_project" => resume_project(app, &intent).await,
        other => Err(format!("unknown delete intent kind {other}")),
    };
    match res {
        Ok(()) => Outcome::Finished,
        Err(why) => fail_attempt(app, id, why).await,
    }
}

async fn fail_attempt(app: &Arc<App>, id: &str, why: String) -> Outcome {
    match intents::record_failure(&app.db, id, &why).await {
        Ok(true) => {
            tracing::error!(intent = id, error = %why, "interrupted delete could not be completed; gave up and told AGM");
            Outcome::Finished
        }
        Ok(false) => Outcome::Retry(why),
        Err(e) => Outcome::Retry(format!("{why}（且記不下失敗：{e:#}）")),
    }
}

fn snapshot(intent: &Intent) -> Vec<(String, String)> {
    intent
        .payload()
        .get("bots")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|b| Some((b.get("id")?.as_str()?.to_string(), b.get("managed_by").and_then(|m| m.as_str()).unwrap_or("child").to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// 鎖住整批之後再確認這件還是我們認領的（等鎖的時間裡活著的 handler 可能已經收尾）。
async fn still_ours(app: &Arc<App>, intent: &Intent) -> Result<bool, String> {
    Ok(matches!(intents::get(&app.db, &intent.id).await.map_err(|e| format!("db: {e:#}"))?, Some(c) if c.status == "running"))
}

async fn in_config_bot(app: &Arc<App>, id: &str) -> bool {
    app.cfg.get().await.projects.iter().any(|p| p.bots.iter().any(|b| b.id.as_deref() == Some(id)))
}

async fn resume_bot(app: &Arc<App>, intent: &Intent) -> Result<(), String> {
    let parent = intent.subject_id.clone();
    let children = snapshot(intent);
    let mut ids: Vec<String> = children.iter().map(|(id, _)| id.clone()).collect();
    ids.push(parent.clone());
    let (_ids, _guards) = lock_bots_in_order(app, ids).await;
    if !still_ours(app, intent).await? {
        return Ok(());
    }
    let db_err = |e: anyhow::Error| format!("db: {e:#}");
    match db::bot(&app.db, &parent).await.map_err(db_err)? {
        // 母 bot 從來沒刪成：定案沒發生，世界沒變。（user bot 的 config 已寫、DB 投影還沒跟上＝稍後重試。）
        Some(b) if b.deleted_at.is_none() => {
            if b.managed_by != "child" && !in_config_bot(app, &parent).await {
                return Err("the bot is gone from config.toml but its soft delete is not projected yet".into());
            }
            intents::abandon(&app.db, &intent.id, "the delete was never decided").await.map_err(|e| format!("{e:#}"))?;
            return Ok(());
        }
        _ => {}
    }
    let host = intent.host.clone();
    // 快照裡的 child（深的先）：停、軟刪、清目錄。
    for (cid, _) in &children {
        let Some(c) = db::bot(&app.db, cid).await.map_err(db_err)? else { continue };
        let settled = stop_for_delete_locked(app, cid).await;
        if c.deleted_at.is_none() {
            soft_delete_child(app, cid).await.map_err(|e| format!("cannot soft-delete child {cid}: {e}"))?;
        }
        if settled.is_ok() {
            let child_host = db::bot_host(&app.db, cid).await.unwrap_or_else(|_| host.clone());
            lifecycle::purge_bot_dir(app, cid, &child_host).await;
        }
        app.emit("bot_changed", json!({"bot_id": cid})).await;
    }
    if stop_for_delete_locked(app, &parent).await.is_ok() {
        lifecycle::purge_bot_dir(app, &parent, &host).await;
    }
    app.emit("bot_changed", json!({"bot_id": parent})).await;
    intents::complete(&app.db, &intent.id).await.map_err(|e| format!("{e:#}"))?;
    Ok(())
}

async fn resume_project(app: &Arc<App>, intent: &Intent) -> Result<(), String> {
    let project_id = intent.subject_id.clone();
    let bots = snapshot(intent);
    let (_ids, _guards) = lock_bots_in_order(app, bots.iter().map(|(id, _)| id.clone()).collect()).await;
    if !still_ours(app, intent).await? {
        return Ok(());
    }
    let db_err = |e: anyhow::Error| format!("db: {e:#}");
    match db::project(&app.db, &project_id).await.map_err(db_err)? {
        Some(p) if p.deleted_at.is_none() => {
            if app.cfg.get().await.projects.iter().any(|q| q.id.as_deref() == Some(project_id.as_str())) {
                intents::abandon(&app.db, &intent.id, "the delete was never decided").await.map_err(|e| format!("{e:#}"))?;
                return Ok(());
            }
            return Err("the project is gone from config.toml but its soft delete is not projected yet".into());
        }
        _ => {}
    }
    let host = intent.host.clone();
    for (bid, managed_by) in &bots {
        let Some(b) = db::bot(&app.db, bid).await.map_err(db_err)? else { continue };
        if b.deleted_at.is_none() {
            if managed_by == "user" {
                return Err(format!("user bot {bid} is not soft-deleted yet (projection pending)"));
            }
            soft_delete_child(app, bid).await.map_err(|e| format!("cannot soft-delete child {bid}: {e}"))?;
        }
        // 目錄清不掉不算補不成：本機留給開機清掃、遠端有 `remote_purge` 的帳。
        lifecycle::purge_bot_dir(app, bid, &host).await;
    }
    app.emit("project_changed", json!({"project_id": project_id})).await;
    intents::complete(&app.db, &intent.id).await.map_err(|e| format!("{e:#}"))?;
    Ok(())
}
