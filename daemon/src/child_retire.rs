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
//! - 真的退役了就跟 `deleted_at` 同一個交易寫一筆 `intents`（`kind = retire_child`、`done`），帶原因、呼叫端、
//!   哪一顆 daemon、最後一個 run 怎麼結束與 herdr 對 pane 的回答（#554）。換版腳本靠它分辨「父 bot 收掉的」與
//!   「換版弄丟的」，判斷見 [`cause`]。

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
        let host = db::bot_host(&app.db, &bot.id).await?;
        let record = record_payload(app, &bot, why, mode, &at.to_string()).await?;
        // `deleted_at` 與紀錄同生共死（#554）：寫不進紀錄就不退役，不留「刪了、卻查不到為什麼」。
        let mut tx = app.db.begin().await?;
        let n = sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(db::now())
            .bind(&bot.id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Ok(Outcome::AlreadyGone);
        }
        crate::intents::record_done(&mut tx, RECORD_KIND, &bot.id, &host, &record).await?;
        tx.commit().await?;
        tracing::info!(bot = %bot.name, bot_id = %bot.id, why, cause = %record["cause"], caller = %at, http = %crate::config_audit::http_caller(), ?mode, "child retired");
        app.emit("project_changed", json!({"project_id": bot.project_id})).await;
        Ok(Outcome::Retired)
    }
}

/// 退役紀錄在 `intents` 裡的 `kind`（#554）。`scripts/ops/daemon-swap.sh` 照這個名字查。
pub(crate) const RECORD_KIND: &str = "retire_child";

/// 退役那一刻 herdr 對這顆 child 最後一個 run 的 pane 怎麼說（#554）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    /// herdr 明確回 `pane_not_found`。
    Gone,
    /// pane 還在（裡面認不出 agent，否則對帳不會走到退役）。
    Present,
    /// 問不到：沒有 run、沒有 pane id、沒有連線、RPC 失敗。
    Unknown,
}

impl Pane {
    fn as_str(self) -> &'static str {
        match self {
            Pane::Gone => "gone",
            Pane::Present => "present",
            Pane::Unknown => "unknown",
        }
    }
}

/// `runs.exit_reason` 裡**herdr 親口報的** pane 關閉：`events` 收到 `pane.exited`／`pane.closed`／`workspace.closed`
/// 時 `mark_run_exited` 寫的原因。對帳時才發現 pane 不見（`agent not found during reconcile`）、或定時問 herdr 得到
/// `pane_not_found`（`pane gone`）都不在這裡：那兩種只知道「現在不在」，不知道是誰、什麼時候關的。
const OBSERVED_CLOSE: &[&str] = &["pane exited", "workspace closed"];

/// 這次退役算不算**刻意**收掉（#554）。換版腳本只放過 `pane_closed` 與 `promoted`，其餘照樣回滾。
///
/// 為什麼要分：`reconcile_agent_gone`／`reconcile_run_already_ended` 同時可能是「父 bot `herdr pane close` 收掉 child」
/// 和「換版造成 child 不見」。`daemon-swap.sh` 只重啟 daemon、herdr 不動，所以換版本身關不掉任何 pane；
/// 換版能弄丟 child 的形狀是 **pane 還在、新 daemon 認不出裡面的 agent**（→ `agent_missing`），或新 daemon 連到
/// 不對的 herdr（一個空的 server 對每個 pane 都回 `pane_not_found`）。後者單看「pane 不在」分不出來，
/// 所以 `pane_closed` 要兩個條件都成立：
/// 1. run 是因為 herdr **發了事件**說 pane 關了才結束的（[`OBSERVED_CLOSE`]）——空的 herdr 不會替它沒有的 pane 發事件；
///    daemon 自己關的 pane 裡，對帳收的孤兒 pane 屬於早就結束的 run——事件到的時候 `mark_run_exited` 的 CAS 輸掉，
///    不寫原因；停止／刪除會讓事件寫下 `pane exited`，但那本來就是有人明講的動作。
/// 2. 退役當下 herdr 也說那個 pane 不在（[`Pane::Gone`]）。
///
/// 父 bot 在 daemon 停著的那幾秒關掉 pane：沒有人收到事件，只會是 `unconfirmed`——寧可多回滾一次，也不要放過弄丟的。
pub(crate) fn cause(why: &str, exit_reason: Option<&str>, pane: Pane) -> &'static str {
    match why {
        "promoted" | "promote_recovered" => "promoted",
        // herdr 計畫中重啟之後沒回來的：是 herdr 重啟造成的，不是有人收掉的。
        "herdr_maintenance_closed" => "herdr_restarted",
        _ => match pane {
            Pane::Gone if exit_reason.is_some_and(|r| OBSERVED_CLOSE.contains(&r)) => "pane_closed",
            Pane::Present => "agent_missing",
            _ => "unconfirmed",
        },
    }
}

