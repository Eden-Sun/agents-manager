//! TOML -> SQLite projection (SPEC §3.1).
//!
//! config.toml is the authority for the desired Project / Bot set. Rows removed from
//! the TOML are soft-deleted so their conversation history survives.

use crate::config::{canonical_path, valid_bot_name, valid_id, ConfigStore, ID_RE};
use crate::db;
use anyhow::{bail, Result};
use rand::Rng;
use sqlx::SqlitePool;
use std::collections::HashSet;

pub fn new_token() -> String {
    let mut rng = rand::thread_rng();
    (0..32).map(|_| std::char::from_digit(rng.gen_range(0..16), 16).unwrap()).collect()
}

/// 投影與刪除共用的臨界區：「讀 config → 算 diff → guard → 寫 config → 投影」之間不能插進另一次投影或刪除，
/// 否則 A 會讀到 B 剛寫的 config、把 B 的刪除當成自己沒授權的 removal，或反過來替 B 放行（sol 三輪）。
static PROJECTION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Fill in missing ids (writing them back to the TOML) and upsert everything into SQLite.
///
/// 帶大量軟刪的閘門：啟動與 runtime 的每一次重投都走這條。`ConfigStore::update` 會在磁碟 mtime 變了
/// 時重讀，所以「外面把 TOML 換掉／清空，再由 API 或總管觸發重投」也是事故路徑，不能只擋啟動。
pub async fn project_config(store: &ConfigStore, pool: &SqlitePool) -> Result<()> {
    let _g = PROJECTION.lock().await;
    project_inner(store, pool, None).await
}

/// 刪除 API 這一次**實際從 config.toml 拿掉**的 id。只由 `delete_from_config` 在臨界區內從當下的 TOML 算出來，
/// 不從 DB 猜：TOML 被外部清空時 DB 還記得的 project／bot 不能因為有人按了刪除就被一併放行。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Deleting {
    pub projects: HashSet<String>,
    pub bots: HashSet<String>,
}

pub enum DeleteTarget<'a> {
    /// `held`：呼叫端已經拿著這些 bot 的 per-bot 鎖並確認都停了。TOML 裡這個專案只要多出一顆不在名單上的
    /// bot（例如剛建立、可能正要啟動）就拒絕——不能刪掉一顆沒被鎖住、可能正在跑的 bot（sol 四輪）。
    Project { id: &'a str, held: &'a HashSet<String> },
    Bot(&'a str),
}

/// 目標此刻不在 config.toml（被外部改寫過，或本來就不是 config 管的列）：拒絕，什麼都不動。
#[derive(Debug, thiserror::Error)]
#[error("{0} 此刻不在 config.toml（可能被外部改寫過）：拒絕刪除，什麼都沒動")]
pub struct NotInConfig(pub String);

/// 刪除模式的閘門：除了這次實際拿掉的 id，只要還有別的列會不見就拒絕（不套一般的小量刪除門檻）。
#[derive(Debug, thiserror::Error)]
#[error("拒絕刪除：{0}")]
pub struct DeleteRefused(pub String);

