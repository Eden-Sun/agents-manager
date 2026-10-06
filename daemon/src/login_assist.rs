//! 手機版重新登入失效的 claude 身分（issue #838，使用者 2026-10-04）：`POST /hosts/{host}/identities/{identity}/login`
//! 開的臨時登入 pane 裡，`claude auth login` 把 OAuth 網址與「Paste code here if prompted >」只畫在終端；手機上要選取一長串
//! 網址、再把 code 貼回終端幾乎做不到。這裡讓網頁直接拿到網址（「打開登入網站」）、把 code 打進**那個登入 pane**。
//!
//! * **只認 daemon 自己開的登入 pane**：[`register`] 記 `(host, pane_id) → identity`（只有 claude 的登入，登出與別的 kind 不記），
//!   pane 被關（watcher 收尾、手動關）就 [`forget`]。不在帳上的 pane 一律 404——這兩個端點不是任意 pane 的讀寫通道。
//!   帳上另記開 pane 當時的 `HostShell.created_at`，pane id 之後被別的 shell 重用也對不上。
//! * **網址與 code 不進 log／事件**：網址只在 `GET …/login` 的回應裡（`Cache-Control: no-store`）；code 只經過 `POST …/login/code`
//!   的 body 到 `pane.send_text`，沒有任何一行 tracing 帶它們，也不推 WS。
//! * **畫面對不上就不送**：送 code 前現讀畫面，最後一個非空行必須正好是等 code 的提示（`Paste code here if prompted >`；
//!   CLI 吐出錯誤或已經結束，那一行後面就有字或換成 shell 提示），而且 pane 的前景程序還是 `claude`；對不上回 409 `not_awaiting_code`，
//!   一個字都沒打。code 只收 OAuth code 會有的字元（`A-Za-z0-9_.~#:/+=%-`，最長 1024）：萬一 CLI 在讀畫面與打字之間結束、
//!   字落進 shell，也不是能執行的東西。
//! * 畫面用 `recent_unwrapped` 讀（herdr 把折行還原成一整行）；舊式折行讀法（網址被終端寬度硬折成幾列、中間沒有空白）也接得起來。
//!
//! 送出之後等 CLI 的反應最多 [`OUTCOME_WAIT`]：看到 `Login failed: …` 就把那一句回給網頁（pane 隨後會被 watcher 收掉，網頁來不及讀）。
//! 成功與否仍以 watcher 的重驗為準（身分列的登入狀態），這裡不猜。

use crate::state::App;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::app_ports_p13::{LcError, LcResult};

/// 等 code 的提示整行。claude 2.1.289 實測；之後改字就會變成「不在等 code」——fail closed，終端照樣能貼。
pub const PROMPT: &str = "Paste code here if prompted >";
/// 送出之後最多等多久看 CLI 的反應。
const OUTCOME_WAIT: Duration = Duration::from_secs(8);
const OUTCOME_POLL: Duration = Duration::from_millis(500);
/// 登入 pane 最多活多久（watcher 的上限是 15 分鐘）；帳上的紀錄只多留一點。
const MAX_AGE: Duration = Duration::from_secs(20 * 60);
const MAX_CODE_LEN: usize = 1024;
const MAX_URL_LEN: usize = 4096;
const READ_LINES: u32 = 80;

pub struct Entry {
    identity: String,
    /// 開 pane 當時 `HostShell.created_at`：pane id 被重用時對不上。
    shell_created_at: String,
    opened: Instant,
    code_sent: bool,
}

/// `App.login_panes`：掛在 App 上而不是 process 全域（同一個 process 裡的另一個 App，例如測試，pane id 會撞）。
pub type Registry = Mutex<HashMap<(String, String), Entry>>;
pub type Reservations = Mutex<HashSet<(String, String)>>;

/// Held while `identity_auth` opens and registers the pane. Active panes take over the same
/// `(host, identity)` key, so another request cannot start a competing OAuth flow in the gap.
pub struct IdentityReservation {
    app: Arc<App>,
    key: (String, String),
    active: bool,
}

