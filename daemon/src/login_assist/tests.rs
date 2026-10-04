//! 手機版登入協助：畫面取網址、送 code 的守衛。畫面來自 claude 2.1.289 `auth login` 的真輸出
//! （`lifecycle/fixtures/claude-2.1.289-auth-login-*`；state／code_challenge 換成等長的假值）。

use super::*;
use crate::testing as tt;

const UNWRAPPED: &str = include_str!("../lifecycle/fixtures/claude-2.1.289-auth-login-unwrapped.txt");
const WRAPPED: &str = include_str!("../lifecycle/fixtures/claude-2.1.289-auth-login-wrapped.txt");
const FAILED: &str = include_str!("../lifecycle/fixtures/claude-2.1.289-auth-login-failed.txt");
const M4P_RAW: &str = include_str!("../lifecycle/fixtures/claude-2.1.289-auth-login-m4p.ansi");

const URL: &str = "https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code+user%3Amcp_servers+user%3Afile_upload+user%3Aplugins&code_challenge=FAKEchallengeFAKEchallengeFAKEchallenge0000&code_challenge_method=S256&state=FAKEstateFAKEstateFAKEstateFAKEstate0000000";

/// 終端畫出來的字：拿掉 OSC（超連結 `ESC ] 8 ; ; url BEL`）與 CSI（顏色）。m4p 的真輸出是帶這些的原始位元組。
fn rendered(raw: &str) -> String {
    let mut out = String::new();
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        match (c, it.peek().copied()) {
            ('\u{1b}', Some(']')) => {
                for d in it.by_ref() {
                    if d == '\u{7}' {
                        break;
                    }
                }
            }
            ('\u{1b}', Some('[')) => {
                it.next();
                for d in it.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            ('\r', _) => {}
            _ => out.push(c),
        }
    }
    out
}

#[test]
fn the_real_screens_give_the_url_and_the_prompt() {
    for (name, screen) in [("unwrapped", UNWRAPPED.to_string()), ("wrapped", WRAPPED.to_string()), ("m4p", rendered(M4P_RAW))] {
        let s = parse_screen(&screen);
        assert_eq!(s.url.as_deref(), Some(URL), "{name}");
        assert!(s.awaiting_code, "{name}: 最後一行是 Paste code 提示");
        assert_eq!(s.failure, None, "{name}");
    }
}

#[test]
fn the_wrapped_screen_really_is_wrapped_mid_word() {
    // 前提：折行版的網址分在好幾列、中間沒有空白，單看哪一列都不是完整網址。
    assert!(WRAPPED.lines().filter(|l| l.contains("oauth") || l.starts_with("callback") || l.starts_with("p_servers")).count() >= 3);
    assert!(!WRAPPED.lines().any(|l| l.contains(URL)));
}

#[test]
fn after_a_failed_code_the_cli_is_no_longer_waiting() {
    let s = parse_screen(FAILED);
    assert!(!s.awaiting_code, "提示後面接了 Login failed、shell 提示又回來了");
    assert_eq!(s.failure.as_deref(), Some("Login failed: Request failed with status code 400"));
    assert_eq!(s.url.as_deref(), Some(URL), "網址還在畫面上，但不會被當成可送 code");
}

#[test]
fn the_prompt_must_be_the_last_non_empty_line() {
    let waiting = format!("{UNWRAPPED}\n\n  \n");
    assert!(parse_screen(&waiting).awaiting_code, "後面只有空行：照舊");
    for tail in [" 123abc", "\nuser@host:~$", "\nsomething else"] {
        let screen = format!("{}{tail}", UNWRAPPED.trim_end());
        assert!(!parse_screen(&screen).awaiting_code, "{tail:?}");
    }
    assert!(!parse_screen("user@host:~$ claude auth login\n").awaiting_code, "還沒有提示");
    assert_eq!(parse_screen("").url, None);
}

