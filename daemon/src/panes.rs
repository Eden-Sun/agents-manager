//! 非 agent 的 shell／服務 pane：歸屬、分類與現況（SPEC §6.5e）。
//!
//! agent pane 由 `reconcile` 既有那套處理，這裡完全不碰（§6.9 的教訓）。這一支只認 herdr 說「沒有 agent」
//! 的 pane，把它們收進 `panes` 表：誰開的（pane 行程樹裡的 `AM_BOT_ID`）、在跑什麼、有沒有 listen port、
//! 上次動是什麼時候。GC 與孤兒通知是下一步（§6.5e 生命週期），這裡只負責看得見。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::state::App;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS panes (
           pane_id TEXT NOT NULL, host TEXT NOT NULL,
           workspace_id TEXT, tab_id TEXT, cwd TEXT,
           -- `service`（有前景程式或 listen port）／`shell`（只有 shell）。agent pane 不進這張表。
           kind TEXT NOT NULL,
           owner_bot_id TEXT, project_id TEXT, purpose TEXT,
           foreground TEXT, listen_ports TEXT,
           -- herdr 的 pane.revision：變了才算「有輸出」（§6.5e，不讀畫面內容）。
           last_revision INTEGER,
           last_output_at TEXT NOT NULL,
           first_seen TEXT NOT NULL, last_seen TEXT NOT NULL,
           orphan_notified_at TEXT,
           -- `bot`（AM_BOT_ID 推斷）／`user`（沒有標記但 cwd 對得到專案）／`none`（連專案都對不到）。
           owned_by TEXT NOT NULL DEFAULT 'none',
           unowned_notified_at TEXT,
           -- 使用者手開的 pane 只有人明確簽名（adopt allow_gc）才可自動關。
           gc_optin INTEGER NOT NULL DEFAULT 0,
           -- herdr 上的 pane 名字；scratch 靠它認（`[panes] scratch_name`）。
           label TEXT,
           -- 這一台那顆固定的 scratch（每輪完整掃描後重算，選中就黏住，不隨 first_seen 漂移）。
           scratch INTEGER NOT NULL DEFAULT 0,
           -- 環境（AM_PROJECT_ID，或 AM_BOT_ID 的專案）綁過的專案。讀不到環境的那幾輪（macOS 閒著的 -zsh）沿用。
           bound_project_id TEXT,
           -- 綁過的專案被刪、或擁有它的 bot 被刪（§6.5e 的 `pane_orphaned`）。孤兒不是 scratch 的候選。
           orphaned INTEGER NOT NULL DEFAULT 0,
           -- 人用 adopt 指定過 owner：掃描不再以環境覆寫它。
           owner_adopted INTEGER NOT NULL DEFAULT 0,
           PRIMARY KEY (host, pane_id)
         )",
    )
    .execute(pool)
    .await?;
    // 表可能是上一版建的：欄位 additive 補上（migrate 可重入）。
    for (col, ddl) in [
        ("owned_by", "ALTER TABLE panes ADD COLUMN owned_by TEXT NOT NULL DEFAULT 'none'"),
        ("unowned_notified_at", "ALTER TABLE panes ADD COLUMN unowned_notified_at TEXT"),
        ("label", "ALTER TABLE panes ADD COLUMN label TEXT"),
        ("scratch", "ALTER TABLE panes ADD COLUMN scratch INTEGER NOT NULL DEFAULT 0"),
        ("bound_project_id", "ALTER TABLE panes ADD COLUMN bound_project_id TEXT"),
        ("orphaned", "ALTER TABLE panes ADD COLUMN orphaned INTEGER NOT NULL DEFAULT 0"),
        ("owner_adopted", "ALTER TABLE panes ADD COLUMN owner_adopted INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !crate::db::has_column(pool, "panes", col).await? {
            sqlx::query(ddl).execute(pool).await?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    pub pane_id: String,
    pub kind: &'static str,
    pub owner_bot_id: Option<String>,
    pub foreground: Option<String>,
    pub listen_ports: Vec<u16>,
}

/// 分類（SPEC §6.5e）：有前景程式或 listen port＝`service`，其餘＝`shell`。
pub fn classify(foreground: Option<&str>, ports: &[u16]) -> &'static str {
    if foreground.is_some() || !ports.is_empty() {
        "service"
    } else {
        "shell"
    }
}

/// herdr 的 `pane.process_info` ＋ 一份 `ps` dump → 這顆 pane 的事實（§6.5e）。`None`＝判不出來：
/// herdr 沒報 `shell_pid`、那個 pid 不在樹裡。兩邊誰看到東西都算有：`ps` 的樹抓得到 root 的 `sudo`
/// （herdr 這時回空的前景），herdr 的前景行程組抓得到樹還來不及出現的那一個。
pub fn facts_from(shell: &crate::herdr::PaneShell, dump: &str, pane_id: &str) -> Option<crate::memproc::PaneFacts> {
    let pid = i32::try_from(shell.shell_pid?).ok()?;
    let mut f = crate::memproc::pane_facts_for_shell(dump, pane_id, pid)?;
    // 閒著的 shell，herdr 會把 shell 自己列成前景；其他任何一個（含 pid 不明的）都不是「只有 shell」。
    if let Some(p) = shell.foreground_processes.iter().flatten().find(|p| p.pid != Some(i64::from(pid))) {
        f.shell_only = false;
        if f.foreground.is_none() {
            let argv = if p.argv.is_empty() { p.argv0.clone().unwrap_or_default() } else { p.argv.join(" ") };
            f.foreground = Some(argv);
        }
    }
    Some(f)
}

async fn read_facts(
    client: Option<&crate::herdr::HerdrClient>,
    dump: Option<&str>,
    pane_id: &str,
) -> Option<crate::memproc::PaneFacts> {
    let (client, dump) = (client?, dump?);
    match client.pane_shell(pane_id).await {
        Ok(shell) => facts_from(&shell, dump, pane_id),
        Err(e) => {
            tracing::debug!(pane_id, error = %e, "pane.process_info failed");
            None
        }
    }
}

/// `panes.listen_ports` 欄（逗號分隔）→ port 清單。
pub fn parse_ports(s: Option<&str>) -> Vec<u16> {
    s.map(|s| s.split(',').filter_map(|p| p.trim().parse::<u16>().ok()).collect()).unwrap_or_default()
}

/// 本機才算 listen port：pane 行程樹的 pid 對 `lsof`，回這些 pid 合起來的 port（排序、去重）。遠端回空的
/// （§6.5e：不為了它多開 ssh 往返）。`None`＝`lsof` 起不來或逾時：**不是「沒有 port」**，呼叫端要當成讀不到
/// （以前這裡回空的，打字前的複查就等於放行）。
pub async fn listen_ports(host: &str, pids: &[i32]) -> Option<Vec<u16>> {
    if host != crate::config::LOCAL_HOST || pids.is_empty() {
        return Some(Vec::new());
    }
    let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let script = format!("lsof -nP -iTCP -sTCP:LISTEN -a -p {list} -Fpn 2>/dev/null");
    let Ok(Some(o)) = crate::hosts::sh_local(&script, std::time::Duration::from_secs(10)).await else { return None };
    let mut ports: Vec<u16> = parse_lsof(&String::from_utf8_lossy(&o.stdout)).into_values().flatten().collect();
    ports.sort_unstable();
    ports.dedup();
    Some(ports)
}

/// `lsof -Fpn` 是一行一個欄位：`p<pid>` 之後的 `n<addr>` 都屬於那個 pid。位址取最後一個 `:` 之後的數字。
pub fn parse_lsof(out: &str) -> HashMap<i32, Vec<u16>> {
    let mut by_pid: HashMap<i32, Vec<u16>> = HashMap::new();
    let mut cur: Option<i32> = None;
    for line in out.lines() {
        let (tag, rest) = line.split_at(line.char_indices().nth(1).map_or(line.len(), |(i, _)| i));
        match tag {
            "p" => cur = rest.trim().parse().ok(),
            "n" => {
                let Some(pid) = cur else { continue };
                let Some(port) = rest.rsplit(':').next().and_then(|p| p.trim().parse::<u16>().ok()) else { continue };
                let ports = by_pid.entry(pid).or_default();
                if !ports.contains(&port) {
                    ports.push(port);
                }
            }
            _ => {}
        }
    }
    for ports in by_pid.values_mut() {
        ports.sort_unstable();
    }
    by_pid
}

/// cwd 回退歸屬（§6.5e，AGM 2026-09-16 第 1 點）：沒有 `AM_BOT_ID` 時，用 pane 的 cwd 對專案路徑；
/// 子目錄也算，取**最長**的那個（巢狀專案時才不會歸錯）。回 `None` = 連專案都對不到。
pub fn project_for_cwd<'a>(cwd: &str, projects: &'a [(String, String)]) -> Option<&'a str> {
    let cwd = cwd.trim_end_matches('/');
    if cwd.is_empty() {
        return None;
    }
    projects
        .iter()
        .filter(|(_, path)| {
            let p = path.trim_end_matches('/');
            !p.is_empty() && (cwd == p || cwd.starts_with(&format!("{p}/")))
        })
        .max_by_key(|(_, path)| path.trim_end_matches('/').len())
        .map(|(id, _)| id.as_str())
}

/// 一輪掃描的結果。`complete=false`＝有 pane 的事實這一輪讀不到（行程 dump 或 herdr 失敗）：那幾列的分類與
/// 歸屬沿用上一輪，呼叫端這一輪不跑 GC 與通知（§6.5e：讀不到不等於是空的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    pub panes: usize,
    pub complete: bool,
    /// 這一輪選出來的 scratch 名字還不是 `scratch_name`：呼叫端去 herdr 改名（純顯示）。
    pub rename_scratch: Option<String>,
}

fn is_agent_pane(p: &Value) -> bool {
    p.get("agent").and_then(Value::as_str).is_some_and(|a| !a.is_empty())
}

fn label_of(p: &Value) -> Option<&str> {
    p.get("label").and_then(Value::as_str).filter(|l| !l.is_empty())
}

/// 這一輪對一顆 pane 實際讀到的東西。
#[derive(Debug, Clone, Default)]
pub struct Observed {
    pub facts: crate::memproc::PaneFacts,
    pub ports: Vec<u16>,
}