impl IdentityReservation {
    pub fn register(mut self, pane_id: &str, shell_created_at: &str) {
        let mut reservations = self.app.login_reservations.lock().unwrap_or_else(|e| e.into_inner());
        let mut panes = self.app.login_panes.lock().unwrap_or_else(|e| e.into_inner());
        panes.insert(
            (self.key.0.clone(), pane_id.to_string()),
            Entry {
                identity: self.key.1.clone(),
                shell_created_at: shell_created_at.to_string(),
                opened: Instant::now(),
                code_sent: false,
            },
        );
        drop(panes);
        reservations.remove(&self.key);
        drop(reservations);
        self.active = false;
    }
}

impl Drop for IdentityReservation {
    fn drop(&mut self) {
        if self.active {
            self.app.login_reservations.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.key);
        }
    }
}

/// Claim a single active Claude login flow for this host and configured identity. The host is part
/// of the key because identity env expansion and its credential directory are host-local.
pub fn reserve(app: &Arc<App>, host: &str, identity: &str) -> LcResult<IdentityReservation> {
    let key = (host.to_string(), identity.to_string());
    let mut reservations = app.login_reservations.lock().unwrap_or_else(|e| e.into_inner());
    if reservations.contains(&key) {
        return Err(identity_login_in_progress(host, identity));
    }
    let panes = app.login_panes.lock().unwrap_or_else(|e| e.into_inner());
    let active = panes.iter().any(|((entry_host, _), entry)| {
        entry_host == host && entry.identity == identity && entry.opened.elapsed() < MAX_AGE
    });
    if active {
        return Err(identity_login_in_progress(host, identity));
    }
    reservations.insert(key.clone());
    Ok(IdentityReservation { app: app.clone(), key, active: true })
}

fn identity_login_in_progress(host: &str, identity: &str) -> LcError {
    LcError::conflict(
        "identity_login_in_progress",
        json!({"host": host, "identity": identity, "message": "這個身分已有登入程序；先完成或關閉原登入終端再重試"}),
    )
}

/// Host reconfiguration invalidates the old pane authority and any opening login reservation.
pub fn forget_host(app: &(impl crate::login_assist::LoginPanes + crate::login_assist::LoginReservations), host: &str) {
    let mut reservations = app.login_reservations().lock().unwrap_or_else(|e| e.into_inner());
    reservations.retain(|(entry_host, _)| entry_host != host);
    app.login_panes().lock().unwrap_or_else(|e| e.into_inner()).retain(|(entry_host, _), _| entry_host != host);
}

/// pane 關了（watcher 收尾、手動關）。
pub fn forget(app: &impl crate::login_assist::LoginPanes, host: &str, pane_id: &str) {
    app.login_panes().lock().unwrap_or_else(|e| e.into_inner()).remove(&(host.to_string(), pane_id.to_string()));
}

#[cfg(test)]
pub fn is_registered(app: &App, host: &str, pane_id: &str) -> bool {
    app.login_panes.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&(host.to_string(), pane_id.to_string()))
}

/// 畫面上看得出來的登入進度（純函式，不碰 pane）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoginScreen {
    /// OAuth 網址（原樣；已驗過是 claude 的 https 網址）。
    pub url: Option<String>,
    /// 最後一個非空行正好是等 code 的提示。
    pub awaiting_code: bool,
    /// CLI 吐出的失敗說明（`Login failed: …`）。
    pub failure: Option<String>,
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// 只把 claude 的登入網址交給網頁：https、host 是 claude.com／claude.ai／anthropic.com（含子網域）、是 oauth authorize。
/// 畫面是任意終端輸出，不能讓它塞一個別的網址給「打開登入網站」。
pub fn is_login_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    if url.len() > MAX_URL_LEN || !url.chars().all(is_url_char) {
        return false;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.contains('@') || host.contains(':') {
        return false;
    }
    let ok_host = ["claude.com", "claude.ai", "anthropic.com"].iter().any(|d| host == *d || host.ends_with(&format!(".{d}")));
    ok_host && rest[host.len()..].contains("/oauth/authorize")
}

