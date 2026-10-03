//! 回合結束、背景工作還在跑（issue #714，SPEC §6.14）。
//!
//! child 把長工作（遠端 cargo）丟到背景就結束回合：agent 真的是 idle（可以收訊息），但使用者看到「閒置」會以為它停了。
//! CLI 自己在畫面底部標著還有幾個背景工作，這裡從既有的畫面巡邏（`update_watch`，30 秒一輪）順便讀出來：
//!
//! - claude（2.1.281 真機）：模式列 `⏵⏵ bypass permissions on · 1 shell · ← for agents`。回合收尾那行的
//!   `done 1:30 PM · 1 shell still running` **不算**：它留在捲動區，背景早就跑完了還在（實測同一個 pane 前天的那行）。
//! - codex（0.157.1 真機）：輸入框上方 `1 background terminal running · /ps to view · /stop to close`；回合中併在
//!   `• Working (8s • esc to interrupt) · 2 background terminals running · …` 那行。`/stop` 之後整行消失。
//!
//! 只看畫面最底下幾行（狀態列、輸入框附近），對話內容裡引用到的同一句不算。數字記在記憶體（`App.background_jobs`）、
//! 以 run 為鍵：屬於這個 process，新 run 自然歸零；daemon 重啟後等下一輪巡邏補上。只改顯示與 API 投影，不動排隊／送 prompt。
//!
//! **常駐服務不算**（2026-09-29 使用者：wits-ops 起了 dev server 就一直標「背景執行中」）：claude 的背景 shell
//! 子樹裡有程序在 listen TCP port，就是 bot 起的服務（`next dev`、`vite`…），工作其實做完了。畫面數到 N > 0 時才去
//! 那台主機看 pane 的行程樹（本機直接讀、遠端走既有 ssh；m4p 真機：`zsh -c source …/shell-snapshots/snapshot-zsh-…`
//! 底下的 node listen `*:3200`），扣掉這種 shell。讀不到就不扣（寧可多標，不要把真的在跑的工作藏起來）。
//!
//! **跑了多久**（issue #774）：claude 2.1.288 起，終端 session 的背景指令不再有時間上限，卡住或忘了收的背景 shell 會讓
//! bot 無限期標著「背景執行中」。帳上同時記這一段背景從什麼時候開始（數字第一次 > 0 那一刻；之後 N 變 M 不重算、歸零才清），
//! 投影帶持續多久；超過 [`STUCK_AFTER_SECS`] 改標「背景工作可能卡住」，也列在 `/api/supervisor/health` 的 `background_stuck`。
//! 只標、不自動殺行程。開始時間在記憶體：daemon 重啟後從重啟後第一次看到算起（下限，不是真的開始時間）。

use crate::db;
use crate::state::App;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 畫面底部看幾個非空行：claude 的模式列在最後一兩行；codex 的背景行上面還有額度警告、輸入框、狀態列、快捷鍵提示。
const BOTTOM_LINES: usize = 8;

/// 背景工作持續超過這麼久就標「可能卡住」（#774）。遠端 cargo 這種正常的長工作一兩個小時就跑完；三小時還標著多半是
/// 卡住或忘了收（例如 `sleep`、`tail -f`、沒 listen port 的 watcher）。
pub const STUCK_AFTER_SECS: i64 = 3 * 3600;

/// 一個 run 的背景帳。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub n: u32,
    /// 這一段背景第一次看到 > 0 的時間（unix 毫秒）；`n == 0` 時一定是 `None`。
    pub since: Option<i64>,
    /// 上一次記帳時算出的「可能卡住」：跨過門檻那一輪要推 `bot_status`，網頁才換字。
    pub stuck: bool,
}

