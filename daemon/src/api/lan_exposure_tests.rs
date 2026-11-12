use super::origin_is_local;
use axum::http::{HeaderMap, HeaderValue};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut m = HeaderMap::new();
    for (k, v) in pairs {
        m.insert(*k, HeaderValue::from_str(v).unwrap());
    }
    m
}

#[test]
fn a_rebound_hostname_is_refused_in_lan_mode_too() {
    for host in ["evil.example:7788", "evil.example", "127.0.0.1.evil.example:7788", "192.168.1.5.nip.io", "x.ts.net.evil.example", "local"] {
        let ok = origin_is_local(&h(&[("host", host)]), 7788, true);
        // `local` 是單一標籤（允許）；其他都不行。
        assert_eq!(ok, host == "local", "{host}");
    }
    assert!(!origin_is_local(&h(&[("host", "192.168.1.5:7788"), ("origin", "http://evil.example")]), 7788, true), "別的網頁從瀏覽器打過來");
    assert!(!origin_is_local(&h(&[("host", "192.168.1.5:7788"), ("origin", "null")]), 7788, true));
    assert!(!origin_is_local(&h(&[("host", "192.168.1.5:7788"), ("origin", "http://evil.example@192.168.1.5")]), 7788, true));
}

#[test]
fn lan_names_a_user_actually_types_still_pass() {
    for host in [
        "192.168.1.5:7788", "10.0.0.2", "100.64.0.9:7788", "[fd7a:115c:a1e0::1]:7788", "[::1]", "localhost:5173",
        "agm-host.tailnet.ts.net:7788", "box.local", "agm-host", "nas.home.arpa",
    ] {
        assert!(origin_is_local(&h(&[("host", host)]), 7788, true), "{host}");
    }
    assert!(origin_is_local(&h(&[("host", "192.168.1.5:7788"), ("origin", "http://192.168.1.5:7788")]), 7788, true));
    assert!(origin_is_local(&h(&[("host", "agm-host.tailnet.ts.net:7788"), ("origin", "https://agm-host.tailnet.ts.net")]), 7788, true));
    assert!(origin_is_local(&h(&[("host", "192.168.1.5:7788")]), 7788, true), "沒有 Origin 的（curl、同源 GET）照舊");
    assert!(origin_is_local(&h(&[]), 7788, true));
}

/// vite dev server（5173）的 proxy 一律把 Host／Origin 改成 daemon 自己的位址（`changeOrigin` ＋ `rewriteOrigin`），
/// 所以不論使用者用 IP、.ts.net 或 localhost 連 5173，daemon 看到的都是這一組：兩種模式都要放行，不能被 403。
#[test]
fn what_the_vite_proxy_forwards_passes_in_both_modes() {
    for lan in [false, true] {
        assert!(origin_is_local(&h(&[("host", "127.0.0.1:7788"), ("origin", "http://127.0.0.1:7788")]), 7788, lan), "allow_lan={lan}");
    }
}

#[test]
fn an_explicitly_allowed_hostname_passes() {
    assert!(super::lan_host_ok("agm.example.com:7788", &["agm.example.com".to_string()]));
    assert!(!super::lan_host_ok("evil.example.com", &["agm.example.com".to_string()]));
}

