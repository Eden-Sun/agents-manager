use crate::config::LOCAL_HOST;
use crate::hosts::HostFence;
use crate::state::App;
use crate::webpush;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

pub const EXPIRY_POLL_LOCAL: Duration = Duration::from_secs(55);
pub const EXPIRY_POLL_REMOTE: Duration = Duration::from_secs(5 * 60);
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AccountCredCache {
    mtime: Option<String>,
    expires_at: Option<i64>,
}

#[derive(Default)]
struct HostExpiryCache {
    checked_at: Option<Instant>,
    accounts: BTreeMap<String, AccountCredCache>,
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

fn local_cred_mtime(config_dir: &Path) -> Option<String> {
    let modified = std::fs::metadata(config_dir.join(".credentials.json")).ok()?.modified().ok()?;
    let dur = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(dur.as_nanos().to_string())
}

fn host_caches() -> &'static Mutex<HashMap<String, HostExpiryCache>> {
    static MAP: OnceLock<Mutex<HashMap<String, HostExpiryCache>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

pub fn invalidate_host(host: &str) {
    if let Ok(mut g) = host_caches().lock() {
        g.remove(host);
    }
}

fn poll_interval(host: &str) -> Duration {
    if host == LOCAL_HOST {
        EXPIRY_POLL_LOCAL
    } else {
        EXPIRY_POLL_REMOTE
    }
}

pub async fn refresh_host_if_due(app: &Arc<App>, host: &str) {
    let interval = poll_interval(host);
    let due = {
        let mut g = host_caches().lock().unwrap_or_else(|e| e.into_inner());
        let entry = g.entry(host.to_string()).or_default();
        let ok = entry.checked_at.is_none_or(|at| at.elapsed() >= interval);
        if ok {
            entry.checked_at = Some(Instant::now());
        }
        ok
    };
    if due {
        refresh_host_with_policy(app, host, false).await;
    }
}

pub async fn refresh_host_on_probe(app: &Arc<App>, host: &str) {
    {
        let mut g = host_caches().lock().unwrap_or_else(|e| e.into_inner());
        g.entry(host.to_string()).or_default().checked_at = Some(Instant::now());
    }
    refresh_host_with_policy(app, host, false).await;
}

pub async fn refresh_host(app: &Arc<App>, host: &str) {
    {
        let mut g = host_caches().lock().unwrap_or_else(|e| e.into_inner());
        g.entry(host.to_string()).or_default().checked_at = Some(Instant::now());
    }
    refresh_host_with_policy(app, host, true).await;
}

async fn claude_dirs_for_host(app: &Arc<App>, host: &str, home: &str) -> BTreeMap<String, String> {
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
            .map(|v| crate::config::expand_home(v, home))
            .unwrap_or_else(|| format!("{home}/.claude"));
        dirs.insert(idn.name, dir);
    }
    dirs
}

fn cached_accounts(host: &str) -> BTreeMap<String, AccountCredCache> {
    host_caches()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(host)
        .map(|c| c.accounts.clone())
        .unwrap_or_default()
}

fn store_accounts(host: &str, accounts: BTreeMap<String, AccountCredCache>) {
    let mut g = host_caches().lock().unwrap_or_else(|e| e.into_inner());
    let entry = g.entry(host.to_string()).or_default();
    entry.accounts = accounts;
    entry.checked_at = Some(Instant::now());
}

fn collect_local_accounts<F>(
    dirs: &BTreeMap<String, String>,
    prev: &BTreeMap<String, AccountCredCache>,
    force: bool,
    mut read_fn: F,
) -> (BTreeMap<String, AccountCredCache>, bool)
where
    F: FnMut(&Path) -> Option<i64>,
{
    let mut next_cache = BTreeMap::new();
    let mut changed = false;
    for (account, dir) in dirs {
        let p = PathBuf::from(dir);
        let cur_mtime = local_cred_mtime(&p);
        let unchanged = !force
            && prev
                .get(account)
                .is_some_and(|c| c.mtime == cur_mtime);
        let expires_at = if unchanged {
            prev[account].expires_at
        } else {
            changed = true;
            if cur_mtime.is_some() {
                read_fn(&p)
            } else {
                None
            }
        };
        next_cache.insert(
            account.clone(),
            AccountCredCache {
                mtime: cur_mtime,
                expires_at,
            },
        );
    }
    if prev.keys().any(|k| !dirs.contains_key(k)) {
        changed = true;
    }
    (next_cache, changed)
}

