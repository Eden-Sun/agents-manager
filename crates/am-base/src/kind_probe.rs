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
        #[cfg(any(test, feature = "test-hooks"))]
        {
            let present: Arc<Runner> = Arc::new(|_host, kind, _probe| Some(format!("/test/bin/{kind}")));
            Self(RwLock::new(Some(present)))
        }
        #[cfg(not(any(test, feature = "test-hooks")))]
        {
            Self(RwLock::new(None))
        }
    }
}

/// 先問使用者的登入 shell（`~/.zshrc` 之類才有 PATH 的那種安裝），再退回目前行程的 PATH。
pub fn probe_command(kind: &str) -> String {
    format!("{}; printf '%s\\n' \"$p\"", crate::tools::login_abs_sh(kind))
}

/// 探測結果的判讀：`None`（查不了）不擋；空字串＝確定沒有＝拒絕，並講清楚是哪台機器缺什麼。
pub fn verdict(host: &str, kind: &str, found: Option<String>) -> Result<(), String> {
    match found {
        Some(path) if path.is_empty() || !path.starts_with('/') => {
            let where_ = if host == crate::config::LOCAL_HOST { "本機".to_string() } else { format!("主機 {host}") };
            if kind == "agy" {
                // agy 沒有套件管理器，也不能跑官方 install.sh：AG Man 自己從官方 manifest 裝（SPEC §12a.11）。講清楚去哪裡按，不要只說「請先安裝」。
                return Err(format!(
                    "{where_}尚未安裝 agy。請在額度欄的 agy 格（或新增 bot 的 kind 選單）按「安裝 agy」，由 AG Man 從官方下載並驗 sha512 後裝到 ~/.local/bin/agy（不跑官方 install.sh）；裝好、登入後再啟動這顆 bot。"
                ));
            }
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


/// kind 預檢的接線點。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait KindProbeState: Send + Sync {
    fn kind_probe(&self) -> &crate::kind_probe::KindProbeHook;
}
