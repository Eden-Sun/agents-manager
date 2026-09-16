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
           -- 使用者手開的 pane 只有人明確簽名（adopt allow_gc）才可自動關。
           gc_optin INTEGER NOT NULL DEFAULT 0,
           PRIMARY KEY (host, pane_id)
         )",
    )
    .execute(pool)
    .await?;
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
    let facts = match crate::memproc::dump(app, host).await {
        Ok(out) => crate::memproc::pane_facts_from_dump(&out),
        Err(e) => {
            tracing::warn!(host, error = %e, "讀不到行程環境，這一輪不更新 pane 歸屬");
            HashMap::new()
        }
    };
    let now = crate::db::now();
    let mut seen = Vec::new();
    for p in &non_agent {
        let Some(pane_id) = p.get("pane_id").and_then(Value::as_str) else { continue };
        seen.push(pane_id.to_string());
        let f = facts.get(pane_id).cloned().unwrap_or_default();
        let ports: Vec<u16> = {
            let by_pid = listen_ports(host, &f.pids).await;
            let mut all: Vec<u16> = by_pid.into_values().flatten().collect();
            all.sort_unstable();
            all.dedup();
            all
        };
        let kind = classify(f.foreground.as_deref(), &ports);
        let owner = f.bot_ids.first().cloned();
        let project = match &owner {
            Some(b) => crate::db::bot(&app.db, b).await.ok().flatten().map(|b| b.project_id),
            None => None,
        };
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
                                foreground, listen_ports, last_revision, last_output_at, first_seen, last_seen)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(host, pane_id) DO UPDATE SET
               workspace_id=excluded.workspace_id, tab_id=excluded.tab_id, cwd=excluded.cwd, kind=excluded.kind,
               -- 人工 adopt 過的 owner／purpose 不被掃描蓋掉（§6.5e）。
               owner_bot_id=COALESCE(panes.owner_bot_id, excluded.owner_bot_id),
               project_id=COALESCE(excluded.project_id, panes.project_id),
               foreground=excluded.foreground, listen_ports=excluded.listen_ports,
               last_revision=excluded.last_revision, last_output_at=excluded.last_output_at, last_seen=excluded.last_seen,
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
        let facts = crate::memproc::pane_facts_from_dump(dump);
        let owned = &facts["w1:p1"];
        assert_eq!(owned.bot_ids, vec!["b1".to_string()]);
        assert!(owned.foreground.as_deref().unwrap().contains("dev-server.js"));
        assert!(!owned.shell_only);
        let user = &facts["w1:p9"];
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
