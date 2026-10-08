//! Quota runners and background pollers / limit hit checks for agents-managerd.

use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use serde_json::json;

use crate::app_ports_r2a9::OwedLimitHitProbe;
use crate::quota::{
    billing_identity, clear_limit_hit, cleared_at, cleared_at_key,
    for_each_host, keys_for_bot, limit_hit_blocks_model, limit_hit_expired,
    pane_window_used, parse_utc, pollable_hosts, quota_base_for_host,
    quota_from_codex, quota_from_codex_status, quota_key, running_model,
    set_fenced, status_line_sighting, LimitHit,
};
use crate::state::App;

pub const CODEX_POLL: Duration = Duration::from_secs(300);
pub const CODEX_PANE_POLL: Duration = Duration::from_secs(3);

/// 擋住這顆 bot 的撞限：沒過期、而且撞的那一桶管得到它在跑的模型（[`limit_hit_blocks_model`]）。
///
/// 讀不到這顆 bot 在哪台主機就回錯（#108 重開）：以前退回 `local`，查錯 key、回「沒撞限」，遠端那個已經用盡的身分
/// 就被放行。撞限記不進正確那把 key 而欠著的那一筆（`turn_error::owed_limit_hit`）先算。
pub async fn try_limit_hit_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Result<Option<LimitHit>> {
    let identity = billing_identity(app, bot).await?;
    if let Some(hit) = app.owed_limit_hit(bot, identity.as_deref()).await {
        return Ok(Some(hit));
    }
    let host = crate::db::bot_host(&app.db, &bot.id).await?;
    let keys = keys_for_bot(&**app, &host, bot, identity.as_deref()).await;
    let model = running_model(app, bot).await;
    let q = app.quotas.lock().await;
    for k in keys {
        if let Some(hit) = q.get(&k).and_then(|x| x.limit_hit.clone()) {
            if !limit_hit_expired(Some(&hit)) && limit_hit_blocks_model(&hit, model.as_deref()) {
                return Ok(Some(hit));
            }
        }
    }
    Ok(None)
}

/// [`try_limit_hit_for_bot`]，讀不到時回 `None`（記 warn）。只剩 supervisor 的派送／重送在用：那邊拿到撞限會 park、
/// 群組任務還會換身分，不能拿假的撞限去擋；改用 `try_` 版、讀不到就延後，見 #108 重開時開的 supervisor 票。
#[cfg(test)]
pub async fn limit_hit_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Option<LimitHit> {
    match try_limit_hit_for_bot(app, bot).await {
        Ok(hit) => hit,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot tell whether this bot's identity has hit its limit");
            None
        }
    }
}

/// 只回未來的重置時間；CLI 橫幅時間會舊，supervisor 要兩邊都看（2026-09-13：橫幅 22:15、app-server 22:20）。
/// 讀不到主機回 `None`（沒有這份證據，只看橫幅的時間）：退回 `local` 會拿到本機帳號的重置時間，把重送提早。
pub async fn next_reset_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Option<String> {
    let host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the host of a bot; no quota reset time from its readings");
            return None;
        }
    };
    let identity = match billing_identity(app, bot).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the run of a bot; no quota reset time from its readings");
            return None;
        }
    };
    let now = chrono::Utc::now();
    let keys = keys_for_bot(&**app, &host, bot, identity.as_deref()).await;
    let future = |t: &Option<String>| t.as_deref().and_then(parse_utc).filter(|x| *x > now);
    let q = app.quotas.lock().await;
    for k in keys {
        let Some(entry) = q.get(&k) else { continue };
        let candidates = [
            entry.five_hour.as_ref().and_then(|w| future(&w.resets_at)),
            entry.seven_day.as_ref().and_then(|w| future(&w.resets_at)),
        ];
        if let Some(t) = candidates.into_iter().flatten().min() {
            return Some(crate::db::iso_at(t));
        }
    }
    None
}

