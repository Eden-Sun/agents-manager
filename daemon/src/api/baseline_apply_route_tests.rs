use super::*;
use crate::host_baseline::{BaselineIssue, BaselineReport};
use axum::response::IntoResponse;

async fn body_of(r: Result<Response, LcError>) -> (StatusCode, Value) {
    let resp = r.unwrap_or_else(|e| e.into_response());
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn apply(app: &Arc<App>, host: &str, principal: RequestPrincipal, plan_rev: &str) -> (StatusCode, Value) {
    let b = BaselineApply { plan_rev: plan_rev.into(), ids: None };
    body_of(baseline_apply(State(app.clone()), Path(host.to_string()), Extension(principal), Json(b)).await).await
}

async fn restore(app: &Arc<App>, host: &str, principal: RequestPrincipal, id: &str) -> (StatusCode, Value) {
    let b = BaselineRestore { backup_id: id.into() };
    body_of(baseline_restore(State(app.clone()), Path(host.to_string()), Extension(principal), Json(b)).await).await
}

async fn backups(app: &Arc<App>, host: &str, principal: RequestPrincipal) -> (StatusCode, Value) {
    body_of(baseline_backups(State(app.clone()), Path(host.to_string()), Extension(principal)).await).await
}

async fn detect_now(app: &Arc<App>, host: &str) {
    let fence = app.hosts.fence(host).await.unwrap();
    let checked_at = app.host_baseline.lock().await.get(host).unwrap().checked_at.clone();
    crate::host_baseline_auto::after_detect(app.clone(), host.into(), fence, checked_at).await;
}

fn hooks_report() -> BaselineReport {
    BaselineReport {
        os: Some("Linux".into()),
        issues: Some(vec![
            BaselineIssue { id: "codex.config.toml:features.hooks".into(), severity: "warn", message: "m".into(), note: None },
            BaselineIssue { id: "tool.jq".into(), severity: "critical", message: "m".into(), note: None },
        ]),
        checked_at: crate::db::now(),
        failed_at: None,
        error: None,
        stale: false,
    }
}

async fn plan_rev(app: &Arc<App>, host: &str) -> String {
    cached_baseline_plan(app, host).await.unwrap().1.plan_rev
}

async fn set_sync(app: &Arc<App>, host: &str, mode: &str) {
    let (host, mode) = (host.to_string(), mode.to_string());
    app.cfg
        .update(move |f| {
            let m: crate::config::HostBaselineSyncMode = serde_json::from_value(json!(mode)).unwrap();
            f.host_baseline.sync.insert(host, m);
            Ok(())
        })
        .await
        .unwrap();
}

/// 遠端主機（假 ssh）：apply／restore／列舉備份腳本在假 HOME 裡用真的 `/bin/sh` 跑，其他（偵測）一律失敗。回傳 HOME 與被叫了幾次。
async fn remote_host(app: &Arc<App>, host: &str) -> (std::path::PathBuf, Arc<std::sync::atomic::AtomicUsize>) {
    let cfg = HostCfg { name: host.into(), ssh: "unused".into(), herdr_session: "agents-manager".into(), ..Default::default() };
    let c2 = cfg.clone();
    app.cfg.update(move |f| {
        f.hosts.push(c2);
        Ok(())
    }).await.unwrap();
    app.hosts.insert_remote_for_test(cfg).await;
    let home = crate::testing::scratch_dir("baseline-apply-home");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (h, n) = (home.clone(), calls.clone());
    crate::hosts::set_ssh_fake(host, move |script| {
        if !script.contains("AM_BA begin") && !script.contains("AM_BR begin") && !script.contains("AM_BK begin") {
            anyhow::bail!("test fake: only baseline scripts run here");
        }
        n.fetch_add(1, Ordering::SeqCst);
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env_clear().env("HOME", &h).env("PATH", "/usr/bin:/bin").output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    });
    app.host_baseline.lock().await.insert(host.into(), hooks_report());
    (home, calls)
}

/// 只給使用者；沒開（沒列＝off、明寫 off、鍵對不上）409 `sync_disabled`；plan_rev 不符 409；指名手動項 400。主機一個字都沒碰。
#[tokio::test]
async fn apply_is_user_only_off_by_default_and_checks_the_plan_rev() {
    let e = crate::testing::env().await;
    let host = "bl3a-guard";
    let (home, calls) = remote_host(&e.app, host).await;
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex/config.toml"), "a = 1\n").unwrap();
    let rev = plan_rev(&e.app, host).await;
    for p in [RequestPrincipal::Bot("b1".into()), RequestPrincipal::Service("svc".into())] {
        let (st, body) = apply(&e.app, host, p, &rev).await;
        assert_eq!((st, body["reason"].as_str()), (StatusCode::FORBIDDEN, Some("user_only")), "{body}");
    }
    let (st, _) = apply(&e.app, "no-such-host", RequestPrincipal::User, &rev).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, body) = apply(&e.app, host, RequestPrincipal::User, &rev).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("sync_disabled")), "unlisted = off: {body}");
    set_sync(&e.app, host, "off").await;
    let (st, body) = apply(&e.app, host, RequestPrincipal::User, &rev).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("sync_disabled")), "{body}");
    set_sync(&e.app, "renamed-away", "ask").await;
    let (st, body) = apply(&e.app, host, RequestPrincipal::User, &rev).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("sync_disabled")), "an orphan key turns nothing on: {body}");

    set_sync(&e.app, host, "auto").await;
    let (st, body) = apply(&e.app, host, RequestPrincipal::User, &rev).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("auto_owns_sync")), "auto owns the sync; manual apply is refused: {body}");
    set_sync(&e.app, host, "ask").await;
    let (st, body) = apply(&e.app, host, RequestPrincipal::User, "not-the-rev").await;
    assert_eq!((st, body["reason"].as_str(), body["plan_rev"].as_str()), (StatusCode::CONFLICT, Some("plan_outdated"), Some(rev.as_str())), "{body}");
    let b = BaselineApply { plan_rev: rev.clone(), ids: Some(vec!["tool.jq".into()]) };
    let (st, body) = body_of(baseline_apply(State(e.app.clone()), Path(host.into()), Extension(RequestPrincipal::User), Json(b)).await).await;
    assert_eq!((st, body["reason"].as_str(), body["id"].as_str()), (StatusCode::BAD_REQUEST, Some("not_fixable"), Some("tool.jq")), "{body}");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no guard path may reach the host");
    assert_eq!(std::fs::read(home.join(".codex/config.toml")).unwrap(), b"a = 1\n");
}

