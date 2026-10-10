//! Pre-trusting a workspace directory so an unattended claude/codex pane does not stall on the
//! first-run "do you trust this folder?" dialog (claude's cursor starts on `No, exit`, so it quits
//! → `agent_not_running`). `--dangerously-skip-permissions` does not suppress it (verified).
//!
//! We write the record the dialog would: claude `hasTrustDialogAccepted` in `.claude.json`, codex
//! `trust_level = "trusted"` in `config.toml`, grok `[folders."<path>"] trusted = true` in
//! `trusted_folders.toml` (grok had no gate until 1.0.34 added "Do you trust the contents of this
//! directory?", 2026-09-17). Gotchas: the file must be the one the *bot's identity* reads
//! (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`), and the path must be physical (`/tmp` → `/private/tmp`),
//! see [`canonical`]. These files belong to the CLIs: amend in place via temp file + `rename`,
//! and leave untouched when already trusted.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::expand_home;
use crate::db;
use crate::hosts::HostsAccess;

pub const CLAUDE_KEY: &str = "hasTrustDialogAccepted";

/// 預先信任工作目錄需要從 `App` 拿的外部事實（`App` 在 `app_ports_p3` 實作）：主機表，以及某台主機上某個身分的環境變數
/// （`[[identities]]` 加上那台 shell 偵測到的 `ccN`，SPEC §16）。
pub trait TrustEnv: HostsAccess + 'static {
    /// `host` 上名叫 `name` 的身分的 env（沒有這個身分＝`None`）。
    fn identity_env(app: &Arc<Self>, host: &str, name: &str) -> impl std::future::Future<Output = Option<BTreeMap<String, String>>> + Send;
}

/// Second first-run dialog (2026-09-08): external CLAUDE.md `@imports`, cursor on *No*. Same fix.
pub const CLAUDE_EXTERNAL_KEYS: [&str; 2] = ["hasClaudeMdExternalIncludesApproved", "hasClaudeMdExternalIncludesWarningShown"];
const CODEX_KEY: &str = "trust_level";
const CODEX_TRUSTED: &str = "trusted";

/// Falls back to the input when the directory does not exist (a bad key is inert).
pub fn canonical(path: &str) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

/// `env` = identity ∪ bot env, `$HOME`-expanded. `None` for kinds without a gate.
pub fn store_path(kind: &str, env: &BTreeMap<String, String>, home: &str) -> Option<PathBuf> {
    let var = |k: &str| {
        env.get(k).map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| expand_home(s, home))
    };
    match kind {
        // Unset → `~/.claude.json`, *not* `~/.claude/.claude.json` (unlike settings.json).
        "claude" => Some(match var("CLAUDE_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir).join(".claude.json"),
            None => PathBuf::from(home).join(".claude.json"),
        }),
        "codex" => {
            let dir = var("CODEX_HOME").unwrap_or_else(|| format!("{home}/.codex"));
            Some(PathBuf::from(dir).join("config.toml"))
        }
        "grok" => {
            let dir = var("GROK_HOME").unwrap_or_else(|| format!("{home}/.grok"));
            Some(PathBuf::from(dir).join("trusted_folders.toml"))
        }
        // agy 的設定目錄只認 `$HOME`（沒有任何環境變數能改，設計 A.3），所以不看 identity env。
        "agy" => Some(crate::agy_support::settings_path(Path::new(home))),
        _ => None,
    }
}

/// `None` when nothing changed. Unparseable or wrong-shaped files are an error, never overwritten.
pub fn claude_merge(existing: &str, paths: &[String]) -> Result<Option<String>> {
    let mut root: Value = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(existing).context("`.claude.json` is not valid JSON")?
    };
    let Some(obj) = root.as_object_mut() else { bail!("`.claude.json` is not a JSON object") };

    let projects = obj.entry("projects").or_insert_with(|| json!({}));
    let Some(projects) = projects.as_object_mut() else { bail!("`projects` in `.claude.json` is not an object") };

    let mut changed = false;
    for p in paths {
        let entry = projects.entry(p.as_str()).or_insert_with(|| json!({}));
        let Some(entry) = entry.as_object_mut() else {
            bail!("`projects[{p}]` in `.claude.json` is not an object");
        };
        for key in std::iter::once(CLAUDE_KEY).chain(CLAUDE_EXTERNAL_KEYS) {
            if entry.get(key) != Some(&Value::Bool(true)) {
                entry.insert(key.into(), json!(true));
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(None);
    }
    // Match Claude Code's own formatting so the file does not flip between writers.
    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&root)?)))
}

