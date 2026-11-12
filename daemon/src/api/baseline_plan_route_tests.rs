use super::*;
use crate::host_baseline::{BaselineIssue, BaselineReport};
use axum::response::IntoResponse;

async fn call(app: &Arc<App>, host: &str, principal: RequestPrincipal) -> (StatusCode, Value) {
    let resp = match baseline_plan(State(app.clone()), Path(host.to_string()), Extension(principal)).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn report(issues: Option<Vec<BaselineIssue>>, checked_at: String, failed: bool) -> BaselineReport {
    BaselineReport {
        os: Some("Linux".into()),
        issues,
        checked_at,
        failed_at: failed.then(crate::db::now),
        error: failed.then(|| "ssh: timed out".to_string()),
        stale: false,
    }
}

/// #719 階段 2：只給使用者；未知主機 404；沒量過／探測沒跑完 409；過期照回並標 `stale`；hosts 列表的 `fixable` 跟計畫一致。
#[tokio::test]
async fn the_plan_is_user_only_and_reads_only_the_cached_baseline() {
    let e = crate::testing::env().await;
    let local = crate::config::LOCAL_HOST;
    let (st, body) = call(&e.app, local, RequestPrincipal::Bot("b1".into())).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::FORBIDDEN, Some("user_only")), "{body}");
    let (st, _) = call(&e.app, "no-such-host", RequestPrincipal::User).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, body) = call(&e.app, local, RequestPrincipal::User).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("baseline_unknown")), "never measured: {body}");
    e.app.host_baseline.lock().await.insert(local.into(), report(None, crate::db::now(), false));
    let (st, body) = call(&e.app, local, RequestPrincipal::User).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("baseline_unknown")), "probe cut short: {body}");

    let issues = vec![
        BaselineIssue { id: "codex.config.toml:features.hooks".into(), severity: "warn", message: "m".into(), note: None },
        BaselineIssue { id: "tool.rtk".into(), severity: "critical", message: "m".into(), note: None },
    ];
    e.app.host_baseline.lock().await.insert(local.into(), report(Some(issues.clone()), crate::db::now(), false));
    let (st, body) = call(&e.app, local, RequestPrincipal::User).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!((body["host"].as_str(), body["stale"].as_bool()), (Some(local), Some(false)));
    assert_eq!(body["actions"][0]["action"], "merge_key");
    assert_eq!(body["actions"][0]["value"], true);
    assert_eq!((body["actions"][1]["action"].as_str(), body["actions"][1]["reason"].as_str()), (Some("manual"), Some("no_installer")));
    assert!(body["plan_rev"].as_str().is_some_and(|r| !r.is_empty()));
    let view = e.app.host_baseline.lock().await.get(local).unwrap().view(chrono::Utc::now());
    assert_eq!(view.fixable, vec!["codex.config.toml:features.hooks".to_string()]);
    let v = serde_json::to_value(&view).unwrap();
    assert_eq!((v["fixable"][0].as_str(), v["issues"][1]["id"].as_str()), (Some("codex.config.toml:features.hooks"), Some("tool.rtk")), "flattened: {v}");

    e.app.host_baseline.lock().await.insert(local.into(), report(Some(issues), "2026-01-01T00:00:00.000Z".into(), true));
    let (st, body) = call(&e.app, local, RequestPrincipal::User).await;
    assert_eq!((st, body["stale"].as_bool()), (StatusCode::OK, Some(true)), "a stale plan is still served, marked stale: {body}");
}