/// 刪除的單一臨界區：重讀 config → 確認目標此刻在 TOML → 算出實際要拿掉的 id → 閘門（寫入前）→
/// 寫 config → 投影。呼叫端（API）在這之前拿好 per-bot 鎖、在這之後才停機，所以拒絕一定發生在任何東西被停之前。
pub async fn delete_from_config(store: &ConfigStore, pool: &SqlitePool, target: DeleteTarget<'_>) -> Result<Deleting> {
    let _g = PROJECTION.lock().await;
    // 臨界區內 DB 的 user bot／project 只有投影會改，所以這份快照在寫 config 之前都成立。
    let db_bots: Vec<db::Bot> =
        db::live_bots(pool).await?.into_iter().filter(|b| b.managed_by == "user").collect();
    let db_projects = db::live_projects(pool).await?;
    let allow = store
        .update(|cfg| {
            let allow = match target {
                DeleteTarget::Project { id, held } => {
                    let p = cfg
                        .projects
                        .iter()
                        .find(|p| p.id.as_deref() == Some(id))
                        .ok_or_else(|| anyhow::Error::new(NotInConfig(format!("專案 `{id}`"))))?;
                    let bots: HashSet<String> = p.bots.iter().filter_map(|b| b.id.clone()).collect();
                    let unheld: Vec<String> = bots.iter().filter(|b| !held.contains(*b)).cloned().collect();
                    if !unheld.is_empty() {
                        return Err(anyhow::Error::new(DeleteRefused(format!(
                            "專案 `{id}` 在刪除途中多了沒鎖住的 bot（{}），可能正要啟動；請重試",
                            some_names(&unheld)
                        ))));
                    }
                    Deleting { projects: HashSet::from([id.to_string()]), bots }
                }
                DeleteTarget::Bot(id) => {
                    if !cfg.projects.iter().any(|p| p.bots.iter().any(|b| b.id.as_deref() == Some(id))) {
                        return Err(anyhow::Error::new(NotInConfig(format!("bot `{id}`"))));
                    }
                    Deleting { projects: HashSet::new(), bots: HashSet::from([id.to_string()]) }
                }
            };
            let mut next = cfg.clone();
            next.projects.retain(|p| !p.id.as_ref().is_some_and(|id| allow.projects.contains(id)));
            for p in next.projects.iter_mut() {
                p.bots.retain(|b| !b.id.as_ref().is_some_and(|id| allow.bots.contains(id)));
            }
            let (live_projects, live_bots) = live_ids(&next);
            let (gone_bots, gone_projects) = removals(&db_bots, &db_projects, &live_projects, &live_bots, Some(&allow));
            if !gone_bots.is_empty() || !gone_projects.is_empty() {
                return Err(anyhow::Error::new(DeleteRefused(format!(
                    "這次只該拿掉 {} 顆 bot／{} 個專案，但 config.toml 還少了 {} 顆 bot（{}）與 {} 個專案（{}）；先確認設定檔沒被外部改寫",
                    allow.bots.len(),
                    allow.projects.len(),
                    gone_bots.len(),
                    some_names(&gone_bots),
                    gone_projects.len(),
                    some_names(&gone_projects),
                ))));
            }
            *cfg = next;
            Ok(allow)
        })
        .await?;
    project_inner(store, pool, Some(&allow)).await?;
    Ok(allow)
}

fn live_ids(cfg: &crate::config::ConfigFile) -> (HashSet<String>, HashSet<String>) {
    let projects = cfg.projects.iter().filter_map(|p| p.id.clone()).collect();
    let bots = cfg.projects.iter().flat_map(|p| p.bots.iter()).filter_map(|b| b.id.clone()).collect();
    (projects, bots)
}

/// DB 裡還活著、config 裡卻沒有、也不在授權名單上的 user bot 與專案（回傳名字，給錯誤訊息用）。
fn removals(
    db_bots: &[db::Bot],
    db_projects: &[db::Project],
    live_projects: &HashSet<String>,
    live_bots: &HashSet<String>,
    allow: Option<&Deleting>,
) -> (Vec<String>, Vec<String>) {
    let ok_bots = allow.map(|a| &a.bots);
    let ok_projects = allow.map(|a| &a.projects);
    let bots = db_bots
        .iter()
        .filter(|b| !live_bots.contains(&b.id) && !ok_bots.is_some_and(|ids| ids.contains(&b.id)))
        .map(|b| b.name.clone())
        .collect();
    let projects = db_projects
        .iter()
        .filter(|p| !live_projects.contains(&p.id) && !ok_projects.is_some_and(|ids| ids.contains(&p.id)))
        .map(|p| p.label.clone())
        .collect();
    (bots, projects)
}

