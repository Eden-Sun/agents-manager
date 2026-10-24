//! Remote Codex rollout tails: bounded SSH, 20-second positive/negative cache, single flight.
use crate::codex_async_question::{pending_async_questions, AsyncQuestion, TAIL_BYTES};
use crate::{db, state::App};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// context 讀數的 SSH 上限：第一次可能讀到 8 MiB 檔尾，2 秒不夠；async 提問仍是 [`READ_TIMEOUT`]，不放寬。
const CONTEXT_READ_TIMEOUT: Duration = Duration::from_secs(5);
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

/// 增量版：tail.py 的 `offset` 參數（選用，不帶就是上面的 `script`）。
fn delta_script(home: &str, session: &str, offset: u64) -> String {
    use crate::hosts::sh_quote;
    format!("{} {}", script(home, session), sh_quote(&offset.to_string()))
}

/// tail.py 增量輸出：`AM_ROLLOUT <next>\n` 後接整行；缺表頭、位移不是數字、文字超過上限＝`None`。
fn parse_delta(out: &str) -> Option<am_ports::RemoteRollout> {
    let (header, text) = out.strip_prefix("AM_ROLLOUT ")?.split_once('\n')?;
    let next = header.parse::<u64>().ok()?;
    (text.len() <= TAIL_BYTES as usize).then(|| am_ports::RemoteRollout { text: text.to_string(), next })
}

/// Codex context 讀數（prompt_cache）的遠端增量讀取。**不經**上面的 20 秒快取與 single-flight：
/// 那是 async 提問的 key 空間；context 的節流在 prompt_cache 自己做（[`crate::prompt_cache::REMOTE_EVERY`]）。
/// `offset` `None` 從 0 讀，檔案比上限大時就是檔尾（同 tail.py）。失敗、逾時、身分不符都回 `None`。
pub async fn rollout_read(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str, session: &str, offset: Option<u64>) -> Option<am_ports::RemoteRollout> {
    if session.is_empty() || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let deadline = tokio::time::Instant::now() + CONTEXT_READ_TIMEOUT;
    let fence = tokio::time::timeout_at(deadline, app.hosts.fence(host)).await.ok()??;
    if !fence.conn().is_connected() || !current(app, bot, run, host, session).await {
        return None;
    }
    let home = home_setting(app, bot, run, host).await?;
    let out = tokio::time::timeout_at(deadline, fence.conn().ssh_exec_path_timeout(&delta_script(&home, session, offset.unwrap_or(0)), CONTEXT_READ_TIMEOUT)).await.ok()?.ok()?;
    if !app.hosts.is_current(&fence).await || !current(app, bot, run, host, session).await {
        return None;
    }
    parse_delta(&out)
}

pub async fn home_setting(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str) -> Option<String> {
    // Identity changes apply at restart. Reconstruct the launch identity before comparing the
    // launch revision; there is no persisted launch env, so an unprovable env must degrade.
    let mut launched = bot.clone();
    launched.identity = run.started_identity().unwrap_or_else(|| bot.identity.clone());
    if run.launch_rev.as_deref() != Some(crate::launch_rev::of(&launched).as_str()) {
        return None;
    }
    let mut home = None;
    if let Some(name) = launched.identity.as_deref().filter(|n| !n.is_empty()) {
        home = crate::tools::identity_for_host(app, host, name).await?.env.get("CODEX_HOME").cloned();
    }
    if let Some(value) = bot.env().get("CODEX_HOME") {
        home = Some(value.clone());
    }
    // The reader expands ~ and $HOME on the target. No extra SSH just to discover HOME.
    Some(home.unwrap_or_else(|| "~/.codex".into()))
}

