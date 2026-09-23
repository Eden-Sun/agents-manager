//! config.toml 的寫入／重讀紀錄（issue #406）。
//!
//! 2026-09-23 13:28Z `build` 與 AGM 的 triage 被軟刪，daemon.log 只看得到投影那行 `soft-deleted`，
//! 查不出是誰改的 config.toml——最後是從 `intents` 表才反推出那是兩次 `DELETE /api/bots/{id}`，
//! 而發出 DELETE 的是誰（網頁？哪台？腳本？）到現在都不知道。這裡讓每一次寫入、每一次重讀發現的外部改動都留下：
//! 程式裡的呼叫位置、觸發它的 HTTP 請求（method／path／對端／User-Agent／Origin）、前後 bot 數、被拿掉的 id、
//! 檔案 mtime 與大小。

use crate::config::ConfigFile;
use std::collections::BTreeSet;
use std::panic::Location;
use std::path::Path;

tokio::task_local! {
    /// 正在處理的那個寫入型 HTTP 請求是誰發的（`api::auth` 在 token 驗過之後設）。背景 task 沒有這個值。
    pub static HTTP_CALLER: String;
}

/// 目前 task 所屬的 HTTP 請求；不是從 HTTP 請求來的（開機、背景巡邏、spawn 出去的 task）就是 `-`。
pub fn http_caller() -> String {
    HTTP_CALLER.try_with(Clone::clone).unwrap_or_else(|_| "-".to_string())
}

/// `method path peer=… ua=… origin=… referer=…`：只記辨識呼叫端用得到的標頭，不碰 token。
pub fn describe_request(req: &axum::extract::Request) -> String {
    let h = req.headers();
    let header = |name: &str| -> String {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(160).collect::<String>())
            .unwrap_or_else(|| "-".into())
    };
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.to_string())
        .unwrap_or_else(|| "-".into());
    format!(
        "{} {} peer={peer} ua={} origin={} referer={} caller={}",
        req.method(),
        req.uri().path(),
        header("user-agent"),
        header("origin"),
        header("referer"),
        header("x-am-caller"),
    )
}

/// 兩份 config 之間 bot／專案的差異。bot 以 id 為鍵（還沒補 id 的手寫列用 `?<name>`），顯示成 `id(name)`。
#[derive(Debug, Default, PartialEq)]
pub struct Diff {
    pub bots_before: usize,
    pub bots_after: usize,
    pub removed_bots: Vec<String>,
    pub added_bots: Vec<String>,
    pub projects_before: usize,
    pub projects_after: usize,
    pub removed_projects: Vec<String>,
    pub added_projects: Vec<String>,
}

fn bot_keys(cfg: &ConfigFile) -> BTreeSet<(String, String)> {
    cfg.projects
        .iter()
        .flat_map(|p| p.bots.iter())
        .map(|b| (b.id.clone().unwrap_or_else(|| format!("?{}", b.name)), b.name.clone()))
        .collect()
}

fn project_keys(cfg: &ConfigFile) -> BTreeSet<(String, String)> {
    cfg.projects
        .iter()
        .map(|p| (p.id.clone().unwrap_or_else(|| format!("?{}", p.path)), p.label.clone()))
        .collect()
}

fn minus(a: &BTreeSet<(String, String)>, b: &BTreeSet<(String, String)>) -> Vec<String> {
    let ids: BTreeSet<&String> = b.iter().map(|(id, _)| id).collect();
    a.iter().filter(|(id, _)| !ids.contains(id)).map(|(id, name)| format!("{id}({name})")).collect()
}

pub fn diff(before: &ConfigFile, after: &ConfigFile) -> Diff {
    let (b0, b1) = (bot_keys(before), bot_keys(after));
    let (p0, p1) = (project_keys(before), project_keys(after));
    Diff {
        bots_before: b0.len(),
        bots_after: b1.len(),
        removed_bots: minus(&b0, &b1),
        added_bots: minus(&b1, &b0),
        projects_before: p0.len(),
        projects_after: p1.len(),
        removed_projects: minus(&p0, &p1),
        added_projects: minus(&p1, &p0),
    }
}

/// 檔案目前的 mtime（RFC3339）與大小；讀不到就是 `-`。
fn file_meta(path: &Path) -> (String, String) {
    match std::fs::metadata(path) {
        Ok(m) => (
            m.modified().map(|t| crate::db::iso_at(t.into())).unwrap_or_else(|_| "-".into()),
            m.len().to_string(),
        ),
        Err(_) => ("-".into(), "-".into()),
    }
}

/// daemon 自己把 config.toml 寫出去了（寫完之後呼叫，mtime／大小是新檔的）。
pub fn log_write(at: &Location<'static>, path: &Path, before: &ConfigFile, after: &ConfigFile) {
    let d = diff(before, after);
    let (mtime, size) = file_meta(path);
    tracing::info!(
        caller = %at,
        http = %http_caller(),
        bots_before = d.bots_before,
        bots_after = d.bots_after,
        removed_bots = ?d.removed_bots,
        added_bots = ?d.added_bots,
        projects_before = d.projects_before,
        projects_after = d.projects_after,
        removed_projects = ?d.removed_projects,
        added_projects = ?d.added_projects,
        %mtime,
        size = %size,
        "config.toml written"
    );
}