/// `toml_edit` keeps the rest of the file byte-identical. `None` when already trusted.
pub fn codex_merge(existing: &str, paths: &[String]) -> Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = if existing.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        existing.parse().context("codex `config.toml` is not valid TOML")?
    };

    let projects = doc.entry("projects").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(projects) = projects.as_table_mut() else { bail!("`projects` in codex `config.toml` is not a table") };
    // Renders the children as `[projects."/path"]` instead of emitting a bare `[projects]`.
    projects.set_implicit(true);

    let mut changed = false;
    for p in paths {
        let entry = projects.entry(p).or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let Some(entry) = entry.as_table_mut() else {
            bail!("`projects.{p}` in codex `config.toml` is not a table");
        };
        if entry.get(CODEX_KEY).and_then(|v| v.as_str()) != Some(CODEX_TRUSTED) {
            entry[CODEX_KEY] = toml_edit::value(CODEX_TRUSTED);
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// grok 1.0.34 `trusted_folders.toml`：`[folders."/path"] trusted = true, decided_at = <epoch 秒>`，
/// 跟 grok 自己按 `y` 寫的一樣。`None` when already trusted.
pub fn grok_merge(existing: &str, paths: &[String], now_secs: i64) -> Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = if existing.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        existing.parse().context("grok `trusted_folders.toml` is not valid TOML")?
    };
    let folders = doc.entry("folders").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(folders) = folders.as_table_mut() else { bail!("`folders` in grok `trusted_folders.toml` is not a table") };
    folders.set_implicit(true);

    let mut changed = false;
    for p in paths {
        let entry = folders.entry(p).or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let Some(entry) = entry.as_table_mut() else {
            bail!("`folders.{p}` in grok `trusted_folders.toml` is not a table");
        };
        if entry.get("trusted").and_then(|v| v.as_bool()) != Some(true) {
            entry["trusted"] = toml_edit::value(true);
            entry["decided_at"] = toml_edit::value(now_secs);
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// 若路徑是 symlink（例如使用者用 dotfiles 管理設定），解開成目標檔的實際路徑，
/// 避免 `rename` 把 symlink 換成普通檔而破壞 dotfiles 連結。不存在時回原路徑。
fn resolve_symlink(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            if let Ok(dest) = std::fs::read_link(path) {
                if dest.is_absolute() {
                    dest
                } else if let Some(parent) = path.parent() {
                    parent.join(dest)
                } else {
                    dest
                }
            } else {
                path.to_path_buf()
            }
        }
    }
}

/// Temp file + fsync + `rename`: a crash must not corrupt the user's own agent-CLI state files.
/// 暫存檔一開始就是 0600，結果沿用原檔的權限（`.claude.json` 是 0600，不能被我們的 umask 放寬）；細節見 [`crate::atomic_file`]。
/// 寫入時跟隨 symlink（[`resolve_symlink`]），暫存檔建在目標目錄、`rename` 作用在目標檔，保留 symlink（issue #1073）。
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let target = resolve_symlink(path);
    let dir = target.parent().ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(dir)?;
    crate::atomic_file::write(&target, text.as_bytes(), crate::atomic_file::Mode::Preserve).with_context(|| format!("writing {}", target.display()))
}

/// `None` when already trusted (or the kind has no gate).
fn merged(kind: &str, existing: &str, paths: &[String]) -> Result<Option<String>> {
    match kind {
        "claude" => claude_merge(existing, paths),
        "codex" => codex_merge(existing, paths),
        "grok" => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            grok_merge(existing, paths, now)
        }
        "agy" => crate::agy_support::trusted_workspaces_merge(existing, paths),
        _ => Ok(None),
    }
}

