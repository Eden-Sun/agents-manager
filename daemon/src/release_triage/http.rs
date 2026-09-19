//! `/api/release-triage/*`（issue #204 B）：verdict 進來、帳本查詢、派出標記、publish 重試。docs/API.md。
//!
//! 放在自己的檔案、以 [`routes`] 併進主路由，`api.rs` 只多一行 `.merge`。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::issue::{self, Outcome};
use super::ledger::{self, Row, Status};
use super::verdict::{self, Submission};
use crate::lifecycle::LcError;
use crate::state::App;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/release-triage", get(get_ledger))
        .route("/release-triage/verdicts", post(post_verdicts))
        .route("/release-triage/dispatched", post(post_dispatched))
        .route("/release-triage/publish", post(post_publish))
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

fn row_json(r: &Row) -> Value {
    json!({
        "kind": r.kind,
        "version": r.version,
        "status": r.status.as_str(),
        "entries": r.entries,
        "verdicts": r.verdicts,
        "issues": r.issues,
        "dispatched_at": r.dispatched_at,
        "attempts": r.attempts,
        "publish_error": r.publish_error,
        "created_at": r.created_at,
        "updated_at": r.updated_at,
    })
}

#[derive(Deserialize)]
struct LedgerQuery {
    kind: Option<String>,
    version: Option<String>,
}

/// 逐條結論查得到：`GET /api/release-triage?kind=&version=`（都省略＝全部，新版在前）。
async fn get_ledger(State(app): State<Arc<App>>, Query(q): Query<LedgerQuery>) -> Result<Json<Value>, LcError> {
    let version = q.version.as_deref().and_then(crate::changelog::version_string).or(q.version.clone());
    let rows = ledger::list(&app.db, q.kind.as_deref().filter(|k| !k.is_empty()), version.as_deref().filter(|v| !v.is_empty())).await.map_err(up)?;
    let cfg = app.cfg.get().await.release_triage;
    Ok(Json(json!({
        "publish_enabled": cfg.publish,
        "repo": cfg.repo,
        "rows": rows.iter().map(row_json).collect::<Vec<_>>(),
    })))
}

/// 模型（經 `bin/agm release-triage submit`）交回逐條 verdict 與 issue 提案。整份驗過才收，不合格整份退回。
async fn post_verdicts(State(app): State<Arc<App>>, Json(sub): Json<Submission>) -> Result<Json<Value>, LcError> {
    if !super::rules::supported(&sub.kind) {
        return Err(LcError::Bad(format!("kind `{}` 沒有分診規則（claude｜codex）", sub.kind)));
    }
    let version = crate::changelog::version_string(&sub.version).ok_or_else(|| LcError::Bad(format!("version `{}` 看不出版本", sub.version)))?;
    let row = ledger::get(&app.db, &sub.kind, &version).await.map_err(up)?.ok_or_else(|| LcError::NotFound(format!("帳本沒有 {} {version}", sub.kind)))?;
    if !matches!(row.status, Status::Pending | Status::Dispatched | Status::Failed) {
        return Err(LcError::conflict("not_awaiting_verdict", json!({"status": row.status.as_str()})));
    }
    let (verdicts, proposals) = verdict::validate(&sub, &row.entries)
        .map_err(|problems| LcError::BadValue(json!({"error": "invalid_verdicts", "problems": problems})))?;
    let next = if proposals.is_empty() { Status::Empty } else { Status::Judged };
    let stored = json!({"submitted_at": ledger::now_ts(), "verdicts": verdicts, "issues": proposals});
    if !ledger::save_verdicts(&app.db, &sub.kind, &version, &stored, next).await.map_err(up)? {
        return Err(LcError::conflict("not_awaiting_verdict", json!({"reason": "狀態在驗證期間變了"})));
    }
    let cfg = app.cfg.get().await.release_triage;
    let publish = if next == Status::Judged && cfg.publish {
        Some(issue::publish_version(&app.db, &cfg, &sub.kind, &version).await.map_err(up)?)
    } else {
        None
    };
    let status = ledger::get(&app.db, &sub.kind, &version).await.map_err(up)?.map(|r| r.status.as_str());
    Ok(Json(json!({
        "kind": sub.kind, "version": version, "status": status,
        "verdicts": verdicts.len(), "issues_proposed": proposals.len(),
        "publish_enabled": cfg.publish, "publish": publish,
    })))
}

#[derive(Deserialize)]
struct DispatchedIn {
    kind: String,
    versions: Vec<String>,
}

