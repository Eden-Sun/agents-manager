//! claude 更新完只在 pane 底下印 `Update installed · Restart to update`，沒有事件；巡邏讀出來寫進
//! `runs.update_notice` 讓 web 畫重啟徽章。掛 run 不掛 bot：更新屬於這個 process，重啟後自然清空。
//! 掃所有 `running`（不只停著的）：使用者再送話 run 變 working，通知仍該看得見。
//! codex 也巡（issue #388）：它的新版**還沒安裝**，認法與文字見 [`crate::codex_update`]；claude 的批次重啟不收它。

use crate::db;
use crate::state::App;
use crate::tui_prompts::update_notice;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 每個 claude pane 都要一次 `pane.read`，所以比問卷那支（10 秒）鬆。
const SWEEP: Duration = Duration::from_secs(30);

/// `--version` 是 process spawn（遠端是 ssh），每輪跑太兇；晚幾分鐘亮徽章沒差。
const DISK_VERSION_TTL: Duration = Duration::from_secs(300);

/// 項目鍵是 `<kind>@<host>`，值還綁住讀取時的 HostFence；同名主機換代後，舊 observation 不能命中。
pub(crate) struct DiskVersionEntry {
    at: Instant,
    authority: crate::hosts::HostAuthorityKey,
    version: Option<String>,
}

struct DiskVersionObservation {
    version: Option<String>,
    fence: crate::hosts::HostFence,
}

/// 掛在 `App.disk_versions`，不放 process 全域：測試各自一個 App，別的測試裝完版本時的
/// [`forget_disk_version`] 不會清掉這個測試種的版本、害它改去問本機真的 `codex --version`（issue #759）。
pub(crate) type DiskCache = tokio::sync::Mutex<HashMap<String, DiskVersionEntry>>;

async fn disk_version(app: &Arc<App>, host: &str, kind: &str) -> Option<DiskVersionObservation> {
    let fence = app.hosts.fence(host).await?;
    let key = format!("{kind}@{host}");
    let mut cache = app.disk_versions.lock().await;
    let cached = cache.get(&key).and_then(|entry| {
        (entry.authority.matches(&fence) && entry.at.elapsed() < DISK_VERSION_TTL).then(|| entry.version.clone())
    });
    if let Some(version) = cached {
        if !app.hosts.is_current(&fence).await {
            return None;
        }
        return Some(DiskVersionObservation { version, fence });
    }
    // A cache row from a prior connection generation is not evidence for this host name anymore.
    cache.remove(&key);
    let v = crate::changelog::installed_version(app, host, kind).await.ok();
    // The read may have crossed a reconnect or repoint. Discard it instead of publishing it under the new name.
    if !app.hosts.is_current(&fence).await {
        return None;
    }
    cache.insert(key, DiskVersionEntry { at: Instant::now(), authority: fence.authority_key(), version: v.clone() });
    Some(DiskVersionObservation { version: v, fence })
}

/// 剛在這台裝過新版（`cli_update`）：丟掉快取，下一輪巡邏重讀，不要拿五分鐘前的舊版本把通知改回「需安裝」。
pub(crate) async fn forget_disk_version(app: &App, host: &str, kind: &str) {
    app.disk_versions.lock().await.remove(&format!("{kind}@{host}"));
}

/// statusLine 回報的 process 版本（`runs.status_json.version`）。**跑著的**版本，不是磁碟上的；
/// 插隊送出的版本閘門（issue #103，`lifecycle::send_now`）也讀這一支。
pub(crate) fn running_version(status_json: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(status_json?).ok()?;
    crate::changelog::version_string(v.get("version")?.as_str()?)
}

/// 磁碟比跑著的新 = 重啟就換版。畫面那句會被推掉而漏報（2026-09-12 使用者實測 2.1.267 vs 2.1.269）。
fn version_notice(disk: &str, running: &str) -> Option<String> {
    let (d, r) = (crate::changelog::parse_version(disk)?, crate::changelog::parse_version(running)?);
    (d > r).then(|| format!("磁碟上已是 {disk}（這個 run 跑的是 {running}）· 重啟套用"))
}

