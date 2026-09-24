//! Which processes make up the herdr RAM number, and killing the ones that are nobody's bot (SPEC §15.4).
//!
//! Ownership comes from each process's inherited environment (`AM_BOT_ID`, `HERDR_PANE_ID`),
//! not our bookkeeping, so it stays true for processes started before this daemon booted.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use crate::memstat::{child_index, exe_name, herdr_roots, is_herdr, parse_ps, Proc};
use crate::state::App;

/// Tree and environments in one round trip. macOS has `ps -E`; Linux needs `/proc/<pid>/environ`
/// (same-user only, which is exactly the scope we want).
const MARKER: &str = "---AM-ENV---";
/// #526：送訊號那一趟要確認「這個 pid 還是剛才篩過的那一顆」，靠的是起始時間。獨立一段是因為
/// `lstart` 本身含空白（`Wed Sep 24 10:11:12 2026`），混進主表會把 `parse_ps` 的 argv 欄切壞。
const START_MARKER: &str = "---AM-START---";
const PS_TREE_ENV: &str = r#"ps -Awwo pid=,ppid=,rss=,args= 2>/dev/null
echo '---AM-START---'
ps -Awwo pid=,lstart= 2>/dev/null
echo '---AM-ENV---'
if [ "$(uname -s)" = Linux ]; then
  for f in /proc/[0-9]*/environ; do
    p=${f%/environ}; p=${p#/proc/}
    e=$(tr '\0' ' ' < "$f" 2>/dev/null) || continue
    printf '%s %s\n' "$p" "$e"
  done
else
  ps -Ewwo pid=,args= 2>/dev/null
fi"#;

/// Everything else stays folded into its parent's `subtree_bytes` instead of turning the list
/// into a process explorer.
const LISTED: &[&str] = &["claude", "codex", "grok", "node", "bash", "zsh", "sh", "fish"];

/// Below this a row is noise; its bytes still count towards the parent's subtree.
const MIN_SUBTREE: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemProcess {
    pub pid: i32,
    pub ppid: i32,
    pub rss_bytes: u64,
    pub exe: String,
    pub argv: String,
    pub pane_id: Option<String>,
    /// Pane ids are per herdr session; the user's own panes usually live in `default`, not ours.
    pub socket_path: Option<String>,
    pub bot_id: Option<String>,
    pub bot_name: Option<String>,
    pub project_id: Option<String>,
    /// `bot` | `pane` | `herdr` | `unknown` — see [`owner_of`].
    pub owner: String,
    /// This process plus every descendant: "killing this frees roughly that much".
    pub subtree_bytes: u64,
    pub children: u32,
}

struct Raw {
    p_index: usize,
    pane_id: Option<String>,
    socket_path: Option<String>,
    bot_id: Option<String>,
    owner: &'static str,
    subtree_bytes: u64,
    children: u32,
}

/// `AM_BOT_ID` wins over `HERDR_PANE_ID`: a bot always runs inside a pane, the pane is not its owner.
fn owner_of(is_herdr_proc: bool, bot_id: Option<&str>, pane_id: Option<&str>) -> &'static str {
    if is_herdr_proc {
        "herdr"
    } else if bot_id.is_some() {
        "bot"
    } else if pane_id.is_some() {
        "pane"
    } else {
        "unknown"
    }
}

fn env_value(blob: &str, key: &str) -> Option<String> {
    let want = format!("{key}=");
    blob.split_whitespace()
        .find_map(|t| t.strip_prefix(want.as_str()))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

fn parse_env(section: &str) -> HashMap<i32, String> {
    let mut m = HashMap::new();
    for line in section.lines() {
        let line = line.trim_start();
        let Some((pid, rest)) = line.split_once(char::is_whitespace) else { continue };
        let Ok(pid) = pid.parse::<i32>() else { continue };
        m.insert(pid, rest.to_string());
    }
    m
}

/// Match the marker as a whole line: an argv can contain the marker text, but never a newline.
fn split_at<'a>(out: &'a str, marker: &str) -> (&'a str, &'a str) {
    let mut at = 0usize;
    for line in out.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == marker {
            return (&out[..at], &out[at + line.len()..]);
        }
        at += line.len();
    }
    (out, "")
}