pub fn parse_screen(text: &str) -> LoginScreen {
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut out = LoginScreen::default();
    // 網址：由下往上找最後一個 https:// 起頭的 token，後面沒有空白的整列都是被硬折下來的尾巴。
    'find: for (i, line) in lines.iter().enumerate().rev() {
        let Some(at) = line.find("https://") else { continue };
        let mut url: String = line[at..].split_whitespace().next().unwrap_or("").to_string();
        // 這個 token 後面還有字（空白之後）＝整個網址就在這一列、不會折到下一列。
        let token_ends_line = line[at..].trim_end().len() == url.len();
        if token_ends_line {
            for next in &lines[i + 1..] {
                let t = next.trim_end();
                if t.is_empty() || t.chars().any(char::is_whitespace) || !t.chars().all(is_url_char) {
                    break;
                }
                url.push_str(t);
            }
        }
        if is_login_url(&url) {
            out.url = Some(url);
            break 'find;
        }
    }
    let last = lines.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty());
    out.awaiting_code = last == Some(PROMPT);
    out.failure = lines.iter().rev().find_map(|l| {
        let at = l.find("Login failed")?;
        Some(l[at..].trim().chars().take(200).collect::<String>())
    });
    out
}

/// code 只收 OAuth code 會有的字元；空的、太長、含空白或 shell 會當成語法的字元一律不收。
pub fn valid_code(code: &str) -> bool {
    !code.is_empty() && code.len() <= MAX_CODE_LEN && code.chars().all(|c| c.is_ascii_alphanumeric() || "_.~#:/+=%-".contains(c))
}

struct Target {
    identity: String,
    code_sent: bool,
}

/// 帳上有、而且就是現在這顆 shell（pane id 被重用、shell 已被收掉都算沒有）。
async fn target(app: &(impl crate::capabilities::Db + crate::login_assist::LoginPanes + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables + crate::api::shell::HostShells), host: &str, pane_id: &str) -> LcResult<Target> {
    let key = (host.to_string(), pane_id.to_string());
    let (identity, created, code_sent) = {
        let m = app.login_panes().lock().unwrap_or_else(|e| e.into_inner());
        let e = m.get(&key).filter(|e| e.opened.elapsed() < MAX_AGE).ok_or_else(|| LcError::NotFound("login pane".into()))?;
        (e.identity.clone(), e.shell_created_at.clone(), e.code_sent)
    };
    let live = app.host_shells().lock().await.iter().any(|s| s.host == host && s.pane_id == pane_id && s.created_at == created);
    if !live {
        forget(app, host, pane_id);
        return Err(LcError::NotFound("login pane".into()));
    }
    Ok(Target { identity, code_sent })
}

async fn read_screen(app: &Arc<App>, host: &str, pane_id: &str) -> LcResult<LoginScreen> {
    let v = crate::app_ports_p13::shell_read(app, host, pane_id, "recent_unwrapped", READ_LINES).await?;
    Ok(parse_screen(v.get("text").and_then(Value::as_str).unwrap_or("")))
}

/// `GET /api/hosts/:name/shells/:pane_id/login`
pub async fn status(app: &Arc<App>, host: &str, pane_id: &str) -> LcResult<Value> {
    let t = target(app, host, pane_id).await?;
    let screen = read_screen(app, host, pane_id).await?;
    Ok(json!({
        "host": host,
        "pane_id": pane_id,
        "identity": t.identity,
        "kind": "claude",
        "url": screen.url,
        "awaiting_code": screen.awaiting_code,
        "code_sent": t.code_sent,
        "failure": screen.failure,
    }))
}

fn not_awaiting(why: &str) -> LcError {
    LcError::conflict(
        "not_awaiting_code",
        json!({"sent": false, "message": format!("登入終端現在不在等 code（{why}），一個字都沒送；到「終端」看一下或重新按登入"), "retryable": true}),
    )
}

fn code_already_sent() -> LcError {
    LcError::conflict(
        "code_already_sent",
        json!({"sent": false, "message": "這個登入終端已送過 code；請等結果，或關閉終端後重新登入"}),
    )
}

