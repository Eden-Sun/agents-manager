//! 角色 bot（巡檢／協調者）「現在能不能用」的畫面與 notify 證據（issue #427，#420 的後續）。
//!
//! 為什麼另外一個模組：[`crate::supervisor::health::role_state`] 只讀 DB——`/api/supervisor/health`
//! 與 `/api/supervisor/responder` 都會被 UI 高頻輪詢，每次都去抓一次 pane 會把 herdr 打爆。
//! 這裡是**每 30 秒一次**的那一路：health tick 在 `incidents::sweep` 之前呼叫 [`refresh`]，
//! 把結論放記憶體，`role_state` 只讀記憶體。
//!
//! 兩個缺口就是這張票要補的（#427 第 2 項）：**巡檢從來沒有被看過**（它是 incident 與 ops_alert
//! 的收件人，停在登入失效一樣沒有人知道），而協調者只有在「有事要送、而且送不出去」時才會被看
//! （`responder::notify` 的錯誤路徑，issue #420）——佇列空著時它壞了也要等到下一次有事才發現。
//!
//! 結論**不進 DB**：跟 incident 的門檻計時同一個原則（SPEC §18.9「計時在記憶體，重啟重算」）。
//! 寫進 DB 的故障會跟著重啟活過來，而重啟後第一拍就能重新判定——寧可晚 30 秒開，也不要讓一顆
//! 已經修好的協調者因為陳舊的旗標被繼續當成壞的。同理，**空表不是故障**：daemon 剛起來、第一拍
//! 還沒跑時 [`reason`] 回 `None`，`role_state` 就退回原本只看 DB 的行為（#421 的不變量）。

use crate::state::App;
use crate::supervisor::roles::Role;
use std::sync::Arc;

/// 停在登入失效。契約字串的正本在 [`crate::supervisor::health`]（#421 原樣寫進 inbox payload），
/// 這裡不另外定義一份——同一個字串兩個來源，哪天漂掉沒有人會發現。
pub use crate::supervisor::health::REASON_NEEDS_LOGIN;

/// 兩個角色都要看。巡檢沒有被看是 #427 第 2 項明講的缺口之一。
pub const WATCHED: [Role; 2] = [Role::Responder, Role::Patrol];

/// 一顆角色 bot 這一拍看到的故障。存在 [`crate::state::App::role_faults`]（記憶體，重啟重算）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleFault {
    /// `None` = 這一拍沒看到問題。目前唯一的值是 [`REASON_NEEDS_LOGIN`]。
    pub reason: Option<&'static str>,
    /// 這一拍連畫面都讀不到（沒有 run、herdr 沒回、pane 不見了）。**不是故障**——
    /// 沒有證據不能當成「它壞了」（#421 的不變量），incident 那邊用它決定不開也不關。
    pub probe_failed: bool,
    /// 第一次看到這個 `reason` 的時間，給 incident 的 detail 用。
    pub since: Option<String>,
}

/// 每一拍看一次兩顆角色 bot 的畫面，把結論寫進記憶體。
///
/// 由 health 的 30 秒 tick 在 `incidents::sweep` **之前**呼叫：incident 與同一拍的 health 讀數要講
/// 同一件事（沿用那個 tick 原本的註解「a fault and the health reading that mentions it never
/// disagree by one tick」）。
pub async fn refresh(app: &Arc<App>) {
    for role in WATCHED {
        // 沒建立的角色整個不留紀錄：留著會讓 `incidents::observe` 每一拍都把這個 kind 算成
        // 「探針沒跑」，另一顆角色已經開著的 incident 就再也關不掉（blind 會擋住 resolve）。
        if !is_configured(app, role).await {
            app.role_faults.lock().await.remove(role.as_str());
            continue;
        }
        let probe = probe_role(app, role).await;
        let mut faults = app.role_faults.lock().await;
        let entry = faults.entry(role.as_str().to_string()).or_default();
        entry.probe_failed = probe.is_none();
        let reason = match probe {
            Some(true) => Some(REASON_NEEDS_LOGIN),
            Some(false) => None,
            // 讀不到畫面：維持上一拍的結論，不憑空宣告故障、也不宣告恢復。
            None => entry.reason,
        };
        if entry.reason != reason {
            entry.since = reason.map(|_| crate::db::now());
            match reason {
                Some(r) => tracing::warn!(role = role.as_str(), reason = r, "角色 bot 不可用"),
                None => tracing::info!(role = role.as_str(), "角色 bot 恢復"),
            }
        }
        entry.reason = reason;
    }
}

/// 這一拍的結論：不可用的原因，沒看到問題（或表還沒被填過）就是 `None`。
pub async fn reason(app: &Arc<App>, role: Role) -> Option<&'static str> {
    app.role_faults.lock().await.get(role.as_str()).and_then(|f| f.reason)
}

/// incident 那邊要的整份（`probe_failed` 與 `since` 都要，不只 `reason`）。
pub async fn snapshot(app: &Arc<App>, role: Role) -> Option<RoleFault> {
    app.role_faults.lock().await.get(role.as_str()).cloned()
}

/// 這個角色建立過沒有（有登記的 bot_id）。讀不到就當成「有」——當成沒有會把已經開著的 incident
/// 誤關掉，而 `role_state` 那邊讀不到 DB 本來就會回 `Unknown`。
async fn is_configured(app: &Arc<App>, role: Role) -> bool {
    match crate::supervisor::roles::get(&app.db, role).await {
        Ok(row) => row.bot_id.is_some(),
        Err(_) => true,
    }
}

