// Blanket Arc forwarding for base-owned state traits; kept beside the traits
// so the daemon composition crate does not violate Rust orphan rules.
use std::sync::Arc;

impl<T: crate::quota::QuotaTables + ?Sized> crate::quota::QuotaTables for Arc<T> {
    fn quotas(&self) -> &tokio::sync::Mutex<std::collections::BTreeMap<String, crate::quota::Quota>> {
        (**self).quotas()
    }
}

impl<T: crate::quota::QuotaStaleKeys + ?Sized> crate::quota::QuotaStaleKeys for Arc<T> {
    fn quota_stale(&self) -> &tokio::sync::Mutex<std::collections::BTreeSet<String>> {
        (**self).quota_stale()
    }
}

impl<T: crate::quota::HostIdentities + ?Sized> crate::quota::HostIdentities for Arc<T> {
    async fn identity_for_host(&self, host: &str, name: &str) -> Option<crate::config::IdentityCfg> {
        (**self).identity_for_host(host, name).await
    }
    async fn host_tools_detected(&self, host: &str) -> bool {
        (**self).host_tools_detected(host).await
    }
}

impl<T: crate::background_hook::HookSnapshots + ?Sized> crate::background_hook::HookSnapshots for Arc<T> {
    fn background_hook(&self) -> &crate::background_hook::Snapshots {
        (**self).background_hook()
    }
}

impl<T: crate::login_assist::LoginPanes + ?Sized> crate::login_assist::LoginPanes for Arc<T> {
    fn login_panes(&self) -> &crate::login_assist::Registry {
        (**self).login_panes()
    }
}

impl<T: crate::login_prompt::LoginNeeded + ?Sized> crate::login_prompt::LoginNeeded for Arc<T> {
    fn login_needed(&self) -> &crate::login_prompt::Registry {
        (**self).login_needed()
    }
}

impl<T: crate::github::GithubCache + ?Sized> crate::github::GithubCache for Arc<T> {
    fn github(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, Option<crate::github::GithubInfo>>> {
        (**self).github()
    }
}

impl<T: crate::tools::ToolsTable + ?Sized> crate::tools::ToolsTable for Arc<T> {
    fn tools(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::tools::HostTools>> {
        (**self).tools()
    }
}

impl<T: crate::build_scheduler::BuildSlotLock + ?Sized> crate::build_scheduler::BuildSlotLock for Arc<T> {
    fn build_slot_lock(&self) -> &tokio::sync::Mutex<()> {
        (**self).build_slot_lock()
    }
}

impl<T: crate::upstream_update::UpstreamWatch + ?Sized> crate::upstream_update::UpstreamWatch for Arc<T> {
    fn upstream_watch(&self) -> &crate::upstream_update::Watch {
        (**self).upstream_watch()
    }
}

impl<T: crate::shim_refresh::RemoteShimStale + ?Sized> crate::shim_refresh::RemoteShimStale for Arc<T> {
    fn remote_shim_stale(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, String>> {
        (**self).remote_shim_stale()
    }
}

impl<T: crate::credential_spawn::CredentialSpawnGate + ?Sized> crate::credential_spawn::CredentialSpawnGate for Arc<T> {
    fn credential_spawn_gate(&self) -> &std::sync::Mutex<crate::credential_spawn::Gate> {
        (**self).credential_spawn_gate()
    }
}

impl<T: crate::changelog::ChangelogState + ?Sized> crate::changelog::ChangelogState for Arc<T> {
    fn changelog(&self) -> &crate::changelog::ChangelogCache {
        (**self).changelog()
    }
}

impl<T: crate::herdr::LocalHerdr + ?Sized> crate::herdr::LocalHerdr for Arc<T> {
    fn default_herdr(&self) -> &crate::herdr::HerdrClient {
        (**self).default_herdr()
    }
    fn herdr_session(&self) -> &String {
        (**self).herdr_session()
    }
    fn default_connected(&self) -> &std::sync::atomic::AtomicBool {
        (**self).default_connected()
    }
}

impl<T: crate::models::ModelsCache + ?Sized> crate::models::ModelsCache for Arc<T> {
    fn models_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, serde_json::Value)>> {
        (**self).models_cache()
    }
}

impl<T: crate::login_assist::LoginReservations + ?Sized> crate::login_assist::LoginReservations for Arc<T> {
    fn login_reservations(&self) -> &crate::login_assist::Reservations {
        (**self).login_reservations()
    }
}

impl<T: crate::host_baseline::HostBaselineTable + ?Sized> crate::host_baseline::HostBaselineTable for Arc<T> {
    fn host_baseline(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::host_baseline::BaselineReport>> {
        (**self).host_baseline()
    }
}

impl<T: crate::github::SubmodulesCache + ?Sized> crate::github::SubmodulesCache for Arc<T> {
    fn submodules_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Vec<crate::github::Submodule>)>> {
        (**self).submodules_cache()
    }
}

impl<T: crate::github::IssuesCache + ?Sized> crate::github::IssuesCache for Arc<T> {
    fn issues_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, serde_json::Value)>> {
        (**self).issues_cache()
    }
}

impl<T: crate::kind_probe::KindProbeState + ?Sized> crate::kind_probe::KindProbeState for Arc<T> {
    fn kind_probe(&self) -> &crate::kind_probe::KindProbeHook {
        (**self).kind_probe()
    }
}
