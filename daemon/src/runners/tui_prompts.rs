//! `tui_prompts` 背景巡邏 runner。

use crate::db;
use crate::state::App;
use crate::lifecycle::s6_ports::QueueServices;
use crate::tui_prompts::dismiss_if_survey;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::HashMap;

const SWEEP: Duration = Duration::from_secs(10);
const MENU_SWEEP: Duration = Duration::from_secs(60);
const MENU_MAX_READS: usize = 8;
const MENU_MAX_KEYS: usize = 256;

#[derive(Default)]
struct MenuSchedule {
    cursor: std::sync::Mutex<Option<String>>,
    keys: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Instant>>>>>,
}

impl MenuSchedule {
    fn slot(&self, key: &str) -> Option<Arc<tokio::sync::Mutex<Option<Instant>>>> {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = keys.get(key) { return Some(slot.clone()); }
        keys.retain(|_, slot| {
            Arc::strong_count(slot) != 1 || slot.try_lock().map_or(true, |last| last.is_some_and(|t| t.elapsed() < MENU_SWEEP))
        });
        if keys.len() >= MENU_MAX_KEYS { return None; }
        let slot = Arc::new(tokio::sync::Mutex::new(None));
        keys.insert(key.to_owned(), slot.clone());
        Some(slot)
    }
}

/// `idle` 也掃：問卷在回合結束後插入，herdr 可能只判成 idle，下一句話就會打進問卷裡。
pub fn spawn_survey_watcher(app: Arc<App>) {
    let schedule = Arc::new(MenuSchedule::default());
    let app_for_task = app.clone();
    crate::background_loop::spawn_restartable(&*app, "survey watcher", move || {
        let app = app_for_task.clone();
        let schedule = schedule.clone();
        async move {
            let menu_app = app.clone();
            let menu = async move {
                loop {
                    tokio::select! {
                        _ = menu_app.shutdown.cancelled() => break,
                        _ = tokio::time::sleep(MENU_SWEEP) => {}
                    }
                    let app = menu_app.clone();
                    let Ok(mut runs) = db::all_active_runs(&app.db).await else { continue };
                    runs.sort_by(|a, b| a.id.cmp(&b.id));
                    let active: Vec<String> = runs.iter().map(|r| r.id.clone()).collect();
                    let _ = crate::lifecycle::poller::observe_claude_menu(&app, None, &active).await;
                    let cursor = schedule.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let start = cursor.as_ref().map_or(0, |last| runs.partition_point(|run| run.id <= *last));
                    let mut reads = 0;
                    for run in runs.iter().cycle().skip(start).take(runs.len()) {
                        *schedule.cursor.lock().unwrap_or_else(|e| e.into_inner()) = Some(run.id.clone());
                        if !matches!(run.agent_status.as_str(), "idle" | "unknown" | "blocked") { continue; }
                        let Some(slot) = schedule.slot(&run.id) else { continue };
                        let Ok(mut last) = slot.clone().try_lock_owned() else { continue };
                        if last.is_some_and(|t| t.elapsed() < MENU_SWEEP) { continue; }
                        *last = Some(Instant::now());
                        let result = tokio::select! {
                            _ = app.shutdown.cancelled() => break,
                            result = crate::lifecycle::poller::observe_claude_menu(&app, Some(run), &active) => result,
                        };
                        match result {
                            Ok(true) => reads += 1,
                            Ok(false) => {
                                drop(last);
                                let mut keys = schedule.keys.lock().unwrap_or_else(|e| e.into_inner());
                                if Arc::strong_count(&slot) == 2 && keys.get(&run.id).is_some_and(|current| Arc::ptr_eq(current, &slot)) {
                                    keys.remove(&run.id);
                                }
                            },
                            Err(_) => { reads += 1; },
                        }
                        if reads >= MENU_MAX_READS { break; }
                    }
                }
            };
            let survey = async move {
                loop {
                    tokio::select! {
                        _ = app.shutdown.cancelled() => break,
                        _ = tokio::time::sleep(SWEEP) => {}
                    }
                    let runs = db::all_active_runs(&app.db).await.unwrap_or_default();
                    crate::session_paused::forget_ended(&app, &runs).await;
                    crate::dangerous_rm::forget_ended(&app, &runs).await;
                    for run in runs {
                        // #1876：啟動的一幀空框之後才跳警告、或事件漏掉的備援；跟送達共用 bot 鎖。
                        if matches!(run.agent_status.as_str(), "blocked" | "idle" | "unknown") {
                            let lock = app.bot_lock(&run.bot_id).await;
                            if let Ok(_guard) = lock.try_lock() {
                                if crate::lifecycle::claude_resume_menu::accept_if_shown(&app, &run).await.unwrap_or(false) {
                                    app.bot_status_changed(&run.bot_id).await;
                                    crate::lifecycle::queue::schedule_flush_queued(&app, &run.bot_id);
                                    continue;
                                }
                            };
                        }
                        if run.agent_status == "blocked" || run.agent_status == "idle" || crate::dangerous_rm::is_open(&run.id) {
                            dismiss_if_survey(&app, &run).await;
                            // 防誤刪框：herdr 判成 idle 時的安全網，也負責框關掉之後的收尾（不按任何鍵）。
                            crate::runners::dangerous_rm::observe(&app, &run).await;
                        }
                        // Session paused 選單：herdr 判 idle 時補標 blocked、選單關掉時還原（不按任何鍵）。
                        if run.agent_status == "idle" || crate::session_paused::is_forced(&run.id) {
                            crate::runners::session_paused::observe(&app, &run).await;
                        }
                        // Codex 在 starting／working 狀態也可能停在啟動遷移框；只查畫面，不替使用者選。
                        crate::runners::codex_model_migration::observe(&app, &run).await;
                        // claude 停在一般權限確認選單：事件那一刻漏掉、或選單換了一種工具，這裡每輪重讀一次（只看、不按鍵）。
                        if run.agent_status == "blocked" {
                            crate::blocked_reason::observe(&app, &run).await;
                            crate::runners::codex_async_question::observe(&app, &run).await;
                        }
                    }
                }
            };
            tokio::join!(menu, survey);
        }
    });
}

#[cfg(test)]
mod menu_schedule_tests {
    use super::*;

    #[tokio::test]
    async fn menu_slots_are_single_flight_and_bounded() {
        let schedule = MenuSchedule::default();
        let slot = schedule.slot("run").unwrap();
        let mut held = slot.clone().try_lock_owned().unwrap();
        *held = Some(Instant::now());
        assert!(schedule.slot("run").unwrap().try_lock_owned().is_err());
        drop(held);
        assert!(schedule.slot("run").unwrap().try_lock_owned().unwrap().is_some_and(|t| t.elapsed() < MENU_SWEEP));
        for i in 1..MENU_MAX_KEYS {
            let slot = schedule.slot(&format!("run-{i}")).unwrap();
            *slot.lock().await = Some(Instant::now());
        }
        assert!(schedule.slot("overflow").is_none());
    }
}
