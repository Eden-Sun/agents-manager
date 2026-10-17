
    //! #709：共用 session 的主機上，額度探測 workspace 帶本 daemon 的標記，清殘留（#1795：認登記過的 workspace id，不看 label）只清自己登記的。
    use crate::runners::am_base_tests::shared_host::tests::{set_shared, shared_host, HOST};
    use serde_json::json;

    async fn labels(c: &crate::herdr::HerdrClient) -> Vec<String> {
        let mut v: Vec<String> = c.workspace_list().await.unwrap().into_iter().filter_map(|w| w.label).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn a_shared_host_labels_and_sweeps_only_this_daemons_probes() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let tag = crate::shared_host::probe_tag(&app, HOST).await.unwrap();
        assert_eq!(super::probe_label(&app, HOST, Some("cc1")).await, format!("am-quota-claude-cc1@{tag}"));
        // 兩顆是我們開的（登記過、沒確認關掉）；其餘（別顆 daemon 的、同名卻不是我們開的、使用者的）沒登記。
        let mut ours = Vec::new();
        for l in [format!("am-quota-claude@{tag}"), format!("am-quota-claude-cc1@{tag}")] {
            let (ws, _) = sh.client.workspace_create("/tmp", &l, json!({})).await.unwrap();
            drop(crate::probe_ws::ProbeWorkspace::register(sh.client.socket_path(), &ws.workspace_id));
            ours.push(ws.workspace_id);
        }
        for l in ["am-quota-claude@other", "am-quota-claude", "proj"] {
            sh.client.workspace_create("/tmp", l, json!({})).await.unwrap();
        }
        super::sweep_stale_on_hosts(&app).await;
        assert_eq!(labels(&sh.client).await, ["am-quota-claude", "am-quota-claude@other", "proj"], "沒登記的（含別顆 daemon 的、沒標記的）不動");
        assert!(crate::probe_ws::leftovers(sh.client.socket_path()).iter().all(|id| !ours.contains(id)), "收掉之後登記撤掉");

        set_shared(&app, false).await;
        assert_eq!(super::probe_label(&app, HOST, Some("cc1")).await, "am-quota-claude-cc1");
    }