/// 退役紀錄的 payload：原因、呼叫端、哪一顆 daemon（`boot_id`）、最後一個 run 怎麼結束、herdr 對 pane 的回答，與據此算出的 [`cause`]。
async fn record_payload(app: &Arc<App>, bot: &db::Bot, why: &str, mode: Mode, call_site: &str) -> anyhow::Result<serde_json::Value> {
    // ULID 同毫秒會翻（#100），世代序用 rowid。
    let run: Option<db::Run> =
        sqlx::query_as("SELECT * FROM runs WHERE bot_id = ? ORDER BY rowid DESC LIMIT 1").bind(&bot.id).fetch_optional(&app.db).await?;
    let exit_reason: Option<String> = match &run {
        Some(r) => sqlx::query_scalar("SELECT exit_reason FROM runs WHERE id = ?").bind(&r.id).fetch_one(&app.db).await?,
        None => None,
    };
    let pane = match &run {
        Some(r) => pane_state(app, r).await,
        None => Pane::Unknown,
    };
    Ok(json!({
        "why": why,
        "cause": cause(why, exit_reason.as_deref(), pane),
        "mode": if mode == Mode::Implicit { "implicit" } else { "explicit" },
        "call_site": call_site,
        "requested_by": crate::config_audit::http_caller(),
        "boot_id": app.boot_id,
        "name": bot.name,
        "project_id": bot.project_id,
        "parent_bot_id": bot.parent_bot_id,
        "pane": pane.as_str(),
        "last_run": run.as_ref().map(|r| json!({
            "id": r.id, "state": r.state, "pane_id": r.pane_id, "ended_at": r.ended_at, "exit_reason": exit_reason,
        })),
    }))
}