fn build_remote_batch_script(
    dirs: &BTreeMap<String, String>,
    prev: &BTreeMap<String, AccountCredCache>,
    force: bool,
) -> String {
    let mut script = String::from("set --");
    for (account, dir) in dirs {
        let prev_mt = if force {
            ""
        } else {
            prev.get(account).and_then(|c| c.mtime.as_deref()).unwrap_or("")
        };
        script.push(' ');
        script.push_str(&crate::hosts::sh_quote(account));
        script.push(' ');
        script.push_str(&crate::hosts::sh_quote(dir));
        script.push(' ');
        script.push_str(&crate::hosts::sh_quote(prev_mt));
    }
    script.push('\n');
    script.push_str(
        "if command -v python3 >/dev/null 2>&1; then\n\
         python3 - \"$@\" <<'PY'\n\
         import json, os, sys\n\
         args = sys.argv[1:]\n\
         for i in range(0, len(args), 3):\n\
             acc, d, prev = args[i], args[i+1], args[i+2]\n\
             path = os.path.join(d, '.credentials.json')\n\
             try:\n\
                 mt = str(os.stat(path).st_mtime_ns)\n\
             except OSError:\n\
                 print(f'{acc}\\t\\tNONE')\n\
                 continue\n\
             if prev and mt == prev:\n\
                 print(f'{acc}\\t{mt}\\tKEEP')\n\
                 continue\n\
             val = ''\n\
             try:\n\
                 with open(path) as f:\n\
                     data = json.load(f)\n\
                 v = (data.get('claudeAiOauth') or {}).get('refreshTokenExpiresAt')\n\
                 if isinstance(v, (int, float)) and v > 0:\n\
                     val = str(int(v))\n\
             except Exception:\n\
                 pass\n\
             if val:\n\
                 print(f'{acc}\\t{mt}\\tSET\\t{val}')\n\
             else:\n\
                 print(f'{acc}\\t{mt}\\tNONE')\n\
         PY\n\
         exit 0\n\
         fi\n\
         check_one() {\n\
           ACC=\"$1\"\n\
           FILE=\"$2/.credentials.json\"\n\
           PREV=\"$3\"\n\
           if [ ! -r \"$FILE\" ]; then\n\
             printf '%s\\t\\tNONE\\n' \"$ACC\"\n\
             return\n\
           fi\n\
           MT=$(stat -c '%Y' \"$FILE\" 2>/dev/null || stat -f '%m' \"$FILE\" 2>/dev/null || true)\n\
           if [ -n \"$PREV\" ] && [ \"$MT\" = \"$PREV\" ]; then\n\
             printf '%s\\t%s\\tKEEP\\n' \"$ACC\" \"$MT\"\n\
             return\n\
           fi\n\
           VAL=\"\"\n\
           if command -v jq >/dev/null 2>&1; then\n\
             VAL=$(jq -r '.claudeAiOauth.refreshTokenExpiresAt // empty | floor' \"$FILE\" 2>/dev/null || true)\n\
           fi\n\
           if [ -n \"$VAL\" ]; then\n\
             printf '%s\\t%s\\tSET\\t%s\\n' \"$ACC\" \"$MT\" \"$VAL\"\n\
           else\n\
             printf '%s\\t%s\\tNONE\\n' \"$ACC\" \"$MT\"\n\
           fi\n\
         }\n\
         while [ $# -ge 3 ]; do\n\
           check_one \"$1\" \"$2\" \"$3\"\n\
           shift 3\n\
         done\n",
    );
    script
}

