//! 沒有 hook 的 agy run（典型是 bot 用 `herdr agent start --kind agy` 開的子 agent）的對話紀錄（SPEC §12a.9）。
//!
//! hook 與 statusLine 只有 AG Man 自己啟動的 agy bot 才有（pane env 帶自己的 `AM_BOT_ID`／`AM_HOOK_TOKEN`）；子 agent 的 pane env 是父 bot 的，
//! 它的 hook 會被當成別種 provider 擋掉——所以跟 grok 子 agent 一樣改讀對話檔（`grok_transcript` 把這裡讀到的問答記成回合）：
//! * **session 對 pane**：agy 沒有 `active_sessions.json`，但它的行程一直開著 `~/.gemini/antigravity-cli/conversations/<id>.db`。
//!   pane 內的行程（`memproc::pids_in_pane`，環境 `HERDR_PANE_ID` 指回 pane）的 `/proc/<pid>/fd` 就是對話 id；同一顆 pane 開過好幾段
//!   （`/clear`）取還開著、紀錄檔最新的那一段。別的在跑的 run 已經綁走的不算。找到的記在 `runs.native_session_id`／`transcript_path`。
//! * 遠端主機（issue #1788）：`/proc` 換成那台的 `lsof -Fn -p <pid>`，紀錄檔的時間與尾巴用 `date -r`、`tail -c`，全部經 ssh 在那台跑（家目錄用 `HostConn::home`）。
//! * 讀的是 `…/brain/<id>/.system_generated/logs/transcript_full.jsonl` 的尾巴（`agy_support::parse_turns`）。

use super::*;
use std::path::{Path, PathBuf};

const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

pub fn valid_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 這些開著的對話裡，這個 run 該綁哪一段。`taken`＝別的在跑 run 已綁的；`mtime` 回紀錄檔的修改時間（沒有檔＝`None`＝不算）。
pub fn pick_conversation(
    open: &[String],
    known: Option<&str>,
    taken: &std::collections::HashSet<String>,
    mtime: impl Fn(&str) -> Option<std::time::SystemTime>,
) -> Option<String> {
    if let Some(k) = known.filter(|k| open.iter().any(|o| o == k)) {
        return Some(k.to_string());
    }
    open.iter().filter(|c| !taken.contains(*c)).filter_map(|c| mtime(c).map(|t| (t, c))).max().map(|(_, c)| c.clone())
}

/// 這台主機上的 agy 事實從哪裡讀（issue #1788）：本機直接讀 `/proc` 與檔案；遠端＝那台的 ssh（`lsof`、`date`、`tail`）。
enum Site {
    Local { home: PathBuf, recorded: Option<PathBuf> },
    Remote { conn: std::sync::Arc<crate::hosts::HostConn>, home: PathBuf },
}