/// 這顆 bot 真的答完一回合：清掉**它自己那把 key** 的撞限。key 跟寫入端（`apply_codex_limit_hit_quota`）
/// 與查詢端（[`limit_hit_for_bot`]）走同一支 [`quota_base_for_host`]——以前寫死裸 `codex`，有自己
/// `CODEX_HOME` 的 `cx2` 一撞限就永遠清不掉，反而把預設帳號真的撞限清掉（review 2026-09-16 H1）。
///
/// 讀不到主機就不清（#108 重開）：退回 `local` 會把**本機**那個身分真的撞限清掉。少清一次只是多擋到到期。
pub async fn clear_limit_hit_for_bot(app: &Arc<App>, bot: &crate::db::Bot) {
    let host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the host of a bot; its limit hit is left in place");
            return;
        }
    };
    // 清的是答完這一回合的那個帳號（issue #238）：run 起來時的身分，不是剛改、還沒生效的設定。
    let identity = match billing_identity(app, bot).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the run of a bot; its limit hit is left in place");
            return;
        }
    };
    let base = quota_base_for_host(&**app, &host, &bot.kind, identity.as_deref()).await;
    clear_limit_hit(app, &host, &base).await;
    crate::judge::note_cleared(&app.db, &bot.id).await;
}

/// 這顆 bot 的帳號在 `since` 之後有沒有被成功回合清過撞限。`resume_quota_blocked` 用它分辨
/// 「記憶體裡沒有撞限是因為真的被清掉了」與「只是重啟後什麼都不記得」（review 2026-09-16 M1）。
/// 讀不到主機回 `false`（沒有證據說額度回來了）：退回 `local` 會拿本機帳號的成功回合當作這顆的放行證據。
pub async fn limit_cleared_since(app: &Arc<App>, bot: &crate::db::Bot, since: chrono::DateTime<chrono::Utc>) -> bool {
    let host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the host of a bot; no evidence its limit was cleared");
            return false;
        }
    };
    let identity = match billing_identity(app, bot).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the run of a bot; no evidence its limit was cleared");
            return false;
        }
    };
    let keys = keys_for_bot(&**app, &host, bot, identity.as_deref()).await;
    let m = cleared_at().lock().unwrap();
    keys.iter().any(|k| m.get(&cleared_at_key(app, k)).is_some_and(|t| *t > since))
}

