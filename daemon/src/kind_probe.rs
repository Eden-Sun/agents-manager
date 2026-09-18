//! start_bot 的 preflight「這台主機上有沒有這個 agent 的執行檔」（`lifecycle::start::ensure_kind_installed`）。
//!
//! 找執行檔是**環境事實**（本機登入 shell 的 PATH、遠端 ssh 過去看），不是 daemon 的邏輯。start_bot 的測試由 mock
//! herdr 接手整個 agent 生命週期，真的 claude／codex 根本不會被執行——卻會被這一步偷偷吃掉「跑測試的那台機器」
//! 的 PATH：本機剛好裝了所以綠，外部編譯主機（#104）沒裝，21 條會啟動 agent 的測試必紅（#139）。
//!
//! 所以「怎麼去問」做成 App 上可注入的一環（同 `pane_identity::ProcEnvHook`）：正式 daemon 不設＝照舊跑
//! `command -v`；測試 build 的預設是一個答「有」的假的，不看機器。**判讀**（找不到→明確拒絕、查不了→放行）
//! 與指令本身仍是同一份正式碼，專門的 preflight 測試（下方）用自己的 runner 走過這條路。

use std::sync::{Arc, RwLock};

/// `(host, kind, probe)` → 探測指令的 stdout（去頭尾空白）。`Some("")`＝確定沒有；`None`＝這一步查不了。
pub type Runner = dyn Fn(&str, &str, &str) -> Option<String> + Send + Sync;

pub struct KindProbeHook(RwLock<Option<Arc<Runner>>>);

impl KindProbeHook {
    /// 換掉「怎麼去問」。只有測試需要。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set(&self, runner: Arc<Runner>) {
        *self.0.write().unwrap() = Some(runner);
    }

    /// `None`＝用正式的（本機 `/bin/sh`、遠端 ssh）。
    pub fn get(&self) -> Option<Arc<Runner>> {
        self.0.read().unwrap().clone()
    }
}

impl Default for KindProbeHook {
    fn default() -> Self {
        #[cfg(test)]
        {
            let present: Arc<Runner> = Arc::new(|_host, kind, _probe| Some(format!("/test/bin/{kind}")));
            Self(RwLock::new(Some(present)))
        }
        #[cfg(not(test))]
        {
            Self(RwLock::new(None))
        }
    }
}

/// 先問使用者的登入 shell（`~/.zshrc` 之類才有 PATH 的那種安裝），再退回目前行程的 PATH。
pub fn probe_command(kind: &str) -> String {
    format!("( \"${{SHELL:-/bin/sh}}\" -lic 'command -v {kind}' 2>/dev/null || command -v {kind} 2>/dev/null ) | tail -1")
}

/// 探測結果的判讀：`None`（查不了）不擋；空字串＝確定沒有＝拒絕，並講清楚是哪台機器缺什麼。
pub fn verdict(host: &str, kind: &str, found: Option<String>) -> Result<(), String> {
    match found {
        Some(path) if path.is_empty() => {
            let where_ = if host == crate::config::LOCAL_HOST { "本機".to_string() } else { format!("主機 {host}") };
            Err(format!(
                "{where_}上找不到 `{kind}` 執行檔（用登入 shell 檢查 `command -v {kind}` 沒有結果）。請先在該主機安裝 {kind}，或確認它在登入 shell 的 PATH 中；遠端主機也可在主機設定的 remote_path 補上路徑。"
            ))
        }
        Some(path) => {
            tracing::debug!(host, kind, %path, "kind preflight ok");
            Ok(())
        }
        None => Ok(()),
    }
}

