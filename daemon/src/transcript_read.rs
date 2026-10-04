//! transcript／rollout 的讀取（claude `projects/<cwd>/<session>.jsonl`、codex `sessions/…/rollout-*.jsonl`）。
//!
//! 路徑來自 hook payload（`runs.transcript_path`），而 hook 是 pane 裡的 CLI——也就是任何拿得到那顆 bot 的 token 的行程——送進來的。
//! 所以：寫進 `runs` 之前要驗路徑（[`path_within_roots`]）；讀的時候只讀一般檔、不被 FIFO 卡住、不被 `/dev/zero` 灌爆、
//! 一次讀的量有上限（[`read_tail`]、[`read_since`]）。

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;

/// 一次讀「基準位移之後新長的部分」的上限。回合送出到驗證之間 transcript 不會長這麼多；真的超過就當讀不到（不是沒有命中）。
pub(crate) const MAX_SINCE_BYTES: u64 = 32 * 1024 * 1024;

/// 只開一般檔：FIFO 用 `O_NONBLOCK` 開就不會卡住、之後 `metadata` 說不是一般檔就退；`/dev/zero` 之類的裝置同樣被擋
/// （它們的 `len()` 是 0，後面照 `read_to_end` 就是讀到記憶體滿）。
pub(crate) fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    let anchored = path.ancestors().find(|p| matches!(p.file_name(), Some(n) if n == "projects" || n == "sessions"));
    let f = if let Some(root) = anchored {
        let root_meta = std::fs::symlink_metadata(root)?;
        if !root_meta.file_type().is_dir() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "transcript root is not a real directory"));
        }
        let config = root.parent().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "transcript root has no account directory"))?;
        let config = std::fs::canonicalize(config)?;
        let root_name = root.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "transcript root has no name"))?;
        let expected = config.join(root_name);
        let resolved = std::fs::canonicalize(root)?;
        if resolved != expected || !resolved.starts_with(&config) {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "transcript root escaped its account directory"));
        }
        let rel = path.strip_prefix(root).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "transcript is outside its root"))?;
        let abs = resolved.join(rel);
        let rel = abs.strip_prefix("/").map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "transcript path is not absolute"))?;
        let parts = crate::trusted_open::safe_relative_components(rel).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "unsafe transcript path"))?;
        crate::trusted_open::open_bound_file(Path::new("/"), &parts, None)?
    } else {
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(path)?
    };
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    if meta.nlink() > 1 {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "transcript has more than one hard link"));
    }
    Ok(f)
}

