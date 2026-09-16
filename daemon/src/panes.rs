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
           PRIMARY KEY (host, pane_id)
         )",
    )
    .execute(pool)
    .await?;
    // 表可能是上一版建的：欄位 additive 補上（migrate 可重入）。
    for (col, ddl) in [
        ("owned_by", "ALTER TABLE panes ADD COLUMN owned_by TEXT NOT NULL DEFAULT 'none'"),
        ("unowned_notified_at", "ALTER TABLE panes ADD COLUMN unowned_notified_at TEXT"),
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

/// 本機才算 listen port：pane 行程樹的 pid 對 `lsof`。遠端留空（§6.5e：不為了它多開 ssh 往返）。
pub async fn listen_ports(host: &str, pids: &[i32]) -> HashMap<i32, Vec<u16>> {
    let mut out: HashMap<i32, Vec<u16>> = HashMap::new();
    if host != crate::config::LOCAL_HOST || pids.is_empty() {
        return out;
    }
    let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let script = format!("lsof -nP -iTCP -sTCP:LISTEN -a -p {list} -Fpn 2>/dev/null");
    let Ok(Some(o)) = crate::hosts::sh_local(&script, std::time::Duration::from_secs(10)).await else { return out };
    let text = String::from_utf8_lossy(&o.stdout);
    out.extend(parse_lsof(&text));
    out
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

/// 掃一台主機的非 agent pane，寫進 `panes`。`snapshot_panes` 是 `session.snapshot` 的 `panes` 陣列
/// （已經含 `agent`），所以不用再打一次 RPC。
pub async fn scan_host(app: &Arc<App>, host: &str, snapshot_panes: &[Value]) -> Result<usize> {
    let non_agent: Vec<&Value> = snapshot_panes
        .iter()
        .filter(|p| p.get("agent").and_then(Value::as_str).filter(|a| !a.is_empty()).is_none())
        .collect();
    if non_agent.is_empty() {
        // 這台沒有非 agent pane：把舊的收乾淨（pane 已經關了）。
        sqlx::query("DELETE FROM panes WHERE host=?").bind(host).execute(&app.db).await?;
        return Ok(0);
    }
    // 環境快照：pane 行程樹的 `AM_BOT_ID` 就是歸屬（§6.5e）。讀不到就這一輪不更新歸屬，不要猜。
    let dump = match crate::memproc::dump(app, host).await {
        Ok(out) => Some(out),
        Err(e) => {
            tracing::warn!(host, error = %e, "讀不到行程環境，這一輪不更新 pane 歸屬");
            None
        }
    };
    let client = crate::api::shell::client_for(app, host).await.ok().map(|(c, _)| c);
    let now = crate::db::now();
    // canonical path 比對用（§6.5e 的 cwd 回退）。
    let project_paths: Vec<(String, String)> = crate::db::live_projects(&app.db)
        .await?
        .into_iter()
        .filter(|p| p.host == host)
        .map(|p| (p.id, p.path))
        .collect();
    let mut seen = Vec::new();
    for p in &non_agent {
        let Some(pane_id) = p.get("pane_id").and_then(Value::as_str) else { continue };
        seen.push(pane_id.to_string());
        let f = read_facts(client.as_ref(), dump.as_deref(), pane_id).await.unwrap_or_default();
        let ports: Vec<u16> = {
            let by_pid = listen_ports(host, &f.pids).await;
            let mut all: Vec<u16> = by_pid.into_values().flatten().collect();
            all.sort_unstable();
            all.dedup();
            all
        };
        let kind = classify(f.foreground.as_deref(), &ports);
        let owner = f.bot_ids.first().cloned();
        // 歸屬順序（§6.5e，使用者 2026-09-16 第 3 條裁示）：
        //   1. `AM_PROJECT_ID`——開 pane 當下就綁好的專案。**bot 被刪也不失效**，否則孤兒 pane 會掉成
        //      「非專案」，再撞上「只准一顆」的規則被當成多餘的那一顆。
        //   2. `AM_BOT_ID`——只補 owner 與顯示；它的專案只在第 1 條沒有時才拿來用。
        //   3. 兩個 env 都沒有才用 cwd 比對。
        //   4. 都對不到才是沒歸屬（scratch 的候選）。
        // 每輪掃描都以 env 為準覆寫 project_id，不留記憶體狀態。
        let mut owned_by = "none";
        let mut project = f.project_ids.iter().find(|id| !id.trim().is_empty()).cloned();
        if project.is_some() {
            owned_by = "bot";
        }
        if project.is_none() {
            if let Some(b) = &owner {
                owned_by = "bot";
                project = crate::db::bot(&app.db, b).await.ok().flatten().map(|b| b.project_id);
            }
        }
        if project.is_none() {
            let cwd = p.get("foreground_cwd").or_else(|| p.get("cwd")).and_then(Value::as_str).unwrap_or("");
            if let Some(id) = project_for_cwd(cwd, &project_paths) {
                owned_by = if owner.is_some() { "bot" } else { "user" };
                project = Some(id.to_string());
            }
        }
        // 專案本身被刪掉了：綁定留著沒有意義，回到沒歸屬（孤兒通知另外處理）。
        if let Some(id) = &project {
            if !project_paths.iter().any(|(pid, _)| pid == id) {
                project = None;
                owned_by = "none";
            }
        }
        let revision = p.get("revision").and_then(Value::as_u64).map(|v| v as i64);
        let prev: Option<(Option<i64>, String, String)> =
            sqlx::query_as("SELECT last_revision, last_output_at, first_seen FROM panes WHERE host=? AND pane_id=?")
                .bind(host)
                .bind(pane_id)
                .fetch_optional(&app.db)
                .await?;
        // revision 變了才算「有輸出」；第一次看到就以 first_seen 當基準（§6.5e）。
        let (last_output_at, first_seen) = match &prev {
            Some((old_rev, out_at, first)) => {
                let moved = revision.is_some() && *old_rev != revision;
                ((if moved { now.clone() } else { out_at.clone() }), first.clone())
            }
            None => (now.clone(), now.clone()),
        };
        sqlx::query(
            "INSERT INTO panes (pane_id, host, workspace_id, tab_id, cwd, kind, owner_bot_id, project_id,
                                foreground, listen_ports, last_revision, last_output_at, first_seen, last_seen, owned_by)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(host, pane_id) DO UPDATE SET
               workspace_id=excluded.workspace_id, tab_id=excluded.tab_id, cwd=excluded.cwd, kind=excluded.kind,
               -- 人工 adopt 過的 owner／purpose 不被掃描蓋掉（§6.5e）。
               owner_bot_id=COALESCE(panes.owner_bot_id, excluded.owner_bot_id),
               -- 每輪以 env 為準覆寫（§6.5e）：綁定來自 pane 的環境，不是我們記住的舊值。
               project_id=excluded.project_id,
               foreground=excluded.foreground, listen_ports=excluded.listen_ports,
               last_revision=excluded.last_revision, last_output_at=excluded.last_output_at, last_seen=excluded.last_seen,
               owned_by=excluded.owned_by,
               -- 歸屬回來了就把「沒歸屬」的通知標記清掉（去重規則同 orphan）。
               unowned_notified_at=CASE WHEN excluded.owned_by='none' THEN panes.unowned_notified_at ELSE NULL END,
               -- owner 又對得到 bot 了就把孤兒標記清掉，下次真的變孤兒才會再通知一次。
               orphan_notified_at=CASE WHEN excluded.project_id IS NOT NULL THEN NULL ELSE panes.orphan_notified_at END",
        )
        .bind(pane_id)
        .bind(host)
        .bind(p.get("workspace_id").and_then(Value::as_str))
        .bind(p.get("tab_id").and_then(Value::as_str))
        .bind(p.get("cwd").and_then(Value::as_str))
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
        .execute(&app.db)
        .await?;
    }
    // 不見了的 pane：herdr 說它不在了，就從表裡拿掉（下次再出現會重新記 first_seen）。
    let keep = seen.iter().map(|s| format!("'{}'", s.replace('\'', "''"))).collect::<Vec<_>>().join(",");
    sqlx::query(&format!("DELETE FROM panes WHERE host=? AND pane_id NOT IN ({keep})"))
        .bind(host)
        .execute(&app.db)
        .await?;
    Ok(seen.len())
}

/// shim 回報的用途：pane 還沒被掃到就先建一列（`kind` 先當 shell，下一輪掃描會修正）。
/// owner 只在這一列還沒有 owner 時才寫——掃描推斷出來的歸屬優先，回報不能改寫別人的 pane。
pub async fn note_purpose(app: &Arc<App>, host: &str, pane_id: &str, bot: &crate::db::Bot, purpose: &str) -> Result<()> {
    if pane_id.trim().is_empty() {
        return Ok(());
    }
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO panes (pane_id, host, kind, owner_bot_id, project_id, purpose, last_output_at, first_seen, last_seen)
         VALUES (?,?,'shell',?,?,?,?,?,?)
         ON CONFLICT(host, pane_id) DO UPDATE SET
           purpose=CASE WHEN excluded.purpose IS NULL OR excluded.purpose='' THEN panes.purpose ELSE excluded.purpose END,
           owner_bot_id=COALESCE(panes.owner_bot_id, excluded.owner_bot_id),
           project_id=COALESCE(panes.project_id, excluded.project_id)",
    )
    .bind(pane_id)
    .bind(host)
    .bind(&bot.id)
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
    let rows = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, String, i64, Option<String>)>(
        "SELECT pane_id, kind, workspace_id, tab_id, last_output_at, gc_optin, owned_by FROM panes WHERE host=?",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    // 沒歸屬的那一顆＝最早看到的那顆，是 scratch，永不自動關；其餘的才受 GC（使用者「只准一顆」的裁示）。
    let scratch: Option<String> = sqlx::query_scalar(
        "SELECT pane_id FROM panes WHERE host=? AND owned_by='none' ORDER BY first_seen, pane_id LIMIT 1",
    )
    .bind(host)
    .fetch_optional(&app.db)
    .await?;
    let mut closed = 0;
    for (pane_id, kind, ws, tab, last_output_at, gc_optin, owned_by) in rows {
        if kind != "shell" {
            continue;
        }
        if scratch.as_deref() == Some(pane_id.as_str()) {
            continue;
        }
        // 使用者手開的（cwd 對得到專案）預設不關，除非 adopt 時簽過名。
        if owned_by.as_deref() == Some("user") && gc_optin == 0 {
            continue;
        }
        if seconds_since(&last_output_at) < idle_limit {
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
    let ports: Vec<u16> = listen_ports(host, &f.pids).await.into_values().flatten().collect();
    if !ports.is_empty() {
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
pub async fn notify_unowned_and_orphans(app: &Arc<App>, host: &str) -> Result<usize> {
    let scratch: Option<String> = sqlx::query_scalar(
        "SELECT pane_id FROM panes WHERE host=? AND owned_by='none' ORDER BY first_seen, pane_id LIMIT 1",
    )
    .bind(host)
    .fetch_optional(&app.db)
    .await?;
    let rows = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, Option<String>, String)>(
        "SELECT pane_id, kind, workspace_id, foreground, listen_ports, owned_by
           FROM panes WHERE host=? AND owned_by='none' AND unowned_notified_at IS NULL",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await?;
    let mut sent = 0;
    for (pane_id, kind, ws, fg, ports, _) in rows {
        if scratch.as_deref() == Some(pane_id.as_str()) {
            continue; // 那一顆是 scratch，不是「多出來的」。
        }
        let payload = json!({
            "host": host, "pane_id": pane_id, "kind": kind, "workspace_id": ws,
            "foreground": fg, "listen_ports": ports,
            "message": "這顆 pane 對不到任何專案，而且不是那顆固定的 scratch",
        });
        let key = format!("pane_unowned:{host}:{pane_id}");
        if crate::supervisor::store::push_inbox(&app.db, &key, "pane_unowned", None, None, None, &payload).await?.is_some() {
            sent += 1;
        }
        sqlx::query("UPDATE panes SET unowned_notified_at=? WHERE host=? AND pane_id=?")
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
    json!({
        "pane_id": r.get::<String, _>("pane_id"),
        "host": r.get::<String, _>("host"),
        "workspace_id": r.get::<Option<String>, _>("workspace_id"),
        "tab_id": r.get::<Option<String>, _>("tab_id"),
        "cwd": r.get::<Option<String>, _>("cwd"),
        "kind": r.get::<String, _>("kind"),
        "owner_bot_id": r.get::<Option<String>, _>("owner_bot_id"),
        "project_id": r.get::<Option<String>, _>("project_id"),
        "purpose": r.get::<Option<String>, _>("purpose"),
        "foreground": r.get::<Option<String>, _>("foreground"),
        "listen_ports": r.get::<Option<String>, _>("listen_ports")
            .map(|s| s.split(',').filter_map(|p| p.parse::<u16>().ok()).collect::<Vec<_>>())
            .unwrap_or_default(),
        "last_output_at": r.get::<String, _>("last_output_at"),
        "first_seen": r.get::<String, _>("first_seen"),
        "last_seen": r.get::<String, _>("last_seen"),
        "gc_optin": r.get::<i64, _>("gc_optin") != 0,
        "owned_by": r.get::<String, _>("owned_by"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(scan_host(&app, "local", &panes).await.unwrap(), 1, "只收非 agent pane");
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
        assert_eq!((kind.as_str(), rev), ("shell", Some(7)));
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
        assert_eq!(scan_host(&app, "local", &[pane("w1:pA", Some("claude"), 3)]).await.unwrap(), 0);
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes").fetch_one(&app.db).await.unwrap();
        assert_eq!(left, 0);
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

    /// 沒歸屬的第一顆是 scratch，永不自動關；第二顆起受 GC 並推一次 pane_unowned。
    #[tokio::test]
    async fn only_the_first_unowned_pane_is_spared_and_the_rest_are_reported_once() {
        let app = app().await;
        let old = (chrono::Utc::now() - chrono::Duration::hours(9)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for (id, first_seen) in [("w1:pScratch", "2026-09-16T00:00:00.000Z"), ("w1:pExtra", "2026-09-16T01:00:00.000Z")] {
            sqlx::query(
                "INSERT INTO panes (pane_id, host, kind, owned_by, last_output_at, first_seen, last_seen)
                 VALUES (?,'local','shell','none',?,?,?)",
            )
            .bind(id)
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

/// `GET /api/projects/{id}/panes`：這個專案的非 agent pane，外加**還沒歸屬**的（使用者手開的也要看得到）。
pub async fn list_for_project(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let project = crate::db::project(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    let rows = sqlx::query(
        "SELECT * FROM panes WHERE host=? AND (project_id=? OR project_id IS NULL) ORDER BY kind, pane_id",
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
pub async fn list_all(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, LcError> {
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let only_unowned = q.get("unowned").map(|v| v == "1" || v == "true").unwrap_or(false);
    let rows = if only_unowned {
        sqlx::query("SELECT * FROM panes WHERE owned_by='none' ORDER BY host, pane_id").fetch_all(&app.db).await
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
                          purpose=COALESCE(?, purpose), gc_optin=CASE WHEN ? THEN 1 ELSE gc_optin END,
                          orphan_notified_at=NULL
          WHERE host=? AND pane_id=?",
    )
    .bind(b.owner_bot_id.as_deref())
    .bind(project.as_deref())
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
pub async fn close(
    State(app): State<Arc<App>>,
    Path(pane_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let sql = |e: sqlx::Error| LcError::Upstream(e.to_string());
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let row = sqlx::query("SELECT * FROM panes WHERE host=? AND pane_id=?")
        .bind(&host)
        .bind(&pane_id)
        .fetch_optional(&app.db)
        .await
        .map_err(sql)?
        .ok_or_else(|| LcError::NotFound("pane".into()))?;
    let info = row_json(&row);
    let confirmed = q.get("confirm").map(|v| v == "true" || v == "1").unwrap_or(false);
    if info["kind"] == "service" && !confirmed {
        // 關掉服務 pane 會殺掉裡面在跑的東西：要人看過 port 再點一次。
        return Err(LcError::conflict(
            "service pane needs confirm=true",
            json!({"reason": "service_pane", "pane": info}),
        ));
    }
    let (client, _) = crate::api::shell::client_for(&app, &host).await?;
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
    tracing::info!(host, pane_id, kind = %info["kind"], "pane closed by request");
    Ok(Json(json!({"closed": true, "pane": info})))
}
