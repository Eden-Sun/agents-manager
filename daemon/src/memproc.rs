//! Which processes the herdr RAM number is actually made of, and killing the ones that are
//! nobody's bot (SPEC §15.4).
//!
//! `memstat` answers "how much"; this answers "who". The split matters because half the
//! `claude` processes under herdr on a typical machine are panes the user opened by hand or
//! stale `--resume` sessions — reclaimable memory the UI could not point at before.
//!
//! Ownership is read out of each process's **environment**, not out of any bookkeeping of
//! ours: the daemon injects `AM_BOT_ID` when it starts a bot (`lifecycle.rs`), and herdr
//! injects `HERDR_PANE_ID` into every pane. Environment is inherited, so the CLI several
//! levels below a pane shell still carries both. That makes the answer true even for
//! processes started before this daemon booted, which a registry could never be.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use crate::memstat::{child_index, exe_name, herdr_roots, is_herdr, parse_ps, Proc};
use crate::state::App;

/// Two sections in one round trip: the tree (`pid ppid rss argv`) and the environments.
/// macOS exposes another process's environment through `ps -E`; Linux does not, so there it
/// reads `/proc/<pid>/environ` (same-user only, which is exactly the scope we want).
/// The marker keeps a single parser for both halves.
const MARKER: &str = "---AM-ENV---";
const PS_TREE_ENV: &str = r#"ps -Awwo pid=,ppid=,rss=,args= 2>/dev/null
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

/// Rows worth putting in front of a human: a pane's shell or an agent CLI. Everything else
/// (node workers, ripgrep, the dozens of short-lived helpers) stays folded into its parent's
/// `subtree_bytes` instead of turning the list into a process explorer.
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
    pub bot_id: Option<String>,
    pub bot_name: Option<String>,
    pub project_id: Option<String>,
    /// `bot` | `pane` | `herdr` | `unknown` — see [`owner_of`].
    pub owner: String,
    /// This process plus every descendant: "killing this frees roughly that much".
    pub subtree_bytes: u64,
    /// Descendants folded into `subtree_bytes`.
    pub children: u32,
}

/// One process in the herdr trees, before bot names are looked up.
struct Raw {
    p_index: usize,
    pane_id: Option<String>,
    bot_id: Option<String>,
    owner: &'static str,
    subtree_bytes: u64,
    children: u32,
}

/// Who a process belongs to, from its environment.
///
/// `AM_BOT_ID` wins over `HERDR_PANE_ID` because a bot always runs inside a pane; the pane is
/// how it got there, not who owns it. herdr itself is never anybody's to kill.
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

/// `KEY=value` out of a whitespace-joined environment dump.
fn env_value(blob: &str, key: &str) -> Option<String> {
    let want = format!("{key}=");
    blob.split_whitespace()
        .find_map(|t| t.strip_prefix(want.as_str()))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

/// pid -> its environment dump (argv included on macOS; harmless, we only look for our keys).
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

/// Split the dump at the marker **line**. Matching a whole line matters: an argv can contain
/// anything, including the marker text, and a `ps` line can never contain a newline.
fn split_sections(out: &str) -> (&str, &str) {
    let mut at = 0usize;
    for line in out.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == MARKER {
            return (&out[..at], &out[at + line.len()..]);
        }
        at += line.len();
    }
    (out, "")
}

/// Everything in the herdr trees of one dump, with subtree sums and ownership resolved.
fn scan(out: &str) -> (Vec<Proc>, Vec<Raw>) {
    let (tree, env_section) = split_sections(out);
    let procs = parse_ps(tree);
    let envs = parse_env(env_section);
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let index: HashMap<i32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let children = child_index(&procs);

    // Every pid reachable from a herdr root, roots included.
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
        // Subtree walk stays inside the herdr tree by construction: a descendant of a member
        // is a member.
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
        let owner = owner_of(is_herdr(p), bot_id.as_deref(), pane_id.as_deref());
        raws.push(Raw { p_index: i, pane_id, bot_id, owner, subtree_bytes: bytes, children: kids });
    }
    (procs, raws)
}

