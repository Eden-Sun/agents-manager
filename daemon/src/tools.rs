//! v4.0 — per-host CLI detection (`hosts[].tools`) and "install / log in through an existing
//! agent" (`POST /api/hosts/:name/tools/install`).
//!
//! Detection runs one POSIX `sh` script on the host (locally through `/bin/sh`, remotely
//! through `HostConn::ssh_exec_path`). Executables are looked up the way a pane sees them —
//! through the user's *login* shell — so a tool installed by e.g. Homebrew or `~/.local/bin`
//! is found even though the daemon / a non-interactive ssh shell has a bare PATH.

use crate::config::{valid_kind, LOCAL_HOST};
use crate::state::App;
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolInfo {
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// `None` = could not tell.
    pub logged_in: Option<bool>,
}

impl Default for ToolInfo {
    fn default() -> Self {
        Self { installed: false, path: None, version: None, logged_in: None }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HostTools {
    pub tools: BTreeMap<String, ToolInfo>,
    pub checked_at: String,
}

/// The probe. Every line is `AM_<WHAT> <kind> <value>`; a missing value means unknown.
/// claude's login on macOS lives in the Keychain: `security find-generic-password` without
/// `-w` needs no unlock, but a non-interactive session can still be refused — that case is
/// reported as unknown rather than "not logged in".
pub const PROBE_SH: &str = r#"
for k in claude codex grok; do
  p=$( "${SHELL:-/bin/sh}" -lic "command -v $k" 2>/dev/null | tail -1 )
  [ -n "$p" ] || p=$(command -v "$k" 2>/dev/null)
  case "$p" in /*) ;; *) p="" ;; esac
  printf 'AM_PATH %s %s\n' "$k" "$p"
  if [ -n "$p" ]; then
    v=$( "$p" --version 2>/dev/null </dev/null | head -1 | tr -d '\r' )
    printf 'AM_VER %s %s\n' "$k" "$v"
  fi
done
CD="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
if [ -f "$CD/.credentials.json" ]; then
  printf 'AM_LOGIN claude 1\n'
elif command -v security >/dev/null 2>&1; then
  out=$(security find-generic-password -s "Claude Code-credentials" 2>&1); rc=$?
  if [ $rc -eq 0 ]; then printf 'AM_LOGIN claude 1\n'
  elif printf '%s' "$out" | grep -qi "could not be found"; then printf 'AM_LOGIN claude 0\n'
  else printf 'AM_LOGIN claude ?\n'; fi
else
  printf 'AM_LOGIN claude 0\n'
fi
if [ -f "${CODEX_HOME:-$HOME/.codex}/auth.json" ]; then printf 'AM_LOGIN codex 1\n'; else printf 'AM_LOGIN codex 0\n'; fi
GH="${GROK_HOME:-$HOME/.grok}"
if [ -f "$GH/auth.json" ] || ls "$GH"/auth* >/dev/null 2>&1; then printf 'AM_LOGIN grok 1\n'; else printf 'AM_LOGIN grok 0\n'; fi
"#;

pub fn parse_probe(out: &str) -> BTreeMap<String, ToolInfo> {
    let mut m: BTreeMap<String, ToolInfo> = crate::config::KINDS.iter().map(|k| (k.to_string(), ToolInfo::default())).collect();
    for line in out.lines() {
        let mut it = line.trim_end().splitn(3, ' ');
        let (Some(tag), Some(kind)) = (it.next(), it.next()) else { continue };
        let val = it.next().unwrap_or("").trim();
        let Some(t) = m.get_mut(kind) else { continue };
        match tag {
            "AM_PATH" => {
                if !val.is_empty() {
                    t.installed = true;
                    t.path = Some(val.to_string());
                }
            }
            "AM_VER" => {
                if !val.is_empty() {
                    t.version = Some(val.to_string());
                }
            }
            "AM_LOGIN" => {
                t.logged_in = match val {
                    "1" => Some(true),
                    "0" => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    // A tool that is not installed cannot be "logged in"; keep the file-based answer only
    // when it is positive (credentials may survive an uninstall).
    for t in m.values_mut() {
        if !t.installed && t.logged_in == Some(false) {
            t.logged_in = None;
        }
    }
    m
}

/// Run the probe on `host` and cache the result. Errors are returned (remote ssh failures);
/// the cache is left untouched then.
pub async fn detect(app: &Arc<App>, host: &str) -> Result<HostTools> {
    let out = if host == LOCAL_HOST {
        let o = tokio::time::timeout(
            Duration::from_secs(40),
            tokio::process::Command::new("/bin/sh").arg("-c").arg(PROBE_SH).stdin(std::process::Stdio::null()).output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("local tool probe timed out"))??;
        String::from_utf8_lossy(&o.stdout).to_string()
    } else {
        let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
        conn.ssh_exec_path(PROBE_SH).await?
    };
    let ht = HostTools { tools: parse_probe(&out), checked_at: crate::db::now() };
    app.tools.lock().await.insert(host.to_string(), ht.clone());
    tracing::info!(host, tools = ?ht.tools.iter().map(|(k, t)| (k.clone(), t.installed, t.logged_in)).collect::<Vec<_>>(), "tools detected");
    Ok(ht)
}

/// Fire-and-forget detection (on connect); pushes `host_changed` when done so the UI refetches.
pub fn spawn_detect(app: Arc<App>, host: String) {
    tokio::spawn(async move {
        match detect(&app, &host).await {
            Ok(_) => {
                if let Some(conn) = app.hosts.get(&host).await {
                    crate::state::emit_host_changed(&app, &conn).await;
                }
            }
            Err(e) => tracing::warn!(host, error = %e, "tool detection failed"),
        }
    });
}

/// The resolved executable path for a kind on a host, from the detection cache.
pub async fn cached_path(app: &Arc<App>, host: &str, kind: &str) -> Option<String> {
    app.tools.lock().await.get(host).and_then(|h| h.tools.get(kind)).and_then(|t| t.path.clone())
}

/// The prompt sent to an existing agent to install + log in `kind` (official installers).
pub fn install_prompt(kind: &str) -> Option<String> {
    let (name, install, login) = match kind {
        "claude" => (
            "Claude Code",
            "curl -fsSL https://claude.ai/install.sh | bash   （官方安裝腳本；若失敗可改用 npm i -g @anthropic-ai/claude-code）",
            "claude   （首次啟動會進入登入流程）",
        ),
        "codex" => ("OpenAI Codex CLI", "npm i -g @openai/codex   （官方安裝方式）", "codex login"),
        "grok" => ("xAI Grok CLI", "curl -fsSL https://x.ai/cli/install.sh | bash   （官方安裝腳本）", "grok login"),
        _ => return None,
    };
    Some(format!(
        "請在這台機器上安裝並登入 {name}，步驟如下，逐步執行並回報每一步的輸出：\n\
         1. 安裝：`{install}`。\n\
         2. 確認安裝成功：執行 `{kind} --version` 並印出結果（若找不到指令，檢查安裝腳本輸出的安裝路徑並加入 PATH，例如 ~/.local/bin 或 ~/.{kind}/bin）。\n\
         3. 登入：執行 `{login}`。這是互動式流程，會顯示一個登入 URL（或裝置代碼）；請把該 URL 原封不動、完整地印出來給我，然後停在那裡等待我在瀏覽器完成登入，不要自行中斷或略過。\n\
         4. 登入完成後再執行一次 `{kind} --version` 確認，並回報「{kind} 已安裝並登入」。",
    ))
}

/// `POST /api/hosts/:name/tools/install` — validate, then send the prompt through the
/// ordinary prompt path (per-bot lock, idempotency, delivery states).
pub async fn install_via_bot(
    app: &Arc<App>,
    host: &str,
    kind: &str,
    via_bot_id: &str,
) -> crate::lifecycle::LcResult<crate::lifecycle::PromptOut> {
    use crate::lifecycle::LcError;
    if !valid_kind(kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let bot = crate::db::bot(&app.db, via_bot_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    let bot_host = crate::db::bot_host(&app.db, &bot.id).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if bot_host != host {
        return Err(LcError::Bad(format!("bot `{}` lives on host `{bot_host}`, not `{host}`", bot.name)));
    }
    let text = install_prompt(kind).ok_or_else(|| LcError::Bad("unknown kind".into()))?;
    let crid = format!("tools-install:{kind}:{}", crate::db::ulid());
    crate::lifecycle::prompt(app, &bot.id, &text, &crid).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_output() {
        let out = "AM_PATH claude /opt/homebrew/bin/claude\nAM_VER claude 2.1.0 (Claude Code)\nAM_PATH codex /usr/local/bin/codex\nAM_VER codex codex-cli 0.120.0\nAM_PATH grok \nAM_LOGIN claude ?\nAM_LOGIN codex 1\nAM_LOGIN grok 0\n";
        let m = parse_probe(out);
        assert!(m["claude"].installed);
        assert_eq!(m["claude"].path.as_deref(), Some("/opt/homebrew/bin/claude"));
        assert_eq!(m["claude"].version.as_deref(), Some("2.1.0 (Claude Code)"));
        assert_eq!(m["claude"].logged_in, None);
        assert_eq!(m["codex"].logged_in, Some(true));
        assert!(!m["grok"].installed);
        assert!(m["grok"].path.is_none());
        // not installed + "no auth file" → unknown, not false
        assert_eq!(m["grok"].logged_in, None);
    }

    #[test]
    fn install_prompts_use_official_installers() {
        assert!(install_prompt("grok").unwrap().contains("https://x.ai/cli/install.sh"));
        assert!(install_prompt("claude").unwrap().contains("https://claude.ai/install.sh"));
        assert!(install_prompt("codex").unwrap().contains("npm i -g @openai/codex"));
        assert!(install_prompt("codex").unwrap().contains("codex login"));
        assert!(install_prompt("nope").is_none());
    }
}