/// 檔案最後 `max` 個位元組（第一行可能被切到一半、最後一行可能還沒寫完，解析時各自會被跳過）。不是一般檔、讀不到＝`None`。
pub(crate) fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut f = open_regular(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `offset` 之後的內容。檔案比基準短（被換掉、被截斷）、或新長的超過 [`MAX_SINCE_BYTES`]、或不是一般檔＝錯誤（讀不到≠零）。
pub(crate) fn read_since(path: &Path, offset: u64, what: &str) -> std::io::Result<Vec<u8>> {
    let mut f = open_regular(path)?;
    let len = f.metadata()?.len();
    if len < offset {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{what} shrank below the baseline")));
    }
    if len - offset > MAX_SINCE_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{what} grew by more than {MAX_SINCE_BYTES} bytes since the baseline")));
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.take(MAX_SINCE_BYTES + 1).read_to_end(&mut buf)?;
    Ok(buf)
}

/// 這個路徑能不能當 transcript／rollout：絕對路徑、沒有 `..`、沒有控制字元、`.jsonl`，而且在 `roots` 其中一個底下
/// ——檔案（或最近的既有上層）解開符號連結之後仍在那個 root 的實際位置底下。
pub(crate) fn path_within_roots(path: &str, roots: &[PathBuf]) -> bool {
    let p = Path::new(path);
    if !p.is_absolute()
        || path.chars().any(char::is_control)
        || p.components().any(|c| matches!(c, std::path::Component::ParentDir))
        || p.extension().and_then(|e| e.to_str()) != Some("jsonl")
    {
        return false;
    }
    let canonical_root = |root: &Path| {
        let meta = std::fs::symlink_metadata(root).ok()?;
        if !meta.file_type().is_dir() { return None; }
        let parent = std::fs::canonicalize(root.parent()?).ok()?;
        let expected = parent.join(root.file_name()?);
        let resolved = std::fs::canonicalize(root).ok()?;
        (resolved == expected).then_some(resolved)
    };
    let under = |resolved: &Path| roots.iter().filter_map(|r| canonical_root(r)).any(|rc| resolved.starts_with(rc));
    if !roots.iter().any(|r| p.starts_with(r)) {
        return false;
    }
    let mut cur = Some(p);
    while let Some(c) = cur {
        if let Ok(resolved) = std::fs::canonicalize(c) {
            return under(&resolved);
        }
        cur = c.parent();
    }
    false
}

/// 這顆本機 bot 的 transcript／rollout 該在哪些目錄底下：claude＝它實際用的 `CLAUDE_CONFIG_DIR`（bot 自己的 env → 身分 → `~/.claude`）的
/// `projects/`；codex＝`CODEX_HOME/sessions/`；grok 沒有。子 agent 的身份如果尚未從 pane env 記錄進 DB，就先不收 transcript_path；
/// 放行所有 `~/.claude-*` 會讓一顆 bot 用自己的 hook token 指定另一個身份的對話檔，之後 UI 的輪詢就會替它讀出來。
pub(crate) async fn trusted_roots(app: &Arc<App>, bot: &db::Bot) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    match bot.kind.as_str() {
        "claude" => {
            let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
            let own = match (bot.env().get("CLAUDE_CONFIG_DIR"), &home) {
                (Some(v), Some(h)) if !v.trim().is_empty() => Some(crate::config::expand_home(v.trim(), h)),
                _ => crate::lifecycle::identity_config_dir(app, LOCAL_HOST, bot.identity.as_deref()).await.ok(),
            };
            roots.extend(own.map(|d| PathBuf::from(d).join("projects")));
        }
        "codex" => roots.extend(crate::lifecycle::codex_home(app, bot).await.map(|h| h.join("sessions"))),
        // agy：設定目錄只認 `$HOME`（沒有身分切換），對話在 `brain/<id>/.system_generated/logs/`。
        "agy" => roots.extend(crate::home::dir().map(|h| h.join(".gemini").join("antigravity-cli").join("brain"))),
        _ => {}
    }
    roots
}

/// hook 送來的 `transcript_path` 能不能寫進 `runs`。本機 bot：要在 [`trusted_roots`] 底下。遠端 bot 的檔案在那台、daemon 不在本機讀它
/// （只在換身分時經 `sh_quote` 過的 ssh script 搬），所以只擋形狀（絕對路徑、無 `..`、無控制字元、`.jsonl`）。
pub(crate) async fn transcript_allowed(app: &Arc<App>, bot: &db::Bot, path: &str) -> bool {
    match db::bot_host(&app.db, &bot.id).await {
        Ok(host) if host != LOCAL_HOST => {
            let p = Path::new(path);
            p.is_absolute()
                && !path.chars().any(char::is_control)
                && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
                && p.extension().and_then(|e| e.to_str()) == Some("jsonl")
        }
        Ok(_) => path_within_roots(path, &trusted_roots(app, bot).await),
        // A host lookup failure gives no basis for deciding whether this daemon can read the path.
        Err(_) => false,
    }
}

/// A path may be retained for a remote bot, but only a local bot's own transcript may be opened
/// on this daemon. Recheck both host and bot-owned root at each direct-read call site.
pub(crate) async fn local_transcript_allowed(app: &Arc<App>, bot: &db::Bot, path: &str) -> bool {
    if !matches!(db::bot_host(&app.db, &bot.id).await, Ok(host) if host == LOCAL_HOST) {
        return false;
    }
    path_within_roots(path, &trusted_roots(app, bot).await)
}

