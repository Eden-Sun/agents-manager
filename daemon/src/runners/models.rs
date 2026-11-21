use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use crate::config::LOCAL_HOST;
use crate::hosts::sh_quote;
use crate::models::{
    agy_models_from_screen, agy_models_from_text, agy_models_script, agy_static_models, claude_config_dir, claude_static_models, codex_apply_identity_config,
    codex_models_from_rpc, enrich_grok_models, find_response, grok_models_from_text,
    home_env_prefix, home_file_expr, identity_home_dir, read_claude_effort_settings,
    read_optional_text, rpc_lines, CACHE_TTL,
};
use crate::state::App;

const LOCAL_RPC_TIMEOUT: Duration = Duration::from_secs(20);
const REMOTE_RPC_HOLD_SECS: u32 = 6;
/// `agy models` 要起一個約 200 MB 的執行檔，比 `--version` 慢；逾時就退回寫死的清單。
const AGY_MODELS_TIMEOUT: Duration = Duration::from_secs(30);
/// 遠端 pane 版的時限：開 workspace、等 shell 起來、打指令各要一兩秒，比直接跑寬一點。
const AGY_MODELS_PANE_TIMEOUT: Duration = Duration::from_secs(45);

/// 本機找 `exe` 的絕對路徑：含 `/` 直接用，否則走 login shell 探測。探測走 [`crate::hosts::sh_local_stdout`]，
/// 逾時整個行程群組收掉（卡住的 `-lic` 互動 shell 不會留下來，issue #889）。
async fn resolve_local_exe(exe: &str, probe_timeout: Duration) -> Result<String> {
    resolve_local_exe_with(exe, probe_timeout, "").await
}

/// 同 [`resolve_local_exe`]；`script_prefix` 接在探測腳本前面（測試用來只替子行程設 `SHELL` 等環境，不碰全域環境）。
async fn resolve_local_exe_with(exe: &str, probe_timeout: Duration, script_prefix: &str) -> Result<String> {
    if exe.contains('/') {
        return Ok(exe.to_string());
    }
    let probe = format!("{script_prefix}{}; printf '%s\\n' \"$p\"", crate::tools::login_abs_sh(exe));
    let out = crate::hosts::sh_local_stdout(&probe, probe_timeout, "codex path probe").await?;
    let p = out.trim().to_string();
    if p.is_empty() {
        bail!("`{exe}` is not installed on this machine");
    }
    Ok(p)
}

