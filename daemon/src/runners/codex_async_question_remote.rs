//! Remote Codex rollout tails: bounded SSH, 20-second positive/negative cache, single flight.
use crate::codex_async_question::{pending_async_questions, AsyncQuestion, TAIL_BYTES};
use crate::{db, state::App};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const READ_TIMEOUT: Duration = Duration::from_secs(2);
const FRESH_FOR: Duration = Duration::from_secs(20);
const MAX_SLOTS: usize = 256;
type Value = Option<Vec<AsyncQuestion>>;
type Slot = Arc<tokio::sync::Mutex<Option<(Instant, Value)>>>;

fn cache() -> &'static Mutex<HashMap<String, Slot>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn slot(key: &str) -> Option<Slot> {
    let mut slots = cache().lock().unwrap_or_else(|e| e.into_inner());
    if slots.len() >= MAX_SLOTS {
        slots.retain(|_, s| Arc::strong_count(s) > 1 || s.try_lock().map(|g| g.as_ref().is_some_and(|(at, _)| at.elapsed() < FRESH_FOR)).unwrap_or(true));
        if slots.len() >= MAX_SLOTS && !slots.contains_key(key) {
            return None;
        }
    }
    Some(slots.entry(key.to_string()).or_default().clone())
}

#[cfg(test)]
pub(super) fn age_cache(run: &str, age: Duration) {
    for (key, slot) in cache().lock().unwrap().iter() {
        if key.starts_with(&format!("{run}\0")) {
            if let Some((at, _)) = slot.try_lock().unwrap().as_mut() {
                *at = Instant::now() - age;
            }
        }
    }
}

fn script(home: &str, session: &str) -> String {
    use crate::hosts::sh_quote;
    format!("python3 -c {} {} {} {}", sh_quote(include_str!("codex_async_question_tail.py")), sh_quote(home), sh_quote(session), sh_quote(&TAIL_BYTES.to_string()))
}

async fn home_setting(app: &Arc<App>, bot: &db::Bot, host: &str) -> String {
    let mut home = None;
    if let Some(name) = bot.identity.as_deref().filter(|n| !n.is_empty()) {
        home = crate::tools::identity_for_host(app, host, name).await.and_then(|id| id.env.get("CODEX_HOME").cloned());
    }
    if let Some(value) = bot.env().get("CODEX_HOME") {
        home = Some(value.clone());
    }
    // The reader expands ~ and $HOME on the target. No extra SSH just to discover HOME.
    home.unwrap_or_else(|| "~/.codex".into())
}

