
    //! #709：同 `quota_claude` 那一條，grok 的探測。
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
        assert_eq!(super::probe_label(&app, HOST).await, format!("am-quota-grok@{tag}"));
        for l in [format!("am-quota-grok@{tag}"), "am-quota-grok@other".into(), "am-quota-grok".into(), "proj".into()] {
            sh.client.workspace_create("/tmp", &l, json!({})).await.unwrap();
        }
        super::sweep_stale_on_hosts(&app).await;
        assert_eq!(labels(&sh.client).await, ["am-quota-grok", "am-quota-grok@other", "proj"]);

        set_shared(&app, false).await;
        assert_eq!(super::probe_label(&app, HOST).await, "am-quota-grok");
        super::sweep_stale_on_hosts(&app).await;
        assert_eq!(labels(&sh.client).await, ["proj"]);
    }