/// 行程級（static）的 per-run／per-bot 記憶體帳：run 或 bot 結束後沒人會再回頭清它們，長跑的 daemon 裡只增不減。
/// 這一輪的 active run 名單已經在手，順手把不在名單上的帶走。測試版不呼叫：帳是全域的，平行的測試會互相清掉對方剛記的東西
/// （每個模組的 `retain_*` 本身各有單元測試）。
#[cfg(not(test))]
async fn prune_process_state(app: &Arc<App>, active_runs: &[String]) {
    crate::lifecycle::retain_pane_typed(active_runs);
    crate::tui_prompts::retain_survey_runs(app, active_runs).await;
    crate::codex_model_migration::retain_runs(active_runs);
    crate::blocked_reason::retain_runs(active_runs);
    // 還要留著 per-bot 帳的 bot：讀不到就這一輪不清（把讀失敗當成「沒有 bot」會清光）。
    let live_bots: Vec<String> = match live_bot_ids(app).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = ?e, "could not list live bots; per-bot process state not pruned this round");
            return;
        }
    };
    crate::pane_identity::retain_bots(&live_bots);
    crate::lifecycle::retain_bot_state(&live_bots);
    crate::child_alerts::retain_bots(&live_bots);
    app.retain_bot_locks(&live_bots).await;
}

/// 軟刪（含 child 退役）之後 per-bot 行程帳多留多久：退役的 child 常是 herdr 重啟後 reconcile 暫時收掉的，父 bot 會用 herdr 重開、
/// 復原（SPEC §6.5a）；這段時間帳清掉的話，復原後還停在同一個 blocked 問題會被再通知父 bot 一次。
const RETIRED_KEEP_SECS: i64 = 30 * 60;

/// per-bot 行程帳（`retain_bot_state`、child 通知指紋、per-bot 鎖…）要留著的 bot：
/// - 沒刪掉的；
/// - **軟刪了但還有 active run** 的：`delete_bot` 先定案 `deleted_at`、再停機，停機那幾秒 bot 還在用這些帳（欠著的收尾寫入、中斷標記…），
///   run 結束後下一輪才清；
/// - 剛軟刪／退役不久（[`RETIRED_KEEP_SECS`]）的：可能馬上復原。
pub(crate) async fn live_bot_ids(app: &Arc<App>) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM bots
          WHERE deleted_at IS NULL
             OR deleted_at >= ?
             OR EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = bots.id AND r.state IN ('starting','running','stopping'))",
    )
    .bind(db::iso_in(-RETIRED_KEEP_SECS))
    .fetch_all(&app.db)
    .await?)
}

async fn sweep(app: &Arc<App>) {
    sweep_runs(app, db::all_active_runs(&app.db).await).await;
}