/// 本機信任檔的讀→合併→寫要一次一個：批次重啟時好幾顆 bot 並行預先信任同一個 `~/.claude.json`（不同 worktree），
/// 沒有互斥就是後寫的蓋掉先寫的，那顆 bot 照樣跳出信任提示。
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 讀到寫之間 CLI 自己改了檔就重讀再合併（跟遠端的 `cksum` 比對同一個意思），不拿舊內容蓋掉。
const LOCAL_RACE_ATTEMPTS: usize = 3;

fn read_store(store: &Path) -> Result<String> {
    match std::fs::read_to_string(store) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", store.display())),
    }
}

/// No-op (file not rewritten) when already trusted.
pub fn mark_trusted(kind: &str, store: &Path, paths: &[String]) -> Result<bool> {
    let changed = update_file(store, |existing| merged(kind, existing, paths))?;
    if changed {
        tracing::info!(kind, store = %store.display(), ?paths, "pre-trusted agent workspace directories");
    }
    Ok(changed)
}

/// 讀 → `merge`（回 `None`＝不用改）→ 原子寫回，同一個鎖、同一套「讀到寫之間檔案被改就重讀」。agy 的 `settings.json`
/// 同時放信任清單與 `statusLine`，兩邊都從這裡寫，才不會互相蓋掉。回「有沒有寫」。
pub fn update_file(store: &Path, merge: impl Fn(&str) -> Result<Option<String>>) -> Result<bool> {
    let _one_at_a_time = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let target = resolve_symlink(store);
    for _ in 0..LOCAL_RACE_ATTEMPTS {
        let existing = read_store(&target)?;
        let Some(next) = merge(&existing)? else { return Ok(false) };
        if read_store(&target)? != existing {
            continue;
        }
        write_atomic(&target, &next)?;
        return Ok(true);
    }
    bail!("{} kept changing while updating it", store.display())
}

/// Identity env then bot env (`lifecycle::pane_env` minus daemon vars, which name no config dir).
async fn config_env<T: TrustEnv>(app: &Arc<T>, bot: &db::Bot, host: &str, home: &str) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    // `identity_for_host`, not `cfg.identities`: shell-discovered `ccN` (SPEC §16) aren't in
    // config.toml, which once sent cc2's record to the wrong file (2026-09-08).
    if let Some(name) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if let Some(id_env) = T::identity_env(app, host, name).await {
            for (k, v) in &id_env {
                env.insert(k.clone(), expand_home(v, home));
            }
        }
    }
    for (k, v) in bot.env() {
        env.insert(k, expand_home(&v, home));
    }
    env
}

/// Best effort: returns one error per store rather than refusing the start.
/// Local host; remote bots go through [`pretrust_bots_remote`].
pub async fn pretrust_bots<T: TrustEnv>(app: &Arc<T>, bots: &[db::Bot]) -> Vec<String> {
    let Some(home) = crate::home::dir() else {
        return vec!["no home directory; cannot pre-trust the working directory".into()];
    };
    let home = home.to_string_lossy().into_owned();

    // One write per file, not per bot: the CLIs write these files too, so minimise the race.
    let mut jobs: BTreeMap<PathBuf, (String, BTreeSet<String>)> = BTreeMap::new();
    for b in bots {
        let Some(cwd) = b.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { continue };
        let env = config_env(app, b, crate::config::LOCAL_HOST, &home).await;
        let Some(store) = store_path(&b.kind, &env, &home) else { continue };
        jobs.entry(store).or_insert_with(|| (b.kind.clone(), BTreeSet::new())).1.insert(canonical(cwd));
    }

    let mut errors = Vec::new();
    for (store, (kind, paths)) in jobs {
        let paths: Vec<String> = paths.into_iter().collect();
        if let Err(e) = mark_trusted(&kind, &store, &paths) {
            errors.push(format!("{}: {e:#}", store.display()));
        }
    }
    errors
}

/// `start_inner` 在開 pane 之前呼叫：本機寫檔，遠端經 ssh（#407）。best effort，回傳每個失敗的說明。
pub async fn pretrust_for_start<T: TrustEnv>(app: &Arc<T>, bot: &db::Bot, host: &str, cwd: &str) -> Vec<String> {
    let mut b = bot.clone();
    b.cwd = Some(cwd.to_string());
    if host == crate::config::LOCAL_HOST {
        pretrust_bots(app, std::slice::from_ref(&b)).await
    } else {
        pretrust_bots_remote(app, host, std::slice::from_ref(&b)).await
    }
}