fn parse_remote_batch_output(
    out: &str,
    dirs: &BTreeMap<String, String>,
    prev: &BTreeMap<String, AccountCredCache>,
) -> Option<BTreeMap<String, AccountCredCache>> {
    let mut parsed: BTreeMap<String, AccountCredCache> = BTreeMap::new();
    for line in out.lines() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let account = parts[0];
        if !dirs.contains_key(account) {
            continue;
        }
        let mtime = (!parts[1].is_empty()).then(|| parts[1].to_string());
        let expires_at = match parts[2] {
            "KEEP" => prev.get(account).and_then(|c| c.expires_at),
            "SET" => parts.get(3).and_then(|s| s.trim().parse::<i64>().ok()).filter(|&n| n > 0),
            "NONE" => None,
            _ => continue,
        };
        parsed.insert(
            account.to_string(),
            AccountCredCache { mtime, expires_at },
        );
    }
    if parsed.len() != dirs.len() {
        return None;
    }
    Some(parsed)
}

async fn read_remote_accounts(
    conn: &Arc<crate::hosts::HostConn>,
    dirs: &BTreeMap<String, String>,
    prev: &BTreeMap<String, AccountCredCache>,
    force: bool,
) -> Option<BTreeMap<String, AccountCredCache>> {
    if dirs.is_empty() {
        return Some(BTreeMap::new());
    }
    let script = build_remote_batch_script(dirs, prev, force);
    let out = conn.ssh_exec_timeout(&script, Duration::from_secs(10)).await.ok()?;
    parse_remote_batch_output(&out, dirs, prev)
}

fn expiries_from_cache(cache: &BTreeMap<String, AccountCredCache>) -> BTreeMap<String, i64> {
    cache
        .iter()
        .filter_map(|(k, v)| v.expires_at.map(|ms| (k.clone(), ms)))
        .collect()
}

