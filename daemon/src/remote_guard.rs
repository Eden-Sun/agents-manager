//! 連上（含 daemon 啟動／換版後的第一次連上與重連）遠端主機時的 kill 守門熱更新（使用者 2026-11-28）。
//! 做什麼、為什麼不重啟 bot：見 `lifecycle::guard_refresh`。這裡只負責挑出那台執行中的 bot、退避重試、
//! 把「守門設定待重啟套用」交給 `am_base::remote_guard_pending`（`supervisor::incidents` 開 incident，`agm health` 列出）。

use crate::lifecycle::guard_refresh::{self, GuardBot};
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;

/// 退避：立刻、1 分、5 分、15 分。連上那一刻 ssh 最不穩；host 已經不在線就放棄，下次連上再來一次（冪等）。
const RETRY_WAITS: [u64; 4] = [0, 60, 300, 900];

pub fn spawn_refresh(app: Arc<App>, host: String) {
    if crate::capabilities::Shutdown::shutdown(&*app).is_cancelled() {
        return;
    }
    let tracker = crate::capabilities::BgTasks::background_tasks(&*app).clone();
    crate::background_loop::spawn_named(&tracker, "remote kill-guard refresh", Box::pin(async move {
        for (i, wait) in RETRY_WAITS.iter().enumerate() {
            if *wait > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(*wait)).await;
            }
            match app.hosts.get(&host).await {
                Some(c) if c.is_connected() => {}
                _ => return,
            }
            match refresh_once(&app, &host).await {
                Ok(()) => return,
                Err(e) if i + 1 < RETRY_WAITS.len() => tracing::warn!(host = %host, attempt = i + 1, error = %format!("{e:#}"), "could not refresh the remote kill guard; will retry"),
                Err(e) => tracing::warn!(host = %host, error = %format!("{e:#}"), "gave up refreshing the remote kill guard until this host reconnects or the daemon restarts"),
            }
        }
    }));
}

pub(crate) async fn refresh_once(app: &Arc<App>, host: &str) -> anyhow::Result<()> {
    let Some(conn) = app.hosts.get(host).await else { return Ok(()) };
    if conn.is_local() || !conn.is_connected() {
        return Ok(());
    }
    let bots = running_bots(app, host).await?;
    let time_zone = app.cfg.agents_fresh().await.time_zone().to_string();
    let r = guard_refresh::refresh_host(&conn, app.instance().as_deref(), &time_zone, &bots).await?;
    if r.guard_updated {
        tracing::info!(host, "remote kill-guard.sh updated; running bots pick it up on their next tool call");
    }
    if !r.configs_updated.is_empty() {
        tracing::info!(host, bots = ?r.configs_updated, "rewrote remote hook settings for running bots");
    }
    for (bot, error) in &r.failed {
        tracing::warn!(host, bot = %bot, error = %error, "could not refresh this bot's remote kill-guard settings");
    }
    if !r.pending.is_empty() {
        tracing::warn!(host, bots = ?r.pending.iter().map(|p| &p.bot_name).collect::<Vec<_>>(), "these running bots loaded an older kill-guard hook config; restart them to apply");
    }
    am_base::remote_guard_pending::set_host(host, r.pending);
    Ok(())
}

