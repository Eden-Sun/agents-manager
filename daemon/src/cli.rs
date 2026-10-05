//! 命令列定義（clap）。住在 lib 裡是因為有測試要拿它驗 hook／statusLine 指令的 argv（`lifecycle::setup`、`build_info`）。

use crate::build_info;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "agents-managerd", version = build_info::VERSION)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run the daemon.
    Serve {
        /// Override the config path (default ~/.config/agents-manager/config.toml)
        #[arg(long)]
        config: Option<PathBuf>,
        /// M1 aid: also subscribe to agent status for every pane already in the session.
        #[arg(long)]
        dev_watch_all_panes: bool,
    },
    /// Hook callback invoked by the agent CLI. Always exits 0 with empty stdout.
    Hook {
        /// claude | codex | grok | agy (claude, grok and agy deliver the payload on stdin, codex via argv)
        provider: String,
        #[arg(long)]
        bot: String,
        /// Optional; falls back to `$AM_HOOK_TOKEN` (preferred — keeps the token out of `ps`).
        #[arg(long, default_value = "")]
        token: String,
        #[arg(long, default_value_t = 7788)]
        port: u16,
        /// 這顆 hook 屬於哪顆 daemon 的資料目錄（daemon 啟動 bot 時寫進 hook.sh）。
        #[arg(long, default_value = "")]
        data_dir: String,
        /// agy 的 payload 不帶事件名，由 hooks.json／statusLine 的指令參數（dispatcher 的 `$1`）告訴我們。
        #[arg(long, default_value = "")]
        event: String,
        /// Codex passes the event JSON as the last argv element.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        payload: Vec<String>,
    },
    /// herdr 有沒有新版、對我們有沒有影響（issue #66）：純函式，不碰網路、不執行任何指令，也不會
    /// 觸發升級。三個輸入都由呼叫端（`scripts/ops/herdr-update-kick.sh`）自己去問；這裡只保證版本
    /// 比較是數值比較（不是字串比較，`457dd14` 那個 `0.9.0` 判成比 `0.10.0` 新的坑），CHANGELOG
    /// 段落擷取沒抓漏。JSON 印到 stdout；exit code：0＝比較成功（不論有沒有更新），2＝版本號看不懂。
    HerdrUpdateCheck {
        /// `herdr --version` 讀到的本機版本。
        #[arg(long)]
        installed: String,
        /// GitHub release／Homebrew 查到的最新穩定版。
        #[arg(long)]
        latest: String,
        /// herdr 的 CHANGELOG 全文所在檔案。
        #[arg(long)]
        changelog_file: PathBuf,
        /// 上次真的派過工的版本（`herdr-update.last` 記的那個），沒有就省略。
        #[arg(long)]
        last_notified: Option<String>,
    },
    /// 上游新版分診（issue #204）：抓 claude／codex 的 changelog，把 `(帳本已分診的最大版本, 最新正式版]`
    /// 每一版切成逐條 entry、用 `release_triage/rules.toml` 分桶、記進帳本，JSON 印到 stdout（`--json`；
    /// 契約 `{kind,from,to,pending:[{version,kept,unmatched,dropped_count}]}`）。抓不到 feed 時 exit 1，
    /// 這一輪不做（不當成沒有新版）。第一次跑只記磁碟版本當基準、不回 pending。
    ReleaseTriageCheck {
        /// claude | codex
        #[arg(long)]
        kind: String,
        /// 補歷史：從這一版（不含）起算，忽略帳本裡的最大版本。
        #[arg(long)]
        since: Option<String>,
        /// 輸出 JSON（目前唯一格式，旗標留給 kick 腳本明示）。
        #[arg(long)]
        json: bool,
        /// 指定「磁碟上的版本」，省得第一次跑時去問 login shell（測試用）。
        #[arg(long, hide = true)]
        installed: Option<String>,
        /// 帳本所在的 SQLite（預設 `AM_DATA_DIR`／預設資料目錄底下的 agents-manager.sqlite3）。
        #[arg(long, hide = true)]
        db: Option<PathBuf>,
        /// 不抓網路，直接讀這個 feed 檔（測試用）。
        #[arg(long, hide = true)]
        feed_file: Option<PathBuf>,
    },
    /// Claude Code statusLine command for daemon-started claude bots (v4.0): reports the
    /// rate limits to the daemon, then runs the user's own statusLine command. Always exits 0.
    Statusline {
        #[arg(long)]
        bot: String,
        /// Optional; falls back to `$AM_HOOK_TOKEN` (preferred — keeps the token out of `ps`).
        #[arg(long, default_value = "")]
        token: String,
        #[arg(long, default_value_t = 7788)]
        port: u16,
        /// 跟 hook 同一套 argv（`lifecycle::setup::hook_cmd_parts_for`）帶進來；statusline 不 spool，用不到。
        /// 不收的話 clap 直接報錯退出，claude 的狀態列整個不見、額度也不回報（2026-09-15 回歸，6e09a2e）。
        #[arg(long, default_value = "", hide = true)]
        data_dir: String,
    },
    /// issue #104：cargo shim 的本機 helper；讀設定／密碼後把 verification 整個丟到外部 SSH 主機。
    RemoteCargo {
        #[arg(long)]
        config: PathBuf,
        /// 沒給就從 `--config` 推（issue #417：`scripts/check.sh` 會清掉 `AM_DATA_DIR`）。
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cargo_args: Vec<String>,
    },
}