/// `App.background_jobs`：run id → 背景帳（**看過的**都記，含 0；沒有那一列＝巡邏還沒看過它，#767）。
/// 掛在 App 上而不是 process 全域：同一個 process 裡的另一個 App（測試）清自己的帳時不會清到這一份。
pub type Counts = Mutex<HashMap<String, Entry>>;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn stuck_at(since: Option<i64>, now: i64) -> bool {
    since.is_some_and(|t| now - t >= STUCK_AFTER_SECS * 1000)
}

/// 記一個數字（所有寫帳的地方都走這裡）：0→N 記下開始時間，N→M（M > 0）保留，歸零清掉。
/// 回傳投影有沒有變（數字變了、或剛跨過「可能卡住」的門檻），變了呼叫端要推 `bot_status`。
pub fn record(m: &mut HashMap<String, Entry>, run_id: &str, n: u32) -> bool {
    record_at(m, run_id, n, now_ms())
}

fn record_at(m: &mut HashMap<String, Entry>, run_id: &str, n: u32, now: i64) -> bool {
    let prev = m.get(run_id).copied();
    let since = if n == 0 { None } else { prev.and_then(|e| e.since).or(Some(now)) };
    let stuck = stuck_at(since, now);
    m.insert(run_id.to_string(), Entry { n, since, stuck });
    prev.map(|e| (e.n, e.stuck)) != Some((n, stuck))
}

/// 畫面底部標著的背景工作數；沒有＝0。
pub fn parse(kind: &str, screen: &str) -> u32 {
    let bottom: Vec<&str> = screen.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let bottom = &bottom[bottom.len().saturating_sub(BOTTOM_LINES)..];
    match kind {
        "claude" => bottom.iter().rev().find_map(|l| claude_shells(l)).unwrap_or(0),
        "codex" => bottom.iter().rev().find_map(|l| codex_terminals(l)).unwrap_or(0),
        _ => 0,
    }
}

/// 模式列的一段 `N shell(s)`：前面是行首或 `·`，後面是 `·` 或行尾（`N shells still running` 不符）。
fn claude_shells(line: &str) -> Option<u32> {
    line.split('·').map(str::trim).find_map(|seg| {
        let (n, rest) = seg.split_once(' ')?;
        matches!(rest, "shell" | "shells").then(|| n.parse().ok()).flatten()
    })
}

/// `N background terminal(s) running`（可以接在同一行別的片段後面）。
fn codex_terminals(line: &str) -> Option<u32> {
    line.split('·').map(str::trim).find_map(|seg| {
        let (n, rest) = seg.split_once(' ')?;
        matches!(rest, "background terminal running" | "background terminals running").then(|| n.parse().ok()).flatten()
    })
}

#[cfg(test)]
pub fn get(app: &App, run_id: &str) -> u32 {
    known(app, run_id).unwrap_or(0)
}

/// 巡邏看過這個 run 之後的數字；`None`＝還沒看過（daemon 剛重啟、新 run、畫面讀不到）。**沒有證據**：
/// 一鍵重啟不拿它擋人，確認框標「背景狀態未知」（#767）。
pub fn known(app: &App, run_id: &str) -> Option<u32> {
    app.background_jobs.lock().unwrap_or_else(|e| e.into_inner()).get(run_id).map(|e| e.n)
}

/// 這一段背景跑了多久（#774）：`(開始時間 unix 毫秒, 已持續秒數, 可能卡住)`；沒有背景工作或沒看過是 `None`。
pub fn duration(app: &App, run_id: &str) -> Option<(i64, i64, bool)> {
    let since = app.background_jobs.lock().unwrap_or_else(|e| e.into_inner()).get(run_id)?.since?;
    let now = now_ms();
    Some((since, (now - since).max(0) / 1000, stuck_at(Some(since), now)))
}

/// claude 的 Bash 工具（前景與背景都是）：`<shell> -c source ~/.claude/shell-snapshots/snapshot-<shell>-….sh …`。
const CLAUDE_SHELL_MARK: &str = "/.claude/shell-snapshots/snapshot-";