/// 開了（`ask`）：補上缺的鍵、回備份 id 與每項結果（手動項照列），回應不帶路徑或內容；
/// 之後 sync 改回 off，restore 照樣能用，還原後逐 byte 相同，回應只有目標名與結果。
#[tokio::test]
async fn apply_then_restore_round_trips_and_restore_works_while_off() {
    let e = crate::testing::env().await;
    let host = "bl3a-run";
    let (home, calls) = remote_host(&e.app, host).await;
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    let orig = "approval_policy = \"AM-SECRET\"\n[features]\nx = 1\n";
    std::fs::write(home.join(".codex/config.toml"), orig).unwrap();
    set_sync(&e.app, host, "ask").await;
    let rev = plan_rev(&e.app, host).await;
    let (st, body) = apply(&e.app, host, RequestPrincipal::User, &rev).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["results"], json!([
        {"id": "codex.config.toml:features.hooks", "result": "applied"},
    ]), "tool.jq needs sudo on Linux: manual in the plan and never offered: {body}");
    let backup_id = body["backup_id"].as_str().unwrap().to_string();
    assert!(crate::host_baseline::apply::valid_backup_id(&backup_id), "{body}");
    assert!(body["error"].is_null(), "{body}");
    let text = body.to_string();
    assert!(!text.contains("AM-SECRET") && !text.contains(home.to_str().unwrap()) && !text.contains("baseline-backup"), "{text}");
    let now = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert!(now.contains("[features]\nhooks = true\n"), "{now}");
    assert!(home.join(crate::host_baseline::apply::BACKUP_ROOT_REL).join(&backup_id).join("manifest").is_file());

    set_sync(&e.app, host, "off").await;
    for (id, want) in [("../x", StatusCode::BAD_REQUEST), ("20261024T120000Z-01JABC", StatusCode::BAD_REQUEST)] {
        let (st, body) = restore(&e.app, host, RequestPrincipal::User, id).await;
        assert_eq!((st, body["reason"].as_str()), (want, Some("invalid_backup_id")), "{body}");
    }
    let (st, body) = restore(&e.app, host, RequestPrincipal::Bot("b1".into()), &backup_id).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::FORBIDDEN, Some("user_only")), "{body}");
    let (st, body) = restore(&e.app, host, RequestPrincipal::User, "20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4").await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::NOT_FOUND, Some("backup_not_found")), "{body}");
    let (st, body) = restore(&e.app, host, RequestPrincipal::User, &backup_id).await;
    assert_eq!(st, StatusCode::OK, "restore works while sync is off: {body}");
    assert_eq!(body, json!({"host": host, "backup_id": backup_id, "files": [{"file": "codex/config.toml", "result": "restored"}]}));
    assert_eq!(std::fs::read(home.join(".codex/config.toml")).unwrap(), orig.as_bytes());

    let m = home.join(crate::host_baseline::apply::BACKUP_ROOT_REL).join(&backup_id).join("manifest");
    let bad = std::fs::read_to_string(&m).unwrap().replace("codex/config.toml", "../../.ssh/config");
    std::fs::write(&m, bad).unwrap();
    let (st, body) = restore(&e.app, host, RequestPrincipal::User, &backup_id).await;
    assert_eq!((st, body["reason"].as_str(), body["refused"].as_str()), (StatusCode::CONFLICT, Some("restore_refused"), Some("manifest_invalid")), "{body}");
    assert!(calls.load(Ordering::SeqCst) >= 4);
}