/// 掃一台主機的非 agent pane，寫進 `panes`。`snapshot_panes` 是 `session.snapshot` 的 `panes` 陣列
/// （已經含 `agent`），所以不用再打一次 RPC。
pub async fn scan_host(app: &Arc<App>, host: &str, snapshot_panes: &[Value]) -> Result<ScanOutcome> {
    // 對帳那一輪與定期那一輪不交錯寫同一張表（兩邊的 DELETE 會互相把對方剛記的列刪掉）。
    static SCAN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one_at_a_time = SCAN.lock().await;
    let non_agent: Vec<&Value> = snapshot_panes.iter().filter(|p| !is_agent_pane(p)).collect();
    // 環境快照：pane 行程樹的 `AM_BOT_ID` 就是歸屬（§6.5e）。讀不到就這一輪不更新歸屬，不要猜。
    let dump = match crate::memproc::dump(app, host).await {
        Ok(out) => Some(out),
        Err(e) => {
            tracing::warn!(host, error = %e, "讀不到行程環境，這一輪不更新 pane 歸屬");
            None
        }
    };
    let client = crate::api::shell::client_for(app, host).await.ok().map(|(c, _)| c);
    let mut observed: HashMap<String, Option<Observed>> = HashMap::new();
    for p in &non_agent {
        let Some(pane_id) = p.get("pane_id").and_then(Value::as_str) else { continue };
        let seen = match read_facts(client.as_ref(), dump.as_deref(), pane_id).await {
            // port 讀不到跟事實讀不到一樣：這一輪不改寫這顆。
            Some(facts) => listen_ports(host, &facts.pids).await.map(|ports| Observed { facts, ports }),
            None => None,
        };
        observed.insert(pane_id.to_string(), seen);
    }
    let scan = record_scan(app, host, snapshot_panes, &observed).await?;
    if let (Some(pane_id), Some(client)) = (&scan.rename_scratch, &client) {
        let name = app.cfg.get().await.panes.scratch_name.clone();
        match client.pane_rename(pane_id, &name).await {
            Ok(()) => tracing::info!(host, pane_id, name, "scratch pane renamed"),
            Err(e) => tracing::warn!(host, pane_id, error = %e, "could not rename the scratch pane"),
        }
    }
    Ok(scan)
}

/// 定期掃描的間隔。對帳只在事件觸發時跑（開機、重連、agent 被偵測），pane 裡開始跑 dev server 不會產生任何事件，
/// 表上的 kind／port 會一直是舊的（review 2026-09-16 core 4）。
const RESCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// 單獨重掃一台主機的 pane（不動 agent pane 的對帳、不跑 GC 與通知——那兩件事仍跟著對帳）。
pub async fn rescan(app: &Arc<App>, host: &str) -> Result<ScanOutcome> {
    let (client, _) = crate::api::shell::client_for(app, host).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let snapshot = client.snapshot().await?;
    // 同 reconcile 的規則：連 key 都沒有＝不認得的形狀，當成空的會把整台的列清光。
    let Some(panes) = snapshot.get("panes").and_then(Value::as_array) else {
        anyhow::bail!("session.snapshot on host `{host}` has no `panes`; skipping the pane scan");
    };
    scan_host(app, host, panes).await
}

pub fn spawn_scanner(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(RESCAN_INTERVAL).await;
            for host in app.hosts.names().await {
                if !app.host_connected(&host).await {
                    continue;
                }
                if let Err(e) = rescan(&app, &host).await {
                    tracing::debug!(host, error = %e, "periodic pane scan skipped");
                }
            }
        }
    });
}

/// [`scan_host`] 的寫入那半（可測：事實由呼叫端給）。`observed` 裡沒有、或是 `None` 的 pane＝這一輪讀不到。
pub(crate) async fn record_scan(
    app: &Arc<App>,
    host: &str,
    snapshot_panes: &[Value],
    observed: &HashMap<String, Option<Observed>>,
) -> Result<ScanOutcome> {
    let non_agent: Vec<&Value> = snapshot_panes.iter().filter(|p| !is_agent_pane(p)).collect();
    if non_agent.is_empty() {
        // 這台沒有非 agent pane：把舊的收乾淨（pane 已經關了）。
        sqlx::query("DELETE FROM panes WHERE host=?").bind(host).execute(&app.db).await?;
        return Ok(ScanOutcome { panes: 0, complete: true, rename_scratch: None });
    }
    let now = crate::db::now();
    // canonical path 比對用（§6.5e 的 cwd 回退）。
    let project_paths: Vec<(String, String)> = crate::db::live_projects(&app.db)
        .await?
        .into_iter()
        .filter(|p| p.host == host)
        .map(|p| (p.id, p.path))
        .collect();
    let mut seen = Vec::new();
    let mut complete = true;
    for p in &non_agent {
        let Some(pane_id) = p.get("pane_id").and_then(Value::as_str) else { continue };
        seen.push(pane_id.to_string());
        let revision = p.get("revision").and_then(Value::as_u64).map(|v| v as i64);
        let label = label_of(p);
        type Prev = (Option<i64>, String, String, Option<String>, Option<String>);
        let prev: Option<Prev> = sqlx::query_as(
            "SELECT last_revision, last_output_at, first_seen, bound_project_id, owner_bot_id FROM panes WHERE host=? AND pane_id=?",
        )
        .bind(host)
        .bind(pane_id)
        .fetch_optional(&app.db)
        .await?;
        // revision 變了才算「有輸出」；第一次看到就以 first_seen 當基準（§6.5e）。
        let (last_output_at, first_seen) = match &prev {
            Some((old_rev, out_at, first, _, _)) => {
                let moved = revision.is_some() && *old_rev != revision;
                ((if moved { now.clone() } else { out_at.clone() }), first.clone())
            }
            None => (now.clone(), now.clone()),
        };
        let cwd = p.get("foreground_cwd").or_else(|| p.get("cwd")).and_then(Value::as_str).unwrap_or("");

        let Some(Observed { facts: f, ports }) = observed.get(pane_id).and_then(Option::as_ref) else {
            // 讀不到事實：**不猜**。既有列只更新位置與輸出時間，kind／歸屬／前景／port 沿用上一輪；
            // 新列先當 service（不自動關、關要確認），歸屬只能靠 cwd（review 2026-09-16 core 3）。
            complete = false;
            if prev.is_some() {
                sqlx::query(
                    "UPDATE panes SET workspace_id=?, tab_id=?, cwd=?, label=?, last_revision=?, last_output_at=?, last_seen=?
                      WHERE host=? AND pane_id=?",
                )
                .bind(p.get("workspace_id").and_then(Value::as_str))
                .bind(p.get("tab_id").and_then(Value::as_str))
                .bind(p.get("cwd").and_then(Value::as_str))
                .bind(label)
                .bind(revision)
                .bind(&last_output_at)
                .bind(&now)
                .bind(host)
                .bind(pane_id)
                .execute(&app.db)
                .await?;
            } else {
                let project = project_for_cwd(cwd, &project_paths);
                sqlx::query(
                    "INSERT INTO panes (pane_id, host, workspace_id, tab_id, cwd, label, kind, project_id, last_revision,
                                        last_output_at, first_seen, last_seen, owned_by)
                     VALUES (?,?,?,?,?,?,'service',?,?,?,?,?,?)",
                )
                .bind(pane_id)
                .bind(host)
                .bind(p.get("workspace_id").and_then(Value::as_str))
                .bind(p.get("tab_id").and_then(Value::as_str))
                .bind(p.get("cwd").and_then(Value::as_str))
                .bind(label)
                .bind(project)
                .bind(revision)
                .bind(&last_output_at)
                .bind(&first_seen)
                .bind(&now)
                .bind(if project.is_some() { "user" } else { "none" })
                .execute(&app.db)
                .await?;
            }
            continue;
        };

        let kind = classify(f.foreground.as_deref(), ports);
        let (prev_bound, prev_owner) = prev.as_ref().map(|p| (p.3.clone(), p.4.clone())).unwrap_or_default();
        let env_bot = f.bot_ids.first().cloned();
        let owner = env_bot.clone();
        // 歸屬順序（§6.5e，使用者 2026-09-16 第 3 條裁示）：
        //   1. `AM_PROJECT_ID`——開 pane 當下就綁好的專案。**bot 被刪也不失效**，否則孤兒 pane 會掉成
        //      「非專案」，再撞上「只准一顆」的規則被當成多餘的那一顆。
        //   2. `AM_BOT_ID`——只補 owner 與顯示；它的專案只在第 1 條沒有時才拿來用。
        //   3. 兩個 env 都沒有才用 cwd 比對。
        //   4. 都對不到才是沒歸屬（scratch 的候選）。
        // 綁定（1、2）以讀得到的環境為準；這一輪讀不到這顆 pane 的環境（macOS 讀不到閒著的 `-zsh`）就沿用上一輪記下的，
        // 不然專案一刪、綁定跟著蒸發，孤兒就掉成「沒歸屬」去搶 scratch（review 2026-09-16 core 2）。
        let bound = if f.env_seen {
            match f.project_ids.iter().find(|id| !id.trim().is_empty()).cloned() {
                Some(id) => Some(id),
                None => match &env_bot {
                    Some(b) => crate::db::bot(&app.db, b).await.ok().flatten().map(|b| b.project_id),
                    None => None,
                },
            }
        } else {
            prev_bound
        };
        // 擁有它的 bot 被刪（或根本不在這顆 DB）也是孤兒。DB 讀不到就不下結論。
        let owner_bot = if f.env_seen { env_bot.clone() } else { prev_owner };
        let bot_gone = match &owner_bot {
            Some(b) => matches!(crate::db::bot(&app.db, b).await, Ok(None) | Ok(Some(crate::db::Bot { deleted_at: Some(_), .. }))),
            None => false,
        };
        let live = |id: &str| project_paths.iter().any(|(pid, _)| pid == id);
        let (project, owned_by, orphaned) = match &bound {
            Some(id) if live(id) => (Some(id.clone()), "bot", bot_gone),
            // 綁過的專案被刪了：沒歸屬，但它是孤兒（通知 `pane_orphaned`、照 GC），不是 scratch 的候選。
            Some(_) => (None, "none", true),
            None => match project_for_cwd(cwd, &project_paths) {
                Some(id) if owner.is_some() => (Some(id.to_string()), "bot", bot_gone),
                Some(id) => (Some(id.to_string()), "user", false),
                None => (None, "none", owner.is_some() && bot_gone),
            },
        };
        sqlx::query(
            "INSERT INTO panes (pane_id, host, workspace_id, tab_id, cwd, label, kind, owner_bot_id, project_id,
                                foreground, listen_ports, last_revision, last_output_at, first_seen, last_seen, owned_by,
                                bound_project_id, orphaned)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(host, pane_id) DO UPDATE SET
               workspace_id=excluded.workspace_id, tab_id=excluded.tab_id, cwd=excluded.cwd, label=excluded.label,
               kind=excluded.kind,
               -- owner 以讀到的 `AM_BOT_ID` 為準（§6.5e：歸屬永遠由環境決定，shim 回報先到也蓋得過去）；
               -- 這一輪讀不到就沿用；人工 adopt 指定過的不被掃描蓋掉。
               owner_bot_id=CASE WHEN panes.owner_adopted=1 THEN panes.owner_bot_id
                                 ELSE COALESCE(excluded.owner_bot_id, panes.owner_bot_id) END,
               -- 每輪以 env 為準覆寫（§6.5e）：綁定來自 pane 的環境，不是我們記住的舊值。
               project_id=excluded.project_id, bound_project_id=excluded.bound_project_id,
               foreground=excluded.foreground, listen_ports=excluded.listen_ports,
               last_revision=excluded.last_revision, last_output_at=excluded.last_output_at, last_seen=excluded.last_seen,
               owned_by=excluded.owned_by, orphaned=excluded.orphaned,
               -- 歸屬回來了（或它其實是孤兒）就把「沒歸屬」的通知標記清掉。
               unowned_notified_at=CASE WHEN excluded.owned_by='none' AND excluded.orphaned=0
                                        THEN panes.unowned_notified_at ELSE NULL END,
               -- 不再是孤兒就把孤兒標記清掉，下次真的變孤兒才會再通知一次。
               orphan_notified_at=CASE WHEN excluded.orphaned=0 THEN NULL ELSE panes.orphan_notified_at END",
        )
        .bind(pane_id)
        .bind(host)
        .bind(p.get("workspace_id").and_then(Value::as_str))
        .bind(p.get("tab_id").and_then(Value::as_str))
        .bind(p.get("cwd").and_then(Value::as_str))
        .bind(label)
        .bind(kind)
        .bind(owner.as_deref())
        .bind(project.as_deref())
        .bind(f.foreground.as_deref())
        .bind(if ports.is_empty() { None } else { Some(ports.iter().map(u16::to_string).collect::<Vec<_>>().join(",")) })
        .bind(revision)
        .bind(&last_output_at)
        .bind(&first_seen)
        .bind(&now)
        .bind(owned_by)
        .bind(bound.as_deref())
        .bind(orphaned)
        .execute(&app.db)
        .await?;
    }
    // 不見了的 pane：herdr 說它不在了，就從表裡拿掉（下次再出現會重新記 first_seen）。
    let keep = seen.iter().map(|s| format!("'{}'", s.replace('\'', "''"))).collect::<Vec<_>>().join(",");
    sqlx::query(&format!("DELETE FROM panes WHERE host=? AND pane_id NOT IN ({keep})"))
        .bind(host)
        .execute(&app.db)
        .await?;
    // scratch 只在完整的一輪重選：不完整時 kind／歸屬是上一輪的，拿來選會讓它來回跳。
    let mut rename_scratch = None;
    if complete {
        let name = app.cfg.get().await.panes.scratch_name.clone();
        let cands: Vec<ScratchCandidate> = sqlx::query_as(
            "SELECT pane_id, kind, owned_by, orphaned, label, scratch, first_seen FROM panes WHERE host=?",
        )
        .bind(host)
        .fetch_all(&app.db)
        .await?;
        let name_in_use = !name.is_empty() && snapshot_panes.iter().any(|p| label_of(p) == Some(name.as_str()));
        let pick = pick_scratch(&cands, &name, name_in_use);
        sqlx::query("UPDATE panes SET scratch = CASE WHEN pane_id = ? THEN 1 ELSE 0 END WHERE host=?")
            .bind(pick.map(|c| c.pane_id.as_str()).unwrap_or(""))
            .bind(host)
            .execute(&app.db)
            .await?;
        rename_scratch = pick.filter(|c| !name.is_empty() && c.label.as_deref() != Some(name.as_str())).map(|c| c.pane_id.clone());
    }
    Ok(ScanOutcome { panes: seen.len(), complete, rename_scratch })
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ScratchCandidate {
    pub pane_id: String,
    pub kind: String,
    pub owned_by: String,
    pub orphaned: bool,
    pub label: Option<String>,
    pub scratch: bool,
    pub first_seen: String,
}

