//! UI 要看的 herdr 版本（每台主機一份）：server 版本＋protocol 來自 `ping`，CLI 版本來自 tools 探測的
//! `herdr --version`。兩者可能不一致（CLI 升了但 server 還在跑舊版，#242 審查）——那時 bot 的 herdr 指令
//! 全回 protocol_mismatch，所以要能看出來。讀不到就是 `null`，不猜。
use serde_json::{json, Value};

/// `server`＝最近一次 ping 成功的 `(version, protocol)`（主機沒連上就傳 None）；`cli`＝`herdr --version` 那行原文。
pub fn summary(server: Option<(&str, u32)>, cli: Option<&str>) -> Value {
    let server_version = server.and_then(|(v, _)| crate::changelog::cli_version_string(v).or_else(|| Some(v.trim().to_string()).filter(|s| !s.is_empty())));
    let cli_version = cli.and_then(crate::changelog::cli_version_string);
    let protocol = server.map(|(_, p)| p);
    let mismatch = matches!((&server_version, &cli_version), (Some(s), Some(c)) if s != c);
    json!({
        "server_version": server_version,
        "protocol": protocol,
        // protocol 不在實測過的清單（herdr.rs `SUPPORTED_PROTOCOLS`）：daemon 的 RPC 形狀沒驗過。
        "protocol_supported": protocol.map(crate::herdr::protocol_supported),
        "cli_version": cli_version,
        "mismatch": mismatch,
    })
}

/// `hosts[].herdr` 與 `host_changed.herdr` 共用：server 只在主機連著時報（斷線後的舊 pong 不算現況）。
pub fn for_host(conn: &crate::hosts::HostConn, connected: bool, detected: Option<&crate::tools::HostTools>) -> Value {
    let pong = connected.then(|| conn.client.last_pong()).flatten();
    summary(pong.as_ref().map(|p| (p.version.as_str(), p.protocol)), detected.and_then(|d| d.herdr_cli.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::{for_host, summary};

    #[test]
    fn reports_server_protocol_and_cli() {
        let v = summary(Some(("0.9.1", 22)), Some("herdr 0.9.1"));
        assert_eq!(v["server_version"], "0.9.1");
        assert_eq!(v["protocol"], 22);
        assert_eq!(v["protocol_supported"], true);
        assert_eq!(v["cli_version"], "0.9.1");
        assert_eq!(v["mismatch"], false);
    }

    #[test]
    fn cli_ahead_of_server_is_a_mismatch() {
        let v = summary(Some(("0.8.2", 20)), Some("herdr 0.9.1"));
        assert_eq!(v["mismatch"], true);
        assert_eq!(v["server_version"], "0.8.2");
        assert_eq!(v["cli_version"], "0.9.1");
    }

    #[test]
    fn unknown_is_null_not_a_guess() {
        let v = summary(None, None);
        assert!(v["server_version"].is_null() && v["protocol"].is_null() && v["cli_version"].is_null());
        assert!(v["protocol_supported"].is_null());
        assert_eq!(v["mismatch"], false, "只知道一邊不能說不一致");
        let only_cli = summary(None, Some("herdr 0.9.1"));
        assert_eq!(only_cli["mismatch"], false);
        assert_eq!(only_cli["cli_version"], "0.9.1");
        assert!(summary(Some(("0.9.1", 22)), Some("garbage"))["cli_version"].is_null());
    }

    #[test]
    fn an_unverified_protocol_is_flagged() {
        assert_eq!(summary(Some(("0.9.2", 23)), None)["protocol_supported"], false);
    }

    #[tokio::test]
    async fn handoff_updates_the_cached_server_version() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let conn = app.hosts.get("local").await.unwrap();
        conn.client.ping().await.unwrap();
        app.tools.lock().await.insert("local".into(), crate::tools::HostTools {
            tools: Default::default(), identities: Default::default(), shell_identities: Default::default(),
            utc_offset_secs: None, herdr_cli: Some("herdr 0.8.2".into()), checked_at: crate::db::now(),
        });
        *env.herdr.pong.lock().unwrap() = ("0.9.1".into(), 22);
        let mut rx = app.subscribe();
        crate::runners::herdr_version::refresh_with(app, "local", Some("herdr 0.9.1".into())).await;
        let v = for_host(&conn, true, app.tools.lock().await.get("local"));
        assert_eq!(v["server_version"], "0.9.1");
        assert_eq!(v["protocol"], 22);
        assert_eq!(v["cli_version"], "0.9.1");
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "host_changed" && ev.data["herdr"]["server_version"] == "0.9.1";
        }
        assert!(pushed, "版本有變要推 host_changed");
    }

    #[tokio::test]
    async fn unreadable_cli_or_server_becomes_null_not_stale() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let conn = app.hosts.get("local").await.unwrap();
        conn.client.ping().await.unwrap();
        app.tools.lock().await.insert("local".into(), crate::tools::HostTools {
            tools: Default::default(), identities: Default::default(), shell_identities: Default::default(),
            utc_offset_secs: None, herdr_cli: Some("herdr 0.9.1".into()), checked_at: crate::db::now(),
        });
        env.herdr.fail_next("ping", crate::testing::Fault::Refuse);
        crate::runners::herdr_version::refresh_with(app, "local", None).await;
        let v = for_host(&conn, true, app.tools.lock().await.get("local"));
        assert!(v["server_version"].is_null() && v["cli_version"].is_null());
    }
    /// #347：A 機量到的 CLI 版本，探測途中主機被換成 B，不能掛到 B 的快取上。
    #[tokio::test]
    async fn a_cli_version_measured_before_a_reconfigure_is_not_attached_to_the_new_host() {
        let app = crate::testing::env().await.app.clone();
        let cfg = |ssh: &str| crate::config::HostCfg { name: "build1".into(), ssh: ssh.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let fence = app.hosts.fence("build1").await.unwrap();
        app.hosts.insert_remote_for_test(cfg("target-b")).await;
        app.tools.lock().await.insert("build1".into(), crate::tools::HostTools {
            tools: Default::default(), identities: Default::default(), shell_identities: Default::default(),
            utc_offset_secs: None, herdr_cli: Some("herdr 0.9.1".into()), checked_at: crate::db::now(),
        });
        crate::runners::herdr_version::refresh_with_fence(&app, "build1", &fence, Some("herdr 0.8.2".into())).await;
        assert_eq!(app.tools.lock().await["build1"].herdr_cli.as_deref(), Some("herdr 0.9.1"), "B 的快取不動");
    }
}
