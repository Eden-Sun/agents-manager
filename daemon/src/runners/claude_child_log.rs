//! 沒有 hook 的 claude 子 agent 讀對話檔（`lifecycle::claude_child_log`）需要的 `App` 端事實：herdr 綁的 session、身分的 config 目錄（issue #878）。

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;
use std::sync::Arc;

/// herdr 綁在這顆 run 的 claude agent 上的 session id（沒綁＝`None`）。
pub(crate) async fn herdr_session(app: &Arc<App>, run: &db::Run) -> Option<String> {
    let client0 = app.herdr_for_run(run).await;
    let (Some(pane), Some(client)) = (run.pane_id.as_deref(), client0) else { return None };
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(name) = run.agent_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        candidates.push(name);
    }
    // herdr 在 `agent start` 逾時後會把名字拿掉，pane 與 claude 還在：改用 pane id 查（#878）。
    candidates.push(pane);
    for target in candidates {
        let Ok(Some(agent)) = client.agent_get(target).await else { continue };
        if agent.pane_id != pane || agent.agent.as_deref().is_some_and(|a| a != "claude") {
            continue;
        }
        if let Some(sid) = crate::lifecycle::dead_panes::herdr_session_id(&agent).filter(|s| crate::lifecycle::grok_transcript::valid_session_id(s)) {
            return Some(sid);
        }
    }
    None
}

/// 去哪些 config 目錄找 `projects/`（去重、保持順序）。
pub(crate) async fn config_roots(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    let home = if host == LOCAL_HOST {
        crate::home::dir().map(|h| h.to_string_lossy().into_owned())
    } else {
        match crate::hosts::HostsAccess::hosts(app).get(host).await {
            Some(conn) => conn.home().await.ok(),
            None => None,
        }
    };
    if let (Some(v), Some(h)) = (bot.env().get("CLAUDE_CONFIG_DIR").filter(|v| !v.trim().is_empty()), home.as_deref()) {
        roots.push(crate::config::expand_home(v.trim(), h));
    }
    let identity = run.runtime_identity.as_deref().or(bot.identity.as_deref());
    if let Ok(dir) = crate::lifecycle::identity_config_dir(app, host, identity).await {
        roots.push(dir);
    }
    if let Some(h) = home {
        roots.push(format!("{}/.claude", h.trim_end_matches('/')));
    }
    let mut seen = std::collections::HashSet::new();
    roots.retain(|r| !r.trim().is_empty() && seen.insert(r.clone()));
    roots
}
