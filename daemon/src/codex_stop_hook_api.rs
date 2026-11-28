//! Codex stdin Stop hook 的信任狀態（#1989 phase 3）：`GET /api/codex/stop-hook`、`POST /api/codex/stop-hook/trust`。
//!
//! 狀態只讀、逐主機逐 Codex 帳號目錄；POST 由使用者確認後只寫 `config.toml` 裡 `[hooks.state."…:stop:…"]` 那一鍵
//! （先備份、其餘內容與格式不動）。管理員範圍：User，以及 `[agents] fleet_operators` 列名的頂層 bot（使用者 2026-11-18 授權代為確認）。

use std::sync::Arc;
use std::time::Duration;

use am_base::codex_hook::{self, StopHookStatus};
use am_base::codex_kill_guard;
use am_base::hosts::sh_quote;
use axum::{extract::State, Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{api::RequestPrincipal, lifecycle::LcError, state::App};

const PROBE_BUDGET: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub host: String,
    /// `None` = the CLI's default account directory.
    pub identity: Option<String>,
    pub codex_home: String,
    /// `not_installed` | `untrusted` | `hash_mismatch` | `trusted` | `unavailable`
    pub status: String,
    /// 廣域 kill 守門（`PreToolUse`）的信任狀態，值同 `status`；同一次「信任」一起寫入。
    pub kill_guard: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    /// `[codex] stop_hook`: with it off nothing is installed or used.
    pub enabled: bool,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
pub struct TrustBody {
    pub host: String,
    #[serde(default)]
    pub identity: Option<String>,
}

struct Target {
    host: String,
    identity: Option<String>,
    codex_home: String,
    command: String,
    /// 本機：`None`（用 data dir）；遠端：`kill-guard.sh` 的絕對路徑。
    kill_guard_sh: Option<String>,
}

fn status_str(s: StopHookStatus) -> &'static str {
    match s {
        StopHookStatus::NotInstalled => "not_installed",
        StopHookStatus::Untrusted => "untrusted",
        StopHookStatus::HashMismatch => "hash_mismatch",
        StopHookStatus::Trusted => "trusted",
    }
}

async fn host_names(app: &Arc<App>) -> Vec<String> {
    let mut names = vec![am_config::LOCAL_HOST.to_string()];
    names.extend(app.cfg.get().await.hosts.iter().map(|h| h.name.clone()));
    names
}

/// Resolve the Codex home and the dispatcher command for one (host, identity).
async fn resolve(app: &Arc<App>, host: &str, identity: Option<&str>) -> Result<Target, LcError> {
    let env_home = match identity {
        None => None,
        Some(name) => {
            let id = crate::tools::identity_for_host(app, host, name).await.filter(|i| i.kind == "codex");
            let id = id.ok_or_else(|| LcError::NotFound(format!("codex identity `{name}` on `{host}`")))?;
            id.env.get("CODEX_HOME").cloned()
        }
    };
    let instance = app.instance();
    if host == am_config::LOCAL_HOST {
        let home = crate::home::dir().ok_or_else(|| LcError::Upstream("local home unavailable".into()))?.to_string_lossy().into_owned();
        let codex_home = env_home.map(|v| am_config::expand_home(&v, &home)).unwrap_or_else(|| format!("{}/.codex", home.trim_end_matches('/')));
        let command = sh_quote(&app.data_dir.join(codex_hook::CODEX_HOOK_SH).to_string_lossy());
        return Ok(Target { host: host.into(), identity: identity.map(str::to_owned), codex_home, command, kill_guard_sh: None });
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| LcError::NotFound(format!("host `{host}`")))?;
    let home = conn.home().await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    let codex_home = env_home.map(|v| am_config::expand_home(&v, &home)).unwrap_or_else(|| format!("{}/.codex", home.trim_end_matches('/')));
    let root = crate::hosts::remote_root_for(instance.as_deref());
    let command = sh_quote(&format!("{home}/{root}/{}", codex_hook::CODEX_HOOK_SH));
    let kill_guard_sh = Some(format!("{home}/{root}/{}", am_base::kill_guard::REMOTE_KILL_GUARD_SH));
    Ok(Target { host: host.into(), identity: identity.map(str::to_owned), codex_home, command, kill_guard_sh })
}

async fn probe(app: &Arc<App>, t: &Target) -> anyhow::Result<StopHookStatus> {
    if t.host == am_config::LOCAL_HOST {
        return codex_hook::local_stop_hook_status(std::path::Path::new(&t.codex_home), &t.command);
    }
    let conn = app.hosts.get(&t.host).await.ok_or_else(|| anyhow::anyhow!("unknown host"))?;
    codex_hook::remote_stop_hook_status(&conn, &t.codex_home, &t.command).await
}

async fn probe_kill_guard(app: &Arc<App>, t: &Target) -> &'static str {
    let status = match &t.kill_guard_sh {
        None => codex_kill_guard::local_status(std::path::Path::new(&t.codex_home), &app.data_dir),
        Some(guard) => match app.hosts.get(&t.host).await {
            Some(conn) => match tokio::time::timeout(PROBE_BUDGET, codex_kill_guard::remote_status(&conn, &t.codex_home, guard)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("timed out")),
            },
            None => Err(anyhow::anyhow!("unknown host")),
        },
    };
    let label = status.map(codex_kill_guard::status_label).unwrap_or("unavailable");
    codex_kill_guard::record(&t.host, &t.codex_home, label);
    label
}