/// 開始之後主機換代（重連／改指）：`run_if_current` 擋下，409 `host_superseded`，腳本一次都沒送。
#[tokio::test]
async fn a_superseded_host_gets_no_apply_or_restore() {
    let e = crate::testing::env().await;
    let host = "bl3a-fence";
    let (home, calls) = remote_host(&e.app, host).await;
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex/config.toml"), "a = 1\n").unwrap();
    set_sync(&e.app, host, "ask").await;
    let rev = plan_rev(&e.app, host).await;
    let fence = e.app.hosts.fence(host).await.unwrap();
    e.app.hosts.bump_generation_for_test(host).await;
    let b = BaselineApply { plan_rev: rev, ids: None };
    let (st, body) = body_of(baseline_apply_with_fence(&e.app, host, &fence, b).await).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("host_superseded")), "{body}");
    let (st, body) = body_of(baseline_restore_with_fence(&e.app, host, &fence, "20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4").await).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("host_superseded")), "{body}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(home.join(".codex/config.toml")).unwrap(), b"a = 1\n");
}

/// 孤兒鍵只在使用者的 `/api/state`；bot 的視圖沒有這一欄。
#[tokio::test]
async fn orphan_sync_keys_are_in_the_user_state_only() {
    let e = crate::testing::env().await;
    set_sync(&e.app, "gone-host", "ask").await;
    set_sync(&e.app, crate::config::LOCAL_HOST, "ask").await;
    let Json(user) = get_state(State(e.app.clone()), Extension(RequestPrincipal::User)).await.unwrap();
    assert_eq!(user["host_baseline_orphan_sync_keys"], json!(["gone-host"]));
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "bl3a-bot").await;
    let Json(view) = get_state(State(e.app.clone()), Extension(RequestPrincipal::Bot(bot.id))).await.unwrap();
    assert!(view.as_object().unwrap().get("host_baseline_orphan_sync_keys").is_none(), "{view}");
    assert!(!view.to_string().contains("gone-host"), "{view}");
}