/// `runs` 是這一輪列舉 active run 的結果。讀失敗要整輪跳過、不動任何 retain 狀態（#744）：
/// 把讀不到當成「沒有 active run」會清掉所有基準與背景工作帳，下一輪現有的 run 又被當成第一次看到。
async fn sweep_runs(app: &Arc<App>, runs: anyhow::Result<Vec<db::Run>>) {
    let runs = match runs {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(error = %e, "could not list active runs, skipping this sweep");
            return;
        }
    };
    let active: Vec<String> = runs.iter().map(|r| r.id.clone()).collect();
    crate::background_jobs::retain_runs(app, &active);
    crate::claude_live::retain_runs(&active);
    // 帳是全域的：測試各自種 run、平行跑 sweep，會互相清掉對方剛記的版本，所以測試版不呼叫（`retain_runs` 本身有單元測試）。
    #[cfg(not(test))]
    {
        crate::codex_update::retain_runs(&active);
        prune_process_state(app, &active).await;
    }
    for run in runs.into_iter().filter(|r| r.state == "running") {
        let kind = match db::bot(&app.db, &run.bot_id).await {
            Ok(Some(b)) if b.kind == "claude" || b.kind == "codex" => b.kind,
            _ => continue,
        };
        let Some(pane) = run.pane_id.clone() else { continue };
        let Some(client) = app.herdr_for_run(&run).await else { continue };
        // 讀不到畫面就跳過，不要把已經看到的通知清掉。
        let Ok(read) = client.pane_read(&pane, "visible", 80).await else { continue };
        // #714：同一份畫面順便看底部標的背景工作數（不另開輪詢）。
        crate::background_jobs::observe(app, &run, &kind, &read.text, &client, &pane).await;
        if kind == "claude" {
            // 同一份畫面順便看 `/model`、`/effort` 的確認行（沒掛 hook 的子 agent 只有這個訊號）。
            crate::claude_live::observe(app, &run, &read.text).await;
        }
        if kind == "codex" {
            // 狀態列是 runtime 的權威，每輪校正（讀不到就不動）。
            crate::codex_live::sync_runtime(app, &client, &run.bot_id, &run.id, &pane).await;
        }
        let observation_fence;
        let seen = if kind == "codex" {
            // 讀不到 host 就整輪跳過、不動既有通知（#243 的同一條理由）。
            let Ok(host) = db::bot_host(&app.db, &run.bot_id).await else { continue };
            let prompt = crate::codex_update::parse_prompt(&read.text);
            let running = crate::codex_update::remember_running(&run.id, &read.text, prompt.as_ref());
            let Some(disk) = disk_version(app, &host, "codex").await else { continue };
            observation_fence = Some(disk.fence);
            // 上游最新版：分診帳本（`release-triage-kick` 抓 releases 寫的，跟主機無關）。issue #561。
            let upstream = crate::release_triage::ledger::max_version(&app.db, "codex").await.ok().flatten();
            crate::codex_update::decide(prompt.as_ref(), running.as_deref(), disk.version.as_deref(), upstream.as_deref(), run.update_notice.as_deref())
        } else {
            // Claude 關閉自動更新時不能等 CLI 自己下載。上游快照有新版本就把指定目標寫進 run，
            // 讓 header 的安裝提示持續存在；磁碟追上後改回既有的「重啟套用」通知。
            let screen_notice = update_notice(&read.text);
            let Ok(host) = db::bot_host(&app.db, &run.bot_id).await else {
                continue;
            };
            let Some(disk) = disk_version(app, &host, "claude").await else {
                continue;
            };
            observation_fence = Some(disk.fence);
            let running = running_version(run.status_json.as_deref());
            let upstream = crate::upstream_update::latest_target_for_host(app, "claude", &host).await;
            let disk_version = disk.version.as_deref();
            let previous_installed_notice = run
                .update_notice
                .as_deref()
                .filter(|notice| {
                    crate::upstream_update::claude_installed_to(notice).is_some_and(|installed| {
                        disk_version
                            .and_then(crate::changelog::parse_version)
                            .zip(crate::changelog::parse_version(&installed))
                            .is_none_or(|(disk, installed)| disk >= installed)
                    })
                })
                .map(str::to_owned);
            let below_upstream = upstream.as_deref().filter(|target| {
                disk_version
                    .and_then(crate::changelog::parse_version)
                    .zip(crate::changelog::parse_version(target))
                    .is_none_or(|(disk, target)| disk < target)
            });
            if let Some(target) = below_upstream {
                Some(crate::upstream_update::claude_pending_text(
                    disk_version.or(running.as_deref()),
                    target,
                ))
            } else if upstream.is_none() {
                // 上游抓不到時保留已存在的安裝提示；未知不能當成「沒有更新」。磁碟已追上舊目標則交給下方版本比對。
                let old_target = run
                    .update_notice
                    .as_deref()
                    .and_then(crate::upstream_update::claude_pending_to);
                let old_still_pending = old_target.as_deref().is_some_and(|target| {
                    disk_version
                        .and_then(crate::changelog::parse_version)
                        .zip(crate::changelog::parse_version(target))
                        .is_none_or(|(disk, target)| disk < target)
                });
                if old_still_pending {
                    run.update_notice.clone()
                } else {
                    disk_version
                        .zip(running.as_deref())
                        .and_then(|(disk, running)| version_notice(disk, running))
                        .or(screen_notice)
                        .or(previous_installed_notice.clone())
                }
            } else {
                disk_version
                    .zip(running.as_deref())
                    .and_then(|(disk, running)| version_notice(disk, running))
                    .or(screen_notice)
                    .or(previous_installed_notice)
            }
        };
        if seen.as_deref() == run.update_notice.as_deref() {
            continue;
        }
        if seen.is_some() {
            tracing::info!(run = %run.id, bot = %run.bot_id, kind = %kind, "有新版等著處理");
        }
        let update = async {
            sqlx::query("UPDATE runs SET update_notice = ? WHERE id = ?")
                .bind(seen.as_deref())
                .bind(&run.id)
                .execute(&app.db)
                .await
        };
        if let Some(fence) = observation_fence.as_ref() {
            let Some(result) = app.hosts.run_if_current(fence, update).await else { continue };
            let _ = result;
        } else {
            let _ = update.await;
        }
        app.emit_bot_status(&run.bot_id).await;
    }
}

