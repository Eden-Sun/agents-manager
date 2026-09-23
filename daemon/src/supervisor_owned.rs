//! 哪些 bot 是 AGM 的，刪之前要人明確確認（issue #406）。
//!
//! 2026-09-23 13:28Z AGM 專案裡的 `build` 與 `agm-pxf2pv-triage` 被兩次 `DELETE /api/bots/{id}` 軟刪。#398 的保護只認
//! `parent_bot_id ∈ supervisors`，可是 AGM 的固定工人（build、triage、browser-gc、responder）都是 `managed_by=user`、
//! `parent_bot_id=NULL`，直接放在總管的專案底下——那條判斷對它們一個都認不出來。所以改成三條任一成立就算：
//! 1. 本身就是總管或某個角色（`supervisors.bot_id`／`supervisor_roles.bot_id`）；
//! 2. parent 是上面那些；
//! 3. 在總管或角色的專案裡（`supervisors.project_id`／`supervisor_roles.project_id`，即 `supervisor/*` 那個目錄）。
//!
//! 認出來之後：隱式投影一律拒絕（沒有放行開關）；刪除 API 要帶 `?confirm=supervisor`。兩條被擋時都推一筆 `ops_alert`
//! 給巡檢——不准靜默刪，也不准靜默擋。

use crate::db;
use anyhow::Result;
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone)]
pub struct Owned {
    bot_ids: HashSet<String>,
    project_ids: HashSet<String>,
    /// 總管／角色本身的 bot id → 給人看的角色名（網頁的二次確認框要寫「這顆是 AGM 的什麼」）。
    roles: HashMap<String, &'static str>,
}

pub async fn load(pool: &SqlitePool) -> Result<Owned> {
    let mut o = Owned::default();
    // `supervisors` 那一列就是巡檢本人；`supervisor_roles.role` 是 'patrol' | 'responder'。
    let sup: Vec<(Option<String>, Option<String>)> = sqlx::query_as("SELECT bot_id, project_id FROM supervisors").fetch_all(pool).await?;
    let roles: Vec<(Option<String>, Option<String>, String)> =
        sqlx::query_as("SELECT bot_id, project_id, role FROM supervisor_roles").fetch_all(pool).await?;
    let rows = sup.into_iter().map(|(b, p)| (b, p, "patrol".to_string())).chain(roles);
    for (bot, project, role) in rows {
        if let Some(bot) = bot.filter(|s| !s.is_empty()) {
            let label = if role == "responder" { "AGM 協調者" } else { "AGM 總管（巡檢）" };
            o.roles.entry(bot.clone()).or_insert(label);
            o.bot_ids.insert(bot);
        }
        o.project_ids.extend(project.filter(|s| !s.is_empty()));
    }
    Ok(o)
}

impl Owned {
    pub fn owns(&self, b: &db::Bot) -> bool {
        self.bot_ids.contains(&b.id)
            || b.parent_bot_id.as_ref().is_some_and(|p| self.bot_ids.contains(p))
            || self.project_ids.contains(&b.project_id)
    }

    /// 這顆在 AGM 裡是什麼（只對 [`Self::owns`] 為真的 bot 有意義）。
    pub fn role(&self, b: &db::Bot) -> &'static str {
        if let Some(r) = self.roles.get(&b.id) {
            r
        } else if b.parent_bot_id.as_ref().is_some_and(|p| self.bot_ids.contains(p)) {
            "AGM 開出去的子 agent"
        } else {
            "AGM 專案裡的常駐工人"
        }
    }
}

/// 推一筆 `ops_alert` 給巡檢。推不進去只記 log：擋下本身已經做完了。
///
/// dedupe 的鍵帶 `subject`（被擋的那顆 bot／那個專案，review d77434c0 #4）：`push_inbox` 是
/// `INSERT OR IGNORE`，只用 `reason:hour` 的話，同一小時第二顆被擋的 bot 就只剩一行 log、inbox 沒有——
/// 13:28Z 那次正好是 4 秒內兩顆，會漏掉第二顆。同一顆連按才收斂成一則。
pub async fn alert(pool: &SqlitePool, reason: &str, subject: &str, detail: &str) {
    let hour = db::now().get(..13).unwrap_or_default().to_string();
    let subject: String = subject.chars().filter(|c| !c.is_control() && *c != ':').take(80).collect();
    let key = format!("ops_alert:daemon:{reason}:{subject}:{hour}");
    let payload = json!({
        "source": "daemon",
        "reason": reason,
        "subject": subject,
        "detail": detail,
        "action": "有東西要刪 AGM 的 bot，daemon 已擋下、什麼都沒刪：查 detail 裡的呼叫端與 config.toml 寫入紀錄（daemon.log 搜 `config.toml written`），確認是不是該刪；真的要刪就 `agm bot delete <id> --confirm-supervisor`",
    });
    match crate::supervisor::store::push_inbox(pool, &key, "ops_alert", None, None, None, &payload).await {
        Ok(_) => tracing::warn!(reason, subject, detail, "refused to delete an AGM bot; ops_alert queued"),
        Err(e) => tracing::error!(reason, subject, detail, error = %e, "refused to delete an AGM bot; ops_alert could not be queued"),
    }
}