async fn current(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str, session: &str) -> bool {
    let Ok(Some(now)) = db::run(&app.db, &run.id).await else { return false };
    if !matches!(now.state.as_str(), "starting" | "running") || now.started_at != run.started_at || now.pane_id != run.pane_id || now.bot_id != bot.id {
        return false;
    }
    if !matches!(db::bot_host(&app.db, &bot.id).await, Ok(h) if h == host) {
        return false;
    }
    let Some(client) = app.herdr_for_run(run).await else { return false };
    let Some(pane) = run.pane_id.as_deref() else { return false };
    // Preserve #1879's DB fallback when herdr has no session, but reject a changed live session.
    super::codex_async_question::live_session(&client, pane).await
        .or_else(|| now.native_session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
        .as_deref() == Some(session)
}

pub async fn pending(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str, session: &str) -> Value {
    // Includes waiting for a concurrent reader, identity lookup, SSH, parsing, and validation.
    tokio::time::timeout(READ_TIMEOUT, pending_inner(app, bot, run, host, session)).await.ok().flatten()
}

async fn pending_inner(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str, session: &str) -> Value {
    if session.is_empty() || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let fence = app.hosts.fence(host).await?;
    if !fence.conn().is_connected() || !current(app, bot, run, host, session).await {
        return None;
    }
    let home = home_setting(app, bot, host).await;
    let key = format!("{}\0{}\0{:?}\0{}\0{session}\0{home}", run.id, run.started_at, run.pane_id, fence.authority_scope());
    let slot = slot(&key)?;
    let mut guard = slot.lock().await;
    let value = if let Some((_, value)) = guard.as_ref().filter(|(at, _)| at.elapsed() < FRESH_FOR) {
        value.clone()
    } else {
        // Cache failure too. Keep the outer deadline slightly longer than the transport deadline
        // so a stalled SSH finishes here and its failure is stored, rather than retrying every poll.
        let result = tokio::time::timeout(Duration::from_millis(1500), fence.conn().ssh_exec_path_timeout(&script(&home, session), Duration::from_secs(1))).await;
        let value = match result {
            Ok(Ok(log)) if !log.is_empty() && log.len() <= TAIL_BYTES as usize => {
                tokio::task::spawn_blocking(move || pending_async_questions(&log)).await.ok()
            }
            _ => None,
        };
        *guard = Some((Instant::now(), value.clone()));
        value
    };
    drop(guard);
    if !app.hosts.is_current(&fence).await || !current(app, bot, run, host, session).await {
        return None;
    }
    value
}

fn observations() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

struct Observation(String);
impl Drop for Observation {
    fn drop(&mut self) {
        observations().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// Do not hold the sequential patrol (or a bot lock) while the remote host responds.
pub fn observe(app: &Arc<App>, bot: &db::Bot, run: &db::Run) {
    if !observations().lock().unwrap_or_else(|e| e.into_inner()).insert(run.id.clone()) {
        return;
    }
    let (app, bot, run) = (app.clone(), bot.clone(), run.clone());
    let active = Observation(run.id.clone());
    tokio::spawn(Box::pin(async move {
        let _active = active;
        super::codex_async_question::observe_now(&app, &bot, &run).await;
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_local_rollout_reader_bounds_the_tail_and_rejects_escape_paths() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-async-tail-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        // Run the exact embedded reader, including remote HOME expansion and canonical guards.
        let program = format!("{}\n{}", include_str!("codex_async_question_tail.py").split("if __name__").next().unwrap(), r#"
import tempfile
with tempfile.TemporaryDirectory(dir=sys.argv[1]) as tmp:
    home = Path(tmp) / 'home'
    day = home / 'sessions/2026/10/22'
    day.mkdir(parents=True)
    os.environ['HOME'] = str(home.parent)
    old = home / 'sessions/2025/01/01'
    old.mkdir(parents=True)
    (old / 'rollout-old-ancient.jsonl').write_bytes(b'old')
    first = day / 'rollout-first-live.jsonl'
    second = day / 'rollout-second-live.jsonl'
    first.write_bytes(b'first')
    second.write_bytes(b'x' * (8 * 1024 * 1024) + b'END')
    os.utime(first, (1, 1))
    os.utime(second, (2, 2))
    assert tail(str(home), 'live', 8 * 1024 * 1024).endswith(b'END')
    assert len(tail(str(home), 'live', 16 * 1024 * 1024)) == 8 * 1024 * 1024
    assert tail('$HOME/home', 'live', 3) == b'END'
    assert tail('~/home', 'ancient', 10) == b'old'
    assert tail(str(home), '../live', 10) == b''
    assert tail('relative', 'live', 10) == b''
    outside = Path(tmp) / 'private'
    outside.write_bytes(b'secret')
    (day / 'rollout-out-escape.jsonl').symlink_to(outside)
    assert tail(str(home), 'escape', 100) == b''
    linked = Path(tmp) / 'linked'
    linked.mkdir()
    (linked / 'sessions').symlink_to(home / 'sessions')
    assert tail(str(linked), 'live', 10) == b''
    foreign = Path(tmp) / 'foreign'
    foreign_day = foreign / '01/01'
    foreign_day.mkdir(parents=True)
    (foreign_day / 'rollout-out-secret.jsonl').write_bytes(b'secret')
    (home / 'sessions/2027').symlink_to(foreign)
    assert tail(str(home), 'secret', 10) == b''
"#);
        let out = std::process::Command::new("python3").arg("-c").arg(program).arg(&dir).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    #[tokio::test]
    async fn remote_home_uses_that_hosts_identity_then_bot_override() {
        let env = crate::testing::env().await;
        let mut bot = crate::testing::claude_bot(&env.app, &env.project_id, "home-setting").await;
        bot.identity = Some("cx-test".into());
        env.app.cfg.update(|c| {
            c.identities = vec![crate::config::IdentityCfg {
                name: "cx-test".into(), kind: "codex".into(), host: Some("m4p-test".into()),
                env: [("CODEX_HOME".into(), "$HOME/.codex-remote".into())].into(), args: vec![],
            }];
            Ok(())
        }).await.unwrap();
        assert_eq!(home_setting(&env.app, &bot, "m4p-test").await, "$HOME/.codex-remote");
        assert_eq!(home_setting(&env.app, &bot, "another-host").await, "~/.codex");
        bot.env_json = r#"{"CODEX_HOME":"~/bot override's home"}"#.into();
        assert_eq!(home_setting(&env.app, &bot, "m4p-test").await, "~/bot override's home");
        assert!(script("~/bot override's home", "safe-session").contains("'\\''"));
    }
}
