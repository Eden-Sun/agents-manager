//! 同一台 host 短時間內有多顆 agent 同時「不見」（#1884）。
//!
//! 對帳發現 run 的 agent 不在，只會安靜地把 run 收成 exited。單顆是常態（herdr 掉了一個 pane），
//! 但同一台 host 在幾十秒內一口氣掉三顆以上，多半是外力：某顆 bot 的 `pkill`／`killall` 範圍過大、OOM、整台重啟。
//! 這裡在每輪對帳尾端數一次，超過門檻就推一則 `ops_alert` 給巡檢（只告警，不重啟、不動 autostart 補開）。
//!
//! 計數看 DB（`runs.exit_reason`＋`ended_at`），不看記憶體：一波掉落常跨好幾輪對帳，daemon 重啟也不歸零。
//! 子 agent 不算（它們做完就關 pane，一個 parent 同時收掉幾顆是常態）。計畫中的 herdr 維護、
//! 本機 herdr 非預期重啟（`herdr_recovery` 已經自己推了一則）不重複告警。整台 host 斷線時 `snapshot` 就失敗，
//! 對帳整輪跳過，根本不會有 gone。

use crate::reconcile::ReconcileHostEnv;
use serde_json::json;

/// 往回看多久內「同時」掉的才算同一波。
pub(crate) const WINDOW_SECS: i64 = 30;
/// 窗內至少幾顆（不含子 agent）才告警。
pub(crate) const THRESHOLD: usize = 3;
/// 推過一則之後，同一台 host 這段時間內不再推：一波掉落常跨好幾輪對帳，窗口也會滑動。
const COOLDOWN_SECS: i64 = 300;
/// 讓 `herdr_recovery` 的非預期重啟告警先講：這段時間內有那則，這裡不再重複。
const RECOVERY_ALERT_SECS: i64 = 300;

const GONE_REASONS: [&str; 2] = ["agent not found during reconcile", "pane gone"];

/// 每輪對帳尾端呼叫：這一輪收了 `gone_this_pass` 顆 run 才需要看（沒有就連查都不查）。
pub(crate) async fn check(app: &impl ReconcileHostEnv, host: &str, gone_this_pass: usize) {
    if gone_this_pass == 0 {
        return;
    }
    if let Err(e) = check_inner(app, host).await {
        tracing::warn!(host, error = ?e, "reconcile: could not check for a burst of vanished agents");
    }
}

