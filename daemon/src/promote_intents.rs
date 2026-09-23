//! 開機補完被打斷的 promote（#355 P4；#248）。
//!
//! promote＝複製 transcript → **停 child（回不去）** → 建 user bot（承諾點）→ 種下 session → native resume 啟動 → 收掉 child。
//! handler 在停 child 之前先 commit `promote` intent（payload＝新 bot id、名字、模型、session、複製了哪些檔）。daemon 死掉的話，
//! 開機／主機重連對帳成功後（`recover_host`）檢查世界，**往前補完、不回滾**（使用者 2026-09-20 裁示）：
//!
//! | 世界 | 判斷 |
//! |---|---|
//! | child 還在跑（停 child 從沒發生） | 承諾之前 → 收回複製、`abandoned`，可原樣重送 |
//! | child 已停（或已收掉） | 補完：目標 bot 沒建就建（設定跟 handler 同一份 `user_bot_cfg`）→ 沒種過 session 就種 → 還沒起過就 native resume 起 → 收掉 child → `done` |
//!
//! 每步先驗世界，重跑安全。補不成最多試 [`crate::intents::MAX_ATTEMPTS`] 次（背景退避），用完 `failed` 並同交易推 AGM inbox；
//! 不回滾：child 已停、session 記在它的 run 上，人工或重送同一個請求都接得回。

use crate::config::LOCAL_HOST;
use crate::db;
use crate::intents::{self, Intent};
use crate::lifecycle::{self, StartOpts};
use crate::restart_intents::Outcome;
use crate::state::App;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// 這台主機上還開著的 promote intent 各補一次（內嵌）；補不成的丟背景重試。promote 只支援本機專案。
pub async fn recover_host(app: &Arc<App>, host: &str) {
    if host != LOCAL_HOST {
        return;
    }
    let open = match intents::open(&app.db).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "cannot list open promote intents yet");
            return;
        }
    };
    for i in open.into_iter().filter(|i| i.kind == "promote") {
        if let Outcome::Retry(why) = drive_once(app, &i.id).await {
            tracing::warn!(intent = %i.id, child = %i.subject_id, error = %why, "interrupted promote could not be completed yet; retrying in the background");
            let (app, id) = (app.clone(), i.id.clone());
            tokio::spawn(async move { retry_loop(&app, &id).await });
        }
    }
}

async fn retry_loop(app: &Arc<App>, id: &str) {
    for attempt in 0.. {
        tokio::time::sleep(crate::reconcile::recovery_retry_delay(attempt)).await;
        if drive_once(app, id).await == Outcome::Finished {
            return;
        }
    }
}

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
    match resume(app, &intent).await {
        Ok(()) => Outcome::Finished,
        Err(why) => fail_attempt(app, id, why).await,
    }
}

async fn fail_attempt(app: &Arc<App>, id: &str, why: String) -> Outcome {
    match intents::record_failure(&app.db, id, &why).await {
        Ok(true) => {
            tracing::error!(intent = id, error = %why, "interrupted promote could not be completed; gave up and told AGM");
            Outcome::Finished
        }
        Ok(false) => Outcome::Retry(why),
        Err(e) => Outcome::Retry(format!("{why}（且記不下失敗：{e:#}）")),
    }
}

fn s(p: &serde_json::Value, k: &str) -> Option<String> {
    p.get(k).and_then(|v| v.as_str()).map(str::to_string)
}