/// 這一台那顆固定的 scratch（§6.5e「連 cwd 都對不到任何專案的 shell pane：全機只准有一顆」）。
///
/// 候選只有「沒歸屬、不是孤兒」的 pane，依序：
/// 1. 名字就是 `scratch_name` 的（SPEC：scratch 的名字固定，靠名字認，不隨 first_seen 漂移）；
/// 2. 上一輪已經選中的（改名失敗時靠這一欄黏住；裡面暫時跑著 htop 變成 service 也還是它）；
/// 3. `scratch_name` 被一顆不是候選的 pane 佔著（例如 scratch 裡正在跑 claude，這一輪是 agent pane）→ 這一輪**不選**，
///    不然會有另一顆被扶正、改名，原本那顆回來反而變成多出來的；
/// 4. 否則在 `kind='shell'` 裡取 first_seen 最早的——跑著 `tail -f` 的 service pane 不能搶走這個位置（review core 2）。
pub(crate) fn pick_scratch<'a>(cands: &'a [ScratchCandidate], name: &str, name_in_use: bool) -> Option<&'a ScratchCandidate> {
    let eligible = || cands.iter().filter(|c| c.owned_by == "none" && !c.orphaned);
    let earliest = |it: &mut dyn Iterator<Item = &'a ScratchCandidate>| {
        it.min_by(|a, b| a.first_seen.cmp(&b.first_seen).then_with(|| a.pane_id.cmp(&b.pane_id)))
    };
    if !name.is_empty() {
        if let Some(c) = earliest(&mut eligible().filter(|c| c.label.as_deref() == Some(name))) {
            return Some(c);
        }
    }
    if let Some(c) = earliest(&mut eligible().filter(|c| c.scratch)) {
        return Some(c);
    }
    if name_in_use {
        return None;
    }
    earliest(&mut eligible().filter(|c| c.kind == "shell"))
}

/// shim 回報的用途：pane 還沒被掃到就先建一列（`kind` 先當 shell，下一輪掃描會修正）。
/// owner 只在這一列還沒有 owner 時才寫——回報不能改寫別人的 pane；之後掃描讀到的 `AM_BOT_ID` 會蓋過它。
/// 綁定（`bound_project_id`）同樣只補空的：讀不到那顆 pane 環境的輪次（macOS 閒著的 -zsh）才靠它知道是 bot 開的。
pub async fn note_purpose(app: &Arc<App>, host: &str, pane_id: &str, bot: &crate::db::Bot, purpose: &str) -> Result<()> {
    if pane_id.trim().is_empty() {
        return Ok(());
    }
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO panes (pane_id, host, kind, owner_bot_id, project_id, bound_project_id, purpose, last_output_at, first_seen, last_seen)
         VALUES (?,?,'shell',?,?,?,?,?,?,?)
         ON CONFLICT(host, pane_id) DO UPDATE SET
           purpose=CASE WHEN excluded.purpose IS NULL OR excluded.purpose='' THEN panes.purpose ELSE excluded.purpose END,
           owner_bot_id=COALESCE(panes.owner_bot_id, excluded.owner_bot_id),
           project_id=COALESCE(panes.project_id, excluded.project_id),
           bound_project_id=COALESCE(panes.bound_project_id, excluded.bound_project_id)",
    )
    .bind(pane_id)
    .bind(host)
    .bind(&bot.id)
    .bind(&bot.project_id)
    .bind(&bot.project_id)
    .bind(if purpose.is_empty() { None } else { Some(purpose) })
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(&app.db)
    .await?;
    tracing::info!(host, pane_id, bot = %bot.id, purpose, "pane purpose reported by the shim");
    Ok(())
}

/// 一輪 GC（§6.5e 生命週期）。只碰 `shell`，而且只碰可以碰的：
/// * 有歸屬（`owned_by='bot'`）、或使用者簽過名（`gc_optin`）、或「多出來的沒歸屬 pane」。
/// * scratch（沒歸屬的那唯一一顆，名字固定）永不自動關。
/// 三條守門：行程樹只有 shell、關前重新取值（取不到就不關）、關前把畫面最後幾行記進 log。
pub async fn gc_host(app: &Arc<App>, host: &str) -> Result<usize> {
    let cfg = app.cfg.get().await;
    let idle_limit = cfg.panes.idle_close_secs() as i64;
    let log_lines = cfg.panes.close_log_lines;
    type GcRow = (String, String, Option<String>, Option<String>, String, String, i64, Option<String>, bool);
    let rows = sqlx::query_as::<_, GcRow>(
        "SELECT pane_id, kind, workspace_id, tab_id, last_output_at, first_seen, gc_optin, owned_by, scratch FROM panes WHERE host=?",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await?;
    let mut closed = 0;
    for (pane_id, kind, ws, tab, last_output_at, first_seen, gc_optin, owned_by, scratch) in rows {
        if let Some(why) = gc_skip(&kind, scratch, owned_by.as_deref(), gc_optin, &last_output_at, &first_seen, idle_limit) {
            if why == "output_unmeasured" {
                tracing::debug!(host, pane_id, "pane GC 跳過：輸出訊號從沒動過，量不到閒置多久");
            }
            continue;
        }
        match close_if_still_idle(app, host, &pane_id, ws.as_deref(), tab.as_deref(), log_lines).await {
            Ok(true) => closed += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(host, pane_id, error = ?e, "pane GC 放棄這一顆"),
        }
    }
    Ok(closed)
}

/// 這一顆為什麼不是 GC 候選（`None`＝是候選，接著走關前的三條守門）。純函式，規則全在這裡。
pub(crate) fn gc_skip(
    kind: &str,
    scratch: bool,
    owned_by: Option<&str>,
    gc_optin: i64,
    last_output_at: &str,
    first_seen: &str,
    idle_limit: i64,
) -> Option<&'static str> {
    if kind != "shell" {
        return Some("not_a_shell");
    }
    // 那顆固定的 scratch 永不自動關；其餘沒歸屬的才受 GC（使用者「只准一顆」的裁示）。
    if scratch {
        return Some("scratch");
    }
    // 使用者手開的（cwd 對得到專案）預設不關，除非 adopt 時簽過名。
    if owned_by == Some("user") && gc_optin == 0 {
        return Some("user_pane");
    }
    if seconds_since(last_output_at) < idle_limit {
        return Some("recent_output");
    }
    // 輸出訊號從沒動過＝**量不到**，不是「閒置 6 小時」（2026-09-16 實測 herdr 0.8.2：一直在輸出的 claude pane，
    // `pane.get` 的 revision 10 秒內都不變、`pane.read` 的是 0）。沒有這條，GC 實際上是「第一次看到超過 6 小時、
    // 此刻剛好停在提示字元」就關——使用者剛在裡面打過指令的 pane 也算。讀不到就不關（同 §6.5e 三條守門）。
    if last_output_at == first_seen {
        return Some("output_unmeasured");
    }
    None
}