/// #719 階段 3b：每台主機生效的模式只在使用者的 `/api/state`——local 與 `[[hosts]]` 都列、`auto` 輸出成 "auto"、沒列是 "off"、
/// 孤兒鍵不列；bot 的視圖沒有這一欄。
#[tokio::test]
async fn the_effective_sync_mode_is_in_the_user_state_only() {
    let e = crate::testing::env().await;
    let (_home, _calls) = remote_host(&e.app, "bl3b-auto").await;
    let (_home2, _calls2) = remote_host(&e.app, "bl3b-unset").await;
    set_sync(&e.app, crate::config::LOCAL_HOST, "ask").await;
    set_sync(&e.app, "bl3b-auto", "auto").await;
    set_sync(&e.app, "gone-host", "ask").await;
    let Json(user) = get_state(State(e.app.clone()), Extension(RequestPrincipal::User)).await.unwrap();
    let sync = user["host_baseline_sync"].as_object().expect("使用者有這欄");
    assert_eq!(sync.get(crate::config::LOCAL_HOST), Some(&json!("ask")), "{sync:?}");
    assert_eq!(sync.get("bl3b-auto"), Some(&json!("auto")), "auto 輸出 auto：{sync:?}");
    assert_eq!(sync.get("bl3b-unset"), Some(&json!("off")), "沒列＝off：{sync:?}");
    assert!(sync.get("gone-host").is_none(), "孤兒鍵不列：{sync:?}");
    assert!(sync.values().all(|v| v == "off" || v == "ask" || v == "auto"), "{sync:?}");
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "bl3b-bot").await;
    let Json(view) = get_state(State(e.app.clone()), Extension(RequestPrincipal::Bot(bot.id))).await.unwrap();
    assert!(view.as_object().unwrap().get("host_baseline_sync").is_none(), "{view}");
}

