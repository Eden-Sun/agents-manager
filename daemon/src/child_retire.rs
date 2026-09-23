//! 子 agent 退役（寫 `bots.deleted_at`）的唯一入口（issue #413）。
//!
//! #406（d77434c0）替 `DELETE /api/bots|projects` 記了呼叫端、擋了 AGM 的 bot。可是子 agent 還有四條路直接
//! `UPDATE bots SET deleted_at`：reconcile 的兩處（agent 不見、run 早就結束）、維護窗口收尾、promote（含開機補完）。
//! 這四條沒有守衛也沒有紀錄——AGM 專案底下的 build／triage 開的子 agent 消失時，連 log 都查不到是誰收的。
//!
//! 所以都走這裡：
//! - 每一次退役都記一行 `child retired`，帶程式裡的呼叫位置（`#[track_caller]`）、原因、HTTP 呼叫端（背景巡邏是 `-`）。
//! - **隱式**退役（[`Mode::Implicit`]）先問同一份 `supervisor_owned`：是 AGM 的就不刪，改推一則
//!   `child_retire_refused` 給巡檢（帶 bot、原因、最後一個 run 的 pane 狀態），由人決定。讀不到擁有關係也不刪。
//! - promote 是有人明講要做的事（[`Mode::Explicit`]）：不擋，只記。

use crate::{db, state::App};
use serde_json::json;
use std::future::Future;
use std::panic::Location;
use std::sync::Arc;

/// 誰要退役這顆 child。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// 對帳／維護收尾自己判斷的：AGM 的 child 要擋。
    Implicit,
    /// 使用者或 AGM 明講的（promote）：照做，只記呼叫端。
    Explicit,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// 寫進去了。
    Retired,
    /// 早就不在（或已經退役）：什麼都沒寫。
    AlreadyGone,
    /// AGM 角色 bot、它們的 child、或 AGM 專案底下的 bot：不軟刪，已推 `child_retire_refused` 給巡檢。
    Refused,
    /// 讀不到誰是 AGM 的：這一輪不退役，晚一點再看。
    Unreadable,
}

/// 退役 `bot_id` 這顆 child。DB 寫不進去回 `Err`（呼叫端各自決定重試或回滾）；守衛擋下、讀不到擁有關係不是錯誤。
#[track_caller]
pub(crate) fn retire<'a>(app: &'a Arc<App>, bot_id: &'a str, why: &'static str, mode: Mode) -> impl Future<Output = anyhow::Result<Outcome>> + 'a {
    let at = Location::caller();
    async move {
        let Some(bot) = db::bot(&app.db, bot_id).await? else { return Ok(Outcome::AlreadyGone) };
        if bot.deleted_at.is_some() {
            return Ok(Outcome::AlreadyGone);
        }
        if mode == Mode::Implicit {
            match agm_guard(app, &bot, why).await {
                Outcome::Retired => {}
                other => return Ok(other),
            }
        }
        let n = sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(db::now())
            .bind(&bot.id)
            .execute(&app.db)
            .await?
            .rows_affected();
        if n == 0 {
            return Ok(Outcome::AlreadyGone);
        }
        tracing::info!(bot = %bot.name, bot_id = %bot.id, why, caller = %at, http = %crate::config_audit::http_caller(), ?mode, "child retired");
        app.emit("project_changed", json!({"project_id": bot.project_id})).await;
        Ok(Outcome::Retired)
    }
}

/// 隱式退役前的 AGM 判斷。`Retired`＝放行（不是 AGM 的）。
async fn agm_guard(app: &Arc<App>, bot: &db::Bot, why: &str) -> Outcome {
    let owned = match crate::supervisor_owned::load(&app.db).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(bot = %bot.name, why, error = ?e, "cannot read which bots are AGM's; child kept this pass");
            return Outcome::Unreadable;
        }
    };
    if !owned.owns(bot) {
        return Outcome::Retired;
    }
    let role = owned.role(bot);
    // 最後一個 run 的樣子給巡檢當線索。ULID 同毫秒會翻（#100），世代序用 rowid。讀不到就不寫這一段。
    let last: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT state, agent_status, pane_id FROM runs WHERE bot_id = ? ORDER BY rowid DESC LIMIT 1")
            .bind(&bot.id)
            .fetch_optional(&app.db)
            .await
            .unwrap_or(None);
    let payload = json!({
        "bot_id": bot.id,
        "name": bot.name,
        "project_id": bot.project_id,
        "parent_bot_id": bot.parent_bot_id,
        "role": role,
        "why": why,
        "last_run": last.as_ref().map(|(state, status, pane)| json!({"state": state, "agent_status": status, "pane_id": pane})),
        "action": format!("daemon 沒有刪它：這顆是 AGM 的（{role}），隱式退役要人決定。確定不要了就 `agm bot delete {} --confirm-supervisor`；還要用就查它的 pane 為什麼不見了", bot.id),
    });
    // 同一顆、同一個小時只推一次（跟 `supervisor_owned::alert` 同一個慣例）：擋下來本身已經做完了。
    let hour = db::now().get(..13).unwrap_or_default().to_string();
    let key = format!("child_retire_refused:{}:{hour}", bot.id);
    match crate::supervisor::store::push_inbox(&app.db, &key, "child_retire_refused", None, Some(&bot.id), None, &payload).await {
        Ok(_) => tracing::warn!(bot = %bot.name, role, why, "refused to retire an AGM child; patrol notified"),
        Err(e) => tracing::error!(bot = %bot.name, role, why, error = %e, "refused to retire an AGM child; the inbox row could not be written"),
    }
    Outcome::Refused
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    async fn a_child(env: &tt::Env) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok','child',?)",
        )
        .bind(&id)
        .bind(&env.project_id)
        .bind(format!("kid-{id}"))
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        id
    }

    /// 每一次退役都留得下「是誰、為什麼」：呼叫位置指到呼叫端那一行，HTTP 呼叫端在背景是 `-`。
    #[tokio::test]
    async fn every_retirement_logs_the_call_site_and_the_reason() {
        let env = tt::env().await;
        let app = env.app.clone();
        let kid = a_child(&env).await;
        let (buf, _guard) = crate::config_audit::capture::start();

        let here = line!() + 1;
        let out = retire(&app, &kid, "test_reason", Mode::Explicit).await.unwrap();

        assert_eq!(out, Outcome::Retired);
        assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_some());
        let log = buf.text();
        let line = log.lines().find(|l| l.contains("child retired")).unwrap_or_else(|| panic!("no retire line: {log}"));
        assert!(line.contains("why=\"test_reason\""), "{line}");
        assert!(line.contains(&format!("caller={}:{here}:", file!())), "呼叫位置是呼叫端那一行：{line}");
        assert!(line.contains("http=-"), "{line}");
        assert_eq!(retire(&app, &kid, "again", Mode::Explicit).await.unwrap(), Outcome::AlreadyGone, "已退役的不再寫一次");
    }
}