/// 整段遠端預先信任的**總**預算。這段在 `start_inner` 裡 await，擋在開 pane 前面：主機沒死透（TCP 收下但不回話）
/// 時每一趟 ssh 都會慢慢等到 `SSH_EXEC_TIMEOUT`（30 秒），累起來每次遠端啟動要多等好幾分鐘。
/// 所以整段包一個上限，超時只記 warn、照樣啟動——最差就是回到原本那個 trust 提示（#407 review）。
const REMOTE_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// 讀到寫之間被 CLI 改掉時重讀幾次。只給真的 race 用（`AM_TRUST_CHANGED`）：ssh 失敗是直接回錯，不在這裡重試。
const REMOTE_RACE_ATTEMPTS: usize = 2;

/// 同 [`pretrust_bots`]，檔案在遠端：經 ssh 讀、在這裡合併（規則只有一份）、再經 ssh 寫回（#407）。
pub async fn pretrust_bots_remote<T: TrustEnv>(app: &Arc<T>, host: &str, bots: &[db::Bot]) -> Vec<String> {
    pretrust_bots_remote_within(app, host, bots, REMOTE_BUDGET).await
}

/// `budget` 拆出來是為了測得到上限：假 ssh 掛住時，呼叫端必須在上限內拿回警告。
/// 逾時會把整個 future 丟掉，`ssh_exec` 的子行程是 `kill_on_drop`，所以不會留下跑著的 ssh。
pub async fn pretrust_bots_remote_within<T: TrustEnv>(app: &Arc<T>, host: &str, bots: &[db::Bot], budget: std::time::Duration) -> Vec<String> {
    match tokio::time::timeout(budget, remote_jobs(app, host, bots)).await {
        Ok(errors) => errors,
        Err(_) => vec![format!("{host}: pre-trusting the working directory took longer than {}s; left to the trust dialog", budget.as_secs())],
    }
}

async fn remote_jobs<T: TrustEnv>(app: &Arc<T>, host: &str, bots: &[db::Bot]) -> Vec<String> {
    let Some(conn) = app.hosts().get(host).await else { return vec![format!("unknown host `{host}`")] };
    let home = match conn.home().await {
        Ok(h) => h,
        Err(e) => return vec![format!("{host}: cannot resolve the remote home: {e:#}")],
    };
    let mut jobs: BTreeMap<PathBuf, (String, BTreeSet<String>)> = BTreeMap::new();
    for b in bots {
        let Some(cwd) = b.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { continue };
        let env = config_env(app, b, host, &home).await;
        let Some(store) = store_path(&b.kind, &env, &home) else { continue };
        jobs.entry(store).or_insert_with(|| (b.kind.clone(), BTreeSet::new())).1.insert(cwd.to_string());
    }
    let mut errors = Vec::new();
    for (store, (kind, paths)) in jobs {
        let paths: Vec<String> = paths.into_iter().collect();
        let store = store.to_string_lossy().into_owned();
        if let Err(e) = mark_trusted_remote(&conn, &kind, &store, &paths).await {
            errors.push(format!("{host}:{store}: {e:#}"));
        }
    }
    errors
}

/// 遠端設定檔若為 symlink，解開成目標路徑，避免 `mv -f "$T" "$F"` 覆蓋 symlink 本身（issue #1073）。
/// 包含舊版 macOS 沒有 `readlink -f` 時的 `cd -P` fallback。
const REMOTE_RESOLVE_SYMLINK_SH: &str = "\
while [ -L \"$F\" ]; do\n\
  _R=$(readlink -f -- \"$F\" 2>/dev/null || realpath -- \"$F\" 2>/dev/null || true)\n\
  if [ -n \"$_R\" ]; then F=\"$_R\"; break; fi\n\
  _T=$(readlink -- \"$F\" 2>/dev/null || true)\n\
  _P=\"\"\n\
  case \"$_T\" in\n\
    /*) _P=\"$_T\" ;;\n\
    ?*) _D=$(cd -P -- \"$(dirname -- \"$F\")\" 2>/dev/null && cd -P -- \"$(dirname -- \"$_T\")\" 2>/dev/null && pwd -P)\n\
        [ -n \"$_D\" ] && _P=\"$_D/$(basename -- \"$_T\")\" ;;\n\
  esac\n\
  [ -n \"$_P\" ] && [ \"$_P\" != \"$F\" ] || break\n\
  F=\"$_P\"\n\
done\n";

