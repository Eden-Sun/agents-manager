//! agy（Antigravity CLI）額度（SPEC §12a.7）。數字來自 `agy -p "/usage" --output-format json`：唯讀指令，**不開對話、不耗額度**，
//! 所以不必像 grok 那樣開一顆探測 pane——在拋棄式暫存目錄裡直接跑、有逾時、跑完刪目錄。
//!
//! 回應（2026-10-04 真機，1.2.16）的 `response` 是 tab 分隔的幾列：
//! ```text
//! Gemini Models\tWeekly Limit Remaining\t98%\t2026-10-11T15:39:29Z
//! Claude and GPT models\tWeekly Limit Remaining\t100%\t2026-10-11T15:56:55Z
//! ```
//! 兩個**每週**桶，各自是一把 key：`agy`（Gemini）與 `agy:claude-gpt`（Claude 與 GPT-OSS），都只填 `seven_day`（跟 grok 一樣，網頁標「週」）。
//! 探測失敗**不覆蓋**舊值（讀數自己會變陳舊，`quota::STALE_AFTER`）；格式變了讀不懂就回 `None`，不編數字。

use crate::config::LOCAL_HOST;
use crate::quota::{Quota, Window};
use crate::state::App;
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// 每週桶變動很慢，而每次探測要起一個 200 MB 的執行檔。
pub const AGY_POLL: Duration = Duration::from_secs(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(40);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(15 * 60);

/// Gemini 那一桶用裸 `agy`；Claude／GPT 那一桶的子帳號名。
pub const CLAUDE_GPT_KEY: &str = "agy:claude-gpt";

/// 探測指令（本機與遠端同一份）：拋棄式 cwd、不讀 stdin、關自動更新、跑完刪目錄。`exe` 是偵測到的絕對路徑，沒有就用 PATH 上的 `agy`。
pub fn probe_script(exe: Option<&str>) -> String {
    let exe = crate::hosts::sh_quote(exe.unwrap_or("agy"));
    format!(
        "d=$(mktemp -d 2>/dev/null) || exit 1; cd \"$d\" || exit 1; \
         AGY_CLI_DISABLE_AUTO_UPDATE=true {exe} -p /usage --output-format json </dev/null; rc=$?; cd / ; rm -rf \"$d\"; exit $rc"
    )
}

fn bucket_key(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.contains("gemini") {
        Some("agy")
    } else if n.contains("claude") || n.contains("gpt") {
        Some(CLAUDE_GPT_KEY)
    } else {
        None
    }
}

fn parse_remaining(field: &str) -> Option<f64> {
    let t = field.trim().strip_suffix('%')?.trim();
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `[(key, Quota)]`，一列一桶；認不得的列（桶名、窗名、百分比）略過，一列都讀不到回 `None`。
/// 只收每週窗（`Weekly`）；別的窗名不猜它是 5h 還是 7d。
pub fn parse_usage(stdout: &str) -> Option<Vec<(&'static str, Quota)>> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let response = v.get("response")?.as_str()?;
    let now = crate::db::now();
    let mut out: Vec<(&'static str, Quota)> = Vec::new();
    for line in response.lines() {
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        if f.len() < 3 {
            continue;
        }
        let Some(key) = bucket_key(f[0]) else { continue };
        if !f[1].to_ascii_lowercase().contains("week") {
            continue;
        }
        let Some(left) = f[2..].iter().find_map(|x| parse_remaining(x)) else { continue };
        // 重置時間讀不出來就 `None`（由窗長規則收尾），不拿別的欄位湊。
        let resets_at = f[2..]
            .iter()
            .find_map(|x| chrono::DateTime::parse_from_rfc3339(x).ok())
            .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        if out.iter().any(|(k, _)| *k == key) {
            continue;
        }
        out.push((
            key,
            Quota {
                five_hour: None,
                seven_day: Some(Window { observed_at: None, used_pct: (100.0 - left).clamp(0.0, 100.0), resets_at }),
                fable: None,
                reset_credits: None,
                limit_hit: None,
                plan: None,
                updated_at: now.clone(),
                source: "agy-usage".into(),
                account: None,
                host: LOCAL_HOST.into(),
            },
        ));
    }
    (!out.is_empty()).then_some(out)
}

/// `Ok(false)` ＝這台主機沒裝 agy（不探測、不報錯）。
pub async fn refresh_agy(app: &Arc<App>, host: &str) -> Result<bool> {
    let _guard = crate::quota::probe_lock(&format!("{host}#agy")).await;
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    if !app.tools.lock().await.contains_key(host) {
        crate::tools::detect(app, host).await?;
    }
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` changed before its agy quota probe");
    }
    let Some(exe) = crate::tools::cached_path(app, host, "agy").await else { return Ok(false) };
    let script = probe_script(Some(&exe));
    let stdout = if fence.conn().is_local() {
        crate::hosts::sh_local_stdout(&script, PROBE_TIMEOUT, "`agy -p /usage`").await?
    } else {
        fence.conn().ssh_exec_path_timeout(&script, PROBE_TIMEOUT).await?
    };
    let Some(buckets) = parse_usage(&stdout) else {
        bail!("`agy -p /usage` on {host} printed no weekly limit row: {}", stdout.chars().take(200).collect::<String>());
    };
    for (key, q) in buckets {
        crate::quota::set_fenced(app, host, key, q, &fence).await?;
    }
    Ok(true)
}

fn backoff() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// `None` ＝這一輪跳過（上次失敗還在冷卻）。`GET /api/quota?refresh=1` 不走這裡，一律真的探測。
pub async fn refresh_agy_if_due(app: &Arc<App>, host: &str) -> Result<Option<bool>> {
    let key = crate::quota::quota_key(host, "agy");
    if backoff().lock().unwrap().get(&key).is_some_and(|t| *t > std::time::Instant::now()) {
        return Ok(None);
    }
    match refresh_agy(app, host).await {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            backoff().lock().unwrap().insert(key, std::time::Instant::now() + RETRY_AFTER_FAILURE);
            Err(e)
        }
    }
}

pub fn spawn_agy_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            crate::quota::for_each_host(crate::quota::pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
                    match refresh_agy_if_due(&app, &host).await {
                        Ok(Some(true) | None) => {}
                        Ok(Some(false)) => tracing::debug!(host = %host, "agy not installed; agy quota stays null"),
                        Err(e) => tracing::warn!(host = %host, error = %e, retry_in_s = RETRY_AFTER_FAILURE.as_secs(), "agy quota refresh failed; keeping the last reading"),
                    }
                }
            })
            .await;
            tokio::time::sleep(AGY_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-10-04 真機輸出（1.2.16，欄位原樣，其餘鍵略）。
    fn real() -> String {
        json!({"conversation_id": "", "status": "SUCCESS",
               "response": "Gemini Models\tWeekly Limit Remaining\t98%\t2026-10-11T15:39:29Z\nClaude and GPT models\tWeekly Limit Remaining\t100%\t2026-10-11T15:56:55Z\n"})
        .to_string()
    }

    #[test]
    fn the_real_output_gives_two_weekly_buckets_keyed_gemini_and_claude_gpt() {
        let got = parse_usage(&real()).expect("parsed");
        assert_eq!(got.iter().map(|(k, _)| *k).collect::<Vec<_>>(), ["agy", "agy:claude-gpt"]);
        let (_, gemini) = &got[0];
        let w = gemini.seven_day.as_ref().unwrap();
        assert!((w.used_pct - 2.0).abs() < 1e-9, "98% remaining = 2% used");
        assert_eq!(w.resets_at.as_deref(), Some("2026-10-11T15:39:29.000Z"));
        assert!(gemini.five_hour.is_none() && gemini.fable.is_none(), "只有週窗");
        assert_eq!(gemini.source, "agy-usage");
        assert_eq!(got[1].1.seven_day.as_ref().unwrap().used_pct, 0.0);
    }

    #[test]
    fn a_format_change_is_none_never_a_made_up_number() {
        for bad in [
            "",
            "not json",
            r#"{"response": 5}"#,
            r#"{"status":"SUCCESS"}"#,
            r#"{"response": "nothing useful here\n"}"#,
            r#"{"response": "Gemini Models\tWeekly Limit Remaining\tunknown\t2026-10-11T15:39:29Z"}"#,
            r#"{"response": "Gemini Models\tDaily Limit Remaining\t90%\t2026-10-11T15:39:29Z"}"#,
            r#"{"response": "Some Other Models\tWeekly Limit Remaining\t90%\t2026-10-11T15:39:29Z"}"#,
        ] {
            assert!(parse_usage(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn extra_columns_a_missing_reset_and_out_of_range_percentages_are_tolerated() {
        let r = json!({"response": "Gemini Models\tWeekly Limit Remaining\t120%\nClaude and GPT models\tWeekly Limit Remaining\t40%\tnote\t2026-10-11T00:00:00+00:00\n"}).to_string();
        let got = parse_usage(&r).unwrap();
        let g = got[0].1.seven_day.as_ref().unwrap();
        assert_eq!((g.used_pct, g.resets_at.as_deref()), (0.0, None), "超過 100% 夾進範圍、沒有重置時間就是 None");
        let c = got[1].1.seven_day.as_ref().unwrap();
        assert!((c.used_pct - 60.0).abs() < 1e-9);
        assert_eq!(c.resets_at.as_deref(), Some("2026-10-11T00:00:00.000Z"));
        // 只有一桶也算（另一桶之後再說）。
        let one = json!({"response": "Gemini Models\tWeekly Limit Remaining\t50%\t2026-10-11T00:00:00Z"}).to_string();
        assert_eq!(parse_usage(&one).unwrap().len(), 1);
    }

    #[test]
    fn the_probe_runs_in_a_throwaway_dir_without_updates_or_stdin_and_cleans_up() {
        let s = probe_script(Some("/home/u/.local/bin/agy"));
        assert!(s.contains("mktemp -d") && s.contains("rm -rf \"$d\""), "{s}");
        assert!(s.contains("AGY_CLI_DISABLE_AUTO_UPDATE=true") && s.contains("'/home/u/.local/bin/agy' -p /usage --output-format json </dev/null"), "{s}");
        assert!(probe_script(Some("/tmp/a b/agy")).contains("'/tmp/a b/agy'"), "路徑要 quote");
        assert!(probe_script(None).contains(" agy -p /usage") || probe_script(None).contains("'agy' -p /usage") || probe_script(None).contains("agy -p /usage"));
    }

    /// 真的跑一次那段 shell：用假 `agy`（輸出真機的 JSON），確認在別的目錄跑、目錄用完就刪、環境變數有帶、exit code 傳得出來。
    #[test]
    fn the_script_runs_a_fake_agy_in_a_temp_dir_that_is_gone_afterwards() {
        let dir = crate::testing::scratch_dir("am-agy-quota");
        let fake = dir.join("agy");
        let seen = dir.join("seen.txt");
        crate::testing::write_exec(
            &fake,
            format!("#!/bin/sh\npwd > {s}\necho \"$AGY_CLI_DISABLE_AUTO_UPDATE $*\" >> {s}\nprintf '%s' '{j}'\n", s = seen.display(), j = real()),
        );
        let out = crate::exec_retry::output(std::process::Command::new("/bin/sh").arg("-c").arg(probe_script(fake.to_str()))).unwrap();
        assert!(out.status.success());
        assert!(parse_usage(&String::from_utf8_lossy(&out.stdout)).is_some());
        let seen = std::fs::read_to_string(&seen).unwrap();
        let mut lines = seen.lines();
        let cwd = lines.next().unwrap();
        assert!(!cwd.starts_with(dir.to_str().unwrap()) && !std::path::Path::new(cwd).exists(), "拋棄式 cwd 用完刪掉：{cwd}");
        assert_eq!(lines.next().unwrap(), "true -p /usage --output-format json");
    }

    #[tokio::test]
    async fn a_host_without_agy_is_not_probed_and_not_an_error() {
        let e = crate::testing::env().await;
        let tools = |agy: bool| crate::tools::HostTools {
            tools: agy
                .then(|| ("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/opt/agy".into()), version: None, logged_in: None }))
                .into_iter()
                .collect(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        e.app.tools.lock().await.insert(LOCAL_HOST.into(), tools(false));
        assert!(!refresh_agy(&e.app, LOCAL_HOST).await.expect("no agy: Ok(false), no error"));
        assert!(e.app.quotas.lock().await.get("agy").is_none());

        // 遠端：沒裝＝不碰 ssh；有裝＝跑同一段 script，兩桶都記在 `<host>/` 底下。
        let host = format!("agy-remote-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = e.app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Ok(real())
        });
        e.app.tools.lock().await.insert(host.clone(), tools(false));
        assert!(!refresh_agy(&e.app, &host).await.unwrap());
        assert!(calls.lock().unwrap().is_empty(), "沒裝 agy 的遠端不探測");
        e.app.tools.lock().await.insert(host.clone(), tools(true));
        assert!(refresh_agy(&e.app, &host).await.unwrap());
        assert!(calls.lock().unwrap()[0].contains("'/opt/agy' -p /usage --output-format json"));
        let q = e.app.quotas.lock().await;
        assert!(q.contains_key(&format!("{host}/agy")) && q.contains_key(&format!("{host}/agy:claude-gpt")), "{:?}", q.keys().collect::<Vec<_>>());
        assert!(!q.contains_key("agy"), "遠端的讀數不能落在本機那格");
    }

    #[tokio::test]
    async fn both_buckets_are_published_and_survive_each_other_and_a_failed_probe_keeps_the_old_reading() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        for (key, q) in parse_usage(&real()).unwrap() {
            crate::quota::set(&app, LOCAL_HOST, key, q).await;
        }
        // 兩把 key 都在；`agy` 的寫入不會把 `agy:claude-gpt` 當成「收斂到裸 key 的分開那一格」清掉。
        let again = parse_usage(&real()).unwrap().remove(0);
        crate::quota::set(&app, LOCAL_HOST, again.0, again.1).await;
        let q = app.quotas.lock().await.clone();
        assert!(q.contains_key("agy") && q.contains_key("agy:claude-gpt"), "{:?}", q.keys().collect::<Vec<_>>());
        let before = q["agy"].seven_day.clone();
        // 探測壞掉（沒有 agy 輸出）：解析回 None，呼叫端不寫任何東西。
        assert!(parse_usage("garbage").is_none());
        assert_eq!(app.quotas.lock().await["agy"].seven_day, before);
        let snap = crate::quota::snapshot(&app).await;
        assert!(snap["kinds"]["agy"]["seven_day"]["used_pct"].is_number() && snap["kinds"]["agy:claude-gpt"]["seven_day"]["used_pct"].is_number(), "{snap}");
    }
}