#[test]
fn only_claude_login_urls_are_handed_to_the_page() {
    assert!(is_login_url(URL));
    for bad in [
        "http://claude.com/cai/oauth/authorize?x=1",
        "https://evil.example/cai/oauth/authorize?x=1",
        "https://claude.com.evil.example/oauth/authorize",
        "https://evilclaude.com/oauth/authorize",
        "https://claude.com@evil.example/oauth/authorize",
        "https://claude.com:8443/oauth/authorize",
        "https://claude.com/cai/other?x=1",
        "https://claude.com/oauth/authorize?x=\"<script>",
        "javascript:alert(1)",
        "",
    ] {
        assert!(!is_login_url(bad), "{bad}");
    }
    assert!(is_login_url("https://platform.claude.com/oauth/authorize?a=b"));
    assert!(is_login_url("https://claude.ai/oauth/authorize?a=b"));
    // 畫面上塞了別的網址：不交出去。
    let evil = UNWRAPPED.replace("https://claude.com/", "https://evil.example/");
    assert_eq!(parse_screen(&evil).url, None);
    // 一列裡網址後面還有字：不往下一列接。
    let trailing = "visit: https://claude.com/oauth/authorize?a=b and then\nTAIL\nPaste code here if prompted >";
    assert_eq!(parse_screen(trailing).url.as_deref(), Some("https://claude.com/oauth/authorize?a=b"));
}

#[test]
fn only_oauth_code_characters_are_typed() {
    for ok in ["abc123", "AbC-def_ghi.jkl~mno", "code#state", "a/b+c=d%20e:f"] {
        assert!(valid_code(ok), "{ok}");
    }
    for bad in ["", "two words", "a;rm -rf /", "$(id)", "`id`", "a|b", "a&b", "x\ny", "x\ty", "a'b", "a\"b", "é", &"a".repeat(MAX_CODE_LEN + 1)] {
        assert!(!valid_code(bad), "{bad:?}");
    }
}

// ---- 走 daemon：fake herdr 的登入 pane ----

struct Fx {
    env: tt::Env,
    pane: String,
}

/// 開一顆 shell、當成 claude 身分 `cc9` 的登入 pane 登記，畫面是 `screen`，前景程序 `claude`。
async fn login_pane(screen: &str) -> Fx {
    let env = tt::env().await;
    let shell = crate::api::shell::open(&env.app, "local", Some("/tmp")).await.unwrap();
    reserve(&env.app, "local", "cc9").unwrap().register(&shell.pane_id, &shell.created_at);
    env.herdr.set_screen(&shell.pane_id, screen);
    // 打字前 daemon 會即時問 herdr／行程樹這顆 pane 現在的樣子（`shell::live_verdict`）：給一棵沒有 listen port 的樹。
    env.herdr.set_shell_pid(&shell.pane_id, 41101);
    *env.app.pane_probe.lock().unwrap() = Arc::new(crate::pane_probe::Fixed::tree(&[(41101, 1, "-zsh")]));
    env.herdr.argvs.lock().unwrap().insert(shell.pane_id.clone(), vec!["claude".into(), "auth".into(), "login".into()]);
    Fx { env, pane: shell.pane_id }
}

impl Fx {
    fn typed(&self) -> Vec<Value> {
        self.env.herdr.calls_to("pane.send_text")
    }
    fn keys(&self) -> Vec<Value> {
        self.env.herdr.calls_to("pane.send_keys").into_iter().filter_map(|c| c.get("keys").cloned()).collect()
    }
}

fn conflict(r: LcResult<Value>) -> Value {
    match r {
        Err(LcError::Conflict(v)) => v,
        other => panic!("expected a 409, got {other:?}"),
    }
}

#[tokio::test]
async fn the_status_gives_the_url_the_identity_and_whether_a_code_is_wanted() {
    let f = login_pane(UNWRAPPED).await;
    let v = status(&f.env.app, "local", &f.pane).await.unwrap();
    assert_eq!(v["url"], URL);
    assert_eq!(v["awaiting_code"], true);
    assert_eq!(v["identity"], "cc9");
    assert_eq!(v["kind"], "claude");
    assert_eq!(v["code_sent"], false);
    assert_eq!(v["failure"], Value::Null);
    // 剛開、CLI 還沒印網址：什麼都沒有，但已經知道這是登入 pane。
    f.env.herdr.set_screen(&f.pane, "user@host:~$ claude auth login\n");
    let v = status(&f.env.app, "local", &f.pane).await.unwrap();
    assert_eq!((v["url"].clone(), v["awaiting_code"].clone()), (Value::Null, json!(false)));
}