/// kick 派出交辦後標記：`pending` → `dispatched`（CAS）。同一版不會被下一輪再派。
async fn post_dispatched(State(app): State<Arc<App>>, Json(b): Json<DispatchedIn>) -> Result<Json<Value>, LcError> {
    if !super::rules::supported(&b.kind) {
        return Err(LcError::Bad(format!("kind `{}` 沒有分診規則", b.kind)));
    }
    let versions: Vec<String> = b.versions.iter().filter_map(|v| crate::changelog::version_string(v)).collect();
    let n = ledger::mark_dispatched(&app.db, &b.kind, &versions).await.map_err(up)?;
    Ok(Json(json!({"kind": b.kind, "dispatched": n})))
}

#[derive(Deserialize, Default)]
struct PublishIn {
    kind: Option<String>,
    version: Option<String>,
}

/// 重試 publish：對所有（或指定的）`judged` 版本重跑 issue 開立，不重派模型。`publish = false` 時什麼都不做。
async fn post_publish(State(app): State<Arc<App>>, body: Option<Json<PublishIn>>) -> Result<Json<Value>, LcError> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let cfg = app.cfg.get().await.release_triage;
    let rows = ledger::list(&app.db, b.kind.as_deref(), b.version.as_deref()).await.map_err(up)?;
    let mut out = Vec::new();
    for r in rows.iter().filter(|r| r.status == Status::Judged) {
        let o: Outcome = issue::publish_version(&app.db, &cfg, &r.kind, &r.version).await.map_err(up)?;
        out.push(json!({"kind": r.kind, "version": r.version, "result": o}));
    }
    Ok(Json(json!({"publish_enabled": cfg.publish, "results": out})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release_triage::verdict::{EntryVerdict, Proposal, Verdict};
    use crate::release_triage::{build_entries, source_sections, Bucket};

    #[tokio::test]
    async fn verdicts_flow_through_the_real_db_and_stay_ledger_only_by_default() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sections = source_sections("claude", include_str!("fixtures/claude_2.1.276-278.md"));
        let entries = build_entries("claude", sections.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap();
        ledger::insert_version(&app.db, "claude", "2.1.277", &entries).await.unwrap();

        let d = post_dispatched(State(app.clone()), Json(DispatchedIn { kind: "claude".into(), versions: vec!["2.1.277".into()] })).await.unwrap();
        assert_eq!(d.0["dispatched"], 1);

        let judged: Vec<&crate::release_triage::Entry> = entries.iter().filter(|e| e.bucket != Bucket::Dropped).collect();
        let mk = |verdict| judged.iter().map(|e| EntryVerdict { entry_id: e.id.clone(), verdict, reason: "r".into(), module: "m".into() }).collect::<Vec<_>>();
        // 缺一條 → 400，狀態不動。
        let mut partial = mk(Verdict::None);
        partial.pop();
        let bad = post_verdicts(State(app.clone()), Json(Submission { kind: "claude".into(), version: "2.1.277".into(), verdicts: partial, issues: vec![] })).await;
        assert!(matches!(bad, Err(LcError::BadValue(_))));
        assert_eq!(ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap().status, Status::Dispatched);

        let mut vs = mk(Verdict::None);
        vs[0].verdict = Verdict::Guard;
        let issues = vec![Proposal {
            entry_ids: vec![vs[0].entry_id.clone()],
            verdict: None,
            title: "t".into(),
            goal: "g".into(),
            suggestion: "s".into(),
            acceptance: "a".into(),
            duplicate_of: None,
        }];
        let ok = post_verdicts(State(app.clone()), Json(Submission { kind: "claude".into(), version: "v2.1.277".into(), verdicts: vs.clone(), issues: issues.clone() })).await.unwrap();
        assert_eq!(ok.0["status"], "judged");
        assert_eq!(ok.0["publish_enabled"], false, "預設只寫帳本");
        assert!(ok.0["publish"].is_null());
        // 已 judged：同一版不接第二份。
        let again = post_verdicts(State(app.clone()), Json(Submission { kind: "claude".into(), version: "2.1.277".into(), verdicts: vs, issues })).await;
        assert!(matches!(again, Err(LcError::Conflict(_))));

        let got = get_ledger(State(app.clone()), Query(LedgerQuery { kind: Some("claude".into()), version: Some("2.1.277".into()) })).await.unwrap();
        assert_eq!(got.0["rows"][0]["status"], "judged");
        assert_eq!(got.0["rows"][0]["verdicts"]["issues"][0]["triage"], "guard");
        assert_eq!(got.0["rows"][0]["entries"].as_array().unwrap().len(), entries.len());
        // publish 重試端點在 publish=false 時什麼都不做。
        let p = post_publish(State(app.clone()), None).await.unwrap();
        assert_eq!(p.0["results"][0]["result"]["outcome"], "disabled");
    }
}