/// 刪除 API 的閘門：要刪的 bot 是 AGM 的，而且沒有帶 `confirm=supervisor` → 409 `supervisor_owned`＋`ops_alert`，什麼都不動。
/// bot 不存在就放過（交給刪除本身回 404）。
pub async fn guard_bot_delete(pool: &SqlitePool, bot_id: &str, confirm: Option<&str>) -> Result<(), crate::lifecycle::LcError> {
    let up = |e: anyhow::Error| crate::lifecycle::LcError::Upstream(format!("{e:#}"));
    let Some(bot) = db::bot(pool, bot_id).await.map_err(up)? else { return Ok(()) };
    let owned = load(pool).await.map_err(up)?;
    if bot.deleted_at.is_some() || !owned.owns(&bot) {
        return Ok(());
    }
    let role = owned.role(&bot);
    refuse_unless_confirmed(
        pool,
        confirm,
        &bot.id,
        &format!("bot `{}`（{}，{role}）", bot.name, bot.id),
        json!({"bot_id": bot.id, "name": bot.name, "role": role}),
    )
    .await
}

/// 刪整個專案：專案本身是總管／角色的專案，或裡面有任何一顆 AGM 的 bot。
pub async fn guard_project_delete(pool: &SqlitePool, project_id: &str, confirm: Option<&str>) -> Result<(), crate::lifecycle::LcError> {
    let up = |e: anyhow::Error| crate::lifecycle::LcError::Upstream(format!("{e:#}"));
    let owned = load(pool).await.map_err(up)?;
    let bots = db::live_bots(pool).await.map_err(up)?;
    if !owned.project_ids.contains(project_id) && !bots.iter().any(|b| b.project_id == project_id && owned.owns(b)) {
        return Ok(());
    }
    refuse_unless_confirmed(pool, confirm, project_id, &format!("專案 `{project_id}`"), json!({"project_id": project_id})).await
}

async fn refuse_unless_confirmed(
    pool: &SqlitePool,
    confirm: Option<&str>,
    subject: &str,
    what: &str,
    extra: serde_json::Value,
) -> Result<(), crate::lifecycle::LcError> {
    let by = crate::config_audit::http_caller();
    if confirm == Some(CONFIRM) {
        tracing::warn!(what = %what, http = %by, "deleting an AGM bot/project with confirm=supervisor");
        return Ok(());
    }
    alert(pool, "supervisor_bot_delete_refused", subject, &format!("刪除 API 要刪 AGM 的 {what}，沒帶 confirm={CONFIRM}，已拒絕；呼叫端：{by}"))
        .await;
    let mut body = extra;
    body["message"] = json!(format!("{what} 屬於 AGM（總管／角色本身、它們的 child、或在總管專案裡）；確定要刪請帶 ?confirm={CONFIRM}"));
    Err(crate::lifecycle::LcError::conflict("supervisor_owned", body))
}

/// 刪除 API 放行 AGM bot 的 query 值。
pub const CONFIRM: &str = "supervisor";

#[cfg(test)]
mod tests {
    use crate::api::{delete_bot, delete_bot_http, delete_project_http, restore_bot, DeleteQuery};
    use crate::db;
    use crate::lifecycle::LcError;
    use axum::extract::{Path, Query, State};

