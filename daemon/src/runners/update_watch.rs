use std::sync::Arc;
use std::time::Instant;
use crate::db;
use crate::state::App;
use crate::tui_prompts::update_notice;
use crate::update_watch::{
    live_bot_ids, running_version, version_notice, DiskVersionEntry, DiskVersionObservation,
    DISK_VERSION_TTL, SWEEP,
};

pub(crate) async fn disk_version(app: &Arc<App>, host: &str, kind: &str) -> Option<DiskVersionObservation> {
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
pub async fn forget_disk_version(app: &App, host: &str, kind: &str) {
    app.disk_versions.lock().await.remove(&format!("{kind}@{host}"));
}

#[cfg(not(test))]
async fn prune_process_state(app: &Arc<App>, active_runs: &[String]) {
    crate::app_ports_p12::retain_pane_typed(active_runs);
    crate::tui_prompts::retain_survey_runs(app, active_runs).await;
    crate::codex_model_migration::retain_runs(active_runs);
    crate::blocked_reason::retain_runs(active_runs);
    // 還要留著 per-bot 帳的 bot：讀不到就這一輪不清（把讀失敗當成「沒有 bot」會清光）。
    let live_bots: Vec<String> = match live_bot_ids(app.as_ref()).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = ?e, "could not list live bots; per-bot process state not pruned this round");
            return;
        }
    };
    crate::pane_identity::retain_bots(&live_bots);
    crate::app_ports_p12::retain_bot_state(&live_bots);
    crate::child_alerts::retain_bots(&live_bots);
    app.retain_bot_locks(&live_bots).await;
}

pub async fn sweep(app: &Arc<App>) {
    sweep_runs(app, db::all_active_runs(&app.db).await).await;
}

pub async fn sweep_runs(app: &Arc<App>, runs: anyhow::Result<Vec<db::Run>>) {
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
    #[cfg(not(test))]
    {
        crate::codex_update::retain_runs(&active);
        crate::prompt_suggestion::retain_runs(&active);
        crate::cache_clock::retain_runs(&active);
        crate::prompt_cache::retain_runs(&active);
        prune_process_state(app, &active).await;
    }
    for run in runs.into_iter().filter(|r| r.state == "running") {
        let kind = match db::bot(&app.db, &run.bot_id).await {
            Ok(Some(b)) if matches!(b.kind.as_str(), "claude" | "codex" | "grok") => b.kind,
            _ => continue,
        };
        let Some(pane) = run.pane_id.clone() else { continue };
        let Some(client) = app.herdr_for_run(&run).await else { continue };
        if kind == "grok" {
            let Ok(read) = client.pane_read(&pane, "visible", 60).await else { continue };
            crate::grok_live::sync_runtime(app, &client, &run.bot_id, &run.id, &pane, Some(&read.text)).await;
            continue;
        }
        let Ok(read) = client.pane_read(&pane, "visible", 80).await else { continue };
        crate::runners::background_jobs::observe(app, &run, &kind, &read.text, &client, &pane).await;
        if kind == "claude" {
            crate::claude_live::observe(app, &run, &read.text).await;
            crate::prompt_suggestion::observe_sweep(app, &run, &read.text, &client, &pane).await;
        }
        if kind == "codex" {
            crate::runners::codex_live::sync_runtime(app, &client, &run.bot_id, &run.id, &pane, Some(&read.text)).await;
            crate::runners::prompt_cache::refresh_codex(app, &run).await;
        }
        let observation_fence;
        let seen = if kind == "codex" {
            let Ok(host) = db::bot_host(&app.db, &run.bot_id).await else { continue };
            let prompt = crate::codex_update::parse_prompt(&read.text);
            let running = crate::codex_update::remember_running(&run.id, &read.text, prompt.as_ref());
            let Some(disk) = disk_version(app, &host, "codex").await else { continue };
            observation_fence = Some(disk.fence);
            let upstream = crate::release_triage::ledger::max_version(&app.db, "codex").await.ok().flatten();
            crate::codex_update::decide(prompt.as_ref(), running.as_deref(), disk.version.as_deref(), upstream.as_deref(), run.update_notice.as_deref())
        } else {
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
            // 「裝著的版本」：磁碟讀得到用磁碟，讀不到才用跑著的；兩個都不知道就不判成落後（未知不等於落後，#974）。
            let installed = disk_version.or(running.as_deref());
            let below_upstream = upstream.as_deref().filter(|target| {
                installed
                    .and_then(crate::changelog::parse_version)
                    .zip(crate::changelog::parse_version(target))
                    .is_some_and(|(have, target)| have < target)
            });
            if let Some(target) = below_upstream {
                Some(crate::upstream_update::claude_pending_text(
                    disk_version.or(running.as_deref()),
                    target,
                ))
            } else if upstream.is_none() {
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
    crate::background_loop::spawn_periodic(&app, "update watcher", SWEEP, SWEEP, |app| async move {
        sweep(&app).await;
    });
}
