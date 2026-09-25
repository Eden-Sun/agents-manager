//! claude 更新完只在 pane 底下印 `Update installed · Restart to update`，沒有事件；巡邏讀出來寫進
//! `runs.update_notice` 讓 web 畫重啟徽章。掛 run 不掛 bot：更新屬於這個 process，重啟後自然清空。
//! 掃所有 `running`（不只停著的）：使用者再送話 run 變 working，通知仍該看得見。
//! codex 也巡（issue #388）：它的新版**還沒安裝**，認法與文字見 [`crate::codex_update`]；claude 的批次重啟不收它。

use crate::db;
use crate::state::App;
use crate::tui_prompts::update_notice;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// 每個 claude pane 都要一次 `pane.read`，所以比問卷那支（10 秒）鬆。
const SWEEP: Duration = Duration::from_secs(30);

/// `--version` 是 process spawn（遠端是 ssh），每輪跑太兇；晚幾分鐘亮徽章沒差。
const DISK_VERSION_TTL: Duration = Duration::from_secs(300);

/// 鍵是 `<kind>@<host>`：claude 與 codex 各有各的磁碟版本。
type DiskCache = tokio::sync::Mutex<HashMap<String, (Instant, Option<String>)>>;

fn disk_cache() -> &'static DiskCache {
    static C: OnceLock<DiskCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

async fn disk_version(app: &Arc<App>, host: &str, kind: &str) -> Option<String> {
    let key = format!("{kind}@{host}");
    let mut cache = disk_cache().lock().await;
    if let Some((at, v)) = cache.get(&key) {
        if at.elapsed() < DISK_VERSION_TTL {
            return v.clone();
        }
    }
    let v = crate::changelog::installed_version(app, host, kind).await.ok();
    cache.insert(key, (Instant::now(), v.clone()));
    v
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

async fn sweep(app: &Arc<App>) {
    let runs = db::all_active_runs(&app.db).await.unwrap_or_default();
    for run in runs.into_iter().filter(|r| r.state == "running") {
        let kind = match db::bot(&app.db, &run.bot_id).await {
            Ok(Some(b)) if b.kind == "claude" || b.kind == "codex" => b.kind,
            _ => continue,
        };
        let Some(pane) = run.pane_id.clone() else { continue };
        let Some(client) = app.herdr_for_run(&run).await else { continue };
        // 讀不到畫面就跳過，不要把已經看到的通知清掉。
        let Ok(read) = client.pane_read(&pane, "visible", 80).await else { continue };
        if kind == "codex" {
            // 狀態列是 runtime 的權威，每輪校正（讀不到就不動）。
            crate::codex_live::sync_runtime(app, &client, &run.bot_id, &run.id, &pane).await;
        }
        let seen = if kind == "codex" {
            // 讀不到 host 就整輪跳過、不動既有通知（#243 的同一條理由）。
            let Ok(host) = db::bot_host(&app.db, &run.bot_id).await else { continue };
            let prompt = crate::codex_update::parse_prompt(&read.text);
            let running = crate::codex_update::remember_running(&run.id, &read.text, prompt.as_ref());
            let disk = disk_version(app, &host, "codex").await;
            // 上游最新版：分診帳本（`release-triage-kick` 抓 releases 寫的，跟主機無關）。issue #561。
            let upstream = crate::release_triage::ledger::max_version(&app.db, "codex").await.ok().flatten();
            crate::codex_update::decide(prompt.as_ref(), running.as_deref(), disk.as_deref(), upstream.as_deref(), run.update_notice.as_deref())
        } else {
            let mut seen = update_notice(&read.text);
            // 畫面原句優先（claude 自己說的較準），沒有才用版本比對。
            if seen.is_none() {
                if let Some(running) = running_version(run.status_json.as_deref()) {
                    // 讀不到 host 就整輪跳過、不動既有通知：拿本機的 claude 去比遠端跑的版本會造出或清掉假通知（#243）。
                    let Ok(host) = db::bot_host(&app.db, &run.bot_id).await else { continue };
                    if let Some(disk) = disk_version(app, &host, "claude").await {
                        seen = version_notice(&disk, &running);
                    }
                }
            }
            seen
        };
        if seen.as_deref() == run.update_notice.as_deref() {
            continue;
        }
        if seen.is_some() {
            tracing::info!(run = %run.id, bot = %run.bot_id, kind = %kind, "有新版等著處理");
        }
        let _ = sqlx::query("UPDATE runs SET update_notice = ? WHERE id = ?")
            .bind(seen.as_deref())
            .bind(&run.id)
            .execute(&app.db)
            .await;
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

    /// 磁碟版本快取是全域的（鍵只有 `<kind>@<host>`）：這幾條測試各自種不同的版本，不能平行。
    fn serial() -> &'static tokio::sync::Mutex<()> {
        static M: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        M.get_or_init(Default::default)
    }

    async fn seed_disk(kind: &str, v: &str) {
        disk_cache().lock().await.insert(format!("{kind}@local"), (Instant::now(), Some(v.to_string())));
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

    #[tokio::test]
    async fn a_codex_run_gets_a_pending_notice_that_says_it_must_be_installed_first() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, CODEX_MENU);
        seed_disk("codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        let n = notice_of(&e.app, &run).await.expect("codex 的提示要被巡到");
        assert!(n.contains("0.154.0") && n.contains("0.155.1") && n.contains("需安裝後重啟"), "{n}");
    }

    /// 畫面被推掉：選單／方框不在了，通知不消失；磁碟被人裝好之後改成「已安裝，重啟套用」。
    #[tokio::test]
    async fn when_the_prompt_leaves_the_screen_the_notice_stays_until_the_disk_has_the_new_version() {
        let _serial = serial().lock().await;
        let e = crate::testing::env().await;
        let (_, run, pane) = codex_bot_with_run(&e).await;
        e.herdr.set_screen(&pane, CODEX_MENU);
        seed_disk("codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        let pending = notice_of(&e.app, &run).await.unwrap();
        e.herdr.set_screen(&pane, "› a long conversation now\n");
        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await, Some(pending), "提示被推掉不代表新版不存在");
        seed_disk("codex", "codex-cli 0.155.1").await;
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
        seed_disk("codex", "codex-cli 0.150.0").await;
        sweep(&e.app).await;
        assert_eq!(notice_of(&e.app, &run).await, None);
        e.herdr.set_screen(&pane, "› later, the banner is gone\n");
        seed_disk("codex", "codex-cli 0.151.2").await;
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
        seed_disk("codex", "codex-cli 0.154.0").await;
        sweep(&e.app).await;
        assert!(notice_of(&e.app, &run).await.is_some());
        let cands = crate::bulk_restart::candidates(&e.app).await.unwrap();
        let mine = cands.iter().find(|c| c.bot_id == bot).expect("候選清單有它");
        assert!(mine.has_update && mine.needs_manual_install && crate::bulk_restart::is_candidate(mine));
        let (go, skip) = crate::bulk_restart::plan(&cands);
        assert!(go.iter().all(|c| c.bot_id != bot), "不會被自動重啟");
        let (_, why) = skip.iter().find(|(c, _)| c.bot_id == bot).expect("要在跳過清單裡才會出現在 header");
        assert_eq!(*why, crate::bulk_restart::Skip::NeedsManualInstall);
    }
}