/// 這顆角色 bot 是不是停在登入失效；讀不到回 `None`（＝這一拍沒有證據）。
///
/// **兩個訊號都要成立**：
/// 1. 畫面的回覆槽那一行（`tui_prompts::is_not_logged_in_reply`），以及
/// 2. daemon 自己那份額度讀數是空的或陳舊的（[`quota_is_blank`]）。
///
/// 為什麼要第二個：第一個訊號本來只在「送不出去時才看畫面」那條路上用（`responder::notify` 的錯誤
/// 路徑，issue #420），那個前提本身就是很強的守衛。#427 第 2 項改成每一拍都看、而且巡檢也看，前提
/// 就沒了——一顆完全正常的 bot 只要在回報裡引用那句話就會被判成故障。
///
/// 第二個訊號刻意**不看畫面**：statusLine 上那兩格額度是使用者自己的 `statusline-command.sh` 印的，
/// 認它的字面（`5h:-`）等於把偵測綁在一支外部腳本的格式上，換一台沒有那支腳本的機器就永遠不成立，
/// 於是真的登出也偵測不到，而且測試照不出來（i407 review 2026-09-24）。daemon 自己那份是 claude 的
/// StatusLine hook 送進來的 `rate_limits`（`/api/quota` 同一個來源），登入不了就不會有新的讀數。
async fn probe_role(app: &Arc<App>, role: Role) -> Option<bool> {
    let row = crate::supervisor::roles::get(&app.db, role).await.ok()?;
    let bot_id = row.bot_id?;
    // claude 以外的 CLI 沒有這個畫面，不要拿別人的版面去猜——但那不是「讀不到」，是「不是這個問題」。
    let bot = crate::db::bot(&app.db, &bot_id).await.ok().flatten()?;
    if bot.kind != "claude" {
        return Some(false);
    }
    let run = crate::db::active_run(&app.db, &bot_id).await.ok().flatten()?;
    let pane = run.pane_id.clone()?;
    let client = app.herdr_for_run(&run).await?;
    let read = client.pane_read(&pane, "visible", 80).await.ok()?;
    if !crate::tui_prompts::is_not_logged_in_reply(&read.text) {
        return Some(false);
    }
    Some(quota_is_blank(app, &bot).await)
}

/// daemon 自己那份額度讀數是不是「空的」：整把 key 不在表裡、或它只是開機從 `quota_cache` 回填的
/// 陳舊值（`App.quota_stale`，下一次真的探測成功才會被清掉）、或兩個視窗都沒有數字。
///
/// 登入失效的 CLI 跑不完任何一個回合，也就送不出 StatusLine hook，所以它的讀數只會停在舊的那一份；
/// 而在回報裡引用那句話的 bot 是**剛跑完一個回合**才印得出那份回報，那一回合就會帶一份新的讀數進來。
/// 這正是兩者的差別，而且完全不依賴畫面上印了什麼。
async fn quota_is_blank(app: &Arc<App>, bot: &crate::db::Bot) -> bool {
    let host = crate::db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
    let base = crate::quota::quota_base_for_host(app, &host, &bot.kind, bot.identity.as_deref()).await;
    let key = crate::quota::quota_key(&host, &base);
    if app.quota_stale.lock().await.contains(&key) {
        return true;
    }
    match app.quotas.lock().await.get(&key) {
        None => true,
        Some(q) => q.five_hour.is_none() && q.seven_day.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONDER: Role = Role::Responder;

    async fn configure(app: &Arc<App>, role: Role) {
        sqlx::query("INSERT OR IGNORE INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let bot = format!("bot-{}", role.as_str());
        sqlx::query("INSERT OR IGNORE INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p',?,'claude',?,?)")
            .bind(&bot)
            .bind(&bot)
            .bind(format!("tok-{bot}"))
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        crate::supervisor::roles::set_env(&app.db, role, &bot, "p", "/tmp").await.unwrap();
    }

    /// #421 的不變量：**讀不到畫面不能變成「它壞了」**。沒有 active run 時探針回不了答案，
    /// 這一拍只記 `probe_failed`，`reason` 仍然是 `None`——`role_state` 因此維持原本只看 DB 的行為。
    #[tokio::test]
    async fn a_probe_that_cannot_read_the_screen_is_not_a_fault() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;

        refresh(&app).await;
        let f = snapshot(&app, RESPONDER).await.expect("建立過的角色要有項目");
        assert!(f.probe_failed, "沒有 active run＝這一拍沒有證據");
        assert_eq!(f.reason, None, "沒有證據不能宣告故障");
        assert_eq!(reason(&app, RESPONDER).await, None);
    }

    /// 沒建立的角色**整個不留項目**：留著會讓 `incidents::observe` 每一拍把這個 kind 算成
    /// 「探針沒跑」，另一顆角色已經開著的 incident 就再也關不掉（blind 會擋住 resolve）。
    #[tokio::test]
    async fn a_role_that_was_never_set_up_leaves_no_entry_behind() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;
        refresh(&app).await;
        assert!(snapshot(&app, RESPONDER).await.is_some());
        assert!(snapshot(&app, Role::Patrol).await.is_none(), "巡檢還沒建立：不留紀錄");

        // 角色被拆掉（bot 刪了、登記清空）之後，上一輪留下的項目也要跟著消失。
        sqlx::query("UPDATE supervisor_roles SET bot_id=NULL").execute(&app.db).await.unwrap();
        refresh(&app).await;
        assert!(snapshot(&app, RESPONDER).await.is_none(), "不再建立就不留陳舊的結論");
    }
}