/// app-server 讀數會落後 CLI 一整輪（2026-09-13 使用者截圖：量表 5h 100、pane 90% left），CLI 狀態列才是它當下擋你的
/// 依據。與 app-server 共用同一格；什麼時候採用 pane 上的數字見 [`pane_window_used`]。
pub async fn refresh_codex_from_panes(app: &Arc<App>, host: &str) -> usize {
    // 讀 pane 要好幾趟 RPC：途中同名主機被換掉，舊機器 pane 上的讀數不能寫進新機器的 key（#347），所以先記下權威。
    let Some(fence) = app.hosts.fence(host).await else { return 0 };
    let rows: Vec<(String, Option<String>, Option<String>)> = match sqlx::query_as(
        // 最近有動靜的 pane 排前面：它的狀態列最新。閒著的 pane 也會刷新，但剛跑完回合的那顆最準。
        // 第三欄是那顆 pane 畫面的年紀：最後一回合結束（或開始）的時間，沒有回合就是 run 起來的時間。
        // run 實際的身分（issue #238）：記了就用它，沒記才用 bot 設定的。
        "SELECT r.pane_id, CASE WHEN r.runtime_identity IS NULL THEN b.identity ELSE NULLIF(TRIM(r.runtime_identity), '') END,
                COALESCE((SELECT MAX(COALESCE(t.completed_at, t.created_at)) FROM turns t WHERE t.run_id = r.id), r.started_at)
           FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND b.kind = 'codex' AND r.state = 'running' AND r.pane_id IS NOT NULL
            AND b.deleted_at IS NULL AND p.handed_off_to IS NULL
          ORDER BY COALESCE((SELECT MAX(t.created_at) FROM turns t WHERE t.run_id = r.id), r.started_at) DESC",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(host, error = ?e, "codex statusline quota: query failed");
            return 0;
        }
    };
    let mut wrote = 0;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (pane_id, identity, screen_at) in rows {
        let base = quota_base_for_host(&**app, host, "codex", identity.as_deref()).await;
        // 同一個身分讀到一次就夠——但要「讀到」才算：那顆 pane 正在壓縮對話、捲動中讀不到狀態列時，
        // 換同帳號的下一顆，而不是整個帳號這輪都停在 app-server 落後的數字（2026-09-15）。
        if seen.contains(&base) {
            continue;
        }
        let client = fence.conn().client.clone();
        let Ok(read) = client.pane_read(&pane_id, "visible", 60).await else { continue };
        let Some(parsed) = crate::codex_live::parse_status_quota(&read.text) else { continue };
        seen.insert(base.clone());
        let reading = format!("{:?}/{:?}", parsed.five_hour_left, parsed.weekly_left);
        let sighting = status_line_sighting(host, &pane_id, &reading).await;
        let stored = app.quotas.lock().await.get(&quota_key(host, &base)).cloned();
        let screen_at = screen_at.as_deref().and_then(parse_utc);
        let now = chrono::Utc::now();
        let used = |left: Option<f64>| left.map(|l| (100.0 - l).clamp(0.0, 100.0));
        let five = pane_window_used(used(parsed.five_hour_left), stored.as_ref().and_then(|q| q.five_hour.as_ref()), chrono::Duration::hours(5), screen_at, sighting, now);
        let weekly = pane_window_used(used(parsed.weekly_left), stored.as_ref().and_then(|q| q.seven_day.as_ref()), chrono::Duration::days(7), screen_at, sighting, now);
        let status = crate::codex_live::CodexStatusQuota { five_hour_left: five.map(|u| 100.0 - u), weekly_left: weekly.map(|u| 100.0 - u) };
        let Some(q) = quota_from_codex_status(&status, identity.as_deref()) else { continue };
        #[cfg(test)]
        crate::race_point::hit("codex_panes_before_set", host).await;
        if let Err(e) = set_fenced(app, host, &base, q, &fence).await {
            tracing::debug!(host, error = %e, "codex statusline quota: host superseded; dropping this round");
            break;
        }
        wrote += 1;
    }
    wrote
}

/// `Ok(false)` = codex not installed there (quota stays null).
pub async fn refresh_codex(app: &Arc<App>, host: &str) -> Result<bool> {
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    let r = crate::runners::models::codex_rpc(app, host, "account/rateLimits/read", json!({})).await;
    let r = match r {
        Ok(v) => v,
        Err(e) if e.to_string().contains("is not installed") => return Ok(false),
        Err(e) => return Err(e),
    };
    match quota_from_codex(&r) {
        Some(q) => {
            set_fenced(app, host, "codex", q, &fence).await?;
            Ok(true)
        }
        None => anyhow::bail!("unexpected rateLimits shape: {r}"),
    }
}

