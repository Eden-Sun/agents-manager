
    //! #709：共用 session 的主機上，額度探測 workspace 帶本 daemon 的標記，清殘留時只清自己的。
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
        for l in [format!("am-quota-claude@{tag}"), format!("am-quota-claude-cc1@{tag}"), "am-quota-claude@other".into(), "am-quota-claude".into(), "proj".into()] {
            sh.client.workspace_create("/tmp", &l, json!({})).await.unwrap();
        }
        super::sweep_stale_on_hosts(&app).await;
        assert_eq!(labels(&sh.client).await, ["am-quota-claude", "am-quota-claude@other", "proj"], "別顆 daemon 的（含沒標記的）不動");

        set_shared(&app, false).await;
        assert_eq!(super::probe_label(&app, HOST, Some("cc1")).await, "am-quota-claude-cc1");
        super::sweep_stale_on_hosts(&app).await;
        assert_eq!(labels(&sh.client).await, ["proj"], "不共用時照舊全清");
    }
