//! P5（capture／transcript）的 App 端接縫：把 `transcript_read` 要的窄介面 [`TranscriptRoots`] 用 `App`＋`db::Bot` 實作出來，
//! 並提供既有呼叫端用的 `(&Arc<App>, &db::Bot, path)` 簽名。`App` 只活在這個檔（composition 層），`transcript_read` 本身不再看得到它。

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;
use crate::transcript_read::{self, TranscriptRoots};

/// 一顆 bot 的 transcript 來源事實（主機、身分目錄、codex home）從 `App` 取。
struct AppTranscriptRoots<'a> {
    app: &'a Arc<App>,
    bot: &'a db::Bot,
}

impl TranscriptRoots for AppTranscriptRoots<'_> {
    fn kind(&self) -> &str {
        &self.bot.kind
    }

    fn is_local(&self) -> impl Future<Output = Option<bool>> + Send + '_ {
        async move { db::bot_host(&self.app.db, &self.bot.id).await.ok().map(|host| host == LOCAL_HOST) }
    }

    fn claude_config_dir(&self) -> impl Future<Output = Option<String>> + Send + '_ {
        async move {
            let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
            match (self.bot.env().get("CLAUDE_CONFIG_DIR"), &home) {
                (Some(v), Some(h)) if !v.trim().is_empty() => Some(crate::config::expand_home(v.trim(), h)),
                _ => crate::lifecycle::identity_config_dir(self.app, LOCAL_HOST, self.bot.identity.as_deref()).await.ok(),
            }
        }
    }

    fn codex_home(&self) -> impl Future<Output = Option<PathBuf>> + Send + '_ {
        async move { crate::lifecycle::codex_home(self.app, self.bot).await }
    }

    fn user_home(&self) -> Option<PathBuf> {
        crate::home::dir()
    }
}

// 目前只有測試（`lifecycle::agy_tests`）直接問 root 清單。
#[cfg(test)]
pub(crate) async fn trusted_roots(app: &Arc<App>, bot: &db::Bot) -> Vec<PathBuf> {
    transcript_read::trusted_roots_for(&AppTranscriptRoots { app, bot }).await
}

pub(crate) async fn transcript_allowed(app: &Arc<App>, bot: &db::Bot, path: &str) -> bool {
    transcript_read::transcript_allowed_for(&AppTranscriptRoots { app, bot }, path).await
}

pub(crate) async fn local_transcript_allowed(app: &Arc<App>, bot: &db::Bot, path: &str) -> bool {
    transcript_read::local_transcript_allowed_for(&AppTranscriptRoots { app, bot }, path).await
}

