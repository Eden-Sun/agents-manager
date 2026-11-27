use super::*;
use axum::{extract::State, http::HeaderMap, http::StatusCode, routing::{get, post}, Json, Router};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

const TZ: chrono_tz::Tz = chrono_tz::Asia::Taipei;

fn jwt(payload: Value) -> String {
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
    format!("eyJhbGciOiJub25lIn0.{b}.c2ln")
}

fn now() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_790_000_000, 0).unwrap()
}

fn grants_body() -> Value {
    json!({
        "object": "credit_summary",
        "total_granted": 120.0,
        "total_used": 37.5,
        "total_available": 82.5,
        "total_paid_available": 50.0,
        "grants": {"data": [
            {"grant_amount": 100.0, "used_amount": 30.0, "effective_at": 1_780_000_000.0, "expires_at": 1_798_732_800.0},
            {"grant_amount": 20.0, "used_amount": 7.5, "effective_at": 1_780_000_000, "expires_at": null}
        ]}
    })
}

#[test]
fn parses_credit_grants_and_converts_expiry_to_the_user_zone() {
    let d = parse_credit_grants(&grants_body(), TZ).unwrap();
    assert_eq!((d.total_available_usd, d.total_granted_usd, d.total_used_usd), (82.5, 120.0, 37.5));
    assert_eq!(d.grants, vec![
        Grant { remaining_usd: 70.0, total_usd: 100.0, expires_on: Some("2027-01-01".into()) },
        Grant { remaining_usd: 12.5, total_usd: 20.0, expires_on: None },
    ]);
    assert_eq!(epoch_to_day(1_798_732_800.0, chrono_tz::UTC).as_deref(), Some("2026-12-31"), "UTC 還是前一天，台北已跨日");
    assert!(parse_credit_grants(&json!({"object": "credit_summary"}), TZ).is_none(), "沒有 total_available 算解析失敗");
}

fn org(id: &str, personal: bool) -> Org {
    Org { id: id.into(), title: Some(format!("t-{id}")), personal }
}

#[test]
fn org_choice_prefers_config_then_the_only_business_org_then_first() {
    let orgs = vec![org("org-p", true), org("org-b", false)];
    assert_eq!(pick_org(&orgs, None).unwrap().id, "org-b");
    assert_eq!(pick_org(&orgs, Some("org-p")).unwrap().id, "org-p");
    assert!(pick_org(&orgs, Some("org-x")).is_none(), "指定的 org 不在清單就不亂挑");
    let two = vec![org("org-p", true), org("org-1", false), org("org-2", false)];
    assert_eq!(pick_org(&two, None).unwrap().id, "org-p");
    assert_eq!(pick_org(&[org("org-p", true)], Some("  ")).unwrap().id, "org-p");
    assert!(pick_org(&[], None).is_none());
}

#[test]
fn refresh_only_when_less_than_a_day_left() {
    let n = now().timestamp();
    assert!(!needs_refresh(Some(n + 25 * 3600), now()));
    assert!(needs_refresh(Some(n + 23 * 3600), now()));
    assert!(needs_refresh(Some(n - 10), now()));
    assert!(!needs_refresh(None, now()));
    let c = jwt_claims(&jwt(json!({"exp": n, "client_id": "app_X"}))).unwrap();
    assert_eq!(c, JwtClaims { exp: Some(n), client_id: Some("app_X".into()) });
    assert!(jwt_claims("not-a-jwt").is_none());
}

#[derive(Default)]
struct Mock {
    login_status: StdMutex<u16>,
    token_status: StdMutex<u16>,
    login_calls: AtomicUsize,
    token_calls: AtomicUsize,
    grants_headers: StdMutex<Option<(String, String)>>,
    login_auth: StdMutex<Option<String>>,
    token_body: StdMutex<Option<Value>>,
    user_agents: StdMutex<Vec<String>>,
}

fn record_ua(m: &Mock, h: &HeaderMap) {
    m.user_agents.lock().unwrap().push(h.get("user-agent").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default());
}

async fn login(State(m): State<Arc<Mock>>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    m.login_calls.fetch_add(1, Ordering::SeqCst);
    record_ua(&m, &h);
    *m.login_auth.lock().unwrap() = h.get("authorization").map(|v| v.to_str().unwrap().to_string());
    let code = *m.login_status.lock().unwrap();
    let body = json!({"user": {"session": {"sensitive_id": "sess-SECRET"}, "orgs": {"data": [
        {"id": "org-p", "title": "Personal", "personal": true},
        {"id": "org-b", "title": "Biz", "personal": false}
    ]}}});
    (StatusCode::from_u16(code).unwrap(), Json(if code == 200 { body } else { json!({"error": {"message": "bad token eyJx sess-y"}}) }))
}