#[cfg(test)]
mod tests {
    use crate::db;
    use crate::hookrecv::{process, HookBody};
    use crate::testing as tt;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    fn tmp(tag: &str) -> std::path::PathBuf {
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
        assert!(super::transcript_allowed(&e.app, &bot, own_file.to_str().unwrap()).await);
        assert!(!super::transcript_allowed(&e.app, &bot, foreign_file.to_str().unwrap()).await, "child token cannot nominate another identity's transcript");
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

    /// root 底下放一條指到外面的符號連結：字面上在 root 裡、實際上在別處，不收。
    #[test]
    fn a_symlink_out_of_the_root_is_not_inside_it() {
        let root = tmp("symroot");
        let outside = tmp("symout");
        std::fs::write(outside.join("s.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(root.join("projects")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("projects/-work")).unwrap();
        std::os::unix::fs::symlink(outside.join("s.jsonl"), root.join("projects/link.jsonl")).unwrap();
        let roots = vec![root.join("projects")];
        for p in ["projects/-work/s.jsonl", "projects/link.jsonl", "projects/-work/new.jsonl"] {
            assert!(!super::path_within_roots(&root.join(p).to_string_lossy(), &roots), "{p} 解開符號連結後在 root 外面");
        }
        std::fs::create_dir_all(root.join("projects/-real")).unwrap();
        assert!(super::path_within_roots(&root.join("projects/-real/not-yet-written.jsonl").to_string_lossy(), &roots), "還沒寫出來的檔、目錄在 root 裡：收");
    }

    #[test]
    fn a_symlinked_projects_root_cannot_expand_the_trusted_boundary() {
        let base = tmp("root-link");
        let config = base.join(".claude-own");
        let other = base.join(".claude-other");
        std::fs::create_dir_all(other.join("projects/-work")).unwrap();
        std::fs::write(other.join("projects/-work/foreign.jsonl"), "secret\n").unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::os::unix::fs::symlink(other.join("projects"), config.join("projects")).unwrap();
        let forged = config.join("projects/-work/foreign.jsonl");
        assert!(!super::path_within_roots(&forged.to_string_lossy(), &[config.join("projects")]), "projects 根 symlink 不能擴張 trusted root");
    }

    #[test]
    fn a_symlinked_sessions_root_cannot_expand_promote_identity_lookup() {
        let base = tmp("session-root-link");
        let config = base.join(".claude-own");
        let other = base.join(".claude-other");
        std::fs::create_dir_all(other.join("sessions")).unwrap();
        std::fs::write(other.join("sessions/123.json"), r#"{"pid":123,"sessionId":"foreign","cwd":"/work"}"#).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::os::unix::fs::symlink(other.join("sessions"), config.join("sessions")).unwrap();
        assert!(super::open_regular(&config.join("sessions/123.json")).is_err(), "promote must not read session metadata through another identity's root");
    }

    #[test]
    fn transcript_reader_refuses_final_symlinks_and_hard_links() {
        let dir = tmp("aliases");
        let source = dir.join("source.jsonl");
        std::fs::write(&source, "private transcript\n").unwrap();
        let symlink = dir.join("symlink.jsonl");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        let hardlink = dir.join("hardlink.jsonl");
        std::fs::hard_link(&source, &hardlink).unwrap();
        assert!(super::open_regular(&symlink).is_err(), "一般檔讀取不能跟最終 symlink");
        assert!(super::open_regular(&hardlink).is_err(), "transcript 不能以 hard link 跨身分別名進來");
    }

    /// 裝置檔（`/dev/zero`）的長度是 0、讀不完：不是一般檔就不讀。尾端讀取半行、壞 JSON 照舊由解析端跳過。
    #[test]
    fn a_device_is_not_read_and_a_cut_first_line_is_tolerated() {
        assert_eq!(super::read_tail(std::path::Path::new("/dev/zero"), 4096), None);
        let dir = tmp("tail");
        let p = dir.join("t.jsonl");
        let body = format!("{}\n{{\"a\":1}}\n{{\"b\":", "x".repeat(10_000));
        std::fs::write(&p, &body).unwrap();
        let got = super::read_tail(&p, 64).unwrap();
        assert!(got.len() <= 64 && got.ends_with("{\"b\":"));
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