/// 跨模組的 transcript 安全測試（讀的人、收路徑的人、App 的身分／主機事實）：它們量的是整個 daemon 接起來之後的行為，所以留在 composition 層。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::hookrecv::{process, HookBody};
    use crate::testing as tt;
    use serde_json::json;
    use std::time::Duration;

    fn tmp(tag: &str) -> PathBuf {
        let d = tt::track(std::env::temp_dir().join(format!("am-test-tx-{tag}-{}", db::ulid())));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 指到沒有人在寫的 FIFO：`File::open` 會永遠卡在那裡——占住 blocking 執行緒，輪詢每幾秒再丟一個，執行緒池就被吃光。
    #[test]
    fn a_fifo_as_the_transcript_never_blocks_the_reader() {
        let dir = tmp("fifo");
        let fifo = dir.join("t.jsonl");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        for (name, read) in [
            ("pending_question", Box::new({ let p = fifo.clone(); move || crate::pending_question::read_tail(&p, 4096) }) as Box<dyn FnOnce() -> Option<String> + Send>),
            ("transcript_origin", Box::new({ let p = fifo.clone(); move || crate::lifecycle::transcript_origin::read_tail(&p) })),
        ] {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(read());
            });
            let got = rx.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| panic!("{name}: 讀 FIFO 卡住了（blocking 執行緒被永遠占住）"));
            assert!(got.is_none(), "{name}: 不是一般檔就不讀");
        }
    }

    /// 回合送出之後到驗證之前 transcript 長了幾百 MB：以前從基準位移一路讀到檔尾（`read_to_end`），整份進記憶體。
    /// 要有上限；超過就回錯（讀不到≠沒有命中，呼叫端照「證不出來」處理）。
    #[test]
    fn the_new_bytes_since_the_baseline_are_capped() {
        let dir = tmp("since");
        let big = dir.join("big.jsonl");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(40 * 1024 * 1024).unwrap(); // 稀疏檔：幾乎不占磁碟
        let res = crate::lifecycle::log_hits_since(crate::lifecycle::LogFormat::Claude, &big, 0, "x");
        assert!(res.is_err(), "40 MB 的新內容要被擋下（上限 32 MB）：{res:?}");
        let small = dir.join("small.jsonl");
        std::fs::write(&small, "{}\n").unwrap();
        assert_eq!(crate::lifecycle::log_hits_since(crate::lifecycle::LogFormat::Claude, &small, 0, "x").unwrap(), 0);
    }

    async fn claude_run(app: &Arc<crate::state::App>, project_id: &str, name: &str, cfg_dir: &std::path::Path) -> (db::Bot, String) {
        let bot = tt::claude_bot(app, project_id, name).await;
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CLAUDE_CONFIG_DIR": cfg_dir.to_string_lossy()}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = db::ulid();
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at) VALUES (?,?,'running','idle','pane-tx',?)")
            .bind(&run)
            .bind(&bot.id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        (db::bot(&app.db, &bot.id).await.unwrap().unwrap(), run)
    }

    #[tokio::test]
    async fn an_unrecorded_child_identity_cannot_read_another_claude_identity_transcript() {
        let e = tt::env().await;
        let home = crate::test_home::dir();
        let own = home.join(".claude-own");
        let foreign = home.join(".claude-foreign");
        let bot = tt::claude_bot(&e.app, &e.project_id, "child-read-boundary").await;
        sqlx::query("UPDATE bots SET managed_by='child', env_json=? WHERE id=?")
            .bind(json!({"CLAUDE_CONFIG_DIR": own.to_string_lossy()}).to_string())
            .bind(&bot.id)
            .execute(&e.app.db)
            .await
            .unwrap();
        let own_file = own.join("projects/-work/session.jsonl");
        let foreign_file = foreign.join("projects/-work/session.jsonl");
        std::fs::create_dir_all(own_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(foreign_file.parent().unwrap()).unwrap();
        std::fs::write(&own_file, "own\n").unwrap();
        std::fs::write(&foreign_file, "foreign\n").unwrap();
        let bot = db::bot(&e.app.db, &bot.id).await.unwrap().unwrap();
        assert!(transcript_allowed(&e.app, &bot, own_file.to_str().unwrap()).await);
        assert!(!transcript_allowed(&e.app, &bot, foreign_file.to_str().unwrap()).await, "child token cannot nominate another identity's transcript");
    }

    async fn session_start(app: &Arc<crate::state::App>, bot: &db::Bot, run: &str, path: &str) {
        let body = HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "SessionStart", "session_id": "s-tx", "transcript_path": path}),
            received_at: None,
            truncated: false,
            run_id: Some(run.to_string()),
        };
        process(app, &body).await.unwrap();
    }

    async fn stored_path(app: &Arc<crate::state::App>, run: &str) -> Option<String> {
        sqlx::query_scalar("SELECT transcript_path FROM runs WHERE id=?").bind(run).fetch_one(&app.db).await.unwrap()
    }

    /// hook 是 pane 裡的行程送的：它說 transcript 在 `/etc/passwd`、在別顆 bot（別的身分）的目錄、或用 `..` 爬出去，
    /// daemon 之後每次輪詢都會去讀那個檔（回合結束判斷、AskUserQuestion 補抓、`GET /pending-question`…），
    /// 把別人的對話內容當成這顆 bot 的。路徑要在這顆 bot 自己身分的 `projects/` 底下才收。
    #[tokio::test]
    async fn a_hook_cannot_point_the_transcript_outside_the_bots_own_projects_dir() {
        let e = tt::env().await;
        let cfg = tmp("cfg");
        let other = tmp("other-bot-cfg");
        let (bot, run) = claude_run(&e.app, &e.project_id, "txbot", &cfg).await;
        std::fs::create_dir_all(cfg.join("projects/-work")).unwrap();
        std::fs::create_dir_all(other.join("projects/-work")).unwrap();
        std::fs::write(other.join("projects/-work/s.jsonl"), "{}\n").unwrap();
        let outside = std::fs::canonicalize(&other).unwrap();

        for forged in [
            "/etc/passwd".to_string(),
            outside.join("projects/-work/s.jsonl").to_string_lossy().into_owned(),
            format!("{}/projects/../../{}/projects/-work/s.jsonl", cfg.display(), other.file_name().unwrap().to_string_lossy()),
            format!("{}/projects/-work/notes.txt", cfg.display()),
            "relative/path/s.jsonl".to_string(),
        ] {
            session_start(&e.app, &bot, &run, &forged).await;
            assert_eq!(stored_path(&e.app, &run).await, None, "{forged} 不能被收下");
        }
        let good = cfg.join("projects/-work/s-tx.jsonl");
        session_start(&e.app, &bot, &run, &good.to_string_lossy()).await;
        assert_eq!(stored_path(&e.app, &run).await.as_deref(), Some(good.to_string_lossy().as_ref()), "自己的 projects 目錄底下照收");
    }

    /// Stop 的 `transcript_path` 之後會被直接讀、把最後一則使用者訊息當成這顆 bot 的對話記下來：
    /// 指到別處的檔，內容不能進對話。
    #[tokio::test]
    async fn a_stop_pointing_at_someone_elses_transcript_does_not_put_its_text_in_the_conversation() {
        let e = tt::env().await;
        let cfg = tmp("cfg-stop");
        let other = tmp("other-stop");
        let (bot, run) = claude_run(&e.app, &e.project_id, "txstop", &cfg).await;
        let victim = other.join("projects/-work/s.jsonl");
        std::fs::create_dir_all(victim.parent().unwrap()).unwrap();
        let line = json!({"type": "user", "message": {"role": "user", "content": "別顆 bot 的機密指令 LEAK-9431"}}).to_string();
        std::fs::write(&victim, format!("{line}\n")).unwrap();
        let body = HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-tx", "prompt_id": "p-1", "stop_hook_active": false,
                            "last_assistant_message": "好的", "transcript_path": victim.to_string_lossy()}),
            received_at: None,
            truncated: false,
            run_id: Some(run.clone()),
        };
        process(&e.app, &body).await.unwrap();
        let leaked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE content LIKE '%LEAK-9431%'").fetch_one(&e.app.db).await.unwrap();
        assert_eq!(leaked, 0, "別處的 transcript 內容進了對話");
        assert_eq!(stored_path(&e.app, &run).await, None);
    }
}
