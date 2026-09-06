//! How much RAM the herdr side of the world is holding.
//!
//! The number the UI shows in its top-left corner is the **resident set of every herdr
//! process tree**: the `herdr` server itself plus everything it spawned — the panes and the
//! agent CLIs living in them. That is the honest answer to "how much is this costing me",
//! because a pane full of `claude` is where the memory actually goes; herdr's own daemon is
//! a rounding error next to it.
//!
//! One `ps` per host per tick, parsed here. `ps` is used rather than a sysinfo crate because
//! remote hosts have to be measured over ssh anyway, and running the same command in both
//! places keeps the two numbers comparable.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use crate::state::App;

/// Long enough that a per-host `ps` (an ssh round trip for remote ones) is cheap, short
/// enough that starting a bot shows up while you are still looking at the screen.
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// `ps` output is the same shape everywhere we run: pid, ppid, RSS in KiB, then argv.
/// `-ww` so macOS does not clip argv to the terminal width; the exe name is the first
/// token, but a clipped line still costs us the tail of long agent command lines.
const PS_CMD: &str = "ps -Awwo pid=,ppid=,rss=,args= 2>/dev/null";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HostMem {
    pub host: String,
    /// RSS of the `herdr` processes themselves, bytes.
    pub herdr_bytes: u64,
    /// RSS of everything running underneath them (panes, agent CLIs), bytes.
    pub agents_bytes: u64,
    /// `herdr_bytes + agents_bytes`, so the UI never has to add them up itself.
    pub total_bytes: u64,
    /// How many processes that total covers, herdr roots included.
    pub processes: u32,
    /// Set when this host could not be sampled; the other fields are then 0.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct MemSnapshot {
    pub total_bytes: u64,
    pub herdr_bytes: u64,
    pub agents_bytes: u64,
    pub processes: u32,
    pub hosts: Vec<HostMem>,
}

struct Proc {
    pid: i32,
    ppid: i32,
    rss_kib: u64,
    argv: String,
}

/// The command's own name, with the path and any interpreter prefix stripped:
/// `/opt/homebrew/bin/herdr --session x` → `herdr`.
fn exe_name(argv: &str) -> &str {
    let first = argv.split_whitespace().next().unwrap_or("");
    first.rsplit('/').next().unwrap_or(first)
}

fn parse_ps(out: &str) -> Vec<Proc> {
    let mut v = Vec::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(rss)) = (it.next(), it.next(), it.next()) else { continue };
        let (Ok(pid), Ok(ppid), Ok(rss_kib)) = (pid.parse::<i32>(), ppid.parse::<i32>(), rss.parse::<u64>()) else {
            continue;
        };
        // argv keeps its internal spacing; only the three fixed columns were split off.
        let argv = line.split_whitespace().skip(3).collect::<Vec<_>>().join(" ");
        v.push(Proc { pid, ppid, rss_kib, argv });
    }
    v
}