async fn project_inner(store: &ConfigStore, pool: &SqlitePool, allow: Option<&Deleting>) -> Result<()> {
    // 1. fill in ids / canonicalize paths, write back only if something changed.
    let changed = store
        .update(|cfg| {
            let mut dirty = false;
            let identities = cfg.identities.clone();
            for i in &identities {
                if !crate::config::valid_identity_name(&i.name) {
                    bail!("invalid identity name `{}` (must match {})", i.name, crate::config::SLUG_NAME_RE);
                }
                if !crate::config::valid_kind(&i.kind) {
                    bail!("invalid identity kind `{}` (must be {})", i.kind, crate::config::kinds_list());
                }
            }
            for p in cfg.projects.iter_mut() {
                if p.id.is_none() {
                    p.id = Some(db::ulid());
                    dirty = true;
                }
                if let Some(id) = p.id.as_deref() {
                    if !valid_id(id) {
                        bail!("invalid project id `{id}` for project `{}` (must match {})", p.label, ID_RE);
                    }
                }
                // Only local paths can be canonicalized here; a remote path was already
                // canonicalized on its host when the project was created (SPEC §11.6).
                if p.host == crate::config::LOCAL_HOST {
                    if let Ok(c) = canonical_path(&p.path) {
                        if c != p.path {
                            p.path = c;
                            dirty = true;
                        }
                    }
                }
                for b in p.bots.iter_mut() {
                    if b.id.is_none() {
                        b.id = Some(db::ulid());
                        dirty = true;
                    }
                    if let Some(id) = b.id.as_deref() {
                        if !valid_id(id) {
                            bail!(
                                "invalid bot id `{id}` for bot `{}` in project `{}` (must match {})",
                                b.name,
                                p.label,
                                ID_RE
                            );
                        }
                    }
                    if !valid_bot_name(&b.name) {
                        bail!("invalid bot name `{}` ({})", b.name, crate::config::BOT_NAME_RE);
                    }
                    if !crate::config::valid_kind(&b.kind) {
                        bail!("invalid bot kind `{}` (must be {})", b.kind, crate::config::kinds_list());
                    }
                    if let Some(idn) = b.identity.as_deref().filter(|s| !s.is_empty()) {
                        // A host's shell `ccN` alias is a legal binding too (API.md §10.2: the
                        // identity list is `[[identities]]` ∪ that host's `ccN`); the config never
                        // owns those, so an unknown name is only an error outside that set.
                        match identities.iter().find(|i| i.name == idn) {
                            None if crate::tools::SHELL_IDENTITY_NAMES.contains(&idn) => {}
                            None => bail!("bot `{}` references unknown identity `{idn}`", b.name),
                            Some(i) if i.kind != b.kind => {
                                bail!("identity `{idn}` is for {} but bot `{}` is {}", i.kind, b.name, b.kind)
                            }
                            Some(_) => {}
                        }
                    }
                }
            }
            Ok(dirty)
        })
        .await?;
    if changed {
        tracing::info!("config.toml: filled in missing ids / canonical paths");
    }

    let cfg = store.get().await;
    let now = db::now();
    let live_projects: HashSet<String> = cfg.projects.iter().filter_map(|p| p.id.clone()).collect();
    let live_bots: HashSet<String> =
        cfg.projects.iter().flat_map(|p| p.bots.iter()).filter_map(|b| b.id.clone()).collect();

    // 任何寫入之前先擋：投錯 DB／被換掉的 config 長得就像「config 裡什麼都沒有」。
    guard_removals(pool, &live_projects, &live_bots, allow).await?;

    for (p_at, p) in cfg.projects.iter().enumerate() {
        let pid = p.id.clone().unwrap();
        sqlx::query(
            "INSERT INTO projects (id, path, label, host, position, created_at) VALUES (?,?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET path=excluded.path, label=excluded.label,
               host=excluded.host, position=excluded.position, deleted_at=NULL",
        )
        .bind(&pid)
        .bind(&p.path)
        .bind(&p.label)
        .bind(&p.host)
        // 陣列位置就是側欄順序（`POST /api/order` 會重排這個陣列）。
        .bind(p_at as i64)
        .bind(&now)
        .execute(pool)
        .await?;

        for (b_at, b) in p.bots.iter().enumerate() {
            let bid = b.id.clone().unwrap();
            let args_json = serde_json::to_string(&b.args)?;
            let env_json = serde_json::to_string(&b.env)?;
            let token = new_token();
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, model, effort, fast, persona, args_json, autostart, inject_hooks, auto_approve, identity, env_json, herdr_session, position, hook_token, created_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, name=excluded.name, kind=excluded.kind,
                   model=excluded.model, effort=excluded.effort, fast=excluded.fast, persona=excluded.persona, args_json=excluded.args_json, autostart=excluded.autostart, inject_hooks=excluded.inject_hooks,
                   auto_approve=excluded.auto_approve, identity=excluded.identity, env_json=excluded.env_json, herdr_session=excluded.herdr_session, position=excluded.position, deleted_at=NULL",
            )
            .bind(&bid)
            .bind(&pid)
            .bind(&b.name)
            .bind(&b.kind)
            .bind(b.model.as_deref().filter(|s| !s.trim().is_empty()))
            // v4.0: effort is kind-dependent; a hand-edited value that the kind rejects is dropped.
            .bind(crate::config::normalize_effort(&b.kind, b.effort.as_deref()).unwrap_or(None))
            .bind(b.fast as i64)
            .bind(b.persona.as_deref().filter(|s| !s.trim().is_empty()))
            .bind(&args_json)
            .bind(b.autostart as i64)
            .bind(b.inject_hooks as i64)
            .bind(b.auto_approve as i64)
            .bind(&b.identity)
            .bind(&env_json)
            .bind(&b.herdr_session)
            .bind(b_at as i64)
            .bind(&token)
            .bind(&now)
            .execute(pool)
            .await?;
            db::conversation_id(pool, &bid).await?;
        }
    }

    // 2. soft-delete rows no longer in the TOML.
    //
    // `child` bots (agents a bot spawned, adopted by the reconcile) never enter config.toml,
    // so the "not in the TOML ⇒ deleted" rule must not touch them.
    for b in db::live_bots(pool).await? {
        if b.managed_by != "user" {
            continue;
        }
        if !live_bots.contains(&b.id) {
            sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(&now).bind(&b.id).execute(pool).await?;
            tracing::info!(bot = %b.name, "bot removed from config.toml; soft-deleted");
        }
    }
    for p in db::live_projects(pool).await? {
        if !live_projects.contains(&p.id) {
            sqlx::query("UPDATE projects SET deleted_at=? WHERE id=?").bind(&now).bind(&p.id).execute(pool).await?;
        }
    }
    Ok(())
}