/// 讀的那一趟同時把每個工作目錄的**遠端**真實路徑（`pwd -P`）帶回來：CLI 寫的鍵是它自己 `cwd` 看到的路徑，
/// worktree、symlink、`/tmp`→`/private/tmp` 都會讓字面路徑變成一個沒用的鍵，claude 照樣問（#407 review）。
/// 本機那半用 [`canonical`]；這裡不能用，那是 daemon 這台的檔案系統。目錄還不存在就照字面留著（跟 [`canonical`] 一樣）。
fn remote_read_script(store_q: &str, paths: &[String]) -> String {
    use crate::hosts::sh_quote;
    let mut s = String::new();
    for path in paths {
        let q = sh_quote(path);
        s.push_str(&format!("P={q}\nC=$(cd -- \"$P\" 2>/dev/null && pwd -P)\n[ -n \"$C\" ] || C=$P\nprintf 'AM_P=%s\\n' \"$C\"\n"));
    }
    s.push_str(&format!(
        "F={store_q}\n{REMOTE_RESOLVE_SYMLINK_SH}if [ -f \"$F\" ]; then printf 'AM_SUM=%s\\n' \"$(cksum < \"$F\")\"; cat \"$F\"; else printf 'AM_SUM=missing\\n'; fi\n"
    ));
    s
}

/// `(遠端真實路徑, cksum, 檔案內容)`。缺了哪一段都當成錯：把「讀不完整」當成「檔案是空的」會整個蓋掉使用者的檔。
pub fn parse_remote_read(out: &str, want: usize) -> Result<(Vec<String>, String, String)> {
    let mut paths = Vec::new();
    let mut rest = out;
    loop {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        if let Some(p) = line.strip_prefix("AM_P=") {
            paths.push(p.to_string());
            rest = tail;
            continue;
        }
        let Some(sum) = line.strip_prefix("AM_SUM=") else {
            bail!("unexpected reply while reading the trust store: {}", out.trim());
        };
        if paths.len() != want {
            bail!("expected {want} remote paths, got {}: {}", paths.len(), out.trim());
        }
        return Ok((paths, sum.to_string(), tail.to_string()));
    }
}

/// 讀 → 合併 → 寫回，寫之前比對 `cksum`：CLI 自己在中間改過檔（claude 常寫 `.claude.json`）就放棄這次、重讀再合併，
/// 不拿舊內容蓋掉。暫存檔＋`mv`，權限照原檔（新檔 0600，跟 `.claude.json` 一樣）；`mv` 之前死掉由 `trap` 收掉暫存檔。
/// 已經信任的在第一趟（只讀）就回 `Ok(false)`，不會有第二趟 ssh。
async fn mark_trusted_remote(conn: &crate::hosts::HostConn, kind: &str, store: &str, paths: &[String]) -> Result<bool> {
    use crate::hosts::sh_quote;
    let f = sh_quote(store);
    for _ in 0..REMOTE_RACE_ATTEMPTS {
        let out = conn.ssh_exec(&remote_read_script(&f, paths)).await?;
        let (real_paths, sum, existing) = parse_remote_read(&out, paths.len()).with_context(|| format!("reading {store}"))?;
        // `pwd -P` 之後才去重：兩顆 bot 可能用不同的 symlink 指到同一個目錄。
        let real_paths: Vec<String> = real_paths.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
        let Some(next) = merged(kind, &existing, &real_paths)? else { return Ok(false) };
        let body = next.strip_suffix('\n').unwrap_or(&next);
        let mut delim = String::from("AM_TRUST_EOF");
        while body.contains(&delim) {
            delim.push('_');
        }
        let write = format!(
            "set -e\nF={f}\n{REMOTE_RESOLVE_SYMLINK_SH}\
             D=$(dirname \"$F\")\nmkdir -p \"$D\"\ncur=missing\nif [ -f \"$F\" ]; then cur=$(cksum < \"$F\"); fi\n\
             if [ \"$cur\" != {sum} ]; then printf 'AM_TRUST_CHANGED\\n'; exit 0; fi\n\
             T=\"$D/.$(basename \"$F\").am-trust.$$.tmp\"\ntrap 'rm -f \"$T\"' EXIT\numask 077\ncat > \"$T\" <<'{delim}'\n{body}\n{delim}\n\
             if [ -f \"$F\" ]; then chmod \"$(stat -c %a \"$F\" 2>/dev/null || stat -f %Lp \"$F\")\" \"$T\" 2>/dev/null || true; fi\n\
             mv -f \"$T\" \"$F\"\nprintf 'AM_TRUST_OK\\n'\n",
            sum = sh_quote(&sum),
        );
        let out = conn.ssh_exec(&write).await?;
        if out.contains("AM_TRUST_OK") {
            tracing::info!(host = %conn.name, kind, store, paths = ?real_paths, "pre-trusted agent workspace directories on the remote");
            return Ok(true);
        }
        if !out.contains("AM_TRUST_CHANGED") {
            bail!("writing {store} did not confirm: {}", out.trim());
        }
    }
    bail!("{store} kept changing while pre-trusting it")
}