/// `(行程表, 環境段)`。先切環境段再切起始時間段，所以沒有 `---AM-START---` 的舊輸出
/// （測試夾具、手工餵的 dump）照樣解得出前兩段。
fn split_sections(out: &str) -> (&str, &str) {
    let (head, env) = split_at(out, MARKER);
    let (tree, _) = split_at(head, START_MARKER);
    (tree, env)
}

fn start_section(out: &str) -> &str {
    let (head, _) = split_at(out, MARKER);
    split_at(head, START_MARKER).1
}

/// `pid` → 起始時間（空白收斂成一個，前後修掉）。比對用，不解析內容——格式是那台機器的 `ps` 給的，
/// 送訊號那趟也用同一支 `ps` 產生，兩邊一致就夠了。
fn parse_start(section: &str) -> HashMap<i32, String> {
    let mut m = HashMap::new();
    for line in section.lines() {
        let line = line.trim();
        let Some((pid, rest)) = line.split_once(char::is_whitespace) else { continue };
        let Ok(pid) = pid.parse::<i32>() else { continue };
        let started = rest.split_whitespace().collect::<Vec<_>>().join(" ");
        if !started.is_empty() {
            m.insert(pid, started);
        }
    }
    m
}

fn scan(out: &str) -> (Vec<Proc>, Vec<Raw>) {
    let (tree, env_section) = split_sections(out);
    let procs = parse_ps(tree);
    let envs = parse_env(env_section);
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let index: HashMap<i32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let children = child_index(&procs);

    let mut in_tree: Vec<i32> = Vec::new();
    let mut seen: HashSet<i32> = HashSet::new();
    for root in herdr_roots(&procs, &by_pid) {
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            in_tree.push(pid);
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
    }

    let mut raws = Vec::new();
    for pid in in_tree {
        let Some(&i) = index.get(&pid) else { continue };
        let p = &procs[i];
        let (mut bytes, mut kids) = (p.rss_kib * 1024, 0u32);
        let mut stack: Vec<i32> = children.get(&pid).cloned().unwrap_or_default();
        let mut walked: HashSet<i32> = HashSet::new();
        while let Some(c) = stack.pop() {
            if c == pid || !walked.insert(c) {
                continue;
            }
            if let Some(cp) = by_pid.get(&c) {
                bytes += cp.rss_kib * 1024;
                kids += 1;
            }
            if let Some(gs) = children.get(&c) {
                stack.extend(gs.iter().copied());
            }
        }
        let blob = envs.get(&pid);
        let bot_id = blob.and_then(|b| env_value(b, "AM_BOT_ID"));
        let pane_id = blob.and_then(|b| env_value(b, "HERDR_PANE_ID"));
        let socket_path = blob.and_then(|b| env_value(b, "HERDR_SOCKET_PATH"));
        let owner = owner_of(is_herdr(p), bot_id.as_deref(), pane_id.as_deref());
        raws.push(Raw { p_index: i, pane_id, socket_path, bot_id, owner, subtree_bytes: bytes, children: kids });
    }
    (procs, raws)
}