pub fn spawn_update_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP).await;
            sweep(&app).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-12 使用者實測：跑著 2.1.267、磁碟上 2.1.269。
    #[test]
    fn a_newer_version_on_disk_is_an_update_waiting_for_a_restart() {
        let n = version_notice("2.1.269", "2.1.267").expect("磁碟比較新 = 有更新");
        assert!(n.contains("2.1.269") && n.contains("2.1.267"), "兩個版本都要寫出來：{n}");
    }

    #[test]
    fn same_or_older_on_disk_is_not_an_update() {
        assert!(version_notice("2.1.267", "2.1.267").is_none());
        // 磁碟比較舊（降版、或探到別的 PATH）不是「有更新」，不要叫使用者重啟。
        assert!(version_notice("2.1.266", "2.1.267").is_none());
    }

    /// 版本號位數不同也要比得對：2.1.9 < 2.1.10（字串比較會給反的答案）。
    #[test]
    fn versions_compare_numerically_not_as_strings() {
        assert!(version_notice("2.1.10", "2.1.9").is_some());
        assert!(version_notice("2.1.9", "2.1.10").is_none());
    }

    #[test]
    fn unparsable_versions_are_silent() {
        assert!(version_notice("", "2.1.267").is_none());
        assert!(version_notice("2.1.269", "unknown").is_none());
    }

    #[test]
    fn running_version_comes_from_the_statusline_payload() {
        assert_eq!(running_version(Some(r#"{"version":"2.1.267 (Claude Code)","model_name":"Opus"}"#)).as_deref(), Some("2.1.267"));
        assert!(running_version(Some(r#"{"model_name":"Opus"}"#)).is_none());
        assert!(running_version(None).is_none());
    }

    // ── codex（issue #388）：整條 sweep 走過去，畫面用 MockHerdr 餵，`codex --version` 用預先種好的磁碟版本快取，不起真行程 ──

    const CODEX_MENU: &str = "\
>_ OpenAI Codex (v0.154.0)\n\n✨ Update available! 0.154.0 -> 0.155.1\n\n› 1. Update now (runs `npm install -g @openai/codex`)\n  2. Skip\n  3. Skip until next version\n";
    /// 磁碟版本快取已改掛 App（#759）；這幾條仍序列化，只是保守、成本低。
    /// 磁碟版本快取是全域的：這幾條測試各自種不同的版本，不能平行。
    fn serial() -> &'static tokio::sync::Mutex<()> {
        static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(Default::default)
    }

    async fn seed_disk(app: &Arc<App>, host: &str, kind: &str, v: &str) {
        let fence = app.hosts.fence(host).await.expect("test host exists");
        app.disk_versions.lock().await.insert(
            format!("{kind}@{host}"),
            DiskVersionEntry { at: Instant::now(), authority: fence.authority_key(), version: Some(v.to_string()) },
        );
    }

    async fn notice_of(app: &Arc<App>, run_id: &str) -> Option<String> {
        db::run(&app.db, run_id).await.unwrap().unwrap().update_notice
    }

    async fn codex_bot_with_run(e: &crate::testing::Env) -> (String, String, String) {
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind = 'codex' WHERE id = ?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        (bot.id.clone(), run, format!("pane-{}", bot.id))
    }

    fn remote_host_cfg(name: &str, target: &str) -> crate::config::HostCfg {
        crate::config::HostCfg {
            shared_session: false,
            name: name.into(),
            ssh: target.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }
    }

    #[tokio::test]
    async fn a_repointed_host_does_not_use_the_old_disk_version_cache_for_update_notices() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let host = "update-watch-597";
        let conn_a = e.app.hosts.insert_remote_for_test(remote_host_cfg(host, "target-a")).await;
        conn_a.connected.store(true, std::sync::atomic::Ordering::SeqCst); // 有 pane 在巡的主機是連著的；連不上的不讀磁碟版本
        let remote_herdr = crate::testing::MockHerdr::start(conn_a.client.socket_path().to_path_buf());
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "watch").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        remote_herdr.set_screen(&pane, "❯ hello\n");
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?")
            .bind(host)
            .bind(&e.project_id)
            .execute(&e.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET status_json = ?, herdr_session = 'agents-manager' WHERE id = ?")
            .bind(r#"{"version":"2.1.0 (Claude Code)"}"#)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();

        let installed = Arc::new(std::sync::Mutex::new("2.2.0 (Claude Code)\n".to_string()));
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let installed_for_ssh = installed.clone();
        let probes_for_ssh = probes.clone();
        crate::hosts::set_ssh_fake(host, move |_| {
            probes_for_ssh.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(installed_for_ssh.lock().unwrap().clone())
        });

        sweep(&e.app).await;
        let cached_notice = notice_of(&e.app, &run).await.expect("A 的較新版本先建立通知");
        assert!(cached_notice.contains("2.2.0"), "A 版本要進入 notice：{cached_notice}");
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);

        *installed.lock().unwrap() = "2.0.0 (Claude Code)\n".to_string();
        e.app.hosts.replace_remote_for_test(&e.app, remote_host_cfg(host, "target-b")).await;
        e.app.hosts.get(host).await.unwrap().connected.store(true, std::sync::atomic::Ordering::SeqCst);
        sweep(&e.app).await;

        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2, "B 必須在 TTL 到期前重新讀版本");
        assert_eq!(notice_of(&e.app, &run).await, None, "B 的舊版本不可沿用 A 的 update_notice");
    }

    /// #714：同一輪巡邏順便讀出背景工作數、推 `bot_status`；run 結束之後不留帳。
    #[tokio::test]
    async fn the_sweep_reads_background_jobs_off_the_same_screen() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "bg").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let screen = std::fs::read_to_string(format!(
            "{}/src/lifecycle/fixtures/claude-2.1.281-background-shell.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        e.herdr.set_screen(&format!("pane-{}", bot.id), &screen);
        let mut rx = e.app.subscribe();

        sweep(&e.app).await;
        assert_eq!(crate::background_jobs::get(&e.app, &run), 1);
        let frame = std::iter::from_fn(|| rx.try_recv().ok()).find(|f| f.kind == "bot_status").expect("推 bot_status");
        assert_eq!(frame.data["run"]["background_jobs"], 1);

        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&run).execute(&e.app.db).await.unwrap();
        sweep(&e.app).await;
        assert_eq!(crate::background_jobs::get(&e.app, &run), 0, "結束的 run 不留帳");
    }

    /// `delete_bot` 先定案（`deleted_at`）、再停機：停機那幾秒 bot 已經「沒了」但還在用它的 per-bot 行程帳（欠著的收尾寫入、
    /// 中斷標記…）。這時撞上 sweep 不能把帳清掉——名單要留著還有 active run 的軟刪 bot，停機完成（run 結束）後下一輪才清。
    #[tokio::test]
    async fn a_soft_deleted_bot_that_is_still_stopping_keeps_its_process_state() {
        let e = crate::testing::env().await;
        let stopping = crate::testing::claude_bot(&e.app, &e.project_id, "del-stopping").await;
        let stopped = crate::testing::claude_bot(&e.app, &e.project_id, "del-stopped").await;
        let alive = crate::testing::claude_bot(&e.app, &e.project_id, "alive").await;
        let run = crate::testing::fake_run(&e.app, &stopping.id).await;
        let old_run = crate::testing::fake_run(&e.app, &stopped.id).await;
        let long_ago = db::iso_in(-24 * 3600);
        for id in [&stopping.id, &stopped.id] {
            sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(&long_ago).bind(id).execute(&e.app.db).await.unwrap();
        }
        sqlx::query("UPDATE runs SET state = 'stopping' WHERE id = ?").bind(&run).execute(&e.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&old_run).execute(&e.app.db).await.unwrap();

        let live = live_bot_ids(&e.app).await.unwrap();
        assert!(live.contains(&alive.id));
        assert!(live.contains(&stopping.id), "軟刪了、停機還沒完成（run 還 active）：帳要留著");
        assert!(!live.contains(&stopped.id), "停完很久了：可以清");

        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&run).execute(&e.app.db).await.unwrap();
        assert!(!live_bot_ids(&e.app).await.unwrap().contains(&stopping.id), "停機結束後下一輪才清");
    }

    /// 剛軟刪／退役的 bot 多留一段（可能馬上復原），退役很久的才不在名單裡；沒有 active run 也一樣。
    #[tokio::test]
    async fn a_recently_retired_bot_stays_in_the_live_set_for_a_while() {
        let e = crate::testing::env().await;
        let recent = crate::testing::claude_bot(&e.app, &e.project_id, "ret-recent").await;
        let old = crate::testing::claude_bot(&e.app, &e.project_id, "ret-old").await;
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::iso_in(-120)).bind(&recent.id).execute(&e.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::iso_in(-RETIRED_KEEP_SECS - 60)).bind(&old.id).execute(&e.app.db).await.unwrap();
        let live = live_bot_ids(&e.app).await.unwrap();
        assert!(live.contains(&recent.id), "兩分鐘前退役：可能馬上復原，帳留著");
        assert!(!live.contains(&old.id), "超過保留時間：清");
    }

    /// #767：「沒觀察過」跟「觀察過、是 0」是兩回事——前者（daemon 剛重啟、巡邏還沒輪到）沒有證據，一鍵重啟不擋、確認框標「未知」；
    /// 後者才是乾淨。API 的 `run.background_jobs`：沒觀察過是 `null`，不是 0。
    #[tokio::test]
    async fn an_unobserved_run_is_unknown_and_an_observed_clean_one_is_zero() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "bgz").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let state_run = |app: &Arc<App>| {
            let r = Some(serde_json::json!({"id": run}));
            crate::background_jobs::run_json(app, &r, Some(run.as_str()))["background_jobs"].clone()
        };
        assert_eq!(crate::background_jobs::known(&e.app, &run), None);
        assert!(state_run(&e.app).is_null(), "沒觀察過：null");

        let screen = std::fs::read_to_string(format!(
            "{}/src/lifecycle/fixtures/claude-2.1.281-no-background-shell.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        e.herdr.set_screen(&format!("pane-{}", bot.id), &screen);
        let mut rx = e.app.subscribe();
        sweep(&e.app).await;
        assert_eq!(crate::background_jobs::known(&e.app, &run), Some(0), "看過、乾淨：0");
        assert_eq!(state_run(&e.app), 0);
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|f| f.kind == "bot_status"), "null → 0 也要推，確認框才不會一直停在「未知」");

        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&run).execute(&e.app.db).await.unwrap();
        sweep(&e.app).await;
        assert_eq!(crate::background_jobs::known(&e.app, &run), None, "結束的 run 不留帳");
    }

    /// #767：第一次看過也推 `bot_status`（null → 0）。daemon 剛重啟時每個 run 都是第一次，但每個 run 只推這一次：
    /// 之後的巡邏畫面沒變就不再推，不會變成每 30 秒一波事件風暴。
    #[tokio::test]
    async fn the_first_observation_of_many_runs_pushes_once_each_and_then_stays_quiet() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let screen = std::fs::read_to_string(format!(
            "{}/src/lifecycle/fixtures/claude-2.1.281-no-background-shell.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut bots = Vec::new();
        for i in 0..5 {
            let bot = crate::testing::claude_bot(&e.app, &e.project_id, &format!("storm{i}")).await;
            crate::testing::fake_run(&e.app, &bot.id).await;
            e.herdr.set_screen(&format!("pane-{}", bot.id), &screen);
            bots.push(bot.id);
        }
        let mut rx = e.app.subscribe();
        let pushed = |rx: &mut tokio::sync::broadcast::Receiver<_>| -> usize {
            std::iter::from_fn(|| rx.try_recv().ok()).filter(|f: &crate::state::WsEvent| f.kind == "bot_status" && bots.iter().any(|b| f.data["bot_id"] == b.as_str())).count()
        };
        sweep(&e.app).await;
        assert_eq!(pushed(&mut rx), 5, "每個 run 第一次看過各推一次");
        sweep(&e.app).await;
        sweep(&e.app).await;
        assert_eq!(pushed(&mut rx), 0, "畫面沒變，之後的巡邏不再推");
    }

    /// #744：列舉 active run 失敗的那一輪不能清基準／背景工作帳；成功列舉出空清單才清。
    #[tokio::test]
    async fn a_failed_active_run_listing_keeps_baselines_and_background_jobs() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "keep744").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let screen = "❯ /model sonnet\n  ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions\n";
        e.herdr.set_screen(&pane, screen);
        sweep(&e.app).await;
        let model = || async { db::run(&e.app.db, &run).await.unwrap().unwrap().runtime_model };
        assert_eq!(model().await, None, "第一次看到只當基準");
        let set_jobs = |n: u32| {
            crate::background_jobs::record(&mut e.app.background_jobs.lock().unwrap(), &run, n);
        };
        set_jobs(2);

        sweep_runs(&e.app, Err(anyhow::anyhow!("db is locked"))).await;
        assert_eq!(crate::background_jobs::get(&e.app, &run), 2, "讀失敗不清背景工作帳");

        e.herdr.set_screen(&pane, &format!("{screen}\n❯ /model haiku\n  ⎿  Set model to Haiku 4.5 and saved\n"));
        sweep(&e.app).await;
        assert_eq!(model().await.as_deref(), Some("claude-haiku-4-5"), "基準還在，之後的真切換被採用");

        set_jobs(2);
        sweep_runs(&e.app, Ok(vec![])).await;
        assert_eq!(crate::background_jobs::get(&e.app, &run), 0, "成功列舉出空清單才清");
        assert!(!crate::claude_live::has_baseline(&run), "成功列舉出空清單才清基準");
    }

    #[tokio::test]
    async fn a_codex_run_gets_a_pending_notice_that_says_it_must_be_installed_first() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, CODEX_MENU);
        seed_disk(&e.app, "local", "codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        let n = notice_of(&e.app, &run).await.expect("codex 的提示要被巡到");
        assert!(n.contains("0.154.0") && n.contains("0.155.1") && n.contains("需安裝後重啟"), "{n}");
    }

    #[tokio::test]
    async fn claude_upstream_update_stays_visible_until_that_host_has_the_target_version() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "claude-upstream").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.set_screen(&pane, "› conversation\n");
        sqlx::query("UPDATE runs SET status_json=? WHERE id=?")
            .bind(r#"{"version":"2.1.281 (Claude Code)"}"#)
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        seed_disk(&e.app, "local", "claude", "2.1.281 (Claude Code)").await;
        crate::upstream_update::set_snapshot_for_test(&e.app, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[("local".into(), Ok("2.1.281 (Claude Code)".into()))],
            None,
        ))
        .await;

        sweep(&e.app).await;
        let pending = notice_of(&e.app, &run)
            .await
            .expect("上游比磁碟新時要持續顯示安裝提示");
        assert!(
            pending.contains("2.1.284") && pending.contains("需安裝"),
            "{pending}"
        );

        seed_disk(&e.app, "local", "claude", "2.1.284 (Claude Code)").await;
        sweep(&e.app).await;
        let installed = notice_of(&e.app, &run)
            .await
            .expect("裝到目標後改成重啟套用提示");
        assert!(
            installed.contains("已是 2.1.284") && installed.contains("重啟套用"),
            "{installed}"
        );
        assert!(!installed.contains("需安裝"), "安裝提示要清掉：{installed}");
    }

    #[tokio::test]
    async fn an_installed_claude_notice_survives_a_sweep_without_a_running_version() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "claude-installed-no-version").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.set_screen(&pane, "› conversation\n");
        sqlx::query("UPDATE runs SET status_json = NULL, update_notice = ? WHERE id = ?")
            .bind(crate::upstream_update::claude_installed_text("2.1.284", "2.1.281"))
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        seed_disk(&e.app, "local", "claude", "2.1.284 (Claude Code)").await;
        crate::upstream_update::set_snapshot_for_test(&e.app, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[("local".into(), Ok("2.1.284 (Claude Code)".into()))],
            None,
        ))
        .await;

        sweep(&e.app).await;

        let notice = notice_of(&e.app, &run)
            .await
            .expect("run 版本讀不到時，已安裝的重啟提示仍須持續存在");
        assert!(notice.contains("已安裝") && notice.contains("重啟套用"), "{notice}");
    }

    /// 畫面被推掉：選單／方框不在了，通知不消失；磁碟被人裝好之後改成「已安裝，重啟套用」。
    #[tokio::test]
    async fn when_the_prompt_leaves_the_screen_the_notice_stays_until_the_disk_has_the_new_version() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, CODEX_MENU);
        seed_disk(&e.app, "local", "codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        let pending = notice_of(&e.app, &run).await.unwrap();
        e.herdr.set_screen(&pane, "› a long conversation now\n");
        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await, Some(pending), "提示被推掉不代表新版不存在");
        seed_disk(&e.app, "local", "codex", "codex-cli 0.155.1").await;
        sweep(&e.app).await;
        let n = notice_of(&e.app, &run).await.unwrap();
        assert!(n.contains("已安裝") && n.contains("重啟套用") && !n.contains("需安裝"), "{n}");
    }

    /// 只靠版本比對：從頭到尾沒看過提示，但看過啟動畫面的版本，磁碟後來是新的。
    #[tokio::test]
    async fn a_codex_run_with_no_prompt_at_all_is_noticed_by_the_version_comparison() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, ">_ OpenAI Codex (v0.150.0)\n› hello\n");
        seed_disk(&e.app, "local", "codex", "codex-cli 0.150.0").await;
        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await, None);
        e.herdr.set_screen(&pane, "› later, the banner is gone\n");
        seed_disk(&e.app, "local", "codex", "codex-cli 0.151.2").await;
        sweep(&e.app).await;
        let n = notice_of(&e.app, &run).await.expect("版本比對補位");
        assert!(n.contains("0.151.2") && n.contains("0.150.0"), "{n}");
    }

    /// codex 新版**還沒安裝**時：批次不會自動重啟它（重啟一顆沒裝新版的 codex 換不到任何東西），
    /// 但它仍是候選——header 要看得到，只是被跳過並講清楚原因（2026-09-22：以前整顆連候選都不算，
    /// 這種還沒裝的 codex 有更新在 header 上完全消失，使用者以為只有 claude 會被巡）。
    #[tokio::test]
    async fn a_codex_notice_needing_install_is_a_candidate_but_is_skipped_not_restarted() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (bot, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, CODEX_MENU);
        seed_disk(&e.app, "local", "codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        assert!(notice_of(&e.app, &run).await.is_some());
        let cands = crate::bulk_restart::candidates(&e.app, None).await.unwrap();
        let mine = cands.iter().find(|c| c.bot_id == bot).expect("候選清單有它");
        assert!(mine.has_update && mine.needs_manual_install && crate::bulk_restart::is_candidate(mine));
        let (go, skip) = crate::bulk_restart::plan(&cands);
        assert!(go.iter().all(|c| c.bot_id != bot), "不會被自動重啟");
        let (_, why) = skip.iter().find(|(c, _)| c.bot_id == bot).expect("要在跳過清單裡才會出現在 header");
        assert_eq!(*why, crate::bulk_restart::Skip::NeedsManualInstall);
    }
}