/// 遠端任一個 JSON／設定檔的「讀 → 合併 → 寫回」，規則同 [`mark_trusted_remote`]（cksum 圍欄、暫存檔＋`mv`、權限照原檔、新檔 0600、
/// 讀到寫之間被改過就重讀）：`merge` 收現有內容（沒有檔＝空字串）回新內容，`None`＝已經是對的、不寫。回「有沒有寫」。
/// 讀不懂（`merge` 回錯）就整個放棄、不碰檔案。給 agy 的遠端 `hooks.json`／`settings.json`（使用者與 agy 自己也在寫）。
pub(crate) async fn update_remote_file(
    conn: &crate::hosts::HostConn,
    store: &str,
    merge: impl Fn(&str) -> Result<Option<String>>,
) -> Result<bool> {
    use crate::hosts::sh_quote;
    let f = sh_quote(store);
    for _ in 0..REMOTE_RACE_ATTEMPTS {
        let out = conn.ssh_exec(&remote_read_script(&f, &[])).await?;
        let (_, sum, existing) = parse_remote_read(&out, 0).with_context(|| format!("reading {store}"))?;
        let Some(next) = merge(&existing).with_context(|| format!("merging into {store}"))? else { return Ok(false) };
        let body = next.strip_suffix('\n').unwrap_or(&next);
        let mut delim = String::from("AM_UPDATE_EOF");
        while body.contains(&delim) {
            delim.push('_');
        }
        let write = format!(
            "set -e\nF={f}\n{REMOTE_RESOLVE_SYMLINK_SH}\
             D=$(dirname \"$F\")\nmkdir -p \"$D\"\ncur=missing\nif [ -f \"$F\" ]; then cur=$(cksum < \"$F\"); fi\n\
             if [ \"$cur\" != {sum} ]; then printf 'AM_UPDATE_CHANGED\\n'; exit 0; fi\n\
             T=\"$D/.$(basename \"$F\").am-update.$$.tmp\"\ntrap 'rm -f \"$T\"' EXIT\numask 077\ncat > \"$T\" <<'{delim}'\n{body}\n{delim}\n\
             if [ -f \"$F\" ]; then chmod \"$(stat -c %a \"$F\" 2>/dev/null || stat -f %Lp \"$F\")\" \"$T\" 2>/dev/null || true; fi\n\
             mv -f \"$T\" \"$F\"\nprintf 'AM_UPDATE_OK\\n'\n",
            sum = sh_quote(&sum),
        );
        let out = conn.ssh_exec(&write).await?;
        if out.contains("AM_UPDATE_OK") {
            return Ok(true);
        }
        if !out.contains("AM_UPDATE_CHANGED") {
            bail!("writing {store} did not confirm: {}", out.trim());
        }
    }
    bail!("{store} kept changing while updating it")
}
