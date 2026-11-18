//! Codex stdin Stop hook 的信任狀態（#1989 phase 3）：`GET /api/codex/stop-hook`、`POST /api/codex/stop-hook/trust`。
//!
//! 狀態只讀、逐主機逐 Codex 帳號目錄；POST 由使用者確認後只寫 `config.toml` 裡 `[hooks.state."…:stop:…"]` 那一鍵
//! （先備份、其餘內容與格式不動）。管理員範圍：User-only。

use std::sync::Arc;
use std::time::Duration;

use am_base::codex_hook::{self, StopHookStatus};
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
        return Ok(Target { host: host.into(), identity: identity.map(str::to_owned), codex_home, command });
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| LcError::NotFound(format!("host `{host}`")))?;
    let home = conn.home().await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    let codex_home = env_home.map(|v| am_config::expand_home(&v, &home)).unwrap_or_else(|| format!("{}/.codex", home.trim_end_matches('/')));
    let root = crate::hosts::remote_root_for(instance.as_deref());
    let command = sh_quote(&format!("{home}/{root}/{}", codex_hook::CODEX_HOOK_SH));
    Ok(Target { host: host.into(), identity: identity.map(str::to_owned), codex_home, command })
}

async fn probe(app: &Arc<App>, t: &Target) -> anyhow::Result<StopHookStatus> {
    if t.host == am_config::LOCAL_HOST {
        return codex_hook::local_stop_hook_status(std::path::Path::new(&t.codex_home), &t.command);
    }
    let conn = app.hosts.get(&t.host).await.ok_or_else(|| anyhow::anyhow!("unknown host"))?;
    codex_hook::remote_stop_hook_status(&conn, &t.codex_home, &t.command).await
}

pub(crate) async fn get(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Report>, LcError> {
    if principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
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
                    entries.push(Entry { host: host.clone(), identity, codex_home: String::new(), status: "unavailable".into() });
                    continue;
                }
            };
            let status = match tokio::time::timeout(PROBE_BUDGET, probe(&app, &t)).await {
                Ok(Ok(s)) => status_str(s),
                _ => "unavailable",
            };
            entries.push(Entry { host: t.host, identity: t.identity, codex_home: t.codex_home, status: status.into() });
        }
    }
    Ok(Json(Report { enabled: app.cfg.get().await.codex.stop_hook, entries }))
}

type TrustLocks = std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// 每個 (host, CODEX_HOME) 一把鎖；寫入信任與安裝 handler 只在這把鎖底下做。
fn trust_lock(host: &str, codex_home: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<TrustLocks> = std::sync::OnceLock::new();
    let mut map = LOCKS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    map.entry(format!("{host}\n{codex_home}")).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
}

pub(crate) async fn post_trust(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<TrustBody>,
) -> Result<Json<Entry>, LcError> {
    if principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    if !host_names(&app).await.contains(&body.host) {
        return Err(LcError::NotFound(format!("host `{}`", body.host)));
    }
    let t = resolve(&app, &body.host, body.identity.as_deref()).await?;
    // 同一台主機、同一個 CODEX_HOME 的寫入序列化：兩次按鈕不會互相覆蓋 config.toml／hooks.json。
    let lock = trust_lock(&t.host, &t.codex_home);
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
    // Not installed / unreadable config etc. are the caller's state to fix, not a server fault.
    wrote.map_err(|e| LcError::Conflict(json!({"error": "stop_hook_not_trustable", "detail": format!("{e:#}")})))?;
    let status = probe(&app, &t).await.map(status_str).unwrap_or("unavailable");
    Ok(Json(Entry { host: t.host, identity: t.identity, codex_home: t.codex_home, status: status.into() }))
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
        // 安裝本身不註冊 handler：沒有信任之前，hooks.json 裡不會出現我們的項目（#1989）。
        assert!(!codex_hook::install_local(&codex_home, &app.data_dir, app.instance().as_deref()).unwrap());
        let before = "# keep me\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
        std::fs::write(codex_home.join("config.toml"), before).unwrap();
        assert_eq!(status_str(probe(&app, &t).await.unwrap()), "not_installed");
        let Json(e) = post_trust(State(app.clone()), Extension(RequestPrincipal::User), Json(TrustBody { host: "local".into(), identity: None })).await.unwrap();
        assert_eq!(e.status, "trusted");
        let after = std::fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(after.starts_with(before), "everything the user wrote stays byte-identical");
        assert!(after.contains("trusted_hash"));
        let backups: Vec<_> = std::fs::read_dir(&codex_home).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with("config.toml.am-bak-")).collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), before);
        let bot = post_trust(State(app), Extension(RequestPrincipal::Bot("b".into())), Json(TrustBody { host: "local".into(), identity: None })).await;
        assert!(matches!(bot, Err(LcError::Forbidden(_))));
    }
}