/// 使用者，或 `[agents] fleet_operators` 列名的頂層 bot（使用者授權它代為確認信任）。回傳 fleet operator 的 bot id（使用者＝`None`）。
async fn user_or_fleet_operator(app: &Arc<App>, principal: &RequestPrincipal) -> Result<Option<String>, LcError> {
    match principal {
        RequestPrincipal::User => Ok(None),
        RequestPrincipal::Bot(caller) if crate::api::is_fleet_operator(app, caller).await? => Ok(Some(caller.clone())),
        _ => Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"}))),
    }
}

pub(crate) async fn get(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Report>, LcError> {
    user_or_fleet_operator(&app, &principal).await?;
    let mut entries = Vec::new();
    for host in host_names(&app).await {
        let mut ids: Vec<Option<String>> = vec![None];
        ids.extend(
            crate::tools::identities_for_host(&app, &host).await.into_iter().filter(|i| i.kind == "codex").map(|i| Some(i.name)),
        );
        for identity in ids {
            let t = match resolve(&app, &host, identity.as_deref()).await {
                Ok(t) => t,
                Err(_) => {
                    entries.push(Entry { host: host.clone(), identity, codex_home: String::new(), status: "unavailable".into(), kill_guard: "unavailable".into() });
                    continue;
                }
            };
            let status = match tokio::time::timeout(PROBE_BUDGET, probe(&app, &t)).await {
                Ok(Ok(s)) => status_str(s),
                _ => "unavailable",
            };
            let kill_guard = probe_kill_guard(&app, &t).await;
            entries.push(Entry { host: t.host, identity: t.identity, codex_home: t.codex_home, status: status.into(), kill_guard: kill_guard.into() });
        }
    }
    Ok(Json(Report { enabled: app.cfg.get().await.codex.stop_hook, entries }))
}

pub(crate) async fn post_trust(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<TrustBody>,
) -> Result<Json<Entry>, LcError> {
    let fleet_operator = user_or_fleet_operator(&app, &principal).await?;
    if !host_names(&app).await.contains(&body.host) {
        return Err(LcError::NotFound(format!("host `{}`", body.host)));
    }
    let t = resolve(&app, &body.host, body.identity.as_deref()).await?;
    if let Some(caller) = &fleet_operator {
        tracing::info!(
            http = %crate::config_audit::http_caller(),
            fleet_operator = %caller,
            op = "codex_stop_hook_trust",
            host = %t.host,
            identity = t.identity.as_deref().unwrap_or(""),
            codex_home = %t.codex_home,
            "fleet operator trusts the Codex Stop hook on behalf of the user"
        );
    }
    let lock = codex_hook::home_lock(&t.host, &t.codex_home);
    let _guard = lock.lock().await;
    let instance = app.instance();
    let wrote = if t.host == am_config::LOCAL_HOST {
        codex_hook::install_and_trust_local(std::path::Path::new(&t.codex_home), &app.data_dir, instance.as_deref())
    } else {
        let conn = app.hosts.get(&t.host).await.ok_or_else(|| LcError::NotFound(format!("host `{}`", t.host)))?;
        let root = crate::hosts::remote_root_for(instance.as_deref());
        match tokio::time::timeout(PROBE_BUDGET, codex_hook::install_and_trust_remote(&conn, &t.codex_home, &root, instance.as_deref())).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("timed out")),
        }
    };
    wrote.map_err(|e| LcError::Conflict(json!({"error": "stop_hook_not_trustable", "detail": format!("{e:#}")})))?;
    let status = probe(&app, &t).await.map(status_str).unwrap_or("unavailable");
    let kill_guard = probe_kill_guard(&app, &t).await;
    Ok(Json(Entry { host: t.host, identity: t.identity, codex_home: t.codex_home, status: status.into(), kill_guard: kill_guard.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trust_writes_one_key_and_status_turns_trusted() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let home = crate::home::dir().unwrap();
        let codex_home = home.join(".codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let t = resolve(&app, "local", None).await.unwrap();
        assert_eq!(t.codex_home, codex_home.to_string_lossy());
        assert!(!codex_hook::install_local(&codex_home, &app.data_dir, app.instance().as_deref()).unwrap());
        let before = "# keep me\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
        std::fs::write(codex_home.join("config.toml"), before).unwrap();
        assert_eq!(status_str(probe(&app, &t).await.unwrap()), "not_installed");
        assert_eq!(probe_kill_guard(&app, &t).await, "not_installed");
        let untrusted = codex_kill_guard::health_json();
        assert_eq!(untrusted["status"], "not_effective");
        assert!(untrusted["not_effective"].as_array().unwrap().iter().any(|r| r["codex_home"] == t.codex_home.as_str() && r["message"] == codex_kill_guard::NOT_EFFECTIVE));
        let Json(e) = post_trust(State(app.clone()), Extension(RequestPrincipal::User), Json(TrustBody { host: "local".into(), identity: None })).await.unwrap();
        assert_eq!(e.status, "trusted");
        assert_eq!(e.kill_guard, "trusted", "one trust writes both the Stop and the kill-guard hash");
        let after = std::fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(after.starts_with(before), "everything the user wrote stays byte-identical");
        assert!(after.contains("trusted_hash"));
        assert!(after.contains(&codex_kill_guard::trust_hash(&codex_kill_guard::local_command(&app.data_dir))));
        assert!(after.contains(&codex_hook::stop_hook_trust_hash(&t.command)));
        assert_eq!(codex_kill_guard::health_json()["not_effective"].as_array().unwrap().iter().filter(|r| r["codex_home"] == t.codex_home.as_str()).count(), 0);
        let backups: Vec<_> = std::fs::read_dir(&codex_home).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with("config.toml.am-bak-")).collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), before);
        let bot = post_trust(State(app), Extension(RequestPrincipal::Bot("b".into())), Json(TrustBody { host: "local".into(), identity: None })).await;
        assert!(matches!(bot, Err(LcError::Forbidden(_))));
    }
}