fn listed(procs: &[Proc], raws: &[Raw]) -> Vec<MemProcess> {
    let mut rows: Vec<MemProcess> = raws
        .iter()
        .filter(|r| {
            let exe = exe_name(&procs[r.p_index].argv);
            LISTED.contains(&exe) && r.subtree_bytes >= MIN_SUBTREE
        })
        .map(|r| {
            let p = &procs[r.p_index];
            MemProcess {
                pid: p.pid,
                ppid: p.ppid,
                rss_bytes: p.rss_kib * 1024,
                exe: exe_name(&p.argv).to_string(),
                argv: p.argv.clone(),
                pane_id: r.pane_id.clone(),
                socket_path: r.socket_path.clone(),
                bot_id: r.bot_id.clone(),
                bot_name: None,
                project_id: None,
                owner: r.owner.to_string(),
                subtree_bytes: r.subtree_bytes,
                children: r.children,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.subtree_bytes.cmp(&a.subtree_bytes).then(a.pid.cmp(&b.pid)));
    rows
}

/// 一個 pane 現在到底在跑什麼（§6.5e 的 shell／service 分類用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneFacts {
    /// pane 行程樹裡看到的 `AM_BOT_ID`（通常只有一個；多個代表這個 pane 被不同 bot 用過）。
    pub bot_ids: Vec<String>,
    /// pane 行程樹裡看到的 `AM_PROJECT_ID`：開 pane 當下綁的專案，bot 被刪也還在（§6.5e）。
    pub project_ids: Vec<String>,
    /// 這個 pane 底下所有行程的 pid（shell 自己在最前面），用來對 listen port。
    pub pids: Vec<i32>,
    /// 最有代表性的前景程式（shell 以外最上層的那個）；只有 shell 時是 `None`。
    pub foreground: Option<String>,
    /// 行程樹只有 shell 自己：沒有前景程式、沒有被 Ctrl-Z 丟到背景的 job、也沒有巢狀 shell。
    pub shell_only: bool,
    /// 樹裡至少有一個行程讀得到**這顆 pane** 的環境。`false` 時 `bot_ids`／`project_ids` 是空的不代表
    /// 「沒人開的」：macOS 讀不到 `-zsh` 本身的環境，閒著的 shell 永遠是這樣。
    pub env_seen: bool,
}

const SHELLS: &[&str] = &["bash", "zsh", "sh", "fish", "dash", "ksh", "login", "-zsh", "-bash"];

/// 以 herdr 報的 shell pid 為根，沿 `ps -A` 的 ppid 樹把**全部子孫**算進這顆 pane（§6.5e 的 GC 守門）。
///
/// 不靠子孫自己的環境歸屬：`sudo`（euid root）與它底下的 `vim`、`env -i` 起的東西、macOS 上連 `-zsh`
/// 本身都讀不到環境，以前這些行程在事實裡根本不存在，卡在密碼提示的 pane 於是被當成「只有 shell」關掉。
/// 子孫裡只要有任何行程（含巢狀 shell、讀不到環境的、非本人的）就不是 `shell_only`。
///
/// `AM_*` 只從**這顆 pane** 的行程讀：環境裡的 `HERDR_PANE_ID` 是別的 pane 的（別的 herdr session
/// 剛好同號，或從別的 pane 繼承下來）就不算。回 `None`＝樹裡找不到這個 pid，呼叫端一律當「判不出來」。
pub fn pane_facts_for_shell(out: &str, pane_id: &str, shell_pid: i32) -> Option<PaneFacts> {
    let (tree, env_section) = split_sections(out);
    let procs = parse_ps(tree);
    let envs = parse_env(env_section);
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let root = by_pid.get(&shell_pid)?;
    let children = child_index(&procs);

    // 由上往下（BFS），所以第一個非 shell 的就是最上層的前景程式。
    let mut order = vec![shell_pid];
    let mut seen: HashSet<i32> = HashSet::from([shell_pid]);
    let mut at = 0;
    while at < order.len() {
        if let Some(kids) = children.get(&order[at]) {
            let mut kids = kids.clone();
            kids.sort_unstable();
            order.extend(kids.into_iter().filter(|k| seen.insert(*k)));
        }
        at += 1;
    }

    let mut f = PaneFacts::default();
    let root_is_shell = SHELLS.contains(&exe_name(&root.argv));
    for pid in &order {
        let Some(p) = by_pid.get(pid) else { continue };
        f.pids.push(*pid);
        let is_shell = SHELLS.contains(&exe_name(&p.argv));
        if f.foreground.is_none() && !is_herdr(p) && !is_shell {
            f.foreground = Some(p.argv.clone());
        }
        let Some(blob) = envs.get(pid) else { continue };
        if env_value(blob, "HERDR_PANE_ID").as_deref() != Some(pane_id) {
            continue;
        }
        f.env_seen = true;
        for (key, into) in [("AM_BOT_ID", &mut f.bot_ids), ("AM_PROJECT_ID", &mut f.project_ids)] {
            if let Some(v) = env_value(blob, key) {
                if !into.contains(&v) {
                    into.push(v);
                }
            }
        }
    }
    f.shell_only = root_is_shell && f.pids.len() == 1;
    Some(f)
}

/// Resident memory by bot, from one dump: every process in the herdr tree that carries `AM_BOT_ID`
/// counts once (its own RSS, not its subtree — children inherit the variable and count themselves),
/// with the panes those processes run in. Bot ids only; the caller maps them to projects.
pub fn bot_totals_from_dump(out: &str) -> HashMap<String, (u64, HashSet<String>)> {
    let (procs, raws) = scan(out);
    let mut by_bot: HashMap<String, (u64, HashSet<String>)> = HashMap::new();
    for r in &raws {
        let Some(bot) = r.bot_id.clone() else { continue };
        if r.owner != "bot" {
            continue;
        }
        let e = by_bot.entry(bot).or_default();
        e.0 += procs[r.p_index].rss_kib * 1024;
        if let Some(pane) = &r.pane_id {
            e.1.insert(format!("{}|{pane}", r.socket_path.as_deref().unwrap_or("")));
        }
    }
    by_bot
}

/// Split out from the ssh/`sh` plumbing so ownership and subtree rules are testable.
pub fn processes_from_dump(out: &str) -> Vec<MemProcess> {
    let (procs, raws) = scan(out);
    listed(&procs, &raws)
}

pub(crate) async fn dump(app: &Arc<App>, host: &str) -> anyhow::Result<String> {
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    if conn.is_local() {
        let o = crate::local_sh::output(PS_TREE_ENV).await?;
        if !o.status.success() {
            anyhow::bail!("ps exited {}", o.status);
        }
        return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
    }
    if !conn.is_connected() {
        anyhow::bail!("未連線");
    }
    conn.ssh_exec_path(PS_TREE_ENV).await
}

/// `GET /api/mem/processes?host=…`
pub async fn processes(app: &Arc<App>, host: &str) -> anyhow::Result<Value> {
    let out = dump(app, host).await?;
    let mut rows = processes_from_dump(&out);
    for r in &mut rows {
        let Some(id) = r.bot_id.clone() else { continue };
        // A deleted bot still reads as a bot: stopping it is the bot's business, not a kill.
        if let Ok(Some(b)) = crate::db::bot(&app.db, &id).await {
            r.bot_name = Some(b.name);
            r.project_id = Some(b.project_id);
        }
    }
    Ok(json!({
        "host": host,
        "sampled_at": crate::db::now(),
        "processes": rows,
    }))
}

/// `GET /api/mem/processes/pane` (SPEC §15.2). Any pane herdr knows is readable (no registration
/// check, unlike `shell::read`), but only the last `lines` visible rows as plain text — never keys or input.
pub async fn pane_preview(app: &Arc<App>, host: &str, pane_id: &str, socket: Option<&str>, lines: u32) -> crate::lifecycle::LcResult<Value> {
    use crate::lifecycle::LcError;
    // Another local herdr session's socket: same user's socket, no wider than `herdr` in their shell.
    let client = match socket.filter(|s| !s.is_empty()) {
        Some(path) if host == crate::config::LOCAL_HOST => {
            if !std::path::Path::new(path).exists() {
                return Err(LcError::Upstream(format!("herdr socket `{path}` 不在了")));
            }
            crate::herdr::HerdrClient::new(path)
        }
        Some(_) => return Err(LcError::Bad("遠端主機只能讀它設定的那個 herdr session".into())),
        None => crate::api::shell::client_for(app, host).await?.0,
    };
    let read = client.pane_read(pane_id, "visible", lines).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    let (columns, rows) = match client.pane_size(pane_id).await {
        Ok(Some((w, h))) => (Some(w), Some(h)),
        _ => (None, None),
    };
    Ok(json!({
        "host": host, "pane_id": pane_id,
        "source": read.source, "text": read.text, "revision": read.revision, "truncated": read.truncated,
        "columns": columns, "rows": rows,
    }))
}

/// `Bot` is a 409, not a 400: the request is fine, the better door is `POST /bots/{id}/stop`.
#[derive(Debug)]
pub enum KillDenied {
    NotInTree,
    Herdr,
    Bot(String),
    /// #526：篩選完到送訊號之間這個 pid 已經不是同一顆行程了（退出、pid 被回收）。什麼都沒送。
    PidChanged,
}

/// `screen_kill` 放行時帶回來的東西：送給誰、回報釋放多少、以及送之前要比對的起始時間。
#[derive(Debug)]
struct Target {
    pid: i32,
    exe: String,
    freed: u64,
    started: String,
}

/// 送訊號前的篩選（純函式，好測）：不在樹裡、herdr、bot 都擋。
/// **讀不到這個 pid 的環境＝判不出是不是 bot 的行程**（`ps -E` 壞了、環境段整段空、Linux 的 `/proc/<pid>/environ` 讀不了），
/// 這時 owner 會退成 `unknown`、被當成沒主人的行程放行，等於 bot 的 claude 就能被砍——所以直接不送。
fn screen_kill(out: &str, pid: i32) -> anyhow::Result<Result<Target, KillDenied>> {
    let (procs, raws) = scan(out);
    let Some(raw) = raws.iter().find(|r| procs[r.p_index].pid == pid) else {
        return Ok(Err(KillDenied::NotInTree));
    };
    if raw.owner == "herdr" {
        return Ok(Err(KillDenied::Herdr));
    }
    if raw.owner == "bot" {
        return Ok(Err(KillDenied::Bot(raw.bot_id.clone().unwrap_or_default())));
    }
    if !parse_env(split_sections(out).1).contains_key(&pid) {
        anyhow::bail!("讀不到 pid {pid} 的環境變數，判不出它是不是 bot 的行程，不送訊號");
    }
    // #526：沒有起始時間就沒辦法確認送訊號那一刻還是同一顆行程，寧可不送。
    let Some(started) = parse_start(start_section(out)).get(&pid).cloned() else {
        anyhow::bail!("讀不到 pid {pid} 的起始時間，確認不了送訊號時還是同一顆行程，不送");
    };
    Ok(Ok(Target { pid, exe: exe_name(&procs[raw.p_index].argv).to_string(), freed: raw.subtree_bytes, started }))
}

/// 確認與送訊號放進**同一趟**指令（#526）：兩趟之間 pid 被回收的話，訊號會打在別人身上，
/// 而回報還是被篩選那一顆的 exe 與大小。對不上就什麼都不送。
fn kill_script(target: &Target, sig: &str) -> String {
    let pid = target.pid;
    format!(
        "s=$(ps -o lstart= -p {pid} 2>/dev/null | tr -s ' ' | sed -e 's/^ *//' -e 's/ *$//')\n\
         [ \"$s\" = {started} ] || {{ printf 'AM_PID_CHANGED\\n'; exit 0; }}\n\
         kill -{sig} {pid} 2>/dev/null\n\
         printf 'AM_KILLED\\n'\n\
         exit 0\n",
        started = crate::hosts::sh_quote(&target.started),
    )
}

/// Re-samples instead of trusting the caller's list: pids are recycled, and a stale row must
/// never let a `kill` escape the herdr trees.
pub async fn kill(app: &Arc<App>, host: &str, pid: i32, signal: &str) -> anyhow::Result<Result<Value, KillDenied>> {
    let out = dump(app, host).await?;
    let target = match screen_kill(&out, pid)? {
        Ok(t) => t,
        Err(d) => return Ok(Err(d)),
    };
    let sig = if signal.eq_ignore_ascii_case("KILL") { "KILL" } else { "TERM" };
    let cmd = kill_script(&target, sig);
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    let stdout = if conn.is_local() {
        let o = crate::local_sh::output(&cmd).await?;
        if !o.status.success() {
            anyhow::bail!("kill exited {}: {}", o.status, String::from_utf8_lossy(&o.stderr).trim());
        }
        String::from_utf8_lossy(&o.stdout).into_owned()
    } else {
        conn.ssh_exec_path(&cmd).await?
    };
    let (exe, freed) = (target.exe, target.freed);
    if stdout.lines().any(|l| l.trim() == "AM_PID_CHANGED") {
        return Ok(Err(KillDenied::PidChanged));
    }
    if !stdout.lines().any(|l| l.trim() == "AM_KILLED") {
        anyhow::bail!("kill 沒有回報結果，不確定送出去沒有：{}", stdout.trim());
    }

    // Update the badge now rather than up to 15s later.
    let snap = crate::memstat::sample(app).await;
    app.emit("mem_updated", json!(snap)).await;
    Ok(Ok(json!({"host": host, "pid": pid, "signal": sig, "exe": exe, "freed_bytes": freed})))
}

/// 測試用：跑一段 sh 並回 stdout。放在這裡是因為 `kill_script` 的驗證要真的執行腳本。
#[cfg(test)]
fn run_sh(script: &str) -> String {
    let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).output().expect("sh");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "\
  400     1  48000 /opt/homebrew/bin/herdr --session agents-manager
  401   400  30000 /bin/zsh -l
  402   401 820000 claude --dangerously-skip-permissions
  403   402  40000 node /x/worker.js
  404   400  20000 /bin/zsh -l
  405   404 640000 codex --yolo
  406   400  10000 /bin/zsh -l
  407   406 300000 claude --resume
  500     1  90000 claude --not-under-herdr
---AM-START---
  400 Wed Sep 24 10:00:00 2026
  401 Wed Sep 24 10:00:01 2026
  402 Wed Sep 24 10:00:02 2026
  403 Wed Sep 24 10:00:03 2026
  404 Wed Sep 24 10:00:04 2026
  405 Wed Sep 24 10:00:05 2026
  406 Wed Sep 24 10:00:06 2026
  407 Wed Sep 24 10:00:07 2026
  500 Wed Sep 24 10:00:08 2026
---AM-ENV---
  401 /bin/zsh -l HERDR_PANE_ID=w1:p1 AM_BOT_ID=b1
  402 claude HERDR_PANE_ID=w1:p1 AM_BOT_ID=b1
  403 node HERDR_PANE_ID=w1:p1 AM_BOT_ID=b1
  404 /bin/zsh -l HERDR_PANE_ID=w2:p1
  405 codex HERDR_PANE_ID=w2:p1
  406 /bin/zsh -l
  407 claude
";

    fn row(rows: &[MemProcess], pid: i32) -> &MemProcess {
        rows.iter().find(|r| r.pid == pid).expect("row present")
    }

    #[test]
    fn owner_comes_from_the_environment() {
        let rows = processes_from_dump(DUMP);
        assert_eq!(row(&rows, 402).owner, "bot");
        assert_eq!(row(&rows, 402).bot_id.as_deref(), Some("b1"));
        assert_eq!(row(&rows, 405).owner, "pane");
        assert_eq!(row(&rows, 405).pane_id.as_deref(), Some("w2:p1"));
        assert_eq!(row(&rows, 405).bot_id, None);
        // No env at all: we cannot claim it for anyone.
        assert_eq!(row(&rows, 407).owner, "unknown");
    }

    /// 專案標籤的底：每顆 bot 的程序各算自己的 RSS（不重複算子樹），pane 以 socket+pane id 去重；沒有 AM_BOT_ID 的不歸任何 bot。
    #[test]
    fn bot_totals_count_each_process_once_with_its_panes() {
        // 行程列要插在**行程表**那一段（`---AM-START---` 之前），不是起始時間段裡。
        let dump = DUMP.replace("---AM-START---\n", "  408   400  50000 /bin/zsh -l\n  409   408 200000 claude\n---AM-START---\n")
            + "  408 /bin/zsh HERDR_PANE_ID=w1:p2 AM_BOT_ID=b1\n  409 claude HERDR_PANE_ID=w1:p2 AM_BOT_ID=b1\n";
        let totals = bot_totals_from_dump(&dump);
        assert_eq!(totals.len(), 1, "只有 b1 帶 AM_BOT_ID");
        let (bytes, panes) = &totals["b1"];
        assert_eq!(*bytes, (30_000 + 820_000 + 40_000 + 50_000 + 200_000) * 1024);
        assert_eq!(panes.len(), 2);
    }

    #[test]
    fn subtree_folds_children_in() {
        let rows = processes_from_dump(DUMP);
        // zsh 30M + claude 820M + node 40M.
        assert_eq!(row(&rows, 401).subtree_bytes, (30_000 + 820_000 + 40_000) * 1024);
        assert_eq!(row(&rows, 401).children, 2);
        assert_eq!(row(&rows, 402).subtree_bytes, (820_000 + 40_000) * 1024);
        assert_eq!(row(&rows, 402).children, 1);
    }

    #[test]
    fn only_herdr_trees_and_only_big_enough_rows() {
        let rows = processes_from_dump(DUMP);
        // Not under any herdr root.
        assert!(rows.iter().all(|r| r.pid != 500));
        // herdr itself is not offered as a row.
        assert!(rows.iter().all(|r| r.exe != "herdr"));
        // Anything under 8 MiB is folded away.
        let small = processes_from_dump("  400     1  48000 herdr\n  401   400   100 /bin/zsh -l\n");
        assert!(small.is_empty());
    }

    #[test]
    fn an_argv_containing_the_marker_does_not_split_the_dump_early() {
        let dump = "\
  400     1  48000 herdr
  401   400  30000 /bin/zsh -c echo ---AM-ENV--- here
  402   401 820000 claude
---AM-ENV---
  401 /bin/zsh HERDR_PANE_ID=w9:p1
  402 claude HERDR_PANE_ID=w9:p1
";
        let rows = processes_from_dump(dump);
        assert_eq!(row(&rows, 402).owner, "pane");
        assert_eq!(row(&rows, 402).pane_id.as_deref(), Some("w9:p1"));
    }

    #[test]
    fn sorted_by_what_killing_it_would_free() {
        let rows = processes_from_dump(DUMP);
        let sizes: Vec<u64> = rows.iter().map(|r| r.subtree_bytes).collect();
        assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "{sizes:?}");
    }

    /// §6.5e GC 守門：子孫一律算進來，不管讀不讀得到它的環境、是不是 shell。
    #[test]
    fn every_descendant_of_the_pane_shell_counts_whatever_its_env() {
        let dump = "\
  400     1  48000 /opt/homebrew/bin/herdr --session agents-manager
  401   400  30000 -zsh
  402   401  20000 bash deploy.sh
  403   400  30000 -zsh
  404   403  20000 env -i /usr/bin/python3 -m http.server
  405   400  30000 -zsh
  406   405  20000 node dev.js
---AM-ENV---
  402 bash deploy.sh HERDR_PANE_ID=w1:p1 AM_BOT_ID=b1 AM_PROJECT_ID=p1
  406 node dev.js HERDR_PANE_ID=w9:p9 AM_BOT_ID=someone-else
";
        // 巢狀 shell 卡在 `read -p`：它是 shell，但不是「只有 shell 自己」。
        let nested = pane_facts_for_shell(dump, "w1:p1", 401).unwrap();
        assert!(!nested.shell_only);
        assert_eq!(nested.foreground, None, "巢狀 shell 不算前景程式");
        assert_eq!(nested.pids, vec![401, 402]);
        assert_eq!((nested.bot_ids.as_slice(), nested.project_ids.as_slice()), (&["b1".to_string()][..], &["p1".to_string()][..]));

        // 讀不到環境的子行程照樣是這顆 pane 的。
        let no_env = pane_facts_for_shell(dump, "w1:p2", 403).unwrap();
        assert!(!no_env.shell_only);
        assert!(no_env.foreground.as_deref().unwrap().contains("http.server"));

        // 環境裡的 pane id 是別人的（別的 herdr session 同號、或繼承來的）：行程算這顆，歸屬不算。
        let foreign = pane_facts_for_shell(dump, "w1:p3", 405).unwrap();
        assert!(!foreign.shell_only);
        assert!(foreign.bot_ids.is_empty(), "{foreign:?}");

        assert_eq!(pane_facts_for_shell(dump, "w1:p4", 999), None, "不在樹裡就判不出來");
    }

    #[test]
    fn kill_targets_are_screened_before_the_signal() {
        let (procs, raws) = scan(DUMP);
        let find = |pid: i32| raws.iter().find(|r| procs[r.p_index].pid == pid);
        // A pid outside every herdr tree is not ours to signal.
        assert!(find(1).is_none());
        assert!(find(500).is_none());
        // A bot goes through `POST /bots/{id}/stop`, not through kill.
        assert_eq!(find(402).unwrap().owner, "bot");
        assert_eq!(find(400).unwrap().owner, "herdr");
        assert_eq!(find(405).unwrap().owner, "pane");
    }

    /// #526：確認與送訊號要在同一趟。這裡真的把產生出來的腳本跑起來，但 **pid 是本測試行程自己**、
    /// 訊號送給它會出事，所以只驗到比對那一段：對不上時腳本在 `kill` 之前就 exit，對得上那次
    /// 送的是 `kill -TERM <自己>`——所以**不執行**那一版，改看腳本長相（下一條）。
    #[test]
    fn the_kill_script_stops_at_the_check_when_the_pid_changed() {
        // 起始時間對不上：什麼都不送，連 `kill` 那一行都走不到。
        let me = std::process::id() as i32;
        let other = Target { pid: me, exe: "x".into(), freed: 1, started: "Wed Sep 24 10:00:05 2026".into() };
        let out = super::run_sh(&kill_script(&other, "TERM"));
        assert!(out.contains("AM_PID_CHANGED"), "{out:?}");
        assert!(!out.contains("AM_KILLED"), "對不上就不准往下走：{out:?}");
    }

    /// 腳本長相：比對一定在 `kill` 之前，而且比的是篩選當下那顆的起始時間。
    #[test]
    fn the_kill_script_checks_the_start_time_before_it_signals() {
        let t = match screen_kill(DUMP, 405).unwrap() {
            Ok(t) => t,
            Err(d) => panic!("405 是 pane 的行程，應該放行：{d:?}"),
        };
        assert_eq!(t.started, "Wed Sep 24 10:00:05 2026");
        let script = kill_script(&t, "TERM");
        let check = script.find("AM_PID_CHANGED").expect("要有比對那一段");
        let first_kill = script.find("kill -").expect("要有送訊號那一段");
        assert!(check < first_kill, "比對必須在送訊號之前：{script}");
        assert!(script.contains("'Wed Sep 24 10:00:05 2026'"), "比的是篩選當下那顆的起始時間：{script}");
    }

    /// 沒有起始時間（那台的 `ps` 不吃 `lstart`、或輸出被截斷）＝確認不了同一顆行程，寧可不送。
    #[test]
    fn a_target_without_a_start_time_is_refused() {
        let no_start = DUMP.replace("  405 Wed Sep 24 10:00:05 2026\n", "");
        let err = screen_kill(&no_start, 405).unwrap_err().to_string();
        assert!(err.contains("起始時間"), "{err}");
    }

    /// 環境段讀不到（`ps -E` 壞了、Linux 的 environ 讀不了）：bot 的行程會被誤當成沒主人，所以不能送訊號。
    #[test]
    fn kill_refuses_when_the_target_env_was_not_read() {
        let (tree, _) = DUMP.split_once("---AM-ENV---\n").unwrap();
        let no_env_at_all = format!("{tree}---AM-ENV---\n");
        let err = screen_kill(&no_env_at_all, 402).unwrap_err().to_string();
        assert!(err.contains("環境"), "{err}");
        // 只缺這個 pid 的那一行也一樣。
        let missing_one = DUMP.replace("  405 codex HERDR_PANE_ID=w2:p1\n", "");
        let err = screen_kill(&missing_one, 405).unwrap_err().to_string();
        assert!(err.contains("環境"), "{err}");
        // 讀得到、真的沒主人：照舊放行；bot 照舊 409。
        assert!(matches!(screen_kill(DUMP, 407), Ok(Ok(_))));
        assert!(matches!(screen_kill(DUMP, 402), Ok(Err(KillDenied::Bot(_)))));
    }
}
