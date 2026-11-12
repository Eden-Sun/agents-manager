use super::*;
use crate::config::host_target_problem;

/// `ssh_opts` is not purely cosmetic: `User` and `IdentityFile` can attach existing runs to a
/// different remote account. A live project's pane ids must not cross that identity change
/// without the same explicit repoint confirmation as a new ssh target.
#[test]
fn changing_ssh_identity_options_is_a_host_repoint() {
    let old = HostCfg { daemon_url: None,
        name: "build-box".into(),
        ssh: "build-box".into(),
        ssh_port: 22,
        ssh_opts: vec!["-o".into(), "User=alice".into(), "-i".into(), "/keys/alice".into()],
        herdr_session: "agents-manager".into(),
        remote_path: String::new(),
        shared_session: false,
    };
    let mut changed_user = old.clone();
    changed_user.ssh_opts[1] = "User=bob".into();
    assert!(repoints_host(&old, &changed_user), "SSH User changes the pane authority");

    let mut changed_key = old.clone();
    changed_key.ssh_opts[3] = "/keys/bob".into();
    assert!(repoints_host(&old, &changed_key), "IdentityFile can select another remote account");
}

/// `POST /api/hosts` 的 `ssh` 是 ssh 的一個 argv、`herdr_session` 會進遠端路徑與 plist：形狀不對一律 400，不寫進 config。
#[test]
fn a_host_target_cannot_smuggle_an_ssh_option_or_a_path() {
    for ok in ["mac_builder@100.64.0.2", "build-box", "ssh-alias", "user@host.example.com", "[::1]", "10.0.0.5"] {
        assert_eq!(host_target_problem(ok, "agents-manager"), None, "{ok}");
    }
    for bad in ["-oProxyCommand=touch /tmp/pwned", "-F/tmp/evil", "host name", "host\nname", "ho\tst", "a\u{0}b", ""] {
        assert!(host_target_problem(bad, "agents-manager").is_some(), "{bad:?}");
    }
    for ok in ["agents-manager", "am_v40", "Session.1"] {
        assert_eq!(host_target_problem("box", ok), None, "{ok}");
    }
    for bad in ["../../etc", "a/b", "-x", ".hidden", "a b", "a;b", "a\"b", "<x>", "a&b", "x\ny"] {
        assert!(host_target_problem("box", bad).is_some(), "{bad:?}");
    }
}

