use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use crate::config::LOCAL_HOST;
use crate::hosts::sh_quote;
use crate::models::{
    agy_models_from_text, agy_models_script, agy_static_models, claude_config_dir, claude_static_models, codex_apply_identity_config,
    codex_models_from_rpc, enrich_grok_models, find_response, grok_models_from_text,
    home_env_prefix, home_file_expr, identity_home_dir, read_claude_effort_settings,
    read_optional_text, rpc_lines, CACHE_TTL,
};
use crate::state::App;

const LOCAL_RPC_TIMEOUT: Duration = Duration::from_secs(20);
const REMOTE_RPC_HOLD_SECS: u32 = 6;
/// `agy models` 要起一個約 200 MB 的執行檔，比 `--version` 慢；逾時就退回寫死的清單。
const AGY_MODELS_TIMEOUT: Duration = Duration::from_secs(30);

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
    // 身份的 `CODEX_HOME`：model/list 的結果（含預設模型）跟著這個家目錄走。沒有身份就照 daemon 的環境。
    if let Some(dir) = home {
        cmd.env("CODEX_HOME", dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn {exe} app-server"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    // 任何一步失敗（含寫 stdin、讀 stdout）都要走到下面的 kill，不留 app-server。
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

/// agy 的模型清單：本機跑 `agy models`（經 [`agy_models_from_text`] 過濾，只收 Gemini 3.8 含以上）。讀不到、解不出來、逾時，
/// 或是遠端（ssh 進去讀不到 Keychain，會回未登入）都退回寫死的 3.8 Flash 三檔。exe 沒有快取就不猜 PATH，同樣退回。
async fn agy_model_list(app: &Arc<App>, host: &str) -> (&'static str, Vec<Value>) {
    if host == LOCAL_HOST {
        if let Some(exe) = crate::tools::cached_path(app, host, "agy").await {
            if let Ok(run) = crate::hosts::sh_local_capture(&agy_models_script(&exe), AGY_MODELS_TIMEOUT).await {
                let models = agy_models_from_text(&String::from_utf8_lossy(&run.stdout));
                if !run.timed_out && !models.is_empty() {
                    return ("agy-cli", models);
                }
            }
        }
    }
    ("static", agy_static_models())
}

pub async fn fetch(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>) -> Result<Value> {
    let (source, models) = match kind {
        "codex" => {
            // 身份的 CODEX_HOME：app-server 的 model/list 與這個身份 config.toml 的預設才會是它自己的。
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
            // 身份的 GROK_HOME：`grok models` 與兩個檔都讀這個家目錄（沒有身份＝`~/.grok`）。
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
        "agy" => agy_model_list(app, host).await,
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

pub async fn list(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>, refresh: bool) -> Result<Value> {
    let key = format!("{host}/{kind}/{}", identity.unwrap_or(""));
    if !refresh {
        if let Some((at, v)) = app.models_cache.lock().await.get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(v.clone());
            }
        }
    }
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let v = fetch(app, host, kind, identity).await?;
    let mut cache = app.models_cache.lock().await;
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` was reconnected/reconfigured while listing {kind} models; stale result discarded");
    }
    // 過期的格子順手丟掉（同 `github::remember_issues`）：identity 是任意字串，不回收就只增不減（#1349）。
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
        std::fs::write(&fake, "#!/bin/sh\necho $$ > \"$PIDFILE\"\nsleep 30\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pidfile = dir.join("pid");
        let prefix = format!(
            "SHELL={} PIDFILE={}; export SHELL PIDFILE\n",
            sh_quote(&fake.to_string_lossy()),
            sh_quote(&pidfile.to_string_lossy())
        );
        // 200ms 內要 fork/exec 兩層 shell 再寫一次檔；整樹平行忙的時候不一定來得及。沒寫到就用更長的時限重來，
        // 寫到了才往下驗（正常機器第一輪就過）。時限照時限回來、卡住的 shell 被收掉，這兩件事每一輪都驗。
        let mut pid = None;
        for timeout_ms in [200u64, 1_000, 5_000, 20_000] {
            let _ = std::fs::remove_file(&pidfile);
            let started = Instant::now();
            let err = resolve_local_exe_with("codex", Duration::from_millis(timeout_ms), &prefix).await.unwrap_err();
            assert!(err.to_string().contains("timed out"), "{err}");
            assert!(started.elapsed() < Duration::from_millis(timeout_ms) + Duration::from_secs(10), "逾時要照時限回來");
            if let Ok(p) = std::fs::read_to_string(&pidfile) {
                // 假 shell 寫檔與逾時砍它可能同時發生：只收完整的一行數字。
                if p.ends_with('\n') && p.trim().parse::<u32>().is_ok() {
                    pid = Some(p.trim().to_string());
                    break;
                }
            }
        }
        let pid = pid.expect("四輪（最長 20 秒）假 shell 都沒跑到寫 pid 檔");
        // 被 SIGKILL 的行程要等排程器輪到才真的消失，2 秒在忙的機器上不夠。
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

    #[tokio::test]
    async fn an_exe_with_a_slash_skips_the_probe() {
        assert_eq!(resolve_local_exe("/opt/x/codex", Duration::from_millis(1)).await.unwrap(), "/opt/x/codex");
    }
}