/// 種下 native resume 要讀的那一列（handler 與開機補完共用）。**`started_at` 與 `ended_at` 必須是同一個值**：
/// 補完靠 `ended_at = started_at` 認出「這列是種的、這顆還沒真的起過」。以前兩欄各叫一次 `db::now()`，
/// 兩次呼叫跨過毫秒邊界時這列就像「起過又停了」，補完跳過 native resume、直接收 child 記 `done`——
/// 目標 bot 從沒起來、transcript 不見也照樣 `done`（#401）。`child_id` 只是測試注入點的鍵。
pub(crate) async fn seed_session_run(pool: &sqlx::SqlitePool, child_id: &str, new_id: &str, session_id: &str, dest: &str) -> sqlx::Result<()> {
    let at = db::now();
    #[cfg(test)]
    lifecycle::race_point::hit("promote_seed", child_id).await;
    #[cfg(not(test))]
    let _ = child_id;
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
         VALUES (?,?,'stopped','idle',?,?,?,?)",
    )
    .bind(db::ulid())
    .bind(new_id)
    .bind(session_id)
    .bind(dest)
    .bind(&at)
    .bind(&at)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn resume(app: &Arc<App>, intent: &Intent) -> Result<(), String> {
    let e = |e: anyhow::Error| format!("db: {e:#}");
    let child_id = intent.subject_id.clone();
    let p = intent.payload();
    let (Some(new_id), Some(name), Some(session_id), Some(dest), Some(project_id)) =
        (s(&p, "new_id"), s(&p, "name"), s(&p, "session_id"), s(&p, "dest"), s(&p, "project_id"))
    else {
        return Err("the promote intent payload is unreadable".into());
    };
    let model = s(&p, "model");
    let effort = s(&p, "effort");
    let created: Vec<PathBuf> = p.get("created").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(PathBuf::from)).collect()).unwrap_or_default();

    let lock = app.bot_lock(&child_id).await;
    let _g = lock.lock().await;
    match intents::get(&app.db, &intent.id).await.map_err(e)? {
        Some(cur) if cur.status == "running" => {}
        _ => return Ok(()),
    }
    let child = db::bot(&app.db, &child_id).await.map_err(e)?;
    let child_live = child.as_ref().is_some_and(|c| c.deleted_at.is_none());
    // child 還在跑：停 child 從沒發生（承諾之前）。
    if child_live && db::active_run(&app.db, &child_id).await.map_err(e)?.is_some() {
        for path in created.iter().rev() {
            let _ = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
        }
        intents::abandon(&app.db, &intent.id, "the child was never stopped").await.map_err(|x| format!("{x:#}"))?;
        return Ok(());
    }

    // 往前補完。1. 目標 bot 沒建就建。
    let in_cfg = app.cfg.get().await.projects.iter().any(|q| q.bots.iter().any(|b| b.id.as_deref() == Some(new_id.as_str())));
    let row = db::bot(&app.db, &new_id).await.map_err(e)?;
    if !in_cfg && row.is_none() {
        let Some(child) = child.as_ref() else { return Err("the child is gone and the target bot was never created".into()) };
        let cfg_entry = crate::promote::user_bot_cfg(child, &new_id, &name, &model, &effort);
        let pid = project_id.clone();
        crate::projection::update_and_project(&app.cfg, &app.db, move |cfg| {
            let proj = cfg.projects.iter_mut().find(|q| q.id.as_deref() == Some(pid.as_str())).ok_or_else(|| anyhow::anyhow!("not-in-config"))?;
            proj.bots.push(cfg_entry.clone());
            Ok(())
        })
        .await
        .map_err(|x| format!("cannot create the promoted bot: {x:#}"))?;
    }
    // 2. 沒種過 session 就種（native resume 讀「最近一個結束的 run 的 native_session_id」）。
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ?").bind(&new_id).fetch_one(&app.db).await.map_err(|x| x.to_string())?;
    if runs == 0 {
        seed_session_run(&app.db, &child_id, &new_id, &session_id, &dest).await.map_err(|x| format!("cannot seed the session: {x}"))?;
    }
    // 3. 還沒起過（沒有 active run、也沒有一個真的起過的 run；種下的那列 started_at＝ended_at）就 native resume 起。
    let started_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ? AND (ended_at IS NULL OR ended_at <> started_at)").bind(&new_id).fetch_one(&app.db).await.map_err(|x| x.to_string())?;
    if started_before == 0 {
        lifecycle::start_bot_with(app, &new_id, StartOpts { resume_native: true, resume_required: true, ..Default::default() })
            .await
            .map_err(|x| format!("cannot start the promoted bot: {x:?}"))?;
    }
    // 4. 收掉 child 紀錄（它的 run 已經停了）。
    if child_live {
        crate::child_retire::retire(app, &child_id, "promote_recovered", crate::child_retire::Mode::Explicit)
            .await
            .map_err(|x| format!("cannot retire the child: {x:#}"))?;
        lifecycle::purge_bot_dir(app, &child_id, LOCAL_HOST).await;
    }
    app.emit("bot_changed", json!({"bot_id": child_id})).await;
    app.emit("bot_changed", json!({"bot_id": new_id})).await;
    app.emit("project_changed", json!({"project_id": project_id})).await;
    intents::complete(&app.db, &intent.id).await.map_err(|x| format!("{x:#}"))?;
    Ok(())
}