/// `ps -Ao pid=,ppid=,args=` 裡，`pane_shell` 子樹中的 claude 工具 shell（最外層那個）有幾個的子樹含 `listening` 的 pid。
pub fn service_shells(ps: &str, listening: &HashSet<i32>, pane_shell: i32) -> u32 {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut tool_shell: HashSet<i32> = HashSet::new();
    for line in ps.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next().and_then(|p| p.parse().ok()), it.next().and_then(|p| p.parse().ok())) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
        if line.contains(CLAUDE_SHELL_MARK) {
            tool_shell.insert(pid);
        }
    }
    let subtree = |root: i32| {
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() {
            out.extend(children.get(&out[i]).into_iter().flatten().copied());
            i += 1;
        }
        out
    };
    // 從 pane 的 shell 往下走；碰到工具 shell 就結算它、不再往下（它底下的 shell 是它自己的事）。
    let (mut n, mut stack) = (0, vec![pane_shell]);
    while let Some(pid) = stack.pop() {
        for &c in children.get(&pid).into_iter().flatten() {
            if tool_shell.contains(&c) {
                n += u32::from(subtree(c).iter().any(|p| listening.contains(p)));
            } else {
                stack.push(c);
            }
        }
    }
    n
}

/// `lsof -Fp`／`-Fpn` 格式裡的 pid（`p<pid>` 行）。
fn listening_pids(fpn: &str) -> HashSet<i32> {
    fpn.lines().filter_map(|l| l.strip_prefix('p')?.trim().parse().ok()).collect()
}

const PS_ARGS: &str = "ps -Awwo pid=,ppid=,args= 2>/dev/null";
const LISTEN_MARK: &str = "---AM-LISTEN---";

/// 這個 run 的 pane 底下有幾個背景 shell 是常駐服務。讀不到任何一塊都回 0。
pub(crate) async fn services(app: &Arc<App>, run: &db::Run, client: &crate::herdr::HerdrClient, pane: &str) -> u32 {
    let Some(shell) = client.pane_shell(pane).await.ok().and_then(|s| s.shell_pid) else { return 0 };
    let Ok(host) = db::bot_host(&app.db, &run.bot_id).await else { return 0 };
    let Some(conn) = app.hosts.get(&host).await else { return 0 };
    let t = Duration::from_secs(10);
    let (ps, listen) = if conn.is_local() {
        let Ok(o) = crate::local_sh::output(PS_ARGS).await else { return 0 };
        // Linux 讀 /proc、macOS 走 lsof（`linux_proc`）；None＝讀不到。
        let Some(l) = crate::linux_proc::listen_fpn(None, t).await else { return 0 };
        (String::from_utf8_lossy(&o.stdout).into_owned(), l)
    } else {
        // 遠端一趟 ssh 拿兩份。沒有 lsof 的遠端 Linux 只會讀到空的 listen 清單＝不扣。
        let script = format!("{PS_ARGS}; echo '{LISTEN_MARK}'; lsof -nP -iTCP -sTCP:LISTEN -Fp 2>/dev/null; true");
        let Ok(out) = conn.ssh_exec_path_timeout(&script, t).await else { return 0 };
        let Some((ps, l)) = out.split_once(LISTEN_MARK) else { return 0 };
        (ps.to_string(), l.to_string())
    };
    service_shells(&ps, &listening_pids(&listen), shell as i32)
}

/// 巡邏讀到一份畫面：數字變了才記、才推 `bot_status`。
pub async fn observe(app: &Arc<App>, run: &db::Run, kind: &str, screen: &str, client: &crate::herdr::HerdrClient, pane: &str) {
    let raw = parse(kind, screen);
    let mut n = raw;
    if n > 0 && kind == "claude" {
        n = n.saturating_sub(services(app, run, client, pane).await);
    }
    // claude 的 Stop hook 自己報過背景工作（`background_hook.rs`）就以它為準；沒報過（舊版）才是畫面的數字。
    if kind == "claude" {
        n = crate::background_hook::reconcile(app, &run.id, n);
    }
    let changed = {
        let mut m = app.background_jobs.lock().unwrap_or_else(|e| e.into_inner());
        // 0 也記：「看過、乾淨」跟「沒看過」不同（#767）。從沒看過到第一次看過也算變了，要推，前端才把「未知」換掉。
        // 數字沒變、但這一輪跨過「可能卡住」門檻（#774）也算變了。
        record(&mut m, &run.id, n)
    };
    if changed {
        tracing::info!(run = %run.id, bot = %run.bot_id, kind, background_jobs = n, "background jobs changed");
        app.emit_bot_status(&run.bot_id).await;
    }
}