pub async fn current(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str, session: &str) -> bool {
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

/// Start a negative cache entry before polling any work. Cancellation drops the lock but
/// leaves this entry intact, including when a caller drops the entire pending-question future.
async fn read_through<F, Fut>(slot: Slot, deadline: tokio::time::Instant, fetch: F) -> Value
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Value>,
{
    let mut guard = tokio::time::timeout_at(deadline, slot.lock()).await.ok()?;
    if let Some((_, value)) = guard.as_ref().filter(|(at, _)| at.elapsed() < FRESH_FOR) {
        return value.clone();
    }
    *guard = Some((Instant::now(), None));
    let value = tokio::time::timeout_at(deadline, fetch()).await.ok().flatten();
    *guard = Some((Instant::now(), value.clone()));
    value
}

pub async fn pending(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str, session: &str) -> Value {
    let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
    if session.is_empty() || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let fence = tokio::time::timeout_at(deadline, app.hosts.fence(host)).await.ok()??;
    // Mutable bot settings must not bypass the per-run/session failure backoff.
    let key = format!("{}\0{}\0{:?}\0{}\0{session}", run.id, run.started_at, run.pane_id, fence.authority_scope());
    let slot = slot(&key)?;
    let value = read_through(slot, deadline, || async {
        if !fence.conn().is_connected() || !current(app, bot, run, host, session).await {
            return None;
        }
        let home = home_setting(app, bot, run, host).await?;
        let log = fence.conn().ssh_exec_path_timeout(&script(&home, session), Duration::from_secs(1)).await.ok()?;
        if log.is_empty() || log.len() > TAIL_BYTES as usize {
            return None;
        }
        let value = tokio::task::spawn_blocking(move || pending_async_questions(&log)).await.ok()?;
        if !app.hosts.is_current(&fence).await || !current(app, bot, run, host, session).await {
            return None;
        }
        Some(value)
    }).await;
    // Cached tails also need the current run/session/host observation. Include this in the
    // original deadline so validating a cached value cannot extend the read budget.
    tokio::time::timeout_at(deadline, async {
        if app.hosts.is_current(&fence).await && current(app, bot, run, host, session).await {
            value
        } else {
            None
        }
    }).await.ok().flatten()
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

    #[tokio::test(start_paused = true)]
    async fn outer_deadline_and_caller_cancellation_leave_negative_cache_before_ssh_finishes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let slot: Slot = Default::default();
        let calls = AtomicUsize::new(0);
        let value = read_through(slot.clone(), tokio::time::Instant::now() + Duration::from_millis(80), || async {
            // Front work consumes the budget before SSH starts. The transport deadline would
            // be later than the outer deadline, reproducing the review's cancellation ordering.
            tokio::time::sleep(Duration::from_millis(50)).await;
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(1)).await;
            Some(vec![])
        }).await;
        assert_eq!(value, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(read_through(slot.clone(), tokio::time::Instant::now() + READ_TIMEOUT, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(vec![])
        }).await, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "an outer timeout must back off");

        let cancelled: Slot = Default::default();
        let (started, ready) = tokio::sync::oneshot::channel();
        let work = cancelled.clone();
        let task = tokio::spawn(async move {
            read_through(work, tokio::time::Instant::now() + READ_TIMEOUT, || async {
                started.send(()).unwrap();
                std::future::pending::<Value>().await
            }).await
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(read_through(cancelled, tokio::time::Instant::now() + READ_TIMEOUT, || async {
            panic!("a dropped caller must leave its negative entry");
        }).await, None);
    }

    #[tokio::test]
    async fn a_run_started_without_an_identity_does_not_switch_to_the_new_configured_identity() {
        let env = crate::testing::env().await;
        let mut bot = crate::testing::claude_bot(&env.app, &env.project_id, "default-home").await;
        bot.identity = None;
        let id = crate::testing::fake_run(&env.app, &bot.id).await;
        let mut run = db::run(&env.app.db, &id).await.unwrap().unwrap();
        run.runtime_identity = Some(String::new());
        run.launch_rev = Some(crate::launch_rev::of(&bot));
        bot.identity = Some("new-identity".into());
        assert_eq!(home_setting(&env.app, &bot, &run, "m4p-test").await.as_deref(), Some("~/.codex"));
        run.launch_rev = None;
        assert_eq!(home_setting(&env.app, &bot, &run, "m4p-test").await, None);
    }

    #[tokio::test]
    async fn remote_home_uses_that_hosts_identity_then_bot_override() {
        let env = crate::testing::env().await;
        let mut bot = crate::testing::claude_bot(&env.app, &env.project_id, "home-setting").await;
        bot.identity = Some("cx-test".into());
        let id = crate::testing::fake_run(&env.app, &bot.id).await;
        let mut run = db::run(&env.app.db, &id).await.unwrap().unwrap();
        run.runtime_identity = Some("cx-test".into());
        run.launch_rev = Some(crate::launch_rev::of(&bot));
        env.app.cfg.update(|c| {
            c.identities = vec![crate::config::IdentityCfg {
                name: "cx-test".into(), kind: "codex".into(), host: Some("m4p-test".into()),
                env: [("CODEX_HOME".into(), "$HOME/.codex-remote".into())].into(), args: vec![],
            }];
            Ok(())
        }).await.unwrap();
        assert_eq!(home_setting(&env.app, &bot, &run, "m4p-test").await.as_deref(), Some("$HOME/.codex-remote"));
        assert_eq!(home_setting(&env.app, &bot, &run, "another-host").await, None);
        bot.env_json = r#"{"CODEX_HOME":"~/bot override's home"}"#.into();
        assert_eq!(home_setting(&env.app, &bot, &run, "m4p-test").await, None, "changed env has no proven launch snapshot");
        run.launch_rev = Some(crate::launch_rev::of(&bot));
        assert_eq!(home_setting(&env.app, &bot, &run, "m4p-test").await.as_deref(), Some("~/bot override's home"));
        assert!(script("~/bot override's home", "safe-session").contains("'\\''"));
    }
}

/// tail.py 的增量路徑（`offset`）：用假 ssh（把 script 交給本機 `sh -c` 執行）實跑同一支 python，不連真主機。
#[cfg(test)]
mod delta_tests {
    use super::*;
    use crate::hosts::sh_quote;

    const LINES: &str = "one\ntwo\nthr";

    /// `home/sessions/2026/10/24/rollout-x-abc.jsonl`，回傳 home 路徑。
    fn rollout(body: &[u8]) -> std::path::PathBuf {
        let home = crate::testing::scratch_dir("am-delta-home").join("home");
        let day = home.join("sessions/2026/10/24");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-x-abc.jsonl"), body).unwrap();
        home
    }

    /// 假 ssh：執行遠端會收到的 script，回傳 stdout。
    fn fake_ssh(script: &str) -> String {
        let out = std::process::Command::new("sh").arg("-c").arg(script).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn read(home: &std::path::Path, offset: u64) -> Option<am_ports::RemoteRollout> {
        parse_delta(&fake_ssh(&delta_script(&home.to_string_lossy(), "abc", offset)))
    }

    #[test]
    fn the_delta_reads_whole_lines_from_the_offset_and_resumes_at_next() {
        let home = rollout(LINES.as_bytes());
        // 0：整檔的整行；未結束的 "thr" 不讀、位移停在它前面。
        assert_eq!(read(&home, 0), Some(am_ports::RemoteRollout { text: "one\ntwo\n".into(), next: 8 }));
        // 位移正好在行首：只讀之後的新增（這裡沒有）。
        assert_eq!(read(&home, 8), Some(am_ports::RemoteRollout { text: String::new(), next: 8 }));
        // 位移落在行中：跳到下一個換行之後，不吃半行。
        assert_eq!(read(&home, 2), Some(am_ports::RemoteRollout { text: "two\n".into(), next: 8 }));
        // 新增完整行之後，從上次位移讀到新的 next。
        std::fs::write(home.join("sessions/2026/10/24/rollout-x-abc.jsonl"), "one\ntwo\nthree\n").unwrap();
        assert_eq!(read(&home, 8), Some(am_ports::RemoteRollout { text: "three\n".into(), next: 14 }));
    }

    #[test]
    fn a_shrunk_or_replaced_file_is_read_from_the_start() {
        let home = rollout(b"new-a\nnew-b\n");
        // 位移超過檔案大小（rotate／截斷）：從頭讀，next 會比舊位移小。
        assert_eq!(read(&home, 500), Some(am_ports::RemoteRollout { text: "new-a\nnew-b\n".into(), next: 12 }));
    }

    #[test]
    fn a_delta_never_reads_more_than_the_cap_and_keeps_line_alignment() {
        // 9 MiB 的整行：只取最後 8 MiB（上限），第一個（可能是半行的）片段丟掉。
        let line = format!("{}\n", "x".repeat(99));
        let body = line.repeat(9 * 1024 * 1024 / line.len());
        let home = rollout(body.as_bytes());
        let got = read(&home, 0).unwrap();
        assert!(got.text.len() <= TAIL_BYTES as usize);
        assert!(got.text.starts_with("xxxx") && got.text.ends_with("x\n"));
        assert_eq!(got.next, body.len() as u64, "整檔讀到最後一個換行");
        assert_eq!(got.text.len() % line.len(), 0, "每一段都是完整行");
    }

    #[test]
    fn a_bad_offset_or_a_bad_channel_is_none_not_a_guess() {
        let home = rollout(LINES.as_bytes());
        let home_str = home.to_string_lossy().into_owned();
        // 負數、非數字：python 不輸出；解析不到表頭＝None。
        for bad in ["-1", "1x", "+3", " 3"] {
            let script = format!("{} {}", script(&home_str, "abc"), sh_quote(bad));
            assert_eq!(parse_delta(&fake_ssh(&script)), None, "{bad}");
        }
        // session 不在、路徑逃出 sessions：空輸出＝None（通道失敗，呼叫端保留上一次讀數）。
        assert_eq!(parse_delta(&fake_ssh(&delta_script(&home_str, "nope", 0))), None);
        assert_eq!(parse_delta(&fake_ssh(&delta_script(&home_str, "../abc", 0))), None);
        assert_eq!(parse_delta(""), None);
        assert_eq!(parse_delta("AM_ROLLOUT 8"), None, "沒有換行的表頭不算");
        assert_eq!(parse_delta("AM_ROLLOUT x\none\n"), None, "位移不是數字");
    }

    #[test]
    fn the_legacy_tail_without_offset_is_unchanged() {
        // 不帶 offset：仍是原來的檔尾原文，沒有 AM_ROLLOUT 表頭（async 提問的行為不准變）。
        let home = rollout(LINES.as_bytes());
        let out = fake_ssh(&script(&home.to_string_lossy(), "abc"));
        assert_eq!(out, LINES);
    }
}
