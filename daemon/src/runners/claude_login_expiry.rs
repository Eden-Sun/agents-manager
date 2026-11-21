use crate::config::LOCAL_HOST;
use crate::hosts::HostFence;
use crate::state::App;
use crate::webpush;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const EXPIRY_POLL: Duration = Duration::from_secs(30 * 60);
const DAY_MS: i64 = 86_400_000;
const THREE_DAYS_MS: i64 = 3 * DAY_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryStage {
    ThreeDays,
    OneDay,
    Expired,
}

impl ExpiryStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ThreeDays => "3d",
            Self::OneDay => "1d",
            Self::Expired => "expired",
        }
    }
}

pub fn parse_refresh_token_expires_at(raw: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let n = v.pointer("/claudeAiOauth/refreshTokenExpiresAt")?;
    let ms = n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite() && *f > 0.0).map(|f| f as i64))?;
    (ms > 0).then_some(ms)
}

pub fn expiry_stage(expires_at_ms: i64, now_ms: i64) -> Option<ExpiryStage> {
    let rem = expires_at_ms.saturating_sub(now_ms);
    if rem <= 0 {
        Some(ExpiryStage::Expired)
    } else if rem < DAY_MS {
        Some(ExpiryStage::OneDay)
    } else if rem < THREE_DAYS_MS {
        Some(ExpiryStage::ThreeDays)
    } else {
        None
    }
}

pub fn read_local_expiry(config_dir: &Path) -> Option<i64> {
    let raw = std::fs::read_to_string(config_dir.join(".credentials.json")).ok()?;
    parse_refresh_token_expires_at(&raw)
}

async fn read_remote_expiry(conn: &Arc<crate::hosts::HostConn>, config_dir: &str) -> Option<i64> {
    let quoted_dir = crate::hosts::sh_quote(config_dir);
    let script = format!(
        "DIR={quoted_dir}\n\
         FILE=\"$DIR/.credentials.json\"\n\
         [ -r \"$FILE\" ] || exit 0\n\
         if command -v python3 >/dev/null 2>&1; then\n\
           python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); v=(d.get(\"claudeAiOauth\") or {{}}).get(\"refreshTokenExpiresAt\"); print(int(v) if isinstance(v,(int,float)) and v>0 else \"\")' \"$FILE\" 2>/dev/null || true\n\
         elif command -v jq >/dev/null 2>&1; then\n\
           jq -r '.claudeAiOauth.refreshTokenExpiresAt // empty | floor' \"$FILE\" 2>/dev/null || true\n\
         fi"
    );
    let out = conn.ssh_exec_timeout(&script, Duration::from_secs(10)).await.ok()?;
    out.trim().parse::<i64>().ok().filter(|&n| n > 0)
}

fn last_checked() -> &'static Mutex<HashMap<String, Instant>> {
    static MAP: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

pub fn invalidate_host(host: &str) {
    if let Ok(mut g) = last_checked().lock() {
        g.remove(host);
    }
}

pub async fn refresh_host_if_due(app: &Arc<App>, host: &str) {
    let due = {
        let mut g = last_checked().lock().unwrap_or_else(|e| e.into_inner());
        let ok = g.get(host).is_none_or(|at| at.elapsed() >= EXPIRY_POLL);
        if ok {
            g.insert(host.to_string(), Instant::now());
        }
        ok
    };
    if due {
        refresh_host(app, host).await;
    }
}