impl Site {
    /// 拿不到家目錄、遠端沒連線＝`None`（照舊看畫面）。agy 非預設身份的 HOME 是身份目錄，紀錄檔在那底下（身份 env 的 `HOME`）。
    async fn of(app: &(impl crate::hosts::HostsAccess + crate::lifecycle::s6_ports::IdentityAccess), host: &str, identity: Option<&str>, recorded: Option<&str>) -> Option<Site> {
        let identity_home = match identity.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => app.identity_for_host(host, name).await.filter(|i| i.kind == "agy").and_then(|i| crate::tools::agy_identity_home(&i.env).map(str::to_owned)),
            None => None,
        };
        let effective = |base: PathBuf| match &identity_home {
            Some(h) => PathBuf::from(crate::config::expand_home(h, &base.to_string_lossy())),
            None => base,
        };
        if host == LOCAL_HOST {
            let home = effective(crate::home::dir()?);
            let recorded = recorded.map(PathBuf::from).filter(|p| p.is_absolute() && p.is_file());
            return Some(Site::Local { home, recorded });
        }
        let conn = app.hosts().get(host).await?;
        if !conn.is_connected() {
            return None;
        }
        match conn.home().await {
            Ok(home) => Some(Site::Remote { conn, home: effective(home.into()) }),
            Err(e) => {
                tracing::debug!(host, error = ?e, "agy transcript: remote home unknown");
                None
            }
        }
    }

    fn agy_dir(&self) -> PathBuf {
        let (Site::Local { home, .. } | Site::Remote { home, .. }) = self;
        home.join(".gemini").join("antigravity-cli")
    }

    fn path_of(&self, id: &str) -> PathBuf {
        if let Site::Local { recorded: Some(p), .. } = self {
            if p.components().any(|c| c.as_os_str() == id) && p.file_name().is_some_and(|n| n == "transcript_full.jsonl") {
                return p.clone();
            }
        }
        let home = match self {
            Site::Local { home, .. } | Site::Remote { home, .. } => home,
        };
        crate::agy_support::transcript_path(home, id)
    }

    /// pane 裡的行程開著的對話 id。
    async fn open_conversations(&self, pids: &[i32]) -> anyhow::Result<Vec<String>> {
        match self {
            Site::Local { .. } => {
                let pids = pids.to_vec();
                Ok(tokio::task::spawn_blocking(move || crate::agy_support::open_conversations(Path::new("/proc"), &pids)).await?)
            }
            Site::Remote { conn, .. } => {
                if pids.is_empty() {
                    return Ok(Vec::new());
                }
                let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
                let out = conn.ssh_exec_path(&format!("lsof -nP -Fn -p {list} 2>/dev/null; :")).await?;
                Ok(crate::agy_support::parse_lsof_conversations(&out))
            }
        }
    }

    /// 紀錄檔的修改時間（沒有紀錄檔的不在結果裡）。遠端一次 ssh 把全部候選問完。
    async fn mtimes(&self, ids: &[String]) -> std::collections::HashMap<String, std::time::SystemTime> {
        match self {
            Site::Local { .. } => ids
                .iter()
                .filter_map(|id| {
                    let t = std::fs::metadata(self.path_of(id)).and_then(|m| m.modified()).ok()?;
                    Some((id.clone(), t))
                })
                .collect(),
            Site::Remote { conn, .. } => {
                let script: String = ids
                    .iter()
                    .filter(|id| valid_session_id(id))
                    .map(|id| {
                        let f = crate::hosts::sh_quote(&self.path_of(id).to_string_lossy());
                        format!("if [ -f {f} ] && [ ! -L {f} ]; then printf '%s %s\\n' '{id}' \"$(date -r {f} +%s)\"; fi; ")
                    })
                    .collect();
                if script.is_empty() {
                    return Default::default();
                }
                match conn.ssh_exec_path(&format!("{script}:")).await {
                    Ok(out) => crate::agy_support::parse_mtimes(&out),
                    Err(e) => {
                        tracing::debug!(error = ?e, "agy transcript: remote transcript times unknown");
                        Default::default()
                    }
                }
            }
        }
    }

    /// 紀錄檔尾巴（最多 [`MAX_READ_BYTES`]）。讀不到＝`None`。
    async fn tail(&self, path: &Path) -> Option<String> {
        match self {
            Site::Local { .. } => crate::transcript_read::read_tail(path, MAX_READ_BYTES),
            Site::Remote { conn, .. } => {
                let f = crate::hosts::sh_quote(&path.to_string_lossy());
                let script = format!("if [ -f {f} ] && [ ! -L {f} ]; then tail -c {MAX_READ_BYTES} {f}; fi; :");
                match conn.ssh_exec_path(&script).await {
                    Ok(out) if !out.is_empty() => Some(out),
                    Ok(_) => None,
                    Err(e) => {
                        tracing::debug!(error = ?e, "agy transcript: remote transcript unreadable");
                        None
                    }
                }
            }
        }
    }
}