#[tokio::test]
async fn backup_list_is_user_only_read_only_and_available_while_off() {
    let e = crate::testing::env().await;
    let host = "bl3a-backup-list";
    let (home, calls) = remote_host(&e.app, host).await;
    let root = home.join(crate::host_baseline::apply::BACKUP_ROOT_REL);
    let good_id = "20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4";
    let good = root.join(good_id);
    std::fs::create_dir_all(good.join("files")).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex/config.toml"), "current AM-SECRET\n").unwrap();
    std::fs::write(good.join("files/1"), "backup AM-SECRET\n").unwrap();
    std::fs::write(
        good.join("manifest"),
        format!("am-baseline-backup 1\nid {good_id}\nplan_rev rev1\nfile 1 1 644 codex/config.toml\n"),
    )
    .unwrap();
    let bad_id = "20261024T120001Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c5";
    let bad = root.join(bad_id);
    std::fs::create_dir_all(bad.join("files")).unwrap();
    std::fs::write(bad.join("manifest"), format!("am-baseline-backup 1\nid {bad_id}\nfile 1 1 600 /etc/passwd\n")).unwrap();
    let link_id = "20261024T120002Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c6";
    std::os::unix::fs::symlink(&good, root.join(link_id)).unwrap();

    for principal in [RequestPrincipal::Bot("b1".into()), RequestPrincipal::Service("svc".into())] {
        let (status, body) = backups(&e.app, host, principal).await;
        assert_eq!((status, body["reason"].as_str()), (StatusCode::FORBIDDEN, Some("user_only")), "{body}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "unauthorized requests do not query the host");

    let (status, body) = backups(&e.app, host, RequestPrincipal::User).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["skipped"], 2, "invalid manifest and symlink are counted");
    assert_eq!(body.as_object().unwrap().keys().cloned().collect::<std::collections::BTreeSet<_>>(), ["backups", "skipped"].into_iter().map(str::to_string).collect());
    assert_eq!(body["backups"][0]["backup_id"], good_id);
    assert_eq!(body["backups"][0]["created_at"], "2026-10-24T12:00:00Z");
    assert_eq!(body["backups"][0]["files"], json!([{"file":"codex/config.toml","restore":"overwrite"}]));
    let text = body.to_string();
    assert!(!text.contains("AM-SECRET") && !text.contains(home.to_str().unwrap()) && !text.contains("baseline-backup") && !text.contains("/etc/passwd"), "{text}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn auto_writes_once_after_a_detect_and_ask_or_off_never_touch_the_host() {
    let e = crate::testing::env().await;
    let host = "bl3-auto-run";
    let (home, _calls) = remote_host(&e.app, host).await;
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    let orig = "approval_policy = \"never\"\n";
    std::fs::write(home.join(".codex/config.toml"), orig).unwrap();
    e.app.host_baseline.lock().await.insert(host.into(), hooks_report());
    let backups = || std::fs::read_dir(home.join(crate::host_baseline::apply::BACKUP_ROOT_REL)).map(|d| d.count()).unwrap_or(0);

    for mode in ["off", "ask"] {
        set_sync(&e.app, host, mode).await;
        detect_now(&e.app, host).await;
        assert_eq!(std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(), orig, "{mode} must write nothing");
        assert_eq!(backups(), 0, "{mode} must not back anything up");
    }

    set_sync(&e.app, host, "auto").await;
    detect_now(&e.app, host).await;
    let now = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    let v: toml::Value = toml::from_str(&now).unwrap();
    assert_eq!(v["features"]["hooks"].as_bool(), Some(true), "{now}");
    assert_eq!(v["approval_policy"].as_str(), Some("never"), "the existing value is never touched: {now}");
    for k in ["model", "model_reasoning_effort", "service_tier"] {
        assert!(v.get(k).is_none(), "{k} is never written: {now}");
    }
    assert_eq!(backups(), 1, "one backup before the one write");

    detect_now(&e.app, host).await;
    assert_eq!(std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(), now, "a second pass on the same plan does nothing");
    assert_eq!(backups(), 1, "no re-entry on the same plan");
}

#[tokio::test]
async fn a_manual_apply_waits_its_turn_behind_the_host_sync_lock() {
    let e = crate::testing::env().await;
    let host = "bl3-lock";
    let (home, _calls) = remote_host(&e.app, host).await;
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex/config.toml"), "approval_policy = \"never\"\n").unwrap();
    e.app.host_baseline.lock().await.insert(host.into(), hooks_report());
    set_sync(&e.app, host, "ask").await;
    let rev = plan_rev(&e.app, host).await;
    let held = crate::host_baseline_auto::host_lock(host).lock_owned().await;
    let (st, body) = apply(&e.app, host, RequestPrincipal::User, &rev).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("sync_busy")), "{body}");
    drop(held);
    let (st, _) = apply(&e.app, host, RequestPrincipal::User, &rev).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn a_detect_fence_from_before_a_reconnect_never_writes_to_the_new_connection() {
    let e = crate::testing::env().await;
    let host = "bl3-stale";
    let (home, calls) = remote_host(&e.app, host).await;
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    let orig = "approval_policy = \"never\"\n";
    std::fs::write(home.join(".codex/config.toml"), orig).unwrap();
    e.app.host_baseline.lock().await.insert(host.into(), hooks_report());
    let checked_at = e.app.host_baseline.lock().await.get(host).unwrap().checked_at.clone();
    set_sync(&e.app, host, "auto").await;
    let old = e.app.hosts.fence(host).await.unwrap();
    e.app.hosts.bump_generation_for_test(host).await;
    let before = calls.load(Ordering::SeqCst);
    crate::host_baseline_auto::after_detect(e.app.clone(), host.into(), old, checked_at).await;
    assert_eq!(calls.load(Ordering::SeqCst), before, "a superseded fence reaches the host");
    assert_eq!(std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(), orig);
}

#[tokio::test]
async fn a_report_replaced_after_its_detect_is_not_applied() {
    let e = crate::testing::env().await;
    let host = "bl3-replaced";
    let (home, calls) = remote_host(&e.app, host).await;
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    let orig = "approval_policy = \"never\"\n";
    std::fs::write(home.join(".codex/config.toml"), orig).unwrap();
    e.app.host_baseline.lock().await.insert(host.into(), hooks_report());
    set_sync(&e.app, host, "auto").await;
    let fence = e.app.hosts.fence(host).await.unwrap();
    let before = calls.load(Ordering::SeqCst);
    crate::host_baseline_auto::after_detect(e.app.clone(), host.into(), fence, "1999-01-01T00:00:00Z".into()).await;
    assert_eq!(calls.load(Ordering::SeqCst), before, "a report from another detect must not be applied");
    assert_eq!(std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(), orig);
}