pub async fn refresh_host(app: &Arc<App>, host: &str) {
    let Some(fence) = app.hosts.fence(host).await else { return };
    let Ok(home) = crate::hosts::home_for_fence(&fence).await else { return };
    if !app.hosts.is_current(&fence).await {
        return;
    }
    let identities = crate::tools::identities_for_host(app, host).await;
    let mut dirs: BTreeMap<String, String> = BTreeMap::new();
    dirs.insert("cc0".to_string(), format!("{home}/.claude"));
    for idn in identities {
        if idn.kind != "claude" {
            continue;
        }
        let dir = idn
            .env
            .get("CLAUDE_CONFIG_DIR")
            .map(|v| crate::config::expand_home(v, &home))
            .unwrap_or_else(|| format!("{home}/.claude"));
        dirs.insert(idn.name, dir);
    }

    let conn = fence.conn().clone();
    let is_local = host == LOCAL_HOST;
    let mut next: BTreeMap<String, i64> = BTreeMap::new();
    for (account, dir) in dirs {
        let ts = if is_local {
            let p = std::path::PathBuf::from(&dir);
            tokio::task::spawn_blocking(move || read_local_expiry(&p)).await.ok().flatten()
        } else {
            read_remote_expiry(&conn, &dir).await
        };
        if let Some(ms) = ts {
            next.insert(account, ms);
        }
    }
    apply_host_expiry(app, host, &fence, next).await;
}

async fn apply_host_expiry(app: &Arc<App>, host: &str, fence: &HostFence, next: BTreeMap<String, i64>) {
    if !app.hosts.is_current(fence).await {
        return;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    for (account, &expires_at) in &next {
        match expiry_stage(expires_at, now_ms) {
            Some(stage) => webpush::spawn_login_expiry_push(app, host, account, stage.as_str(), expires_at),
            None => webpush::clear_login_expiry_push(host, account),
        }
    }
    let changed = {
        let mut g = app.claude_login_expiry.lock().unwrap_or_else(|e| e.into_inner());
        let prev = g.get(host).cloned().unwrap_or_default();
        if prev == next {
            false
        } else {
            if next.is_empty() {
                g.remove(host);
            } else {
                g.insert(host.to_string(), next);
            }
            true
        }
    };
    if changed {
        crate::state::emit_host_changed(app, fence).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_refresh_token_expires_at_and_ignores_missing() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"sk-ant-secret","refreshToken":"rt-secret","expiresAt":1700000000000,"refreshTokenExpiresAt":1795363200000}}"#;
        assert_eq!(parse_refresh_token_expires_at(raw), Some(1795363200000));
        assert_eq!(parse_refresh_token_expires_at(r#"{"claudeAiOauth":{"expiresAt":1700000000000}}"#), None);
        assert_eq!(parse_refresh_token_expires_at("not json"), None);
        assert_eq!(parse_refresh_token_expires_at(r#"{"claudeAiOauth":{"refreshTokenExpiresAt":0}}"#), None);
    }

    #[test]
    fn stages_match_three_days_one_day_and_expired_thresholds() {
        let now = 1_700_000_000_000i64;
        assert_eq!(expiry_stage(now + 4 * DAY_MS, now), None);
        assert_eq!(expiry_stage(now + THREE_DAYS_MS, now), None);
        assert_eq!(expiry_stage(now + THREE_DAYS_MS - 1, now), Some(ExpiryStage::ThreeDays));
        assert_eq!(expiry_stage(now + DAY_MS, now), Some(ExpiryStage::ThreeDays));
        assert_eq!(expiry_stage(now + DAY_MS - 1, now), Some(ExpiryStage::OneDay));
        assert_eq!(expiry_stage(now + 1, now), Some(ExpiryStage::OneDay));
        assert_eq!(expiry_stage(now, now), Some(ExpiryStage::Expired));
        assert_eq!(expiry_stage(now - 10_000, now), Some(ExpiryStage::Expired));
    }

    #[test]
    fn missing_or_unreadable_credentials_file_returns_none() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-claude-cred-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_local_expiry(&dir), None);
        std::fs::write(dir.join(".credentials.json"), r#"{"claudeAiOauth":{"expiresAt":123}}"#).unwrap();
        assert_eq!(read_local_expiry(&dir), None);
        std::fs::write(dir.join(".credentials.json"), r#"{"claudeAiOauth":{"refreshTokenExpiresAt":1795363200000}}"#).unwrap();
        assert_eq!(read_local_expiry(&dir), Some(1795363200000));
    }
}