/// 重讀時發現檔案跟 daemon 記得的不一樣：**不是 daemon 自己寫的**（daemon 寫完會更新記憶體那份）。
pub fn log_external_change(at: &Location<'static>, path: &Path, before: &ConfigFile, after: &ConfigFile) {
    let d = diff(before, after);
    let (mtime, size) = file_meta(path);
    tracing::warn!(
        noticed_by = %at,
        http = %http_caller(),
        bots_before = d.bots_before,
        bots_after = d.bots_after,
        removed_bots = ?d.removed_bots,
        added_bots = ?d.added_bots,
        projects_before = d.projects_before,
        projects_after = d.projects_after,
        removed_projects = ?d.removed_projects,
        added_projects = ?d.added_projects,
        %mtime,
        size = %size,
        "config.toml changed outside this daemon; reloaded before applying update"
    );
}

/// 重讀了、內容沒變但 mtime 變了（別人原樣重寫、或 touch）。只記 debug。
pub fn log_reload_unchanged(at: &Location<'static>, path: &Path) {
    let (mtime, size) = file_meta(path);
    tracing::debug!(caller = %at, %mtime, size = %size, "config.toml reloaded; unchanged");
}

#[cfg(test)]
pub(crate) mod capture {
    //! 測試用：把 tracing 事件收進一個 buffer，斷言 log 真的有寫出來（只收這個 task 的，平行測試不互相污染）。
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct Buf(pub Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Buf {
        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// 行程裡永遠活著的第二個 dispatcher（什麼都不收）。
    ///
    /// tracing-core 0.1.36 只註冊過一個 dispatcher 時走 `Rebuilder::JustOne`（callsite.rs 的 `rebuilder`／`for_each`）：
    /// 新 callsite 的 interest 是拿**註冊它的那條執行緒**的 default 算的。這個 binary 裡只有捕捉用 `set_default`，
    /// 所以捕捉期間別的測試執行緒第一次打到 `log_write` 之類的 callsite，會用它自己的 NoSubscriber 算出 `never` 並快取住，
    /// 之後這條執行緒的事件就再也收不到（CI run 35890397719；flaky-sweep 120 輪紅 8 輪）。
    /// 多一個一直活著的 dispatcher，註冊表就不會是「只有一個」，callsite 的 interest 一律對整張表算（有鎖、不看執行緒），
    /// 兩者合起來是 `sometimes`，每個事件照常問目前執行緒的 subscriber。production 用全域 subscriber，沒有這個問題。
    static KEEP_REGISTRY_PLURAL: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();

    /// 回傳一個只在 `guard` 活著的期間生效的 subscriber（`set_default` 綁在目前執行緒；測試要用 current_thread runtime）。
    pub fn start() -> (Buf, tracing::subscriber::DefaultGuard) {
        // 一定要在 `set_default` 之前：註冊自己那一刻表裡就已經有兩個，`has_just_one` 才會是 false。
        KEEP_REGISTRY_PLURAL.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt().with_writer(buf.clone()).with_ansi(false).with_max_level(tracing::Level::DEBUG).finish();
        (buf.clone(), tracing::subscriber::set_default(sub))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> ConfigFile {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn diff_names_removed_and_added_bots_by_id() {
        let before = cfg("[[projects]]\nid='p1'\npath='/tmp'\nlabel='one'\n[[projects.bots]]\nid='b1'\nname='build'\nkind='claude'\n[[projects.bots]]\nid='b2'\nname='triage'\nkind='claude'\n");
        let after = cfg("[[projects]]\nid='p1'\npath='/tmp'\nlabel='one'\n[[projects.bots]]\nid='b2'\nname='triage'\nkind='claude'\n[[projects.bots]]\nid='b3'\nname='new'\nkind='codex'\n");
        let d = diff(&before, &after);
        assert_eq!((d.bots_before, d.bots_after), (2, 2));
        assert_eq!(d.removed_bots, vec!["b1(build)".to_string()]);
        assert_eq!(d.added_bots, vec!["b3(new)".to_string()]);
        assert!(d.removed_projects.is_empty() && d.added_projects.is_empty());
    }

    /// CI run 35890397719 那條偶發紅的確定性重現：捕捉進行中，**另一條執行緒先第一次**打到某個 callsite
    /// （全樹很多測試都會寫 config，`log_write` 的 callsite 常常是別人先打到），這條執行緒之後打同一個 callsite 還是要收得到。
    #[test]
    fn a_callsite_first_hit_on_another_thread_while_capturing_still_reaches_the_capture() {
        fn probe() {
            tracing::info!("capture-probe-7f3a");
        }
        let (buf, _guard) = capture::start();
        std::thread::spawn(probe).join().unwrap();
        probe();
        assert!(buf.text().contains("capture-probe-7f3a"), "別的執行緒先註冊的 callsite 被快取成 never：{:?}", buf.text());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_caller_is_dash_outside_a_request_and_the_request_inside_one() {
        assert_eq!(http_caller(), "-");
        let seen = HTTP_CALLER.scope("DELETE /api/bots/x peer=1.2.3.4:5".into(), async { http_caller() }).await;
        assert_eq!(seen, "DELETE /api/bots/x peer=1.2.3.4:5");
    }
}