/// 不是 daemon 開的登入 pane（沒登記、登記過但 shell 已被收掉、pane id 被另一個 shell 重用）：404，什麼都不讀不打。
#[tokio::test]
async fn only_registered_login_panes_are_served() {
    let env = tt::env().await;
    let plain = crate::api::shell::open(&env.app, "local", Some("/tmp")).await.unwrap();
    env.herdr.set_screen(&plain.pane_id, UNWRAPPED);
    assert!(matches!(status(&env.app, "local", &plain.pane_id).await, Err(LcError::NotFound(_))), "沒登記");
    assert!(matches!(submit_code(&env.app, "local", &plain.pane_id, "abc").await, Err(LcError::NotFound(_))));
    assert!(env.herdr.calls_to("pane.send_text").is_empty());

    // 登記過，但帳上那顆 shell 的身分（created_at）對不上＝pane id 被別的 shell 重用。
    reserve(&env.app, "local", "cc9").unwrap().register(&plain.pane_id, "some-other-shell");
    assert!(matches!(status(&env.app, "local", &plain.pane_id).await, Err(LcError::NotFound(_))));
    assert!(!is_registered(&env.app, "local", &plain.pane_id), "對不上就順手清掉");

    // 關 pane 就忘掉。
    let f = login_pane(UNWRAPPED).await;
    assert!(is_registered(&f.env.app, "local", &f.pane));
    crate::api::shell::close(&f.env.app, "local", &f.pane).await.unwrap();
    assert!(!is_registered(&f.env.app, "local", &f.pane));
    assert!(matches!(status(&f.env.app, "local", &f.pane).await, Err(LcError::NotFound(_))));
}

#[tokio::test]
async fn a_second_live_pane_for_the_same_host_identity_is_rejected() {
    let env = tt::env().await;
    let first = crate::api::shell::open(&env.app, "local", Some("/tmp")).await.unwrap();
    let second = crate::api::shell::open(&env.app, "local", Some("/tmp")).await.unwrap();
    reserve(&env.app, "local", "cc9").unwrap().register(&first.pane_id, &first.created_at);
    let conflict = match reserve(&env.app, "local", "cc9") {
        Err(error) => error,
        Ok(_) => panic!("a second live pane for this identity was reserved"),
    };
    assert!(matches!(conflict, LcError::Conflict(body) if body["reason"] == "identity_login_in_progress"));

    assert!(is_registered(&env.app, "local", &first.pane_id));
    assert!(
        !is_registered(&env.app, "local", &second.pane_id),
        "same host and identity must not have two live OAuth code entry panes"
    );
}

#[tokio::test]
async fn identity_login_reservations_serialize_open_and_release_when_closed() {
    let env = tt::env().await;
    let first = reserve(&env.app, "local", "cc9").unwrap();
    let conflict = match reserve(&env.app, "local", "cc9") {
        Err(error) => error,
        Ok(_) => panic!("same identity was reserved twice"),
    };
    assert!(matches!(conflict, LcError::Conflict(body) if body["reason"] == "identity_login_in_progress"));
    drop(first);

    let second = reserve(&env.app, "local", "cc9").unwrap();
    second.register("w1:p1", "shell-1");
    let conflict = match reserve(&env.app, "local", "cc9") {
        Err(error) => error,
        Ok(_) => panic!("an active login pane did not keep the identity reservation"),
    };
    assert!(matches!(conflict, LcError::Conflict(body) if body["reason"] == "identity_login_in_progress"));
    let other_host = reserve(&env.app, "remote", "cc9").unwrap();
    drop(other_host);

    forget(&env.app, "local", "w1:p1");
    assert!(reserve(&env.app, "local", "cc9").is_ok(), "closing a pane releases the identity");
}