/// Sum the herdr trees in one `ps` dump.
///
/// A herdr **root** is a process whose own executable is `herdr` and whose parent is not
/// already inside a herdr tree — otherwise a `herdr` that shells out to `herdr` would be
/// counted twice. Everything reachable from a root is an "agent": that is where the CLIs
/// live, and they are the reason this number is worth showing at all.
pub fn sum_herdr(out: &str, host: &str) -> HostMem {
    let procs = parse_ps(out);
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in &procs {
        children.entry(p.ppid).or_default().push(p.pid);
    }

    let is_herdr = |p: &Proc| exe_name(&p.argv) == "herdr";
    // Walk up to the top: a herdr under another herdr is not its own root.
    let has_herdr_ancestor = |p: &Proc| {
        let mut cur = p.ppid;
        for _ in 0..64 {
            let Some(parent) = by_pid.get(&cur) else { return false };
            if is_herdr(parent) {
                return true;
            }
            cur = parent.ppid;
        }
        false
    };

    let mut herdr_bytes = 0u64;
    let mut agents_bytes = 0u64;
    let mut processes = 0u32;
    let mut seen: std::collections::HashSet<i32> = std::collections::HashSet::new();

    for root in procs.iter().filter(|p| is_herdr(p) && !has_herdr_ancestor(p)) {
        let mut stack = vec![root.pid];
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            let Some(p) = by_pid.get(&pid) else { continue };
            processes += 1;
            if is_herdr(p) {
                herdr_bytes += p.rss_kib * 1024;
            } else {
                agents_bytes += p.rss_kib * 1024;
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
    }

    HostMem {
        host: host.to_string(),
        herdr_bytes,
        agents_bytes,
        total_bytes: herdr_bytes + agents_bytes,
        processes,
        error: None,
    }
}

async fn sample_local(host: &str) -> HostMem {
    let out = tokio::process::Command::new("/bin/sh").arg("-c").arg(PS_CMD).output().await;
    match out {
        Ok(o) if o.status.success() => sum_herdr(&String::from_utf8_lossy(&o.stdout), host),
        Ok(o) => HostMem {
            host: host.to_string(),
            herdr_bytes: 0,
            agents_bytes: 0,
            total_bytes: 0,
            processes: 0,
            error: Some(format!("ps exited {}", o.status)),
        },
        Err(e) => HostMem {
            host: host.to_string(),
            herdr_bytes: 0,
            agents_bytes: 0,
            total_bytes: 0,
            processes: 0,
            error: Some(e.to_string()),
        },
    }
}

/// One sample of every host we can currently reach. A disconnected host is reported with an
/// `error` rather than dropped, so the UI can say "this host is not counted" instead of
/// silently showing a smaller number.
pub async fn sample(app: &Arc<App>) -> MemSnapshot {
    let mut hosts = Vec::new();
    for conn in app.hosts.list().await {
        let name = conn.name.clone();
        if conn.is_local() {
            hosts.push(sample_local(&name).await);
            continue;
        }
        if !conn.connected.load(std::sync::atomic::Ordering::Relaxed) {
            hosts.push(HostMem {
                host: name,
                herdr_bytes: 0,
                agents_bytes: 0,
                total_bytes: 0,
                processes: 0,
                error: Some("未連線".into()),
            });
            continue;
        }
        match conn.ssh_exec(PS_CMD).await {
            Ok(out) => hosts.push(sum_herdr(&out, &name)),
            Err(e) => hosts.push(HostMem {
                host: name,
                herdr_bytes: 0,
                agents_bytes: 0,
                total_bytes: 0,
                processes: 0,
                error: Some(format!("{e:#}")),
            }),
        }
    }

    MemSnapshot {
        total_bytes: hosts.iter().map(|h| h.total_bytes).sum(),
        herdr_bytes: hosts.iter().map(|h| h.herdr_bytes).sum(),
        agents_bytes: hosts.iter().map(|h| h.agents_bytes).sum(),
        processes: hosts.iter().map(|h| h.processes).sum(),
        hosts,
    }
}

/// Sample every `SAMPLE_EVERY` and push the result out over the socket. Only a change is
/// pushed: the number moves constantly by a few KiB and a frame every 15s per client for
/// that is noise. `GET /api/mem` still answers with the latest value at any time.
pub fn spawn_poller(app: Arc<App>) {
    tokio::spawn(async move {
        let mut last: Option<MemSnapshot> = None;
        loop {
            let snap = sample(&app).await;
            let changed = match &last {
                // Ignore drift below 1 MiB; it is never what the user is looking at.
                Some(prev) => prev.total_bytes.abs_diff(snap.total_bytes) >= 1024 * 1024 || prev.hosts.len() != snap.hosts.len(),
                None => true,
            };
            if changed {
                app.emit("mem_updated", json!(snap)).await;
                last = Some(snap);
            }
            tokio::time::sleep(SAMPLE_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "\
    1     0  12000 /sbin/launchd
  400     1  48000 /opt/homebrew/bin/herdr --session agents-manager
  401   400  30000 /bin/zsh -l
  402   401 820000 claude --dangerously-skip-permissions
  403   400 640000 codex --yolo
  500     1  20000 /usr/bin/ssh -N remote
  600     1   9000 grep herdr
";

    #[test]
    fn sums_the_tree_not_just_herdr() {
        let m = sum_herdr(PS, "local");
        assert_eq!(m.herdr_bytes, 48_000 * 1024);
        // zsh + claude + codex, not the unrelated ssh/grep.
        assert_eq!(m.agents_bytes, (30_000 + 820_000 + 640_000) * 1024);
        assert_eq!(m.total_bytes, m.herdr_bytes + m.agents_bytes);
        assert_eq!(m.processes, 4);
    }

    #[test]
    fn a_process_merely_mentioning_herdr_is_not_one() {
        // `grep herdr` has "herdr" in its argv but is not the herdr binary.
        let m = sum_herdr("  600     1   9000 grep herdr\n", "local");
        assert_eq!(m.total_bytes, 0);
        assert_eq!(m.processes, 0);
    }

    #[test]
    fn nested_herdr_is_counted_once() {
        let ps = "\
  400     1  48000 /opt/homebrew/bin/herdr --session a
  401   400  10000 herdr pane send
  402   401  5000 sh -c true
";
        let m = sum_herdr(ps, "local");
        assert_eq!(m.herdr_bytes, (48_000 + 10_000) * 1024);
        assert_eq!(m.agents_bytes, 5_000 * 1024);
        assert_eq!(m.processes, 3);
    }

    #[test]
    fn argv_with_spaces_survives_the_split() {
        let ps = "  400     1  48000 /opt/homebrew/bin/herdr --session my session\n";
        let m = sum_herdr(ps, "local");
        assert_eq!(m.herdr_bytes, 48_000 * 1024);
    }
}