async fn refresh_host_with_policy(app: &Arc<App>, host: &str, force: bool) {
    let Some(fence) = app.hosts.fence(host).await else { return };
    let Ok(home) = crate::hosts::home_for_fence(&fence).await else { return };
    if !app.hosts.is_current(&fence).await {
        return;
    }
    let dirs = claude_dirs_for_host(app, host, &home).await;
    let prev = cached_accounts(host);

    let next_cache = if host == LOCAL_HOST {
        let prev_clone = prev.clone();
        let Ok((cache, _)) = tokio::task::spawn_blocking(move || {
            collect_local_accounts(&dirs, &prev_clone, force, read_local_expiry)
        })
        .await
        else {
            return;
        };
        cache
    } else {
        let conn = fence.conn().clone();
        let Some(cache) = read_remote_accounts(&conn, &dirs, &prev, force).await else {
            return;
        };
        cache
    };

    if !app.hosts.is_current(&fence).await {
        return;
    }
    store_accounts(host, next_cache.clone());
    let next = expiries_from_cache(&next_cache);
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
        for account in prev.keys() {
            if !next.contains_key(account) {
                webpush::clear_login_expiry_push(host, account);
            }
        }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

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

    async fn next_host_changed(
        rx: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>,
    ) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        loop {
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                return None;
            }
            match tokio::time::timeout(rem, rx.recv()).await {
                Ok(Ok(ev)) if ev.kind == "host_changed" => return Some(ev.data),
                Ok(Ok(_)) => continue,
                _ => return None,
            }
        }
    }

    #[tokio::test]
    async fn mtime_unchanged_skips_reread_and_mtime_change_triggers_reread_and_event() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let fence = app.hosts.fence(LOCAL_HOST).await.unwrap();
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-claude-mtime-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let cred_path = dir.join(".credentials.json");

        let now_ms = chrono::Utc::now().timestamp_millis();
        let near_exp = now_ms + 12 * 3600 * 1000;
        let fresh_exp = now_ms + 30 * DAY_MS;

        std::fs::write(&cred_path, format!(r#"{{"claudeAiOauth":{{"refreshTokenExpiresAt":{near_exp}}}}}"#)).unwrap();
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        std::fs::OpenOptions::new().write(true).open(&cred_path).unwrap().set_modified(t1).unwrap();

        let mut dirs = BTreeMap::new();
        dirs.insert("cc0".to_string(), dir.display().to_string());

        let reads = Arc::new(AtomicUsize::new(0));
        let mut rx = app.subscribe();

        let (cache1, changed1) = collect_local_accounts(&dirs, &BTreeMap::new(), false, |p| {
            reads.fetch_add(1, Ordering::SeqCst);
            read_local_expiry(p)
        });
        assert!(changed1);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        apply_host_expiry(&app, LOCAL_HOST, &fence, expiries_from_cache(&cache1)).await;

        let ev1 = next_host_changed(&mut rx).await.unwrap();
        assert_eq!(ev1["claude_login_expires_at"]["cc0"], near_exp);
        assert_eq!(
            app.claude_login_expiry.lock().unwrap().get(LOCAL_HOST).and_then(|m| m.get("cc0")).copied(),
            Some(near_exp)
        );

        let (cache2, changed2) = collect_local_accounts(&dirs, &cache1, false, |p| {
            reads.fetch_add(1, Ordering::SeqCst);
            read_local_expiry(p)
        });
        assert!(!changed2);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        apply_host_expiry(&app, LOCAL_HOST, &fence, expiries_from_cache(&cache2)).await;
        assert!(next_host_changed(&mut rx).await.is_none());

        std::fs::write(&cred_path, format!(r#"{{"claudeAiOauth":{{"refreshTokenExpiresAt":{fresh_exp}}}}}"#)).unwrap();
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_120);
        std::fs::OpenOptions::new().write(true).open(&cred_path).unwrap().set_modified(t2).unwrap();

        let (cache3, changed3) = collect_local_accounts(&dirs, &cache2, false, |p| {
            reads.fetch_add(1, Ordering::SeqCst);
            read_local_expiry(p)
        });
        assert!(changed3);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        apply_host_expiry(&app, LOCAL_HOST, &fence, expiries_from_cache(&cache3)).await;

        let ev2 = next_host_changed(&mut rx).await.unwrap();
        assert_eq!(ev2["claude_login_expires_at"]["cc0"], fresh_exp);
        assert_eq!(
            app.claude_login_expiry.lock().unwrap().get(LOCAL_HOST).and_then(|m| m.get("cc0")).copied(),
            Some(fresh_exp)
        );
    }

    #[test]
    fn remote_batch_output_handles_keep_set_and_none() {
        let mut dirs = BTreeMap::new();
        dirs.insert("cc0".to_string(), "/home/u/.claude".to_string());
        dirs.insert("cc1".to_string(), "/home/u/.claude-1".to_string());
        dirs.insert("cc2".to_string(), "/home/u/.claude-2".to_string());

        let mut prev = BTreeMap::new();
        prev.insert(
            "cc0".to_string(),
            AccountCredCache {
                mtime: Some("100".to_string()),
                expires_at: Some(1_795_000_000_000),
            },
        );
        prev.insert(
            "cc1".to_string(),
            AccountCredCache {
                mtime: Some("200".to_string()),
                expires_at: Some(1_795_000_000_000),
            },
        );

        let out = "cc0\t100\tKEEP\ncc1\t250\tSET\t1796000000000\ncc2\t\tNONE\n";
        let parsed = parse_remote_batch_output(out, &dirs, &prev).unwrap();
        assert_eq!(parsed["cc0"].expires_at, Some(1_795_000_000_000));
        assert_eq!(parsed["cc1"].mtime.as_deref(), Some("250"));
        assert_eq!(parsed["cc1"].expires_at, Some(1_796_000_000_000));
        assert_eq!(parsed["cc2"].mtime, None);
        assert_eq!(parsed["cc2"].expires_at, None);
    }
}
