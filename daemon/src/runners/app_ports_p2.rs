//! P2 config／projection 的窄介面在 daemon 裡組起來的地方（crate 拆分第 3 步）。每個方法逐行委派給原本被呼叫的函式，
//! 不加任何邏輯，所以行為與錯誤型別不變。介面本身在 `am-config`（`ConfigChangeHooks`）與 `projection.rs`
//! （`SupervisorOwnedSource`）；這個檔案是 config 唯一還知道 projection／config_audit／supervisor_owned 的地方
//! （composition root 側的 adapter，不屬於 am-config）。
//!
//! 模組掛在 `runners/mod.rs`，projection 本身不宣告 composition adapter。

use crate::config::{ConfigChangeHooks, ConfigFile, ConfigStore};
use crate::projection::SupervisorOwnedSource;
use anyhow::Result;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type At = &'static std::panic::Location<'static>;

/// daemon 的 `ConfigChangeHooks`：投影驗證＝`projection::validate`，稽核＝`config_audit::log_*`。
pub struct DaemonConfigHooks;

impl ConfigChangeHooks for DaemonConfigHooks {
    fn validate_projection(&self, next: &ConfigFile) -> Result<()> {
        crate::projection::validate(next)
    }
    fn audit_external_change(&self, at: At, path: &Path, old: &ConfigFile, new: &ConfigFile) {
        crate::config_audit::log_external_change(at, path, old, new)
    }
    fn audit_reload_unchanged(&self, at: At, path: &Path) {
        crate::config_audit::log_reload_unchanged(at, path)
    }
    fn audit_write(&self, at: At, path: &Path, old: &ConfigFile, new: &ConfigFile) {
        crate::config_audit::log_write(at, path, old, new)
    }
}

/// daemon 的入口：帶 [`DaemonConfigHooks`] 載入。放在 adapter 檔而不是 `am-config`，這樣 config 本身不認識
/// projection／config_audit（`am-config` 是獨立 crate，inherent impl 不能留在 daemon，所以是自由函式）。
pub async fn load_config(path: PathBuf) -> Result<ConfigStore> {
    ConfigStore::load_with_hooks(path, Arc::new(DaemonConfigHooks)).await
}