async fn grants(State(m): State<Arc<Mock>>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    record_ua(&m, &h);
    let auth = h.get("authorization").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    let org = h.get("openai-organization").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    *m.grants_headers.lock().unwrap() = Some((auth.clone(), org));
    if auth != "Bearer sess-SECRET" {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"code": "invalid_api_key"}})));
    }
    (StatusCode::OK, Json(grants_body()))
}

async fn token(State(m): State<Arc<Mock>>, h: HeaderMap, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
    record_ua(&m, &h);
    m.token_calls.fetch_add(1, Ordering::SeqCst);
    *m.token_body.lock().unwrap() = Some(b);
    let code = *m.token_status.lock().unwrap();
    let new_access = jwt(json!({"exp": 1_800_000_000i64, "client_id": "app_X"}));
    (StatusCode::from_u16(code).unwrap(), Json(if code == 200 { json!({"access_token": new_access, "refresh_token": "rt.NEW", "expires_in": 864000}) } else { json!({"error": "invalid_grant"}) }))
}

async fn serve(login_status: u16, token_status: u16) -> (Arc<Mock>, Endpoints) {
    let m = Arc::new(Mock::default());
    *m.login_status.lock().unwrap() = login_status;
    *m.token_status.lock().unwrap() = token_status;
    let app = Router::new()
        .route("/login", post(login))
        .route("/grants", get(grants))
        .route("/token", post(token))
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (m, Endpoints { login: format!("{base}/login"), credit_grants: format!("{base}/grants"), token: format!("{base}/token") })
}

fn state_file(dir: &Path, exp: i64) -> PathBuf {
    let p = dir.join("openai-console-state.json");
    let v = json!({"access_token": jwt(json!({"exp": exp, "client_id": "app_X"})), "refresh_token": "rt.OLD", "id_token": "eyJid.tok.en"});
    std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    crate::claude_console_balance::set_mode_600(&p).unwrap();
    p
}

fn assert_no_secrets(v: &Value) {
    let s = v.to_string();
    for bad in ["eyJ", "sess-", "rt."] {
        assert!(!s.contains(bad), "{bad} leaked: {s}");
    }
}

async fn run(st: &OpenAiConsoleState, ep: &Endpoints, path: &Path, org_id: Option<&str>) -> Value {
    let o = fetch_outcome(st, ep, path, org_id, TZ, now()).await;
    let snap = match o {
        Outcome::Ok(d) => ConsoleSnapshot { status: Status::Ok, data: Some(d), fetched_at: "t".into(), error: None },
        Outcome::LoggedOut(w) => ConsoleSnapshot { status: Status::LoggedOut, data: None, fetched_at: "t".into(), error: Some(w.into()) },
        Outcome::Error(e) => ConsoleSnapshot { status: Status::Error, data: None, fetched_at: "t".into(), error: Some(e) },
    };
    console_json(Some(&snap), true)
}

#[tokio::test]
async fn ok_flow_exchanges_a_session_key_and_reads_the_business_org() {
    let (m, ep) = serve(200, 200).await;
    let dir = crate::testing::scratch_dir("am-oai-console");
    let p = state_file(dir.as_path(), now().timestamp() + 5 * 86400);
    let st = OpenAiConsoleState::default();
    let v = run(&st, &ep, &p, None).await;
    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(v["total_available_usd"], 82.5);
    assert_eq!(v["org_title"], "Biz");
    assert_eq!(v["grants"][0], json!({"remaining_usd": 70.0, "total_usd": 100.0, "expires_on": "2027-01-01"}));
    assert_eq!(m.grants_headers.lock().unwrap().clone().unwrap(), ("Bearer sess-SECRET".to_string(), "org-b".to_string()));
    assert!(m.login_auth.lock().unwrap().as_deref().unwrap().starts_with("Bearer eyJ"));
    assert_eq!(m.token_calls.load(Ordering::SeqCst), 0, "還有 5 天不續期");
    assert!(!v.to_string().contains("sess-"));

    let v = run(&st, &ep, &p, Some("org-p")).await;
    assert_eq!((v["org_title"].clone(), m.grants_headers.lock().unwrap().clone().unwrap().1), (json!("Personal"), "org-p".to_string()));
    assert_eq!(run(&st, &ep, &p, Some("org-zzz")).await["error"], "no_org");
}

