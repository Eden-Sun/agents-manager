use super::origin_is_local;
use axum::http::{HeaderMap, HeaderValue};

fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut m = HeaderMap::new();
    for (k, v) in pairs {
        m.insert(*k, HeaderValue::from_str(v).unwrap());
    }
    m
}

#[test]
fn a_rebound_hostname_is_refused_even_without_an_origin_header() {
    assert!(!origin_is_local(&h(&[("host", "evil.example:7788")]), 7788, false), "同源 GET 沒有 Origin，只看 Host 才擋得住");
    assert!(!origin_is_local(&h(&[("host", "127.0.0.1.evil.example:7788")]), 7788, false));
    assert!(!origin_is_local(&h(&[("host", "localhost.evil.example")]), 7788, false));
    assert!(!origin_is_local(&h(&[("host", "evil.example"), ("origin", "http://127.0.0.1:7788")]), 7788, false));
}

#[test]
fn loopback_hosts_and_hostless_clients_still_pass() {
    for host in ["127.0.0.1:7788", "127.0.0.1", "localhost:5173", "localhost", "[::1]:7788", "[::1]", "LOCALHOST:7788"] {
        assert!(origin_is_local(&h(&[("host", host)]), 7788, false), "{host}");
    }
    // 沒有 Host 的不是瀏覽器（HTTP/1.0、內部呼叫）：照舊放行。
    assert!(origin_is_local(&h(&[]), 7788, false));
    assert!(origin_is_local(&h(&[("origin", "http://localhost:5173"), ("host", "127.0.0.1:7788")]), 7788, false));
}

/// 審查（2026-10-01）：Host 的各種寫法。能放行的只有 `127.0.0.1`、`localhost`、`[::1]`（大小寫不拘、可帶純數字 port）；
/// 其他一律拒絕＝fail closed——包括「其實指向本機」的寫法（`localhost.`、`[::ffff:127.0.0.1]`、`127.1`），瀏覽器不會這樣送，
/// 寧可擋掉也不要讓比對變成可被繞的字串遊戲。
#[test]
fn host_shapes_are_matched_exactly_not_by_prefix_or_suffix() {
    for ok in ["127.0.0.1:1", "127.0.0.1:65535", "LocalHost:7788", "[::1]:0", "  localhost:7788  "] {
        assert!(origin_is_local(&h(&[("host", ok)]), 7788, false), "{ok:?}");
    }
    for bad in [
        "localhost.", "localhost.:7788", "127.0.0.1.", "127.1", "2130706433", "0.0.0.0:7788", "[::ffff:127.0.0.1]", "[0:0:0:0:0:0:0:1]",
        "localhost:", "localhost:7788:", "localhost:7788:80", "localhost:+80", "localhost:８０", "localhost:7788@evil.example",
        "evil.example#localhost", "evil.example/localhost", "localhost@evil.example", "user@localhost", "localhost,evil.example",
        "localhost evil.example", "foo.localhost", "localhost.evil.example:7788", "127.0.0.1:7788.evil.example", "[::1]x", "[::1", "::1",
        "ⅼocalhost", "",
    ] {
        assert!(!origin_is_local(&h(&[("host", bad)]), 7788, false), "{bad:?}");
    }
    // Origin 也是整段比對：帶路徑、帳密、別的 scheme、`null` 都不收。
    for bad in ["http://localhost@evil.example", "http://localhost.evil.example", "http://localhost/x", "ftp://localhost", "null", "file://", "http://127.0.0.1:7788.evil.example"] {
        assert!(!origin_is_local(&h(&[("host", "127.0.0.1:7788"), ("origin", bad)]), 7788, false), "origin {bad:?}");
    }
}

#[test]
fn allow_lan_keeps_accepting_lan_names() {
    assert!(origin_is_local(&h(&[("host", "agm-host.tailnet.ts.net:7788")]), 7788, true));
}
