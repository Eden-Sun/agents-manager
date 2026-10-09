//! `/api/release-triage/*`（issue #204 B）：verdict 進來、帳本查詢、派出標記、publish 重試。docs/API.md。

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::RequestPrincipal;
use crate::lc_error::LcError;
use crate::release_triage::issue::{self, Outcome};
use crate::release_triage::ledger::{self, Row, Status};
use crate::release_triage::verdict::{self, Submission};
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
        "assigned_bot_id": r.assigned_bot_id,
        "dispatch_gen": r.dispatch_gen,
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
/// Bot principal（#801）只看到派給它（或它底下的 child）、還在 `dispatched` 的列，要拿來交 verdict 用的 `dispatch_gen` 也在裡面；
/// 全域帳本只給 User 與 AGM 角色。
async fn get_ledger(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>, Query(q): Query<LedgerQuery>) -> Result<Json<Value>, LcError> {
    let version = q.version.as_deref().and_then(crate::changelog::version_string).or(q.version.clone());
    let mut rows = ledger::list(&app.db, q.kind.as_deref().filter(|k| !k.is_empty()), version.as_deref().filter(|v| !v.is_empty())).await.map_err(up)?;
    if let RequestPrincipal::Bot(caller) = &principal {
        let mut mine = Vec::new();
        for r in rows {
            let ours = match (r.status == Status::Dispatched, r.assigned_bot_id.as_deref()) {
                (true, Some(root)) => ledger::bot_within(&app.db, caller, root).await.map_err(up)?,
                _ => false,
            };
            if ours {
                mine.push(r);
            }
        }
        rows = mine;
    }
    let cfg = app.cfg.get().await.release_triage;
    Ok(Json(json!({
        "publish_enabled": cfg.publish,
        "repo": cfg.repo,
        "rows": rows.iter().map(row_json).collect::<Vec<_>>(),
    })))
}

/// Bot principal 交回 verdict 的綁定（#801）：只收派給它（或它底下的 child）、還在 `dispatched`、而且是這一代的列。
/// 順序是先看「派給誰」（不是它的人一律 403，不論列的狀態），再看代數與狀態；過了才回 `Some((派給的 bot, 代數))` 給 CAS 用。
async fn bot_verdict_binding(app: &App, caller: &str, row: &Row, gen: Option<i64>) -> Result<(String, i64), LcError> {
    let root = match row.assigned_bot_id.as_deref() {
        Some(root) if ledger::bot_within(&app.db, caller, root).await.map_err(up)? => root.to_string(),
        _ => return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "not_assigned"}))),
    };
    let Some(gen) = gen else {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "dispatch_gen_required", "message": "交回時要帶 `dispatch_gen`（`agm release-triage show` 那一列的值）"})));
    };
    if row.status != Status::Dispatched {
        return Err(LcError::conflict("not_awaiting_verdict", json!({"status": row.status.as_str()})));
    }
    if gen != row.dispatch_gen {
        return Err(LcError::conflict("stale_assignment", json!({"message": "這份交辦已被換掉（代數不符），請用最新的 show 再交"})));
    }
    Ok((root, row.dispatch_gen))
}