/// 現場讀一次這個 run 的畫面並記帳（一鍵重啟在計畫時與輪到時用）。巡邏每 30 秒才一輪：回合剛結束、背景工作剛丟出去的
/// 那幾秒，帳上是「沒看過」或上一輪的 0，不能拿來當乾淨的證據。讀不到（沒有 pane、主機沒連、herdr 讀失敗）就維持原帳——
/// 沒有新證據不改舊證據。
pub async fn refresh(app: &Arc<App>, run: &db::Run, kind: &str) {
    if run.state != "running" || !matches!(kind, "claude" | "codex") {
        return;
    }
    let Some(pane) = run.pane_id.as_deref() else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    observe(app, run, kind, &read.text, &client, pane).await;
}

/// 這一輪沒看到的 run（結束了）不留帳。
pub fn retain_runs(app: &App, active: &[String]) {
    app.background_jobs.lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
    crate::background_hook::retain_runs(app, active);
}

/// API 的 run 物件加上 `background_jobs`（`GET /api/state` 與 `bot_status` 共用）。
pub fn run_json<T: serde::Serialize>(app: &App, run: &Option<T>, run_id: Option<&str>) -> Value {
    let mut v = serde_json::to_value(run).unwrap_or(Value::Null);
    if let (Some(o), Some(id)) = (v.as_object_mut(), run_id) {
        // 沒觀察過是 `null`，不是 0（#767）。
        o.insert("background_jobs".into(), known(app, id).into());
        // #774：這一段背景從什麼時候開始、跑了多久、是不是可能卡住；沒有背景工作（或沒看過）是 null／false。
        let d = duration(app, id);
        let since = d.and_then(|(t, _, _)| chrono::DateTime::from_timestamp_millis(t));
        o.insert("background_since".into(), since.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).into());
        o.insert("background_secs".into(), d.map(|(_, s, _)| s).into());
        o.insert("background_stuck".into(), d.is_some_and(|(_, _, s)| s).into());
        // claude 的 Stop hook 報的明細（`background_hook.rs`）；沒報過（舊版 claude、剛重啟）都是 null，數字來自畫面。
        let (tasks, crons) = crate::background_hook::details(app, id);
        o.insert("background_source".into(), if tasks.is_null() { Value::Null } else { "hook".into() });
        o.insert("background_tasks".into(), tasks);
        o.insert("session_crons".into(), crons);
        // 為什麼停在 blocked（結構化原因，`blocked_reason.rs`）：只在 run 現在真的是 blocked 才帶，其他一律 null；
        // 舊的前端忽略這個欄位。
        let blocked = o.get("agent_status").and_then(Value::as_str) == Some("blocked");
        o.insert("blocked_reason".into(), if blocked { crate::blocked_reason::json(id) } else { Value::Null });
        // claude 輸入框裡那句灰字「建議下一句」（`prompt_suggestion.rs`）：只在 idle 帶，其他一律 null。
        let status = o.get("agent_status").and_then(Value::as_str).map(str::to_owned);
        o.insert("prompt_suggestion".into(), crate::prompt_suggestion::json(id, status.as_deref()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/src/lifecycle/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn claude_counts_the_mode_line_not_the_turn_summary() {
        let live = fixture("claude-2.1.281-background-shell.txt");
        assert_eq!(parse("claude", &live), 1);
        assert_eq!(parse("claude", &live.replace("· 1 shell ·", "· 3 shells ·")), 3);
        // 背景跑完：模式列那段消失，收尾行 `· 1 shell still running` 還留在畫面上——不算。
        let done = live.replace("bypass permissions on · 1 shell · ← for agents", "bypass permissions on (shift+tab to cycle) · ← for agents");
        assert!(done.contains("1 shell still running"), "前提：收尾行還在");
        assert_eq!(parse("claude", &done), 0);
        assert_eq!(parse("claude", &fixture("claude-2.1.281-no-background-shell.txt")), 0, "`Ran 1 shell command` 不算");
    }

    #[test]
    fn a_background_shell_serving_a_port_is_a_service_not_a_job() {
        // m4p 真機的形狀（pid 改小）：pane zsh → claude → 兩個背景 shell，一個底下的 node listen 3200、一個在跑 check。
        let ps = "\
  10     1 -zsh
  11    10 claude --dangerously-skip-permissions
  20    11 /bin/zsh -c source /Users/m4p/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true && eval 'pnpm dev'
  21    20 node /x/next dev -p 3200
  22    21 node /x/next-server
  30    11 /bin/zsh -c source /Users/m4p/.claude/shell-snapshots/snapshot-zsh-2.sh 2>/dev/null || true && eval 'scripts/check.sh'
  31    30 bash scripts/check.sh changed
  40     1 /bin/zsh -c source /Users/m4p/.claude/shell-snapshots/snapshot-zsh-3.sh && eval 'vite'
  41    40 node vite
";
        let listening = listening_pids("p22\nf13\np41\nf9\n");
        assert_eq!(service_shells(ps, &listening, 10), 1, "只算這個 pane 底下的；別的 pane 的 vite（pid 40）不算");
        assert_eq!(service_shells(ps, &HashSet::new(), 10), 0, "沒人 listen＝都是工作");
        assert_eq!(service_shells(ps, &listening, 999), 0, "pane shell 不在表上＝讀不到＝不扣");
    }

    #[test]
    fn claude_ignores_the_same_words_quoted_in_the_conversation() {
        let quoted = "  ⏵⏵ bypass permissions on · 1 shell · ← for agents\n".to_string()
            + &"⏺ 一般輸出\n".repeat(BOTTOM_LINES)
            + "❯\n  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n";
        assert_eq!(parse("claude", &quoted), 0, "只看畫面最底下");
    }

    #[test]
    fn codex_counts_idle_and_working_footers_and_clears_after_stop() {
        assert_eq!(parse("codex", &fixture("codex-0.157-background-idle-bg.txt")), 1);
        assert_eq!(parse("codex", &fixture("codex-0.157-background-working-bg.txt")), 2);
        assert_eq!(parse("codex", &fixture("codex-0.157-background-stopped.txt")), 0);
        assert_eq!(parse("claude", &fixture("codex-0.157-background-idle-bg.txt")), 0, "kind 對不上不算");
    }

    #[test]
    fn the_start_time_is_set_on_zero_to_n_kept_while_running_and_cleared_on_zero() {
        let (mut m, h) = (HashMap::new(), 3_600_000);
        assert!(record_at(&mut m, "r", 0, 0), "第一次看過（乾淨）也算變了");
        assert_eq!(m["r"], Entry { n: 0, since: None, stuck: false });
        assert!(record_at(&mut m, "r", 1, 10 * h));
        assert_eq!(m["r"].since, Some(10 * h), "0→N 記下開始時間");
        assert!(record_at(&mut m, "r", 2, 11 * h), "數字變了");
        assert_eq!(m["r"].since, Some(10 * h), "N→M 不重算：還是同一段背景");
        assert!(!record_at(&mut m, "r", 2, 12 * h), "沒變、也還沒到門檻：不推");
        assert!(record_at(&mut m, "r", 2, 13 * h), "數字沒變，但跨過門檻（3 小時）：要推，網頁才換字");
        assert!(m["r"].stuck);
        assert!(!record_at(&mut m, "r", 2, 14 * h), "已經標過卡住：不重推");
        assert!(record_at(&mut m, "r", 0, 15 * h));
        assert_eq!(m["r"], Entry { n: 0, since: None, stuck: false }, "歸零清掉");
        assert!(record_at(&mut m, "r", 1, 16 * h));
        assert_eq!(m["r"].since, Some(16 * h), "下一段背景重新起算");
        assert!(!m["r"].stuck);
    }

    #[tokio::test]
    async fn a_long_running_background_is_projected_with_its_age_and_flagged_in_health() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "alfa").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let run = Some(db::run(&app.db, &run_id).await.unwrap().unwrap());

        let v = run_json(&app, &run, Some(&run_id));
        assert_eq!((v["background_since"].clone(), v["background_secs"].clone(), v["background_stuck"].clone()), (Value::Null, Value::Null, false.into()), "沒看過");

        // 一小時前開始的背景：帶得出持續秒數，還不算卡住。
        record_at(&mut app.background_jobs.lock().unwrap(), &run_id, 1, now_ms() - 3_600_000);
        let v = run_json(&app, &run, Some(&run_id));
        let secs = v["background_secs"].as_i64().unwrap();
        assert!((3600..3660).contains(&secs), "持續秒數：{secs}");
        assert!(v["background_since"].as_str().unwrap().ends_with('Z'));
        assert_eq!(v["background_stuck"], false);
        let health = crate::supervisor::health::snapshot(&app).await.unwrap();
        assert_eq!(health["background_stuck"], serde_json::json!([]), "沒到門檻不列");

        // 四小時前開始：可能卡住，巡檢健康摘要列出來（但不改 status）。
        app.background_jobs.lock().unwrap().clear();
        record_at(&mut app.background_jobs.lock().unwrap(), &run_id, 2, now_ms() - 4 * 3_600_000);
        let v = run_json(&app, &run, Some(&run_id));
        assert_eq!(v["background_stuck"], true);
        let health = crate::supervisor::health::snapshot(&app).await.unwrap();
        let stuck = health["background_stuck"].as_array().unwrap();
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0]["bot_id"], bot.id.as_str());
        assert_eq!(stuck[0]["background_jobs"], 2);
        assert!(stuck[0]["secs"].as_i64().unwrap() >= 4 * 3600);

        // 歸零：開始時間清掉、不再列。
        record(&mut app.background_jobs.lock().unwrap(), &run_id, 0);
        let v = run_json(&app, &run, Some(&run_id));
        assert_eq!((v["background_since"].clone(), v["background_stuck"].clone()), (Value::Null, false.into()));
        let health = crate::supervisor::health::snapshot(&app).await.unwrap();
        assert_eq!(health["background_stuck"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn a_count_is_published_on_change_and_dropped_with_its_run() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "alfa").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        let mut rx = app.subscribe();
        let live = fixture("claude-2.1.281-background-shell.txt");
        let client = app.herdr_for_run(&run).await.expect("mock herdr");
        let pane = run.pane_id.clone().unwrap_or_default();

        observe(&app, &run, "claude", &live, &client, &pane).await;
        assert_eq!(get(&app, &run_id), 1);
        let frame = rx.try_recv().expect("數字變了要推 bot_status");
        assert_eq!(frame.kind, "bot_status");
        let state = crate::api::state_json(&app).await.unwrap();
        assert_eq!(state["projects"][0]["bots"][0]["run"]["background_jobs"], 1, "GET /api/state 帶得出來");

        observe(&app, &run, "claude", &live, &client, &pane).await;
        assert!(rx.try_recv().is_err(), "沒變就不推");

        retain_runs(&app, &[]);
        assert_eq!(get(&app, &run_id), 0, "結束的 run 不留帳");
    }
}