/// 這台上正在跑的 claude／agy／grok bot（不含受限分享 bot：它的設定由 `cage_settings` 另外產生，留給重啟時處理）。
async fn running_bots(app: &Arc<App>, host: &str) -> anyhow::Result<Vec<GuardBot>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT r.bot_id, r.id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND r.state = 'running' AND b.deleted_at IS NULL AND p.deleted_at IS NULL
            AND b.inject_hooks != 0 AND b.kind IN ('claude','agy','grok')",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await?;
    let mut out = Vec::new();
    for (bot_id, run_id) in rows {
        let Some(bot) = crate::db::bot(&app.db, &bot_id).await? else { continue };
        if crate::db::caged_workspace(&app.db, &bot.id).await?.is_some() {
            continue;
        }
        let mut env: serde_json::Map<String, Value> = bot.env().into_iter().map(|(k, v)| (k, json!(v))).collect();
        if let Some(name) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
            if let Some(ienv) = <App as crate::trust::TrustEnv>::identity_env(app, host, name).await {
                env.extend(ienv.into_iter().map(|(k, v)| (k, json!(v))));
            }
        }
        out.push(GuardBot { bot, run_id, env: Value::Object(env) });
    }
    Ok(out)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::lifecycle::guard_refresh::GUARD_SIG_FILE;

    fn mtime(p: &std::path::Path) -> std::time::SystemTime {
        std::fs::metadata(p).unwrap().modified().unwrap()
    }

    /// 連上主機：換掉舊的 `kill-guard.sh`、改寫執行中 claude bot 的設定、標待重啟；再連一次雜湊相同什麼都不寫；
    /// 重啟過（啟動那趟寫了新簽章）就不再待重啟。
    #[tokio::test]
    async fn a_reconnect_refreshes_the_guard_and_flags_bots_that_loaded_an_old_config() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = format!("guard-{}", crate::db::ulid());
        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id).execute(&app.db).await.unwrap();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "remote-claude").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let grok = crate::testing::claude_bot(&app, &env.project_id, "remote-grok").await;
        sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&grok.id).execute(&app.db).await.unwrap();
        let grok_run = crate::testing::fake_run(&app, &grok.id).await;

        let home = crate::testing::track(std::env::temp_dir().join(format!("am-guard-{}", crate::db::ulid())));
        let root = home.join(crate::startup::REMOTE_ROOT);
        let bot_dir = root.join("bots").join(&bot.id);
        std::fs::create_dir_all(&bot_dir).unwrap();
        let kg = root.join(am_base::kill_guard::REMOTE_KILL_GUARD_SH);
        std::fs::write(&kg, "#!/bin/sh\nexit 0\n").unwrap();
        let settings = bot_dir.join("claude-settings.json");
        std::fs::write(&settings, "{\n  \"hooks\": {}\n}\n").unwrap();
        std::fs::create_dir_all(root.join("bots").join(&grok.id)).unwrap();
        let grok_hooks = home.join(".grok/hooks").join(crate::lifecycle::setup::grok_hooks_file(None));

        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "am-test".into(), ..Default::default() }).await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        let fake_home = home.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env("HOME", &fake_home).output()?;
            anyhow::ensure!(out.status.success(), "remote script failed: {}", String::from_utf8_lossy(&out.stderr));
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        });

        refresh_once(&app, &host).await.unwrap();
        assert_eq!(std::fs::read_to_string(&kg).unwrap(), am_base::kill_guard::remote_kill_guard_sh(), "kill-guard.sh 換成這顆 binary 的版本");
        let now: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let cmd = now["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap_or_default().to_string();
        assert!(cmd.contains("kill-guard.sh"), "執行中 bot 的設定檔改寫成帶守門的版本：{now}");
        let pending = |h: &str| am_base::remote_guard_pending::snapshot().into_iter().filter(|(x, _)| x == h).map(|(_, p)| (p.bot_id, p.run_id)).collect::<Vec<_>>();
        let grok_cmds = std::fs::read_to_string(&grok_hooks).unwrap();
        assert!(grok_cmds.contains("kill-guard.sh"), "grok 的全域 hooks 檔補上守門：{grok_cmds}");
        let mut want_pending = vec![(bot.id.clone(), run_id.clone()), (grok.id.clone(), grok_run.clone())];
        want_pending.sort();
        assert_eq!(pending(&host), want_pending, "啟動時載入的是舊設定（grok 的全域檔這一趟才補上）：待重啟");
        let sig = std::fs::read_to_string(bot_dir.join(GUARD_SIG_FILE)).unwrap();

        let (kg_at, settings_at, grok_at) = (mtime(&kg), mtime(&settings), mtime(&grok_hooks));
        std::thread::sleep(std::time::Duration::from_millis(20));
        refresh_once(&app, &host).await.unwrap();
        assert_eq!((mtime(&kg), mtime(&settings), mtime(&grok_hooks)), (kg_at, settings_at, grok_at), "雜湊／內容相同就不重寫");
        assert_eq!(std::fs::read_to_string(bot_dir.join(GUARD_SIG_FILE)).unwrap(), sig, "啟動簽章不被蓋掉");
        assert_eq!(pending(&host).len(), 2, "還沒重啟就一直待重啟（daemon 重啟也一樣，靠 guard.sig）");
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&grok_run).execute(&app.db).await.unwrap();

        let p = crate::lifecycle::remote_bot_dir_for(&conn, &bot.id, None).await.unwrap();
        let want = crate::lifecycle::guard_refresh::guard_sig_for("claude", &p, &bot, &app.cfg.agents_fresh().await.time_zone().to_string(), None).unwrap();
        std::fs::write(bot_dir.join(GUARD_SIG_FILE), &want).unwrap();
        refresh_once(&app, &host).await.unwrap();
        assert!(pending(&host).is_empty(), "重啟後載入的就是新版");
        let _ = std::fs::remove_dir_all(&home);
    }
}
