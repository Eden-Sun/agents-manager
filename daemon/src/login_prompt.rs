//! 「被登出就主動提示我登入」（issue #838 補充，使用者 2026-10-04：「偵測到有被登出就提示我登入，而不是去身份裡面點」）的 daemon 那一半。
//!
//! 網頁的提示條有兩個來源：
//! * **身分探測**（`identities[].logged_in === false`）——原本就在 host 快照裡，網頁自己看得到，這裡不重複。
//! * **回合授權失敗**（這個模組）：claude 的 `StopFailure` 被分類成 `FailureReason::Auth`（或 `Stop` 的最後一句只剩
//!   `Not logged in · Please run /login`）＝某個綁著身分的 bot 剛因為登入失效而失敗。**遠端 claude 的探測一律問不出「未登入」**
//!   （ssh 沒有 GUI session、讀不到 Keychain，[`crate::tools::login_answer_to_cache`] 刻意丟掉），m4p 的 cc1 被登出時探測永遠不會說，
//!   所以這條回合訊號是遠端唯一的來源；本機也靠它抓到「探測說已登入、API 卻 401」的過期憑證。
//!
//! 記錄是記憶體帳 `(host, identity) → {since, via}`（不存 DB：daemon 重啟後身分探測與下一次授權失敗會補回來），
//! 跟著 host 快照一起出去：`hosts[].identities.<name>.login_needed`（`GET /api/state`、`host_changed`、`tools/refresh`），沒有就不帶。
//! 標記時順便立刻重探該身分（不等探測週期）並推 `host_changed`。
//!
//! 清掉：該身分在這台主機被探測成已登入（從非已登入變過來）、登入 watcher 收尾時重驗已登入、登入協助貼完 code 且 CLI 沒報失敗、
//! 或綁著這個身分的 bot 又正常答完一回合。使用者「先關掉」提示只存在網頁這一頁（見 UI-DECISIONS），daemon 不記。

use crate::db;
use crate::state::App;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

pub const VIA_TURN: &str = "turn_auth_failure";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needed {
    /// 這一段「需要登入」從什麼時候開始（ISO）；網頁拿它當「同一次登出」的識別，關掉過就不再提示同一次。
    pub since: String,
    pub via: &'static str,
}

/// `App.login_needed`：`(host, identity)` → 帳。掛在 App 上（同一個 process 裡的另一個 App，例如測試，不共用）。
pub type Registry = Mutex<HashMap<(String, String), Needed>>;

fn key(host: &str, identity: &str) -> (String, String) {
    (host.to_string(), identity.to_string())
}

/// 記一筆（已經有就保留最早的 `since`）；回傳有沒有新增。
pub fn mark(app: &App, host: &str, identity: &str, via: &'static str) -> bool {
    let mut m = app.login_needed.lock().unwrap_or_else(|e| e.into_inner());
    if m.contains_key(&key(host, identity)) {
        return false;
    }
    m.insert(key(host, identity), Needed { since: db::now(), via });
    true
}

/// 拿掉一筆；回傳有沒有拿掉。
pub fn clear(app: &App, host: &str, identity: &str) -> bool {
    app.login_needed.lock().unwrap_or_else(|e| e.into_inner()).remove(&key(host, identity)).is_some()
}

pub fn get(app: &App, host: &str, identity: &str) -> Option<Needed> {
    app.login_needed.lock().unwrap_or_else(|e| e.into_inner()).get(&key(host, identity)).cloned()
}

/// 把這台主機的 `identities`（序列化出去的那份）加上 `login_needed: {since, via}`；沒有帳的身分照舊。
pub fn identities_json(app: &App, host: &str, identities: &BTreeMap<String, crate::tools::IdentityInfo>) -> Value {
    let mut v = json!(identities);
    if let Some(o) = v.as_object_mut() {
        let m = app.login_needed.lock().unwrap_or_else(|e| e.into_inner());
        for (name, entry) in o.iter_mut() {
            if let (Some(n), Some(e)) = (m.get(&key(host, name)), entry.as_object_mut()) {
                e.insert("login_needed".into(), json!({"since": n.since, "via": n.via}));
            }
        }
    }
    v
}

/// claude 沒登入時回合只回的那一行（`Not logged in · Please run /login`、舊寫法 `Run /login`）。
/// 整則就是這一行才算：bot 在回報裡引用那句話不能觸發提示（跟網頁 `lib/authFailure.ts` 同一條規則）。
pub fn is_not_logged_in_line(text: &str) -> bool {
    let t = text.trim().trim_start_matches(['⎿', ' ', '\t']).trim_end_matches(['.', '。']).trim().to_ascii_lowercase();
    let Some(rest) = t.strip_prefix("not logged in") else { return false };
    let rest = rest.trim_start().trim_start_matches('·').trim_start();
    let rest = rest.strip_prefix("please ").unwrap_or(rest);
    rest == "run /login"
}

async fn push_host(app: &Arc<App>, host: &str) {
    if let Some(fence) = app.hosts.fence(host).await {
        crate::state::emit_host_changed(app, &fence).await;
    }
}

/// 拿掉並在有變時推 `host_changed`。
pub async fn clear_and_push(app: &Arc<App>, host: &str, identity: &str) {
    if clear(app, host, identity) {
        push_host(app, host).await;
    }
}

/// 綁著身分的 claude bot 的回合因為授權失敗收尾：記下、推快照，並立刻重探該身分（不等探測週期）。
/// 沒綁身分（用主機預設帳號）的 bot 不處理：沒有「哪個身分」可以提示。
pub async fn on_auth_failure(app: &Arc<App>, bot: &db::Bot) {
    if bot.kind != "claude" {
        return;
    }
    let Some(identity) = bot.identity.as_deref().filter(|i| !i.is_empty()) else { return };
    let Ok(host) = db::bot_host(&app.db, &bot.id).await else { return };
    // 身分不在這台主機的表裡（改名、刪除、還沒偵測）：沒有東西可以附註，也就不記。
    let known = app.tools.lock().await.get(&host).is_some_and(|h| h.identities.contains_key(identity));
    if !known {
        return;
    }
    if mark(app, &host, identity, VIA_TURN) {
        tracing::info!(bot = %bot.name, %host, identity, "a turn failed on authentication; asking the user to log this identity in again");
        push_host(app, &host).await;
    }
    let (app, host, identity) = (app.clone(), host, identity.to_string());
    tokio::spawn(async move {
        // 本機問得出「未登入」會更新快取並推快照（`record_identity_login_fenced`）；遠端 claude 問不出來就維持上面的記號。
        let _ = crate::tools::recheck_identity_login(&app, &host, &identity).await;
    });
}

/// 綁著身分的 claude bot 又正常答完一回合：那個身分是通的。
pub async fn on_turn_ok(app: &Arc<App>, bot: &db::Bot) {
    if bot.kind != "claude" {
        return;
    }
    let Some(identity) = bot.identity.as_deref().filter(|i| !i.is_empty()) else { return };
    // 先看有沒有帳：絕大多數回合都沒有，不必每次都查 DB。
    if !app.login_needed.lock().unwrap_or_else(|e| e.into_inner()).keys().any(|(_, i)| i == identity) {
        return;
    }
    let Ok(host) = db::bot_host(&app.db, &bot.id).await else { return };
    clear_and_push(app, &host, identity).await;
}

#[cfg(test)]
mod tests;