pub(crate) fn seconds_since(at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(0)
}

/// 關之前再確認一次（§6.5e 的三條守門）。回 `false` = 這一輪不關。
async fn close_if_still_idle(
    app: &Arc<App>,
    host: &str,
    pane_id: &str,
    workspace_id: Option<&str>,
    tab_id: Option<&str>,
    log_lines: u32,
) -> Result<bool> {
    // 1. 重新取前景／行程樹：**讀不到就不關**（讀不到不等於是空的）。
    let dump = crate::memproc::dump(app, host).await?;
    let (client, _) = crate::api::shell::client_for(app, host).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let Some(f) = read_facts(Some(&client), Some(&dump), pane_id).await else {
        tracing::info!(host, pane_id, "GC 前讀不到這顆 pane 的行程樹，這一輪不關");
        return Ok(false);
    };
    if !f.shell_only {
        return Ok(false);
    }
    // port 讀不到也不關。
    if listen_ports(host, &f.pids).await.is_none_or(|ports| !ports.is_empty()) {
        return Ok(false);
    }
    // 2. 關之前把畫面最後幾行記進 log：自動關不可逆，出事要說得出關掉的是什麼。
    let tail = client
        .pane_read(pane_id, "recent_unwrapped", log_lines)
        .await
        .map(|r| r.text.lines().rev().take(log_lines as usize).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|e| format!("（讀不到畫面：{e}）"));
    tracing::info!(host, pane_id, workspace_id, tab_id, screen_tail = %tail, "pane GC：閒置太久，關掉這顆 shell pane");
    crate::lifecycle::close_pane_and_tab(&client, workspace_id, tab_id, pane_id).await;
    sqlx::query("DELETE FROM panes WHERE host=? AND pane_id=?").bind(host).bind(pane_id).execute(&app.db).await?;
    Ok(true)
}