async fn pane_state(app: &Arc<App>, run: &db::Run) -> Pane {
    let Some(pane) = run.pane_id.as_deref() else { return Pane::Unknown };
    let Some(client) = app.herdr_for_run(run).await else { return Pane::Unknown };
    match client.pane_get(pane).await {
        Ok(None) => Pane::Gone,
        Ok(Some(_)) => Pane::Present,
        Err(_) => Pane::Unknown,
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
    // 同一顆、一小時最多一則（跟 `supervisor_owned::alert` 同一個慣例）：擋下來本身已經做完了。
    let key = notice_key(app, &bot.id, chrono::Utc::now()).await;
    match crate::supervisor::store::push_inbox(&app.db, &key, "child_retire_refused", None, Some(&bot.id), None, &payload).await {
        Ok(_) => tracing::warn!(bot = %bot.name, role, why, "refused to retire an AGM child; patrol notified"),
        Err(e) => tracing::error!(bot = %bot.name, role, why, error = %e, "refused to retire an AGM child; the inbox row could not be written"),
    }
    Outcome::Refused
}

/// 「同一顆 child 一小時最多一則退役拒絕」的視窗長度。
const NOTICE_WINDOW_SECS: i64 = 3600;

/// 這一則退役拒絕要用哪一把去重鍵。
///
/// 先找「同一顆 child、`created_at` 距現在**不超過** [`NOTICE_WINDOW_SECS`]」的最新一筆通知：有就
/// **沿用它那把鍵**，`INSERT OR IGNORE` 於是原地吞掉——那一小時因此是從上一則通知起算的。
///
/// 以前這裡是 `db::now()[..13]`，牆上時鐘切出來的固定小時格。那樣決定要不要去重的其實是「兩次對帳
/// 各自落在哪一格」，不是「它們相隔多久」：對帳每一拍都跑，所以每個整點都會有相隔幾秒的兩次對帳落在
/// 不同格子，巡檢收到兩則講同一件事的通知（issue #536，跟 #442 的十分鐘分格同一類）。
///
/// **不挑 `state`**（跟 #442 的 `window_anchor` 差在這裡，那邊只認 `pending`／`delivered`）：那邊的
/// 來源是 bot 自己送的申請，已結案的不該把重問吸進去；這裡的來源是每拍都跑的對帳，而且被擋下來的情況
/// 會一直成立。只認「還沒結案」的話，巡檢一裁示完、下一拍就再推一則，去重就變成刷屏。所以維持原本的
/// 意思「一小時最多一則」，只把那一小時改成從上一則算起。
///
/// 讀不到就開新的一把：這只是去重，寧可讓巡檢多看到一則，也不要讓通知整筆消失。
async fn notice_key(app: &Arc<App>, bot_id: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let since = db::iso_at(now - chrono::Duration::seconds(NOTICE_WINDOW_SECS));
    // `supervisor_inbox.created_at` 只由 `db::now()` 寫（同一種毫秒格式），所以字串比較就是照時刻比。
    let found: Result<Option<String>, _> = sqlx::query_scalar(
        "SELECT event_key FROM supervisor_inbox
          WHERE supervisor_id=? AND kind='child_retire_refused' AND bot_id=? AND created_at >= ?
          ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(crate::supervisor::store::SUPERVISOR_ID)
    .bind(bot_id)
    .bind(&since)
    .fetch_optional(&app.db)
    .await;
    match found {
        Ok(Some(key)) => key,
        Ok(None) => fresh_notice_key(bot_id, now),
        Err(e) => {
            tracing::warn!(bot_id, error = %e, "cannot look for an earlier retire refusal; opening a new dedupe window");
            fresh_notice_key(bot_id, now)
        }
    }
}

/// 視窗裡沒有前一則時開的新鍵。
///
/// 尾巴仍是小時格（不是 `now` 的秒數）：同一瞬間有兩條路同時被擋下來（對帳與維護窗口收尾）時，兩邊
/// 都找不到前一則，靠這一格還是會撞成同一把鍵。**這不是回到舊行為**——只有「視窗裡一則都沒有」才會
/// 走到這裡，而那代表上一則至少是一小時前，它的格號一定比現在的小，不會誤撞。
fn fresh_notice_key(bot_id: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    format!("child_retire_refused:{bot_id}:{}", now.timestamp().div_euclid(NOTICE_WINDOW_SECS))
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

    /// #554：退役跟 `deleted_at` 同一個交易寫一筆 `retire_child` intent（`done`），換版腳本才查得到；
    /// 已經退役過的不再寫第二筆。
    #[tokio::test]
    async fn a_retirement_leaves_one_retire_child_record() {
        let env = tt::env().await;
        let app = env.app.clone();
        let kid = a_child(&env).await;

        let here = line!() + 1;
        assert_eq!(retire(&app, &kid, "promoted", Mode::Explicit).await.unwrap(), Outcome::Retired);
        assert_eq!(retire(&app, &kid, "promoted", Mode::Explicit).await.unwrap(), Outcome::AlreadyGone);

        let rows: Vec<(String, String, String, String)> =
            sqlx::query_as("SELECT status, host, payload_json, created_at FROM intents WHERE kind = 'retire_child' AND subject_id = ?")
                .bind(&kid)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (status, host, payload, created_at) = &rows[0];
        assert_eq!(status, "done");
        assert_eq!(host, crate::config::LOCAL_HOST);
        let deleted_at = db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.unwrap();
        assert!(created_at >= &deleted_at, "紀錄時間不早於 deleted_at：{created_at} vs {deleted_at}");
        let p: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(p["why"], "promoted");
        assert_eq!(p["cause"], "promoted", "promote 是明講的動作：{p}");
        assert_eq!(p["mode"], "explicit");
        assert_eq!(p["requested_by"], "-");
        assert!(p["call_site"].as_str().unwrap().contains(&format!("{}:{here}:", file!())), "{p}");
    }

    /// #554：只有 herdr 親口報的關閉、而且當下 pane 確實不在，才算刻意收掉；其餘一律不算（換版腳本會回滾）。
    #[test]
    fn only_an_observed_close_of_a_pane_that_is_gone_counts_as_deliberate() {
        let why = "reconcile_run_already_ended";
        assert_eq!(cause(why, Some("pane exited"), Pane::Gone), "pane_closed");
        assert_eq!(cause(why, Some("workspace closed"), Pane::Gone), "pane_closed");
        assert_eq!(cause("reconcile_agent_gone", Some("pane exited"), Pane::Gone), "pane_closed", "CAS 輸給事件那條路：原因是事件寫的");
        // 事件說關了，但 herdr 現在說 pane 還在（id 被重用、連錯 server）：不信。
        assert_eq!(cause(why, Some("pane exited"), Pane::Present), "agent_missing");
        assert_eq!(cause(why, Some("pane exited"), Pane::Unknown), "unconfirmed");
        // 只知道「現在不在」：空的 herdr 也會這樣回答。
        assert_eq!(cause(why, Some("pane gone"), Pane::Gone), "unconfirmed");
        assert_eq!(cause("reconcile_agent_gone", Some("agent not found during reconcile"), Pane::Gone), "unconfirmed");
        assert_eq!(cause(why, None, Pane::Gone), "unconfirmed", "升級前結束的 run 沒有原因");
        assert_eq!(cause("reconcile_agent_gone", Some("agent not found during reconcile"), Pane::Present), "agent_missing");
        assert_eq!(cause("promoted", None, Pane::Unknown), "promoted");
        assert_eq!(cause("promote_recovered", None, Pane::Present), "promoted");
        assert_eq!(cause("herdr_maintenance_closed", Some("pane exited"), Pane::Gone), "herdr_restarted");
    }

    /// #554：`runs.exit_reason` 只記把 run 收成 exited 的那一次；之後別的路徑再收（CAS 輸掉）不改寫。
    #[tokio::test]
    async fn the_exit_reason_is_the_one_that_ended_the_run() {
        let env = tt::env().await;
        let app = env.app.clone();
        let kid = a_child(&env).await;
        let run = db::ulid();
        sqlx::query("INSERT INTO runs (id, bot_id, state, pane_id, started_at) VALUES (?,?,'running','w9:p9',?)")
            .bind(&run)
            .bind(&kid)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        crate::lifecycle::mark_run_exited(&app, &run, "pane exited").await;
        crate::lifecycle::mark_run_exited(&app, &run, "agent not found during reconcile").await;

        let reason: Option<String> = sqlx::query_scalar("SELECT exit_reason FROM runs WHERE id = ?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(reason.as_deref(), Some("pane exited"));
    }

    /// #554：紀錄寫不進去就整筆不退役（跟 `deleted_at` 同生共死），不留「刪了、卻沒有紀錄」——那正是換版會誤判回滾的形狀。
    #[tokio::test]
    async fn an_unwritable_record_keeps_the_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let kid = a_child(&env).await;
        sqlx::query("CREATE TRIGGER am_test_no_intents BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        assert!(retire(&app, &kid, "promoted", Mode::Explicit).await.is_err());
        assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_none(), "deleted_at 一起回滾");
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

    fn at(iso: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&chrono::Utc)
    }

    /// 種一則已經在收件匣裡的退役拒絕，回傳它的 `event_key`。
    async fn earlier_notice(env: &tt::Env, bot_id: &str, created_at: chrono::DateTime<chrono::Utc>) -> String {
        let key = format!("child_retire_refused:{bot_id}:seeded-{}", created_at.timestamp());
        sqlx::query(
            "INSERT INTO supervisor_inbox (id, supervisor_id, event_key, bot_id, kind, payload_json, state, created_at, updated_at)
             VALUES (?, 'AGM', ?, ?, 'child_retire_refused', '{}', 'pending', ?, ?)",
        )
        .bind(db::ulid())
        .bind(&key)
        .bind(bot_id)
        .bind(db::iso_at(created_at))
        .bind(db::iso_at(created_at))
        .execute(&env.app.db)
        .await
        .unwrap();
        key
    }

    /// #536：去重視窗錨在**上一則通知**，不是牆上時鐘切出來的小時格。相隔 40 秒的兩次對帳跨過整點時，
    /// 以前落在不同格子、推兩則講同一件事的通知給巡檢；現在沿用上一則那把鍵，`INSERT OR IGNORE` 吞掉。
    #[tokio::test]
    async fn two_refusals_a_minute_apart_share_one_notice_across_the_hour_boundary() {
        let env = tt::env().await;
        let kid = a_child(&env).await;
        let (before, after) = (at("2026-09-24T12:59:30Z"), at("2026-09-24T13:00:10Z"));
        assert_ne!(
            fresh_notice_key(&kid, before),
            fresh_notice_key(&kid, after),
            "這兩個時刻本來就落在不同的小時格：不然這條測試什麼都沒測到"
        );

        let seeded = earlier_notice(&env, &kid, before).await;

        assert_eq!(notice_key(&env.app, &kid, after).await, seeded, "跨過整點也沿用上一則那把鍵");
    }

    /// 視窗本身照樣有效：上一則剛好滿一小時（SQL 是 `>=`）還算同一則；更早的就開新的一把，巡檢才不會
    /// 在情況一直成立時永遠只看到最初那一則。
    #[tokio::test]
    async fn a_notice_older_than_the_window_opens_a_new_one() {
        let env = tt::env().await;
        let kid = a_child(&env).await;
        let now = at("2026-09-24T13:00:00Z");

        let edge = earlier_notice(&env, &kid, at("2026-09-24T12:00:00Z")).await;
        assert_eq!(notice_key(&env.app, &kid, now).await, edge, "剛好一小時前的還算同一則");

        sqlx::query("UPDATE supervisor_inbox SET created_at = ? WHERE event_key = ?")
            .bind(db::iso_at(at("2026-09-24T11:59:59Z")))
            .bind(&edge)
            .execute(&env.app.db)
            .await
            .unwrap();

        let key = notice_key(&env.app, &kid, now).await;
        assert_ne!(key, edge, "超過一小時：開新的一把鍵");
        assert_eq!(key, fresh_notice_key(&kid, now), "新的那把鍵就是現在這一格");
    }

    /// 別顆 child 的通知不是這顆的錨：兩顆同時被擋下來時，第二顆不能被第一顆那把鍵吞掉。
    #[tokio::test]
    async fn another_childs_notice_is_not_this_ones_anchor() {
        let env = tt::env().await;
        let (mine, theirs) = (a_child(&env).await, a_child(&env).await);
        let now = at("2026-09-24T13:00:10Z");
        let seeded = earlier_notice(&env, &theirs, at("2026-09-24T12:59:30Z")).await;

        let key = notice_key(&env.app, &mine, now).await;

        assert_ne!(key, seeded);
        assert_eq!(key, fresh_notice_key(&mine, now));
    }

    /// 讀不到收件匣時開新的一把鍵、只留一行 warn：擋下來已經發生了，寧可讓巡檢多看到一則，也不要
    /// 因為去重查詢失敗就把通知整筆弄不見。
    #[tokio::test]
    async fn an_unreadable_inbox_opens_a_new_window_instead_of_losing_the_notice() {
        let env = tt::env().await;
        let kid = a_child(&env).await;
        let now = at("2026-09-24T13:00:10Z");
        earlier_notice(&env, &kid, at("2026-09-24T12:59:30Z")).await;
        sqlx::query("ALTER TABLE supervisor_inbox RENAME TO supervisor_inbox_unreadable")
            .execute(&env.app.db)
            .await
            .unwrap();
        let (buf, _guard) = crate::config_audit::capture::start();

        let key = notice_key(&env.app, &kid, now).await;

        assert_eq!(key, fresh_notice_key(&kid, now));
        let log = buf.text();
        assert!(log.contains("cannot look for an earlier retire refusal"), "留得下線索：{log}");
    }
}