fn claim_code_send(app: &impl crate::login_assist::LoginPanes, host: &str, pane_id: &str) -> LcResult<()> {
    let key = (host.to_string(), pane_id.to_string());
    let mut panes = app.login_panes().lock().unwrap_or_else(|e| e.into_inner());
    let entry = panes
        .get_mut(&key)
        .filter(|e| e.opened.elapsed() < MAX_AGE)
        .ok_or_else(|| LcError::NotFound("login pane".into()))?;
    if entry.code_sent {
        return Err(code_already_sent());
    }
    // Claim before the RPC: concurrent POSTs must not both reach the pane. On an ambiguous RPC
    // failure the User closes/reopens the pane instead of risking a duplicate one-time code.
    entry.code_sent = true;
    Ok(())
}

/// `POST /api/hosts/:name/shells/:pane_id/login/code`：確認畫面還在等 code 才把它打進去、按 Enter。
/// 回 `{sent:true, outcome}`：`failed`（帶 `message`＝CLI 的 `Login failed: …`）／`finished`（CLI 結束了或 pane 已收掉，成功與否看身分列重驗）／`pending`。
pub async fn submit_code(app: &Arc<App>, host: &str, pane_id: &str, code: &str) -> LcResult<Value> {
    let code = code.trim();
    if !valid_code(code) {
        return Err(LcError::Bad("code 只能有英數字與 _ . ~ # : / + = % -，最長 1024 字".into()));
    }
    let t = target(app, host, pane_id).await?;
    if t.code_sent {
        return Err(code_already_sent());
    }
    let screen = read_screen(app, host, pane_id).await?;
    if !screen.awaiting_code {
        return Err(not_awaiting(if screen.url.is_some() { "提示已經不在畫面最後一行" } else { "畫面上還沒有等 code 的提示" }));
    }
    // 前景程序還是 claude：讀畫面到打字之間 CLI 結束的話，字會落進 shell。問不到就不送。
    let Some(fence) = app.hosts.fence(host).await else { return Err(LcError::NotFound("host".into())) };
    let alive = match fence.conn().client.pane_process_info(pane_id).await {
        Ok(ps) => ps.iter().any(|p| {
            p.argv.iter().chain(p.argv0.iter()).any(|a| std::path::Path::new(a).file_name().and_then(|v| v.to_str()) == Some("claude"))
        }),
        Err(_) => false,
    };
    if !alive {
        return Err(not_awaiting("claude 已經不在這個終端裡跑了"));
    }
    claim_code_send(app, host, pane_id)?;
    crate::app_ports_p13::shell_send_text(app, host, pane_id, code, true).await?;
    tracing::info!(host, pane_id, identity = %t.identity, "a login code was typed into the login pane");
    // 等 CLI 的反應：`Login failed` 要在 pane 被收掉之前讀走。
    let deadline = Instant::now() + OUTCOME_WAIT;
    loop {
        tokio::time::sleep(OUTCOME_POLL).await;
        match read_screen(app, host, pane_id).await {
            Err(_) => return Ok(finished(app, host, &t.identity).await),
            Ok(s) if s.failure.is_some() => return Ok(json!({"sent": true, "outcome": "failed", "message": s.failure})),
            Ok(s) if !s.awaiting_code => return Ok(finished(app, host, &t.identity).await),
            Ok(_) => {}
        }
        if Instant::now() >= deadline {
            return Ok(json!({"sent": true, "outcome": "pending", "message": Value::Null}));
        }
    }
}

/// CLI 收下 code 之後結束了、沒報失敗：主動提示（`login_prompt`）到此為止。
/// 偶爾是「失敗訊息來不及讀、pane 就被收掉」被當成 finished——那時提示會少一次，下一次授權失敗或探測會再把它叫回來。
async fn finished(app: &Arc<App>, host: &str, identity: &str) -> Value {
    crate::login_prompt::clear_and_push(app, host, identity).await;
    json!({"sent": true, "outcome": "finished", "message": Value::Null})
}

#[cfg(test)]
mod tests;

/// 登入輔助開出的 pane。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait LoginPanes: Send + Sync {
    fn login_panes(&self) -> &crate::login_assist::Registry;
}

/// 登入殼的保留名單。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait LoginReservations: Send + Sync {
    fn login_reservations(&self) -> &crate::login_assist::Reservations;
}
