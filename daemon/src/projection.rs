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
use std::panic::Location;

pub fn new_token() -> String {
    let mut rng = rand::thread_rng();
    (0..32).map(|_| std::char::from_digit(rng.gen_range(0..16), 16).unwrap()).collect()
}

/// 投影與刪除共用的臨界區：「讀 config → 算 diff → guard → 寫 config → 投影」之間不能插進另一次投影或刪除，
/// 否則 A 會讀到 B 剛寫的 config、把 B 的刪除當成自己沒授權的 removal，或反過來替 B 放行（sol 三輪）。
static PROJECTION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Fill in missing ids (writing them back to the TOML) and upsert everything into SQLite.
///
/// 帶大量軟刪的閘門：啟動與 runtime 的每一次重投都走這條。`ConfigStore::update` 每次都從磁碟重讀，
/// 所以「外面把 TOML 換掉／清空，再由 API 或總管觸發重投」也是事故路徑，不能只擋啟動。
#[track_caller]
pub fn project_config<'a>(store: &'a ConfigStore, pool: &'a SqlitePool) -> impl std::future::Future<Output = Result<()>> + 'a {
    let at = Location::caller();
    async move {
        let _g = PROJECTION.lock().await;
        project_inner(at, store, pool, None, false).await
    }
}

/// 啟動時那一次投影。`allow_bulk`＝使用者帶 `AM_ALLOW_BULK_DELETE=1` 重啟 daemon（`serve` 啟動時讀一次 env）：
/// **只放行這一次**。之後 runtime 的每次重投都不吃這個 env——閘門下沉到每次投影之後，env 一直留在行程裡，
/// 「放行一次」就變成整個行程期間放行，config 再被外部換掉時只記 warn 就照刪（review3 c3 L7）。
#[track_caller]
pub fn project_config_at_startup<'a>(
    store: &'a ConfigStore,
    pool: &'a SqlitePool,
    allow_bulk: bool,
) -> impl std::future::Future<Output = Result<()>> + 'a {
    let at = Location::caller();
    async move {
        let _g = PROJECTION.lock().await;
        project_inner(at, store, pool, None, allow_bulk).await
    }
}

/// `serve` 啟動時讀一次；其他地方不讀。
pub fn bulk_delete_allowed_by_env() -> bool {
    std::env::var(ALLOW_BULK_ENV).is_ok_and(|v| v == "1")
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

/// 投影被大量軟刪閘門擋下來。**不是上游壞掉**：呼叫端（UI、AGM、其他 bot）要分得出
/// 「你的設定沒被套用，因為 config.toml 看起來被外部改過」跟「ssh 斷了」，否則只會看到一個
/// 沒有線索的 502，而且之後每一次寫設定都會再撞一次（review 2026-09-16）。
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct ProjectionRefused {
    pub detail: String,
    /// 這次會被軟刪掉的 bot／專案名字，讓人知道要去 config.toml 補回哪幾列。
    pub bots: Vec<String>,
    pub projects: Vec<String>,
    /// Supervisor-owned child bot names: these can never be removed by an implicit projection.
    pub supervisor_children: Vec<String>,
    /// Bot-count limit violations are never waived by AM_ALLOW_BULK_DELETE.
    pub bot_limit_exceeded: bool,
}

/// 刪除的單一臨界區：重讀 config → 確認目標此刻在 TOML → 算出實際要拿掉的 id → 閘門（寫入前）→
/// 寫 config → 投影。呼叫端（API）在這之前拿好 per-bot 鎖、在這之後才停機，所以拒絕一定發生在任何東西被停之前。
#[track_caller]
pub fn delete_from_config<'a>(
    store: &'a ConfigStore,
    pool: &'a SqlitePool,
    target: DeleteTarget<'a>,
) -> impl std::future::Future<Output = Result<Deleting>> + 'a {
    let at = Location::caller();
    async move { delete_from_config_at(at, store, pool, target).await }
}

