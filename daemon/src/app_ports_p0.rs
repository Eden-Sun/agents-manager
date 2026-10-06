//! P0：把 `App` 上「feature 自己的狀態欄位」包成各 feature 自己定義的單一能力 trait（`App` 只在這個 composition 檔出現）。
//! 這些 trait 各在擁有該狀態的模組裡，所以 feature 不必依賴 `capabilities`／`state`；欄位照舊住在 `App`，行為不變。

use crate::state::App;
use std::sync::Arc;

impl crate::quota::QuotaTables for App {
    fn quotas(&self) -> &tokio::sync::Mutex<std::collections::BTreeMap<String, crate::quota::Quota>> {
        &self.quotas
    }
}
impl<T: crate::quota::QuotaTables + ?Sized> crate::quota::QuotaTables for Arc<T> {
    fn quotas(&self) -> &tokio::sync::Mutex<std::collections::BTreeMap<String, crate::quota::Quota>> {
        (**self).quotas()
    }
}

impl crate::quota::QuotaStaleKeys for App {
    fn quota_stale(&self) -> &tokio::sync::Mutex<std::collections::BTreeSet<String>> {
        &self.quota_stale
    }
}
impl<T: crate::quota::QuotaStaleKeys + ?Sized> crate::quota::QuotaStaleKeys for Arc<T> {
    fn quota_stale(&self) -> &tokio::sync::Mutex<std::collections::BTreeSet<String>> {
        (**self).quota_stale()
    }
}

impl crate::background_jobs::JobCounts for App {
    fn background_jobs(&self) -> &crate::background_jobs::Counts {
        &self.background_jobs
    }
}
impl<T: crate::background_jobs::JobCounts + ?Sized> crate::background_jobs::JobCounts for Arc<T> {
    fn background_jobs(&self) -> &crate::background_jobs::Counts {
        (**self).background_jobs()
    }
}

impl crate::background_hook::HookSnapshots for App {
    fn background_hook(&self) -> &crate::background_hook::Snapshots {
        &self.background_hook
    }
}
impl<T: crate::background_hook::HookSnapshots + ?Sized> crate::background_hook::HookSnapshots for Arc<T> {
    fn background_hook(&self) -> &crate::background_hook::Snapshots {
        (**self).background_hook()
    }
}

impl crate::deploy_wait::DeployWaitState for App {
    fn deploy_wait(&self) -> &std::sync::Mutex<Option<crate::deploy_wait::Wait>> {
        &self.deploy_wait
    }
}
impl<T: crate::deploy_wait::DeployWaitState + ?Sized> crate::deploy_wait::DeployWaitState for Arc<T> {
    fn deploy_wait(&self) -> &std::sync::Mutex<Option<crate::deploy_wait::Wait>> {
        (**self).deploy_wait()
    }
}

impl crate::api::shell::HostShells for App {
    fn host_shells(&self) -> &crate::api::shell::Registry {
        &self.host_shells
    }
}
impl<T: crate::api::shell::HostShells + ?Sized> crate::api::shell::HostShells for Arc<T> {
    fn host_shells(&self) -> &crate::api::shell::Registry {
        (**self).host_shells()
    }
}

impl crate::login_assist::LoginPanes for App {
    fn login_panes(&self) -> &crate::login_assist::Registry {
        &self.login_panes
    }
}
impl<T: crate::login_assist::LoginPanes + ?Sized> crate::login_assist::LoginPanes for Arc<T> {
    fn login_panes(&self) -> &crate::login_assist::Registry {
        (**self).login_panes()
    }
}

impl crate::login_prompt::LoginNeeded for App {
    fn login_needed(&self) -> &crate::login_prompt::Registry {
        &self.login_needed
    }
}
impl<T: crate::login_prompt::LoginNeeded + ?Sized> crate::login_prompt::LoginNeeded for Arc<T> {
    fn login_needed(&self) -> &crate::login_prompt::Registry {
        (**self).login_needed()
    }
}

impl crate::gh_auth::GhDevice for App {
    fn gh_device(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::gh_auth::DeviceSession>> {
        &self.gh_device
    }
}
impl<T: crate::gh_auth::GhDevice + ?Sized> crate::gh_auth::GhDevice for Arc<T> {
    fn gh_device(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::gh_auth::DeviceSession>> {
        (**self).gh_device()
    }
}
