//! transcript／rollout 的讀取（claude `projects/<cwd>/<session>.jsonl`、codex `sessions/…/rollout-*.jsonl`）。
//!
//! 路徑來自 hook payload（`runs.transcript_path`），而 hook 是 pane 裡的 CLI——也就是任何拿得到那顆 bot 的 token 的行程——送進來的。
//! 所以：寫進 `runs` 之前要驗路徑（[`path_within_roots`]）；讀的時候只讀一般檔、不被 FIFO 卡住、不被 `/dev/zero` 灌爆、
//! 一次讀的量有上限（[`read_tail`]、[`read_since`]）。

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::future::Future;
use std::path::{Path, PathBuf};

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

/// 判斷「這顆 bot 的 transcript 該在哪」所需的全部外部事實，窄到不必看到 `App`／`db::Bot`：呼叫端（目前是
/// `app_ports_p5` 的 `AppTranscriptRoots`，之後是 composition root）為**一顆 bot**實作它，這個檔只剩純規則。
/// 查不到的事實一律回 `None`（＝這個 root 不收），不是猜一個。
pub(crate) trait TranscriptRoots: Send + Sync {
    /// `claude`／`codex`／`grok`／`agy`。
    fn kind(&self) -> &str;
    /// 這顆 bot 跑在本機嗎。`None`＝主機查不到：沒有依據判斷 daemon 讀不讀得了這個路徑，一律不收。
    fn is_local(&self) -> impl Future<Output = Option<bool>> + Send + '_;
    /// claude 實際用的 `CLAUDE_CONFIG_DIR`（bot 自己的 env → 身分 → `~/.claude`）；查不到＝`None`。
    fn claude_config_dir(&self) -> impl Future<Output = Option<String>> + Send + '_;
    /// codex 的 `CODEX_HOME`（身分 → bot env → `~/.codex`）；沒有家目錄＝`None`。
    fn codex_home(&self) -> impl Future<Output = Option<PathBuf>> + Send + '_;
    /// 使用者的家目錄（agy 的設定目錄只認 `$HOME`，沒有身分切換）。
    fn user_home(&self) -> Option<PathBuf>;
}

/// 這顆本機 bot 的 transcript／rollout 該在哪些目錄底下：claude＝它實際用的 `CLAUDE_CONFIG_DIR`（bot 自己的 env → 身分 → `~/.claude`）的
/// `projects/`；codex＝`CODEX_HOME/sessions/`；grok 沒有。子 agent 的身份如果尚未從 pane env 記錄進 DB，就先不收 transcript_path；
/// 放行所有 `~/.claude-*` 會讓一顆 bot 用自己的 hook token 指定另一個身份的對話檔，之後 UI 的輪詢就會替它讀出來。
pub(crate) async fn trusted_roots_for(src: &impl TranscriptRoots) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    match src.kind() {
        "claude" => roots.extend(src.claude_config_dir().await.map(|d| PathBuf::from(d).join("projects"))),
        "codex" => roots.extend(src.codex_home().await.map(|h| h.join("sessions"))),
        // agy：設定目錄只認 `$HOME`（沒有身分切換），對話在 `brain/<id>/.system_generated/logs/`。
        "agy" => roots.extend(src.user_home().map(|h| h.join(".gemini").join("antigravity-cli").join("brain"))),
        _ => {}
    }
    roots
}

/// hook 送來的 `transcript_path` 能不能寫進 `runs`。本機 bot：要在 [`trusted_roots_for`] 底下。遠端 bot 的檔案在那台、daemon 不在本機讀它
/// （只在換身分時經 `sh_quote` 過的 ssh script 搬），所以只擋形狀（絕對路徑、無 `..`、無控制字元、`.jsonl`）。
pub(crate) async fn transcript_allowed_for(src: &impl TranscriptRoots, path: &str) -> bool {
    match src.is_local().await {
        Some(false) => {
            let p = Path::new(path);
            p.is_absolute()
                && !path.chars().any(char::is_control)
                && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
                && p.extension().and_then(|e| e.to_str()) == Some("jsonl")
        }
        Some(true) => path_within_roots(path, &trusted_roots_for(src).await),
        // A host lookup failure gives no basis for deciding whether this daemon can read the path.
        None => false,
    }
}

/// A path may be retained for a remote bot, but only a local bot's own transcript may be opened
/// on this daemon. Recheck both host and bot-owned root at each direct-read call site.
pub(crate) async fn local_transcript_allowed_for(src: &impl TranscriptRoots, path: &str) -> bool {
    if src.is_local().await != Some(true) {
        return false;
    }
    path_within_roots(path, &trusted_roots_for(src).await)
}