async fn codex_rpc_local(exe: &str, lines: &[String], id: u64, home: Option<&str>) -> Result<Value> {
    let exe = resolve_local_exe(exe, Duration::from_secs(10)).await?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(dir) = home {
        cmd.env("CODEX_HOME", dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn {exe} app-server"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let result: Result<Value> = async {
        let mut stdin = stdin.ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = stdout.ok_or_else(|| anyhow!("no stdout"))?;
        for l in lines {
            stdin.write_all(l.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
        }
        stdin.flush().await?;
        let mut reader = tokio::io::BufReader::new(stdout).lines();
        loop {
            match reader.next_line().await? {
                None => break Err(anyhow!("codex app-server exited before answering")),
                Some(line) => {
                    if let Some(r) = find_response(&line, id) {
                        break r;
                    }
                }
            }
        }
    }
    .await;
    let _ = child.kill().await;
    result
}

pub async fn codex_rpc(app: &Arc<App>, host: &str, method: &str, params: Value) -> Result<Value> {
    codex_rpc_in(app, host, None, method, params).await
}

/// 同 [`codex_rpc`]，但 `home` 是身份的 `CODEX_HOME`（絕對路徑）；`None` ＝ daemon 預設家目錄。
pub async fn codex_rpc_in(app: &Arc<App>, host: &str, home: Option<&str>, method: &str, params: Value) -> Result<Value> {
    const ID: u64 = 2;
    let lines = rpc_lines(ID, method, &params);
    let exe = crate::tools::cached_path(app, host, "codex").await.unwrap_or_else(|| "codex".into());

    if host == LOCAL_HOST {
        return tokio::time::timeout(LOCAL_RPC_TIMEOUT, codex_rpc_local(&exe, &lines, ID, home))
            .await
            .map_err(|_| anyhow!("codex app-server `{method}` timed out"))?;
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let printf_args = lines.iter().map(|l| sh_quote(l)).collect::<Vec<_>>().join(" ");
    let env = home.map(|dir| home_env_prefix("CODEX_HOME", dir)).unwrap_or_default();
    let script = format!(
        "{env}{{ printf '%s\\n' {printf_args}; sleep {REMOTE_RPC_HOLD_SECS}; }} | {exe} app-server 2>/dev/null | awk '{{print}} /\"id\":{ID}[,}}]/ {{exit}}'\n",
        exe = sh_quote(&exe),
    );
    let out = conn.ssh_exec_path(&script).await.with_context(|| format!("codex app-server on {host}"))?;
    find_response(&out, ID).ok_or_else(|| anyhow!("codex app-server on {host}: no response to `{method}`:\n{}", out.trim()))?
}

/// agy 的模型清單，依「主機＋帳號」抓（身份＝換 `HOME`，帳號不同可用的模型也不同，不能拿別的帳號或寫死的清單冒充；
/// 經 [`agy_models_from_text`] 過濾，只收 Gemini 3.8 含以上）。
/// - 本機：用這個身份的 `HOME` 跑 `agy models`。
/// - 遠端：先走 ssh（憑證存在檔案裡的帳號讀得到：影子家目錄沒有 Keychain，agy 把 token 寫在 `.gemini/antigravity-cli/antigravity-oauth-token`，
///   2026-11-20 m4p 的 ag1 實測）；ssh 拿不到（macOS 預設帳號的憑證在 login Keychain，非互動 ssh 讀不到，回「Please sign in」）
///   就在那台 herdr 的 pane 裡跑（GUI session 讀得到 Keychain，同額度探測 `quota_agy::pane_probe` 的做法），從畫面讀回來。
/// 都拿不到（沒登入、逾時、exe 沒有快取）回 `static`，呼叫端（[`list`]）先用這台這個身份上次抓到的，再借別台同身份的，最後才是寫死的 [`agy_static_models`]。
async fn agy_model_list(app: &Arc<App>, host: &str, identity: Option<&str>) -> Result<(&'static str, Vec<Value>)> {
    let Some(exe) = crate::tools::cached_path(app, host, "agy").await else { return Ok(("static", agy_static_models())) };
    let home = identity_home_dir(app, host, identity, "agy", "HOME").await?;
    if host == LOCAL_HOST {
        if let Ok(run) = crate::hosts::sh_local_capture(&agy_models_script(&exe, home.as_deref()), AGY_MODELS_TIMEOUT).await {
            let models = agy_models_from_text(&String::from_utf8_lossy(&run.stdout));
            if !run.timed_out && !models.is_empty() {
                return Ok(("agy-cli", models));
            }
        }
        return Ok(("static", agy_static_models()));
    }
    let Some(conn) = app.hosts.get(host).await else { return Ok(("static", agy_static_models())) };
    let script = format!("( {} ) 2>/dev/null || true\n", agy_models_script(&exe, home.as_deref()));
    if let Ok(out) = conn.ssh_exec_path_timeout(&script, AGY_MODELS_TIMEOUT).await {
        let models = agy_models_from_text(&out);
        if !models.is_empty() {
            return Ok(("agy-cli", models));
        }
    }
    match agy_models_via_pane(app, host, &exe, home.as_deref()).await {
        Ok(models) if !models.is_empty() => Ok(("agy-cli", models)),
        Ok(_) => Ok(("static", agy_static_models())),
        Err(e) => {
            tracing::debug!(host, identity = identity.unwrap_or(""), error = %format!("{e:#}"), "agy models pane probe failed; falling back to the last known list");
            Ok(("static", agy_static_models()))
        }
    }
}

/// 在遠端 herdr 的 pane 裡跑 `agy models`、從畫面讀清單。跟 agy 額度探測共用 `#agy` 探測鎖與 workspace label
/// （一次只開一個探測 pane；殘留的由同一套清理收掉）。沒登入（`Please sign in`）提早結束，回空清單。
async fn agy_models_via_pane(app: &Arc<App>, host: &str, exe: &str, home: Option<&str>) -> Result<Vec<Value>> {
    use crate::quota_agy::{is_probe_label, models_sign_in_required, pane_done, pane_models_command, after_begin, PANE_BEGIN, PROBE_LABEL_PREFIX};
    let _guard = crate::quota::probe_lock(&format!("{host}#agy")).await;
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let client = crate::quota_claude::client_for_fence(host, &fence).await?;
    let real_home = crate::hosts::home_for_fence(&fence).await?;
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` changed before its agy models probe");
    }
    let env: std::collections::BTreeMap<String, String> = home.map(|dir| [("HOME".to_string(), dir.to_string())].into()).unwrap_or_default();
    let label = crate::quota_claude::tagged_probe_label(app.as_ref(), host, PROBE_LABEL_PREFIX.to_string()).await;
    let run = crate::quota_claude::run_marked_pane(
        &client,
        &real_home,
        &label,
        json!({}),
        &pane_models_command(Some(exe), &env),
        AGY_MODELS_PANE_TIMEOUT,
        PANE_BEGIN,
        is_probe_label,
        pane_done,
        |screen| models_sign_in_required(after_begin(screen)),
    )
    .await?;
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` was reconnected/reconfigured during the agy models probe; stale result discarded");
    }
    Ok(match run {
        crate::quota_claude::MarkedRun::Done((_rc, text)) => agy_models_from_screen(&text),
        crate::quota_claude::MarkedRun::Early(_) => vec![],
        crate::quota_claude::MarkedRun::TimedOut(_) => bail!("`agy models` on {host} did not finish within {}s", AGY_MODELS_PANE_TIMEOUT.as_secs()),
    })
}

pub async fn fetch(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>) -> Result<Value> {
    let (source, models) = match kind {
        "codex" => {
            let home = identity_home_dir(app, host, identity, "codex", "CODEX_HOME").await?;
            let r = codex_rpc_in(app, host, home.as_deref(), "model/list", json!({"includeHidden": false})).await?;
            let m = codex_models_from_rpc(&r);
            if m.is_empty() {
                bail!("codex app-server returned no models");
            }
            let cfg_text = read_optional_text(app.as_ref(), host, &home_file_expr(home.as_deref(), ".codex", "config.toml")).await?;
            ("codex-app-server", codex_apply_identity_config(m, &cfg_text))
        }
        "grok" => {
            let home = identity_home_dir(app, host, identity, "grok", "GROK_HOME").await?;
            let env = home.as_deref().map(|dir| home_env_prefix("GROK_HOME", dir)).unwrap_or_default();
            let exe = crate::tools::cached_path(app, host, "grok").await.unwrap_or_else(|| "grok".into());
            let text = if host == LOCAL_HOST {
                let script = format!(
                    "{env}( \"${{SHELL:-/bin/sh}}\" -lic {q} 2>/dev/null || {exe} models 2>/dev/null )",
                    q = sh_quote(&format!("{exe} models")),
                    exe = sh_quote(&exe)
                );
                crate::hosts::sh_local_stdout(&script, Duration::from_secs(30), "`grok models`").await?
            } else {
                let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
                conn.ssh_exec_path(&format!("{env}{} models 2>/dev/null </dev/null\n", sh_quote(&exe))).await?
            };
            let m = grok_models_from_text(&text);
            if m.is_empty() {
                bail!("could not parse `grok models` output:\n{}", text.trim());
            }
            let cfg_text = read_optional_text(app.as_ref(), host, &home_file_expr(home.as_deref(), ".grok", "config.toml")).await?;
            let cache_text = read_optional_text(app.as_ref(), host, &home_file_expr(home.as_deref(), ".grok", "models_cache.json")).await?;
            let m = enrich_grok_models(m, &cache_text, &cfg_text);
            ("grok-cli", m)
        }
        "claude" => {
            let config_dir = claude_config_dir(app, host, identity).await?;
            let (global, per_model, default_model) = read_claude_effort_settings(app.as_ref(), host, config_dir.as_deref()).await?;
            ("static", claude_static_models(global.as_deref(), &per_model, default_model.as_deref()))
        }
        "agy" => agy_model_list(app, host, identity).await?,
        other => bail!("unknown kind `{other}`"),
    };
    Ok(json!({
        "kind": kind,
        "host": host,
        "source": source,
        "fetched_at": crate::db::now(),
        "models": models,
    }))
}

/// 檔名片段：只留 `[A-Za-z0-9._-]`，其餘換成 `_`；有被換過的再補 FNV-1a 雜湊，避免 `a/b` 與 `a_b` 撞檔。
fn safe_part(raw: &str) -> String {
    let clean: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') { c } else { '_' }).collect();
    if clean == raw && !clean.is_empty() && clean != "." && clean != ".." {
        return clean;
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in raw.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
    format!("{}~{h:016x}", clean.trim_start_matches('.'))
}

/// 「最後抓到」檔名裡的身份片段。agy 的預設帳號（沒有身份、`ag0`、舊資料 `agy`）一律寫成 `ag0`，
/// 檔名才看得出是哪個帳號（`local-agy-ag0.json`／`local-agy-ag1.json`）；其他 kind 照舊（沒有身份＝空字串的雜湊）。
fn last_known_identity_part(kind: &str, identity: Option<&str>) -> String {
    match identity {
        Some(name) if !crate::tools::is_builtin_default_identity(kind, name) => safe_part(name),
        _ if kind == "agy" => crate::tools::AGY_DEFAULT_IDENTITY.to_string(),
        _ => safe_part(""),
    }
}

fn last_known_path(dir: &std::path::Path, host: &str, kind: &str, identity: Option<&str>) -> std::path::PathBuf {
    let name = format!("{}-{}-{}.json", safe_part(host), safe_part(kind), last_known_identity_part(kind, identity));
    dir.join("models-last").join(name)
}

/// 舊檔名遷移：agy 預設帳號以前存成 `<host>-agy-~<空字串雜湊>.json`，改名成 `<host>-agy-ag0.json`（新檔已在就丟掉舊的）。
/// 讀寫 agy 的「最後抓到」之前各跑一次；沒有舊檔時只是一次 `read_dir`。
fn migrate_agy_default_last_known(dir: &std::path::Path) {
    let legacy = format!("-agy-{}.json", safe_part(""));
    let Ok(entries) = std::fs::read_dir(dir.join("models-last")) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(head) = name.strip_suffix(&legacy) else { continue };
        let target = entry.path().with_file_name(format!("{head}-agy-{}.json", crate::tools::AGY_DEFAULT_IDENTITY));
        if target.exists() {
            let _ = std::fs::remove_file(entry.path());
        } else {
            let _ = std::fs::rename(entry.path(), target);
        }
    }
}

/// 成功查詢後寫「最後抓到」：0600、暫存檔＋rename。寫不成不影響回應。
fn save_last_known(dir: &std::path::Path, host: &str, kind: &str, identity: Option<&str>, v: &Value) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if kind == "agy" {
        migrate_agy_default_last_known(dir);
    }
    let path = last_known_path(dir, host, kind, identity);
    let Some(parent) = path.parent() else { return };
    let _ = std::fs::create_dir_all(parent);
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(v.to_string().as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// 讀「最後抓到」並標 `stale: true`；檔案不在、壞掉、沒有模型都當沒有。
fn load_last_known(dir: &std::path::Path, host: &str, kind: &str, identity: Option<&str>) -> Option<Value> {
    let text = std::fs::read_to_string(last_known_path(dir, host, kind, identity)).ok()?;
    let mut v: Value = serde_json::from_str(&text).ok()?;
    if !v.get("models").and_then(Value::as_array).is_some_and(|m| !m.is_empty()) {
        return None;
    }
    v["stale"] = json!(true);
    Some(v)
}

/// agy 專用：這台抓不到時，借「其他主機」上次抓到的 agy 清單（`fetched_at` 最新的那份）。只看 kind 是 agy、
/// **同一個身份**（檔名尾巴 `-agy-<身份>.json`；帳號不同，可用模型也不同，不能借）、來源不是這台、清單非空的檔。
/// 標 `stale`、`source: "other-host"`、`source_host`（`host` 仍是要查的這台）。
fn load_agy_sibling(dir: &std::path::Path, host: &str, identity: Option<&str>) -> Option<Value> {
    let entries = std::fs::read_dir(dir.join("models-last")).ok()?;
    let suffix = format!("-agy-{}.json", last_known_identity_part("agy", identity));
    let mut best: Option<(String, Value)> = None;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().ends_with(&suffix) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        if v.get("kind").and_then(Value::as_str) != Some("agy") {
            continue;
        }
        let other = v.get("host").and_then(Value::as_str).unwrap_or("");
        if other.is_empty() || other == host || !v.get("models").and_then(Value::as_array).is_some_and(|m| !m.is_empty()) {
            continue;
        }
        let at = v.get("fetched_at").and_then(Value::as_str).unwrap_or("").to_string();
        if best.as_ref().map_or(true, |(b, _)| at > *b) {
            best = Some((at, v));
        }
    }
    let (_, mut v) = best?;
    v["source_host"] = v["host"].clone();
    v["host"] = json!(host);
    v["source"] = json!("other-host");
    v["stale"] = json!(true);
    Some(v)
}

/// 讀不到（或回了寫死清單）時的回退順序：這台上次抓到 → （agy 才有）同 kind、同身份的其他主機上次抓到 → `None`（呼叫端用寫死清單）。
fn fallback_last_known(dir: &std::path::Path, host: &str, kind: &str, identity: Option<&str>) -> Option<Value> {
    if kind == "agy" {
        migrate_agy_default_last_known(dir);
    }
    load_last_known(dir, host, kind, identity).or_else(|| if kind == "agy" { load_agy_sibling(dir, host, identity) } else { None })
}

pub async fn list(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>, refresh: bool) -> Result<Value> {
    let identity = identity.filter(|name| !crate::tools::is_builtin_default_identity(kind, name));
    let key = format!("{host}/{kind}/{}", identity.unwrap_or(""));
    if !refresh {
        if let Some((at, v)) = app.models_cache.lock().await.get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(v.clone());
            }
        }
    }
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let fetched = fetch(app, host, kind, identity).await;
    let v = match fetched {
        Ok(v) if v["source"] != "static" => {
            save_last_known(&app.data_dir, host, kind, identity, &v);
            v
        }
        Ok(v) if kind == "claude" => v,
        Ok(v) => match fallback_last_known(&app.data_dir, host, kind, identity) {
            Some(last) => return Ok(last),
            None => v,
        },
        Err(e) => match fallback_last_known(&app.data_dir, host, kind, identity) {
            Some(last) => return Ok(last),
            None => return Err(e),
        },
    };
    let mut cache = app.models_cache.lock().await;
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` was reconnected/reconfigured while listing {kind} models; stale result discarded");
    }
    cache.retain(|_, entry| entry.0.elapsed() < CACHE_TTL);
    cache.insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// 行程還活著嗎（殭屍不算）。
    fn alive(pid: &str) -> bool {
        let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", pid]).output().unwrap();
        let st = String::from_utf8_lossy(&out.stdout);
        let st = st.trim();
        !st.is_empty() && !st.starts_with('Z')
    }

    /// issue #889：login shell 探測卡住時，逾時要回錯，而且卡住的 shell 行程要被收掉。
    #[tokio::test]
    async fn a_hung_login_shell_probe_times_out_and_is_reaped() {
        let dir = crate::testing::scratch_dir("am-models-probe");
        let fake = dir.join("fake-sh");
        crate::testing::write_exec(&fake, "#!/bin/sh\necho $$ > \"$PIDFILE\"\nsleep 30\n");
        let pidfile = dir.join("pid");
        let prefix = format!(
            "SHELL={} PIDFILE={}; export SHELL PIDFILE\n",
            sh_quote(&fake.to_string_lossy()),
            sh_quote(&pidfile.to_string_lossy())
        );
        let mut pid = None;
        for timeout_ms in [200u64, 1_000, 5_000, 20_000] {
            let _ = std::fs::remove_file(&pidfile);
            let started = Instant::now();
            let err = resolve_local_exe_with("codex", Duration::from_millis(timeout_ms), &prefix).await.unwrap_err();
            assert!(err.to_string().contains("timed out"), "{err}");
            assert!(started.elapsed() < Duration::from_millis(timeout_ms) + Duration::from_secs(10), "逾時要照時限回來");
            if let Ok(p) = std::fs::read_to_string(&pidfile) {
                if p.ends_with('\n') && p.trim().parse::<u32>().is_ok() {
                    pid = Some(p.trim().to_string());
                    break;
                }
            }
        }
        let pid = pid.expect("四輪（最長 20 秒）假 shell 都沒跑到寫 pid 檔");
        let dead = crate::testing::eventually!(!alive(&pid));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(dead, "逾時後卡住的 login shell（pid {pid}）要被收掉");
    }

    /// #1349：每次 list 順手丟掉過期的格子（identity 是任意字串，過期的每一格不回收就永遠留在 map 裡）。
    #[tokio::test]
    async fn listing_evicts_entries_older_than_the_ttl() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let old = std::time::Instant::now().checked_sub(CACHE_TTL + Duration::from_secs(1)).unwrap();
        app.models_cache.lock().await.insert("local/agy/stale".into(), (old, json!({"models": []})));
        list(app, "local", "agy", Some("fresh"), false).await.unwrap();
        let mut keys: Vec<String> = app.models_cache.lock().await.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["local/agy/fresh".to_string()]);
    }

    /// agy 回退順序（2026-11-20：m4p 的 `agy models` 讀不到登入）：這台上次抓到 → 同 kind 其他主機上次抓到（最新、清單非空）→ `None`（呼叫端用寫死清單）。
    #[test]
    fn agy_fallback_order_own_then_other_host_then_none() {
        let dir = crate::testing::scratch_dir("am-models-agy-fallback");
        let one = |id: &str| json!([{"id": id}]);
        let local_old = json!({"kind": "agy", "host": "local", "source": "agy-cli", "fetched_at": "2026-11-19T00:00:00Z", "models": one("gemini-4-neon-medium")});
        let local_new = json!({"kind": "agy", "host": "local", "source": "agy-cli", "fetched_at": "2026-11-20T00:00:00Z", "models": one("gemini-4-argon-high")});
        let codex = json!({"kind": "codex", "host": "other", "source": "codex-app-server", "fetched_at": "2026-11-21T00:00:00Z", "models": one("gpt-6.1-sol")});
        let empty = json!({"kind": "agy", "host": "lab", "source": "agy-cli", "fetched_at": "2026-11-22T00:00:00Z", "models": []});
        assert!(fallback_last_known(&dir, "m4p", "agy", Some("ag1")).is_none(), "什麼都沒有＝None");
        save_last_known(&dir, "local", "agy", None, &local_old);
        save_last_known(&dir, "local", "agy", Some("ag1"), &local_new);
        save_last_known(&dir, "other", "codex", None, &codex);
        save_last_known(&dir, "lab", "agy", None, &empty);
        let got = fallback_last_known(&dir, "m4p", "agy", Some("ag1")).unwrap();
        assert_eq!(got["source"], json!("other-host"));
        assert_eq!(got["source_host"], json!("local"), "借的是最新的 local 那份（清單非空的）");
        assert_eq!(got["host"], json!("m4p"), "host 仍是查的這台");
        assert_eq!(got["stale"], json!(true));
        assert_eq!(got["models"][0]["id"], json!("gemini-4-argon-high"));
        let own = json!({"kind": "agy", "host": "m4p", "source": "agy-cli", "fetched_at": "2026-11-18T00:00:00Z", "models": one("gemini-3.8-flash-high")});
        save_last_known(&dir, "m4p", "agy", Some("ag1"), &own);
        let got = fallback_last_known(&dir, "m4p", "agy", Some("ag1")).unwrap();
        assert_eq!(got["host"], json!("m4p"));
        assert_eq!(got["source"], json!("agy-cli"), "自己的上次抓到優先");
        assert_eq!(got["models"][0]["id"], json!("gemini-3.8-flash-high"));
        assert!(fallback_last_known(&dir, "m4p", "codex", None).is_none(), "非 agy 不借別台");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// agy 身份＝換 HOME：`agy models` 的指令要帶這個身份的 HOME，沒有身份就不改 HOME（預設帳號）。
    #[test]
    fn agy_models_script_runs_under_the_identity_home() {
        let with = agy_models_script("/opt/agy", Some("/home/u/.agy-ag1"));
        assert!(with.starts_with("HOME='/home/u/.agy-ag1'; export HOME; "), "{with}");
        assert!(with.contains("'/opt/agy' models"), "{with}");
        assert!(!agy_models_script("/opt/agy", None).contains("export HOME"), "沒有身份＝預設 HOME");
    }

    /// agy 借別台的清單只借「同一個身份」的：ag1 的清單不借給 ag2，也不借給預設帳號；預設帳號的清單不借給 ag1。
    #[test]
    fn agy_sibling_borrows_only_the_same_identity() {
        let dir = crate::testing::scratch_dir("am-models-agy-identity");
        let one = |id: &str| json!([{"id": id}]);
        let ag1 = json!({"kind": "agy", "host": "local", "source": "agy-cli", "fetched_at": "2026-11-20T00:00:00Z", "models": one("gemini-3.8-flash-high")});
        let default = json!({"kind": "agy", "host": "local", "source": "agy-cli", "fetched_at": "2026-11-21T00:00:00Z", "models": one("gemini-4-neon-medium")});
        save_last_known(&dir, "local", "agy", Some("ag1"), &ag1);
        save_last_known(&dir, "local", "agy", None, &default);
        let got = load_agy_sibling(&dir, "m4p", Some("ag1")).expect("同身份要借得到");
        assert_eq!(got["models"][0]["id"], json!("gemini-3.8-flash-high"));
        assert!(load_agy_sibling(&dir, "m4p", Some("ag2")).is_none(), "別的身份不借");
        let got = load_agy_sibling(&dir, "m4p", None).expect("預設帳號借預設帳號的");
        assert_eq!(got["models"][0]["id"], json!("gemini-4-neon-medium"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 遠端 pane 版：指令包在 `AM_AGY_BEGIN`／`AM_AGY_DONE=` 標記裡、帶身份的 HOME；畫面上 tab 變空白也解得出 slug，
    /// 不收的模型與雜訊行（提示字、shell 回顯）照樣濾掉；沒登入那句認得出來。
    #[test]
    fn agy_models_read_back_from_a_remote_pane_screen() {
        use crate::quota_agy::{models_sign_in_required, pane_done, pane_models_command};
        let env: std::collections::BTreeMap<String, String> = [("HOME".to_string(), "/Users/m4p/.agy-ag1".to_string())].into();
        let cmd = pane_models_command(Some("/Users/m4p/.local/bin/agy"), &env);
        assert!(cmd.contains("HOME='/Users/m4p/.agy-ag1' '/Users/m4p/.local/bin/agy' models </dev/null"), "{cmd}");
        assert!(!cmd.contains("AM_AGY_BEGIN") && !cmd.contains("AM_AGY_DONE="), "標記拆成 printf 參數，shell 回顯不會被當成標記：{cmd}");
        assert!(!pane_models_command(Some("agy"), &Default::default()).contains("HOME="), "預設帳號不改 HOME");

        let screen = "m4p@mac ~ % d=$(mktemp -d) && …\n\nAM_AGY_BEGIN\nFetching available models...\n\
gemini-4-neon-high          Gemini 4 Neon (High)\n\
gemini-3.8-flash-medium     Gemini 3.8 Flash (Medium)\n\
gemini-3.8-flash-low        Gemini 3.8 Flash (Low)\n\
claude-opus-5-5-low         Claude Opus 5.5 (Low)\n\
gpt-oss-120b-medium         GPT-OSS 120B (Medium)\n\nAM_AGY_DONE=0\nm4p@mac ~ % ";
        let (rc, body) = pane_done(screen).expect("跑完");
        assert_eq!(rc, 0);
        let models = agy_models_from_screen(&body);
        let ids: Vec<&str> = models.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["gemini-3.8-flash-medium", "gemini-4-neon-high", "gemini-3.8-flash-low"], "預設排第一，claude／gpt-oss 不收");
        assert_eq!(models[1]["display_name"], json!("Gemini 4 Neon (High)"));
        assert_eq!(models[0]["is_default"], json!(true));

        let out = "\nAM_AGY_BEGIN\nFetching available models...\nError: Please sign in to view available models. Launch the CLI without arguments to sign in.\n";
        assert!(models_sign_in_required(out));
        assert!(agy_models_from_screen(out).is_empty(), "沒登入＝空清單，呼叫端退回上次抓到的");
        assert!(!models_sign_in_required(&body));
    }

    /// agy 預設帳號的「最後抓到」檔名是 `<host>-agy-ag0.json`（沒有身份、`ag0`、舊資料 `agy` 同一個檔）；
    /// 舊的雜湊檔名在下一次讀寫時改名過來，新檔已在就丟掉舊的；ag1 的檔不受影響。
    #[test]
    fn the_agy_default_accounts_last_known_file_is_named_ag0_and_the_old_name_migrates() {
        let dir = crate::testing::scratch_dir("am-models-agy-ag0");
        let last = dir.join("models-last");
        std::fs::create_dir_all(&last).unwrap();
        let body = |id: &str| json!({"kind": "agy", "host": "local", "source": "agy-cli", "fetched_at": "2026-11-20T00:00:00Z", "models": [{"id": id}]});
        let legacy = last.join(format!("local-agy-{}.json", safe_part("")));
        std::fs::write(&legacy, body("gemini-4-neon-medium").to_string()).unwrap();
        std::fs::write(last.join("local-agy-ag1.json"), body("gemini-3.8-flash-high").to_string()).unwrap();
        for identity in [None, Some("ag0"), Some("agy")] {
            assert_eq!(last_known_path(&dir, "local", "agy", identity), last.join("local-agy-ag0.json"), "{identity:?}");
        }
        let got = fallback_last_known(&dir, "local", "agy", None).expect("舊檔名遷移後讀得到");
        assert_eq!(got["models"][0]["id"], json!("gemini-4-neon-medium"));
        assert!(!legacy.exists() && last.join("local-agy-ag0.json").exists(), "舊檔改名成 ag0");
        assert_eq!(fallback_last_known(&dir, "local", "agy", Some("ag1")).unwrap()["models"][0]["id"], json!("gemini-3.8-flash-high"));
        std::fs::write(&legacy, body("stale").to_string()).unwrap();
        save_last_known(&dir, "local", "agy", Some("ag0"), &body("gemini-4-argon-low"));
        assert!(!legacy.exists());
        assert_eq!(fallback_last_known(&dir, "m4p", "agy", None).expect("別台借 ag0 的")["models"][0]["id"], json!("gemini-4-argon-low"));
        assert_eq!(last_known_path(&dir, "local", "codex", None), last.join(format!("local-codex-{}.json", safe_part(""))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_known_roundtrip_stale_and_isolation() {
        let dir = crate::testing::scratch_dir("am-models-last");
        let a = json!({"kind": "codex", "host": "local", "source": "codex-app-server", "fetched_at": "t1", "models": [{"id": "gpt-6.1-sol"}]});
        let b = json!({"kind": "codex", "host": "local", "source": "codex-app-server", "fetched_at": "t2", "models": [{"id": "other"}]});
        assert!(load_last_known(&dir, "local", "codex", None).is_none(), "沒有檔");
        save_last_known(&dir, "local", "codex", None, &a);
        save_last_known(&dir, "local", "codex", Some("work"), &b);
        save_last_known(&dir, "local", "codex", Some("a/b"), &a);
        save_last_known(&dir, "local", "codex", Some("a_b"), &b);
        let got = load_last_known(&dir, "local", "codex", None).unwrap();
        assert_eq!(got["stale"], json!(true));
        assert_eq!(got["fetched_at"], json!("t1"));
        assert_eq!(got["models"][0]["id"], json!("gpt-6.1-sol"));
        assert_eq!(load_last_known(&dir, "local", "codex", Some("work")).unwrap()["models"][0]["id"], json!("other"), "不同 identity 不互蓋");
        assert_ne!(load_last_known(&dir, "local", "codex", Some("a/b")).unwrap()["models"][0]["id"], load_last_known(&dir, "local", "codex", Some("a_b")).unwrap()["models"][0]["id"]);
        let path = last_known_path(&dir, "local", "codex", None);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(std::fs::read_dir(path.parent().unwrap()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".tmp")), "不留暫存檔");
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_last_known(&dir, "local", "codex", None).is_none(), "壞檔當沒有");
        std::fs::write(&path, r#"{"models": []}"#).unwrap();
        assert!(load_last_known(&dir, "local", "codex", None).is_none(), "空清單當沒有");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 查詢失敗（未知 host 之外：codex 不存在的 kind 之類）時，有上次的就回 stale，沒有才回錯。
    #[tokio::test]
    async fn failed_fetch_serves_last_known_or_errors() {
        let env = crate::testing::env().await;
        let app = &env.app;
        assert!(list(app, "local", "nope", None, true).await.is_err());
        let last = json!({"kind": "nope", "host": "local", "source": "x", "fetched_at": "t0", "models": [{"id": "m1"}]});
        save_last_known(&app.data_dir, "local", "nope", None, &last);
        let v = list(app, "local", "nope", None, true).await.unwrap();
        assert_eq!(v["stale"], json!(true));
        assert_eq!(v["models"][0]["id"], json!("m1"));
    }

    #[tokio::test]
    async fn an_exe_with_a_slash_skips_the_probe() {
        assert_eq!(resolve_local_exe("/opt/x/codex", Duration::from_millis(1)).await.unwrap(), "/opt/x/codex");
    }
}