/// `Some((對話 id, 紀錄檔尾巴))`；找不到對話、讀不到檔＝`None`（照舊看畫面）。遠端的 `/proc`、對話檔都在那台，經 ssh 讀。
pub async fn load(app: &(impl crate::capabilities::Db + crate::hosts::HostsAccess + crate::lifecycle::s6_ports::IdentityAccess), run: &db::Run, host: &str) -> anyhow::Result<Option<(String, String)>> {
    let identity = match run.runtime_identity.clone() {
        Some(i) => Some(i),
        None => db::bot(app.db(), &run.bot_id).await?.and_then(|b| b.identity),
    };
    let Some(site) = Site::of(app, host, identity.as_deref(), run.transcript_path.as_deref()).await else { return Ok(None) };
    let known = run.native_session_id.clone().filter(|s| valid_session_id(s));
    let mut open: Vec<String> = Vec::new();
    if let Some(pane) = run.pane_id.as_deref() {
        match crate::memproc::dump(app, host).await {
            Ok(out) => {
                let pids = crate::memproc::pids_in_pane(&out, pane, run.herdr_session.as_deref());
                match site.open_conversations(&pids).await {
                    Ok(ids) => open = ids,
                    Err(e) => tracing::debug!(run = %run.id, error = ?e, "agy transcript: open conversations unknown; keeping the known conversation"),
                }
            }
            Err(e) => tracing::debug!(run = %run.id, error = ?e, "agy transcript: process dump failed; keeping the known conversation"),
        }
    }
    let taken: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT native_session_id FROM runs WHERE state = 'running' AND id <> ? AND native_session_id IS NOT NULL",
    )
    .bind(&run.id)
    .fetch_all(app.db())
    .await?
    .into_iter()
    .collect();
    let candidates: Vec<String> = open.iter().chain(known.iter()).cloned().collect();
    let mtimes = site.mtimes(&candidates).await;
    let mtime = |id: &str| mtimes.get(id).copied();
    let picked = pick_conversation(&open, known.as_deref(), &taken, mtime).or(known.clone().filter(|k| mtime(k).is_some()));
    let Some(sid) = picked else { return Ok(None) };
    let path = site.path_of(&sid);
    let path_s = path.to_string_lossy().into_owned();
    if known.as_deref() != Some(sid.as_str()) || run.transcript_path.as_deref() != Some(path_s.as_str()) {
        sqlx::query("UPDATE runs SET native_session_id = ?, transcript_path = ? WHERE id = ?").bind(&sid).bind(&path_s).bind(&run.id).execute(app.db()).await?;
        tracing::info!(run = %run.id, session = %sid, host, "agy transcript: bound the pane to its conversation");
    }
    let Some(text) = site.tail(&path).await else { return Ok(None) };
    Ok(Some((sid, text)))
}

/// 排程基準快照的「分岔」（agy 沒有原生 fork，1.5.16 實測）：把對話資料庫 `conversations/<id>.db` 複成新 id、
/// 改掉庫裡 `trajectory_meta.cascade_id`，連同 `brain/<id>/` 一起複製，再 `--conversation=<新 id>` 接續；來源檔不動。
/// 回新對話 id。本機與遠端都在 agy 那台主機上跑（身份 HOME 與 [`Site`] 同一套）。主機沒有 `sqlite3` 指令時錯誤以 [`SQLITE3_MISSING`] 開頭。
pub async fn clone_conversation(app: &(impl crate::hosts::HostsAccess + crate::lifecycle::s6_ports::IdentityAccess), host: &str, identity: Option<&str>, src: &str) -> anyhow::Result<String> {
    if !valid_session_id(src) {
        anyhow::bail!("bad conversation id `{src}`");
    }
    let site = Site::of(app, host, identity, None).await.ok_or_else(|| anyhow::anyhow!("agy home on `{host}` is unknown or the host is not connected"))?;
    let new = new_conversation_id();
    let script = clone_script(&site.agy_dir().to_string_lossy(), src, &new);
    let out = run_on(&site, &script).await.map_err(|e| sqlite3_missing(host, e))?;
    if out.contains(SQLITE3_MARK) {
        return Err(sqlite3_missing(host, anyhow::anyhow!("{out}")));
    }
    if !out.contains(CLONE_OK) {
        anyhow::bail!("conversation copy did not finish: {}", out.trim());
    }
    Ok(new)
}

async fn run_on(site: &Site, script: &str) -> anyhow::Result<String> {
    match site {
        Site::Local { .. } => {
            let o = tokio::process::Command::new("sh").arg("-c").arg(script).output().await?;
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            if !o.status.success() {
                anyhow::bail!("{}{}", stdout, String::from_utf8_lossy(&o.stderr).trim());
            }
            Ok(stdout)
        }
        Site::Remote { conn, .. } => conn.ssh_exec_path(script).await,
    }
}

fn sqlite3_missing(host: &str, e: anyhow::Error) -> anyhow::Error {
    if format!("{e:#}").contains(SQLITE3_MARK) {
        anyhow::anyhow!("{SQLITE3_MISSING}: `{host}` 上沒有 sqlite3 指令，無法複製 agy 對話")
    } else {
        e
    }
}

/// 錯誤碼：agy 那台主機沒有 `sqlite3` 指令。
pub const SQLITE3_MISSING: &str = "agy_sqlite3_missing";

pub fn is_sqlite3_missing(e: &anyhow::Error) -> bool {
    e.to_string().starts_with(SQLITE3_MISSING)
}