#[tokio::test]
async fn a_code_sent_once_cannot_be_submitted_again_while_the_prompt_is_still_visible() {
    let f = login_pane(UNWRAPPED).await;
    f.env
        .app
        .login_panes
        .lock()
        .unwrap()
        .get_mut(&("local".into(), f.pane.clone()))
        .unwrap()
        .code_sent = true;

    let later = f.env.herdr.set_screen_later();
    let pane = f.pane.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        later(&pane, FAILED);
    });
    let result = submit_code(&f.env.app, "local", &f.pane, "second-code").await;

    assert!(
        matches!(&result, Err(LcError::Conflict(body)) if body["reason"] == "code_already_sent"),
        "a pending or repeated browser request must not type a second code: {result:?}"
    );
    assert!(f.typed().is_empty() && f.keys().is_empty(), "one pane gets one code submission");
}

/// 畫面還在等 code：code 整個打進 pane、再按 Enter；之後 CLI 吐出 `Login failed` 就把那一句回給網頁。
#[tokio::test]
async fn a_code_is_typed_only_while_the_prompt_is_the_last_line() {
    let f = login_pane(UNWRAPPED).await;
    // CLI 收到 code 之後的反應：把畫面換成失敗那一張。
    let later = f.env.herdr.set_screen_later();
    let pane = f.pane.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        later(&pane, FAILED);
    });
    let out = submit_code(&f.env.app, "local", &f.pane, "  fake-code#state123  ").await.unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(out["sent"], true);
    assert_eq!(out["outcome"], "failed");
    assert_eq!(out["message"], "Login failed: Request failed with status code 400");
    assert_eq!(f.typed().iter().map(|c| c["text"].as_str().unwrap().to_string()).collect::<Vec<_>>(), ["fake-code#state123"], "去頭尾空白、整段一次");
    assert_eq!(f.keys(), [json!(["enter"])], "Enter 另送");
    let v = status(&f.env.app, "local", &f.pane).await.unwrap();
    assert_eq!(v["code_sent"], true);
}

#[tokio::test]
async fn a_code_is_refused_when_the_screen_is_not_waiting_for_one() {
    for (why, screen) in [
        ("CLI 已吐出失敗", FAILED.to_string()),
        ("還沒有提示", "user@host:~$ claude auth login\nOpening browser to sign in…\n".to_string()),
        ("提示後面有字", format!("{} typed", UNWRAPPED.trim_end())),
        ("回到 shell", format!("{}\nuser@host:~$ ", UNWRAPPED.trim_end())),
    ] {
        let f = login_pane(&screen).await;
        let body = conflict(submit_code(&f.env.app, "local", &f.pane, "abc123").await);
        assert_eq!(body["reason"], "not_awaiting_code", "{why}: {body}");
        assert_eq!(body["sent"], false, "{why}");
        assert!(f.typed().is_empty() && f.keys().is_empty(), "{why}：一個字、一個鍵都沒送");
    }
}

/// 畫面看起來在等，但 pane 的前景已經不是 claude（讀畫面之後 CLI 結束）：不送，字不能落進 shell。
#[tokio::test]
async fn a_code_is_not_typed_when_claude_is_no_longer_the_foreground_process() {
    let f = login_pane(UNWRAPPED).await;
    f.env.herdr.argvs.lock().unwrap().insert(f.pane.clone(), vec!["-zsh".into()]);
    let body = conflict(submit_code(&f.env.app, "local", &f.pane, "abc123").await);
    assert_eq!(body["reason"], "not_awaiting_code", "{body}");
    assert!(f.typed().is_empty() && f.keys().is_empty());
}

#[tokio::test]
async fn a_malformed_code_is_a_400_before_anything_is_read() {
    let f = login_pane(UNWRAPPED).await;
    for bad in ["", "   ", "a b", "x;y", "$(id)"] {
        assert!(matches!(submit_code(&f.env.app, "local", &f.pane, bad).await, Err(LcError::Bad(_))), "{bad:?}");
    }
    assert!(f.typed().is_empty() && f.keys().is_empty());
}
