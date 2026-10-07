//! `login_assist` runner。

use crate::api::shell;
use crate::lc_error::{LcError, LcResult};
use crate::login_assist::{
    claim_code_send, parse_screen, valid_code, Entry, LoginScreen, Target, MAX_AGE, OUTCOME_POLL, OUTCOME_WAIT, READ_LINES,
};
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

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

async fn read_screen(app: &Arc<App>, host: &str, pane_id: &str) -> LcResult<LoginScreen> {
    let v = shell::read(app, host, pane_id, "recent_unwrapped", READ_LINES).await?;
    let text = v.get("text").and_then(Value::as_str).unwrap_or("");
    Ok(parse_screen(text))
}

/// 帳上有、而且就是現在這顆 shell（pane id 被重用、shell 已被收掉都算沒有）。
async fn target(app: &Arc<App>, host: &str, pane_id: &str) -> LcResult<Target> {
    let key = (host.to_string(), pane_id.to_string());
    let (identity, created, code_sent) = {
        let m = app.login_panes.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.get(&key).filter(|e| e.opened.elapsed() < MAX_AGE).ok_or_else(|| LcError::NotFound("login pane".into()))?;
        (e.identity.clone(), e.shell_created_at.clone(), e.code_sent)
    };
    let live = app.host_shells.lock().await.iter().any(|s| s.host == host && s.pane_id == pane_id && s.created_at == created);
    if !live {
        crate::login_assist::forget(app, host, pane_id);
        return Err(LcError::NotFound("login pane".into()));
    }
    Ok(Target { identity, code_sent })
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
    shell::send_text(app, host, pane_id, code, true).await?;
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
    crate::runners::login_prompt::clear_and_push(app, host, identity).await;
    json!({"sent": true, "outcome": "finished", "message": Value::Null})
}