impl SupervisorOwnedSource for SqlitePool {
    async fn load_owned(&self) -> Result<crate::projection::Owned> {
        crate::supervisor_owned::load(self).await
    }
    async fn ops_alert(&self, reason: &str, subject: &str, detail: &str) {
        crate::supervisor_owned::alert(self, reason, subject, detail).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 每一行是否落在 `#[cfg(test)]` 項目裡（從屬性那行到項目的大括號收尾）。
    fn test_mask(src: &str) -> Vec<bool> {
        let lines: Vec<&str> = src.lines().collect();
        let mut mask = vec![false; lines.len()];
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim_start().starts_with("#[cfg(test)]") {
                let (mut depth, mut seen, mut j) = (0i32, false, i);
                while j < lines.len() {
                    mask[j] = true;
                    depth += lines[j].matches('{').count() as i32 - lines[j].matches('}').count() as i32;
                    seen |= lines[j].contains('{');
                    if seen && depth <= 0 {
                        break;
                    }
                    if !seen && lines[j].trim_end().ends_with(';') && !lines[j].trim_start().starts_with("#[") {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        mask
    }

    fn offenders(file: &str, src: &str, forbidden: &[&str]) -> Vec<String> {
        let mask = test_mask(src);
        let mut out = Vec::new();
        for (n, line) in src.lines().enumerate() {
            if mask[n] || line.trim_start().starts_with("//") {
                continue;
            }
            for pat in forbidden {
                if line.contains(pat) {
                    out.push(format!("{file}:{}: {pat}  ← {}", n + 1, line.trim()));
                }
            }
        }
        out
    }

    /// 埠 0 不是合法的 ssh 埠：手改進 `[[hosts]]`／`[build.remote]` 要在寫入前就被擋，而不是等到連線才失敗。
    /// （原在 `config.rs` 的測試；`projection::validate` 在 daemon，所以搬到這裡。）
    #[test]
    fn a_zero_ssh_port_is_refused_before_it_can_be_written() {
        let mut cfg = ConfigFile::default();
        cfg.hosts.push(crate::config::HostCfg { shared_session: false, name: "m4p".into(), ssh: "m4p@host".into(), ssh_port: 0, ssh_opts: vec![], herdr_session: "s".into(), remote_path: String::new() });
        let err = crate::projection::validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("ssh_port"), "{err}");
        let mut cfg = ConfigFile::default();
        cfg.build.remote.ssh_port = 0;
        assert!(crate::projection::validate(&cfg).unwrap_err().to_string().contains("ssh_port"));
    }

    #[test]
    fn projection_reaches_supervisor_owned_only_through_the_port() {
        let found = offenders("projection.rs", include_str!("../projection.rs"), &["supervisor_owned::load(", "supervisor_owned::alert("]);
        assert!(found.is_empty(), "projection 要走 SupervisorOwnedSource：\n{}", found.join("\n"));
    }

    /// 反向確認：護欄禁的呼叫真的在 adapter 裡（改名或搬走時這條先紅）。
    #[test]
    fn the_adapter_still_calls_what_the_guards_forbid_elsewhere() {
        let adapter = include_str!("app_ports_p2.rs");
        for pat in ["projection::validate(", "config_audit::log_external_change(", "config_audit::log_reload_unchanged(", "config_audit::log_write(", "supervisor_owned::load(", "supervisor_owned::alert("] {
            assert!(adapter.contains(pat), "adapter 不再含 {pat}");
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<&'static str>>);
    impl ConfigChangeHooks for Recorder {
        fn validate_projection(&self, _next: &ConfigFile) -> Result<()> {
            self.0.lock().unwrap().push("validate");
            Ok(())
        }
        fn audit_external_change(&self, _at: At, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {
            self.0.lock().unwrap().push("external");
        }
        fn audit_reload_unchanged(&self, _at: At, _path: &Path) {
            self.0.lock().unwrap().push("unchanged");
        }
        fn audit_write(&self, _at: At, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {
            self.0.lock().unwrap().push("write");
        }
    }

    #[tokio::test]
    async fn the_store_calls_the_injected_hooks_in_order_and_a_refusing_hook_blocks_the_write() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-config-hooks-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let rec = Arc::new(Recorder::default());
        struct Shared(Arc<Recorder>);
        impl ConfigChangeHooks for Shared {
            fn validate_projection(&self, n: &ConfigFile) -> Result<()> {
                self.0.validate_projection(n)
            }
            fn audit_external_change(&self, at: At, p: &Path, o: &ConfigFile, n: &ConfigFile) {
                self.0.audit_external_change(at, p, o, n)
            }
            fn audit_reload_unchanged(&self, at: At, p: &Path) {
                self.0.audit_reload_unchanged(at, p)
            }
            fn audit_write(&self, at: At, p: &Path, o: &ConfigFile, n: &ConfigFile) {
                self.0.audit_write(at, p, o, n)
            }
        }
        let store = ConfigStore::load_with_hooks(path.clone(), Arc::new(Shared(rec.clone()))).await.unwrap();
        store.update(|c| { c.server.herdr_session = "changed".into(); Ok(()) }).await.unwrap();
        assert_eq!(*rec.0.lock().unwrap(), vec!["validate", "write"]);

        struct Refuse;
        impl ConfigChangeHooks for Refuse {
            fn validate_projection(&self, _n: &ConfigFile) -> Result<()> {
                anyhow::bail!("nope")
            }
            fn audit_external_change(&self, _: At, _: &Path, _: &ConfigFile, _: &ConfigFile) {}
            fn audit_reload_unchanged(&self, _: At, _: &Path) {}
            fn audit_write(&self, _: At, _: &Path, _: &ConfigFile, _: &ConfigFile) {
                panic!("a refused change must not be audited as written");
            }
        }
        let before = std::fs::read_to_string(&path).unwrap();
        let store = ConfigStore::load_with_hooks(path.clone(), Arc::new(Refuse)).await.unwrap();
        let err = store.update(|c| { c.server.herdr_session = "other".into(); Ok(()) }).await.unwrap_err();
        assert!(format!("{err:#}").contains("nope"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "驗不過就一個字都不寫");
    }
}