/// The rows the UI shows, biggest subtree first.
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

/// Parse one host dump into the rows the API returns. Split out from the ssh/`sh` plumbing so
/// the ownership and subtree rules are testable without a machine to sample.
pub fn processes_from_dump(out: &str) -> Vec<MemProcess> {
    let (procs, raws) = scan(out);
    listed(&procs, &raws)
}

/// Run the sampler on `host` (locally or over ssh) and return its raw output.
async fn dump(app: &Arc<App>, host: &str) -> anyhow::Result<String> {
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    if conn.is_local() {
        let o = tokio::process::Command::new("/bin/sh").arg("-c").arg(PS_TREE_ENV).output().await?;
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

/// `GET /api/mem/processes?host=…`: every listed process in that host's herdr trees, with the
/// bot rows filled in from the database.
pub async fn processes(app: &Arc<App>, host: &str) -> anyhow::Result<Value> {
    let out = dump(app, host).await?;
    let mut rows = processes_from_dump(&out);
    for r in &mut rows {
        let Some(id) = r.bot_id.clone() else { continue };
        // A deleted bot still reads as a bot: its process is alive and stopping it is still
        // the bot's own business, not a kill.
        if let Ok(Some(b)) = crate::db::bot(&app.db, &id).await {
            r.bot_name = Some(b.name);
            r.project_id = Some(b.project_id);
        }
    }
    Ok(json!({
        "host": host,
        "sampled_at": chrono::Utc::now().to_rfc3339(),
        "processes": rows,
    }))
}

/// Why a kill was refused. `Bot` is a 409 and not a 400: the request is well formed, there is
/// simply a better door (`POST /bots/{id}/stop`, which also records the stop).
pub enum KillDenied {
    NotInTree,
    Herdr,
    Bot(String),
}

/// Re-sample, check the pid is still a killable member of a herdr tree, then signal it.
///
/// Re-sampling rather than trusting the caller's list is the whole safety story: pids are
/// recycled, and a stale row must never let a `kill` escape the herdr trees.
pub async fn kill(app: &Arc<App>, host: &str, pid: i32, signal: &str) -> anyhow::Result<Result<Value, KillDenied>> {
    let out = dump(app, host).await?;
    let (procs, raws) = scan(&out);
    let Some(raw) = raws.iter().find(|r| procs[r.p_index].pid == pid) else {
        return Ok(Err(KillDenied::NotInTree));
    };
    if raw.owner == "herdr" {
        return Ok(Err(KillDenied::Herdr));
    }
    if raw.owner == "bot" {
        return Ok(Err(KillDenied::Bot(raw.bot_id.clone().unwrap_or_default())));
    }
    let sig = if signal.eq_ignore_ascii_case("KILL") { "KILL" } else { "TERM" };
    let cmd = format!("kill -{sig} {pid}");
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    if conn.is_local() {
        let o = tokio::process::Command::new("/bin/sh").arg("-c").arg(&cmd).output().await?;
        if !o.status.success() {
            anyhow::bail!("kill exited {}: {}", o.status, String::from_utf8_lossy(&o.stderr).trim());
        }
    } else {
        conn.ssh_exec_path(&cmd).await?;
    }

    let p = &procs[raw.p_index];
    let freed = raw.subtree_bytes;
    let exe = exe_name(&p.argv).to_string();
    // The badge is the reason the user came here; let it move now rather than up to 15s later.
    let snap = crate::memstat::sample(app).await;
    app.emit("mem_updated", json!(snap)).await;
    Ok(Ok(json!({"host": host, "pid": pid, "signal": sig, "exe": exe, "freed_bytes": freed})))
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
        // The 40 MiB node worker is over the floor but still listed only because it clears it;
        // anything under 8 MiB would be folded away.
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
}
