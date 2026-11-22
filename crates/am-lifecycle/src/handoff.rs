//! 專案移交（#708，SPEC §6.5h）：`projects.handed_off_to` 有值＝這個專案已經交給另一台主機的 daemon 管。
//!
//! 這顆 daemon 對它的 bot／pane 一律不動：不對帳（不建／結束 run、不收編、不退役 child）、不當子 agent 認領的
//! 父候選、不關 pane／tab、不 GC、不送 prompt／佇列、hook 進來記一行就丟、不開不停不重啟，也不打任何 pane RPC
//! （連讀畫面都不讀）。設定當下不停也不關任何東西：agent 繼續跑，交給對方接；清掉旗標＝收回，下一輪對帳照常接手。
//!
//! 守衛放在收口的地方，一處擋一整類：`mark_run_exited`（結束 run 的唯一寫入）、`child_retire::retire`（軟刪 child 的
//! 唯一寫入）、`client_for_run`／`App::herdr_for_run`（pane RPC）、start／stop／restart／prompt 的入口、
//! `flush_queued_locked`、`hookrecv::process_locked`、對帳的逐 bot 迴圈與子 agent 認領、`panes::scan_snapshot`。

use crate::lc_error::{LcError, LcResult};
use anyhow::Result;
use serde_json::json;
use sqlx::{SqliteConnection, SqlitePool};
use std::collections::HashSet;

/// 這顆 bot 所屬專案移交給誰；`None`＝這顆 daemon 管（bot 不存在也是 `None`，由呼叫端自己的 not-found 處理）。
pub async fn bot_handed_off_to(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT p.handed_off_to FROM bots b JOIN projects p ON p.id = b.project_id WHERE b.id = ?",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?
    .flatten())
}

/// 這個專案歸誰管；`None`＝這顆 daemon 管（專案不存在也是 `None`，由呼叫端自己處理）。
pub async fn project_handed_off_to(pool: &SqlitePool, project_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>("SELECT handed_off_to FROM projects WHERE id = ?")
        .bind(project_id)
        .fetch_optional(pool)
        .await?
        .flatten())
}

/// Transaction-scoped variant for callers that must keep the handoff check atomic with a write.
pub async fn bot_handed_off_to_on(conn: &mut SqliteConnection, bot_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT p.handed_off_to FROM bots b JOIN projects p ON p.id = b.project_id WHERE b.id = ?",
    )
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?
    .flatten())
}

/// 409 `handed_off`：這個專案歸另一台主機的 daemon 管。讀不到就不做（502），不當成「沒移交」。
pub async fn refuse(pool: &SqlitePool, bot_id: &str) -> LcResult<()> {
    match bot_handed_off_to(pool, bot_id).await {
        Ok(None) => Ok(()),
        Ok(Some(host)) => Err(LcError::conflict(
            "handed_off",
            json!({"bot_id": bot_id, "handed_off_to": host,
                   "message": format!("這個專案已移交給 {host} 的 daemon 管理，這裡不替它開、關、送 prompt 或操作 pane；要收回請先清掉專案的「已移交」。")}),
        )),
        Err(e) => Err(LcError::Upstream(format!("cannot tell whether bot `{bot_id}` was handed off: {e}"))),
    }
}

/// 一台主機上所有已移交專案看得見的足跡：對帳認領 child 與 pane 掃描拿它排除別人的東西。
#[derive(Debug, Default)]
pub struct Footprint {
    pub workspaces: HashSet<String>,
    pub tabs: HashSet<String>,
    pub panes: HashSet<String>,
}

impl Footprint {
    pub fn covers_agent(&self, a: &crate::herdr::AgentInfo) -> bool {
        self.panes.contains(&a.pane_id)
            || self.tabs.contains(&a.tab_id)
            || self.workspaces.contains(&a.workspace_id)
    }

    /// `session.snapshot` 的一顆 pane。
    pub fn covers_pane(&self, p: &serde_json::Value) -> bool {
        let field = |k: &str| p.get(k).and_then(serde_json::Value::as_str);
        field("pane_id").is_some_and(|v| self.panes.contains(v))
            || field("tab_id").is_some_and(|v| self.tabs.contains(v))
            || field("workspace_id").is_some_and(|v| self.workspaces.contains(v))
    }
}

pub async fn footprint(pool: &SqlitePool, host: &str, session: &str, host_session: &str) -> Result<Footprint> {
    let mut fp = Footprint::default();
    let projects = crate::db::live_projects(pool).await?;
    for p in projects.iter().filter(|p| p.host == host && p.handed_off_to.is_some()) {
        type RunRow = (Option<String>, Option<String>, Option<String>, bool);
        let runs: Vec<RunRow> = sqlx::query_as(
            "SELECT r.pane_id, r.tab_id, r.workspace_id, r.state IN ('starting','running','stopping')
               FROM runs r JOIN bots b ON b.id = r.bot_id WHERE b.project_id = ? AND b.deleted_at IS NULL AND COALESCE(r.herdr_session, b.herdr_session, ?) = ?",
        )
        .bind(&p.id)
        .bind(host_session)
        .bind(session)
        .fetch_all(pool)
        .await?;
        for (pane, tab, ws, active) in runs {
            if active {
                fp.panes.extend(pane.filter(|v| !v.is_empty()));
                fp.tabs.extend(tab.filter(|v| !v.is_empty()));
                fp.workspaces.extend(ws.filter(|v| !v.is_empty()));
            }
        }
    }
    Ok(fp)
}