const CLONE_OK: &str = "AGY_CLONE_OK";
const SQLITE3_MARK: &str = "AGY_SQLITE3_MISSING";
const GONE: &str = "AGY_GONE";

pub fn new_conversation_id() -> String {
    let mut b = ulid::Ulid::new().0.to_be_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn sql_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn clone_script(agy_dir: &str, src: &str, new: &str) -> String {
    let d = crate::hosts::sh_quote(agy_dir);
    let target = format!("{}/conversations/{new}.db", agy_dir.trim_end_matches('/'));
    let vacuum = crate::hosts::sh_quote(&format!("VACUUM INTO {}", sql_text(&target)));
    let relabel = crate::hosts::sh_quote(&format!("UPDATE trajectory_meta SET cascade_id={}", sql_text(new)));
    format!(
        "set -e; D={d}; S={src}; N={new}; command -v sqlite3 >/dev/null 2>&1 || {{ echo {SQLITE3_MARK}; echo {SQLITE3_MARK} >&2; exit 4; }}; \
         [ -f \"$D/conversations/$S.db\" ] || {{ echo \"no conversation $S\" >&2; exit 3; }}; \
         trap 'rm -f \"$D/conversations/$N.db\"; rm -rf \"$D/brain/$N\"' EXIT; \
         sqlite3 \"$D/conversations/$S.db\" {vacuum}; \
         sqlite3 \"$D/conversations/$N.db\" {relabel}; \
         if [ -d \"$D/brain/$S\" ]; then cp -R \"$D/brain/$S\" \"$D/brain/$N\"; fi; trap - EXIT; echo {CLONE_OK}"
    )
}

/// 刪掉 agy 目錄底下這些對話（`conversations/<id>.db` 與其 WAL、`brain/<id>/`）；符號連結不跟、不刪。印出刪完的 id。
pub fn remove_script(agy_dir: &str, ids: &[String]) -> String {
    let d = crate::hosts::sh_quote(agy_dir);
    let list = ids.iter().filter(|id| valid_session_id(id)).cloned().collect::<Vec<_>>().join(" ");
    format!(
        "D={d}; for N in {list}; do C=\"$D/conversations/$N.db\"; B=\"$D/brain/$N\"; \
         if [ -L \"$C\" ] || [ -L \"$B\" ]; then continue; fi; \
         rm -f \"$C\" \"$C-wal\" \"$C-shm\" && {{ [ ! -d \"$B\" ] || rm -rf \"$B\"; }} && echo \"{GONE} $N\"; done; :"
    )
}

/// 記下 AG Man 複出來的對話（清除只刪這裡記的）。
pub async fn record_copy(pool: &sqlx::SqlitePool, id: &str, bot_id: &str, host: &str, identity: Option<&str>, purpose: &str, run_id: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO agy_conversation_copies (id, bot_id, host, identity, purpose, run_id, created_at) VALUES (?,?,?,?,?,?,?)")
        .bind(id)
        .bind(bot_id)
        .bind(host)
        .bind(identity)
        .bind(purpose)
        .bind(run_id)
        .bind(db::now())
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Copy {
    id: String,
    host: String,
    identity: Option<String>,
}

/// 用不到的複本：bot 已刪除；不是目前基準的 base 複本；不是該 bot 最新、也沒有在跑的 run 綁著的 run 複本。
async fn stale_copies(pool: &sqlx::SqlitePool, bot_id: Option<&str>) -> anyhow::Result<Vec<Copy>> {
    Ok(sqlx::query_as(
        "SELECT c.id, c.host, c.identity FROM agy_conversation_copies c LEFT JOIN bots b ON b.id = c.bot_id
          WHERE (?1 IS NULL OR c.bot_id = ?1)
            AND (b.id IS NULL OR b.deleted_at IS NOT NULL
              OR (c.purpose = 'base' AND NOT EXISTS (SELECT 1 FROM bot_base_snapshots s WHERE s.bot_id = c.bot_id AND s.session_id = c.id))
              OR (c.purpose = 'run'
                  AND c.id <> (SELECT n.id FROM agy_conversation_copies n WHERE n.bot_id = c.bot_id AND n.purpose = 'run' ORDER BY n.created_at DESC, n.rowid DESC LIMIT 1)
                  AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.state = 'running' AND r.ended_at IS NULL AND (r.native_session_id = c.id OR r.resume_session_id = c.id))))",
    )
    .bind(bot_id)
    .fetch_all(pool)
    .await?)
}

/// 刪掉用不到的複本（[`stale_copies`]），回刪掉幾份。主機連不上的先留著，下次再清。`bot_id = None`＝全部（啟動時）。
pub async fn sweep_copies(app: &(impl crate::capabilities::Db + crate::hosts::HostsAccess + crate::lifecycle::s6_ports::IdentityAccess), bot_id: Option<&str>) -> anyhow::Result<usize> {
    let mut groups: std::collections::BTreeMap<(String, Option<String>), Vec<String>> = Default::default();
    for c in stale_copies(app.db(), bot_id).await? {
        groups.entry((c.host, c.identity)).or_default().push(c.id);
    }
    let mut removed = 0;
    for ((host, identity), ids) in groups {
        let Some(site) = Site::of(app, &host, identity.as_deref(), None).await else { continue };
        let bad: Vec<&String> = ids.iter().filter(|id| !valid_session_id(id)).collect();
        for id in bad {
            sqlx::query("DELETE FROM agy_conversation_copies WHERE id = ?").bind(id).execute(app.db()).await?;
        }
        let out = match run_on(&site, &remove_script(&site.agy_dir().to_string_lossy(), &ids)).await {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!(host, error = ?e, "agy copies: removal failed; will retry");
                continue;
            }
        };
        for id in out.lines().filter_map(|l| l.strip_prefix(GONE).map(str::trim)) {
            if ids.iter().any(|i| i == id) {
                sqlx::query("DELETE FROM agy_conversation_copies WHERE id = ?").bind(id).execute(app.db()).await?;
                removed += 1;
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, bot = ?bot_id, "agy copies: removed conversation copies no longer needed");
    }
    Ok(removed)
}

/// 讀到紀錄之後順手補網頁的 `status_json`（模型與 context token 數）。已經收過 statusLine 時以 statusLine 為準，不覆蓋；沒變也不寫。
pub async fn record_status(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit + crate::capabilities::Cfg), bot: &db::Bot, run: &db::Run, text: &str) {
    if let Some(at) = crate::agy_support::last_assistant_at(text) {
        crate::cache_clock::on_agy_transcript(&run.id, &at);
    }
    if crate::agy_support::has_statusline(run.status_json.as_deref()) {
        return;
    }
    let model = run.runtime_model.as_deref().or(bot.model.as_deref());
    let window = app.cfg().get().await.agy.context_window_tokens;
    let Some(json) = crate::agy_support::status_json(model, crate::agy_support::last_input_tokens(text), window) else { return };
    if run.status_json.as_deref() == Some(json.as_str()) {
        return;
    }
    if sqlx::query("UPDATE runs SET status_json = ? WHERE id = ?").bind(&json).bind(&run.id).execute(app.db()).await.is_ok() {
        crate::capabilities::BotStatusEmit::emit_bot_status(app, &bot.id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::{Duration, SystemTime};
    #[test]
    fn a_clone_gets_its_own_id_and_leaves_the_source_alone() {
        let dir = crate::home::dir().unwrap().join(format!(".agy-clone-{}", new_conversation_id()));
        let (conv, brain) = (dir.join("conversations"), dir.join("brain"));
        let src = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        std::fs::create_dir_all(brain.join(src).join(".system_generated")).unwrap();
        std::fs::create_dir_all(&conv).unwrap();
        std::fs::write(brain.join(src).join(".system_generated/t.jsonl"), "x").unwrap();
        let db = conv.join(format!("{src}.db"));
        let q = |f: &std::path::Path, sql: &str| {
            let o = std::process::Command::new("sqlite3").arg(f).arg(sql).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        q(&db, &format!("create table trajectory_meta (trajectory_id text, cascade_id text); insert into trajectory_meta values ('t','{src}')"));
        let before = std::fs::read(&db).unwrap();
        let new = new_conversation_id();
        assert!(valid_session_id(&new) && new.len() == 36);
        let o = std::process::Command::new("sh").arg("-c").arg(clone_script(&dir.to_string_lossy(), src, &new)).output().unwrap();
        assert!(String::from_utf8_lossy(&o.stdout).contains(CLONE_OK), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(q(&conv.join(format!("{new}.db")), "select cascade_id from trajectory_meta"), new);
        assert_eq!(q(&db, "select cascade_id from trajectory_meta"), src);
        assert_eq!(std::fs::read(&db).unwrap(), before);
        assert!(brain.join(&new).join(".system_generated/t.jsonl").is_file());
        let missing = std::process::Command::new("sh").arg("-c").arg(clone_script(&dir.to_string_lossy(), "nope", &new_conversation_id())).output().unwrap();
        assert!(!missing.status.success());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn a_home_with_a_single_quote_still_clones_and_a_missing_sqlite3_says_so() {
        let dir = crate::home::dir().unwrap().join(format!(".agy-o'clone-{}", new_conversation_id()));
        let conv = dir.join("conversations");
        std::fs::create_dir_all(&conv).unwrap();
        let src = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let o = std::process::Command::new("sqlite3").arg(conv.join(format!("{src}.db"))).arg(format!("create table trajectory_meta (cascade_id text); insert into trajectory_meta values ('{src}')")).output().unwrap();
        assert!(o.status.success());
        let new = new_conversation_id();
        let o = std::process::Command::new("sh").arg("-c").arg(clone_script(&dir.to_string_lossy(), src, &new)).output().unwrap();
        assert!(String::from_utf8_lossy(&o.stdout).contains(CLONE_OK), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(conv.join(format!("{new}.db")).is_file());
        let empty = dir.join("nobin");
        std::fs::create_dir_all(&empty).unwrap();
        let other = new_conversation_id();
        let o = std::process::Command::new("/bin/sh").env("PATH", &empty).arg("-c").arg(clone_script(&dir.to_string_lossy(), src, &other)).output().unwrap();
        assert!(!o.status.success());
        assert!(String::from_utf8_lossy(&o.stdout).contains(SQLITE3_MARK));
        assert!(is_sqlite3_missing(&sqlite3_missing("m4p", anyhow::anyhow!("{}", String::from_utf8_lossy(&o.stderr)))));
        assert!(!conv.join(format!("{other}.db")).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removal_only_touches_the_named_copies_and_never_follows_links() {
        let dir = crate::home::dir().unwrap().join(format!(".agy-rm-{}", new_conversation_id()));
        let (conv, brain) = (dir.join("conversations"), dir.join("brain"));
        let (gone, kept, linked) = (new_conversation_id(), new_conversation_id(), new_conversation_id());
        for id in [&gone, &kept] {
            std::fs::create_dir_all(brain.join(id).join("x")).unwrap();
            std::fs::create_dir_all(&conv).unwrap();
            std::fs::write(conv.join(format!("{id}.db")), "db").unwrap();
        }
        std::fs::write(conv.join(format!("{gone}.db-wal")), "wal").unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, brain.join(&linked)).unwrap();
        let ids = vec![gone.clone(), linked.clone(), "../outside".to_string()];
        let o = std::process::Command::new("sh").arg("-c").arg(remove_script(&dir.to_string_lossy(), &ids)).output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout).to_string();
        assert_eq!(out.lines().collect::<Vec<_>>(), vec![format!("{GONE} {gone}")]);
        assert!(!conv.join(format!("{gone}.db")).exists() && !conv.join(format!("{gone}.db-wal")).exists() && !brain.join(&gone).exists());
        assert!(conv.join(format!("{kept}.db")).exists() && brain.join(&kept).exists());
        assert!(outside.is_dir() && brain.join(&linked).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_known_conversation_wins_while_it_is_open_otherwise_the_newest_free_one() {
        let t = |s: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(s));
        let open: Vec<String> = ["a", "b", "c"].map(String::from).into();
        let mtime = |id: &str| match id {
            "a" => t(10),
            "b" => t(30),
            "c" => None,
            _ => None,
        };
        let none = HashSet::new();
        assert_eq!(pick_conversation(&open, Some("a"), &none, mtime).as_deref(), Some("a"), "記過而且還開著：不換");
        assert_eq!(pick_conversation(&open, Some("gone"), &none, mtime).as_deref(), Some("b"), "記的那段已經關了（/clear）：取最新的");
        let taken: HashSet<String> = ["b".to_string()].into();
        assert_eq!(pick_conversation(&open, None, &taken, mtime).as_deref(), Some("a"), "別的 run 綁走的不給");
        assert_eq!(pick_conversation(&["c".to_string()], None, &none, mtime), None, "沒有紀錄檔的不算");
        assert_eq!(pick_conversation(&[], None, &none, mtime), None);
    }
}