#[tokio::test]
async fn near_expiry_refreshes_and_writes_back_0600_via_rename() {
    use std::os::unix::fs::PermissionsExt;
    let (m, ep) = serve(200, 200).await;
    let dir = crate::testing::scratch_dir("am-oai-console");
    let p = state_file(dir.as_path(), now().timestamp() + 3600);
    let st = OpenAiConsoleState::default();
    let v = run(&st, &ep, &p, None).await;
    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(m.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(m.token_body.lock().unwrap().clone().unwrap(), json!({"grant_type": "refresh_token", "client_id": "app_X", "refresh_token": "rt.OLD"}));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(saved["refresh_token"], "rt.NEW", "refresh token 輪替要寫回");
    assert_eq!(saved["id_token"], "eyJid.tok.en", "沒回的欄位保留");
    assert_eq!(jwt_claims(saved["access_token"].as_str().unwrap()).unwrap().exp, Some(1_800_000_000));
    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    let names: Vec<String> = std::fs::read_dir(dir.as_path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["openai-console-state.json".to_string()], "暫存檔不留下");
    let login_auth = m.login_auth.lock().unwrap().clone().unwrap();
    assert_eq!(login_auth, format!("Bearer {}", saved["access_token"].as_str().unwrap()), "login 用新的 access token");
    let uas = m.user_agents.lock().unwrap().clone();
    assert_eq!(uas.len(), 3, "token、login、grants 各一次：{uas:?}");
    assert!(uas.iter().all(|ua| ua.starts_with("agents-manager/") && ua.len() > "agents-manager/".len()), "api.openai.com 對沒有 User-Agent 的請求回 403：{uas:?}");
}

#[tokio::test]
async fn refresh_failure_is_logged_out_and_not_retried_with_the_same_token() {
    let (m, ep) = serve(200, 400).await;
    let dir = crate::testing::scratch_dir("am-oai-console");
    let p = state_file(dir.as_path(), now().timestamp() - 60);
    let before = std::fs::read(&p).unwrap();
    let st = OpenAiConsoleState::default();
    let v = run(&st, &ep, &p, None).await;
    assert_eq!(v["status"], "logged_out", "{v}");
    assert_no_secrets(&v);
    let v = run(&st, &ep, &p, None).await;
    assert_eq!(v["status"], "logged_out");
    assert_eq!(m.token_calls.load(Ordering::SeqCst), 1, "同一把 refresh token 不重試");
    assert_eq!(m.login_calls.load(Ordering::SeqCst), 0, "access token 已過期就不打 login");
    assert_eq!(std::fs::read(&p).unwrap(), before, "失敗不動登入態檔");

    state_file(dir.as_path(), now().timestamp() - 60);
    let st2 = OpenAiConsoleState::default();
    *m.token_status.lock().unwrap() = 200;
    assert_eq!(run(&st2, &ep, &p, None).await["status"], "ok");
}

#[tokio::test]
async fn login_401_is_logged_out_and_errors_carry_no_tokens() {
    let (_m, ep) = serve(401, 200).await;
    let dir = crate::testing::scratch_dir("am-oai-console");
    let p = state_file(dir.as_path(), now().timestamp() + 5 * 86400);
    let st = OpenAiConsoleState::default();
    let v = run(&st, &ep, &p, None).await;
    assert_eq!(v["status"], "logged_out", "{v}");
    assert_no_secrets(&v);

    let (_m, ep) = serve(500, 200).await;
    let v = run(&st, &ep, &p, None).await;
    assert_eq!((v["status"].clone(), v["error"].clone()), (json!("error"), json!("login_http_500")));
    assert_no_secrets(&v);

    let dead = Endpoints { login: "http://127.0.0.1:1/login".into(), ..ep.clone() };
    let v = run(&st, &dead, &p, None).await;
    assert_eq!((v["status"].clone(), v["error"].clone()), (json!("error"), json!("network")));
    assert_no_secrets(&v);

    std::fs::write(&p, "{not json eyJ").unwrap();
    let v = run(&st, &ep, &p, None).await;
    assert_eq!(v["error"], "state_parse");
    assert_no_secrets(&v);
}

#[test]
fn console_json_states() {
    assert_eq!(console_json(None, false), json!({"status": "not_configured"}));
    assert_eq!(console_json(None, true), json!({"status": "error", "error": "pending"}));
}

#[test]
fn config_defaults() {
    let c = am_config::openai_api::OpenAiApiCfg::default();
    assert!(c.console_balance_enabled);
    assert_eq!(c.console_balance_interval_minutes, 30);
    assert_eq!(c.console_state_file, "~/.config/agents-manager/openai-console-state.json");
    assert!(c.console_org_id.is_none());
}