pub fn spawn_codex_poller(app: Arc<App>) {
    crate::background_loop::spawn_restartable(&app, "codex quota poller", {
        let app = app.clone();
        move || {
            let app = app.clone();
            async move {
        let shutdown = app.shutdown.clone();
        let mut last_server: Option<std::time::Instant> = None;
        loop {
            // app-server 每 CODEX_POLL 問一次；狀態列每 CODEX_PANE_POLL 讀一次。同一輪兩個都做時先問 app-server，
            // 狀態列後到蓋前（CLI 狀態列較即時且分得出身分）。
            let ask_server = last_server.map_or(true, |t| t.elapsed() >= CODEX_POLL);
            if ask_server {
                last_server = Some(std::time::Instant::now());
            }
            for_each_host(pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
                    if ask_server {
                        match refresh_codex(&app, &host).await {
                            Ok(true) => {}
                            Ok(false) => tracing::info!(host = %host, "codex not installed; codex quota stays null"),
                            Err(e) => tracing::warn!(host = %host, error = %e, "codex quota refresh failed"),
                        }
                    }
                    let n = refresh_codex_from_panes(&app, &host).await;
                    if n > 0 {
                        tracing::debug!(host = %host, panes = n, "codex quota read off the status line");
                    }
                }
            })
            .await;
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(CODEX_PANE_POLL) => {}
            }
        }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LOCAL_HOST;
    use crate::quota::{seed_limit_hit, set, Window};

    fn codex_q(source: &str, limit_hit: Option<LimitHit>) -> crate::quota::Quota {
        crate::quota::Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit,
            plan: None,
            updated_at: crate::db::now(),
            source: source.into(),
            account: None,
            host: LOCAL_HOST.into(),
        }
    }

    async fn remote_bot_beside_a_local_hit(app: &Arc<App>) -> crate::db::Bot {
        let pid = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
            .bind(&pid)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let bot = crate::testing::claude_bot(app, &pid, "far").await;
        let later = |h: i64| crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(h));
        let mut local = codex_q("statusline", None);
        local.five_hour = Some(Window { observed_at: None, used_pct: 40.0, resets_at: Some(later(1)) });
        set(app, LOCAL_HOST, "claude", local).await;
        assert!(seed_limit_hit(app, LOCAL_HOST, "claude", &later(2), "You've hit your session limit", Some("five_hour".into())).await);
        bot
    }

    async fn projects_unreadable(app: &Arc<App>, unreadable: bool) {
        let sql = if unreadable { "ALTER TABLE projects RENAME TO projects_unreadable" } else { "ALTER TABLE projects_unreadable RENAME TO projects" };
        sqlx::query(sql).execute(&app.db).await.unwrap();
    }

    #[tokio::test]
    async fn a_bot_whose_host_cannot_be_read_is_never_read_or_cleared_on_the_local_key() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let bot = remote_bot_beside_a_local_hit(&app).await;
        let since = chrono::Utc::now() - chrono::Duration::seconds(1);

        projects_unreadable(&app, true).await;
        assert!(try_limit_hit_for_bot(&app, &bot).await.is_err(), "讀不到主機是錯，不是「沒撞限」");
        assert_eq!(next_reset_for_bot(&app, &bot).await, None, "讀不到主機：不交出本機帳號的重置時間");
        clear_limit_hit_for_bot(&app, &bot).await;
        assert!(!limit_cleared_since(&app, &bot, since).await, "讀不到主機：不拿本機帳號的成功回合當證據");

        projects_unreadable(&app, false).await;
        let local_hit = app.quotas.lock().await["claude"].limit_hit.clone();
        assert!(local_hit.is_some(), "剛才那次清撞限沒有清到本機那把 key");
        assert_eq!(try_limit_hit_for_bot(&app, &bot).await.unwrap(), None, "讀得到：看自己那把 `remote1/claude`");
    }

    #[tokio::test]
    async fn codex_statusline_reading_updates_the_window() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();
        let codex = |name: &'static str| {
            let app = app.clone();
            let pid = pid.clone();
            async move {
                let id = crate::db::ulid();
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
                     VALUES (?,?,?,'codex','[]',0,1,'tok','user',?)",
                )
                .bind(&id)
                .bind(&pid)
                .bind(name)
                .bind(crate::db::now())
                .execute(&app.db)
                .await
                .unwrap();
                let run = crate::testing::fake_run(&app, &id).await;
                (id, run)
            }
        };
        let (_old_bot, old_run) = codex("idle-old").await;
        let (fresh_bot, fresh_run) = codex("busy-fresh").await;
        let turn = |run: String, bot: String, at: &'static str| {
            let app = app.clone();
            async move {
                let conv = crate::db::conversation_id(&app.db, &bot).await.unwrap();
                sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, created_at) VALUES (?,?,?,'web','completed',?)")
                    .bind(crate::db::ulid())
                    .bind(conv)
                    .bind(run)
                    .bind(at)
                    .execute(&app.db)
                    .await
                    .unwrap();
            }
        };
        turn(old_run.clone(), _old_bot.clone(), "2026-09-15T03:00:00Z").await;
        turn(fresh_run.clone(), fresh_bot.clone(), "2026-09-15T09:00:00Z").await;
        let pane = |run: &str| futures::executor::block_on(crate::db::run(&app.db, run)).unwrap().unwrap().pane_id.unwrap();
        let (old_pane, fresh_pane) = (pane(&old_run), pane(&fresh_run));
        let line = |five: u32| format!("\n› Ask Codex\n  gpt-6-astra low · /tmp · Context 20% used · 5h {five}% left · weekly 65% left\n");

        env.herdr.screens.lock().unwrap().insert(old_pane.clone(), line(100));
        env.herdr.screens.lock().unwrap().insert(fresh_pane.clone(), line(93));
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 1);
        let used = |app: Arc<App>| async move { app.quotas.lock().await.get("codex").unwrap().five_hour.clone().unwrap().used_pct };
        assert_eq!(used(app.clone()).await, 7.0, "93% left 那顆較新");

        env.herdr.screens.lock().unwrap().insert(fresh_pane.clone(), "• Compacting context (1m 17s • esc to interrupt)\n".into());
        env.herdr.screens.lock().unwrap().insert(old_pane.clone(), line(88));
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 1);
        assert_eq!(used(app.clone()).await, 12.0);

        let before = app.quotas.lock().await.get("codex").unwrap().updated_at.clone();
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 0, "畫面沒變就不算一次讀數");
        assert_eq!(app.quotas.lock().await.get("codex").unwrap().updated_at, before);

        env.herdr.screens.lock().unwrap().insert(old_pane, line(70));
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 1);
        assert_eq!(used(app.clone()).await, 30.0);
    }

    #[tokio::test]
    async fn an_idle_codex_pane_corrects_a_lagging_server_but_not_a_newer_window() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,'cx','codex','[]',0,1,'tok','user',?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = crate::testing::fake_run(&app, &bot).await;
        let now = chrono::Utc::now();
        let turn_done = |ago: chrono::Duration| {
            let app = app.clone();
            let (run, bot) = (run.clone(), bot.clone());
            async move {
                sqlx::query("DELETE FROM turns WHERE run_id=?").bind(&run).execute(&app.db).await.unwrap();
                let conv = crate::db::conversation_id(&app.db, &bot).await.unwrap();
                let t = crate::db::iso_at(now - ago);
                sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, created_at, completed_at) VALUES (?,?,?,'web','completed',?,?)")
                    .bind(crate::db::ulid())
                    .bind(conv)
                    .bind(&run)
                    .bind(&t)
                    .bind(&t)
                    .execute(&app.db)
                    .await
                    .unwrap();
            }
        };
        let pane = crate::db::run(&app.db, &run).await.unwrap().unwrap().pane_id.unwrap();
        let screen = |left: u32| format!("\n› Ask Codex\n  gpt-6-astra low · /tmp · Context 20% used · 5h {left}% left · weekly 65% left\n");
        let server = |used: f64, resets: chrono::DateTime<chrono::Utc>| {
            let mut q = codex_q("codex-app-server", None);
            q.five_hour = Some(Window { observed_at: None, used_pct: used, resets_at: Some(crate::db::iso_at(resets)) });
            q.seven_day = Some(Window { observed_at: None, used_pct: 35.0, resets_at: Some(crate::db::iso_at(now + chrono::Duration::days(3))) });
            q
        };
        let five_used = |app: Arc<App>| async move { app.quotas.lock().await.get("codex").unwrap().five_hour.clone().unwrap().used_pct };

        turn_done(chrono::Duration::minutes(10)).await;
        set(&app, LOCAL_HOST, "codex", server(0.0, now + chrono::Duration::hours(4))).await;
        env.herdr.screens.lock().unwrap().insert(pane.clone(), screen(90));
        refresh_codex_from_panes(&app, LOCAL_HOST).await;
        assert_eq!(five_used(app.clone()).await, 10.0);

        set(&app, LOCAL_HOST, "codex", server(0.0, now + chrono::Duration::hours(4))).await;
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 1);
        assert_eq!(five_used(app.clone()).await, 10.0, "落後的 app-server 不能讓量表停在偏滿");
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 0);

        turn_done(chrono::Duration::hours(3)).await;
        set(&app, LOCAL_HOST, "codex", server(5.0, now + chrono::Duration::hours(4))).await;
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 0);
        assert_eq!(five_used(app.clone()).await, 5.0, "重置之前的畫面不能把量表蓋回去");
    }

    #[tokio::test]
    async fn a_codex_pane_reading_from_a_superseded_host_is_not_published() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = format!("cx-{}", crate::db::ulid());
        let cfg = |ssh: &str| crate::config::HostCfg { name: host.clone(), ssh: ssh.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let herdr_a = crate::testing::MockHerdr::start(env.dir.join("herdr-cx-a.sock"));
        app.hosts.insert_remote_with_client_for_test(cfg("target-a"), crate::herdr::HerdrClient::new(env.dir.join("herdr-cx-a.sock"))).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id).execute(&app.db).await.unwrap();
        let bot = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,'cx','codex','[]',0,1,'tok','user',?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = crate::testing::fake_run(&app, &bot).await;
        let pane = crate::db::run(&app.db, &run).await.unwrap().unwrap().pane_id.unwrap();
        herdr_a.screens.lock().unwrap().insert(pane, "\n› Ask Codex\n  gpt-6-astra low · /tmp · Context 20% used · 5h 90% left · weekly 65% left\n".into());

        let (a2, cfg_b) = (app.clone(), cfg("target-b"));
        crate::lifecycle::race_point::arm("codex_panes_before_set", &host, move || async move {
            a2.hosts.replace_remote_for_test(&a2, cfg_b).await;
        });
        assert_eq!(refresh_codex_from_panes(&app, &host).await, 0, "換掉的主機上的讀數不算寫入");
        assert!(app.quotas.lock().await.get(&format!("{host}/codex")).is_none(), "舊機器的讀數不能種進新機器的 key");
    }

    #[tokio::test]
    async fn snapshot_covers_live_hosts_only() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-quota-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::app_ports_p1::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        let app = App::new(
            pool,
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
            false,
        );
        let q = crate::quota::Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 10.0, resets_at: None }),
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: "local".into(),
        };
        crate::quota::set(&app, LOCAL_HOST, "claude", q.clone()).await;
        crate::quota::set(&app, "other", "claude", q).await;
        let snap = crate::quota::snapshot(&app).await;
        assert!(snap["kinds"].get("claude").is_some());
        assert!(snap["kinds"].get("other/claude").is_none());
    }

    #[tokio::test]
    async fn a_model_bucket_hit_holds_a_bot_whose_running_model_cannot_be_read() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let bot = crate::testing::claude_bot(&app, &env_.project_id, "switched").await;
        sqlx::query("UPDATE bots SET model='opus' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        let run = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model='fable' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let until = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2));
        assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &until, "You've hit your Fable limit", Some("fable".into())).await);
        assert!(try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "前提：它實際在跑 fable");

        sqlx::query("ALTER TABLE runs RENAME TO runs_unreadable").execute(&app.db).await.unwrap();
        assert_eq!(running_model(&app, &bot).await, None, "不知道，不是設定值的 opus");
        let got = try_limit_hit_for_bot(&app, &bot).await;
        assert!(!matches!(got, Ok(None)), "不知道在跑什麼：照擋，不是沒撞限：{got:?}");
        sqlx::query("ALTER TABLE runs_unreadable RENAME TO runs").execute(&app.db).await.unwrap();
    }
}