/// 孤兒與「該歸屬而沒歸屬」的通知，各自只發一次（§6.5e 去重）。
///
/// - `pane_unowned`：沒歸屬、不是孤兒、也不是 scratch 的（「多出來的」）。
/// - `pane_orphaned`：綁過的專案被刪、或擁有它的 bot 被刪。帶前景、port、最後輸出時間，由人決定（service 不自動關，
///   shell 照 GC）。
///
/// inbox 的 key 帶 `first_seen`：herdr 重開後 pane id 會重用，不能讓舊 pane 用掉的 key 擋住新 pane 的通知。
pub async fn notify_unowned_and_orphans(app: &Arc<App>, host: &str) -> Result<usize> {
    type NotifyRow = (String, String, Option<String>, Option<String>, Option<String>, String, String, Option<String>, Option<String>, bool);
    let rows = sqlx::query_as::<_, NotifyRow>(
        "SELECT pane_id, kind, workspace_id, foreground, listen_ports, last_output_at, first_seen,
                owner_bot_id, bound_project_id, orphaned
           FROM panes
          WHERE host=? AND scratch=0
            AND ((orphaned=1 AND orphan_notified_at IS NULL)
              OR (orphaned=0 AND owned_by='none' AND unowned_notified_at IS NULL))",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await?;
    let mut sent = 0;
    for (pane_id, kind, ws, fg, ports, last_output_at, first_seen, owner, bound, orphaned) in rows {
        let (event, column, message) = if orphaned {
            ("pane_orphaned", "orphan_notified_at", "這顆 pane 綁過的專案或擁有它的 bot 已經刪掉了；service 不會自動關，要不要關由人決定")
        } else {
            ("pane_unowned", "unowned_notified_at", "這顆 pane 對不到任何專案，而且不是那顆固定的 scratch")
        };
        let payload = json!({
            "host": host, "pane_id": pane_id, "kind": kind, "workspace_id": ws,
            "foreground": fg, "listen_ports": ports, "last_output_at": last_output_at,
            "owner_bot_id": owner, "project_id": bound,
            "message": message,
        });
        let key = format!("{event}:{host}:{pane_id}:{first_seen}");
        if crate::supervisor::store::push_inbox(&app.db, &key, event, None, None, None, &payload).await?.is_some() {
            sent += 1;
        }
        sqlx::query(&format!("UPDATE panes SET {column}=? WHERE host=? AND pane_id=?"))
            .bind(crate::db::now())
            .bind(host)
            .bind(&pane_id)
            .execute(&app.db)
            .await?;
    }
    Ok(sent)
}

pub fn row_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    use sqlx::Row;
    let ports = parse_ports(r.get::<Option<String>, _>("listen_ports").as_deref());
    let host: String = r.get("host");
    let kind: String = r.get("kind");
    // 跟 `shell::typing_decision` 同一條：遠端不算 port，退回表上的 kind（service 就唯讀）。
    let read_only = !ports.is_empty() || (host != crate::config::LOCAL_HOST && kind == "service");
    json!({
        "pane_id": r.get::<String, _>("pane_id"),
        "host": host,
        "workspace_id": r.get::<Option<String>, _>("workspace_id"),
        "tab_id": r.get::<Option<String>, _>("tab_id"),
        "cwd": r.get::<Option<String>, _>("cwd"),
        "kind": kind,
        "owner_bot_id": r.get::<Option<String>, _>("owner_bot_id"),
        "project_id": r.get::<Option<String>, _>("project_id"),
        "purpose": r.get::<Option<String>, _>("purpose"),
        "foreground": r.get::<Option<String>, _>("foreground"),
        // 打字權限（`shell::typing_decision`）：本機看 port，遠端退回 kind。前端直接用這一欄決定鎖不鎖輸入框。
        "read_only": read_only,
        "listen_ports": ports,
        "last_output_at": r.get::<String, _>("last_output_at"),
        "first_seen": r.get::<String, _>("first_seen"),
        "last_seen": r.get::<String, _>("last_seen"),
        "gc_optin": r.get::<i64, _>("gc_optin") != 0,
        "owned_by": r.get::<String, _>("owned_by"),
        "label": r.get::<Option<String>, _>("label"),
        "scratch": r.get::<bool, _>("scratch"),
        "orphaned": r.get::<bool, _>("orphaned"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-16 實測：herdr 0.8.2 的 revision 對一直在輸出的 pane 也不動。量不到的 pane 不能被當成閒置關掉。
    #[test]
    fn a_pane_whose_output_signal_never_moved_is_not_idle_it_is_unmeasured() {
        let old = "2026-09-01T01:00:00.000Z";
        let later = "2026-09-01T05:00:00.000Z";
        let six_h = 21600;
        assert_eq!(gc_skip("shell", false, Some("bot"), 0, old, old, six_h), Some("output_unmeasured"), "從沒動過：量不到，不關");
        assert_eq!(gc_skip("shell", false, Some("bot"), 0, later, old, six_h), None, "動過而且閒置夠久才是候選");
        // 其他規則照舊。
        assert_eq!(gc_skip("service", false, Some("bot"), 0, later, old, six_h), Some("not_a_shell"));
        assert_eq!(gc_skip("shell", true, Some("none"), 0, later, old, six_h), Some("scratch"));
        assert_eq!(gc_skip("shell", false, Some("user"), 0, later, old, six_h), Some("user_pane"));
        assert_eq!(gc_skip("shell", false, Some("user"), 1, later, old, six_h), None, "簽過名的手開 pane 才算");
        let now = crate::db::now();
        assert_eq!(gc_skip("shell", false, Some("bot"), 0, &now, old, six_h), Some("recent_output"));
    }

    /// schema 變更（additive）：上一版的 `panes` 表（沒有 label／scratch／bound_project_id／orphaned／owner_adopted）
    /// 開機 migrate 後補齊，舊列照樣讀得出 pane 列（`row_json` 會讀這幾欄）。
    #[tokio::test]
    async fn an_old_panes_table_gains_the_new_columns_on_migrate() {
        let dir = std::env::temp_dir().join(format!("am-panes-old-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=rwc", dir.join("old.sqlite3").display())).await.unwrap();
        sqlx::query(
            "CREATE TABLE panes (
               pane_id TEXT NOT NULL, host TEXT NOT NULL, workspace_id TEXT, tab_id TEXT, cwd TEXT, kind TEXT NOT NULL,
               owner_bot_id TEXT, project_id TEXT, purpose TEXT, foreground TEXT, listen_ports TEXT, last_revision INTEGER,
               last_output_at TEXT NOT NULL, first_seen TEXT NOT NULL, last_seen TEXT NOT NULL, orphan_notified_at TEXT,
               owned_by TEXT NOT NULL DEFAULT 'none', unowned_notified_at TEXT, gc_optin INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (host, pane_id))",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO panes (pane_id, host, kind, listen_ports, last_output_at, first_seen, last_seen) VALUES ('w1:p1','local','service','3010','t','t','t')")
            .execute(&pool)
            .await
            .unwrap();
        migrate(&pool).await.expect("舊表補欄位");
        migrate(&pool).await.expect("可重入");
        for col in ["label", "scratch", "bound_project_id", "orphaned", "owner_adopted"] {
            assert!(crate::db::has_column(&pool, "panes", col).await.unwrap(), "{col}");
        }
        let row = sqlx::query("SELECT * FROM panes").fetch_one(&pool).await.unwrap();
        let v = row_json(&row);
        assert_eq!((v["scratch"].as_bool(), v["orphaned"].as_bool(), v["read_only"].as_bool()), (Some(false), Some(false), Some(true)));
        pool.close().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_pane_with_a_foreground_program_or_a_port_is_a_service() {
        assert_eq!(classify(None, &[]), "shell");
        assert_eq!(classify(Some("next dev"), &[]), "service");
        assert_eq!(classify(None, &[3010]), "service", "只有 port 也算服務");
    }

    #[test]
    fn lsof_fields_are_grouped_by_pid() {
        let out = "p4242\nn*:3010\nn127.0.0.1:5432\np99\nn[::1]:8080\n";
        let got = parse_lsof(out);
        assert_eq!(got[&4242], vec![3010, 5432]);
        assert_eq!(got[&99], vec![8080]);
        assert!(parse_lsof("").is_empty());
    }

    async fn app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("am-panes-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::db::open(&dir.join("t.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        Arc::new(App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false))
            .as_ref()
            .clone()
            .into()
    }

    fn pane(id: &str, agent: Option<&str>, rev: u64) -> Value {
        json!({"pane_id": id, "workspace_id": "w1", "tab_id": "t1", "cwd": "/tmp", "agent": agent, "revision": rev})
    }

    /// 掃描只收非 agent pane；revision 沒變就不更新 last_output_at，變了才算「有輸出」（§6.5e）。
    #[tokio::test]
    async fn the_scan_keeps_agent_panes_out_and_tracks_output_by_revision() {
        let app = app().await;
        let panes = vec![pane("w1:pA", Some("claude"), 3), pane("w1:pB", None, 7)];
        let scan = scan_host(&app, "local", &panes).await.unwrap();
        assert_eq!(scan.panes, 1, "只收非 agent pane");
        assert!(!scan.complete, "測試裡沒有 herdr：讀不到事實，這一輪不能拿來跑 GC");
        let owned: String = sqlx::query_scalar("SELECT owned_by FROM panes WHERE pane_id='w1:pB'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(owned, "none", "沒有 AM_BOT_ID、cwd 也對不到專案");
        let (kind, rev, out_at, first): (String, Option<i64>, String, String) =
            sqlx::query_as("SELECT kind, last_revision, last_output_at, first_seen FROM panes WHERE pane_id='w1:pB'")
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!((kind.as_str(), rev), ("service", Some(7)), "讀不到事實的新列先當 service");
        assert_eq!(out_at, first, "第一次看到就以 first_seen 當基準");

        // revision 沒變：last_output_at 不動。
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        scan_host(&app, "local", &panes).await.unwrap();
        let same: String = sqlx::query_scalar("SELECT last_output_at FROM panes WHERE pane_id='w1:pB'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(same, out_at, "沒動就不算有輸出");

        // revision 變了：last_output_at 往前推。
        let moved = vec![pane("w1:pA", Some("claude"), 3), pane("w1:pB", None, 8)];
        scan_host(&app, "local", &moved).await.unwrap();
        let newer: String = sqlx::query_scalar("SELECT last_output_at FROM panes WHERE pane_id='w1:pB'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(newer > out_at, "{newer} > {out_at}");

        // pane 不見了就從表裡拿掉。
        assert_eq!(scan_host(&app, "local", &[pane("w1:pA", Some("claude"), 3)]).await.unwrap().panes, 0);
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes").fetch_one(&app.db).await.unwrap();
        assert_eq!(left, 0);
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    async fn project_and_bot(app: &Arc<App>) {
        let now = crate::db::now();
        sqlx::query("INSERT INTO projects (id,path,label,host,created_at) VALUES ('p1','/tmp/p1','p1','local',?)")
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b1','p1','b1','claude','t',?)")
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
    }

    fn observed(foreground: Option<&str>, bot: Option<&str>, project: Option<&str>, ports: &[u16]) -> Option<Observed> {
        Some(Observed {
            facts: crate::memproc::PaneFacts {
                bot_ids: bot.map(|b| vec![b.to_string()]).unwrap_or_default(),
                project_ids: project.map(|p| vec![p.to_string()]).unwrap_or_default(),
                pids: vec![1],
                shell_only: foreground.is_none(),
                foreground: foreground.map(String::from),
                env_seen: bot.is_some() || project.is_some(),
            },
            ports: ports.to_vec(),
        })
    }

    type Row = (String, String, Option<String>, Option<String>, Option<String>, Option<String>);

    async fn row(app: &Arc<App>, id: &str) -> Row {
        sqlx::query_as("SELECT kind, owned_by, project_id, owner_bot_id, foreground, listen_ports FROM panes WHERE pane_id=?")
            .bind(id)
            .fetch_one(&app.db)
            .await
            .unwrap()
    }

    /// review 2026-09-16 core 3：遠端 ssh 抖一下、或 herdr 這一輪沒回 process_info，事實就是空的。
    /// 那一輪不能把 dev server 改寫成 shell（UI 就能對它按 Ctrl-C、關閉不用確認），也不能把 bot 的 pane
    /// 改成沒歸屬（推一則不實的 pane_unowned、讓 scratch 漂移）。
    #[tokio::test]
    async fn a_round_without_facts_keeps_the_last_known_kind_and_owner() {
        let app = app().await;
        project_and_bot(&app).await;
        let dev = json!({"pane_id": "w1:pDev", "workspace_id": "w1", "tab_id": "t1", "cwd": "/elsewhere", "revision": 1});
        let known = HashMap::from([("w1:pDev".to_string(), observed(Some("next dev"), Some("b1"), Some("p1"), &[3010]))]);
        let scan = record_scan(&app, "local", &[dev], &known).await.unwrap();
        assert!(scan.complete);
        let before = row(&app, "w1:pDev").await;
        assert_eq!((before.0.as_str(), before.1.as_str(), before.2.as_deref()), ("service", "bot", Some("p1")));

        // 這一輪讀不到：什麼都不改寫。
        let moved = json!({"pane_id": "w1:pDev", "workspace_id": "w2", "tab_id": "t9", "cwd": "/elsewhere", "revision": 2});
        let fresh = json!({"pane_id": "w1:pNew", "workspace_id": "w1", "tab_id": "t1", "cwd": "/elsewhere", "revision": 1});
        let unknown = HashMap::from([("w1:pDev".to_string(), None)]);
        let scan = record_scan(&app, "local", &[moved, fresh], &unknown).await.unwrap();
        assert_eq!(scan, ScanOutcome { panes: 2, complete: false, rename_scratch: None });
        assert_eq!(row(&app, "w1:pDev").await, before, "kind／歸屬／前景／port 沿用上一輪");
        let ws: String = sqlx::query_scalar("SELECT workspace_id FROM panes WHERE pane_id='w1:pDev'").fetch_one(&app.db).await.unwrap();
        assert_eq!(ws, "w2", "位置照樣更新");
        let new_row = row(&app, "w1:pNew").await;
        assert_eq!((new_row.0.as_str(), new_row.1.as_str()), ("service", "none"), "新列先當 service，不會被 GC 當成閒置 shell");
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    fn cand(id: &str, kind: &str, owned_by: &str, first_seen: &str) -> ScratchCandidate {
        ScratchCandidate {
            pane_id: id.into(),
            kind: kind.into(),
            owned_by: owned_by.into(),
            orphaned: false,
            label: None,
            scratch: false,
            first_seen: first_seen.into(),
        }
    }

    /// review 2026-09-16 core 2 情境 A：先在 `~` 開一顆跑 `tail -f` 的 pane，後來才開當雜事用的 zsh。
    /// 以前 service 那顆因為比較早被選成 scratch，真正的 scratch 反而成了「多出來的」被 GC。
    #[test]
    fn the_scratch_is_a_shell_found_by_name_and_it_sticks() {
        let tail = cand("w1:pTail", "service", "none", "2026-09-16T00:00:00Z");
        let zsh = cand("w1:pZsh", "shell", "none", "2026-09-16T01:00:00Z");
        let pick = |c: &[ScratchCandidate], in_use: bool| pick_scratch(c, "scratch", in_use).map(|c| c.pane_id.clone());
        assert_eq!(pick(&[tail.clone(), zsh.clone()], false).as_deref(), Some("w1:pZsh"), "service 不能搶 scratch");

        // 名字就是 scratch 的那顆優先，比它早的 shell 也搶不走。
        let early = cand("w1:pEarly", "shell", "none", "2026-09-15T00:00:00Z");
        let named = ScratchCandidate { label: Some("scratch".into()), ..zsh.clone() };
        assert_eq!(pick(&[early.clone(), named.clone()], true).as_deref(), Some("w1:pZsh"));

        // 選中之後就黏住：裡面暫時跑 htop 變成 service 也還是它（改名失敗時靠這一欄）。
        let busy = ScratchCandidate { kind: "service".into(), scratch: true, ..zsh.clone() };
        assert_eq!(pick(&[early.clone(), busy], false).as_deref(), Some("w1:pZsh"));

        // 孤兒（綁過的專案被刪）與有歸屬的都不是候選。
        let orphan = ScratchCandidate { orphaned: true, ..early.clone() };
        let owned = cand("w1:pUser", "shell", "user", "2026-09-14T00:00:00Z");
        assert_eq!(pick(&[orphan, owned, zsh.clone()], false).as_deref(), Some("w1:pZsh"));

        // 名字被一顆不是候選的 pane 佔著（scratch 裡正在跑 claude）：這一輪不扶正別人。
        assert_eq!(pick(&[early, zsh], true), None);
    }

    /// review 2026-09-16 core 2 情境 B：刪掉專案 P，P 的 bot 開的 build shell 以前會變成「沒歸屬」而且比
    /// 使用者的 scratch 早，於是孤兒永不 GC、使用者的 scratch 反而被關。現在它是孤兒：不當 scratch、推 pane_orphaned。
    /// macOS 讀不到閒著的 `-zsh` 的環境，所以綁定要沿用上一輪記下的。
    #[tokio::test]
    async fn a_deleted_projects_pane_is_an_orphan_not_the_scratch() {
        let app = app().await;
        project_and_bot(&app).await;
        let build = json!({"pane_id": "w1:pBuild", "workspace_id": "w1", "tab_id": "t1", "cwd": "/tmp/p1", "revision": 1});
        let facts = HashMap::from([("w1:pBuild".to_string(), observed(None, Some("b1"), Some("p1"), &[]))]);
        let scan = record_scan(&app, "local", &[build.clone()], &facts).await.unwrap();
        assert_eq!(scan.rename_scratch, None, "有歸屬的不是 scratch");

        sqlx::query("UPDATE projects SET deleted_at=? WHERE id='p1'").bind(crate::db::now()).execute(&app.db).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        // 這一輪讀得到事實，但讀不到 build shell 的環境（閒著的 -zsh）；使用者這時才開了雜事 pane。
        let mine = json!({"pane_id": "w1:pMine", "workspace_id": "w1", "tab_id": "t2", "cwd": "/Users/me", "revision": 1});
        let idle = |id: &str| (id.to_string(), Some(Observed { facts: crate::memproc::PaneFacts { pids: vec![1], shell_only: true, ..Default::default() }, ports: vec![] }));
        let facts = HashMap::from([idle("w1:pBuild"), idle("w1:pMine")]);
        let scan = record_scan(&app, "local", &[build, mine], &facts).await.unwrap();
        assert!(scan.complete);
        assert_eq!(scan.rename_scratch.as_deref(), Some("w1:pMine"), "使用者那顆才是 scratch，而且要改名");

        let (owned_by, orphaned, scratch): (String, bool, bool) =
            sqlx::query_as("SELECT owned_by, orphaned, scratch FROM panes WHERE pane_id='w1:pBuild'").fetch_one(&app.db).await.unwrap();
        assert_eq!((owned_by.as_str(), orphaned, scratch), ("none", true, false));

        assert_eq!(notify_unowned_and_orphans(&app, "local").await.unwrap(), 1);
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM supervisor_inbox").fetch_all(&app.db).await.unwrap();
        assert_eq!(kinds, vec!["pane_orphaned".to_string()], "孤兒推 pane_orphaned；scratch 不推");
        assert_eq!(notify_unowned_and_orphans(&app, "local").await.unwrap(), 0, "同一顆只推一次");
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// cwd 回退歸屬：子目錄也算，巢狀時取最長的那個；對不到就是 none（§6.5e 第 1 點）。
    #[test]
    fn a_pane_without_a_bot_marker_falls_back_to_its_cwd() {
        let projects = vec![
            ("p-outer".to_string(), "/Users/m4p/project".to_string()),
            ("p-am".to_string(), "/Users/m4p/project/agents-manager".to_string()),
        ];
        assert_eq!(project_for_cwd("/Users/m4p/project/agents-manager", &projects), Some("p-am"));
        assert_eq!(project_for_cwd("/Users/m4p/project/agents-manager/web/src", &projects), Some("p-am"), "子目錄算");
        assert_eq!(project_for_cwd("/Users/m4p/project/other", &projects), Some("p-outer"), "取得到的最長那個");
        assert_eq!(project_for_cwd("/tmp", &projects), None, "對不到就是沒歸屬");
        assert_eq!(project_for_cwd("", &projects), None);
        // 前綴不是路徑邊界：`/Users/m4p/projectX` 不算在 `/Users/m4p/project` 底下。
        assert_eq!(project_for_cwd("/Users/m4p/projectX", &projects), None);
    }

    /// 歸屬順序（§6.5e 第 3 條裁示）：AM_PROJECT_ID 最優先，**bot 被刪也還在**；
    /// 都沒有才 cwd；都對不到才是沒歸屬。重啟後靠同一輪掃描重建，不留記憶體狀態。
    #[tokio::test]
    async fn the_project_binding_comes_from_the_pane_env_and_survives_a_deleted_bot() {
        let app = app().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p1','/tmp/p1','p1',?)")
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b1','p1','b1','claude','t',?)")
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        let dump = "\
  400     1  48000 /opt/homebrew/bin/herdr --session agents-manager
  401   400  30000 /bin/zsh -l
---AM-ENV---
  401 /bin/zsh -l HERDR_PANE_ID=w1:pE AM_BOT_ID=b1 AM_PROJECT_ID=p1
";
        let facts = crate::memproc::pane_facts_for_shell(dump, "w1:pE", 401).unwrap();
        assert_eq!(facts.project_ids, vec!["p1".to_string()]);
        assert_eq!(facts.bot_ids, vec!["b1".to_string()]);

        // bot 被刪：綁定照舊在（不然這顆會掉成「非專案」，再撞上「只准一顆」）。
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id='b1'").bind(&now).execute(&app.db).await.unwrap();
        assert_eq!(facts.project_ids, vec!["p1".to_string()], "env 的綁定與 bot 是否存在無關");
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// scratch 永不自動關；其他沒歸屬的受 GC 並推一次 pane_unowned。
    #[tokio::test]
    async fn only_the_scratch_is_spared_and_the_rest_are_reported_once() {
        let app = app().await;
        let old = (chrono::Utc::now() - chrono::Duration::hours(9)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for (id, first_seen, scratch) in [("w1:pScratch", "2026-09-16T00:00:00.000Z", 1), ("w1:pExtra", "2026-09-16T01:00:00.000Z", 0)] {
            sqlx::query(
                "INSERT INTO panes (pane_id, host, kind, owned_by, scratch, last_output_at, first_seen, last_seen)
                 VALUES (?,'local','shell','none',?,?,?,?)",
            )
            .bind(id)
            .bind(scratch)
            .bind(&old)
            .bind(first_seen)
            .bind(&old)
            .execute(&app.db)
            .await
            .unwrap();
        }
        let sent = notify_unowned_and_orphans(&app, "local").await.unwrap();
        assert_eq!(sent, 1, "只有多出來的那顆要通知");
        let notified: Vec<String> =
            sqlx::query_scalar("SELECT pane_id FROM panes WHERE unowned_notified_at IS NOT NULL ORDER BY pane_id")
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(notified, vec!["w1:pExtra".to_string()]);
        // 再跑一次不會重複通知。
        assert_eq!(notify_unowned_and_orphans(&app, "local").await.unwrap(), 0);

        // GC：scratch 不在候選裡，多出來的那顆才是（herdr 不在，close 會失敗，這裡只驗選誰）。
        let closed = gc_host(&app, "local").await.unwrap();
        assert_eq!(closed, 0, "沒有 herdr 可關，但不能因此誤刪資料");
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes").fetch_one(&app.db).await.unwrap();
        assert_eq!(left, 2, "關不掉就原樣留著");
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// 使用者手開（cwd 對得到專案）預設不受 GC；簽過名（gc_optin）才算候選。閒置不夠久也不關。
    #[tokio::test]
    async fn a_user_pane_is_only_a_gc_candidate_after_someone_signs_for_it() {
        let app = app().await;
        let fresh = crate::db::now();
        let old = (chrono::Utc::now() - chrono::Duration::hours(9)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for (id, out_at, optin, owned) in [
            ("w1:pUser", old.as_str(), 0, "user"),
            ("w1:pSigned", old.as_str(), 1, "user"),
            ("w1:pFresh", fresh.as_str(), 0, "bot"),
        ] {
            sqlx::query(
                "INSERT INTO panes (pane_id, host, kind, owned_by, gc_optin, last_output_at, first_seen, last_seen)
                 VALUES (?,'local','shell',?,?,?,?,?)",
            )
            .bind(id)
            .bind(owned)
            .bind(optin)
            .bind(out_at)
            .bind(&old)
            .bind(&fresh)
            .execute(&app.db)
            .await
            .unwrap();
        }
        // 候選判斷與實際關閉分開：這裡沒有 herdr，所以看的是「有沒有走到關閉那一步」。
        assert_eq!(gc_host(&app, "local").await.unwrap(), 0);
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes").fetch_one(&app.db).await.unwrap();
        assert_eq!(left, 3);
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// shim 回報的用途：pane 還沒被掃到也先記著；掃描推斷出來的 owner 不會被回報改寫。
    #[tokio::test]
    async fn a_reported_purpose_is_kept_without_overwriting_the_scanned_owner() {
        let app = app().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(&now).execute(&app.db).await.unwrap();
        for (id, name) in [("b1", "owner"), ("b2", "someone-else")] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p',?,'claude','t',?)")
                .bind(id)
                .bind(name)
                .bind(&now)
                .execute(&app.db)
                .await
                .unwrap();
        }
        let b1 = crate::db::bot(&app.db, "b1").await.unwrap().unwrap();
        let b2 = crate::db::bot(&app.db, "b2").await.unwrap().unwrap();

        // 還沒掃到就先記：建一列。
        note_purpose(&app, "local", "w1:pS", &b1, "dev-server").await.unwrap();
        let (owner, purpose): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT owner_bot_id, purpose FROM panes WHERE pane_id='w1:pS'").fetch_one(&app.db).await.unwrap();
        assert_eq!((owner.as_deref(), purpose.as_deref()), (Some("b1"), Some("dev-server")));

        // 別的 bot 事後回報：用途可以更新，owner 不會被改寫（歸屬是掃描推斷的）。
        note_purpose(&app, "local", "w1:pS", &b2, "logs").await.unwrap();
        let (owner, purpose): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT owner_bot_id, purpose FROM panes WHERE pane_id='w1:pS'").fetch_one(&app.db).await.unwrap();
        assert_eq!((owner.as_deref(), purpose.as_deref()), (Some("b1"), Some("logs")));

        // 空字串不會把用途洗掉。
        note_purpose(&app, "local", "w1:pS", &b1, "").await.unwrap();
        let purpose: Option<String> =
            sqlx::query_scalar("SELECT purpose FROM panes WHERE pane_id='w1:pS'").fetch_one(&app.db).await.unwrap();
        assert_eq!(purpose.as_deref(), Some("logs"));
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// review 2026-09-16 core 10：shim 開完 pane 立刻回報，常常比掃描早到；以前 owner 由第一個寫入者決定，
    /// 掃描讀到的 `AM_BOT_ID` 永遠蓋不上去。現在環境為準；人用 adopt 指定過的才留著。
    /// 讀不到環境的輪次（macOS 閒著的 -zsh），回報帶來的綁定讓它仍算 bot 開的。
    #[tokio::test]
    async fn the_scanned_owner_wins_over_an_early_report_but_not_over_an_adopt() {
        let app = app().await;
        project_and_bot(&app).await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b2','p1','b2','claude','t',?)")
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        let b2 = crate::db::bot(&app.db, "b2").await.unwrap().unwrap();
        let owner = |app: Arc<App>| async move {
            sqlx::query_as::<_, (Option<String>, String, Option<String>)>("SELECT owner_bot_id, owned_by, project_id FROM panes WHERE pane_id='w1:pS'")
                .fetch_one(&app.db)
                .await
                .unwrap()
        };
        let pane = json!({"pane_id": "w1:pS", "workspace_id": "w1", "tab_id": "t1", "cwd": "/Users/me", "revision": 1});

        note_purpose(&app, "local", "w1:pS", &b2, "build").await.unwrap();
        // 環境讀不到：回報的綁定讓它仍算 p1 的 bot pane，不掉成沒歸屬。
        let idle = HashMap::from([("w1:pS".to_string(), Some(Observed { facts: crate::memproc::PaneFacts { pids: vec![1], shell_only: true, ..Default::default() }, ports: vec![] }))]);
        record_scan(&app, "local", &[pane.clone()], &idle).await.unwrap();
        assert_eq!(owner(app.clone()).await, (Some("b2".into()), "bot".into(), Some("p1".into())));

        // 環境讀到的是 b1：蓋過回報。
        let env = HashMap::from([("w1:pS".to_string(), observed(None, Some("b1"), Some("p1"), &[]))]);
        record_scan(&app, "local", &[pane.clone()], &env).await.unwrap();
        assert_eq!(owner(app.clone()).await.0.as_deref(), Some("b1"));

        // 人 adopt 指定 b2：之後掃描不再改它。
        let _ = adopt(State(app.clone()), Path("w1:pS".into()), Query(HashMap::new()), Some(Json(AdoptIn { owner_bot_id: Some("b2".into()), purpose: None, allow_gc: false })))
            .await
            .unwrap();
        record_scan(&app, "local", &[pane], &env).await.unwrap();
        assert_eq!(owner(app.clone()).await.0.as_deref(), Some("b2"));
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    async fn cached(app: &Arc<App>, pane: &crate::herdr::PaneInfo, kind: &str) {
        let now = crate::db::now();
        sqlx::query(
            "INSERT INTO panes (pane_id, host, workspace_id, tab_id, kind, last_output_at, first_seen, last_seen)
             VALUES (?, 'local', ?, ?, ?, ?, ?, ?)",
        )
        .bind(&pane.pane_id)
        .bind(&pane.workspace_id)
        .bind(&pane.tab_id)
        .bind(kind)
        .bind(&now)
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .unwrap();
    }

    fn with_confirm() -> Query<HashMap<String, String>> {
        Query(HashMap::from([("confirm".to_string(), "true".to_string())]))
    }

    /// review 2026-09-16 core 4：表上的 kind 是掃描的快取，關閉不能只信它，也要比照打字擋 agent。
    #[tokio::test]
    async fn closing_a_pane_checks_it_live_instead_of_trusting_the_cached_kind() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let (_, pane) = app.herdr.workspace_create("/tmp", "w", json!({})).await.unwrap();
        cached(app, &pane, "shell").await;

        // 記成 shell，但這一刻讀不到它在跑什麼：當成 service，要人確認。
        match close(State(app.clone()), Path(pane.pane_id.clone()), Query(HashMap::new())).await {
            Err(LcError::Conflict(body)) => {
                assert_eq!(body["reason"], "service_pane");
                assert_eq!(body["unverified"], true);
            }
            other => panic!("讀不到就要確認：{other:?}"),
        }

        // herdr 說裡面現在有 agent（shell 裡被 `herdr agent start --pane` 起了子 agent）：帶 confirm 也不給關。
        env.herdr.set_agent("kid", &pane.pane_id, false);
        match close(State(app.clone()), Path(pane.pane_id.clone()), with_confirm()).await {
            Err(LcError::Forbidden(body)) => assert_eq!(body["error"], "agent_pane"),
            other => panic!("agent 的 pane 不歸這支關：{other:?}"),
        }

        // 有 active run 也一樣（不用問 herdr）。
        let (_, busy) = app.herdr.workspace_create("/tmp", "b", json!({})).await.unwrap();
        cached(app, &busy, "shell").await;
        let bot = crate::testing::claude_bot(app, &env.project_id, "b").await;
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at) VALUES ('r1', ?, 'running', 'idle', ?, 'test', ?)")
            .bind(&bot.id)
            .bind(&busy.pane_id)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        assert!(matches!(close(State(app.clone()), Path(busy.pane_id.clone()), with_confirm()).await, Err(LcError::Forbidden(_))));

        // herdr 說 pane 已經不在：刪掉快取的列，回 404。
        let gone = crate::herdr::PaneInfo { pane_id: "ws-9:p9".into(), ..pane.clone() };
        cached(app, &gone, "shell").await;
        assert!(matches!(close(State(app.clone()), Path(gone.pane_id.clone()), with_confirm()).await, Err(LcError::NotFound(_))));
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes WHERE pane_id='ws-9:p9'").fetch_one(&app.db).await.unwrap();
        assert_eq!(left, 0);

        // 確認過就關。
        let (_, plain) = app.herdr.workspace_create("/tmp", "p", json!({})).await.unwrap();
        cached(app, &plain, "shell").await;
        let out = close(State(app.clone()), Path(plain.pane_id.clone()), with_confirm()).await.expect("確認過就關");
        assert_eq!(out.0["closed"], true);
        assert!(app.herdr.pane_get(&plain.pane_id).await.unwrap().is_none(), "herdr 上真的關掉了");
    }

    /// 對帳只在事件觸發時跑；定期重掃讓表跟上 herdr（pane 被 agent 佔走、關掉）。
    #[tokio::test]
    async fn the_periodic_rescan_keeps_the_table_in_step_with_herdr() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let (_, pane) = app.herdr.workspace_create("/tmp", "w", json!({})).await.unwrap();
        let scan = rescan(app, "local").await.unwrap();
        assert_eq!(scan.panes, 1);
        let row: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes WHERE pane_id=?").bind(&pane.pane_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(row, 1);
        env.herdr.set_agent("kid", &pane.pane_id, false);
        assert_eq!(rescan(app, "local").await.unwrap().panes, 0, "變成 agent pane 就不歸這張表");
    }

    /// web review M4：沒歸屬的 pane（含 scratch）以前掛在同一台**每個**專案底下。現在專案清單只回自己的，
    /// 沒歸屬的只走 `?unowned=1`，而且標出哪一顆是 scratch。
    #[tokio::test]
    async fn unowned_panes_are_listed_on_their_own_with_the_scratch_marked() {
        let app = app().await;
        project_and_bot(&app).await;
        let now = crate::db::now();
        for (id, project, owned_by, scratch) in
            [("w1:pMine", Some("p1"), "bot", 0), ("w1:pScratch", None, "none", 1), ("w1:pExtra", None, "none", 0)]
        {
            sqlx::query(
                "INSERT INTO panes (pane_id, host, kind, project_id, owned_by, scratch, last_output_at, first_seen, last_seen)
                 VALUES (?, 'local', 'shell', ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(project)
            .bind(owned_by)
            .bind(scratch)
            .bind(&now)
            .bind(&now)
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        }
        let ids = |v: &Value| v["panes"].as_array().unwrap().iter().map(|p| p["pane_id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let project = list_for_project(State(app.clone()), Path("p1".into())).await.unwrap().0;
        assert_eq!(ids(&project), vec!["w1:pMine".to_string()], "沒歸屬的不掛在專案底下");

        let unowned = list_all(State(app.clone()), Query(HashMap::from([("unowned".to_string(), "1".to_string())]))).await.unwrap().0;
        assert_eq!(ids(&unowned), vec!["w1:pScratch".to_string(), "w1:pExtra".to_string()], "scratch 排第一");
        let flags: Vec<(bool, bool)> =
            unowned["panes"].as_array().unwrap().iter().map(|p| (p["scratch"].as_bool().unwrap(), p["read_only"].as_bool().unwrap())).collect();
        assert_eq!(flags, vec![(true, false), (false, false)]);
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    /// adopt 不會偷偷把使用者手開的 pane 變成可 GC：要明確帶 allow_gc（§6.5e）。
    #[tokio::test]
    async fn adopt_only_opts_a_user_pane_into_gc_when_asked() {
        let app = app().await;
        scan_host(&app, "local", &[pane("w1:pU", None, 1)]).await.unwrap();
        let gc = |app: Arc<App>| async move {
            sqlx::query_scalar::<_, i64>("SELECT gc_optin FROM panes WHERE pane_id='w1:pU'")
                .fetch_one(&app.db)
                .await
                .unwrap()
        };
        adopt(
            State(app.clone()),
            Path("w1:pU".into()),
            Query(HashMap::new()),
            Some(Json(AdoptIn { owner_bot_id: None, purpose: Some("使用者的 shell".into()), allow_gc: false })),
        )
        .await
        .unwrap();
        assert_eq!(gc(app.clone()).await, 0, "沒簽名就不能自動關");
        adopt(
            State(app.clone()),
            Path("w1:pU".into()),
            Query(HashMap::new()),
            Some(Json(AdoptIn { owner_bot_id: None, purpose: None, allow_gc: true })),
        )
        .await
        .unwrap();
        assert_eq!(gc(app.clone()).await, 1, "人明確簽名才行");
        let purpose: Option<String> = sqlx::query_scalar("SELECT purpose FROM panes WHERE pane_id='w1:pU'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(purpose.as_deref(), Some("使用者的 shell"), "第二次 adopt 不會洗掉用途");
        std::fs::remove_dir_all(&app.data_dir).ok();
    }

    fn herdr_shell(shell_pid: Option<i64>, fg: &[(Option<i64>, &str)]) -> crate::herdr::PaneShell {
        serde_json::from_value(json!({
            "pane_id": "w1:p1",
            "shell_pid": shell_pid,
            "foreground_processes": fg.iter().map(|(pid, argv)| json!({"pid": pid, "argv": argv.split(' ').collect::<Vec<_>>(), "argv0": argv})).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    /// 2026-09-16 實機：卡在 `sudo make dev` 的 pane，herdr 回的前景是空的、`ps -E` 也讀不到 root 行程的環境。
    /// 只有沿 shell pid 的 ppid 樹往下走看得到它——看到了就不是「只有 shell」，GC 不能關。
    #[test]
    fn a_pane_waiting_on_sudo_is_not_an_idle_shell() {
        let dump = "\
  9413     1  48000 /opt/homebrew/bin/herdr --session agents-manager server
 35092  9413   3000 -zsh
  3822 35092   5000 sudo make -C witsper-ops dev
 70323  9413   3000 -zsh
---AM-ENV---
 35092 -zsh
 70323 -zsh
";
        let sudo = facts_from(&herdr_shell(Some(35092), &[]), dump, "w168:p6C").expect("樹裡有這顆 shell");
        assert!(!sudo.shell_only, "root 的子行程也算：{sudo:?}");
        assert_eq!(sudo.foreground.as_deref(), Some("sudo make -C witsper-ops dev"));
        assert_eq!(classify(sudo.foreground.as_deref(), &[]), "service");

        // 閒著的 shell：herdr 把 shell 自己列成前景，那不算。
        let idle = facts_from(&herdr_shell(Some(70323), &[(Some(70323), "zsh")]), dump, "w168:p6J").unwrap();
        assert!(idle.shell_only, "{idle:?}");
        assert_eq!(idle.pids, vec![70323]);

        // herdr 看到的前景樹還沒出現（剛啟動、或 dump 早一步取的）：也不算只有 shell。
        let racing = facts_from(&herdr_shell(Some(70323), &[(Some(99999), "vim notes.md")]), dump, "w168:p6J").unwrap();
        assert!(!racing.shell_only);
        assert_eq!(racing.foreground.as_deref(), Some("vim notes.md"));

        // 判不出來：herdr 沒報 shell pid、或 pid 不在樹裡。
        assert_eq!(facts_from(&herdr_shell(None, &[]), dump, "w168:p6J"), None);
        assert_eq!(facts_from(&herdr_shell(Some(424242), &[]), dump, "w168:p6J"), None);
    }

    /// 歸屬只認 `AM_BOT_ID`：使用者手開的 pane（沒有這個變數）不會被算成誰的（§6.5e）。
    #[test]
    fn ownership_comes_from_the_pane_environment() {
        let dump = "\
  400     1  48000 /opt/homebrew/bin/herdr --session agents-manager
  401   400  30000 /bin/zsh -l
  402   401 820000 node /x/dev-server.js
  410   400  20000 /bin/zsh -l
---AM-ENV---
  401 /bin/zsh -l HERDR_PANE_ID=w1:p1 AM_BOT_ID=b1
  402 node HERDR_PANE_ID=w1:p1 AM_BOT_ID=b1
  410 /bin/zsh -l HERDR_PANE_ID=w1:p9
";
        let owned = &crate::memproc::pane_facts_for_shell(dump, "w1:p1", 401).unwrap();
        assert_eq!(owned.bot_ids, vec!["b1".to_string()]);
        assert!(owned.foreground.as_deref().unwrap().contains("dev-server.js"));
        assert!(!owned.shell_only);
        let user = &crate::memproc::pane_facts_for_shell(dump, "w1:p9", 410).unwrap();
        assert!(user.bot_ids.is_empty(), "使用者手開的不歸任何 bot");
        assert!(user.shell_only, "只有 shell");
        assert_eq!(classify(user.foreground.as_deref(), &[]), "shell");
    }
}

// ------------------------------------------------------------------ API

use axum::extract::{Path, Query, State};
use axum::Json;

use crate::lifecycle::LcError;

/// `GET /api/projects/{id}/panes`：這個專案的非 agent pane。**沒歸屬的不在這裡**（SPEC §6.5e：它不屬於任何專案，
/// 掛在每個專案底下會重複出現、看起來像那個專案的東西）；它們走 `GET /api/panes?unowned=1`。
pub async fn list_for_project(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let project = crate::db::project(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    let rows = sqlx::query(
        "SELECT * FROM panes WHERE host=? AND project_id=? ORDER BY kind, pane_id",
    )
    .bind(&project.host)
    .bind(&id)
    .fetch_all(&app.db)
    .await
    .map_err(sql)?;
    let panes: Vec<Value> = rows.iter().map(row_json).collect();
    Ok(Json(json!({"project_id": id, "host": project.host, "panes": panes})))
}

#[derive(serde::Deserialize, Default)]
pub struct AdoptIn {
    pub owner_bot_id: Option<String>,
    pub purpose: Option<String>,
    /// 使用者手開的 pane 只有這個明確帶 true 才會變成可自動關（§6.5e）。
    #[serde(default)]
    pub allow_gc: bool,
}

/// `GET /api/panes?unowned=1`：全機的非 agent pane；`unowned=1` 只回「連專案都對不到」的那些（§6.5e）。
/// 哪一顆是 scratch 由 daemon 標（`scratch`），前端不自己重算。
pub async fn list_all(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, LcError> {
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let only_unowned = q.get("unowned").map(|v| v == "1" || v == "true").unwrap_or(false);
    let rows = if only_unowned {
        sqlx::query("SELECT * FROM panes WHERE owned_by='none' ORDER BY host, scratch DESC, pane_id").fetch_all(&app.db).await
    } else {
        sqlx::query("SELECT * FROM panes ORDER BY host, kind, pane_id").fetch_all(&app.db).await
    }
    .map_err(sql)?;
    Ok(Json(json!({"panes": rows.iter().map(row_json).collect::<Vec<_>>()})))
}

/// `POST /api/panes/{id}/focus?host=local`：把 herdr 的焦點切到這顆 pane。只動焦點，不改內容。
pub async fn focus(
    State(app): State<Arc<App>>,
    Path(pane_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let (client, _) = crate::api::shell::client_for(&app, &host).await?;
    client.pane_focus(&pane_id).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok(Json(json!({"focused": true, "pane_id": pane_id})))
}

/// `POST /api/panes/{id}/adopt`：補 owner／purpose。不會偷偷讓使用者的 pane 變成可 GC。
pub async fn adopt(
    State(app): State<Arc<App>>,
    Path(pane_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Option<Json<AdoptIn>>,
) -> Result<Json<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let project = match b.owner_bot_id.as_deref() {
        Some(bot) => Some(
            crate::db::bot(&app.db, bot)
                .await
                .map_err(up)?
                .ok_or_else(|| LcError::NotFound("bot".into()))?
                .project_id,
        ),
        None => None,
    };
    let n = sqlx::query(
        "UPDATE panes SET owner_bot_id=COALESCE(?, owner_bot_id), project_id=COALESCE(?, project_id),
                          owner_adopted=CASE WHEN ? IS NULL THEN owner_adopted ELSE 1 END,
                          purpose=COALESCE(?, purpose), gc_optin=CASE WHEN ? THEN 1 ELSE gc_optin END,
                          orphan_notified_at=NULL
          WHERE host=? AND pane_id=?",
    )
    .bind(b.owner_bot_id.as_deref())
    .bind(project.as_deref())
    .bind(b.owner_bot_id.as_deref())
    .bind(b.purpose.as_deref())
    .bind(b.allow_gc)
    .bind(&host)
    .bind(&pane_id)
    .execute(&app.db)
    .await
    .map_err(sql)?
    .rows_affected();
    if n == 0 {
        return Err(LcError::NotFound("pane".into()));
    }
    let row = sqlx::query("SELECT * FROM panes WHERE host=? AND pane_id=?")
        .bind(&host)
        .bind(&pane_id)
        .fetch_one(&app.db)
        .await
        .map_err(sql)?;
    tracing::info!(host, pane_id, owner = ?b.owner_bot_id, purpose = ?b.purpose, allow_gc = b.allow_gc, "pane adopted");
    Ok(Json(row_json(&row)))
}

/// `POST /api/panes/{id}/close`：人按的關閉。服務 pane 要帶 `confirm=true`（UI 會先顯示 port）。
///
/// 表上的 kind 是掃描的快取（review 2026-09-16 core 4），所以關之前即時再看一次：
/// - 有 active run、或 herdr 說裡面現在有 agent → 403 `agent_pane`（比照打字那條線；agent 走 bot 的 stop）。
/// - herdr 說 pane 已經不在 → 刪列、404。
/// - 現在是 service（前景有非 shell 程式或 listen port），或**讀不到**事實 → 沒帶 confirm 就 409 `service_pane`，
///   body 的 `pane` 換成即時的 kind／前景／port；讀不到時多一個 `unverified: true`。
pub async fn close(
    State(app): State<Arc<App>>,
    Path(pane_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let confirmed = q.get("confirm").map(|v| v == "true" || v == "1").unwrap_or(false);
    close_tracked(&app, &host, &pane_id, confirmed).await.map(Json)
}

/// [`close`] 的本體；`DELETE /api/hosts/{name}/shells/{pane_id}` 找不到記憶體那份時也走這裡（web review M2）。
pub(crate) async fn close_tracked(app: &Arc<App>, host: &str, pane_id: &str, confirmed: bool) -> Result<Value, LcError> {
    let (app, host, pane_id) = (app.clone(), host.to_string(), pane_id.to_string());
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let up = |e: anyhow::Error| LcError::Upstream(format!("{e:#}"));
    let row = sqlx::query("SELECT * FROM panes WHERE host=? AND pane_id=?")
        .bind(&host)
        .bind(&pane_id)
        .fetch_optional(&app.db)
        .await
        .map_err(sql)?
        .ok_or_else(|| LcError::NotFound("pane".into()))?;
    let mut info = row_json(&row);
    let agent_pane = || LcError::Forbidden(json!({"error": "agent_pane", "message": "這顆 pane 正在跑 agent，請從 bot 停掉"}));
    let session = app.session_for_host(&host).await.unwrap_or_default();
    if !crate::db::active_runs_for_pane(&app.db, &host, &pane_id, &session, &session).await.map_err(up)?.is_empty() {
        return Err(agent_pane());
    }
    let (client, _) = crate::api::shell::client_for(&app, &host).await?;
    match client.pane_get(&pane_id).await.map_err(up)? {
        None => {
            sqlx::query("DELETE FROM panes WHERE host=? AND pane_id=?").bind(&host).bind(&pane_id).execute(&app.db).await.map_err(sql)?;
            return Err(LcError::NotFound("pane".into()));
        }
        Some(p) if p.agent.as_deref().is_some_and(|a| !a.is_empty()) => return Err(agent_pane()),
        Some(_) => {}
    }
    let live = match (crate::memproc::dump(&app, &host).await, client.pane_shell(&pane_id).await) {
        (Ok(dump), Ok(shell)) => match facts_from(&shell, &dump, &pane_id) {
            Some(f) => listen_ports(&host, &f.pids).await.map(|ports| (f, ports)),
            None => None,
        },
        _ => None,
    };
    let needs_confirm = match &live {
        Some((f, ports)) => {
            let kind = classify(f.foreground.as_deref(), ports);
            info["kind"] = json!(kind);
            info["foreground"] = json!(f.foreground);
            info["read_only"] = json!(!ports.is_empty());
            info["listen_ports"] = json!(ports);
            kind == "service"
        }
        None => true,
    };
    if needs_confirm && !confirmed {
        // 關掉服務 pane 會殺掉裡面在跑的東西：要人看過 port 再點一次。
        return Err(LcError::conflict(
            "service pane needs confirm=true",
            json!({"reason": "service_pane", "pane": info, "unverified": live.is_none()}),
        ));
    }
    crate::lifecycle::close_pane_and_tab(
        &client,
        info["workspace_id"].as_str(),
        info["tab_id"].as_str(),
        &pane_id,
    )
    .await;
    sqlx::query("DELETE FROM panes WHERE host=? AND pane_id=?")
        .bind(&host)
        .bind(&pane_id)
        .execute(&app.db)
        .await
        .map_err(sql)?;
    app.pane_live.lock().await.remove(&(host.clone(), pane_id.clone()));
    tracing::info!(host, pane_id, kind = %info["kind"], "pane closed by request");
    Ok(json!({"closed": true, "pane": info}))
}
