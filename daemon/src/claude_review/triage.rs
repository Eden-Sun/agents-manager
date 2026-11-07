//! Update dialogs use the scheduler's structured judgement and publisher. No second summary call.
use std::sync::Arc;

use serde_json::{json, Value};

use crate::claude_review::ReviewState;
use crate::lc_error::LcError;
use crate::release_triage::ledger::{self, Status};
use crate::release_triage_auto::{ApiModel, TriageModel};
use crate::state::App;

pub async fn ready(app: &App) -> bool {
    ApiModel(app).enabled().await
}

/// SQLite wins over legacy summary files, independently of the current API switches.
pub async fn state(app: &(impl crate::capabilities::Db + crate::capabilities::DataDir), kind: &str, version: &str) -> Result<Option<ReviewState>, LcError> {
    if !crate::release_triage::rules::supported(kind) {
        return Ok(None);
    }
    let row = ledger::get(app.db(), kind, version).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let Some(row) = row else { return Ok(None) };
    let analysis = row.verdicts.as_ref().and_then(|v| v.get("analysis"));
    let state = match row.status {
        Status::Judged | Status::Published | Status::Empty => "done",
        Status::Failed => "failed",
        Status::Pending | Status::Dispatched => "pending",
    };
    let result = (state == "done").then(|| {
        let verdicts = row.verdicts.as_ref().and_then(|v| v.get("verdicts")).and_then(Value::as_array);
        let lines: Vec<String> = verdicts.into_iter().flatten()
            .filter(|v| v["verdict"] != "none")
            .map(|v| format!("[{}] {}", v["verdict"].as_str().unwrap_or(""), v["reason"].as_str().unwrap_or("")))
            .collect();
        format!("{}\n來源：共用上游分診；只做解析，未實測、未查程式碼。", if lines.is_empty() { "沒有需要處理的項目。".into() } else { lines.join("\n") })
    });
    Ok(Some(ReviewState {
        state,
        asked_at: Some(row.created_at),
        answered_at: (state == "done" || state == "failed").then_some(row.updated_at),
        source: analysis.filter(|a| a["source"] == "api").map(|_| "api"),
        model: analysis.and_then(|a| a["model"].as_str()).map(str::to_string),
        error: (state == "failed").then(|| analysis.and_then(|a| a["error"].as_str()).unwrap_or("分診失敗，請重試").to_string()),
        result,
        ..Default::default()
    }))
}

/// Seed exactly the requested versions, then let the scheduler's lock serialize both entrances.
/// Existing ledger snapshots (including dispatched bot work) are never overwritten.
pub async fn start(app: &Arc<App>, kind: &str, to: &str, from: Option<&str>, sections: &[crate::changelog::Section], binary_diff: Option<crate::claude_review_api::BinaryDiff>, auto: bool) -> Result<Value, LcError> {
    let existing = ledger::get(&app.db, kind, to).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if existing.is_none() && !sections.iter().any(|s| s.version == to) {
        return Err(LcError::conflict("missing_analysis_input", json!({"message": "changelog 缺少目標版本，不以其他版本替代"})));
    }
    let mut versions = vec![to.to_string()];
    for section in sections {
        let entries = crate::release_triage::build_entries(kind, section).map_err(|e| LcError::Upstream(e.to_string()))?;
        ledger::insert_version(&app.db, kind, &section.version, &entries).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        if !versions.contains(&section.version) { versions.push(section.version.clone()); }
    }
    versions.sort_by_key(|v| crate::changelog::parse_version(v));
    // Capture retry intent before queueing: concurrent first requests must not turn into
    // an implicit retry if the request ahead of them fails while they wait for AUTO_LOCK.
    let mut jobs = Vec::new();
    let mut duplicate = true;
    for version in &versions {
        let row = ledger::get(&app.db, kind, version).await.map_err(|e| LcError::Upstream(e.to_string()))?.unwrap();
        match row.status {
            Status::Pending => { duplicate = false; jobs.push((version.clone(), false, false)); }
            Status::Failed if !auto => jobs.push((version.clone(), true, false)),
            Status::Judged if !auto => jobs.push((version.clone(), false, true)),
            _ => {}
        }
    }
    if !jobs.is_empty() {
        let worker = Arc::clone(app);
        let kind = kind.to_string();
        let target = to.to_string();
        app.background_tasks.spawn(Box::pin(async move {
            let diff = if jobs.iter().any(|(_, _, publish_only)| !publish_only) {
                match &binary_diff { Some(d) => crate::claude_review_api::run_binary_diff(d).await, None => None }
            } else { None };
            for (version, retry_failed, publish_only) in jobs {
                if publish_only {
                    let cfg = worker.cfg.get().await.release_triage;
                    if cfg.publish {
                        let _ = crate::release_triage::issue::publish_version(&worker.db, &cfg, &kind, &version).await;
                    }
                } else {
                    let model = ApiModel(&worker);
                    let result = crate::release_triage_auto::run_auto_with_retry(&worker, &model, &kind, &[version.clone()], if version == target { diff.as_deref() } else { None }, retry_failed).await;
                    if let Err(error) = result { tracing::warn!(?error, "update triage failed"); }
                }
            }
            worker.emit("supervisor_changed", json!({"release_triage": kind})).await;
        }));
    }
    Ok(json!({"kind": kind, "version": to, "from_version": from, "mode": "api", "duplicate": duplicate,
        "sections": sections.len(), "review": state(app.as_ref(), kind, to).await?}))
}