// 過渡：既有呼叫端（hookrecv、lifecycle、pending_question…）仍用 `transcript_read::{…}(&app, &bot, …)` 這三個名字。
// App 版本住在 composition 層（`app_ports_p5`）；呼叫端改 import 之後把這行刪掉，這個檔就不再碰 `App`。
pub(crate) use crate::app_ports_p5::{local_transcript_allowed, transcript_allowed};
#[cfg(test)]
pub(crate) use crate::app_ports_p5::trusted_roots;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = tt::track(std::env::temp_dir().join(format!("am-test-tx-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))));
        std::fs::create_dir_all(&d).unwrap();
        d
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
            assert!(!path_within_roots(&root.join(p).to_string_lossy(), &roots), "{p} 解開符號連結後在 root 外面");
        }
        std::fs::create_dir_all(root.join("projects/-real")).unwrap();
        assert!(path_within_roots(&root.join("projects/-real/not-yet-written.jsonl").to_string_lossy(), &roots), "還沒寫出來的檔、目錄在 root 裡：收");
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
        assert!(!path_within_roots(&forged.to_string_lossy(), &[config.join("projects")]), "projects 根 symlink 不能擴張 trusted root");
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
        assert!(open_regular(&config.join("sessions/123.json")).is_err(), "promote must not read session metadata through another identity's root");
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
        assert!(open_regular(&symlink).is_err(), "一般檔讀取不能跟最終 symlink");
        assert!(open_regular(&hardlink).is_err(), "transcript 不能以 hard link 跨身分別名進來");
    }

    /// 裝置檔（`/dev/zero`）的長度是 0、讀不完：不是一般檔就不讀。尾端讀取半行、壞 JSON 照舊由解析端跳過。
    #[test]
    fn a_device_is_not_read_and_a_cut_first_line_is_tolerated() {
        assert_eq!(read_tail(std::path::Path::new("/dev/zero"), 4096), None);
        let dir = tmp("tail");
        let p = dir.join("t.jsonl");
        let body = format!("{}\n{{\"a\":1}}\n{{\"b\":", "x".repeat(10_000));
        std::fs::write(&p, &body).unwrap();
        let got = read_tail(&p, 64).unwrap();
        assert!(got.len() <= 64 && got.ends_with("{\"b\":"));
    }


    /// 單顆 bot 的假來源：每個事實都由測試給定，證明授權規則不需要 `App`／DB。
    struct Fake {
        kind: &'static str,
        local: Option<bool>,
        claude: Option<PathBuf>,
        codex: Option<PathBuf>,
        home: Option<PathBuf>,
    }

    impl TranscriptRoots for Fake {
        fn kind(&self) -> &str {
            self.kind
        }
        fn is_local(&self) -> impl Future<Output = Option<bool>> + Send + '_ {
            async move { self.local }
        }
        fn claude_config_dir(&self) -> impl Future<Output = Option<String>> + Send + '_ {
            async move { self.claude.as_ref().map(|p| p.to_string_lossy().into_owned()) }
        }
        fn codex_home(&self) -> impl Future<Output = Option<PathBuf>> + Send + '_ {
            async move { self.codex.clone() }
        }
        fn user_home(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    fn fake(kind: &'static str) -> Fake {
        Fake { kind, local: Some(true), claude: None, codex: None, home: None }
    }

    #[tokio::test]
    async fn roots_follow_the_kind_and_a_missing_fact_means_no_root() {
        let base = tmp("roots");
        let mut f = fake("claude");
        assert!(trusted_roots_for(&f).await.is_empty(), "claude 設定目錄查不到就沒有 root");
        f.claude = Some(base.join(".claude-own"));
        assert_eq!(trusted_roots_for(&f).await, vec![base.join(".claude-own/projects")]);
        let mut f = fake("codex");
        f.codex = Some(base.join(".codex"));
        assert_eq!(trusted_roots_for(&f).await, vec![base.join(".codex/sessions")]);
        let mut f = fake("agy");
        assert!(trusted_roots_for(&f).await.is_empty());
        f.home = Some(base.clone());
        assert_eq!(trusted_roots_for(&f).await, vec![base.join(".gemini/antigravity-cli/brain")]);
        assert!(trusted_roots_for(&fake("grok")).await.is_empty(), "grok 沒有 transcript root");
    }

    #[tokio::test]
    async fn a_local_bot_reads_only_under_its_own_root_and_an_unknown_host_reads_nothing() {
        let cfg = tmp("auth-cfg");
        let other = tmp("auth-other");
        std::fs::create_dir_all(cfg.join("projects/-w")).unwrap();
        std::fs::create_dir_all(other.join("projects/-w")).unwrap();
        let own = cfg.join("projects/-w/s.jsonl");
        let foreign = other.join("projects/-w/s.jsonl");
        let (own_s, foreign_s) = (own.to_string_lossy().into_owned(), foreign.to_string_lossy().into_owned());

        let mut f = fake("claude");
        f.claude = Some(cfg.clone());
        assert!(transcript_allowed_for(&f, &own_s).await && local_transcript_allowed_for(&f, &own_s).await);
        assert!(!transcript_allowed_for(&f, &foreign_s).await && !local_transcript_allowed_for(&f, &foreign_s).await);

        f.local = None;
        assert!(!transcript_allowed_for(&f, &own_s).await, "主機查不到：沒有依據，不收");
        assert!(!local_transcript_allowed_for(&f, &own_s).await);
    }

    /// 遠端 bot 的檔案在那台：能留下路徑（只擋形狀），但這台 daemon 絕不開它。
    #[tokio::test]
    async fn a_remote_bot_keeps_a_well_shaped_path_but_is_never_opened_locally() {
        let mut f = fake("claude");
        f.local = Some(false);
        for ok in ["/remote/home/.claude/projects/-w/s.jsonl"] {
            assert!(transcript_allowed_for(&f, ok).await, "{ok}");
            assert!(!local_transcript_allowed_for(&f, ok).await, "{ok}");
        }
        for bad in ["relative/s.jsonl", "/a/../b/s.jsonl", "/a/b/s.txt", "/a/b\u{7}/s.jsonl"] {
            assert!(!transcript_allowed_for(&f, bad).await, "{bad:?}");
        }
    }
}
