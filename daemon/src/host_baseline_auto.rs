use crate::config::HostBaselineSyncMode;
use crate::host_baseline::apply;
use crate::host_baseline::auto::{gate, record, AutoState, Gate};
use crate::state::App;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Instant;
use tokio::sync::Mutex;

static LOCKS: LazyLock<StdMutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(Default::default);
static STATES: LazyLock<StdMutex<HashMap<String, AutoState>>> = LazyLock::new(Default::default);

pub fn host_lock(host: &str) -> Arc<Mutex<()>> {
    LOCKS.lock().unwrap_or_else(|e| e.into_inner()).entry(host.to_string()).or_default().clone()
}

fn state_of(host: &str) -> AutoState {
    STATES.lock().unwrap_or_else(|e| e.into_inner()).get(host).cloned().unwrap_or_default()
}

fn save_result(host: &str, plan_rev: &str, ok: bool) {
    let mut states = STATES.lock().unwrap_or_else(|e| e.into_inner());
    let entry = states.entry(host.to_string()).or_default();
    record(entry, plan_rev, ok, Instant::now());
}

pub async fn after_detect(app: Arc<App>, host: String) {
    let lock = host_lock(&host);
    let Ok(_guard) = lock.try_lock() else { return };
    let mode = app.cfg.get().await.host_baseline_mode(&host);
    if mode != HostBaselineSyncMode::Auto {
        return;
    }
    let Some(fence) = app.hosts.fence(&host).await else { return };
    let Ok((_, plan)) = crate::api::cached_baseline_plan(&app, &host).await else { return };
    if gate(mode, &plan, &state_of(&host), Instant::now()) != Gate::Apply {
        return;
    }
    let Ok(prepared) = apply::prepare(&plan, None) else { return };
    if prepared.writes.is_empty() {
        return;
    }
    let Ok(script) = apply::render_apply_sh(&apply::new_backup_id(), &plan.plan_rev, &prepared.writes) else { return };
    let ok = match app.hosts.run_if_current(&fence, crate::api::baseline_run_script(&fence, &script)).await {
        Some(Ok(out)) => apply::parse_apply(&out, &prepared.writes).is_some_and(|r| r.error.is_none()),
        _ => false,
    };
    save_result(&host, &plan.plan_rev, ok);
    if ok {
        crate::tools::spawn_detect(app.clone(), host);
    }
}
