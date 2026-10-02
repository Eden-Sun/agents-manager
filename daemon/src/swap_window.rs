//! 例行自動部署換版窗口的核准（`POST /api/services/daemon-swap/restart-window`，SPEC §18.10）。
//!
//! 窗口要等「沒人在 working」，等滿 [`crate::supervisor::maintenance::escalate_after_secs`]（30 分）之後 working
//! 才不再擋——但計時綁在**同一張核准**上。以前每一輪 daemon-swap 都開一張新的、拿不到窗口就撤掉，計時每輪歸零：
//! 2026-10-01 一直有 bot 在忙，自動部署從 15:18 卡到隔天還沒換成版（每則 DEFER 的 `escalates_at` 都是當下 +30 分）。
//!
//! 現在：同一個 owner 還活著的那張自動核准，commit 一樣就**沿用**；main 動了換 commit 就開新的並 `supersedes` 舊的
//! （`wait_since` 接過來，計時不歸零）。拿不到窗口只因為還有人在忙（`not_idle`）時**不撤**，下一輪帶同一張再試；
//! 其他原因（別人握著窗口、送達臨界區……）照舊撤掉，不留 approved 的殘單。核准本身活 [`APPROVAL_TTL_SECS`]，
//! 要比升級門檻長，否則等不到放寬就先過期。

use std::sync::Arc;

use crate::lifecycle::LcError;
use crate::state::App;
use crate::supervisor::store;

/// 自動核准活多久：遠長於 30 分的升級門檻，讓一直在忙的機群也等得到放寬。
pub const APPROVAL_TTL_SECS: i64 = 6 * 3600;

/// 這個 owner 現在可以拿去開窗口的核准 id：沿用同 commit 的活核准，或開一張新的（接續舊的等待）。
pub async fn approval_for(app: &Arc<App>, owner: &str, commit: &str, actor: &str) -> anyhow::Result<String> {
    let now = crate::db::now();
    let expires_at = crate::db::ts_sql("expires_at");
    let live: Option<store::Approval> = sqlx::query_as(&format!(
        "SELECT * FROM supervisor_approvals
          WHERE supervisor_id=? AND requester=? AND purpose='restart' AND status='approved'
            AND (expires_at IS NULL OR {expires_at} > ?)
          ORDER BY rowid DESC LIMIT 1"
    ))
    .bind(store::SUPERVISOR_ID)
    .bind(owner)
    .bind(&now)
    .fetch_optional(&app.db)
    .await?;
    if let Some(a) = live.as_ref().filter(|a| a.target_commit.as_deref() == Some(commit)) {
        return Ok(a.id.clone());
    }
    let created = store::create_approval_superseding(
        &app.db,
        owner,
        "restart",
        &format!("例行自動部署換版 {commit}（daemon-swap 自動核准）"),
        Some(commit),
        Some(&crate::db::iso_in(APPROVAL_TTL_SECS)),
        Some(&format!("auto-deploy-restart-{}", crate::db::ulid())),
        live.as_ref().map(|a| a.id.as_str()),
        None,
    )
    .await?;
    let id = created.approval.id;
    store::decide_approval_from(&app.db, &id, "pending", "approved", actor, Some("例行自動部署：建置與整樹測試在推 main 前後已由 ubuntu-ci 驗過"), None).await?;
    Ok(id)
}

/// 拿不到窗口時這張核准要不要留著：只因為還有人在忙（`not_idle`）就留，計時才接得下去。
pub fn keep_after(err: &LcError) -> bool {
    match err {
        LcError::Conflict(v) => v.get("reason").and_then(|r| r.as_str()) == Some("not_idle")
            || v.get("detail").and_then(|d| d.get("reason")).and_then(|r| r.as_str()) == Some("not_idle"),
        _ => false,
    }
}