async fn delete_from_config_at(at: &'static Location<'static>, store: &ConfigStore, pool: &SqlitePool, target: DeleteTarget<'_>) -> Result<Deleting> {
    let _g = PROJECTION.lock().await;
    // 臨界區內 DB 的 user bot／project 只有投影會改，所以這份快照在寫 config 之前都成立。
    let db_bots: Vec<db::Bot> =
        db::live_bots(pool).await?.into_iter().filter(|b| b.managed_by == "user").collect();
    let db_projects = db::live_projects(pool).await?;
    let allow = store
        .update_guarded_at(at, |cfg| {
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
        }, |_| Ok(()))
        .await?;
    project_inner(at, store, pool, Some(&allow), false).await?;
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

/// 投影的乾跑：**這份 config 投出去會不會被擋？** 純函式，不碰 DB、不碰檔案。
///
/// `ConfigStore::update` 在 `write_atomic` 之前對「改完之後」的整份 config 跑這支（issue #73）。
/// 以前只在投影當下查，而投影是在 config 已經落盤之後才跑的：一筆會被擋下的修改先把 TOML 改壞，
/// API 回錯誤，現場卻已經變了，daemon 下次啟動才爆（`project_inner` 裡那句「而那時 config 已經落盤、
/// 之後 daemon 起不來」講的就是這個）。搬到落盤前，錯誤照樣回，但檔案一個字都不會動。
///
/// 只查**投影自己會擋**的事。需要 DB 才判得出來的安全閘（大量軟刪）不在這裡：那條路是刪除
/// （`delete_from_config`），它本來就是先在記憶體算出 `next`、對著 DB 快照驗過才 `*cfg = next`。
pub fn validate(cfg: &crate::config::ConfigFile) -> Result<()> {
    // 包成具名型別，呼叫端才分得出「你的設定有問題」與「上游壞掉」（api.rs 的 `cfg_err`）。
    check(cfg).map_err(|e| anyhow::Error::new(ConfigInvalid(format!("{e:#}"))))
}

/// 設定本身不合法。**看到它就代表 config.toml 沒有被動過**（`ConfigStore::update` 在落盤前擋下來）。
#[derive(Debug, thiserror::Error)]
#[error("{0}（config.toml 未變更）")]
pub struct ConfigInvalid(pub String);

fn check(cfg: &crate::config::ConfigFile) -> Result<()> {
    for i in &cfg.identities {
        if !crate::config::valid_identity_name(&i.name) {
            bail!("invalid identity name `{}` (must match {})", i.name, crate::config::SLUG_NAME_RE);
        }
        if !crate::config::valid_kind(&i.kind) {
            bail!("invalid identity kind `{}` (must be {})", i.kind, crate::config::kinds_list());
        }
    }
    for p in &cfg.projects {
        if let Some(id) = p.id.as_deref() {
            if !valid_id(id) {
                bail!("invalid project id `{id}` for project `{}` (must match {})", p.label, ID_RE);
            }
        }
        for b in &p.bots {
            if let Some(id) = b.id.as_deref() {
                if !valid_id(id) {
                    bail!("invalid bot id `{id}` for bot `{}` in project `{}` (must match {})", b.name, p.label, ID_RE);
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
                // 身份以 `(host, name)` 為鍵（`26a14c2`）：本機的 `work`（claude）與 m4p 的 `work`（codex）可以並存，
                // 只看名字會拿到第一筆同名的、判成 kind 不符。規則跟 API 的 `tools::identity_for_host`
                // 同一份（`merge_identities`）。這裡拿不到那台偵測到的 `ccN`，所以傳 `None`：沒寫 host 的
                // `ccN` 在遠端先不給，落到「shell alias 本來就合法」那條。
                let merged = crate::tools::merge_identities(&cfg.identities, &p.host, None);
                match merged.iter().map(|(i, _)| i).find(|i| i.name == idn) {
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
    Ok(())
}

/// 統一 commit boundary（issue #73 reopen）：mutation 的「套用 → 純驗證 → DB-backed 大量軟刪閘門」在
/// **同一個 `PROJECTION` 臨界區內、寫入 TOML 之前**做完，全部過了才寫檔、才投影進 SQLite。
///
/// 取代「呼叫端先 `ConfigStore::update` 落盤、再另外呼叫 `project_config` 投影」那個兩段式：中間那個縫隙
/// 會讓一筆會被大量軟刪閘門擋下的修改先把 TOML 改壞（`config_written: true`），DB 卻沒套用——閘門本身
/// 需要 DB 快照才判得出來，以前只能等寫完檔、真正投影時才問。這裡在寫檔前用同一把 `PROJECTION` 鎖把
/// DB 快照查出來，交給 [`ConfigStore::update_guarded_at`] 的 `guard` 做同步比對；驗不過就直接回錯誤，
/// `next` 被丟掉、檔案與 DB 都不動。
///
/// 只給「這次呼叫本身就是一筆 mutation」的呼叫端用（目前是 API 的 Project／Bot 寫入端點）。
/// 沒有伴隨 mutation 的重投（啟動時的 `project_config_at_startup`、背景巡邏的定期 reproject）不走這裡：
/// 那些是把既有 config 套進 DB，不是「這次要不要寫」的判斷，繼續用 `project_config`。
#[track_caller]
pub fn update_and_project<'a, F, T>(
    store: &'a ConfigStore,
    pool: &'a SqlitePool,
    f: F,
) -> impl std::future::Future<Output = Result<T>> + 'a
where
    F: FnOnce(&mut crate::config::ConfigFile) -> Result<T> + 'a,
    T: 'a,
{
    let at = Location::caller();
    async move { update_and_project_at(at, store, pool, f).await }
}

async fn update_and_project_at<F, T>(at: &'static Location<'static>, store: &ConfigStore, pool: &SqlitePool, f: F) -> Result<T>
where
    F: FnOnce(&mut crate::config::ConfigFile) -> Result<T>,
{
    let _g = PROJECTION.lock().await;
    let db_bots: Vec<db::Bot> =
        db::live_bots(pool).await?.into_iter().filter(|b| b.managed_by == "user").collect();
    let db_projects = db::live_projects(pool).await?;
    let owned = crate::supervisor_owned::load(pool).await?;
    let recent = recently_removed_bots(pool).await?;
    let out = store
        .update_guarded_at(at, f, |next| {
            let (live_projects, live_bots) = live_ids(next);
            match bulk_removal_check(&db_bots, &db_projects, &live_projects, &live_bots, &owned, recent) {
                Ok(()) => Ok(()),
                Err(refused) => {
                    tracing::warn!(caller = %at, http = %crate::config_audit::http_caller(), "拒絕投影 config.toml；本次不執行軟刪：{}", refused.detail);
                    Err(anyhow::Error::new(refused))
                }
            }
        })
        .await;
    let out = match out {
        Ok(v) => v,
        Err(e) => {
            alert_on_refusal(pool, &e).await;
            return Err(e);
        }
    };
    // 寫檔已經過了同一份閘門，這裡是投影（`delete_from_config` 也是同一個形狀：拿著 `_g` 直接叫
    // `project_inner`，不重新拿鎖）。DB 沒被別人動過（還在臨界區內），所以 `project_inner` 自己那次
    // `guard_removals` 一定過，純粹是既有流程（補 id、upsert、soft-delete）的重用。
    project_inner(at, store, pool, None, false).await?;
    Ok(out)
}

async fn project_inner(
    at: &'static Location<'static>,
    store: &ConfigStore,
    pool: &SqlitePool,
    allow: Option<&Deleting>,
    allow_bulk: bool,
) -> Result<()> {
    // 1. fill in ids / canonicalize paths, write back only if something changed.
    //    連帶在落盤前套用刪除閘門：補 id 的寫回也不能先於拒絕決定。
    let db_bots: Vec<db::Bot> =
        db::live_bots(pool).await?.into_iter().filter(|b| b.managed_by == "user").collect();
    let db_projects = db::live_projects(pool).await?;
    let owned = if allow.is_none() { crate::supervisor_owned::load(pool).await? } else { Default::default() };
    let recent = if allow.is_none() { recently_removed_bots(pool).await? } else { 0 };
    let changed = store
        .update_guarded_at(
            at,
            |cfg| {
                let mut dirty = false;
                for p in cfg.projects.iter_mut() {
                    if p.id.is_none() {
                        p.id = Some(db::ulid());
                        dirty = true;
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
                    }
                }
                Ok(dirty)
            },
            |next| {
                if allow.is_some() {
                    return Ok(());
                }
                let (live_projects, live_bots) = live_ids(next);
                match bulk_removal_check(&db_bots, &db_projects, &live_projects, &live_bots, &owned, recent) {
                    Ok(()) => Ok(()),
                    Err(refused)
                        if allow_bulk && refused.supervisor_children.is_empty() && !refused.bot_limit_exceeded =>
                    {
                        tracing::warn!("{ALLOW_BULK_ENV}=1：照使用者確認的做大量軟刪（{}）", refused.detail);
                        Ok(())
                    }
                    Err(refused) => {
                        tracing::warn!(caller = %at, http = %crate::config_audit::http_caller(), "拒絕投影 config.toml；本次不執行軟刪：{}", refused.detail);
                        Err(anyhow::Error::new(refusal_with_guidance(refused)))
                    }
                }
            },
        )
        .await;
    let changed = match changed {
        Ok(v) => v,
        Err(e) => {
            alert_on_refusal(pool, &e).await;
            return Err(e);
        }
    };
    if changed {
        tracing::info!("config.toml: filled in missing ids / canonical paths");
    }

    let cfg = store.get().await;
    let now = db::now();
    let live_projects: HashSet<String> = cfg.projects.iter().filter_map(|p| p.id.clone()).collect();
    let live_bots: HashSet<String> =
        cfg.projects.iter().flat_map(|p| p.bots.iter()).filter_map(|b| b.id.clone()).collect();

    // 任何寫入之前先擋：投錯 DB／被換掉的 config 長得就像「config 裡什麼都沒有」。
    if let Err(e) = guard_removals(pool, &live_projects, &live_bots, allow, allow_bulk).await {
        alert_on_refusal(pool, &e).await;
        return Err(e);
    }

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
                "INSERT INTO bots (id, project_id, name, kind, model, effort, fast, persona, instruction_files, args_json, autostart, inject_hooks, auto_approve, identity, env_json, herdr_session, position, hook_token, created_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, name=excluded.name, kind=excluded.kind,
                   model=excluded.model, effort=excluded.effort, fast=excluded.fast, persona=excluded.persona, instruction_files=excluded.instruction_files, args_json=excluded.args_json, autostart=excluded.autostart, inject_hooks=excluded.inject_hooks,
                   auto_approve=excluded.auto_approve, identity=excluded.identity, env_json=excluded.env_json, herdr_session=excluded.herdr_session, position=excluded.position, deleted_at=NULL",
            )
            .bind(&bid)
            .bind(&pid)
            .bind(&b.name)
            .bind(&b.kind)
            .bind(b.model.as_deref().filter(|s| !s.trim().is_empty()).map(|m| crate::models::canonical_model(&b.kind, m.trim())))
            // v4.0: effort is kind-dependent; a hand-edited value that the kind rejects is dropped.
            .bind(crate::config::normalize_effort(&b.kind, b.effort.as_deref()).unwrap_or(None))
            .bind(b.fast as i64)
            .bind(b.persona.as_deref().filter(|s| !s.trim().is_empty()))
            // 手改 TOML 寫錯（值不在 CLI 的選項裡、或不是 claude）丟掉存 NULL＝釘住的預設；寫進 --settings 的話 CLI 會退回它自己的預設。
            .bind(crate::config::normalize_instruction_files(&b.kind, b.instruction_files.as_deref()).unwrap_or(None))
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
            tracing::info!(bot = %b.name, bot_id = %b.id, caller = %at, http = %crate::config_audit::http_caller(), "bot removed from config.toml; soft-deleted");
        }
    }
    for p in db::live_projects(pool).await? {
        if !live_projects.contains(&p.id) {
            sqlx::query("UPDATE projects SET deleted_at=? WHERE id=?").bind(&now).bind(&p.id).execute(pool).await?;
        }
    }
    Ok(())
}

/// Implicit projection may retire at most one user bot at a time; explicit DELETE has its own authorization.
const MAX_REMOVED_BOTS: usize = 1;
/// Projects retain the previous count limit. The ratio also catches large drops from larger configurations.
const MAX_REMOVED_PROJECTS: usize = 3;
/// A drop above thirty percent is suspicious; single-row changes are allowed by the ratio rule.
const MAX_REMOVED_RATIO: f64 = 0.30;
/// 真的要刪這麼多（人已確認）時，帶這個 env 重啟 daemon：只放行啟動那一次投影。
pub const ALLOW_BULK_ENV: &str = "AM_ALLOW_BULK_DELETE";

fn too_many(gone: usize, total: usize, max_removed: usize) -> bool {
    gone > max_removed || (gone > 1 && gone as f64 > total as f64 * MAX_REMOVED_RATIO)
}

fn some_names(names: &[String]) -> String {
    let head: Vec<&str> = names.iter().take(5).map(String::as_str).collect();
    if names.len() > head.len() {
        format!("{}…", head.join("、"))
    } else {
        head.join("、")
    }
}

fn refusal_with_guidance(mut refused: ProjectionRefused) -> ProjectionRefused {
    let hint = if !refused.supervisor_children.is_empty() {
        "補回 supervisor child 的設定列或先由人工確認處理；AM_ALLOW_BULK_DELETE 不會放行 supervisor child.".to_string()
    } else if refused.bot_limit_exceeded {
        "本次 bot 軟刪數超過隱式投影上限；請修復 config.toml 後重試，AM_ALLOW_BULK_DELETE 不會放行 bot 刪除.".to_string()
    } else {
        format!(
            "確認過真的要刪，就帶 {ALLOW_BULK_ENV}=1 重啟 daemon（只放行啟動時那一次投影，之後的重投照樣擋）。"
        )
    };
    refused.detail = format!(
        "拒絕投影 config.toml：{}。這通常是 daemon 開錯資料目錄或 config.toml 被外部改寫；\
         先確認 --config 與資料目錄（見 startup.rs）。{}",
        refused.detail, hint
    );
    refused
}

/// 「一次最多軟刪 1 顆」的計數窗口（issue #406）：只看單次投影的話，config 被連續改兩次、兩次投影各少 1 顆就繞過去了。
/// 窗口內已經軟刪過的 user bot（不論是投影還是刪除 API 刪的）都算進這一次的額度。
pub const REMOVAL_WINDOW_SECS: i64 = 60;

/// 窗口內軟刪的 user bot 數。直接讀 DB 的 `deleted_at`：跨重啟、跨呼叫端都是同一份帳，也不需要另外記。
async fn recently_removed_bots(pool: &SqlitePool) -> Result<usize> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE managed_by = 'user' AND deleted_at IS NOT NULL AND deleted_at >= ?")
        .bind(db::iso_in(-REMOVAL_WINDOW_SECS))
        .fetch_one(pool)
        .await?;
    Ok(n as usize)
}

/// 投影被擋下來＝有東西想刪 bot 而 daemon 沒照做：推 `ops_alert` 給巡檢，不能只留一行 log（issue #406）。
async fn alert_on_refusal(pool: &SqlitePool, e: &anyhow::Error) {
    let Some(r) = e.downcast_ref::<ProjectionRefused>() else { return };
    let reason = if r.supervisor_children.is_empty() { "projection_removal_refused" } else { "supervisor_bot_removal_refused" };
    crate::supervisor_owned::alert(pool, reason, &format!("{}（呼叫端：{}）", r.detail, crate::config_audit::http_caller())).await;
}

/// 大量軟刪閘門的純判斷（2026-09-14 事故）：`next` 的活列相對於 DB 快照，是不是「config 空了但 DB 還有列」
/// 或「一次少掉太多列」。不碰 DB／檔案，只比對兩份已經算好的集合——commit 前（`update_and_project`）與
/// 投影當下（`guard_removals`）共用同一份規則，兩邊差在「什麼時候問」，不是「怎麼判」。
fn bulk_removal_check(
    db_bots: &[db::Bot],
    db_projects: &[db::Project],
    live_projects: &HashSet<String>,
    live_bots: &HashSet<String>,
    owned: &crate::supervisor_owned::Owned,
    recently_removed: usize,
) -> Result<(), ProjectionRefused> {
    let (gone_bots, gone_projects) = removals(db_bots, db_projects, live_projects, live_bots, None);
    // AGM 的 bot（總管／角色本身、它們的 child、總管專案裡的工人）：隱式投影一律不刪（issue #406）。
    let supervisor_children: Vec<String> =
        db_bots.iter().filter(|b| !live_bots.contains(&b.id) && owned.owns(b)).map(|b| b.name.clone()).collect();
    if gone_bots.is_empty() && gone_projects.is_empty() {
        return Ok(());
    }
    let empty_config = live_projects.is_empty();
    // 窗口內已經刪掉的也算：兩次投影各刪 1 顆＝這一次要刪第 2 顆。
    let bots_in_window = if gone_bots.is_empty() { 0 } else { gone_bots.len() + recently_removed };
    let bot_limit_exceeded = bots_in_window > MAX_REMOVED_BOTS;
    let bulk = too_many(bots_in_window, db_bots.len() + recently_removed, MAX_REMOVED_BOTS)
        || too_many(gone_projects.len(), db_projects.len(), MAX_REMOVED_PROJECTS);
    if !empty_config && !bulk && supervisor_children.is_empty() {
        return Ok(());
    }
    let why = if !supervisor_children.is_empty() {
        "config.toml 遺漏 AGM 的 bot"
    } else if empty_config {
        "config.toml 沒有任何專案"
    } else if recently_removed > 0 && gone_bots.len() <= MAX_REMOVED_BOTS {
        "前 60 秒內已經軟刪過 bot，這次又要再刪"
    } else {
        "一次少掉太多列"
    };
    Err(ProjectionRefused {
        detail: format!(
            "{why}，但 DB 裡有 {} 顆 bot／{} 個專案：會軟刪 {} 顆 bot（{}）與 {} 個專案（{}）；前 {REMOVAL_WINDOW_SECS} 秒內已軟刪 {recently_removed} 顆；AGM 的 bot：{}",
            db_bots.len(),
            db_projects.len(),
            gone_bots.len(),
            some_names(&gone_bots),
            gone_projects.len(),
            some_names(&gone_projects),
            some_names(&supervisor_children),
        ),
        bots: gone_bots,
        projects: gone_projects,
        supervisor_children,
        bot_limit_exceeded,
    })
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
    allow_bulk: bool,
) -> Result<()> {
    // `child` bot 本來就不在 config.toml 裡，不算「少掉」。
    let db_bots: Vec<db::Bot> =
        db::live_bots(pool).await?.into_iter().filter(|b| b.managed_by == "user").collect();
    let db_projects = db::live_projects(pool).await?;

    // 刪除模式：授權名單以外只要還有一列會不見就拒絕，不看門檻、也不吃 env 放行
    // （`delete_from_config` 在寫檔前已經擋過一次，這裡是投影當下的最後一道）。
    if let Some(allow) = allow {
        let (gone_bots, gone_projects) = removals(&db_bots, &db_projects, live_projects, live_bots, Some(allow));
        if gone_bots.is_empty() && gone_projects.is_empty() {
            return Ok(());
        }
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

    let owned = crate::supervisor_owned::load(pool).await?;
    let recent = recently_removed_bots(pool).await?;
    match bulk_removal_check(&db_bots, &db_projects, live_projects, live_bots, &owned, recent) {
        Ok(()) => Ok(()),
        Err(refused) if allow_bulk && refused.supervisor_children.is_empty() && !refused.bot_limit_exceeded => {
            tracing::warn!("{ALLOW_BULK_ENV}=1：照使用者確認的做大量軟刪（{}）", refused.detail);
            Ok(())
        }
        Err(refused) => {
            tracing::warn!("拒絕投影 config.toml；本次不執行軟刪：{}", refused.detail);
            Err(anyhow::Error::new(refusal_with_guidance(refused)))
        }
    }
}

/// `AM_ALLOW_BULK_DELETE` 是整個行程共用的環境變數：設它的測試與「期待被擋下」的測試平行跑時，
/// 後者會偶爾被放行（實測 3 次紅 2 次）。兩邊（含 api 的測試）都拿這把鎖。
#[cfg(test)]
pub(crate) static BULK_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    /// issue #73：會被投影擋下的修改，**連 config.toml 都不准動**。
    ///
    /// 以前的順序是「寫檔 → 投影 → 投影拒絕 → 回錯誤」：API 是回了錯沒錯，但現場已經被改掉，
    /// daemon 下次啟動才爆，而且爆在一個使用者沒同意過的狀態上。
    #[tokio::test]
    async fn a_mutation_the_projection_would_reject_leaves_the_file_and_the_db_untouched() {
        let dir = std::env::temp_dir().join(format!("am-cfg-validate-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[server]\nlisten = '127.0.0.1:7788'\n\n             [[identities]]\nname = 'work'\nkind = 'claude'\n\n             [[projects]]\nid = 'p1'\npath = '/tmp'\nlabel = 'demo'\nhost = 'remote'\n\n             [[projects.bots]]\nid = 'b1'\nname = 'worker'\nkind = 'claude'\nidentity = 'work'\n",
        )
        .unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        project_config(&store, &pool).await.unwrap();

        let before_text = std::fs::read_to_string(&path).unwrap();
        let before_cfg = store.get().await;
        let before_bots: Vec<(String, String)> =
            sqlx::query_as("SELECT id, kind FROM bots ORDER BY rowid").fetch_all(&pool).await.unwrap();
        assert_eq!(before_bots.len(), 1, "投影過了，DB 有那顆 bot");

        // 三種投影一定會擋的修改，每一種都不可以留下痕跡。
        let cases: Vec<(&str, Box<dyn Fn(&mut crate::config::ConfigFile)>)> = vec![
            (
                "unknown identity",
                Box::new(|cfg: &mut crate::config::ConfigFile| {
                    cfg.projects[0].bots[0].identity = Some("nobody".into());
                }),
            ),
            (
                "kind mismatch",
                Box::new(|cfg: &mut crate::config::ConfigFile| {
                    cfg.projects[0].bots[0].kind = "codex".into();
                }),
            ),
            (
                "invalid bot name",
                Box::new(|cfg: &mut crate::config::ConfigFile| {
                    cfg.projects[0].bots[0].name = "not@valid".into();
                }),
            ),
        ];
        for (what, mutate) in cases {
            let err = store
                .update(|cfg| {
                    mutate(cfg);
                    Ok(())
                })
                .await
                .expect_err(&format!("{what} 應該被擋下來"))
                .to_string();
            assert!(err.contains("config.toml 未變更"), "{what}: 錯誤要說清楚什麼都沒動：{err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before_text, "{what}: 檔案被動過了");
            assert_eq!(store.get().await, before_cfg, "{what}: 記憶體裡那份也不能變");
        }

        // DB 同樣沒被碰過，而且合法的修改照樣走得通（不是把整條路堵死）。
        let after_bots: Vec<(String, String)> =
            sqlx::query_as("SELECT id, kind FROM bots ORDER BY rowid").fetch_all(&pool).await.unwrap();
        assert_eq!(after_bots, before_bots, "DB 不該被動到");
        store.update(|cfg| { cfg.projects[0].label = "renamed".into(); Ok(()) }).await.expect("合法的改動要過");
        assert!(std::fs::read_to_string(&path).unwrap().contains("renamed"), "合法的改動要真的落盤");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
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

    #[tokio::test]
    async fn projection_canonicalizes_retired_config_models_but_preserves_explicit_versions() {
        let dir = std::env::temp_dir().join(format!("am-projection-models-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let text = "[server]\nlisten='127.0.0.1:7788'\n[[projects]]\nid='p1'\npath='/tmp'\nlabel='p'\nhost='local'\n[[projects.bots]]\nid='c'\nname='c'\nkind='codex'\nmodel='gpt-5.6-luna'\n[[projects.bots]]\nid='a'\nname='a'\nkind='claude'\nmodel='claude-opus-4-1'\n[[projects.bots]]\nid='alias'\nname='alias'\nkind='claude'\nmodel='opus'\n";
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        project_text(&path, &pool, text).await.unwrap();
        let models: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id,model FROM bots ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(models, vec![("a".into(), Some("claude-opus-4-1".into())), ("alias".into(), Some("claude-opus-5-5".into())), ("c".into(), Some("gpt-6-luna".into()))]);
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// review 2026-09-16 core 6：跨主機同名、不同 kind 的身份。API 分主機放行並寫進 config，投影卻只看名字、
    /// 拿到本機那份 claude 判成 kind 不符——之後每支寫設定的 API 都 502，重啟時 daemon 起不來。
    #[tokio::test]
    async fn an_identity_name_is_resolved_on_the_bots_own_host() {
        let dir = std::env::temp_dir().join(format!("am-projection-identity-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let text = |bot_kind: &str| {
            format!(
                "[server]\nlisten = '127.0.0.1:7788'\n\n\
                 [[identities]]\nname = 'work'\nkind = 'claude'\n\n\
                 [[identities]]\nname = 'work'\nkind = 'codex'\nhost = 'm4p'\n\n\
                 [[projects]]\nid = 'p1'\npath = '/Users/me/wt'\nlabel = 'wt'\nhost = 'm4p'\n\n\
                 [[projects.bots]]\nid = 'b1'\nname = 'worker'\nkind = '{bot_kind}'\nidentity = 'work'\n"
            )
        };
        std::fs::write(&path, text("codex")).unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        project_config(&store, &pool).await.expect("m4p 的 work 是 codex，跟 bot 一致");

        // 在 m4p 上綁 claude：那台的 work 是 codex，照樣擋（不會去拿本機那份 claude 放行）。
        std::fs::write(&path, text("claude")).unwrap();
        let err = project_config(&store, &pool).await.unwrap_err().to_string();
        assert!(err.contains("is for codex"), "{err}");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    use super::BULK_ENV;

    /// 2026-09-14：第二顆 daemon 用 /tmp 的空 config 開到正式 DB，曾經軟刪 15 顆 bot／6 個專案。
    #[tokio::test]
    async fn bulk_override_never_bypasses_the_bot_count_limit() {
        let _env = BULK_ENV.lock().await;
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

        // 人確認過、帶 env 重啟也不能一次軟刪多顆 bot。
        let err = project_config_at_startup(&ConfigStore::load(path.clone()).await.unwrap(), &pool, true)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("不會放行 bot 刪除"), "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 4);
        assert_eq!(db::live_projects(&pool).await.unwrap().len(), 1);

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `AM_ALLOW_BULK_DELETE=1` 只放行啟動那一次的空 config（單 bot）；runtime 重投照樣要擋。
    #[tokio::test]
    async fn the_bulk_allowance_does_not_outlive_the_startup_projection() {
        let _env = BULK_ENV.lock().await;
        let dir = std::env::temp_dir().join(format!("am-projection-once-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        std::fs::write(&path, config_text(&["b1"])).unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();
        project_config(&store, &pool).await.unwrap();

        // 使用者清空 config，帶 env 重啟：空設定的舊式明確覆寫仍放行一顆 bot。
        std::fs::write(&path, "[server]\nlisten = '127.0.0.1:7788'\n").unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();
        std::env::set_var(ALLOW_BULK_ENV, "1");
        assert!(bulk_delete_allowed_by_env());
        let started = project_config_at_startup(&store, &pool, bulk_delete_allowed_by_env()).await;
        started.unwrap();
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 0);

        // DB 有一顆 bot 時，外部清空後 runtime 重投不能靠 process env 放行。
        std::fs::write(&path, config_text(&["b1"])).unwrap();
        project_config_at_startup(&ConfigStore::load(path.clone()).await.unwrap(), &pool, false).await.unwrap();
        std::fs::write(&path, "[server]\nlisten = '127.0.0.1:7788'\n").unwrap();
        store.update(|_| Ok(())).await.unwrap();
        let out = project_config(&store, &pool).await;
        std::env::remove_var(ALLOW_BULK_ENV);
        let err = out.unwrap_err().to_string();
        assert!(err.contains("拒絕投影") && err.contains("重啟 daemon"), "{err}");
        assert_eq!(db::live_projects(&pool).await.unwrap().len(), 1, "一列都不能動");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 1, "一列都不能動");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// runtime 也要擋：外面把 TOML 換掉／清空，`ConfigStore::update` 每次都重讀，
    /// 再由 API／總管觸發重投——不擋的話事故路徑只是換個入口（sol 複審 2026-09-14）。
    #[tokio::test]
    async fn a_config_swapped_under_a_running_daemon_is_refused() {
        let _env = BULK_ENV.lock().await;
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

    /// issue #73 reopen：`update_and_project` 把 DB-backed 的大量軟刪閘門搬到落盤之前。以前只有
    /// `project_inner`（投影當下）會問，這裡驗的是「commit 之前就先問」這件事本身——`f` 只是把 bot 列表
    /// 換成一份會踩到閘門的（相對 DB 少了 3 顆），不代表任何一支真的存在的 API：目的是釘住
    /// `ConfigStore::update_guarded_at` 的 `guard` 真的接在寫檔前，不是形式上傳進去卻沒生效。
    #[tokio::test]
    async fn update_and_project_refuses_a_bulk_removal_before_writing() {
        let dir = std::env::temp_dir().join(format!("am-uap-refuse-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        project_text(&path, &pool, &config_text(&["b1", "b2", "b3", "b4"])).await.unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();

        let before_text = std::fs::read_to_string(&path).unwrap();
        let err = update_and_project(&store, &pool, |cfg| {
            cfg.projects[0].bots.truncate(1);
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(err.downcast_ref::<ProjectionRefused>().is_some(), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before_text, "TOML 一個字都不能動");
        assert_eq!(store.get().await.projects[0].bots.len(), 4, "記憶體裡那份也不能變");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 4, "DB 不該被動到");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn refuses_two_missing_bots_even_when_the_ratio_is_below_thirty_percent() {
        let dir = std::env::temp_dir().join(format!("am-projection-two-missing-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        let all = ["b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9", "b10"];
        project_text(&path, &pool, &config_text(&all)).await.unwrap();

        let err = project_text(&path, &pool, &config_text(&all[..8])).await.unwrap_err().to_string();
        assert!(err.contains("拒絕投影") && err.contains("2 顆 bot"), "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 10, "一次少兩顆也不可軟刪");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn refuses_to_soft_delete_a_supervisor_child_even_with_startup_bulk_override() {
        let dir = std::env::temp_dir().join(format!("am-projection-supervisor-child-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        project_text(&path, &pool, &config_text(&["supervisor", "build"])).await.unwrap();
        sqlx::query("UPDATE bots SET parent_bot_id = 'supervisor' WHERE id = 'build'").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO supervisors (id, bot_id, created_at, updated_at) VALUES ('sup', 'supervisor', 't', 't')")
            .execute(&pool)
            .await
            .unwrap();

        std::fs::write(&path, config_text(&["supervisor"])).unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();
        let err = project_config_at_startup(&store, &pool, true).await.unwrap_err().to_string();
        assert!(err.contains("supervisor child") && err.contains("build"), "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 2, "明確 bulk override 也不能刪 supervisor child");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 正常路徑不變：小改動一樣寫檔、一樣投影（`update_and_project` 不是把 Project／Bot mutation
    /// 整條路堵死，只是把 guard 提前到落盤前）。
    #[tokio::test]
    async fn update_and_project_writes_and_projects_a_legitimate_change() {
        let dir = std::env::temp_dir().join(format!("am-uap-ok-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        project_text(&path, &pool, &config_text(&["b1", "b2"])).await.unwrap();
        let store = ConfigStore::load(path.clone()).await.unwrap();

        update_and_project(&store, &pool, |cfg| {
            cfg.projects[0].label = "renamed".into();
            Ok(())
        })
        .await
        .unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("renamed"), "合法的改動要真的落盤");
        let projects = db::live_projects(&pool).await.unwrap();
        assert_eq!(projects[0].label, "renamed", "也要真的投影進 DB");

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
        assert!(!too_many(1, 20, MAX_REMOVED_BOTS));
        assert!(too_many(2, 20, MAX_REMOVED_BOTS));
        assert!(!too_many(1, 1, MAX_REMOVED_PROJECTS));
        assert!(!too_many(3, 20, MAX_REMOVED_PROJECTS));
        assert!(too_many(4, 20, MAX_REMOVED_PROJECTS));
        assert!(too_many(2, 4, MAX_REMOVED_PROJECTS));
        assert!(too_many(15, 15, MAX_REMOVED_PROJECTS));
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

    // ---- issue #406 ----

    async fn ops_alerts(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind = 'ops_alert' ORDER BY created_at")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// 13:28Z 那兩顆的形狀：`managed_by=user`、`parent_bot_id=NULL`，只是放在總管的專案裡。#398 的 parent 判斷認不出來。
    #[tokio::test]
    async fn a_bot_in_the_supervisor_project_is_never_soft_deleted_and_raises_an_ops_alert() {
        let (dir, store, pool) = seeded("agm-project-bot", &["agm", "build", "triage", "other"]).await;
        sqlx::query("INSERT INTO supervisors (id, bot_id, project_id, created_at, updated_at) VALUES ('AGM', 'agm', 'p1', 't', 't')")
            .execute(&pool)
            .await
            .unwrap();
        assert!(db::bot(&pool, "build").await.unwrap().unwrap().parent_bot_id.is_none(), "fixture：沒有 parent");

        std::fs::write(&store.path, config_text(&["agm", "triage", "other"])).unwrap();
        let err = project_config(&store, &pool).await.unwrap_err();
        let refused = err.downcast_ref::<ProjectionRefused>().expect("ProjectionRefused");
        assert_eq!(refused.supervisor_children, vec!["build".to_string()], "{err}");
        assert_eq!(db::live_bots(&pool).await.unwrap().len(), 4, "一顆都不能少");
        let alerts = ops_alerts(&pool).await;
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert!(alerts[0].contains("supervisor_bot_removal_refused") && alerts[0].contains("build"), "{alerts:?}");

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 「一次最多 1 顆」要跨投影累計：config 被改兩次、兩次投影各少 1 顆，第 2 顆要被擋。
    #[tokio::test]
    async fn two_single_removals_inside_the_window_are_counted_together() {
        let (dir, store, pool) = seeded("window", &["b1", "b2", "b3", "b4", "b5", "b6"]).await;

        std::fs::write(&store.path, config_text(&["b2", "b3", "b4", "b5", "b6"])).unwrap();
        project_config(&store, &pool).await.expect("第 1 顆照刪");
        std::fs::write(&store.path, config_text(&["b3", "b4", "b5", "b6"])).unwrap();
        let err = project_config(&store, &pool).await.unwrap_err();
        let refused = err.downcast_ref::<ProjectionRefused>().expect("ProjectionRefused");
        assert!(refused.bot_limit_exceeded && refused.bots == vec!["b2".to_string()], "{err}");
        assert!(db::bot(&pool, "b2").await.unwrap().unwrap().deleted_at.is_none(), "第 2 顆不能刪");
        assert!(ops_alerts(&pool).await.iter().any(|a| a.contains("projection_removal_refused")));

        // 窗口過了（把第 1 顆的 deleted_at 往前推）就又能刪 1 顆。
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = 'b1'").bind(db::iso_in(-(REMOVAL_WINDOW_SECS + 5))).execute(&pool).await.unwrap();
        project_config(&store, &pool).await.expect("窗口外");
        assert!(db::bot(&pool, "b2").await.unwrap().unwrap().deleted_at.is_some());

        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 外面改掉 config.toml：重讀時要記 WARN 與被拿掉的 id；daemon 自己寫的要記呼叫位置與前後 bot 數。
    #[tokio::test(flavor = "current_thread")]
    async fn external_rewrites_and_own_writes_are_both_logged() {
        let (dir, store, pool) = seeded("audit", &["b1", "b2", "b3"]).await;
        let (logs, _guard) = crate::config_audit::capture::start();

        std::fs::write(&store.path, config_text(&["b1", "b3"])).unwrap();
        store.update(|cfg| {
            cfg.projects[0].label = "renamed".into();
            Ok(())
        })
        .await
        .unwrap();

        let text = logs.text();
        let external = text.lines().find(|l| l.contains("config.toml changed outside this daemon")).unwrap_or_else(|| panic!("{text}"));
        assert!(external.contains("WARN") && external.contains("b2(b2)") && external.contains("bots_before=3") && external.contains("bots_after=2"), "{external}");
        let written = text.lines().find(|l| l.contains("config.toml written")).unwrap_or_else(|| panic!("{text}"));
        assert!(written.contains("projection.rs:") && written.contains("bots_after=2") && written.contains("size="), "{written}");
        drop(pool);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