    async fn a_user_bot(e: &crate::testing::Env, name: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok','user',?)",
        )
        .bind(&id)
        .bind(&e.project_id)
        .bind(name)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        let (pid, repo, bot_id, bot_name) = (e.project_id.clone(), e.repo.to_string_lossy().to_string(), id.clone(), name.to_string());
        e.app
            .cfg
            .update(move |cfg| {
                let bot: crate::config::BotCfg = toml::from_str(&format!("id = '{bot_id}'\nname = '{bot_name}'\nkind = 'claude'\n")).unwrap();
                match cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())) {
                    Some(p) => p.bots.push(bot),
                    None => cfg.projects.push(crate::config::ProjectCfg {
                        id: Some(pid),
                        path: repo,
                        label: "proj".into(),
                        host: crate::config::LOCAL_HOST.into(),
                        bots: vec![bot],
                    }),
                }
                Ok(())
            })
            .await
            .unwrap();
        id
    }

    fn in_config(cfg: &crate::config::ConfigFile, id: &str) -> bool {
        cfg.projects.iter().flat_map(|p| p.bots.iter()).any(|b| b.id.as_deref() == Some(id))
    }

    const CALLER: &str = "DELETE /api/bots/x peer=10.0.0.9:51234 ua=phone-safari origin=- referer=- bot=- caller_self_reported=-";

    /// 把 AGM 登記在這個專案上（`build`／`triage` 就是這樣掛著的）。
    async fn agm_owns_project(app: &std::sync::Arc<crate::state::App>, project_id: &str) {
        sqlx::query("INSERT INTO supervisors (id, bot_id, project_id, created_at, updated_at) VALUES ('AGM', 'agm-bot', ?, 't', 't')")
            .bind(project_id)
            .execute(&app.db)
            .await
            .unwrap();
    }

    async fn intents_for(app: &std::sync::Arc<crate::state::App>, subject: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM intents WHERE subject_id = ?").bind(subject).fetch_one(&app.db).await.unwrap()
    }

    async fn ops_alerts(app: &std::sync::Arc<crate::state::App>) -> Vec<String> {
        sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind = 'ops_alert' ORDER BY rowid")
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 13:28Z 那一刀：AGM 專案裡、沒有 parent 的 user bot，被 `DELETE /api/bots/{id}` 直接刪。沒帶 confirm 要 409＋ops_alert，什麼都不動；
    /// 帶了才刪，而且 intent 記得是誰刪的。
    #[tokio::test]
    async fn the_delete_api_refuses_an_agm_bot_unless_confirmed_and_records_who_asked() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let build = a_user_bot(&e, "build").await;
        sqlx::query("INSERT INTO supervisors (id, bot_id, project_id, created_at, updated_at) VALUES ('AGM', 'agm-bot', ?, 't', 't')")
            .bind(&e.project_id)
            .execute(&app.db)
            .await
            .unwrap();

        let refused = crate::config_audit::HTTP_CALLER
            .scope(CALLER.into(), delete_bot_http(State(app.clone()), Path(build.clone()), Query(DeleteQuery::default())))
            .await
            .unwrap_err();
        match refused {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "supervisor_owned", "{body}");
                assert_eq!(body["role"], "AGM 專案裡的常駐工人", "網頁的二次確認框要寫角色：{body}");
            }
            other => panic!("{other:?}"),
        }
        assert!(db::bot(&app.db, &build).await.unwrap().unwrap().deleted_at.is_none(), "沒刪");
        assert!(in_config(&app.cfg.get().await, &build), "config 沒動");
        let alerts: Vec<String> = sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind = 'ops_alert'")
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(alerts.len() == 1 && alerts[0].contains("10.0.0.9") && alerts[0].contains("build"), "{alerts:?}");

        let q = Query(DeleteQuery { confirm: Some(super::CONFIRM.into()) });
        crate::config_audit::HTTP_CALLER.scope(CALLER.into(), delete_bot_http(State(app.clone()), Path(build.clone()), q)).await.unwrap();
        assert!(db::bot(&app.db, &build).await.unwrap().unwrap().deleted_at.is_some());
        let payload: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE kind = 'delete_bot' AND subject_id = ?")
            .bind(&build)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(payload.contains("peer=10.0.0.9:51234") && payload.contains("phone-safari"), "{payload}");
    }

    /// 刪除把 `bots/<id>/` 搬進回收區，還原時搬回來（以前是當場 `remove_dir_all`，還原得回 bot 也拿不回目錄）。
    #[tokio::test]
    async fn a_deleted_bots_dir_is_recoverable_on_restore() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_user_bot(&e, "alfa").await;
        let dir = app.bot_dir(&id).unwrap();
        std::fs::create_dir_all(dir.join("spool")).unwrap();
        std::fs::write(dir.join("spool/pending.json"), "{\"hook\":1}").unwrap();

        delete_bot(State(app.clone()), Path(id.clone())).await.unwrap();
        assert!(!dir.exists(), "刪除後 bots/<id>/ 不在原地");
        assert!(std::fs::read_dir(crate::bot_trash::root(&app.data_dir)).unwrap().count() == 1, "在回收區");

        restore_bot(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("spool/pending.json")).unwrap(), "{\"hook\":1}", "還原把目錄搬回來");
    }

    /// review d77434c0 #1：`guard_project_delete` 與它在 `delete_project_http` 的接線本來沒有任何測試蓋到——
    /// 整條拿掉也不會紅。AGM 的專案 DELETE 要 409 `supervisor_owned`，而且**什麼都不動**：沒開 intent、沒有人被軟刪。
    #[tokio::test]
    async fn the_delete_api_refuses_the_agm_project_unless_confirmed() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let build = a_user_bot(&e, "build").await;
        agm_owns_project(&app, &e.project_id).await;

        let refused = crate::config_audit::HTTP_CALLER
            .scope(CALLER.into(), delete_project_http(State(app.clone()), Path(e.project_id.clone()), Query(DeleteQuery::default())))
            .await
            .unwrap_err();

        match refused {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "supervisor_owned", "{body}");
                assert_eq!(body["project_id"], e.project_id, "{body}");
            }
            other => panic!("{other:?}"),
        }
        assert!(db::project(&app.db, &e.project_id).await.unwrap().unwrap().deleted_at.is_none(), "專案沒刪");
        assert!(db::bot(&app.db, &build).await.unwrap().unwrap().deleted_at.is_none(), "裡面的 bot 沒刪");
        assert!(in_config(&app.cfg.get().await, &build), "config 沒動");
        assert_eq!(intents_for(&app, &e.project_id).await, 0, "定案之前就擋下來了，不該留 intent");
        let alerts = ops_alerts(&app).await;
        assert!(alerts.len() == 1 && alerts[0].contains(&e.project_id) && alerts[0].contains("10.0.0.9"), "{alerts:?}");

        // 明講了就刪得掉：閘門擋的是「沒說清楚」，不是「永遠不准」。
        let q = Query(DeleteQuery { confirm: Some(super::CONFIRM.into()) });
        crate::config_audit::HTTP_CALLER
            .scope(CALLER.into(), delete_project_http(State(app.clone()), Path(e.project_id.clone()), q))
            .await
            .unwrap();
        assert!(db::project(&app.db, &e.project_id).await.unwrap().unwrap().deleted_at.is_some());
    }

    /// 專案本身不是 AGM 的，但裡面住著一顆 AGM 的 child：一樣要擋（`owns` 的第二條）。
    #[tokio::test]
    async fn a_project_holding_an_agm_child_is_refused_too() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let kid = a_user_bot(&e, "agm-kid").await;
        sqlx::query("INSERT INTO supervisors (id, bot_id, project_id, created_at, updated_at) VALUES ('AGM', 'agm-bot', 'elsewhere', 't', 't')")
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET parent_bot_id = 'agm-bot' WHERE id = ?").bind(&kid).execute(&app.db).await.unwrap();

        let refused = crate::config_audit::HTTP_CALLER
            .scope(CALLER.into(), delete_project_http(State(app.clone()), Path(e.project_id.clone()), Query(DeleteQuery::default())))
            .await
            .unwrap_err();

        assert!(matches!(&refused, LcError::Conflict(b) if b["reason"] == "supervisor_owned"), "{refused:?}");
        assert_eq!(intents_for(&app, &e.project_id).await, 0);
    }

    /// 跟 AGM 無關的專案照樣刪得掉：閘門不能把所有刪除都擋住。
    #[tokio::test]
    async fn an_ordinary_project_is_not_affected_by_the_guard() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_user_bot(&e, "alfa").await;

        delete_project_http(State(app.clone()), Path(e.project_id.clone()), Query(DeleteQuery::default())).await.unwrap();

        assert!(db::bot(&app.db, &id).await.unwrap().unwrap().deleted_at.is_some());
        assert!(ops_alerts(&app).await.is_empty(), "沒擋任何東西就不該吵巡檢");
    }

    /// review d77434c0 #4：同一小時第二顆被擋的 bot 也要有 inbox（13:28Z 那次是 4 秒內兩顆）。
    /// dedupe 收斂的範圍是「同一顆連按」，不是「同一小時」。
    #[tokio::test]
    async fn each_refused_bot_gets_its_own_alert_within_the_same_hour() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let build = a_user_bot(&e, "build").await;
        let triage = a_user_bot(&e, "agm-pxf2pv-triage").await;
        agm_owns_project(&app, &e.project_id).await;

        for id in [&build, &triage, &build] {
            crate::config_audit::HTTP_CALLER
                .scope(CALLER.into(), delete_bot_http(State(app.clone()), Path(id.clone()), Query(DeleteQuery::default())))
                .await
                .unwrap_err();
        }

        let alerts = ops_alerts(&app).await;
        assert_eq!(alerts.len(), 2, "兩顆各一則、同一顆連按收斂成一則：{alerts:?}");
        assert!(alerts.iter().any(|a| a.contains(&build)), "{alerts:?}");
        assert!(alerts.iter().any(|a| a.contains(&triage)), "{alerts:?}");
    }
}