fn opts(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// `ssh_opts` 是「原樣附加到每個 ssh 指令」：`-oProxyCommand=…`／`LocalCommand` 就是在**跑 daemon 的這台機器**上執行任意命令。
/// `POST /api/hosts` 任何 principal（含 bot 的 hook token）都能呼叫，以前只驗 `ssh` 與 `herdr_session`，這個欄位完全沒驗。
/// 只放行真的用得到的：`-i 檔`、`-o 白名單鍵=值`（dev sshd 與 ConnectTimeout 那類）。
#[test]
fn ssh_opts_cannot_carry_a_command_to_run_locally() {
    for ok in [
        opts(&[]),
        opts(&["-o", "ConnectTimeout=2"]),
        opts(&["-i", "/home/u/.ssh/id_ed25519", "-o", "UserKnownHostsFile=/tmp/k", "-o", "StrictHostKeyChecking=yes"]),
        opts(&["-oConnectTimeout=5", "-oStrictHostKeyChecking=no"]),
        opts(&["-o", "serveraliveinterval=30"]),
        opts(&["-4"]),
    ] {
        assert_eq!(crate::config::ssh_opts_problem(&ok), None, "{ok:?}");
    }
    for bad in [
        opts(&["-oProxyCommand=touch /tmp/pwned"]),
        opts(&["-o", "ProxyCommand=touch /tmp/pwned"]),
        opts(&["-o", "proxycommand=touch /tmp/pwned"]),
        opts(&["-o", "ProxyCommand touch /tmp/pwned"]),
        opts(&["-o", "PermitLocalCommand=yes", "-o", "LocalCommand=touch /tmp/pwned"]),
        opts(&["-o", "KnownHostsCommand=touch /tmp/pwned"]),
        opts(&["-o", "Include=/tmp/evil.conf"]),
        opts(&["-F", "/tmp/evil.conf"]),
        opts(&["-J", "-oProxyCommand=true"]),
        opts(&["-i", "-oProxyCommand=true"]),
        opts(&["-o"]),
        opts(&["-o", "ConnectTimeout"]),
        opts(&["host"]),
        opts(&["-o", "ConnectTimeout=2\nProxyCommand=true"]),
        opts(&["-L", "8080:localhost:80"]),
        opts(&["-p", "2222"]),
    ] {
        assert!(crate::config::ssh_opts_problem(&bad).is_some(), "{bad:?}");
    }
    assert!(host_target_problem("me@-oProxyCommand=true", "agents-manager").is_some(), "@ 後面以 - 開頭的主機名也會被當成選項");
}

#[tokio::test]
async fn create_host_refuses_ssh_opts_that_run_a_command_before_touching_config() {
    let e = crate::testing::env().await;
    let before = e.app.cfg.get().await.hosts.len();
    let err = create_host(
        State(e.app.clone()),
        Query(DeleteQuery::default()),
        Extension(RequestPrincipal::User),
        Json(NewHost { name: "evil2".into(), ssh: "me@10.0.0.9".into(), ssh_port: None, ssh_opts: Some(opts(&["-o", "ProxyCommand=touch /tmp/pwned"])), herdr_session: None, remote_path: None, shared_session: None }),
    )
    .await
    .expect_err("ProxyCommand must be refused");
    assert!(matches!(err, LcError::Bad(_)), "{err:?}");
    assert_eq!(e.app.cfg.get().await.hosts.len(), before, "什麼都沒寫");
}

#[tokio::test]
async fn create_host_refuses_a_target_that_looks_like_an_option_before_touching_config() {
    let e = crate::testing::env().await;
    let before = e.app.cfg.get().await.hosts.len();
    let err = create_host(
        State(e.app.clone()),
        Query(DeleteQuery::default()),
        Extension(RequestPrincipal::User),
        Json(NewHost { name: "evil1".into(), ssh: "-oProxyCommand=true".into(), ssh_port: None, ssh_opts: None, herdr_session: None, remote_path: None, shared_session: None }),
    )
    .await
    .expect_err("an option-looking target must be refused");
    assert!(matches!(err, LcError::Bad(_)), "{err:?}");
    assert_eq!(e.app.cfg.get().await.hosts.len(), before, "什麼都沒寫");
}

/// `ssh_port` 0 不是合法的 TCP 埠（`remote_cargo` 的 `[build.remote]` 也明確拒絕），以前照樣寫進 config，
/// 之後每次連線才以 `ssh -p 0` 失敗，錯誤跑到很後面才出現。
#[tokio::test]
async fn create_host_refuses_ssh_port_zero_before_touching_config() {
    let e = crate::testing::env().await;
    let before = e.app.cfg.get().await.hosts.len();
    let err = create_host(
        State(e.app.clone()),
        Query(DeleteQuery::default()),
        Extension(RequestPrincipal::User),
        Json(NewHost { name: "port0".into(), ssh: "me@10.0.0.9".into(), ssh_port: Some(0), ssh_opts: None, herdr_session: None, remote_path: None, shared_session: None }),
    )
    .await
    .expect_err("ssh_port 0 must be refused");
    // 設定不合法是請求的問題：400 `config_invalid`＋`config_written:false`（同 `cfg_err`），不是 502。
    assert!(matches!(&err, LcError::BadValue(v) if v["error"] == "config_invalid" && v["config_written"] == false), "{err:?}");
    assert_eq!(e.app.cfg.get().await.hosts.len(), before, "什麼都沒寫");
}