/// 一次從 config 消失超過這麼多列，就不是「使用者刪了一顆」，而是投影投錯了 DB。
const MAX_REMOVED: usize = 3;
/// 或是一口氣少掉三成以上；兩顆以上才算，免得「三顆刪一顆」這種正常操作被擋。
const MAX_REMOVED_RATIO: f64 = 0.30;
/// 真的要刪這麼多（人已確認）時，用這個 env 放行一次。
pub const ALLOW_BULK_ENV: &str = "AM_ALLOW_BULK_DELETE";

fn too_many(gone: usize, total: usize) -> bool {
    gone > MAX_REMOVED || (gone > 1 && gone as f64 > total as f64 * MAX_REMOVED_RATIO)
}

fn some_names(names: &[String]) -> String {
    let head: Vec<&str> = names.iter().take(5).map(String::as_str).collect();
    if names.len() > head.len() {
        format!("{}…", head.join("、"))
    } else {
        head.join("、")
    }
}

/// 大量軟刪的閘門（2026-09-14 事故）：第二顆 daemon 用 /tmp 的空 config 開到正式 DB，
/// 8 秒內把 15 顆 bot、6 個專案標成 `deleted_at`。DB 的活列＝上一次投影的結果，所以
/// 「config 空了但 DB 還有列」必然是拿錯 config／被換掉的檔案，不是使用者剛刪完——
/// 真的刪走的是刪除 API，那條路自己帶授權（`project_config_after_delete`）。
async fn guard_removals(
    pool: &SqlitePool,
    live_projects: &HashSet<String>,
    live_bots: &HashSet<String>,
    allow: Option<&Deleting>,
) -> Result<()> {
    // `child` bot 本來就不在 config.toml 裡，不算「少掉」。
    let db_bots: Vec<db::Bot> =
        db::live_bots(pool).await?.into_iter().filter(|b| b.managed_by == "user").collect();
    let db_projects = db::live_projects(pool).await?;
    let (gone_bots, gone_projects) = removals(&db_bots, &db_projects, live_projects, live_bots, allow);
    if gone_bots.is_empty() && gone_projects.is_empty() {
        return Ok(());
    }
    // 刪除模式：授權名單以外只要還有一列會不見就拒絕，不看門檻、也不吃 env 放行
    // （`delete_from_config` 在寫檔前已經擋過一次，這裡是投影當下的最後一道）。
    if allow.is_some() {
        let detail = format!(
            "投影時發現授權以外的 removals：{} 顆 bot（{}）與 {} 個專案（{}）",
            gone_bots.len(),
            some_names(&gone_bots),
            gone_projects.len(),
            some_names(&gone_projects),
        );
        tracing::error!("{detail}");
        return Err(anyhow::Error::new(DeleteRefused(detail)));
    }

    let empty_config = live_projects.is_empty();
    let bulk = too_many(gone_bots.len(), db_bots.len()) || too_many(gone_projects.len(), db_projects.len());
    if !empty_config && !bulk {
        return Ok(());
    }

    let why = if empty_config { "config.toml 沒有任何專案" } else { "一次少掉太多列" };
    let detail = format!(
        "{why}，但 DB 裡有 {} 顆 bot／{} 個專案：會軟刪 {} 顆 bot（{}）與 {} 個專案（{}）",
        db_bots.len(),
        db_projects.len(),
        gone_bots.len(),
        some_names(&gone_bots),
        gone_projects.len(),
        some_names(&gone_projects),
    );
    if std::env::var(ALLOW_BULK_ENV).map(|v| v == "1").unwrap_or(false) {
        tracing::warn!("{ALLOW_BULK_ENV}=1：照使用者確認的做大量軟刪（{detail}）");
        return Ok(());
    }
    tracing::error!("拒絕投影 config.toml：{detail}");
    bail!(
        "拒絕投影 config.toml：{detail}。\
         這通常是 daemon 開錯資料目錄（同一顆 DB 被另一份 config 投影），不是有人刪了 bot；\
         先確認 --config 與資料目錄（見 startup.rs）。確認過真的要刪就用 {ALLOW_BULK_ENV}=1 放行一次。"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn projection_error(project_id: &str, bot_id: &str) -> String {
        let dir = std::env::temp_dir().join(format!("am-projection-id-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let text = format!(
            "[server]\nlisten = '127.0.0.1:7788'\n\n[[projects]]\nid = '{project_id}'\npath = '/tmp'\nlabel = 'demo'\nhost = 'remote'\n\n[[projects.bots]]\nid = '{bot_id}'\nname = 'worker'\nkind = 'claude'\n"
        );
        std::fs::write(&path, text).unwrap();
        let store = ConfigStore::load(path).await.unwrap();
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        let error = project_config(&store, &pool).await.unwrap_err().to_string();
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
        error
    }

    fn config_text(bots: &[&str]) -> String {
        let mut text = String::from(
            "[server]\nlisten = '127.0.0.1:7788'\n\n[[projects]]\nid = 'p1'\npath = '/tmp'\nlabel = 'demo'\nhost = 'remote'\n",
        );
        for b in bots {
            text.push_str(&format!("\n[[projects.bots]]\nid = '{b}'\nname = '{b}'\nkind = 'claude'\n"));
        }
        text
    }

    async fn project_text(path: &std::path::Path, pool: &SqlitePool, text: &str) -> Result<()> {
        std::fs::write(path, text).unwrap();
        project_config(&ConfigStore::load(path.to_path_buf()).await.unwrap(), pool).await
    }

    /// 2026-09-14：第二顆 daemon 用 /tmp 的空 config 開到正式 DB，8 秒軟刪 15 顆 bot／6 個專案。
    #[tokio::test]
    async fn refuses_an_empty_config_over_a_populated_db() {
        let dir = std::env::temp_dir().join(format!("am-projection-bulk-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();

        project_text(&path, &pool, &config_text(&["b1", "b2", "b3", "b4"])).await.unwrap();
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 4);

        let err = project_text(&path, &pool, "[server]\nlisten = '127.0.0.1:7788'\n")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("拒絕投影"), "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 4, "拒絕投影時一列都不能動");
        assert_eq!(db::live_projects(&pool).await.unwrap().len(), 1);

        // 人確認過就放行一次。
        std::env::set_var(ALLOW_BULK_ENV, "1");
        let out = project_text(&path, &pool, "[server]\nlisten = '127.0.0.1:7788'\n").await;
        std::env::remove_var(ALLOW_BULK_ENV);
        out.unwrap();
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 0);
        assert_eq!(db::live_projects(&pool).await.unwrap().len(), 0);

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// runtime 也要擋：外面把 TOML 換掉／清空，`ConfigStore::update` 會在 mtime 變了時重讀，
    /// 再由 API／總管觸發重投——不擋的話事故路徑只是換個入口（sol 複審 2026-09-14）。
    #[tokio::test]
    async fn a_config_swapped_under_a_running_daemon_is_refused() {
        let dir = std::env::temp_dir().join(format!("am-projection-reload-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();

        std::fs::write(&path, config_text(&["b1", "b2", "b3", "b4"])).unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();
        project_config(&store, &pool).await.unwrap();

        // daemon 還活著、store 還在手上，檔案被外面換成另一份（這裡是空的）。
        std::fs::write(&path, "[server]\nlisten = '127.0.0.1:7788'\n").unwrap();
        store.update(|_| Ok(())).await.unwrap();
        let err = project_config(&store, &pool).await.unwrap_err().to_string();
        assert!(err.contains("拒絕投影"), "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 4, "一列都不能動");

        // 刪除 API 找不到目標（TOML 被清空）就拒絕：不會因為有人按了一次刪除就全部放行。
        let err = delete_from_config(&store, &pool, DeleteTarget::Bot("b1")).await.unwrap_err();
        assert!(err.downcast_ref::<NotInConfig>().is_some(), "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 4, "一列都不能動");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    async fn seeded(name: &str, bots: &[&str]) -> (std::path::PathBuf, ConfigStore, SqlitePool) {
        let dir = std::env::temp_dir().join(format!("am-projection-{name}-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        project_text(&path, &pool, &config_text(bots)).await.unwrap();
        (dir, ConfigStore::load(path).await.unwrap(), pool)
    }

    /// 刪掉一個含多顆 bot 的專案仍是正常操作：授權範圍從當下的 TOML 算出來。
    #[tokio::test]
    async fn deleting_a_project_removes_it_and_its_bots() {
        let (dir, store, pool) = seeded("del-project", &["b1", "b2", "b3", "b4"]).await;
        let held: HashSet<String> = ["b1", "b2", "b3", "b4"].map(String::from).into();
        let allow = delete_from_config(&store, &pool, DeleteTarget::Project { id: "p1", held: &held }).await.unwrap();
        assert_eq!(allow.bots.len(), 4);
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 0);
        assert_eq!(db::live_projects(&pool).await.unwrap().len(), 0);
        assert!(store.get().await.projects.is_empty());
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 1 個專案／1 顆 bot 的 TOML 被外部清空：刪專案、刪 bot 都找不到目標 → 拒絕、不寫、不投影。
    #[tokio::test]
    async fn a_cleared_toml_turns_every_delete_into_a_refusal() {
        let (dir, store, pool) = seeded("del-cleared", &["b1"]).await;
        std::fs::write(&store.path, "[server]\nlisten = '127.0.0.1:7788'\n").unwrap();
        let held: HashSet<String> = ["b1".to_string()].into();
        for target in [DeleteTarget::Project { id: "p1", held: &held }, DeleteTarget::Bot("b1")] {
            let err = delete_from_config(&store, &pool, target).await.unwrap_err();
            assert!(err.downcast_ref::<NotInConfig>().is_some(), "{err}");
        }
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 1);
        assert_eq!(db::live_projects(&pool).await.unwrap().len(), 1);
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 1 個專案／2 顆 bot，外部拿掉 b2 之後刪 b1：只要授權以外還有一列會不見就拒絕，
    /// 不套一般的小量刪除門檻；config 也不寫（b1 還在）。
    #[tokio::test]
    async fn a_delete_refuses_any_removal_it_did_not_ask_for() {
        let (dir, store, pool) = seeded("del-extra", &["b1", "b2"]).await;
        std::fs::write(&store.path, config_text(&["b1"])).unwrap();
        let err = delete_from_config(&store, &pool, DeleteTarget::Bot("b1")).await.unwrap_err();
        assert!(err.downcast_ref::<DeleteRefused>().is_some(), "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 2);
        let on_disk = std::fs::read_to_string(&store.path).unwrap();
        assert!(on_disk.contains("b1"), "拒絕時 config 不能寫：{on_disk}");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 刪專案時 TOML 裡多了一顆呼叫端沒鎖住的 bot（剛建立、可能正要啟動）→ 拒絕，一列都不動。
    #[tokio::test]
    async fn a_project_delete_refuses_bots_it_does_not_hold() {
        let (dir, store, pool) = seeded("del-unheld", &["b1", "b2"]).await;
        let held: HashSet<String> = ["b1".to_string()].into();
        let err = delete_from_config(&store, &pool, DeleteTarget::Project { id: "p1", held: &held }).await.unwrap_err();
        assert!(err.downcast_ref::<DeleteRefused>().is_some(), "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 2);
        assert_eq!(store.get().await.projects.len(), 1);
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 兩個不同的刪除真的同時跑（多執行緒＋Barrier）：各刪各的，都成功。沒有共用臨界區時，
    /// 後投影的那一邊會把對方剛寫進 config 的刪除當成未授權而拒絕。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_deletes_are_serialized_not_mixed() {
        for round in 0..5 {
            let (dir, store, pool) = seeded(&format!("del-race-{round}"), &["b1", "b2", "b3", "b4"]).await;
            let (store, pool) = (std::sync::Arc::new(store), pool);
            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let run = |id: &'static str| {
                let (store, pool, gate) = (store.clone(), pool.clone(), gate.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    delete_from_config(&store, &pool, DeleteTarget::Bot(id)).await.map(|_| ()).map_err(|e| e.to_string())
                })
            };
            let (a, b) = (run("b1"), run("b2"));
            assert_eq!(a.await.unwrap(), Ok(()), "round {round}");
            assert_eq!(b.await.unwrap(), Ok(()), "round {round}");
            let live: Vec<String> = db::live_bots(&pool).await.unwrap().into_iter().map(|b| b.id).collect();
            assert_eq!(live, vec!["b3", "b4"], "round {round}");
            pool.close().await;
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// 正常路徑不變：刪一顆還是刪一顆。
    #[tokio::test]
    async fn a_single_removal_still_projects() {
        let dir = std::env::temp_dir().join(format!("am-projection-one-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();

        project_text(&path, &pool, &config_text(&["b1", "b2", "b3", "b4"])).await.unwrap();
        project_text(&path, &pool, &config_text(&["b1", "b2", "b3"])).await.unwrap();
        let live: Vec<String> = db::live_bots(&pool).await.unwrap().into_iter().map(|b| b.id).collect();
        assert_eq!(live, vec!["b1", "b2", "b3"]);

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_threshold_lets_normal_deletes_through() {
        assert!(!too_many(1, 1));
        assert!(!too_many(1, 20));
        assert!(!too_many(3, 20));
        assert!(too_many(4, 20));
        assert!(too_many(2, 4));
        assert!(too_many(15, 15));
    }

    #[tokio::test]
    async fn rejects_invalid_project_and_bot_ids() {
        for id in ["../..", "foo/bar", r"..\..", ""] {
            let error = projection_error(id, "bot-1").await;
            assert!(error.contains("invalid project id"), "{error}");
            assert!(error.contains(id), "{error}");
        }
        for id in ["../..", "foo/bar", r"..\..", ""] {
            let error = projection_error("project-1", id).await;
            assert!(error.contains("invalid bot id"), "{error}");
            assert!(error.contains("worker") && error.contains("demo"), "{error}");
        }
    }
}