/// preflight 本身的測試：跟 start_bot 的其他測試不同，這裡**要**走過「找不找得到」。用自己的 runner——真的
/// `/bin/sh -c <probe>`，環境釘死在測試給的 `bin/`——所以答案只取決於 stub 在不在，跟跑測試的機器裝了什麼無關
/// （開發機、乾淨 Linux 都一樣）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{start_bot, LcError};
    use crate::testing::{claude_bot, env, Env};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    /// 不走登入 shell（`SHELL` 指到不存在的東西；登入 shell 會把 PATH 換成這台機器的），PATH 只有 `bin` 與系統目錄。
    fn sh_runner(bin: &Path) -> Arc<Runner> {
        let path = format!("{}:/usr/bin:/bin", bin.display());
        Arc::new(move |_host, _kind, probe| {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(probe)
                .env_clear()
                .env("PATH", &path)
                .env("SHELL", "/nonexistent-shell")
                .output()
                .ok()?;
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
    }

    fn stub(bin: &Path, name: &str) {
        std::fs::create_dir_all(bin).unwrap();
        let f = bin.join(name);
        std::fs::write(&f, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn agent_started(e: &Env) -> bool {
        e.herdr.methods().iter().any(|m| m == "agent.start")
    }

    /// PATH 上（這裡是測試自己的 `bin/`）找得到＝放行，而且真的走到 agent.start。
    #[tokio::test]
    async fn a_cli_that_is_on_the_path_passes_the_preflight() {
        let e = env().await;
        let bin = e.dir.join("bin");
        stub(&bin, "claude");
        e.app.kind_probe.set(sh_runner(&bin));
        let bot = claude_bot(&e.app, &e.project_id, "pm").await;
        start_bot(&e.app, &bot.id).await.expect("stub 在 PATH 上，preflight 要過");
        assert!(agent_started(&e));
    }

    /// 真的缺 CLI：start 要拒絕、講清楚缺什麼，而且沒有碰 herdr——不是讓它在 launch_pending 裡默默等 60 秒。
    /// 只有別的 agent 的執行檔（codex）不算數。
    #[tokio::test]
    async fn a_missing_cli_refuses_the_start_and_says_which_one() {
        let e = env().await;
        let bin = e.dir.join("bin");
        stub(&bin, "codex");
        e.app.kind_probe.set(sh_runner(&bin));
        let bot = claude_bot(&e.app, &e.project_id, "pm").await;

        let err = start_bot(&e.app, &bot.id).await.expect_err("PATH 上沒有 claude：不能啟動");
        let LcError::Bad(reason) = err else { panic!("expected LcError::Bad, got {err:?}") };
        assert!(reason.contains("本機上找不到 `claude` 執行檔"), "{reason}");
        assert!(!agent_started(&e), "沒有執行檔就不該碰 herdr 開 agent：{:?}", e.herdr.methods());

        let conv = crate::db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        let note: String = sqlx::query_scalar(
            "SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&conv)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(note, reason, "聊天室裡要看得到同一句原因");
    }

    /// 查不了（探測本身跑不起來）不擋 start：這一步只是提早報錯，不是關卡。
    #[tokio::test]
    async fn a_probe_that_cannot_run_does_not_block_the_start() {
        let e = env().await;
        e.app.kind_probe.set(Arc::new(|_host, _kind, _probe| None));
        let bot = claude_bot(&e.app, &e.project_id, "pm").await;
        start_bot(&e.app, &bot.id).await.expect("查不了就放行");
        assert!(agent_started(&e));
    }

    #[test]
    fn the_reason_names_the_machine_that_lacks_the_cli() {
        assert_eq!(verdict("local", "claude", Some("/usr/bin/claude".into())), Ok(()));
        assert_eq!(verdict("box", "claude", None), Ok(()));
        let local = verdict("local", "codex", Some(String::new())).unwrap_err();
        assert!(local.starts_with("本機上找不到 `codex` 執行檔"), "{local}");
        let remote = verdict("box", "grok", Some(String::new())).unwrap_err();
        assert!(remote.starts_with("主機 box上找不到 `grok` 執行檔"), "{remote}");
    }

    /// 測試 build 的預設 runner 不看機器：沒有任何 stub、也不必裝任何 CLI，每個 kind 都答「有」。
    /// （這是 #139 的根本：21 條 start_bot 測試在沒裝 claude／codex 的外部編譯主機上必紅。）
    #[tokio::test]
    async fn the_test_default_does_not_depend_on_what_the_machine_has_installed() {
        let e = env().await;
        let run = e.app.kind_probe.get().expect("測試 build 預設就有假的 runner");
        for kind in crate::config::KINDS {
            let found = run("local", kind, &probe_command(kind));
            assert!(matches!(&found, Some(p) if !p.is_empty()), "{kind}: {found:?}");
        }
        let bot = claude_bot(&e.app, &e.project_id, "pm").await;
        start_bot(&e.app, &bot.id).await.expect("預設就過 preflight");
    }
}
