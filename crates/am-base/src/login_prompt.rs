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
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

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
pub fn mark(app: &impl crate::login_prompt::LoginNeeded, host: &str, identity: &str, via: &'static str) -> bool {
    let mut m = app.login_needed().lock().unwrap_or_else(|e| e.into_inner());
    if m.contains_key(&key(host, identity)) {
        return false;
    }
    m.insert(key(host, identity), Needed { since: db::now(), via });
    true
}

/// 拿掉一筆；回傳有沒有拿掉。
pub fn clear(app: &impl crate::login_prompt::LoginNeeded, host: &str, identity: &str) -> bool {
    app.login_needed().lock().unwrap_or_else(|e| e.into_inner()).remove(&key(host, identity)).is_some()
}

#[allow(dead_code)]
pub fn get(app: &impl crate::login_prompt::LoginNeeded, host: &str, identity: &str) -> Option<Needed> {
    app.login_needed().lock().unwrap_or_else(|e| e.into_inner()).get(&key(host, identity)).cloned()
}

/// 把這台主機的 `identities`（序列化出去的那份）加上 `login_needed: {since, via}`；沒有帳的身分照舊。
pub fn identities_json(app: &impl crate::login_prompt::LoginNeeded, host: &str, identities: &BTreeMap<String, crate::tools::IdentityInfo>) -> Value {
    let mut v = json!(identities);
    if let Some(o) = v.as_object_mut() {
        let m = app.login_needed().lock().unwrap_or_else(|e| e.into_inner());
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

/// 「請登入」提示。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait LoginNeeded: Send + Sync {
    fn login_needed(&self) -> &crate::login_prompt::Registry;
}