async fn check_inner(app: &impl ReconcileHostEnv, host: &str) -> anyhow::Result<()> {
    let since = crate::db::iso_in(-WINDOW_SECS);
    let reasons = serde_json::to_string(&GONE_REASONS)?;
    let gone: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT b.id, b.name, r.ended_at FROM runs r
           JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND p.handed_off_to IS NULL AND b.managed_by != 'child'
            AND r.state = 'exited' AND r.ended_at >= ?
            AND r.exit_reason IN (SELECT value FROM json_each(?))
          ORDER BY r.ended_at, r.rowid",
    )
    .bind(host)
    .bind(&since)
    .bind(&reasons)
    .fetch_all(app.db())
    .await?;
    if gone.len() < THRESHOLD {
        return Ok(());
    }
    // 計畫中的維護：herdr 重啟的那幾分鐘所有 pane 同時消失是預期的。讀不到維護狀態就當它在維護，寧可漏一次也不誤報。
    match app.herdr_maintenance_active().await {
        Ok(None) => {}
        Ok(Some(_)) => {
            tracing::info!(host, count = gone.len(), "reconcile: several agents vanished during planned herdr maintenance; no alert");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(host, error = ?e, "reconcile: cannot read the herdr maintenance state; no burst alert this pass");
            return Ok(());
        }
    }
    let recent = |secs: i64| crate::db::iso_in(-secs);
    // 本機 herdr 非預期重啟有自己的告警（`herdr_recovery`），同一件事不推兩則。
    if host == crate::config::LOCAL_HOST {
        let covered: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'ops_alert' AND event_key LIKE 'ops_alert:daemon:herdr_unexpected_restart:%' AND created_at >= ?",
        )
        .bind(recent(RECOVERY_ALERT_SECS))
        .fetch_one(app.db())
        .await?;
        if covered > 0 {
            tracing::info!(host, count = gone.len(), "reconcile: several agents vanished with a herdr restart already reported; no burst alert");
            return Ok(());
        }
    }
    // 同一波只推一則：這台 host 冷卻期內推過就不再推（key 帶 ULID 才不會被 INSERT OR IGNORE 吞掉下一波，所以冷卻要自己查）。
    let prefix = format!("ops_alert:daemon:agents_gone_burst:{host}:");
    let cooling: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'ops_alert' AND substr(event_key, 1, ?) = ? AND created_at >= ?")
        .bind(prefix.chars().count() as i64)
        .bind(&prefix)
        .bind(recent(COOLDOWN_SECS))
        .fetch_one(app.db())
        .await?;
    if cooling > 0 {
        return Ok(());
    }
    let names: Vec<&str> = gone.iter().map(|(_, name, _)| name.as_str()).collect();
    let bots: Vec<_> = gone.iter().map(|(id, name, at)| json!({"bot_id": id, "name": name, "gone_at": at})).collect();
    let first = gone.first().map(|g| g.2.as_str()).unwrap_or_default();
    let last = gone.last().map(|g| g.2.as_str()).unwrap_or_default();
    let detail = format!(
        "host `{host}` 在 {WINDOW_SECS} 秒內有 {} 顆 agent 同時不見（{}）：{}。",
        gone.len(),
        names.join("、"),
        "daemon 只把它們的 run 收成 exited，沒有重啟任何一顆",
    );
    let payload = json!({
        "source": "daemon",
        "reason": "agents_gone_burst",
        "subject": host,
        "host": host,
        "count": gone.len(),
        "bots": bots,
        "bot_names": names,
        "window_secs": WINDOW_SECS,
        "first_gone_at": first,
        "last_gone_at": last,
        "detail": detail,
        "action": "不是使用者停的、也不是計畫中的 herdr 維護：查那台 host 上有沒有範圍過大的 pkill／killall／kill -9 -1、OOM 或整台重啟（daemon.log 搜 `agent gone`）；autostart 的 bot 照原本規則補開，其餘要人手 `start?resume=native`。",
    });
    let key = format!("{prefix}{}", crate::db::ulid());
    if crate::supervisor_inbox::push_inbox(app.db(), &key, "ops_alert", None, None, None, &payload).await?.is_some() {
        tracing::warn!(host, count = gone.len(), "reconcile: several agents vanished at once; ops_alert queued");
        app.emit("supervisor_changed", json!({"ops_alert": key})).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::state::App;
    use crate::testing as tt;
    use crate::{config, db};
    use std::sync::Arc;

    /// 直接寫一顆「剛被對帳收成 exited」的 run（秒數＝多久以前）。`child`＝子 agent。
    async fn gone_bot(app: &Arc<App>, project: &str, name: &str, child: bool, ago_secs: i64) {
        let bot = tt::claude_bot(app, project, name).await;
        if child {
            sqlx::query("UPDATE bots SET managed_by = 'child' WHERE id = ?").bind(&bot.id).execute(&app.db).await.unwrap();
        }
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at, ended_at, exit_reason) VALUES (?,?,'exited','unknown',?,?,?)")
            .bind(db::ulid())
            .bind(&bot.id)
            .bind(db::iso_in(-ago_secs - 600))
            .bind(db::iso_in(-ago_secs))
            .bind("agent not found during reconcile")
            .execute(&app.db)
            .await
            .unwrap();
    }

    async fn other_host_project(app: &Arc<App>, host: &str) -> String {
        let id = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)")
            .bind(&id)
            .bind(format!("/srv/{host}"))
            .bind(host)
            .bind(host)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn alerts(app: &Arc<App>) -> Vec<serde_json::Value> {
        crate::supervisor::store::inbox(&app.db, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "ops_alert")
            .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload_json).unwrap())
            .filter(|p| p["reason"] == "agents_gone_burst")
            .collect()
    }

    #[tokio::test]
    async fn several_agents_gone_on_one_host_push_one_alert_with_who_and_how_many() {
        let env = tt::env().await;
        for n in ["a", "b", "c", "d"] {
            gone_bot(&env.app, &env.project_id, n, false, 5).await;
        }
        super::check(&env.app, config::LOCAL_HOST, 4).await;
        let a = alerts(&env.app).await;
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0]["host"], config::LOCAL_HOST);
        assert_eq!(a[0]["count"], 4);
        assert_eq!(a[0]["window_secs"], super::WINDOW_SECS);
        let mut names: Vec<String> = a[0]["bot_names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["a", "b", "c", "d"]);
    }

    #[tokio::test]
    async fn below_the_threshold_nothing_is_pushed() {
        let env = tt::env().await;
        for n in ["a", "b"] {
            gone_bot(&env.app, &env.project_id, n, false, 5).await;
        }
        super::check(&env.app, config::LOCAL_HOST, 2).await;
        assert!(alerts(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn agents_gone_outside_the_window_or_children_do_not_count() {
        let env = tt::env().await;
        gone_bot(&env.app, &env.project_id, "old", false, super::WINDOW_SECS + 60).await;
        gone_bot(&env.app, &env.project_id, "old2", false, super::WINDOW_SECS + 120).await;
        gone_bot(&env.app, &env.project_id, "kid1", true, 5).await;
        gone_bot(&env.app, &env.project_id, "kid2", true, 5).await;
        gone_bot(&env.app, &env.project_id, "fresh", false, 5).await;
        super::check(&env.app, config::LOCAL_HOST, 5).await;
        assert!(alerts(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn different_hosts_are_not_merged() {
        let env = tt::env().await;
        let other = other_host_project(&env.app, "m4p").await;
        gone_bot(&env.app, &env.project_id, "l1", false, 5).await;
        gone_bot(&env.app, &env.project_id, "l2", false, 5).await;
        gone_bot(&env.app, &other, "r1", false, 5).await;
        super::check(&env.app, config::LOCAL_HOST, 2).await;
        super::check(&env.app, "m4p", 1).await;
        assert!(alerts(&env.app).await.is_empty(), "2＋1 跨 host 不合併");
        gone_bot(&env.app, &other, "r2", false, 5).await;
        gone_bot(&env.app, &other, "r3", false, 5).await;
        super::check(&env.app, "m4p", 2).await;
        let a = alerts(&env.app).await;
        assert_eq!((a.len(), a[0]["host"].as_str(), a[0]["count"].as_u64()), (1, Some("m4p"), Some(3)));
    }

    #[tokio::test]
    async fn the_same_wave_is_pushed_once_even_across_passes() {
        let env = tt::env().await;
        for n in ["a", "b", "c"] {
            gone_bot(&env.app, &env.project_id, n, false, 5).await;
        }
        super::check(&env.app, config::LOCAL_HOST, 3).await;
        gone_bot(&env.app, &env.project_id, "late", false, 1).await;
        super::check(&env.app, config::LOCAL_HOST, 1).await;
        super::check(&env.app, config::LOCAL_HOST, 3).await;
        assert_eq!(alerts(&env.app).await.len(), 1);
    }

    #[tokio::test]
    async fn a_pass_that_ended_nothing_does_not_even_look() {
        let env = tt::env().await;
        for n in ["a", "b", "c"] {
            gone_bot(&env.app, &env.project_id, n, false, 5).await;
        }
        super::check(&env.app, config::LOCAL_HOST, 0).await;
        assert!(alerts(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn planned_herdr_maintenance_is_not_reported() {
        let env = tt::env().await;
        for n in ["a", "b", "c"] {
            gone_bot(&env.app, &env.project_id, n, false, 5).await;
        }
        sqlx::query("INSERT OR REPLACE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,'2999-01-01T00:00:00.000Z','patrol','test')")
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        super::check(&env.app, config::LOCAL_HOST, 3).await;
        assert!(alerts(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn a_herdr_restart_already_reported_by_recovery_is_not_reported_twice() {
        let env = tt::env().await;
        for n in ["a", "b", "c"] {
            gone_bot(&env.app, &env.project_id, n, false, 5).await;
        }
        crate::supervisor_inbox::push_inbox(&env.app.db, "ops_alert:daemon:herdr_unexpected_restart:r1", "ops_alert", None, None, None, &serde_json::json!({"reason": "herdr_unexpected_restart"}))
            .await
            .unwrap();
        super::check(&env.app, config::LOCAL_HOST, 3).await;
        assert!(alerts(&env.app).await.is_empty());
    }

    /// 端到端：herdr 把所有 agent 一起忘掉，真的跑一輪對帳，就推一則。
    #[tokio::test]
    async fn a_reconcile_pass_that_loses_many_agents_pushes_one_alert() {
        let env = tt::env().await;
        for n in ["a", "b", "c"] {
            let bot = tt::claude_bot(&env.app, &env.project_id, n).await;
            crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        }
        env.herdr.agents.lock().unwrap().clear();
        env.herdr.workspaces.lock().unwrap().clear();
        env.herdr.tabs.lock().unwrap().clear();
        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();
        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();
        let a = alerts(&env.app).await;
        assert_eq!((a.len(), a[0]["count"].as_u64()), (1, Some(3)), "{a:?}");
    }
}