async fn raw(app: std::sync::Arc<crate::state::App>, request: &str) -> String {
    let router = super::router(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

/// 端到端：LAN 模式下，rebind 過來的 Host 拿不到 token、也不能用 token 打 /api。
#[tokio::test]
async fn lan_mode_does_not_hand_the_token_to_a_rebound_host() {
    let env = crate::testing::env().await;
    let app = crate::testing::restart_app_lan(&env, true).await;
    let rebound = raw(app.clone(), "GET /api/session HTTP/1.1\r\nHost: evil.example:7788\r\nConnection: close\r\n\r\n").await;
    assert!(rebound.starts_with("HTTP/1.1 403"), "{rebound}");
    assert!(!rebound.contains("test-token"), "{rebound}");
    let lan = raw(app.clone(), "GET /api/session HTTP/1.1\r\nHost: 192.168.1.5:7788\r\nConnection: close\r\n\r\n").await;
    assert!(lan.starts_with("HTTP/1.1 200"), "區網 IP 存取照舊（#556：持有 UI token 就是使用者，allow_lan 的風險使用者已接受）：{lan}");
    let api = raw(app.clone(), "GET /api/state HTTP/1.1\r\nHost: evil.example\r\nX-AM-Token: test-token\r\nConnection: close\r\n\r\n").await;
    assert!(api.starts_with("HTTP/1.1 403"), "有 token 也不行：{api}");
    let ws = raw(app, "GET /ws?token=test-token HTTP/1.1\r\nHost: 192.168.1.5:7788\r\nOrigin: http://evil.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await;
    assert!(ws.starts_with("HTTP/1.1 403"), "別的網頁開 WebSocket：{ws}");
}

/// Host 檢查要蓋住認證層外面的路由。rebind 後的同源頁面讀得到靜態檔，也能對 `/hook` 發 POST；
/// 這兩條以前不看 Host。回應不能把送來的 token 再印出來。
#[tokio::test]
async fn lan_mode_rejects_a_rebound_host_on_static_files_and_hooks() {
    let env = crate::testing::env().await;
    let app = crate::testing::restart_app_lan(&env, true).await;
    let secret = "super-secret-hook-token";
    for req in [
        "GET / HTTP/1.1\r\nHost: evil.example:7788\r\nConnection: close\r\n\r\n",
        "GET /assets/index.js HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
        &format!("POST /hook/claude HTTP/1.1\r\nHost: evil.example:7788\r\nX-AM-Bot-Token: {secret}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"),
        "GET /ws?token=test-token HTTP/1.1\r\nHost: evil.example:7788\r\nUpgrade: websocket\r\nConnection: close\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    ] {
        let resp = raw(app.clone(), req).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{req}\n{resp}");
        assert!(!resp.contains(secret) && !resp.contains("test-token"), "{resp}");
    }
    let local = raw(app, "GET / HTTP/1.1\r\nHost: 127.0.0.1:7788\r\nConnection: close\r\n\r\n").await;
    assert!(!local.starts_with("HTTP/1.1 403"), "loopback 靜態檔照舊：{local}");
}

/// 沒有憑證的呼叫端看到的錯誤不能帶出路徑或內部細節。
#[tokio::test]
async fn unauthenticated_errors_do_not_leak_paths() {
    let env = crate::testing::env().await;
    let data = env.app.data_dir.display().to_string();
    let app = crate::testing::restart_app_lan(&env, true).await;
    for req in [
        "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        "GET /api/bots/x/outbox/file?path=../../etc/passwd HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        "GET /api/nope HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        "POST /hook/claude HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        "POST /relay/announce HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "GET /../../etc/passwd HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    ] {
        let resp = raw(app.clone(), req).await;
        assert!(!resp.contains(&data) && !resp.contains("/home/") && !resp.contains("/Users/") && !resp.contains("root:"), "{req}\n{resp}");
    }
    let state = raw(app, "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await;
    assert!(state.starts_with("HTTP/1.1 401"), "{state}");
}

/// service principal 只能打明列路徑：UI／bot 的路徑（含 /api/state、/api/session 之外的寫入）一律 403。
#[tokio::test]
async fn a_service_principal_is_confined_to_its_listed_paths() {
    let env = crate::testing::env().await;
    let app = crate::testing::restart_app_lan(&env, true).await;
    app.service_tokens.write().unwrap().insert("herdr-upgrade".into(), "svc-token".into());
    for (method, path) in [
        ("GET", "/api/state"), ("GET", "/api/supervisor/state/"), ("POST", "/api/bots/x/prompt"), ("POST", "/api/bots/x/keys"),
        ("GET", "/api/panes/"), ("POST", "/api/services/daemon-swap/restart-window"), ("POST", "/api/services/herdr-upgrade/resume/a/b"),
        ("GET", "/api/capabilities?x=1/../state"), ("DELETE", "/api/hosts/mac_builder"), ("PUT", "/api/drafts/k"),
    ] {
        let resp = raw(app.clone(), &format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: herdr-upgrade\r\nX-AM-Service-Token: svc-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
        let allowed_listed = path.starts_with("/api/capabilities");
        assert!(allowed_listed || resp.starts_with("HTTP/1.1 403") || resp.starts_with("HTTP/1.1 404") || resp.starts_with("HTTP/1.1 405"), "{method} {path}: {}", resp.lines().next().unwrap_or(""));
    }
    // 同時帶 UI token 與 service 身份：拒絕，不降級。
    let mixed = raw(app, "GET /api/capabilities HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: herdr-upgrade\r\nX-AM-Service-Token: svc-token\r\nX-AM-Token: test-token\r\nConnection: close\r\n\r\n").await;
    assert!(mixed.starts_with("HTTP/1.1 401"), "{mixed}");
}