/// 模型（經 `bin/agm release-triage submit`）交回逐條 verdict 與 issue 提案。整份驗過才收，不合格整份退回。
/// Bot principal 只能交派給它的那一版（#801，見 [`bot_verdict_binding`]）；User 與 AGM 補交不綁。
async fn post_verdicts(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>, Json(sub): Json<Submission>) -> Result<Json<Value>, LcError> {
    if !crate::release_triage::rules::supported(&sub.kind) {
        return Err(LcError::Bad(format!("kind `{}` 沒有分診規則（claude｜codex）", sub.kind)));
    }
    let version = crate::changelog::version_string(&sub.version).ok_or_else(|| LcError::Bad(format!("version `{}` 看不出版本", sub.version)))?;
    let row = ledger::get(&app.db, &sub.kind, &version).await.map_err(up)?.ok_or_else(|| LcError::NotFound(format!("帳本沒有 {} {version}", sub.kind)))?;
    let binding = match &principal {
        RequestPrincipal::Bot(caller) => Some(bot_verdict_binding(&app, caller, &row, sub.dispatch_gen).await?),
        _ => {
            if !matches!(row.status, Status::Pending | Status::Dispatched | Status::Failed) {
                return Err(LcError::conflict("not_awaiting_verdict", json!({"status": row.status.as_str()})));
            }
            None
        }
    };
    let (verdicts, proposals) = verdict::validate(&sub, &row.entries)
        .map_err(|problems| LcError::BadValue(json!({"error": "invalid_verdicts", "problems": problems})))?;
    // `duplicate_of` 會被 publish 直接拿去 `gh issue comment <n>`：只收帳本裡真的有的 release-triage issue，
    // 模型填錯或幻覺出別的 issue／PR 編號時整份退回，不能把留言貼到不相干的地方。
    let dups: Vec<i64> = proposals.iter().filter_map(|p| p.duplicate_of).collect();
    if !dups.is_empty() {
        let known = issue::ledger_issue_numbers(&app.db).await.map_err(up)?;
        let problems: Vec<String> = dups
            .iter()
            .filter(|d| !known.contains(d))
            .map(|d| format!("duplicate_of #{d} 不是帳本裡任何一張 release-triage issue（只能指向 `agm release-triage show` 看得到的 issue）"))
            .collect();
        if !problems.is_empty() {
            return Err(LcError::BadValue(json!({"error": "invalid_verdicts", "problems": problems})));
        }
    }
    let next = if proposals.is_empty() { Status::Empty } else { Status::Judged };
    let stored = json!({"submitted_at": ledger::now_ts(), "verdicts": verdicts, "issues": proposals});
    let cas = binding.as_ref().map(|(root, gen)| (root.as_str(), *gen));
    if !ledger::save_verdicts(&app.db, &sub.kind, &version, &stored, next, cas).await.map_err(up)? {
        // 說明放 `message`：extra 的 `reason` 會蓋掉機器 key（#228，同 #219）。
        return Err(LcError::conflict("not_awaiting_verdict", json!({"message": "狀態在驗證期間變了"})));
    }
    let cfg = app.cfg.get().await.release_triage;
    let publish = if next == Status::Judged && cfg.publish {
        let before = row.issues.clone();
        let outcome = issue::publish_version(&app.db, &cfg, &sub.kind, &version).await.map_err(up)?;
        let created = match &outcome {
            Outcome::Published { created, .. } => *created,
            _ => 0,
        };
        crate::judge::collision::hint_after_publish(&app, &before, &sub.kind, &version, created).await;
        Some(outcome)
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
    /// #801：這批交辦收件的 bot（kick 用的 `assign --bot`）。之後只有它（或它底下的 child）能交這些版的 verdict。
    bot_id: String,
}

/// kick 派出交辦後標記：`pending` → `dispatched`（CAS）。同一版不會被下一輪再派。記下收件 bot 並把派工代數＋1（#801）。
async fn post_dispatched(State(app): State<Arc<App>>, Json(b): Json<DispatchedIn>) -> Result<Json<Value>, LcError> {
    if !crate::release_triage::rules::supported(&b.kind) {
        return Err(LcError::Bad(format!("kind `{}` 沒有分診規則", b.kind)));
    }
    match crate::db::bot(&app.db, &b.bot_id).await.map_err(up)? {
        Some(bot) if bot.deleted_at.is_none() => {}
        _ => return Err(LcError::Bad(format!("bot_id `{}` 不是現存的 bot", b.bot_id))),
    }
    let versions: Vec<String> = b.versions.iter().filter_map(|v| crate::changelog::version_string(v)).collect();
    let n = ledger::mark_dispatched(&app.db, &b.kind, &versions, &b.bot_id).await.map_err(up)?;
    Ok(Json(json!({"kind": b.kind, "dispatched": n, "bot_id": b.bot_id})))
}

#[derive(Deserialize, Default)]
struct PublishIn {
    kind: Option<String>,
    version: Option<String>,
    /// 乾跑：只讀 gh（auth／repo／標籤／去重）並算出「會開哪幾張」，一張都不開、帳本不動。
    #[serde(default)]
    dry_run: bool,
}

/// 重試 publish：對所有（或指定的）`judged` 版本重跑 issue 開立，不重派模型。`publish = false` 時什麼都不做。
/// `dry_run` 例外：`publish = false` 時也會跑，因為打開開關之前就要看得到會發生什麼。
async fn post_publish(State(app): State<Arc<App>>, body: Option<Json<PublishIn>>) -> Result<Json<Value>, LcError> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let cfg = app.cfg.get().await.release_triage;
    // 篩選條件跟 `post_dispatched`／`get_ledger` 對齊（#458）：`ledger::list` 是完全相等比對，
    // 不驗、不正規化的話 `--kind codx`（打錯）或 `--version v0.156.0`（帶 v）都只會靜靜回
    // 「沒事要做」，跟「這一版真的沒東西要開」在輸出上分不出來——而乾跑的數字正是打開
    // `publish` 之前的依據。
    if let Some(kind) = b.kind.as_deref() {
        if !crate::release_triage::rules::supported(kind) {
            return Err(LcError::Bad(format!("kind `{kind}` 沒有分診規則（claude｜codex）")));
        }
    }
    let version = match b.version.as_deref() {
        Some(v) => Some(crate::changelog::version_string(v).ok_or_else(|| LcError::Bad(format!("version `{v}` 看不出版本")))?),
        None => None,
    };
    if b.dry_run {
        return Ok(Json(issue::preflight(&app.db, &cfg, b.kind.as_deref(), version.as_deref()).await.map_err(up)?));
    }
    let rows = ledger::list(&app.db, b.kind.as_deref(), version.as_deref()).await.map_err(up)?;
    let mut out = Vec::new();
    for r in rows.iter().filter(|r| r.status == Status::Judged) {
        let before = r.issues.clone();
        let o: Outcome = issue::publish_version(&app.db, &cfg, &r.kind, &r.version).await.map_err(up)?;
        let created = match &o {
            Outcome::Published { created, .. } => *created,
            _ => 0,
        };
        crate::judge::collision::hint_after_publish(&app, &before, &r.kind, &r.version, created).await;
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
        let sections = source_sections("claude", include_str!("../../../crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"));
        let entries = build_entries("claude", sections.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap();
        ledger::insert_version(&app.db, "claude", "2.1.277", &entries).await.unwrap();
        let worker = crate::testing::claude_bot(&app, &env.project_id, "rt-worker").await;

        let d = post_dispatched(State(app.clone()), Json(DispatchedIn { kind: "claude".into(), versions: vec!["2.1.277".into()], bot_id: worker.id.clone() })).await.unwrap();
        assert_eq!(d.0["dispatched"], 1);

        let judged: Vec<&crate::release_triage::Entry> = entries.iter().filter(|e| e.bucket != Bucket::Dropped).collect();
        let mk = |verdict| judged.iter().map(|e| EntryVerdict { entry_id: e.id.clone(), verdict, reason: "r".into(), module: "m".into() }).collect::<Vec<_>>();
        // 缺一條 → 400，狀態不動。
        let mut partial = mk(Verdict::None);
        partial.pop();
        let bad = post_verdicts(State(app.clone()), Extension(RequestPrincipal::User), Json(Submission { dispatch_gen: None, kind: "claude".into(), version: "2.1.277".into(), verdicts: partial, issues: vec![] })).await;
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
        let ok = post_verdicts(State(app.clone()), Extension(RequestPrincipal::User), Json(Submission { dispatch_gen: None, kind: "claude".into(), version: "v2.1.277".into(), verdicts: vs.clone(), issues: issues.clone() })).await.unwrap();
        assert_eq!(ok.0["status"], "judged");
        assert_eq!(ok.0["publish_enabled"], false, "預設只寫帳本");
        assert!(ok.0["publish"].is_null());
        // 已 judged：同一版不接第二份。
        let again = post_verdicts(State(app.clone()), Extension(RequestPrincipal::User), Json(Submission { dispatch_gen: None, kind: "claude".into(), version: "2.1.277".into(), verdicts: vs, issues })).await;
        assert!(matches!(again, Err(LcError::Conflict(_))));

        let got = get_ledger(State(app.clone()), Extension(RequestPrincipal::User), Query(LedgerQuery { kind: Some("claude".into()), version: Some("2.1.277".into()) })).await.unwrap();
        assert_eq!(got.0["rows"][0]["status"], "judged");
        assert_eq!(got.0["rows"][0]["verdicts"]["issues"][0]["triage"], "guard");
        assert_eq!(got.0["rows"][0]["entries"].as_array().unwrap().len(), entries.len());
        // publish 重試端點在 publish=false 時什麼都不做。
        let p = post_publish(State(app.clone()), None).await.unwrap();
        assert_eq!(p.0["results"][0]["result"]["outcome"], "disabled");
    }

    /// #458：`publish` 的篩選條件原本不正規化也不驗——`v2.1.277`（帶 v）與打錯的 kind 都只會
    /// 靜靜回「沒事要做」，跟「這一版真的沒東西要開」分不出來。`show` 一直都會正規化，兩支對同一個
    /// 輸入給不一樣的答案就是這張票。
    #[tokio::test]
    async fn the_publish_filters_are_normalised_and_a_bad_kind_is_rejected() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sections = source_sections("claude", include_str!("../../../crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"));
        let entries = build_entries("claude", sections.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap();
        ledger::insert_version(&app.db, "claude", "2.1.277", &entries).await.unwrap();
        let vs: Vec<EntryVerdict> = entries
            .iter()
            .filter(|e| e.bucket != Bucket::Dropped)
            .map(|e| EntryVerdict { entry_id: e.id.clone(), verdict: Verdict::Guard, reason: "r".into(), module: "m".into() })
            .collect();
        let issues = vec![Proposal {
            entry_ids: vec![vs[0].entry_id.clone()],
            verdict: None,
            title: "t".into(),
            goal: "g".into(),
            suggestion: "s".into(),
            acceptance: "a".into(),
            duplicate_of: None,
        }];
        let _ = post_verdicts(State(app.clone()), Extension(RequestPrincipal::User), Json(Submission { dispatch_gen: None, kind: "claude".into(), version: "2.1.277".into(), verdicts: vs, issues })).await.unwrap();
        assert_eq!(ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap().status, Status::Judged);

        let pub_in = |kind: Option<&str>, version: Option<&str>, dry_run: bool| {
            Json(PublishIn { kind: kind.map(str::to_string), version: version.map(str::to_string), dry_run })
        };
        // 帶 v 的版本要對到同一列（以前回 results: []）。
        let hit = post_publish(State(app.clone()), Some(pub_in(None, Some("v2.1.277"), false))).await.unwrap();
        assert_eq!(hit.0["results"].as_array().unwrap().len(), 1, "v2.1.277 要對到 2.1.277：{}", hit.0);
        assert_eq!(hit.0["results"][0]["version"], "2.1.277");
        // 乾跑那條路同樣要正規化，否則打開 publish 前看到的 would_create 會是假的 0。
        let dry = post_publish(State(app.clone()), Some(pub_in(None, Some("v2.1.277"), true))).await.unwrap();
        assert_eq!(dry.0["versions"].as_array().unwrap().len(), 1, "{}", dry.0);
        // 認不出的版本與不支援的 kind 都是 400，不再靜靜回空。
        for bad in [pub_in(None, Some("不是版本"), false), pub_in(Some("codx"), None, false), pub_in(Some("codx"), None, true)] {
            assert!(matches!(post_publish(State(app.clone()), Some(bad)).await, Err(LcError::Bad(_))));
        }
    }

    /// `duplicate_of` 是模型填的數字，publish 會直接 `gh issue comment <n>`：填錯（或幻覺出）一個 repo 裡別的 issue／PR 編號，
    /// 留言就貼到不相干的地方。只收帳本裡真的有的 release-triage issue；整份退回、列出原因。
    #[tokio::test]
    async fn duplicate_of_must_name_a_release_triage_issue_the_ledger_knows() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sections = source_sections("claude", include_str!("../../../crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"));
        let entries = build_entries("claude", sections.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap();
        ledger::insert_version(&app.db, "claude", "2.1.277", &entries).await.unwrap();
        let vs: Vec<EntryVerdict> = entries
            .iter()
            .filter(|e| e.bucket != Bucket::Dropped)
            .map(|e| EntryVerdict { entry_id: e.id.clone(), verdict: Verdict::Guard, reason: "r".into(), module: "m".into() })
            .collect();
        let proposal = |dup: Option<i64>| Proposal {
            entry_ids: vec![vs[0].entry_id.clone()],
            verdict: None,
            title: "t".into(),
            goal: "g".into(),
            suggestion: "s".into(),
            acceptance: "a".into(),
            duplicate_of: dup,
        };
        let submit = |dup| post_verdicts(State(app.clone()), Extension(RequestPrincipal::User), Json(Submission { dispatch_gen: None, kind: "claude".into(), version: "2.1.277".into(), verdicts: vs.clone(), issues: vec![proposal(dup)] }));
        // 帳本裡沒有 #9999：退回，狀態不動。
        let err = submit(Some(9999)).await.expect_err("不認得的 issue 編號不能當 duplicate_of");
        let LcError::BadValue(body) = err else { panic!("要是 400：{err:?}") };
        assert!(body["problems"].to_string().contains("9999"), "{body}");
        assert_eq!(ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap().status, Status::Pending);
        // 帳本裡別的版本開過的 #123 是合法的重複對象。
        let mut other = entries.clone();
        other.truncate(1);
        ledger::insert_version(&app.db, "claude", "2.1.276", &other).await.unwrap();
        sqlx::query("UPDATE release_triage SET issue_numbers_json = ? WHERE kind = 'claude' AND version = '2.1.276'")
            .bind(r#"[
                {"marker":"claude@2.1.276#comment","entry_ids":["x"],"number":555,"url":"","created_at":"2026-09-01T00:00:00.000Z","comment":true},
                {"marker":"claude@2.1.276#x","entry_ids":["x"],"number":123,"url":"u","created_at":"2026-09-01T00:00:00.000Z","comment":false}
            ]"#)
            .execute(&app.db)
            .await
            .unwrap();
        let err = submit(Some(555)).await.expect_err("只有留言記錄而沒有原 issue 記錄，不算合法 duplicate_of");
        assert!(matches!(err, LcError::BadValue(_)), "{err:?}");
        assert_eq!(ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap().status, Status::Pending);
        let ok = submit(Some(123)).await.expect("帳本裡有的 issue 照收");
        assert_eq!(ok.0["status"], "judged");
    }

    /// herdr 沒有分診規則（第二階段）：更新框照樣能讀 `?kind=herdr`（200、空的 rows），寫入端點仍是 400。
    /// herdr 的「請 AGM 解析」走 `claude_review`，跟 `herdr-update-kick.sh` 共用同一個 request id。
    #[tokio::test]
    async fn herdr_reads_an_empty_ledger_and_cannot_be_written() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let got = get_ledger(State(app.clone()), Extension(RequestPrincipal::User), Query(LedgerQuery { kind: Some("herdr".into()), version: None })).await.unwrap();
        assert_eq!(got.0["rows"], json!([]));
        let d = post_dispatched(State(app.clone()), Json(DispatchedIn { kind: "herdr".into(), versions: vec!["0.9.3".into()], bot_id: String::new() })).await;
        assert!(matches!(d, Err(LcError::Bad(_))));
    }

    /// 驗證期間狀態被別人改掉（`save_verdicts` 沒更新到任何一列）：一樣是 API.md 的 409 `not_awaiting_verdict`。
    /// 以前 extra 帶 `reason: "狀態在驗證期間變了"`，`LcError::conflict` 合併時蓋掉機器 key（跟 #219 同一類），
    /// `agm release-triage submit` 的呼叫端對不到。
    #[tokio::test]
    async fn a_state_change_during_validation_is_still_the_documented_409() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sections = source_sections("claude", include_str!("../../../crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"));
        let entries = build_entries("claude", sections.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap();
        ledger::insert_version(&app.db, "claude", "2.1.277", &entries).await.unwrap();
        let vs: Vec<EntryVerdict> = entries
            .iter()
            .filter(|e| e.bucket != Bucket::Dropped)
            .map(|e| EntryVerdict { entry_id: e.id.clone(), verdict: Verdict::None, reason: "r".into(), module: "m".into() })
            .collect();
        // 讀的時候還是 pending、寫的時候一列都沒更新到＝驗證期間狀態變了。
        sqlx::query("CREATE TRIGGER hold_release_triage BEFORE UPDATE ON release_triage BEGIN SELECT RAISE(IGNORE); END")
            .execute(&app.db)
            .await
            .unwrap();
        let err = post_verdicts(State(app.clone()), Extension(RequestPrincipal::User), Json(Submission { dispatch_gen: None, kind: "claude".into(), version: "2.1.277".into(), verdicts: vs, issues: vec![] }))
            .await
            .expect_err("沒寫進去就不能回成功");
        let LcError::Conflict(body) = err else { panic!("要是 409：{err:?}") };
        assert_eq!((body["error"].as_str(), body["reason"].as_str()), (Some("conflict"), Some("not_awaiting_verdict")), "{body}");
    }

    fn verdicts_for(entries: &[crate::release_triage::Entry]) -> Vec<EntryVerdict> {
        entries
            .iter()
            .filter(|e| e.bucket != Bucket::Dropped)
            .map(|e| EntryVerdict { entry_id: e.id.clone(), verdict: Verdict::None, reason: "r".into(), module: "m".into() })
            .collect()
    }

    fn submission(gen: Option<i64>, version: &str, entries: &[crate::release_triage::Entry]) -> Submission {
        Submission { kind: "claude".into(), version: version.into(), verdicts: verdicts_for(entries), issues: vec![], dispatch_gen: gen }
    }

    /// #801：交辦給一顆 bot 之後，verdict 只收它（或它底下的 child）、只收這一代的。不相干的 bot 一律 403、列不動。
    #[tokio::test]
    async fn only_the_assigned_bot_or_its_child_can_submit_the_current_generation() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sections = source_sections("claude", include_str!("../../../crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"));
        let entries = build_entries("claude", sections.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap();
        ledger::insert_version(&app.db, "claude", "2.1.277", &entries).await.unwrap();
        let worker = crate::testing::claude_bot(&app, &env.project_id, "rt-assignee").await;
        let child = crate::testing::claude_bot(&app, &env.project_id, "rt-assignee-child").await;
        let stranger = crate::testing::claude_bot(&app, &env.project_id, "rt-stranger").await;
        sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&worker.id).bind(&child.id).execute(&app.db).await.unwrap();
        let _ = post_dispatched(State(app.clone()), Json(DispatchedIn { kind: "claude".into(), versions: vec!["2.1.277".into()], bot_id: worker.id.clone() })).await.unwrap();
        let row = ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap();
        assert_eq!((row.assigned_bot_id.as_deref(), row.dispatch_gen), (Some(worker.id.as_str()), 1));

        // 不相干的 bot：帶不帶代數都 403（順序是先看派給誰），列不動。
        for gen in [None, Some(1)] {
            let err = post_verdicts(State(app.clone()), Extension(RequestPrincipal::Bot(stranger.id.clone())), Json(submission(gen, "2.1.277", &entries)))
                .await
                .expect_err("不相干的 bot 不能交 verdict");
            assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");
        }
        assert_eq!(ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap().status, Status::Dispatched);

        // 派給的 bot 自己不帶代數：403 要它帶。
        let err = post_verdicts(State(app.clone()), Extension(RequestPrincipal::Bot(worker.id.clone())), Json(submission(None, "2.1.277", &entries)))
            .await
            .expect_err("綁定的交辦要帶代數");
        assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");

        // 代數對不上：409 stale_assignment，不是 403（派給的人沒錯，只是這份交辦過期了）。
        let err = post_verdicts(State(app.clone()), Extension(RequestPrincipal::Bot(child.id.clone())), Json(submission(Some(0), "2.1.277", &entries)))
            .await
            .expect_err("過期的代數不能交");
        let LcError::Conflict(body) = err else { panic!("要是 409：{err:?}") };
        assert_eq!(body["reason"].as_str(), Some("stale_assignment"), "{body}");

        // 派給的 bot 的 child 用現在這一代交：收。
        let ok = post_verdicts(State(app.clone()), Extension(RequestPrincipal::Bot(child.id.clone())), Json(submission(Some(1), "2.1.277", &entries))).await.unwrap();
        assert_eq!(ok.0["status"], "empty");
        assert_eq!(ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap().status, Status::Empty);
    }

    /// #801：重新派工（代數＋1）之後，舊的交辦不能覆蓋新派工；GET 給 bot 的只有它現在這一代的列。
    #[tokio::test]
    async fn a_superseded_assignment_cannot_overwrite_a_newer_dispatch() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sections = source_sections("claude", include_str!("../../../crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"));
        let entries = build_entries("claude", sections.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap();
        ledger::insert_version(&app.db, "claude", "2.1.277", &entries).await.unwrap();
        let worker = crate::testing::claude_bot(&app, &env.project_id, "rt-first").await;
        let next = crate::testing::claude_bot(&app, &env.project_id, "rt-second").await;
        let dispatch = |bot: &str| DispatchedIn { kind: "claude".into(), versions: vec!["2.1.277".into()], bot_id: bot.to_string() };
        let _ = post_dispatched(State(app.clone()), Json(dispatch(&worker.id))).await.unwrap();
        // 超時退回 pending（真的 requeue 要等六小時，這裡直接改狀態）再派給另一顆。
        sqlx::query("UPDATE release_triage SET status = 'pending' WHERE kind = 'claude' AND version = '2.1.277'").execute(&app.db).await.unwrap();
        let _ = post_dispatched(State(app.clone()), Json(dispatch(&next.id))).await.unwrap();
        let row = ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap();
        assert_eq!((row.assigned_bot_id.as_deref(), row.dispatch_gen), (Some(next.id.as_str()), 2));

        // 第一顆的 bot 已經不是收件方：403（不是 409，它從來沒有這一代的權利）。
        let err = post_verdicts(State(app.clone()), Extension(RequestPrincipal::Bot(worker.id.clone())), Json(submission(Some(1), "2.1.277", &entries)))
            .await
            .expect_err("舊的收件方不能交");
        assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");
        assert_eq!(ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap().status, Status::Dispatched);

        // 同一顆 bot 再被派一次（代數 3）：第一次的代數 2 過期，要 409；用現在的代數就收。
        sqlx::query("UPDATE release_triage SET status = 'pending' WHERE kind = 'claude' AND version = '2.1.277'").execute(&app.db).await.unwrap();
        let _ = post_dispatched(State(app.clone()), Json(dispatch(&next.id))).await.unwrap();
        let err = post_verdicts(State(app.clone()), Extension(RequestPrincipal::Bot(next.id.clone())), Json(submission(Some(2), "2.1.277", &entries)))
            .await
            .expect_err("過期的代數不能交");
        let LcError::Conflict(body) = err else { panic!("要是 409：{err:?}") };
        assert_eq!(body["reason"].as_str(), Some("stale_assignment"), "{body}");
        let ok = post_verdicts(State(app.clone()), Extension(RequestPrincipal::Bot(next.id.clone())), Json(submission(Some(3), "2.1.277", &entries))).await.unwrap();
        assert_eq!(ok.0["status"], "empty");
    }

    /// #801：bot 只看得到派給它（或它底下的 child）、還在 `dispatched` 的列；User 看全部。
    #[tokio::test]
    async fn a_bot_reads_only_its_own_dispatched_rows_and_the_user_reads_all() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sections = source_sections("claude", include_str!("../../../crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"));
        for v in ["2.1.277", "2.1.278"] {
            let entries = build_entries("claude", sections.iter().find(|s| s.version == v).unwrap()).unwrap();
            ledger::insert_version(&app.db, "claude", v, &entries).await.unwrap();
        }
        let mine = crate::testing::claude_bot(&app, &env.project_id, "rt-reader").await;
        let theirs = crate::testing::claude_bot(&app, &env.project_id, "rt-other-reader").await;
        let _ = post_dispatched(State(app.clone()), Json(DispatchedIn { kind: "claude".into(), versions: vec!["2.1.277".into()], bot_id: mine.id.clone() })).await.unwrap();
        let _ = post_dispatched(State(app.clone()), Json(DispatchedIn { kind: "claude".into(), versions: vec!["2.1.278".into()], bot_id: theirs.id.clone() })).await.unwrap();

        let as_bot = get_ledger(State(app.clone()), Extension(RequestPrincipal::Bot(mine.id.clone())), Query(LedgerQuery { kind: Some("claude".into()), version: None })).await.unwrap();
        let rows = as_bot.0["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!((rows[0]["version"].as_str(), rows[0]["dispatch_gen"].as_i64()), (Some("2.1.277"), Some(1)));

        let as_user = get_ledger(State(app.clone()), Extension(RequestPrincipal::User), Query(LedgerQuery { kind: Some("claude".into()), version: None })).await.unwrap();
        assert_eq!(as_user.0["rows"].as_array().unwrap().len(), 2);
    }

    /// #801：派工要指向現存的 bot；打錯或已刪的 id 直接 400，不記成沒人接的交辦。
    #[tokio::test]
    async fn dispatch_must_name_an_existing_bot() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sections = source_sections("claude", include_str!("../../../crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"));
        let entries = build_entries("claude", sections.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap();
        ledger::insert_version(&app.db, "claude", "2.1.277", &entries).await.unwrap();
        let bad = post_dispatched(State(app.clone()), Json(DispatchedIn { kind: "claude".into(), versions: vec!["2.1.277".into()], bot_id: "no-such-bot".into() })).await;
        assert!(matches!(bad, Err(LcError::Bad(_))));
        assert_eq!(ledger::get(&app.db, "claude", "2.1.277").await.unwrap().unwrap().status, Status::Pending);
    }
}
